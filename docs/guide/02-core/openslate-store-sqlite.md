# 02-4 · openslate-store-sqlite

> SQLite 持久化 crate（`crates/openslate-store-sqlite/src/`，6 个模块）：schema（DDL + 版本化迁移）、store（连接管理）、write（全部写操作）、query（只读查询 + 记录类型）、recorder（逐消息持久化适配器）、codec（v5 压缩列编解码）。sqlx 0.9（sqlite-bundled），**不用宏**——动态 SQL 走 `AssertSqlSafe`。依赖 core（错误类型 + MessageSink + Message/MessageMeta）+ zstd 0.13。
>
> **当前 schema 版本：v7**（v5 多模态改造 + v6 长输出落盘 `tool_outputs` 表 + v7 `ref` 列/会话隔离）。

## 1. schema.rs —— DDL + 版本化迁移（640 行）

### 版本常量与历史

```rust
// :44
pub const SCHEMA_VERSION: i64 = 7;
```

- **v1 基线**：7 张表 + 索引（等价旧无版本机制的最终形态；基线 DDL 已含 CHECK 约束、不含 `runs.cwd`）。
- **v2**：`runs.title` 列 + `messages(run_id, seq)` 唯一索引 + 清理死列 `runs.cwd`/死索引 `idx_runs_cwd`（后两步 best-effort，失败仅告警）。
- **v3**：新增 `pending_questions` 表 + `idx_pending_questions_status`（ask_user 问卷持久化，ask-user 设计 r4/r5）。
- **v4**：新增 `credentials` 表（ask_user secret 持久凭据库，见下）。
- **v5**（存储 v5 设计）：
  1. `messages` **整表重建**：`content_json TEXT` → `content_zst BLOB NOT NULL`（zstd level 3，见 §2 codec）+ 新增 `meta_json TEXT`（per-message 元数据旁路通道）；
  2. `runs` 加 `input_tokens` / `output_tokens` 聚合列（INTEGER NOT NULL DEFAULT 0）；
  3. 新表 `attachments` 与 `transcript_entries`（+2 索引）；
  4. 非唯一索引 `idx_messages_run_seq` **删除不重建**（`idx_messages_run_seq_uq` 最左前缀 `(run_id, seq)` 完全覆盖其查询计划）。
- **v4→v5 是破坏性迁移**（用户拍板 2026-10-21：软件未发布开发期）：不复制旧数据——`DROP TABLE messages` 后直接以最终 DDL 重建，旧行历史丢弃；存量脏数据（v2 时代重复 seq）一并消失，新表 `UNIQUE(run_id, seq)` 必然可建。`runs` / `credentials` / `pending_questions` 原样保留（API key、会话列表不丢）。无 FK 引用 messages，重建无级联风险，无需临时表 + RENAME。
- **v6**（长输出落盘 + output_query 设计 C1）：新表 `tool_outputs`（+ `idx_tool_outputs_sha256` 索引）。超限工具输出全文的 content-addressed 落盘（压缩字节在 `files/outputs/{sha256}.zst`，DB 存引用行 + 计量字段）。纯新增、无数据搬移（v6 前的超限输出已被截断丢弃，无从恢复）。
- **v7**（会话内短 ref + 严格会话隔离，用户拍板 2026-10-22）：`tool_outputs` **破坏性重建**为带 `ref` 列的最终形态——ref 是**会话内唯一**短 id（`out-N`，宿主 storer 按 run 内自增序号分配）+ `UNIQUE INDEX idx_tool_outputs_run_ref(run_id, ref)`。**破坏性**（开发期拍板 2026-10-22，用户本地数据可弃）：DROP 整表重建、存量行丢弃、不做回填兼容——长输出缓存可弃，旧消息里的 16hex 标记查无行即走既有降级路径（弹窗回落预览原文 / output_query 报未知 ref），孤儿文件由启动 GC 清扫。

### 12 张表

| 表 | DDL 常量 | 关键列与约束 |
|----|----------|--------------|
| `runs` | `DDL_RUNS`（:88） | id PK / title? / root_agent_id / **status CHECK IN ('running','completed','interrupted','error','failed','cancelled')**（`error` 预留；实际写入终态为 completed/interrupted/failed/cancelled）/ input_json / output_json? / started_at / finished_at? / cost_usd REAL DEFAULT 0 / **input_tokens INTEGER NOT NULL DEFAULT 0（v5）** / **output_tokens INTEGER NOT NULL DEFAULT 0（v5）**——run 级 token 聚合，存量 run 无 meta → 保持 0 不回填 |
| `execution_nodes` | :102 | id PK / run_id FK→runs / agent_id / parent_execution_id? / parent_call_id? / status / input_json / started_at / finished_at? |
| `steps` | :118 | id PK / run_id FK / execution_node_id FK→execution_nodes / agent_id / kind / data_json / seq / started_at / finished_at? |
| `messages` | :142（v5 形态） | id PK / run_id FK / execution_node_id FK / agent_id? / **role CHECK IN ('system','user','assistant','tool')** / **content_zst BLOB NOT NULL**（`serde_json::to_string(Message)` 的 zstd-3 压缩字节，见 §2）/ **meta_json TEXT?**（`Option<MessageMeta>` serde JSON；assistant 行的 usage/finish_reason/latency/model_id，user/tool 行 NULL）/ seq / created_at |
| `prompt_snapshots` | :158 | id PK / run_id FK / execution_node_id FK / agent_id / profile_name / source_kind / source_path? / content_hash / rendered_prompt / created_at |
| `audit_log` | :175 | id PK / run_id?（无 FK——审计行可先于 run）/ agent_id? / event_type / event_json / created_at |
| `trace_events` | :186 | id PK / run_id FK→runs / execution_node_id? / step_id? / agent_id? / event_name / event_kind / ts_ns / dur_ns? / track / args_json? |
| `pending_questions` | v3（:208） | request_id TEXT PK（host call id，幂等主键）/ tool_call_id（关联消息历史悬挂调用；**无 FK**——行按 request_id 寻址且随 TTL 24h 过期，孤儿行无害）/ questions_json / **status CHECK IN ('pending','answered','declined','superseded','expired')** / version / created_at |
| `credentials` | v4（:240） | name TEXT PK（凭据键：env 变量名或 `file:<path>`）/ **kind CHECK IN ('env','file')** / target / value（**明文**——加密边界与威胁模型见 DDL 注释：app 私有 sqlite + Android 沙箱，防意外泄漏不防 root/备份提取；升级路径 Android Keystore via FFI）/ created_at / updated_at（upsert 刷新，created 首写固定）。**无 FK**。读写方法刻意分层：`list_credentials()`/`CredentialRecord` **无明文**（管理面），`get_credential_value()`/`load_credentials_with_values()` 明文仅 vault 消费 |
| `attachments` | v5（:263） | id TEXT PK（`att-<uuid>`）/ **run_id?**（NULL = 已导入未绑定，composer 暂存；Submit 时 `bind_attachments` 批量绑定。SQLite FK 对 NULL 父值放行）/ sha256 NOT NULL / mime NOT NULL（归一化后，白名单见 `mime_extension`）/ filename? / size_bytes NOT NULL（归一化后字节数）/ created_at / **UNIQUE(run_id, sha256)**（run 内去重；跨 run 复用靠相同 sha256 指向同一文件）/ FK(run_id)→runs。**字节不进 DB**——content-addressed 落 `files/{sha256}.{ext}`，见 §3 |
| `transcript_entries` | v5（:284） | id TEXT PK / run_id NOT NULL FK→runs / seq（mobile 层收口 helper 分配，run 内唯一）/ **entry_json TEXT NOT NULL**（`EntryDto` serde JSON **透传**，store 不解析；不压缩——单条几百字节负收益）/ created_at。与 `steps` 的语义边界：steps 是执行步骤，Approval/Delegate/StepBreak/Meta 不是；行级写入天然增量，回合中途崩溃不丢整回合 UI 条目 |
| `tool_outputs` | v6（v7 加 ref 列重建，DDL_TOOL_OUTPUTS） | id TEXT PK（`out-<uuid simple>`）/ run_id NOT NULL FK→runs / **ref TEXT NOT NULL（v7——会话内唯一短 id `out-N`，宿主 storer 按 run 自增序号分配）** / sha256 NOT NULL（全 64 hex）/ tool_name / **orig_bytes**（压缩前原文字节数）/ **line_count**（原文行数）/ created_at / **UNIQUE(run_id, sha256)** + **UNIQUE INDEX (run_id, ref)**。生命周期对齐 attachments 的 content-addressed 模式但**无 bind/引用计数**：行按 run 独立、文件按 sha 跨 run 共享于 `files/outputs/{sha256}.zst`（调用方传入已压缩字节） |

建表顺序有意义：被外键引用的表先建（`ddl_statements()` :51 返回序即此序；v5 两张新表只引用 runs，追加在尾部）。

### 索引

- 基线（`index_statements()` :70，全部 `IF NOT EXISTS` 幂等）：`idx_runs_status` / `idx_runs_started_at` / `idx_steps_run_id` / `idx_messages_execution_node_id` / `idx_trace_events_run_id` / `idx_audit_log_run_id`。
- v2 新增：`idx_messages_run_seq_uq`（:353 唯一索引；存量重复 seq 创建失败仅告警跳过）。
- v3：`idx_pending_questions_status`。
- **v5 新增**：`idx_attachments_sha256`（delete_run / 启动 GC 的行级 `SELECT 1 WHERE sha256=?` 走此索引）、`idx_transcript_run_seq`（**UNIQUE**）。
- **v6 新增**：`idx_tool_outputs_sha256`（delete_run / 启动 GC 的行级引用判定走此索引）。
- **v7 新增**：`idx_tool_outputs_run_ref`（**UNIQUE**，`(run_id, ref)`——`find_tool_output` 精确匹配走此索引；run 内唯一性兜底，分配器在 mobile storer 的串行路径上）。
- **v5 删除**：`idx_messages_run_seq`（非唯一）——随 `DROP TABLE messages` 消亡且不重建，uq 最左前缀完全覆盖。

### 迁移机制（`SqliteStore::run_migrations` 驱动）

- `PRAGMA user_version` 记录当前版本；目标 = `SCHEMA_VERSION`。
- **每版本步进独立事务**：`user_version` 与 DDL 同事务提交——崩溃后重跑不会出现半迁移。
- v0 兼容：DDL 全部 `IF NOT EXISTS`；`ALTER TABLE` 前先经 `pragma_table_info` 列存在检查（`column_exists` :426）。
- 步进函数：`migrate_v0_to_v1`（:444）/ `migrate_v1_to_v2`（:468）/ `migrate_v2_to_v3`（:518）/ `migrate_v3_to_v4`（:534）/ **`migrate_v4_to_v5`（:558，单事务内 7 步：DROP messages → 最终 DDL 重建 → 建 uq + execution_node_id 两索引 → runs 两列 ALTER（column_exists 守卫）→ attachments/transcript_entries 建表建索引 → user_version=5 由驱动器写入）** / `migrate_v5_to_v6`（tool_outputs 建表建索引，纯新增幂等）/ **`migrate_v6_to_v7`（破坏性：DROP tool_outputs → 最终 DDL 重建（含 ref 列）→ 建 sha256 + run_ref 两索引；存量行丢弃不回填，幂等——全新库重放为空转）**。v4→v5 步进事务提交后驱动器做一次 `wal_checkpoint(TRUNCATE)` 回收 WAL（best-effort，失败仅告警）——迁移 O(1) DDL 无数据复制，无耗时风险。
- 全新库路径：基线 `DDL_MESSAGES` 直接是 v5 最终形态、`DDL_TOOL_OUTPUTS` 直接是 v7 最终形态，逐版本步进幂等收敛到 v7。
- 高版本库报 `MigrationError`（"请先升级程序"）。
- 测试常量：`TABLE_NAMES`（12 表）/ `INDEX_NAMES`（12 索引）+ CHECK 约束防漂移测试。
- **回滚策略：无**（与既有 4 个版本一致）。

**新表/新列变更流程**：新 `SCHEMA_VERSION` 步进 + 独立事务函数（`migrate_vN_to_vN+1`）+ 同步 `TABLE_NAMES`/`INDEX_NAMES` + 索引计数测试。

## 2. codec.rs —— content_zst 编解码（94 行）

空间热点集中在 `messages.content_json`（reasoning 全文 + 64KB 截断的工具输出），v5 起整表重建为压缩列（设计 §1：只压这一张表的一个载荷列，zstd-3 典型压缩比 5–10×，64KB 文本压缩 <1ms）。

```rust
// :19 / :29
pub fn encode_content(content: &str) -> Vec<u8>;              // zstd level 3，纯内存不返回 Result
pub fn decode_content(message_id: &str, data: &[u8]) -> Result<String, StoreError>;
```

- zstd 帧自带 magic（`0x28 B5 2F FD`），列内数据自描述，无需标志列。
- **decode 失败绝不静默**：错误信息携带消息 id（调用方传入定位用）；损坏帧 / 非 UTF-8 → `StoreError::QueryError`。
- 压缩等级 3 与 mobile `registry.rs::zstd_level()` 一致（zstd 0.13，Android 交叉编译已验证）。

## 3. store.rs —— 连接管理（1287 行，测试占大半）

```rust
// :27
#[derive(Clone)]                       // 廉价：池基于 Arc
pub struct SqliteStore { pool: SqlitePool, db_path: Option<String> }
```

- **PRAGMA 全走 `SqliteConnectOptions`**（:81）：WAL + synchronous Normal + busy_timeout 5s + foreign_keys on——连接池回收重建连接后**依然生效**。
- 构造必须 `filename()` + `create_if_missing(true)`，**不拼 `sqlite:{path}?mode=rwc` URL**（路径含 `?`/`#` 会错乱）。
- **池配置单连接**（max=min=1）：SQLite 写并发收益有限 + 避免 `database is locked`。
- `new(path)`（:127）：建池 → `restrict_db_file_permissions`（0600，含 WAL/-shm 旁车）→ 记录 `db_path`。
- `new_in_memory()`（:107）：**仅测试**。单连接永不回收；`db_path = None`。
- **`files_dir() -> Option<PathBuf>`（:151，v5）**：`{db 文件父目录}/files`——附件 content-addressed 文件布局的根（`files/{sha256}.{ext}`，**不按 run_id 分目录**，跨 run 按 sha 复用天然去重）。内存库返回 `None` → `insert_attachment` 明确报错。三个前端的 db 都落在各自 data 目录，派生零管线成本。
- `user_version()`（:161）/ `run_migrations()`（:172，逐版本步进 + v5 后 checkpoint，见 §1）/ `verify_pragma() -> PragmaState`（:232）/ `pool()`（:269）。

## 4. write.rs —— 全部写操作（1901 行，测试占大半）

生命周期与写入 API（`impl SqliteStore`）：

| 函数 | 行 | 语义 |
|------|----|------|
| `insert_run` | :20 | 基础插 run 行 |
| `insert_run_with_persist_node` | :53 | **单事务**插 run 行 + recorder 执行节点（`RunRecorder::begin` 的底层） |
| `update_run_title` | :97 | LLM 标题替换占位 |
| `update_run_status` / `update_run_cost` / `activate_run` | :123/:154/:109 | resume 激活路径用 |
| `finish_run(id, status, output?, finished_at, cost, input_tokens, output_tokens)` | :172（pub(crate)） | 终态单事务落库（v5 起 token 聚合并入同一 UPDATE；`RunRecorder::finish_with` 的底层） |
| `delete_run(run_id) -> u64` | :231 | 级联删除全部子表（**v5 起含 attachments / transcript_entries，v6 起含 tool_outputs**，单事务按依赖序）；**事务提交后孤儿文件清扫**（附件 + 长输出两路，见下） |
| `sweep_orphan_files(sha256s)` | :274（私有） | 逐 sha256 行级 `SELECT 1 FROM attachments WHERE sha256=?`，无行引用则删 `files/{sha256}.*`（前缀匹配，ext 未知）；非全表 diff |
| `sweep_orphan_output_files(sha256s)` | v6（私有） | 同构附件版但目录不同：逐 sha256 行级 `SELECT 1 FROM tool_outputs WHERE sha256=?`，无引用删 `files/outputs/{sha}.zst` |
| `gc_stale_unbound_attachments(now_ms) -> u64` | :317 | **启动暂存清扫**：删 `run_id IS NULL AND created_at < now-7d` 的 attachments 行 + 对应文件（用户选图未发送的暂存）；绑定行与 7 天内暂存不受影响。调用时机归 L4（app 启动装配） |
| `gc_orphan_output_files() -> u64` | v6 | **长输出启动 GC**：read_dir(`files/outputs`) 全量 ↔ `SELECT DISTINCT sha256 FROM tool_outputs`，删无行引用文件（收敛「文件写成功、行未落」的崩溃窗口）。mobile 装配启动序列调用，失败仅告警 |
| `prune_runs_keep_last(keep_n) -> Vec<String>` | :344 | 生命周期清理（内部走 delete_run，附件 GC 随行） |
| `mark_running_runs_interrupted(now_ms)` | :368 | 启动僵尸 running 标记 |
| `wal_checkpoint` / `count_runs` | :381/:390 | |
| `insert_execution_node` / `insert_step` / `insert_prompt_snapshot` / `insert_audit_event` / `insert_trace_event` | :398/:432/:635/:671/:697 | 各表 insert |
| **`insert_message(id, run_id, node, agent?, role, content_zst: &[u8], meta_json: Option<&str>, seq, created_at)`** | :474 | **v5 签名**：载荷是 `codec::encode_content` 的压缩字节 + meta 旁路 JSON（该 API 实际只有 recorder 一个调用方，直接改签名） |

v5 附件 API（字节 content-addressed 落 files/，DB 存引用）：

| 函数 | 行 | 语义 |
|------|----|------|
| `mime_extension(mime)` | :510（私有） | mime→ext 白名单映射：jpg/png/webp/gif/mp4/mkv/mov（`image/jpeg`→jpg、`video/x-matroska`→mkv、`video/quicktime`→mov 等 7 项）；未映射 mime 报错 |
| `insert_attachment(id, run_id?, sha256, mime, filename?, size_bytes, data: &[u8], created_at)` | :532 | 先写文件 `files/{sha256}.{ext}`（幂等覆盖），再插行。内存库（无 files_dir）或未映射 mime → **明确报错 fail fast**。sha/size 由调用方（mobile ingest）计算保证，store 信任不复算 |
| `bind_attachments(ids, run_id) -> u64` | :584 | 批量把暂存附件绑定到 run（Submit 门控）；不存在的 id 静默跳过，返回实际更新行数 |

v5 转写 API：

| 函数 | 行 | 语义 |
|------|----|------|
| `insert_transcript_entry(id, run_id, seq, entry_json, created_at)` | :611 | 一行一条 `EntryDto`（JSON 透传不解析）；**`INSERT OR IGNORE`**——同 run 同 seq 冲突静默让位首写行（历史转写丢失修复 C1：自举读失败按 0 重插的场景从「唯一索引致命告警死循环」降级为幂等补写；同 run 同 seq 内容确定性地来自同一转写下标，按 attempted 推进游标安全） |
| `delete_messages_from_seq(run_id, from_seq) -> u64` | 新增（write.rs） | 删该 run `seq >= from_seq` 的尾部消息行（**悬挂尾巴 pop 回写，C2**：open_session / 重启恢复 pop 悬挂 assistant 后同步删行）；返回删除行数 |
| `delete_transcript_entries_for_run(run_id) -> u64` | 新增（write.rs） | 删该 run 全部转写条目行（**悬挂 pop 重建清场，C2**）：全删 + 调用方失效 SyncState 游标 → sync_loop 从 0 全量重写（幂等） |

v6/v7 长输出 API（压缩字节 content-addressed 落 `files/outputs/`，DB 存引用 + ref）：

| 函数 | 语义 |
|------|------|
| `write_tool_output_file(outputs_dir, sha256, data_zst)`（私有） | 幂等写 `files/outputs/{sha256}.zst`（存在即跳过，跨 run 共享） |
| `insert_tool_output(id, run_id, ref_id, sha256, tool_name, orig_bytes, line_count, data_zst: &[u8], created_at) -> String` | 先落文件再插行；`data_zst` 是**调用方已压缩字节**（sha 为原文 sha，store 信任不复算）；同 run 同 sha 幂等（`ON CONFLICT(run_id, sha256) DO UPDATE SET created_at`）并**返回既有行的 ref**（`RETURNING`，调用方拿真实 ref 构造回执）；同 run 同 ref 撞不同 sha 防御性报错（DO NOTHING 不覆盖）；内存库明确报错 fail fast |

ask_user 问卷持久化（v3）：`insert_pending_question`（:738）/ `transition_pending_question(request_id, from, to) -> bool`（:766，**原子状态迁移，幂等首答者赢**）。

ask_user 持久凭据（v4）：`upsert_credential`（:790，name 主键 upsert）/ `delete_credential(name) -> bool`（:821，幂等删除）。

## 5. query.rs —— 只读查询 + 记录类型（1678 行）

记录类型（serde derive）：`RunRecord`（:18，含 title?/cost_usd + **v5 input_tokens/output_tokens**）/ `ExecutionNodeRecord` / `StepRecord` / **`MessageRecord`（:74——`content_json` 是 `content_zst` 解压后的 JSON 文本（解压在 store 侧完成交付），新增 `meta_json: Option<String>` 旁路列）** / `PromptSnapshotRecord` / `AuditLogRecord` / `TraceEventRecord` / `PendingQuestionRecord`（v3）/ `CredentialRecord`（v4，无明文）/ **`AttachmentRecord`（:92，v5——引用形状，wire/DB 同构）** / **`TranscriptEntryRecord`（:109，v5——entry_json 透传）** / **`ToolOutputRecord`（v6/v7——id/run_id/ref_id（会话内短 id）/sha256(64hex)/tool_name/orig_bytes/line_count/created_at，orig_bytes·line_count 为压缩前原文计量）**。

查询 API：

| 函数 | 行 | 语义 |
|------|----|------|
| `get_run(id)` | :491 | |
| `list_runs(limit, offset)` | :511 | 历史列表（按 started_at DESC, id DESC tiebreaker） |
| `get_execution_node` / `list_steps(run_id)` | :528/:548 | |
| `list_messages(execution_node_id)` | :563 | 按节点（逐条解压） |
| **`list_messages_by_run(run_id)`** | :587 | **resume 路径**：按 seq 读回完整会话（解压在 store 侧；损坏帧报带消息 id 的错误） |
| `list_messages_by_run_after_seq(run_id, seq, limit)` | :610 | keyset 增量读 |
| `count_messages_by_run` / `max_message_seq` | :632/:644 | resume 续 seq 依据 |
| `get_prompt_snapshot` / `list_audit_logs` / `list_trace_events` | :655/:678/:693 | |
| `get_last_interrupted_run` / `get_last_resumable_run` | :713/:733 | resume 入口发现 |
| **`get_attachment(id)`** | :840（v5） | 单条附件元数据 |
| **`list_run_attachments(run_id)`** | :857（v5） | run 的附件清单（created_at, id 稳定序） |
| **`list_transcript_entries(run_id)`** | :878（v5） | run 的转写条目按 seq 升序；**空结果 = v5 前的 run 或无条目——调用方回退 message 投影（设计 §4 两级策略）** |
| **`find_tool_output(run_id, ref_id)`** | v7 | **严格会话隔离**：`(run_id, ref)` 精确匹配（`WHERE run_id = ? AND ref = ?`）——一个 run 的 ref 不能被其他 run 解析，跨 run 同 ref（各自 `out-1`）互不可见，**无「当前 run 优先 / 任意 run 回落」路径**；ref 形状 = `out-` + 1..16 hex（格式非法 Err）；未命中 None（调用方回落消息 content 原文或报未知 ref） |
| **`find_tool_output_by_sha(run_id, sha256)`** | v7 | storer 幂等路径：同 run 同 sha 命中 → 带回该行真实 ref（同内容同 ref，序号不跳号）；sha 必须 64 hex |
| **`max_tool_output_ref_num(run_id) -> i64`** | v7 | 该 run 现有 ref 序号最大值（无可解析行 → 0）；storer 分配下一个 ref 用 max+1（写入路径串行，撞号由唯一索引兜底） |
| **`read_tool_output_bytes(sha256)`** | v6 | 读 `files/outputs/{sha256}.zst` **压缩字节**（解压与 orig_bytes 校验归调用方）；sha 必须全 64 hex（防路径拼接）；文件缺失 Ok(None)；内存库 Ok(None) |

问卷/凭据查询：`list_pending_questions()` / `get_pending_question(request_id)` / `list_credentials()` / `get_credential_value(name)` / `load_credentials_with_values()`（明文仅 vault 消费）。

## 6. recorder.rs —— RunRecorder（1014 行，测试占大半）

```rust
// :51 / :63
pub struct RunFinal {
    pub status: String,            // 小写词汇：completed/interrupted/cancelled/failed
    pub output_json: Option<String>,  // None 保持库中已有值
    pub cost_usd: f64,
    pub input_tokens: u64,         // run 级 token 聚合（无 usage 采集时传 0）
    pub output_tokens: u64,
}

pub struct RunRecorder {
    store: SqliteStore,
    run_id: RunId,
    execution_node_id: String,   // 恒 "{run_id}-persist"（:116）
    agent_id: String,            // resume 优先用 run 行记录的 root agent
    last_seq: AtomicI64,         // sink 与 CLI/session 侧写共享一个单调计数器
    activated: AtomicBool,       // resume 过的 recorder 是否已置回 running
    pending: Mutex<VecDeque<(Message, Option<MessageMeta>)>>,  // v5 §5 挂起队列（仅进程内）
}
```

### 生命周期

| 阶段 | 方法 | 行为 |
|------|------|------|
| 开始 | `begin(store, run_id, root_agent_id, title?, input_json)`（:123） | **单事务**插 run 行（status=running）+ persist 执行节点——**run 在任何 LLM 调用前就存在**；activated=true |
| 接管 | `resume(store, run_id, root_agent_id)`（:154） | run 行必须已存在；persist 节点幂等按需补建；seq 续自 `max_message_seq`；**不改 status**；activated=false |
| 逐条（带 meta） | **`write_message_with_meta(&Message, Option<&MessageMeta>)`（:214，v5）** | 完整可靠性路径（见下）：flush 队列 → 重试 → 入队。content serde 后经 codec 压缩落 `content_zst`，meta serde 落 `meta_json`（assistant 消息专属；user/tool 传 None）。主键 `msg-{run_id}-{seq}` |
| 逐条（旧） | `write_message(&Message)`（:243） | 委托 `write_message_with_meta(msg, None)`——兼容全部既有调用方（CLI 初始 user 消息、ask_user 补写等） |
| 感知 | **`pending_persisted_writes() -> usize`（:249）** | 挂起队列长度；>0 时 mobile 层经 notice 通道发"存储写入失败，历史可能不完整"横幅 |
| 落盘 | `impl MessageSink::{append, append_with_meta}`（:460） | `append` 调 `write_message_with_meta(msg, None)` 但**吞掉返回值**（重试/入队/error 记录已在内部完成，持久化永不杀死 run）。`append_with_meta` override（L2 已接线）走 `write_message_with_meta(message, Some(meta))`——assistant 消息采集元数据 → `meta_json` 列 |
| 终态（v5） | **`finish_with(RunFinal)`（:357）** | status/output/finished_at/cost/token 聚合**单事务**落库；写入前先冲刷挂起队列 |
| 终态（旧） | `finish(status, output_json?, cost_usd)`（:378） | 委托 `finish_with`（tokens=0）——run_manager/session/mobile 现有调用方编译不动；core/session/cli/server 已迁移 `RunFinal`（L2），mobile 归 L4 |
| 读回 | `load_messages(store, run_id)`（:401） | 按 seq 读回 → 解码 core Message（content_zst 已在 query 侧解压）；**尾部悬空 tool_calls 的 assistant 消息被丢弃**——丢弃动作显性化：`tracing::warn` 携带 run_id 与消息 id |
| 读回（保留悬挂） | `load_messages_keep_dangling(store, run_id)`（:433） | 同上但**不丢**尾部悬挂 assistant——ask_user 重启恢复用（ask-user r5） |

### 可靠性语义（v5 设计 §5）

消息持久化失败从"warn 后丢弃"升级为**有界重试 + 进程内排队 + UI 上报**：

1. `write_message_with_meta` 先 `flush_pending`（前序失败消息优先补写，队首优先保持顺序；锁只在取队首/弹出时短暂持有，绝不跨 await）；
2. 单次尝试失败 → 退避重试 3 次（`APPEND_RETRY_BACKOFF_MS = [100, 250, 500]`，:46；初始尝试 + 3 重试共 4 次，每次失败 warn 含下一次间隔）；
3. 穷尽 → `(Message, Option<MessageMeta>)` 推入 `Mutex<VecDeque>` 挂起队列 + `tracing::error`，同时返回最后错误（sink 侧吞掉；fallible 调用方可上浮——入队保证稍后补写不丢）；
4. 每次 append / finish 前先冲刷队列；`finish_with` 同样先 flush。

**边界**（文档明示）：队列仅进程内——进程崩溃仍丢；彻底解决需要 WAL 外 journal，follow-up。seq 在重试间会烧出空洞（每次尝试重新分配），无害——单调唯一即可。

### 关键设计点

- **"落盘后再 continue"**：sink 在 execute_run 循环内联 await——崩溃/取消/退出总留可恢复的部分转写。
- **seq 是 load-bearing**：run 内唯一（uq 索引兜底 + 派生主键双保险）。
- **meta 不进 content envelope**：content_zst 是 `Message` serde 直接产物；meta 走单列 NULL 语义（user/tool 行 NULL 零成本），读端解码代码不翻倍，provider 回放路径零泄漏。
- persist 节点 vs runtime 树：recorder 的消息挂 `{run_id}-persist` 确定性节点；runtime 自己的执行树 run 后单独写。

## 7. 协作速查（四 crate）

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

## 8. 注意事项汇总

1. **seq 单调且 run 内唯一**是 resume 正确性的根基（重复 seq → 加载顺序不稳定 → provider 拒绝乱序 tool_calls 对）。
2. 新表/新列 = 新 `SCHEMA_VERSION` 步进 + 独立事务 + 同步测试常量。破坏性迁移需用户拍板记录（先例：v4→v5）。
3. `RunRecorder` 的 `append` 吞错（但已重试+入队+error 记录）；需要拿错误的调用方用 `write_message` / `write_message_with_meta`。
4. 查询侧 `MessageRecord.content_json` 是压缩列解压后的 core `Message` 整体 JSON（含 `reasoning_content`/`tool_calls`），解码零损；**损坏帧报带消息 id 的错误，绝不静默丢行**。
5. 库文件权限 0600 是明文会话历史的最低防线（含 WAL/-shm 旁车）。
6. 附件字节永不进 DB：行在 `attachments`、文件在 `files/{sha256}.{ext}`；删除走 `delete_run`（事务内删行 + 事务外按 sha256 行级判定清扫文件，跨 run 复用文件只在最后一个引用消失时删）。
7. 内存库（`new_in_memory`）无 `files_dir`——附件写入明确报错；相关测试用 tmpdir 文件库。
