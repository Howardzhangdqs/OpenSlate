package dev.openslate.mobile.ui

import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.AnimatedVisibilityScope
import androidx.compose.animation.SharedTransitionLayout
import androidx.compose.animation.SharedTransitionScope
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandHorizontally
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkHorizontally
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.DragInteraction
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxScope
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.CompositingStrategy
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Code
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.Psychology
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.History
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material.icons.outlined.Autorenew
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.derivedStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshotFlow
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.hapticfeedback.HapticFeedbackType
import androidx.compose.ui.platform.LocalHapticFeedback
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.BlurEffect
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.RectangleShape
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.graphics.TileMode
import androidx.compose.ui.graphics.drawscope.translate
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.layer.GraphicsLayer
import androidx.compose.ui.graphics.layer.drawLayer
import androidx.compose.ui.graphics.rememberGraphicsLayer
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.layout.onGloballyPositioned
import androidx.compose.ui.platform.LocalClipboardManager
import dev.openslate.mobile.bridge.McpRecoverPhase
import dev.openslate.mobile.bridge.McpRecoverStep
import androidx.compose.ui.platform.LocalContext
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalFocusManager
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import com.gallatinapps.syntaxmp.tokenizer.SyntaxTokenizer
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import dev.openslate.mobile.R
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.PendingQuestionUi
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.UiEntry
import dev.openslate.mobile.bridge.RuntimeBridge.ASK_USER_TOOL
import kotlin.math.roundToInt
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch

/**
 * Phase 1 聊天主界面：transcript 流 + 输入框 + 审批横幅。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    onOpenHistory: () -> Unit,
    onOpenSettings: () -> Unit,
    onOpenAskUser: () -> Unit = {},
    onOpenReviewAskUser: () -> Unit = {},
) {
    val state by RuntimeBridge.state.collectAsState()
    val listState = rememberLazyListState()
    // 点击空白区域 → 收起输入法 + 取消输入框焦点。可点击的子组件（按钮、工具 chip、
    // 输入胶囊）会消费 tap 事件，不会走到这里，行为互不影响。
    val focusManager = LocalFocusManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    // 收起聚焦面板的统一路径（与空白点击同款：键盘收 + 焦点清 → 面板经
    // 既有反向动画收回、卡片落回输入框）。
    val collapseComposer = {
        keyboard?.hide()
        focusManager.clearFocus()
    }
    val context = LocalContext.current
    // 输入框聚焦面板：显示 = 输入框持有焦点（编辑状态）。未聚焦时面板
    // 不参与组合，底部布局零变化。Termux/MCP 活性走 tracker 状态流
    // （聚焦展开时面板内部 refresh），未聚焦时喂输入框内常驻状态行。
    var composerFocused by remember { mutableStateOf(false) }
    val live by ComposerMcpStatusTracker.state.collectAsState()
    val inlineStatus = composerInlineStatus(state.modelAlias, state.config, live)
    // MCP 活性常新鲜（不依赖聚焦）：初次组合刷一次 + 回前台（ON_RESUME）
    // 刷一次。tracker 自带单飞去重（重复触发安全），探活为本机回环 HTTP
    // 成本低，故不设轮询定时器；面板聚焦时另有 refresh（ComposerFocusPanel）。
    // ChatScreen 无 config_changed / MCP reload 回调处理点，按约定不新增链路。
    LaunchedEffect(Unit) { ComposerMcpStatusTracker.refresh(context) }
    // Termux stall 激活期间周期重探连接（约 2.5s；tracker CAS 单飞防抖，
    // 在途探测的 refresh 为无操作）——stall 解除（termuxStallSince 变
    // null）即停。重探与面板可见性解耦：Ready 收起面板后重探仍继续到
    // stall 清零，重发后再 stall 能被下一轮探测重新捕获。
    LaunchedEffect(state.termuxStallSince) {
        if (state.termuxStallSince == null) return@LaunchedEffect
        while (true) {
            TermuxStallTracker.refresh(context)
            delay(2_500)
        }
    }
    // MCP 恢复激活期间周期重探（沿用 TermuxStallTracker 范式：CAS 单飞
    // 防抖、探测结论喂恢复面板的「Termux 运行」行）；恢复完成
    // （mcpRecovering 变 null）即停。
    LaunchedEffect(state.mcpRecovering) {
        if (state.mcpRecovering == null) return@LaunchedEffect
        TermuxStallTracker.refresh(context)
        while (true) {
            delay(2_500)
            TermuxStallTracker.refresh(context)
        }
    }
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                ComposerMcpStatusTracker.refresh(context)
                // stall 在途时回前台立即重探：用户从 Termux 切回的最快
                // 路径确认恢复并续跑（gateRefresh 同款 ON_RESUME 门控）。
                if (RuntimeBridge.state.value.termuxStallSince != null) {
                    TermuxStallTracker.refresh(context)
                }
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
    }
    // 相邻的连续工具调用聚合为一组（圆角矩形容器内多个 chip），
    // 其余条目原样单行渲染。
    val renderItems: List<RenderItem> = remember(state.entries) { groupEntries(state.entries) }
    // 当前模型声明支持思维链时，TTFT 等待期显示"思考中"；无思维链模型不显示。
    // 会话别名经 modelEntryOf 解析到条目（levels 遗留兜底）再查能力。
    val thinkEligible = state.config.models
        .find {
            it.entry == RuntimeBridge.modelEntryOf(state.modelAlias, state.config)
        }?.supportsReasoning == true
    // 顶栏副标题的模型显示名：条目键经共享助手换「Provider显示名/modelId」，
    // 内部 entry 不直出；levels 已不在 UI 暴露，仅作解析兜底。
    val modelDisplay = RuntimeBridge
        .modelAliasDisplay(RuntimeBridge.modelEntryOf(state.modelAlias, state.config), state.config)
    val showThinking = state.running && state.awaitingFirstToken && thinkEligible

    // 磨砂玻璃：共享的内容捕获层（source）+ 模糊参数；每个玻璃面板在各自
    // draw 阶段录制「自身区域 + 模糊半径出血带」的小模糊层（见 GlassPanel）。
    // rootCoords 用于把内容捕获层平移到面板自身位置。
    val glass = rememberGlass()
    // Haze 内容源：弹层玻璃（GlassDropdownMenu，聚焦面板模型选择器）对聊天
    // 内容做局部真实模糊；与 GlassPanel 的 GraphicsLayer 捕获体系互不相干。
    val hazeState = rememberHazeState()
    var rootCoords by remember { mutableStateOf<LayoutCoordinates?>(null) }
    // 工具详情弹窗：点击工具条目打开（完整命令 + 输出，可选中复制）。
    // 保存的是打开瞬间的快照副本；渲染前按 name+fullArgs 回查 entries 取
    // 最新状态（执行中的工具完成时输出能实时刷新进来）。
    var detailTool by remember { mutableStateOf<UiEntry.ToolCall?>(null) }
    val detailLive = detailTool?.let { d ->
        state.entries.filterIsInstance<UiEntry.ToolCall>()
            .firstOrNull { it.name == d.name && it.fullArgs == d.fullArgs }
            ?: d
    }

    // ── 智能贴底跟随 ────────────────────────────────────────────────────
    // 「跟随」是用户意图闩（followBottom），几何「贴底」（listAtBottom）只
    // 是闩的合上条件。不把几何直接当跟随开关：流式输出会让末条目在一帧内
    // 长出几十 dp，瞬时几何立刻翻成「不在底」，直接驱动会跟丢；闩 + 明确
    // 的合上/断开事件才稳定。
    //
    // - 合上：滚动完全停稳（fling 结束，isScrollInProgress 门控）且几何贴底。
    // - 断开：手指拖动列表（DragInteraction.Start）。程序滚动（贴底跟随、
    //   聚焦轻推、pill 跳转）不经过 interactionSource，绝不误断。
    // - 跟随：闩合上时，条目追加（size）/ 末条目内容增长（lastEntrySig：
    //   流式 delta 与工具输出回填都并入同一条目、size 不变）/ 思考中伪条
    //   目（showThinking）任一变化 → scrollToItem 即时贴底（不用
    //   animateScrollToItem：高频重入会动画打架）。闩断开时完全静默。
    //
    // 问卷/设置等 NavHost 往返：ChatScreen 离开组合再回来，listState
    // （rememberLazyListState 内部 saveable）、followBottom、
    // revealedSessionId 原样恢复；重跑的跟随 effect 被闩拦住（不在底 →
    // 静默），会话揭示 effect 被 revealedSessionId 拦住（同会话不重揭示）
    // → 打开问卷时的阅读位置原样保留。
    var followBottom by rememberSaveable { mutableStateOf(false) }

    // 几何贴底判定：末条目可见、且其底缘距视口底 ≤ 余量。完全贴底时该距
    // 离恰为 LazyColumn 的底部 contentPadding（84dp，悬浮输入条的让位空
    // 间），余量取 96dp = 84dp 基准 + 12dp 手感容差；调整 contentPadding
    // 时同步核对本值。「末条目就是最后一条」是硬条件——它整条在视口下方
    // 时（用户在读更早内容）即便可见区贴近视口底也不是贴底。首帧 / 恢复
    // 初期 layout 未就绪 → 一律视为不在底（保守，恢复路径绝不误滚）。
    val atBottomSlackPx = with(LocalDensity.current) { AT_BOTTOM_SLACK_DP.dp.toPx() }.roundToInt()
    val listAtBottom by remember(atBottomSlackPx) {
        derivedStateOf {
            val info = listState.layoutInfo
            val last = info.visibleItemsInfo.lastOrNull() ?: return@derivedStateOf false
            if (last.index != info.totalItemsCount - 1) return@derivedStateOf false
            info.viewportEndOffset - (last.offset + last.size) <= atBottomSlackPx
        }
    }

    // 末条目内容签名：条目追加看 size，同条目内的流式增长看它。
    val lastEntrySig = remember(state.entries) { entrySig(state.entries.lastOrNull()) }

    // 会话揭示（旧「加载/切换即滚底」语义的守卫版）：首次拿到会话内容（启
    // 动 snapshot）与切换会话（openSession / new_session 的新 snapshot，
    // session_id 必变）→ 落到最新末条并合上闩。rememberSaveable 记住已揭
    // 示的 session_id：NavHost 往返（session_id 不变）不重跑，返回不滚底。
    var revealedSessionId by rememberSaveable { mutableStateOf<String?>(null) }
    LaunchedEffect(state.sessionId) {
        val id = state.sessionId
        if (id.isEmpty() || id == revealedSessionId) return@LaunchedEffect
        revealedSessionId = id
        followBottom = true
        val target = renderItems.lastIndex + if (showThinking) 1 else 0
        if (target >= 0) listState.scrollToItem(target)
    }

    // 空转写（新会话 / session_reset）视为贴底：无内容可读，后续首条内容
    // 直接跟随（快照尚未换新 session_id 的路径也能恢复跟随语义）。
    LaunchedEffect(state.entries.isEmpty()) {
        if (state.entries.isEmpty()) followBottom = true
    }

    // 内容跟随触发器：仅闩合上时滚。末条目 scrollToItem 受 max scroll 钳
    // 制——条目再高也稳稳落在「底」，长消息流式增长时始终露出最新内容。
    LaunchedEffect(state.entries.size, lastEntrySig, showThinking) {
        if (!followBottom) return@LaunchedEffect
        val target = renderItems.lastIndex + if (showThinking) 1 else 0
        if (target >= 0) listState.scrollToItem(target)
    }

    // 闩的合上：滚动停稳且几何贴底。isScrollInProgress 门控挡住拖动/fling
    // 途经贴底区的瞬时满足——轻拂到底、聚焦轻推、跳转落底等「停稳后在底」
    // 才恢复跟随。
    LaunchedEffect(Unit) {
        snapshotFlow { !listState.isScrollInProgress && listAtBottom }
            .collect { settledAtBottom -> if (settledAtBottom) followBottom = true }
    }

    // 闩的断开：手指拖动 = 用户接管视口（上翻阅读），期间到停稳前绝不自动滚。
    LaunchedEffect(Unit) {
        listState.interactionSource.interactions.collect { interaction ->
            if (interaction is DragInteraction.Start) followBottom = false
        }
    }

    // 聚焦面板展开时会盖住列表底部约 100dp：聚焦瞬间若视口已贴近底部，
    // 顺势把末条滚到面板上沿之上；正身处更早的阅读位置则不动（不打断浏览）。
    LaunchedEffect(composerFocused) {
        if (!composerFocused) return@LaunchedEffect
        val target = renderItems.lastIndex + if (showThinking) 1 else 0
        if (target < 0) return@LaunchedEffect
        val lastVisible =
            listState.layoutInfo.visibleItemsInfo.lastOrNull()?.index ?: return@LaunchedEffect
        if (lastVisible >= target - 2) listState.animateScrollToItem(target)
    }

    // 面板展开时：手指拖动聊天列表 → 收起面板（collapseComposer 统一路径）。
    // 监听 LazyListState.interactionSource 的 DragInteraction.Start——只有
    // 真实手指拖动才产生；聚焦轻推与贴底跟随的 scrollToItem 是程序滚动、
    // 不经过 interactionSource，不会误收刚展开的面板。
    LaunchedEffect(composerFocused) {
        if (!composerFocused) return@LaunchedEffect
        listState.interactionSource.interactions.collect { interaction ->
            if (interaction is DragInteraction.Start) collapseComposer()
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
                                // 直绑条目键经共享显示助手换「Provider/modelId」，
                                // 内部 entry 不直出；levels 代号原样（遗留兜底）。
                                // 模型名（标识类文本）用内嵌 MonoFont 子集，混排的
                                // 运行状态文案保持默认字体；启动/就绪等纯状态分支
                                // 不涉及模型名，原样渲染。
                                text = when {
                                    !state.ready -> AnnotatedString("runtime 启动中…")
                                    state.running -> buildAnnotatedString {
                                        withStyle(SpanStyle(fontFamily = MonoFont)) { append(modelDisplay) }
                                        append(" · Step ${state.step} · 工具 ${state.toolCalls}")
                                    }
                                    else -> if (modelDisplay.isEmpty()) AnnotatedString("就绪") else buildAnnotatedString {
                                        withStyle(SpanStyle(fontFamily = MonoFont)) { append(modelDisplay) }
                                    }
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
                        SoftButton(onClick = { RuntimeBridge.newSession() }) { Text("新会话") }
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
                    detectTapGestures(onTap = { collapseComposer() })
                }
                .padding(bottom = padding.calculateBottomPadding())
                .imePadding(),
        ) {
            // 背景捕获层：把列表内容录制进共享 GraphicsLayer；屏显也经由该层
            // 绘制（录制与显示共用一份 displaylist）。各玻璃面板在自身 draw
            // 阶段引用此层录制带出血带的小模糊层（见 GlassPanel）。
            // GraphicsLayer.renderEffect 依赖 RenderEffect，Android 12+ 才生效。
            // 同时标 hazeSource：弹层玻璃（GlassDropdownMenu）对此内容区做
            // Haze 局部真实模糊——源即弹层背后实际可见的内容区域。
            Box(
                Modifier
                    .fillMaxSize()
                    .hazeSource(hazeState)
                    .drawWithContent {
                        val layerSize = IntSize(
                            width = size.width.roundToInt(),
                            height = size.height.roundToInt(),
                        )
                        glass.source.record(size = layerSize) {
                            this@drawWithContent.drawContent()
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
                                    onOpenTool = { detailTool = it },
                                    hazeState = hazeState,
                                    // 气泡容器 onTap 空消费会拦住外层空白点击，
                                    // 面板展开时点气泡 = 收起面板（未聚焦时为
                                    // 无操作，仅保留事件消费边界）。
                                    onTapWhileFocused = { if (composerFocused) collapseComposer() },
                                    pendingQuestion = state.pendingQuestion,
                                    onOpenAskUser = onOpenAskUser,
                                    onOpenReview = onOpenReviewAskUser,
                                )
                                is RenderItem.ToolGroup -> ToolGroupCard(
                                    item.parts,
                                    onOpenTool = { detailTool = it },
                                )
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
            // 底部悬浮组：termux 停滞连接面板（stall 时从输入区上方展开）+ 输入区。
            // SharedTransitionLayout：输入框常驻状态行 ↔ 聚焦面板卡片之间的
            // sharedBounds 位移动画容器（key composer.model / composer.effort，
            // 见 Composer 状态行与 ModelPickerRow/EffortPickerRow 的锚点）。
            SharedTransitionLayout(
                modifier = Modifier
                    .align(Alignment.BottomCenter)
                    .fillMaxWidth(),
            ) {
                Column(Modifier.fillMaxWidth()) {
                    // ask_user 待答提醒 pill：pending 问卷滚出视口时浮出
                    //（TermuxStallBanner 同槽位同语言），点击滚动回卡片。
                    val askPending = state.pendingQuestion?.takeIf {
                        it.phase == "pending" || it.phase == "submitting"
                    }
                    val askItemIdx = askPending?.let { pq ->
                        renderItems.indexOfFirst {
                            it is RenderItem.Single &&
                                it.entry is UiEntry.ToolCall &&
                                it.entry.name == ASK_USER_TOOL &&
                                it.entry.fullArgs == pq.argsKey
                        }
                    }
                    val askCardVisible = askItemIdx != null && askItemIdx >= 0 &&
                        listState.layoutInfo.visibleItemsInfo.any { it.index == askItemIdx }
                    val listScope = rememberCoroutineScope()
                    PendingNudgePill(
                        visible = askPending?.phase == "pending" &&
                            askItemIdx != null && askItemIdx >= 0 && !askCardVisible,
                        count = askPending?.questions?.size ?: 0,
                        glass = glass,
                        rootCoords = { rootCoords },
                        onClick = {
                            askItemIdx?.takeIf { it >= 0 }?.let { idx ->
                                listScope.launch { listState.animateScrollToItem(idx) }
                            }
                        },
                    )
                    // Termux 停滞连接面板：stall 激活期间实时显示连接阶梯；
                    // 探测确认 Ready 时收起（连接事件已交 RuntimeBridge 续跑，
                    // 结果经既有链路回填，全部命令完成后 termuxStallSince 清零）。
                    val termuxStall by TermuxStallTracker.state.collectAsState()
                    TermuxStallBanner(
                        visible = state.running && state.termuxStallSince != null &&
                            termuxStall.status !is TermuxStatus.Ready,
                        stallSince = state.termuxStallSince,
                        status = termuxStall.status,
                        glass = glass,
                        rootCoords = { rootCoords },
                    )
                    // MCP host 恢复面板：mcp.recover_host 在途（mcpRecovering
                    // 非 null）时显示恢复阶梯（Termux 运行 → host 进程 →
                    // 工具注册）；恢复协程完成即收起——成功路径 Rust 装饰器
                    // 同名重发、结果无感回填；失败路径原因已并入工具错误。
                    // 与 TermuxStallBanner 独立并存（HostTool 兜底停滞与 MCP
                    // 死亡自愈分属两条链路，极端并存时两面板堆叠）。
                    val mcpRecover by RuntimeBridge.mcpRecoverPhase.collectAsState()
                    McpRecoverBanner(
                        visible = state.mcpRecovering != null,
                        since = state.mcpRecovering,
                        phase = mcpRecover,
                        termux = termuxStall.status,
                        glass = glass,
                        rootCoords = { rootCoords },
                    )
                    // 聚焦面板：composer 上方展开（与 TermuxStallBanner 同列同语言），
                    // 未聚焦时不参与组合 → composer 几何与未聚焦态完全一致。
                    ComposerFocusPanel(
                        visible = composerFocused,
                        modelAlias = state.modelAlias,
                        config = state.config,
                        // 选择器「直接选」双写：会话热切 + 持久化为启动默认
                        // （见 RuntimeBridge.selectModel）。
                        onSelectModel = { RuntimeBridge.selectModel(it) },
                        // Effort 直改条目配置：config 里原 ModelUi copy 改字段
                        // 后 upsertModel 回写（persist 落盘 + config_changed
                        // 回刷面板回显）；条目不在当前 config 时静默跳过。
                        onSelectEffort = { entry, effort ->
                            state.config.models.firstOrNull { it.entry == entry }?.let {
                                RuntimeBridge.upsertModel(it.copy(reasoningEffort = effort))
                            }
                        },
                        sharedTransitionScope = this@SharedTransitionLayout,
                        glass = glass,
                        rootCoords = { rootCoords },
                        hazeState = hazeState,
                    )
                    Composer(
                        glass = glass,
                        rootCoords = { rootCoords },
                        running = state.running,
                        onInputFocusChange = { composerFocused = it },
                        onSend = { text ->
                            RuntimeBridge.submit(text)
                            // 发送后退出编辑状态：清焦点 → 聚焦面板收起（键盘随之收起）。
                            focusManager.clearFocus()
                        },
                        onCancel = { RuntimeBridge.cancel() },
                        // 常驻状态行：仅未聚焦时显示（聚焦后同源信息移交面板卡片）。
                        showInlineStatus = !composerFocused,
                        inlineStatus = inlineStatus,
                        sharedTransitionScope = this@SharedTransitionLayout,
                    )
                }
            }

            // 工具详情弹窗（Dialog 自身是独立 window，不参与列表布局）。
            detailLive?.let { tc ->
                ToolDetailDialog(tc, onDismiss = { detailTool = null })
            }
        }
    }
}

/** 贴底判定余量（dp）：完全贴底时末条目底缘距视口底 = 底部 contentPadding（84dp），另加 12dp 手感容差。 */
private const val AT_BOTTOM_SLACK_DP = 96

/** 渲染投影：相邻的连续工具调用聚合为一组（中间夹的 Meta/StepBreak 一并吸收），其余条目单行。 */
private sealed class RenderItem {
    data class Single(val entry: UiEntry, val index: Int) : RenderItem()

    /** parts 保序混装工具调用与夹缝中的 Meta/StepBreak；tools 是其中的 ToolCall 子集。 */
    data class ToolGroup(val parts: List<UiEntry>, val startIdx: Int) : RenderItem()
}

/**
 * 末条目内容签名（类型 + 长度/终态）：智能贴底跟随的「同条目增长」触发源。
 * 流式 delta / reasoning 与工具输出回填都并入最后一条条目自身（entries.size
 * 不变），签名随之逐次变化，跟随触发器才「看得见」这些增长。
 */
private fun entrySig(e: UiEntry?): String = when (e) {
    null -> ""
    is UiEntry.User -> "u:${e.text.length}"
    is UiEntry.Assistant -> "a:${e.text.length}"
    is UiEntry.Reasoning -> "r:${e.text.length}"
    is UiEntry.Meta -> "m:${e.text.length}"
    // status/output 是 var，但 bridge 侧每次回填都以 copy 换新实例/新列表，
    // 这里的读取随重组刷新。
    is UiEntry.ToolCall -> "t:${e.status}:${e.output?.length ?: -1}"
    is UiEntry.Delegate -> "d:${e.done}"
    is UiEntry.Approval -> "p"
    UiEntry.StepBreak -> "b"
}

private fun groupEntries(entries: List<UiEntry>): List<RenderItem> {
    val out = mutableListOf<RenderItem>()
    var i = 0
    while (i < entries.size) {
        val e = entries[i]
        if (e is UiEntry.ToolCall) {
            // ask_user 的工具条目由 QuestionCard 吸收（内联问卷卡特化渲染），
            // 不进通用工具组、不出工具 chip；摘要/终态与 output 转写同源。
            if (e.name == ASK_USER_TOOL) {
                out.add(RenderItem.Single(e, i))
                i++
                continue
            }
            val start = i
            val parts = mutableListOf<UiEntry>()
            while (i < entries.size) {
                val cur = entries[i]
                when {
                    cur is UiEntry.ToolCall && cur.name != ASK_USER_TOOL -> { parts.add(cur); i++ }
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
private fun EntryRow(
    entry: UiEntry,
    activeLast: Boolean = false,
    onOpenTool: (UiEntry.ToolCall) -> Unit = {},
    hazeState: HazeState,
    onTapWhileFocused: () -> Unit = {},
    pendingQuestion: PendingQuestionUi? = null,
    onOpenAskUser: () -> Unit = {},
    onOpenReview: () -> Unit = {},
) {
    when (entry) {
        is UiEntry.User -> Bubble(entry.text, mine = true)
        is UiEntry.Assistant -> MarkdownBubble(entry.text, hazeState, onTapWhileFocused)
        is UiEntry.Reasoning -> ReasoningBlock(entry.text, entry.meta, entry.startTs, activeLast)
        is UiEntry.Meta -> Text(
            entry.text,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.tertiary,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        // 分组投影后正常不会到达；保留兜底（sealed when 完整性）。
        is UiEntry.ToolCall -> when {
            // ask_user 特化渲染：内联问卷卡 / 已答摘要 / 终态轻摘要。
            entry.name == ASK_USER_TOOL -> QuestionCardHost(
                tc = entry,
                pending = pendingQuestion,
                onOpenFullScreen = onOpenAskUser,
                onOpenReview = onOpenReview,
            )
            else -> ToolGroupCard(listOf(entry), onOpenTool)
        }
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
 * 单个条目点击弹出详情弹窗（完整命令 + 输出）。
 */
private val TOOL_GROUP_RADIUS = 12.dp

@Composable
private fun ToolGroupCard(parts: List<UiEntry>, onOpenTool: (UiEntry.ToolCall) -> Unit = {}) {
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
                    // 弹窗打开时也能直观看出每个工具调用的边界。
                    if (idx > 0) {
                        HorizontalDivider(
                            thickness = 0.5.dp,
                            color = groupBorder,
                        )
                    }
                    // 条目自身的圆角规则：首条目上圆、末条目下圆。
                    val bottomR = if (idx == tools.lastIndex) TOOL_GROUP_RADIUS else 0.dp
                    val shape = RoundedCornerShape(
                        topStart = if (idx == 0) TOOL_GROUP_RADIUS else 0.dp,
                        topEnd = if (idx == 0) TOOL_GROUP_RADIUS else 0.dp,
                        bottomEnd = bottomR,
                        bottomStart = bottomR,
                    )
                    ToolChipRow(part, shape, onOpenTool)
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

/** 单个工具条目：状态图标 + 名称 + 参数摘要，点击弹出详情弹窗（完整命令 + 输出）。 */
@Composable
private fun ToolChipRow(
    tc: UiEntry.ToolCall,
    shape: RoundedCornerShape,
    onOpen: (UiEntry.ToolCall) -> Unit,
) {
    // 不用 Surface(onClick)：M3 会强制 48dp 最小触摸目标，把条目撑高、组内出现大空隙。
    // 改用 foundation 组合（clip+background+clickable）精确控制尺寸；
    // 边框由 ToolGroupCard 统一绘制，条目只负责底色。
    // 状态着色：成功淡绿、失败淡红、执行中保持中性紫（均跟随深浅色主题；
    // 绿色经 StatusColors 适配深色模式，此前写死浅绿在深色下刺眼）。
    // tool_end 事件不带失败状态，native 工具的失败以非 0 exit_code 表达，
    // 直接从输出里识别。
    val exitFail = Regex("""exit_code: (?:[1-9]\d*)""").containsMatchIn(tc.output.orEmpty())
    val ok = tc.status == "done" && !exitFail
    val failed = tc.status == "failed" || exitFail
    val statusTint = when {
        ok -> successGreen()
        failed -> MaterialTheme.colorScheme.error
        else -> MaterialTheme.colorScheme.secondary
    }
    val chipColor = when {
        ok -> successGreenContainer()
        failed -> MaterialTheme.colorScheme.errorContainer.copy(alpha = 0.3f)
        else -> MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.4f)
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .clip(shape)
            .background(chipColor)
            .clickable { onOpen(tc) }
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
    }
}

/** 工具输出的结构化拆解（与后端 format_output 的分段约定一致）。 */
private class ToolOutParts(val exitCode: String?, val stdout: String, val stderr: String)

/**
 * 取更长的输出（null 视为空）。懒加载全量与条目自带输出（host 缓存 /
 * 事件 preview / 恢复快照）各有更完整的场景，按长度择优，两边都不丢。
 */
private fun longerOf(a: String?, b: String?): String? =
    if ((a ?: "").length >= (b ?: "").length) a else b

/**
 * 拆解工具输出：`--- stdout ---` / `--- stderr ---` 分段（native bash）；
 * 非格式化输出（如 termux 回传的合并文本）整体按 stdout 处理；
 * exit_code 行（"exit_code: N"）提取为独立字段。
 */
private fun parseToolOut(raw: String): ToolOutParts {
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
    val exitCode = Regex("""exit_code: (-?\d+)""").find(raw)?.groupValues?.get(1)
    return ToolOutParts(exitCode, stdout, stderr)
}

/**
 * 工具详情弹窗：完整命令 + 完整输出（stdout/stderr 切换，无行数上限、
 * 可滚动），文本经 SelectionContainer 长按选择复制；底部提供整段一键复制。
 * 替代原就地展开视图（maxLines=16 硬限制 + 不可滚动，长输出看不全）。
 * 执行中的工具打开时显示等待文案，完成输出经 ChatScreen 的实时回查刷新。
 *
 * 懒加载全量输出：条目携带 callId 且非 running / 非 ask_user 时，经 FFI
 * 从 Rust 内存 history 拉取 max_output_bytes 上限内的完整版本，与条目
 * 自带输出（host 工具的 Kotlin 缓存 / 事件 preview / 恢复快照）**取更
 * 长者**显示——host 缓存可能比 DB 版本更长且未截断，DB 版有 secret 打
 * 码，两者都安全，丢任何一边都可能缺内容。旧条目无 callId / FFI 未命
 * 中（history 被压缩、子代理工具）→ 静默回退 preview，维持现状显示。
 */
@Composable
private fun ToolDetailDialog(tc: UiEntry.ToolCall, onDismiss: () -> Unit) {
    val clipboard = LocalClipboardManager.current
    val scroll = rememberScrollState()
    // 系统等宽（不用 MonoFont 子集）：工具输出是任意内容，子集字形覆盖不全。
    val mono = MaterialTheme.typography.bodySmall.copy(
        fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
    )
    // 懒加载资格：有 callId + 已结束 + 非 ask_user（其条目输出即权威脱敏
    // 终态，绝不懒加载替换）。
    val lazyEligible = tc.callId != null && tc.status != "running" && tc.name != RuntimeBridge.ASK_USER_TOOL
    var fullOut by remember { mutableStateOf<String?>(null) }
    // 载入完成标记：区分「拉取中」（显示进度行）与「未命中返回 null」
    // （隐进度行，回退 preview）。
    var fullLoaded by remember { mutableStateOf(!lazyEligible) }
    LaunchedEffect(tc.callId, tc.status) {
        if (lazyEligible) {
            fullOut = RuntimeBridge.fetchToolOutput(tc.callId!!)
            fullLoaded = true
        } else {
            fullOut = null
            fullLoaded = true
        }
    }
    // 取更长者：懒加载全量与条目自带输出（host 缓存/preview/恢复快照）。
    val shownOut = longerOf(fullOut, tc.output)
    val parts = remember(shownOut) { parseToolOut(shownOut.orEmpty()) }
    var tab by remember(shownOut) { mutableStateOf(0) }
    val running = tc.status == "running"

    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        Surface(
            shape = RoundedCornerShape(16.dp),
            color = MaterialTheme.colorScheme.surface,
            modifier = Modifier
                .fillMaxWidth(0.96f)
                .fillMaxHeight(0.9f),
        ) {
            Column {
                // 头部：工具名 + exit code 徽标 + 关闭。
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.padding(start = 16.dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
                ) {
                    Text(
                        tc.name,
                        style = MaterialTheme.typography.titleSmall,
                        modifier = Modifier.weight(1f),
                    )
                    if (parts.exitCode != null) {
                        val ok = parts.exitCode == "0"
                        val tint = if (ok) {
                            successGreen()
                        } else {
                            MaterialTheme.colorScheme.error
                        }
                        Text(
                            "exit ${parts.exitCode}",
                            style = MaterialTheme.typography.labelSmall.copy(
                                // 系统等宽：与输出体 mono 同源（见上），不换子集字体。
                                fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                            ),
                            color = tint,
                            modifier = Modifier
                                .clip(RoundedCornerShape(50))
                                .background(tint.copy(alpha = 0.12f))
                                .border(0.5.dp, tint.copy(alpha = 0.4f), RoundedCornerShape(50))
                                .padding(horizontal = 10.dp, vertical = 3.dp),
                        )
                        Spacer(Modifier.width(8.dp))
                    }
                    IconButton(onClick = onDismiss) {
                        Icon(Icons.Filled.Close, contentDescription = "关闭")
                    }
                }
                HorizontalDivider(
                    thickness = 0.5.dp,
                    color = MaterialTheme.colorScheme.outline.copy(alpha = 0.3f),
                )
                // 内容区：命令 + 输出，整体可滚动、可选择。
                Column(
                    Modifier
                        .weight(1f)
                        .verticalScroll(scroll)
                        .padding(horizontal = 16.dp, vertical = 10.dp),
                ) {
                    // 懒加载进度行（极轻）：FFI 拉取全量输出期间显示，到货即隐。
                    if (!fullLoaded) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier.padding(bottom = 8.dp),
                        ) {
                            CircularProgressIndicator(
                                strokeWidth = 1.5.dp,
                                modifier = Modifier.size(13.dp),
                            )
                            Spacer(Modifier.width(8.dp))
                            Text(
                                "载入完整输出",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                    }
                    Text(
                        "命令",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                    SelectionContainer {
                        Text(tc.fullArgs.ifBlank { "（无参数）" }, style = mono)
                    }
                    Spacer(Modifier.height(12.dp))
                    // 输出区头：标签 + stdout/stderr 切换 + 字符数提示。
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Text(
                            if (running) "输出（执行中…）" else "输出",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.primary,
                        )
                        Spacer(Modifier.width(12.dp))
                        if (parts.stderr.isNotEmpty()) {
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
                                        .border(0.5.dp, labelColor.copy(alpha = 0.4f), RoundedCornerShape(50))
                                        .clickable { tab = idx }
                                        .padding(horizontal = 10.dp, vertical = 3.dp),
                                )
                                Spacer(Modifier.width(6.dp))
                            }
                        }
                        Spacer(Modifier.weight(1f))
                        if (!running) {
                            val shownLen = if (tab == 1) parts.stderr.length else parts.stdout.length
                            Text(
                                "$shownLen 字符",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                    }
                    Spacer(Modifier.height(4.dp))
                    if (running) {
                        Text(
                            "执行中，完成后自动显示…",
                            style = mono,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    } else {
                        SelectionContainer {
                            Text(
                                when {
                                    parts.stderr.isNotEmpty() && tab == 1 ->
                                        parts.stderr.ifBlank { "（空）" }
                                    parts.stdout.isNotBlank() -> parts.stdout
                                    else -> "（无输出）"
                                },
                                style = mono,
                            )
                        }
                    }
                }
                HorizontalDivider(
                    thickness = 0.5.dp,
                    color = MaterialTheme.colorScheme.outline.copy(alpha = 0.3f),
                )
                // 底部动作行：整段一键复制（配合长按手动选择）。
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 8.dp, vertical = 4.dp),
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    SoftButton(
                        onClick = { clipboard.setText(AnnotatedString(tc.fullArgs)) },
                        enabled = tc.fullArgs.isNotBlank(),
                    ) {
                        Icon(
                            Icons.Filled.ContentCopy,
                            contentDescription = null,
                            modifier = Modifier.size(14.dp),
                        )
                        Spacer(Modifier.width(4.dp))
                        Text("复制命令")
                    }
                    SoftButton(
                        onClick = { clipboard.setText(AnnotatedString(parts.stdout)) },
                        enabled = !running && parts.stdout.isNotBlank(),
                    ) {
                        Icon(
                            Icons.Filled.ContentCopy,
                            contentDescription = null,
                            modifier = Modifier.size(14.dp),
                        )
                        Spacer(Modifier.width(4.dp))
                        Text("复制 stdout")
                    }
                    if (parts.stderr.isNotEmpty()) {
                        SoftButton(
                            onClick = { clipboard.setText(AnnotatedString(parts.stderr)) },
                            enabled = !running,
                        ) {
                            Icon(
                                Icons.Filled.ContentCopy,
                                contentDescription = null,
                                modifier = Modifier.size(14.dp),
                            )
                            Spacer(Modifier.width(4.dp))
                            Text("复制 stderr")
                        }
                    }
                }
            }
        }
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

/**
 * 模型回复气泡：增强 Markdown 渲染（高亮代码块/图片 SVG/HTML 降级，紧凑字号）。
 *
 * 交互（原「点击整段复制」已移除——误触率高，且长文一键复制鲜有用处）：
 * - 长按弹上下文菜单（玻璃弹层，锚定长按触点 + 触觉反馈）：复制全文 /
 *   查看源码。
 * - tap：消费事件（不透传底层），面板展开时 = 收起面板（[onTapWhileFocused]
 *   回调内部判聚焦）；子元素（链接、代码块头部按钮、代码可选文本）先于
 *   容器命中——链接点击不受影响；长按代码块内文本仍走文本选择，不弹菜单。
 */
@Composable
private fun MarkdownBubble(
    text: String,
    hazeState: HazeState,
    onTapWhileFocused: () -> Unit = {},
) {
    val clipboard = LocalClipboardManager.current
    val haptics = LocalHapticFeedback.current
    // 上下文菜单触点锚点（null = 菜单关闭）；源码查看器开关。
    var menuAnchor by remember { mutableStateOf<Offset?>(null) }
    var showSource by remember { mutableStateOf(false) }
    // 空心气泡：不用实底色（surfaceVariant 浅色下发白、与页面难分），改为
    // 透明背景 + 1dp outlineVariant 描边；与用户气泡（primaryContainer 实底）
    // 形成"空心/实心"对比，深浅色主题都成立。
    Surface(
        shape = RoundedCornerShape(
            topStart = 4.dp,
            topEnd = 16.dp,
            bottomStart = 16.dp,
            bottomEnd = 16.dp,
        ),
        color = Color.Transparent,
        border = BorderStroke(1.dp, MaterialTheme.colorScheme.outlineVariant),
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp)
            .pointerInput(Unit) {
                detectTapGestures(
                    // 消费 tap 维持既有事件边界（不透传底层）；面板展开时
                    // 点气泡 = 收起面板（回调内部已判聚焦，未聚焦为无操作）。
                    // 点击自动复制早已移除。
                    onTap = { onTapWhileFocused() },
                    onLongPress = { offset ->
                        haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                        menuAnchor = offset
                    },
                )
            },
    ) {
        // 标题降档等排版细节内聚在 EnhancedMarkdown 中。
        EnhancedMarkdown(
            text = text,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
        )
        // 触点锚定的零尺寸 Box：GlassDropdownMenu 的 Popup 以父布局锚定，
        // 菜单在触点下方展开（放不下自动上翻，横向超窗夹取进屏内）。
        menuAnchor?.let { anchor ->
            Box(
                Modifier
                    .offset { IntOffset(anchor.x.roundToInt(), anchor.y.roundToInt()) }
                    .size(0.dp),
            ) {
                GlassDropdownMenu(
                    expanded = true,
                    onDismissRequest = { menuAnchor = null },
                    hazeState = hazeState,
                    items = listOf(
                        GlassDropdownItem(
                            value = "copy",
                            label = "复制",
                            leadingIcon = Icons.Filled.ContentCopy,
                        ),
                        GlassDropdownItem(
                            value = "source",
                            label = "查看源码",
                            leadingIcon = Icons.Filled.Code,
                        ),
                    ),
                    onSelect = { opt ->
                        when (opt.value) {
                            "copy" -> clipboard.setText(AnnotatedString(text))
                            "source" -> showSource = true
                        }
                    },
                )
            }
        }
    }
    if (showSource) {
        SourceCodeDialog(text = text, onDismiss = { showSource = false })
    }
}

/**
 * 源码查看器：气泡原始 Markdown 带语法高亮展示。高亮管线与气泡内代码块
 * 同源——SyntaxMP `markdown` 语法整段 tokenize，其 grammar 内置 fence 嵌套
 * （代码块内容按围栏语言二次着色：kotlin/python/xml 等各自出 keyword/
 * string/tag 角色），markdown 结构角色（标题/强调/引用/markup.*）与嵌套
 * 代码角色共用代码块同一套 GitHub light/dark 配色映射；tokenize 较重，
 * produceState 异步计算、首帧先显示纯文本。硬约束保持：SelectionContainer
 * 选中复制片段、softWrap 自动换行、仅纵向滚动、系统 FontFamily.Monospace
 * 全字形（禁 JetBrains Mono 子集，见 ui/Fonts.kt 边界）。
 *
 * 布局：无标题行（不占标题空间），自定义 Dialog（usePlatformDefaultWidth
 * = false）近全屏铺开、整块即代码底（surfaceVariant），右上角小号关闭
 * 按钮（圆形，悬浮于内容之上）——关闭途径 = BACK + 该按钮。
 */
@Composable
private fun SourceCodeDialog(text: String, onDismiss: () -> Unit) {
    val dark = isSystemInDarkTheme()
    val tokenizer = remember { SyntaxTokenizer() }
    // 角色配色随明暗主题切换（与气泡内代码块同一套映射）。
    val theme = remember(dark) { syntaxRoleColors(dark) }
    val highlighted = produceState(
        initialValue = remember(text) { AnnotatedString(text) },
        text, tokenizer, theme,
    ) {
        value = buildSyntaxAnnotatedString(text, "markdown", tokenizer, theme)
    }.value
    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        Box(
            Modifier
                .padding(horizontal = 10.dp)
                .padding(vertical = 20.dp)
                .fillMaxWidth()
                .heightIn(max = 620.dp)
                .clip(RoundedCornerShape(16.dp))
                .background(MaterialTheme.colorScheme.surfaceVariant)
                .border(0.5.dp, MaterialTheme.colorScheme.outlineVariant, RoundedCornerShape(16.dp)),
        ) {
            SelectionContainer {
                Box(
                    Modifier
                        .fillMaxWidth()
                        .verticalScroll(rememberScrollState())
                        // 顶部 40dp 让出关闭按钮悬浮区（滚动内容从其下穿过）。
                        .padding(top = 40.dp, start = 12.dp, end = 12.dp, bottom = 12.dp),
                ) {
                    Text(
                        highlighted,
                        style = MaterialTheme.typography.bodySmall.copy(
                            fontFamily = FontFamily.Monospace,
                        ),
                        color = MaterialTheme.colorScheme.onSurface,
                        softWrap = true,
                    )
                }
            }
            Box(
                contentAlignment = Alignment.Center,
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .padding(6.dp)
                    .size(28.dp)
                    .clip(CircleShape)
                    .background(MaterialTheme.colorScheme.surface)
                    .clickable { onDismiss() },
            ) {
                Icon(
                    Icons.Filled.Close,
                    contentDescription = "关闭",
                    tint = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.size(16.dp),
                )
            }
        }
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

/** 磨砂玻璃共享状态：内容捕获层 + 模糊参数（effect 为 null 表示不支持真模糊，退化为蒙层）。
 *  internal：聚焦面板（ComposerPanel.kt）复用同一 GlassPanel 视觉语言。 */
internal class Glass(
    val source: GraphicsLayer,
    val effect: BlurEffect?,
    val radiusPx: Float,
    val base: Color,
)

/** 模糊半径：12dp 轻磨砂（此前 20dp 观感偏重，可按需微调）。 */
private val GlassBlurRadius = 12.dp

@Composable
internal fun rememberGlass(): Glass {
    val source = rememberGraphicsLayer()
    val radiusPx = with(LocalDensity.current) { GlassBlurRadius.toPx() }
    val effect = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
        remember(radiusPx) {
            BlurEffect(radiusX = radiusPx, radiusY = radiusPx, edgeTreatment = TileMode.Clamp)
        }
    } else {
        null
    }
    val base = MaterialTheme.colorScheme.surface
    return remember(source, radiusPx, effect, base) { Glass(source, effect, radiusPx, base) }
}

/**
 * 磨砂玻璃面板（真·背景模糊，参考 Haze 的实现思路、零第三方依赖）。
 * 每个面板在自身 draw 阶段录制一块「面板区域 + 四周各一个模糊半径出血带」的
 * 小图层：先铺不透明底色（整层不透明才能盖住底下未模糊的原文，否则细文字处
 * 模糊 alpha 过薄、透出清晰重影），再平移绘制共享的内容捕获层，套 BlurEffect
 * 后回绘到自身位置。出血带保证可见区边缘的卷积核也能采到真实内容——没有它，
 * Clamp 会在面板边缘把贴边像素拉丝抹进玻璃内侧，滚动时贴边内容逐帧变化，
 * 表现为玻璃上下边缘闪烁。面板级小层也远比"整屏模糊层 × 2 面板"省 GPU。
 * 低版本（renderEffect 依赖 API 31 的 RenderEffect）退化为纯半透明蒙层。
 */
@Composable
internal fun GlassPanel(
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
    tint: Color,
    modifier: Modifier = Modifier,
    shape: Shape = RectangleShape,
    border: Boolean = false,
    content: @Composable BoxScope.() -> Unit,
) {
    var offsetInRoot by remember { mutableStateOf(Offset.Zero) }
    val panelLayer = rememberGraphicsLayer()
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
                if (glass.effect != null) {
                    val o = glass.radiusPx
                    val bleed = IntSize(
                        width = (size.width + 2 * o).roundToInt(),
                        height = (size.height + 2 * o).roundToInt(),
                    )
                    panelLayer.record(size = bleed) {
                        // 不透明底 + 平移的内容捕获层：模糊输入整体不透明。
                        drawRect(glass.base)
                        translate(left = o - offsetInRoot.x, top = o - offsetInRoot.y) {
                            drawLayer(glass.source)
                        }
                    }
                    panelLayer.renderEffect = glass.effect
                    // 回绘时内缩一个出血带：可见区恰好对应面板自身区域。
                    translate(left = -o, top = -o) {
                        drawLayer(panelLayer)
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
 * 聚焦状态经 [onInputFocusChange] 上抛，驱动 ChatScreen 底部的聚焦面板。
 *
 * 常驻状态块（[showInlineStatus] = 未聚焦）：输入区居左，**原字号**单行
 * （bodyLarge 16sp/24sp 行盒 + 10dp 上下边距，与无状态行时代完全一致 →
 * 输入列单独把条高精确锚在原单条高度，任意 fontScale 成立）；右侧上下
 * 两行小字块贴右缘——上行 模型名 · effort 档，下行 Termux/MCP/tools
 * （labelSmall 族，块高恒小于输入列预算，垂直居中不撑高）。输入文字横向
 * 溢出末尾 DstIn 离屏渐隐。整条点按 = 聚焦展开（FocusRequester 直打输入
 * 框，不拦输入区可点性）。聚焦后状态块横向收出、输入区扩至全宽恢复多行。
 * 模型名 / effort 档与聚焦面板卡片做 sharedBounds 位移动画（key
 * composer.model / composer.effort，320ms FastOutSlowInEasing，收起反向）
 * ——状态块其余部分尺寸与卡片锚点差异大，降级为随面板容器过渡。
 */
@Composable
private fun Composer(
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
    running: Boolean,
    onInputFocusChange: (Boolean) -> Unit = {},
    onSend: (String) -> Unit,
    onCancel: () -> Unit,
    showInlineStatus: Boolean,
    inlineStatus: ComposerInlineStatus,
    sharedTransitionScope: SharedTransitionScope,
    modifier: Modifier = Modifier,
) {
    var input by remember { mutableStateOf("") }
    // 常驻状态行「整块点按 = 聚焦展开」：行本身不拦输入区可点性，
    // 点击经 FocusRequester 直打输入框（焦点 → 面板展开 + IME 弹出，
    // 与直接点输入框同一路径）。
    val inputFocusRequester = remember { FocusRequester() }
    // 输入区字号/行盒/上下 padding 一律保持原单条输入条的原始值
    //（bodyLarge 16sp/24sp 行盒 + 10dp 上下边距）→ 输入列单独就把条高
    // 锚在原单条高度（20dp + 24sp，任意 fontScale 精确成立）；右侧状态
    // 块（两行 labelSmall = 32sp + 1dp 间隙）恒小于该预算，垂直居中于
    // 条内不撑高。
    // 未聚焦单行横向溢出标记（onTextLayout 布局相位回填）：驱动文字
    // 末尾 DstIn 渐隐遮罩。
    var inputOverflow by remember { mutableStateOf(false) }
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
                // 未聚焦左右布局：输入区在左（weight 占剩余宽，原字号单行
                // + 末尾渐隐），右侧上下两行状态块贴右缘（wrap 内容）。聚焦
                // 后状态块横向收出、输入区扩至全宽恢复多行。
                Row(verticalAlignment = Alignment.CenterVertically) {
                    BasicTextField(
                        value = input,
                        onValueChange = { input = it },
                        textStyle = MaterialTheme.typography.bodyLarge.copy(
                            color = MaterialTheme.colorScheme.onSurface,
                        ),
                        cursorBrush = SolidColor(MaterialTheme.colorScheme.primary),
                        maxLines = if (showInlineStatus) 1 else 5,
                        onTextLayout = { result ->
                            if (showInlineStatus) inputOverflow = result.hasVisualOverflow
                        },
                        modifier = Modifier
                            .weight(1f)
                            .focusRequester(inputFocusRequester)
                            .onFocusChanged { onInputFocusChange(it.isFocused) }
                            .padding(horizontal = 14.dp, vertical = 10.dp)
                            // 溢出末尾渐隐需独立离屏层：DstIn 只与自身内容
                            // 运算，否则会透掉玻璃底（同模型名遮罩）。蒙层在
                            // padding 之内 → 渐变恰好收在文字区右缘，淡出
                            // 完成后才轮到状态块内容。
                            .then(
                                if (showInlineStatus && inputOverflow) {
                                    Modifier
                                        .graphicsLayer {
                                            compositingStrategy = CompositingStrategy.Offscreen
                                        }
                                        .drawWithContent {
                                            drawContent()
                                            drawRect(
                                                brush = Brush.horizontalGradient(
                                                    0f to Color.Black,
                                                    0.75f to Color.Black,
                                                    1f to Color.Black.copy(alpha = 0f),
                                                ),
                                                blendMode = BlendMode.DstIn,
                                            )
                                        }
                                } else {
                                    Modifier
                                },
                            ),
                        decorationBox = { inner ->
                            Box {
                                if (input.isEmpty()) {
                                    Text(
                                        "发消息…",
                                        style = MaterialTheme.typography.bodyLarge,
                                        color = MaterialTheme.colorScheme.outline,
                                        maxLines = 1,
                                    )
                                }
                                inner()
                            }
                        },
                    )
                    AnimatedVisibility(
                        visible = showInlineStatus,
                        // 状态块水平收放（贴右缘）：聚焦时向右收出，未聚焦
                        // 反向从右缘展开；与面板 320ms 过渡同一时钟家族，
                        // 输入区随 weight 逐帧让位/扩宽，无字号跳变。
                        enter = fadeIn() + expandHorizontally(
                            expandFrom = Alignment.End,
                            animationSpec = tween(320, easing = FastOutSlowInEasing),
                        ),
                        exit = fadeOut() + shrinkHorizontally(
                            shrinkTowards = Alignment.End,
                            animationSpec = tween(320, easing = FastOutSlowInEasing),
                        ),
                    ) {
                        ComposerInlineStatusRow(
                            status = inlineStatus,
                            sharedTransitionScope = sharedTransitionScope,
                            animatedVisibilityScope = this,
                            onFocusInput = { inputFocusRequester.requestFocus() },
                        )
                    }
                }
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

/**
 * 常驻状态行模型名三态降级阶段：FULL（Provider/modelId 全串）→ SHORT
 * （仅 modelId）→ FADE（短版 + 末尾渐隐）。只进不退——一次定型 + 随
 * 模型名变化重置（remember key），杜绝临界宽度来回翻转闪烁。
 */
private enum class ModelNameStage { FULL, SHORT, FADE }

/**
 * 输入框内常驻状态块（未聚焦）：右侧上下两行小字（labelSmall 族），wrap
 * 内容贴玻璃容器右缘——上行 模型名 · effort 档，下行 Termux · MCP N ·
 * tools N。块高（两行 16sp 行盒 + 1dp 间隙）恒小于输入列的 20dp + 24sp
 * 预算，垂直居中于原单条高度内不撑高（高度账见 Composer）。模型名三态
 * 降级见 [ModelNameStage]（宽压 110dp 上限；无障碍语义固定读全串，视觉
 * 降级不牺牲朗读）。整块点按 = 聚焦输入框（见 Composer 的 FocusRequester
 * 说明）。模型名 / effort 档带 sharedBounds 锚点，与聚焦面板卡片的两个
 * 下拉值文本配对位移。
 */
@Composable
private fun ComposerInlineStatusRow(
    status: ComposerInlineStatus,
    sharedTransitionScope: SharedTransitionScope,
    animatedVisibilityScope: AnimatedVisibilityScope,
    onFocusInput: () -> Unit,
) {
    var nameStage by remember(status.modelDisplay) { mutableStateOf(ModelNameStage.FULL) }
    Column(
        horizontalAlignment = Alignment.End,
        modifier = Modifier
            .clickable(onClickLabel = "展开输入") { onFocusInput() }
            // 贴右缘：end 14dp 对齐容器内边距，start 2dp 呼吸位紧挨输入区
            // 渐隐尾部（输入区自身右 padding 14dp 是主间隙）。
            .padding(start = 2.dp, end = 14.dp),
    ) {
        // 上行：模型名 · effort 档。
        Row(verticalAlignment = Alignment.CenterVertically) {
            val modelBounds = with(sharedTransitionScope) {
                Modifier.sharedBounds(
                    rememberSharedContentState(key = "composer.model"),
                    animatedVisibilityScope = animatedVisibilityScope,
                    boundsTransform = { _, _ -> tween(320, easing = FastOutSlowInEasing) },
                )
            }
            Text(
                // 三态：全串 → 仅 modelId → 短版末尾渐隐（Clip + 内容级 DstIn
                // 渐变遮罩——遮的是内容本身，玻璃底上正确；不用省略号字符）。
                // MonoFont 与面板下拉当前值同字体（sharedBounds 位移过渡）。
                text = buildAnnotatedString {
                    withStyle(SpanStyle(fontFamily = MonoFont)) {
                        append(
                            if (nameStage == ModelNameStage.FULL) {
                                status.modelDisplay
                            } else {
                                status.modelId
                            },
                        )
                    }
                },
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.primary,
                maxLines = 1,
                overflow = if (nameStage == ModelNameStage.FADE) {
                    TextOverflow.Clip
                } else {
                    TextOverflow.Ellipsis
                },
                onTextLayout = { result ->
                    when (nameStage) {
                        ModelNameStage.FULL -> if (result.hasVisualOverflow) {
                            nameStage = ModelNameStage.SHORT
                        }
                        ModelNameStage.SHORT -> if (result.hasVisualOverflow) {
                            nameStage = ModelNameStage.FADE
                        }
                        ModelNameStage.FADE -> Unit
                    }
                },
                modifier = modelBounds
                    // 右侧状态块 wrap 内容：模型名压到固定上限，三态降级在
                    // 上限内逐级触发（FULL → SHORT → FADE）。
                    .widthIn(max = 110.dp)
                    // 淡出遮罩需独立离屏层：DstIn 只与自身内容运算，
                    // 否则会透掉玻璃底。
                    .graphicsLayer { compositingStrategy = CompositingStrategy.Offscreen }
                    .drawWithContent {
                        drawContent()
                        if (nameStage == ModelNameStage.FADE) {
                            drawRect(
                                brush = Brush.horizontalGradient(
                                    0f to Color.Black,
                                    0.75f to Color.Black,
                                    1f to Color.Black.copy(alpha = 0f),
                                ),
                                blendMode = BlendMode.DstIn,
                            )
                        }
                    }
                    // 视觉可能已降级为短版/渐隐，语义固定读完整
                    // 「Provider/modelId」。
                    .clearAndSetSemantics { contentDescription = status.modelDisplay },
            )
            if (status.effortLabel != null) {
                Spacer(Modifier.width(3.dp))
                val effortBounds = with(sharedTransitionScope) {
                    Modifier.sharedBounds(
                        rememberSharedContentState(key = "composer.effort"),
                        animatedVisibilityScope = animatedVisibilityScope,
                        boundsTransform = { _, _ -> tween(320, easing = FastOutSlowInEasing) },
                    )
                }
                Text(
                    status.effortLabel,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    modifier = effortBounds,
                )
            }
        }
        // 下行：Termux · MCP N · tools N（右对齐，图标全 Material）。
        Spacer(Modifier.height(1.dp))
        Row(verticalAlignment = Alignment.CenterVertically) {
            Icon(
                Icons.Outlined.Terminal,
                // 文案作状态点语义（运行中/未连接/检测中…），行内空间留给图形。
                contentDescription = status.termuxText,
                modifier = Modifier.size(11.dp),
                tint = status.termuxColor,
            )
            Spacer(Modifier.width(4.dp))
            Icon(
                painter = painterResource(R.drawable.ic_mcp),
                contentDescription = null,
                modifier = Modifier.size(11.dp),
                tint = if ((status.mcpConnected ?: 0) > 0) {
                    MaterialTheme.colorScheme.secondary
                } else {
                    MaterialTheme.colorScheme.outline
                },
            )
            Spacer(Modifier.width(2.dp))
            Text(
                status.mcpConnected?.toString() ?: "—",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
            Spacer(Modifier.width(5.dp))
            Icon(
                Icons.Outlined.Build,
                contentDescription = null,
                modifier = Modifier.size(11.dp),
                tint = if ((status.agentTools ?: 0) > 0) {
                    MaterialTheme.colorScheme.secondary
                } else {
                    MaterialTheme.colorScheme.outline
                },
            )
            Spacer(Modifier.width(2.dp))
            Text(
                status.agentTools?.toString() ?: "—",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
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
                SoftButton(onClick = { onAnswer(pending.id, "approve_all") }) { Text("全部允许") }
                SoftButton(onClick = { onAnswer(pending.id, "approve") }) { Text("允许") }
                SoftButton(onClick = { onAnswer(pending.id, "deny") }) { Text("拒绝") }
            }
        }
    }
}

/**
 * Termux 停滞连接面板：termux 命令发出 0.5s 仍无应答时悬浮在输入区上方
 * （与输入胶囊同语言：磨砂玻璃面板 + 细边框 + 轻阴影）。**检查清单面板**
 * （与设置页 MCP 控制区同一套视觉，复用 StatusChecklist）：按探测阶梯
 * （TermuxStallTracker → detectTermuxStatus：MCP host 探活 → 安装/权限
 * → echo ok 通道往返）映射为四行检查记录（已安装/权限/已运行/通道应答），
 * 完成项绿色对勾、卡点 spinner + 对应操作按钮；stall 激活期间聊天页
 * 周期重探 + ON_RESUME 重探，连接恢复（Ready）经 RuntimeBridge 原 id
 * 重发续跑、结果回填后（termuxStallSince 变 null）经 AnimatedVisibility
 * 自动收起（收起动画期间可见全绿清单 + 收尾语）。
 */
@Composable
private fun TermuxStallBanner(
    visible: Boolean,
    stallSince: Long?,
    status: TermuxStatus,
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
) {
    AnimatedVisibility(
        visible = visible,
        enter = fadeIn() + expandVertically(expandFrom = Alignment.Bottom),
        exit = fadeOut() + shrinkVertically(shrinkTowards = Alignment.Bottom),
    ) {
        val context = LocalContext.current
        // 「打开 Termux」点击后的轻量反馈：按钮文字切换 + 禁用重复点击。
        var pulled by remember { mutableStateOf(false) }
        // 已等待秒数（每秒跳一次；stallSince 换新则重算）。
        var waitedSec by remember(stallSince) { mutableStateOf(0) }
        LaunchedEffect(stallSince) {
            if (stallSince == null) return@LaunchedEffect
            while (true) {
                waitedSec =
                    ((System.currentTimeMillis() - stallSince) / 1000).toInt().coerceAtLeast(0)
                delay(1_000)
            }
        }
        // 检查记录行集合：探测阶梯 → 四行状态映射（阶梯短路的语义 = 前置
        // 项通过才轮到当前卡点；未轮到的行保持 Pending 不抢注意力）。
        val rows = buildList {
            add(
                StatusCheckRow(
                    state = when (status) {
                        is TermuxStatus.Checking -> StatusCheckState.InProgress
                        is TermuxStatus.NotInstalled -> StatusCheckState.Failed
                        else -> StatusCheckState.Done
                    },
                    title = "Termux 已安装",
                    summary = if (status is TermuxStatus.NotInstalled) {
                        "bash 依赖 Termux 执行命令；安装后打开一次再回到本页"
                    } else {
                        null
                    },
                    actionLabel = if (status is TermuxStatus.NotInstalled) "去安装" else null,
                    onAction = if (status is TermuxStatus.NotInstalled) {
                        {
                            // Termux 不在 Play 上架：开 F-Droid 详情页（浏览器
                            // 兜底；无浏览器静默失败不崩溃）。
                            runCatching {
                                context.startActivity(
                                    Intent(
                                        Intent.ACTION_VIEW,
                                        Uri.parse("https://f-droid.org/en/packages/com.termux/"),
                                    ),
                                )
                            }
                        }
                    } else {
                        null
                    },
                ),
            )
            add(
                StatusCheckRow(
                    state = when (status) {
                        is TermuxStatus.Checking, is TermuxStatus.NotInstalled -> StatusCheckState.Pending
                        is TermuxStatus.NoPermission -> StatusCheckState.Failed
                        else -> StatusCheckState.Done
                    },
                    title = "RUN_COMMAND 权限",
                    summary = if (status is TermuxStatus.NoPermission) {
                        "系统设置 → 应用 → OpenSlate → 权限，或经 Shizuku 一键授予"
                    } else {
                        null
                    },
                    actionLabel = if (status is TermuxStatus.NoPermission) "去授权" else null,
                    onAction = if (status is TermuxStatus.NoPermission) {
                        {
                            // 优先系统权限弹窗（Activity 上下文可达，与设置页
                            // McpSection 同款）；非 Activity 上下文兜底直达本
                            // 应用详情页。
                            val activity = context as? android.app.Activity
                            if (activity != null) {
                                activity.requestPermissions(
                                    arrayOf(
                                        dev.openslate.mobile.bridge.TermuxExec.RUN_COMMAND_PERMISSION,
                                    ),
                                    1002,
                                )
                            } else {
                                runCatching {
                                    context.startActivity(
                                        Intent(
                                            Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                                            Uri.parse("package:${context.packageName}"),
                                        ),
                                    )
                                }
                            }
                        }
                    } else {
                        null
                    },
                ),
            )
            add(
                StatusCheckRow(
                    state = when (status) {
                        is TermuxStatus.Checking, is TermuxStatus.NotInstalled,
                        is TermuxStatus.NoPermission,
                        -> StatusCheckState.Pending
                        is TermuxStatus.ChannelDown -> StatusCheckState.InProgress
                        is TermuxStatus.Ready -> StatusCheckState.Done
                    },
                    title = "Termux 已运行",
                    summary = if (status is TermuxStatus.ChannelDown) {
                        "${status.summary}；打开后回到本页即自动继续"
                    } else {
                        null
                    },
                    actionLabel = if (status is TermuxStatus.ChannelDown) {
                        if (pulled) "已拉起，等待应答…" else "打开 Termux"
                    } else {
                        null
                    },
                    onAction = if (status is TermuxStatus.ChannelDown) {
                        {
                            if (McpHostManager.startTermuxApp(context.applicationContext)) {
                                pulled = true
                            }
                        }
                    } else {
                        null
                    },
                ),
            )
            add(
                StatusCheckRow(
                    state = when (status) {
                        is TermuxStatus.Ready -> StatusCheckState.Done
                        is TermuxStatus.ChannelDown -> StatusCheckState.InProgress
                        else -> StatusCheckState.Pending
                    },
                    title = "命令通道应答",
                ),
            )
        }
        Box(Modifier.fillMaxWidth().padding(horizontal = 10.dp)) {
            GlassPanel(
                glass = glass,
                rootCoords = rootCoords,
                modifier = Modifier
                    .fillMaxWidth()
                    .shadow(elevation = 2.dp, shape = RoundedCornerShape(16.dp), clip = false),
                shape = RoundedCornerShape(16.dp),
                tint = MaterialTheme.colorScheme.surfaceVariant.copy(
                    alpha = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) 0.6f else 0.92f,
                ),
                border = true,
            ) {
                Column(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 10.dp),
                ) {
                    // 标题行：面板身份 + 已等待计数（阶梯无关的不变量）。
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(
                            Icons.Outlined.Terminal,
                            contentDescription = null,
                            modifier = Modifier.size(16.dp),
                            tint = MaterialTheme.colorScheme.tertiary,
                        )
                        Spacer(Modifier.width(6.dp))
                        Text("Termux 连接", style = MaterialTheme.typography.titleSmall)
                        Spacer(Modifier.width(8.dp))
                        Text(
                            "已等待 ${waitedSec}s",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    // 检查记录主体：四行阶梯映射（见上方 rows 构建）。
                    StatusChecklist(rows = rows, modifier = Modifier.padding(top = 8.dp))
                    if (status is TermuxStatus.Ready) {
                        // 全绿收尾语（收起动画期间可见；连接事件已触发原 id
                        // 重发，命令结果经既有链路回填）。
                        Text(
                            "已连接，命令继续执行，结果将自动回填…",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                            modifier = Modifier.padding(top = 2.dp),
                        )
                    }
                }
            }
        }
    }
}

/**
 * MCP host 恢复面板：mcp.recover_host 在途（mcpRecovering 非 null）时
 * 悬浮在输入区上方（与 TermuxStallBanner 同列同语言：磨砂玻璃面板 +
 * StatusChecklist 检查清单）。行集合 = MCP 恢复阶梯（Termux 运行 →
 * host 进程 → 工具注册）：
 * - 「Termux 运行」行融合 TermuxStallTracker 的周期探测结论（恢复期间
 *   聊天页 2.5s 重探，沿用 stall 面板范式——卡点/引导文案复用其状态）；
 * - 「host 进程」「工具注册」行由恢复协程的 [McpRecoverPhase] 阶梯驱动。
 * 恢复完成（成功或失败）mcpRecovering 清零 → AnimatedVisibility 自动
 * 收起：成功路径 Rust 装饰器经注册表同名重发、结果无感回填；失败路径
 * 原因已并入工具错误（工具卡片与模型可见）。
 */
@Composable
private fun McpRecoverBanner(
    visible: Boolean,
    since: Long?,
    phase: McpRecoverPhase,
    termux: TermuxStatus,
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
) {
    AnimatedVisibility(
        visible = visible,
        enter = fadeIn() + expandVertically(expandFrom = Alignment.Bottom),
        exit = fadeOut() + shrinkVertically(shrinkTowards = Alignment.Bottom),
    ) {
        // 已等待秒数（每秒跳一次；恢复起始时刻换新则重算）。
        var waitedSec by remember(since) { mutableStateOf(0) }
        LaunchedEffect(since) {
            if (since == null) return@LaunchedEffect
            while (true) {
                waitedSec =
                    ((System.currentTimeMillis() - since) / 1000).toInt().coerceAtLeast(0)
                delay(1_000)
            }
        }
        // 检查记录行集合：恢复阶梯三行（阶梯短路语义 = 前置项通过才轮到
        // 当前步；未轮到的行保持 Pending 不抢注意力）。
        val rows = buildList {
            add(
                StatusCheckRow(
                    state = when {
                        // 阶梯已过 Termux 步（进入 host/重载）= 通道已确认。
                        phase.step == McpRecoverStep.HOST || phase.step == McpRecoverStep.RELOAD ->
                            StatusCheckState.Done
                        phase.step == McpRecoverStep.TERMUX -> when (termux) {
                            is TermuxStatus.NotInstalled, is TermuxStatus.NoPermission ->
                                StatusCheckState.Failed
                            else -> StatusCheckState.InProgress
                        }
                        // 探活中：spinner 占位（host 活着则直接跳过本步）。
                        else -> StatusCheckState.InProgress
                    },
                    title = "Termux 运行",
                    summary = when (termux) {
                        is TermuxStatus.ChannelDown -> "${termux.summary}；正在自动拉起"
                        is TermuxStatus.NotInstalled ->
                            "MCP host 依赖 Termux；安装后经设置页一键安装 host"
                        is TermuxStatus.NoPermission ->
                            "系统设置 → 应用 → OpenSlate → 权限，或经 Shizuku 授予"
                        else -> null
                    },
                ),
            )
            add(
                StatusCheckRow(
                    state = when (phase.step) {
                        McpRecoverStep.PROBE, McpRecoverStep.TERMUX -> StatusCheckState.Pending
                        McpRecoverStep.HOST -> StatusCheckState.InProgress
                        McpRecoverStep.RELOAD -> StatusCheckState.Done
                    },
                    title = "host 进程",
                    summary = if (phase.step == McpRecoverStep.HOST) phase.detail else null,
                ),
            )
            add(
                StatusCheckRow(
                    state = when (phase.step) {
                        McpRecoverStep.RELOAD -> StatusCheckState.InProgress
                        else -> StatusCheckState.Pending
                    },
                    title = "工具注册（热重载）",
                    summary = if (phase.step == McpRecoverStep.RELOAD) phase.detail else null,
                ),
            )
        }
        Box(Modifier.fillMaxWidth().padding(horizontal = 10.dp)) {
            GlassPanel(
                glass = glass,
                rootCoords = rootCoords,
                modifier = Modifier
                    .fillMaxWidth()
                    .shadow(elevation = 2.dp, shape = RoundedCornerShape(16.dp), clip = false),
                shape = RoundedCornerShape(16.dp),
                tint = MaterialTheme.colorScheme.surfaceVariant.copy(
                    alpha = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) 0.6f else 0.92f,
                ),
                border = true,
            ) {
                Column(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 10.dp),
                ) {
                    // 标题行：面板身份 + 已等待计数（阶梯无关的不变量）。
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(
                            Icons.Outlined.Autorenew,
                            contentDescription = null,
                            modifier = Modifier.size(16.dp),
                            tint = MaterialTheme.colorScheme.tertiary,
                        )
                        Spacer(Modifier.width(6.dp))
                        Text("MCP host 恢复", style = MaterialTheme.typography.titleSmall)
                        Spacer(Modifier.width(8.dp))
                        Text(
                            "已等待 ${waitedSec}s",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    // 检查记录主体：三行恢复阶梯映射（见上方 rows 构建）。
                    StatusChecklist(rows = rows, modifier = Modifier.padding(top = 8.dp))
                    Text(
                        "正在恢复 MCP 服务，完成后工具调用将自动继续…",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                }
            }
        }
    }
}
