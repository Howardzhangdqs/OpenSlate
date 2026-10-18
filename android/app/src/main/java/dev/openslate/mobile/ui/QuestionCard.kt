package dev.openslate.mobile.ui

import android.provider.Settings
import androidx.compose.animation.AnimatedContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.animateDpAsState
import androidx.compose.animation.core.animateFloat
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.rememberInfiniteTransition
import androidx.compose.animation.core.tween
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.animation.togetherWith
import androidx.compose.foundation.ScrollState
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.pager.HorizontalPager
import androidx.compose.foundation.pager.rememberPagerState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.OpenInFull
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material.icons.outlined.Block
import androidx.compose.material.icons.outlined.Description
import androidx.compose.material.icons.outlined.HelpOutline
import androidx.compose.material.icons.outlined.Key
import androidx.compose.material.icons.outlined.Recommend
import androidx.compose.material.icons.outlined.Schedule
import androidx.compose.material.icons.outlined.SwapHoriz
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material.icons.outlined.Visibility
import androidx.compose.material.icons.outlined.VisibilityOff
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Checkbox
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.CornerRadius
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.PendingQuestionUi
import dev.openslate.mobile.bridge.QuestionDraft
import dev.openslate.mobile.bridge.QuestionUi
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.UiEntry
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.launch

/**
 * ask_user 问卷 UI（设计规范：docs/proposals/ask-user-ui-design.md）。
 *
 * 组件树：QuestionCard（内联卡：SegmentBar + QuestionHeader + 题区 Pager +
 * CardFooter）、SummaryCard（answered 折叠摘要）、TerminalLine（终态轻摘要）、
 * PendingNudgePill（输入区上方待答提醒）。全屏作答见 [QuestionFullScreen]。
 *
 * 状态与草稿全部挂 RuntimeBridge 的 UiState（requestId 为键），本文件全部
 * 无状态渲染；终态（answered/declined/…）由关联 ToolCall 条目的 output JSON
 * 承载，重启后经 transcript 恢复同源渲染。
 */

// ── 校验（纯函数，便于测试）──────────────────────────────────────────

/**
 * 题目是否「已处理」：choice ≥1 选中（选「其他」时说明非空）；text 非空白；
 * confirm 已选定一侧；secret 非空白；跳过 / 逐题不答视为已处理。
 * 未知题型按 text 兜底（不崩、不吞题）。
 */
internal fun isQuestionProcessed(q: QuestionUi, d: QuestionDraft): Boolean = when {
    d.skipped || d.declined -> true
    q.type == "choice" ->
        (d.selected.isNotEmpty() || (d.otherChecked && d.otherText.isNotBlank())) &&
            (!d.otherChecked || d.otherText.isNotBlank())
    q.type == "confirm" -> d.confirm != null
    q.type == "secret" -> d.text.isNotBlank()
    else -> d.text.isNotBlank() // text 与未知题型兜底
}

/** 未作答（未处理）题数 → CardFooter「还有 N 题未作答」+ 提交禁用。 */
internal fun unansweredQuestionCount(p: PendingQuestionUi): Int =
    p.questions.countIndexed { i, q -> !isQuestionProcessed(q, p.drafts[i]) }

private inline fun <T> List<T>.countIndexed(predicate: (Int, T) -> Boolean): Int {
    var n = 0
    for (i in indices) if (predicate(i, this[i])) n++
    return n
}

// ── output JSON 解析（转写 / 摘要同源）───────────────────────────────

/** 已答条目的展示投影（header 由提交时的脱敏快照携带，重启可恢复）。 */
internal data class AskAnswerDisplay(
    val id: String,
    val header: String,
    /** chip 主文本（长文截断；完整文本进详情弹窗）。 */
    val primary: String,
    /** 详情弹窗完整文本（secret 为引用描述，永不出现明文）。 */
    val full: String,
    val isSecret: Boolean,
)

internal sealed interface AskOutcomeUi {
    data class Answered(val answers: List<AskAnswerDisplay>) : AskOutcomeUi
    /** declined / cancelled / superseded / expired / timeout。 */
    data class Terminal(val kind: String) : AskOutcomeUi
}

/**
 * 解析 ask_user ToolCall 条目的 output JSON → 渲染投影。
 * null = 尚无结果（等待 host call / 提交回写）。
 */
internal fun parseAskUserOutput(output: String?): AskOutcomeUi? {
    if (output.isNullOrBlank()) return null
    val o = runCatching { org.json.JSONObject(output) }.getOrNull() ?: return null
    return when (o.optString("outcome")) {
        "answered" -> {
            val arr = o.optJSONArray("answers") ?: return AskOutcomeUi.Answered(emptyList())
            val list = mutableListOf<AskAnswerDisplay>()
            for (i in 0 until arr.length()) {
                val a = arr.optJSONObject(i) ?: continue
                val header = a.optString("header").ifBlank { a.optString("id") }
                val secret = a.optJSONObject("secret")
                val (primary, full) = when {
                    secret != null -> {
                        val name = secret.optString("name").ifBlank { "凭据" }
                        "$name · 已存入凭据" to "$name · 已存入凭据" +
                            if (secret.optBoolean("remember")) "（已记住）" else "（仅本会话）"
                    }
                    a.has("selected") -> {
                        val sel = buildList {
                            val sa = a.optJSONArray("selected") ?: return@buildList
                            for (j in 0 until sa.length()) add(sa.optString(j))
                        }
                        sel.joinToString("、").let { it to it }
                    }
                    a.has("value") -> a.optString("value").let { it to it }
                    a.optBoolean("skipped") -> "已跳过" to "已跳过"
                    a.optBoolean("declined") -> "未作答" to "未作答"
                    else -> "" to ""
                }
                list.add(AskAnswerDisplay(a.optString("id"), header, primary, full, secret != null))
            }
            AskOutcomeUi.Answered(list)
        }
        "declined" -> AskOutcomeUi.Terminal("declined")
        "cancelled" -> AskOutcomeUi.Terminal("cancelled")
        "superseded" -> AskOutcomeUi.Terminal("superseded")
        "expired" -> AskOutcomeUi.Terminal("expired")
        "timeout" -> AskOutcomeUi.Terminal("timeout")
        else -> null
    }
}

// ── 条目入口（ChatScreen 条目渲染调用）───────────────────────────────

/** ask_user 卡片视图描述（AnimatedContent 交叉淡替的目标态）。 */
private sealed interface AskCardView {
    data class Interactive(val p: PendingQuestionUi, val submitting: Boolean) : AskCardView
    data class Answered(val o: AskOutcomeUi.Answered, val feedback: Boolean) : AskCardView
    data class Terminal(val kind: String) : AskCardView
    data object Waiting : AskCardView
}

/**
 * ask_user ToolCall 条目的特化渲染入口：pending/submitting → 内联问卷卡；
 * answered → 折叠摘要卡；终态 → 轻摘要行；running 无结果 → 等待载荷行
 * （tool_start 与 host_call_requested 之间的窗口 / 重启回放等待）。
 */
@Composable
internal fun QuestionCardHost(
    tc: UiEntry.ToolCall,
    pending: PendingQuestionUi?,
    onOpenFullScreen: () -> Unit,
) {
    val matches = pending != null && pending.argsKey == tc.fullArgs
    val view: AskCardView = when {
        matches && (pending!!.phase == "pending" || pending.phase == "submitting") ->
            AskCardView.Interactive(pending, pending.phase == "submitting")
        // answered 后 pendingQuestion 保留到 turn 结束：摘要卡带 4s 提交反馈行
        matches && pending!!.phase == "answered" ->
            (parseAskUserOutput(tc.output) as? AskOutcomeUi.Answered)
                ?.let { AskCardView.Answered(it, feedback = true) } ?: AskCardView.Waiting
        else -> when (val o = parseAskUserOutput(tc.output)) {
            is AskOutcomeUi.Answered -> AskCardView.Answered(o, feedback = false)
            is AskOutcomeUi.Terminal -> AskCardView.Terminal(o.kind)
            null -> AskCardView.Waiting
        }
    }
    // 入场动画（pending 卡首次出现；rememberSaveable 保证滚动回收重进不重播）。
    val enterState = rememberSaveable(
        tc.fullArgs,
        saver = Saver<MutableTransitionState<Boolean>, Boolean>(
            save = { it.currentState },
            restore = { b -> MutableTransitionState(b).apply { targetState = true } },
        ),
    ) { MutableTransitionState(false).apply { targetState = true } }
    androidx.compose.animation.AnimatedVisibility(
        visibleState = enterState,
        enter = fadeIn(tween(220, easing = FastOutSlowInEasing)) +
            expandVertically(
                expandFrom = Alignment.Bottom,
                animationSpec = tween(220, easing = FastOutSlowInEasing),
            ),
        exit = fadeOut(tween(200)) + shrinkVertically(tween(200)),
    ) {
        AnimatedContent(
            targetState = view,
            transitionSpec = { (fadeIn(tween(200)) togetherWith fadeOut(tween(200))) },
            label = "askCard",
        ) { v ->
            when (v) {
                is AskCardView.Interactive -> InteractiveQuestionCard(v.p, v.submitting, onOpenFullScreen)
                is AskCardView.Answered -> SummaryCard(v.o, v.feedback)
                is AskCardView.Terminal -> AskTerminalLine(v.kind)
                AskCardView.Waiting -> AskWaitingLine()
            }
        }
    }
}

// ── 终态轻摘要行 / 等待行 ───────────────────────────────────────────

/** 终态轻摘要（与审批转写行同款排版：13dp 图标 + labelMedium + 横向 8dp）。 */
@Composable
internal fun AskTerminalLine(kind: String) {
    val (icon, text) = when (kind) {
        "declined" -> Icons.Outlined.Block to "未作答 —— 已告知 Agent 自行判断继续"
        "cancelled" -> Icons.Filled.Stop to "提问已取消"
        "superseded" -> Icons.Outlined.SwapHoriz to "提问已被新消息取代"
        else -> Icons.Outlined.Schedule to "提问已过期" // expired / timeout（S2）
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.padding(horizontal = 8.dp),
    ) {
        Icon(icon, contentDescription = null, modifier = Modifier.size(13.dp), tint = MaterialTheme.colorScheme.outline)
        Spacer(Modifier.width(8.dp))
        Text(text, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.outline)
    }
}

/** running 但结果未回（载荷窗口 / 重启回放等待 Rust lane 的 pending_questions）。 */
@Composable
private fun AskWaitingLine() {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.padding(horizontal = 8.dp),
    ) {
        CircularProgressIndicator(strokeWidth = 1.5.dp, modifier = Modifier.size(13.dp))
        Spacer(Modifier.width(8.dp))
        Text("提问…", style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.outline)
    }
}

// ── 内联问卷卡 ──────────────────────────────────────────────────────

/** 系统动画时长缩放（Settings.Global.ANIMATOR_DURATION_SCALE；0 = 静止终帧）。 */
@Composable
private fun rememberAnimatorScale(): Float {
    val context = LocalContext.current
    return remember {
        runCatching {
            Settings.Global.getFloat(
                context.contentResolver,
                Settings.Global.ANIMATOR_DURATION_SCALE,
                1f,
            )
        }.getOrDefault(1f)
    }
}

@Composable
private fun InteractiveQuestionCard(
    p: PendingQuestionUi,
    submitting: Boolean,
    onOpenFullScreen: () -> Unit,
) {
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    val keyboard = LocalSoftwareKeyboardController.current
    val focusManager = LocalFocusManager.current
    val n = p.questions.size
    val pagerState = rememberPagerState(initialPage = p.current.coerceIn(0, n - 1)) { n }

    // 滑动/点分段 → 同步当前题到 UiState（分段高亮 + 底部次要动作跟随）。
    LaunchedEffect(pagerState) {
        snapshotFlow { pagerState.currentPage }
            .collect { RuntimeBridge.setCurrentQuestion(p.requestId, it) }
    }
    // 切题收起键盘、清焦点（键盘不横跨两题）；drop(1) 跳过初始页。
    LaunchedEffect(pagerState) {
        snapshotFlow { pagerState.currentPage }
            .drop(1)
            .collect {
                keyboard?.hide()
                focusManager.clearFocus()
            }
    }

    // 呼吸边框（pending 专属；跟随系统动画缩放，归零静止终帧）。
    // alpha 经 derivedStateOf 组合 → draw 阶段读取，零重组逐帧绘制。
    val scale = rememberAnimatorScale()
    val primary = MaterialTheme.colorScheme.primary
    val borderColor: androidx.compose.runtime.State<Color> = if (submitting || scale <= 0f) {
        remember(primary) { androidx.compose.runtime.mutableStateOf(primary.copy(alpha = 0.65f)) }
    } else {
        val alpha = rememberInfiniteTransition(label = "askBreath").animateFloat(
            initialValue = 0.25f,
            targetValue = 0.65f,
            animationSpec = infiniteRepeatable(
                animation = tween(2000, easing = FastOutSlowInEasing),
                repeatMode = RepeatMode.Reverse,
            ),
            label = "askBreathAlpha",
        )
        remember(primary) { androidx.compose.runtime.derivedStateOf { primary.copy(alpha = alpha.value) } }
    }

    val processed = p.questions.indices.map { isQuestionProcessed(p.questions[it], p.drafts[it]) }
    val cardShape = RoundedCornerShape(16.dp)

    // 4 题满配：题区内部滚动 + 高度上限（屏幕高的 60%）。LazyColumn item 内
    // 约束无界（BoxWithConstraints.maxHeight = Infinity，×0.6 仍无界，heightIn
    // 形同虚设），上限必须取真实视口（screenHeightDp）。
    val bodyCap = LocalConfiguration.current.screenHeightDp.dp * 0.6f
    // 各题题区是否超出可视高（由 Pager 页内滚动状态上报）→ 全屏按钮描边加重。
    var bodyOverflow by remember(p.requestId) { mutableStateOf(List(n) { false }) }
    val emphasized = bodyOverflow.getOrNull(p.current.coerceIn(0, n - 1)) == true

    Box(Modifier.fillMaxWidth().padding(horizontal = 4.dp)) {
        Column(
            Modifier
                .fillMaxWidth()
                // 呼吸边框走 draw 阶段（零重组逐帧绘制）；置于 clip 之前，
                // 0.5dp 描边不被圆角裁掉外半。
                .drawWithContent {
                    drawContent()
                    drawRoundRect(
                        color = borderColor.value,
                        style = Stroke(0.5.dp.toPx()),
                        cornerRadius = CornerRadius(16.dp.toPx(), 16.dp.toPx()),
                    )
                }
                .clip(cardShape)
                .background(MaterialTheme.colorScheme.surfaceContainerLowest)
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(10.dp),
        ) {
            if (n > 1) {
                Row(verticalAlignment = Alignment.CenterVertically) {
                    SegmentBar(
                        count = n,
                        current = p.current,
                        processed = processed,
                        onSelect = { i ->
                            scope.launch { pagerState.scrollToPage(i) }
                        },
                        modifier = Modifier.weight(1f),
                    )
                    Spacer(Modifier.width(6.dp))
                    FullScreenButton(onOpenFullScreen, emphasized = emphasized)
                }
            }
            // 当前题头部（单题卡把全屏按钮留在此行右侧）。
            QuestionHeaderRow(
                q = p.questions[p.current.coerceIn(0, n - 1)],
                submitting = submitting,
                timeoutSec = p.timeoutSec,
                createdAtMs = p.createdAtMs,
                trailing = { if (n == 1) FullScreenButton(onOpenFullScreen, emphasized = emphasized) },
            )
            // 题区横滑 Pager（与点分段等效；草稿在 UiState 不丢）。
            Box(Modifier.fillMaxWidth().heightIn(max = bodyCap)) {
                AskQuestionPager(
                    p = p,
                    pagerState = pagerState,
                    enabled = !submitting,
                    minLines = 3,
                    onDraftChange = { idx, d ->
                        RuntimeBridge.updateQuestionDraft(p.requestId, idx) { d }
                    },
                    onBodyOverflow = { page, over ->
                        bodyOverflow = bodyOverflow.toMutableList().also { it[page] = over }
                    },
                )
            }
            // 底部：次要动作 + 整卡 decline + 提交（全屏页复用同款）。
            AskCardFooter(
                p = p,
                enabled = !submitting,
                onSubmit = { RuntimeBridge.submitQuestionAnswers(p.requestId) },
            )
        }
    }
}

/** 全屏进入按钮（44dp 命中区、36dp 视觉描边位；满配内容超高时描边加重引导全屏）。 */
@Composable
private fun FullScreenButton(onOpen: () -> Unit, emphasized: Boolean = false) {
    // 外层 44dp 只做命中区（视觉语言仍由内层 36dp 承担）。
    Box(
        contentAlignment = Alignment.Center,
        modifier = Modifier
            .size(44.dp)
            .clip(CircleShape)
            .clickable(onClick = onOpen),
    ) {
        Box(
            contentAlignment = Alignment.Center,
            modifier = Modifier
                .size(36.dp)
                .let {
                    if (emphasized) {
                        it.border(
                            1.dp,
                            MaterialTheme.colorScheme.primary.copy(alpha = 0.6f),
                            CircleShape,
                        )
                    } else it
                },
        ) {
            Icon(
                Icons.Filled.OpenInFull,
                contentDescription = "全屏作答",
                modifier = Modifier.size(16.dp),
                tint = MaterialTheme.colorScheme.primary,
            )
        }
    }
}

// ── 分段指示器 ──────────────────────────────────────────────────────

/**
 * 分段指示器（自绘线段，非 M3 SegmentedButton——进度指示 + 快速跳转语义）。
 * 颜色表达「是否已处理」（成功绿 / outline 25%），高度表达「当前在哪题」
 * （当前段加高，animateDpAsState 150ms 平移）。tall = 全屏加高版（6/8dp）。
 */
@Composable
internal fun SegmentBar(
    count: Int,
    current: Int,
    processed: List<Boolean>,
    onSelect: (Int) -> Unit,
    modifier: Modifier = Modifier,
    tall: Boolean = false,
) {
    Row(
        modifier = modifier.fillMaxWidth().heightIn(min = 44.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        for (i in 0 until count) {
            val isCurrent = i == current
            val h by animateDpAsState(
                targetValue = when {
                    isCurrent && tall -> 8.dp
                    isCurrent -> 6.dp
                    else -> 4.dp
                },
                animationSpec = tween(150),
                label = "segH$i",
            )
            val color = when {
                processed.getOrElse(i) { false } -> successGreen()
                isCurrent -> MaterialTheme.colorScheme.primary
                else -> MaterialTheme.colorScheme.outline.copy(alpha = 0.25f)
            }
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxHeight()
                    .padding(horizontal = 3.dp)
                    .clickable(
                        interactionSource = remember { MutableInteractionSource() },
                        indication = null,
                    ) { onSelect(i) },
                contentAlignment = Alignment.Center,
            ) {
                Box(
                    Modifier
                        .fillMaxWidth()
                        .height(h)
                        .clip(RoundedCornerShape(50))
                        .background(color),
                )
            }
        }
    }
}

// ── 题头 ────────────────────────────────────────────────────────────

/** 标签 chip + 等待指示（spinner + 「等待回答」/「提交中」；超时预留第三种文案）。 */
@Composable
private fun QuestionHeaderRow(
    q: QuestionUi,
    submitting: Boolean,
    timeoutSec: Long,
    createdAtMs: Long,
    trailing: @Composable () -> Unit = {},
) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        // 标签 chip：secondaryContainer@40% 底 + 0.5dp secondary@40% 边（§3.1）。
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .clip(RoundedCornerShape(50))
                .background(MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.4f))
                .border(
                    0.5.dp,
                    MaterialTheme.colorScheme.secondary.copy(alpha = 0.4f),
                    RoundedCornerShape(50),
                )
                .padding(horizontal = 8.dp, vertical = 3.dp),
        ) {
            Icon(
                Icons.Outlined.HelpOutline,
                contentDescription = null,
                modifier = Modifier.size(12.dp),
                tint = MaterialTheme.colorScheme.secondary,
            )
            Spacer(Modifier.width(4.dp))
            Text(
                q.header,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.onSecondaryContainer,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
        }
        Spacer(Modifier.weight(1f))
        trailing()
        Spacer(Modifier.width(6.dp))
        // 等待指示：13dp/1.5dp spinner + labelSmall（与工具 chip「执行中」同款）。
        Row(verticalAlignment = Alignment.CenterVertically) {
            CircularProgressIndicator(strokeWidth = 1.5.dp, modifier = Modifier.size(13.dp))
            Spacer(Modifier.width(4.dp))
            val timedOut = timeoutSec > 0 &&
                (System.currentTimeMillis() - createdAtMs) / 1000L > timeoutSec
            Text(
                when {
                    submitting -> "提交中"
                    timedOut -> "已超时，仍可补答" // S2 预留文案位
                    else -> "等待回答"
                },
                style = MaterialTheme.typography.labelSmall,
                color = if (timedOut && !submitting) {
                    MaterialTheme.colorScheme.outline
                } else {
                    MaterialTheme.colorScheme.primary
                },
            )
        }
    }
    Spacer(Modifier.height(2.dp))
    Text(q.question, style = MaterialTheme.typography.bodyLarge)
}

// ── 题区 Pager（内联 / 全屏共用）────────────────────────────────────

/**
 * 题区 Pager（内联 / 全屏共用）。页内容统一包一层 Box：底缘渐隐条需
 * [Alignment.BottomCenter] 贴页底（平铺在页 scope 会叠到页顶）。
 *
 * [onBodyOverflow] 上报各页内容是否超出可视高（内容变化时重报）；内联卡用其
 * 加重全屏按钮描边，全屏页不限高、用默认 no-op。
 */
@Composable
internal fun AskQuestionPager(
    p: PendingQuestionUi,
    pagerState: androidx.compose.foundation.pager.PagerState,
    enabled: Boolean,
    minLines: Int,
    onDraftChange: (Int, QuestionDraft) -> Unit,
    onBodyOverflow: (page: Int, overflows: Boolean) -> Unit = { _, _ -> },
) {
    HorizontalPager(state = pagerState, pageSpacing = 12.dp) { page ->
        val q = p.questions[page]
        val draft = p.drafts[page]
        val scroll = rememberSaveable(q.id, saver = ScrollState.Saver) { ScrollState(0) }
        // 内容是否超出可视高（maxValue 不随滚动位置变化，是稳定的「超高」信号）。
        LaunchedEffect(scroll) {
            snapshotFlow { scroll.maxValue > 0 }.collect { onBodyOverflow(page, it) }
        }
        Box(Modifier.fillMaxWidth()) {
            Column(Modifier.fillMaxWidth().verticalScroll(scroll)) {
                QuestionBody(
                    q = q,
                    draft = draft,
                    enabled = enabled && !draft.skipped && !draft.declined,
                    minLines = minLines,
                    onChange = { onDraftChange(page, it) },
                )
            }
            // 底缘渐隐：内容高出可视区（可继续下滚）时提示还有更多
            // （transparent→solid 朝下，贴底缘）。
            if (scroll.canScrollForward) {
                Box(
                    Modifier
                        .align(Alignment.BottomCenter)
                        .fillMaxWidth()
                        .height(18.dp)
                        .background(
                            Brush.verticalGradient(
                                0f to MaterialTheme.colorScheme.surfaceContainerLowest.copy(alpha = 0f),
                                1f to MaterialTheme.colorScheme.surfaceContainerLowest,
                            ),
                        ),
                )
            }
        }
    }
}

/** 每题型一节（未知 type 按 text 兜底）。 */
@Composable
private fun QuestionBody(
    q: QuestionUi,
    draft: QuestionDraft,
    enabled: Boolean,
    minLines: Int,
    onChange: (QuestionDraft) -> Unit,
) {
    when (q.type) {
        "choice" -> ChoiceGroup(q, draft, enabled, onChange)
        "confirm" -> ConfirmPair(q, draft, enabled, onChange)
        "secret" -> SecretInputSection(q, draft, enabled, minLines = 1, onChange)
        else -> TextInputSection(q, draft, enabled, minLines, onChange) // text + 未知兜底
    }
    // 逐题状态提示（跳过 / 不答后控件禁用，动作按钮变为撤销，见 Footer）。
    if (draft.skipped || draft.declined) {
        Text(
            if (draft.skipped) "已跳过此题" else "已选择不答此题",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
    }
}

// ── choice：单选 / 多选 ─────────────────────────────────────────────

@Composable
private fun ChoiceGroup(
    q: QuestionUi,
    draft: QuestionDraft,
    enabled: Boolean,
    onChange: (QuestionDraft) -> Unit,
) {
    val clearFlags = { d: QuestionDraft -> d.copy(skipped = false, declined = false) }
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        q.options.forEach { opt ->
            OptionRow(
                label = opt.label,
                description = opt.description,
                recommended = opt.recommended,
                selected = opt.label in draft.selected,
                multi = q.multiSelect,
                enabled = enabled,
                onClick = {
                    onChange(
                        clearFlags(
                            draft.copy(
                                selected = if (q.multiSelect) {
                                    if (opt.label in draft.selected) draft.selected - opt.label
                                    else draft.selected + opt.label
                                } else {
                                    listOf(opt.label)
                                },
                            ),
                        ),
                    )
                },
            )
        }
        // 「其他」兜底行（schema 禁止模型自带 Other，UI 恒附）。
        OptionRow(
            label = "其他",
            description = "",
            recommended = false,
            selected = draft.otherChecked,
            multi = q.multiSelect,
            enabled = enabled,
            onClick = { onChange(clearFlags(draft.copy(otherChecked = !draft.otherChecked))) },
        )
        if (draft.otherChecked) {
            Column {
                OtherInput(q, draft, enabled, onChange)
                if (draft.otherText.isBlank()) {
                    Text(
                        "请填写说明，或取消选择「其他」",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
        }
    }
}

/** 选项行：最小高 48dp、10dp 圆角、横向 10dp 内边距；选中行 primaryContainer@35% 底。 */
@Composable
private fun OptionRow(
    label: String,
    description: String,
    recommended: Boolean,
    selected: Boolean,
    multi: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .heightIn(min = 48.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(
                if (selected) MaterialTheme.colorScheme.primaryContainer.copy(alpha = 0.35f)
                else Color.Transparent,
            )
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 10.dp, vertical = 4.dp),
    ) {
        // 单选 RadioButton / 多选 Checkbox（方形角，与项目 CheckboxRow 语义一致）。
        if (multi) {
            Checkbox(checked = selected, onCheckedChange = null, enabled = enabled)
        } else {
            RadioButton(selected = selected, onClick = null, enabled = enabled)
        }
        Spacer(Modifier.width(6.dp))
        Column(Modifier.weight(1f, fill = false)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    label,
                    style = MaterialTheme.typography.bodyMedium,
                    color = MaterialTheme.colorScheme.onSurface,
                    modifier = Modifier.weight(1f, fill = false),
                )
                if (recommended) {
                    Spacer(Modifier.width(8.dp))
                    // 推荐徽章：Recommend 12dp + labelSmall，primaryContainer 底。
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier
                            .clip(RoundedCornerShape(50))
                            .background(MaterialTheme.colorScheme.primaryContainer)
                            .padding(horizontal = 6.dp, vertical = 2.dp),
                    ) {
                        Icon(
                            Icons.Outlined.Recommend,
                            contentDescription = null,
                            modifier = Modifier.size(12.dp),
                            tint = MaterialTheme.colorScheme.onPrimaryContainer,
                        )
                        Spacer(Modifier.width(2.dp))
                        Text(
                            "推荐",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.onPrimaryContainer,
                        )
                    }
                }
            }
            if (description.isNotEmpty()) {
                Text(
                    description,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        }
    }
}

/** 「其他」说明输入框（选中「其他」才展开）。 */
@Composable
private fun OtherInput(q: QuestionUi, draft: QuestionDraft, enabled: Boolean, onChange: (QuestionDraft) -> Unit) {
    AskTextField(
        value = draft.otherText,
        onValueChange = { onChange(draft.copy(otherText = it, skipped = false, declined = false)) },
        placeholder = q.placeholder ?: "请补充说明",
        enabled = enabled,
        minLines = 1,
    )
}

// ── text：多行输入 ──────────────────────────────────────────────────

@Composable
private fun TextInputSection(
    q: QuestionUi,
    draft: QuestionDraft,
    enabled: Boolean,
    minLines: Int,
    onChange: (QuestionDraft) -> Unit,
) {
    AskTextField(
        value = draft.text,
        onValueChange = { onChange(draft.copy(text = it, skipped = false, declined = false)) },
        placeholder = q.placeholder ?: "",
        enabled = enabled,
        minLines = minLines,
    )
}

/** 自绘输入框（BasicTextField + 中性底 + 0.5dp outline 细边，10dp 圆角）。 */
@Composable
private fun AskTextField(
    value: String,
    onValueChange: (String) -> Unit,
    placeholder: String,
    enabled: Boolean,
    minLines: Int,
    singleLine: Boolean = false,
    mask: Boolean = false,
    passwordKeyboard: Boolean = false,
    trailing: @Composable () -> Unit = {},
) {
    var visible by remember { mutableStateOf(false) }
    BasicTextField(
        value = value,
        onValueChange = onValueChange,
        enabled = enabled,
        singleLine = singleLine,
        minLines = if (singleLine) 1 else minLines,
        maxLines = if (singleLine) 1 else 8,
        textStyle = MaterialTheme.typography.bodyMedium.copy(color = MaterialTheme.colorScheme.onSurface),
        cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
        visualTransformation = if (mask && !visible) PasswordVisualTransformation() else VisualTransformation.None,
        keyboardOptions = if (passwordKeyboard) {
            KeyboardOptions(keyboardType = KeyboardType.Password)
        } else {
            KeyboardOptions.Default
        },
        decorationBox = { inner ->
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier
                    .fillMaxWidth()
                    .heightIn(min = 48.dp)
                    .clip(RoundedCornerShape(10.dp))
                    .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.3f))
                    .border(
                        0.5.dp,
                        MaterialTheme.colorScheme.outline.copy(alpha = 0.4f),
                        RoundedCornerShape(10.dp),
                    )
                    .padding(horizontal = 12.dp, vertical = 10.dp),
            ) {
                Box(Modifier.weight(1f)) {
                    if (value.isEmpty() && placeholder.isNotEmpty()) {
                        Text(
                            placeholder,
                            style = MaterialTheme.typography.bodyMedium,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    inner()
                }
                if (mask) {
                    Spacer(Modifier.width(6.dp))
                    Icon(
                        if (visible) Icons.Outlined.VisibilityOff else Icons.Outlined.Visibility,
                        contentDescription = if (visible) "隐藏明文" else "显示明文",
                        modifier = Modifier
                            .size(18.dp)
                            .clip(CircleShape)
                            .clickable(enabled = enabled) { visible = !visible },
                        tint = MaterialTheme.colorScheme.outline,
                    )
                }
                trailing()
            }
        },
    )
}

// ── confirm：两枚正反按钮 ───────────────────────────────────────────

@Composable
private fun ConfirmPair(
    q: QuestionUi,
    draft: QuestionDraft,
    enabled: Boolean,
    onChange: (QuestionDraft) -> Unit,
) {
    val positive = q.options.getOrNull(0)?.label ?: "是"
    val negative = q.options.getOrNull(1)?.label ?: "否"
    Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
        ConfirmSideButton(
            label = positive,
            icon = Icons.Filled.Check,
            selected = draft.confirm == true,
            tint = MaterialTheme.colorScheme.primary,
            enabled = enabled,
            onClick = { onChange(draft.copy(confirm = true, skipped = false, declined = false)) },
            modifier = Modifier.weight(1f),
        )
        ConfirmSideButton(
            label = negative,
            icon = Icons.Filled.Close,
            selected = draft.confirm == false,
            tint = MaterialTheme.colorScheme.secondary,
            enabled = enabled,
            onClick = { onChange(draft.copy(confirm = false, skipped = false, declined = false)) },
            modifier = Modifier.weight(1f),
        )
    }
}

/** 选中：tint 12% 底 + 1dp 同色边；未选中：中性底 + 0.5dp outline 边。 */
@Composable
private fun ConfirmSideButton(
    label: String,
    icon: ImageVector,
    selected: Boolean,
    tint: Color,
    enabled: Boolean,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.Center,
        modifier = modifier
            .heightIn(min = 48.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(
                if (selected) tint.copy(alpha = 0.12f)
                else MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.3f),
            )
            .let {
                if (selected) it.border(1.dp, tint, RoundedCornerShape(10.dp))
                else it.border(
                    0.5.dp,
                    MaterialTheme.colorScheme.outline.copy(alpha = 0.4f),
                    RoundedCornerShape(10.dp),
                )
            }
            .clickable(enabled = enabled, onClick = onClick)
            .padding(horizontal = 12.dp, vertical = 12.dp),
    ) {
        Icon(
            icon,
            contentDescription = null,
            modifier = Modifier.size(16.dp),
            tint = if (selected) tint else MaterialTheme.colorScheme.outline,
        )
        Spacer(Modifier.width(6.dp))
        Text(
            label,
            style = MaterialTheme.typography.labelLarge,
            color = if (selected) tint else MaterialTheme.colorScheme.onSurfaceVariant,
        )
    }
}

// ── secret：掩码输入 + 记住 + 落地形态 ──────────────────────────────

@Composable
private fun SecretInputSection(
    q: QuestionUi,
    draft: QuestionDraft,
    enabled: Boolean,
    minLines: Int,
    onChange: (QuestionDraft) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        AskTextField(
            value = draft.text,
            onValueChange = { onChange(draft.copy(text = it, skipped = false, declined = false)) },
            placeholder = q.placeholder ?: "",
            enabled = enabled,
            minLines = minLines,
            singleLine = true,
            mask = true,
            passwordKeyboard = true,
        )
        // 记住勾选（CheckboxRow 基线：Checkbox + bodyMedium + bodySmall 副文本）。
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 44.dp)
                .clickable(enabled = enabled) { onChange(draft.copy(remember = !draft.remember)) },
        ) {
            Checkbox(
                checked = draft.remember,
                onCheckedChange = null,
                enabled = enabled,
            )
            Column {
                Text("记住此凭据", style = MaterialTheme.typography.bodyMedium)
                Text(
                    "勾选后以后会话也能用；默认仅本会话有效",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        }
        // 落地形态行（来自工具参数 storage；变量名/路径 MonoFont）。
        val isFile = q.storageKind == "file"
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(10.dp))
                .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.3f))
                .border(
                    0.5.dp,
                    MaterialTheme.colorScheme.outline.copy(alpha = 0.4f),
                    RoundedCornerShape(10.dp),
                )
                .padding(horizontal = 12.dp, vertical = 10.dp),
        ) {
            Icon(
                if (isFile) Icons.Outlined.Description else Icons.Outlined.Terminal,
                contentDescription = null,
                modifier = Modifier.size(14.dp),
                tint = MaterialTheme.colorScheme.secondary,
            )
            Spacer(Modifier.width(8.dp))
            Text(
                buildAnnotatedString {
                    withStyle(SpanStyle(fontFamily = MonoFont)) {
                        append(q.storageName ?: if (isFile) "文件" else "环境变量")
                    }
                    append(
                        // file：路径已在行首 MonoFont 位展示，不再加「路径见…」尾缀。
                        if (isFile) " · 写入文件"
                        else " · 环境变量 · 本会话的命令自动生效",
                    )
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
        }
        Text(
            "明文不会出现在对话和日志里",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline,
        )
    }
}

// ── 底部：次要动作 + 整卡 decline + 提交（内联 / 全屏共用）──────────

@Composable
internal fun AskCardFooter(
    p: PendingQuestionUi,
    enabled: Boolean,
    onSubmit: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val cur = p.current.coerceIn(0, p.questions.size - 1)
    val q = p.questions[cur]
    val d = p.drafts[cur]
    var confirmDecline by remember(p.requestId) { mutableStateOf(false) }
    val unanswered = unansweredQuestionCount(p)
    val submitting = p.phase == "submitting"

    Column(modifier, verticalArrangement = Arrangement.spacedBy(8.dp)) {
        // 次要动作行：跳过（仅 optional）/ 此题不答（仅多题）。
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(16.dp),
        ) {
            if (q.optional) {
                TextAction(
                    text = if (d.skipped) "取消跳过" else "跳过",
                    onClick = {
                        RuntimeBridge.updateQuestionDraft(p.requestId, cur) {
                            it.copy(skipped = !it.skipped, declined = false)
                        }
                    },
                )
            }
            if (p.questions.size > 1) {
                TextAction(
                    text = if (d.declined) "撤销不答" else "此题不答",
                    onClick = {
                        RuntimeBridge.updateQuestionDraft(p.requestId, cur) {
                            it.copy(declined = !it.declined, skipped = false)
                        }
                    },
                )
            }
            Spacer(Modifier.weight(1f))
            // 整卡 decline（labelMedium outline 色，点后二次确认）。
            TextAction(
                text = "不回答",
                onClick = { confirmDecline = true },
            )
        }
        // 必填校验提示（提交按钮上方右侧）。
        if (unanswered > 0) {
            Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                Text(
                    "还有 $unanswered 题未作答",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        }
        // 主按钮：SoftButton 放大变体（全宽、最小高 48dp、primary）。
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.Center,
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 48.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(
                    MaterialTheme.colorScheme.primary.copy(
                        alpha = if (enabled && unanswered == 0) 0.12f else 0.05f,
                    ),
                )
                .clickable(enabled = enabled && unanswered == 0 && !submitting, onClick = onSubmit),
        ) {
            if (submitting) {
                CircularProgressIndicator(strokeWidth = 1.5.dp, modifier = Modifier.size(13.dp))
                Spacer(Modifier.width(6.dp))
            }
            Text(
                if (submitting) "提交中" else "提交",
                style = MaterialTheme.typography.labelLarge,
                color = MaterialTheme.colorScheme.primary.copy(
                    alpha = if (enabled && unanswered == 0) 1f else 0.45f,
                ),
            )
        }
    }

    if (confirmDecline) {
        DeclineConfirmDialog(
            onDismiss = { confirmDecline = false },
            onConfirm = {
                confirmDecline = false
                RuntimeBridge.declineQuestionCard(p.requestId)
            },
        )
    }
}

/** 纯文字次要动作（labelMedium；内边距补足 ≥44dp 命中区）。 */
@Composable
private fun TextAction(text: String, onClick: () -> Unit) {
    Text(
        text,
        style = MaterialTheme.typography.labelMedium,
        color = MaterialTheme.colorScheme.outline,
        modifier = Modifier
            .heightIn(min = 44.dp)
            .clickable(onClick = onClick)
            .padding(horizontal = 2.dp),
    )
}

/** 整卡 decline 二次确认（AppDialog 骨架；确认键 error tint）。 */
@Composable
private fun DeclineConfirmDialog(onDismiss: () -> Unit, onConfirm: () -> Unit) {
    AppDialog(onDismiss = onDismiss, title = "不回答这组问题？") {
        Text(
            "Agent 会自行判断并继续，这组问题不再等待你作答。",
            style = MaterialTheme.typography.bodyMedium,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End)) {
            SoftButton(onClick = onDismiss) { Text("继续回答") }
            SoftButton(onClick = onConfirm, tint = MaterialTheme.colorScheme.error) { Text("不回答") }
        }
    }
}

// ── answered 折叠摘要卡 ─────────────────────────────────────────────

@OptIn(ExperimentalLayoutApi::class)
@Composable
internal fun SummaryCard(outcome: AskOutcomeUi.Answered, feedback: Boolean) {
    var showDetail by remember { mutableStateOf(false) }
    // 提交成功反馈：淡入 4s 后淡出收矮（从 resolve 成功起算）。
    var feedbackVisible by remember(outcome) { mutableStateOf(feedback) }
    LaunchedEffect(feedback) {
        if (feedback) {
            delay(4000)
            feedbackVisible = false
        }
    }
    val shape = RoundedCornerShape(16.dp)
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp)
            .clip(shape)
            .background(MaterialTheme.colorScheme.surfaceContainerLowest)
            .border(0.5.dp, MaterialTheme.colorScheme.secondary.copy(alpha = 0.4f), shape)
            .clickable { showDetail = true }
            .padding(horizontal = 12.dp, vertical = 10.dp)
            .animateContentSize(tween(200, easing = FastOutSlowInEasing)),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(
                Icons.Filled.Check,
                contentDescription = null,
                modifier = Modifier.size(14.dp),
                tint = successGreen(),
            )
            Spacer(Modifier.width(6.dp))
            Text(
                "已回答 · ${outcome.answers.size} 题",
                style = MaterialTheme.typography.labelMedium,
                color = successGreen(),
            )
        }
        FlowRow(
            horizontalArrangement = Arrangement.spacedBy(6.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            outcome.answers.forEach { a -> AnswerChip(a) }
        }
        androidx.compose.animation.AnimatedVisibility(
            visible = feedbackVisible,
            enter = fadeIn(tween(200)),
            exit = fadeOut(tween(200)) + shrinkVertically(tween(200)),
        ) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    Icons.Filled.Check,
                    contentDescription = null,
                    modifier = Modifier.size(13.dp),
                    tint = MaterialTheme.colorScheme.primary,
                )
                Spacer(Modifier.width(6.dp))
                Text(
                    "已提交，Agent 正在继续",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
        }
    }
    if (showDetail) {
        AskAnswersDialog(outcome = outcome, onDismiss = { showDetail = false })
    }
}

/** 答案 chip：50% 全圆角、surfaceContainerHigh 底、labelSmall；secret 特例成功绿 container。 */
@Composable
private fun AnswerChip(a: AskAnswerDisplay) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .widthIn(max = 280.dp)
            .clip(RoundedCornerShape(50))
            .background(
                if (a.isSecret) successGreenContainer()
                else MaterialTheme.colorScheme.surfaceContainerHigh,
            )
            .padding(horizontal = 10.dp, vertical = 4.dp),
    ) {
        if (a.isSecret) {
            Icon(
                Icons.Outlined.Key,
                contentDescription = null,
                modifier = Modifier.size(13.dp),
                tint = successGreen(),
            )
            Spacer(Modifier.width(4.dp))
        }
        Text(
            "${a.header}: ${a.primary}",
            style = MaterialTheme.typography.labelSmall,
            color = if (a.isSecret) successGreen() else MaterialTheme.colorScheme.onSurfaceVariant,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
}

/** 完整答案弹窗（ToolDetailDialog 容器语言：16dp Surface + 可滚动 + 关闭钮）。 */
@Composable
private fun AskAnswersDialog(outcome: AskOutcomeUi.Answered, onDismiss: () -> Unit) {
    androidx.compose.ui.window.Dialog(
        onDismissRequest = onDismiss,
        properties = androidx.compose.ui.window.DialogProperties(usePlatformDefaultWidth = false),
    ) {
        androidx.compose.material3.Surface(
            shape = RoundedCornerShape(16.dp),
            color = MaterialTheme.colorScheme.surface,
            modifier = Modifier.fillMaxWidth(0.96f).heightIn(max = 560.dp),
        ) {
            Column {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.padding(start = 16.dp, end = 4.dp, top = 2.dp),
                ) {
                    Text(
                        "已回答 · ${outcome.answers.size} 题",
                        style = MaterialTheme.typography.titleSmall,
                        modifier = Modifier.weight(1f),
                    )
                    androidx.compose.material3.IconButton(onClick = onDismiss) {
                        Icon(Icons.Filled.Close, contentDescription = "关闭")
                    }
                }
                androidx.compose.material3.HorizontalDivider(
                    thickness = 0.5.dp,
                    color = MaterialTheme.colorScheme.outline.copy(alpha = 0.3f),
                )
                Column(
                    Modifier
                        .weight(1f, fill = false)
                        .verticalScroll(rememberScrollState())
                        .padding(horizontal = 16.dp, vertical = 10.dp),
                    verticalArrangement = Arrangement.spacedBy(10.dp),
                ) {
                    outcome.answers.forEach { a ->
                        Text(
                            a.header,
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.primary,
                        )
                        Text(a.full, style = MaterialTheme.typography.bodyMedium)
                    }
                }
            }
        }
    }
}

// ── 待答提醒 pill（输入区上方悬浮）──────────────────────────────────

/** pending 时用户滚走 → 输入胶囊上方浮出；点击滚动回卡片（GlassPanel 语言）。 */
@Composable
internal fun PendingNudgePill(
    visible: Boolean,
    count: Int,
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
    onClick: () -> Unit,
) {
    AnimatedVisibility(
        visible = visible,
        enter = fadeIn() + expandVertically(expandFrom = Alignment.Bottom),
        exit = fadeOut() + shrinkVertically(shrinkTowards = Alignment.Bottom),
    ) {
        Box(Modifier.fillMaxWidth().padding(horizontal = 10.dp), contentAlignment = Alignment.Center) {
            GlassPanel(
                glass = glass,
                rootCoords = rootCoords,
                tint = MaterialTheme.colorScheme.surfaceVariant.copy(
                    alpha = if (android.os.Build.VERSION.SDK_INT >= android.os.Build.VERSION_CODES.S) {
                        0.6f
                    } else 0.92f,
                ),
                modifier = Modifier.askPillShadow(),
                shape = RoundedCornerShape(50),
                border = true,
            ) {
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier
                        .heightIn(min = 44.dp)
                        .clickable(onClick = onClick)
                        .padding(horizontal = 14.dp),
                ) {
                    Icon(
                        Icons.Outlined.HelpOutline,
                        contentDescription = null,
                        modifier = Modifier.size(13.dp),
                        tint = MaterialTheme.colorScheme.primary,
                    )
                    Spacer(Modifier.width(6.dp))
                    Text(
                        "等待你的回答 · $count 题",
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
        }
    }
}

/** pill 的轻阴影（与输入胶囊同款语言）。 */
private fun Modifier.askPillShadow(): Modifier = this.shadow(
    elevation = 2.dp,
    shape = RoundedCornerShape(50),
    clip = false,
)
