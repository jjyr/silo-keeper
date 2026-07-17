use anyhow::{Result, bail};
use std::fs;

use crate::backup::{
    aws_command, postgres_command, run_checked, upload_storage_canary, verify_database_dump_access,
};
use crate::config::Config;
use crate::util::command_exists;

pub fn run(config: &Config, canary: bool) -> Result<()> {
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
            let status = postgres_command("pg_isready", target).and_then(|mut command| {
                command
                    .args(["--timeout", "5"])
                    .status()
                    .map_err(Into::into)
            });
            let success = status.is_ok_and(|status| status.success());
            failed |= !report(
                &format!("database {} connectivity", target.name),
                success,
                "pg_isready connectivity check failed",
            );
        }
    }

    let mut s3_connectivity = false;
    if command_exists("aws") {
        let mut command = aws_command(&config.storage);
        command.args(["s3api", "head-bucket", "--bucket", &config.storage.bucket]);
        s3_connectivity = run_checked(&mut command, "aws s3api head-bucket").is_ok();
        failed |= !report(
            "S3 bucket connectivity",
            s3_connectivity,
            "head-bucket failed",
        );
    }

    if canary {
        if command_exists("pg_dump") {
            for target in config.targets.iter().filter(|target| target.enabled) {
                let success = verify_database_dump_access(target).is_ok();
                failed |= !report(
                    &format!("database {} authenticated full dump canary", target.name),
                    success,
                    "pg_dump canary failed",
                );
            }
        }
        if command_exists("aws") && s3_connectivity {
            match upload_storage_canary(config) {
                Ok((key, version_id)) => println!(
                    "[ok] S3 write canary: s3://{}/{} (VersionId {})",
                    config.storage.bucket, key, version_id
                ),
                Err(_) => {
                    failed = true;
                    report(
                        "S3 write canary",
                        false,
                        "upload or object metadata check failed",
                    );
                }
            }
        }
        println!(
            "doctor canary performs a full pg_dump to /dev/null and leaves its tiny versioned S3 object in place"
        );
    } else {
        println!(
            "connectivity checks do not validate database dump or S3 upload permissions; rerun with --canary for end-to-end permission checks"
        );
    }

    if failed {
        bail!("doctor found one or more failed checks");
    }
    println!("all requested silo-keeper checks passed");
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
