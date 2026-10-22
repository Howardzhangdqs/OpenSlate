@file:OptIn(ExperimentalSerializationApi::class)
package dev.openslate.mobile.bridge

import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.decodeFromString
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonClassDiscriminator
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.booleanOrNull
import kotlinx.serialization.json.contentOrNull
import kotlinx.serialization.json.decodeFromJsonElement

/**
 * kotlinx.serialization 试点解析层（纯 Kotlin，无 Android 依赖——JVM 单测可直载）。
 *
 * 覆盖三个热点的 wire 契约（事件泵 handleEvent 热路径与其余 org.json 用点**不在**本层）：
 * 1. **ask_user 往返**：下发信封 questions[] 解析（Rust 源：
 *    `crates/openslate-mobile/src/askuser.rs` `NormalizedQuestion::to_envelope`）+
 *    提交 answers[] 拼装（resolve 明文 / display 脱敏两份）+ 已答回看；
 * 2. **config 解析**：ConfigViewDto 投影（Rust 源：`crates/openslate-protocol/src/lib.rs`
 *    `ProviderDto` / `ModelDto` / `ApprovalConfigViewDto`）；
 * 3. **本地转写镜像**：8 种 UiEntry 的短键 JSON（t/x/n/a/s/f/o/c/d/m/atts）——
 *    **字节级兼容旧镜像文件**（短键与形状完全不变，旧文件缺新键按默认值）。
 *
 * 行为对齐说明：原 org.json 实现的 opt* 读取对类型偏差有静默强制转换（数字当
 * 字符串等）；本层对**字段类型错误**的行/条目按「跳过该行」处理（decode 失败
 * → runCatching fallback），对**缺键/JSON null** 的容错语义与原先完全一致
 * （缺键 → 默认值；null → null）。wire 数据由 Rust serde 生成、结构恒定型，
 * 类型偏差仅在协议破损时出现，跳过即原防御语义的等价收敛。
 */

/**
 * 统一 Json 实例。关键配置（wire 兼容的根）：
 * - `ignoreUnknownKeys`：协议加新字段（如 ConfigViewDto 后加的 approval/capabilities）
 *   旧客户端安全跳过——对齐 Rust serde 默认容忍未知字段；
 * - `explicitNulls = false`：编码侧 null 字段**跳键不写**（对齐 Rust
 *   `skip_serializing_if = "Option::is_none"`：ProviderDto.title / ModelDto.reasoning_effort /
 *   envelope 的 description/placeholder/recommended）；解码侧显式 null 视同缺键 → 默认值；
 * - `encodeDefaults = false`：默认值字段不输出（镜像紧凑；answers 按分支互斥输出）。
 *   **例外**：必须始终出现的字段（outcome/header/secret 的 name·kind·remember、
 *   镜像短键 o/c）在 DTO 里声明为**无默认必填**或 nullable（null ≠ 默认），绕开该策略；
 * - `coerceInputValues`：解码侧非空字段收到 JSON null 时回退默认值（多一层防御）。
 */
internal val BridgeJson = Json {
    ignoreUnknownKeys = true
    explicitNulls = false
    encodeDefaults = false
    coerceInputValues = true
}

// ── ask_user：下发信封 questions[]（Rust `NormalizedQuestion::to_envelope` 镜像）──

/**
 * secret 落地形态（envelope `storage` 键）：`{kind:"env",name}` 或 `{kind:"file",path}`。
 * 两键按 kind 互斥出现；name/path 可空对齐「另一形态缺键」。
 */
@Serializable
internal data class StorageDto(
    val kind: String = "",
    val name: String? = null,
    val path: String? = null,
)

/** 选项（envelope `options[]`）；description/recommended 仅在有值时序列化（Rust 侧条件写入）。 */
@Serializable
internal data class QuestionOptionDto(
    val label: String = "",
    val description: String = "",
    val recommended: Boolean = false,
)

/**
 * 单题（envelope `questions[]` 项）。字段集 = askuser.rs `to_envelope` 输出并集：
 * id/header/question/type 恒有；options 仅 choice/confirm；multi_select 仅 choice；
 * placeholder 有值才出现；optional 恒有；storage 仅 secret。type 保持原始串
 * （未知值由 UI 按 text 兜底渲染，与旧 parseAskUserQuestions 行为一致）。
 */
@Serializable
internal data class QuestionDto(
    val id: String = "",
    val header: String = "",
    val question: String = "",
    val type: String = "",
    val options: List<QuestionOptionDto> = emptyList(),
    @SerialName("multi_select") val multiSelect: Boolean = false,
    val placeholder: String? = null,
    val optional: Boolean = false,
    val storage: StorageDto? = null,
)

/**
 * 转写脱敏 secret 引用（display answers 的 `secret` 键；原 org.json 实现恒写
 * name/kind/remember 三键——name 无值写 ""、kind 缺省 "env"、remember 恒出现）。
 * name/kind 声明为必填（无默认）+ remember nullable：三者绕开 encodeDefaults=false，
 * 编码时始终输出，与旧实现字段集逐字节一致。
 */
@Serializable
internal data class SecretRefDto(
    val name: String,
    val kind: String,
    val remember: Boolean? = null,
)

/**
 * 单题答案（解码侧：resolve payload 与转写 display output 共用；编码侧：提交拼装）。
 * Rust 消费端（askuser.rs `answer_item` / `answer_line` / `answer_json_item` /
 * `apply_secret_answers`）按 id 定位、逐题识别 selected/value/remember/skipped/declined——
 * 对未携带的分支字段宽容。
 *
 * `secret` 特意放 JsonElement：模型侧结构化 JSON 里是**字符串引用**
 * （`"secret":"env:GITHUB_TOKEN"`）、转写脱敏快照里是**对象**
 * （`{name,kind,remember}`），回看解析只判「在场」不判形状（对齐旧实现的
 * `has("secret")`）；remember 从对象形态读取，其余形状按 false。
 */
@Serializable
internal data class AnswerDto(
    val id: String = "",
    val selected: List<String>? = null,
    val value: String? = null,
    val confirm: Boolean? = null,
    val remember: Boolean? = null,
    val skipped: Boolean = false,
    val declined: Boolean = false,
    val secret: JsonElement? = null,
)

/**
 * 转写脱敏条目（display answers[] 项）：answer 各分支字段 + 恒有的 header。
 * id/header 必填（无默认）保证编码始终输出——旧实现 `put("id",…).put("header",…)` 先写。
 */
@Serializable
internal data class DisplayAnswerDto(
    val id: String,
    val header: String,
    val selected: List<String>? = null,
    val value: String? = null,
    val confirm: Boolean? = null,
    val skipped: Boolean = false,
    val declined: Boolean = false,
    val secret: SecretRefDto? = null,
)

/** resolve payload / 转写 output 的外壳：`{"outcome":"answered","answers":[…]}`。 */
@Serializable
internal data class OutcomeAnswersDto(
    val outcome: String,
    val answers: List<AnswerDto>,
)

@Serializable
internal data class OutcomeDisplayDto(
    val outcome: String,
    val answers: List<DisplayAnswerDto>,
)

/**
 * 迟到答案上行（重启恢复卡提交）：`{"type":"question_reply","request_id":"<id>",
 * "action":"accept","answers":[…]}`——answers 与 live resolve payload 的 answers
 * 数组同构（call-chains.md §3.2）。
 */
@Serializable
internal data class QuestionReplyDto(
    val type: String,
    @SerialName("request_id") val requestId: String,
    val action: String,
    val answers: List<AnswerDto>? = null,
)

// ── ask_user：解析 / 回看 / 提交拼装（纯函数）─────────────────────────

/**
 * questions[] JSON 文本（裸数组）→ DTO 列表。非 JSON / 非数组 → 空表；非对象行
 * 跳过（对齐旧 `optJSONObject(i) ?: continue`）；对象行 decode 失败（字段类型
 * 破损）跳过该行。
 */
internal fun decodeQuestionDtos(json: String?): List<QuestionDto> {
    if (json == null) return emptyList()
    val arr = runCatching { BridgeJson.parseToJsonElement(json) }.getOrNull() as? JsonArray
        ?: return emptyList()
    return questionDtosOf(arr)
}

private fun questionDtosOf(arr: JsonArray): List<QuestionDto> = arr.mapNotNull { el ->
    if (el !is JsonObject) return@mapNotNull null
    runCatching { BridgeJson.decodeFromJsonElement(QuestionDto.serializer(), el) }.getOrNull()
}

/**
 * DTO → UI 投影。index 参与缺省回填（对齐旧实现逐位读取的语义）：
 * id 空 → `q{n}`、header 空 → `问题 {n}`、type 空 → choice；label 空的选项行
 * 丢弃；storage kind 非 env/file 视为无 storage，name/path 按 kind 三态取值
 * （file 的 path 空 → 回退 name，恒得非 null 串——旧 optString 缺键即 ""）。
 */
internal fun QuestionDto.toQuestionUi(index: Int): QuestionUi {
    val s = storage
    return QuestionUi(
        id = id.ifBlank { "q${index + 1}" },
        header = header.ifBlank { "问题 ${index + 1}" },
        question = question,
        type = type.ifBlank { "choice" },
        options = options.asSequence()
            .filter { it.label.isNotEmpty() }
            .map { QuestionOptionUi(it.label, it.description, it.recommended) }
            .toList(),
        multiSelect = multiSelect,
        placeholder = placeholder,
        optional = optional,
        storageKind = s?.kind?.takeIf { it == "env" || it == "file" },
        storageName = when (s?.kind) {
            "env" -> s.name ?: ""
            "file" -> (s.path ?: "").ifBlank { s.name ?: "" }
            else -> null
        },
    )
}

/**
 * 已答问卷回看（原 `buildReviewedQuestion` 纯逻辑）：args 的 questions[] 出题目
 * 结构，output 的 answers[] 按 id 对齐回填只读草稿。五分支判定顺序与旧实现一致：
 * skipped → declined → secret（只留 remember，明文不回填）→ confirm（题型+键双
 * 判）→ selected（命中选项进 selected，未命中的自由文本 = 「其他」说明）→ value
 * → 未知形态留空。无法还原（args 缺失 / 题目空 / 非 answered / answers 空）→ null。
 */
internal fun decodeReviewedQuestion(args: String?, output: String?): ReviewedQuestionUi? {
    val argsObj = args?.let { runCatching { BridgeJson.parseToJsonElement(it) }.getOrNull() as? JsonObject }
    val questionsArr = argsObj?.get("questions") as? JsonArray ?: return null
    val questions = questionDtosOf(questionsArr).mapIndexed { i, dto -> dto.toQuestionUi(i) }
    if (questions.isEmpty()) return null
    val o = output?.let { runCatching { BridgeJson.parseToJsonElement(it) }.getOrNull() as? JsonObject }
        ?: return null
    if ((o["outcome"] as? JsonPrimitive)?.contentOrNull != "answered") return null
    val answersArr = o["answers"] as? JsonArray ?: return null
    if (answersArr.isEmpty()) return null
    val byId = HashMap<String, AnswerDto>()
    for (el in answersArr) {
        if (el !is JsonObject) continue
        val a = runCatching { BridgeJson.decodeFromJsonElement(AnswerDto.serializer(), el) }.getOrNull()
            ?: continue
        byId[a.id] = a
    }
    val drafts = questions.map { q ->
        val a = byId[q.id] ?: return@map QuestionDraft()
        when {
            a.skipped -> QuestionDraft(skipped = true)
            a.declined -> QuestionDraft(declined = true)
            a.secret != null -> QuestionDraft(
                remember = (a.secret as? JsonObject)
                    ?.get("remember")?.let { (it as? JsonPrimitive)?.booleanOrNull } ?: false,
            )
            q.type == "confirm" && a.confirm != null -> QuestionDraft(confirm = a.confirm)
            a.selected != null -> {
                var selected = emptyList<String>()
                var other = ""
                for (s in a.selected) {
                    val t = s.trim()
                    when {
                        t.isEmpty() -> Unit
                        q.options.any { it.label == t } -> selected = selected + t
                        // 未命中选项的自由文本 = 「其他」说明（提交时并入 selected）
                        other.isEmpty() -> other = t
                    }
                }
                QuestionDraft(selected = selected, otherChecked = other.isNotEmpty(), otherText = other)
            }
            a.value != null -> QuestionDraft(text = a.value)
            else -> QuestionDraft() // 未知形态：留空（只读页显示未填状态，不吞题）
        }
    }
    return ReviewedQuestionUi(questions, drafts)
}

/** 提交产物：resolve 明文 payload（FFI 通道）/ 脱敏转写 output / 裸 answers 数组（迟到上行复用）。 */
internal data class AskUserSubmitPayloads(
    val resolveJson: String,
    val displayJson: String,
    val answers: List<AnswerDto>,
)

/**
 * 提交拼装（原 `submitQuestionAnswers` 的双份 JSONArray 手工同步逻辑）。
 * 六分支字段集与旧实现逐键一致：
 * - skipped / declined：`{id,skipped|declined:true}`（display 加 header）；
 * - choice：`{id,selected:[…label]}`——「其他」勾选且文字非空时 trim 后**并入
 *   selected 尾部**（空列表也保留 selected 键，旧实现恒 put）；
 * - secret：resolve `{id,value,remember}`（明文仅 FFI 通道；remember 恒出现——
 *   nullable 显式置值绕开 encodeDefaults）；display `{id,header,secret:{name,kind,remember}}`
 *   （name 空串/kind 缺省 env 与旧 ?: 兜底一致）；
 * - confirm：`{id,confirm:bool,selected:[label]}`（label 取第 0/1 选项，缺省 是/否）；
 * - text/未知：`{id,value:trim()}`。
 */
internal fun buildAskUserSubmitPayloads(
    questions: List<QuestionUi>,
    drafts: List<QuestionDraft>,
): AskUserSubmitPayloads {
    val answers = ArrayList<AnswerDto>(questions.size)
    val display = ArrayList<DisplayAnswerDto>(questions.size)
    for ((i, q) in questions.withIndex()) {
        val d = drafts[i]
        val answer: AnswerDto
        val disp: DisplayAnswerDto
        when {
            d.skipped -> {
                answer = AnswerDto(id = q.id, skipped = true)
                disp = DisplayAnswerDto(id = q.id, header = q.header, skipped = true)
            }
            d.declined -> {
                answer = AnswerDto(id = q.id, declined = true)
                disp = DisplayAnswerDto(id = q.id, header = q.header, declined = true)
            }
            q.type == "choice" -> {
                val sel = d.selected.toMutableList()
                if (d.otherChecked && d.otherText.isNotBlank()) sel.add(d.otherText.trim())
                answer = AnswerDto(id = q.id, selected = sel)
                disp = DisplayAnswerDto(id = q.id, header = q.header, selected = sel)
            }
            q.type == "secret" -> {
                // resolve：明文 + remember（仅 FFI 通道）；转写：仅引用。
                answer = AnswerDto(id = q.id, value = d.text, remember = d.remember)
                disp = DisplayAnswerDto(
                    id = q.id,
                    header = q.header,
                    secret = SecretRefDto(
                        name = q.storageName ?: "",
                        kind = q.storageKind ?: "env",
                        remember = d.remember,
                    ),
                )
            }
            q.type == "confirm" -> {
                val label = if (d.confirm == true) {
                    q.options.getOrNull(0)?.label ?: "是"
                } else {
                    q.options.getOrNull(1)?.label ?: "否"
                }
                answer = AnswerDto(id = q.id, confirm = d.confirm == true, selected = listOf(label))
                disp = DisplayAnswerDto(
                    id = q.id,
                    header = q.header,
                    confirm = d.confirm == true,
                    selected = listOf(label),
                )
            }
            else -> { // text（含未知题型兜底）
                answer = AnswerDto(id = q.id, value = d.text.trim())
                disp = DisplayAnswerDto(id = q.id, header = q.header, value = d.text.trim())
            }
        }
        answers.add(answer)
        display.add(disp)
    }
    return AskUserSubmitPayloads(
        resolveJson = BridgeJson.encodeToString(OutcomeAnswersDto(outcome = "answered", answers = answers)),
        displayJson = BridgeJson.encodeToString(OutcomeDisplayDto(outcome = "answered", answers = display)),
        answers = answers,
    )
}

// ── config：ConfigViewDto 投影（Rust openslate-protocol `ConfigViewDto` 镜像）──

/**
 * `[providers.<name>]` 投影。Rust `ProviderDto`：base_url/api_key_env 恒有；
 * adapter `#[serde(default)]` Option（null 或缺键 → None）；title
 * `skip_serializing_if = "Option::is_none"`（None 跳键）。max_attempts /
 * retry_base_ms Kotlin 不消费——ignoreUnknownKeys 跳过。
 */
@Serializable
internal data class ProviderWireDto(
    @SerialName("base_url") val baseUrl: String = "",
    @SerialName("api_key_env") val apiKeyEnv: String = "",
    val adapter: String? = null,
    val title: String? = null,
)

/**
 * `[models.<entry>]` 投影（11 字段）。Rust `ModelDto`：数值/计价 Option 双形态
 * （null 与缺键同义 → Kotlin null）；supports_tool_call 显式注意——Rust serde
 * default 是 false，但旧 parseConfig 的 optBoolean 默认 **true**（缺键视为支持），
 * 此处保持 Kotlin 侧行为；reasoning_effort None 跳键（wire 缺省 = Auto）。
 */
@Serializable
internal data class ModelWireDto(
    val provider: String = "",
    val model: String = "",
    @SerialName("max_context_tokens") val maxContextTokens: Long? = null,
    @SerialName("max_output_tokens") val maxOutputTokens: Long? = null,
    @SerialName("supports_tool_call") val supportsToolCall: Boolean = true,
    @SerialName("supports_vision") val supportsVision: Boolean = false,
    @SerialName("supports_reasoning") val supportsReasoning: Boolean = false,
    @SerialName("input_price_per_mtok") val inputPricePerMtok: Double? = null,
    @SerialName("output_price_per_mtok") val outputPricePerMtok: Double? = null,
    @SerialName("reasoning_effort") val reasoningEffort: String? = null,
)

/** `[approval]` 生效规则视图（Rust `ApprovalConfigViewDto`）。default_level 缺/空 → auto。 */
@Serializable
internal data class ApprovalWireDto(
    @SerialName("default_level") val defaultLevel: String? = null,
    @SerialName("tool_levels") val toolLevels: Map<String, String> = emptyMap(),
    @SerialName("server_levels") val serverLevels: Map<String, String> = emptyMap(),
)

/**
 * config 视图 JSON（snapshot.config / config_changed.config）→ [ConfigUi]。
 * 逐条目 decode 失败跳过（对齐旧 `optJSONObject(name) ?: continue` 的行级容错）；
 * providers/models 保持文档序（BTreeMap 序列化恒有序，LinkedHashMap 解码保序）；
 * 整体解析失败 → 全默认 ConfigUi（旧实现对垃圾输入同样得空投影）。
 */
internal fun decodeConfigJson(json: String): ConfigUi {
    val root = runCatching { BridgeJson.parseToJsonElement(json) }.getOrNull() as? JsonObject
        ?: return ConfigUi()
    val providers = (root["providers"] as? JsonObject)?.entries?.mapNotNull { (name, el) ->
        if (el !is JsonObject) return@mapNotNull null
        runCatching { BridgeJson.decodeFromJsonElement(ProviderWireDto.serializer(), el) }
            .getOrNull()
            ?.let { ProviderUi(name, it.baseUrl, it.apiKeyEnv, it.adapter, it.title) }
    } ?: emptyList()
    val models = (root["models"] as? JsonObject)?.entries?.mapNotNull { (entry, el) ->
        if (el !is JsonObject) return@mapNotNull null
        runCatching { BridgeJson.decodeFromJsonElement(ModelWireDto.serializer(), el) }
            .getOrNull()
            ?.let { m ->
                ModelUi(
                    entry = entry,
                    provider = m.provider,
                    modelId = m.model,
                    maxContextTokens = m.maxContextTokens,
                    maxOutputTokens = m.maxOutputTokens,
                    supportsToolCall = m.supportsToolCall,
                    supportsVision = m.supportsVision,
                    supportsReasoning = m.supportsReasoning,
                    inputPricePerMtok = m.inputPricePerMtok,
                    outputPricePerMtok = m.outputPricePerMtok,
                    reasoningEffort = m.reasoningEffort,
                )
            }
    } ?: emptyList()
    val approval = (root["approval"] as? JsonObject)?.let { a ->
        runCatching { BridgeJson.decodeFromJsonElement(ApprovalWireDto.serializer(), a) }
            .getOrNull()
    }?.let {
        ApprovalRulesUi(
            defaultLevel = it.defaultLevel?.ifBlank { "auto" } ?: "auto",
            toolLevels = it.toolLevels,
            serverLevels = it.serverLevels,
        )
    }
    return ConfigUi(
        providers = providers,
        models = models,
        levels = jsonStringMap(root, "levels"),
        capabilities = jsonStringMap(root, "capabilities"),
        approval = approval,
        activeConfig = root.stringOrNull("active_config") ?: "",
    )
}

/** 对象值区 → 字符串 map（对齐旧 optString 的逐键强制转换：非字符串值落 ""）。 */
private fun jsonStringMap(root: JsonObject, key: String): Map<String, String> {
    val obj = root[key] as? JsonObject ?: return emptyMap()
    return buildMap { for ((k, v) in obj) put(k, v.primitiveContentOrNull() ?: "") }
}

private fun JsonElement.primitiveContentOrNull(): String? =
    (this as? JsonPrimitive)?.takeIf { it !is JsonNull }?.content

private fun JsonObject.stringOrNull(key: String): String? = this[key]?.primitiveContentOrNull()

// ── 本地转写镜像：UiEntry ↔ 短键 JSON（字节级兼容旧镜像文件）──────────

/**
 * 镜像附件引用（user 条目 `atts` 数组；照 wire 形态 `{id,mime,filename?,tokens?}`）。
 * filename/tokens null 跳键（encodeDefaults + nullable 默认）；空表时整段 atts 不写。
 */
@Serializable
internal data class AttachmentRefDto(
    val id: String = "",
    val mime: String = "",
    val filename: String? = null,
    val tokens: Long? = null,
)

/**
 * 镜像条目（discriminator = 短键 `t`；字段短键 x/n/a/s/f/o/c/d/m/atts 与旧
 * `entryToJson` 输出一致——**双向兼容**：旧文件缺新键（如 c/atts）按默认值
 * 解码，正是旧 `optString(key)` 缺键回落语义；新文件空值键省略（encodeDefaults），
 * 旧/新读侧都是宽容读取，值等价）。UiEntry.ToolCall 的 expanded 不持久化
 * （DTO 无此字段，恢复侧恒 false）；Reasoning 的 startTs 同样只在内存。
 * s 缺键回退 done（远古镜像防御）；空串字段（o/c/x 等）编码时省略——
 * 双向解码值等价（旧侧 opt* 缺键本就落空串/false/null）。
 */
@JsonClassDiscriminator("t")
@Serializable
internal sealed interface EntryMirrorDto {
    @Serializable
    @SerialName("user")
    data class User(
        @SerialName("x") val text: String = "",
        @SerialName("atts") val attachments: List<AttachmentRefDto> = emptyList(),
    ) : EntryMirrorDto

    @Serializable
    @SerialName("assistant")
    data class Assistant(@SerialName("x") val text: String = "") : EntryMirrorDto

    @Serializable
    @SerialName("reasoning")
    data class Reasoning(
        @SerialName("x") val text: String = "",
        @SerialName("m") val meta: String? = null,
    ) : EntryMirrorDto

    @Serializable
    @SerialName("meta")
    data class Meta(@SerialName("x") val text: String = "") : EntryMirrorDto

    @Serializable
    @SerialName("tool")
    data class ToolCall(
        @SerialName("n") val name: String = "",
        @SerialName("a") val argsPreview: String = "",
        @SerialName("s") val status: String = "done",
        @SerialName("f") val fullArgs: String = "",
        @SerialName("o") val output: String = "",
        @SerialName("c") val callId: String = "",
    ) : EntryMirrorDto

    @Serializable
    @SerialName("delegate")
    data class Delegate(
        @SerialName("n") val agent: String = "",
        @SerialName("d") val done: Boolean = false,
    ) : EntryMirrorDto

    @Serializable
    @SerialName("approval")
    data class Approval(
        @SerialName("n") val toolName: String = "",
        @SerialName("d") val decision: String = "",
    ) : EntryMirrorDto

    @Serializable
    @SerialName("step")
    data object StepBreak : EntryMirrorDto
}

private fun AttachmentRefUi.toMirrorDto(): AttachmentRefDto =
    AttachmentRefDto(id = id, mime = mime, filename = filename, tokens = tokens)

/** 旧 `parseAttachmentRefs` 语义镜像：filename 空串归 null（optStringOrNull）、tokens 负值归 null（-1 哨兵）。 */
private fun AttachmentRefDto.toUi(): AttachmentRefUi = AttachmentRefUi(
    id = id,
    mime = mime,
    filename = filename?.takeIf { it.isNotEmpty() },
    tokens = tokens?.takeIf { it >= 0 },
)

internal fun UiEntry.toMirrorDto(): EntryMirrorDto = when (this) {
    is UiEntry.User -> EntryMirrorDto.User(text = text, attachments = attachments.map { it.toMirrorDto() })
    is UiEntry.Assistant -> EntryMirrorDto.Assistant(text = text)
    is UiEntry.Reasoning -> EntryMirrorDto.Reasoning(text = text, meta = meta)
    is UiEntry.Meta -> EntryMirrorDto.Meta(text = text)
    is UiEntry.ToolCall -> EntryMirrorDto.ToolCall(
        name = name,
        argsPreview = argsPreview,
        status = status,
        fullArgs = fullArgs,
        output = output ?: "",
        callId = callId ?: "",
    )
    is UiEntry.Delegate -> EntryMirrorDto.Delegate(agent = agent, done = done)
    is UiEntry.Approval -> EntryMirrorDto.Approval(toolName = toolName, decision = decision)
    UiEntry.StepBreak -> EntryMirrorDto.StepBreak
}

/** m 空白归 null、o/c 空串归 null（旧 ifBlank 语义）；expanded/startTs 不持久化（恢复默认）。 */
internal fun EntryMirrorDto.toUiEntry(): UiEntry = when (this) {
    is EntryMirrorDto.User -> UiEntry.User(text = text, attachments = attachments.map { it.toUi() })
    is EntryMirrorDto.Assistant -> UiEntry.Assistant(text)
    is EntryMirrorDto.Reasoning -> UiEntry.Reasoning(text, meta?.ifBlank { null })
    is EntryMirrorDto.Meta -> UiEntry.Meta(text)
    is EntryMirrorDto.ToolCall -> UiEntry.ToolCall(
        name = name,
        argsPreview = argsPreview,
        status = status,
        fullArgs = fullArgs,
        output = output.ifBlank { null },
        callId = callId.ifBlank { null },
    )
    is EntryMirrorDto.Delegate -> UiEntry.Delegate(agent, done)
    is EntryMirrorDto.Approval -> UiEntry.Approval(toolName, decision)
    EntryMirrorDto.StepBreak -> UiEntry.StepBreak
}

/** 镜像条目 → JSON 文本（t 先行，短键形状与旧 entryToJson 逐字节一致）。 */
internal fun encodeEntryMirror(e: UiEntry): String = BridgeJson.encodeToString(e.toMirrorDto())

/**
 * JSON 文本 → 镜像条目；**任何 decode 失败（含未知 t）→ null**——调用方
 * （entryFromJson）回退 StepBreak，对齐旧 `else -> UiEntry.StepBreak`。
 */
internal fun decodeEntryMirror(json: String): UiEntry? =
    runCatching { BridgeJson.decodeFromString<EntryMirrorDto>(json).toUiEntry() }.getOrNull()
