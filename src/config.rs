use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

pub const INSTALLED_CONFIG_PATH: &str = "/etc/moat-silo/config.toml";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub version: u32,
    pub storage: StorageConfig,
    #[serde(default)]
    pub defaults: DefaultsConfig,
    pub targets: Vec<TargetConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    #[serde(rename = "type")]
    pub kind: StorageKind,
    pub bucket: String,
    pub region: String,
    pub endpoint: Option<String>,
    #[serde(default = "default_storage_prefix")]
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    #[serde(default)]
    pub object_lock_days: u32,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum StorageKind {
    S3,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DefaultsConfig {
    pub state_dir: PathBuf,
    pub scratch_dir: PathBuf,
    pub randomized_delay_seconds: u64,
    pub max_backup_age_hours: i64,
}

impl Default for DefaultsConfig {
    fn default() -> Self {
        Self {
            state_dir: PathBuf::from("/var/lib/moat-silo"),
            scratch_dir: PathBuf::from("/var/lib/moat-silo/tmp"),
            randomized_delay_seconds: 900,
            max_backup_age_hours: 36,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: TargetKind,
    pub database_url: String,
    pub on_calendar: String,
    pub age_recipient: String,
    pub healthcheck_url: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub max_backup_age_hours: Option<i64>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TargetKind {
    Postgres,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        ensure_secure_config(path, true)?;
        Self::read(path)
    }

    pub fn load_install_source(path: &Path) -> Result<Self> {
        ensure_secure_config(path, false)?;
        Self::read(path)
    }

    fn read(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        Self::parse(&raw)
    }

    pub fn parse(raw: &str) -> Result<Self> {
        let config: Self = toml::from_str(raw).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        match self.storage.kind {
            StorageKind::S3 => {}
        }
        ensure!(
            self.version == 1,
            "unsupported configuration version {}",
            self.version
        );
        ensure!(
            !self.storage.bucket.trim().is_empty(),
            "storage.bucket is required"
        );
        ensure!(
            !self.storage.region.trim().is_empty(),
            "storage.region is required"
        );
        ensure!(
            !contains_line_break(&self.storage.region),
            "storage.region contains a line break"
        );
        ensure!(
            !contains_line_break(&self.storage.prefix),
            "storage.prefix contains a line break"
        );
        ensure!(
            !self.storage.access_key_id.trim().is_empty(),
            "storage.access_key_id is required"
        );
        ensure!(
            !self.storage.secret_access_key.is_empty(),
            "storage.secret_access_key is required"
        );
        if let Some(endpoint) = &self.storage.endpoint {
            ensure!(
                endpoint.starts_with("https://"),
                "storage.endpoint must use HTTPS"
            );
            ensure!(
                !contains_line_break(endpoint),
                "storage.endpoint contains a line break"
            );
        }
        ensure!(
            self.defaults.state_dir.is_absolute(),
            "defaults.state_dir must be absolute"
        );
        ensure!(
            is_safe_systemd_path(&self.defaults.state_dir),
            "defaults.state_dir contains characters unsupported by the systemd installer"
        );
        ensure!(
            self.defaults
                .state_dir
                .starts_with(Path::new("/var/lib/moat-silo")),
            "defaults.state_dir must be /var/lib/moat-silo or one of its subdirectories"
        );
        ensure!(
            self.defaults.scratch_dir.is_absolute(),
            "defaults.scratch_dir must be absolute"
        );
        ensure!(
            is_safe_systemd_path(&self.defaults.scratch_dir),
            "defaults.scratch_dir contains characters unsupported by the systemd installer"
        );
        ensure!(
            self.defaults
                .scratch_dir
                .starts_with(&self.defaults.state_dir),
            "defaults.scratch_dir must be inside defaults.state_dir"
        );
        ensure!(
            self.defaults.max_backup_age_hours > 0,
            "defaults.max_backup_age_hours must be positive"
        );
        ensure!(!self.targets.is_empty(), "at least one target is required");

        let mut names = HashSet::new();
        for target in &self.targets {
            match target.kind {
                TargetKind::Postgres => {}
            }
            ensure!(
                is_safe_name(&target.name),
                "invalid target name {:?}",
                target.name
            );
            ensure!(
                names.insert(&target.name),
                "duplicate target name {}",
                target.name
            );
            ensure!(
                target.database_url.starts_with("postgres://")
                    || target.database_url.starts_with("postgresql://"),
                "target {} database_url must be a PostgreSQL URL",
                target.name
            );
            ensure!(
                !contains_line_break(&target.database_url),
                "target {} database_url contains a line break",
                target.name
            );
            ensure!(
                !target.on_calendar.trim().is_empty() && !contains_line_break(&target.on_calendar),
                "target {} on_calendar is invalid",
                target.name
            );
            ensure!(
                target.age_recipient.starts_with("age1")
                    && !contains_line_break(&target.age_recipient),
                "target {} age_recipient is invalid",
                target.name
            );
            if let Some(url) = &target.healthcheck_url {
                ensure!(
                    url.starts_with("https://") && !contains_line_break(url),
                    "target {} healthcheck_url must use HTTPS",
                    target.name
                );
            }
            if let Some(max_age) = target.max_backup_age_hours {
                ensure!(
                    max_age > 0,
                    "target {} max_backup_age_hours must be positive",
                    target.name
                );
            }
        }
        Ok(())
    }

    pub fn target(&self, name: &str) -> Result<&TargetConfig> {
        self.targets
            .iter()
            .find(|target| target.name == name)
            .with_context(|| format!("unknown backup target {name}"))
    }

    pub fn max_age_hours(&self, target: &TargetConfig) -> i64 {
        target
            .max_backup_age_hours
            .unwrap_or(self.defaults.max_backup_age_hours)
    }
}

pub fn ensure_secure_config(path: &Path, require_root_owner: bool) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to inspect configuration {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "configuration is not a regular file: {}",
        path.display()
    );
    let mode = metadata.permissions().mode() & 0o777;
    ensure!(
        mode & 0o077 == 0,
        "configuration {} must not be accessible by group or other users (current mode {:03o})",
        path.display(),
        mode
    );
    if require_root_owner && unsafe { libc::geteuid() } == 0 {
        ensure!(
            metadata.uid() == 0,
            "configuration {} must be owned by root",
            path.display()
        );
    }
    Ok(())
}

fn is_safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn contains_line_break(value: &str) -> bool {
    value.contains(['\n', '\r'])
}

fn is_safe_systemd_path(path: &Path) -> bool {
    use std::path::Component;

    path.components()
        .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && path
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
}

fn default_true() -> bool {
    true
}

fn default_storage_prefix() -> String {
    "moat-silo".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1

[storage]
type = "s3"
bucket = "backups"
region = "eu-central-1"
access_key_id = "test"
secret_access_key = "secret"
object_lock_days = 14

[[targets]]
name = "production-db"
type = "postgres"
database_url = "postgres://backup:secret@localhost/production"
on_calendar = "*-*-* 02:15:00 UTC"
age_recipient = "age1test"
"#;

    #[test]
    fn parses_valid_configuration() {
        let config = Config::parse(VALID).unwrap();
        assert_eq!(config.targets[0].name, "production-db");
        assert_eq!(config.defaults.max_backup_age_hours, 36);
    }

    #[test]
    fn rejects_duplicate_or_unsafe_targets() {
        let duplicate = format!(
            "{VALID}\n[[targets]]\nname = \"production-db\"\ntype = \"postgres\"\ndatabase_url = \"postgres://localhost/db\"\non_calendar = \"daily\"\nage_recipient = \"age1test\"\n"
        );
        assert!(
            Config::parse(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let unsafe_name = VALID.replace("production-db", "production/db");
        assert!(
            Config::parse(&unsafe_name)
                .unwrap_err()
                .to_string()
                .contains("invalid target name")
        );
    }

    #[test]
    fn rejects_unknown_fields_and_insecure_endpoints() {
        assert!(Config::parse(&VALID.replace("bucket =", "unknown = true\nbucket =")).is_err());
        let insecure = VALID.replace(
            "region = \"eu-central-1\"",
            "region = \"eu-central-1\"\nendpoint = \"http://s3.invalid\"",
        );
        assert!(
            Config::parse(&insecure)
                .unwrap_err()
                .to_string()
                .contains("HTTPS")
        );
    }

    #[test]
    fn rejects_dangerous_state_paths_and_insecure_healthchecks() {
        let dangerous_state = VALID.replace(
            "[[targets]]",
            "[defaults]\nstate_dir = \"/var/lib/moat-silo/../..\"\n\n[[targets]]",
        );
        assert!(Config::parse(&dangerous_state).is_err());

        let external_scratch = VALID.replace(
            "[[targets]]",
            "[defaults]\nscratch_dir = \"/tmp/moat-silo\"\n\n[[targets]]",
        );
        assert!(Config::parse(&external_scratch).is_err());

        let insecure_healthcheck = VALID.replace(
            "age_recipient = \"age1test\"",
            "age_recipient = \"age1test\"\nhealthcheck_url = \"http://hc-ping.com/test\"",
        );
        assert!(
            Config::parse(&insecure_healthcheck)
                .unwrap_err()
                .to_string()
                .contains("HTTPS")
        );
    }
}
