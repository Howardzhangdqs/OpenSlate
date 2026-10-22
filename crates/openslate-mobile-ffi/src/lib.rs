//! OpenSlate mobile runtime 的 UniFFI 边界（proc-macro 模式）。
//!
//! 暴露面与 PLAN §5-§6 一一对应：
//! - `create(paths, callback)` → 装配 + 首份 snapshot（create 即握手）
//! - `send(client_msg_json)` → 复用 ClientMsg/ServerMsg 协议语义
//! - `set_api_key(provider, value)` → 内存注入（宿主负责 Keystore 持久化）
//! - `resolve_host_call(id, ok, payload)` → host tool request/resolve
//! - `shutdown()` → host call fail-all + 审批全拒 + runtime 收束
//! - `list_sessions(offset)` / `open_session` / `delete_session` → 历史会话
//! - `set_mcp_host_token(token)`（namespace 函数）→ MCP host 鉴权注入
//! - `reload_mcp_servers()` → 外部 MCP 热重载（重连 + 工具重注册）
//! - `get/set_mcp_tool_name_template()` → MCP 工具注册名模板读写
//! - `get/set_mcp_builtin_tool_name_template()` → builtin（裸名）工具名
//!   模板读写（占位符仅 `{prefix}`/`{tool}`；落盘，生效走既有
//!   `reload_mcp_servers()`）
//! - `get/set_mcp_server_name()` → 聚合 server 键名读写（`{prefix}`
//!   占位符的展开值；生效同样走 `reload_mcp_servers()`）
//! - `set/get_experimental_video()` → 实验视频开关（落盘
//!   `[attachments] experimental_video` + 内存即时生效，Submit 门控
//!   下一轮即按新值放行/拒绝视频附件）
//!
//! 线程约定：所有方法立即返回（send 内部 spawn；shutdown 有限等待）；
//! `callback.on_event` 从 Rust 泓线程调用——宿主实现必须线程安全、
//! 尽快返回（重活转投宿主队列）。
//!
//! Panic 防线：workspace release 是 panic=abort，本 crate 以 `mobile`
//! profile（panic=unwind）构建，且每个入口 catch_unwind → Err(String)，
//! 绝不让 Rust panic 穿过 FFI 边界。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use openslate_mobile::{MobilePaths, MobileRuntime, RuntimeOptions};

// uniffi 0.32 proc-macro 模式：命名空间 + UniFfiTag + FFI 脚手架。
uniffi::setup_scaffolding!("openslate_mobile");

/// Android 宿主注入的真实路径（全部必须可写；Rust 不做路径发现）。
#[derive(uniffi::Record)]
pub struct MobilePathsDto {
    pub config_dir: String,
    pub workspace_dir: String,
    pub data_dir: String,
    pub cache_dir: String,
}

/// 宿主事件回调：每条事件 = 一个 JSON 对象（ServerMsg 或
/// `host_call_requested` 信封；`type` 字段区分，snake_case 命名空间不重叠）。
#[uniffi::export(callback_interface)]
pub trait EventCallback: Send + Sync {
    fn on_event(&self, event: String);
}

/// FFI 错误（uniffi 要求错误为枚举 + Display，不能是裸 String）。
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum MobileError {
    /// runtime 装配失败（目录 bootstrap / AppContext 构建）。
    #[error("create failed: {reason}")]
    CreateFailed { reason: String },
    /// 消息发送失败（无法解析 / runtime 已关停）。
    #[error("send failed: {reason}")]
    SendFailed { reason: String },
}

/// FFI 对象（包一层 MobileRuntime，避免 mobile crate 依赖 uniffi）。
#[derive(uniffi::Object)]
pub struct OpenSlateRuntime {
    inner: Arc<MobileRuntime>,
}

fn catch<T>(
    label: &str,
    err: MobileError,
    f: impl FnOnce() -> anyhow::Result<T>,
) -> Result<T, MobileError> {
    std::panic::catch_unwind(AssertUnwindSafe(f))
        .map_err(|p| {
            let panic_text = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".to_owned());
            let reason = format!("{label}: rust panic: {panic_text}");
            tracing::error!("{reason}");
            err_dup(&err, reason)
        })
        .and_then(|r| r.map_err(|e| err_dup(&err, format!("{label}: {e}"))))
}

/// 把 reason 填进同类错误的克隆（保持 Kotlin 侧单一 catch 类型）。
fn err_dup(err: &MobileError, reason: String) -> MobileError {
    match err {
        MobileError::CreateFailed { .. } => MobileError::CreateFailed { reason },
        MobileError::SendFailed { .. } => MobileError::SendFailed { reason },
    }
}

#[uniffi::export]
impl OpenSlateRuntime {
    /// 装配 runtime：磁盘 bootstrap → 会话核心 → 首份 snapshot 事件。
    #[uniffi::constructor]
    pub fn create(
        paths: MobilePathsDto,
        callback: Box<dyn EventCallback>,
    ) -> Result<Self, MobileError> {
        openslate_mobile::alog!("ffi create: entry");
        let result = catch(
            "create",
            MobileError::CreateFailed {
                reason: String::new(),
            },
            || {
                let mobile_paths = MobilePaths {
                    config_dir: paths.config_dir.into(),
                    workspace_dir: paths.workspace_dir.into(),
                    data_dir: paths.data_dir.into(),
                    cache_dir: paths.cache_dir.into(),
                };
                // UniFFI 0.32 回调接口以 Box<dyn> 抬升；转 Arc 进 runtime。
                let cb = Arc::new(UniffiCallbackAdapter(callback));
                let runtime =
                    MobileRuntime::create_with(mobile_paths, cb, RuntimeOptions::default())
                        .map_err(|e| anyhow::anyhow!("{e:#}"))?;
                openslate_mobile::alog!("ffi create: success");
                Ok(Self { inner: runtime })
            },
        );
        match &result {
            Ok(_) => {}
            Err(e) => openslate_mobile::alog!("ffi create: FAILED: {e}"),
        }
        result
    }

    /// 发送 ClientMsg（JSON 文本）。非阻塞（内部 spawn）。
    pub fn send(&self, msg: String) -> Result<(), MobileError> {
        catch(
            "send",
            MobileError::SendFailed {
                reason: String::new(),
            },
            || self.inner.send(&msg).map_err(|e| anyhow::anyhow!("{e:#}")),
        )
    }

    /// 注入 provider API key（内存；不落盘）。
    pub fn set_api_key(&self, provider: String, value: String) {
        self.inner.set_api_key(&provider, &value);
    }

    /// 设置 HTTP(S) 出站代理（受限网络：宿主可经 adb reverse 共享电脑
    /// 代理）。进程级、立即生效于后续 provider 构建；空串清除。
    pub fn set_http_proxy(&self, url: String) {
        // NOTE: 进程级 env；约定由宿主主线程在回合间调用（2021 edition
        // 中 set_var 为 safe；workspace 尚未迁 2024）。
        if url.is_empty() {
            std::env::remove_var("OPENSLATE_HTTP_PROXY");
            openslate_mobile::alog!("ffi: http proxy cleared");
        } else {
            std::env::set_var("OPENSLATE_HTTP_PROXY", &url);
            openslate_mobile::alog!("ffi: http proxy set to {url}");
        }
    }

    /// 设置 bash 工具后端多选（逗号分隔："native" / "termux" /
    /// "native,termux"）。热生效：单选 → 工具名 `bash`；双选 →
    /// `bash`（native）+ `termux_bash`（termux）。
    pub fn set_exec_backends(&self, backends: String) {
        self.inner.set_exec_backends(&backends);
    }

    /// 热重载外部 MCP server（设置页增删改 server 后调用，新工具清单
    /// 免重启生效）。读最新落盘配置 → 重连 → 重注册工具；阻塞至全部
    /// server 连接完成（各 server 有内部连接超时；宿主须在 IO 线程
    /// 调用）。返回
    /// `{"ok":true,"tools":N,"servers":{"name":count,…},"failed":[…]}`
    /// 或 `{"ok":false,"tools":0,"servers":{},"failed":[…],"error":"…"}`
    /// （失败时旧工具保留，重启后按新配置装配）。
    pub fn reload_mcp_servers(&self) -> String {
        openslate_mobile::alog!("ffi: reload_mcp_servers");
        self.inner.reload_mcp()
    }

    /// 读取当前生效的 MCP 工具名模板（未设置 / 空白 → 默认
    /// `"{prefix}_{server}__{tool}"`；读最新落盘配置）。占位符：
    /// `{prefix}` = 服务名（[mcp.servers] 键）、`{server}` = 上游服务
    /// 器名（聚合名 slug 段）、`{tool}` = 不带前缀的工具名。
    pub fn get_mcp_tool_name_template(&self) -> String {
        openslate_mobile::alog!("ffi: get_mcp_tool_name_template");
        self.inner.get_mcp_tool_name_template()
    }

    /// 读取当前生效的 builtin 工具名模板（小模板；未设置 / 空白 →
    /// 默认 `"{prefix}_{tool}"`；读最新落盘配置）。作用于不含 `__`
    /// 的裸上游名——聚合 host 内置 server 的裸名与桌面直连裸名共用
    /// 这条展开分支。占位符仅 `{prefix}`/`{tool}`（无 `{server}`）。
    pub fn get_mcp_builtin_tool_name_template(&self) -> String {
        openslate_mobile::alog!("ffi: get_mcp_builtin_tool_name_template");
        self.inner.get_mcp_builtin_tool_name_template()
    }

    /// 设置 builtin 工具名模板：校验（与启动校验同一套规则：必须含
    /// `{tool}`、占位符仅 `{prefix}`/`{tool}`、拒 `{server}`、禁字面
    /// `__`）→ 落盘；空白 = 清除回默认。**不自动热重载**——新模板经
    /// 既有 `reload_mcp_servers()` 生效（reload 会按新模板的展开名交接
    /// HostTool 让位/兜底）。返回 `{"ok":true}` 或
    /// `{"ok":false,"error":"…"}`。
    pub fn set_mcp_builtin_tool_name_template(&self, template: String) -> String {
        openslate_mobile::alog!("ffi: set_mcp_builtin_tool_name_template({template:?})");
        self.inner.set_mcp_builtin_tool_name_template(template)
    }

    /// 设置 MCP 工具名模板：校验（与启动校验同一套规则）→ 落盘；
    /// 空白 = 清除回默认。**不自动热重载**——新模板经既有
    /// `reload_mcp_servers()` 生效（重连后按新模板重注册工具名）。
    /// 返回 `{"ok":true}` 或 `{"ok":false,"error":"…"}`。
    pub fn set_mcp_tool_name_template(&self, template: String) -> String {
        openslate_mobile::alog!("ffi: set_mcp_tool_name_template({template:?})");
        self.inner.set_mcp_tool_name_template(template)
    }

    /// 读取当前聚合 server 名（`[mcp.servers]` 的键，模板 `{prefix}`
    /// 占位符的展开值）。恰好一个键 → 该键；多键 → termux 优先否则
    /// 任取；空 / 读失败 → `termux`。
    pub fn get_mcp_server_name(&self) -> String {
        openslate_mobile::alog!("ffi: get_mcp_server_name");
        self.inner.get_mcp_server_name()
    }

    /// 改聚合 server 名（即 `{prefix}` 值）：校验（非空、
    /// [A-Za-z0-9_.-]、不含 `__`）→ 落盘重命名（transport/headers
    /// 原样搬移）；与当前名相同 → 幂等成功。**不自动热重载**——新名
    /// 经既有 `reload_mcp_servers()` 生效（重连后注册名前缀 /
    /// namespace 随新键名变化）。返回 `{"ok":true}` 或
    /// `{"ok":false,"error":"…"}`。
    pub fn set_mcp_server_name(&self, name: String) -> String {
        openslate_mobile::alog!("ffi: set_mcp_server_name({name:?})");
        self.inner.set_mcp_server_name(name)
    }

    /// 实验视频开关（存储 v5 §3.5；设置页「实验性功能」，L4→L6 交接，
    /// 签名与 L5 已约定）。写 openslate.toml
    /// `[attachments] experimental_video`（保留文件其余内容）+ 同步更新
    /// 运行中内存配置——Submit 附件门控下一轮即生效，无需重启。返回
    /// `{"ok":true}` 或 `{"ok":false,"error":"…"}`（落盘失败时内存
    /// 不变）。
    pub fn set_experimental_video(&self, enabled: bool) -> String {
        openslate_mobile::alog!("ffi: set_experimental_video({enabled})");
        self.inner.set_experimental_video(enabled)
    }

    /// 读实验视频开关当前生效值（内存配置：装配期从落盘装载 + set 即时
    /// 更新，与 Submit 门控消费的值同源）。
    pub fn get_experimental_video(&self) -> bool {
        openslate_mobile::alog!("ffi: get_experimental_video");
        self.inner.get_experimental_video()
    }

    /// 图片处理双旋钮（设置页「图片最长边 / 处理后文件大小上限」）：
    /// `max_dim` = 等比缩放目标边长（值域 512–6000），`max_mb` = 处理后
    /// 大小上限 MB（值域 1–5；归一化迭代压缩 q85→70→55 → ×0.85 缩边的
    /// 达标线）。值域外 → `{"ok":false,"error":"…"}`（core validation
    /// 同源校验）。合法 → 写 openslate.toml `[attachments]` 两键（保留
    /// 文件其余内容）+ 同步更新运行中内存配置——下一次 import_attachment
    /// 即生效，无需重启。返回 `{"ok":true}` 或 `{"ok":false,"error":…}`
    ///（落盘失败时内存不变）。
    pub fn set_image_processing(&self, max_dim: i64, max_mb: i64) -> String {
        openslate_mobile::alog!("ffi: set_image_processing(max_dim={max_dim}, max_mb={max_mb})");
        self.inner.set_image_processing(max_dim, max_mb)
    }

    /// 读图片处理双旋钮当前生效值（内存配置：装配期从落盘装载 + set
    /// 即时更新，与归一化消费的值同源）。返回
    /// `{"max_image_dim":N,"max_image_mb":M}`。
    pub fn get_image_processing(&self) -> String {
        openslate_mobile::alog!("ffi: get_image_processing");
        self.inner.get_image_processing()
    }

    /// 导入附件（PLAN §3.6；Kotlin 侧 cache 拷贝 + HEIC 预转码后调用，
    /// 路径传递不走 base64）。magic-byte 嗅探 → sha256 → 图像归一化
    ///（小图 JPEG/PNG header 快速通道 passthrough；其余按用户双旋钮
    /// 迭代压缩：≤max_dim 等比缩 + q85→70→55 → ×0.85 缩边至 ≤max_mb，
    /// 见 set_image_processing）→ files/ 落盘 → attachments 行（run_id
    /// NULL 暂存）。返回含 orig/处理后对比信息的 JSON（见 mobile-ffi.md
    /// 契约）或 `{"ok":false,"error":"…"}`。
    pub fn import_attachment(
        &self,
        source_path: String,
        declared_mime: String,
        filename: String,
    ) -> String {
        openslate_mobile::alog!("ffi: import_attachment({source_path:?}, {declared_mime:?}, {filename:?})");
        self.inner
            .import_attachment_json(&source_path, &declared_mime, &filename)
    }

    /// 附件缩略图（des-2 Android lane 契约）。`id` = import_attachment
    /// 返回的附件 id；原图经 sha256 从 files 读出，长边缩到 `max_px`
    /// （只缩不放；`<=0` 按 256、上限 2048）后 JPEG q80 编码。返回
    /// `{"ok":true,"data":"<base64>","mime":"image/jpeg"}` 或
    /// `{"ok":false,"error":"…"}`（附件不存在 / 视频等非图像 / 文件
    /// 缺失 / 解码失败均走 error）。阻塞（磁盘 + 解码），IO 线程调用。
    pub fn get_attachment_thumbnail(&self, id: String, max_px: i32) -> String {
        openslate_mobile::alog!("ffi: get_attachment_thumbnail(id={id:?}, max_px={max_px})");
        self.inner.get_attachment_thumbnail_json(&id, max_px)
    }

    // ── 工具可见性（工具管理页）──────────────────────────────────────

    /// 全部已注册工具清单（JSON 数组，按 name 排序）：
    /// `[{"name":"bash","description":"…"}, …]`。
    pub fn list_tools(&self) -> String {
        openslate_mobile::alog!("ffi: list_tools");
        self.inner.list_tools_json()
    }

    /// 当前生效 agent（root）的工具可见性（JSON 对象）：
    /// `{"tools":[…],"hidden_tools":[…]}`。
    pub fn get_agent_tool_visibility(&self) -> String {
        openslate_mobile::alog!("ffi: get_agent_tool_visibility");
        self.inner.get_agent_tool_visibility_json()
    }

    /// 全量覆写 root agent 的 `tools:` / `hidden_tools:`（工具对 agent
    /// 可见/隐藏）。外科手术式改写 `agents/root.md` frontmatter（其余
    /// 字段与正文不动）并热生效——下一对话回合即按新可见性过滤，无需
    /// 重启 app。返回 `{"ok":true}` 或 `{"ok":false,"error":"…"}`。
    pub fn set_agent_tool_visibility(
        &self,
        tools: Vec<String>,
        hidden_tools: Vec<String>,
    ) -> String {
        openslate_mobile::alog!(
            "ffi: set_agent_tool_visibility(tools={tools:?}, hidden={hidden_tools:?})"
        );
        self.inner
            .set_agent_tool_visibility_json(tools, hidden_tools)
    }

    // ── 角色定位（system prompt 编辑；设置页）────────────────────────

    /// 读当前生效 agent（root）的角色定位正文（= 每回合 system prompt
    /// 的全量源；内存树快照，与 runner 每回合拼 system prompt 同源）。
    /// 同步、无副作用，可主线程调用。
    pub fn get_agent_persona(&self) -> String {
        openslate_mobile::alog!("ffi: get_agent_persona");
        self.inner.get_agent_persona()
    }

    /// 全量覆写 root agent 的角色定位正文（= 下一回合 system prompt 的
    /// 全量源）并热生效：外科手术式替换 `agents/root.md` **正文**
    /// （frontmatter 逐字节保留——tools 白名单机制不受影响）→ 重装
    /// agent 树热替换——下一对话回合即用新 system prompt，无需重启
    /// app。skills catalog 由 runner 在正文之上追加，本编辑不触碰。
    /// 入参规范化（trim；空白回默认文案）。阻塞（文件读写 + 树重装），
    /// **IO 线程调用**。返回生效正文；失败抛 `MobileError`（reason 带
    /// `set_agent_persona:` 前缀）。
    pub fn set_agent_persona(&self, body: String) -> Result<String, MobileError> {
        openslate_mobile::alog!("ffi: set_agent_persona(len={})", body.len());
        catch(
            "set_agent_persona",
            MobileError::SendFailed {
                reason: String::new(),
            },
            || self.inner.set_agent_persona(&body).map_err(|e| anyhow::anyhow!("{e:#}")),
        )
    }

    /// 应答 host call。`ok=true` → payload 为结果 JSON；`ok=false` →
    /// payload 为错误说明。返回 false = 无此在途调用（超时/已答/关停）。
    pub fn resolve_host_call(&self, id: u64, ok: bool, payload: String) -> bool {
        self.inner.resolve_host_call(id, ok, payload)
    }

    /// 历史会话列表（JSON 数组：id/title/status/started_ms/cost_usd）。
    /// 分页：每页 50 条，offset 递增（0、50、100…；首页传 0）。
    pub fn list_sessions(&self, offset: u32) -> String {
        openslate_mobile::alog!("ffi: list_sessions(offset={offset})");
        self.inner.list_sessions_json(offset)
    }

    /// 拉取 provider 的可用模型清单（设置页 Provider 域"自动检测"）。
    /// 返回 `{"ok":true,"models":[...]}` 或 `{"ok":false,"error":"..."}`
    /// （密钥未配置 / 网络 / 协议错误）。含 10s 超时，宿主须在 IO 线程调用。
    pub fn list_provider_models(&self, provider: String) -> String {
        openslate_mobile::alog!("ffi: list_provider_models(provider={provider})");
        self.inner.list_provider_models(provider)
    }

    // ── 模型元数据注册表（数据源页面 / 元数据自动补全）────────────

    /// 数据源列表 + 各源本地状态（条目数 / 更新时间 / 体积）。
    pub fn registry_sources(&self) -> String {
        openslate_mobile::alog!("ffi: registry_sources");
        self.inner.registry_sources_json()
    }

    /// 手动更新一个数据源（阻塞，最长 120s；IO 线程调用）。
    /// 返回 `{"ok":true,"entries":N}` 或 `{"ok":false,"error":"..."}`。
    pub fn registry_update_source(&self, id: String) -> String {
        openslate_mobile::alog!("ffi: registry_update_source(id={id})");
        self.inner.registry_update_source_json(id)
    }

    /// 启动时按需自动更新（后台执行；本地缺失或过期才拉取）。
    pub fn registry_schedule_auto_update(&self) {
        openslate_mobile::alog!("ffi: registry_schedule_auto_update");
        self.inner.registry_schedule_auto_update();
    }

    /// 本地条目搜索 / 浏览（数据源页面「查看条目 / 跨源搜索」）。纯
    /// 本地（首次会读盘解压，宿主建议 IO 线程调用）。`source_id` 空 =
    /// 跨全部已缓存源搜索；`query` 空 = 浏览模式（前 limit 条，键字
    /// 典序）；`limit <= 0` 按 50 处理。返回
    /// `{"total":N,"results":[{source,sourceId,id,ctx,out,vision,reasoning,tool,priceIn,priceOut}]}`：
    /// `total` = 匹配总数（分页计数），`results` 截断到 limit；`id`
    /// 为源 JSON 原始键（保留 provider 前缀）；数值 / 能力 / 计价字段
    /// 源没给为 null。未知 source_id → `{"total":0,"results":[]}`。
    pub fn registry_search(&self, source_id: String, query: String, limit: i32) -> String {
        openslate_mobile::alog!(
            "ffi: registry_search(source_id={source_id:?}, query={query:?}, limit={limit})"
        );
        self.inner.registry_search_json(source_id, query, limit)
    }

    /// 模型元数据查询（本地优先，miss 时在线兜底；IO 线程调用）。
    pub fn lookup_model_meta(&self, model_id: String) -> String {
        openslate_mobile::alog!("ffi: lookup_model_meta(model_id={model_id})");
        self.inner.lookup_model_meta_json(model_id)
    }

    /// 当前会话 run id（无进行中会话返回 null）。
    pub fn current_run_id(&self) -> Option<String> {
        self.inner.current_run_id()
    }

    /// 按 call_id 拉取工具调用的完整输出（内存 history 直读；UI 详情
    /// 弹窗懒加载用）。无匹配（未知 id / history 已被压缩 / 子代理工具）
    /// 返回 null，调用方回退 preview。
    pub fn tool_output_by_call_id(&self, call_id: String) -> Option<String> {
        openslate_mobile::alog!("ffi: tool_output_by_call_id(call_id={call_id})");
        self.inner.tool_output_by_call_id(call_id)
    }

    /// 切换到指定历史会话（成功后回推新 snapshot 事件）。
    pub fn open_session(&self, run_id: String) -> Result<(), MobileError> {
        openslate_mobile::alog!("ffi: open_session {run_id}");
        self.inner
            .open_session(run_id)
            .map_err(|e| MobileError::SendFailed {
                reason: format!("open_session: {e:#}"),
            })
    }

    /// 删除历史会话（连同全部消息/步骤数据）。返回是否删除成功：
    /// false = 拒绝或失败——当前活动会话不可删（先新建会话切换走）；
    /// run 不存在 / 数据库错误也返回 false。Kotlin 宿主负责同步删除
    /// 自己的 transcript 文件（Rust 只管数据库行）。
    pub fn delete_session(&self, run_id: String) -> bool {
        openslate_mobile::alog!("ffi: delete_session {run_id}");
        self.inner.delete_session(run_id)
    }

    /// 优雅关停（幂等）。
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }
}

/// 注入 MCP host 鉴权 token（进程级，对所有 HTTP MCP server 生效：
/// 请求头加 `Authorization: Bearer <token>`，toml 不必持久化 token）。
///
/// **必须在 `create` 之前调用**——MCP 连接在装配期建立，之后注入不
/// 影响已建连接。namespace 级函数（非对象方法）：它配置的是进程而非
/// 某个 runtime 实例。
#[uniffi::export]
pub fn set_mcp_host_token(token: String) {
    openslate_mobile::alog!("ffi: set_mcp_host_token ({} bytes)", token.len());
    openslate_mobile::set_mcp_host_token(&token);
}

/// UniFFI foreign callback → mobile `EventCallback`（Box 装进适配器，
/// 生命周期由 runtime 侧 Arc 持有）。
struct UniffiCallbackAdapter(Box<dyn EventCallback>);

impl openslate_mobile::EventCallback for UniffiCallbackAdapter {
    fn on_event(&self, event_json: String) {
        // FFI 边界最薄处：不做任何解析，直接转发字符串。
        self.0.on_event(event_json);
    }
}
