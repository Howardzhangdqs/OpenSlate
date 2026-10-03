package dev.openslate.mobile.bridge

import android.content.Context
import android.util.Log
import dev.openslate.mobile.BuildConfig
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import org.json.JSONObject
import uniffi.openslate.mobile.EventCallback
import uniffi.openslate.mobile.MobilePathsDto
import uniffi.openslate.mobile.OpenSlateRuntime
import java.util.concurrent.atomic.AtomicReference

/**
 * UI 条目（transcript 的精简投影；完整镜像在 Rust 侧，UI 只保留渲染所需）。
 */
sealed class UiEntry {
    data class User(val text: String) : UiEntry()
    data class Assistant(val text: String) : UiEntry()
    data class Reasoning(
        val text: String,
        /** 折叠标签元数据："12.3s · ↑1.2k ↓3.4k"（usage 事件回填）。 */
        var meta: String? = null,
        /** 本条思维链开始流式输出的时间戳（appendDelta 打点），思考中 chip 实时秒数用。 */
        val startTs: Long = 0L,
    ) : UiEntry()
    data class Meta(val text: String) : UiEntry()
    data class ToolCall(
        val name: String,
        val argsPreview: String,
        var status: String, // running | done | failed
        val fullArgs: String = "",
        var output: String? = null,
        var expanded: Boolean = false,
    ) : UiEntry()
    data class Delegate(val agent: String, var done: Boolean) : UiEntry()
    data class Approval(val toolName: String, val decision: String) : UiEntry()
    object StepBreak : UiEntry()
}

data class PendingApprovalUi(
    val id: Long,
    val toolName: String,
    val riskLevel: String,
    val reason: String,
)

/** 配置视图（snapshot/config_changed 的投影）。 */
data class ProviderUi(
    val name: String,
    val baseUrl: String,
    val apiKeyEnv: String,
    val adapter: String?,
    /** 人类可读显示名（中文/空格/大小写均可）；null = 显示 name（内部 ID）。 */
    val title: String? = null,
) {
    /** UI 展示用：优先显示名，无则回退内部 ID。 */
    val displayName: String get() = title?.takeIf { it.isNotBlank() } ?: name
}

data class ModelUi(
    val entry: String,
    val provider: String,
    val modelId: String,
    val maxContextTokens: Long? = null,
    val maxOutputTokens: Long? = null,
    val supportsToolCall: Boolean = true,
    val supportsVision: Boolean = false,
    val supportsReasoning: Boolean = false,
    val inputPricePerMtok: Double? = null,
    val outputPricePerMtok: Double? = null,
)

data class ConfigUi(
    val providers: List<ProviderUi> = emptyList(),
    val models: List<ModelUi> = emptyList(),
    val levels: Map<String, String> = emptyMap(),
    val activeConfig: String = "",
)

data class RuntimeUiState(
    val ready: Boolean = false,
    val running: Boolean = false,
    val modelAlias: String = "",
    val entries: List<UiEntry> = emptyList(),
    val pendingApproval: PendingApprovalUi? = null,
    val sessionId: String = "",
    val lastError: String? = null,
    val config: ConfigUi = ConfigUi(),
    /** 进度：当前 step / 本回合工具调用数。 */
    val step: Int = 0,
    val toolCalls: Int = 0,
    /** 请求已发出但尚未收到任何首个 token（reasoning/content/tool）：
     *  期间若模型支持思维链则显示"思考中"，TTFT 等待计入思考时长。 */
    val awaitingFirstToken: Boolean = false,
)

/**
 * Rust MobileRuntime 的宿主桥。
 *
 * - 事件：UniFFI 回调（Rust 泵线程）→ Channel → 单消费协程 → StateFlow，
 *   全程保序。
 * - 模型回复以 delta 增量并入最后一条 Assistant 条目（对齐 Rust 侧
 *   StreamBuffers 语义）。
 * - host call（type=host_call_requested）由本层直接应答/上抛；Phase 1
 *   仅 mobile.ping 自动应答，Phase 3+ 起由各能力宿主分发。
 */
/** 历史会话摘要（list_sessions 投影）。 */
data class SessionSummaryUi(
    val id: String,
    val title: String,
    val status: String,
    val startedMs: Long,
    val costUsd: Double,
)

object RuntimeBridge {

    private const val TAG = "OpenSlateBridge"

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val eventChannel = Channel<String>(Channel.UNLIMITED)

    /** 当前事件消费协程（单例泵：start 前取消旧泵，避免 shutdown→start 后双消费者竞态）。 */
    private var pumpJob: Job? = null

    private val _state = MutableStateFlow(RuntimeUiState())
    val state: StateFlow<RuntimeUiState> = _state.asStateFlow()

    private val runtimeRef = AtomicReference<OpenSlateRuntime?>(null)
    private val startMutex = Mutex()
    private var started = false
    private var appContext: Context? = null

    /** 当前步起始时间戳（usage 事件计算步耗时用）。 */
    private var stepStartTs: Long? = null

    /** 工具名 → 待消费输出队列（同名工具并发时按序配对）。 */
    private val pendingToolOutputs =
        java.util.concurrent.ConcurrentHashMap<String, java.util.concurrent.ConcurrentLinkedDeque<String>>()

    private fun recordToolOutput(name: String, output: String) {
        pendingToolOutputs.getOrPut(name) { java.util.concurrent.ConcurrentLinkedDeque() }.add(output)
    }

    private fun takeToolOutput(name: String): String? =
        pendingToolOutputs[name]?.poll()

/** Rust on_event 回调（泵线程，非阻塞——只入队）。 */
    private val callback = object : EventCallback {
        override fun onEvent(event: String) {
            eventChannel.trySend(event)
        }
    }

    suspend fun start(context: Context) {
        startMutex.withLock {
            if (started) return
            started = true
            val appContext = context.applicationContext
            // Rust 装配（磁盘 bootstrap + sqlite + tokio）可能阻塞数秒，
            // 必须离线主线程（否则 ANR / UI 冻结）。
            withContext(Dispatchers.IO) {
                val configDir = appContext.filesDir.resolve("openslate").absolutePath
                val workspaceDir = appContext.filesDir.resolve("workspace").absolutePath
                val dataDir = appContext.filesDir.resolve("data").absolutePath
                val cacheDir = appContext.cacheDir.resolve("openslate").absolutePath
                Log.i(TAG, "start: creating runtime (config=$configDir)")
                try {
                    // MCP host token 必须在 create 之前注入：Rust 侧在装配期
                    // 建立 MCP 连接，运行时注入 Authorization（token 不落
                    // openslate.toml，持久化仅走 SecretStore 加密）。
                    // token() 可能触发 SecretStore 轮换写（首次/解密失败），
                    // 此处已在 IO 线程，安全。
                    val mcpToken = McpHostManager.token(appContext)
                    uniffi.openslate.mobile.setMcpHostToken(mcpToken)
                    val runtime = OpenSlateRuntime.create(
                        paths = MobilePathsDto(
                            configDir = configDir,
                            workspaceDir = workspaceDir,
                            dataDir = dataDir,
                            cacheDir = cacheDir,
                        ),
                        callback = callback,
                    )
                    runtimeRef.set(runtime)
                    this@RuntimeBridge.appContext = appContext
                    Log.i(TAG, "start: runtime created ✓ (native=${runtime.uniffiIsDestroyed.not()})")
                    // 重注入持久化密钥（Keystore → FFI 内存）；解密失败的
                    // provider 显式告警（UI 侧密钥管理页会显示"解密失败，
                    // 请重新录入"，此处不静默混作未配置）。
                    for (p in SecretStore.allProviders(appContext)) {
                        when (val r = SecretStore.load(appContext, p)) {
                            is SecretStore.KeyLoadResult.Ok -> {
                                runtime.setApiKey(p, r.value)
                                Log.i(TAG, "start: restored api key for provider=$p")
                            }
                            is SecretStore.KeyLoadResult.Failed -> Log.w(
                                TAG,
                                "start: api key decrypt FAILED for provider=$p" +
                                    " (${r.error?.message ?: r.error?.javaClass?.simpleName}), 需重新录入",
                            )
                            SecretStore.KeyLoadResult.Missing -> Unit
                        }
                    }
                    restoreHttpProxy()
                    restoreExecBackend()
                    restoreModelAlias()
                } catch (e: Throwable) {
                    Log.e(TAG, "start: runtime create FAILED", e)
                    _state.value = _state.value.copy(ready = false, lastError = e.message ?: e.javaClass.simpleName)
                    started = false
                    return@withContext
                }
                // 事件消费协程（start 后常驻）。先取消旧泵：同进程
                // shutdown→start 后 Channel 里可能残留旧消费者，双消费者
                // 会竞态分走事件、打破"单消费保序"假设。
                pumpJob?.cancel()
                pumpJob = scope.launch {
                    Log.i(TAG, "event pump: started")
                    var n = 0
                    for (event in eventChannel) {
                        n++
                        if (n <= 20 || n % 100 == 0) {
                            // 内容型日志（事件可含对话/工具参数）仅 debug，
                            // release 不剥离 logcat，防泄漏。
                            if (BuildConfig.DEBUG) {
                                Log.d(TAG, "event #$n: ${event.take(160)}")
                            }
                        }
                        try {
                            handleEvent(event)
                        } catch (t: Throwable) {
                            Log.w(TAG, "event handling failed: ${event.take(200)}", t)
                        }
                    }
                    Log.w(TAG, "event pump: channel closed")
                }
            }
            Unit
        }
    }

    val isReady: Boolean get() = _state.value.ready

    fun submit(text: String) {
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return
        // 乐观本地回显（Rust 侧推 transcript 但不广播 User 事件——与桌面
        // TUI 同语义）；失败保留气泡，与 TUI「可重发」行为一致。
        if (sendJson(JSONObject().put("type", "submit").put("text", trimmed).toString())) {
            mutate { copy(entries = entries + UiEntry.User(trimmed)) }
        }
    }

    fun cancel() = sendJson(JSONObject().put("type", "cancel").toString())

    fun newSession() = sendJson(JSONObject().put("type", "new_session").toString())

    fun setApiKey(provider: String, value: String) {
        runtimeRef.get()?.setApiKey(provider, value)
    }

    /** 设置页入口：注入内存 + Keystore 加密持久化。返回 false = 持久化失败
     *  （内存注入仍完成，本回合可用但重启丢失），调用方不得置「已配置」。 */
    fun setApiKeyAndPersist(provider: String, value: String): Boolean {
        setApiKey(provider, value)
        val ctx = appContext ?: return false
        return SecretStore.save(ctx, provider, value)
    }

    // ── bash 工具执行后端（native shell.run / termux）──────────────

    private const val PREF_EXEC_BACKEND = "exec_backend"

    /** 当前 bash 后端多选（CSV："native" / "termux" / "native,termux"）。 */
    fun execBackends(): String =
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.getString(PREF_EXEC_BACKEND, "native") ?: "native"

    /** 切换 bash 后端多选：FFI 热切换 + 本地持久化（重启恢复）。
     *  命名规则（Rust 侧）：单选 → `bash`；双选 → `bash`=native + `termux_bash`。 */
    fun setExecBackendsAndPersist(backends: String) {
        runtimeRef.get()?.setExecBackends(backends)
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.edit()?.putString(PREF_EXEC_BACKEND, backends)?.apply()
        Log.i(TAG, "exec backends set to $backends")
    }

    private fun restoreExecBackend() {
        val backends = execBackends()
        runtimeRef.get()?.setExecBackends(backends)
        Log.i(TAG, "start: restored exec backends = $backends")
    }

    // ── 模型选择持久化（Rust set_model 仅内存态，重启即回默认；本地
    //    持久化 + 启动重放，保证用户可感知的模型选择不丢）─────────

    private const val PREF_MODEL_ALIAS = "model_alias"

    /** 当前选中的模型别名（默认 main）。 */
    fun modelAlias(): String =
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.getString(PREF_MODEL_ALIAS, "main") ?: "main"

    /** 切换模型：FFI 热切换 + 本地持久化（重启重放）。 */
    fun setModelAliasAndPersist(alias: String) {
        setModelAlias(alias)
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.edit()?.putString(PREF_MODEL_ALIAS, alias)?.apply()
        Log.i(TAG, "model alias set to $alias")
    }

    /** 启动重放：恢复用户上次选择的模型（仅非默认时发送）。 */
    private fun restoreModelAlias() {
        val alias = modelAlias()
        if (alias != "main") {
            setModelAlias(alias)
            Log.i(TAG, "start: restored model alias = $alias")
        }
    }

    // ── HTTP 代理（受限网络出站；经 adb reverse 共享电脑侧代理）──

    fun setHttpProxy(url: String, persist: Boolean = true) {
        runtimeRef.get()?.setHttpProxy(url)
        if (persist) {
            appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
                ?.edit()?.putString("http_proxy", url)?.apply()
        }
    }

    private fun restoreHttpProxy() {
        val url = appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.getString("http_proxy", "") ?: ""
        if (url.isNotBlank()) {
            runtimeRef.get()?.setHttpProxy(url)
            Log.i(TAG, "start: restored http proxy = $url")
        }
    }

    /** 密钥三态（UI 展示用）：区分「未配置」与「密文在但解不开」。 */
    enum class KeyPresence { CONFIGURED, MISSING, DECRYPT_FAILED }

    fun keyPresence(provider: String): KeyPresence {
        val ctx = appContext ?: return KeyPresence.MISSING
        return when (SecretStore.load(ctx, provider)) {
            is SecretStore.KeyLoadResult.Ok -> KeyPresence.CONFIGURED
            SecretStore.KeyLoadResult.Missing -> KeyPresence.MISSING
            is SecretStore.KeyLoadResult.Failed -> KeyPresence.DECRYPT_FAILED
        }
    }

    // ── 配置 CRUD（协议复用：persist 层落盘 → config_changed 刷新 UI）──

    fun upsertProvider(
        name: String,
        baseUrl: String,
        apiKeyEnv: String,
        adapter: String?,
        title: String? = null,
    ) = sendJson(
        JSONObject()
            .put("type", "upsert_provider")
            .put("name", name)
            .put(
                "provider",
                JSONObject()
                    .put("base_url", baseUrl)
                    .put("api_key_env", apiKeyEnv)
                    .apply { title?.let { put("title", it) } }
                    .put("max_attempts", 3)
                    .put("retry_base_ms", 500)
                    .put("adapter", adapter ?: JSONObject.NULL),
            ).toString()
    )

    fun upsertModel(m: ModelUi) = sendJson(
        JSONObject()
            .put("type", "upsert_model")
            .put("entry", m.entry)
            .put(
                "model",
                JSONObject()
                    .put("provider", m.provider)
                    .put("model", m.modelId)
                    .put("supports_tool_call", m.supportsToolCall)
                    .put("supports_vision", m.supportsVision)
                    .put("supports_reasoning", m.supportsReasoning)
                    .put(
                        "max_context_tokens",
                        m.maxContextTokens ?: JSONObject.NULL,
                    )
                    .put(
                        "max_output_tokens",
                        m.maxOutputTokens ?: JSONObject.NULL,
                    )
                    .put(
                        "input_price_per_mtok",
                        m.inputPricePerMtok ?: JSONObject.NULL,
                    )
                    .put(
                        "output_price_per_mtok",
                        m.outputPricePerMtok ?: JSONObject.NULL,
                    ),
            ).toString()
    )

    fun setLevel(level: String, entry: String) = sendJson(
        JSONObject().put("type", "set_level").put("level", level).put("entry", entry).toString()
    )

    fun setModelAlias(alias: String) = sendJson(
        JSONObject().put("type", "set_model").put("alias", alias).toString()
    )

    /** 注册/更新 MCP server（http 条目；重启会话后生效——MCP 无热更设计）。
     *  Authorization 不经此传入：token 由 start() 在 create 前经
     *  setMcpHostToken() 运行时注入（SecretStore 加密持久化，不落 toml）。 */
    fun upsertMcpServer(name: String, url: String) = sendJson(
        JSONObject()
            .put("type", "upsert_mcp_server")
            .put("name", name)
            .put("url", url)
            .toString()
    )

    /** 审批应答（choice: approve / deny / approve_all）。 */
    fun answerApproval(id: Long, choice: String) = sendJson(
        JSONObject().put("type", "approval_answer").put("id", id).put("choice", choice).toString()
    )

    fun sendJson(json: String): Boolean {
        val runtime = runtimeRef.get() ?: run {
            // 内容型日志（json 可含用户消息）仅 debug，release 不剥离 logcat。
            if (BuildConfig.DEBUG) {
                Log.w(TAG, "send: runtime not ready, dropping: ${json.take(80)}")
            }
            return false
        }
        return try {
            runtime.send(json)
            true
        } catch (e: Throwable) {
            Log.e(TAG, "send failed", e)
            _state.value = _state.value.copy(lastError = e.message)
            false
        }
    }

    suspend fun shutdown() {
        startMutex.withLock {
            runtimeRef.getAndSet(null)?.let {
                runCatching { it.shutdown() }.onFailure { t -> Log.w(TAG, "shutdown: ${t.message}") }
            }
            started = false
            // 关停时取消事件泵（下次 start 会重建），防旧泵滞留消费。
            pumpJob?.cancel()
            pumpJob = null
            TermuxExec.cancelAll()
            _state.value = RuntimeUiState()
        }
    }

    // ── 事件处理（单消费协程，天然有序）────────────────────────────────

    private fun handleEvent(raw: String) {
        val obj = JSONObject(raw)
        when (obj.optString("type")) {
            "snapshot" -> applySnapshot(obj.optJSONObject("session"))
            "delta" -> appendDelta(obj.optString("text", ""), assistant = true)
            "reasoning" -> appendDelta(obj.optString("text", ""), assistant = false)
            "tool_start" -> {
                val name = obj.optString("name", "tool")
                val fullArgs = obj.optString("args", "")
                mutate {
                    copy(
                        toolCalls = toolCalls + 1,
                        awaitingFirstToken = false,
                        entries = entries + UiEntry.ToolCall(
                            name = name,
                            argsPreview = fullArgs.take(64),
                            status = "running",
                            fullArgs = fullArgs,
                        )
                    )
                }
            }
            "tool_end" -> {
                val name = obj.optString("name", "tool")
                // 优先 Kotlin 侧缓存（host 工具的完整输出）；native 工具在 Rust
                // 进程内执行，没有 host call，用事件携带的 preview 兜底。
                val out = takeToolOutput(name)
                    ?: obj.optString("preview", "").ifBlank { null }
                mutate {
                    // Rust 保证 tool_end 按工具调用顺序发射：并发多条同名调用时，
                    // 第 N 个 tool_end 对应该名下第一个仍在 running 的条目。
                    // （indexOfLast 会在并发时把输出互相配错。）
                    val idx = entries.indexOfFirst { it is UiEntry.ToolCall && it.name == name && it.status == "running" }
                    if (idx >= 0) {
                        val e = entries[idx] as UiEntry.ToolCall
                        copy(entries = entries.toMutableList().also { it[idx] = e.copy(status = "done", output = out ?: e.output) })
                    } else this
                }
            }
            "usage" -> {
                // token 用量 → 回填最近思维链的 meta + 追加 Meta 行（含步耗时）。
                // 事件形态：{"type":"usage","usage":{"input_tokens":…,"output_tokens":…,
                //   "cached_input_tokens":…(可选),"reasoning_tokens":…(可选，网关注入细分时)}}
                val u = obj.optJSONObject("usage")
                val inTok = u?.optInt("input_tokens", -1) ?: obj.optInt("input_tokens", -1)
                val outTok = u?.optInt("output_tokens", -1) ?: obj.optInt("output_tokens", -1)
                // 思考专属 token：Rust 链路透传（vendor/genai anthropic 适配器提取网关
                // 注入的 OpenAI 风格细分；网关未报时为 -1，退回仅时长显示）。
                val thinkTok = u?.optInt("reasoning_tokens", -1) ?: -1
                // 缓存命中 token：OpenAI prompt_tokens_details.cached_tokens /
                // Anthropic cache_read_input_tokens 由 Rust 链路归一化为
                // cached_input_tokens 透传；网关未报时为 -1，不显示该段。
                val cachedTok = u?.optInt("cached_input_tokens", -1)
                    ?: obj.optInt("cached_input_tokens", -1)
                val elapsed = stepStartTs?.let { (System.currentTimeMillis() - it) / 1000.0 }
                if (inTok >= 0) {
                    val fmtTok = { n: Int -> if (n >= 1000) "%.1fk".format(n / 1000.0) else "$n" }
                    // chip：时长 + （有细分时）思考 token；网关未报细分则仅时长
                    // （总输出 token 与 ⚡ 行重复且易误导）。
                    val meta = buildString {
                        elapsed?.let { append("%.1fs".format(it)) }
                        if (thinkTok > 0) {
                            if (isNotEmpty()) append(" · ")
                            append("${fmtTok(thinkTok)} tok")
                        }
                    }.toString()
                    // ⚡ 行用详细格式（含 in/out，缓存命中与 think 细分存在时附带）。
                    val detail = buildString {
                        elapsed?.let { append("%.1fs".format(it)) }
                        append(" · ↑${fmtTok(inTok)} ↓${fmtTok(outTok)}")
                        if (cachedTok > 0) append(" · c${fmtTok(cachedTok)}")
                        if (thinkTok > 0) append(" · think ${fmtTok(thinkTok)}")
                    }
                    mutate {
                        val entries2 = entries.toMutableList()
                        // 回填最近的 Reasoning.meta。
                        for (i in entries2.indices.reversed()) {
                            val e = entries2[i]
                            if (e is UiEntry.Reasoning && e.meta == null) {
                                entries2[i] = e.copy(meta = meta)
                                break
                            }
                        }
                        entries2.add(UiEntry.Meta(detail))
                        copy(entries = entries2, awaitingFirstToken = false)
                    }
                }
            }
            "request_start" -> mutate {
                val turnBegan = !running
                copy(
                    running = true,
                    step = obj.optInt("step", step),
                    toolCalls = if (turnBegan) 0 else toolCalls,
                    awaitingFirstToken = true,
                )
            }.also { stepStartTs = System.currentTimeMillis() }
            "turn_ok" -> mutate {
                copy(running = false, awaitingFirstToken = false)
            }.also { persistTranscriptAsync() }
            "turn_error" -> mutate {
                copy(
                    running = false,
                    awaitingFirstToken = false,
                    entries = entries + UiEntry.Meta("⚠ " + obj.optString("message", "error")),
                )
            }.also { persistTranscriptAsync() }
            "notice" -> mutate {
                copy(entries = entries + UiEntry.Meta(obj.optString("text", "")))
            }
            "error" -> mutate { copy(lastError = obj.optString("message", "protocol error")) }
            "approval_requested" -> {
                val summary = obj.optJSONObject("summary")
                mutate {
                    copy(
                        pendingApproval = PendingApprovalUi(
                            id = obj.optLong("id"),
                            toolName = summary?.optString("tool_name") ?: "?",
                            riskLevel = summary?.optString("risk_level") ?: "?",
                            reason = summary?.optString("reason") ?: "",
                        )
                    )
                }
            }
            "approval_resolved" -> mutate { copy(pendingApproval = null) }
            "session_reset" -> mutate { copy(entries = emptyList(), pendingApproval = null) }
            "model_changed" -> mutate { copy(modelAlias = obj.optString("alias")) }
            "config_changed" -> obj.optJSONObject("config")?.let { applyConfig(it) } ?: Unit
            "host_call_requested" -> handleHostCall(obj)
            else -> Unit // step_end / first_token 等暂不投影
        }
    }

    /** 解析 ConfigViewDto → ConfigUi。 */
    private fun parseConfig(c: JSONObject): ConfigUi {
        val providers = buildList {
            val ps = c.optJSONObject("providers") ?: return@buildList
            for (name in ps.keys()) {
                val p = ps.optJSONObject(name) ?: continue
                add(
                    ProviderUi(
                        name = name,
                        baseUrl = p.optString("base_url"),
                        apiKeyEnv = p.optString("api_key_env"),
                        adapter = if (p.has("adapter") && !p.isNull("adapter")) p.optString("adapter") else null,
                        title = if (p.has("title") && !p.isNull("title")) p.optString("title") else null,
                    )
                )
            }
        }
        val models = buildList {
            val ms = c.optJSONObject("models") ?: return@buildList
            for (entry in ms.keys()) {
                val m = ms.optJSONObject(entry) ?: continue
                fun optLong(k: String): Long? =
                    if (m.has(k) && !m.isNull(k)) m.optLong(k) else null
                fun optDouble(k: String): Double? =
                    if (m.has(k) && !m.isNull(k)) m.optDouble(k) else null
                add(
                    ModelUi(
                        entry = entry,
                        provider = m.optString("provider"),
                        modelId = m.optString("model"),
                        maxContextTokens = optLong("max_context_tokens"),
                        maxOutputTokens = optLong("max_output_tokens"),
                        supportsToolCall = m.optBoolean("supports_tool_call", true),
                        supportsVision = m.optBoolean("supports_vision"),
                        supportsReasoning = m.optBoolean("supports_reasoning"),
                        inputPricePerMtok = optDouble("input_price_per_mtok"),
                        outputPricePerMtok = optDouble("output_price_per_mtok"),
                    )
                )
            }
        }
        val levels = buildMap {
            val ls = c.optJSONObject("levels") ?: return@buildMap
            for (k in ls.keys()) put(k, ls.optString(k))
        }
        return ConfigUi(
            providers = providers,
            models = models,
            levels = levels,
            activeConfig = c.optString("active_config"),
        )
    }

    private fun applyConfig(c: JSONObject) {
        mutate { copy(config = parseConfig(c)) }
    }

    private fun applySnapshot(session: JSONObject?) {
        if (session == null) return
        val transcript = session.optJSONArray("transcript") ?: return
        val entries = buildList {
            for (i in 0 until transcript.length()) {
                val e = transcript.optJSONObject(i) ?: continue
                when (e.optString("kind")) {
                    "user" -> add(UiEntry.User(e.optString("text")))
                    "assistant" -> add(UiEntry.Assistant(e.optString("text")))
                    "reasoning" -> add(UiEntry.Reasoning(e.optString("text")))
                    "meta" -> add(UiEntry.Meta(e.optString("text")))
                    "tool_call" -> {
                        val statusObj = e.optJSONObject("status")
                        val detail = e.optJSONObject("detail")
                        add(
                            UiEntry.ToolCall(
                                name = e.optString("name"),
                                argsPreview = e.optString("args"),
                                status = if (statusObj != null) {
                                    when (statusObj.optString("state", statusObj.optString("type", "done"))) {
                                        "running" -> "running"; "failed" -> "failed"; else -> "done"
                                    }
                                } else "done",
                                fullArgs = detail?.optString("args") ?: e.optString("args"),
                                output = detail?.takeIf { it.has("output") && !it.isNull("output") }?.optString("output"),
                            )
                        )
                    }
                    "delegate" -> add(UiEntry.Delegate(e.optString("agent"), e.optBoolean("done")))
                    "approval" -> add(UiEntry.Approval(e.optString("tool_name"), e.optString("decision")))
                    "step_break" -> add(UiEntry.StepBreak)
                }
            }
        }
        val pending = session.optJSONObject("pending_approval")
        val config = session.optJSONObject("config")?.let { parseConfig(it) } ?: _state.value.config
        _state.value = RuntimeUiState(
            ready = true,
            running = session.optBoolean("running"),
            modelAlias = session.optString("model_alias"),
            entries = entries,
            sessionId = session.optString("session_id"),
            config = config,
            step = _state.value.step.takeIf { session.optBoolean("running") } ?: 0,
            toolCalls = session.optInt("tool_calls_cur", _state.value.toolCalls),
            pendingApproval = pending?.let {
                PendingApprovalUi(
                    id = it.optLong("id"),
                    toolName = it.optJSONObject("summary")?.optString("tool_name") ?: "?",
                    riskLevel = it.optJSONObject("summary")?.optString("risk_level") ?: "?",
                    reason = it.optJSONObject("summary")?.optString("reason") ?: "",
                )
            },
        )
        // 本地 transcript（含思维链/⚡meta）若存在则整体覆盖镜像版。
        tryRestoreLocalTranscript()
    }

    /** 增量并入最后一条同类条目（或新起一条）。 */
    private fun appendDelta(text: String, assistant: Boolean) {
        if (text.isEmpty()) return
        mutate {
            val entries = entries.toMutableList()
            val last = entries.lastOrNull()
            if (assistant && last is UiEntry.Assistant) {
                entries[entries.size - 1] = last.copy(text = last.text + text)
            } else if (!assistant && last is UiEntry.Reasoning) {
                entries[entries.size - 1] = last.copy(text = last.text + text)
            } else {
                entries.add(
                    if (assistant) {
                        UiEntry.Assistant(text)
                    } else {
                        // TTFT 等待期计入思考时长：awaiting 期间新起思维链，
                        // 计时起点回拨到 request_start（而非首个思考 token）。
                        UiEntry.Reasoning(
                            text,
                            startTs = if (awaitingFirstToken) {
                                stepStartTs ?: System.currentTimeMillis()
                            } else {
                                System.currentTimeMillis()
                            },
                        )
                    }
                )
            }
            // 首个 token 已到：思考中 chip 的使命结束。
            copy(entries = entries, awaitingFirstToken = false)
        }
    }

    /**
     * host call 处理：mobile.ping 直答；termux.run 经 RUN_COMMAND intent 在
     * Termux 执行、共享存储文件回传输出（阶段五预演）；其余显式 unsupported。
     */
    private fun handleHostCall(obj: JSONObject) {
        val id = obj.optLong("id")
        val tool = obj.optString("tool")
        val runtime = runtimeRef.get() ?: return
        when (tool) {
            "mobile.ping" -> {
                recordToolOutput(tool, "{\"pong\":true,\"runtime\":\"rust\",\"host\":\"android\"}")
                runtime.resolveHostCall(
                    id.toULong(), true,
                    """{"pong":true,"runtime":"rust","host":"android"}""",
                )
            }
            // termux 后端工具名随设置可为 termux.run（旧）或 termux_bash（现）。
            "termux.run", "termux_bash" ->
                runInTermux(runtime, id, obj.optJSONObject("args")?.optString("command") ?: "")
            else -> runtime.resolveHostCall(
                id.toULong(), false,
                "host tool '$tool' not implemented in Phase 1",
            )
        }
    }

    /**
     * termux.run：TermuxExec（RUN_COMMAND + 结果 PendingIntent 本机直传）。
     * 无 PC、无 adb、无局域网中继；失败原因随错误返回供模型自纠。
     */
    private fun runInTermux(runtime: OpenSlateRuntime, id: Long, command: String) {
        val ctx = appContext
        if (ctx == null) {
            runtime.resolveHostCall(id.toULong(), false, "termux.run: no app context")
            return
        }
        scope.launch(Dispatchers.IO) {
            val (output, error) = TermuxExec.run(ctx, id, command)
            if (output != null) {
                recordToolOutput("termux_bash", output)
                runtime.resolveHostCall(
                    id.toULong(), true,
                    JSONObject().put("output", output).put("command", command).toString(),
                )
            } else {
                recordToolOutput("termux_bash", error)
                runtime.resolveHostCall(id.toULong(), false, error)
            }
        }
    }

    /** 展开/收起工具卡片（点击交互）。 */
    fun toggleToolExpanded(target: UiEntry.ToolCall) {
        Log.i(TAG, "toggleToolExpanded: ${target.name} args=${target.argsPreview.take(30)}")
        // 优先按实例身份匹配：transcript 中可能存在多条同名同参的调用，
        // 字段匹配会命中第一条导致"点了没反应"。身份失配（如恢复后引用失效）
        // 才退回字段匹配。
        var hit = -1
        var newExpanded = false
        mutate {
            var idx = entries.indexOfFirst { it === target }
            if (idx < 0) {
                idx = entries.indexOfFirst {
                    it is UiEntry.ToolCall && it.name == target.name &&
                        it.argsPreview == target.argsPreview && it.status == target.status
                }
            }
            if (idx >= 0) {
                val e = entries[idx] as UiEntry.ToolCall
                hit = idx
                newExpanded = !e.expanded
                copy(entries = entries.toMutableList().also { it[idx] = e.copy(expanded = !e.expanded) })
            } else this
        }
        Log.i(TAG, "toggleToolExpanded: idx=$hit expanded=$newExpanded")
    }

    // ── 历史会话 ─────────────────────────────────────────────

    /** 拉取历史会话列表（分页：每页 50 条，offset 递增）。 */
    fun listSessions(offset: Int = 0): List<SessionSummaryUi> {
        val runtime = runtimeRef.get() ?: return emptyList()
        return runCatching {
            val arr = org.json.JSONArray(runtime.listSessions(offset.toUInt()))
            buildList {
                for (i in 0 until arr.length()) {
                    val o = arr.optJSONObject(i) ?: continue
                    add(
                        SessionSummaryUi(
                            id = o.optString("id"),
                            title = o.optString("title"),
                            status = o.optString("status"),
                            startedMs = o.optLong("started_ms"),
                            costUsd = o.optDouble("cost_usd", 0.0),
                        )
                    )
                }
            }
        }.getOrDefault(emptyList())
    }

    fun openSession(id: String): Boolean {
        val runtime = runtimeRef.get() ?: return false
        return runCatching { runtime.openSession(id) }.onFailure {
            Log.e(TAG, "openSession failed", it)
        }.isSuccess
    }

    /** 当前活动会话 id（UI 用于禁删活动会话）。 */
    fun currentRunId(): String? {
        val runtime = runtimeRef.get() ?: return null
        return runCatching { runtime.currentRunId() }.getOrNull()
    }

    /** 删除历史会话：Rust 侧 DB 级联删除 + 本地 transcript 文件同步清理。
     *  当前活动会话由 Rust 侧拒绝（返回 false）；两侧都成功才算成功。 */
    fun deleteSession(id: String): Boolean {
        val runtime = runtimeRef.get() ?: return false
        val ok = runCatching { runtime.deleteSession(id) }.onFailure {
            Log.e(TAG, "deleteSession failed", it)
        }.getOrDefault(false)
        if (ok) deleteTranscript(id)
        return ok
    }

    private inline fun mutate(block: RuntimeUiState.() -> RuntimeUiState) {
        _state.value = block(_state.value)
    }

    // ── 本地 transcript 持久化（含思维链/元数据，Rust 侧不持久化这些）──

    private fun transcriptFile(runId: String): java.io.File? =
        appContext?.let { java.io.File(java.io.File(it.filesDir, "transcripts"), "$runId.json") }

    /** turn 结束后落盘当前聊天流（后台线程；tmp+rename 原子替换，
     *  进程中途被杀不留截断 JSON）。 */
    private fun persistTranscriptAsync() {
        val runtime = runtimeRef.get() ?: return
        val runId = runCatching { runtime.currentRunId() }.getOrNull() ?: return
        val snapshotEntries = _state.value.entries
        scope.launch(Dispatchers.IO) {
            runCatching {
                val f = transcriptFile(runId) ?: return@launch
                f.parentFile?.mkdirs()
                val arr = org.json.JSONArray()
                for (e in snapshotEntries) arr.put(entryToJson(e))
                // 顶层短键风格（对齐条目级 t/x/f…）：n = 条目数（恢复仲裁依据），
                // e = 条目数组；旧版为裸数组，恢复侧兼容两种格式。
                val payload = org.json.JSONObject()
                    .put("n", snapshotEntries.size)
                    .put("e", arr)
                atomicWrite(f, payload.toString())
                Log.d(TAG, "transcript persisted: $runId (${snapshotEntries.size} entries)")
            }.onFailure { Log.w(TAG, "persist transcript failed", it) }
        }
    }

    /** 同目录 tmp → rename 原子替换；rename 失败（跨文件系统等）回退
     *  copyTo+delete，仍失败抛错交由调用方 warn。 */
    private fun atomicWrite(target: java.io.File, content: String) {
        val tmp = java.io.File(target.parentFile, target.name + ".tmp")
        tmp.writeText(content)
        if (tmp.renameTo(target)) return
        Log.w(TAG, "atomicWrite: renameTo failed, fallback copy+delete: ${target.name}")
        try {
            tmp.copyTo(target, overwrite = true)
            if (!tmp.delete()) Log.w(TAG, "atomicWrite: tmp cleanup failed: ${tmp.name}")
        } catch (t: Throwable) {
            runCatching { tmp.delete() }
            throw t
        }
    }

    /**
     * snapshot 到达后与本地 transcript 仲裁：仅当本地条目数 **大于** 当前内存
     * 条目数（applySnapshot 刚写入的 DB 快照）时才用本地版本整体覆盖——本地
     * 含思维链/⚡meta，通常更丰富；本地更旧（DB 已推进更多对话）则忽略之，
     * 避免旧文件遮蔽新数据。本函数只被 applySnapshot 末尾调用，比较时两者
     * 均已可见，无先到后到问题。
     */
    private fun tryRestoreLocalTranscript() {
        val runtime = runtimeRef.get() ?: return
        val runId = runCatching { runtime.currentRunId() }.getOrNull() ?: return
        val f = transcriptFile(runId) ?: return
        if (!f.exists()) return
        runCatching {
            val trimmed = f.readText().trim()
            // 兼容旧版裸数组格式（无顶层 n）。
            val (localCount, entriesArr) = if (trimmed.startsWith("[")) {
                val arr = org.json.JSONArray(trimmed)
                arr.length() to arr
            } else {
                val o = org.json.JSONObject(trimmed)
                val arr = o.optJSONArray("e") ?: return
                o.optInt("n", arr.length()) to arr
            }
            val memCount = _state.value.entries.size
            if (localCount <= memCount) {
                Log.d(
                    TAG,
                    "transcript restore skipped: local=$localCount <= snapshot=$memCount" +
                        "（DB 快照更新或相同，忽略本地文件）",
                )
                return
            }
            val restored = buildList {
                for (i in 0 until entriesArr.length()) add(entryFromJson(entriesArr.optJSONObject(i) ?: continue))
            }
            _state.value = _state.value.copy(entries = restored)
            Log.i(TAG, "transcript restored: $runId (${restored.size} entries, snapshot had $memCount)")
        }.onFailure { Log.w(TAG, "restore transcript failed", it) }
    }

    /** 删除指定 run 的本地 transcript（filesDir/transcripts/<runId>.json；
     *  不存在则忽略）。历史会话删除流程的清理入口——调用接入由后续任务完成。 */
    fun deleteTranscript(runId: String) {
        val f = transcriptFile(runId) ?: return
        runCatching {
            if (f.exists() && !f.delete()) Log.w(TAG, "deleteTranscript: delete failed for $runId")
        }.onFailure { Log.w(TAG, "deleteTranscript failed for $runId", it) }
    }

    private fun entryToJson(e: UiEntry): org.json.JSONObject {
        val type = when (e) {
            is UiEntry.User -> "user"
            is UiEntry.Assistant -> "assistant"
            is UiEntry.Reasoning -> "reasoning"
            is UiEntry.Meta -> "meta"
            is UiEntry.ToolCall -> "tool"
            is UiEntry.Delegate -> "delegate"
            is UiEntry.Approval -> "approval"
            UiEntry.StepBreak -> "step"
        }
        val o = org.json.JSONObject().put("t", type)
        when (e) {
            is UiEntry.User -> o.put("x", e.text)
            is UiEntry.Assistant -> o.put("x", e.text)
            is UiEntry.Reasoning -> {
                o.put("x", e.text)
                e.meta?.let { o.put("m", it) }
            }
            is UiEntry.Meta -> o.put("x", e.text)
            is UiEntry.ToolCall -> o.put("n", e.name).put("a", e.argsPreview)
                .put("s", e.status).put("f", e.fullArgs).put("o", e.output ?: "")
            is UiEntry.Delegate -> o.put("n", e.agent).put("d", e.done)
            is UiEntry.Approval -> o.put("n", e.toolName).put("d", e.decision)
            UiEntry.StepBreak -> {}
        }
        return o
    }

    private fun entryFromJson(o: org.json.JSONObject): UiEntry = when (o.optString("t")) {
        "user" -> UiEntry.User(o.optString("x"))
        "assistant" -> UiEntry.Assistant(o.optString("x"))
        "reasoning" -> UiEntry.Reasoning(o.optString("x"), o.optString("m", "").ifBlank { null })
        "meta" -> UiEntry.Meta(o.optString("x"))
        "tool" -> UiEntry.ToolCall(
            name = o.optString("n"),
            argsPreview = o.optString("a"),
            status = o.optString("s", "done"),
            fullArgs = o.optString("f"),
            output = o.optString("o").ifBlank { null },
        )
        "delegate" -> UiEntry.Delegate(o.optString("n"), o.optBoolean("d"))
        "approval" -> UiEntry.Approval(o.optString("n"), o.optString("d"))
        else -> UiEntry.StepBreak
    }
}
