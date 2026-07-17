use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

use crate::backup::run_checked;
use crate::config::{Config, INSTALLED_CONFIG_PATH};
use crate::util::command_exists;

const INSTALLED_BINARY: &str = "/usr/local/bin/silo-keeper";
const SYSTEMD_DIR: &str = "/etc/systemd/system";
const SERVICE_UNIT: &str = "silo-keeper@.service";

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
    preflight_install(config)?;

    create_private_directory(&config.defaults.state_dir)?;
    create_private_directory(&config.defaults.scratch_dir)?;

    let existing_units = installed_timer_units()?;
    let previous_timers = existing_units
        .iter()
        .map(|unit| timer_runtime_state(unit))
        .collect::<Result<Vec<_>>>()?;
    let mut desired_units = BTreeMap::new();
    desired_units.insert(SERVICE_UNIT.to_owned(), service_unit(config));
    for target in config.targets.iter().filter(|target| target.enabled) {
        let unit_name = format!("silo-keeper@{}.timer", target.name);
        desired_units.insert(unit_name, timer_unit(config, target));
    }
    let enabled_units = desired_units
        .keys()
        .filter(|unit| unit.ends_with(".timer"))
        .cloned()
        .collect::<Vec<_>>();

    let mut managed_paths = vec![
        PathBuf::from(INSTALLED_BINARY),
        PathBuf::from(INSTALLED_CONFIG_PATH),
        Path::new(SYSTEMD_DIR).join(SERVICE_UNIT),
    ];
    managed_paths.extend(
        existing_units
            .iter()
            .chain(enabled_units.iter())
            .map(|unit| Path::new(SYSTEMD_DIR).join(unit)),
    );
    managed_paths.sort();
    managed_paths.dedup();
    let snapshots = managed_paths
        .iter()
        .map(|path| FileSnapshot::capture(path))
        .collect::<Result<Vec<_>>>()?;

    let commit_result = commit_install(
        source_config,
        &desired_units,
        &existing_units,
        &enabled_units,
    );
    if let Err(error) = commit_result {
        let rollback = rollback_install(&snapshots, &previous_timers, &enabled_units);
        return match rollback {
            Ok(()) => Err(error.context("installation failed; previous installation restored")),
            Err(rollback_error) => Err(error.context(format!(
                "installation failed and rollback was incomplete: {rollback_error:#}"
            ))),
        };
    }

    println!(
        "installed silo-keeper with {} enabled target(s)",
        enabled_units.len()
    );
    println!("configuration: {INSTALLED_CONFIG_PATH}");
    println!("status: sudo silo-keeper status");
    Ok(())
}

fn commit_install(
    source_config: &Path,
    desired_units: &BTreeMap<String, String>,
    existing_units: &[String],
    enabled_units: &[String],
) -> Result<()> {
    install_binary()?;
    copy_private_file(source_config, Path::new(INSTALLED_CONFIG_PATH), 0o600)?;
    for (unit, content) in desired_units {
        write_unit(&Path::new(SYSTEMD_DIR).join(unit), content)?;
    }

    run_systemctl(["daemon-reload"])?;
    if !enabled_units.is_empty() {
        run_systemctl_units(&["enable"], enabled_units)?;
        run_systemctl_units(&["restart"], enabled_units)?;
        for unit in enabled_units {
            ensure!(
                systemctl_bool(&["is-active", "--quiet", unit])?,
                "new timer {unit} did not become active"
            );
        }
    }

    let stale_units = existing_units
        .iter()
        .filter(|unit| !enabled_units.contains(unit))
        .cloned()
        .collect::<Vec<_>>();
    if !stale_units.is_empty() {
        run_systemctl_units(&["disable", "--now"], &stale_units)?;
        for unit in &stale_units {
            fs::remove_file(Path::new(SYSTEMD_DIR).join(unit))
                .with_context(|| format!("failed to remove stale timer {unit}"))?;
        }
        run_systemctl(["daemon-reload"])?;
    }
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
        remove_directory_if_present(Path::new("/etc/silo-keeper"))?;
    }

    let binary = Path::new(INSTALLED_BINARY);
    if binary.exists() {
        fs::remove_file(binary)?;
    }
    println!("uninstalled silo-keeper scheduling environment");
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

fn preflight_install(config: &Config) -> Result<()> {
    ensure!(
        command_exists("systemd-analyze"),
        "systemd-analyze is required to validate timer schedules"
    );
    for target in config.targets.iter().filter(|target| target.enabled) {
        let mut calendar = Command::new("systemd-analyze");
        calendar.args(["calendar", &target.on_calendar]);
        run_checked(
            &mut calendar,
            &format!("validate OnCalendar for {}", target.name),
        )?;

        let mut age = Command::new("age");
        age.args(["--encrypt", "--recipient", &target.age_recipient])
            .stdin(Stdio::null());
        run_checked(
            &mut age,
            &format!("validate age recipient for {}", target.name),
        )?;
    }
    Ok(())
}

fn install_binary() -> Result<()> {
    let current = fs::canonicalize(std::env::current_exe()?)?;
    let destination = Path::new(INSTALLED_BINARY);
    if destination.exists() && fs::canonicalize(destination)? == current {
        return Ok(());
    }
    atomic_copy_file(&current, destination, 0o755).context("failed to install silo-keeper binary")
}

fn copy_private_file(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    let parent = destination
        .parent()
        .context("installation path has no parent")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    if fs::canonicalize(source).ok().as_deref() == fs::canonicalize(destination).ok().as_deref() {
        fs::set_permissions(destination, fs::Permissions::from_mode(mode))?;
        return Ok(());
    }
    atomic_copy_file(source, destination, mode)
}

fn create_private_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_unit(path: &Path, content: &str) -> Result<()> {
    atomic_write_file(path, content.as_bytes(), 0o644)
}

fn service_unit(config: &Config) -> String {
    format!(
        "[Unit]\nDescription=Silo Keeper backup target %i\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=oneshot\nUser=root\nUMask=0077\nEnvironment=HOME={}\nExecStart={} --config {} run-scheduled %i\nNice=10\nIOSchedulingClass=best-effort\nIOSchedulingPriority=7\nNoNewPrivileges=true\nPrivateTmp=true\nProtectHome=true\nProtectSystem=strict\nReadWritePaths={} {}\nTimeoutStartSec=12h\n",
        config.defaults.state_dir.display(),
        INSTALLED_BINARY,
        INSTALLED_CONFIG_PATH,
        config.defaults.state_dir.display(),
        config.defaults.scratch_dir.display()
    )
}

fn timer_unit(config: &Config, target: &crate::config::TargetConfig) -> String {
    format!(
        "[Unit]\nDescription=Schedule Silo Keeper backup {}\n\n[Timer]\nOnCalendar={}\nRandomizedDelaySec={}\nPersistent=true\nAccuracySec=1m\nUnit=silo-keeper@{}.service\n\n[Install]\nWantedBy=timers.target\n",
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
        .filter(|name| name.starts_with("silo-keeper@") && name.ends_with(".timer"))
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

fn run_systemctl_units(prefix: &[&str], units: &[String]) -> Result<()> {
    if units.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("systemctl");
    command.args(prefix).args(units);
    run_checked(&mut command, &format!("systemctl {}", prefix.join(" ")))?;
    Ok(())
}

fn systemctl_bool(args: &[&str]) -> Result<bool> {
    let output = Command::new("systemctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .context("failed to start systemctl")?;
    if output.status.success() {
        return Ok(true);
    }
    if output.stderr.is_empty() {
        return Ok(false);
    }
    bail!(
        "systemctl {} failed with {}: {}",
        args.join(" "),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[derive(Debug)]
struct TimerRuntimeState {
    unit: String,
    enabled: bool,
    active: bool,
}

fn timer_runtime_state(unit: &str) -> Result<TimerRuntimeState> {
    Ok(TimerRuntimeState {
        unit: unit.to_owned(),
        enabled: systemctl_bool(&["is-enabled", "--quiet", unit])?,
        active: systemctl_bool(&["is-active", "--quiet", unit])?,
    })
}

#[derive(Debug)]
struct FileSnapshot {
    path: PathBuf,
    contents: Option<Vec<u8>>,
    mode: Option<u32>,
}

impl FileSnapshot {
    fn capture(path: &Path) -> Result<Self> {
        match fs::metadata(path) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_file(),
                    "managed installation path is not a regular file: {}",
                    path.display()
                );
                Ok(Self {
                    path: path.to_owned(),
                    contents: Some(
                        fs::read(path)
                            .with_context(|| format!("failed to snapshot {}", path.display()))?,
                    ),
                    mode: Some(metadata.permissions().mode() & 0o777),
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: path.to_owned(),
                contents: None,
                mode: None,
            }),
            Err(error) => {
                Err(error).with_context(|| format!("failed to inspect {}", path.display()))
            }
        }
    }

    fn restore(&self) -> Result<()> {
        match (&self.contents, self.mode) {
            (Some(contents), Some(mode)) => atomic_write_file(&self.path, contents, mode),
            (None, None) => match fs::remove_file(&self.path) {
                Ok(()) => sync_parent(&self.path),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("failed to remove {}", self.path.display()))
                }
            },
            _ => bail!("invalid snapshot for {}", self.path.display()),
        }
    }
}

fn rollback_install(
    snapshots: &[FileSnapshot],
    previous_timers: &[TimerRuntimeState],
    desired_timers: &[String],
) -> Result<()> {
    let mut failures = Vec::new();
    if let Err(error) = run_systemctl_units(&["disable", "--now"], desired_timers) {
        failures.push(format!("failed to stop new timers: {error:#}"));
    }
    for snapshot in snapshots.iter().rev() {
        if let Err(error) = snapshot.restore() {
            failures.push(format!(
                "failed to restore {}: {error:#}",
                snapshot.path.display()
            ));
        }
    }
    if let Err(error) = run_systemctl(["daemon-reload"]) {
        failures.push(format!("failed to reload restored units: {error:#}"));
    }
    for timer in previous_timers {
        let enable_action = if timer.enabled { "enable" } else { "disable" };
        if let Err(error) = run_systemctl_units(&[enable_action], std::slice::from_ref(&timer.unit))
        {
            failures.push(format!(
                "failed to restore enablement for {}: {error:#}",
                timer.unit
            ));
        }
        let active_action = if timer.active { "restart" } else { "stop" };
        if let Err(error) = run_systemctl_units(&[active_action], std::slice::from_ref(&timer.unit))
        {
            failures.push(format!(
                "failed to restore activity for {}: {error:#}",
                timer.unit
            ));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

fn atomic_copy_file(source: &Path, destination: &Path, mode: u32) -> Result<()> {
    let parent = destination
        .parent()
        .context("installation path has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = tempfile::Builder::new()
        .prefix(".silo-keeper.installing-")
        .tempfile_in(parent)?;
    fs::copy(source, temporary.path())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .with_context(|| format!("failed to atomically replace {}", destination.display()))?;
    sync_parent(destination)
}

fn atomic_write_file(destination: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = destination
        .parent()
        .context("installation path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".silo-keeper.installing-")
        .tempfile_in(parent)?;
    temporary.write_all(contents)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .with_context(|| format!("failed to atomically replace {}", destination.display()))?;
    sync_parent(destination)
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("installation path has no parent")?)?.sync_all()?;
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
state_dir = "/var/lib/silo-keeper"
scratch_dir = "/var/lib/silo-keeper/tmp"
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
            "ExecStart=/usr/local/bin/silo-keeper --config /etc/silo-keeper/config.toml run-scheduled %i"
        ));
        assert_eq!(
            timer_unit(&config, &config.targets[0]),
            "[Unit]\nDescription=Schedule Silo Keeper backup production-db\n\n[Timer]\nOnCalendar=*-*-* 02:15:00 UTC\nRandomizedDelaySec=300\nPersistent=true\nAccuracySec=1m\nUnit=silo-keeper@production-db.service\n\n[Install]\nWantedBy=timers.target\n"
        );
    }

    #[test]
    fn file_snapshots_restore_replaced_and_new_files() {
        let directory = tempfile::tempdir().unwrap();
        let existing = directory.path().join("existing");
        let new = directory.path().join("new");
        fs::write(&existing, b"old").unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o600)).unwrap();

        let existing_snapshot = FileSnapshot::capture(&existing).unwrap();
        let new_snapshot = FileSnapshot::capture(&new).unwrap();
        atomic_write_file(&existing, b"replacement", 0o644).unwrap();
        atomic_write_file(&new, b"created", 0o644).unwrap();

        existing_snapshot.restore().unwrap();
        new_snapshot.restore().unwrap();
        assert_eq!(fs::read(&existing).unwrap(), b"old");
        assert_eq!(
            fs::metadata(&existing).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!new.exists());
    }
}
