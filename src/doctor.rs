use std::fs;
use std::process::Command;

use anyhow::{Result, bail};

use crate::backup::{aws_command, run_checked};
use crate::config::Config;
use crate::util::command_exists;

pub fn run(config: &Config) -> Result<()> {
    let mut failed = false;
    let requires_curl = config
        .targets
        .iter()
        .any(|target| target.healthcheck_url.is_some());
    for command in ["pg_dump", "pg_isready", "zstd", "age", "aws"]
        .into_iter()
        .chain(requires_curl.then_some("curl"))
    {
        failed |= !report(
            &format!("command {command}"),
            command_exists(command),
            "not found in PATH",
        );
    }

    for directory in [&config.defaults.state_dir, &config.defaults.scratch_dir] {
        let result = fs::create_dir_all(directory);
        failed |= !report(
            &format!("directory {}", directory.display()),
            result.is_ok(),
            &result
                .err()
                .map(|error| error.to_string())
                .unwrap_or_default(),
        );
    }

    if command_exists("pg_isready") {
        for target in config.targets.iter().filter(|target| target.enabled) {
            let status = Command::new("pg_isready")
                .env("PGDATABASE", &target.database_url)
                .args(["--timeout", "5"])
                .status();
            let success = status.is_ok_and(|status| status.success());
            failed |= !report(
                &format!("database {}", target.name),
                success,
                "connection check failed",
            );
        }
    }

    if command_exists("aws") {
        let mut command = aws_command(&config.storage);
        command.args(["s3api", "head-bucket", "--bucket", &config.storage.bucket]);
        let success = run_checked(&mut command, "aws s3api head-bucket").is_ok();
        failed |= !report("S3 bucket", success, "head-bucket failed");
    }

    if failed {
        bail!("doctor found one or more failed checks");
    }
    println!("all moat-silo checks passed");
    Ok(())
}

fn report(name: &str, success: bool, failure: &str) -> bool {
    if success {
        println!("[ok] {name}");
    } else {
        println!("[failed] {name}: {failure}");
    }
    success
}
