use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration as StdDuration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, StorageConfig, TargetConfig, validated_database_url};
use crate::state::{RunPhase, RunRecord, RunStatus, StateStore};

const SCHEDULED_ATTEMPTS: u32 = 4;
const SCHEDULED_RETRY_DELAY: StdDuration = StdDuration::from_secs(15 * 60);

#[derive(Debug, Serialize)]
struct BackupManifest<'a> {
    schema_version: u32,
    target: &'a str,
    created_at: DateTime<Utc>,
    format: &'static str,
    compression: &'static str,
    encryption: &'static str,
    object_key: &'a str,
    object_version_id: Option<&'a str>,
    encrypted_bytes: u64,
    encrypted_sha256: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ObjectMetadata {
    version_id: Option<String>,
    object_lock_mode: Option<String>,
    object_lock_retain_until_date: Option<String>,
}

pub fn run(config: &Config, target: &TargetConfig) -> Result<()> {
    ensure!(target.enabled, "backup target {} is disabled", target.name);
    let state = StateStore::new(&config.defaults.state_dir);
    state.ensure_layout()?;
    fs::create_dir_all(&config.defaults.scratch_dir).with_context(|| {
        format!(
            "failed to create scratch directory {}",
            config.defaults.scratch_dir.display()
        )
    })?;

    let lock = state.open_lock(&target.name)?;
    lock.try_lock_exclusive()
        .with_context(|| format!("backup target {} is already running", target.name))?;

    let mut record = RunRecord::running(&target.name);
    state.write_current(&record)?;
    ping_healthcheck(target.healthcheck_url.as_deref(), "/start");
    let started = Instant::now();

    let outcome = perform_backup(config, target, &state, &mut record);
    let finished_at = Utc::now();
    record.finished_at = Some(finished_at);
    record.duration_seconds = Some(started.elapsed().as_secs());

    match outcome {
        Ok(()) => {
            record.status = RunStatus::Success;
            state.write_current(&record)?;
            state.append_history(&record)?;
            ping_healthcheck(target.healthcheck_url.as_deref(), "");
            println!(
                "backup {} succeeded in {}s: s3://{}/{}",
                target.name,
                record.duration_seconds.unwrap_or_default(),
                config.storage.bucket,
                record.backup_key.as_deref().unwrap_or_default()
            );
            Ok(())
        }
        Err(error) => {
            record.status = RunStatus::Failed;
            record.error = Some(redact_failure(&error, config, target));
            let state_result = state
                .write_current(&record)
                .and_then(|_| state.append_history(&record));
            ping_healthcheck(target.healthcheck_url.as_deref(), "/fail");
            state_result.context("backup failed and its failure state could not be persisted")?;
            Err(error)
        }
    }
}

pub fn run_scheduled(config: &Config, target: &TargetConfig) -> Result<()> {
    run_with_retries(
        &target.name,
        SCHEDULED_ATTEMPTS,
        SCHEDULED_RETRY_DELAY,
        || run(config, target),
        thread::sleep,
    )
}

fn run_with_retries(
    target_name: &str,
    attempts: u32,
    delay: StdDuration,
    mut operation: impl FnMut() -> Result<()>,
    mut wait: impl FnMut(StdDuration),
) -> Result<()> {
    ensure!(attempts > 0, "scheduled backup attempts must be positive");
    let mut last_error = None;
    for attempt in 1..=attempts {
        match operation() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt < attempts {
            eprintln!(
                "backup {target_name} attempt {attempt}/{attempts} failed; retrying in {}s",
                delay.as_secs()
            );
            wait(delay);
        }
    }
    Err(last_error.expect("at least one scheduled backup attempt ran"))
}

fn redact_failure(error: &anyhow::Error, config: &Config, target: &TargetConfig) -> String {
    let mut message = format!("{error:#}");
    let secrets = [
        Some(target.database_url.as_str()),
        Some(config.storage.access_key_id.as_str()),
        Some(config.storage.secret_access_key.as_str()),
        config.storage.session_token.as_deref(),
        target.healthcheck_url.as_deref(),
    ];
    for secret in secrets
        .into_iter()
        .flatten()
        .filter(|value| !value.is_empty())
    {
        message = message.replace(secret, "<redacted>");
    }
    message
}

fn perform_backup(
    config: &Config,
    target: &TargetConfig,
    state: &StateStore,
    record: &mut RunRecord,
) -> Result<()> {
    let now = Utc::now();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_prefix = now.format("%Y/%m/%d");
    let temporary = tempfile::Builder::new()
        .prefix(&format!("{}.{}.", target.name, timestamp))
        .tempdir_in(&config.defaults.scratch_dir)
        .context("failed to create backup scratch directory")?;
    let encrypted_path = temporary
        .path()
        .join(format!("{}-{}.dump.zst.age", target.name, timestamp));
    let manifest_path = temporary
        .path()
        .join(format!("{}-{}.manifest.json", target.name, timestamp));

    stream_encrypted_dump(target, &encrypted_path)?;

    let encrypted_bytes = fs::metadata(&encrypted_path)?.len();
    let encrypted_sha256 = sha256_file(&encrypted_path)?;
    record.encrypted_bytes = Some(encrypted_bytes);
    record.encrypted_sha256 = Some(encrypted_sha256.clone());
    checkpoint(state, record, RunPhase::Encrypted)?;

    let backup_key = object_key(
        &config.storage,
        &format!(
            "{}/{}/{}-{}.dump.zst.age",
            target.name, date_prefix, target.name, timestamp
        ),
    );
    record.backup_key = Some(backup_key.clone());
    checkpoint(state, record, RunPhase::UploadingBackup)?;
    upload_file(
        &config.storage,
        &encrypted_path,
        &backup_key,
        "application/octet-stream",
    )?;
    checkpoint(state, record, RunPhase::BackupUploaded)?;
    let metadata = head_object(&config.storage, &backup_key)?;
    record.backup_version_id = metadata.version_id.clone();
    checkpoint(state, record, RunPhase::BackupUploaded)?;
    verify_object_metadata(&config.storage, &backup_key, &metadata)?;
    checkpoint(state, record, RunPhase::BackupVerified)?;

    let manifest_key = object_key(
        &config.storage,
        &format!(
            "{}/{}/{}-{}.manifest.json",
            target.name, date_prefix, target.name, timestamp
        ),
    );
    let manifest = BackupManifest {
        schema_version: 1,
        target: &target.name,
        created_at: now,
        format: "PostgreSQL custom dump",
        compression: "zstd",
        encryption: "age X25519/ChaCha20-Poly1305",
        object_key: &backup_key,
        object_version_id: metadata.version_id.as_deref(),
        encrypted_bytes,
        encrypted_sha256: &encrypted_sha256,
    };
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)
        .context("failed to write backup manifest")?;
    record.manifest_key = Some(manifest_key.clone());
    checkpoint(state, record, RunPhase::UploadingManifest)?;
    upload_file(
        &config.storage,
        &manifest_path,
        &manifest_key,
        "application/json",
    )?;
    checkpoint(state, record, RunPhase::ManifestUploaded)?;
    let manifest_metadata = head_object(&config.storage, &manifest_key)?;
    record.manifest_version_id = manifest_metadata.version_id.clone();
    checkpoint(state, record, RunPhase::ManifestUploaded)?;
    verify_object_metadata(&config.storage, &manifest_key, &manifest_metadata)?;
    checkpoint(state, record, RunPhase::ManifestVerified)?;

    Ok(())
}

fn checkpoint(state: &StateStore, record: &mut RunRecord, phase: RunPhase) -> Result<()> {
    record.phase = phase;
    state.write_current(record)
}

fn zstd_stream_command() -> Command {
    let mut command = Command::new("zstd");
    command.args(["--quiet", "--threads=0", "--stdout"]);
    command
}

fn stream_encrypted_dump(target: &TargetConfig, encrypted_path: &Path) -> Result<()> {
    let mut pg_dump = postgres_command("pg_dump", target)?;
    pg_dump
        .args(["--format=custom", "--compress=0", "--no-password"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut pg_dump = pg_dump.spawn().context("failed to start pg_dump")?;

    let Some(pg_dump_stdout) = pg_dump.stdout.take() else {
        terminate_and_wait(&mut pg_dump);
        bail!("pg_dump stdout is unavailable");
    };
    let mut zstd = zstd_stream_command();
    zstd.stdin(Stdio::from(pg_dump_stdout))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut zstd = match zstd.spawn().context("failed to start zstd") {
        Ok(child) => child,
        Err(error) => {
            terminate_and_wait(&mut pg_dump);
            return Err(error);
        }
    };

    let Some(zstd_stdout) = zstd.stdout.take() else {
        terminate_and_wait(&mut zstd);
        terminate_and_wait(&mut pg_dump);
        bail!("zstd stdout is unavailable");
    };
    let mut age = Command::new("age");
    age.args(["--recipient", &target.age_recipient, "--output"])
        .arg(encrypted_path)
        .stdin(Stdio::from(zstd_stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let age = match age.spawn().context("failed to start age") {
        Ok(child) => child,
        Err(error) => {
            terminate_and_wait(&mut zstd);
            terminate_and_wait(&mut pg_dump);
            return Err(error);
        }
    };

    let pg_dump_wait = thread::spawn(move || pg_dump.wait_with_output());
    let zstd_wait = thread::spawn(move || zstd.wait_with_output());
    let age_output = age.wait_with_output().context("failed to wait for age");
    let zstd_output = join_child(zstd_wait, "zstd");
    let pg_dump_output = join_child(pg_dump_wait, "pg_dump");

    let age_output = age_output?;
    let zstd_output = zstd_output?;
    let pg_dump_output = pg_dump_output?;

    check_output(&age_output, "age")?;
    check_output(&zstd_output, "zstd")?;
    check_output(&pg_dump_output, "pg_dump")?;
    Ok(())
}

fn join_child(handle: thread::JoinHandle<std::io::Result<Output>>, label: &str) -> Result<Output> {
    handle
        .join()
        .map_err(|_| anyhow::anyhow!("{label} wait thread panicked"))?
        .with_context(|| format!("failed to wait for {label}"))
}

fn terminate_and_wait(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub fn postgres_command(program: &str, target: &TargetConfig) -> Result<Command> {
    let mut database_url = validated_database_url(&target.database_url)
        .context("target database_url is not a valid safe PostgreSQL URL")?;
    let password = database_url
        .password()
        .map(|value| {
            percent_decode_str(value)
                .decode_utf8()
                .context("target database_url password is not valid UTF-8")
                .map(|value| value.into_owned())
        })
        .transpose()?;
    database_url
        .set_password(None)
        .map_err(|_| anyhow::anyhow!("failed to remove password from target database_url"))?;

    let mut command = Command::new(program);
    command.arg("--dbname").arg(database_url.as_str());
    if let Some(password) = password {
        command.env("PGPASSWORD", password);
    }
    Ok(command)
}

pub fn verify_database_dump_access(target: &TargetConfig) -> Result<()> {
    let mut pg_dump = postgres_command("pg_dump", target)?;
    pg_dump.args([
        "--format=custom",
        "--compress=0",
        "--no-password",
        "--file",
        "/dev/null",
    ]);
    run_checked(&mut pg_dump, "pg_dump canary")?;
    Ok(())
}

pub fn upload_storage_canary(config: &Config) -> Result<(String, String)> {
    fs::create_dir_all(&config.defaults.scratch_dir)?;
    let mut marker = tempfile::Builder::new()
        .prefix("doctor-canary.")
        .tempfile_in(&config.defaults.scratch_dir)
        .context("failed to create S3 canary marker")?;
    marker.write_all(b"silo-keeper doctor write canary\n")?;
    marker.as_file().sync_all()?;

    let timestamp = Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
    let key = object_key(
        &config.storage,
        &format!("_doctor/{timestamp}-{}.canary", std::process::id()),
    );
    upload_file(
        &config.storage,
        marker.path(),
        &key,
        "application/octet-stream",
    )?;
    let metadata = head_object(&config.storage, &key)?;
    verify_object_metadata(&config.storage, &key, &metadata)?;
    let version_id = metadata
        .version_id
        .context("verified S3 canary has no VersionId")?;
    Ok((key, version_id))
}

fn upload_file(
    storage: &StorageConfig,
    source: &Path,
    key: &str,
    content_type: &str,
) -> Result<()> {
    let mut command = aws_command(storage);
    command
        .args(["s3", "cp"])
        .arg(source)
        .arg(format!("s3://{}/{key}", storage.bucket))
        .args([
            "--only-show-errors",
            "--checksum-algorithm",
            "SHA256",
            "--content-type",
            content_type,
        ]);
    run_checked(&mut command, "aws s3 cp")?;
    Ok(())
}

fn head_object(storage: &StorageConfig, key: &str) -> Result<ObjectMetadata> {
    let mut command = aws_command(storage);
    command.args([
        "s3api",
        "head-object",
        "--bucket",
        &storage.bucket,
        "--key",
        key,
        "--output",
        "json",
    ]);
    let output = run_checked(&mut command, "aws s3api head-object")?;
    serde_json::from_slice(&output.stdout).context("invalid S3 head-object response")
}

fn verify_object_metadata(
    storage: &StorageConfig,
    key: &str,
    metadata: &ObjectMetadata,
) -> Result<()> {
    ensure!(
        metadata
            .version_id
            .as_deref()
            .is_some_and(|value| !value.is_empty()),
        "S3 versioning is required but object {key} has no VersionId"
    );
    if storage.object_lock_days == 0 {
        return Ok(());
    }
    let mode = metadata.object_lock_mode.as_deref().unwrap_or_default();
    ensure!(
        matches!(mode, "COMPLIANCE" | "GOVERNANCE"),
        "object {key} has no S3 Object Lock retention"
    );
    let retain_until = metadata
        .object_lock_retain_until_date
        .as_deref()
        .context("S3 Object Lock retention date is missing")?
        .parse::<DateTime<Utc>>()
        .context("invalid S3 Object Lock retention date")?;
    let minimum =
        Utc::now() + Duration::days(i64::from(storage.object_lock_days)) - Duration::hours(1);
    ensure!(
        retain_until >= minimum,
        "object {key} retention is shorter than {} days",
        storage.object_lock_days
    );
    Ok(())
}

pub fn aws_command(storage: &StorageConfig) -> Command {
    let mut command = Command::new("aws");
    if let Some(endpoint) = &storage.endpoint {
        command.args(["--endpoint-url", endpoint]);
    }
    command
        .env("AWS_ACCESS_KEY_ID", &storage.access_key_id)
        .env("AWS_SECRET_ACCESS_KEY", &storage.secret_access_key)
        .env("AWS_DEFAULT_REGION", &storage.region)
        .env("AWS_PAGER", "");
    if let Some(token) = &storage.session_token {
        command.env("AWS_SESSION_TOKEN", token);
    }
    command
}

pub fn run_checked(command: &mut Command, label: &str) -> Result<Output> {
    let output = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .with_context(|| format!("failed to start {label}"))?;
    check_output(&output, label)?;
    Ok(output)
}

fn check_output(output: &Output, label: &str) -> Result<()> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        bail!(
            "{label} failed with {}{}",
            output.status,
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(())
}

fn ping_healthcheck(base_url: Option<&str>, suffix: &str) {
    let Some(base_url) = base_url else {
        return;
    };
    let _ = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            "--retry",
            "2",
            &format!("{base_url}{suffix}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn object_key(storage: &StorageConfig, suffix: &str) -> String {
    let prefix = storage.prefix.trim_matches('/');
    if prefix.is_empty() {
        suffix.to_owned()
    } else {
        format!("{prefix}/{suffix}")
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config() -> Config {
        Config::parse(
            r#"
version = 1
[storage]
type = "s3"
bucket = "backups"
region = "eu-central-1"
prefix = "/production/"
access_key_id = "test"
secret_access_key = "secret"

[[targets]]
name = "db"
type = "postgres"
database_url = "postgres://localhost/db"
on_calendar = "daily"
age_recipient = "age1test"
"#,
        )
        .unwrap()
    }

    #[test]
    fn object_keys_are_canonical() {
        assert_eq!(
            object_key(&config().storage, "db/file"),
            "production/db/file"
        );
    }

    #[test]
    fn object_lock_can_be_optional_but_versioning_cannot() {
        let storage = config().storage;
        let metadata = ObjectMetadata {
            version_id: Some("version-1".to_owned()),
            object_lock_mode: None,
            object_lock_retain_until_date: None,
        };
        verify_object_metadata(&storage, "key", &metadata).unwrap();
        let missing_version = ObjectMetadata {
            version_id: None,
            ..metadata
        };
        assert!(verify_object_metadata(&storage, "key", &missing_version).is_err());
    }

    #[test]
    fn persisted_failures_redact_credentials_and_urls() {
        let config = config();
        let target = &config.targets[0];
        let error = anyhow::anyhow!(
            "failed for {} with {} and {}",
            target.database_url,
            config.storage.access_key_id,
            config.storage.secret_access_key
        );
        let message = redact_failure(&error, &config, target);
        assert!(!message.contains(&target.database_url));
        assert!(!message.contains(&config.storage.access_key_id));
        assert!(!message.contains(&config.storage.secret_access_key));
        assert_eq!(
            message,
            "failed for <redacted> with <redacted> and <redacted>"
        );
    }

    #[test]
    fn postgres_commands_keep_passwords_out_of_process_arguments() {
        let mut config = config();
        config.targets[0].database_url =
            "postgresql://backup:p%40ssword@127.0.0.1:5432/production".to_owned();
        let command = postgres_command("pg_dump", &config.targets[0]).unwrap();
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(
            arguments,
            ["--dbname", "postgresql://backup@127.0.0.1:5432/production"]
        );
        let password = command
            .get_envs()
            .find(|(name, _)| *name == "PGPASSWORD")
            .and_then(|(_, value)| value)
            .unwrap();
        assert_eq!(password, "p@ssword");
    }

    #[test]
    fn postgres_commands_reject_password_query_parameters() {
        let mut config = config();
        config.targets[0].database_url =
            "postgresql://backup@127.0.0.1/production?password=secret".to_owned();
        let error = postgres_command("pg_dump", &config.targets[0]).unwrap_err();
        assert!(error.to_string().contains("safe PostgreSQL URL"));
    }

    #[test]
    fn zstd_command_streams_to_stdout() {
        let command = zstd_stream_command();
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy())
            .collect::<Vec<_>>();
        assert_eq!(arguments, ["--quiet", "--threads=0", "--stdout"]);
    }

    #[test]
    fn backup_upload_checkpoint_preserves_orphan_recovery_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::new(directory.path());
        let mut record = RunRecord::running("db");
        record.backup_key = Some("production/db/backup.age".to_owned());
        record.backup_version_id = Some("version-1".to_owned());
        record.encrypted_bytes = Some(42);
        record.encrypted_sha256 = Some("abc123".to_owned());
        checkpoint(&state, &mut record, RunPhase::BackupVerified).unwrap();

        let persisted = state.current("db").unwrap().unwrap();
        assert_eq!(persisted.phase, RunPhase::BackupVerified);
        assert_eq!(persisted.backup_version_id.as_deref(), Some("version-1"));
        assert_eq!(persisted.encrypted_bytes, Some(42));
    }

    #[test]
    fn scheduled_retries_are_bounded_and_stop_after_success() {
        let mut attempts = 0;
        let mut waits = Vec::new();
        run_with_retries(
            "db",
            4,
            StdDuration::from_secs(10),
            || {
                attempts += 1;
                if attempts < 3 {
                    bail!("transient failure")
                }
                Ok(())
            },
            |delay| waits.push(delay),
        )
        .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(waits, [StdDuration::from_secs(10); 2]);

        let mut exhausted_attempts = 0;
        let error = run_with_retries(
            "db",
            4,
            StdDuration::ZERO,
            || {
                exhausted_attempts += 1;
                bail!("still failing")
            },
            |_| {},
        )
        .unwrap_err();
        assert_eq!(exhausted_attempts, 4);
        assert_eq!(error.to_string(), "still failing");
    }
}
