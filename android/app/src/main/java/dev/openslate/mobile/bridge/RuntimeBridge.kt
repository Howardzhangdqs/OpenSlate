package dev.openslate.mobile.bridge

import android.content.Context
import android.util.Log
import dev.openslate.mobile.BuildConfig
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONArray
import org.json.JSONObject
import uniffi.openslate.mobile.EventCallback
import uniffi.openslate.mobile.MobilePathsDto
import uniffi.openslate.mobile.OpenSlateRuntime
import java.util.concurrent.atomic.AtomicReference

/**
 * 附件引用（协议 `AttachmentDto` 的 UI 投影）：Submit JSON / transcript
 * `EntryDto::User` 条目共用；id 指向 Rust 侧 attachments 行（content-
 * addressed 字节在 files/，引用只有几百字节——设计 §6.1）。
 */
data class AttachmentRefUi(
    val id: String,
    val mime: String,
    val filename: String? = null,
    /**
     * 图片 token 数（transcript `tokens` 可选键，持久化在 entries 里重启仍在；
     * 只有该回合恰好带 1 张图片且模型响应 usage 顺带含图片 token（OpenAI 系）
     * 时才有值——智谱下恒 null，旧快照缺键亦 null）。只读卡右下角徽标用，
     * 草稿侧不用（草稿不产生该数据）。
     */
    val tokens: Long? = null,
) {
    /** chip 图标判定：归一化 mime 以 video/ 开头 = 视频（contract L4）。
     *  值域 image/jpeg|png 与 video/mp4|x-matroska|quicktime。 */
    val isVideo: Boolean get() = mime.startsWith("video/")
}

/**
 * UI 条目（transcript 的精简投影；完整镜像在 Rust 侧，UI 只保留渲染所需）。
 */
sealed class UiEntry {
    /** attachments = 随行附件引用（v4 多模态；无附件恒空表）。 */
    data class User(
        val text: String,
        val attachments: List<AttachmentRefUi> = emptyList(),
    ) : UiEntry()
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
        /** 工具调用 id（live 事件与 DB 重建条目携带；旧镜像/旧事件为
         *  null）。详情弹窗凭它懒加载 history 里的全量输出。 */
        val callId: String? = null,
    ) : UiEntry()
    data class Delegate(val agent: String, var done: Boolean) : UiEntry()
    data class Approval(val toolName: String, val decision: String) : UiEntry()
    object StepBreak : UiEntry()
}

/**
 * 工具调用审批请求（approval_requested 事件 / snapshot pending_approval 的投影）。
 * summary 可选字段（server/trigger/note）缺键即 null，与 Rust Option 对齐。
 */
data class PendingApprovalUi(
    val id: Long,
    val toolName: String,
    /** 参数预览（Rust 侧已单行化并截断 120 字符的 JSON）。 */
    val arguments: String,
    /** "low" | "medium" | "high"；未知值由 UI 兜底显示原始串。 */
    val riskLevel: String,
    /** MCP 下游 server slug（host 内置工具 = host 前缀，如 "termux"）；
     *  null = 非 MCP 工具（不显示 MCP 组操作）。 */
    val server: String? = null,
    /** 本次询问触发来源："manual"（等级=全部手动）| "auto_risk"（AI 审查判定
     *  高风险）；null = 协议缺省，按 manual 处理。 */
    val trigger: String? = null,
    /** AI 审查理由（trigger=auto_risk 时由 Rust 携带；manual 触发无）。 */
    val note: String? = null,
)

/**
 * 审批规则（ConfigViewDto.approval 的投影，映射后的生效规则）。
 * level 值域："manual" | "allow" | "auto"（全部手动 / 全部允许 / 自动判断）。
 */
data class ApprovalRulesUi(
    val defaultLevel: String = "auto",
    /** 工具注册全名 → 等级（显式覆盖；未含键 = 跟随 server/default）。 */
    val toolLevels: Map<String, String> = emptyMap(),
    /** MCP server slug → 等级（组级覆盖；未含键 = 跟随默认）。 */
    val serverLevels: Map<String, String> = emptyMap(),
)

/** 审批等级 → 中文文案（未知值兜底显示原始串）。 */
internal fun approvalLevelLabel(level: String): String = when (level) {
    "auto" -> "自动判断"
    "allow" -> "全部允许"
    "manual" -> "全部手动"
    else -> level
}

/**
 * 转写审批条目（UiEntry.Approval）decision → 中文文案；未知值兜底显示
 * 原始串（协议新值 / 旧客户端残留都可见，不吞不猜）。
 */
internal fun approvalDecisionLabel(decision: String): String = when (decision) {
    "approved" -> "已允许"
    "denied" -> "已拒绝"
    "approve-all" -> "本会话允许"
    "always-tool" -> "已设为一直允许"
    "session-mcp" -> "本会话允许（MCP）"
    "always-mcp" -> "已设为一直允许（MCP）"
    else -> decision
}

/**
 * summary 可选字符串字段：缺键 / JSON null / 空串均归 null（org.json 的
 * optString 对缺失返回空串，无法区分「有但空」，此处统一对齐 Rust Option）。
 */
internal fun JSONObject.optStringOrNull(key: String): String? =
    if (has(key) && !isNull(key)) optString(key).takeIf { it.isNotEmpty() } else null

// ── ask_user（Agent 向用户提问）的 UI 投影与草稿 ─────────────────────

/** ask_user 单题选项。 */
data class QuestionOptionUi(
    val label: String,
    val description: String = "",
    val recommended: Boolean = false,
)

/** ask_user 单题（payload 解析投影；type 未知值由 UI 按 text 兜底渲染）。 */
data class QuestionUi(
    val id: String,
    val header: String,
    val question: String,
    val type: String, // choice | text | confirm | secret
    val options: List<QuestionOptionUi>,
    val multiSelect: Boolean = false,
    val placeholder: String? = null,
    val optional: Boolean = false,
    /** 仅 secret：落地形态（env / file）。 */
    val storageKind: String? = null,
    /** 仅 secret：env 变量名 / file 路径。 */
    val storageName: String? = null,
)

/**
 * 每题作答草稿。挂 UiState 层（以 requestId 为键随 [PendingQuestionUi]
 * 携带）：切题、进出全屏、卡片重组、列表滚动回收都不丢；**未提交草稿
 * 不持久化**（重启后空草稿重答，r4/r5 语义）。
 */
data class QuestionDraft(
    /** choice：已选模型选项 label（单/多选共用）。 */
    val selected: List<String> = emptyList(),
    /** choice：内置「其他」兜底行是否勾选。 */
    val otherChecked: Boolean = false,
    /** choice：「其他」说明文字。 */
    val otherText: String = "",
    /** text / secret：输入内容（secret 明文仅存在于内存草稿与 resolve payload）。 */
    val text: String = "",
    /** confirm：null 未选，true/false 已选。 */
    val confirm: Boolean? = null,
    /** secret：记住此凭据。 */
    val remember: Boolean = false,
    /** 逐题 decline（「此题不答」）。 */
    val declined: Boolean = false,
    /** 跳过（仅 optional 题显示入口）。 */
    val skipped: Boolean = false,
)

/**
 * pending 问卷（含草稿与卡内相位）。终态渲染不依赖它——declined /
 * cancelled / superseded / expired / answered 摘要统一由关联
 * [UiEntry.ToolCall] 的 output JSON 承载（转写与摘要同源，重启可恢复）。
 */
data class PendingQuestionUi(
    val requestId: Long,
    val questions: List<QuestionUi>,
    val drafts: List<QuestionDraft>,
    /** 秒；-1 = 永不（S2 超时语义本版仅预留展示位）。 */
    val timeoutSec: Long = -1L,
    /** 关联 ToolCall 条目的 fullArgs（工具 chip 吸收 / 答案回写的定位键）。 */
    val argsKey: String = "",
    /** pending → submitting → answered / declined；answered/declined 随 turn 结束清除。 */
    val phase: String = "pending",
    /** 当前题页码（分段条 / Pager 共享；存 UiState 保证进出全屏不丢）。 */
    val current: Int = 0,
    /** host call 到达时刻（S2 超时文案预留；不驱动任何自动 resolve）。 */
    val createdAtMs: Long = System.currentTimeMillis(),
    /** 重启恢复卡（snapshot `pending_questions` 回放）：提交/decline 不走
     *  resolveHostCall（进程重启后 host call 通道已不存在——id 打到失效
     *  信道），改发协议上行 question_reply（request_id 即原 host call id，
     *  Rust 侧 MobileQuestionHandler 双通路兜底）。live 卡（host call 到达
     *  路径建立）恒 false，行为不变。 */
    val restored: Boolean = false,
)

/**
 * 已答问卷的只读回看投影（摘要卡点击 → 全屏只读还原页）。题目结构来自
 * 关联 ToolCall 条目的 **args JSON**（与 pending 卡同源解析），答案按
 * **output JSON**（提交时的脱敏快照）逐题回填为只读 [QuestionDraft]；
 * secret 只保留引用描述（name/kind/remember），明文永不出现（transcript
 * 本就不含明文）。
 */
data class ReviewedQuestionUi(
    val questions: List<QuestionUi>,
    val drafts: List<QuestionDraft>,
)

// ── ask_user 解析 / 回放 + 转写恢复仲裁（internal 纯函数，便于补测）────

/**
 * ask_user `questions[]` 数组 → 题目列表。host call 信封（args.questions）
 * 与 snapshot `pending_questions[].questions`（归一化题目数组，两处同构）
 * 共用同一解析。null / 无可解析题 → 空表，失败语义由调用方决定
 * （live 路径 resolve 失败；回放路径跳过该行）。
 */
internal fun parseAskUserQuestions(questionsArr: JSONArray?): List<QuestionUi> {
    if (questionsArr == null) return emptyList()
    // kotlinx JsonDto 层解析（JsonDto.kt）：org.json 数组此处仅作载体（toString
    // 后 decode），逐行容错语义与原手工 opt* 一致（非对象行跳过、缺键取默认）。
    return decodeQuestionDtos(questionsArr.toString()).mapIndexed { i, dto -> dto.toQuestionUi(i) }
}

/**
 * snapshot `pending_questions[]` → 恢复卡（重启回放）。取**首个**
 * `status=="pending"` 且可解析的行——多行 = 同 turn 并发提问，UI 单卡槽
 * 展示最早一张，兄弟悬挂由 Rust 侧下一 turn 前的 superseded 修复收口。
 *
 * - `questions` 为归一化题目数组（与 host call 信封 args.questions 同构），
 *   复用 [parseAskUserQuestions]；
 * - `request_id` 必须可解析为数字（= 原 host call id，提交路径以 Long 路由），
 *   非数字行跳过（协议 String 承载，实际恒为数字字符串）；
 * - `argsKey` 按 `tool_call_id` 从快照 tool_call 条目的 call_id 关联其
 *   fullArgs——QuestionCardHost 凭 `argsKey == fullArgs` 出交互卡；查无
 *   （理论不可达：DTO 仅收录当前 history 声明过的调用）置空，卡片退化为
 *   等待行、提交仍可用（无转写回写）；
 * - DTO 不含 timeout（恢复卡按永不；timeoutSec 仅 S2 展示位预留）。
 */
internal fun buildRestoredPendingQuestion(
    pendingArr: JSONArray?,
    argsByCallId: Map<String, String>,
): PendingQuestionUi? {
    if (pendingArr == null) return null
    for (i in 0 until pendingArr.length()) {
        val row = pendingArr.optJSONObject(i) ?: continue
        if (row.optString("status") != "pending") continue
        val requestId = row.optString("request_id").toLongOrNull() ?: continue
        val questions = parseAskUserQuestions(row.optJSONArray("questions"))
        if (questions.isEmpty()) continue
        return PendingQuestionUi(
            requestId = requestId,
            questions = questions,
            drafts = List(questions.size) { QuestionDraft() },
            timeoutSec = -1L,
            argsKey = row.optString("tool_call_id").let { argsByCallId[it] ?: "" },
            restored = true,
        )
    }
    return null
}

/**
 * 已答问卷还原（internal 纯函数）：ToolCall 条目的 args + output →
 * [ReviewedQuestionUi]。args 的 `questions[]` 出题目结构（复用
 * [parseAskUserQuestions]）；output 的 `answers[]` 按 id 对齐逐题回填只读
 * 草稿（映射规则与 [RuntimeBridge.submitQuestionAnswers] 写回的 display
 * JSON 一一对应）：
 *
 * - `skipped` / `declined` → 对应布尔（只读页控件禁用 +「已跳过此题」提示）；
 * - `secret{name,remember}` → 只留 remember 与引用描述，**明文不回填**
 *   （transcript 快照本就脱敏，只读页渲染「已存入凭据」引用行）；
 * - `confirm` 布尔 → draft.confirm（confirm 答案同时带 selected，须先于
 *   通用 selected 分支判定）；
 * - `selected[]`（choice）→ 命中 options[].label 的进 selected，未命中的
 *   （「其他」说明，提交时以自由文本并入 selected）回填 otherChecked/otherText；
 * - `value`（text/未知题型兜底）→ draft.text。
 *
 * null = 无法还原（args 缺失 / 题目解析为空 / 无 answered outcome / answers
 * 为空），调用方退回旧答案弹窗兜底。
 */
internal fun buildReviewedQuestion(args: String?, output: String?): ReviewedQuestionUi? =
    // kotlinx JsonDto 层解析（decodeReviewedQuestion 纯函数，分支判定与映射
    // 规则和本函数 doc 描述一一对应；decode 失败 → null 走调用方兜底）。
    decodeReviewedQuestion(args, output)

/**
 * 转写恢复仲裁：本地 transcript 文件（只在上个**完成** turn 落盘）何时允许
 * 覆盖 DB 重建的镜像。durable = 两侧同源持久化的条目（user / assistant /
 * reasoning / tool / delegate / approval）；装饰条目（⚡meta 行 / step）
 * 仅本地有。旧「总数多者胜」会让旧文件凭多余装饰条目压过**含中断 turn
 * 条目**的 DB 权威镜像（悬挂的 user / ask_user 调用被抹掉）。新规则：
 * 本地 durable 不落后 **且** 总数更多（装饰更丰富）才覆盖；DB 转写为空时
 * 本地兜底（0 ≥ 0 恒过）。正常完成场景的丰富装饰恢复保持不变。
 */
internal fun shouldRestoreLocalTranscript(
    localTotal: Int,
    localDurable: Int,
    dbTotal: Int,
    dbDurable: Int,
): Boolean = localDurable >= dbDurable && localTotal > dbTotal

/** 镜像条目的 durable 计数（排除 Meta / StepBreak 装饰条目）。 */
internal fun durableUiEntryCount(entries: List<UiEntry>): Int =
    entries.count { it !is UiEntry.Meta && it !is UiEntry.StepBreak }

/** 本地文件条目（"t" 短键格式）的 durable 计数（排除 meta / step）。 */
internal fun localTranscriptDurableCount(arr: JSONArray): Int {
    var n = 0
    for (i in 0 until arr.length()) {
        val t = arr.optJSONObject(i)?.optString("t") ?: continue
        if (t != "meta" && t != "step") n++
    }
    return n
}

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
    /** thinking effort 档位（minimal/low/medium/high/xhigh/max）；
     *  null = Auto（不传参，跟随 provider 默认）——wire 键 reasoning_effort
     *  在 Rust 侧 null 时 skip 序列化（serde skip_serializing_if）。 */
    val reasoningEffort: String? = null,
)

data class ConfigUi(
    val providers: List<ProviderUi> = emptyList(),
    val models: List<ModelUi> = emptyList(),
    val levels: Map<String, String> = emptyMap(),
    /** 功能 → 代号（server 侧已并入缺省回退的生效值）。 */
    val capabilities: Map<String, String> = emptyMap(),
    /** 审批规则视图（config 视图 `approval` 键；旧 runtime 无此键 → null）。 */
    val approval: ApprovalRulesUi? = null,
    val activeConfig: String = "",
)

/** 工具参数（input_schema.properties 条目的投影）。 */
data class ToolParamUi(
    val name: String,
    val type: String,
    val required: Boolean,
    val description: String,
    /** 嵌套深度：0 = 顶层参数；>0 = 嵌套 schema 展开的子字段（UI 缩进展示）。 */
    val depth: Int = 0,
)

/** 工具清单条目（listTools FFI JSON 数组的元素投影）。 */
data class ToolInfoUi(
    val name: String,
    val description: String,
    /** input_schema 提取的参数（schema 缺失/空 → 空表）。 */
    val params: List<ToolParamUi> = emptyList(),
    /** 所属 MCP server slug（host 内置 = host 前缀）；null = 非 MCP 工具。
     *  旧 runtime 的清单无此键 → 调用方回退 [RuntimeBridge.mcpServerSlugOf]
     *  推导（有则用，缺失兜底）。 */
    val server: String? = null,
)

/** 工具可见性快照（getAgentToolVisibility FFI JSON 的投影）。
 *  语义：工具可见 = (tools 为空 || tools 含该名) && hiddenTools 不含该名。
 *  列表可能含 globs（如 "mcp.*"）等非精确条目，回写时须原样保留。 */
data class ToolVisibilityUi(
    val tools: List<String> = emptyList(),
    val hiddenTools: List<String> = emptyList(),
)

data class RuntimeUiState(
    val ready: Boolean = false,
    val running: Boolean = false,
    val modelAlias: String = "",
    val entries: List<UiEntry> = emptyList(),
    val pendingApproval: PendingApprovalUi? = null,
    /** ask_user 待答问卷（含草稿）；null = 无待答卡。 */
    val pendingQuestion: PendingQuestionUi? = null,
    /** ask_user 已答问卷只读回看（摘要卡 → 全屏还原页）；null = 不在回看。 */
    val questionReview: ReviewedQuestionUi? = null,
    val sessionId: String = "",
    val lastError: String? = null,
    val config: ConfigUi = ConfigUi(),
    /** 进度：当前 step / 本回合工具调用数。 */
    val step: Int = 0,
    val toolCalls: Int = 0,
    /** 请求已发出但尚未收到任何首个 token（reasoning/content/tool）：
     *  期间若模型支持思维链则显示"思考中"，TTFT 等待计入思考时长。 */
    val awaitingFirstToken: Boolean = false,
    /** termux 命令发出 0.5s 仍无应答的最早时刻（null = 无 stall）。非 null
     *  时聊天页显示"Termux 未响应"横幅，UI 可据此换算已等待秒数。 */
    val termuxStallSince: Long? = null,
    /** MCP host 恢复中：本轮恢复起始时刻（null = 无恢复）。非 null 时
     *  聊天页显示「MCP host 恢复」面板（Termux 运行 → host 进程 → 工具
     *  注册阶梯，见 RuntimeBridge.handleMcpRecoverHost；与 termuxStallSince
     *  独立字段互不打架——前者覆盖 HostTool 兜底路径的 RUN_COMMAND 停滞，
     *  本字段覆盖 MCP 路径的 host 死亡自愈，极端并存时两面板堆叠展示）。 */
    val mcpRecovering: Long? = null,
    /** run 标题异步生成完成的版本号（run_title 事件递增）。历史会话页
     *  观察它做增量刷新——LLM 标题通常在首条消息后几秒到达，此时
     *  列表里的占位标题需要原位替换。 */
    val titlesVersion: Int = 0,
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
    /** 聚合 token（v5 runs 列；存量 run = 0，UI 侧 0 时省略不显示）。 */
    val inputTokens: Long = 0,
    val outputTokens: Long = 0,
)

/** 模型元数据（registry 查询投影；字段可空 = 数据源未提供）。 */
data class ModelMetaUi(
    val displayName: String? = null,
    val contextTokens: Long? = null,
    val maxOutputTokens: Long? = null,
    val supportsVision: Boolean? = null,
    val supportsReasoning: Boolean? = null,
    val supportsToolCall: Boolean? = null,
    val inputPricePerMtok: Double? = null,
    val outputPricePerMtok: Double? = null,
    val source: String? = null,
) {
    val hasAny: Boolean
        get() = contextTokens != null || maxOutputTokens != null || supportsVision != null ||
            supportsReasoning != null || supportsToolCall != null ||
            inputPricePerMtok != null || outputPricePerMtok != null
}

/** 数据源本地状态（数据源页面渲染）。 */
data class RegistrySourceUi(
    val id: String,
    val name: String,
    val desc: String,
    val url: String,
    val fetchedAtMs: Long?,
    val entries: Int?,
    val zstBytes: Long?,
    val staleAfterHrs: Long,
)

/** registry 条目（数据源详情浏览 / 跨源搜索结果；null = 源未提供该字段）。 */
data class RegistryEntryUi(
    val source: String, // 展示名（如 models.dev）
    val sourceId: String, // 稳定 id
    val id: String, // 源内原始键（保留 provider 前缀，如 zai/glm-5.3）
    val ctx: Long?,
    val out: Long?,
    val vision: Boolean?,
    val reasoning: Boolean?,
    val tool: Boolean?,
    val priceIn: Double?,
    val priceOut: Double?,
)

/** registry 搜索结果：total = 匹配总数，results 截断到 limit。 */
data class RegistrySearchUi(val total: Int, val results: List<RegistryEntryUi>)

/** MCP 恢复阶梯当前步（恢复面板行渲染；驱动方 = RuntimeBridge 恢复协程）。 */
internal enum class McpRecoverStep { PROBE, TERMUX, HOST, RELOAD }

/** MCP 恢复快照：step = 当前步；detail = 该步说明（面板副文本）。 */
internal data class McpRecoverPhase(
    val step: McpRecoverStep = McpRecoverStep.PROBE,
    val detail: String? = null,
)

object RuntimeBridge {

    private const val TAG = "OpenSlateBridge"

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)
    private val eventChannel = Channel<String>(Channel.UNLIMITED)

    /** 当前事件消费协程（单例泵：start 前取消旧泵，避免 shutdown→start 后双消费者竞态）。 */
    private var pumpJob: Job? = null

    /**
     * 最近一次 snapshot 携带的 run_id（C4：转写镜像键修复）。**只在
     * applySnapshot（泵协程）里写**，读方（turn_error 落盘兜底）同在泵
     * 协程——单协程 FIFO 保证读到的事件序与写入序一致，无需同步原语。
     * 替代旧实现里 runtime.currentRunId() 的 FFI 旁路直读（open_session
     * 在 Dispatchers.IO 与事件泵并发，直读实时态会产生 run 键错位）。
     */
    private var lastKnownRunId: String? = null

    private val _state = MutableStateFlow(RuntimeUiState())
    val state: StateFlow<RuntimeUiState> = _state.asStateFlow()

    /**
     * 附件提交终态信号（seq 单调递增；多模态输入契约）：带附件的 Submit
     * 是异步门控（Rust 侧拒经 notice 通道回来），composer 据此决定清空
     * 还是保留草稿与附件 chips——
     * - accepted=true：引擎已接受（request_start）→ composer 清空；
     * - accepted=false：门控拒绝（Error）/ 回合进行中（Warn）/ 会话切换
     *   等意外终结 → composer **保留草稿**，错误文案经 [lastNotice] snackbar。
     */
    data class SubmitResolutionUi(val seq: Long, val accepted: Boolean)

    private val _submitResolution = MutableStateFlow(SubmitResolutionUi(0, false))
    val submitResolution: StateFlow<SubmitResolutionUi> = _submitResolution.asStateFlow()
    private var resolutionSeq = 0L

    /** notice 事件投影（含语义级别；level 值域 info|warn|error，serde
     *  snake_case）。ChatScreen 据此把 error/warn 级提为 snackbar；纯
     *  info 仍只进转写 Meta 行。 */
    data class BridgeNoticeUi(val seq: Long, val level: String, val text: String)

    private val _lastNotice = MutableStateFlow<BridgeNoticeUi?>(null)
    val lastNotice: StateFlow<BridgeNoticeUi?> = _lastNotice.asStateFlow()
    private var noticeSeq = 0L

    /** 等待引擎接受的附件提交（request_start 后回显；拒绝/会话切换时
     *  丢弃）。submit 在 UI 线程写、事件泵协程读写 → 原子引用。 */
    private class PendingEchoSubmit(val text: String, val refs: List<AttachmentRefUi>)

    private val pendingEcho = AtomicReference<PendingEchoSubmit?>(null)

    private fun emitSubmitResolution(accepted: Boolean) {
        resolutionSeq += 1
        _submitResolution.value = SubmitResolutionUi(resolutionSeq, accepted)
    }

    /** 丢弃在途附件提交（accepted=false 结束 composer 等待）。 */
    private fun dropPendingEcho() {
        if (pendingEcho.getAndSet(null) != null) emitSubmitResolution(accepted = false)
    }

    private val runtimeRef = AtomicReference<OpenSlateRuntime?>(null)
    private val startMutex = Mutex()
    private var started = false
    private var appContext: Context? = null

    /** 当前步起始时间戳（usage 事件计算步耗时用）。 */
    private var stepStartTs: Long? = null

    /** 工具名 → 待消费输出队列（同名工具并发时按序配对）。 */
    private val pendingToolOutputs =
        java.util.concurrent.ConcurrentHashMap<String, java.util.concurrent.ConcurrentLinkedDeque<String>>()

    /** 0.5s 仍无应答的 termux host call id 集合（并发调用共用；空 = 熄灭面板）。
     *  点亮/熄灭与状态更新都经 termuxStallMutex 串行，避免竞态下横幅残留。 */
    private val termuxStalledCalls: MutableSet<Long> =
        java.util.concurrent.ConcurrentHashMap.newKeySet()
    private val termuxStallMutex = Mutex()

    /** stall 看门狗阈值：命令发出多久仍无应答判定停滞（进入等待连接模式）。 */
    private const val TERMUX_STALL_WATCHDOG_MS = 500L

    /** 连接事件确认 Ready 后、重发前的宽限：停止态期间排队的原始命令可能
     *  正在补跑，先等结果落地再只重发仍无应答的（防命令重复执行）。 */
    private const val TERMUX_RESEND_GRACE_MS = 2_000L

    /** reloadMcpServers 串行化：设置页保存、诊断自愈、工具页刷新等多个
     *  调用点可能并发触发热重载；Rust 侧 reload（让位→swap→重算）非
     *  原子，交错会让 HostTool 交接序列短暂纠缠（下次 reload 可自愈，
     *  此处串行化消除放大）。 */
    private val reloadMcpMutex = Mutex()

    /** MCP host 恢复单飞（Kotlin 侧，与 Rust 侧单飞闩 + 冷却互补）：
     *  mcp.recover_host 请求串行化——排队期间已有一轮完成的等待者只做
     *  轻量复检后直接 resolve（不重走拉起阶梯）。round 由本闩守卫。 */
    private val mcpRecoverMutex = Mutex()
    private var mcpRecoverRound = 0L

    /** 恢复面板阶梯快照（ChatScreen 订阅；恢复协程驱动，非探测驱动）。 */
    private val _mcpRecoverPhase = MutableStateFlow(McpRecoverPhase())
    internal val mcpRecoverPhase: StateFlow<McpRecoverPhase> = _mcpRecoverPhase.asStateFlow()

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
                // 清理历史 bug 遗留的僵尸镜像（org.json null 陷阱曾把部分
                // turn_error 落盘键污染成 "null" → transcripts/null.json；
                // 修复后永不再读写，一次性删除）。
                runCatching {
                    java.io.File(java.io.File(appContext.filesDir, "transcripts"), "null.json")
                        .takeIf { it.exists() }?.let {
                            if (it.delete()) Log.i(TAG, "cleaned zombie transcript mirror null.json")
                        }
                }
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
                    // ask_user 行为配置：本地偏好推一次给引擎做同步
                    //（openslate.toml [ask_user] 为生效真值）。
                    syncAskUserConfig()
                    // 模型元数据注册表：启动时按需自动更新（后台执行；
                    // 本地缺失或超过各源 stale_after 才真正拉取）。
                    runtime.registryScheduleAutoUpdate()
                    // 模型选择不再本地重放：[capabilities].main 是唯一真相源
                    // （openslate.toml），Rust 侧 open_session 初始别名已取它。
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

    /**
     * 提交用户消息（可带附件引用）。
     *
     * - 无附件：保持既有乐观语义——sendJson 成功即本地回显 User 条目
     *   （Rust 侧推 transcript 但不广播 User 事件，与桌面 TUI 同语义）。
     * - 带附件：**不立即回显**。Submit 是异步门控（模态能力/数量/体积/
     *   图视频互斥在 Rust 侧，拒绝经 Error 级 notice 返回），立即回显会
     *   在拒绝后留下幽灵气泡。改为挂 [pendingEcho]，引擎接受
     *   （request_start）后回显并广播 [submitResolution](accepted=true)；
     *   拒绝/会话切换广播 accepted=false——composer 草稿与附件 chips 保
     *   留，可修正后重发。
     *
     * 返回 false = 未送达（runtime 未就绪 / FFI 异常），调用方保留草稿。
     */
    fun submit(text: String, attachments: List<AttachmentRefUi> = emptyList()): Boolean {
        val trimmed = text.trim()
        if (trimmed.isEmpty()) return false
        val json = JSONObject().put("type", "submit").put("text", trimmed)
        if (attachments.isNotEmpty()) {
            // 契约（§7）：attachments = [{id, mime, filename?}]，mime/filename
            // 用 import 返回值原样回填；filename null 跳键（serde Option）。
            json.put(
                "attachments",
                JSONArray().apply {
                    for (a in attachments) {
                        put(
                            JSONObject()
                                .put("id", a.id)
                                .put("mime", a.mime)
                                .apply { a.filename?.takeIf { it.isNotEmpty() }?.let { put("filename", it) } },
                        )
                    }
                },
            )
        }
        if (!sendJson(json.toString())) return false
        if (attachments.isEmpty()) {
            mutate { copy(entries = entries + UiEntry.User(trimmed)) }
        } else {
            pendingEcho.set(PendingEchoSubmit(trimmed, attachments))
        }
        return true
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

    // ── bash 工具执行后端（native 开关 + termux 恒在）──────────────

    private const val PREF_EXEC_BACKEND = "exec_backend"

    /** 首装默认（无存值时）：native 开 + termux 恒在。 */
    private const val DEFAULT_EXEC_BACKENDS = "native,termux"

    /**
     * 存量集合归一化（并行契约，Rust 侧同语义）：**termux 恒在**——旧
     * {native} 补入 termux、空/未知集合归为 {termux}；native 开关状态
     * 只看结果是否含 "native"。未知 token 丢弃（历史写入只可能产生
     * native/termux 子集），{} / {native} 单独形态经此一律折到新契约。
     */
    private fun normalizeExecBackends(backends: String?): String {
        val tokens = backends?.split(',')?.map { it.trim() }.orEmpty()
        val set = linkedSetOf<String>()
        if ("native" in tokens) set.add("native")
        set.add("termux")
        return set.joinToString(",")
    }

    /** 当前 bash 后端集合（CSV，恒含 "termux"；含 "native" 与否 = 设置页
     *  「启用 native bash」开关）。无存值（首装）→ 两者齐开。 */
    fun execBackends(): String {
        val stored = appContext
            ?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.getString(PREF_EXEC_BACKEND, null)
            ?: return DEFAULT_EXEC_BACKENDS
        return normalizeExecBackends(stored)
    }

    /** 写入后端集合：FFI 热切换 + 本地持久化（重启恢复）。写入同样过
     *  归一化——termux 恒在不变量在桥层收口，UI 侧写法再偏差也不破坏。
     *  命名规则（Rust 侧）：含 native → `bash` 注册；termux_bash 恒注册。 */
    fun setExecBackendsAndPersist(backends: String) {
        val normalized = normalizeExecBackends(backends)
        runtimeRef.get()?.setExecBackends(normalized)
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.edit()?.putString(PREF_EXEC_BACKEND, normalized)?.apply()
        Log.i(TAG, "exec backends set to $normalized")
    }

    private fun restoreExecBackend() {
        // execBackends() 已归一化：旧 {native} / {} / {termux} 存量在恢复
        // 入口一次折到新契约形态（termux 恒在），分发方式不变。
        val backends = execBackends()
        runtimeRef.get()?.setExecBackends(backends)
        Log.i(TAG, "start: restored exec backends = $backends")
    }

    // ── Agent 工具可见性（工具管理页：清单 + 对 agent 可见/隐藏开关）──

    /** 全部可用工具清单（FFI JSON 数组 → 已按 name 排序）；
     *  null = 运行时未就绪，空表 = 无工具。 */
    fun listTools(): List<ToolInfoUi>? {
        val json = runtimeRef.get()?.listTools() ?: return null
        val arr = runCatching { JSONArray(json) }.getOrNull() ?: return emptyList()
        return (0 until arr.length()).mapNotNull { i ->
            val obj = arr.optJSONObject(i) ?: return@mapNotNull null
            val name = obj.optString("name")
            if (name.isEmpty()) {
                null
            } else {
                ToolInfoUi(
                    name = name,
                    description = obj.optString("description"),
                    params = parseToolParams(obj),
                    // server 字段（协议新增，有则用）：MCP slug / host 前缀；
                    // 缺失（旧 runtime）→ null，调用方回退 mcpServerSlugOf 推导。
                    server = obj.optStringOrNull("server"),
                )
            }
        }
    }

    /** input_schema → 参数列表：properties 的每个键 = 参数名（值为
     *  {type, description}），required 数组 = 必填名单；schema 缺失或
     *  无 properties → 空表。嵌套结构（object 的 properties / array 的
     *  items.properties）递归展开为缩进子参数——MCP 工具的字段描述大多
     *  藏在嵌套层（如 server-memory 的 entities.items.properties），
     *  深度上限 3 层防病态 schema。 */
    private fun parseToolParams(obj: JSONObject): List<ToolParamUi> {
        val schema = obj.optJSONObject("input_schema") ?: return emptyList()
        return parseSchemaParams(schema, depth = 0)
    }

    private fun parseSchemaParams(schema: JSONObject, depth: Int): List<ToolParamUi> {
        if (depth > 3) return emptyList()
        val props = schema.optJSONObject("properties") ?: return emptyList()
        val required = buildSet {
            val a = schema.optJSONArray("required") ?: return@buildSet
            for (i in 0 until a.length()) add(a.optString(i))
        }
        val out = mutableListOf<ToolParamUi>()
        for (key in props.keys()) {
            val p = props.optJSONObject(key) ?: continue
            out.add(
                ToolParamUi(
                    name = key,
                    type = p.optString("type"),
                    required = key in required,
                    description = p.optString("description"),
                    depth = depth,
                )
            )
            // 嵌套展开：object → 自身 properties；array → items.properties
            //（server-memory 的 entities 即后者）。required 同理取自本级
            // 或 items.required（JSON Schema 的对象内必填语义）。
            val itemObj = p.optJSONObject("items")
            val nestedProps = p.optJSONObject("properties")
                ?: itemObj?.optJSONObject("properties") ?: continue
            val nestedReq = p.optJSONArray("required") ?: itemObj?.optJSONArray("required")
            val sub = JSONObject().put("properties", nestedProps)
            if (nestedReq != null) sub.put("required", nestedReq)
            out.addAll(parseSchemaParams(sub, depth + 1))
        }
        return out
    }

    /** 当前工具可见性快照；null = 运行时未就绪。 */
    fun getAgentToolVisibility(): ToolVisibilityUi? {
        val json = runtimeRef.get()?.getAgentToolVisibility() ?: return null
        val obj = runCatching { JSONObject(json) }.getOrNull() ?: return ToolVisibilityUi()
        fun strings(key: String): List<String> {
            val a = obj.optJSONArray(key) ?: return emptyList()
            return (0 until a.length()).mapNotNull { i ->
                a.optString(i).takeIf { it.isNotEmpty() }
            }
        }
        return ToolVisibilityUi(strings("tools"), strings("hidden_tools"))
    }

    /** 全量覆写工具可见性并热生效（FFI 侧为唯一真相源，无本地 prefs）。
     *  调用方须把快照里无法识别的条目（globs 等）原样带回。
     *  false = 运行时未就绪（未写入）。 */
    fun setAgentToolVisibility(tools: List<String>, hiddenTools: List<String>): Boolean {
        val runtime = runtimeRef.get() ?: return false
        val result = runtime.setAgentToolVisibility(tools, hiddenTools)
        // FFI 返回 {"ok":true} / {"ok":false,"error":…}，落盘失败须反馈给 UI。
        val ok = runCatching { JSONObject(result).optBoolean("ok", false) }.getOrDefault(false)
        if (ok) {
            Log.i(TAG, "agent tool visibility set: tools=${tools.size}, hidden=${hiddenTools.size}")
        } else {
            Log.w(TAG, "setAgentToolVisibility failed: $result")
        }
        return ok
    }

    // ── 模型选择（持久化已迁移到 [capabilities].main：设置页改绑即
    //    落盘 openslate.toml 并热切换；不再本地 SharedPreferences 重放，
    //    双真相源会互相覆盖）。会话内即时切换用下方 setModelAlias()
    //    （协议 set_model，不落盘）；对话页选择器的「直接选」双写两者
    //    （见 [selectModel]）。──────────────────────────────

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
                        // null = Auto（跟随 provider 默认）：显式 JSON null 经
                        // serde Option 反序列化为 None，落盘即跳键——与既有
                        // 可空字段（max_context_tokens 等）同一模式。
                        "reasoning_effort",
                        m.reasoningEffort ?: JSONObject.NULL,
                    )
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

    /** 删除模型条目（被代号引用时服务端拒绝并定向 notice）。 */
    fun deleteModel(entry: String) = sendJson(
        JSONObject().put("type", "delete_model").put("entry", entry).toString()
    )

    fun setLevel(level: String, entry: String) = sendJson(
        JSONObject().put("type", "set_level").put("level", level).put("entry", entry).toString()
    )

    fun deleteLevel(level: String) = sendJson(
        JSONObject().put("type", "delete_level").put("level", level).toString()
    )

    /** 功能 → 代号绑定（main/compact/title → 代号）。main 同时热切换
     *  会话模型（Rust 侧处理，广播 model_changed + config_changed）。 */
    fun setCapability(capability: String, alias: String) = sendJson(
        JSONObject().put("type", "set_capability")
            .put("capability", capability).put("alias", alias).toString()
    )

    fun setModelAlias(alias: String) = sendJson(
        JSONObject().put("type", "set_model").put("alias", alias).toString()
    )

    /** 对话页模型选择器的统一入口（「直接选」的语义闭环）：一次选中双发
     *  - [setModelAlias]：会话级热切换（协议 set_model，下回合即生效）；
     *  - [setCapability]("main", …)：持久化为启动默认（落盘 openslate.toml）。
     *  返回两次发送是否全部成功（与单发 set* 同为 Boolean 发送语义；用
     *  `and` 而非 `&&` 确保第一条失败第二条仍发）。发送失败容忍：会话已
     *  切换而默认滞后到下次选择，不回滚不重试（下次选中会再写齐）。 */
    fun selectModel(entry: String): Boolean =
        setModelAlias(entry) and setCapability("main", entry)

    // ── 模型别名显示（共享助手；entry 内部键不直出 UI）──────────────

    /**
     * 别名 → 实际命中的 [models] 条目键（镜像 Rust resolve_model 首步：
     * 先查 [levels]，未命中则原样直查 [models]）。UI 侧「把当前值解析到
     * 条目」统一走此助手：模型选择器选中比对/显示、顶栏副标题、思考中
     * chip 的能力判定共用。levels 为 UI 不再暴露的遗留层（保留不写），
     * 此处只读解析作兜底。
     */
    fun modelEntryOf(alias: String, config: ConfigUi): String =
        if (alias.isEmpty()) alias else config.levels[alias] ?: alias

    /**
     * 别名 → UI 显示串。命中 [ConfigUi.models] 条目（直绑形态）→
     * 「Provider显示名/modelId」——provider 显示名与 Provider 页/选择
     * 器同源（title 优先、回退内部 name，见 [ProviderUi.displayName]）；
     * levels 代号（用户自命名，不转换）与未命中 → 原样返回（levels 为
     * UI 不再暴露的遗留层，本分支仅作遗留兜底——调用方一律先经
     * [modelEntryOf] 解析到条目再显示）；空串原样（「未设置」等占位由
     * 调用方处理）。内部 entry 键（`zhipuai/glm-4.7`，历史 `zhipu__glm-4-7`）
     * 经此转换后不出现在任何 UI 文本——顶栏副标题（ChatScreen）、模型
     * 选择器（ComposerPanel）、功能模型绑定行（SettingsScreen）三处共用。
     * 纯函数读传入 config，无额外缓存，config_changed 刷新 state 后自动跟随。
     */
    fun modelAliasDisplay(alias: String, config: ConfigUi): String {
        if (alias.isEmpty()) return alias
        val m = config.models.firstOrNull { it.entry == alias } ?: return alias
        val provider = config.providers.firstOrNull { it.name == m.provider }
            ?.displayName ?: m.provider
        return "$provider/${m.modelId}"
    }

    /** 注册/更新 MCP server（http 条目）。Authorization 不经此传入：
     *  token 由 start() 在 create 前经 setMcpHostToken() 运行时注入
     *  （SecretStore 加密持久化，不落 toml）。 */
    fun upsertMcpServer(name: String, url: String) = sendJson(
        JSONObject()
            .put("type", "upsert_mcp_server")
            .put("name", name)
            .put("url", url)
            .toString()
    )

    /** 热重载外部 MCP server（设置页保存清单后调用，新工具免重启生效）。
     *  FFI 阻塞至全部 server 连接完成（各 server 有内部连接超时），故挂
     *  IO 线程执行；null = 运行时未就绪。返回 FFI 原始 JSON：
     *  {"ok":bool,"tools":N,"servers":{…},"failed":[…],"error":"…"}。
     *  经 [reloadMcpMutex] 串行：reload 含 HostTool 让位/兜底交接，
     *  并发交错会纠缠交接序列（见字段注释）。 */
    suspend fun reloadMcpServers(): String? = withContext(Dispatchers.IO) {
        reloadMcpMutex.withLock {
            runtimeRef.get()?.reloadMcpServers()
        }
    }

    /**
     * 按 call_id 拉取工具调用的完整输出（详情弹窗懒加载）。FFI 侧是
     * Rust 内存 history 的同步扫描（锁内 clone，无网络/磁盘 IO），挂
     * IO 线程仅为不阻塞主线程。null = 运行时未就绪 / 无匹配（call_id
     * 未知、history 被压缩、子代理工具）——调用方回退条目自带的
     * preview 显示。
     */
    suspend fun fetchToolOutput(callId: String): String? = withContext(Dispatchers.IO) {
        runtimeRef.get()?.toolOutputByCallId(callId)
    }

    // ── MCP 服务名（[mcp.servers] 单键名；校验/改名落盘在 Rust）────

    /** 服务名缓存：get / set 成功后刷新；初值 = Rust 缺省（改名前与
     *  存量配置一致）。@Volatile：UI 线程读（[MCP_SERVER_NAMES] 计算
     *  属性）、IO 线程写（get/set/refreshMcpNaming）。 */
    @Volatile
    private var mcpServerNameCache: String = "termux"

    /** 配置里的 MCP server 名单（openslate.toml `[mcp.servers]` 键名，
     *  可经设置页改名，默认 termux）。计算属性直读缓存——注册表工具名
     *  解析与工具管理页聚合共用，调用方零改动自动跟随改名。
     *  （SettingsScreen 的同名私有副本同为计算属性，透传到这里。） */
    val MCP_SERVER_NAMES: List<String> get() = listOf(mcpServerNameCache)

    /**
     * 当前服务名（IO）。null = runtime 未就绪；FFI 返回空串时兜底
     * Rust 缺省名 "termux"（Rust 侧对空/失败本就回退，此处双保险）。
     * 每次调用刷新缓存。
     */
    suspend fun getMcpServerName(): String? = withContext(Dispatchers.IO) {
        val runtime = runtimeRef.get() ?: return@withContext null
        (runtime.getMcpServerName().takeIf { it.isNotBlank() } ?: "termux")
            .also { mcpServerNameCache = it }
    }

    /**
     * 校验 + 改服务名（IO）。Rust 侧规则：非空、`[A-Za-z0-9_.-]`、不含
     * `__`；改名只换 `[mcp.servers]` 键、保留 transport 配置；幂等。
     * 返回 FFI 信封 JSON（{"ok":bool,"error"?}）或 null（runtime 未就绪）。
     * **不自动 reload**：调用方随后自行 [reloadMcpServers] 热生效。仅
     * 信封 ok 时刷新缓存（剥离/聚合立即可用新名）。
     */
    suspend fun setMcpServerName(name: String): String? = withContext(Dispatchers.IO) {
        val runtime = runtimeRef.get() ?: return@withContext null
        val envelope = runtime.setMcpServerName(name)
        if (runCatching { JSONObject(envelope).optBoolean("ok", false) }.getOrDefault(false)) {
            mcpServerNameCache = name.trim()
        }
        envelope
    }

    /**
     * 命名缓存统一刷新点（服务名 + 两个工具名模板）：注册表聚合等后台
     * 路径的前置，三个轻量 FFI 读收敛到一个入口，防调用方漏刷其一导致
     * 剥离用旧名/旧模板。runtime 未就绪时各自不动缓存。
     */
    private suspend fun refreshMcpNaming() {
        getMcpServerName()
        getMcpToolNameTemplate()
        getMcpBuiltinToolNameTemplate()
    }

    // ── MCP 工具名模板（[mcp].tool_name_template；校验/落盘在 Rust）──

    /** Rust 侧 [mcp] 缺省模板的字面（配置未设置 / 保存输入留空时的生效值）。
     *  三占位符模型：{prefix}=服务名设置值（[mcp.servers] 键名）、
     *  {server}=MCP 服务器名（注册名的 slug 段）、{tool}=工具名（不带
     *  服务器名前缀）。默认展开与旧 `{server}_{tool}` 逐字节一致。 */
    private const val DEFAULT_MCP_NAME_TEMPLATE = "{prefix}_{server}__{tool}"

    /** 生效模板缓存：每次 get / set 成功后刷新；null = 尚未从 runtime
     *  读到（此时剥离按默认字面，与 Rust 缺省一致）。@Volatile：UI 线程
     *  读（工具管理页聚合）、IO 线程写（get/set/reload 前置刷新）。 */
    @Volatile
    private var mcpNameTemplate: String? = null

    /**
     * 生效的工具名模板（IO）。null = runtime 未就绪；FFI 空值回退默认
     * 字面。每次调用都刷新缓存——设置页预填与 [mcpServerSlugOf] 剥离
     * 共用同源，防漂移。
     */
    suspend fun getMcpToolNameTemplate(): String? = withContext(Dispatchers.IO) {
        val runtime = runtimeRef.get() ?: return@withContext null
        (runtime.getMcpToolNameTemplate().takeIf { it.isNotBlank() } ?: DEFAULT_MCP_NAME_TEMPLATE)
            .also { mcpNameTemplate = it }
    }

    /**
     * 校验 + 落盘工具名模板（IO）。返回 FFI 信封 JSON
     * （{"ok":bool,"error"?}）或 null（runtime 未就绪）。**不自动
     * reload**：调用方随后自行 [reloadMcpServers] 热生效。仅信封 ok 时
     * 按「留空 = 默认」语义刷新本地缓存（剥离逻辑立即可用新值）。
     */
    suspend fun setMcpToolNameTemplate(template: String): String? = withContext(Dispatchers.IO) {
        val runtime = runtimeRef.get() ?: return@withContext null
        val envelope = runtime.setMcpToolNameTemplate(template)
        if (runCatching { JSONObject(envelope).optBoolean("ok", false) }.getOrDefault(false)) {
            mcpNameTemplate = template.trim().takeIf { it.isNotEmpty() } ?: DEFAULT_MCP_NAME_TEMPLATE
        }
        envelope
    }

    // ── MCP builtin 工具名模板（[mcp].builtin_tool_name_template）────
    // 作用于不含 `__` 的裸上游名：聚合 host 内置 server 的裸名（bash /
    // read_file…）与桌面直连裸名共用同一条展开分支（Rust 侧
    // format_tool_name 的裸名分支）。termux bash 降级阶梯：host 在线
    // → MCP 以该模板展开名接管（默认 termux_bash）；离线 → HostTool
    // RUN_COMMAND 兜底（同名的展示形态一致）。

    /** Rust 侧 builtin 小模板缺省的字面镜像（与 [DEFAULT_MCP_NAME_TEMPLATE]
     *  同款模式：Rust 是唯一事实源，此处仅 FFI 未就绪/配置未设置时的
     *  回退字面）。两占位符模型：{prefix}=服务名、{tool}=裸工具名；无
     *  {server}（裸名没有 server slug 段）。默认展开 termux_bash 与旧
     *  拼接逐字节一致。 */
    private const val DEFAULT_MCP_BUILTIN_NAME_TEMPLATE = "{prefix}_{tool}"

    /** 生效 builtin 模板缓存（语义同 [mcpNameTemplate]）。 */
    @Volatile
    private var mcpBuiltinNameTemplate: String? = null

    /**
     * 生效的 builtin 工具名模板（IO）。null = runtime 未就绪；FFI 空值
     * 回退默认字面。每次调用都刷新缓存——设置页预填与
     * [mcpServerSlugOf]/[mcpToolBareName] 的裸名剥离共用同源，防漂移。
     */
    suspend fun getMcpBuiltinToolNameTemplate(): String? = withContext(Dispatchers.IO) {
        val runtime = runtimeRef.get() ?: return@withContext null
        (runtime.getMcpBuiltinToolNameTemplate().takeIf { it.isNotBlank() }
            ?: DEFAULT_MCP_BUILTIN_NAME_TEMPLATE)
            .also { mcpBuiltinNameTemplate = it }
    }

    /**
     * 校验 + 落盘 builtin 工具名模板（IO）。Rust 侧规则：必须含
     * `{tool}`、占位符仅 `{prefix}`/`{tool}`（拒 `{server}`）、除占位
     * 符外仅 [A-Za-z0-9_.-] 且禁字面 `__`。返回 FFI 信封 JSON 或 null
     * （runtime 未就绪）。**不自动 reload**：调用方随后自行
     * [reloadMcpServers] 热生效（reload 按新模板展开名交接 HostTool
     * 让位/兜底）。仅信封 ok 时按「留空 = 默认」语义刷新缓存。
     */
    suspend fun setMcpBuiltinToolNameTemplate(template: String): String? =
        withContext(Dispatchers.IO) {
            val runtime = runtimeRef.get() ?: return@withContext null
            val envelope = runtime.setMcpBuiltinToolNameTemplate(template)
            if (runCatching { JSONObject(envelope).optBoolean("ok", false) }.getOrDefault(false)) {
                mcpBuiltinNameTemplate = template.trim().takeIf { it.isNotEmpty() }
                    ?: DEFAULT_MCP_BUILTIN_NAME_TEMPLATE
            }
            envelope
        }

    /** 模板编译产物：正则 + {server}/{tool} 的捕获组号（模板缺某占位符
     *  则对应组号 null；组号按占位符出现顺序编号）。 */
    private class TemplateRegex(
        val regex: Regex,
        val serverGroup: Int?,
        val toolGroup: Int?,
    )

    /**
     * 模板 → 正则（三占位符模型）：字面段逐字符 [Regex.escape]（防
     * `.`/`__` 等当元字符），`{prefix}` → 候选前缀值（字面 escape）、
     * `{server}` → lazy 捕获组 `(.+?)`（slug 可含 `_`，lazy 让正则优先
     * 在最近的分隔符处截断）、`{tool}` → greedy 捕获组 `(.+)`（吞到
     * 结尾，工具名含 `__` 也不怕）。配 matchEntire 全名匹配。
     */
    private fun buildTemplateRegex(template: String, prefix: String): TemplateRegex {
        val sb = StringBuilder()
        var serverGroup: Int? = null
        var toolGroup: Int? = null
        var groupCount = 0
        var i = 0
        while (i < template.length) {
            when {
                template.startsWith("{prefix}", i) -> {
                    sb.append(Regex.escape(prefix)); i += "{prefix}".length
                }
                template.startsWith("{server}", i) -> {
                    groupCount += 1; serverGroup = groupCount; sb.append("(.+?)")
                    i += "{server}".length
                }
                template.startsWith("{tool}", i) -> {
                    groupCount += 1; toolGroup = groupCount; sb.append("(.+)")
                    i += "{tool}".length
                }
                else -> {
                    sb.append(Regex.escape(template[i].toString())); i += 1
                }
            }
        }
        return TemplateRegex(Regex(sb.toString()), serverGroup, toolGroup)
    }

    /**
     * 工具名 → 所属 MCP server 的 slug（模板 {server} 捕获组的提取值）。
     * 主路径：对每个候选前缀编译模板正则（[buildTemplateRegex]）后
     * matchEntire 全名匹配，{server} 组值即 slug。不匹配任何候选前缀 →
     * null（非 MCP 名，含恰好同前缀的本地工具如 termux_bash）。
     * 分组退化：模板缺 {server}（注册名不含服务器身份，如
     * `{prefix}_{tool}`）→ slug 不可从模板提取，落 [legacySlugOf] 兜底。
     * builtin 小模板形态（host 裸名注册，默认 termux_bash /
     * termux_read_file）：大模板不匹配且大模板本身也没命中时，用
     * builtin 模板再试一轮——命中 = MCP 域名，slug 退化为前缀本身
     * （这些工具属于聚合 host，没有下游 server 段）。注意 HostTool
     * 兜底形态的 termux_bash 同样命中（名侧无法区分归属，纯展示级
     * 分组，可接受）。工具管理页的 MCP 聚合与 [mcpRegistryToolsByPrefix]
     * 共用此判定，防两处逻辑漂移。
     */
    fun mcpServerSlugOf(toolName: String, serverNames: List<String>): String? {
        val template = mcpNameTemplate ?: DEFAULT_MCP_NAME_TEMPLATE
        var bigMatchedWithoutServer = false
        for (prefix in serverNames) {
            val tr = buildTemplateRegex(template, prefix)
            val m = tr.regex.matchEntire(toolName) ?: continue
            if (tr.serverGroup != null) return m.groups[tr.serverGroup]?.value
            bigMatchedWithoutServer = true
            break // 前缀已匹配但模板无 {server}：正则给不出 slug，走兜底
        }
        if (!bigMatchedWithoutServer) {
            // builtin 小模板（两占位符，无 {server}）：命中 → slug = 前缀。
            val builtin = mcpBuiltinNameTemplate ?: DEFAULT_MCP_BUILTIN_NAME_TEMPLATE
            for (prefix in serverNames) {
                val tr = buildTemplateRegex(builtin, prefix)
                if (tr.regex.matchEntire(toolName) != null) return prefix
            }
        }
        return legacySlugOf(toolName, serverNames)
    }

    /**
     * 旧版 `prefix_` 剥离 + 首个 `__` 取前段启发式：模板未加载（缓存
     * null 理论不可达——get 兜底缺省字面）、模板缺 {server}、或注册表
     * 残留旧命名等异常形态的兜底（非主路径：默认模板下正则恒先命中）。
     * 剥 `候选前缀_` 后含 `__` → 前段即 slug；不含 `__` → null，同旧版。
     */
    private fun legacySlugOf(toolName: String, serverNames: List<String>): String? {
        for (prefix in serverNames) {
            val ns = "$prefix" + "_"
            if (!toolName.startsWith(ns)) continue
            val rest = toolName.substring(ns.length)
            if (!rest.contains("__")) return null
            return rest.substringBefore("__")
        }
        return null
    }

    /**
     * 注册表全名 → {tool} 段（与 [mcpServerSlugOf] 同一模板正则的工具
     * 组提取，{prefix}/{server} 段一并剥掉）。null = 非模板名（正则不
     * 匹配；模板缺 {tool} 理论不可达——Rust 校验必含，缺组同样返回
     * null）。大模板不匹配时再用 builtin 小模板试一轮（host 裸名注册
     * 形态，如 termux_bash → bash；HostTool 兜底形态同样命中 → 展示
     * 裸名一致）。工具管理页的短名展示用；调用方自行决定兜底（现有
     * 调用回退 `__` 启发式）。
     */
    fun mcpToolBareName(fullName: String): String? {
        val template = mcpNameTemplate ?: DEFAULT_MCP_NAME_TEMPLATE
        for (prefix in MCP_SERVER_NAMES) {
            val tr = buildTemplateRegex(template, prefix)
            val m = tr.regex.matchEntire(fullName) ?: continue
            return tr.toolGroup?.let { m.groups[it]?.value }
        }
        val builtin = mcpBuiltinNameTemplate ?: DEFAULT_MCP_BUILTIN_NAME_TEMPLATE
        for (prefix in MCP_SERVER_NAMES) {
            val tr = buildTemplateRegex(builtin, prefix)
            val m = tr.regex.matchEntire(fullName) ?: continue
            return tr.toolGroup?.let { m.groups[it]?.value }
        }
        return null
    }

    /**
     * 工具管理页前缀说明：模板 {tool} 之前的部分，{prefix} 用当前前缀
     * 缓存值、{server} 用传入 slug 替换（缺省模板 + slug=memory →
     * `termux_memory__`）。slug = 前缀本身（builtin 组：host 裸名走
     * builtin 小模板，缺省 → `termux_`）。{tool} 前无内容（如模板就是
     * `{tool}`）→ 空串（UI 隐藏说明行）。
     */
    fun mcpAgentPrefixNote(slug: String): String {
        if (slug == mcpServerNameCache) {
            // builtin 组（[mcpServerSlugOf] 的裸名分支返回前缀本身）。
            val builtin = mcpBuiltinNameTemplate ?: DEFAULT_MCP_BUILTIN_NAME_TEMPLATE
            if (!builtin.contains("{tool}")) return ""
            return builtin.substringBefore("{tool}").replace("{prefix}", mcpServerNameCache)
        }
        val template = mcpNameTemplate ?: DEFAULT_MCP_NAME_TEMPLATE
        if (!template.contains("{tool}")) return ""
        return template.substringBefore("{tool}")
            .replace("{prefix}", mcpServerNameCache)
            .replace("{server}", slug)
    }

    /** 注册表内外部 MCP 工具按 host 前缀（alias）分组计数。
     *  slug 判定复用 [mcpServerSlugOf]（工具管理页聚合同源，模板正则
     *  提取）。
     *  先经 [refreshMcpNaming] 刷新命名缓存（服务名+模板）——后台路径
     *  也会走到这里，用户没开过设置页时缓存可能仍是缺省值而配置已改；
     *  runtime 未就绪则沿用缓存。null = 运行时未就绪。
     *  serverNames：配置里的 MCP server 名单（经 [MCP_SERVER_NAMES] 取，
     *  随改名动态），做成参数防将来多 server。 */
    suspend fun mcpRegistryToolsByPrefix(serverNames: List<String>): Map<String, Int>? =
        withContext(Dispatchers.IO) {
            refreshMcpNaming()
            val tools = listTools() ?: return@withContext null
            val counts = linkedMapOf<String, Int>()
            for (t in tools) {
                val prefix = mcpServerSlugOf(t.name, serverNames) ?: continue
                counts[prefix] = (counts[prefix] ?: 0) + 1
            }
            counts
        }

    /** 审批上行 choice 值域（协议 v3）。approve 为 TUI/兜底保留，mobile UI 只出 5 项。 */
    object ApprovalChoice {
        const val APPROVE = "approve" // TUI/兜底保留，UI 不出
        const val DENY = "deny" // (e) 取消/禁止执行
        const val APPROVE_ALL = "approve_all" // (a) 当前会话允许该工具
        const val ALWAYS_TOOL = "always_tool" // (b) 一直允许该工具
        const val SESSION_MCP = "session_mcp" // (c) 当前会话允许该 MCP
        const val ALWAYS_MCP = "always_mcp" // (d) 一直允许该 MCP
    }

    /** 审批应答（原始 choice 入口；值域见 [ApprovalChoice]）。 */
    fun answerApproval(id: Long, choice: String) = sendJson(
        JSONObject().put("type", "approval_answer").put("id", id).put("choice", choice).toString()
    )

    // ── 审批应答的语义化入口（聊天页审批卡专用；首答赢，重复发送无害）──

    /** (a) 当前会话允许该工具。 */
    fun approveForSession(id: Long) = answerApproval(id, ApprovalChoice.APPROVE_ALL)

    /** (b) 一直允许该工具（Rust 侧落盘 + 广播 config_changed）。 */
    fun approveAlways(id: Long) = answerApproval(id, ApprovalChoice.ALWAYS_TOOL)

    /** (c) 当前会话允许该 MCP 服务器的全部工具。 */
    fun approveMcpForSession(id: Long) = answerApproval(id, ApprovalChoice.SESSION_MCP)

    /** (d) 一直允许该 MCP 服务器的全部工具。 */
    fun approveMcpAlways(id: Long) = answerApproval(id, ApprovalChoice.ALWAYS_MCP)

    /** (e) 取消/禁止执行。拒绝必须真正发送 deny——纯 UI 关闭会让 Rust 侧
     *  审批等待永久挂起（turn 卡死）。 */
    fun deny(id: Long) = answerApproval(id, ApprovalChoice.DENY)

    /**
     * 写审批规则（协议 set_approval_config：Rust 侧整段重写 [approval] 落盘 +
     * 热替换 + 广播 config_changed）。全量覆写语义：默认级 + 两个 map 一次发齐，
     * 「跟随上级/默认」（删除键）= 对应 map 里不带该键。
     */
    fun setApprovalConfig(
        defaultLevel: String,
        toolLevels: Map<String, String>,
        serverLevels: Map<String, String>,
    ) {
        val tools = JSONObject()
        for ((k, v) in toolLevels) tools.put(k, v)
        val servers = JSONObject()
        for ((k, v) in serverLevels) servers.put(k, v)
        sendJson(
            JSONObject()
                .put("type", "set_approval_config")
                .put("default_level", defaultLevel)
                .put("tool_levels", tools)
                .put("server_levels", servers)
                .toString(),
        )
    }

    // ── ask_user：Agent 向用户提问（内联问卷卡 + 全屏作答）──────────────
    //
    // 契约（供 Rust 侧对齐）：
    // - 下发：host_call_requested 事件 {"type":"host_call_requested","id":<Long>,
    //   "tool":"ask_user","args":{"v":1,"questions":[…],"timeout":-1}}；
    //   questions[] 每题 {id,header,question,type,options[{label,description,recommended}],
    //   multi_select,placeholder,optional,storage{kind,name|path}}。
    // - 提交：resolveHostCall(id, true, {"outcome":"answered","answers":[…]}）；
    //   answers[] 每题 {id,selected:[label…]} | {id,value} | {id,value,remember}（secret，
    //   明文仅走本通道）| {id,confirm:bool,selected:[label]} | {id,skipped:true} | {id,declined:true}。
    // - 整卡 decline：resolveHostCall(id, true, {"outcome":"declined"})。
    // - 一切业务结果 ok=true；ok=false 仅基础设施错误。幂等首答赢——对失效
    //   id 的 resolve 必须是无害 no-op（Rust 侧保证）。
    // - 转写：Kotlin 侧在提交/decline 时把**脱敏后**的 outcome JSON 写入关联
    //   ToolCall 条目的 output（secret 只留引用 {name,kind,remember}，明文不进
    //   output、不进日志、不进本地持久化）。

    /** ask_user 工具注册名（groupEntries 吸收判定 / 条目定位共用）。 */
    const val ASK_USER_TOOL = "ask_user"

    /** host call 到达 → 解析题目、建立 pendingQuestion（UI 出卡）。 */
    private fun handleAskUser(id: Long, obj: JSONObject) {
        val args = obj.optJSONObject("args")
        val questionsArr = args?.optJSONArray("questions") ?: obj.optJSONArray("questions")
        val timeout = args?.optLong("timeout", -1L) ?: obj.optLong("timeout", -1L)
        // 解析与 snapshot pending_questions 回放共用（parseAskUserQuestions）。
        val questions = parseAskUserQuestions(questionsArr)
        if (questions.isEmpty()) {
            runtimeRef.get()?.resolveHostCall(id.toULong(), false, "ask_user: empty questions payload")
            return
        }
        // 关联最近的 running ask_user ToolCall 条目（tool_start 先于 host call
        // 到达）；定位失败仍出卡（argsKey 为空则无法回写转写，卡片自身可用）。
        val argsKey = _state.value.entries.asReversed().asSequence()
            .filterIsInstance<UiEntry.ToolCall>()
            .firstOrNull { it.name == ASK_USER_TOOL && it.status == "running" }
            ?.fullArgs ?: ""
        if (argsKey.isEmpty()) {
            Log.w(TAG, "ask_user: no running tool entry to associate (id=$id)")
        }
        mutate {
            copy(
                pendingQuestion = PendingQuestionUi(
                    requestId = id,
                    questions = questions,
                    drafts = List(questions.size) { QuestionDraft() },
                    timeoutSec = timeout,
                    argsKey = argsKey,
                )
            )
        }
        Log.i(TAG, "ask_user: pending card shown (id=$id, ${questions.size} question(s))")
    }

    /** 草稿更新（pending 期有效；切题/进出全屏/重组均经 UiState 不丢）。 */
    fun updateQuestionDraft(requestId: Long, index: Int, transform: (QuestionDraft) -> QuestionDraft) {
        mutate {
            val p = pendingQuestion ?: return@mutate this
            if (p.requestId != requestId || p.phase != "pending" || index !in p.drafts.indices) {
                return@mutate this
            }
            val drafts = p.drafts.toMutableList()
            drafts[index] = transform(drafts[index])
            copy(pendingQuestion = p.copy(drafts = drafts))
        }
    }

    /** 当前题切换（分段条点击 / Pager 滑动共用入口）。 */
    fun setCurrentQuestion(requestId: Long, index: Int) {
        mutate {
            val p = pendingQuestion ?: return@mutate this
            if (p.requestId != requestId || index !in p.questions.indices || index == p.current) {
                return@mutate this
            }
            copy(pendingQuestion = p.copy(current = index))
        }
    }

    /**
     * 提交全部答案。resolve payload 含 secret 明文（仅 FFI 通道传递，不打
     * 日志）；转写 output 为脱敏快照（重启后摘要卡可恢复）。校验由 UI 侧
     * 完成（全部题目「已处理」才可点），此处不再重复判定。
     */
    fun submitQuestionAnswers(requestId: Long) {
        val p = _state.value.pendingQuestion ?: return
        if (p.requestId != requestId || p.phase != "pending") return
        // kotlinx JsonDto 层拼装（buildAskUserSubmitPayloads）：resolve 明文 /
        // 脱敏转写两份 payload 字段集与旧手工 org.json 逐键一致（见 JsonDto.kt）。
        val payloads = buildAskUserSubmitPayloads(p.questions, p.drafts)
        if (p.restored) {
            // 重启恢复卡：host call 通道已不存在，迟到答案走 question_reply 上行。
            submitRestoredQuestionAnswer(requestId, p.argsKey, payloads.answers, payloads.displayJson)
            return
        }
        mutate {
            val cur = pendingQuestion ?: return@mutate this
            if (cur.requestId != requestId) return@mutate this
            copy(pendingQuestion = cur.copy(phase = "submitting"))
        }
        scope.launch(Dispatchers.Default) {
            val runtime = runtimeRef.get()
            val ok = runtime != null && runCatching {
                runtime.resolveHostCall(requestId.toULong(), true, payloads.resolveJson)
            }.getOrDefault(false)
            if (!ok) {
                // 发送失败（runtime 亡 / FFI 异常）：回滚到 pending 允许重试。
                Log.w(TAG, "ask_user: submit resolve failed (id=$requestId)")
                mutate {
                    val cur = pendingQuestion
                    if (cur != null && cur.requestId == requestId && cur.phase == "submitting") {
                        copy(pendingQuestion = cur.copy(phase = "pending"))
                    } else this
                }
                return@launch
            }
            // turn 可能已在提交瞬间结束（cancel 竞态）→ pendingQuestion 被
            // finalize 清除：跳过回写（cancelled 已胜出，resolve 为无害 no-op）。
            val stillOwned = _state.value.pendingQuestion
                ?.let { it.requestId == requestId } == true
            if (stillOwned) {
                writeAskUserOutcome(p.argsKey, payloads.displayJson)
                mutate {
                    val cur = pendingQuestion
                    if (cur != null && cur.requestId == requestId && cur.phase == "submitting") {
                        copy(pendingQuestion = cur.copy(phase = "answered"))
                    } else this
                }
            }
            Log.i(TAG, "ask_user: answered (id=$requestId)")
        }
    }

    /**
     * 恢复卡提交（[PendingQuestionUi.restored]）：改走协议上行
     * `{"type":"question_reply","request_id":"<id>","action":"accept",
     * "answers":[…]}`（request_id 即原 host call id；answers 与 live resolve
     * payload 的 answers 数组同构，Rust 侧 reply_payload 会再包
     * outcome 外壳）。发送成功即写脱敏转写 output 并出卡——恢复态没有
     * live turn 锚定 submitting→answered 相位（续跑由 Rust 侧
     * resume_from_tool_result 驱动，其 turn 事件走常规链路），迟到被拒时
     * Rust 广播 notice、卡不回滚；发送失败（runtime 亡 / FFI 异常）回滚
     * pending 允许重试。
     */
    private fun submitRestoredQuestionAnswer(requestId: Long, argsKey: String, answers: List<AnswerDto>, transcript: String) {
        mutate {
            val cur = pendingQuestion ?: return@mutate this
            if (cur.requestId != requestId) return@mutate this
            copy(pendingQuestion = cur.copy(phase = "submitting"))
        }
        // question_reply 上行（answers 与 live resolve payload 的 answers 数组
        // 同构；kotlinx QuestionReplyDto 拼装，字段集与旧 org.json 版一致）。
        val sent = sendJson(
            BridgeJson.encodeToString(
                QuestionReplyDto(type = "question_reply", requestId = requestId.toString(), action = "accept", answers = answers),
            )
        )
        if (!sent) {
            Log.w(TAG, "ask_user: late submit send failed (id=$requestId)")
            mutate {
                val cur = pendingQuestion
                if (cur != null && cur.requestId == requestId && cur.phase == "submitting") {
                    copy(pendingQuestion = cur.copy(phase = "pending"))
                } else this
            }
            return
        }
        writeAskUserOutcome(argsKey, transcript)
        mutate { copy(pendingQuestion = pendingQuestion?.takeIf { it.requestId != requestId }) }
        Log.i(TAG, "ask_user: late answer sent (id=$requestId)")
    }

    /** 整卡 decline（「不回答」，UI 层已二次确认）。 */
    fun declineQuestionCard(requestId: Long) {
        val p = _state.value.pendingQuestion ?: return
        if (p.requestId != requestId || p.phase != "pending") return
        if (p.restored) {
            // 恢复卡 decline 同走 question_reply（decline 负载协议透传，null 即可；
            // Rust 侧与 accept 同一迟到通路——告知模型用户未作答后续跑）。
            val sent = sendJson(
                JSONObject()
                    .put("type", "question_reply")
                    .put("request_id", requestId.toString())
                    .put("action", "decline")
                    .put("answers", JSONObject.NULL)
                    .toString()
            )
            if (!sent) return // 发送失败保留卡允许重试（send 失败 = 确定未达）
            writeAskUserOutcome(p.argsKey, """{"outcome":"declined"}""")
            mutate { copy(pendingQuestion = pendingQuestion?.takeIf { it.requestId != requestId }) }
            Log.i(TAG, "ask_user: late decline sent (id=$requestId)")
            return
        }
        scope.launch(Dispatchers.Default) {
            val runtime = runtimeRef.get() ?: return@launch
            runtime.resolveHostCall(requestId.toULong(), true, """{"outcome":"declined"}""")
        }
        writeAskUserOutcome(p.argsKey, """{"outcome":"declined"}""")
        mutate { copy(pendingQuestion = pendingQuestion?.takeIf { it.requestId != requestId }) }
        Log.i(TAG, "ask_user: declined (id=$requestId)")
    }

    /** 把终态/答案 JSON 写回关联条目（转写与摘要同源）。首答赢：条目已有的
     *  本工具终态 JSON（含 outcome 字段）不覆盖（cancelled/declined 先胜）；
     *  tool_end 先到的事件 preview（无 outcome 字段的普通文本）可被修正。 */
    private fun writeAskUserOutcome(argsKey: String, json: String) {
        if (argsKey.isEmpty()) return
        mutate {
            val idx = entries.indexOfFirst {
                it is UiEntry.ToolCall && it.name == ASK_USER_TOOL && it.fullArgs == argsKey
            }
            if (idx < 0) return@mutate this
            val e = entries[idx] as UiEntry.ToolCall
            if (e.output != null && isAskUserOutcomeJson(e.output)) return@mutate this
            copy(entries = entries.toMutableList().also { it[idx] = e.copy(output = json) })
        }
    }

    private fun isAskUserOutcomeJson(s: String?): Boolean {
        if (s.isNullOrBlank()) return false
        val o = runCatching { JSONObject(s) }.getOrNull() ?: return false
        return o.has("outcome")
    }

    /**
     * turn 离开 running 态：清理 ask_user pending 态。已答/已拒只清（摘要由
     * entry.output 承载）；仍 pending/submitting = turn 被打断（用户停止等）
     * → 写 cancelled 终态。迟到的 resolve 对失效 id 无害（幂等首答赢）。
     */
    private fun finalizeAskUserOnTurnEnd() {
        val p = _state.value.pendingQuestion ?: return
        if (p.phase == "pending" || p.phase == "submitting") {
            writeAskUserOutcome(p.argsKey, """{"outcome":"cancelled"}""")
        }
        mutate { copy(pendingQuestion = null) }
    }

    // ── ask_user 已答问卷只读回看 ───────────────────────────────────────

    /**
     * 打开已答问卷回看（摘要卡点击入口）：取关联条目的 fullArgs + output，
     * 经 [buildReviewedQuestion] 装配只读投影写入 UiState（NavHost 路由
     * `question-review` 由 UI 层随后导航）。解析失败（args 缺失的老条目 /
     * 非 answered outcome）返回 false 且**不动 state**，调用方退回旧答案
     * 弹窗兜底。
     */
    fun openQuestionReview(fullArgs: String, output: String?): Boolean {
        val review = buildReviewedQuestion(fullArgs, output) ?: return false
        mutate { copy(questionReview = review) }
        return true
    }

    /** 关闭回看（回看页退出时清态，避免陈旧投影跨场景残留）。 */
    fun closeQuestionReview() {
        if (_state.value.questionReview != null) mutate { copy(questionReview = null) }
    }

    // ── ask_user 行为偏好（本地 SharedPreferences = UI 显示源；生效真值
    //    在 Rust 侧 openslate.toml 的 [ask_user] 节——每次变更经协议
    //    set_ask_user_config 推送落盘，启动时同步一次本地值）──

    private const val PREF_ASK_USER_TIMEOUT_SEC = "ask_user_timeout_sec"
    private const val PREF_ASK_USER_MASK_SECRETS = "ask_user_mask_secrets"

    /** ask_user 默认超时（秒；-1 = 永不。首装默认永不超时）。 */
    fun askUserTimeoutSec(): Long =
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.getLong(PREF_ASK_USER_TIMEOUT_SEC, -1L) ?: -1L

    fun setAskUserTimeoutSecAndPersist(sec: Long) {
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.edit()?.putLong(PREF_ASK_USER_TIMEOUT_SEC, sec)?.apply()
        Log.i(TAG, "ask_user default timeout set to ${sec}s")
        pushAskUserConfig(sec, askUserMaskSecrets())
    }

    /** 工具输出脱敏开关（对已知密钥值自动打码；默认开）。 */
    fun askUserMaskSecrets(): Boolean =
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.getBoolean(PREF_ASK_USER_MASK_SECRETS, true) ?: true

    fun setAskUserMaskSecretsAndPersist(on: Boolean) {
        appContext?.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
            ?.edit()?.putBoolean(PREF_ASK_USER_MASK_SECRETS, on)?.apply()
        Log.i(TAG, "ask_user secret masking set to $on")
        pushAskUserConfig(askUserTimeoutSec(), on)
    }

    /**
     * 推送 ask_user 行为配置到引擎（协议 set_ask_user_config：Rust 侧写
     * active openslate.toml [ask_user] 落盘 + 热替换 + 广播 config_changed）。
     * timeoutSec：-1 = 永不（线格式 "never"），>0 = 秒（{"seconds": n}）。
     */
    private fun pushAskUserConfig(timeoutSec: Long, maskSecrets: Boolean): Boolean {
        val timeout = if (timeoutSec < 0) "never"
            else JSONObject().put("seconds", timeoutSec)
        return sendJson(
            JSONObject().put("type", "set_ask_user_config")
                .put("default_timeout", timeout)
                .put("mask_secrets", maskSecrets)
                .toString()
        )
    }

    /** 启动同步：本地偏好推一次给引擎（配置文件为生效真值；此后设置页
     *  每次变更均双写，两端不漂移）。 */
    private fun syncAskUserConfig() {
        val sec = askUserTimeoutSec()
        val mask = askUserMaskSecrets()
        val sent = pushAskUserConfig(sec, mask)
        Log.i(TAG, "start: ask_user config synced (timeout=${sec}s, mask=$mask, sent=$sent)")
    }

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
            // 在途附件提交失去落点：终结等待（composer 草稿保留）。
            dropPendingEcho()
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
                val callId = obj.optString("call_id").ifBlank { null }
                mutate {
                    copy(
                        toolCalls = toolCalls + 1,
                        awaitingFirstToken = false,
                        entries = entries + UiEntry.ToolCall(
                            name = name,
                            argsPreview = fullArgs.take(64),
                            status = "running",
                            fullArgs = fullArgs,
                            callId = callId,
                        )
                    )
                }
            }
            "tool_end" -> {
                val name = obj.optString("name", "tool")
                val callId = obj.optString("call_id").ifBlank { null }
                // 优先 Kotlin 侧缓存（host 工具的完整输出）；native 工具在 Rust
                // 进程内执行，没有 host call，用事件携带的 preview 兜底。
                val out = takeToolOutput(name)
                    ?: obj.optString("preview", "").ifBlank { null }
                mutate {
                    // 配对：call_id 精确匹配优先（新链路，事件与条目都携带）；
                    // 任一侧缺失 call_id 时回退既有契约——Rust 保证 tool_end
                    // 按工具调用顺序发射，并发多条同名调用时第 N 个 tool_end
                    // 对应该名下第一个仍在 running 的条目（旧事件/旧镜像路径）。
                    // （indexOfLast 会在并发时把输出互相配错。）
                    val idx = (if (callId != null) {
                        entries.indexOfFirst { it is UiEntry.ToolCall && it.callId == callId }
                    } else -1).let { exact ->
                        if (exact >= 0) exact else entries.indexOfFirst {
                            it is UiEntry.ToolCall && it.name == name && it.status == "running"
                        }
                    }
                    if (idx >= 0) {
                        val e = entries[idx] as UiEntry.ToolCall
                        // ask_user 的答案/终态 JSON（提交时由 Kotlin 写入）优先于
                        // 事件 preview：保持已答摘要与转写同源、不被覆盖。
                        val finalOut = if (e.name == ASK_USER_TOOL && e.output != null) e.output else out ?: e.output
                        copy(entries = entries.toMutableList().also { it[idx] = e.copy(status = "done", output = finalOut) })
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
            "request_start" -> {
                // 附件提交被引擎接受：回显 User 条目（携带附件引用）并广播
                // accepted 终态（composer 清空草稿与 chips）。
                pendingEcho.getAndSet(null)?.let { pe ->
                    mutate { copy(entries = entries + UiEntry.User(pe.text, pe.refs)) }
                    emitSubmitResolution(accepted = true)
                }
                // 恢复卡失效出清：新 turn 开启前 Rust 侧悬挂修复已把 pending 行
                // superseded 关卷（迟到通道关闭）；且重启后 host call id 计数器
                // 从 1 复位，恢复卡的数字 request_id 可能撞上新在途调用——
                // question_reply 的在途通路（MobileQuestionHandler 先按 id resolve）
                // 会误投。出清并记 cancelled 终态（与停止回合同语义）。live 卡
                // 不受影响（restored=false；恢复卡自身触发的续跑在提交时已出卡）。
                _state.value.pendingQuestion?.takeIf { it.restored }?.let { stale ->
                    writeAskUserOutcome(stale.argsKey, """{"outcome":"cancelled"}""")
                    mutate { copy(pendingQuestion = pendingQuestion?.takeIf { it.requestId != stale.requestId }) }
                    Log.i(TAG, "ask_user: restored card superseded by new turn (id=${stale.requestId})")
                }
                mutate {
                    val turnBegan = !running
                    copy(
                        running = true,
                        step = obj.optInt("step", step),
                        toolCalls = if (turnBegan) 0 else toolCalls,
                        awaitingFirstToken = true,
                    )
                }.also { stepStartTs = System.currentTimeMillis() }
            }
            "turn_ok" -> mutate {
                copy(running = false, awaitingFirstToken = false)
            }.also {
                finalizeAskUserOnTurnEnd()
                // 落盘键取 TurnSummaryDto 自带的 run_id（wire 契约恒有），
                // 与本事件同时到达 → 键与 _state.entries 严格同代；不读
                // FFI 实时态（open_session 并发换 run 时会错位）。
                // optStringOrNull：optString 对 JSON null 返回字面量 "null"
                // （org.json 陷阱）——曾致镜像写入 transcripts/null.json 僵尸文件。
                persistTranscriptAsync(obj.optJSONObject("summary")?.optStringOrNull("run_id"))
            }
            "turn_error" -> mutate {
                copy(
                    running = false,
                    awaitingFirstToken = false,
                    entries = entries + UiEntry.Meta("⚠ " + obj.optString("message", "error")),
                )
            }.also {
                finalizeAskUserOnTurnEnd()
                // turn_error 无 summary：兜底最近快照的 run_id（泵协程内，
                // FIFO 保证它仍是本 run——见 persistTranscriptAsync 注释）。
                persistTranscriptAsync(lastKnownRunId)
            }
            "notice" -> {
                val level = obj.optString("level", "info")
                val text = obj.optString("text", "")
                mutate { copy(entries = entries + UiEntry.Meta(text)) }
                // 级别化投影：error/warn 由 ChatScreen 提为 snackbar（附件
                // 门控拒绝等即时反馈；info 仍只进转写）。
                noticeSeq += 1
                _lastNotice.value = BridgeNoticeUi(noticeSeq, level, text)
                // 附件提交等待接受期间收到 warn/error = 提交未落地（门控
                // 拒绝 Error / 回合进行中 Warn）→ 结束等待，草稿保留。
                if (level == "error" || level == "warn") dropPendingEcho()
            }
            "error" -> mutate { copy(lastError = obj.optString("message", "protocol error")) }
            "approval_requested" -> {
                val summary = obj.optJSONObject("summary")
                mutate {
                    copy(
                        pendingApproval = PendingApprovalUi(
                            id = obj.optLong("id"),
                            toolName = summary?.optString("tool_name") ?: "?",
                            arguments = summary?.optString("arguments").orEmpty(),
                            riskLevel = summary?.optString("risk_level") ?: "?",
                            server = summary?.optStringOrNull("server"),
                            trigger = summary?.optStringOrNull("trigger"),
                            note = summary?.optStringOrNull("note"),
                        )
                    )
                }
            }
            "approval_resolved" -> mutate { copy(pendingApproval = null) }
            "session_reset" -> {
                dropPendingEcho()
                mutate {
                    copy(
                        entries = emptyList(),
                        pendingApproval = null,
                        pendingQuestion = null,
                        questionReview = null, // 回看页在场时随 null 自动 pop（页内 LaunchedEffect）
                    )
                }
            }
            "model_changed" -> mutate { copy(modelAlias = obj.optString("alias")) }
            // run 标题生成完成：只递增版本号，历史页观察到后自行刷新
            // （会话列表不在全局 state 里，拉取式投影）。
            "run_title" -> mutate { copy(titlesVersion = titlesVersion + 1) }
            "config_changed" -> obj.optJSONObject("config")?.let { applyConfig(it) } ?: Unit
            "host_call_requested" -> handleHostCall(obj)
            else -> Unit // step_end / first_token 等暂不投影
        }
    }

    /** 解析 ConfigViewDto → ConfigUi。 */
    /** transcript User 条目的 attachments 数组 → 引用投影（缺键/旧快照 → 空表）。 */
    private fun parseAttachmentRefs(arr: JSONArray?): List<AttachmentRefUi> {
        if (arr == null) return emptyList()
        return buildList {
            for (i in 0 until arr.length()) {
                val o = arr.optJSONObject(i) ?: continue
                add(
                    AttachmentRefUi(
                        id = o.optString("id"),
                        mime = o.optString("mime"),
                        filename = o.optStringOrNull("filename"),
                        // 可选键：缺键 / 类型异常 optLong 均落 -1 哨兵，统一
                        // 收敛 null（同上一轮 importAttachment 写法）。
                        tokens = o.optLong("tokens", -1L).takeIf { it >= 0 },
                    ),
                )
            }
        }
    }

    private fun parseConfig(c: JSONObject): ConfigUi =
        // kotlinx JsonDto 层解析（decodeConfigJson）：ConfigViewDto 投影的逐条目
        // 容错（坏条目跳过、Option 双形态 null/缺键同义）与旧手工遍历一致。
        decodeConfigJson(c.toString())

    private fun applyConfig(c: JSONObject) {
        mutate { copy(config = parseConfig(c)) }
        // 迁移第二段：config_changed 到达 → 完成校验（幂等，无待校验则 no-op）。
        verifyModelEntryMigration()
        verifyCapabilityDirectMigration()
        // 条目键迁移若在本轮校验通过，链式启动 capabilities 直绑迁移（闸门/
        // 标记幂等，未轮到时为 no-op）。
        maybeMigrateCapabilityDirect()
    }

    // ── 模型条目键一次性迁移（legacy `p__m` slug → `p/m`；两段式）──────

    /** 迁移防重跑标记（FILE_MAIN；**校验通过才置位**，置位后永不再跑）。
     *  v2：v1（`migrated_model_entry_slash_v1`）被修复前的旧代码在**未校验**
     *  的情况下无条件置位——真机上迁移实际失败但 v1=true，永久跳过修复。
     *  v1 不可信，弃用；v2 由两段式校验通过后才写。 */
    private const val PREF_MIGRATED_MODEL_ENTRY_SLASH = "migrated_model_entry_slash_v2"

    /** 进程内单发闸：snapshot 可能多次到达（重开会话等），防重复挂协程。 */
    @Volatile
    private var modelEntryMigrationLaunched = false

    /**
     * 第二段「待校验」态：本轮已发送迁移 op 的条目清单（null = 无待校验）。
     * 第一段（IO 协程）全部发送完毕后置入；第二段（事件泵，config 落
     * state 处）在每次 config 更新到达时核对，**全部通过才写 prefs 标记**。
     * AtomicReference 保证 IO 协程与事件泵跨线程互见。
     */
    private val pendingMigrationVerify = AtomicReference<List<MigrationAttempt>?>(null)

    /** 单条核对记录：旧键应消失、新键应存在、迁移时引用旧键的
     *  levels/capabilities 应已改指新键。 */
    private class MigrationAttempt(
        val oldEntry: String,
        val newEntry: String,
        val levelKeys: List<String>,
        val capKeys: List<String>,
    )

    /**
     * 第一段（挂载点不变：首个 snapshot = runtime 就绪且 config 已入
     * state 的最早时刻）：逐条发送迁移 op（**先删后插**，见
     * [migrateModelEntrySlash]），发完**不置标记**、进入待校验态，由
     * [verifyModelEntryMigration] 在后续 config 事件路径上完成校验。
     * prefs 标记已置位则只闸不跑。整体异常不置标记 → 下次启动重试；
     * 操作幂等（已迁移条目不再匹配公式、目标被占用安全跳过），重跑
     * 无副作用。
     */
    private fun maybeMigrateModelEntrySlash() {
        if (modelEntryMigrationLaunched) return
        val ctx = appContext ?: return
        modelEntryMigrationLaunched = true
        val prefs = ctx.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
        if (prefs.getBoolean(PREF_MIGRATED_MODEL_ENTRY_SLASH, false)) return
        scope.launch(Dispatchers.IO) {
            try {
                val attempted = migrateModelEntrySlash(_state.value.config)
                if (attempted.isEmpty()) {
                    // 本轮没有实际发送任何迁移 op（无匹配 / 全部安全跳过）：
                    // 空核对清单视同校验通过，直接置标记。
                    prefs.edit().putBoolean(PREF_MIGRATED_MODEL_ENTRY_SLASH, true).apply()
                    Log.i(TAG, "model-entry migration: nothing attempted, marked done ($PREF_MIGRATED_MODEL_ENTRY_SLASH)")
                    // 条目键迁移完成 → capabilities 直绑迁移解锁（幂等）。
                    maybeMigrateCapabilityDirect()
                } else {
                    pendingMigrationVerify.set(attempted)
                    Log.i(TAG, "model-entry migration: ops sent for ${attempted.size} entrie(s), awaiting config verification")
                }
            } catch (t: Throwable) {
                Log.e(TAG, "model-entry migration: aborted, will retry next launch", t)
            }
        }
    }

    /**
     * 第二段（挂既有 config 事件路径：applyConfig / applySnapshot，无轮询
     * 定时器）：config 更新到达时重读 state，逐条核对**本轮实际尝试迁移**
     * 的条目——旧键消失 && 新键存在 && levels/capabilities 引用已指向
     * 新键。全部通过 → 写 prefs 标记 + Log.i 汇总；任一不通过 → Log.e
     * 明细、**不置标记**（本批 op 逐条回刷期间的中间态也会短暂不通过，
     * 保留待校验态等后续 config 事件再核；进程内始终未过 → 下次启动整体
     * 重试，幂等自愈）。「目标键占用导致 skip」的条目没实际尝试，不在
     * 核对清单内。
     */
    private fun verifyModelEntryMigration() {
        val records = pendingMigrationVerify.get() ?: return
        val ctx = appContext ?: return
        val cfg = _state.value.config
        val entries = cfg.models.map { it.entry }.toSet()
        val problems = buildList {
            for (r in records) {
                if (r.oldEntry in entries) add("'${r.oldEntry}': old entry still present")
                if (r.newEntry !in entries) add("'${r.newEntry}': new entry missing")
                for (lvl in r.levelKeys) {
                    val v = cfg.levels[lvl]
                    if (v != r.newEntry) add("levels.$lvl = '${v ?: "<unset>"}' (expected '${r.newEntry}')")
                }
                for (cap in r.capKeys) {
                    val v = cfg.capabilities[cap]
                    if (v != r.newEntry) add("capabilities.$cap = '${v ?: "<unset>"}' (expected '${r.newEntry}')")
                }
            }
        }
        if (problems.isEmpty()) {
            pendingMigrationVerify.set(null)
            ctx.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
                .edit().putBoolean(PREF_MIGRATED_MODEL_ENTRY_SLASH, true).apply()
            Log.i(TAG, "model-entry migration: verified ${records.size} entrie(s), marked done ($PREF_MIGRATED_MODEL_ENTRY_SLASH)")
            // 条目键迁移完成 → capabilities 直绑迁移解锁（此前被闸住未启动）。
            maybeMigrateCapabilityDirect()
        } else {
            Log.e(TAG, "model-entry migration: verification not passed (${problems.size}): ${problems.joinToString("; ")}")
        }
    }

    /** 旧生成器（SettingsScreen.genModelEntryId 的 `__` 版本）slug 公式的镜像。 */
    private fun legacyEntrySlug(s: String): String =
        s.lowercase().replace(Regex("[^a-z0-9-]+"), "-").trim('-').ifEmpty { "m" }

    /**
     * 自动生成判定（精确公式匹配，零误伤）：entry == `<slug(P)>__<slug(M)>`
     * 或再带 4 位 hex 冲突后缀——旧生成器唯一两种产出。用户手写 entry
     * 不匹配公式，一律不动；levels 的 KEY（用户命名空间）同样不动。
     */
    private fun isLegacyAutoEntry(entry: String, provider: String, modelId: String): Boolean {
        val base = Regex.escape("${legacyEntrySlug(provider)}__${legacyEntrySlug(modelId)}")
        return Regex("^$base(-[0-9a-f]{4})?$").matches(entry)
    }

    /**
     * 逐条迁移（单条失败跳过并日志，不中断整体），返回本轮**实际尝试**
     * （delete 已发送）的核对清单。每条顺序**先删后插**——core 的
     * `upsert_model` 有 (provider, model) 全局唯一守卫（config_ops.rs），
     * upsert 先行则旧键还在、同组合新键必被拒（error 通知直入用户事件
     * 流，且 levels 改指后会指向从未建成的幽灵新键）；删模型不设引用
     * 拦截（levels/capabilities 松耦合、悬空合法，仅发一条 Info 级悬空
     * 提示、随后立即改指消除）：
     * ① deleteModel 旧键（发送失败 → 跳过该条，服务端无变化，无损失）；
     * ② upsertModel 新键（完整复制原字段；此时同 (provider, model) 已无
     *    占用、必过守卫。发送失败 → 立即回补旧键恢复条目完整性——组合
     *    已空出、回补同样必过守卫，**确保下次启动重试路径能自愈**；回补
     *    也失败（通道已死）才留半迁移残局，计入 skipped + Log.e。两种
     *    结局均入核对清单，拦住标记不置）；
     * ③ `[levels]` value 改指新键（KEY 不变）→ `[capabilities]` 直绑值
     *    同理（新键已在，服务端可解析性检查可过；任一发送失败只 Log.e，
     *    由核对清单拦标记）；
     * ④ 会话别名仍指旧键则热切（set_model，防删除后内存别名悬空）。
     * 中途 crash 的窗口（删了没插回）按既有日志口径记 Log.e、可接受。
     * 全部经现有协议操作（persist 落盘 + config_changed 回刷），**无新
     * FFI**。目标键被其他 entry 占用则跳过该条（不尝试、不入清单）。
     */
    private fun migrateModelEntrySlash(config: ConfigUi): List<MigrationAttempt> {
        val occupied = config.models.map { it.entry }.toMutableSet()
        val attempted = mutableListOf<MigrationAttempt>()
        var migrated = 0
        var skipped = 0
        val userAuthored = config.models.count {
            !isLegacyAutoEntry(it.entry, it.provider, it.modelId)
        }
        for (m in config.models) {
            if (!isLegacyAutoEntry(m.entry, m.provider, m.modelId)) continue
            val target = "${m.provider.trim()}/${m.modelId.trim()}"
            if (target != m.entry && target in occupied) {
                skipped++
                Log.w(TAG, "model-entry migration: skip '${m.entry}' -> '$target' (target occupied)")
                continue
            }
            val levelKeys = config.levels.filterValues { it == m.entry }.keys.toList()
            val capKeys = config.capabilities.filterValues { it == m.entry }.keys.toList()
            try {
                // ① 先删旧键（避让 (provider, model) 唯一守卫）。
                if (!deleteModel(m.entry)) {
                    skipped++
                    Log.w(TAG, "model-entry migration: skip '${m.entry}' (delete send failed; nothing changed)")
                    continue
                }
                // ② 插新键；发送失败 → 回补旧键保住条目完整性。
                if (!upsertModel(m.copy(entry = target))) {
                    skipped++
                    if (upsertModel(m)) {
                        Log.e(
                            TAG,
                            "model-entry migration: '${m.entry}' rolled back " +
                                "(new-key upsert send failed, old key restored); will retry next launch",
                        )
                    } else {
                        Log.e(
                            TAG,
                            "model-entry migration: '${m.entry}' half-migrated " +
                                "(old deleted, new & rollback sends failed); left for next-launch retry",
                        )
                    }
                    attempted.add(MigrationAttempt(m.entry, target, levelKeys, capKeys))
                    continue
                }
                // ③ levels/capabilities 改指新键；④ 会话别名热切。
                var repointOk = true
                for (level in levelKeys) if (!setLevel(level, target)) repointOk = false
                for (cap in capKeys) if (!setCapability(cap, target)) repointOk = false
                if (!repointOk) {
                    Log.e(
                        TAG,
                        "model-entry migration: '${m.entry}' repoint send failed; " +
                            "refs may stay on deleted old key (verification will hold the mark)",
                    )
                }
                if (_state.value.modelAlias == m.entry) setModelAlias(target)
                occupied.remove(m.entry)
                occupied.add(target)
                attempted.add(MigrationAttempt(m.entry, target, levelKeys, capKeys))
                if (repointOk) migrated++ else skipped++
                Log.i(
                    TAG,
                    "model-entry migration: '${m.entry}' -> '$target' ops sent" +
                        if (repointOk) "" else " (repoint incomplete)",
                )
            } catch (t: Throwable) {
                skipped++
                // sendJson 不抛（返回 Boolean），此处纯防御；delete 可能已发，
                // 入清单让校验段拦标记，宁可多等一轮也不误置。
                attempted.add(MigrationAttempt(m.entry, target, levelKeys, capKeys))
                Log.e(TAG, "model-entry migration: '${m.entry}' failed, skipped", t)
            }
        }
        Log.i(
            TAG,
            "model-entry migration: pass done (total=${config.models.size}, " +
                "migrated=$migrated, skipped=$skipped, userAuthored=$userAuthored, " +
                "awaitingVerify=${attempted.size})",
        )
        return attempted
    }

    // ── capabilities 直绑一次性迁移（代号引用 → models 条目；两段式）─────
    //
    // UI 已全面去 levels（设置页功能绑定/选择器只列 models 条目），本迁移把
    // 存量配置里「capability → levels 代号」的引用一次性改写为「capability →
    // 条目直绑」，并给走缺省回退的功能补显式落点。迁移范围只有 main/title：
    // compact 不再生成写入（未绑定时 core 侧继承会话模型；已存在的显式绑定
    // 不删不写，留给用户/TUI 管理）。[levels] 表不删不写（无删除 FFI，留作
    // 不被引用的遗留层；resolve_model 的 levels 优先序不变，只读解析作遗留
    // 兜底）。沿用模型条目键迁移的两段式状态机：第一段发 op 不置标记，第二
    // 段挂 config 事件路径校验、全过才写 prefs 标记；操作幂等（stored ==
    // target 不重发），失败下次启动重试。

    /** 迁移防重跑标记（FILE_MAIN；**校验通过才置位**，置位后永不再跑）。 */
    private const val PREF_MIGRATED_CAPABILITY_DIRECT = "migrated_capability_direct_v1"

    /** 迁移目标功能（镜像 core model_config.rs CAPABILITIES，但 **compact
     *  除外**——未绑定时 core 侧继承会话模型，不再为它生成显式写入；已存在
     *  的显式绑定不删不写，留给用户/TUI）。main 的显式写入即启动默认机制，
     *  保留。 */
    private val CAPABILITY_IDS = listOf("main", "title")

    /** 进程内单发闸：snapshot 多次到达防重复挂协程。 */
    @Volatile
    private var capDirectMigrationLaunched = false

    /**
     * 第二段「待校验」态：本轮已发送的 (功能, 目标条目) 清单（null = 无待
     * 校验）。第一段发完置入，第二段（applyConfig/applySnapshot 的 config
     * 路径）在每次 config 更新到达时核对，全部通过才写 prefs 标记。
     */
    private val pendingCapDirectVerify = AtomicReference<List<Pair<String, String>>?>(null)

    /**
     * 迁移计划（纯函数）。每个功能算目标条目：
     * - 生效别名（capabilities[cap]，缺省回退 main→"main"、compact/title→
     *   "fast"，镜像 core capability_alias）命中 [levels] 键 → 改写为 levels
     *   值（条目键）——含功能完全缺省、回退别名实为代号的情况；
     * - 功能缺省且回退别名直接命中 [models] 条目（无 levels 的存量配置）→
     *   显式写入，让主对话有明确绑定落点（行为不变，只把隐式缺省变显式）；
     * - 已直绑（生效别名是条目键）→ 不动；
     * - 悬空（levels 指向缺失条目 / 别名既非代号也非条目）→ 自动改写无从
     *   谈起，剔除——UI 端该功能行显示「未设置」，用户重选条目即自愈；
     *   main 侧 Rust 启动链对不可解析别名本就回退 root agent 别名，不会瘫。
     * stored == target（代号与条目同名的 "fast"→"fast" 形态）不发 op：配置
     * 值已就位，写无变化。
     *
     * 迁移范围只有 main/title（[CAPABILITY_IDS]）：compact 不再生成写入——
     * 未绑定时 core 侧继承会话模型，已存在的显式绑定不删不写（留给用户/
     * TUI）；main 的显式写入即启动默认机制，保留。
     */
    private fun planCapabilityDirect(config: ConfigUi): List<Pair<String, String>> {
        val entries = config.models.map { it.entry }.toSet()
        val plan = mutableListOf<Pair<String, String>>()
        for (cap in CAPABILITY_IDS) {
            val stored = config.capabilities[cap]
            val fallback = if (cap == "main") "main" else "fast"
            val effective = stored ?: fallback
            val target = when {
                config.levels.containsKey(effective) -> config.levels[effective]
                stored == null && effective in entries -> effective
                else -> null
            }?.takeIf { it in entries }
            if (target != null && target != stored) plan.add(cap to target)
        }
        return plan
    }

    /**
     * 第一段（挂载点：applySnapshot 末尾 + 条目键迁移校验通过后的链式调用；
     * **必须等模型条目键迁移完成**——后者同样改写 capabilities 引用，并行
     * 互踩会把引用写回已删除的旧键，故 prefs 里 slash 标记未置位时直接
     * 返回、不置本闸门，由后续 config 事件重试）：逐条发送 setCapability
     * （fire-and-forget，发送失败仅 Log.e，由校验段拦标记），发完进入待
     * 校验态；无计划（已迁移/无可改）直接置标记。
     */
    private fun maybeMigrateCapabilityDirect() {
        if (capDirectMigrationLaunched) return
        val ctx = appContext ?: return
        val prefs = ctx.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
        if (!prefs.getBoolean(PREF_MIGRATED_MODEL_ENTRY_SLASH, false)) return
        capDirectMigrationLaunched = true
        if (prefs.getBoolean(PREF_MIGRATED_CAPABILITY_DIRECT, false)) return
        scope.launch(Dispatchers.IO) {
            try {
                val plan = planCapabilityDirect(_state.value.config)
                if (plan.isEmpty()) {
                    prefs.edit().putBoolean(PREF_MIGRATED_CAPABILITY_DIRECT, true).apply()
                    Log.i(TAG, "capability-direct migration: nothing to do, marked done ($PREF_MIGRATED_CAPABILITY_DIRECT)")
                } else {
                    for ((cap, target) in plan) {
                        if (!setCapability(cap, target)) {
                            Log.e(TAG, "capability-direct migration: send failed for $cap -> $target (verification will hold the mark)")
                        }
                    }
                    pendingCapDirectVerify.set(plan)
                    Log.i(TAG, "capability-direct migration: ops sent for ${plan.size} cap(s), awaiting config verification")
                }
            } catch (t: Throwable) {
                Log.e(TAG, "capability-direct migration: aborted, will retry next launch", t)
            }
        }
    }

    /**
     * 第二段（挂 applyConfig / applySnapshot 的 config 事件路径，无轮询）：
     * config 更新到达时重读 state，逐条核对**本轮实际发送**的功能——值已
     * 等于目标条目 && 条目在 [models] 里。全部通过 → 写 prefs 标记；任一
     * 不通过 → Log.e 明细、不置标记（op 逐条回刷的中间态会短暂不通过，
     * 保留待校验态等后续 config 事件再核；进程内始终未过 → 下次启动幂等
     * 重试）。
     */
    private fun verifyCapabilityDirectMigration() {
        val plan = pendingCapDirectVerify.get() ?: return
        val ctx = appContext ?: return
        val cfg = _state.value.config
        val entries = cfg.models.map { it.entry }.toSet()
        val problems = buildList {
            for ((cap, target) in plan) {
                val v = cfg.capabilities[cap]
                if (v != target) add("capabilities.$cap = '${v ?: "<unset>"}' (expected '$target')")
                if (target !in entries) add("'$target': entry missing")
            }
        }
        if (problems.isEmpty()) {
            pendingCapDirectVerify.set(null)
            ctx.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)
                .edit().putBoolean(PREF_MIGRATED_CAPABILITY_DIRECT, true).apply()
            Log.i(TAG, "capability-direct migration: verified ${plan.size} cap(s), marked done ($PREF_MIGRATED_CAPABILITY_DIRECT)")
        } else {
            Log.e(TAG, "capability-direct migration: verification not passed (${problems.size}): ${problems.joinToString("; ")}")
        }
    }

    private fun applySnapshot(session: JSONObject?) {
        if (session == null) return
        // 会话快照重挂（启动/打开会话）：在途附件提交已失去落点，静默终结
        // （accepted=false；composer 草稿保留，但不再等待回显）。
        dropPendingEcho()
        // 快照内嵌 run 键（协议 run_id）：镜像仲裁用它，不读 FFI 实时态——
        // 快照在泵协程 FIFO 内到达，键与本快照的 entries 天然同源同代。
        // optStringOrNull 防 JSON null→"null" 字面量（新会话快照 run_id
        // 合法为 null——run 懒创建；旧写法曾把镜像键污染成 "null"）。
        lastKnownRunId = session.optStringOrNull("run_id")
        val transcript = session.optJSONArray("transcript") ?: return
        // ask_user 恢复关联表：tool_call 条目的 call_id（仅 DB 重建条目携带，
        // live 条目恒缺省）→ fullArgs。pending_questions 回放按 tool_call_id
        // 取 argsKey——QuestionCardHost 凭 argsKey==fullArgs 判定出交互卡。
        val askArgsByCallId = mutableMapOf<String, String>()
        val entries = buildList {
            for (i in 0 until transcript.length()) {
                val e = transcript.optJSONObject(i) ?: continue
                when (e.optString("kind")) {
                    // attachments（v4 多模态）：缺键/旧快照 → 空表。
                    "user" -> add(
                        UiEntry.User(
                            e.optString("text"),
                            parseAttachmentRefs(e.optJSONArray("attachments")),
                        ),
                    )
                    "assistant" -> add(UiEntry.Assistant(e.optString("text")))
                    "reasoning" -> add(UiEntry.Reasoning(e.optString("text")))
                    "meta" -> add(UiEntry.Meta(e.optString("text")))
                    "tool_call" -> {
                        val statusObj = e.optJSONObject("status")
                        val detail = e.optJSONObject("detail")
                        val name = e.optString("name")
                        val fullArgs = detail?.optString("args") ?: e.optString("args")
                        // call_id（live 事件 / DB 重建条目携带；旧快照无 → null，
                        // 详情弹窗跳过懒加载维持现状显示）。
                        val callId = e.optString("call_id").ifBlank { null }
                        if (name == ASK_USER_TOOL && callId != null) {
                            askArgsByCallId[callId] = fullArgs
                        }
                        add(
                            UiEntry.ToolCall(
                                name = name,
                                argsPreview = e.optString("args"),
                                status = if (statusObj != null) {
                                    when (statusObj.optString("state", statusObj.optString("type", "done"))) {
                                        "running" -> "running"; "failed" -> "failed"; else -> "done"
                                    }
                                } else "done",
                                fullArgs = fullArgs,
                                output = detail?.takeIf { it.has("output") && !it.isNull("output") }?.optString("output"),
                                callId = callId,
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
        // ask_user pending 卡：live 卡跨 snapshot 保留（同会话刷新，running
        // 门控——host call 阻塞 turn，非 running 快照不可能有活问卷）；无
        // live 卡时回放 snapshot 的 pending_questions（重启恢复，**与 running
        // 无关**——恢复场景 running 恒 false，迟到答案走 question_reply 上行，
        // 见 [submitRestoredQuestionAnswer]。新 host call 到达时 handleAskUser
        // 无条件覆写单卡槽，恢复卡自然让位）。
        val livePending = _state.value.pendingQuestion.takeIf { session.optBoolean("running") }
        val restoredPending = if (livePending == null) {
            buildRestoredPendingQuestion(session.optJSONArray("pending_questions"), askArgsByCallId)
        } else {
            null
        }
        restoredPending?.let {
            Log.i(
                TAG,
                "ask_user: restored pending card (id=${it.requestId}, ${it.questions.size} question(s)" +
                    ", argsKey hit=${it.argsKey.isNotEmpty()})",
            )
        }
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
                val summary = it.optJSONObject("summary")
                PendingApprovalUi(
                    id = it.optLong("id"),
                    toolName = summary?.optString("tool_name") ?: "?",
                    arguments = summary?.optString("arguments").orEmpty(),
                    riskLevel = summary?.optString("risk_level") ?: "?",
                    server = summary?.optStringOrNull("server"),
                    trigger = summary?.optStringOrNull("trigger"),
                    note = summary?.optStringOrNull("note"),
                )
            },
            // ask_user pending 卡：live 卡优先（running 门控），否则回放
            // pending_questions（重启恢复，与 running 无关，见上方注释）。
            pendingQuestion = livePending ?: restoredPending,
        )
        // 本地 transcript（含思维链/⚡meta）按 durable 仲裁尝试覆盖镜像版
        //（DB 含恢复条目时不再让旧文件遮蔽，见 shouldRestoreLocalTranscript）。
        // 键 = 本快照的 run_id（与上方 entries 同代，泵 FIFO 内一致）。
        tryRestoreLocalTranscript(lastKnownRunId)
        // 模型条目键迁移第二段：config 已入 state → 完成校验（幂等，无待
        // 校验则 no-op；先于第一段，首个 snapshot 时待校验必为空）。
        verifyModelEntryMigration()
        verifyCapabilityDirectMigration()
        // 模型条目键迁移第一段（legacy `p__m` slug → `p/m`；prefs 防重跑，
        // 挂载点选在此处 = runtime 就绪且 config 已入 state 的最早时刻）。
        maybeMigrateModelEntrySlash()
        // capabilities 直绑迁移（代号引用 → 条目直绑 + main 显式落点）：
        // 仅在条目键迁移已完成时启动，否则等其校验通过的链式调用。
        maybeMigrateCapabilityDirect()
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
     * host call 处理：mobile.ping 直答；termux 后端工具（HostTool 兜底
     * 壳）经 RUN_COMMAND intent 在 Termux 执行、结果 PendingIntent 回传。
     * 工具名形态：termux.run（历史）/ termux_bash（termux 后端恒注册，
     * MCP 离线时的 RUN_COMMAND 兜底名）/ bash（旧「仅 termux」形态的
     * HostTool 顶替名——并行契约下不再产生，防御性保留；MCP 接管后
     * 同名工具走 MCP 路径，不会到这）。mcp.recover_host = MCP host 死亡自愈通道（Rust
     * 装饰器发起）。其余显式 unsupported。
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
            "termux.run", "termux_bash", "bash" ->
                runInTermux(runtime, id, tool, obj.optJSONObject("args")?.optString("command") ?: "")
            "ask_user" -> handleAskUser(id, obj)
            "mcp.recover_host" -> handleMcpRecoverHost(runtime, id, obj)
            else -> runtime.resolveHostCall(
                id.toULong(), false,
                "host tool '$tool' not implemented in Phase 1",
            )
        }
    }

    /**
     * termux bash（HostTool 兜底形态）：TermuxExec（RUN_COMMAND + 结果
     * PendingIntent 本机直传）。无 PC、无 adb、无局域网中继；失败原因随
     * 错误返回供模型自纠。`toolName` 用于输出配对（tool_end 事件按注册
     * 名取数——仅选 termux 时注册名是 bash 而非 termux_bash）。
     */
    private fun runInTermux(runtime: OpenSlateRuntime, id: Long, toolName: String, command: String) {
        val ctx = appContext
        if (ctx == null) {
            runtime.resolveHostCall(id.toULong(), false, "termux.run: no app context")
            return
        }
        scope.launch(Dispatchers.IO) {
            // stall 看门狗（首发与重发共用，见 armTermuxStallWatchdog）。
            val watchdog = armTermuxStallWatchdog(id)
            try {
                val (output, error) = TermuxExec.run(ctx, id, command)
                if (output != null) {
                    recordToolOutput(toolName, output)
                    runtime.resolveHostCall(
                        id.toULong(), true,
                        JSONObject().put("output", output).put("command", command).toString(),
                    )
                } else {
                    recordToolOutput(toolName, error)
                    runtime.resolveHostCall(id.toULong(), false, error)
                }
            } finally {
                watchdog.cancel()
                // 结果到达/取消/异常都退出 stall；仅当没有其他并发 termux
                // 调用仍处 stall 时才熄灭面板（不能被先完成的那次误清）。
                termuxStallMutex.withLock {
                    termuxStalledCalls.remove(id)
                    if (termuxStalledCalls.isEmpty()) {
                        mutate { copy(termuxStallSince = null) }
                    }
                }
            }
        }
    }

    /**
     * mcp.recover_host：MCP host 死亡自愈的宿主侧阶梯（Rust 装饰器
     * RecoveringMcpTool 在 MCP 工具连接失败签名后单飞发起）。
     *
     * 阶梯：点亮恢复面板（mcpRecovering）→ hostAliveDetail 探活 →
     * 不活则确保 Termux 通道（probeTermux；通道死先 startTermuxApp
     * 前台拉起再复检，参照 prewarm 的复检节奏）→ restartHost（kill 旧
     * 进程 + nohup 拉起 + 端口轮询）→ reloadMcpServers 热换工具 →
     * resolve(true, {reloaded, message})。彻底失败（Termux 未装 /
     * host 未安装 / 通道无应答 / reload 失败）resolve(false, reason)，
     * Rust 侧把原因并进工具错误（模型可见，非死端）。
     *
     * 单飞（mcpRecoverMutex，与 Rust 侧单飞闩互补）：恢复中再来的请求
     * 排队等同一轮完成后只做轻量复检（host 在线即 resolve ok）直接
     * resolve，不重走拉起阶梯。全程 IO 线程；恢复面板在 finally 收起。
     */
    private fun handleMcpRecoverHost(runtime: OpenSlateRuntime, id: Long, obj: JSONObject) {
        val ctx = appContext
        if (ctx == null) {
            runtime.resolveHostCall(id.toULong(), false, "mcp.recover_host: no app context")
            return
        }
        val failingTool = obj.optJSONObject("args")?.optString("failing_tool").orEmpty()
        scope.launch(Dispatchers.IO) {
            val entryRound = mcpRecoverMutex.withLock { mcpRecoverRound }
            val (ok, message) = mcpRecoverMutex.withLock {
                val sharedRound = mcpRecoverRound > entryRound
                runMcpRecoveryLadder(ctx, failingTool, sharedRound)
                    .also { mcpRecoverRound += 1 }
            }
            if (ok) {
                runtime.resolveHostCall(
                    id.toULong(), true,
                    JSONObject().put("reloaded", true).put("message", message).toString(),
                )
            } else {
                runtime.resolveHostCall(id.toULong(), false, message)
            }
        }
    }

    /**
     * 恢复阶梯本体（IO；mcpRecoverMutex 内调用）。返回 (ok, message)；
     * ok=true 时 Rust 装饰器将经注册表无感重发原工具调用。面板点亮/
     * 收起在本函数 try/finally 内闭环（失败也收起——原因已并入工具
     * 错误，模型与工具卡片可见）。
     */
    private suspend fun runMcpRecoveryLadder(
        ctx: Context,
        failingTool: String,
        sharedRound: Boolean,
    ): Pair<Boolean, String> {
        val appCtx = ctx.applicationContext
        mutate { copy(mcpRecovering = System.currentTimeMillis()) }
        try {
            if (sharedRound) {
                // 排队期间已有一轮完成：注册表已被那轮 reload 刷新（或那轮
                // 已失败），轻量复检后直接 resolve，不重走拉起阶梯。
                val (alive, detail) = McpHostManager.hostAliveDetail(appCtx)
                return if (alive) {
                    Log.i(TAG, "mcp.recover_host: 共享恢复轮（host 在线，$detail）")
                    true to "shared recovery round (host alive, $detail)"
                } else {
                    false to "MCP host 仍不在线（$detail）；上一轮恢复未奏效"
                }
            }
            _mcpRecoverPhase.value = McpRecoverPhase(
                McpRecoverStep.PROBE,
                if (failingTool.isEmpty()) "探活 MCP host…"
                else "探活 MCP host（${failingTool} 调用失败触发）…",
            )
            val (alive, detail) = McpHostManager.hostAliveDetail(appCtx)
            if (alive) {
                Log.i(TAG, "mcp.recover_host: host 在线（$detail），直接热重载")
            } else {
                Log.i(TAG, "mcp.recover_host: host 不在线（$detail），进入恢复阶梯")
                // ── 阶梯 1：Termux 运行 ──
                _mcpRecoverPhase.value = McpRecoverPhase(McpRecoverStep.TERMUX, "确认 Termux 通道…")
                if (!TermuxExec.isTermuxInstalled(appCtx)) {
                    return false to "Termux 未安装：MCP host 依赖 Termux 运行（设置页 → MCP 一键安装）"
                }
                var probe = withTimeoutOrNull(20_000) { McpHostManager.probeTermux(appCtx) }
                    ?: Triple(false, false, "通道 20s 无应答")
                if (!probe.first) {
                    // 通道死：先拉起 Termux 应用（前台冷启动最稳；RUN_COMMAND
                    // 在进程被杀但非 stopped 时也能自动冷启，这里统一先拉前台
                    // 再复检一次）。
                    McpHostManager.startTermuxApp(appCtx)
                    probe = withTimeoutOrNull(20_000) { McpHostManager.probeTermux(appCtx) }
                        ?: Triple(false, false, "拉起后通道仍 20s 无应答")
                }
                val (allowed, hostInstalled, probeDetail) = probe
                if (!allowed) {
                    return false to "Termux 通道无应答（$probeDetail）：请手动打开 Termux 后重试"
                }
                if (!hostInstalled) {
                    return false to "MCP host 未安装（~/.openslate/mcp-host 缺失）：请到设置页「MCP」一键安装"
                }
                // ── 阶梯 2：host 进程 ──
                _mcpRecoverPhase.value = McpRecoverPhase(McpRecoverStep.HOST, "重启 MCP host…")
                try {
                    McpHostManager.restartHost(appCtx) { Log.i(TAG, "mcp.recover_host｜$it") }
                } catch (t: Throwable) {
                    return false to "host 重启失败：${t.message ?: t.javaClass.simpleName}"
                }
            }
            // ── 阶梯 3：工具注册（热重载）──
            _mcpRecoverPhase.value = McpRecoverPhase(McpRecoverStep.RELOAD, "热重载 MCP 工具…")
            val reload = reloadMcpServers()
                ?: return false to "runtime 未就绪，无法热重载 MCP 工具"
            val envelope = runCatching { JSONObject(reload) }.getOrNull()
            return if (envelope?.optBoolean("ok", false) == true) {
                val tools = envelope.optInt("tools", 0)
                Log.i(TAG, "mcp.recover_host: 恢复完成，重载 $tools 个工具")
                true to "host 已恢复，$tools 个 MCP 工具已重载"
            } else {
                false to "热重载失败：${envelope?.optString("error")?.take(200) ?: reload.take(200)}"
            }
        } finally {
            _mcpRecoverPhase.value = McpRecoverPhase()
            mutate { copy(mcpRecovering = null) }
        }
    }

    /**
     * termux stall 看门狗：命令发出 [TERMUX_STALL_WATCHDOG_MS] 仍无应答
     * （Termux 未启动/被冻结时结果 PendingIntent 永不回传）→ 记入 stall
     * 集合、标记「等待 Termux 连接」模式（豁免 TermuxExec 90s 计时，等
     * 连接后续跑）并点亮连接面板（termuxStallSince）。首发与重发共用：
     * 重发后命令再 stall（Ready 误判，进程起了又死）→ 回到等待模式，
     * 等待下一次连接事件。
     */
    private fun armTermuxStallWatchdog(id: Long): Job = scope.launch {
        delay(TERMUX_STALL_WATCHDOG_MS)
        termuxStallMutex.withLock {
            // 锁内双检 isPending：结果可能在 delay 期间已到达。
            if (TermuxExec.isPending(id)) {
                termuxStalledCalls.add(id)
                TermuxExec.setWaitingConnection(id, true)
                mutate {
                    val now = System.currentTimeMillis()
                    // coerceAtMost：并发多次 stall 保持最早时刻（已等待
                    // 秒数从首条停滞起算）。
                    copy(termuxStallSince = termuxStallSince?.coerceAtMost(now) ?: now)
                }
            }
        }
    }

    /**
     * Termux 连接恢复入口（TermuxStallTracker 探测确认 Ready 时回调；
     * 停滞面板的周期重探 / ON_RESUME 重探驱动）。结果回传走既有
     * `complete(id)` → 本类 runInTermux 挂起点 → `resolveHostCall` 链路，
     * 零新增回填路径。
     *
     * @param resend true = 探测阶梯观察到过通道中断（Termux 真死过）→
     *   宽限 [TERMUX_RESEND_GRACE_MS] 后对 stalled 集合里**仍无应答**的
     *   id 用原 id 重发 RUN_COMMAND 续跑（每 id 每次连接事件至多一次）；
     *   false = 未见中断（慢命令误报）→ 仅解除等待豁免、恢复正常计时，
     *   不重发（防命令重复执行）。
     */
    internal fun onTermuxConnectionRestored(resend: Boolean) {
        val ctx = appContext ?: return
        scope.launch(Dispatchers.IO) {
            if (!resend) {
                termuxStallMutex.withLock {
                    termuxStalledCalls.forEach { TermuxExec.setWaitingConnection(it, false) }
                    // 无重发即无重发看门狗：不会再 stall 重亮，顺手熄灭
                    // stallSince（面板条件失效 + 聊天页周期重探停摆；各 id
                    // 完成时 finally 幂等收尾）。
                    if (termuxStalledCalls.isNotEmpty()) {
                        mutate { copy(termuxStallSince = null) }
                    }
                }
                return@launch
            }
            // 宽限：用户打开 Termux 的瞬间，停止态期间排队的原始命令可能
            // 正在补跑——先等结果落地，只重发仍无应答的（防重复执行）。
            delay(TERMUX_RESEND_GRACE_MS)
            val resended = mutableListOf<Long>()
            termuxStallMutex.withLock {
                for (id in termuxStalledCalls.toList()) {
                    // isPending 双检：结果可能已在宽限期内到达。
                    if (TermuxExec.isPending(id) && TermuxExec.resend(ctx, id)) {
                        // 解除等待豁免：awaitResult 重起完整 90s 窗口。
                        TermuxExec.setWaitingConnection(id, false)
                        resended += id
                    }
                }
                // 重发即消费本次连接事件：清出等待集合（防同事件重复重发，
                // 再 stall 由重发的看门狗重新标记）；集合清空时熄灭面板。
                termuxStalledCalls.removeAll(resended.toSet())
                if (termuxStalledCalls.isEmpty()) {
                    mutate { copy(termuxStallSince = null) }
                }
            }
            // 重发后重武装看门狗（锁外启动，launch 不持锁等待）。
            resended.forEach { armTermuxStallWatchdog(it) }
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

    // ── Provider 模型自动检测（设置页 Provider 域）──────────────

    /** 检测结果：ok=false 时 error 带可直接展示的中文文案。 */
    data class ModelsProbe(
        val ok: Boolean,
        val models: List<String> = emptyList(),
        val error: String? = null,
    )

    /** 拉取 provider 的可用模型清单（Rust 侧 10s 超时；必须 IO 线程调用）。 */
    fun listProviderModels(provider: String): ModelsProbe {
        val runtime = runtimeRef.get() ?: return ModelsProbe(false, error = "运行时未启动")
        return runCatching {
            val o = JSONObject(runtime.listProviderModels(provider))
            if (o.optBoolean("ok")) {
                val arr = o.optJSONArray("models") ?: org.json.JSONArray()
                ModelsProbe(true, List(arr.length()) { arr.optString(it) })
            } else {
                ModelsProbe(false, error = o.optString("error", "检测失败"))
            }
        }.getOrElse { ModelsProbe(false, error = it.message ?: it.javaClass.simpleName) }
    }

    // ── 模型元数据注册表（数据源页面 / 元数据自动补全）──────────

    /** 数据源列表（含本地状态）。IO 线程调用。 */
    fun registrySources(): List<RegistrySourceUi> {
        val runtime = runtimeRef.get() ?: return emptyList()
        return runCatching {
            val arr = JSONObject(runtime.registrySources()).optJSONArray("sources")
                ?: return emptyList()
            List(arr.length()) { i ->
                val o = arr.getJSONObject(i)
                RegistrySourceUi(
                    id = o.optString("id"),
                    name = o.optString("name"),
                    desc = o.optString("desc"),
                    url = o.optString("url"),
                    fetchedAtMs = if (o.isNull("fetched_at_ms")) null else o.optLong("fetched_at_ms"),
                    entries = if (o.isNull("entries")) null else o.optInt("entries"),
                    zstBytes = if (o.isNull("zst_bytes")) null else o.optLong("zst_bytes"),
                    staleAfterHrs = o.optLong("stale_after_hrs"),
                )
            }
        }.getOrDefault(emptyList())
    }

    /** 手动更新一个数据源。IO 线程调用（阻塞至完成，最长 120s）。 */
    fun updateRegistrySource(id: String): ModelsProbe {
        val runtime = runtimeRef.get() ?: return ModelsProbe(false, error = "运行时未启动")
        return runCatching {
            val o = JSONObject(runtime.registryUpdateSource(id))
            if (o.optBoolean("ok")) {
                ModelsProbe(true, models = listOf(o.optInt("entries").toString()))
            } else {
                ModelsProbe(false, error = o.optString("error", "更新失败"))
            }
        }.getOrElse { ModelsProbe(false, error = it.message ?: it.javaClass.simpleName) }
    }

    /** 搜索/浏览 registry 条目。sourceId 空 = 跨全部本地已缓存源；
     *  query 空 = 浏览前 limit 条（键字典序）。IO 线程调用。 */
    fun registrySearch(sourceId: String, query: String, limit: Int): RegistrySearchUi {
        val runtime = runtimeRef.get() ?: return RegistrySearchUi(0, emptyList())
        return runCatching {
            val o = JSONObject(runtime.registrySearch(sourceId, query, limit))
            val arr = o.optJSONArray("results")
            val list = if (arr == null) {
                emptyList()
            } else {
                List(arr.length()) { i ->
                    val e = arr.getJSONObject(i)
                    RegistryEntryUi(
                        source = e.optString("source"),
                        sourceId = e.optString("sourceId"),
                        id = e.optString("id"),
                        ctx = if (e.isNull("ctx")) null else e.optLong("ctx"),
                        out = if (e.isNull("out")) null else e.optLong("out"),
                        vision = if (e.isNull("vision")) null else e.optBoolean("vision"),
                        reasoning = if (e.isNull("reasoning")) null else e.optBoolean("reasoning"),
                        tool = if (e.isNull("tool")) null else e.optBoolean("tool"),
                        priceIn = if (e.isNull("priceIn")) null else e.optDouble("priceIn"),
                        priceOut = if (e.isNull("priceOut")) null else e.optDouble("priceOut"),
                    )
                }
            }
            RegistrySearchUi(o.optInt("total"), list)
        }.getOrDefault(RegistrySearchUi(0, emptyList()))
    }

    /** 模型元数据查询（本地优先，miss 在线兜底）。IO 线程调用。 */
    fun lookupModelMeta(modelId: String): ModelMetaUi? {
        val runtime = runtimeRef.get() ?: return null
        return runCatching {
            val o = JSONObject(runtime.lookupModelMeta(modelId))
            fun bl(k: String) = if (o.isNull(k)) null else o.optBoolean(k)
            fun ln(k: String) = if (o.isNull(k)) null else o.optLong(k)
            fun db(k: String) = if (o.isNull(k)) null else o.optDouble(k)
            ModelMetaUi(
                displayName = o.optString("display_name").ifEmpty { null },
                contextTokens = ln("context_tokens"),
                maxOutputTokens = ln("max_output_tokens"),
                supportsVision = bl("supports_vision"),
                supportsReasoning = bl("supports_reasoning"),
                supportsToolCall = bl("supports_tool_call"),
                inputPricePerMtok = db("input_price_per_mtok"),
                outputPricePerMtok = db("output_price_per_mtok"),
                source = o.optString("source").ifEmpty { null },
            )
        }.getOrNull()
    }

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
                            // v5 聚合列；存量 run 无 meta → 0（UI 省略显示）。
                            inputTokens = o.optLong("input_tokens", 0),
                            outputTokens = o.optLong("output_tokens", 0),
                        ),
                    )
                }
            }
        }.getOrDefault(emptyList())
    }

    /**
     * import_attachment FFI 结果投影（JSON {ok,id,mime,filename,size_bytes,
     * orig_width?,orig_height?,orig_bytes?,width?,height?,processed?} |
     * {ok:false,error}）。
     */
    sealed interface AttachmentImportUi {
        data class Ok(
            val id: String,
            /** 归一化后 mime（webp/gif→jpeg 等；UI 判类型用它）。 */
            val mime: String,
            val filename: String?,
            val sizeBytes: Long,
            /**
             * 处理对比（fix-11 可选契约——旧版 Rust 响应无这些键）：原图
             * 尺寸/字节与处理后尺寸。缺键 / 类型异常 optInt/optLong 均落
             * -1 哨兵收敛 null，如实透传由 UI 侧组装（全键齐备才出对比块）。
             */
            val origWidth: Int? = null,
            val origHeight: Int? = null,
            val origBytes: Long? = null,
            val width: Int? = null,
            val height: Int? = null,
            /** passthrough | transcode | video_passthrough；缺键/null/空串 → null。 */
            val processed: String? = null,
        ) : AttachmentImportUi

        /** error 文案可直接展示（不识别格式/图过大/视频过大/落盘失败）。 */
        data class Failed(val message: String) : AttachmentImportUi
    }

    /**
     * 导入附件（IO 线程调用——FFI 同步签名内部 block_on，registry 先例）。
     * 调用方（ui/ComposerAttachments.kt）先完成拷贝与 HEIC 预转码，这里只
     * 做薄包装：runtime 取用 + JSON 解析 + 异常收敛为 Failed。
     *
     * TODO(binding)：经反射调用 `importAttachment`——并行 lane 重跑
     * uniffi-gen 的窗口内，工作树 binding 可能短暂缺该方法（2026-10-21 实
     * 发：L6 再生成时 L4 导出被截掉，orchestrator 最终集成重跑后恢复）。
     * 反射让 binding 就位即生效、零代码改动；binding 完整后可换直调。
     */
    fun importAttachment(sourcePath: String, declaredMime: String, filename: String): AttachmentImportUi {
        val runtime = runtimeRef.get() ?: return AttachmentImportUi.Failed("运行时未就绪")
        val json = runCatching {
            val method = runCatching {
                runtime.javaClass.getMethod(
                    "importAttachment",
                    String::class.java, String::class.java, String::class.java,
                )
            }.getOrNull()
            if (method == null) {
                Log.w(TAG, "importAttachment: binding not generated yet (uniffi-gen pending)")
                return AttachmentImportUi.Failed("附件功能加载中（binding 未生成），请更新安装包")
            }
            method.invoke(runtime, sourcePath, declaredMime, filename) as? String
        }.getOrElse {
            Log.e(TAG, "importAttachment failed", it)
            null
        } ?: return AttachmentImportUi.Failed("导入失败")
        return runCatching {
            val o = JSONObject(json)
            if (o.optBoolean("ok", false)) {
                AttachmentImportUi.Ok(
                    id = o.optString("id"),
                    mime = o.optString("mime"),
                    filename = o.optStringOrNull("filename"),
                    sizeBytes = o.optLong("size_bytes", 0),
                    // fix-11 处理对比（可选键，旧响应无）：-1 哨兵收敛 null，
                    // processed 缺键/null/空串经 optStringOrNull 收敛 null。
                    origWidth = o.optInt("orig_width", -1).takeIf { it >= 0 },
                    origHeight = o.optInt("orig_height", -1).takeIf { it >= 0 },
                    origBytes = o.optLong("orig_bytes", -1L).takeIf { it >= 0 },
                    width = o.optInt("width", -1).takeIf { it >= 0 },
                    height = o.optInt("height", -1).takeIf { it >= 0 },
                    processed = o.optStringOrNull("processed"),
                )
            } else {
                AttachmentImportUi.Failed(o.optString("error", "导入失败"))
            }
        }.getOrElse {
            Log.e(TAG, "importAttachment response malformed: ${json.take(160)}", it)
            AttachmentImportUi.Failed("导入失败：响应异常")
        }
    }

    // ── 附件缩略图（transcript 只读 chips 预览；反射，同 importAttachment 模式）──

    /**
     * 附件缩略图字节（JPEG；IO 线程调用——FFI 同步签名）。null = runtime
     * 未就绪 / binding 未生成 / 附件无缩略图 / 响应异常，调用方
     * （ui/AttachmentThumbnails.kt）一律回退图标占位并记 missing 不重试。
     *
     * TODO(binding)：经反射调用 `getAttachmentThumbnail`——方法正由并行
     * lane 添加，同 [importAttachment] 模式：binding 就位即生效、零代码
     * 改动；binding 完整后可换直调。
     */
    fun attachmentThumbnail(id: String, maxPx: Int): ByteArray? {
        val runtime = runtimeRef.get() ?: return null
        val json = runCatching {
            val method = runCatching {
                runtime.javaClass.getMethod(
                    "getAttachmentThumbnail",
                    String::class.java, Int::class.java,
                )
            }.getOrNull()
            if (method == null) {
                Log.w(TAG, "getAttachmentThumbnail: binding not generated yet (uniffi-gen pending)")
                return null
            }
            method.invoke(runtime, id, maxPx) as? String
        }.getOrElse {
            Log.w(TAG, "getAttachmentThumbnail failed", it)
            null
        } ?: return null
        // 契约：{"ok":true,"data":"<base64 jpeg>","mime":"image/jpeg"} | {"ok":false}。
        return runCatching {
            val o = JSONObject(json)
            if (!o.optBoolean("ok", false)) return@runCatching null
            android.util.Base64.decode(o.optString("data"), android.util.Base64.DEFAULT)
        }.getOrElse {
            Log.w(TAG, "getAttachmentThumbnail response malformed: ${json.take(120)}", it)
            null
        }
    }

    // ── 实验性视频输入开关（设计 §3.5/§6.6；L6 FFI 已就位，直调）──

    /** 读开关（runtime 未就绪 → false，与默认值一致；FFI 轻量读配置）。 */
    fun experimentalVideoEnabled(): Boolean {
        val runtime = runtimeRef.get() ?: return false
        return runCatching { runtime.getExperimentalVideo() }
            .onFailure { Log.w(TAG, "getExperimentalVideo failed", it) }
            .getOrDefault(false)
    }

    /** 写开关（Rust 落 openslate.toml 并即时生效）。false = 写失败，调用方回滚 UI。 */
    fun setExperimentalVideoEnabled(enabled: Boolean): Boolean {
        val runtime = runtimeRef.get() ?: return false
        return runCatching {
            val o = JSONObject(runtime.setExperimentalVideo(enabled))
            o.optBoolean("ok", false)
        }.onFailure { Log.w(TAG, "setExperimentalVideo failed", it) }
            .getOrDefault(false)
    }

    // ── 图片处理限制（设置页「处理图片」；fix-12 FFI 直调——2026-10-21
    // 17:07 binding 已含 getImageProcessing/setImageProcessing 导出，反射
    // 网关退役）──

    /** 处理限制投影（getImageProcessing 响应字段）。 */
    data class ImageProcessingUi(val maxDim: Long, val maxMb: Long)

    /** setImageProcessing 写结果（[Ok] | [Failed]——error 文案可直接展示）。 */
    sealed interface ImageProcessingWriteUi {
        object Ok : ImageProcessingWriteUi
        data class Failed(val message: String) : ImageProcessingWriteUi
    }

    /**
     * 读处理限制（`getImageProcessing` → `{"max_image_dim":2048,"max_image_mb":5}`）。
     * 返回 null = runtime 未就绪 / 响应异常（调用方显示「需更新应用」占位）。
     */
    fun imageProcessing(): ImageProcessingUi? {
        val runtime = runtimeRef.get() ?: return null
        return runCatching {
            val o = JSONObject(runtime.getImageProcessing())
            val maxDim = o.optLong("max_image_dim", -1L).takeIf { it >= 0 }
                ?: return@runCatching null
            val maxMb = o.optLong("max_image_mb", -1L).takeIf { it >= 0 }
                ?: return@runCatching null
            ImageProcessingUi(maxDim, maxMb)
        }.onFailure { Log.w(TAG, "getImageProcessing failed", it) }
            .getOrNull()
    }

    /**
     * 写处理限制（`setImageProcessing(maxDim, maxMb)` → `{"ok":true}` |
     * `{"ok":false,"error":"…"}`，非法值 Rust 返回 error）。[Failed] 的
     * message = FFI error / 运行时未就绪，调用方 snackbar 展示 + 乐观值
     * 回滚。
     */
    fun setImageProcessing(maxDim: Long, maxMb: Long): ImageProcessingWriteUi {
        val runtime = runtimeRef.get() ?: return ImageProcessingWriteUi.Failed("运行时未就绪")
        return runCatching {
            val o = JSONObject(runtime.setImageProcessing(maxDim, maxMb))
            if (o.optBoolean("ok", false)) ImageProcessingWriteUi.Ok
            else ImageProcessingWriteUi.Failed(o.optString("error", "保存失败"))
        }.onFailure { Log.w(TAG, "setImageProcessing failed", it) }
            .getOrDefault(ImageProcessingWriteUi.Failed("保存失败"))
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
        // "null"/空串防御：org.json optString 陷阱曾把镜像键污染成 "null"
        //（JSON null → 字面量），读写一概拒绝该键。
        if (runId.isBlank() || runId == "null") null
        else appContext?.let { java.io.File(java.io.File(it.filesDir, "transcripts"), "$runId.json") }

    /**
     * turn 结束后落盘当前聊天流（后台线程；tmp+rename 原子替换，
     *  进程中途被杀不留截断 JSON）。
     *
     *  run 键（C4）：由调用点从事件流内嵌值解析——turn_ok 取
     *  summary.run_id，turn_error 兜底 [lastKnownRunId]（applySnapshot 写入）。
     *  FIFO 正确性：open_session 有 busy 守卫，引擎在 core 锁内置
     *  running=false 并广播 TurnOk（落镜像先于广播），事件入队序 =
     *  TurnOk(旧run) → Snapshot(新run)；本函数与 applySnapshot 同在
     *  单泵协程，处理 turn_ok 时 lastKnownRunId 仍是旧 run 且
     *  _state.entries 属旧 run，两侧一致。旧 bug 根源是
     *  runtime.currentRunId() 绕过 FIFO 直读 Rust 实时态（open_session
     *  在 Dispatchers.IO 并发轮换 run 时读到新 run 键 + 旧 entries）。
     *  取不到键 → 不落盘（宁缺勿错：错键文件会污染新会话的镜像仲裁）。
     */
    private fun persistTranscriptAsync(runId: String?) {
        if (runId.isNullOrBlank()) return
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
     * snapshot 到达后与本地 transcript 仲裁（重启恢复场景 DB 镜像为权威）。
     * 旧规则「条目总数多者胜」有洞：本地文件只落盘到上个**完成** turn，
     * 但它多出的装饰条目（⚡meta / step）能让**旧**文件在总数上压过含
     * 中断 turn 条目（悬挂 user / ask_user 调用）的 DB 镜像、整体覆盖抹掉
     * 恢复条目。新规则见 [shouldRestoreLocalTranscript]：本地须在 durable
     * 条目（两侧同源持久化的种类）上不落后才允许以其更丰富的装饰覆盖；
     * DB 转写为空时本地兜底。live 会话的落盘逻辑（turn 结束写文件）不变。
     * 本函数只被 applySnapshot 末尾调用，比较时两者均已可见。
     *
     * run 键（C4）：改为 applySnapshot 传入的**快照 run_id**（泵协程 FIFO
     * 内与本快照 entries 同代）。旧实现读 runtime.currentRunId() 是 FFI
     * 旁路直读实时态——open_session（Dispatchers.IO）并发换 run 时，仲裁
     * 两侧可能分属两个会话（read-skew）。键缺失（旧引擎快照）→ 不恢复。
     */
    private fun tryRestoreLocalTranscript(snapshotRunId: String?) {
        val runId = snapshotRunId ?: return
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
            val memDurable = durableUiEntryCount(_state.value.entries)
            val localDurable = localTranscriptDurableCount(entriesArr)
            if (!shouldRestoreLocalTranscript(localCount, localDurable, memCount, memDurable)) {
                Log.d(
                    TAG,
                    "transcript restore skipped: local=$localCount ($localDurable durable) vs" +
                        " snapshot=$memCount ($memDurable durable)（DB 快照含恢复条目或更新，本地文件不覆盖）",
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

    private fun entryToJson(e: UiEntry): org.json.JSONObject =
        // kotlinx 镜像 DTO 编码（EntryMirrorDto，短键形状与旧手工版本逐字节一致）；
        // org.json JSONObject 在此仅作返回值载体（persistTranscriptAsync 装数组用）。
        org.json.JSONObject(encodeEntryMirror(e))

    private fun entryFromJson(o: org.json.JSONObject): UiEntry =
        // decode 失败（含未知 t）→ StepBreak，对齐旧 when-else 兜底。
        decodeEntryMirror(o.toString()) ?: UiEntry.StepBreak
}
