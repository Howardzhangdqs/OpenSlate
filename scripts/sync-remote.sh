#!/usr/bin/env bash
# OpenSlate 多机工作区同步（dev 机 thinkbook ↔ lenovo-legion）
#
# 用法:
#   scripts/sync-remote.sh [push|pull] [--dry-run]
#     push      本机 → 对端（默认）。把当前工作区（含 .git）原样覆盖到对端。
#     pull      对端 → 本机。本地有未提交改动时拒绝执行（防 --delete 误删），
#               FORCE=1 ./scripts/sync-remote.sh pull 可强制。
#     --dry-run 只预览要同步的差异，不落地。
#
# 两侧固定约定（身份按 `tailscale status` 本机名判别：thinkbook / lenovo-legion，
# 无歧义；两台机器 OS hostname 仅大小写不同不可用，tailscale 不可用时按路径兜底）:
#   dev 机(thinkbook, Tailscale 别名 lenovo-legion 的对端) 本仓库在
#       /home/howardzhangdqs/DEV/github/openslate_mobile/OpenSlate
#   lenovo-legion (Tailscale: thinkbook) 本仓库在
#       /data/github/OpenSlate
#
# 排除项（EXCLUDES）: 所有构建产物（任意层级 target/、根 target-android/）、
# .ref/ 参考仓库、android 构建目录、机器相关的 android/local.properties、.idea/。
# 同步后自动比对两侧 git HEAD；HEAD 不一致时给出警告（对端可能有本地提交）。
set -euo pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
LOCAL_DIR=$(cd "$SCRIPT_DIR/.." && pwd)

ACTION="${1:-push}"
DRY_RUN=0
for a in "$@"; do
  [[ "$a" == "--dry-run" ]] && DRY_RUN=1
done
case "$ACTION" in
  push|pull) ;;
  *) echo "用法: $0 [push|pull] [--dry-run]" >&2; exit 2 ;;
esac

TS_NAME=$(tailscale status 2>/dev/null | head -1 | awk '{print $2}')
case "$TS_NAME" in
  thinkbook)
    REMOTE="lenovo-legion"; REMOTE_DIR="/data/github/OpenSlate" ;;
  lenovo-legion)
    REMOTE="thinkbook"; REMOTE_DIR="/home/howardzhangdqs/DEV/github/openslate_mobile/OpenSlate" ;;
  *)
    # tailscale 不可用时兜底：按仓库路径判别
    case "$LOCAL_DIR" in
      /data/github/*)
        REMOTE="thinkbook"; REMOTE_DIR="/home/howardzhangdqs/DEV/github/openslate_mobile/OpenSlate" ;;
      *)
        REMOTE="lenovo-legion"; REMOTE_DIR="/data/github/OpenSlate" ;;
    esac ;;
esac

EXCLUDES=(
  --exclude 'target/'                  # 任意层级（含 vendor/genai/target/）
  --exclude '/target-android/'
  --exclude '/.ref/'
  --exclude '/android/.gradle/'
  --exclude '/android/build/'
  --exclude '/android/app/build/'
  --exclude '/android/local.properties'
  --exclude '.idea/'
)

RSYNC_FLAGS=(-a --delete "${EXCLUDES[@]}")
if [[ $DRY_RUN == 1 ]]; then
  RSYNC_FLAGS+=(--dry-run --itemize-changes)
fi

# 前置检查：对端可达且目录是 git 仓库
ssh -o ConnectTimeout=8 "$REMOTE" "[ -d '$REMOTE_DIR/.git' ]" || {
  echo "错误: $REMOTE:$REMOTE_DIR 不可达或不是 git 仓库" >&2
  exit 1
}

if [[ "$ACTION" == "pull" ]]; then
  if [[ $DRY_RUN == 0 && "${FORCE:-0}" != "1" ]] \
     && [[ -n "$(git -C "$LOCAL_DIR" status --porcelain 2>/dev/null)" ]]; then
    echo "错误: 本地工作区有未提交改动，pull 会用对端覆盖本目录（--delete 可能删掉本地新文件）。" >&2
    echo "      先 git commit / git stash，或 FORCE=1 $0 pull 强制。" >&2
    exit 1
  fi
  echo ">> pull: $REMOTE:$REMOTE_DIR/ → $LOCAL_DIR/"
  rsync "${RSYNC_FLAGS[@]}" "$REMOTE:$REMOTE_DIR/" "$LOCAL_DIR/"
else
  echo ">> push: $LOCAL_DIR/ → $REMOTE:$REMOTE_DIR/"
  rsync "${RSYNC_FLAGS[@]}" "$LOCAL_DIR/" "$REMOTE:$REMOTE_DIR/"
fi

# 校验：两侧 git HEAD
LOCAL_HEAD=$(git -C "$LOCAL_DIR" rev-parse HEAD)
REMOTE_HEAD=$(ssh "$REMOTE" "git -C '$REMOTE_DIR' rev-parse HEAD")
echo ">> HEAD  本地: $LOCAL_HEAD"
echo ">> HEAD  对端: $REMOTE_HEAD"
if [[ "$LOCAL_HEAD" == "$REMOTE_HEAD" ]]; then
  echo ">> 完成：两侧 HEAD 一致"
else
  echo "!! 警告：两侧 HEAD 不一致（对端可能有本地提交，或本次为 --dry-run 未真正同步）" >&2
fi
