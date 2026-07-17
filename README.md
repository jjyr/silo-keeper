# Silo Keeper

Silo Keeper is a small PostgreSQL backup service for Linux hosts. It runs next
to the database, encrypts each logical backup with an offline `age` public key,
and uploads the result and a recovery manifest to versioned S3 storage.

## How it works

```text
pg_dump -> zstd -> age -> encrypted file -> S3 object + JSON manifest
```

The dump, compression, and encryption processes are connected as a streaming
pipeline. Plaintext dumps are never written to disk. The encrypted object is
hashed, uploaded, and verified with `head-object` before a run is considered
successful.

A local state file records each upload phase, object key, VersionId, size, and
SHA-256 hash. A per-target lock prevents overlapping runs. The installer creates
systemd timers; scheduled failures are retried three times at 15-minute
intervals.

## Requirements

- Linux with systemd
- `pg_dump`, `pg_isready`, `zstd`, `age`, and AWS CLI
- `curl` when a Healthchecks URL is configured
- An S3 bucket with Versioning enabled
- An `age` recipient public key; keep the corresponding identity offline

Object Lock is optional. When `object_lock_days` is greater than zero, Silo
Keeper also verifies the uploaded object's retention metadata.

## Build

```bash
mise run build
```

The release binary is written to `target/release/silo-keeper`.

## Example configuration

Create `config.toml` from the following example and replace every placeholder:

```toml
version = 1

[storage]
type = "s3"
bucket = "company-production-backups"
region = "eu-central-1"
# Optional for AWS S3; required for an S3-compatible provider.
endpoint = "https://s3.eu-central-1.amazonaws.com"
prefix = "silo-keeper"
access_key_id = "<s3-access-key>"
secret_access_key = "<s3-secret-key>"
# session_token = "<optional-session-token>"
# Set to 0 when Object Lock is not used. Versioning is always required.
object_lock_days = 14

[defaults]
state_dir = "/var/lib/silo-keeper"
scratch_dir = "/var/lib/silo-keeper/tmp"
randomized_delay_seconds = 900
max_backup_age_hours = 36

[[targets]]
name = "production-db"
type = "postgres"
database_url = "postgres://backup-user:<password>@127.0.0.1:5432/production"
on_calendar = "*-*-* 02:15:00 UTC"
age_recipient = "age1..."
healthcheck_url = "https://hc-ping.com/<check-id>"
enabled = true
# Optional; overrides defaults.max_backup_age_hours for this target.
max_backup_age_hours = 36
```

The configuration contains secrets and must not be readable by other users:

```bash
chmod 600 config.toml
```

Put the database password in URL userinfo as shown above. Sensitive query
parameters such as `?password=...`, `?sslpassword=...`, and token parameters are
rejected so they cannot appear in process arguments.

## Install

Install the binary, root-only configuration, and timers:

```bash
sudo ./target/release/silo-keeper --config ./config.toml install
```

On a new Ubuntu host, Silo Keeper can install missing runtime packages with
`apt-get`:

```bash
sudo ./target/release/silo-keeper --config ./config.toml \
  install --install-dependencies
```

The installed paths are:

```text
/usr/local/bin/silo-keeper
/etc/silo-keeper/config.toml
/var/lib/silo-keeper
/etc/systemd/system/silo-keeper@.service
/etc/systemd/system/silo-keeper@<target>.timer
```

Installation and upgrades are transactional: schedules and `age` recipients
are checked first, files are replaced atomically, and the previous installation
is restored if the new timers cannot be activated.

## Command-line usage

Validate a configuration before installing it:

```bash
sudo silo-keeper --config /etc/silo-keeper/config.toml check
```

Inspect all targets or one target:

```bash
sudo silo-keeper status
sudo silo-keeper status production-db
sudo silo-keeper status production-db --json
```

Run a backup immediately:

```bash
sudo silo-keeper run production-db
```

Read local execution history:

```bash
sudo silo-keeper history production-db --limit 20
sudo silo-keeper history production-db --limit 20 --json
```

Check commands, directories, database connectivity, and S3 connectivity:

```bash
sudo silo-keeper doctor
```

Exercise real backup permissions with a full `pg_dump` to `/dev/null` and a
small retained S3 canary object:

```bash
sudo silo-keeper doctor --canary
```

The canary can read the full database and leaves its versioned object under
`<prefix>/_doctor/`; use it deliberately.

Remove the binary and systemd scheduling while preserving configuration, local
history, and remote backups:

```bash
sudo silo-keeper uninstall
```

Also remove local configuration and history:

```bash
sudo silo-keeper uninstall --purge-local
```

Remote S3 objects are never deleted by `uninstall`.

## Recovery

Use the manifest's exact object key and VersionId during recovery:

```text
download object VersionId -> verify SHA-256 -> age decrypt -> zstd decompress
-> pg_restore into an isolated database -> validate application data
```

Test this process regularly. A successful upload is not a substitute for a
restore drill.

## Development

```bash
mise run check
mise run test
```
