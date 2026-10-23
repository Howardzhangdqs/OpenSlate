package dev.openslate.mobile.ui

import android.content.Context
import android.os.Build
import android.os.SystemClock
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.AnimatedVisibilityScope
import androidx.compose.animation.SharedTransitionScope
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.Terminal
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.R
import dev.openslate.mobile.bridge.ConfigUi
import dev.openslate.mobile.bridge.ModelUi
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.TermuxDiagnostics

/**
 * 输入框聚焦面板：点击聚焦输入框（进入编辑状态）时，从 composer 上方
 * 展开的一条状态带 + 模型 / Thinking Effort 双下拉控件。
 *
 * 交互形态（与 IME 共存）：
 * - 面板挂在 ChatScreen 底部悬浮列（TermuxStallBanner 同位、同语言：
 *   GlassPanel + 细边框 + 轻阴影，expandVertically 自底部展开）。该列
 *   整体在 imePadding 之上，键盘弹起时列底随 IME 上移——面板在 composer
 *   上方向上生长，composer 相对键盘的位置不变，不存在被顶飞的可能，
 *   也不与 IME 动画争抢布局（同一布局子树一起移动）。
 * - 首行 Termux 状态区可点击：在面板内展开/收起 MCP 五灯子面板
 *   （McpLightsTracker → probeMcpLights 探测 + 共享映射 mcpLightRow
 *   渲染，与设置页 MCP 区同一口径：五项独立判定成败、失败时机与
 *   「打开 Termux」时机同源；展开时走 gateRefresh 智能门控（无快照/
 *   非全绿 → 全量重检含自动拉起 host，全绿 → 廉价探活比对），ON_RESUME
 *   门控由 ChatScreen 全局观察承担——从 Termux 切回即触发重检，不要求
 *   面板展开）。首行 Termux 文案/颜色本身也由五灯快照推导（与设置页
 *   同口径，host 拉起期间显示「MCP host 启动中…」过渡态）。右缘
 *   MCP / tools 计数保留原状、不参与点击。
 * - 未聚焦时 AnimatedVisibility 不参与组合，高度/间距零变化（硬性要求）。
 *
 * 生命周期：
 * - 显示 = 输入框持有焦点（编辑状态）。点空白（既有手势：hide 键盘 +
 *   clearFocus）、发送消息后（ChatScreen 在 onSend 里 clearFocus）即收起。
 * - 数据在每次聚焦瞬间一次性读取（LaunchedEffect(focused, ready)），
 *   无轮询、无定时器；持续聚焦期间不重读（可见性/MCP 变更只能来自
 *   设置页或后台自愈 reload，下次聚焦自然刷新）。失焦不清缓存：再次
 *   聚焦先显示上次快照、后台静默刷新，避免每次聚焦闪加载态。
 *
 * 数据口径：
 * - 首行 Termux 活性：[McpLightsTracker] 五灯快照（[McpLightsUiState]，
 *   与设置页 MCP 区同一探测管线/同门控）经 [termuxLiveStatus] 推导——
 *   host 探活通过即「Termux 运行中」，自动拉起期间显示「MCP host 启动
 *   中…」过渡态，卡点按五灯终态分流。
 * - MCP/tools 计数与 server 名单：[ComposerMcpStatusTracker] 状态流
 *   （host 活性探测 + 下游 server 连接数 + Agent 工具计数）。MCP 数 =
 *   **正常连接着的下游 server 数**——不是注册总数、不是工具总数。
 * - 常驻状态：未聚焦时输入框内部下方一行（模型名 · effort 档 · Termux/MCP
 *   状态）；聚焦展开后同源信息移交卡片首行，模型名/effort 档经
 *   sharedBounds 做「输入框 → 卡片」的位移动画（key composer.model /
 *   composer.effort），收起反向。聚焦展开时调一次 tracker.refresh。
 * - 模型选择：选项 = models 条目（levels 代号不再出现在选项里；capabilities
 *   也只直绑条目，见设置页会话页）。当前值经 [RuntimeBridge.modelEntryOf]
 *   解析到条目（levels 遗留兜底）再显示；显示走 state.modelAlias /
 *   model_changed 事件回填——与顶栏副标题、发消息时实际生效的是同一状态
 *   源，面板选了什么下次请求就用什么，不存在分裂。切换经 onSelectModel
 *   （ChatScreen 接 [RuntimeBridge.selectModel]）双写：set_model 会话级热
 *   切换 + setCapability("main") 持久化为启动默认——「选了即默认」的语义
 *   闭环，设置页会话页因此不再有主对话绑定行。
 * - Thinking Effort：模型下拉旁的紧凑档位选择，仅当前模型解析到条目且
 *   家族表有支持档位时出现（不占位）。选中 = 改该条目 reasoning_effort
 *   （onSelectEffort → ChatScreen 用 config 原条目 copy 后 upsertModel
 *   落盘；null = Auto 不传参跟随 provider 默认），config_changed 回刷回显
 *   ——与模型「选了即默认」同语义层级。
 */

/**
 * 面板本体。visible 由 ChatScreen 的输入框焦点状态驱动；聚焦展开时触发
 * [ComposerMcpStatusTracker.refresh]（上次快照先行显示、异步探测到位回灌），
 * Termux/MCP 活性经 tracker 状态流回灌。sharedTransitionScope 由 ChatScreen
 * 的 SharedTransitionLayout 下发：模型/effort 值文本与输入框常驻状态行做
 * sharedBounds 位移动画。hazeState 透传给模型选择弹层（Haze 局部模糊内容源）。
 */
@Composable
internal fun ComposerFocusPanel(
    visible: Boolean,
    modelAlias: String,
    config: ConfigUi,
    onSelectModel: (String) -> Unit,
    onSelectEffort: (entry: String, effort: String?) -> Unit,
    sharedTransitionScope: SharedTransitionScope,
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
    hazeState: dev.chrisbanes.haze.HazeState,
) {
    val context = LocalContext.current
    // Termux/MCP 活性快照（tracker 持有状态流；未聚焦时输入框常驻状态行
    // 也消费同源数据，见 ChatScreen）。live 只供 MCP/tools 计数与 server
    // 名单；首行 Termux 状态文案/颜色由五灯快照（lights，与设置页 MCP 区
    // 同一口径）推导，见 termuxLiveStatus。
    val live by ComposerMcpStatusTracker.state.collectAsState()
    val lights by McpLightsTracker.state.collectAsState()
    // 聚焦展开刷新一次：上次快照立即上屏（无空白窗），探测结果异步回灌。
    LaunchedEffect(visible) {
        if (visible) ComposerMcpStatusTracker.refresh(context)
    }
    AnimatedVisibility(
        visible = visible,
        enter = fadeIn() + expandVertically(expandFrom = Alignment.Bottom),
        exit = fadeOut() + shrinkVertically(shrinkTowards = Alignment.Bottom),
    ) {
        val animatedVisibilityScope = this
        Box(Modifier.fillMaxWidth().padding(horizontal = 10.dp)) {
            GlassPanel(
                glass = glass,
                rootCoords = rootCoords,
                modifier = Modifier
                    .fillMaxWidth()
                    .shadow(elevation = 2.dp, shape = RoundedCornerShape(16.dp), clip = false),
                shape = RoundedCornerShape(16.dp),
                // 磨砂参数与 TermuxStallBanner 同款（低版本退化为近实底蒙层）。
                tint = MaterialTheme.colorScheme.surfaceVariant.copy(
                    alpha = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) 0.6f else 0.92f,
                ),
                border = true,
            ) {
                Column(
                    Modifier
                        .fillMaxWidth()
                        .padding(horizontal = 12.dp, vertical = 10.dp)
                        // 探测结果回灌（计数/名单变化）时高度过渡平滑化。
                        .animateContentSize(),
                ) {
                    val (termuxText, termuxColor) = termuxLiveStatus(lights)
                    // ── 首行：Termux 连接状态（排首位）+ 右缘 MCP 连接数 ──
                    // MCP 数 = 正常连接着的下游 server 数（host 活着才有值；
                    // 死亡/未知显示 —）。工具计数已删（卡片精简）。
                    // 左侧状态区按钮化：点击在面板内展开/收起 Termux 阶梯
                    // 子面板（下方 AnimatedVisibility）；状态色文案保留，
                    // 尾缘箭头随展开态 180° 旋转（ModelPickerRow 同语言）。
                    var termuxExpanded by remember { mutableStateOf(false) }
                    val termuxArrowAngle by animateFloatAsState(
                        targetValue = if (termuxExpanded) 180f else 0f,
                        label = "composerTermuxArrow",
                    )
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier.fillMaxWidth(),
                    ) {
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier
                                // 竖向加垫出触达高度（≈28dp）；不加水平内边
                                // 距，Terminal 图标与下方行保持同一左缘网格。
                                .clip(RoundedCornerShape(8.dp))
                                .clickable { termuxExpanded = !termuxExpanded }
                                .padding(vertical = 6.dp),
                        ) {
                            Icon(
                                Icons.Outlined.Terminal,
                                contentDescription = null,
                                modifier = Modifier.size(13.dp),
                                tint = termuxColor,
                            )
                            Spacer(Modifier.width(4.dp))
                            Text(
                                // 探测细节（耗时/HTTP 码）只入快照供诊断，不进
                                // 常驻 UI——运行状态文案保持干净。
                                termuxText,
                                style = MaterialTheme.typography.labelMedium,
                                color = termuxColor,
                            )
                            Spacer(Modifier.width(4.dp))
                            Icon(
                                Icons.Filled.KeyboardArrowDown,
                                contentDescription = if (termuxExpanded) {
                                    "收起 Termux 连接详情"
                                } else {
                                    "展开 Termux 连接详情"
                                },
                                tint = MaterialTheme.colorScheme.outline,
                                // 同 ModelPickerRow：绘制相位读取，避免逐帧重组。
                                modifier = Modifier
                                    .size(14.dp)
                                    .graphicsLayer { rotationZ = termuxArrowAngle },
                            )
                        }
                        Spacer(Modifier.weight(1f))
                        // MCP 官方品牌 logo（非 Material 图标）：用户指定的
                        // 「一律 Material 图标」规范例外，仅此一处；资源为
                        // res/drawable/ic_mcp.xml（官方 SVG 机械转换生成，
                        // 来源与许可见该文件头注释）。13dp 与 Terminal 图标
                        // 同排光学对齐。
                        Icon(
                            painter = painterResource(R.drawable.ic_mcp),
                            contentDescription = null,
                            modifier = Modifier.size(13.dp),
                            tint = if ((live.connectedServers ?: 0) > 0) {
                                MaterialTheme.colorScheme.secondary
                            } else {
                                MaterialTheme.colorScheme.outline
                            },
                        )
                        Spacer(Modifier.width(4.dp))
                        Text(
                            "MCP ${live.connectedServers ?: "—"}",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                        // tools 独立计数（与 MCP 并列的第二个小组件）：
                        // 当前 Agent 可用工具总数（可见性过滤后），null =
                        // 注册表不可用显示 —。Build 图标回归——工具计数
                        // 重新有独立语义，不再并入 MCP 显示。
                        Spacer(Modifier.width(14.dp))
                        Icon(
                            Icons.Outlined.Build,
                            contentDescription = null,
                            modifier = Modifier.size(13.dp),
                            tint = if ((live.agentTools ?: 0) > 0) {
                                MaterialTheme.colorScheme.secondary
                            } else {
                                MaterialTheme.colorScheme.outline
                            },
                        )
                        Spacer(Modifier.width(4.dp))
                        Text(
                            "tools ${live.agentTools ?: "—"}",
                            style = MaterialTheme.typography.labelMedium,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    // ── MCP 五灯子面板：首行状态区点击展开/收起 ──
                    // 折叠语言对齐 ToolNameTemplateSection（expandVertically +
                    // 200ms 淡入淡出）。数据 = McpLightsTracker（与设置页 MCP
                    // 区同一探测管线 probeMcpLights、同一五灯映射 mcpLightRow
                    // ——五项独立判定成败）；展开时走 gateRefresh（无快照/非
                    // 全绿→全量重检含自动拉起 host、全绿→廉价 hostAlive 比对，
                    // 与设置页同款门控）；ON_RESUME 门控已上移 ChatScreen 全局
                    // 观察（从 Termux 切回即触发，不再要求面板展开）。不再触发
                    // TermuxStallTracker（onTermuxConnectionRestored 回归停滞
                    // 面板专属语义）。五行全显（checked→终态、当前步骤→进行
                    // 中、其余→待检测；hostAlive 拉起窗口显示「MCP host 启动
                    // 中…」过渡态），高度有界，小屏与 IME 共存无需滚动。
                    val mcpActions = remember(context) { standardMcpLightActions(context) }
                    LaunchedEffect(termuxExpanded) {
                        if (termuxExpanded) McpLightsTracker.gateRefresh(context)
                    }
                    AnimatedVisibility(
                        visible = termuxExpanded,
                        enter = expandVertically() + fadeIn(tween(200)),
                        exit = shrinkVertically() + fadeOut(tween(200)),
                    ) {
                        StatusChecklist(
                            buildList {
                                for (key in MCP_LIGHT_KEYS) {
                                    mcpLightRow(
                                        key = key,
                                        result = lights.result ?: McpLightsResult(),
                                        displayDetails = true,
                                        actions = mcpActions,
                                    )?.let { add(it) }
                                }
                            },
                            modifier = Modifier.padding(top = 6.dp),
                        )
                    }
                    // 次行：已连接 server 名单（精简保留；未连接/为空不占位）。
                    if (live.serverNames.isNotEmpty()) {
                        Text(
                            live.serverNames.joinToString(" · "),
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                            modifier = Modifier.padding(top = 2.dp),
                        )
                    }
                    HorizontalDivider(
                        thickness = 0.5.dp,
                        color = MaterialTheme.colorScheme.outline.copy(alpha = 0.3f),
                        modifier = Modifier.padding(vertical = 7.dp),
                    )
                    // 双控件行：模型下拉与 Thinking Effort 下拉各占
                    // 50%（各 weight(1f)）；effort 控件仅当前模型
                    // 解析到条目且家族表有支持档位时组合，未选/
                    // 不支持时模型下拉独占整行（不占位）。
                    val currentModel = resolvedCurrentModel(modelAlias, config)
                    val effortLevels = currentModel?.let {
                        ReasoningEffortLevels.allowed(it.supportsReasoning, it.modelId)
                    }
                    Row(
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        ModelPickerRow(
                            modelAlias, config, onSelectModel, hazeState,
                            sharedTransitionScope = sharedTransitionScope,
                            animatedVisibilityScope = animatedVisibilityScope,
                            modifier = Modifier.weight(1f),
                        )
                        if (currentModel != null && effortLevels != null) {
                            EffortPickerRow(
                                entry = currentModel.entry,
                                levels = effortLevels,
                                current = currentModel.reasoningEffort,
                                onSelectEffort = onSelectEffort,
                                sharedTransitionScope = sharedTransitionScope,
                                animatedVisibilityScope = animatedVisibilityScope,
                                modifier = Modifier.weight(1f),
                                hazeState = hazeState,
                            )
                        }
                    }
                }
            }
        }
    }
}

/** 模型下拉控件选项：models 条目键（选中/回传语义）+ 显示主文字
 *  （经共享助手换「Provider显示名/modelId」，条目键不直出 UI）。 */
private data class ComposerModelOption(
    val entry: String,
    val label: String,
)

/**
 * 可选模型全集 = models 条目（levels 代号/capabilities 不再进选项——
 * UI 已去映射层，功能与选择器一律直绑条目）。主文字 =
 * [RuntimeBridge.modelAliasDisplay]。
 */
private fun composerModelOptions(config: ConfigUi): List<ComposerModelOption> =
    config.models.map { m ->
        ComposerModelOption(
            entry = m.entry,
            label = RuntimeBridge.modelAliasDisplay(m.entry, config),
        )
    }

/** 当前选中模型条目：会话别名经 modelEntryOf 解析到 models 反查；
 *  未选 / 未命中（levels 遗留兜底也落空）= null。面板双控件行与输入框
 *  常驻状态行（ChatScreen）共用，保证两处「当前模型」恒同源。 */
internal fun resolvedCurrentModel(modelAlias: String, config: ConfigUi): ModelUi? =
    config.models.firstOrNull { it.entry == RuntimeBridge.modelEntryOf(modelAlias, config) }

/** 首行 Termux 状态文案 + 语义色（面板首行与输入框状态点共用）：由五灯
 *  快照（[McpLightsTracker] → [McpLightsUiState]）推导——与设置页 MCP 区
 *  同一口径。自上而下取首个命中：host 探活通过即「Termux 运行中」；host
 *  自动拉起期间（restartingHost）显示「MCP host 启动中…」过渡态而非把
 *  host 未起误报成 Termux 未运行；host 探活未过的具体卡点（未安装/无
 *  权限/通道未放行/host 未装）按五灯终态如实分流；部分完成无结论时按
 *  probing 显示「检测中…/未检测」。 */
@Composable
internal fun termuxLiveStatus(lights: McpLightsUiState): Pair<String, Color> {
    val res = lights.result
    return when {
        res == null -> if (lights.probing) {
            "检测中…" to MaterialTheme.colorScheme.outline
        } else {
            "未检测" to MaterialTheme.colorScheme.outline
        }
        "hostAlive" in res.checked && res.hostAlive ->
            "Termux 运行中" to MaterialTheme.colorScheme.secondary
        res.restartingHost ->
            "MCP host 启动中…" to MaterialTheme.colorScheme.secondary
        "termuxInstalled" in res.checked && !res.installed ->
            "Termux 未安装" to MaterialTheme.colorScheme.error
        "termuxPerm" in res.checked && !res.perm ->
            "Termux 权限未授予" to MaterialTheme.colorScheme.error
        "termuxAllowed" in res.checked && !res.allowed ->
            // NoResponse = Termux 本体没起来；其余（PolicyRejected 等）=
            // 通道配置问题，文案分流不误导。
            if (res.channel is TermuxDiagnostics.ChannelResult.NoResponse) {
                "Termux 未运行" to MaterialTheme.colorScheme.error
            } else {
                "Termux 通道未放行" to MaterialTheme.colorScheme.error
            }
        "hostInstalled" in res.checked && !res.hostInstalled ->
            "MCP host 未安装" to MaterialTheme.colorScheme.error
        "hostAlive" in res.checked && !res.hostAlive ->
            // restartedHost=true = 本轮已尝试拉起仍无应答 → 如实未运行；
            // 否则拉起尚未发生（如用户操作在途的门控轮），按启动中显示。
            if (res.restartedHost) {
                "MCP host 未运行" to MaterialTheme.colorScheme.error
            } else {
                "MCP host 启动中…" to MaterialTheme.colorScheme.secondary
            }
        else -> if (lights.probing) {
            "检测中…" to MaterialTheme.colorScheme.outline
        } else {
            "未检测" to MaterialTheme.colorScheme.outline
        }
    }
}

/** 输入框内常驻状态（未聚焦显示；聚焦后同源信息在面板卡片展开）。
 *  termuxText 兼作状态图标的 contentDescription。 */
internal data class ComposerInlineStatus(
    val modelDisplay: String,
    /** 宽度不足时的短版显示（仅 modelId，如「glm-5.2」）；未解析到条目
     *  时与 modelDisplay 同值（降级无意义但无害）。 */
    val modelId: String,
    /** null = 当前模型不支持档位选择，不显示 effort 段。 */
    val effortLabel: String?,
    /** 正常连接的下游 server 数；null = host 死亡/未知（显示 —）。 */
    val mcpConnected: Int?,
    /** 当前 Agent 可用工具总数（可见性过滤后）；null = 注册表不可用（显示 —）。 */
    val agentTools: Int?,
    val termuxText: String,
    val termuxColor: Color,
)

/** 常驻状态组装（ChatScreen 组合期调用；与面板卡片同源同判定）。
 *  Termux 状态文案/颜色由五灯快照（lights，与设置页 MCP 区同口径）经
 *  [termuxLiveStatus] 推导；live 保留供 MCP/tools 计数。 */
@Composable
internal fun composerInlineStatus(
    modelAlias: String,
    config: ConfigUi,
    live: McpLiveSnapshot,
    lights: McpLightsUiState,
): ComposerInlineStatus {
    val model = resolvedCurrentModel(modelAlias, config)
    val effortLabel = model?.let { m ->
        ReasoningEffortLevels.allowed(m.supportsReasoning, m.modelId)?.let { levels ->
            m.reasoningEffort
                ?.takeIf { v -> v in levels }
                ?.let { v -> ReasoningEffortLevels.label(v) }
                ?: "默认"
        }
    }
    val (termuxText, termuxColor) = termuxLiveStatus(lights)
    val modelDisplay = when {
        config.models.isEmpty() -> "未配置模型"
        modelAlias.isEmpty() -> "未设置"
        else -> RuntimeBridge.modelAliasDisplay(
            RuntimeBridge.modelEntryOf(modelAlias, config),
            config,
        )
    }
    return ComposerInlineStatus(
        modelDisplay,
        model?.modelId ?: modelDisplay,
        effortLabel,
        live.connectedServers,
        live.agentTools,
        termuxText,
        termuxColor,
    )
}

/** 弹层 dismiss 后忽略胶囊重开的时间窗口（毫秒）：focusable=false 的
 *  弹层点外部 dismiss 走 ACTION_OUTSIDE（DOWN 时刻触发、不消费事件），
 *  同一手势的 UP 随后落进胶囊 clickable——无抑制则「关了又开」视觉
 *  收不起。窗口只需盖住一次触摸手势时长，250ms 足够宽。 */
private const val DropdownReopenSuppressMs = 250L

/**
 * 模型下拉控件（输入面板内，双控件行的左/主控件）：无静态标签的软底下拉框，SoftButton 同族
 * 语言——语义 primary 低透明度底 + 同色文字、10dp 圆角、34dp 最小触达
 * 高度；形态为「左当前值 + 右下拉箭头」的选择框。点击整块弹玻璃弹层
 * （[GlassDropdownMenu]，与面板同族磨砂语言；Haze 局部模糊聊天内容，
 * 贴底自动向上展开），选中项带 Check 图标（ui.md 基线）；切换走
 * onSelectModel（ChatScreen 接 RuntimeBridge.selectModel：set_model
 * 会话热切 + setCapability("main") 持久化为启动默认）→ model_changed
 * 回填 state.modelAlias，行内显示与实际生效恒同源。当前值经
 * [RuntimeBridge.modelEntryOf] 解析到条目再显示（levels 遗留兜底）——
 * 内部 entry 键不直出 UI，选中比对/回传仍用原始条目键。
 *
 * 无障碍：静态标签删除后，语义上移到控件级 contentDescription（「选择
 * 本次对话模型，当前 X」，展开态换「收起」措辞）；当前值 Text 清空自身
 * 文本语义，避免与控件描述重复播报裸模型名。
 *
 * modifier 由调用方注入宽度（双控件行里传 weight(1f) 占主宽度；effort
 * 下拉不显示时独占整行效果不变）。
 */
@Composable
private fun ModelPickerRow(
    modelAlias: String,
    config: ConfigUi,
    onSelectModel: (String) -> Unit,
    hazeState: dev.chrisbanes.haze.HazeState,
    sharedTransitionScope: SharedTransitionScope,
    animatedVisibilityScope: AnimatedVisibilityScope,
    modifier: Modifier = Modifier,
) {
    var expanded by remember { mutableStateOf(false) }
    // 同手势抑制时刻（单调时钟，不受系统对时跳变影响）：onDismissRequest
    // 记录，胶囊 click 在 [DropdownReopenSuppressMs] 窗口内忽略打开。
    // 四场景：①弹层开 + 点胶囊 → DOWN dismiss、UP 在窗口内被忽略 →
    // 收起；②弹层关 + 点胶囊 → 无近期 dismiss → 打开；③点弹层外其他
    // 区域 → 仅 dismiss → 收起；④点菜单项 → GlassDropdownMenu 在
    // onSelect 后内部调 onDismissRequest（菜单项 UP 消费在弹层内，本无
    // 同手势竞态，记时刻是选中后速点胶囊误重开的双保险）。
    var lastDismissElapsedMs by remember { mutableStateOf(0L) }
    val options = remember(config) { composerModelOptions(config) }
    // 当前值解析到条目（镜像 Rust resolve_model 首步：levels 优先、未命中
    // 原样直查 models）——与选项同键比对，显示同经 modelAliasDisplay。
    val currentEntry = RuntimeBridge.modelEntryOf(modelAlias, config)
    // 当前值三态合一：视觉文本与控件级无障碍描述共用同一字符串，
    // 不给两处各自 when 漂移的机会。
    val currentDisplay = when {
        options.isEmpty() -> "未配置模型"
        modelAlias.isEmpty() -> "未设置"
        else -> RuntimeBridge.modelAliasDisplay(currentEntry, config)
    }
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "panelModelArrow",
    )
    Box(modifier) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                // 软底选择框（SoftButton 同族公式）：primary 12% 透明底；
                // 未配置模型（无可选项）照 SoftButton 禁用态降到 5%。
                .heightIn(min = 34.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(
                    MaterialTheme.colorScheme.primary.copy(
                        alpha = if (options.isNotEmpty()) 0.12f else 0.05f,
                    ),
                )
                .clickable(enabled = options.isNotEmpty()) {
                    if (SystemClock.elapsedRealtime() - lastDismissElapsedMs > DropdownReopenSuppressMs) {
                        expanded = true
                    }
                }
                .semantics {
                    // 行内静态标签已删，控件语义在此补足；可见值文本的
                    // 语义被清空（见下），TalkBack 只播报这一句。
                    contentDescription =
                        if (expanded) "收起模型选择，当前 $currentDisplay"
                        else "选择本次对话模型，当前 $currentDisplay"
                }
                .padding(horizontal = 12.dp, vertical = 4.dp),
        ) {
            // sharedBounds 锚点：与输入框常驻状态行的模型名同 key——聚焦
            // 展开/收起时做「输入框 ↔ 卡片」bounds 缩放位移（320ms，FOOM
            // 曲线 FastOutSlowInEasing）；面板关闭时另一侧未组合，本修饰
            // 无匹配即原样渲染。
            val modelSharedBounds = with(sharedTransitionScope) {
                Modifier.sharedBounds(
                    rememberSharedContentState(key = "composer.model"),
                    animatedVisibilityScope = animatedVisibilityScope,
                    boundsTransform = { _, _ -> tween(320, easing = FastOutSlowInEasing) },
                )
            }
            Text(
                // 当前值 = 模型标识（modelAliasDisplay 输出）→ 内嵌 MonoFont
                // 子集；「未配置/未设置」等纯状态分支保持默认字体。
                text = if (options.isEmpty() || modelAlias.isEmpty()) {
                    AnnotatedString(currentDisplay)
                } else {
                    buildAnnotatedString {
                        withStyle(SpanStyle(fontFamily = MonoFont)) {
                            append(currentDisplay)
                        }
                    }
                },
                style = MaterialTheme.typography.labelLarge,
                color = if (options.isEmpty()) {
                    MaterialTheme.colorScheme.outline
                } else {
                    MaterialTheme.colorScheme.primary
                },
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = modelSharedBounds
                    .weight(1f)
                    // 控件级 contentDescription 已含当前值，清掉裸文本
                    // 语义避免 TalkBack 把模型名读两遍。
                    .clearAndSetSemantics { },
            )
            Spacer(Modifier.width(4.dp))
            Icon(
                Icons.Filled.KeyboardArrowDown,
                // 描述语义已上移控件级，箭头自身不再播报。
                contentDescription = null,
                tint = MaterialTheme.colorScheme.outline,
                // 同 SettingsScreen.CapabilityModelRow：绘制相位读取，避免逐帧重组。
                modifier = Modifier.graphicsLayer { rotationZ = arrowAngle },
            )
        }
        GlassDropdownMenu(
            expanded = expanded,
            onDismissRequest = {
                expanded = false
                lastDismissElapsedMs = SystemClock.elapsedRealtime()
            },
            hazeState = hazeState,
            items = options.map { opt ->
                GlassDropdownItem(value = opt.entry, label = opt.label, selected = opt.entry == currentEntry)
            },
            labelFontFamily = MonoFont,
            alignEnd = true,
            // 非聚焦 Popup：弹层不抢窗口焦点 → 输入框保焦、键盘不收
            // （面板不塌）。代价：BACK 不再先关弹层（作用于底层界面），
            // composer 场景可接受——点选/点外部均正常关闭。
            focusable = false,
            onSelect = { opt -> onSelectModel(opt.value) },
        )
    }
}

/**
 * Thinking Effort 下拉（双控件行右半，与模型下拉各占 50%）：软底同族
 * 视觉——primary
 * 12% 透明底 + 10dp 圆角 + 34dp 最小高度，左当前档位 + 右箭头（展开
 * 180° 旋转）。仅当前模型解析到条目且 [ReasoningEffortLevels.allowed]
 * 有支持档位时由调用方组合——不显示时不占位。选项 = 「默认」+ 支持档
 * 位（同 [ReasoningEffortLevels.label] 文案）；当前值 = 条目
 * reasoning_effort（null / 越界显示「默认」）。
 *
 * 选中语义 = 改模型条目配置：onSelectEffort 回传 (entry, effort)，由
 * ChatScreen 用 config 里原 ModelUi copy 改字段后 upsertModel 回写
 * （选了即落盘，config_changed 回刷面板回显）——与模型选择的「选了即
 * 默认」同语义层级；null = Auto（不传参，跟随 provider 默认）。
 *
 * 设置页会话域功能模型行的档位次行（SettingsScreen ThinkingEffortRow）
 * 同链路直改条目 reasoning_effort、措辞同源（label +「默认」）——功能行
 * 未绑定回退解析到会话主条目时即编辑同一字段；两处无独立状态源，
 * config_changed 回证后恒同值。
 */
@Composable
private fun EffortPickerRow(
    entry: String,
    levels: List<String>,
    current: String?,
    onSelectEffort: (String, String?) -> Unit,
    sharedTransitionScope: SharedTransitionScope,
    animatedVisibilityScope: AnimatedVisibilityScope,
    modifier: Modifier = Modifier,
    hazeState: dev.chrisbanes.haze.HazeState,
) {
    var expanded by remember { mutableStateOf(false) }
    // 同手势抑制：完整四场景语义与 GlassDropdownMenu 回调路径说明见
    // ModelPickerRow（本控件同款，onSelect 后弹层内部同样走
    // onDismissRequest，选中即记时刻）。
    var lastDismissElapsedMs by remember { mutableStateOf(0L) }
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "panelEffortArrow",
    )
    // 越界值（换模型后旧档位不在新支持列表）→ 按「默认」显示与回显。
    val currentEffort = current?.takeIf { it in levels }
    Box(modifier) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                // 宽度由外层 Box 承接（weight 在调用方的双控件行注入，
                // 对各占 50%）；fillMaxWidth 铺满 Box，值文本 weight(1f)
                // 吸收富余宽度，超长档位名省略。
                .fillMaxWidth()
                .heightIn(min = 34.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.12f))
                .clickable {
                    if (SystemClock.elapsedRealtime() - lastDismissElapsedMs > DropdownReopenSuppressMs) {
                        expanded = true
                    }
                }
                .semantics {
                    // 控件级语义（值文本自身语义被清空，见下），TalkBack
                    // 只播报这一句；展开态换「收起」措辞，与模型下拉一致。
                    val cur = currentEffort?.let { ReasoningEffortLevels.label(it) } ?: "默认"
                    contentDescription =
                        if (expanded) "收起思考档位选择，当前 $cur" else "思考档位，当前 $cur"
                }
                .padding(horizontal = 12.dp, vertical = 4.dp),
        ) {
            Text(
                text = currentEffort?.let { ReasoningEffortLevels.label(it) } ?: "默认",
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.primary,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                // sharedBounds 锚点同模型下拉（key composer.effort）：与
                // 输入框常驻状态行的 effort 档位做 bounds 位移过渡。
                modifier = with(sharedTransitionScope) {
                    Modifier.sharedBounds(
                        rememberSharedContentState(key = "composer.effort"),
                        animatedVisibilityScope = animatedVisibilityScope,
                        boundsTransform = { _, _ -> tween(320, easing = FastOutSlowInEasing) },
                    )
                }
                    .weight(1f)
                    .clearAndSetSemantics { },
            )
            Spacer(Modifier.width(4.dp))
            Icon(
                Icons.Filled.KeyboardArrowDown,
                // 描述语义已上移控件级，箭头自身不再播报。
                contentDescription = null,
                tint = MaterialTheme.colorScheme.outline,
                // 同模型下拉：绘制相位读取，避免逐帧重组。
                modifier = Modifier.graphicsLayer { rotationZ = arrowAngle },
            )
        }
        GlassDropdownMenu(
            expanded = expanded,
            onDismissRequest = {
                expanded = false
                lastDismissElapsedMs = SystemClock.elapsedRealtime()
            },
            hazeState = hazeState,
            items = listOf(
                // GlassDropdownItem.value 非空类型：空串表达「默认」，回传
                // 时 ifEmpty 归 null（与 EditModelDialog 选择器同约定）。
                GlassDropdownItem(value = "", label = "默认", selected = currentEffort == null),
            ) + levels.map { lvl ->
                GlassDropdownItem(
                    value = lvl,
                    label = ReasoningEffortLevels.label(lvl),
                    selected = lvl == currentEffort,
                )
            },
            // 档位名非模型 ID，不注入 MonoFont 子集。
            alignEnd = true,
            // 非聚焦 Popup 同模型下拉：键盘不收；BACK 取舍见 ModelPickerRow。
            focusable = false,
            onSelect = { opt -> onSelectEffort(entry, opt.value.ifEmpty { null }) },
        )
    }
}
