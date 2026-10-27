# 多机工作区同步（dev ↔ lenovo-legion）

> 脚本：`scripts/sync-remote.sh`（rsync over SSH，走 Tailscale 内网）。加入时间：2026-10。

## 为什么需要

dev 机（thinkbook）是日常开发机，但重活（全量 `cargo build`、并行 `cargo test`）在 16 核 / 62G 的 lenovo-legion 上快得多。两台机器各保留一份完整工作区（含 `.git`），用脚本把源码侧双向搬移，构建产物各自本地生成、绝不同步（dev 机 `target/` du 虚高 100G+，见 [troubleshooting.md](troubleshooting.md) #20）。

legion 已装齐 Android 构建链（SDK platform 37 / build-tools 36 / NDK r21 / JDK 21 / Gradle 缓存 + `~/.cargo-android` 交叉配置），**可出完整 APK**（2026-10-27 实测 `./scripts/build-apk.sh --release` 2m08s，产物字节数与 dev 机一致）。日常分工仍是：legion 主跑重编译与 `cargo test`，dev 机负责出包+装机（测试设备都连在 dev 侧，legion 无 adb 设备）。

## 机器与路径

| 机器 | Tailscale 名（SSH 别名） | 仓库路径 | OS hostname（注意） |
|------|------|------|------|
| dev 机 | `thinkbook`（IP `100.96.226.113`） | `/home/howardzhangdqs/DEV/github/openslate_mobile/OpenSlate` | `howardzhangdqs`（小写） |
| 大机器 | `lenovo-legion` | `/data/github/OpenSlate` | `HowardZhangdqs`（大写，仅大小写之差） |

**身份判别**：`tailscale status` 首行即本机 Tailscale 名（`thinkbook` / `lenovo-legion`，完全不同、无歧义）。不用 OS hostname 判别的原因：两台机器的 OS hostname 仅大小写不同（`howardzhangdqs` / `HowardZhangdqs`）。tailscale 不可用时按仓库路径兜底（`/data/github/` 下 = legion）。

## SSH 互信（已配置，仅备查）

- dev → legion：dev 机 `~/.ssh/config` 有 `Host lenovo-legion` 别名，公钥已注入 legion。
- legion → dev：legion `~/.ssh/config` 有 `Host thinkbook`（HostName `100.96.226.113`），legion 的 `id_ed25519` 公钥已注入 dev 机 `~/.ssh/authorized_keys`。
- 两台均在同一 Tailscale 网络；Tailscale IP 每台设备固定，换 IP 只需改 ssh config。

## 用法

```bash
./scripts/sync-remote.sh push            # 本机 → 对端（默认）
./scripts/sync-remote.sh pull            # 对端 → 本机
./scripts/sync-remote.sh push --dry-run  # 只预览差异（--itemize-changes）
FORCE=1 ./scripts/sync-remote.sh pull    # 本地有未提交改动时强制 pull
```

## 行为细节

- rsync `-a --delete`，即**镜像覆盖**对端（对端多出的文件会被删掉，构建产物除外）。
- 排除项（`EXCLUDES`，改脚本即可调整）：
  - `target/`——**任意层级**（含 `vendor/genai/target/` 这类嵌套构建目录）；
  - `/target-android/`、`/.ref/`；
  - `/android/.gradle/`、`/android/build/`、`/android/app/build/`；
  - `/android/local.properties`（机器相关 SDK 路径，各自维护）、`.idea/`。
- 同步前后校验：
  - 前置：对端可达且 `$REMOTE_DIR/.git` 存在，否则报错退出；
  - 后置：比对两侧 `git rev-parse HEAD`，一致打印成功，不一致给警告（对端可能有本地提交——覆盖前先 `git -C ... log` 确认，避免丢提交）。
- 安全阀：`pull` 且本地 `git status --porcelain` 非空时拒绝执行（防 `--delete` 删掉本地未提交文件）；`--dry-run` 与 `FORCE=1` 不受限。

## 典型流程

1. dev 机改代码、commit（工作区干净）；
2. `./scripts/sync-remote.sh push`；
3. `ssh lenovo-legion`，`cd /data/github/OpenSlate`，跑 `cargo build` / `cargo test`（实测全量 host 构建约 1m10s；也可 `./scripts/build-apk.sh --release` 出 APK，实测 2m08s）；
4. 若在 legion 上有修改：commit 后在 legion 上 `./scripts/sync-remote.sh push`（或在 dev 机 `pull`）。

## 排错

| 症状 | 处置 |
|------|------|
| `thinkbook`/`lenovo-legion` 连不上 | `tailscale status` 确认两台都在线；别名失效则直接用 Tailscale IP |
| HEAD 不一致警告 | 先 `git log` 看对端提交，确认要覆盖再 push；要保留对端提交则在 dev 机 `pull` |
| pull 被拒绝 | 按 提示 commit/stash，确认后 `FORCE=1` 强制 |
