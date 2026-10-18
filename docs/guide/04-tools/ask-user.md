# 04 · ask_user（Agent 向用户提问工具）

> 设计真相源：`docs/proposals/ask-user-tool-design.md`（r6）。UI 规范：
> `docs/proposals/ask-user-ui-design.md`；移动端全链路时序见
> [06-mobile/call-chains.md §3.2](../06-mobile/call-chains.md)。
> 工具本体：`crates/openslate-mobile/src/askuser.rs`（宿主直连 `Tool`，
> `HostTool` 同款模式，不进 mcp-builtin）。

## 1. 定位与边界

- **信息收集**工具：Agent 缺少必要信息、或面临分叉决策时向用户提问并等待
  回答。与审批系统（高风险操作的风险门，无自由答案负载）**保持分离**——
  提问走 host call，审批走 `SessionApprovalBridge`。
- **不做二次审批**：默认策略（AutoExcept shell/run_code）自然放行；Manual
  全审模式下经 `AskUserExemptApproval` 装饰器豁免（装饰 mobile 装配的
  `manager.approval` callback 槽，core 审批逻辑不动）。
- PTC 可见性：默认 **Both**（沙箱内 run_code 亦可调用；bridge 路径无
  tool_call 声明 → 不持久化，仅内存通路）。

## 2. 输入 schema（模型侧）

`questions[]`（1~4 题，建议 ≤3）每题：`id`（缺省 q1..qn，snake_case 答案
键）/ `header`（≤12 字符卡片标签，缺省 = 问句截断）/ `question`（完整问句，
必填）/ `type`（choice|text|confirm|secret，缺省 choice）/ `options`
（choice/confirm 必填 **2~4 个**，label 必填 + description + recommended；
**禁止自带 Other**——UI 恒附自由输入框）/ `multi_select`（仅 choice）/
`placeholder` / `optional` / `storage`（仅 secret 必填：
`{kind:"env",name}` 或 `{kind:"file",path}`）。顶层 `timeout`：秒数，
`-1` = 永不等待超时，缺省取 `[ask_user] default_timeout`。

description 文案要点（收敛模型行为）：只问阻塞的问题；能假设就假设并说
明；推荐项放第一、label 加 "(Recommended)" + `recommended:true`；相关小
问题合并一次调用；confirm 二值 / text 命名类 / secret 密钥；label 1~5 词。

校验失败（0/5 题、选项数越界、secret 缺 storage、timeout=0 等）→ 错误文本
回喂 LLM 自纠（业务错误约定，`Ok(ToolOutput{status:Error})`）。

## 3. 下发契约（host_call_requested，Android UI 已对齐）

```jsonc
{"type":"host_call_requested","id":42,"tool":"ask_user",
 "args":{"v":1,"timeout":-1,          // timeout: -1=永不 | N 秒
   "questions":[
     {"id":"db","header":"数据库","question":"…","type":"choice",
      "options":[{"label":"SQLite","description":"…","recommended":true}, …],
      "multi_select":false,"optional":false},
     {"id":"tok","type":"secret","storage":{"kind":"env","name":"GITHUB_TOKEN"}, …}
   ]}}
```

`options` 仅 choice/confirm 出现；`multi_select` 仅 choice；`storage` 仅
secret；`placeholder` 有值才出现。requestId（幂等主键）= host call id。

resolve（ok=true 的全部业务形态）：提交
`{"outcome":"answered","answers":[{"id","selected":[…]}|{"id","value"}|
{"id","value",…,"remember"}|{"id","skipped":true}|{"id","declined":true}]}`；
整卡 decline `{"outcome":"declined"}`。**ok=false 仅基础设施错误**；对失效
id 的 resolve 是无害 no-op（`HostCallRouter::resolve` 返回 false）。

## 4. 回填与 secret

- 回填 = 自然语言逐题列表 + 结构化 JSON 并存（`format_question_result`）：
  `- "db" → SQLite (Recommended)`（推荐标记复述）、多选拼接、`(skipped)` /
  `(declined)` / `(no answer)`；decline 整卡给指定文案「用户关闭了提问卡片
  未作答。请自行判断继续…」；timeout 给「用户在 N 秒内未回应，可能不在。
  请自行判断继续。」
- **secret 明文绝不进上下文/日志**：答案入 `AskUserSecretVault`（生产 =
  `PersistentSecretVault`，见 §8），模型只收
  `已存入凭据：环境变量 GITHUB_TOKEN（本会话生效）`（remember=true 时
  「（已记住，后续会话可用）」）与 `"secret":"env:GITHUB_TOKEN"` 引用。

## 5. 生命周期与持久化（store v3 `pending_questions` 表）

状态机 `pending → answered | declined | superseded | expired`（cancelled
不在枚举——用户停止由回合取消通道表达，行按 superseded 收卷）。TTL 24h。

| 事件 | 路径 |
|------|------|
| 工具发起 | 落库 pending 行（request_id=host call id、tool_call_id=**当前 run 持久化消息中悬挂的 ask_user 声明**——runner 在执行工具前已落盘 assistant 消息；`find_call_id` 读 store 而非 `inner.history`，后者回合中是旧副本）→ 发信封 → 自定义超时等待 |
| 提交/整卡拒绝（在途） | `resolve_host_call` → 工具格式化回填 → 行 answered/declined |
| 超时 | 业务结果回填 timeout 文案；行 expired |
| 用户取消回合 | 引擎 select! 合成 cancelled 结果；`on_cancel_all` 把全部 pending 行关 superseded（仅在有进行中回合时；无回合的取消不误杀重启恢复的问卷） |
| 新 turn 抢答 | session `handle_submit` 的 superseded 修复关卷 tool call；行由快照自愈（`snapshot_dtos` 发现历史已解决 → superseded） |
| 进程被杀 | `fail_all` **豁免 ask_user**（不 fail，问卷留在持久层）；重启后 `assemble_state` keep-dangling 加载 + `startup_question_maintenance`（过期→合成 EXPIRED_RESULT 补全并落库；已解决→关卷；其余保留） |
| 重启后作答 | 快照 `pending_questions` 回放（`AskUserContext::snapshot_dtos` 回填，server 投影恒空）→ UI 重现问卷卡 |

## 6. 迟到答案（question_reply）与等价不变量

`ClientMsg::question_reply {request_id, action, answers}` →
`MobileQuestionHandler::on_reply`（mobile 装配进
`SessionCore::set_question_handler`）：

1. request_id 解析为 u64 先试 `router.resolve`（在途 → 即时路径，等价不变量
   天然成立）；
2. 迟到路径：行前置校验（pending + tool_call_id 仍悬挂）→ busy 守卫 + take
   manager + running 置位（同锁内）→ **原子关卷**（`transition_pending_question`
   pending→answered/declined，幂等首答者赢；输掉竞态即回收引擎占用返回
   false，调用方定向 notice「已被应答或已关闭」）→ provider 构建（失败回收
   + 行重开 pending 可重试）→ `spawn_resume_turn`：
   `RunManager::resume_from_tool_result` 以**与即时路径同一
   `format_question_result` 输出**补全原 tool result 并从该点续跑
   （`engine::engine_turn` 同款收尾：manager 回家 / 转写折叠 / TurnOk）。

**迟到等价不变量**（设计 §3 工程要点 7）：答案无论即时返回还是迟到补全，
消息历史与工具结果完全一致——迟到的时间信息只进 UI 转写，不进模型上下文。

## 7. 测试

- `askuser.rs` 单测：normalize（默认值/越界/secret storage/timeout 矩阵）、
  下发信封逐字段契约、结果格式化（全形态 + secret 明文不泄露断言）、vault
  行为、启动清扫（TTL expired / 已解决关卷 / 可恢复保留）、审批豁免。
- `runtime.rs` 集成：`ask_user_immediate_path_roundtrip`（模型发调用 → 信封
  → resolve → turn_ok → 行 answered → 工具结果入历史）、
  `ask_user_restart_recovery_and_first_answer_wins`（预置崩溃现场 → 快照
  回放 → question_reply 续跑 → 重复应答首答者赢）。
- `hostcall.rs`：`fail_all` 对 ask_user 豁免（仍可 resolve 证明未死）。
- store crate：问卷行 insert / 原子迁移 / CHECK 约束 / pending 清单。

## 8. secret 子系统（持久化凭据库 / env 注入 / file 写入 / 输出脱敏）

设计 r3 拍板的完整实现。生产装配：`PersistentSecretVault`
（`crates/openslate-mobile/src/secrets.rs`）；trait 接缝
`AskUserSecretVault`（`store` async / `lookup` / `is_remembered` /
`env_pairs` / `known_secret_values`），测试降级用 `InMemorySecretVault`。

### 8.1 凭据库（会话/持久两级）

- **会话级**（提交时未勾「记住」）：仅内存（进程存活期），requestId 域
  语义与旧内存版一致（`(request_id, key)` 唯一、同名最新写入优先）。
- **持久级**（勾选「记住」）：store v4 `credentials` 表（name 主键 =
  env 变量名或 `file:<path>` 键、kind env|file、target、value、时间戳）
  + 内存镜像（`attach_store` 启动加载，此后读路径全走内存——env 注入
  是同步闭包，不能每次 spawn 打库）。会话写覆盖持久值（lookup 最新
  优先）；会话覆盖不继承 remember 标记、不动持久行。
- **加密边界（工程拍板）**：value 明文存 app 私有 sqlite（Android 沙箱
  保护）；**不做** Keystore 级加密。威胁模型：防意外泄漏（回显/日志/
  转写/误分享库文件），**不防** root、调试备份提取、模型诱导 shell 间接
  拼接输出。升级路径：Android Keystore via FFI 包裹对称密钥（value 已
  隔离为单列，替换点集中在 vault 读写两侧）。不引入 SQLCipher。
- **明文面控制**：vault `Debug`/`Display` 手写（只输出计数）；store 层
  `CredentialRecord`（管理面）无 value 列，明文仅
  `get_credential_value`/`load_credentials_with_values` 按需取。

### 8.2 env 注入（shell/bash 执行环境）

注入点选型：`ShellServer`/`BashServer` 由 openslate-app 装配链在
`connect_builtin_servers` 内部构造，构造期传参会波及所有装配层 → 采用
**进程级注入源注册表**（`openslate_mcp_builtin::set_shell_env_source`，
与 core `set_mcp_auth_override` 同款既有模式）：mobile runtime 装配期
注册 `vault.env_pairs` 闭包，命令 spawn 时取最新值（会话内新存凭据即时
可用）。注入在**敏感命名脱敏之后**应用——用户有意存入的 TOKEN 等命名
得以进入子进程，继承环境照旧脱敏。值绝不打日志。mobile 的 native bash
（`NativeShellTool`，`EnvSource` 直传）与 mcp-builtin shell/bash（共享
执行核心 `run_shell_command`）两线都覆盖。**已知边界**：termux
HostTool 壳经 Kotlin 宿主 RUN_COMMAND 执行，env 注入不可达（文档化，
非本 lane 缺口）。

### 8.3 file 写入（storage.kind=file）

secret 提交且 storage 为 file 时 vault 侧写文件：`~` 前缀展开、父目录
按需创建、**权限恒 0600**（create mode + 显式 set_permissions 双保险，
覆盖已存在文件亦收敛）；无论 remember 都写（落地形态是工具契约，
remember 只决定凭据库持久化）；写失败不阻断回填（告警）。路径不做额外
限制——路径来自模型声明、提交前已在 UI 完整展示，用户确认即授权（代码
注释说明）。回填模型仍只给路径引用。删除持久凭据时 best-effort 联动删
除该文件。

### 8.4 输出脱敏（mask_secrets 消费点）

- **core 消费点**：`openslate-core/src/ask_user.rs` 的进程级脱敏器注册表
  （`set_secret_masker` 整体替换 / `push_secret_masker` 追加——mobile
  装配期 push，多 runtime 并行叠加安全）；`mask_tool_output` 作用于
  runtime.rs 工具结果形成处（顺序/并行两路径，output-size 上限之后、
  `tool_end` preview / 消息历史 / tracing preview 之前——模型与日志
  可见面一次全覆盖）。无注册时恒等（桌面端零影响）。
- **脱敏提示追加**（`MASKED_NOTE`）：`mask_tool_output` 在**实际发生
  ≥1 处替换**时于 tool result 内容末尾追加
  `<system-note>Output masked: secret values were replaced with *** by
  the host. Actual values are stored in the credential vault and
  referenced by name; you do not need the plaintext.</system-note>`——
  告知模型值在宿主凭据库、按名引用即可，无需索要明文（防看到 `***`
  后反向追问/尝试还原）。仅实际替换时追加（无替换零噪音）；作为
  tool result 内容的一部分（不注入独立 system 消息）；bytes 随终稿
  重算。
- **mobile 接线**：`create_with` 装配期 push 闭包 =
  `[ask_user] mask_secrets`（配置快照，默认开）短路 + vault
  `known_secret_values()` 子串替换 `***`。仅替换长度 ≥4 的值（过短值
  的子串替换会大面积破坏正常文本——工程取舍）。
- **值源**：会话 + 持久、env + file 全部已知值（文件内容被命令回显同样
  打码）。

### 8.5 设置页接线（Android）

- ask_user 默认超时 / 脱敏开关：设置页（SettingsScreen「内置工具」）每次
  变更经 RuntimeBridge 以协议 `set_ask_user_config` 推送
  （`{"default_timeout":"never"|{"seconds":n},"mask_secrets":bool}`，
  与 core `AskTimeout` serde 形状逐字节一致）；session config_ops 写
  active openslate.toml `[ask_user]` 落盘 + 热替换 + 广播 config_changed。
  本地 SharedPreferences 仅 UI 显示源，启动时（`syncAskUserConfig`）另推
  一次本地值做同步——openslate.toml 为生效真值，两端不漂移。
- 凭据管理列表：Rust 侧已备无明文清单（`PersistentSecretVault::
  list_persistent` + store `list_credentials`）；接 UI 需新增 uniffi
  导出 `listCredentialsJson` / `deleteCredential`（禁新增导出为本 lane
  约束，占位保留）。
- 配置快照随装配固定：协议写回后 AppState 的 config 镜像即时热替换、
  toml 已落盘；但 mobile `AskUserContext` 的配置快照（工具超时 /
  mask_secrets 消费点）随装配固定——**重启后生效**（后续 lane 可在写回
  顺带 refresh）。

### 8.6 测试

- `secrets.rs`：会话/持久分级（含跨实例重启恢复）、env_pairs 合并覆盖、
  file 写入 0600（含覆盖宽松文件收敛）、delete 联动、mask（含短值跳过）、
  Debug 不泄露。
- `exec.rs`：native bash env 注入（注入生效 + 继承脱敏仍有效 + 覆盖
  被脱敏的继承名）。
- mcp-builtin `shell.rs`：注入源注入/覆盖/无源脱敏（单测试两阶段防全局
  竞态）。
- core `ask_user.rs`/`runtime.rs`：注册表替换/追加/清空 + 工具结果进
  历史前脱敏（端到端）。
- `runtime.rs` 集成：`ask_user_secret_env_injection_and_masking_roundtrip`
  ——secret 提交（remember）→ 凭据行落库 → 后续 bash 子进程拿到注入值 →
  历史/转写只见 `***`（明文两处断言均不含）。
