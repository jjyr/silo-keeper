# Moat Silo

Moat Silo 是部署在数据所在生产机上的战时备份粮仓。第一版专注于一件事：

```text
PostgreSQL pg_dump -> zstd -> age -> S3 -> Healthchecks
```

它不需要额外的备份服务器。每台生产机运行同一个 `moat-silo` 二进制，通过一个
root-only TOML 文件声明本机需要备份的数据库。

## 功能

- 一个 TOML 文件配置多个 PostgreSQL target。
- `status` 查看最近运行、最近成功、备份大小和 timer 状态。
- `run` 立即运行指定备份，并用文件锁阻止同一 target 并发执行。
- `history` 查询不包含 secret 的本地执行记录。
- `doctor` 检查配置权限、依赖、数据库连接和 S3 权限。
- `install` 自安装二进制、配置和 systemd timers。
- `uninstall` 一条命令停止并移除调度环境，默认保留配置、状态和远端备份。

## 快速开始

构建：

```bash
mise run build
```

准备配置。配置包含数据库和 S3 secret，必须是 root-only：

```bash
cp config.example.toml config.toml
chmod 600 config.toml
```

安装并启动所有 enabled targets：

```bash
sudo ./target/release/moat-silo --config ./config.toml install
```

这条命令会安装当前二进制和配置、生成 systemd service/timer，并立即启用调度。
日常不需要在生产机上保留源码目录。

如果是全新的 Ubuntu/Hetzner 主机，可以让安装命令补齐 `pg_dump`、`zstd`、`age`、
AWS CLI 和 `curl`：

```bash
sudo ./target/release/moat-silo --config ./config.toml install --install-dependencies
```

常用命令：

```bash
sudo moat-silo status
sudo moat-silo status production-db
sudo moat-silo run production-db
sudo moat-silo history production-db --limit 20
sudo moat-silo doctor
```

停止并卸载调度环境：

```bash
sudo moat-silo uninstall
```

该命令不会删除 S3 备份，也不会删除 `/etc/moat-silo/config.toml` 和
`/var/lib/moat-silo`。只有显式添加 `--purge-local` 才会删除本地配置与历史。

## S3 前置条件

- bucket 必须已启用 Versioning；没有 `VersionId` 的上传会被判为失败。
- 若 `object_lock_days > 0`，bucket 必须在创建时启用 Object Lock，并配置不短于该值的
  默认 retention。Moat Silo 会验证 retention，但不会替你改变 bucket 策略。
- S3 凭据应只允许访问指定 bucket/prefix，至少具备上传、读取对象元数据、读取 retention
  和恢复所需的读取权限。
- `age_recipient` 是公钥，可以留在生产配置中；对应私钥必须另存于离线介质或独立密钥
  管理系统。不要只把解密私钥放在被备份的生产机上。

AWS S3 可以省略 `storage.endpoint`。其他兼容 S3 的服务应填写其 HTTPS endpoint；是否支持
Versioning 和 Object Lock 需由服务商确认。

## 配置说明

生产配置安装到 `/etc/moat-silo/config.toml`，权限固定为 `0600`。主要字段：

- `storage`：S3 bucket、region、endpoint、凭据、公共 prefix 和 Object Lock 要求。
- `defaults`：状态目录、scratch 目录、timer 随机延迟和 stale 阈值。
- `targets`：数据库 URL、systemd `OnCalendar`、age recipient 和 Healthchecks URL。

`database_url` 只通过 `PGDATABASE` 环境变量传给 libpq，不会出现在 `pg_dump` 命令行。
运行记录不会保存数据库 URL、S3 secret 或 Healthchecks URL。

## 安全约束

- 配置不是 root 所有或存在 group/other 权限时，生产命令拒绝运行。
- S3 备份只有在上传和 `head-object` 验证成功后才记录为成功。
- `object_lock_days > 0` 时，每个备份必须返回 VersionId、Object Lock mode 和足够的
  retention，否则本次运行失败。
- Healthchecks 是 best-effort；监控服务不可用不会使一个已经成功的备份失败。
- `uninstall` 永远不会自动删除远端对象。

## 恢复演练

备份不等于可恢复。上线后应至少每月把一个备份恢复到隔离数据库，并核对业务关键表。
基本流程如下：

```text
S3 下载指定 VersionId -> 按 manifest 校验 SHA-256 -> age 解密
-> zstd 解压 -> pg_restore 到隔离数据库 -> 业务校验
```

manifest 与加密备份放在相同日期 prefix 下，记录对象 key、VersionId、大小和加密文件的
SHA-256。恢复时应优先使用 manifest 中的 VersionId，而不是默认取对象的 latest 版本。

## 开发

```bash
mise run check
mise run test
```
