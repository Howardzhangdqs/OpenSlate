# 参考项目源码（.ref/）

`.ref/` 目录存放外部参考项目（同类 coding agent / harness）的源码，供开发时查阅对照。**浅克隆、无 git 历史、不入库。**

## 当前收录

| 目录 | 仓库 | 说明 |
|------|------|------|
| `.ref/opencode` | [sst/opencode](https://github.com/sst/opencode) | 终端 coding agent（默认分支 `dev`） |
| `.ref/codex` | [openai/codex](https://github.com/openai/codex) | OpenAI Codex CLI |
| `.ref/pi` | [badlogic/pi-mono](https://github.com/badlogic/pi-mono) | pi coding agent monorepo |
| `.ref/minimax-code` | [MiniMax-AI/minimax-code](https://github.com/MiniMax-AI/minimax-code) | MiniMax 终端 coding agent |
| `.ref/deepseek-harness` | [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) | DeepSeek Harness（插件化 agent 框架） |

## 更新方式

```bash
scripts/update-ref-repos.sh            # 全部更新到最新
scripts/update-ref-repos.sh pi codex   # 只更新指定仓库
```

脚本行为：

- 初次/仓库缺失：`git clone --depth 1 --single-branch`（无历史）。
- 已存在：浅 fetch 远端默认分支（自动探测，兼容 `main`/`master`/`dev`）→ `reset --hard` 到最新提交 → 过期 reflog + `git gc --prune=now` 丢弃旧对象，`.git` 内始终只有最新一版提交。
- 新增参考项目：往脚本内 `REPOS` 数组加一行 `目录名|仓库URL` 即可。

## git 与搜索工具的行为

- `.gitignore` 含 `.ref/`，整个目录**不被 OpenSlate 仓库跟踪**。
- 根 `.ignore` 里有 `!.ref/**` 反向规则，因此 rg / OpenCode 搜索工具**仍能检索**该目录（与 `.slim/deepwork/` 同一套约定）。

## 磁盘占用

浅克隆 + 单版快照，全量约 0.7 GB（2026-10 实测：opencode 234M、deepseek-harness 215M、codex 137M、minimax-code 110M、pi 38M）。不需要时直接 `rm -rf .ref/`，重跑脚本即可恢复。
