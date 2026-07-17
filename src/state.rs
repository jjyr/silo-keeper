use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Success,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunPhase {
    #[default]
    Preparing,
    Encrypted,
    UploadingBackup,
    BackupUploaded,
    BackupVerified,
    UploadingManifest,
    ManifestUploaded,
    ManifestVerified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub target: String,
    pub status: RunStatus,
    #[serde(default)]
    pub phase: RunPhase,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_seconds: Option<u64>,
    pub pid: u32,
    pub backup_key: Option<String>,
    pub manifest_key: Option<String>,
    pub backup_version_id: Option<String>,
    pub manifest_version_id: Option<String>,
    pub encrypted_bytes: Option<u64>,
    pub encrypted_sha256: Option<String>,
    pub error: Option<String>,
}

impl RunRecord {
    pub fn running(target: &str) -> Self {
        Self {
            target: target.to_owned(),
            status: RunStatus::Running,
            phase: RunPhase::Preparing,
            started_at: Utc::now(),
            finished_at: None,
            duration_seconds: None,
            pid: std::process::id(),
            backup_key: None,
            manifest_key: None,
            backup_version_id: None,
            manifest_version_id: None,
            encrypted_bytes: None,
            encrypted_sha256: None,
            error: None,
        }
    }
}

pub struct StateStore {
    root: PathBuf,
}

impl StateStore {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
        }
    }

    pub fn ensure_layout(&self) -> Result<()> {
        for path in [self.current_dir(), self.history_dir(), self.lock_dir()] {
            fs::create_dir_all(&path)
                .with_context(|| format!("failed to create state directory {}", path.display()))?;
        }
        Ok(())
    }

    pub fn open_lock(&self, target: &str) -> Result<File> {
        self.ensure_layout()?;
        let path = self.lock_dir().join(format!("{target}.lock"));
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open backup lock {}", path.display()))
    }

    pub fn write_current(&self, record: &RunRecord) -> Result<()> {
        self.ensure_layout()?;
        atomic_json_write(
            &self.current_dir().join(format!("{}.json", record.target)),
            record,
        )
    }

    pub fn append_history(&self, record: &RunRecord) -> Result<()> {
        self.ensure_layout()?;
        let target_dir = self.history_dir().join(&record.target);
        fs::create_dir_all(&target_dir).with_context(|| {
            format!(
                "failed to create history directory {}",
                target_dir.display()
            )
        })?;
        let timestamp = record.started_at.format("%Y%m%dT%H%M%S%.3fZ");
        atomic_json_write(&target_dir.join(format!("{timestamp}.json")), record)
    }

    pub fn current(&self, target: &str) -> Result<Option<RunRecord>> {
        read_optional_json(&self.current_dir().join(format!("{target}.json")))
    }

    pub fn history(&self, target: &str, limit: usize) -> Result<Vec<RunRecord>> {
        let directory = self.history_dir().join(target);
        if !directory.exists() {
            return Ok(Vec::new());
        }
        let mut paths = fs::read_dir(&directory)
            .with_context(|| format!("failed to read history directory {}", directory.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect::<Vec<_>>();
        paths.sort_by(|left, right| right.cmp(left));
        paths
            .into_iter()
            .take(limit)
            .map(|path| read_json(&path))
            .collect()
    }

    pub fn latest_success(&self, target: &str) -> Result<Option<RunRecord>> {
        Ok(self
            .history(target, 1_000)?
            .into_iter()
            .find(|record| record.status == RunStatus::Success))
    }

    pub fn is_lock_held(&self, target: &str) -> Result<bool> {
        let path = self.lock_dir().join(format!("{target}.lock"));
        if !path.exists() {
            return Ok(false);
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to inspect backup lock {}", path.display()))?;
        match lock.try_lock_exclusive() {
            Ok(()) => {
                lock.unlock()?;
                Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(true),
            Err(error) => Err(error)
                .with_context(|| format!("failed to inspect backup lock {}", path.display())),
        }
    }

    fn current_dir(&self) -> PathBuf {
        self.root.join("current")
    }

    fn history_dir(&self) -> PathBuf {
        self.root.join("history")
    }

    fn lock_dir(&self) -> PathBuf {
        self.root.join("locks")
    }
}

fn atomic_json_write(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("state path has no parent")?;
    fs::create_dir_all(parent)?;
    let prefix = format!(
        ".{}.tmp-",
        path.file_name()
            .context("state path has no file name")?
            .to_string_lossy()
    );
    let mut temporary = tempfile::Builder::new()
        .prefix(&prefix)
        .tempfile_in(parent)
        .with_context(|| {
            format!(
                "failed to create temporary state file in {}",
                parent.display()
            )
        })?;
    let bytes = serde_json::to_vec_pretty(value)?;
    temporary.write_all(&bytes)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .with_context(|| format!("failed to publish state file {}", path.display()))?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn read_optional_json(path: &Path) -> Result<Option<RunRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    read_json(path).map(Some)
}

fn read_json(path: &Path) -> Result<RunRecord> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("invalid state file {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_current_and_history_without_secrets() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::new(directory.path());
        let mut record = RunRecord::running("production-db");
        record.status = RunStatus::Success;
        record.finished_at = Some(Utc::now());
        state.write_current(&record).unwrap();
        state.append_history(&record).unwrap();

        assert_eq!(
            state.current("production-db").unwrap().unwrap().status,
            RunStatus::Success
        );
        assert_eq!(
            state
                .latest_success("production-db")
                .unwrap()
                .unwrap()
                .target,
            "production-db"
        );
    }

    #[test]
    fn detects_held_backup_lock() {
        let directory = tempfile::tempdir().unwrap();
        let state = StateStore::new(directory.path());
        assert!(!state.is_lock_held("production-db").unwrap());

        let lock = state.open_lock("production-db").unwrap();
        lock.try_lock_exclusive().unwrap();
        assert!(state.is_lock_held("production-db").unwrap());
        lock.unlock().unwrap();
        assert!(!state.is_lock_held("production-db").unwrap());
    }

    #[test]
    fn old_records_default_to_preparing_phase() {
        let record = RunRecord::running("production-db");
        let mut value = serde_json::to_value(record).unwrap();
        value.as_object_mut().unwrap().remove("phase");
        value.as_object_mut().unwrap().remove("manifest_version_id");

        let restored: RunRecord = serde_json::from_value(value).unwrap();
        assert_eq!(restored.phase, RunPhase::Preparing);
        assert_eq!(restored.manifest_version_id, None);
    }
}
