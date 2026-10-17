package dev.openslate.mobile.ui

import android.content.Context
import android.os.Build
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.outlined.Build
import androidx.compose.material.icons.outlined.Hub
import androidx.compose.material.icons.outlined.Memory
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.layout.LayoutCoordinates
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.ConfigUi
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/**
 * 输入框聚焦面板：点击聚焦输入框（进入编辑状态）时，从 composer 上方
 * 展开的一条状态带 + 「本次对话模型」选择器。
 *
 * 交互形态（与 IME 共存）：
 * - 面板挂在 ChatScreen 底部悬浮列（TermuxStallBanner 同位、同语言：
 *   GlassPanel + 细边框 + 轻阴影，expandVertically 自底部展开）。该列
 *   整体在 imePadding 之上，键盘弹起时列底随 IME 上移——面板在 composer
 *   上方向上生长，composer 相对键盘的位置不变，不存在被顶飞的可能，
 *   也不与 IME 动画争抢布局（同一布局子树一起移动）。
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
 * - MCP 服务器数：运行时注册表（`mcpRegistryToolsByPrefix`，内部先
 *   refreshMcpNaming 再 listTools 按模板正则分 slug 计数）——只统计
 *   真正把工具注册进本次会话注册表的 server，连接失败的 server 不会
 *   出现；配置层 loadServers 只反映"配了什么"，不反映"加载了什么"。
 * - 可用工具数：`listTools()` + `getAgentToolVisibility()`，可见性
 *   判定与工具管理页逐字同式：可见 = (tools 空 || tools 含该名) &&
 *   hidden_tools 不含该名。
 * - 模型选择：选项 = models 条目（levels 代号不再出现在选项里；capabilities
 *   也只直绑条目，见设置页会话页）。当前值经 [RuntimeBridge.modelEntryOf]
 *   解析到条目（levels 遗留兜底）再显示；显示走 state.modelAlias /
 *   model_changed 事件回填——与顶栏副标题、发消息时实际生效的是同一状态
 *   源，面板选了什么下次请求就用什么，不存在分裂。切换经 onSelectModel
 *   （ChatScreen 接 [RuntimeBridge.selectModel]）双写：set_model 会话级热
 *   切换 + setCapability("main") 持久化为启动默认——「选了即默认」的语义
 *   闭环，设置页会话页因此不再有主对话绑定行。
 */

/** 面板数据快照（聚焦瞬间一次性 FFI 读取的投影）。 */
internal data class ComposerPanelData(
    /** 注册表里有工具注册进来的 MCP server 数（真实加载口径，0 = 无）。 */
    val mcpServerCount: Int,
    /** server 展示名串联（配置清单反查 slug；未命中回退 slug 本身）。 */
    val mcpServerNames: String?,
    /** 注册表工具总数。 */
    val toolsTotal: Int,
    /** 对 agent 可见的工具数（与工具管理页同口径）。 */
    val toolsVisible: Int,
)

/**
 * 面板数据加载（挂起，全程 IO 线程；仅聚焦瞬间调用）。
 * null = 运行时未就绪（listTools 拿不到注册表），面板据此显示降级文案。
 */
internal suspend fun loadComposerPanelData(context: Context): ComposerPanelData? =
    withContext(Dispatchers.IO) {
        val tools = RuntimeBridge.listTools() ?: return@withContext null
        val visibility = RuntimeBridge.getAgentToolVisibility()

        // 可见性判定与 ToolManagementSection.isVisible 逐字同式（同口径）。
        fun isVisible(name: String): Boolean {
            val v = visibility ?: return false
            return (v.tools.isEmpty() || name in v.tools) && name !in v.hiddenTools
        }

        // MCP 分组计数：内部自带 refreshMcpNaming（服务名+模板缓存刷新），
        // 用户没开过设置页时缓存也能对齐当前配置。
        val mcpBySlug = RuntimeBridge.mcpRegistryToolsByPrefix(RuntimeBridge.MCP_SERVER_NAMES)
        // slug → 显示名反查（prefs 读，轻量）；失败只影响名称展示不阻塞计数。
        val configuredNames = runCatching {
            McpHostManager.serversSlugMap(McpHostManager.loadServers(context))
                .associate { it.second to it.first.name }
        }.getOrDefault(emptyMap())
        ComposerPanelData(
            mcpServerCount = mcpBySlug?.size ?: 0,
            mcpServerNames = mcpBySlug?.keys
                ?.joinToString(" · ") { slug -> configuredNames[slug] ?: slug },
            toolsTotal = tools.size,
            toolsVisible = tools.count { isVisible(it.name) },
        )
    }

/**
 * 面板本体。visible 由 ChatScreen 的输入框焦点状态驱动；data 为 null
 * 且 loading = 首次聚焦读取中，data 为 null 且 !loading = 运行时未就绪。
 * hazeState 透传给「本次对话模型」弹层（Haze 局部模糊内容源）。
 */
@Composable
internal fun ComposerFocusPanel(
    visible: Boolean,
    data: ComposerPanelData?,
    loading: Boolean,
    modelAlias: String,
    config: ConfigUi,
    onSelectModel: (String) -> Unit,
    glass: Glass,
    rootCoords: () -> LayoutCoordinates?,
    hazeState: dev.chrisbanes.haze.HazeState,
) {
    AnimatedVisibility(
        visible = visible,
        enter = fadeIn() + expandVertically(expandFrom = Alignment.Bottom),
        exit = fadeOut() + shrinkVertically(shrinkTowards = Alignment.Bottom),
    ) {
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
                        // 首帧加载态 → 数据态的高度过渡平滑化。
                        .animateContentSize(),
                ) {
                    when {
                        loading && data == null -> {
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                CircularProgressIndicator(
                                    strokeWidth = 1.5.dp,
                                    modifier = Modifier.size(12.dp),
                                )
                                Spacer(Modifier.width(6.dp))
                                Text(
                                    "读取 MCP 与工具状态…",
                                    style = MaterialTheme.typography.labelSmall,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                            }
                        }
                        data == null -> {
                            Text(
                                "运行时未就绪，无法读取 MCP / 工具状态",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                        else -> {
                            // ── 状态行：MCP server 数 + 可见工具数 ──
                            Row(verticalAlignment = Alignment.CenterVertically) {
                                Icon(
                                    Icons.Outlined.Hub,
                                    contentDescription = null,
                                    modifier = Modifier.size(14.dp),
                                    tint = if (data.mcpServerCount > 0) {
                                        MaterialTheme.colorScheme.secondary
                                    } else {
                                        MaterialTheme.colorScheme.outline
                                    },
                                )
                                Spacer(Modifier.width(4.dp))
                                Text(
                                    "MCP ${data.mcpServerCount}",
                                    style = MaterialTheme.typography.labelMedium,
                                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                                )
                                Spacer(Modifier.width(14.dp))
                                Icon(
                                    Icons.Outlined.Build,
                                    contentDescription = null,
                                    modifier = Modifier.size(13.dp),
                                    tint = if (data.toolsVisible > 0) {
                                        MaterialTheme.colorScheme.secondary
                                    } else {
                                        MaterialTheme.colorScheme.error
                                    },
                                )
                                Spacer(Modifier.width(4.dp))
                                Text(
                                    "可用工具 ${data.toolsVisible}",
                                    style = MaterialTheme.typography.labelMedium,
                                    color = if (data.toolsVisible > 0) {
                                        MaterialTheme.colorScheme.onSurfaceVariant
                                    } else {
                                        MaterialTheme.colorScheme.error
                                    },
                                )
                                // 有隐藏工具时右上角补总数（与工具管理页统计呼应）。
                                if (data.toolsTotal > data.toolsVisible) {
                                    Spacer(Modifier.weight(1f))
                                    Text(
                                        "共 ${data.toolsTotal} 个",
                                        style = MaterialTheme.typography.labelSmall,
                                        color = MaterialTheme.colorScheme.outline,
                                    )
                                }
                            }
                            // 第二行：加载到的 server 名单 / 零加载提示。
                            Text(
                                if (data.mcpServerCount > 0) {
                                    data.mcpServerNames.orEmpty()
                                } else {
                                    "未加载 MCP 服务器（可在设置中配置）"
                                },
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                                maxLines = 1,
                                overflow = TextOverflow.Ellipsis,
                                modifier = Modifier.padding(top = 2.dp),
                            )
                            HorizontalDivider(
                                thickness = 0.5.dp,
                                color = MaterialTheme.colorScheme.outline.copy(alpha = 0.3f),
                                modifier = Modifier.padding(vertical = 7.dp),
                            )
                            ModelPickerRow(modelAlias, config, onSelectModel, hazeState)
                        }
                    }
                }
            }
        }
    }
}

/** 「本次对话模型」选项：models 条目键（选中/回传语义）+ 显示主文字
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

/**
 * 「本次对话模型」行：整行点击弹玻璃弹层（[GlassDropdownMenu]，与面板同
 * 族磨砂语言；Haze 局部模糊聊天内容，贴底自动向上展开），选中项带 Check
 * 图标（ui.md 基线）；切换走 onSelectModel（ChatScreen 接
 * RuntimeBridge.selectModel：set_model 会话热切 + setCapability("main")
 * 持久化为启动默认）→ model_changed 回填 state.modelAlias，行内显示与
 * 实际生效恒同源。当前值经 [RuntimeBridge.modelEntryOf] 解析到条目再显示
 * （levels 遗留兜底）——内部 entry 键不直出 UI，选中比对/回传仍用原始
 * 条目键。
 */
@Composable
private fun ModelPickerRow(
    modelAlias: String,
    config: ConfigUi,
    onSelectModel: (String) -> Unit,
    hazeState: dev.chrisbanes.haze.HazeState,
) {
    var expanded by remember { mutableStateOf(false) }
    val options = remember(config) { composerModelOptions(config) }
    // 当前值解析到条目（镜像 Rust resolve_model 首步：levels 优先、未命中
    // 原样直查 models）——与选项同键比对，显示同经 modelAliasDisplay。
    val currentEntry = RuntimeBridge.modelEntryOf(modelAlias, config)
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "panelModelArrow",
    )
    Box {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(8.dp))
                .clickable(enabled = options.isNotEmpty()) { expanded = true }
                .padding(horizontal = 2.dp, vertical = 4.dp),
        ) {
            Icon(
                Icons.Outlined.Memory,
                contentDescription = null,
                modifier = Modifier.size(14.dp),
                tint = MaterialTheme.colorScheme.primary,
            )
            Spacer(Modifier.width(6.dp))
            Text(
                "本次对话模型",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
            Spacer(Modifier.weight(1f))
            Text(
                when {
                    options.isEmpty() -> "未配置模型"
                    modelAlias.isEmpty() -> "未设置"
                    else -> RuntimeBridge.modelAliasDisplay(currentEntry, config)
                },
                style = MaterialTheme.typography.labelLarge,
                color = if (options.isEmpty()) {
                    MaterialTheme.colorScheme.outline
                } else {
                    MaterialTheme.colorScheme.primary
                },
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(2f, fill = false),
            )
            Spacer(Modifier.width(4.dp))
            Icon(
                Icons.Filled.KeyboardArrowDown,
                contentDescription = if (expanded) "收起模型选择" else "选择本次对话模型",
                tint = MaterialTheme.colorScheme.outline,
                modifier = Modifier.rotate(arrowAngle),
            )
        }
        GlassDropdownMenu(
            expanded = expanded,
            onDismissRequest = { expanded = false },
            hazeState = hazeState,
            items = options.map { opt ->
                GlassDropdownItem(value = opt.entry, label = opt.label, selected = opt.entry == currentEntry)
            },
            onSelect = { opt -> onSelectModel(opt.value) },
        )
    }
}
