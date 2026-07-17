use std::process::{Command, Stdio};

use anyhow::{Result, ensure};
use chrono::Utc;
use serde::Serialize;

use crate::config::{Config, TargetConfig};
use crate::state::{RunPhase, RunRecord, RunStatus, StateStore};
use crate::util::command_exists;

#[derive(Debug, Serialize)]
pub struct TargetStatus {
    target: String,
    state: String,
    timer: String,
    last_attempt: Option<RunRecord>,
    last_success: Option<RunRecord>,
}

pub fn show(config: &Config, target_name: Option<&str>, json: bool) -> Result<()> {
    let targets = selected_targets(config, target_name)?;
    let state = StateStore::new(&config.defaults.state_dir);
    let mut statuses = Vec::with_capacity(targets.len());
    for target in targets {
        let current = state.current(&target.name)?;
        let last_success = state.latest_success(&target.name)?;
        let computed_state = backup_state(
            config,
            target,
            state.is_lock_held(&target.name)?,
            current.as_ref(),
            last_success.as_ref(),
        );
        statuses.push(TargetStatus {
            target: target.name.clone(),
            state: computed_state,
            timer: timer_state(&target.name, target.enabled),
            last_attempt: current,
            last_success,
        });
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&statuses)?);
        return Ok(());
    }

    println!(
        "{:<24} {:<10} {:<10} {:<22} {:>10}",
        "TARGET", "STATE", "TIMER", "LAST SUCCESS", "SIZE"
    );
    for status in statuses {
        let last_success = status
            .last_success
            .as_ref()
            .map(|record| record.started_at.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "-".to_owned());
        let size = status
            .last_success
            .as_ref()
            .and_then(|record| record.encrypted_bytes)
            .map(human_bytes)
            .unwrap_or_else(|| "-".to_owned());
        println!(
            "{:<24} {:<10} {:<10} {:<22} {:>10}",
            status.target, status.state, status.timer, last_success, size
        );
        if let Some(record) = status
            .last_attempt
            .as_ref()
            .filter(|record| record.status == RunStatus::Failed)
        {
            if let Some(error) = record.error.as_deref() {
                println!("  last error: {error}");
            }
            if matches!(
                record.phase,
                RunPhase::BackupUploaded
                    | RunPhase::BackupVerified
                    | RunPhase::UploadingManifest
                    | RunPhase::ManifestUploaded
                    | RunPhase::ManifestVerified
            ) && let Some(key) = record.backup_key.as_deref()
            {
                println!(
                    "  recoverable backup: s3://{}/{} (VersionId {})",
                    config.storage.bucket,
                    key,
                    record.backup_version_id.as_deref().unwrap_or("unknown")
                );
            }
        }
    }
    Ok(())
}

pub fn history(config: &Config, target_name: &str, limit: usize, json: bool) -> Result<()> {
    config.target(target_name)?;
    ensure!(
        limit > 0 && limit <= 1_000,
        "history limit must be between 1 and 1000"
    );
    let records = StateStore::new(&config.defaults.state_dir).history(target_name, limit)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
        return Ok(());
    }
    if records.is_empty() {
        println!("no backup history for {target_name}");
        return Ok(());
    }
    for record in records {
        println!(
            "{} {:<7} {:>6}s {:>10} {}",
            record.started_at.format("%Y-%m-%d %H:%M:%S UTC"),
            format!("{:?}", record.status).to_lowercase(),
            record.duration_seconds.unwrap_or_default(),
            record
                .encrypted_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "-".to_owned()),
            record.backup_key.as_deref().unwrap_or("-")
        );
        if let Some(error) = record.error {
            println!("  error: {error}");
        }
    }
    Ok(())
}

fn selected_targets<'a>(config: &'a Config, name: Option<&str>) -> Result<Vec<&'a TargetConfig>> {
    match name {
        Some(name) => Ok(vec![config.target(name)?]),
        None => Ok(config.targets.iter().collect()),
    }
}

fn backup_state(
    config: &Config,
    target: &TargetConfig,
    lock_held: bool,
    current: Option<&RunRecord>,
    last_success: Option<&RunRecord>,
) -> String {
    if lock_held {
        return "running".to_owned();
    }
    if let Some(record) = current {
        if record.status == RunStatus::Failed {
            return "failed".to_owned();
        }
        if record.status == RunStatus::Running {
            return "orphaned".to_owned();
        }
    }
    let Some(last_success) = last_success else {
        return "never".to_owned();
    };
    let age = Utc::now() - last_success.started_at;
    if age.num_hours() > config.max_age_hours(target) {
        "stale".to_owned()
    } else {
        "healthy".to_owned()
    }
}

fn timer_state(target: &str, enabled: bool) -> String {
    if !enabled {
        return "disabled".to_owned();
    }
    if !command_exists("systemctl") {
        return "unknown".to_owned();
    }
    let unit = format!("silo-keeper@{target}.timer");
    match Command::new("systemctl")
        .args(["is-active", "--quiet", &unit])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => "active".to_owned(),
        Ok(_) => "inactive".to_owned(),
        Err(_) => "unknown".to_owned(),
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_backup_sizes() {
        assert_eq!(human_bytes(1_073_741_824), "1.0 GiB");
        assert_eq!(human_bytes(42), "42 B");
    }
}
