package dev.openslate.mobile.ui

import android.content.Context
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.outlined.Warning
import androidx.compose.material3.Card
import androidx.compose.material3.Checkbox
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import dev.openslate.mobile.R
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.TermuxDiagnostics
import dev.openslate.mobile.bridge.TermuxExec
import dev.openslate.mobile.bridge.ToolInfoUi
import dev.openslate.mobile.bridge.ToolParamUi
import dev.openslate.mobile.bridge.ToolVisibilityUi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/** 内嵌等宽字体（JetBrains Mono 子集；ASCII+常用标点，中文自动回退系统
 *  字体）。用于工具名、参数名/类型等代码类文本。 */
private val MonoFont = FontFamily(Font(R.font.jetbrains_mono_subset))

/** 工具分组固定展示顺序（类别按名前缀推导）。 */
private val TOOL_GROUP_ORDER = listOf("主机", "命令行", "MCP 工具", "内置")

/** 工具归类：`mobile.*` → 主机；`bash`/`termux_bash` → 命令行；
 *  MCP（模板正则命中，旧 `__` 启发式兜底）→ MCP 工具；其余 → 内置。 */
private fun toolGroupLabel(name: String): String = when {
    name.startsWith("mobile.") -> "主机"
    name == "bash" || name == "termux_bash" -> "命令行"
    // MCP 判定与卡片聚合同源（mcpServerSlugOf，模板正则提取）：三占位
    // 符模型的注册名不必含 `__`（如 `{prefix}.{server}.{tool}` 展开成
    // `termux.memory.create_entities`），contains("__") 旧启发式仅兜底。
    RuntimeBridge.mcpServerSlugOf(name, RuntimeBridge.MCP_SERVER_NAMES) != null ||
        name.contains("__") -> "MCP 工具"
    else -> "内置"
}

/** Termux 依赖工具的可用状态；检测中/正常不产生警示标识（避免闪烁）。 */
private sealed interface TermuxStatus {
    /** 检测进行中（含重入重置后的过渡态）：不显示任何标识。 */
    object Checking : TermuxStatus
    /** 通道 echo 往返通过：无需警示。 */
    object Ready : TermuxStatus
    /** Termux 未安装。 */
    object NotInstalled : TermuxStatus
    /** RUN_COMMAND 权限未授予。 */
    object NoPermission : TermuxStatus
    /** 通道探测失败；stopped = Termux 处于停止态（首装未打开/被强停）。 */
    data class ChannelDown(val summary: String, val stopped: Boolean) : TermuxStatus
}

/** 后端 CSV 是否启用了 termux（解析同 ExecBackendSection：逗号分隔 + trim）。 */
private fun termuxBackendEnabled(backendsCsv: String?): Boolean =
    backendsCsv?.split(',')?.map { it.trim() }?.contains("termux") == true

/**
 * 工具是否依赖 Termux（命名规则来自 Rust 侧注册：单选后端 → 工具名
 * `bash`；双选 → `bash`=native + `termux_bash`）。`termux_bash` 必走
 * Termux；`bash` 仅在 termux 后端单独启用（无 native 兜底）时才走
 * Termux——双后端时 bash 是系统 shell，不依赖 Termux。
 */
private fun dependsOnTermux(name: String, backendsCsv: String?): Boolean {
    if (!termuxBackendEnabled(backendsCsv)) return false
    if (name == "termux_bash") return true
    if (name == "bash") {
        val backends = backendsCsv?.split(',')?.map { it.trim() }.orEmpty()
        return "native" !in backends
    }
    return false
}

/**
 * Termux 可用性检测阶梯（权威检测，无副作用、不自动拉起）：
 * 未安装 → 无权限 → `echo ok` 通道往返。全程 IO 线程（PackageManager
 * 查询与 RUN_COMMAND 派发都不该压主线程）。
 */
private suspend fun detectTermuxStatus(context: Context): TermuxStatus =
    withContext(Dispatchers.IO) {
        when {
            !TermuxExec.isTermuxInstalled(context) -> TermuxStatus.NotInstalled
            !TermuxExec.hasPermission(context) -> TermuxStatus.NoPermission
            else -> when (val r = TermuxDiagnostics.ensureTermuxUp(context) { }) {
                is TermuxDiagnostics.ChannelResult.Ok -> TermuxStatus.Ready
                is TermuxDiagnostics.ChannelResult.NoResponse ->
                    TermuxStatus.ChannelDown(TermuxDiagnostics.describe(r), stopped = r.stopped)
                else -> TermuxStatus.ChannelDown(TermuxDiagnostics.describe(r), stopped = false)
            }
        }
    }

/** 不可用提示文案（定稿 copy，勿改）；Ready/检测中返回 null → 不警示。 */
private fun termuxUnavailableHint(status: TermuxStatus): String? = when (status) {
    TermuxStatus.Checking, TermuxStatus.Ready -> null
    TermuxStatus.NotInstalled ->
        "Termux 未安装：该工具需要 Termux 环境，暂无法使用。"
    TermuxStatus.NoPermission ->
        "RUN_COMMAND 权限未授予：该工具暂无法使用。可在「bash 工具执行后端」中申请权限。"
    is TermuxStatus.ChannelDown ->
        // stopped（未启动过/被强停）用「未启动」比「未就绪」更准确，也不带 describe 摘要。
        if (status.stopped) {
            "Termux 未启动：该工具暂无法使用。打开 Termux 后返回本页，会自动重新检测。"
        } else {
            "Termux 未就绪（${status.summary}）：该工具暂无法使用。打开 Termux 并返回本页后自动重新检测。"
        }
}

/**
 * 工具管理域：列出全部可用工具，逐个切换「对 agent 可见 / 隐藏」。
 *
 * - 数据每次进页从 FFI 直读（无本地缓存/SharedPreferences）；
 * - 开关即时生效：全量覆写可见性（globs 等无法识别条目原样带回）；
 * - 可见性语义：可见 = (tools 为空 || tools 含该名) && hidden_tools 不含该名；
 * - 点击行展开详情：工具名字号放大，描述原位展开全显，参数区淡入；
 * - MCP 工具组按所属 server（slug）聚合为卡片：卡片总开关 = 批量可见性
 *   （单次全量覆写 FFI），展开后逐工具复用 ToolRow；显示名经 prefs 的
 *   server 清单反查，server 已删（工具仍在注册表）回退 slug +「（未配置）」；
 * - Termux 依赖工具（termux_bash / 单后端 bash）：进页与 ON_RESUME 时
 *   检测 Termux 可用性（后端含 termux 才检测），异常时名字旁警示图标
 *   + 展开详情提示，用户打开 Termux 返回后自动重检。
 */
@Composable
fun ToolManagementSection() {
    val scope = rememberCoroutineScope()
    // null = 加载中；非 null 空表 = 无工具。
    var tools by remember { mutableStateOf<List<ToolInfoUi>?>(null) }
    var visibility by remember { mutableStateOf<ToolVisibilityUi?>(null) }
    var error by remember { mutableStateOf<String?>(null) }
    // 当前展开详情的工具名集合（各工具独立展开/收起）。
    var expandedTools by remember { mutableStateOf<Set<String>>(emptySet()) }
    // MCP 聚合卡片的展开集合（键 = server slug，与工具名互不相干）。
    var expandedGroups by remember { mutableStateOf<Set<String>>(emptySet()) }
    // slug → MCP 显示名反查表；null = 仍在读 prefs（期间卡片不标
    // 「未配置」，避免加载间隙闪标注）。
    var mcpServerNames by remember { mutableStateOf<Map<String, String>?>(null) }

    // ── Termux 依赖工具的可用性检测 ──
    // 后端 CSV（null = 仍在读取）；检测状态（null = 不涉及 Termux 或检测
    // 进行中——两种情况都不显示标识，避免加载/重检间隙闪烁）。
    val context = LocalContext.current
    var execBackendsCsv by remember { mutableStateOf<String?>(null) }
    var termuxStatus by remember { mutableStateOf<TermuxStatus?>(null) }
    var termuxProbe by remember { mutableStateOf<Job?>(null) }

    /**
     * 检测入口：读后端 CSV，仅当含 termux 才走检测阶梯。每次调用先取消
     * 在途检测并重置为「检测中」（ON_RESUME 重检视为全新一轮，旧结果作废）。
     */
    fun refreshTermuxReadiness() {
        termuxProbe?.cancel()
        termuxProbe = scope.launch {
            val csv = withContext(Dispatchers.IO) {
                runCatching { RuntimeBridge.execBackends() }.getOrNull()
            }
            execBackendsCsv = csv
            if (!termuxBackendEnabled(csv)) {
                termuxStatus = null
                return@launch
            }
            termuxStatus = TermuxStatus.Checking
            termuxStatus = detectTermuxStatus(context)
        }
    }

    // 首次进入异步取数：FFI 读离主线程（照抄 ExecBackendSection 的 IO 范式）。
    LaunchedEffect(Unit) {
        val loaded = withContext(Dispatchers.IO) {
            runCatching {
                RuntimeBridge.listTools() to RuntimeBridge.getAgentToolVisibility()
            }
        }
        loaded.fold(
            onSuccess = { (t, v) ->
                if (t == null || v == null) {
                    error = "运行时未就绪，无法读取工具清单"
                    tools = emptyList()
                    visibility = ToolVisibilityUi()
                } else {
                    tools = t
                    visibility = v
                }
            },
            onFailure = {
                error = "加载工具清单失败：${it.message ?: it.javaClass.simpleName}"
            },
        )
        // Termux 检测与工具清单加载相互独立：清单失败不影响可用性检测。
        refreshTermuxReadiness()
    }

    // MCP 显示名反查（slug → name）：与工具清单加载并行、互不影响；
    // 失败/为空只影响卡片标题回退显示 slug，不阻塞主数据。
    LaunchedEffect(Unit) {
        mcpServerNames = withContext(Dispatchers.IO) {
            runCatching {
                McpHostManager.serversSlugMap(McpHostManager.loadServers(context))
                    .associate { it.second to it.first.name }
            }.getOrDefault(emptyMap())
        }
    }

    // 用户去打开 Termux 后返回本页：ON_RESUME 自动重检（照抄
    // ExecBackendSection 的生命周期范式）。观察者挂上时生命周期已处于
    // RESUMED，会先收到一次合成的 ON_RESUME；用 resumedOnce 跳过它，
    // 避免与进页首检重复探测。
    var resumedOnce by remember { mutableStateOf(false) }
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner) {
        val obs = LifecycleEventObserver { _, event ->
            if (event == Lifecycle.Event.ON_RESUME) {
                if (resumedOnce) refreshTermuxReadiness()
                resumedOnce = true
            }
        }
        lifecycleOwner.lifecycle.addObserver(obs)
        onDispose { lifecycleOwner.lifecycle.removeObserver(obs) }
    }

    fun isVisible(name: String): Boolean {
        val v = visibility ?: return false
        return (v.tools.isEmpty() || name in v.tools) && name !in v.hiddenTools
    }

    /**
     * 全量覆写的公共尾部：FFI 写包 IO 协程，成功后回主线程更新状态
     * （现有范式）；globs 等无法识别条目由调用方构造 next 时原样保留，
     * 写入即热生效。
     */
    fun applyVisibility(next: ToolVisibilityUi) {
        error = null
        scope.launch(Dispatchers.IO) {
            val ok = RuntimeBridge.setAgentToolVisibility(next.tools, next.hiddenTools)
            withContext(Dispatchers.Main) {
                if (ok) {
                    visibility = next
                } else {
                    error = "切换未生效（运行时未就绪），请重试"
                }
            }
        }
    }

    /**
     * MCP 组总开关（批量可见性）：开 = 全部成员移出 hidden_tools 且补齐
     * tools 白名单（非空时）；关 = 全部加入 hidden_tools。一次构造 next
     * 后**单次**全量覆写——绝不逐名循环调 FFI；globs 等条目原样保留。
     */
    fun setGroupVisible(names: List<String>, on: Boolean) {
        val v = visibility ?: return
        val newTools = v.tools.toMutableList()
        val newHidden = v.hiddenTools.toMutableList()
        if (on) {
            for (name in names) {
                newHidden.remove(name)
                if (newTools.isNotEmpty() && name !in newTools) newTools.add(name)
            }
        } else {
            for (name in names) {
                if (name !in newHidden) newHidden.add(name)
            }
        }
        applyVisibility(ToolVisibilityUi(newTools.toList(), newHidden.toList()))
    }

    /**
     * 单名开→关：加入 hidden_tools；关→开：移出 hidden_tools，且 tools
     * 白名单非空而缺该名时补进去。批量版 [setGroupVisible] 的单元素特例
     * （语义与逐名循环完全一致，覆写同样是一次）。
     */
    fun setVisible(name: String, on: Boolean) = setGroupVisible(listOf(name), on)

    Section(title = "工具管理") {
        Text(
            "控制每个工具对 agent 的可见性：关闭后 agent 即看不到该工具，" +
                "立即生效，无需重启会话。点击工具行可查看详情。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline,
        )
        error?.let { msg ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                Icon(
                    imageVector = Icons.Outlined.Warning,
                    contentDescription = "警告",
                    tint = MaterialTheme.colorScheme.error,
                    modifier = Modifier.height(14.dp).width(14.dp),
                )
                Text(
                    "  $msg",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.error,
                )
            }
        }
        val loadedTools = tools
        when {
            // 预取进行中：占位一行，避免加载间隙闪空态。
            loadedTools == null -> Text(
                "加载中…",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
            loadedTools.isEmpty() -> Text(
                "暂无可用工具：运行时未注册任何工具。",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
            else -> {
                val visibleCount = loadedTools.count { isVisible(it.name) }
                Text(
                    "共 ${loadedTools.size} 个工具，$visibleCount 个对 agent 可见",
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.outline,
                )
                for (group in TOOL_GROUP_ORDER) {
                    val members = loadedTools.filter { toolGroupLabel(it.name) == group }
                    if (members.isEmpty()) continue
                    Text(
                        group,
                        style = MaterialTheme.typography.titleSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                    if (group == "MCP 工具") {
                        // 按 server slug 聚合（保持注册表出现顺序）；键 =
                        // 剥 core 前缀后的 `__` 前段，与 SettingsScreen 的
                        // 注册表计数同源（mcpServerSlugOf）。
                        val bySlug = linkedMapOf<String, MutableList<ToolInfoUi>>()
                        for (t in members) {
                            val slug = RuntimeBridge.mcpServerSlugOf(
                                t.name, RuntimeBridge.MCP_SERVER_NAMES,
                            ) ?: continue
                            bySlug.getOrPut(slug) { mutableListOf() }.add(t)
                        }
                        for ((slug, groupTools) in bySlug) {
                            val names = mcpServerNames
                            McpToolGroupCard(
                                slug = slug,
                                displayName = names?.get(slug) ?: slug,
                                // 反查表未就绪时不标「未配置」（防加载间隙闪标注）。
                                configured = names != null && slug in names,
                                tools = groupTools,
                                expanded = slug in expandedGroups,
                                onToggleExpand = {
                                    expandedGroups =
                                        if (slug in expandedGroups) expandedGroups - slug
                                        else expandedGroups + slug
                                },
                                isVisible = { isVisible(it) },
                                onSetGroupVisible = { on ->
                                    setGroupVisible(groupTools.map { it.name }, on)
                                },
                                expandedTools = expandedTools,
                                onToggleToolExpand = { name ->
                                    expandedTools =
                                        if (name in expandedTools) expandedTools - name
                                        else expandedTools + name
                                },
                                onToolVisibleChange = { name, on -> setVisible(name, on) },
                                termuxStatusFor = { name ->
                                    if (dependsOnTermux(name, execBackendsCsv)) termuxStatus else null
                                },
                            )
                        }
                    } else {
                        for (t in members) {
                            ToolRow(
                                tool = t,
                                visible = isVisible(t.name),
                                expanded = t.name in expandedTools,
                                // 仅 Termux 依赖工具传状态；其余工具 null → 永不警示。
                                termuxStatus =
                                    if (dependsOnTermux(t.name, execBackendsCsv)) termuxStatus
                                    else null,
                                onToggleExpand = {
                                    expandedTools =
                                        if (t.name in expandedTools) expandedTools - t.name
                                        else expandedTools + t.name
                                },
                                onVisibleChange = { setVisible(t.name, it) },
                            )
                        }
                    }
                }
            }
        }
    }
}

/**
 * MCP server 聚合卡片：头部 = 显示名（+小字 slug/未配置标注）+ 工具数
 * 统计 + 总开关（批量可见性）+ 展开箭头；展开后逐工具复用 [ToolRow]
 * （各工具自己的可见性开关与详情展开互不干扰，外层 animateContentSize
 * 平滑跟随）。
 *
 * 动效对齐 ToolRow 范式：箭头随展开态旋转；头部两态是同一组合不重建；
 * 只有展开区参与纵向展开 + 200ms 淡入淡出。
 *
 * 总开关语义：全部可见 = 勾选；混合/全关 = 不勾选（混合态语义由统计行
 * 「N 个工具 · M 个可见」文字承载——工程无 TriState 先例，优先简单方案）。
 * 混合态点击 = 全开，全开态点击 = 全关。Checkbox 自带消费点击，不冒泡
 * 触发卡片展开（ToolRow 同款）。
 */
@Composable
private fun McpToolGroupCard(
    slug: String,
    displayName: String,
    configured: Boolean,
    tools: List<ToolInfoUi>,
    expanded: Boolean,
    onToggleExpand: () -> Unit,
    isVisible: (String) -> Boolean,
    onSetGroupVisible: (Boolean) -> Unit,
    expandedTools: Set<String>,
    onToggleToolExpand: (String) -> Unit,
    onToolVisibleChange: (String, Boolean) -> Unit,
    termuxStatusFor: (String) -> TermuxStatus?,
) {
    // 箭头旋转与 ToolRow 同节奏（默认 spring）。
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "mcpGroupArrow",
    )
    val visibleCount = tools.count { isVisible(it.name) }
    Card(Modifier.fillMaxWidth()) {
        Column(
            Modifier
                .fillMaxWidth()
                .clickable(onClick = onToggleExpand)
                .animateContentSize()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            // 头部行：两态同一组合，不重建、不交叉淡化。
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Column(Modifier.weight(1f)) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        Text(displayName, style = MaterialTheme.typography.titleSmall)
                        // 与显示名不同时展示代号（路由身份；auto 派生时常
                        // 相同则省去，避免「memory（memory）」式重复）。
                        if (slug != displayName) {
                            Text(
                                "（$slug）",
                                style = MaterialTheme.typography.labelSmall,
                                fontFamily = MonoFont,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                        // server 已删但注册表工具还在（清单未清）→ 标注。
                        if (!configured) {
                            Text(
                                "（未配置）",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                    }
                    Text(
                        "${tools.size} 个工具 · $visibleCount 个可见",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
                Checkbox(
                    checked = visibleCount == tools.size,
                    onCheckedChange = onSetGroupVisible,
                )
                Icon(
                    imageVector = Icons.Filled.KeyboardArrowDown,
                    contentDescription = if (expanded) "收起工具列表" else "展开工具列表",
                    tint = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.rotate(arrowAngle),
                )
            }
            // 展开区：组内逐工具复用 ToolRow；仅此块参与淡入淡出与纵向展开。
            AnimatedVisibility(
                visible = expanded,
                enter = expandVertically() + fadeIn(tween(200)),
                exit = shrinkVertically() + fadeOut(tween(200)),
            ) {
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    HorizontalDivider()
                    // 灰字说明（置于列表上方，先建立预期再看短名）：此处显示的
                    // 是短名，agent 看到的完整名会自动加回前缀（模板 {tool}
                    // 之前的部分按当前前缀/slug 展开；模板无前缀段 → 空串，
                    // 整行不渲染）。旧 `__` 边界启发式仅作正则提取的兜底。
                    val agentPrefix = RuntimeBridge.mcpAgentPrefixNote(slug)
                    if (agentPrefix.isNotEmpty()) {
                        Text(
                            buildAnnotatedString {
                                append("Agent 调用名会自动加上前缀 ")
                                withStyle(SpanStyle(fontFamily = MonoFont)) { append(agentPrefix) }
                            },
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    // 组内工具行之间加分隔符：相邻卡片同色视觉易粘连，
                    // 分隔线让每行边界清晰（首行前不重复，顶部已有整体分隔线）。
                    for ((index, t) in tools.withIndex()) {
                        if (index > 0) HorizontalDivider()
                        ToolRow(
                            tool = t,
                            visible = isVisible(t.name),
                            expanded = t.name in expandedTools,
                            // MCP 工具名不依赖 Termux（dependsOnTermux 仅
                            // bash/termux_bash 为真，此 lambda 对组内恒返
                            // 回 null）；仍走同一表达式以防规则将来变化。
                            termuxStatus = termuxStatusFor(t.name),
                            onToggleExpand = { onToggleToolExpand(t.name) },
                            onVisibleChange = { onToolVisibleChange(t.name, it) },
                            // 显示名：模板正则提取 {tool} 段（{prefix}/{server}
                            // 段一并剥掉）；正则不匹配（异常形态）兜底旧 `__`
                            // 启发式。可见性等回调仍用真实 tool.name（键 = 精确名）。
                            displayName = RuntimeBridge.mcpToolBareName(t.name)
                                ?: t.name.substringAfter("__"),
                        )
                    }
                }
            }
        }
    }
}

/**
 * 单个工具行：头部行 = 名称（等宽，展开时字号放大）+ 可见性 Checkbox +
 * 展开指示箭头；描述按态分流——收起态在名称下方单行截断（与控件同行
 * 布局），展开态移至独立全宽行全显；点击行展开/收起参数详情。依赖 Termux
 * 的工具在状态异常时：名字旁常驻警示图标（收起态可见），展开后详情区顶部
 * 给完整提示。Checkbox 不受影响（独立偏好）。
 *
 * 动效原则（真机反馈调整）：头部行（名字/Checkbox/箭头）在收起/展开两态
 * 是同一组合——不重建、不交叉淡化；描述文本只存在一份、位置永不移动
 * （仅切 maxLines，高度变化由外层 animateContentSize 平滑）；只有参数
 * 详情区参与 AnimatedVisibility 的纵向展开 + 200ms 淡入淡出。警示标识
 * 的出现/消失是低频事件（仅重检翻面时），直接增删不做动画。
 */
@Composable
private fun ToolRow(
    tool: ToolInfoUi,
    visible: Boolean,
    expanded: Boolean,
    termuxStatus: TermuxStatus?,
    onToggleExpand: () -> Unit,
    onVisibleChange: (Boolean) -> Unit,
    // 显示名可与真实名不同（MCP 聚合卡内剥前缀显示）；回调一律用真实名。
    displayName: String = tool.name,
) {
    // 箭头随展开态旋转 180°；工具名字号 14sp ↔ 17sp 同节奏缩放。
    // 两者均用默认 spring（damping 1.0，无弹跳），节奏天然一致。
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "toolRowArrow",
    )
    val nameFontSize by animateFloatAsState(
        targetValue = if (expanded) 17f else 14f,
        label = "toolNameFontSize",
    )
    // 行高按 titleSmall（14sp/20sp）等比缩放，放大后单行不被截断。
    val nameLineHeight = nameFontSize * 20f / 14f
    // 非 null ⇔ 名字旁警示图标与详情区提示行同时存在（同源判定，
    // 检测中/正常均为 null → 两处都不出现）。
    val unavailableHint = termuxStatus?.let { termuxUnavailableHint(it) }
    Card(Modifier.fillMaxWidth()) {
        Column(
            Modifier
                .fillMaxWidth()
                .clickable(onClick = onToggleExpand)
                .animateContentSize()
                .padding(12.dp),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            // 头部行：名称 + 右侧控件（Checkbox/箭头）；描述按态分流——
            // 收起态留在名称下方（列内、单行截断）保持行紧凑；展开态移到
            // 头部行下方的独立全宽行（见下）。
            Row(
                verticalAlignment = Alignment.CenterVertically,
                horizontalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                Column(Modifier.weight(1f)) {
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                    ) {
                        Text(
                            displayName,
                            style = MaterialTheme.typography.titleSmall,
                            fontFamily = MonoFont,
                            fontSize = nameFontSize.sp,
                            lineHeight = nameLineHeight.sp,
                        )
                        // Termux 不可用警示：收起态也常驻（一眼定位不可用
                        // 工具），图标规格与页内错误横幅一致。
                        if (unavailableHint != null) {
                            Icon(
                                imageVector = Icons.Outlined.Warning,
                                contentDescription = "Termux 不可用",
                                tint = MaterialTheme.colorScheme.error,
                                modifier = Modifier.height(14.dp).width(14.dp),
                            )
                        }
                    }
                    // 收起态描述：名称下方、单行截断，与开关/箭头同行布局。
                    if (!expanded && tool.description.isNotEmpty()) {
                        Text(
                            tool.description,
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                            maxLines = 1,
                            overflow = TextOverflow.Ellipsis,
                        )
                    }
                }
                // Checkbox 自带 toggleable 消费点击，不冒泡触发行级展开；
                // 比 Switch 紧凑，与行内文字排版更协调。
                Checkbox(checked = visible, onCheckedChange = onVisibleChange)
                Icon(
                    imageVector = Icons.Filled.KeyboardArrowDown,
                    contentDescription = if (expanded) "收起详情" else "展开详情",
                    tint = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.rotate(arrowAngle),
                )
            }
            // 展开态描述：移出头部行，独立全宽行多行全显——不与 Checkbox/
            // 箭头挤占横向空间；两态位置有意不同（用户拍板），高度过渡由
            // 外层 animateContentSize 平滑。
            if (expanded && tool.description.isNotEmpty()) {
                Text(
                    tool.description,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
            // 参数详情区：仅此块参与淡入淡出与纵向展开/收起，行头部不动。
            AnimatedVisibility(
                visible = expanded,
                enter = expandVertically() + fadeIn(tween(200)),
                exit = shrinkVertically() + fadeOut(tween(200)),
            ) {
                Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
                    // 不可用提示行：图标 + 完整指引文案，随详情一起淡入淡出。
                    unavailableHint?.let { hint ->
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Icon(
                                imageVector = Icons.Outlined.Warning,
                                contentDescription = null,
                                tint = MaterialTheme.colorScheme.error,
                                modifier = Modifier.height(14.dp).width(14.dp),
                            )
                            Text(
                                "  $hint",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.error,
                            )
                        }
                    }
                    ToolDetail(tool)
                }
            }
        }
    }
}

/** 展开后的详情区：分隔线 + 参数标题 + 参数列表（名/类型等宽、必填徽标）。 */
@Composable
private fun ToolDetail(tool: ToolInfoUi) {
    Column(verticalArrangement = Arrangement.spacedBy(6.dp)) {
        HorizontalDivider()
        Text(
            if (tool.params.isEmpty()) "参数" else "参数（${tool.params.size} 个）",
            style = MaterialTheme.typography.labelMedium,
            color = MaterialTheme.colorScheme.primary,
        )
        if (tool.params.isEmpty()) {
            Text(
                "无参数：调用时不需要任何输入。",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        } else {
            for (p in tool.params) {
                ToolParamRow(p)
            }
        }
    }
}

/** 单个参数行：参数名 + 类型（均等宽）+ 必填徽标，下一行参数描述。
 *  嵌套 schema 展开的子字段（depth > 0）按层级缩进，一眼可辨归属。 */
@Composable
private fun ToolParamRow(p: ToolParamUi) {
    Column(Modifier.padding(start = (p.depth * 12).dp)) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            Text(
                p.name,
                style = MaterialTheme.typography.labelMedium,
                fontFamily = MonoFont,
            )
            if (p.type.isNotEmpty()) {
                Text(
                    p.type,
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = MonoFont,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
            if (p.required) {
                // 必填徽标：primary 12% 底 + primary 文字（与 SoftButton 同款配色惯例）。
                Text(
                    "必填",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier
                        .clip(RoundedCornerShape(4.dp))
                        .background(MaterialTheme.colorScheme.primary.copy(alpha = 0.12f))
                        .padding(horizontal = 6.dp, vertical = 1.dp),
                )
            }
        }
        if (p.description.isNotEmpty()) {
            Text(
                p.description,
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
    }
}

/** 标题区块（与 SettingsScreen 的 Section 同款视觉；独立文件内私有实现）。 */
@Composable
private fun Section(
    title: String,
    content: @Composable androidx.compose.foundation.layout.ColumnScope.() -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(
            title,
            style = MaterialTheme.typography.titleMedium,
            modifier = Modifier.fillMaxWidth(),
        )
        content()
    }
}
