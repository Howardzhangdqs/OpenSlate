# OpenSlate 存储层 v5 + 多模态输入 统一改造设计（最终版）

> 状态：已评审定稿（2026-10-21，@oracle 设计 + GLM-5.3-Flash 官方 API 事实修正）。
> **2026-10-21 用户拍板：软件未发布、开发期，允许破坏性数据库迁移**——v4→v5 不做数据保真复制，messages 历史直接丢弃重建；runs/credentials/pending_questions 保留（API key 与会话列表不丢）。§1/§8.2 已按此简化。
> 本文是实施权威依据；与代码冲突时以代码为准并回改本文。
> 实施进度见文末 lane 表。

**版本**：v2（合并 lib-2 GLM-5.3-Flash 事实修正后的自洽全文）
**范围**：`openslate-store-sqlite` v5 迁移（压缩 / per-message 元数据 / 附件 / transcript 持久化 / 可靠性）+ 端到端多模态输入（图像/视频，智谱 GLM-5.3-Flash 第一目标，Gemini 第二目标）。
**不范围**：MCP 工具返回图像的透传（§6.5 明确 defer）；输出侧多模态；语音输入。
**基线事实**：已核实的源码锚点（schema.rs / store.rs / write.rs / recorder.rs / core runtime.rs:656 捕获点 / convert.rs:51 / vendored genai 0.6.5 / FFI lib.rs / sqlx `sqlite-bundled` / PROTOCOL_VERSION=3）与 lib-2 智谱实测事实（附录 A）。

---

## 1. 压缩方案

**结论**：只压 `messages` 一张表的一个载荷列。v5 迁移中**整表重建**为 `content_zst BLOB NOT NULL`（zstd level 3），不做"新列+旧行并存"的双列方案。`zstd = "0.13"` 引入 store-sqlite（mobile registry.rs 已用同版本，Android 交叉编译已验证可行）。

**理由**：
- 空间热点集中在 `messages.content_json`（reasoning 全文 + 64KB 截断的工具输出，均为 JSON 文本，zstd-3 典型压缩比 5–10×）。`runs.input_json` 极小；`steps/prompt_snapshots/audit_log/trace_events` 生产零写入；`execution_nodes` 只有 CLI 占位 `"{}"`。压别的列是纯复杂度。
- SQLite 列亲和性下往 TEXT 列塞 BLOB 虽可行但语义肮脏；保留 `content_json TEXT NOT NULL` 则新行必须双写。**重建表**（CREATE 新表 → 流式复制压缩 → DROP → RENAME → 重建索引）一步拿到干净的最终 schema，且**没有任何表 FK 引用 messages**，重建无级联风险。
- zstd 帧自带 magic（`0x28 B5 2F FD`），列内数据自描述，无需标志列。

**等级**：level 3（与 `registry.rs::zstd_level()` 一致）。64KB 工具输出压缩 <1ms，交互路径无感；再高档位在手机上收益递减。

**迁移耗时与批处理**：v4→v5 步进保持**单事务**（沿用"每步一事务、user_version 同事务提交"的不变量，崩溃可重跑）。**用户已拍板允许破坏性迁移（未发布开发期）**：不复制旧数据——`DROP TABLE messages` 后直接以最终 DDL 重建，旧行历史丢弃；`runs`/`credentials`/`pending_questions` 原样保留（API key、会话列表不丢）。迁移即 O(1) DDL，无耗时风险；迁移完成后调一次已有的 `wal_checkpoint(TRUNCATE)` 回收 WAL。

**存量脏数据**：随 DROP 一并消失（v2 时代重复 seq 的库也无需重编号逻辑——新表从零开始，`UNIQUE(run_id, seq)` 必然可建）。

**替代方案（否决）**：① ALTER 加 `content_zst` 列 + 永久双格式读取——schema 永久带疤；② 不压缩只调小 `max_output_bytes`——治标，reasoning 依旧膨胀；③ SQLite compression extension——bundled 构建没带，不可移植。

## 2. per-message 元数据

**结论**：`messages` 加一列 `meta_json TEXT NULL`；`runs` 加 `input_tokens`/`output_tokens` 两列聚合；**core `Message` 结构不加 usage/finish_reason 字段**，元数据走 `MessageSink` 新方法的旁路通道。

**core 新类型**（`openslate-core/src/types.rs`）：

```rust
/// 单条消息的采集元数据（assistant 消息专属；落 messages.meta_json，
/// 不进 Message 本体——Message 是 provider 回放单元，混入 usage 会在
/// convert.rs / resume 回放路径泄漏给 provider）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct MessageMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
}
```

**捕获链**：
1. **捕获点**：core `runtime.rs` assistant 消息构造处（现 :656-672）。此处已有 `response.usage`、`response.finish_reason`、`call_elapsed`、model id——流式/非流式两路径在汇合点（:639 附近）都拿得到。构造 `MessageMeta` 随消息一起交给 sink。
2. **MessageSink 扩展**（core/runtime.rs:47）：

```rust
#[async_trait::async_trait]
pub trait MessageSink: Send + Sync {
    async fn append(&self, message: &Message);
    /// 默认实现丢弃 meta 委托 append —— 第三方 sink 不破坏。
    async fn append_with_meta(&self, message: &Message, _meta: &MessageMeta) {
        self.append(message).await;
    }
}
```
3. **RunRecorder**：override `append_with_meta` → `write_message_with_meta(&Message, Option<&MessageMeta>)`（旧 `write_message` 委托、meta=None，兼容 ask_user/初始 user 消息等无 meta 写入点）。store 的 `insert_message` 签名改为收 `content_zst: &[u8]` + `meta_json: Option<&str>`（该 API 实际只有 recorder 一个调用方，直接改签名，不加重载）。
4. **runs 聚合**：`RunRecorder::finish` 改收结构体：

```rust
pub struct RunFinal {
    pub status: String,
    pub output_json: Option<String>,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}
```
`finish_run` UPDATE 追加两列（调用方 run_manager/session/mobile 全量改，grep 可数）。存量 run 无 meta → 聚合列保持 0（接受，不回填）。

**为什么 meta 不进 content_json envelope**：content_json 是 `Message` 的 serde 直接产物，塞 meta 要么改 Message（泄漏 provider 路径 + 全 workspace 数百处构造 churn），要么 envelope 包一层（所有读端解码代码翻倍）。单列 NULL 语义干净：user/tool 行 NULL，零成本。

**为什么 runs 加列而新表**：会话列表页要展示"本次共 N tokens"，`SELECT … FROM runs` 一行带走；按 run 聚合的明细表没有消费方（YAGNI）。

## 3. 附件存储

**结论**：字节进文件系统（content-addressed），元数据进 `attachments` 表；DB 只存引用——与 `.ref/opencode` 架构同构。ingest 逻辑放 **openslate-mobile**（不进 store-sqlite，store 只管行和文件生命周期）。

### 3.1 表 DDL

```sql
CREATE TABLE IF NOT EXISTS attachments (
    id TEXT PRIMARY KEY,              -- att-<uuid>
    run_id TEXT,                      -- NULL = 已导入未绑定（composer 暂存）
    sha256 TEXT NOT NULL,
    mime TEXT NOT NULL,               -- 归一化后（见 3.2/3.4）
    filename TEXT,
    size_bytes INTEGER NOT NULL,      -- 归一化后字节数
    created_at INTEGER NOT NULL,
    UNIQUE(run_id, sha256),           -- run 内去重；跨 run 靠 sha256 复用文件
    FOREIGN KEY(run_id) REFERENCES runs(id)
);
CREATE INDEX IF NOT EXISTS idx_attachments_sha256 ON attachments(sha256);
```

- `run_id` 可空：用户在 composer 选图时 run 可能尚未创建。Submit 时 mobile 层批量 `UPDATE attachments SET run_id=? WHERE id IN (…)`（store 新 API `bind_attachments`）。SQLite FK 对 NULL 父值放行。
- **文件布局**：`{db 文件父目录}/files/{sha256}.{ext}`（ext 由 mime 映射：jpg/png/webp/gif/mp4/mkv/mov）。**不按 run_id 分目录**——content-addressing 天然跨 run 去重。files 根从 db path 派生（`SqliteStore::files_dir()` = `Path::new(db_path).parent().join("files")`）：三个前端的 db 都落在各自 data 目录，派生零管线成本。内存库（测试）→ `files_dir()` 返回 None，附件写入返回明确错误。
- **GC**：`delete_run` 的 CHILD_DELETES 追加 `DELETE FROM attachments WHERE run_id = ?` 与 `DELETE FROM transcript_entries WHERE run_id = ?`；事务提交后扫描判定 `sha256` 不再出现在 attachments 表的文件并删除（行级 `SELECT 1 FROM attachments WHERE sha256=?`，非全表 diff）。另加启动 GC：`DELETE FROM attachments WHERE run_id IS NULL AND created_at < now-7d` + 对应文件清扫（用户选了图没发送的暂存）。

### 3.2 mime 白名单：ingest 接受面与 wire 发送面两层

| 层 | 集合 | 说明 |
|---|---|---|
| **ingest 接受**（Kotlin→Rust 入口） | `image/jpeg, image/png, image/webp, image/gif`（+Kotlin 侧 HEIC/HEIF→JPEG 预转码） | `image` crate 均可解码；API 26/27 无 HEIC 解码 → UI 明确报"不支持此格式" |
| **wire 发送**（convert.rs 产出） | 图像仅 `image/jpeg \| image/png`；视频 `video/mp4, video/x-matroska(mkv), video/quicktime(mov)` | 智谱事实：图像 jpg/png/jpeg；视频 mp4/mkv/mov |

**归一化从"可选降采样"升级为"强制转码保证"**：webp/gif 在 mobile `attachments.rs` 一律转码 JPEG（动图取首帧——模型本就不消费动画）。已有 PNG 且 ≤ 上限 → passthrough 保 PNG（截图文字清晰度受益）；其余全走降采样 + JPEG q85 重编码。**归一化器就是 wire 白名单的执行者**，convert 层不需要格式分支。

### 3.3 尺寸/数量上限

- **归一化目标**：max(w,h) ≤ `attachments.max_image_dim`（**默认 2048**，配置允许到 6000）。
- **硬校验（ingest 出口断言）**：归一化后解码字节 ≤5MB、维度 ≤6000²。2048+q85 实际产出 ~0.3–2MB，断言只是防呆。
- **每请求 ≤50 张图**：Submit 门控常量（不做配置项，YAGNI）+ convert.rs 兜底跳过多余项并 warn。
- **总内联封顶** `max_inline_total_mb = 25`：超限在 Submit 门控拒绝，报错列出实际总量（fail fast，不静默裁剪）。

### 3.4 图/视频互斥（保守读法拍板）

**结论**：Submit 门控强制「**本轮附件 = 纯图像 或 纯视频**（文本始终可随行）」，图+视频混发直接拒绝，notice 提示"图像与视频请分开发送"。

- 依据智谱事实"file 块与 image/video 互斥，图文视频别混"。文本+多图同发是安全的。
- 我们永不出 `file` 块（无文档附件功能）；file 互斥约束记录在 convert.rs 注释里防未来踩坑。
- **若 L5 真机验证发现图+视频混发实际可用**：门控收在一个常量判断里，放开是一行改动。

### 3.5 本地视频策略（实验性 data:URL，默认关）

- `[attachments]` 新增 `experimental_video = false`。关闭时选视频 → 门控拒绝："当前版本视频输入为实验性功能，请在设置中开启"。开启后走 §6.3 的 video_url data:URL 路径。
- **内联视频上限 `max_inline_video_mb = 20`**（对齐 Gemini inline 惯例量级；200MB 是智谱 URL 托管上限，不是 base64 内联该碰的）。
- `max_video_mb = 50`（ingest 落盘上限）保留——文件先落盘（可重发、跨 run 复用），实际发送时才受 20MB 内联上限约束。
- **降级路径预埋**：experimental 开关 + 单点构造（§6.3），若智谱端实测拒绝 data:URL 视频，把默认值钉死 false、UI 文案改"暂不支持"，零结构改动。

### 3.6 FFI/Kotlin 数据流（拍板：路径传递，不走 base64/ByteArray）

1. Kotlin `ContentResolver.openInputStream(uri)` 拷贝到 `{cacheDir}/import-<ts>`（视频 100MB+ 走 JNI String base64 会同时炸内存和 33% 膨胀，直接否决）。
2. **Kotlin 侧预归一化 HEIC/HEIF → JPEG**（平台 codec 在 Kotlin：API 28+ `ImageDecoder`；API 26/27 无 HEIC 解码 → UI 报"不支持"）。契约写死：`import_attachment` 图像入参只接受 jpeg/png/webp/gif。
3. FFI 新导出（uniffi 同步签名，内部 `rt.block_on`，registry.rs 先例）：

```rust
#[uniffi::export]
pub fn import_attachment(&self, source_path: String, declared_mime: String,
                         filename: String) -> String  // JSON: {ok,id,mime,size_bytes} | {error}
```
4. Rust：magic-byte 嗅探（不信任 declared_mime）→ sha256（`sha2`，mobile 加依赖）→ 图像归一化（§3.2/3.3）→ 写 `files/` → insert 行 → 返回 DTO JSON。Kotlin 拿 id 存 composer 状态，Submit 时拼进 JSON。
- **视频**：passthrough 不转码（手机端 Rust 侧 ffmpeg 级转码是另一个项目级依赖，否决）。大小上限走 §3.5 配置。
- **原图不保留**——附件是"输入的拷贝"，用户原件在相册；移动端存储寸土寸金。

## 4. transcript 完整性

**结论**：**新表 `transcript_entries`**（一行一条 EntryDto），不复用 `steps`，不做每回合快照。重建逻辑收敛为"优先读 entries 表，空则回退 message 投影"的两级策略。

**理由**：
- `steps` 的既有语义是执行步骤，Approval/Delegate/StepBreak/Meta 不是执行步骤——复用是语义盗用。
- 每回合快照 JSON 在"回合中途崩溃"场景下整回合 UI 条目丢失，且 Approval 恰恰多发生在回合中；行级写入天然增量。
- `entry_json` 不压缩（单条几百字节，压缩负收益）。

```sql
CREATE TABLE IF NOT EXISTS transcript_entries (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    entry_json TEXT NOT NULL,        -- EntryDto serde
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES runs(id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_transcript_run_seq
    ON transcript_entries(run_id, seq);
```

- **写点**：mobile 层维护 live transcript（`Vec<EntryDto>`）的所有 append 路径收口到一个 helper，helper 同时写 store（seq 由 recorder 同款 AtomicI64 模式分配）。Approval/Delegate/Meta/StepBreak/on_tool_end 的 elapsed_ms/bytes 全部随 EntryDto 自然落库。
- **投影统一**：`EntryDto::project_from_messages(&[Message]) -> Vec<EntryDto>` 作为共享函数**放进 openslate-protocol**（DTO 的家），实现 = 现 mobile `messages_to_transcript`（runtime.rs:1713）搬迁 + 保持 reasoning 还原。消费方：mobile 的回退路径（打开 v5 之前的旧 run、entries 为空时）。TUI `entry_from_message`（transcript.rs:1884，实测不映射 `reasoning_content`）**单独补一行 Reasoning 映射**——TUI 的 TranscriptEntry 是自己的枚举，强行统一是大重构，本期只修 bug 不做架构合并（记为后续收敛项）。
- **resume 流程**：`open_session` → `list_transcript_entries(run_id)` 非空用之；空（v5 前的 run）→ `project_from_messages`。转写与消息从此各自真相源：消息管 provider 回放，entries 管 UI 完整性。

## 5. 可靠性

**结论**：`MessageSink` 失败从"warn 后丢弃"升级为"有界重试 + 进程内排队 + UI 上报"；悬挂尾部 pop 语义保持，但丢弃动作显性化。

- **重试与排队**（recorder.rs，随 L1 落地）：`append_with_meta` 失败 → 退避重试 3 次（100/250/500ms）→ 仍失败则把 `(Message, MessageMeta)` 推入 `Mutex<VecDeque>` 挂起队列 + `tracing::error`；每次新 append 和 `finish` 前先冲刷队列。队列只进内存（进程崩溃仍丢——彻底解决需要 WAL 外的 journal，超出本期；文档明示边界）。
- **UI 上报**：`RunRecorder` 暴露 `pending_persisted_writes() -> usize`；mobile 层发现 >0 时经现有 notice 通道发"存储写入失败，历史可能不完整"横幅。
- **悬挂尾部**：保持 pop。改动仅两点：① pop 时 `tracing::warn` 升级为携带 run_id 与消息 id；② 若 transcript_entries 存在该 run 的条目，追加一条 `EntryDto::Meta{text: "上次回合未完成，已丢弃待答复的工具调用"}`——恢复后的 UI 不再"无声少一截"。

## 6. 多模态管线

### 6.1 core Attachment 类型（引用+内联混合，单字段）

```rust
/// 消息附件。持久化形状 = {id, mime, filename}（引用，DB/协议 wire 都是这个
/// 形状）；`data` 仅供内存中直达 provider（ingest 层填充、#[serde(skip)]
/// 永不序列化），resume 恢复时由 mobile 层可选回填。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub id: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip, default)]
    pub data: Option<std::sync::Arc<[u8]>>,
}

pub struct Message {
    // …现有字段不动…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}
```

- **纯引用式**需要 `AttachmentResolver` trait 注入 core runtime——跨三层管线，收益只是省一个 skip 字段；**纯内联式**把 2.7MB base64 塞进消息 JSON——与 §1 压缩目标直接对撞；**混合式**：DB/wire 只有几百字节引用；内存里 `Arc<[u8]>` 克隆零拷贝；`serde(default)` 保证全部存量 payload 与 provider 回放兼容。字段添加的构造 churn 与当年 `reasoning_content` 同型（机械补 `attachments: None`）。
- **resume 回填**：打开会话时 mobile 层遍历加载的消息，对有 `attachments` 的项从 store+files 回填 `data`（历史图随每轮重发）；总回填字节受 `max_inline_total_mb` 封顶，超限的旧附件不回填（模型丢图不丢文本，可接受并文档化）。

### 6.2 convert.rs User 分支改造

`to_chat_message`（convert.rs:51）User 分支：

```
无附件 → 现状（纯文本，wire 不变）
有附件 → MessageContent::from_parts：
    ContentPart::Text(content)  // 文本永远在前
    + 每个 data=Some 的附件 → ContentPart::Binary(
        Binary::from_base64(mime, base64::STANDARD.encode(&data), filename))
```
- data=None 的引用（未回填的旧附件）→ **跳过并 tracing::warn**，绝不发送空 Binary。
- 附件不进 Assistant/Tool 分支（本期只有用户输入侧）。
- 单请求图像数 >50 的兜底跳过在此处（门控为主，此处防直接 API 调用）。

### 6.3 vendored genai：打补丁，不升级 0.7.0（拍板）

- 补丁内容（vendor/genai `adapter/adapters/openai/adapter_shared.rs` User 分支，照抄 0.7.0 语义）：
  1. `Binary::is_video()`（`content_type` 以 `video/` 开头，加在 `chat/binary.rs`）；
  2. `is_image` 分支后插入 `else if is_video { let url = binary.into_url(); values.push(json!({"type":"video_url","video_url":{"url": url}})); }`。
- `into_url()` 对 Base64 source 生成 `data:{mime};base64,{b64}`——**恰好满足智谱"data: 前缀必须带"的要求，图像侧已是实证兼容形态**。
- 视频分支 mime 不设白名单过滤（ingest 层已保证）。mkv 的 mime 用嗅探结果（`video/x-matroska`），是否需伪装成 `video/mp4` 是 L3 实测项。
- **不构造 `detail` 字段**（智谱无此参数）。
- ZAI adapter 委托 OpenAIAdapter → 自动获得。**Gemini**：0.6.5 gemini adapter 对 Binary 的映射（inlineData）图像可用；视频是否原样透传，lane 内验证，不行则同型小补丁——第二目标，不阻塞主线。
- **否决升级 0.7.0**：vendored 树已带一个本地补丁，0.6→0.7 API 漂移会波及全部 adapter 的 rebase；为一段 15 行的 video_url 分支承担整树 rebase 风险，不成比例。vendor 补丁清单继续在根 Cargo.toml `[patch]` + 注释处登记。

### 6.4 能力门控：按模态分权（supports_vision/vision 首个消费方）

```
提交含图像附件：
  model caps 无 image 模态（如 glm-5.3 本体）→ 拒绝
  有 image 模态（glm-5.3-flash）→ 放行（走 §3.3 上限校验）
提交含视频附件：
  model caps 无 video 模态 → 拒绝（无论开关）
  有 video 模态 且 experimental_video = false → 拒绝（提示开启实验开关）
  有 video 模态 且 experimental_video = true → 放行（≤20MB 内联校验）
附加约束（门控一并执行）：
  图+视频混发 → 拒绝；图像 >50 张 → 拒绝；总字节 > max_inline_total_mb → 拒绝
caps 未知（手填模型无元数据）→ 放行，provider 错误原样透出
```

- 数据源：models.dev 已有 glm-5.3-flash 条目且含 video modalities → registry 刷新后 caps 自动就位。
- mobile dispatch 门控实现读 registry 合并后的 caps（补 video 维读取）。
- 不做"非视觉模型自动转占位文本"——静默降级用户不知情，与 fail-fast 原则矛盾。

### 6.5 MCP image 透传：本期不做 → **已由 read_file 读图落地（2026-10-21）**

~~`mcp.rs:250 downgrade_content_blocks` 维持现状。~~ 后续增量已实现完整读图链路：
mcp-builtin fs.rs `read_file` magic-byte 嗅探 + host 侧归一化（≤2048/≤5MB/JPEG q85，小 PNG
passthrough）→ `CallToolResult` 携 Image block → core `downgrade_content_blocks` base64 解码
收进 `ToolOutput.attachments`（新 additive 字段）→ runtime 透传进 Tool Message → genai
convert 1:N 展开（ToolResponse + 紧随 user Binary parts）。视频仍不支持（结构化错误提示）。
工具附件 id `tool-{n}` 不落 attachments 表——resume 后不回填（模型丢图不丢文本，R5 语义）；
请求路径总量封顶由 `AttachmentDataProxy.fill` 对 data=Some 就地丢弃兜底。附件缩略图 FFI
`getAttachmentThumbnail` 同期落地（仅覆盖用户 ingest 附件）。

### 6.6 `[attachments]` 配置节（core config 新节，最终形态）

```toml
[attachments]
max_image_dim = 2048        # 归一化最大边；校验上限 6000
max_image_mb = 10           # ingest 入口（归一化前）上限
max_video_mb = 50           # ingest 落盘上限
max_inline_video_mb = 20    # 实际发送内联上限（实验视频路径）
max_inline_total_mb = 25    # 单请求附件总字节
experimental_video = false  # data:URL 视频实验开关（智谱未文档化）
```
validation 最小值：dim ≥ 512、各 mb ≥ 1。默认值即安全值。端点说明（coding/paas/v4 均可调 flash）与 thinking/reasoning_effort 属 provider 配置文档事项（写进 03-models 接入说明）。

## 7. 协议与 FFI

**结论**：`Submit` **加字段**（不新增变体）；`PROTOCOL_VERSION` **3 → 4**；FFI 只加 `import_attachment` 一个导出，`send` 签名不动。

- **AttachmentDto**（protocol）：

```rust
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttachmentDto {
    pub id: String,
    pub mime: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
}
```
（与 core `Attachment` 的持久化形状逐字段一致，From/Into 互转。data 永不过 wire。）

- **Submit 变体**：

```rust
Submit {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    attachments: Option<Vec<AttachmentDto>>,
},
```
`serde(default)`：旧客户端（无字段）→ None；按项目惯例仍升版本 + 契约测试新增样例：带附件 Submit、EntryDto::User 带附件。

- **EntryDto::User** 加同款 `attachments: Option<Vec<AttachmentDto>>`（transcript 渲染缩略图用，additive）。
- **图/视频混排由 mobile dispatch 拒绝**（错误经现有 notice 通道返回），协议层不表达该约束。
- **Kotlin 数据流**：`PickVisualMedia`（Activity Result API，API 26+ 兼容）→ 拷贝/HEIC 转码到 cache → `import_attachment(path, mime, filename)` → id 入 composer 状态 → 提交时 Kotlin 拼 `{"type":"submit","text":…,"attachments":[…]}` → `send(json)`。**ByteArray/base64 不出现在任何边界**。
- uniffi：`uniffi-bindgen` 重跑 scaffolding；Kotlin 侧 `AttachmentDto` 手写 data class（延续现状手拼 JSON 路线）。

## 8. 迁移与版本（SCHEMA_VERSION = 5）

### 8.1 v5 全部 DDL

```sql
-- ① messages 整表重建
CREATE TABLE messages_new (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    execution_node_id TEXT NOT NULL,
    agent_id TEXT,
    role TEXT NOT NULL CHECK (role IN ('system','user','assistant','tool')),
    content_zst BLOB NOT NULL,               -- zstd(level 3) of serde_json(Message)
    meta_json TEXT,                          -- Option<MessageMeta>；user/tool 行为 NULL
    seq INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(run_id) REFERENCES runs(id),
    FOREIGN KEY(execution_node_id) REFERENCES execution_nodes(id)
);

-- ② runs 聚合列（column_exists 守卫，幂等）
ALTER TABLE runs ADD COLUMN input_tokens INTEGER NOT NULL DEFAULT 0;
ALTER TABLE runs ADD COLUMN output_tokens INTEGER NOT NULL DEFAULT 0;

-- ③ attachments（全文见 §3.1）
-- ④ transcript_entries（全文见 §4）

-- ⑤ messages 索引（DROP 旧表后重建）
CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_run_seq_uq ON messages(run_id, seq);
CREATE INDEX IF NOT EXISTS idx_messages_execution_node_id ON messages(execution_node_id);
```

**注意**：`idx_messages_run_seq`（非唯一）**删除不重建**——`idx_messages_run_seq_uq` 最左前缀完全覆盖其查询。`INDEX_NAMES`/`TABLE_NAMES` 常量同步：+attachments、+transcript_entries、+idx_attachments_sha256、+idx_transcript_run_seq、−idx_messages_run_seq（索引计数测试随之更新）。schema.rs 版本历史注释追加 v5 条目。

### 8.2 `migrate_v4_to_v5` 步骤（单事务内，破坏性——用户已授权）

1. `DROP TABLE IF EXISTS messages`（连带其索引；旧消息历史丢弃）。
2. `CREATE TABLE messages`（§8.1 ① 最终 DDL，直接以最终表名建，无需临时表+RENAME）。
3. 建 ⑤ 索引（`idx_messages_run_seq_uq` + `idx_messages_execution_node_id`）。
4. runs 两列 ALTER（column_exists 守卫）。
5. attachments / transcript_entries 建表建索引（IF NOT EXISTS）。
6. `PRAGMA user_version = 5`（run_migrations 驱动器统一写入）。
7. 步进事务外：`wal_checkpoint(TRUNCATE)`。

全新库路径：`ddl_statements()` 的 `DDL_MESSAGES` 直接换成新形态。**回滚策略：无**（与既有 4 个版本一致）。现有测试资产复用：store.rs:711+ 的"手工造旧库→迁移→校验"模式新增 v4 库（含大 content、重复 seq 样本）→ v5 用例。

### 8.3 读写路径收口

- store 新模块 `codec.rs`：`encode_content(&str) -> Vec<u8>` / `decode_content(&[u8]) -> Result<String>`（zstd-3；decode 失败给带消息 id 的 StoreError，绝不静默）。
- `insert_message` 签名改 `content_zst: &[u8]` + `meta_json: Option<&str>`；`list_messages_by_run` 返回结构加 `meta_json: Option<String>`。
- `delete_run` CHILD_DELETES 迩加 attachments、transcript_entries 两条；文件 GC 在事务外。
- store 新 API：`insert_attachment` / `bind_attachments(ids, run_id)` / `get_attachment(id)` / `list_run_attachments(run_id)` / `insert_transcript_entry` / `list_transcript_entries(run_id)`。

## 9. 实施 lane 表

| Lane | 范围文件 | 写权限 | 依赖 | 验证 | 文档更新 |
|---|---|---|---|---|---|
| **L0 core 类型基座** | `crates/openslate-core/src/types.rs`（MessageMeta、Attachment、Message.attachments + serde 测试）、全 workspace 机械补 `attachments: None` | openslate-core/src/types.rs + 机械修改 | 无 | `cargo test -p openslate-core`；serde 兼容测试 | `02-core/openslate-core.md` |
| **L1 store v5 + 可靠性** | `crates/openslate-store-sqlite/`（schema.rs、store.rs、write.rs、recorder.rs、新 codec.rs、Cargo.toml +zstd）+ recorder 侧 meta/RunFinal API（L2 交接面） | 整个 openslate-store-sqlite crate | L0 | 迁移测试（v4 库 → v5 破坏性：messages 清空且新 schema 就位、runs/credentials 原样保留、幂等重跑）、锁定测试全绿、sink 重试/排队单测、压缩往返 | `02-core/openslate-store-sqlite.md` |
| **L2 元数据捕获（core 侧）** | `crates/openslate-core/src/runtime.rs`（sink trait 扩展 + 捕获点）、`run_manager.rs`、`openslate-session`（RunFinal 改造） | core runtime/run_manager + session | L0；store API 等 L1 | runtime harness 断言 usage/finish_reason/latency/model_id 抵达 meta；RunFinal 落聚合列 | `02-core/openslate-core.md`、`02-core/openslate-session.md` |
| **L3 多模态 core+vendor+registry rider** | `vendor/genai`（is_video、video_url 分支）、`crates/openslate-model-genai/src/convert.rs`、根 Cargo.toml patch 注释；rider：`crates/openslate-core/src/model_registry.rs`（[1m] 归一化 + zhipuai 无 cost 记 0） | vendor/genai 两文件、convert.rs、model_registry.rs | L0 | convert 单测：带附件 User 序列化含 image_url/video_url 与 data: 前缀；无附件路径不变；data=None 跳过；registry 归一化表驱动测试 | `03-models/`、`07-build-dev/`（vendor 补丁登记） |
| **L4 protocol+FFI+mobile ingest** | `crates/openslate-protocol/`（AttachmentDto、Submit/User 加字段、PROTOCOL_VERSION=4、契约测试）、`crates/openslate-mobile/src/attachments.rs`（新）、`runtime.rs`（门控/回填/transcript 持久化）、core config `[attachments]` 节、`openslate-mobile-ffi/src/lib.rs` | protocol、mobile、core config、ffi | L1、L3 | 契约测试新旧样例；import→submit→scripted provider 收到 Binary part；门控矩阵；resume 回填 | `02-core/openslate-protocol.md`、`06-mobile/` |
| **L5 Android UI** | `android/`（ComposerPanel 附件按钮/缩略图 chips/PickVisualMedia/HEIC 转码/Submit JSON/设置页实验开关） | android 工程 Kotlin | L4 FFI 签名 | 真机 release 包人工验证清单（见设计）+ 两台设备 | `06-mobile/android-app.md` |
| **L6 transcript 完整性** | protocol `project_from_messages`、TUI reasoning 映射修复、mobile 回退路径 | protocol lib.rs 独立区段、tui transcript.rs | L1；与 L4 错峰（L4 先） | protocol 投影快照测试；TUI rebuild 补 reasoning case；旧 run 回退不回归 | `02-core/openslate-protocol.md`、`05-frontends/`、`06-mobile/call-chains.md` |

**依赖顺序**：L0 →（L1 ∥ L3）→ L2 → L4 → L5；L6 在 L1 后错峰。

## 风险清单（R1–R11）

| # | 风险 | 处置 |
|---|---|---|
| R1 | ~~v5 单事务重建在超大库上的启动耗时~~ | **已消解**：用户授权破坏性迁移，v4→v5 为 O(1) DDL，无数据复制 |
| R2 | 智谱视频 data:URL 未文档化 | 实验开关默认关；L5 实测回写后定走向 |
| R3 | Gemini 视频透传未验证 | 第二目标；不行则同型小补丁 |
| R4 | sink 排队仅进程内，崩溃窗口仍丢 | 文档明示；journal 记 follow-up |
| R5 | resume 超封顶旧附件不回填 → 模型丢图不失文 | 文档化；封顶可配 |
| R6 | 20MB 视频 base64 在低端机内存峰值 | L5 真机两台各测；超限下调默认值 |
| R7 | `[1m]` 归一化只覆盖已知变体 | 表驱动测试；miss → caps 未知 → 放行 |
| R8 | 图/视频互斥可能过度限制 | 单点常量，实测可行即放开 |
| R9 | zstd 压缩比为估算 | L1 迁移测试记录实测比；结构收益独立成立 |
| R10 | mkv/mov mime 智谱是否接受未验证 | L3/L5 实测；构造点单处 |
| R11 | vendored genai 补丁漂移累积 | 补丁清单登记；升级窗口记 follow-up |

## 附录 A：GLM-5.3-Flash 事实基线（lib-2 查实）

| # | 事实 | 设计落点 |
|---|---|---|
| ① | 视觉模型 = glm-5.3-flash 本体（原生多模态）；旗舰 glm-5.3 纯文本 | §6.4 |
| ② | image_url.url 收 URL 或 data: 前缀 base64；≤5MB/6000²；jpg/png/jpeg；≤50 张 | §3.2/3.3、§6.3 |
| ③ | video_url 官方仅 URL（≤200MB、mp4/mkv/mov）；base64 未文档化 | §3.5、R2/R6/R10 |
| ④ | file 块与 image/video 互斥；图/文/视频别混 | §3.4、R8 |
| ⑤ | 无 detail 参数 | §6.3 |
| ⑥ | coding/paas/v4 端点均可调 flash | 文档事项 |
| ⑦ | thinking 仅 enabled，强度走 reasoning_effort | 无改动（正交） |
| ⑧ | models.dev 有条目（含 video modalities）但 zhipuai 无 cost；`[1m]` 后缀需归一化 | §6.4、L3 rider、R7 |

**遗留 follow-up**：MCP 工具图像透传；sink journal 持久化队列；vendor genai 升级 0.7 窗口评估；TUI/mobile transcript 投影完全统一；智谱文档化 data:URL 视频后转正实验开关。

---

## 实施进度

| Lane | 状态 |
|---|---|
| L0 | ✅ 完成（fix-1，2026-10-21：types.rs :94 Attachment / :194 MessageMeta / :87 Message.attachments；132 处机械修复；615+11+1 测试通过；02-core 文档同步） |
| L1 | ✅ 完成（fix-2：schema v5 破坏性迁移/codec/新表×2/recorder meta+RunFinal+重试队列；106 测试通过；02-core 文档重写；偏差：finish_with 命名、append_with_meta 接线留给 L2） |
| L2 | ✅ 完成（fix-4：MessageSink::append_with_meta 旁路 + runtime 捕获点（流式/非流式汇合，meta 四件套）；RunRecorder override 接线；RunFinal 全量迁移（session /new、server shutdown、cli run×4、repl×3——REPL 用 run-only 口径，session/server 用全 stats 口径，均先于重置快照，顺手修复 session /new 重置后读 0 的旧 bug；mobile 留 L4）；runtime 3 新测试 + session 文件库端到端测试；620+11+26+1+101+5+1 测试通过；02-core 两文档同步） |
| L3 | ✅ 完成（fix-3：vendor 补丁×2 + convert 附件分支 + registry 归一化；617+43+110 测试通过；03-models/07-build-dev 文档同步；交接说明见会话记录） |
| L4 | ✅ 完成（fix-4：protocol v4 AttachmentDto/Submit/User + 契约；core config [attachments] 节；mobile attachments.rs ingest + Submit 门控 + AttachmentDataProxy 请求装载 + resume 回填；transcripts.rs 增量持久化 + 两级重建 + 悬挂 Meta + 存储健康横幅；启动 GC；session list tokens；FFI import_attachment + 绑定再生成；625+96+38 等全绿；5 篇文档同步。偏差：session.rs 按 L2 预留注释补了 attachments pass-through（约 8 行）；transcript append 收口用增量同步任务实现（append 点在 session crate 无法逐点挂钩）；重复 sha256 文件同字节幂等覆盖（store 无 row-only 插入）） |
| L5 | ✅ Kotlin 侧完成（2026-10-21）：composer 附件按钮/Photo Picker（ImageOnly，实验视频开时先弹「图片/视频」选择）/拷贝+HEIC 预转码管线（API 28+ ImageDecoder；26/27 明确报不支持）/附件 chips（导入中 spinner + 就绪图标/文件名/尺寸/删除）/发送预检（文本必填非空白、图视频互斥、≤50 张）+ Submit JSON 拼装；带附件提交等 `request_start` 终态才清草稿（门控拒绝 Error notice → snackbar，草稿与 chips 保留）；设置页「附件输入」实验开关（直调 set/getExperimentalVideo）；会话列表 token 摘要（双 0 省略）；transcript 用户气泡只读附件 chips；HistoryScreen 活动标记 ● Unicode 违规顺手修正。`compileDebugKotlin` 通过；真机验证清单待人工执行。偏差：importAttachment FFI 调用暂经反射封装（L6 重跑 uniffi-gen 窗口内工作树 binding 短暂缺该导出，TODO(binding) 在 RuntimeBridge；binding 完整后可换直调）；附件按钮落位为输入行左缘（ComposerFocusPanel 首行是状态/选择器卡，操作位放输入区更符合惯例） |
| L6 | ✅ 完成（fix：protocol `project_from_messages` 共享投影（原 mobile `messages_to_transcript` 搬迁，含 reasoning 还原/附件引用投影/工具折叠/悬挂 tool_calls，9 项快照测试）；mobile 回退路径换薄委托（防双份漂移）；TUI `entry_from_message` 补 reasoning 映射 + rebuild 测试；FFI `set/get_experimental_video`（toml_edit 落盘保留其余内容 + 内存即时生效，mobile 测试覆盖持久化/重启恢复/零扰动）；uniffi 绑定再生成；674 测试全绿 + workspace check 通过。偏差：ffi crate 因 lib 与依赖同名致 doctest E0464（HEAD 即复现）补 `doctest = false`；6 篇文档同步） |
| 集成 | ✅ orchestrator（2026-10-21）：补齐 L4 遗漏的 FFI `import_attachment` 导出（ffi lib.rs，委托 mobile `import_attachment_json`，&str 签名对齐）+ 重跑 uniffi-gen（绑定 110289 字节，importAttachment/set/getExperimentalVideo 三导出齐全，L5 反射兜底自动生效）；`compileDebugKotlin` 通过；修复 ws_flow 存量环境泄漏（夹具 toml 加 `[skills] enabled=false`——skill 发现扫 ~/.agents/skills 等用户级目录，开发机真实 skills 泄漏进断言；10/10 绿）；approval_flow 确认为并行偶发（单测稳定全绿）；workspace 全量 ~1500+ 测试 0 失败。**真机事故与修复**：首次装机卡「runtime 启动中」——根因是 `build-apk.sh` 不做交叉编译（前置见 android-build.md [1/3]），旧 jniLibs .so（10-20 21:35）配新 Kotlin 绑定 → `UniffiLib <clinit>` UnsatisfiedLinkError → FFI create 永不执行；已按构建链重编 .so（15.9MB，符号 6 处命中）+ 重打 APK + 覆盖安装，logcat 确认 `ffi create: success → runtime created ✓`；给 build-apk.sh 加 .so 新鲜度硬守卫（含 find 优先级修正，正负两向验证）防再犯 |
| 增量 | ✅ read_file 读图 + 附件缩略图 FFI（2026-10-21，§6.5 缓做项落地）：① mcp-builtin fs.rs 字节读 + magic 嗅探（jpeg/png/webp/gif 白名单；视频/非 UTF-8 → 结构化 error 提示）+ crate 内精简归一化（≤2048/≤5MB/JPEG q85、小 PNG passthrough，常量与 mobile 对齐；归一化在 host 侧避免原图 base64 走 JSON-RPC）→ `[text 说明, image block]` 双块；② core `ToolOutput.attachments`（additive）+ `downgrade_content_blocks` 改 `(String, Vec<Attachment>)`（Image→附件收集+占位文本，畸形 base64 降级不中断）+ runtime 顺序/并行两路径透传 + `limit_tool_output` 截断保附件；③ genai convert 1:N 展开（`expand_chat_message`：ToolResponse + 紧随 user Binary parts；全引用/无附件 wire 不变）；④ mobile `AttachmentDataProxy.fill` 对 data=Some 超总封顶就地丢弃（R5）+ `attachment_thumbnail_base64`；⑤ FFI `getAttachmentThumbnail(id, maxPx)` + uniffi 绑定再生成（getAttachmentThumbnail 齐全）；926+ 测试全绿 + workspace check 通过；04-tools/02-core/03-models/06-mobile 文档同步 |
| 增量 | ✅ 导入附件 token 估算（2026-10-21，chip 显示 token 替代文件大小）：① mobile attachments.rs 新增 `estimate_attachment_tokens(base_url, api_key, model_id, mime, data_base64) -> Option<u64>`——`POST {base}/tokenizer`（智谱官方分模态入口，chat usage 不分模态），请求体 = data:URL 前缀 base64 的 image_url/video_url 块（与 chat wire 同形态）+ text `"."` 占位，解析 `usage.image_tokens`/`usage.video_tokens`；总预算 3s 硬切（tokio::time::timeout 总闸），HTTP 4xx 降级重试一次 `glm-4.6v`（tokenizer 枚举已知含它），其余失败 → None；Android TLS = webpki 静态根证书（core list_remote_models 同款，reqwest 声明与 core 对齐）；② runtime `import_attachment_json` 导入成功、返回前 await 估算，成功 JSON 增加可选键 `"tokens": <u64>`，失败/不可用省略（Kotlin 契约已就位：缺键收敛 null，chip 回退文件大小）；`ImportedAttachment` 增携归一化字节（估算免二次读盘）；③ 门控 `estimate_import_tokens`：当前会话主模型（model_alias → resolve_model）provider base_url host 为 bigmodel.cn/z.ai（`is_zhipu_endpoint`）且 FFI set_api_key 已注入密钥（内存 map，不猜 env）——非智谱/无 key 零网络跳过；④ 测试 9 项新增（tokenizer_mock 手写 HTTP 挡板：脚本化响应 + 请求录制）：body 形态 / usage 解析矩阵 / host 门控 / 成功路径 / 4xx 降级恰好一次 / 失败矩阵（5xx 不降级、非 JSON、死端口、非 image-video mime）/ 3s 硬切 / import JSON 两态（无 key / 非智谱省略；mock → tokens 就位 + 主模型 id 断言）；108+9+5 测试全绿 + workspace check 通过；06-mobile 两文档（openslate-mobile.md §10/§12.1 + mobile-ffi.md 契约行）同步 |
| 增量 | ✅ 图片 token 方向反转——响应侧回填（2026-10-21，协议 v5）：用户拍板 ① 发起 API 请求之前绝不把附件数据发到任何服务器——**删除导入侧 tokenizer 预估全套**（attachments.rs `estimate_attachment_tokens`/`is_zhipu_endpoint`/`tokenizer_client`/`tokenizer_mock`、runtime `estimate_import_tokens`、import JSON `tokens` 键、`ImportedAttachment.data`、mobile reqwest/webpki-roots 依赖增量、相关 9 测试）；② 只有真实模型响应顺带携带的 image token 信息才显示。新增链路：**vendor 补丁 #3**（genai `Usage.image_input_tokens: Option<u64>` 顶层归一化计数；openai chat `into_usage` 从 raw Value 抬升 `prompt_tokens_details.image_tokens`（正数才透传，流式/网关统一），openai_resp `InputTokensDetails.image_tokens` + `From<RespUsage>` 抬升 + `is_empty` 纳入；其余 adapter 构造点机械补 None；vendor 测试带/缺/0/畸形矩阵）→ **core** `Usage.image_tokens: Option<u64>`（additive serde default+skip，全 workspace ~44 构造点机械补 None；serde 兼容测试）→ **model-genai** convert 双路径映射 → **protocol v5**（`AttachmentDto.tokens`，契约样例带/不带，`protocol_version_pinned`=5，快照内嵌 proto 4→5）→ **mobile 回填**（新模块 attachment_tokens.rs：EventSink 新增 tap 在广播线程收 `Usage` 闩**首次调用**值（后续 agentic 调用忽略）/`TurnOk`·`TurnError` 收尾推 completed 队列（tap 绝不锁 core——引擎广播 TurnOk 时持 core 锁）；transcripts sync_loop 每周期持锁消费、更新 live transcript `EntryDto::User.attachments[].tokens` → 快照自然携带；**单图限定**（恰 1 张图才回填，多图不拆分）；Submit 标记在 send spawn 内 dispatch 前设置、未落地清除）；**重启可见性**：transcript_entries 追加式不可变（回填晚于落库，无 UPDATE API）→ `rebuild_transcript` 经 `overlay_tokens_from_messages` 从 `messages.meta_json`（MessageMeta.usage.image_tokens）按同口径（单图 user → 其后第一条 assistant）重放，重启后首份快照即带 tokens。智谱 chat usage 不分模态 → 恒回退文件大小（符合预期）。测试：mobile 11 新（7 状态机 + 4 集成：单图快照回填+后续调用忽略/多图恒无/智谱口径无/重启重放）、core 1 新（Usage serde 兼容）、protocol v5 样例、vendor 2 新；指定四 crate 全绿 + ffi lib + workspace check 通过；06-mobile（openslate-mobile.md §12.1 改零网络 + 新 §12.5、mobile-ffi.md 契约行）、02-core（protocol v5、core Usage）、03-models（补丁 #3）文档同步 |
| 增量 | ✅ 图片导入提速（2026-10-21，真机「导入非常慢」修复）：① **小图快速通道**——mobile attachments.rs 与 mcp-builtin fs.rs 两份归一化副本同步加 header-only 尺寸读取（`image::ImageReader::into_dimensions`，只解析 SOF/IHDR 不碰像素）：JPEG/PNG 长边 ≤2048 且 ≤5MB → 字节原样 passthrough（零解码零编码零重采样；JPEG 本就在 wire 白名单；EXIF 方向不处理，字节原样保留交给 provider——全解码路径同样不处理，行为一致；header 失败回落全解码报准确错误；出口硬校验两路径都执行）；② resize 滤波器 Lanczos3 → **CatmullRom**（纯 Rust 无 SIMD 快数倍，2048 降采样画质对视觉模型够用，注释注明取舍）；③ 分段耗时日志（格式统一 `attachment timing:` 前缀）：normalize 内 sniff_header/decode/resize/encode、import_attachment 的 read/normalize/sha256/write/total、FFI import 入口/出口总耗时（mobile alog / mcp-builtin eprintln-to-stderr 零依赖）；④ 测试：mobile 3 新（小 JPEG passthrough 字节相等 / ≤dim 但 >5MB 噪点 JPEG 仍重编码 / 畸形 JPEG 回落解码错误）+ 既有断言更新；fs.rs 2 新（小 JPEG passthrough / 畸形回落）；`cargo test -p openslate-mobile -p openslate-mcp-builtin` 257 全绿 + workspace check 通过；06-mobile §12.1、04-tools/mcp-builtin.md 文档同步 |
| 增量 | ✅ import JSON 对比信息（2026-10-21，chip 点击展开）：成功对象增 `orig_bytes`/`orig_width`/`orig_height`/`width`/`height`/`processed`（`passthrough` 小图零转码 orig=out｜`transcode` 缩放重编码 orig=header 尺寸·out=缩放后｜`video_passthrough` 尺寸四键省略——嗅探层不解析容器头不重解析）；`normalize_image` 返回 `NormalizedImage`（mime/字节+对比字段，orig 尺寸 header 一次读取两路径共用、失败以解码兜底），`ImportedAttachment` 携带对比字段；测试：normalize 4 项断言更新（resize 断言 orig=3000×2000/out=2048×1365 等）、ingest 三态字段断言（PNG passthrough / MP4 video 尺寸 None / declared 说谎）、runtime `import_json_never_estimates_tokens` 扩展 passthrough 态 + 新 `import_json_reports_transcode_and_video_states`（3000×2000 JPEG transcode + MP4 尺寸键省略）；129 测试全绿 + workspace check 通过；06-mobile §12.1 契约、mobile-ffi.md importAttachment 行文档同步 |
| 增量 | ✅ 图片处理用户可配置（2026-10-21）：**迭代压缩达标**替代一刀切 2048/5MB——① attachments.rs：入口 guard 放宽为固定 50MB 源文件常量（`MAX_SOURCE_IMAGE_BYTES`，`max_image_mb` 语义迁移为处理后上限）；transcode 路径迭代（等比缩到 ≤用户 dim（CatmullRom）→ q85 ≤用户 mb 即成 → 质量降档 85→70→55 → 仍超且 ×0.85 后仍会真正变小则缩边重试（每轮从原图重采样）→ 地板 dim≤1024/缩无可缩 取最后结果 + warn 不再失败）；快速通道条件同步（header 长边 ≤用户 dim 且字节 ≤用户 mb）；NormalizedImage 增 quality/attempts（进日志逐档 `attempt=N q=Q …`）；出口硬断言 ≤6000²/≤5MB 保留（wire 公共分母）；② core config：`max_image_mb` 默认 10→**5**、语义注释迁移，validation 增上限（512≤dim≤6000、1≤max_image_mb≤5，默认 openslate.toml 无 [attachments] 节 → 存量安装零迁移风险）；③ FFI：`set_image_processing(max_dim,max_mb)`（core validation 同源校验非法值、toml_edit 零扰动落盘 `[attachments]` 两键、内存即时生效）+ `get_image_processing()`，persist 抽 `persist_attachments_keys` 共用 helper，uniffi 绑定再生成（setImageProcessing/getImageProcessing 齐全）；④ 测试：attachments 迭代档位达标（1550×1200 噪点 cap2MB 前提断言 q85 超限→降档达标）/ 地板（1100px 缩无可缩 attempts=3 q55 不失败）/ 快速通道边界随用户 dim（1024 时 1600px 转码 900px passthrough）/ 入口 50MB 拒收 + 11MB 假 jpeg 不再入口拒；runtime set/get 往返 + 非法值矩阵 + 落盘零扰动 + 重启装载 + **下一次导入即生效**（同一文件 2048 旋钮 passthrough → 1024 旋钮 transcode）；mobile+core 774 全绿 + workspace check 通过；06-mobile §12.1 / mobile-ffi / 02-core / call-chains 文档同步 |
