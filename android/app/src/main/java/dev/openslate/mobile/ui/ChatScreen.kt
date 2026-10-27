package dev.openslate.mobile.ui

import android.net.Uri
import android.os.Build
import android.util.Log
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.AnimatedVisibilityScope
import androidx.compose.animation.SharedTransitionLayout
import androidx.compose.animation.SharedTransitionScope
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.MutableTransitionState
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
import androidx.compose.foundation.layout.PaddingValues
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
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Code
import androidx.compose.material.icons.filled.ContentCopy
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.outlined.AddPhotoAlternate
import androidx.compose.material.icons.outlined.Block
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.CheckCircle
import androidx.compose.material.icons.outlined.Code
import androidx.compose.material.icons.outlined.DataObject
import androidx.compose.material.icons.outlined.FactCheck
import androidx.compose.material.icons.outlined.Gavel
import androidx.compose.material.icons.outlined.GppMaybe
import androidx.compose.material.icons.outlined.Psychology
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material.icons.outlined.Tune
import androidx.compose.material.icons.outlined.Warning
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
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
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
import dev.openslate.mobile.bridge.AttachmentRefUi
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
import dev.openslate.mobile.bridge.PendingApprovalUi
import dev.openslate.mobile.bridge.PendingQuestionUi
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.UiEntry
import dev.openslate.mobile.bridge.approvalDecisionLabel
import dev.openslate.mobile.bridge.RuntimeBridge.ASK_USER_TOOL
import kotlin.math.roundToInt
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.ExperimentalSerializationApi
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonElement
import org.json.JSONObject

/**
 * 聊天主界面：transcript 流 + 输入框 + 工具调用审批卡（底部横幅位）。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen(
    onOpenHistory: () -> Unit,
    onOpenSettings: () -> Unit,
    onOpenAskUser: () -> Unit = {},
    onOpenReviewAskUser: () -> Unit = {},
    /** 查看器对比块「跳转设置」：跳设置页并直落「工具 > 内置工具」（处理图片分区）。 */
    onOpenImageProcessingSettings: () -> Unit = {},
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
    // 错误/提示 snackbar 通道（多模态输入契约）：① RuntimeBridge Error/Warn
    // 级 notice（附件门控拒绝等）→ 自动展示；② composer 侧校验失败/导入
    // 失败经 onMessage 上抛。附件提交被拒时 composer 草稿与 chips 原地保留
    // （submit 终态信号见 RuntimeBridge.submitResolution），snackbar 只管
    // 展示原因。
    val snackbarHostState = remember { SnackbarHostState() }
    val snackbarScope = rememberCoroutineScope()
    val showMessage: (String) -> Unit = { msg ->
        snackbarScope.launch { snackbarHostState.showSnackbar(msg) }
    }
    val lastNotice by RuntimeBridge.lastNotice.collectAsState()
    var seenNoticeSeq by remember { mutableStateOf(0L) }
    LaunchedEffect(lastNotice) {
        val n = lastNotice ?: return@LaunchedEffect
        if (n.seq > seenNoticeSeq) {
            seenNoticeSeq = n.seq
            if (n.level == "error" || n.level == "warn") snackbarHostState.showSnackbar(n.text)
        }
    }
    // 输入框聚焦面板：显示 = 输入框持有焦点（编辑状态）。未聚焦时面板
    // 不参与组合，底部布局零变化。Termux/MCP 活性走 tracker 状态流
    // （聚焦展开时面板内部 refresh），未聚焦时喂输入框内常驻状态行。
    var composerFocused by remember { mutableStateOf(false) }
    val live by ComposerMcpStatusTracker.state.collectAsState()
    val lightsState by McpLightsTracker.state.collectAsState()
    val inlineStatus = composerInlineStatus(state.modelAlias, state.config, live, lightsState)
    // 双 tracker 分工（与 composer 聚焦面板同口径）：MCP/tools 计数与
    // server 名单走 ComposerMcpStatusTracker（廉价 HTTP 探活 + tools/list）；
    // 首行 Termux 活性走五灯 McpLightsTracker（probeMcpLights 完整管线）。
    // 两者常新鲜（不依赖聚焦）：初次组合刷一次 + 回前台（ON_RESUME）刷
    // 一次——lights 侧经 gateRefresh 智能门控（无快照/非全绿→全量重检含
    // 自动拉起 host、全绿→廉价 hostAlive 比对，与设置页同款）；tracker
    // 自带单飞去重（重复触发安全），探活为本机回环 HTTP 成本低，故不设
    // 轮询定时器；面板聚焦时另有 refresh（ComposerFocusPanel）。ChatScreen
    // 无 config_changed / MCP reload 回调处理点，按约定不新增链路。
    LaunchedEffect(Unit) {
        ComposerMcpStatusTracker.refresh(context)
        McpLightsTracker.gateRefresh(context)
    }
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
                // 五灯侧同款 ON_RESUME 门控（与设置页 McpSection 一致）：
                // 非全绿快照（如刚从 Termux 切回）→ 全量重检 + 自动拉起
                // host；全绿 → 廉价 hostAlive 比对，翻转才重检。composer
                // 聚焦面板首行/子面板与输入框常驻状态行同享该快照。
                McpLightsTracker.gateRefresh(context)
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
    // 附件全屏查看器：消息气泡缩略图 / composer 草稿卡点击打开（视频卡无动作）。
    var attachmentViewer by remember { mutableStateOf<AttachmentViewerSource?>(null) }
    // 「编辑」请求（用户消息长按菜单 → 文本回填 composer 输入框）：seq 单调
    // 递增驱动 Composer 侧 LaunchedEffect，同一文本重复编辑也能再触发。
    var editSeq by remember { mutableStateOf(0L) }
    var editText by remember { mutableStateOf("") }

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
            state.pendingApproval?.let { pending ->
                ApprovalCard(
                    pending = pending,
                    glass = glass,
                    rootCoords = { rootCoords },
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
                                    // 用户消息长按「编辑」→ 文本回填 composer（seq
                                    // 触发，同文本重复编辑也生效）。
                                    onEditMessage = { t -> editText = t; editSeq += 1 },
                                    // 气泡内图片缩略图点击 → 全屏查看器（视频卡无动作）。
                                    onOpenAttachment = { ref ->
                                        if (!ref.isVideo) {
                                            attachmentViewer = AttachmentViewerSource.Stored(ref.id, ref.filename)
                                        }
                                    },
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
                    // 错误/提示 snackbar：压在聚焦面板与输入区之间（出现时
                    // 把底部组顶起，自动消失后回落；空状态零占位）。
                    SnackbarHost(
                        hostState = snackbarHostState,
                        modifier = Modifier.padding(horizontal = 10.dp),
                    )
                    Composer(
                        glass = glass,
                        rootCoords = { rootCoords },
                        running = state.running,
                        onInputFocusChange = { composerFocused = it },
                        // 发送（纯文本/带附件同路）：附件提交是异步门控（拒绝
                        // 经 Error notice 返回），composer 等终态信号
                        // （submitResolution）再清草稿；这里只透传送达结果。
                        // 发送成功退出编辑状态：清焦点 → 聚焦面板收起。
                        onSend = { text, refs ->
                            val delivered = RuntimeBridge.submit(text, refs)
                            if (delivered) focusManager.clearFocus()
                            delivered
                        },
                        onCancel = { RuntimeBridge.cancel() },
                        hazeState = hazeState,
                        onMessage = showMessage,
                        // 常驻状态行：仅未聚焦时显示（聚焦后同源信息移交面板卡片）。
                        showInlineStatus = !composerFocused,
                        inlineStatus = inlineStatus,
                        sharedTransitionScope = this@SharedTransitionLayout,
                        // 「编辑」请求：seq 变化 → 输入框回填 + 聚焦（见 Composer）。
                        editRequestSeq = editSeq,
                        editRequestText = editText,
                        // 草稿图片卡点击 → 全屏查看器（原 URI 解码；带处理
                        // 对比数据时信息区展示「原图 vs 处理后」）。
                        onOpenDraft = { att ->
                            att.previewUri?.let { uri ->
                                attachmentViewer = AttachmentViewerSource.Draft(
                                    uri = uri,
                                    filename = att.filename,
                                    sizeBytes = att.sizeBytes,
                                    processed = att.processedInfo,
                                )
                            }
                        },
                    )
                }
            }

            // 工具详情弹窗（Dialog 自身是独立 window，不参与列表布局）。
            detailLive?.let { tc ->
                ToolDetailDialog(tc, onDismiss = { detailTool = null })
            }

            // 附件全屏查看器（气泡缩略图 / composer 草稿卡点击；Dialog 独立 window）。
            // 对比块「跳转设置」：先摘 viewer 状态（同帧关 Dialog）再导航，
            // 设置页右滑推入与 Dialog 消失同帧进行，无闪屏。
            attachmentViewer?.let { src ->
                AttachmentViewerDialog(
                    source = src,
                    onDismiss = { attachmentViewer = null },
                    onOpenProcessingSettings = {
                        attachmentViewer = null
                        onOpenImageProcessingSettings()
                    },
                )
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
    is UiEntry.User -> "u:${e.text.length}:${e.attachments.size}"
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
    /** 用户消息长按菜单「编辑」→ 文本回填 composer。 */
    onEditMessage: (String) -> Unit = {},
    /** 气泡内图片缩略图点击 → 全屏查看器。 */
    onOpenAttachment: (AttachmentRefUi) -> Unit = {},
) {
    when (entry) {
        is UiEntry.User -> Bubble(
            entry.text,
            mine = true,
            attachments = entry.attachments,
            hazeState = hazeState,
            onEdit = onEditMessage,
            onOpenAttachment = onOpenAttachment,
        )
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
        is UiEntry.Approval -> ApprovalTranscriptRow(entry)
        UiEntry.StepBreak -> Spacer(Modifier.widthIn(min = 4.dp))
    }
}

/** 允许类决定（转写行绿/红分色判定；未知值走 Gavel 中性兜底）。 */
private val APPROVAL_ALLOW_DECISIONS =
    setOf("approved", "approve-all", "always-tool", "session-mcp", "always-mcp")

/**
 * 审批转写行：Material 图标（按决定分色：允许族绿 / 拒绝红 / 未知中性）
 * + 工具名（等宽内嵌）+ 决定文案（[approvalDecisionLabel]，未知值兜底
 * 显示原始串）。
 */
@Composable
private fun ApprovalTranscriptRow(entry: UiEntry.Approval) {
    val allowed = entry.decision in APPROVAL_ALLOW_DECISIONS
    val icon = when {
        entry.decision == "denied" -> Icons.Outlined.Block
        allowed -> Icons.Outlined.FactCheck
        else -> Icons.Outlined.Gavel
    }
    val tint = when {
        entry.decision == "denied" -> MaterialTheme.colorScheme.error
        allowed -> successGreen()
        else -> MaterialTheme.colorScheme.outline
    }
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier.padding(horizontal = 8.dp),
    ) {
        Icon(icon, contentDescription = null, modifier = Modifier.size(13.dp), tint = tint)
        Spacer(Modifier.width(8.dp))
        Text(
            buildAnnotatedString {
                withStyle(SpanStyle(fontFamily = MonoFont)) { append(entry.toolName) }
                append(" · ")
                append(approvalDecisionLabel(entry.decision))
            },
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.outline,
        )
    }
}

/** bash 类工具注册名（与 Rust 侧注册一一绑定，后端判定即工具名）：Termux 内执行。 */
private const val TERMUX_BASH_TOOL = "termux_bash"

/** bash 类工具注册名：App 进程内 native sh（/system/bin/sh + Toybox）。 */
private const val NATIVE_BASH_TOOL = "bash"

/** bash 类工具判定：命令形态渲染仅覆盖这两个名字，其他工具完全保持现状。 */
private fun isBashLikeTool(name: String): Boolean =
    name == TERMUX_BASH_TOOL || name == NATIVE_BASH_TOOL

/** output_query 工具注册名（与 Rust 侧 OUTPUT_QUERY_TOOL 一一绑定）。 */
private const val OUTPUT_QUERY_TOOL = "output_query"

/**
 * 解析 bash 类工具 fullArgs 的 command 字段（参数形状 `{"command": "..."}`，
 * bash 另有可选 timeout_ms）。命中返回 (命令文本, 其余参数 pretty JSON)——
 * 其余参数为空串表示无额外参数；fullArgs 空 / 非 JSON / 缺 command 键返回
 * null（调用方回退原始 argsPreview / fullArgs 展示，信息不丢）。
 */
private fun parseBashCommand(fullArgs: String): Pair<String, String>? = try {
    val obj = JSONObject(fullArgs)
    val cmd = obj.optString("command")
    if (cmd.isBlank()) {
        null
    } else {
        obj.remove("command")
        cmd to if (obj.length() > 0) obj.toString(2) else ""
    }
} catch (_: Exception) {
    null
}

// ── JSON 自动格式化（工具详情弹窗）─────────────────────────────────────
//
// 检测与美化用 kotlinx.serialization 而非 org.json：JVM 单测里 org.json 是
// stub（AGP 注入的桩类，方法体直接抛异常），kotlinx 是纯 JVM 依赖、单测可
// 直载（bridge/JsonDto.kt 选它的同理由）。

/**
 * JSON 检测/美化共用实例：prettyPrint 只作用于编码输出侧，
 * parseToJsonElement 不受影响；缩进两空格（对齐 parseBashCommand 的
 * org.json toString(2) 视觉）。
 */
@OptIn(ExperimentalSerializationApi::class)
private val JSON_PRETTY: Json = Json {
    prettyPrint = true
    prettyPrintIndent = "  "
}

/** trim 后首字符即非 JSON 容器（`{` / `[`）→ 快速排除，免全量 parse。 */
private fun startsAsJsonContainer(text: String): Boolean {
    val c = text.trim().firstOrNull() ?: return false
    return c == '{' || c == '['
}

/**
 * 文本是否为合法 JSON 对象/数组：trim 后首字符 `{` / `[` 且解析成功。
 * 限定容器形态是有意为之：弹窗「内容」是工具参数/输出，裸标量（纯数字/
 * 字符串/布尔）格式化与否视觉无差，反而会让按钮在非 JSON 输出上误现。
 */
internal fun looksLikeJson(text: String): Boolean =
    startsAsJsonContainer(text) && try {
        JSON_PRETTY.parseToJsonElement(text.trim())
        true
    } catch (_: Exception) {
        false
    }

/**
 * JSON 美化（pretty print，两空格缩进）：解析成功返回格式化文本；失败/
 * 非容器返回 null（调用方回退原文显示）。数字以原字符串存进
 * [JsonPrimitive]——超 long/double 精度的大整数 round-trip 不失真；
 * [JsonObject] 保键插入序。
 */
internal fun prettyJsonOrNull(text: String): String? {
    if (!startsAsJsonContainer(text)) return null
    return try {
        JSON_PRETTY.encodeToString(
            JsonElement.serializer(),
            JSON_PRETTY.parseToJsonElement(text.trim()),
        )
    } catch (_: Exception) {
        null
    }
}

/**
 * output_query 参数 → 人类可读摘要（参数形状与 Rust 侧 schema 一致：ref 必填，
 * op ∈ head/tail/lines/grep/wc；head/tail 带可选 n（默认 50），lines 带
 * start/end，grep 带 pattern / ignore_case）。命中返回 (ref, 摘要)——ref 是
 * 会话内短 id，摘要已把它嵌进句子（如 `out-1 的第 120-140 行`）。fullArgs
 * 空 / 非 JSON / 缺 ref / 未知或缺 op / 子参数不完整（如 lines 缺行号、grep
 * 缺 pattern）返回 null（调用方回退原始 argsPreview 展示，信息不丢）。
 */
internal fun describeOutputQueryArgs(fullArgs: String): Pair<String, String>? = try {
    val obj = JSONObject(fullArgs)
    val ref = obj.optString("ref")
    if (ref.isBlank()) {
        null
    } else {
        val summary = when (obj.optString("op")) {
            "head" -> "$ref 的开头 ${obj.optInt("n", 50)} 行"
            "tail" -> "$ref 的结尾 ${obj.optInt("n", 50)} 行"
            "lines" -> {
                val start = obj.optInt("start", -1)
                val end = obj.optInt("end", -1)
                when {
                    start < 0 || end < 0 -> null
                    start == end -> "$ref 的第 $start 行"
                    else -> "$ref 的第 $start-$end 行"
                }
            }
            "grep" -> obj.optString("pattern").takeIf { it.isNotBlank() }?.let { pattern ->
                val shown = if (pattern.length > 30) pattern.take(30) + "…" else pattern
                val caseNote = if (obj.optBoolean("ignore_case", false)) "（忽略大小写）" else ""
                "在 $ref 中搜索「$shown」$caseNote"
            }
            "wc" -> "$ref 的行数与字节统计"
            else -> null
        }
        summary?.let { ref to it }
    }
} catch (_: Exception) {
    null
}

/** 渲染钳制阈值：超过此长度（UTF-16 字符）的文本不整段进单个 Compose Text。 */
private const val RENDER_CLAMP_THRESHOLD = 32 * 1024

/** 渲染钳制头部保留量（UTF-16 单元，24 KB）。 */
private const val RENDER_CLAMP_HEAD = 24 * 1024

/** 渲染钳制尾部保留量（UTF-16 单元，8 KB）。 */
private const val RENDER_CLAMP_TAIL = 8 * 1024

/** 渲染钳制结果：display = 钳制后可渲染文本（头 + 换行 + 尾）；omissionNote
 *  非空 = 发生了钳制，调用方在显示位置渲染一行该说明（null = 未超限，
 *  display 即原文）。 */
internal data class RenderClamp(val display: String, val omissionNote: String?)

/**
 * 渲染钳制：text 超过 RENDER_CLAMP_THRESHOLD（32K 字符）时返回头
 * RENDER_CLAMP_HEAD（24K）+ 尾 RENDER_CLAMP_TAIL（8K）与省略说明行；
 * 不超返回原串（omissionNote null）。说明行回答「完整输出存在哪」：
 * 已压缩落盘（App 私有目录 files/outputs/）、可让模型用 output_query 按
 * ref 分段查看、或点下方「复制完整输出」按钮；ref 可解析时附在
 * output_query 后（调用方从输出标记/fullArgs 传入，null 不附）。
 *
 * 切点做 UTF-16 代理对保护（[Character.isHighSurrogate] 判缘回退/前进 1，
 * emoji/增补平面字符不劈裂——同 TermuxExec.combined 先例）。KB 数按 UTF-16
 * 单元计（÷1024）。**只影响显示**：字符统计与复制一律用未钳制原串。
 */
internal fun clampTextForRender(text: String, ref: String? = null): RenderClamp {
    if (text.length <= RENDER_CLAMP_THRESHOLD) return RenderClamp(text, null)
    // 头切点：前一 char 是高代理项 = 劈开代理对 → 回退 1。
    var headEnd = RENDER_CLAMP_HEAD
    if (Character.isHighSurrogate(text[headEnd - 1])) headEnd--
    // 尾切点：前一 char 是高代理项（当前位是低代理项）→ 前进 1，
    // 把这对代理整个留给省略段。
    var tailStart = text.length - RENDER_CLAMP_TAIL
    if (Character.isHighSurrogate(text[tailStart - 1])) tailStart++
    val note = "[已显示前 ${RENDER_CLAMP_HEAD / 1024} KB + 后 ${RENDER_CLAMP_TAIL / 1024} KB" +
        "（完整 ${text.length / 1024} KB）——完整输出已压缩存储在 App 私有目录 files/outputs/，" +
        "可让模型用 output_query${if (ref != null) "（ref=$ref）" else ""}分段查看，" +
        "或点下方「复制完整输出」]"
    return RenderClamp(text.substring(0, headEnd) + "\n" + text.substring(tailStart), note)
}

/** 分块渲染行数上限：每块 ≤100 行。 */
private const val CHUNK_MAX_LINES = 100

/** 分块渲染单行折段阈值：>2000 字符的行按 ~2000 字符折段为伪行。 */
private const val CHUNK_FOLD_LINE = 2000

/** 分块渲染每块字符上限（含换行计，~4KB）——单块 Compose Text 布局成本封顶。 */
private const val CHUNK_MAX_CHARS = 4096

/** 病态兜底阈值：任一分块 >16K 字符 = 两级分块失效（理论上恒不触发，
 *  防御分块参数未来回归），该输出整段回退 clampTextForRender 钳制渲染。 */
private const val PATHOLOGICAL_CHUNK_CHARS = 16 * 1024

/**
 * 大文本 → 渲染块序列：按行分块（每块 ≤[CHUNK_MAX_LINES] 行），单行
 * >[CHUNK_FOLD_LINE] 字符再按 ~2000 字符折段为伪行块；同时受
 * [CHUNK_MAX_CHARS]（~4KB）总量约束（行均长时先行触顶封块），保证每块
 * 渲染 Text ≤~4KB。带稳定 key（"c{i}"）供 LazyColumn item(key=) 防重组
 * 错位。空文本不出块。折段切点做 UTF-16 代理对保护（同
 * [clampTextForRender] 先例）。**只影响显示**：伪行折段引入的换行不属于
 * 原文，字符统计与复制一律用未分块原串。
 */
internal fun chunkTextForLazyRender(text: String): List<Pair<String, String>> {
    if (text.isEmpty()) return emptyList()
    val chunks = ArrayList<Pair<String, String>>()
    val lines = ArrayList<String>() // 当前块累积的行/伪行
    var chars = 0                   // 当前块字符数（含行间换行）
    fun flush() {
        if (lines.isNotEmpty()) {
            chunks += "c${chunks.size}" to lines.joinToString("\n")
            lines.clear()
            chars = 0
        }
    }
    for (line in text.split('\n')) {
        var off = 0
        while (true) {
            // 折段：>2000 字符的行切为 ~2000 字符伪行；切点劈开代理对时回退 1。
            val end: Int
            if (line.length - off > CHUNK_FOLD_LINE) {
                var cut = off + CHUNK_FOLD_LINE
                if (Character.isHighSurrogate(line[cut - 1])) cut--
                end = cut
            } else {
                end = line.length
            }
            val piece = line.substring(off, end)
            off = end
            // 总量约束：放入本行会超 ~4KB 且块非空 → 先封块（伪行 ≤2000，
            // 封块后必放得下，无死循环）。
            if (lines.isNotEmpty() && chars + piece.length + 1 > CHUNK_MAX_CHARS) flush()
            if (lines.isNotEmpty()) chars++ // 行间换行计入
            lines.add(piece)
            chars += piece.length
            if (lines.size >= CHUNK_MAX_LINES) flush()
            if (off >= line.length) break
        }
    }
    flush()
    return chunks
}

/** 输出文本里 `[STORED OUTPUT ref="out-N" …]` 标记的 ref 提取（core 落盘预览）。 */
private val STORED_REF_MARKER = Regex("""\[STORED OUTPUT ref="([^"]+)"""")
/** 预览标记里的 size=N（原文总字节）——判断展示是否完整，小输出免打扰。 */
private val STORED_SIZE_MARKER = Regex("""\[STORED OUTPUT [^\]]*?size=(\d+)""")

/**
 * 落盘输出引用解析（渲染钳制说明行用）：输出文本里的 `[STORED OUTPUT
 * ref="out-N"` 标记优先（bash 类超大输出经 expand_stored_output 回填，
 * 恒带标记）；output_query 工具的 fullArgs ref 参数兜底（其输出本身是
 * 查询结果、不含标记，isOutputQuery=false 时不查——其他工具的 fullArgs
 * 碰巧有 ref 字段不是落盘引用，不误认）。两处都没有返回 null。
 */
private fun storedOutputRef(output: String?, fullArgs: String, isOutputQuery: Boolean): String? =
    STORED_REF_MARKER.find(output.orEmpty())?.groupValues?.get(1)
        ?: if (isOutputQuery) {
            try {
                JSONObject(fullArgs).optString("ref").takeIf { it.isNotBlank() }
            } catch (_: Exception) {
                null
            }
        } else {
            null
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

/**
 * 单个工具条目：状态图标 + 名称 + 参数摘要，点击弹出详情弹窗（完整命令 + 输出）。
 * 工具名与参数摘要均 MonoFont 等宽（标识类短文本约定；中文等缺失字形
 * 自动回退系统字体）。bash 类工具（termux_bash / bash）的摘要槽改显实际
 * 命令（解析 fullArgs 的 command 字段）：等宽单行截断 + 前缀区分执行环境
 * （termux_bash 挂 Termux 品牌 logo、bash 挂 `$` 提示符——命令文本语义，
 * 非图标）；output_query 的摘要槽改显人类可读摘要（自然语言句子，用默认
 * 字体 labelSmall 而非 MonoFont——人话不是代码）。解析失败一律回退原
 * argsPreview。
 */
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
    // bash 类命令预览：解析 fullArgs 的 command 字段（remember 缓存，避免
    // 重组重复 parse）。null = 非 bash 类或解析失败/空 fullArgs，回退原
    // argsPreview 展示。
    val bashCmd = remember(tc.name, tc.fullArgs) {
        if (isBashLikeTool(tc.name)) parseBashCommand(tc.fullArgs) else null
    }
    // output_query 人类可读摘要（remember 缓存，照 bashCmd 同款）。null =
    // 解析失败/参数不完整，回退原 argsPreview 展示。
    val outputQueryDesc = remember(tc.name, tc.fullArgs) {
        if (tc.name == OUTPUT_QUERY_TOOL) describeOutputQueryArgs(tc.fullArgs) else null
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
            style = MaterialTheme.typography.labelMedium.copy(fontFamily = MonoFont),
            color = statusTint,
        )
        Spacer(Modifier.widthIn(min = 6.dp))
        if (bashCmd != null) {
            // 前缀区分执行环境：Termux 侧挂品牌 logo（固定配色不走 tint，见
            // ic_termux.xml 头注释）；native 侧用 `$` 提示符（等宽命令文本
            // 语义的一部分，非图标组件）。
            if (tc.name == TERMUX_BASH_TOOL) {
                Icon(
                    painter = painterResource(R.drawable.ic_termux),
                    contentDescription = null,
                    modifier = Modifier.size(13.dp),
                    tint = Color.Unspecified,
                )
                Spacer(Modifier.width(4.dp))
            }
            // 实际命令替代原始 JSON 预览：MonoFont 等宽（标识类短文本约定），
            // 单行截断省略。
            Text(
                if (tc.name == NATIVE_BASH_TOOL) "\$ " + bashCmd.first else bashCmd.first,
                style = MaterialTheme.typography.labelMedium.copy(fontFamily = MonoFont),
                color = MaterialTheme.colorScheme.outline,
                maxLines = 1,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
        } else if (outputQueryDesc != null) {
            // output_query 摘要：自然语言句子用默认字体（非 MonoFont——人话
            // 不是代码）；摘要本身已把 ref 短 id 嵌进句子，直接展示不重复拼。
            Text(
                outputQueryDesc.second,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
                maxLines = 1,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
        } else {
            Text(
                tc.argsPreview,
                style = MaterialTheme.typography.labelSmall.copy(fontFamily = MonoFont),
                color = MaterialTheme.colorScheme.outline,
                maxLines = 1,
                overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f),
            )
        }
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
 * bash 类工具（termux_bash / bash）命令区分层展示：顶部具体命令（去 JSON
 * wrapper、系统等宽、可多行、前缀与工具条同语言）+「调用参数」区（command
 * 外其余参数 pretty JSON，无则省略）；解析失败整块回退原始 JSON 展示，
 * 「复制命令」随解析结果切换纯命令 / 原始 fullArgs。输出区与懒加载逻辑不动。
 *
 * 懒加载全量输出：条目携带 callId 且非 running / 非 ask_user 时，经 FFI
 * 从 Rust 内存 history 拉取 max_output_bytes 上限内的完整版本，与条目
 * 自带输出（host 工具的 Kotlin 缓存 / 事件 preview / 恢复快照）**取更
 * 长者**显示——host 缓存可能比 DB 版本更长且未截断，DB 版有 secret 打
 * 码，两者都安全，丢任何一边都可能缺内容。旧条目无 callId / FFI 未命
 * 中（history 被压缩、子代理工具）→ 静默回退 preview，维持现状显示。
 *
 * 渲染钳制（真机卡死修复史）：b17bf68 曾以「头 24K + 尾 8K」钳制止住单个
 * Text 布局 512 KiB 全文导致的 UI 冻结，代价是中段不可见；现升级为**分块
 * 虚拟滚动**（方案 A：全文载入 + LazyColumn 分块，内存 2MiB 峰值可接受）——
 * 当前分段全文经 [chunkTextForLazyRender] 两级分块（≤100 行/块 + 单行
 * >2000 字符折段 + ~4KB/块封顶）逐块渲染，中段完整可滚；钳制降级为病态
 * 兜底（任一块 >16K 字符才整段回退 [clampTextForRender]，理论上恒不触发）。
 * 分块只影响显示：字符统计与底部「复制完整输出」（未分块全文，输出非空才
 * 出现）均用原串。
 *
 * JSON 自动格式化：参数（非 bash 的 fullArgs / bash 的其余参数）与输出段
 * （stdout/stderr 各自判定，随 tab 切换）为合法 JSON 时，标签行圆按钮
 * （[JsonFormatToggle]，弹窗内多颗共享同一开关）切换 pretty + json 语法
 * 高亮展示（SyntaxMP + GitHub 配色，与代码块同管线）；参数区同步判定
 * （体量小），输出段异步判定/格式化（可达 512KiB，不卡主线程）。开关初始
 * 值读 ToolDisplayPrefs 全局设置，弹窗内切换不写回。格式化只影响显示：
 * 字符统计与复制一律用原串。
 */
@Composable
private fun ToolDetailDialog(tc: UiEntry.ToolCall, onDismiss: () -> Unit) {
    val clipboard = LocalClipboardManager.current
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
    // bash 类分层展示的解析（remember 缓存，避免重组重复 parse）：null =
    // 非 bash 类或解析失败/空 fullArgs，命令区整块回退原始 JSON 展示。
    val bashCmd = remember(tc.name, tc.fullArgs) {
        if (isBashLikeTool(tc.name)) parseBashCommand(tc.fullArgs) else null
    }
    // 落盘输出引用（渲染钳制说明行用，remember 缓存）：输出文本的
    // [STORED OUTPUT ref="out-N"] 标记优先，output_query 的 fullArgs 兜底。
    val storedRef = remember(shownOut, tc.name, tc.fullArgs) {
        storedOutputRef(shownOut, tc.fullArgs, tc.name == OUTPUT_QUERY_TOOL)
    }
    // ── JSON 自动格式化 ──
    // 开关初始值读全局设置（设置页为权威写入点），弹窗内圆按钮本地切换、
    // 不写回；多颗按钮（参数区 + 输出区）共享同一状态。
    val context = LocalContext.current
    var formatJson by remember { mutableStateOf(ToolDisplayPrefs.readJsonAutoFormat(context)) }
    // 高亮管线（与 SourceCodeDialog / 气泡代码块同源）：深色判定走
    // LocalDarkTheme（对齐外观设置，不直读 isSystemInDarkTheme）。
    val dark = LocalDarkTheme.current
    val tokenizer = remember { SyntaxTokenizer() }
    val theme = remember(dark) { syntaxRoleColors(dark) }
    // 参数区 JSON 判定/美化/高亮（参数体量小，同步 remember 即可；输出段
    // 才走异步——输出可达 512KiB）。pretty 非 null = 开关开且内容是 JSON。
    val argsIsJson = remember(tc.fullArgs) { looksLikeJson(tc.fullArgs) }
    val argsPretty = remember(tc.fullArgs, formatJson) {
        if (formatJson && argsIsJson) prettyJsonOrNull(tc.fullArgs) else null
    }
    val argsHighlighted = remember(argsPretty, tokenizer, theme) {
        argsPretty?.let { buildSyntaxAnnotatedString(it, "json", tokenizer, theme) }
    }
    // bash「调用参数」区（parseBashCommand 的 org.json toString(2) 产物，
    // 恒为合法 JSON）：开关开 → 换 kotlinx pretty + json 高亮，关 → 原样。
    val bashArgsIsJson = remember(bashCmd) { bashCmd?.second?.let(::looksLikeJson) == true }
    val bashArgsPretty = remember(bashCmd, formatJson) {
        if (formatJson && bashArgsIsJson) bashCmd?.second?.let(::prettyJsonOrNull) else null
    }
    val bashArgsHighlighted = remember(bashArgsPretty, tokenizer, theme) {
        bashArgsPretty?.let { buildSyntaxAnnotatedString(it, "json", tokenizer, theme) }
    }
    // 「复制完整输出」成功反馈：标签短暂切换「已复制」（同 CodeBlock 惯例）。
    var fullCopied by remember { mutableStateOf(false) }
    LaunchedEffect(fullCopied) {
        if (fullCopied) {
            delay(2000)
            fullCopied = false
        }
    }
    // 当前分段全文（未分块原串，remember 键控防重组重算）：分块渲染的输入；
    // null = 无输出（占位「（无输出）」）。
    val rawText = when {
        running -> null
        parts.stderr.isNotEmpty() && tab == 1 -> parts.stderr.ifBlank { "（空）" }
        parts.stdout.isNotBlank() -> parts.stdout
        else -> null
    }
    // 输出段 JSON 判定 + 美化（异步，Dispatchers.Default）：输出可达
    // 512KiB，parse 不在主线程做。判定与格式化一次算完——first = 当前分段
    // 是否 JSON（输出区圆按钮显隐依据），second = pretty 文本。键控 rawText
    // （tab 切换 / 输出到货即重算）；开关切换不触发重算（判定与开关无关）。
    val outJson by produceState<Pair<Boolean, String>?>(null, rawText) {
        if (rawText == null) {
            value = null
        } else {
            value = withContext(Dispatchers.Default) {
                prettyJsonOrNull(rawText)?.let { true to it } ?: (false to "")
            }
        }
    }
    val outJsonIsJson = outJson?.first == true
    // 输出显示文本：开关开且当前分段是 JSON → pretty（分块渲染输入随之
    // 换源），否则原文。异步未就绪时先显示原文（pretty 到货即切换）。
    val outShown = outJson?.takeIf { formatJson && it.first }?.second ?: rawText
    // pretty JSON 展示是否生效（决定输出分块是否走 json 高亮）。
    val outJsonOn = formatJson && outJsonIsJson
    // 输出分块（分块虚拟滚动的渲染层，纯函数 remember 缓存）：当前分段显示
    // 文本（原文或 pretty JSON）两级分块（≤100 行/块 + 单行 >2000 字符折段
    // + ~4KB/块），每块一个 LazyColumn item——中段完整可滚（替代旧头尾钳
    // 制），内存 2MiB 峰值可接受（方案 A）。
    val chunks = remember(outShown) {
        if (outShown == null) emptyList() else chunkTextForLazyRender(outShown)
    }
    // 病态兜底判断：任一块 >16K 字符 = 两级分块失效（理论上恒不触发，防御
    // 分块参数回归）→ 整段回退 b17bf68 的 clampTextForRender 钳制渲染。
    val pathological = remember(chunks) {
        chunks.any { it.second.length > PATHOLOGICAL_CHUNK_CHARS }
    }
    val clamp = remember(rawText, storedRef) {
        if (pathological && rawText != null) clampTextForRender(rawText, storedRef) else null
    }
    // 存储位置说明行（输出块最上方）：输出为落盘全文（含 [STORED
    // OUTPUT] 标记，或懒加载全量长于条目自带输出）**且展示不完整**
    // （传输层头尾截断：可见长度 < 标记里的原文 size）时才提示存储位置
    // 与查询途径——小输出全文都在眼前，说明行纯属噪音，不出。
    val storedNote = remember(shownOut, fullOut, tc.output, storedRef) {
        val out = shownOut.orEmpty()
        val full = fullOut // 局部捕获：fullOut 是委托属性，lambda 内不可 smart cast
        val stored = STORED_REF_MARKER.containsMatchIn(out) ||
            (full != null && full.length > (tc.output ?: "").length)
        if (!stored) {
            null
        } else {
            // out 含标记行（~80 字符）+ 内容；全文可见时 out.length 恒大于
            // 原文 size。解析失败（异常形态）保守回退：显示。
            val origSize = STORED_SIZE_MARKER.find(out)?.groupValues?.get(1)?.toIntOrNull()
            if (origSize != null && out.length >= origSize) {
                null
            } else {
                "完整输出（${out.length / 1024} KB）已压缩存储在 App 私有目录 files/outputs/，" +
                    "可让模型用 output_query${if (storedRef != null) "（ref=$storedRef）" else ""}分段查看，" +
                    "或点下方「复制完整输出」"
            }
        }
    }

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
                // 头部：工具名 + exit code 徽标 + 关闭。固定在 LazyColumn 之外
                // （不作为首个 item——取舍：标题/关闭钮是弹窗 chrome，恒可见
                // 恒可达优于随内容滚动，且免一个 item 的重组）。
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.padding(start = 16.dp, end = 4.dp, top = 2.dp, bottom = 2.dp),
                ) {
                    Text(
                        tc.name,
                        style = MaterialTheme.typography.titleSmall.copy(fontFamily = MonoFont),
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
                // 内容区：单个 LazyColumn 分块虚拟滚动（方案 A：全文载入 +
                // 分块渲染，内存 2MiB 峰值可接受）。原 Column(verticalScroll)
                // 已移除——进度行/命令块/参数块/输出头/说明行/输出分块全部
                // 成为 item，消除同向嵌套滚动；1000 item 量级默认配置无压力，
                // 不加 animateItemPlacement 之类的额外机制。
                // SelectionContainer 包整个 LazyColumn：输出分块共享同一
                // selection registrar → 跨 item 选择；命令/参数区各自的内层
                // SelectionContainer 就近优先接管（维持原独立选择语义），外层
                // 实际只覆盖输出块。取舍：lazy item 滚出视口被析构时其选中态
                // 随之失效（拖选同时深滚会断选），展示场景可接受。
                SelectionContainer {
                    LazyColumn(
                        modifier = Modifier.weight(1f),
                        contentPadding = PaddingValues(horizontal = 16.dp, vertical = 10.dp),
                    ) {
                        // 懒加载进度行（极轻）：FFI 拉取全量输出期间显示，到货即隐。
                        if (!fullLoaded) {
                            item(key = "loading") {
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
                        }
                        // 命令区：整块一个 item（体量小，无分块必要）。
                        item(key = "cmd") {
                            Column {
                                if (bashCmd == null) {
                                    // 非 bash 类 / 解析失败：「命令」区显示 fullArgs。
                                    // 内容为 JSON 且开关开 → pretty + json 语法高亮
                                    // （与代码块同管线）；否则原文。全文直显，无截断。
                                    Row(verticalAlignment = Alignment.CenterVertically) {
                                        Text(
                                            "命令",
                                            style = MaterialTheme.typography.labelSmall,
                                            color = MaterialTheme.colorScheme.primary,
                                            modifier = Modifier.weight(1f),
                                        )
                                        if (argsIsJson) {
                                            JsonFormatToggle(
                                                formatOn = formatJson,
                                                onToggle = { formatJson = !formatJson },
                                            )
                                        }
                                    }
                                    SelectionContainer {
                                        if (argsHighlighted != null) {
                                            Text(argsHighlighted, style = mono)
                                        } else {
                                            Text(tc.fullArgs.ifBlank { "（无参数）" }, style = mono)
                                        }
                                    }
                                } else {
                                    // bash 类：顶部具体命令（去 JSON wrapper，系统等宽、可
                                    // 多行），前缀与工具条同语言——termux_bash 挂品牌 logo
                                    // （固定配色不走 tint）、bash 挂 `$` 提示符（文本语义，
                                    // 非图标组件）。不设「命令」标签：前缀标记本身即语义。
                                    Row(verticalAlignment = Alignment.Top) {
                                        if (tc.name == TERMUX_BASH_TOOL) {
                                            Icon(
                                                painter = painterResource(R.drawable.ic_termux),
                                                contentDescription = null,
                                                modifier = Modifier
                                                    .padding(top = 2.dp)
                                                    .size(13.dp),
                                                tint = Color.Unspecified,
                                            )
                                            Spacer(Modifier.width(6.dp))
                                        }
                                        SelectionContainer {
                                            Text(
                                                if (tc.name == NATIVE_BASH_TOOL) {
                                                    "\$ " + bashCmd.first
                                                } else {
                                                    bashCmd.first
                                                },
                                                style = mono,
                                            )
                                        }
                                    }
                                    // 调用参数区：command 外其余参数（如 timeout_ms）pretty
                                    // JSON；无其余参数整区省略（命令已全量显示，信息不丢）。
                                    // 开关开 → kotlinx pretty + json 高亮，关 → org.json
                                    // toString(2) 原样（标签行圆钮与参数区/输出区共用开关）。
                                    if (bashCmd.second.isNotBlank()) {
                                        Spacer(Modifier.height(12.dp))
                                        Row(verticalAlignment = Alignment.CenterVertically) {
                                            Text(
                                                "调用参数",
                                                style = MaterialTheme.typography.labelSmall,
                                                color = MaterialTheme.colorScheme.primary,
                                                modifier = Modifier.weight(1f),
                                            )
                                            if (bashArgsIsJson) {
                                                JsonFormatToggle(
                                                    formatOn = formatJson,
                                                    onToggle = { formatJson = !formatJson },
                                                )
                                            }
                                        }
                                        SelectionContainer {
                                            if (bashArgsHighlighted != null) {
                                                Text(bashArgsHighlighted, style = mono)
                                            } else {
                                                Text(bashCmd.second, style = mono)
                                            }
                                        }
                                    }
                                }
                                Spacer(Modifier.height(12.dp))
                            }
                        }
                        // 输出区头：标签 + stdout/stderr 切换 + 字符数提示（统计用
                        // 未分块原串——分块只是渲染层）。
                        item(key = "outhead") {
                            Column {
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
                                    // JSON 格式化圆钮：当前分段（stdout/stderr 各自判定，
                                    // 随 tab 切换）为合法 JSON 时出现，与参数区圆钮共享
                                    // 同一开关状态。
                                    if (!running && outJsonIsJson) {
                                        Spacer(Modifier.width(8.dp))
                                        JsonFormatToggle(
                                            formatOn = formatJson,
                                            onToggle = { formatJson = !formatJson },
                                        )
                                    }
                                }
                                Spacer(Modifier.height(4.dp))
                            }
                        }
                        if (running) {
                            item(key = "running") {
                                Text(
                                    "执行中，完成后自动显示…",
                                    style = mono,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                            }
                        } else {
                            // 存储位置说明行（常驻，输出块最上方）：落盘全文场景
                            // 说明完整输出的存放位置与查询途径——中段经分块已
                            // 完整可看，不再有「已显示头尾」措辞。
                            if (storedNote != null) {
                                item(key = "storenote") {
                                    Text(
                                        storedNote,
                                        style = MaterialTheme.typography.labelSmall.copy(
                                            fontFamily = FontFamily.Monospace,
                                        ),
                                        color = MaterialTheme.colorScheme.tertiary,
                                        modifier = Modifier.padding(bottom = 6.dp),
                                    )
                                }
                            }
                            if (clamp != null) {
                                // 病态兜底（理论上恒不触发，见 pathological 注释）：
                                // 整段回退 b17bf68 的头尾钳制渲染 + 省略说明行。
                                clamp.omissionNote?.let { note ->
                                    item(key = "clampnote") {
                                        Text(
                                            note,
                                            style = MaterialTheme.typography.labelSmall.copy(
                                                fontFamily = FontFamily.Monospace,
                                            ),
                                            color = MaterialTheme.colorScheme.tertiary,
                                            modifier = Modifier.padding(bottom = 6.dp),
                                        )
                                    }
                                }
                                item(key = "clampbody") {
                                    Text(clamp.display, style = mono)
                                }
                            } else if (chunks.isEmpty()) {
                                item(key = "noout") {
                                    Text("（无输出）", style = mono)
                                }
                            } else {
                                // 输出分块：每块一个 Text item（稳定 key "c{i}" 防重组
                                // 错位），样式与旧输出体同款（系统等宽、softWrap
                                // 保持默认开）；虚拟滚动只组合视口内分块。
                                // pretty JSON 生效时逐块 json 语法高亮——高亮在 item
                                // 内 remember 懒计算（不预高亮全文，保持懒加载语义）。
                                itemsIndexed(chunks, key = { _, c -> c.first }) { _, c ->
                                    if (outJsonOn) {
                                        val chunkHighlighted = remember(c.second, tokenizer, theme) {
                                            buildSyntaxAnnotatedString(c.second, "json", tokenizer, theme)
                                        }
                                        Text(chunkHighlighted, style = mono)
                                    } else {
                                        Text(c.second, style = mono)
                                    }
                                }
                            }
                        }
                    }
                }
                HorizontalDivider(
                    thickness = 0.5.dp,
                    color = MaterialTheme.colorScheme.outline.copy(alpha = 0.3f),
                )
                // 底部动作行：整段一键复制（配合长按手动选择）。取舍：保留在
                // LazyColumn 之外的原位（常驻 footer）而非末 item / footer 叠加
                // ——实现最简单（零遮挡/背景处理），复制按钮恒可达（长输出不必
                // 滚到底），说明行文案「点下方『复制完整输出』」也因此恒成立；
                // 仅横向滚动，与 LazyColumn 不同向、无嵌套滚动问题。四枚按钮
                // （命令/stdout/stderr/完整输出）在 360dp 屏放不下时横滑——同
                // SettingsScreen 色板行的横滑先例，放得下时无感知。
                Row(
                    modifier = Modifier
                        .fillMaxWidth()
                        .horizontalScroll(rememberScrollState())
                        .padding(horizontal = 8.dp, vertical = 4.dp),
                    horizontalArrangement = Arrangement.spacedBy(4.dp),
                ) {
                    // bash 类解析成功时复制纯命令文本（不带 JSON wrapper、不含
                    // `$` 提示符装饰）；失败回退复制原始 fullArgs。
                    SoftButton(
                        onClick = {
                            clipboard.setText(AnnotatedString(bashCmd?.first ?: tc.fullArgs))
                        },
                        enabled = bashCmd?.first?.isNotBlank() == true || tc.fullArgs.isNotBlank(),
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
                    // 复制完整输出：未钳制全文（与显示同源——fullOut 与 tc.output
                    // 取更长者），渲染钳制只管显示不影响复制；输出非空才出现，
                    // 成功反馈 = 标签短暂切「已复制」（CodeBlock 同款惯例）。
                    if (!shownOut.isNullOrBlank()) {
                        SoftButton(
                            onClick = {
                                clipboard.setText(AnnotatedString(shownOut.orEmpty()))
                                fullCopied = true
                            },
                            enabled = !running,
                        ) {
                            Icon(
                                Icons.Filled.ContentCopy,
                                contentDescription = null,
                                modifier = Modifier.size(14.dp),
                            )
                            Spacer(Modifier.width(4.dp))
                            Text(if (fullCopied) "已复制" else "复制完整输出")
                        }
                    }
                }
            }
        }
    }
}

/**
 * JSON 格式化切换圆钮：28dp 圆形 Icon 按钮（EnhancedCodeFence 头部按钮同款
 * 形制），开/关两态共用一颗；弹窗内参数区与输出区的多颗按钮共享同一开关
 * 状态（切任意一颗全局生效）。两态视觉：
 * - 开 = [Icons.Outlined.DataObject]（`{}` 形状，JSON 语义）+ primary 着色
 *   ——当前生效，与弹窗内「命令/输出」标签同色系；
 * - 关 = [Icons.Outlined.Code]（`</>` 形状，原文语义）+ outline 中性灰。
 * contentDescription 携带当前状态与动作说明，无障碍语义完整。
 */
@Composable
private fun JsonFormatToggle(formatOn: Boolean, onToggle: () -> Unit) {
    Box(
        contentAlignment = Alignment.Center,
        modifier = Modifier
            .size(28.dp)
            .clip(CircleShape)
            .clickable(onClick = onToggle),
    ) {
        Icon(
            imageVector = if (formatOn) Icons.Outlined.DataObject else Icons.Outlined.Code,
            contentDescription = if (formatOn) {
                "JSON 格式化：已开启，点按恢复原文"
            } else {
                "JSON 格式化：已关闭，点按格式化"
            },
            tint = if (formatOn) {
                MaterialTheme.colorScheme.primary
            } else {
                MaterialTheme.colorScheme.outline
            },
            modifier = Modifier.size(18.dp),
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
private fun Bubble(
    text: String,
    mine: Boolean,
    attachments: List<AttachmentRefUi> = emptyList(),
    hazeState: HazeState,
    /** 长按菜单「编辑」：文本回填 composer 输入框。 */
    onEdit: (String) -> Unit = {},
    /** 图片缩略图点击 → 全屏查看器。 */
    onOpenAttachment: (AttachmentRefUi) -> Unit = {},
) {
    val clipboard = LocalClipboardManager.current
    val haptics = LocalHapticFeedback.current
    // 上下文菜单触点锚点（null = 菜单关闭）——与模型消息 MarkdownBubble 同款。
    var menuAnchor by remember { mutableStateOf<Offset?>(null) }
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
            .padding(horizontal = 4.dp)
            .pointerInput(Unit) {
                detectTapGestures(
                    // 2026-10 改版：点击不再复制（tap 空消费维持既有事件边界，
                    // 与 MarkdownBubble 同理由）；长按弹「复制/编辑」菜单。
                    onTap = {},
                    onLongPress = { offset ->
                        haptics.performHapticFeedback(HapticFeedbackType.LongPress)
                        menuAnchor = offset
                    },
                )
            },
    ) {
        Column(
            // 附件随行（v4 多模态）：chips 在文本上方只读渲染（纵向预览小
            // 卡片——预览图经 getAttachmentThumbnail FFI 反射获取，未就绪/
            // 失败回退 Material 图标占位，LruCache 按附件 id 防滚动重复解码）；
            // 图片卡点击 → 全屏查看器。
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 10.dp),
        ) {
            if (attachments.isNotEmpty()) {
                AttachmentRefChipsRow(
                    refs = attachments,
                    onOpen = onOpenAttachment,
                )
                if (text.isNotBlank()) Spacer(Modifier.height(8.dp))
            }
            if (text.isNotBlank()) {
                Text(
                    text,
                    style = MaterialTheme.typography.bodyMedium,
                )
            }
        }
        // 触点锚定的零尺寸 Box：GlassDropdownMenu 的 Popup 以父布局锚定
        // （模型消息长按菜单同款交互/视觉）。
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
                            value = "edit",
                            label = "编辑",
                            leadingIcon = Icons.Filled.Edit,
                        ),
                    ),
                    onSelect = { opt ->
                        when (opt.value) {
                            "copy" -> clipboard.setText(AnnotatedString(text))
                            "edit" -> onEdit(text)
                        }
                    },
                )
            }
        }
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
    /** 发送（refs 空 = 纯文本，非空 = 带附件）；返回是否送达 runtime。 */
    onSend: (String, List<dev.openslate.mobile.bridge.AttachmentRefUi>) -> Boolean,
    onCancel: () -> Unit,
    showInlineStatus: Boolean,
    inlineStatus: ComposerInlineStatus,
    sharedTransitionScope: SharedTransitionScope,
    hazeState: HazeState,
    onMessage: (String) -> Unit,
    /** 「编辑」请求序号（>0 递增触发）：变化即把 editRequestText 回填输入框。 */
    editRequestSeq: Long = 0L,
    editRequestText: String = "",
    /** 草稿图片卡点击 → 全屏查看器（附件卡为 Ready 图片时携带）。 */
    onOpenDraft: (ComposerAttachment.Ready) -> Unit = {},
    modifier: Modifier = Modifier,
) {
    var input by remember { mutableStateOf("") }
    // ── 附件草稿（多模态输入）────────────────────────────────────
    // chips 两态（导入中/就绪）见 ComposerAttachment；清空时机由提交终态
    // 信号（submitResolution）决定：accepted（request_start）才清，门控
    // 拒绝（Error notice）时草稿与 chips 原地保留供修正重发。
    var attachments by remember { mutableStateOf<List<ComposerAttachment>>(emptyList()) }
    var awaitingResolution by remember { mutableStateOf(false) }
    // 附件选择：图片多选（本期视频入口整体隐藏——设置开关不暴露、附件按钮
    // 恒直开系统图片选择器；Rust 侧 experimental_video 配置与门控不动）。
    var inputFocused by remember { mutableStateOf(false) }
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    // 提交终态观察（seq 门防初始组合误触发）。
    val resolution by RuntimeBridge.submitResolution.collectAsState()
    var seenResolution by remember { mutableStateOf(0L) }
    LaunchedEffect(resolution) {
        if (resolution.seq > seenResolution) {
            seenResolution = resolution.seq
            if (awaitingResolution) {
                awaitingResolution = false
                if (resolution.accepted) {
                    // 引擎已接受：本地草稿使命完成（回显由 bridge 侧补写）。
                    // 只清已发送的部分——文本与就绪 chips；导入仍在途的
                    // chip 保留（不属本次提交）。
                    input = ""
                    attachments = attachments.filterIsInstance<ComposerAttachment.Importing>()
                }
                // rejected：保留草稿与 chips（原因经 ChatScreen snackbar）。
            }
        }
    }
    // 系统照片选择器（Photo Picker，Activity Result API 兼容 API 26+）。
    // 图片 = 多选（上限对齐发送侧 50 张门控）。选回**批量占位先行**（2026-10
    // 用户反馈：原逐个处理到才加 chip，多选时一张张冒出来视觉上像只选了
    // 一张）——预检通过后整批 Importing chips 一次 addAll（全部转圈），再
    // 单个协程串行走导入管线（拷贝 / HEIC 转码 / importAttachment，FFI
    // block_on 一次一个），轮到谁谁原位转就绪 / 失败摘除，先完成先出图。
    fun importUris(uris: List<Uri>) {
        scope.launch {
            // 数量预检（UI 侧提前拦；服务端门控仍是兜底）：chips 已有数量
            // （Ready 图片 + 全部导入中——导入中类型未知，保守按图计）+
            // 本批新增合并计算。
            val existing = attachments.count { it !is ComposerAttachment.Ready || !it.isVideo }
            if (existing + uris.size > MAX_SUBMIT_IMAGES) {
                onMessage("单次最多发送 $MAX_SUBMIT_IMAGES 张图片")
                return@launch
            }
            // 批量类型混合预检：图+视频混批整批不导入（与发送侧混发规则
            // 一致；图片路径 ImageOnly / 视频单选，防御性保留）。
            val types = uris.map { uri ->
                runCatching { context.contentResolver.getType(uri) }.getOrNull().orEmpty()
            }
            if (types.any { it.startsWith("video/") } && types.any { !it.startsWith("video/") }) {
                onMessage("图像与视频请分开发送")
                return@launch
            }
            // 批量占位：key/名称/URI 一次配齐 → 整批 addAll（名称一次 IO
            // 串行查完，失败序号名兜底——chip 标签其实只显大小/导入中，名字
            // 主要供移除钮 contentDescription 与导入管线声明文件名）。
            val isVideoFlags = types.map { it.startsWith("video/") }
            val names = attachmentDisplayNames(context, uris, isVideoFlags)
            val batch = List(uris.size) { idx ->
                ComposerAttachment.Importing(
                    // 尾缀 idx 防同批连发 nanoTime 撞车（粒度粗的机型）。
                    key = "local-${System.nanoTime()}-$idx",
                    displayName = names[idx],
                    // URI 留在 chip 状态供预览位解码（AttachmentThumbnails）。
                    previewUri = uris[idx],
                )
            }
            attachments = attachments + batch
            for ((idx, uri) in uris.withIndex()) {
                val chip = batch[idx]
                when (val r = importComposerAttachment(context, uri, chip.displayName)) {
                    is AttachmentImportResult.Ok ->
                        attachments = attachments.map { if (it.key == chip.key) r.ready else it }
                    is AttachmentImportResult.Failed -> {
                        attachments = attachments.filterNot { it.key == chip.key }
                        onMessage(r.message)
                    }
                }
            }
        }
    }
    val pickImages = rememberLauncherForActivityResult(
        ActivityResultContracts.PickMultipleVisualMedia(MAX_SUBMIT_IMAGES),
    ) { uris ->
        if (uris.isNotEmpty()) importUris(uris)
    }
    // 常驻状态行「整块点按 = 聚焦展开」：行本身不拦输入区可点性，
    // 点击经 FocusRequester 直打输入框（焦点 → 面板展开 + IME 弹出，
    // 与直接点输入框同一路径）。
    val inputFocusRequester = remember { FocusRequester() }
    // 「编辑」请求：用户消息长按菜单 → 文本回填输入框并聚焦（本轮只做
    // 回填，不删历史/不重放；seq 单调递增，同文本重复编辑也再触发）。
    LaunchedEffect(editRequestSeq) {
        if (editRequestSeq > 0L) {
            input = editRequestText
            inputFocusRequester.requestFocus()
        }
    }
    // 输入区字号/行盒/上下 padding 一律保持原单条输入条的原始值
    //（bodyLarge 16sp/24sp 行盒 + 10dp 上下边距）→ 输入列单独就把条高
    // 锚在原单条高度（20dp + 24sp，任意 fontScale 精确成立）；右侧状态
    // 块（两行 labelSmall = 32sp + 1dp 间隙）恒小于该预算，垂直居中于
    // 条内不撑高。
    // 未聚焦单行横向溢出标记（onTextLayout 布局相位回填）：驱动文字
    // 末尾 DstIn 渐隐遮罩。
    var inputOverflow by remember { mutableStateOf(false) }
    // 发送动作（send 按钮点击；纯文本与带附件两路）：
    // - 纯文本（无就绪附件）：维持既有语义——非空即发、送达才清空输入。
    // - 带附件：契约预检（文本必填非空白 / 纯图 XOR 纯视频 / 图 ≤50 张），
    //   通过后携附件引用提交；**不立即清空**——等待 submitResolution 终态，
    //   accepted（request_start）才清；门控拒绝时草稿与 chips 原地保留，
    //   错误原因经 ChatScreen snackbar（Error notice 自动展示）。
    // - 校验失败经 onMessage（snackbar）提示，不发送。
    fun composerSend() {
        if (running) {
            onCancel()
            return
        }
        if (awaitingResolution) return
        val text = input.trim()
        val ready = attachments.filterIsInstance<ComposerAttachment.Ready>()
        if (ready.isEmpty()) {
            if (text.isNotEmpty()) {
                if (onSend(text, emptyList())) input = "" else onMessage("发送失败，请稍后重试")
            }
            return
        }
        if (text.isEmpty()) {
            onMessage("请为附件输入文字说明")
            return
        }
        val videos = ready.count { it.isVideo }
        val images = ready.size - videos
        if (images > 0 && videos > 0) {
            onMessage("图像与视频请分开发送")
            return
        }
        if (images > MAX_SUBMIT_IMAGES) {
            onMessage("单次最多发送 $MAX_SUBMIT_IMAGES 张图片")
            return
        }
        val refs = ready.map { dev.openslate.mobile.bridge.AttachmentRefUi(it.id, it.mime, it.filename) }
        if (!onSend(text, refs)) {
            onMessage("发送失败，请稍后重试")
            return
        }
        awaitingResolution = true
    }
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
                Column(
                    // chips 增删 / 导入完成时高度平滑过渡（草稿条出现/消失）。
                    modifier = Modifier.animateContentSize(),
                ) {
                    ComposerAttachmentChipsRow(
                        items = attachments,
                        onRemove = { key -> attachments = attachments.filterNot { it.key == key } },
                        // 草稿图片卡点击 → 全屏查看器（原 URI 解码大图）。
                        onOpen = onOpenDraft,
                        modifier = Modifier.padding(start = 14.dp, end = 14.dp, top = 10.dp),
                    )
                    // 未聚焦左右布局：附件按钮 + 输入区在左（weight 占剩余宽，
                    // 原字号单行 + 末尾渐隐），右侧上下两行状态块贴右缘（wrap 内容）。
                    // 聚焦后状态块横向收出、输入区扩至全宽恢复多行。
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        // 附件按钮（输入区图标排首位的操作位）：**同心嵌在胶囊
                        // 左圆头内**（2026-10 用户反馈「按钮与胶囊之间有缝、
                        // 对不齐」）——胶囊 r=20dp，40dp 触达圆同心（start 0，
                        // 圆心恰在左端圆弧圆心 x=20dp 处，触达圆与圆弧重合）；
                        // 可见圆底 38dp 内缩 1dp（r=19，半径一致性：与 r=20
                        // 圆头留发丝玻璃环），深浅主题下衔接处无脏缝。触达
                        // 40dp 满足热区下限；文本起始 padding 8dp 让出按钮区
                        // （光标起点 48dp 不被遮挡）。恒直开系统图片多选选择
                        // 器（ImageOnly 系统层过滤；本期视频入口整体隐藏）。
                        Box(
                            contentAlignment = Alignment.Center,
                            modifier = Modifier
                                .size(40.dp)
                                .clip(CircleShape)
                                .clickable {
                                    pickImages.launch(
                                        PickVisualMediaRequest(
                                            ActivityResultContracts.PickVisualMedia.ImageOnly,
                                        ),
                                    )
                                },
                        ) {
                            Box(
                                contentAlignment = Alignment.Center,
                                modifier = Modifier
                                    .size(38.dp)
                                    .clip(CircleShape)
                                    .background(
                                        MaterialTheme.colorScheme.onSurface.copy(alpha = 0.06f),
                                    ),
                            ) {
                                Icon(
                                    Icons.Outlined.AddPhotoAlternate,
                                    contentDescription = "添加图片附件",
                                    tint = MaterialTheme.colorScheme.outline,
                                    modifier = Modifier.size(16.dp),
                                )
                            }
                        }
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
                                .onFocusChanged {
                                    inputFocused = it.isFocused
                                    onInputFocusChange(it.isFocused)
                                }
                                .padding(start = 8.dp, end = 14.dp, top = 10.dp, bottom = 10.dp)
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
                } // Column（chips 条 + 输入行）
            }
            Spacer(Modifier.width(6.dp))
            Box(
                contentAlignment = Alignment.Center,
                modifier = Modifier
                    .size(36.dp)
                    .shadow(elevation = 2.dp, shape = RoundedCornerShape(50), clip = false)
                    .clip(RoundedCornerShape(50))
                    .background(MaterialTheme.colorScheme.primary)
                    .clickable { composerSend() },
            ) {
                when {
                    running -> Icon(
                        Icons.Filled.Stop,
                        contentDescription = "取消",
                        tint = MaterialTheme.colorScheme.onPrimary,
                        modifier = Modifier.size(18.dp),
                    )
                    // 附件提交等待引擎接受（异步门控）：转圈而非可再发。
                    awaitingResolution -> CircularProgressIndicator(
                        strokeWidth = 2.dp,
                        color = MaterialTheme.colorScheme.onPrimary,
                        modifier = Modifier.size(16.dp),
                    )
                    else -> Icon(
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

/** 带附件单次提交的图片数上限（Submit 门控常量，设计 §3.3；客户端预检，
 *  Rust 侧 gate 为最终权威）。 */
private const val MAX_SUBMIT_IMAGES = 50
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

// ── 工具调用审批卡 ──────────────────────────────────────────────────

/** 风险等级 → (中文标签, 图标, 语义色)；未知值兜底显示原始串 + 中性盾形图标。 */
@Composable
private fun approvalRiskVisual(level: String): Triple<String, androidx.compose.ui.graphics.vector.ImageVector, Color> =
    when (level) {
        "low" -> Triple("低风险", Icons.Outlined.CheckCircle, successGreen())
        "medium" -> Triple("中风险", Icons.Outlined.Warning, warningAmber())
        "high" -> Triple("高风险", Icons.Outlined.GppMaybe, MaterialTheme.colorScheme.error)
        else -> Triple(level, Icons.Outlined.GppMaybe, MaterialTheme.colorScheme.outline)
    }

/**
 * 工具调用审批卡（Scaffold 底部横幅位）：待批调用的完整上下文与五项决定。
 * 视觉对齐 ask_user 家族 / TermuxStallBanner（磨砂玻璃面板 + 16dp 圆角 +
 * 细边框 + 轻阴影），入场动画克制（220ms 底部展开淡入，仅播一次）。
 *
 * 内容分级：头部（图标 + 标题 + 风险 chip）→ 工具名（等宽）→ MCP 归属行
 * （server 存在时）→ 参数预览（等宽小字，3 行截断、可展开）→ 触发原因
 * （manual = 权限等级说明；auto_risk = AI 审查 + 理由）→ 决定按钮。
 * 允许类做主强调（primary 两枚 + secondary 两枚 MCP），「拒绝」做次强调
 * （底部静音文字行）——不与允许并列争抢。
 *
 * 「拒绝」必须发送 deny：纯 UI 关闭会让 Rust 侧审批等待永久挂起（turn 卡死）。
 * 首答即锁按钮：Rust 侧首答赢、重复发送无害，但 UI 反馈要干脆。
 */
@Composable
private fun ApprovalCard(
    pending: PendingApprovalUi,
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
) {
    // 入场（首次组合播一次；id 换新 = 新请求重播）。出场随 pendingApproval
    // 清空直接离场，不做过场（克制）。
    val enterState = remember(pending.id) {
        MutableTransitionState(false).apply { targetState = true }
    }
    var answered by remember(pending.id) { mutableStateOf(false) }
    var argsExpanded by remember(pending.id) { mutableStateOf(false) }
    // 收起态参数是否被截断（onTextLayout 回填）：没截断就不显示展开开关。
    var argsTruncated by remember(pending.id) { mutableStateOf(false) }
    AnimatedVisibility(
        visibleState = enterState,
        enter = fadeIn(tween(220, easing = FastOutSlowInEasing)) +
            expandVertically(
                expandFrom = Alignment.Bottom,
                animationSpec = tween(220, easing = FastOutSlowInEasing),
            ),
        exit = fadeOut(tween(180)) + shrinkVertically(tween(180)),
    ) {
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
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    // 头部行：面板身份 + 风险 chip（语义色 pill）。
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(
                            Icons.Outlined.Gavel,
                            contentDescription = null,
                            modifier = Modifier.size(16.dp),
                            tint = MaterialTheme.colorScheme.primary,
                        )
                        Spacer(Modifier.width(6.dp))
                        Text("工具调用确认", style = MaterialTheme.typography.titleSmall)
                        Spacer(Modifier.weight(1f))
                        val (label, icon, tint) = approvalRiskVisual(pending.riskLevel)
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier
                                .clip(RoundedCornerShape(50))
                                .background(tint.copy(alpha = 0.12f))
                                .border(0.5.dp, tint.copy(alpha = 0.4f), RoundedCornerShape(50))
                                .padding(horizontal = 8.dp, vertical = 3.dp),
                        ) {
                            Icon(
                                icon,
                                contentDescription = null,
                                modifier = Modifier.size(12.dp),
                                tint = tint,
                            )
                            Spacer(Modifier.width(4.dp))
                            Text(
                                label,
                                style = MaterialTheme.typography.labelSmall,
                                color = tint,
                            )
                        }
                    }
                    // 工具名（注册全名，等宽；标识类短文本用 MonoFont 子集）。
                    Text(
                        pending.toolName,
                        style = MaterialTheme.typography.labelLarge,
                        fontFamily = MonoFont,
                    )
                    // MCP 归属行（server 存在时；host 内置也带 host 前缀键）。
                    pending.server?.let { server ->
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Icon(
                                painter = painterResource(R.drawable.ic_mcp),
                                contentDescription = null,
                                modifier = Modifier.size(12.dp),
                                tint = MaterialTheme.colorScheme.secondary,
                            )
                            Spacer(Modifier.width(4.dp))
                            Text(
                                "MCP · $server",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                    }
                    // 参数预览：等宽小字（任意 JSON 需全字形 → 系统 Monospace，
                    // 不换 MonoFont 子集），收起 3 行截断、点击展开（上限 12 行）。
                    if (pending.arguments.isNotBlank()) {
                        Text(
                            pending.arguments,
                            style = MaterialTheme.typography.bodySmall.copy(
                                fontFamily = FontFamily.Monospace,
                            ),
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                            maxLines = if (argsExpanded) 12 else 3,
                            overflow = TextOverflow.Ellipsis,
                            softWrap = true,
                            onTextLayout = { result ->
                                if (!argsExpanded) argsTruncated = result.hasVisualOverflow
                            },
                            modifier = Modifier
                                .fillMaxWidth()
                                .clip(RoundedCornerShape(8.dp))
                                .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.45f))
                                .clickable { argsExpanded = !argsExpanded }
                                .padding(horizontal = 8.dp, vertical = 6.dp),
                        )
                        // 展开开关：仅在确实被截断（或已展开）时出现。
                        if (argsTruncated || argsExpanded) {
                            Row(
                                verticalAlignment = Alignment.CenterVertically,
                                modifier = Modifier
                                    .clip(RoundedCornerShape(8.dp))
                                    .clickable { argsExpanded = !argsExpanded }
                                    .padding(horizontal = 2.dp, vertical = 2.dp),
                            ) {
                                Icon(
                                    if (argsExpanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                                    contentDescription = if (argsExpanded) "收起参数" else "展开参数",
                                    modifier = Modifier.size(13.dp),
                                    tint = MaterialTheme.colorScheme.outline,
                                )
                                Spacer(Modifier.width(4.dp))
                                Text(
                                    if (argsExpanded) "收起参数" else "展开参数",
                                    style = MaterialTheme.typography.labelSmall,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                            }
                        }
                    }
                    // 触发原因行：manual = 权限等级说明；auto_risk = AI 审查
                    // 结论 + 理由（note 可空，缺省按 manual 渲染）。
                    val autoRisk = pending.trigger == "auto_risk"
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Icon(
                            if (autoRisk) Icons.Outlined.Psychology else Icons.Outlined.Tune,
                            contentDescription = null,
                            modifier = Modifier.size(13.dp),
                            tint = MaterialTheme.colorScheme.outline,
                        )
                        Spacer(Modifier.width(6.dp))
                        Text(
                            if (autoRisk) "AI 审查判定高风险" else "权限等级：全部手动",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    if (autoRisk && !pending.note.isNullOrBlank()) {
                        Text(
                            pending.note,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                            modifier = Modifier.padding(start = 19.dp),
                        )
                    }
                    // ── 决定区 ──
                    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                            SoftButton(
                                onClick = {
                                    answered = true
                                    RuntimeBridge.approveForSession(pending.id)
                                },
                                enabled = !answered,
                                modifier = Modifier.weight(1f),
                            ) { Text("本会话允许") }
                            SoftButton(
                                onClick = {
                                    answered = true
                                    RuntimeBridge.approveAlways(pending.id)
                                },
                                enabled = !answered,
                                modifier = Modifier.weight(1f),
                            ) { Text("一直允许") }
                        }
                        pending.server?.let { server ->
                            Text(
                                "针对 $server 服务器的全部工具",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                            Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                                SoftButton(
                                    onClick = {
                                        answered = true
                                        RuntimeBridge.approveMcpForSession(pending.id)
                                    },
                                    enabled = !answered,
                                    tint = MaterialTheme.colorScheme.secondary,
                                    modifier = Modifier.weight(1f),
                                ) {
                                    Text(
                                        "本会话允许 $server 工具",
                                        style = MaterialTheme.typography.labelMedium,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                }
                                SoftButton(
                                    onClick = {
                                        answered = true
                                        RuntimeBridge.approveMcpAlways(pending.id)
                                    },
                                    enabled = !answered,
                                    tint = MaterialTheme.colorScheme.secondary,
                                    modifier = Modifier.weight(1f),
                                ) {
                                    Text(
                                        "一直允许 $server 工具",
                                        style = MaterialTheme.typography.labelMedium,
                                        maxLines = 1,
                                        overflow = TextOverflow.Ellipsis,
                                    )
                                }
                            }
                        }
                        // 拒绝：次强调（静音文字行，居中），不与允许并列争抢；
                        // 必须真正发送 deny（挂起语义见组件 KDoc）。
                        Row(
                            horizontalArrangement = Arrangement.Center,
                            modifier = Modifier
                                .fillMaxWidth()
                                .clip(RoundedCornerShape(8.dp))
                                .clickable(enabled = !answered) {
                                    answered = true
                                    RuntimeBridge.deny(pending.id)
                                }
                                .padding(vertical = 4.dp),
                        ) {
                            Icon(
                                Icons.Outlined.Block,
                                contentDescription = null,
                                modifier = Modifier.size(14.dp),
                                tint = MaterialTheme.colorScheme.error.copy(alpha = 0.75f),
                            )
                            Spacer(Modifier.width(4.dp))
                            Text(
                                "拒绝执行",
                                style = MaterialTheme.typography.labelMedium,
                                color = MaterialTheme.colorScheme.error.copy(alpha = 0.75f),
                            )
                        }
                    }
                }
            }
        }
    }
}

/**
 * Termux 停滞连接面板：termux 命令发出 0.5s 仍无应答时悬浮在输入区上方
 * （与输入胶囊同语言：磨砂玻璃面板 + 细边框 + 轻阴影）。**检查清单面板**
 * （与设置页 MCP 控制区同一套视觉，经共享组件 TermuxStatusChecklist
 * 渲染，与 composer 聚焦面板的 Termux 子面板同源）：按探测阶梯
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
                    // 检查记录主体：四行阶梯映射（共享组件，与 composer
                    // 聚焦面板的 Termux 子面板同源同款）。
                    TermuxStatusChecklist(status = status, modifier = Modifier.padding(top = 8.dp))
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
