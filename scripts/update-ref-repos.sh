#!/usr/bin/env bash
# 更新 .ref/ 下的参考项目源码到最新版本。
#
# 设计目标：本地始终只保留各仓库最新一版代码，不累积 git 历史。
#   - 初次：浅克隆（--depth 1 --single-branch，无历史）
#   - 更新：浅 fetch 默认分支 -> 硬重置 -> 过期 reflog -> gc 清掉旧对象
#   - 仓库缺失/损坏时自动重新浅克隆
#
# .ref/ 整个目录被项目 .gitignore 忽略（不入库），但通过 .ignore 保持
# 对 rg / 搜索工具可见，方便随时查阅参考源码。
#
# 用法：scripts/update-ref-repos.sh [名称 ...]   # 不带参数则全部更新
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REF_DIR="$ROOT/.ref"

# 目录名|仓库地址（新增参考项目往这里加一行即可）
REPOS=(
  "opencode|https://github.com/sst/opencode"
  "codex|https://github.com/openai/codex"
  "pi|https://github.com/badlogic/pi-mono"
  "minimax-code|https://github.com/MiniMax-AI/minimax-code"
  "deepseek-harness|https://github.com/deepseek-ai/deepseek-harness"
)

log()  { printf '\033[1;32m[update-ref]\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m[update-ref]\033[0m %s\n' "$*" >&2; }

default_branch() { # $1: repo dir
  git -C "$1" ls-remote --symref origin HEAD \
    | awk '$1=="ref:"{sub(/^refs\/heads\//,"",$2); print $2; exit}'
}

update_repo() { # $1: name  $2: url
  local name="$1" url="$2" dir="$REF_DIR/$name" branch

  if [[ ! -d "$dir/.git" ]]; then
    log "[$name] 本地不存在（或已损坏），重新浅克隆…"
    rm -rf "$dir"
    git clone --depth 1 --single-branch --quiet "$url" "$dir"
    log "[$name] 克隆完成：$(git -C "$dir" log -1 --format='%h %cs' )"
    return 0
  fi

  branch="$(default_branch "$dir")"
  if [[ -z "$branch" ]]; then
    warn "[$name] 无法探测默认分支，跳过（可手动 rm -rf 后重跑以重新克隆）"
    return 1
  fi

  log "[$name] fetch 默认分支 $branch（depth=1）…"
  if ! git -C "$dir" fetch --depth 1 --quiet origin "$branch"; then
    warn "[$name] fetch 失败，跳过"
    return 1
  fi

  git -C "$dir" checkout -q -B "$branch" FETCH_HEAD
  # 丢弃旧历史对象：立即过期 reflog 并 gc，使 .git 只保留最新一版
  git -C "$dir" reflog expire --expire=now --all
  git -C "$dir" gc --prune=now --quiet
  log "[$name] 已更新：$(git -C "$dir" log -1 --format='%h %cs %s' | head -c 100)"
}

mkdir -p "$REF_DIR"

# 按参数过滤要更新的仓库
targets=()
if [[ $# -gt 0 ]]; then
  for want in "$@"; do
    found=""
    for spec in "${REPOS[@]}"; do
      [[ "${spec%%|*}" == "$want" ]] && { found="$spec"; break; }
    done
    if [[ -z "$found" ]]; then
      warn "未知仓库名：$want（可用：${REPOS[*]//|*/ }）"
      exit 1
    fi
    targets+=("$found")
  done
else
  targets=("${REPOS[@]}")
fi

fail=0
for spec in "${targets[@]}"; do
  name="${spec%%|*}"; url="${spec##*|}"
  update_repo "$name" "$url" || fail=$((fail+1))
done

echo
du -sh "$REF_DIR"/* 2>/dev/null | sed 's/^/  /'
log "完成（失败 $fail 个）"
exit "$fail"
