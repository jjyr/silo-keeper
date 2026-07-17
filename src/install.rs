use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};

use crate::backup::run_checked;
use crate::config::{Config, INSTALLED_CONFIG_PATH};
use crate::util::command_exists;

const INSTALLED_BINARY: &str = "/usr/local/bin/moat-silo";
const SYSTEMD_DIR: &str = "/etc/systemd/system";
const SERVICE_UNIT: &str = "moat-silo@.service";

pub fn install(config: &Config, source_config: &Path, install_dependencies: bool) -> Result<()> {
    require_root()?;
    ensure!(
        cfg!(target_os = "linux"),
        "installation is supported only on Linux"
    );
    ensure!(
        command_exists("systemctl"),
        "systemctl is required for installation"
    );
    if install_dependencies {
        install_missing_dependencies(config)?;
    }
    ensure_runtime_dependencies(config)?;

    install_binary()?;
    copy_private_file(source_config, Path::new(INSTALLED_CONFIG_PATH), 0o600)?;
    create_private_directory(&config.defaults.state_dir)?;
    create_private_directory(&config.defaults.scratch_dir)?;

    for unit in installed_timer_units()? {
        let _ = Command::new("systemctl")
            .args(["disable", "--now", &unit])
            .status();
        let path = Path::new(SYSTEMD_DIR).join(&unit);
        if path.exists() {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove stale timer {}", path.display()))?;
        }
    }

    let service = service_unit(config);
    write_unit(&Path::new(SYSTEMD_DIR).join(SERVICE_UNIT), &service)?;

    let mut enabled_units = Vec::new();
    for target in config.targets.iter().filter(|target| target.enabled) {
        let unit_name = format!("moat-silo@{}.timer", target.name);
        write_unit(
            &Path::new(SYSTEMD_DIR).join(&unit_name),
            &timer_unit(config, target),
        )?;
        enabled_units.push(unit_name);
    }

    run_systemctl(["daemon-reload"])?;
    if !enabled_units.is_empty() {
        let mut command = Command::new("systemctl");
        command.args(["enable", "--now"]);
        command.args(&enabled_units);
        run_checked(&mut command, "systemctl enable --now")?;
    }

    println!(
        "installed moat-silo with {} enabled target(s)",
        enabled_units.len()
    );
    println!("configuration: {INSTALLED_CONFIG_PATH}");
    println!("status: sudo moat-silo status");
    Ok(())
}

pub fn uninstall(purge_local: bool) -> Result<()> {
    require_root()?;
    ensure!(
        cfg!(target_os = "linux"),
        "uninstall is supported only on Linux"
    );

    for unit in installed_timer_units()? {
        let _ = Command::new("systemctl")
            .args(["disable", "--now", &unit])
            .status();
        let path = Path::new(SYSTEMD_DIR).join(unit);
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    let service_path = Path::new(SYSTEMD_DIR).join(SERVICE_UNIT);
    if service_path.exists() {
        fs::remove_file(&service_path)?;
    }
    if command_exists("systemctl") {
        run_systemctl(["daemon-reload"])?;
    }

    if purge_local {
        let installed_config = Path::new(INSTALLED_CONFIG_PATH);
        let config = if installed_config.exists() {
            Config::load(installed_config).ok()
        } else {
            None
        };
        if let Some(config) = config {
            remove_directory_if_present(&config.defaults.scratch_dir)?;
            if config.defaults.state_dir != config.defaults.scratch_dir {
                remove_directory_if_present(&config.defaults.state_dir)?;
            }
        }
        remove_directory_if_present(Path::new("/etc/moat-silo"))?;
    }

    let binary = Path::new(INSTALLED_BINARY);
    if binary.exists() {
        fs::remove_file(binary)?;
    }
    println!("uninstalled moat-silo scheduling environment");
    if !purge_local {
        println!("local configuration and history were preserved; remote backups were not touched");
    }
    Ok(())
}

fn require_root() -> Result<()> {
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "this command must run as root"
    );
    Ok(())
}

fn ensure_runtime_dependencies(config: &Config) -> Result<()> {
    let mut missing = required_commands(config)
        .into_iter()
        .filter(|command| !command_exists(command))
        .collect::<Vec<_>>();
    missing.sort_unstable();
    if !missing.is_empty() {
        bail!(
            "missing runtime commands: {}; rerun install with --install-dependencies",
            missing.join(", ")
        );
    }
    Ok(())
}

fn install_missing_dependencies(config: &Config) -> Result<()> {
    let missing = required_commands(config)
        .into_iter()
        .filter(|command| !command_exists(command))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    ensure!(
        command_exists("apt-get"),
        "automatic dependency installation requires apt-get"
    );
    let mut packages = BTreeSet::new();
    for command in missing {
        packages.insert(match command {
            "pg_dump" | "pg_isready" => "postgresql-client",
            "zstd" => "zstd",
            "age" => "age",
            "aws" => "awscli",
            "curl" => "curl",
            _ => bail!("no package mapping for command {command}"),
        });
    }
    let mut update = Command::new("apt-get");
    update.args(["update"]);
    run_checked(&mut update, "apt-get update")?;
    let mut install = Command::new("apt-get");
    install.args(["install", "-y"]);
    install.args(packages);
    run_checked(&mut install, "apt-get install")?;
    Ok(())
}

fn required_commands(config: &Config) -> Vec<&'static str> {
    let mut commands = vec!["pg_dump", "pg_isready", "zstd", "age", "aws"];
    if config
        .targets
        .iter()
        .any(|target| target.healthcheck_url.is_some())
    {
        commands.push("curl");
    }
    commands
}

fn install_binary() -> Result<()> {
    let current = fs::canonicalize(std::env::current_exe()?)?;
    let destination = Path::new(INSTALLED_BINARY);
    if destination.exists() && fs::canonicalize(destination)? == current {
        return Ok(());
    }
    let temporary = Path::new("/usr/local/bin/.moat-silo.installing");
    fs::copy(&current, temporary).context("failed to copy moat-silo binary")?;
    fs::set_permissions(temporary, fs::Permissions::from_mode(0o755))?;
    fs::rename(temporary, destination).context("failed to install moat-silo binary")?;
    Ok(())
}

fn copy_private_file(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    let parent = destination
        .parent()
        .context("installation path has no parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    if fs::canonicalize(source).ok().as_deref() != fs::canonicalize(destination).ok().as_deref() {
        let temporary = parent.join(".config.toml.installing");
        fs::copy(source, &temporary)?;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        fs::rename(temporary, destination)?;
    }
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_unit(path: &Path, content: &str) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn service_unit(config: &Config) -> String {
    format!(
        "[Unit]\nDescription=Moat Silo backup target %i\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nUser=root\nUMask=0077\nEnvironment=HOME={}\nExecStart={} --config {} run %i\nNice=10\nIOSchedulingClass=best-effort\nIOSchedulingPriority=7\nNoNewPrivileges=true\nPrivateTmp=true\nProtectHome=true\nProtectSystem=strict\nReadWritePaths={} {}\nTimeoutStartSec=12h\n",
        config.defaults.state_dir.display(),
        INSTALLED_BINARY,
        INSTALLED_CONFIG_PATH,
        config.defaults.state_dir.display(),
        config.defaults.scratch_dir.display()
    )
}

fn timer_unit(config: &Config, target: &crate::config::TargetConfig) -> String {
    format!(
        "[Unit]\nDescription=Schedule Moat Silo backup {}\n\n[Timer]\nOnCalendar={}\nRandomizedDelaySec={}\nPersistent=true\nAccuracySec=1m\nUnit=moat-silo@{}.service\n\n[Install]\nWantedBy=timers.target\n",
        target.name, target.on_calendar, config.defaults.randomized_delay_seconds, target.name
    )
}

fn installed_timer_units() -> Result<Vec<String>> {
    let directory = Path::new(SYSTEMD_DIR);
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut units = fs::read_dir(directory)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with("moat-silo@") && name.ends_with(".timer"))
        .collect::<Vec<_>>();
    units.sort();
    Ok(units)
}

fn run_systemctl<const N: usize>(args: [&str; N]) -> Result<()> {
    let mut command = Command::new("systemctl");
    command.args(args);
    run_checked(&mut command, "systemctl")?;
    Ok(())
}

fn remove_directory_if_present(path: &Path) -> Result<()> {
    if path.exists() {
        fs::remove_dir_all(path)
            .with_context(|| format!("failed to remove local directory {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn config() -> Config {
        Config::parse(
            r#"
version = 1
[storage]
type = "s3"
bucket = "backups"
region = "eu-central-1"
access_key_id = "test"
secret_access_key = "secret"

[defaults]
state_dir = "/var/lib/moat-silo"
scratch_dir = "/var/lib/moat-silo/tmp"
randomized_delay_seconds = 300

[[targets]]
name = "production-db"
type = "postgres"
database_url = "postgres://localhost/db"
on_calendar = "*-*-* 02:15:00 UTC"
age_recipient = "age1test"
"#,
        )
        .unwrap()
    }

    #[test]
    fn renders_deterministic_systemd_units() {
        let config = config();
        assert!(service_unit(&config).contains(
            "ExecStart=/usr/local/bin/moat-silo --config /etc/moat-silo/config.toml run %i"
        ));
        assert_eq!(
            timer_unit(&config, &config.targets[0]),
            "[Unit]\nDescription=Schedule Moat Silo backup production-db\n\n[Timer]\nOnCalendar=*-*-* 02:15:00 UTC\nRandomizedDelaySec=300\nPersistent=true\nAccuracySec=1m\nUnit=moat-silo@production-db.service\n\n[Install]\nWantedBy=timers.target\n"
        );
    }
}
