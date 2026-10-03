package dev.openslate.mobile.ui

import android.os.Build
import android.util.Log
import androidx.compose.animation.togetherWith
import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.outlined.Psychology
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.History
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Tab
import androidx.compose.material3.TabRow
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.BlurEffect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.layer.GraphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.rememberGraphicsLayer
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.UiEntry
import kotlin.math.roundToInt
import kotlinx.coroutines.delay

/**
 * Phase 1 聊天主界面：transcript 流 + 输入框 + 审批横幅。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(onOpenHistory: () -> Unit, onOpenSettings: () -> Unit) {
    val state by RuntimeBridge.state.collectAsState()
    val listState = rememberLazyListState()
    // 点击空白区域 → 收起输入法 + 取消输入框焦点。可点击的子组件（按钮、工具 chip、
    // 输入胶囊）会消费 tap 事件，不会走到这里，行为互不影响。
    val focusManager = LocalFocusManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    // 相邻的连续工具调用聚合为一组（圆角矩形容器内多个 chip），
    // 其余条目原样单行渲染。
    val renderItems: List<RenderItem> = remember(state.entries) { groupEntries(state.entries) }
    // 当前模型声明支持思维链时，TTFT 等待期显示"思考中"；无思维链模型不显示。
    val thinkEligible = state.config.models
        .find { it.entry == state.modelAlias }?.supportsReasoning == true
    val showThinking = state.running && state.awaitingFirstToken && thinkEligible

    // 磨砂玻璃：背景捕获层 + 全屏模糊副本（各玻璃面板共用一份，性能考虑），
    // rootCoords 用于把模糊副本平移到每个玻璃面板自身位置。
    val glass = rememberGlassLayers()
    var rootCoords by remember { mutableStateOf<LayoutCoordinates?>(null) }
    val blurPx = with(LocalDensity.current) { 20.dp.toPx() }
    val blurEffect = remember(blurPx) {
        BlurEffect(radiusX = blurPx, radiusY = blurPx, edgeTreatment = TileMode.Clamp)
    }

    LaunchedEffect(state.entries.size, showThinking) {
        val target = renderItems.lastIndex + if (showThinking) 1 else 0
        if (target >= 0) {
            listState.animateScrollToItem(target)
        }
    }

    Scaffold(
        topBar = {
            // 磨砂玻璃顶栏：GlassPanel 重绘背景模糊层（Android 12+）+ 半透明 tint；
            // 低版本设备退化为半透明蒙层（无真模糊）。TopAppBar 自身容器全透明。
            GlassPanel(
                glass = glass,
                rootCoords = { rootCoords },
                tint = MaterialTheme.colorScheme.surface.copy(
                    alpha = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) 0.55f else 0.82f,
                ),
                modifier = Modifier.fillMaxWidth(),
            ) {
                TopAppBar(
                    title = {
                        Column {
                            Text("OpenSlate", style = MaterialTheme.typography.titleLarge)
                            Text(
                                text = when {
                                    !state.ready -> "runtime 启动中…"
                                    state.running ->
                                        "${state.modelAlias} · Step ${state.step} · 工具 ${state.toolCalls}"
                                    else -> state.modelAlias.ifEmpty { "就绪" }
                                },
                                style = MaterialTheme.typography.labelSmall,
                            )
                        }
                    },
                    actions = {
                        IconButton(onClick = onOpenHistory) {
                            Icon(
                                androidx.compose.material.icons.Icons.Filled.History,
                                contentDescription = "历史会话",
                            )
                        }
                        IconButton(onClick = onOpenSettings) {
                            Icon(
                                androidx.compose.material.icons.Icons.Filled.Settings,
                                contentDescription = "模型配置",
                            )
                        }
                        TextButton(onClick = { RuntimeBridge.newSession() }) { Text("新会话") }
                    },
                    colors = TopAppBarDefaults.topAppBarColors(
                        containerColor = Color.Transparent,
                        scrolledContainerColor = Color.Transparent,
                    ),
                )
            }
        },
        bottomBar = {
            if (state.pendingApproval != null) {
                ApprovalBanner(
                    pending = state.pendingApproval!!,
                    onAnswer = { id, choice -> RuntimeBridge.answerApproval(id, choice) },
                )
            }
        },
    ) { padding ->
        // 全屏列表 + 悬浮输入区：列表视口铺满整个 Scaffold（不被 padding 裁剪），
        // 条目从磨砂顶栏下方、透明输入条下方穿过——底条由此成为"看得见的透明"。
        // contentPadding 只负责给首末条目留出安全边距。
        Box(
            modifier = Modifier
                .fillMaxSize()
                .onGloballyPositioned { rootCoords = it }
                // 放在 padding/imePadding 之前，手势区域覆盖整个内容区（含 padding 带）。
                .pointerInput(Unit) {
                    detectTapGestures(onTap = {
                        keyboard?.hide()
                        focusManager.clearFocus()
                    })
                }
                .padding(bottom = padding.calculateBottomPadding())
                .imePadding(),
        ) {
            // 背景捕获层：把列表内容录制进 GraphicsLayer，再录制一份全屏模糊副本；
            // 顶栏与输入胶囊共用同一份模糊层（每个玻璃区域各自全屏模糊会拖垮 GPU）。
            // GraphicsLayer.renderEffect 依赖 RenderEffect，Android 12+ 才生效。
            Box(
                Modifier
                    .fillMaxSize()
                    .drawWithContent {
                        val layerSize = IntSize(
                            width = size.width.roundToInt(),
                            height = size.height.roundToInt(),
                        )
                        glass.source.record(size = layerSize) {
                            this@drawWithContent.drawContent()
                        }
                        glass.blurred.record(size = layerSize) {
                            drawLayer(glass.source)
                        }
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                            glass.blurred.renderEffect = blurEffect
                        }
                        drawLayer(glass.source)
                    },
            ) {
            if (!state.ready) {
                Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                    CircularProgressIndicator()
                }
            } else {
                    LazyColumn(
                        state = listState,
                        modifier = Modifier.fillMaxSize(),
                        contentPadding = androidx.compose.foundation.layout.PaddingValues(
                            start = 12.dp, end = 12.dp,
                            top = padding.calculateTopPadding() + 8.dp,
                            // 悬浮输入胶囊（约 60dp）+ 余量，末条消息不被盖住。
                            bottom = 84.dp,
                        ),
                        verticalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        // key 稳定 → 滚动复用组合（Markdown 不重解析，老设备关键）。
                        // 组 key 用组首条目在 entries 中的索引（turn 进行中 entries
                        // 只追加、不改写既有段 → 稳定）。
                        itemsIndexed(renderItems, key = { _, item ->
                            when (item) {
                                is RenderItem.Single -> "s-${item.index}"
                                is RenderItem.ToolGroup -> "g-${item.startIdx}"
                            }
                        }) { idx, item ->
                            when (item) {
                                is RenderItem.Single -> EntryRow(
                                    item.entry,
                                    // 会话进行中且是最后一条 → 思考中态（实时秒数 + 贴底跟随）。
                                    activeLast = state.running && idx == renderItems.lastIndex,
                                )
                                is RenderItem.ToolGroup -> ToolGroupCard(item.parts)
                            }
                        }
                        // TTFT 等待期（request_start → 首个 token）的"思考中"提示，
                        // 仅对声明支持思维链的模型显示；样式与思维链 chip 同款。
                        if (showThinking) {
                            item(key = "thinking-pending") {
                                Row(
                                    verticalAlignment = Alignment.CenterVertically,
                                    modifier = Modifier
                                        .padding(horizontal = 4.dp)
                                        .clip(RoundedCornerShape(50))
                                        .background(
                                            MaterialTheme.colorScheme.tertiaryContainer
                                                .copy(alpha = 0.35f)
                                        )
                                        .border(
                                            0.5.dp,
                                            MaterialTheme.colorScheme.tertiary.copy(alpha = 0.4f),
                                            RoundedCornerShape(50),
                                        )
                                        .padding(horizontal = 10.dp, vertical = 3.dp),
                                ) {
                                    Icon(
                                        Icons.Outlined.Psychology,
                                        contentDescription = null,
                                        modifier = Modifier.size(13.dp),
                                        tint = MaterialTheme.colorScheme.tertiary,
                                    )
                                    Spacer(Modifier.width(4.dp))
                                    Text(
                                        "思考中…",
                                        style = MaterialTheme.typography.labelSmall,
                                        color = MaterialTheme.colorScheme.tertiary,
                                    )
                                }
                            }
                        }
                    }
                }
            }
            Composer(
                glass = glass,
                rootCoords = { rootCoords },
                modifier = Modifier.align(Alignment.BottomCenter),
                running = state.running,
                onSend = { text ->
                    RuntimeBridge.submit(text)
                },
                onCancel = { RuntimeBridge.cancel() },
            )
        }
    }
}

/** 渲染投影：相邻的连续工具调用聚合为一组（中间夹的 Meta/StepBreak 一并吸收），其余条目单行。 */
private sealed class RenderItem {
    data class Single(val entry: UiEntry, val index: Int) : RenderItem()

    /** parts 保序混装工具调用与夹缝中的 Meta/StepBreak；tools 是其中的 ToolCall 子集。 */
    data class ToolGroup(val parts: List<UiEntry>, val startIdx: Int) : RenderItem()
}

private fun groupEntries(entries: List<UiEntry>): List<RenderItem> {
    val out = mutableListOf<RenderItem>()
    var i = 0
    while (i < entries.size) {
        val e = entries[i]
        if (e is UiEntry.ToolCall) {
            val start = i
            val parts = mutableListOf<UiEntry>()
            while (i < entries.size) {
                val cur = entries[i]
                when {
                    cur is UiEntry.ToolCall -> { parts.add(cur); i++ }
                    // 夹在工具调用之间的统计行/步分隔：向后看，若后面还是工具调用则吸收进组。
                    (cur is UiEntry.Meta || cur == UiEntry.StepBreak) &&
                        i + 1 < entries.size && entries[i + 1] is UiEntry.ToolCall -> {
                        parts.add(cur); i++
                    }
                    else -> break
                }
            }
            out.add(RenderItem.ToolGroup(parts, start))
        } else {
            out.add(RenderItem.Single(e, i))
            i++
        }
    }
    return out
}

@Composable
private fun EntryRow(entry: UiEntry, activeLast: Boolean = false) {
    when (entry) {
        is UiEntry.User -> Bubble(entry.text, mine = true)
        is UiEntry.Assistant -> MarkdownBubble(entry.text)
        is UiEntry.Reasoning -> ReasoningBlock(entry.text, entry.meta, entry.startTs, activeLast)
        is UiEntry.Meta -> Text(
            entry.text,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.tertiary,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        // 分组投影后正常不会到达；保留兜底（sealed when 完整性）。
        is UiEntry.ToolCall -> ToolGroupCard(listOf(entry))
        is UiEntry.Delegate -> Text(
            (if (entry.done) "↳ agent " else "↻ agent ") + entry.agent,
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        is UiEntry.Approval -> Text(
            "⚑ ${entry.toolName} → ${entry.decision}",
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        UiEntry.StepBreak -> Spacer(Modifier.widthIn(min = 4.dp))
    }
}

/**
 * 工具组容器：无外框，相邻工具条目紧贴堆叠、中间只留一条缝；
 * 首条目显示上方圆角、末条目显示下方圆角、中间方角，体现一体感。
 * 夹在工具调用之间的 Meta/StepBreak 行在组内原样渲染。
 * 单个条目点击就地展开（命令 + 输出），展开后仍与上下条目紧贴。
 */
private val TOOL_GROUP_RADIUS = 12.dp

@Composable
private fun ToolGroupCard(parts: List<UiEntry>) {
    val tools = parts.filterIsInstance<UiEntry.ToolCall>()
    // 整组共用一条边框：外框按组形状绘制（四角 12dp），条目本身不带 border。
    val groupShape = RoundedCornerShape(TOOL_GROUP_RADIUS)
    val groupBorder = MaterialTheme.colorScheme.secondary.copy(alpha = 0.4f)
    Column(
        Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp)
            .clip(groupShape)
            .border(0.5.dp, groupBorder, groupShape),
        verticalArrangement = Arrangement.spacedBy(0.dp),
    ) {
        parts.forEach { part ->
            when (part) {
                is UiEntry.ToolCall -> {
                    val idx = tools.indexOf(part)
                    // 组内相邻工具之间保留一条分隔线（首个工具上方不需要），
                    // 展开后也能直观看出每个工具调用的边界。
                    if (idx > 0) {
                        HorizontalDivider(
                            thickness = 0.5.dp,
                            color = groupBorder,
                        )
                    }
                    // 条目自身的圆角规则：首条目上圆、末条目下圆（展开与否均保持）。
                    val bottomR = if (idx == tools.lastIndex) TOOL_GROUP_RADIUS else 0.dp
                    val shape = RoundedCornerShape(
                        topStart = if (idx == 0) TOOL_GROUP_RADIUS else 0.dp,
                        topEnd = if (idx == 0) TOOL_GROUP_RADIUS else 0.dp,
                        bottomEnd = bottomR,
                        bottomStart = bottomR,
                    )
                    ToolChipRow(part, shape)
                }
                is UiEntry.Meta -> Text(
                    part.text,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.tertiary,
                    modifier = Modifier.padding(start = 12.dp, top = 3.dp, bottom = 3.dp),
                )
                UiEntry.StepBreak -> Spacer(Modifier.widthIn(min = 4.dp))
                else -> {}
            }
        }
    }
}

/** 单个工具条目：状态图标 + 名称 + 参数摘要，点击就地展开。 */
@Composable
private fun ToolChipRow(tc: UiEntry.ToolCall, shape: RoundedCornerShape) {
    // 不用 Surface(onClick)：M3 会强制 48dp 最小触摸目标，把条目撑高、组内出现大空隙。
    // 改用 foundation 组合（clip+background+clickable）精确控制尺寸；
    // 边框由 ToolGroupCard 统一绘制，条目只负责底色。
    // 状态着色：成功淡绿、失败淡红、执行中保持中性紫。
    // （主题的 secondaryContainer 是淡紫，绿色用固定色值表达。）
    // tool_end 事件不带失败状态，native 工具的失败以非 0 exit_code 表达，
    // 直接从输出里识别。
    val exitFail = Regex("""exit_code: (?:[1-9]\d*)""").containsMatchIn(tc.output.orEmpty())
    val ok = tc.status == "done" && !exitFail
    val failed = tc.status == "failed" || exitFail
    val statusTint = when {
        ok -> androidx.compose.ui.graphics.Color(0xFF1E8A44)
        failed -> MaterialTheme.colorScheme.error
        else -> MaterialTheme.colorScheme.secondary
    }
    val chipColor = when {
        ok -> androidx.compose.ui.graphics.Color(0xFFDDF3DE)
        failed -> MaterialTheme.colorScheme.errorContainer.copy(alpha = 0.3f)
        else -> MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.4f)
    }
    Column(Modifier.animateContentSize()) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .clip(shape)
                .background(chipColor)
                .clickable { RuntimeBridge.toggleToolExpanded(tc) }
                .padding(start = 10.dp, end = 6.dp, top = 4.dp, bottom = 4.dp),
        ) {
            when (tc.status) {
                "running" -> CircularProgressIndicator(
                    strokeWidth = 1.5.dp,
                    modifier = Modifier.size(13.dp),
                )
                "failed" -> Icon(
                    Icons.Filled.Close,
                    contentDescription = null,
                    modifier = Modifier.size(14.dp),
                    tint = MaterialTheme.colorScheme.error,
                )
                else -> Icon(
                    Icons.Filled.Check,
                    contentDescription = null,
                    modifier = Modifier.size(14.dp),
                    tint = MaterialTheme.colorScheme.secondary,
                )
            }
            Spacer(Modifier.widthIn(min = 6.dp))
            Text(
                tc.name,
                style = MaterialTheme.typography.labelMedium,
                color = statusTint,
            )
            Spacer(Modifier.widthIn(min = 6.dp))
            Text(
                tc.argsPreview,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
                maxLines = 1,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
            Icon(
                if (tc.expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                contentDescription = if (tc.expanded) "收起" else "展开",
                modifier = Modifier.size(16.dp),
                tint = MaterialTheme.colorScheme.secondary,
            )
        }
        if (tc.expanded) {
            Column(Modifier.padding(start = 10.dp, end = 4.dp, top = 2.dp, bottom = 6.dp)) {
                Text(
                    "命令",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
                Text(
                    tc.fullArgs.ifBlank { "（无参数）" },
                    style = MaterialTheme.typography.bodySmall.copy(
                        fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                    ),
                )
                if (tc.status == "running") {
                    Text(
                        "结果（执行中…）",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                } else {
                    ToolResultView(tc.output.orEmpty(), tc.status)
                }
            }
        }
    }
}

/**
 * 展开后的结果区：`结果` 标签行右侧显示 exit_code（native 工具的输出带）；
 * stdout / stderr 按 tab 切换，stderr 为空时不显示 stderr tab。
 * 非格式化输出（如 termux 回传的合并文本）整体按 stdout 处理。
 */
@Composable
private fun ToolResultView(raw: String, status: String = "done") {
    if (raw.isBlank()) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                "结果",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.primary,
                modifier = Modifier.weight(1f),
            )
            Text(
                "（无输出）",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        return
    }
    val exitCode = Regex("""exit_code: (-?\d+)""").find(raw)?.groupValues?.get(1)
    val hasSections = raw.contains("--- stdout ---")
    val stdout = if (hasSections) {
        raw.substringAfter("--- stdout ---").substringBefore("--- stderr ---").trim()
    } else {
        raw.trim()
    }
    val stderr = if (hasSections) {
        raw.substringAfter("--- stderr ---", "").trim()
            .takeUnless { it.isEmpty() || it == "(empty)" } ?: ""
    } else {
        ""
    }
    var tab by remember(raw) { mutableStateOf(0) }

    // 标签行：结果 [stdout][stderr chip] …… [exit chip]
    // stdout/stderr 用 chip 做切换（选中高亮）；exit chip 略小一号。
    Row(
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Text(
            "结果",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.primary,
        )
        Spacer(Modifier.width(12.dp))
        if (stderr.isNotEmpty()) {
            listOf("stdout" to 0, "stderr" to 1).forEach { (label, idx) ->
                val selected = tab == idx
                val labelColor = if (selected) {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.outline
                }
                Text(
                    label,
                    style = MaterialTheme.typography.labelSmall,
                    color = labelColor,
                    modifier = Modifier
                        .clip(RoundedCornerShape(50))
                        .background(
                            if (selected) {
                                MaterialTheme.colorScheme.primary.copy(alpha = 0.16f)
                            } else {
                                MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.3f)
                            },
                        )
                        .border(
                            0.5.dp,
                            labelColor.copy(alpha = 0.4f),
                            RoundedCornerShape(50),
                        )
                        .clickable { tab = idx }
                        .padding(horizontal = 10.dp, vertical = 3.dp),
                )
                Spacer(Modifier.width(6.dp))
            }
        }
        Spacer(Modifier.weight(1f))
        if (exitCode != null) {
            val ok = exitCode == "0"
            // 颜色按状态分级：0 → 绿；非 0 → 红。
            val tint = if (ok) {
                androidx.compose.ui.graphics.Color(0xFF1E8A44)
            } else {
                MaterialTheme.colorScheme.error
            }
            // tooltip 状态机：tipMounted 决定 Popup 是否挂载，tipVisible 驱动
            // AnimatedVisibility 的进出场动画（挂载首帧先以 false 进入组合，
            // 再翻转触发入场；退场时 Popup 延迟卸载以播完退出动画）。
            var tipMounted by remember(exitCode) { mutableStateOf(false) }
            var tipVisible by remember(exitCode) { mutableStateOf(false) }
            // 点击：未挂载→挂载（入场动画随后触发）；已显示→仅隐藏（播退场）。
            val toggleTip = {
                if (tipMounted) {
                    tipVisible = false
                } else {
                    tipMounted = true
                }
                Unit
            }
            // 挂载后下一帧再置 visible=true，确保入场动画播放。
            LaunchedEffect(tipMounted) {
                if (tipMounted) tipVisible = true
            }
            // 显示 2.5s 后自动收起。
            LaunchedEffect(tipVisible) {
                if (tipVisible) {
                    kotlinx.coroutines.delay(2500)
                    tipVisible = false
                }
            }
            // 退场动画（约 120ms）播完后卸载 Popup。
            LaunchedEffect(tipMounted, tipVisible) {
                if (tipMounted && !tipVisible) {
                    kotlinx.coroutines.delay(160)
                    tipMounted = false
                }
            }
            Text(
                exitCode,
                style = MaterialTheme.typography.labelSmall.copy(
                    fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                ),
                color = tint,
                modifier = Modifier
                    .clip(RoundedCornerShape(50))
                    .background(tint.copy(alpha = 0.12f))
                    .border(0.5.dp, tint.copy(alpha = 0.4f), RoundedCornerShape(50))
                    .clickable { toggleTip() }
                    .padding(horizontal = 10.dp, vertical = 3.dp),
            )
            if (tipMounted) {
                androidx.compose.ui.window.Popup(
                    alignment = androidx.compose.ui.Alignment.TopEnd,
                    offset = androidx.compose.ui.unit.IntOffset(0, 24),
                    onDismissRequest = {
                        tipVisible = false
                    },
                ) {
                    androidx.compose.animation.AnimatedVisibility(
                        visible = tipVisible,
                        enter = androidx.compose.animation.fadeIn(
                            androidx.compose.animation.core.tween(110),
                        ) + androidx.compose.animation.scaleIn(
                            initialScale = 0.85f,
                            animationSpec = androidx.compose.animation.core.tween(110),
                            transformOrigin = androidx.compose.ui.graphics.TransformOrigin(
                                0.9f, 0f,
                            ),
                        ) + androidx.compose.animation.slideInVertically(
                            initialOffsetY = { -it / 3 },
                            animationSpec = androidx.compose.animation.core.tween(110),
                        ),
                        exit = androidx.compose.animation.fadeOut(
                            androidx.compose.animation.core.tween(120),
                        ) + androidx.compose.animation.scaleOut(
                            targetScale = 0.9f,
                            animationSpec = androidx.compose.animation.core.tween(120),
                            transformOrigin = androidx.compose.ui.graphics.TransformOrigin(
                                0.9f, 0f,
                            ),
                        ) + androidx.compose.animation.slideOutVertically(
                            targetOffsetY = { -it / 4 },
                            animationSpec = androidx.compose.animation.core.tween(120),
                        ),
                    ) {
                        Surface(
                            shape = RoundedCornerShape(8.dp),
                            color = MaterialTheme.colorScheme.inverseSurface,
                            shadowElevation = 2.dp,
                        ) {
                            Text(
                                "Exit Code：命令退出码\n0 = 成功；非 0 = 失败或被信号终止",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.inverseOnSurface,
                                modifier = Modifier.padding(horizontal = 8.dp, vertical = 6.dp),
                            )
                        }
                    }
                }
            }
        }
    }
    // 内容区：切换 stdout/stderr 时按方向左右滑动 + 淡入淡出。
    androidx.compose.animation.AnimatedContent(
        targetState = tab,
        transitionSpec = {
            val dir = if (targetState > initialState) 1 else -1
            (androidx.compose.animation.slideInHorizontally { it / 4 * dir } +
                androidx.compose.animation.fadeIn()) togetherWith
                (androidx.compose.animation.slideOutHorizontally { -it / 4 * dir } +
                    androidx.compose.animation.fadeOut())
        },
        label = "resultTab",
    ) { currentTab ->
        Text(
            when {
                stderr.isNotEmpty() && currentTab == 1 ->
                    stderr.ifBlank { "（空）" }
                stdout.isNotBlank() -> stdout
                else -> "（无输出）"
            },
            style = MaterialTheme.typography.bodySmall.copy(
                fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
            ),
            maxLines = 16,
            overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
        )
    }
}

/**
 * 思维链：默认折叠（超长文本布局昂贵 + 干扰正文），点击展开。
 * 展开后限高（180dp）内滚动：思考中（active）chip 实时显示"已思考 Ns"、
 * 内容始终贴底跟随；思考完毕后再展开则停留在这段思维链的开头。
 */
@Composable
private fun ReasoningBlock(
    text: String,
    meta: String? = null,
    startTs: Long = 0L,
    active: Boolean = false,
) {
    var expanded by remember { mutableStateOf(false) }
    // 思考中的实时秒数：每秒跳一次；结束（active=false）后保留最后值，
    // usage 回填 meta 前的空窗期 chip 仍显示"已思考 Ns"而非退回字数。
    var elapsedSec by remember { mutableStateOf(-1L) }
    LaunchedEffect(active, startTs) {
        // 镜像恢复的旧条目没有 startTs，无法计算真实时长，保持字数兜底。
        if (startTs <= 0) return@LaunchedEffect
        while (active) {
            elapsedSec = ((System.currentTimeMillis() - startTs) / 1000L).coerceAtLeast(0)
            delay(1000)
        }
    }
    // 展开/收起平滑动画。
    Column(
        Modifier
            .padding(horizontal = 4.dp)
            .animateContentSize()
    ) {
        // 迷你 pill（与工具结果区的 stdout/stderr chip 同款，更矮）：点击展开思维链。
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .clip(RoundedCornerShape(50))
                .background(MaterialTheme.colorScheme.tertiaryContainer.copy(alpha = 0.35f))
                .border(
                    0.5.dp,
                    MaterialTheme.colorScheme.tertiary.copy(alpha = 0.4f),
                    RoundedCornerShape(50),
                )
                .clickable { expanded = !expanded }
                .padding(horizontal = 10.dp, vertical = 3.dp),
        ) {
            Icon(
                Icons.Outlined.Psychology,
                contentDescription = null,
                modifier = Modifier.size(13.dp),
                tint = MaterialTheme.colorScheme.tertiary,
            )
            Spacer(Modifier.width(4.dp))
            Text(
                when {
                    !meta.isNullOrBlank() -> "已思考 $meta"
                    elapsedSec >= 0 -> "已思考 ${elapsedSec}s"
                    else -> "思维链 ${text.length} 字"
                },
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.tertiary,
            )
            Spacer(Modifier.width(4.dp))
            Icon(
                if (expanded) {
                    Icons.Filled.ExpandLess
                } else {
                    Icons.Filled.ExpandMore
                },
                contentDescription = if (expanded) "收起" else "展开",
                modifier = Modifier.size(13.dp),
                tint = MaterialTheme.colorScheme.tertiary,
            )
        }
        if (expanded) {
            val scroll = rememberScrollState()
            // 思考中：内容增长（含刚展开）时始终贴底跟随。
            LaunchedEffect(active, expanded, text) {
                if (expanded && active) {
                    withFrameNanos {} // 等一帧布局完成，maxValue 才准确
                    scroll.scrollTo(scroll.maxValue)
                }
            }
            // 思考完毕后（重新）展开：回到这段思维链的开头。
            LaunchedEffect(expanded) {
                if (expanded && !active) scroll.scrollTo(0)
            }
            Text(
                text,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
                modifier = Modifier
                    .padding(top = 2.dp, start = 8.dp, end = 8.dp)
                    .heightIn(max = 180.dp)
                    .verticalScroll(scroll),
            )
        }
    }
}

/** 模型回复气泡：Markdown 渲染（代码块/粗体/列表/标题等，紧凑字号）。 */
@Composable
private fun MarkdownBubble(text: String) {
    val clipboard = LocalClipboardManager.current
    // 标题降档：聊天场景 H1≈titleLarge(20sp)，逐级收紧（默认 headlineMedium
    // 32sp 起太占屏）；正文 bodyMedium。
    val typography = (
        com.mikepenz.markdown.m3.markdownTypography()
            as com.mikepenz.markdown.model.DefaultMarkdownTypography
        ).copy(
            h1 = MaterialTheme.typography.titleLarge,
            h2 = MaterialTheme.typography.titleMedium,
            h3 = MaterialTheme.typography.titleSmall,
            h4 = MaterialTheme.typography.titleSmall,
            h5 = MaterialTheme.typography.bodyLarge,
            h6 = MaterialTheme.typography.bodyLarge,
            text = MaterialTheme.typography.bodyMedium,
            paragraph = MaterialTheme.typography.bodyMedium,
        )
    Surface(
        shape = RoundedCornerShape(
            topStart = 4.dp,
            topEnd = 16.dp,
            bottomStart = 16.dp,
            bottomEnd = 16.dp,
        ),
        color = MaterialTheme.colorScheme.surfaceVariant,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp),
        onClick = { clipboard.setText(AnnotatedString(text)) },
    ) {
        com.mikepenz.markdown.m3.Markdown(
            content = text,
            typography = typography,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
        )
    }
}

@Composable
private fun Bubble(text: String, mine: Boolean) {
    val clipboard = LocalClipboardManager.current
    Surface(
        shape = RoundedCornerShape(
            topStart = 16.dp,
            topEnd = 16.dp,
            bottomStart = if (mine) 16.dp else 4.dp,
            bottomEnd = if (mine) 4.dp else 16.dp,
        ),
        color = if (mine) MaterialTheme.colorScheme.primaryContainer
        else MaterialTheme.colorScheme.surfaceVariant,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp),
        onClick = { clipboard.setText(AnnotatedString(text)) },
    ) {
        Text(
            text,
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 10.dp),
            style = MaterialTheme.typography.bodyMedium,
        )
    }
}

/** 一份背景捕获层 + 一份全屏模糊副本；所有玻璃面板共用（避免每块玻璃各自全屏模糊）。 */
private class GlassLayers(val source: GraphicsLayer, val blurred: GraphicsLayer)

@Composable
private fun rememberGlassLayers(): GlassLayers {
    val source = rememberGraphicsLayer()
    val blurred = rememberGraphicsLayer()
    return remember(source, blurred) { GlassLayers(source, blurred) }
}

/**
 * 磨砂玻璃面板（真·背景模糊，参考 Haze 的实现思路、零第三方依赖）：
 * Android 12+ 时把共用的全屏模糊副本平移到自身位置、只露出被 shape 裁剪的一块，
 * 再叠半透明 tint；低版本退化为纯半透明蒙层（renderEffect 依赖 API 31 的 RenderEffect）。
 */
@Composable
private fun GlassPanel(
    glass: GlassLayers,
    rootCoords: () -> LayoutCoordinates?,
    tint: Color,
    modifier: Modifier = Modifier,
    shape: Shape = RectangleShape,
    border: Boolean = false,
    content: @Composable BoxScope.() -> Unit,
) {
    var offsetInRoot by remember { mutableStateOf(Offset.Zero) }
    Box(
        modifier = modifier
            .onGloballyPositioned { coordinates ->
                val root = rootCoords()
                if (root != null && root.isAttached && coordinates.isAttached) {
                    offsetInRoot = root.localPositionOf(coordinates, Offset.Zero)
                }
            }
            // 圆角裁剪交给 graphicsLayer，保证模糊副本/蒙层都按 shape 裁形。
            .graphicsLayer {
                this.shape = shape
                clip = true
            }
            .drawWithContent {
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    translate(left = -offsetInRoot.x, top = -offsetInRoot.y) {
                        drawLayer(glass.blurred)
                    }
                }
                drawRect(tint)
                drawContent()
            }
            .then(
                if (border) {
                    Modifier.border(
                        0.5.dp,
                        MaterialTheme.colorScheme.outline.copy(alpha = 0.4f),
                        shape,
                    )
                } else {
                    Modifier
                },
            ),
        content = content,
    )
}

/**
 * 紧凑输入区：自绘 BasicTextField 替代 M3 OutlinedTextField（后者强制 56dp 最小高度，
 * 显得过高）。输入胶囊为磨砂玻璃面板（GlassPanel）：Android 12+ 重绘背景模糊层，
 * 低版本退化为半透明蒙层；配细边框 + 轻阴影悬浮于透明底条之上。
 */
@Composable
private fun Composer(
    glass: GlassLayers,
    rootCoords: () -> LayoutCoordinates?,
    running: Boolean,
    onSend: (String) -> Unit,
    onCancel: () -> Unit,
    modifier: Modifier = Modifier,
) {
    var input by remember { mutableStateOf("") }
    // 底部条透明：去掉 tonalElevation 带来的色调底色，仅保留布局占位。
    Surface(color = androidx.compose.ui.graphics.Color.Transparent, modifier = modifier) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 10.dp, vertical = 6.dp),
        ) {
            GlassPanel(
                glass = glass,
                rootCoords = rootCoords,
                modifier = Modifier
                    .weight(1f)
                    .animateContentSize()
                    .shadow(elevation = 2.dp, shape = RoundedCornerShape(20.dp), clip = false),
                shape = RoundedCornerShape(20.dp),
                tint = MaterialTheme.colorScheme.surfaceVariant.copy(
                    alpha = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) 0.5f else 0.9f,
                ),
                border = true,
            ) {
                BasicTextField(
                    value = input,
                    onValueChange = { input = it },
                    textStyle = MaterialTheme.typography.bodyLarge.copy(
                        color = MaterialTheme.colorScheme.onSurface,
                    ),
                    cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
                    maxLines = 5,
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 14.dp, vertical = 10.dp),
                    decorationBox = { inner ->
                        Box {
                            if (input.isEmpty()) {
                                Text(
                                    "发消息…",
                                    style = MaterialTheme.typography.bodyLarge,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                            }
                            inner()
                        }
                    },
                )
            }
            Spacer(Modifier.width(6.dp))
            Box(
                contentAlignment = Alignment.Center,
                modifier = Modifier
                    .size(36.dp)
                    .shadow(elevation = 2.dp, shape = RoundedCornerShape(50), clip = false)
                    .clip(RoundedCornerShape(50))
                    .background(MaterialTheme.colorScheme.primary)
                    .clickable {
                        if (running) onCancel() else {
                            val text = input.trim()
                            if (text.isNotEmpty()) {
                                onSend(text)
                                input = ""
                            }
                        }
                    },
            ) {
                if (running) {
                    Icon(
                        Icons.Filled.Stop,
                        contentDescription = "取消",
                        tint = MaterialTheme.colorScheme.onPrimary,
                        modifier = Modifier.size(18.dp),
                    )
                } else {
                    Icon(
                        Icons.AutoMirrored.Filled.Send,
                        contentDescription = "发送",
                        tint = MaterialTheme.colorScheme.onPrimary,
                        modifier = Modifier.size(16.dp),
                    )
                }
            }
        }
    }
}

@Composable
private fun ApprovalBanner(
    pending: dev.openslate.mobile.bridge.PendingApprovalUi,
    onAnswer: (Long, String) -> Unit,
) {
    Surface(tonalElevation = 6.dp) {
        Column(Modifier.fillMaxWidth().padding(12.dp)) {
            Text(
                "审批请求：${pending.toolName}（${pending.riskLevel}）",
                style = MaterialTheme.typography.titleSmall,
            )
            if (pending.reason.isNotEmpty()) {
                Text(
                    pending.reason,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                TextButton(onClick = { onAnswer(pending.id, "approve_all") }) { Text("全部允许") }
                TextButton(onClick = { onAnswer(pending.id, "approve") }) { Text("允许") }
                TextButton(onClick = { onAnswer(pending.id, "deny") }) { Text("拒绝") }
            }
        }
    }
}
