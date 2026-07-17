use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Duration, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::{Config, StorageConfig, TargetConfig};
use crate::state::{RunRecord, RunStatus, StateStore};

#[derive(Debug)]
struct BackupOutcome {
    backup_key: String,
    manifest_key: String,
    backup_version_id: Option<String>,
    encrypted_bytes: u64,
    encrypted_sha256: String,
}

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

    let outcome = perform_backup(config, target);
    let finished_at = Utc::now();
    record.finished_at = Some(finished_at);
    record.duration_seconds = Some(started.elapsed().as_secs());

    match outcome {
        Ok(outcome) => {
            record.status = RunStatus::Success;
            record.backup_key = Some(outcome.backup_key);
            record.manifest_key = Some(outcome.manifest_key);
            record.backup_version_id = outcome.backup_version_id;
            record.encrypted_bytes = Some(outcome.encrypted_bytes);
            record.encrypted_sha256 = Some(outcome.encrypted_sha256);
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

fn perform_backup(config: &Config, target: &TargetConfig) -> Result<BackupOutcome> {
    let now = Utc::now();
    let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let date_prefix = now.format("%Y/%m/%d");
    let temporary = tempfile::Builder::new()
        .prefix(&format!("{}.{}.", target.name, timestamp))
        .tempdir_in(&config.defaults.scratch_dir)
        .context("failed to create backup scratch directory")?;
    let dump_path = temporary.path().join(format!("{}.dump", target.name));
    let compressed_path = temporary.path().join(format!("{}.dump.zst", target.name));
    let encrypted_path = temporary
        .path()
        .join(format!("{}-{}.dump.zst.age", target.name, timestamp));
    let manifest_path = temporary
        .path()
        .join(format!("{}-{}.manifest.json", target.name, timestamp));

    let mut pg_dump = Command::new("pg_dump");
    pg_dump
        .env("PGDATABASE", &target.database_url)
        .args(["--format=custom", "--compress=0", "--file"])
        .arg(&dump_path);
    run_checked(&mut pg_dump, "pg_dump")?;

    let mut zstd = Command::new("zstd");
    zstd.args(["--quiet", "--threads=0", "--force"])
        .arg(&dump_path)
        .arg("--output")
        .arg(&compressed_path);
    run_checked(&mut zstd, "zstd")?;
    fs::remove_file(&dump_path).context("failed to remove plaintext dump")?;

    let mut age = Command::new("age");
    age.args(["--recipient", &target.age_recipient, "--output"])
        .arg(&encrypted_path)
        .arg(&compressed_path);
    run_checked(&mut age, "age")?;
    fs::remove_file(&compressed_path).context("failed to remove compressed plaintext dump")?;

    let encrypted_bytes = fs::metadata(&encrypted_path)?.len();
    let encrypted_sha256 = sha256_file(&encrypted_path)?;
    let backup_key = object_key(
        &config.storage,
        &format!(
            "{}/{}/{}-{}.dump.zst.age",
            target.name, date_prefix, target.name, timestamp
        ),
    );
    upload_file(
        &config.storage,
        &encrypted_path,
        &backup_key,
        "application/octet-stream",
    )?;
    let metadata = head_object(&config.storage, &backup_key)?;
    verify_object_metadata(&config.storage, &backup_key, &metadata)?;

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
    upload_file(
        &config.storage,
        &manifest_path,
        &manifest_key,
        "application/json",
    )?;
    let manifest_metadata = head_object(&config.storage, &manifest_key)?;
    verify_object_metadata(&config.storage, &manifest_key, &manifest_metadata)?;

    Ok(BackupOutcome {
        backup_key,
        manifest_key,
        backup_version_id: metadata.version_id,
        encrypted_bytes,
        encrypted_sha256,
    })
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
    Ok(output)
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

fn sha256_file(path: &PathBuf) -> Result<String> {
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
}
