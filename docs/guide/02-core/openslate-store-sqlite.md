# 02-4 · openslate-store-sqlite

> SQLite 持久化 crate（`crates/openslate-store-sqlite/src/`，5 个模块）：schema（DDL + 版本化迁移）、store（连接管理）、write（全部写操作）、query（只读查询 + 记录类型）、recorder（逐消息持久化适配器）。sqlx 0.9（sqlite-bundled），**不用宏**——动态 SQL 走 `AssertSqlSafe`。依赖 core（错误类型 + MessageSink + Message）。

## 1. schema.rs —— DDL + 版本化迁移（384 行）

### 版本常量与历史

```rust
// :26
pub const SCHEMA_VERSION: i64 = 4;
```

- **v1 基线**：7 张表 + 索引（等价旧无版本机制的最终形态；基线 DDL 已含 CHECK 约束、不含 `runs.cwd`）。
- **v2**：`runs.title` 列 + `messages(run_id, seq)` 唯一索引 + 清理死列 `runs.cwd`/死索引 `idx_runs_cwd`（后两步 best-effort，失败仅告警）。
- **v3**：新增 `pending_questions` 表 + `idx_pending_questions_status`（ask_user 问卷持久化，ask-user 设计 r4/r5）。
- **v4**：新增 `credentials` 表（ask_user secret 持久凭据库，见下）。

### 9 张表

| 表 | DDL 常量 | 关键列与约束 |
|----|----------|--------------|
| `runs` | `DDL_RUNS`（:66） | id PK / title? / root_agent_id / **status CHECK IN ('running','completed','interrupted','error','failed','cancelled')**（`error` 预留；实际写入终态为 completed/interrupted/failed/cancelled，:63-65 注释）/ input_json / output_json? / started_at / finished_at? / cost_usd REAL DEFAULT 0 |
| `execution_nodes` | :80 | id PK / run_id FK→runs / agent_id / parent_execution_id? / parent_call_id? / status / input_json / started_at / finished_at? |
| `steps` | :96 | id PK / run_id FK / execution_node_id FK→execution_nodes / agent_id / kind / data_json / seq / started_at / finished_at? |
| `messages` | :112 | id PK / run_id FK / execution_node_id FK→execution_nodes / agent_id? / **role CHECK IN ('system','user','assistant','tool')** / content_json（core Message 整体 JSON）/ seq / created_at |
| `prompt_snapshots` | :127 | id PK / run_id FK / execution_node_id FK / agent_id / profile_name / source_kind / source_path? / content_hash / rendered_prompt / created_at |
| `audit_log` | :144 | id PK / run_id?（无 FK——审计行可先于 run）/ agent_id? / event_type / event_json / created_at |
| `trace_events` | :155 | id PK / run_id FK→runs / execution_node_id? / step_id? / agent_id? / event_name / event_kind / ts_ns / dur_ns? / track / args_json? |
| `pending_questions` | v3（`DDL_PENDING_QUESTIONS`） | request_id TEXT PK（host call id，幂等主键）/ tool_call_id（关联消息历史悬挂调用；**无 FK**——行按 request_id 寻址且随 TTL 24h 过期，孤儿行无害）/ questions_json（归一化负载）/ **status CHECK IN ('pending','answered','declined','superseded','expired')** / version / created_at |
| `credentials` | v4（`DDL_CREDENTIALS`） | name TEXT PK（凭据键：env 变量名或 `file:<path>`）/ **kind CHECK IN ('env','file')** / target（变量名/路径）/ value（**明文**——加密边界与威胁模型见 DDL 注释：app 私有 sqlite + Android 沙箱，防意外泄漏不防 root/备份提取；升级路径 Android Keystore via FFI）/ created_at / updated_at（upsert 刷新，created 首写固定）。**无 FK**——凭据跨会话存续，与 run 无从属。读写方法刻意分层：`list_credentials()`/`CredentialRecord` **无明文**（管理面），`get_credential_value()`/`load_credentials_with_values()` 明文仅 vault 消费 |

建表顺序有意义：被外键引用的表先建（`ddl_statements()` :31 返回序即此序）。

### 索引

- 基线（`index_statements()` :47，全部 `IF NOT EXISTS` 幂等）：`idx_runs_status`（按状态过滤）/ `idx_runs_started_at`（时间排序）/ `idx_steps_run_id` / `idx_messages_execution_node_id` / `idx_messages_run_seq`（**resume 路径**：按 seq 加载完整会话）/ `idx_trace_events_run_id` / `idx_audit_log_run_id`。
- v2 新增：`idx_messages_run_seq_uq`（:227 唯一索引；存量重复 seq 创建失败仅告警跳过——best-effort 不阻断迁移；写入端靠派生主键约定兜底）。

### 迁移机制（`SqliteStore::run_migrations` 驱动）

- `PRAGMA user_version` 记录当前版本；目标 = `SCHEMA_VERSION`。
- **每版本步进独立事务**：`user_version` 与 DDL 同事务提交——崩溃后重跑不会出现半迁移。
- v0 兼容：DDL 全部 `IF NOT EXISTS`；`ALTER TABLE` 前先经 `pragma_table_info` 列存在检查（`column_exists` :274——**不靠错误文案字符串匹配**；表名只来自内部常量，无注入风险）。
- 步进函数：`migrate_v0_to_v1`（:292：DDL → `BASELINE_ALTER_STEPS`（:180，runs.cost_usd / messages.seq / steps.seq 三列补齐）→ 索引）/ `migrate_v1_to_v2`（:316）/ `migrate_v2_to_v3`（`pending_questions` 表 + 状态索引，全部 IF NOT EXISTS 幂等）/ `migrate_v3_to_v4`（`credentials` 表，IF NOT EXISTS 幂等，无附加索引——name 主键即全部访问路径）。
- 高版本库报 `MigrationError`（"请先升级程序"）。
- 测试常量：`TABLE_NAMES`（:243）/ `INDEX_NAMES`（:254）+ CHECK 约束防漂移测试（:376）。

**新表/新列变更流程**：新 `SCHEMA_VERSION` 步进 + 独立事务函数（`migrate_vN_to_vN+1`）+ 同步 `TABLE_NAMES`/`INDEX_NAMES`。

## 2. store.rs —— 连接管理（1022 行，测试占大半）

```rust
// :27
#[derive(Clone)]                       // 廉价：池基于 Arc
pub struct SqliteStore { pool: SqlitePool }
```

- **PRAGMA 全走 `SqliteConnectOptions`**（:78-86）：WAL + synchronous Normal + busy_timeout 5s + foreign_keys on——连接池因 idle_timeout/max_lifetime 回收重建连接后**依然生效**（不会静默回退默认值）。
- 构造必须 `filename()` + `create_if_missing(true)`，**不拼 `sqlite:{path}?mode=rwc` URL**（路径含 `?`/`#` 会错乱，:75-77 注释）。
- **池配置单连接**（max=min=1）：SQLite 写并发收益有限 + 避免 `database is locked`。
- `new(path)`（:121）：建池 → `restrict_db_file_permissions`（:45，库文件 + WAL/-shm 旁车收紧 0600——库里有明文会话历史；best-effort，失败仅告警）。
- `new_in_memory()`（:104）：**仅测试**。单连接永不回收（idle_timeout/max_lifetime 均 None）——否则连接重建后整库"丢失"（每条新连接都是全新空库）。内存库不支持 WAL（:90 注释）。
- `user_version()`（:137）/ `run_migrations()`（:148，逐版本步进，见 §1）/ `verify_pragma() -> PragmaState`（:193，journal_mode/synchronous/foreign_keys/busy_timeout 快照）/ `pool()`（:230）。

## 3. write.rs —— 全部写操作（1143 行）

生命周期与写入 API（`impl SqliteStore`）：

| 函数 | 行 | 语义 |
|------|----|------|
| `insert_run` | :20 | 基础插 run 行 |
| `insert_run_with_persist_node` | :53 | **单事务**插 run 行 + recorder 执行节点（消除两语句间崩溃窗口；`RunRecorder::begin` 的底层） |
| `update_run_title` | :97 | LLM 标题替换占位（UPDATE 零行也成功——run 可能已删） |
| `update_run_status` / `update_run_cost` / `activate_run` | :123/:154 | resume 激活路径用 |
| `delete_run` | :218 | 级联删除该 run 的全部数据 |
| `prune_runs_keep_last(keep_n) -> Vec<String>` | :248 | 生命周期清理，返回被删 run id |
| `mark_running_runs_interrupted(now_ms) -> u64` | :272 | 启动时把僵尸 running 行标记为 interrupted |
| `wal_checkpoint` / `count_runs` | :285/:294 | |
| `insert_execution_node` / `insert_step` / `insert_message` / `insert_prompt_snapshot` / `insert_audit_event` / `insert_trace_event` | :302/:336/:373/:404/:440/:466 | 各表 insert（消息主键由调用方派生） |

执行树/steps/trace/audit 的整树写入发生在 **run 结束后由 CLI 侧**调用（不在 recorder 路径上）。

ask_user 问卷持久化（v3）：

| 函数 | 语义 |
|------|------|
| `insert_pending_question(request_id, tool_call_id, questions_json, version, created_at)` | 插 pending 行（request_id = host call id 幂等主键） |
| `transition_pending_question(request_id, from, to) -> bool` | **原子状态迁移（幂等首答者赢）**：仅当当前状态 = `from` 才改 `to`；返回 false = 状态不匹配/不存在。任何状态对可表达（含 answered→pending 回滚重开） |

ask_user 持久凭据（v4）：

| 函数 | 语义 |
|------|------|
| `upsert_credential(name, kind, target, value, now_ms)` | name 主键 upsert（remember=true 的 secret 提交落这里；updated_at 刷新、created_at 首写固定） |
| `delete_credential(name) -> bool` | 幂等删除（不存在 false）；会话内存条目/已写文件由 vault 侧联动清理 |

## 4. query.rs —— 只读查询 + 记录类型（1259 行）

记录类型（serde derive，REST/CLI 消费）：`RunRecord`（:18，含 title?/cost_usd）/ `ExecutionNodeRecord`（:33）/ `StepRecord`（:48）/ `MessageRecord`（:65——**seq 字段注释钉死语义**：per-run 单调、readers 按 it 排序、`created_at` 无法排同批插入、provider 拒绝乱序 tool_calls 对）/ `PromptSnapshotRecord`（:81）/ `AuditLogRecord`（:96）/ `TraceEventRecord`（:107）/ `PendingQuestionRecord`（v3，ask_user 问卷行——status 小写词表与协议 `QuestionStatus` 的映射由 mobile 装配层负责）/ `CredentialRecord`（v4，**无明文**——name/kind/target/created_at/updated_at，设置页管理面用）。

问卷查询：`list_pending_questions()`（status='pending' 按 created_at 升序——快照回填/迟到校验数据源）/ `get_pending_question(request_id)`（单条任意状态）。

凭据查询：`list_credentials()`（无明文清单，按 name 字典序）/ `get_credential_value(name)`（单条明文，vault 注入/取值用）/ `load_credentials_with_values()`（全部行含明文——**仅 vault 启动镜像加载消费**，结果不进日志/Debug）。

查询 API：

| 函数 | 行 | 语义 |
|------|----|------|
| `get_run(id)` | :325 | |
| `list_runs(limit, offset)` | :344 | 历史列表（按 started_at） |
| `get_execution_node` / `list_steps(run_id)` | :360/:380 | |
| `list_messages(execution_node_id)` | :395 | 按节点 |
| **`list_messages_by_run(run_id)`** | :419 | **resume 路径**：按 seq 读回完整会话 |
| `list_messages_by_run_after_seq(run_id, seq)` | :442 | 增量读 |
| `count_messages_by_run` / `max_message_seq` | :464/:476 | resume 续 seq 依据 |
| `get_prompt_snapshot` / `list_audit_logs` / `list_trace_events` | :487/:510/:525 | |
| `get_last_interrupted_run` / `get_last_resumable_run` | :545/:564 | resume 入口发现 |

## 5. recorder.rs —— RunRecorder（640 行，测试占大半）

```rust
// :31
pub struct RunRecorder {
    store: SqliteStore,
    run_id: RunId,
    execution_node_id: String,   // 恒 "{run_id}-persist"（:81）
    agent_id: String,            // resume 优先用 run 行记录的 root agent（树可能已变）
    last_seq: AtomicI64,         // sink 与 CLI/session 侧写共享一个单调计数器
    activated: AtomicBool,       // resume 过的 recorder 是否已置回 running
}
```

### 生命周期

| 阶段 | 方法 | 行为 |
|------|------|------|
| 开始 | `begin(store, run_id, root_agent_id, title?, input_json)`（:88） | **单事务**插 run 行（status=running，title=占位）+ persist 执行节点——**run 在任何 LLM 调用前就存在**；activated=true |
| 接管 | `resume(store, run_id, root_agent_id)`（:118） | run 行必须已存在；persist 节点幂等按需补建；seq 续自 `max_message_seq`；**不改 status**（保持 interrupted——避免 resume 后尚未写消息就再次中断时留下新的僵尸 running 行）；activated=false |
| 逐条 | `write_message(&Message)`（:168） | fallible 孪生；首次写入时一次性激活（`activated.swap` 去重，避免每条消息发 UPDATE）；seq = `last_seq.fetch_add(1)+1`；主键 `msg-{run_id}-{seq}`；content = core Message 整体 JSON |
| 落盘 | `impl MessageSink::append`（:246） | 调 write_message 但**吞错 warn**（持久化永不杀死 run） |
| 终态 | `finish(status, output_json?, cost_usd)`（:199） | status/output/finished_at/cost 四项**单事务**落库（避免"已 completed 但成本缺失"半状态）；status 用小写词汇 |
| 读回 | `load_messages(store, run_id)`（:214） | 按 seq 读回 → 解码 core Message；**尾部悬空 tool_calls 的 assistant 消息被丢弃**（:229-241——provider 拒绝，模型重新决策） |
| 读回（保留悬挂） | `load_messages_keep_dangling(store, run_id)` | 同上但**不丢**尾部悬挂 assistant 消息——ask_user 重启恢复用（悬挂调用要留给迟到答案补全，ask-user r5）；调用方（mobile 装配层）自行决定丢弃或悬挂修复 |

### 关键设计点

- **"落盘后再 continue"**：sink 在 execute_run 循环内联 await——崩溃/取消/退出总留可恢复的部分转写（崩溃窗口 = 当前正在生成的那条）。
- **seq 是 load-bearing**：run 内唯一（v2 唯一索引兜底 + 派生主键双保险）；写入端约定优先于约束兜底——不要依赖"唯一索引创建失败仅告警"路径写出重复 seq。
- persist 节点 vs runtime 树：recorder 的消息挂 `{run_id}-persist` 确定性节点（仅为满足 messages FK）；runtime 自己的 `en-{uuid}` 执行树 run 后单独写，两套互不干扰。
- **每 run 一个 recorder、可跨回合**（REPL 会话复用同一 run id 下多个 turn；session 侧 `SessionRun{run_id, recorder: Arc<RunRecorder>}` 惰性建立、/new 关闭）。

## 6. 协作速查（四 crate）

```text
openslate-core          领域模型 + 引擎（不知道有 DB/协议/传输）
    ▲    ▲    ▲
    │    │    └──────────────┐
    │    │                   │
store-sqlite          openslate-protocol        （都只依赖 core）
    RunRecorder             ClientMsg/ServerMsg/DTO
    impl MessageSink        纯数据，两侧共用
    ▲                       ▲
    └───────────┬───────────┘
                ▼
         openslate-session（+ 依赖 openslate-app 装配链）
           SessionCore 持 RunManager（core）+ SqliteStore（store）
           + transcript: Vec<EntryDto>（protocol）
           出站 ServerMsg 经 MsgSink；入站 ClientMsg 进 session::dispatch
                ▲
         ┌──────┴──────┐
    openslate-server  openslate-mobile
    ConnectionHub      EventSink → FFI callback
```

## 7. 注意事项汇总

1. **seq 单调且 run 内唯一**是 resume 正确性的根基（重复 seq → 加载顺序不稳定 → provider 拒绝乱序 tool_calls 对）。
2. 新表/新列 = 新 `SCHEMA_VERSION` 步进 + 独立事务 + 同步测试常量。
3. `RunRecorder` 的 `append` 吞错——需要报错的调用方（如写首条用户消息）用 `write_message`。
4. 查询侧 `MessageRecord.content_json` 是 core `Message` 的整体序列化（含 `reasoning_content`/`tool_calls`），解码零损。
5. 库文件权限 0600 是明文会话历史的最低防线（含 WAL/-shm 旁车）。
