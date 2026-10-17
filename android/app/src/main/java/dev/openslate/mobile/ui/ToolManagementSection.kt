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
import androidx.compose.material3.OutlinedTextField
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
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalLifecycleOwner
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.TextFieldValue
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
import org.json.JSONObject

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
 * Termux 依赖工具的可用性检测阶梯（权威检测，无副作用、不自动拉起）：
 * **MCP host 健康（探活通过）→ 直接可用**（bash 经 MCP over HTTP 执行，
 * 与 RUN_COMMAND 无关）；host 不在线时才走 RUN_COMMAND 阶梯：未安装 →
 * 无权限 → `echo ok` 通道往返（通过 = RUN_COMMAND 兜底可用）。全程
 * IO 线程（PackageManager 查询、HTTP 探活与 RUN_COMMAND 派发都不该
 * 压主线程）。
 */
private suspend fun detectTermuxStatus(
    context: Context,
    onStall: (() -> Unit)? = null,
): TermuxStatus =
    withContext(Dispatchers.IO) {
        // 主路径：MCP host 探活通过 → bash 走 MCP，RUN_COMMAND 无关紧要。
        val hostAlive = runCatching { McpHostManager.isHostAlive(context) }.getOrDefault(false)
        if (hostAlive) return@withContext TermuxStatus.Ready
        when {
            !TermuxExec.isTermuxInstalled(context) -> TermuxStatus.NotInstalled
            !TermuxExec.hasPermission(context) -> TermuxStatus.NoPermission
            else -> when (
                val r = TermuxDiagnostics.ensureTermuxUp(context, log = { }, onStall = onStall)
            ) {
                is TermuxDiagnostics.ChannelResult.Ok -> TermuxStatus.Ready
                is TermuxDiagnostics.ChannelResult.NoResponse ->
                    TermuxStatus.ChannelDown(TermuxDiagnostics.describe(r), stopped = r.stopped)
                else -> TermuxStatus.ChannelDown(TermuxDiagnostics.describe(r), stopped = false)
            }
        }
    }

/** 不可用提示文案；Ready/检测中返回 null → 不警示。警示只在「host
 *  不在线 且 RUN_COMMAND 阶梯失败」时出现（可用性判定 = RUN_COMMAND
 *  可用 ∨ MCP host 健康，见 [detectTermuxStatus]）。 */
private fun termuxUnavailableHint(status: TermuxStatus): String? = when (status) {
    TermuxStatus.Checking, TermuxStatus.Ready -> null
    TermuxStatus.NotInstalled ->
        "Termux 未安装：该工具需要 Termux 环境，暂无法使用。"
    TermuxStatus.NoPermission ->
        "RUN_COMMAND 权限未授予：该工具暂无法使用。可在「bash 工具执行后端」中申请权限，" +
            "或在设置页启动 MCP 服务后经 MCP 路径执行。"
    is TermuxStatus.ChannelDown ->
        // stopped（未启动过/被强停）用「未启动」比「未就绪」更准确，也不带 describe 摘要。
        if (status.stopped) {
            "Termux 未启动：该工具暂无法使用。打开 Termux 后返回本页，会自动重新检测；" +
                "MCP 服务在线时命令走 MCP 路径，不依赖该通道。"
        } else {
            "Termux 未就绪（${status.summary}）：该工具暂无法使用。打开 Termux 并返回本页后自动重新检测；" +
                "MCP 服务在线时命令走 MCP 路径，不依赖该通道。"
        }
}

/**
 * 工具管理域：列出全部可用工具，逐个切换「对 agent 可见 / 隐藏」。
 *
 * - 「工具命名模板」小节（见 [ToolNameTemplateSection]）：紧邻「MCP 工具」
 *   分组上方，编辑 MCP / builtin 注册名模板（模板即下方分组显示名的来源）；
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
            termuxStatus = detectTermuxStatus(context, onStall = {
                // 0.5s 无应答：先按 ChannelDown 渲染警示（复用
                // termuxUnavailableHint 路径），不等 1s/6s 探测阶梯跑完；
                // 整轮结论到达后由此行赋值自然覆盖。
                termuxStatus = TermuxStatus.ChannelDown(
                    "0.5 秒无应答，检测仍在确认",
                    stopped = TermuxDiagnostics.isTermuxStopped(context),
                )
            })
        }
    }

    /**
     * 工具清单 + 可见性读取（进页首载与「工具命名模板」保存后的重检
     * 共用同一条路径）：FFI 读离主线程（照抄 ExecBackendSection 的 IO 范式）。
     */
    suspend fun reloadTools() {
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
    }

    // 首次进入异步取数。
    LaunchedEffect(Unit) {
        reloadTools()
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
                    // 「工具命名模板」小节：紧邻「MCP 工具」分组上方——模板
                    // 直接决定下方分组的显示名，语境自洽。置于循环内该组的
                    // 位置上（组为空被 continue 跳过也照样渲染：builtin 模板
                    // 还作用于「内置」组的注册名）。
                    if (group == "MCP 工具") {
                        ToolNameTemplateSection(
                            onRecheck = { scope.launch { reloadTools() } },
                        )
                    }
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
 * 「工具命名模板」小节（自 SettingsScreen 的 McpSection 原样迁入，行内
 * 交互逐字节保留，仅落点变化）：两个模板行决定 MCP / host 内置工具在
 * agent 侧的注册名——直接影响本页「MCP 工具」分组与「内置」组的显示名。
 *
 * - 占位符 chip 光标感知插入（聚焦 = 光标/选区处插入，未聚焦 = 追加末尾）；
 * - 实时预览 / 非法提示；Kotlin 预校验镜像 Rust 规则，仅提示，以 Rust
 *   校验为准；
 * - 保存 = setter FFI 校验+落盘（不自动 reload）→ 显式 reloadMcpServers
 *   按新模板重挂 → 读回 effective（同时刷新桥层缓存）→ [onRecheck] 重读
 *   本页工具清单（下方分组随新模板重新聚合显示名）；
 * - 过程日志镜像 logcat（诊断日志面板在设置页 McpSection，本页无面板，
 *   LogExporter 导出路径不变）。
 */
@Composable
private fun ToolNameTemplateSection(onRecheck: () -> Unit) {
    val scope = rememberCoroutineScope()
    // MCP 按钮主题色（primary 12% 底 + primary 文字）：与「必填」徽标/
    // SoftButton 同款惯例，轻量不压卡片内容；disabled 态由 M3 按钮的
    // 默认弱化色承载（buttonColors 只覆盖 enabled 色），可读。
    val mcpBtnContainer = MaterialTheme.colorScheme.primary.copy(alpha = 0.12f)
    val mcpBtnContent = MaterialTheme.colorScheme.primary
    // 两行「保存」共用的在途互斥（沿用 McpSection 原语义）：IO 期间禁用按钮。
    var busy by remember { mutableStateOf(false) }
    // 过程日志出口：镜像到 logcat（OpenSlateDiag，与设置页诊断日志同 tag）。
    fun log(msg: String) {
        android.util.Log.i("OpenSlateDiag", msg)
    }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(
            "工具命名模板",
            style = MaterialTheme.typography.titleSmall,
            color = MaterialTheme.colorScheme.primary,
        )
        Text(
            "模板决定 MCP 工具与 host 内置工具的注册名，下方分组的显示名随之变化；保存后自动重连生效。",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )

        // ── 工具名模板（[mcp].tool_name_template）：保存 = Rust 校验+落盘
        // → reload 热生效（FFI 不自动 reload，此处串起两步）→ 重检。
        // {prefix} 占位符的展开值恒为配置键名（默认 termux，无独立设置行）。
        // 模板状态用 TextFieldValue：占位符插入按钮需要 selection（光标/选区）。
        var templateValue by remember { mutableStateOf(TextFieldValue("")) }
        // 模板输入框当前是否聚焦：未聚焦时插入 = 追加到文本末尾（Compose
        // 的 selection 恒有值，从未聚焦时是 TextRange(0)——不加此标志会
        // 误插到文本开头）。
        var templateFocused by remember { mutableStateOf(false) }
        // 生效值（null = 进页 IO 读取未完成，保存禁用防把缺省值误存）。
        var effectiveTemplate by remember { mutableStateOf<String?>(null) }
        // 进页 IO 读当前生效模板做预填。
        LaunchedEffect(Unit) {
            val cur = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                RuntimeBridge.getMcpToolNameTemplate()
            }
            effectiveTemplate = cur
            templateValue = TextFieldValue(cur ?: "")
        }
        val tpl = templateValue.text.trim()
        // 插入占位符：聚焦时插入光标处 / 替换选区（光标落在插入内容之后）；
        // 未聚焦 → 追加到末尾；selection 越界（理论）clamp 到末尾，同追加语义。
        fun insertPlaceholder(p: String) {
            val v = templateValue
            val len = v.text.length
            val start = (if (templateFocused) v.selection.min else len).coerceIn(0, len)
            val end = (if (templateFocused) v.selection.max else len).coerceIn(0, len)
            templateValue = TextFieldValue(
                text = v.text.substring(0, start) + p + v.text.substring(end),
                selection = TextRange(start + p.length),
            )
        }
        // 本地校验镜像 Rust 侧规则：必须含 {tool}，移除三占位符后剩余
        // 字符仅限字母数字 _ . -（`__` 合法——它是缺省模板的分隔字面）；
        // 留空 = 恢复默认，视为合法。
        val templateValid = tpl.isEmpty() ||
            (tpl.contains("{tool}") &&
                tpl.replace("{prefix}", "").replace("{server}", "").replace("{tool}", "")
                    .matches(Regex("^[A-Za-z0-9_.-]*$")))
        // 实时预览：{prefix} 取当前生效前缀（RuntimeBridge 缓存，只读），
        // {server} 固定示例 memory、{tool} 固定示例 create_entities；空
        // 模板按默认模板预览。展开为字面替换（替换值不含占位符文本，与
        // Rust format_tool_name 的单趟扫描等价）。
        val previewName = tpl.ifEmpty { "{prefix}_{server}__{tool}" }
            .replace("{prefix}", RuntimeBridge.MCP_SERVER_NAMES.firstOrNull() ?: "termux")
            .replace("{server}", "memory")
            .replace("{tool}", "create_entities")
        // ── 工具名模板行 ─────────────────────────────────────────
        OutlinedTextField(
            value = templateValue,
            onValueChange = { templateValue = it },
            label = { Text("工具名模板") },
            placeholder = { Text("{prefix}_{server}__{tool}") },
            singleLine = true,
            modifier = Modifier
                .fillMaxWidth()
                .onFocusChanged { templateFocused = it.isFocused },
        )
        // 占位符快插 chips：等宽字面量，surface 弱化配色（明显轻于主题色
        // 的「保存」）；clickable 不抢焦点——输入框保持聚焦与键盘，
        // 插入位置即当前光标/选区。
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (ph in listOf("{prefix}", "{server}", "{tool}")) {
                Text(
                    ph,
                    style = MaterialTheme.typography.labelMedium,
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .background(MaterialTheme.colorScheme.surfaceVariant)
                        .clickable { insertPlaceholder(ph) }
                        .padding(horizontal = 10.dp, vertical = 5.dp),
                )
            }
        }
        Text(
            "占位符：{prefix} = 前缀，{server} = MCP 服务器名，{tool} = 工具名（不带服务器名前缀）；留空保存恢复默认",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
        if (templateValid) {
            Text(
                "示例：$previewName",
                style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.outline,
            )
        } else {
            Text(
                "模板无效：必须含 {tool}，且除占位符外仅限字母数字 _ . -",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.error,
            )
        }
        CompactButton(
            text = "保存",
            enabled = !busy && effectiveTemplate != null && tpl != effectiveTemplate,
            container = mcpBtnContainer,
            content = mcpBtnContent,
        ) {
            busy = true
            scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                try {
                    val envelope = RuntimeBridge.setMcpToolNameTemplate(tpl)
                    val obj = envelope?.let { runCatching { JSONObject(it) }.getOrNull() }
                    if (obj != null && obj.optBoolean("ok", false)) {
                        log("工具名模板已保存，重连生效中…")
                        // FFI 不自动 reload：手动重连让已注册工具按新模板重挂。
                        val r = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
                        val o = r?.let { runCatching { JSONObject(r) }.getOrNull() }
                        if (o != null && o.optBoolean("ok", false)) {
                            log("${o.optInt("tools", -1)} 个工具已按新模板注册")
                        } else {
                            val err = o?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                                ?: r?.take(120) ?: "runtime 未就绪"
                            log("工具重连失败：$err")
                        }
                        // 读回生效值（同时刷新桥层缓存）；与输入一致后保存钮自动禁用。
                        effectiveTemplate = RuntimeBridge.getMcpToolNameTemplate()
                    } else {
                        val err = obj?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                            ?: envelope?.take(120) ?: "runtime 未就绪"
                        log("模板保存失败：$err")
                    }
                } catch (e: Throwable) {
                    log("模板保存失败：${e.message?.take(160) ?: e.javaClass.simpleName}")
                } finally {
                    busy = false
                }
                // 成败都重读工具清单（重检）：成功让下方分组按新模板重新
                // 聚合显示名；失败让清单如实反映当前注册表。
                onRecheck()
            }
        }

        // ── builtin 工具名模板（[mcp].builtin_tool_name_template）：与上面
        // 「工具名模板」行完全同款交互（光标感知 chip 插入 + 实时预览/非法
        // 提示 + 保存触发显式 reload）。差异仅：作用于不含 `__` 的裸上游
        // 名（host 内置 bash/read_file… 的注册名），chip 只有 {prefix}/
        // {tool} 两个（无 {server}），预览 {tool} 用 bash（默认展示
        // termux_bash）。校验对齐 core 规则：拒 {server}、禁字面 __、
        // 必须含 {tool}——Kotlin 侧仅预校验提示，以 Rust 校验为准。
        var builtinValue by remember { mutableStateOf(TextFieldValue("")) }
        var builtinFocused by remember { mutableStateOf(false) }
        var effectiveBuiltin by remember { mutableStateOf<String?>(null) }
        LaunchedEffect(Unit) {
            val cur = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                RuntimeBridge.getMcpBuiltinToolNameTemplate()
            }
            effectiveBuiltin = cur
            builtinValue = TextFieldValue(cur ?: "")
        }
        val btpl = builtinValue.text.trim()
        fun insertBuiltinPlaceholder(p: String) {
            val v = builtinValue
            val len = v.text.length
            val start = (if (builtinFocused) v.selection.min else len).coerceIn(0, len)
            val end = (if (builtinFocused) v.selection.max else len).coerceIn(0, len)
            builtinValue = TextFieldValue(
                text = v.text.substring(0, start) + p + v.text.substring(end),
                selection = TextRange(start + p.length),
            )
        }
        // 本地校验镜像 Rust 侧规则：必须含 {tool}，不能有 {server}，移除
        // 两占位符后剩余字符仅限字母数字 _ . - 且不含 `__`（裸名注册须
        // 与聚合形态 slug__tool 可区分）；留空 = 恢复默认，视为合法。
        val builtinValid = btpl.isEmpty() ||
            (btpl.contains("{tool}") && !btpl.contains("{server}") &&
                btpl.replace("{prefix}", "").replace("{tool}", "")
                    .matches(Regex("^[A-Za-z0-9_.-]*$")) &&
                !btpl.replace("{prefix}", "").replace("{tool}", "").contains("__"))
        // 实时预览：{prefix} 取当前生效前缀，{tool} 固定示例 bash（host
        // 内置 bash server 的注册名）；空模板按默认模板预览（termux_bash）。
        val builtinPreview = btpl.ifEmpty { "{prefix}_{tool}" }
            .replace("{prefix}", RuntimeBridge.MCP_SERVER_NAMES.firstOrNull() ?: "termux")
            .replace("{tool}", "bash")
        OutlinedTextField(
            value = builtinValue,
            onValueChange = { builtinValue = it },
            label = { Text("builtin 工具名模板") },
            placeholder = { Text("{prefix}_{tool}") },
            singleLine = true,
            modifier = Modifier
                .fillMaxWidth()
                .onFocusChanged { builtinFocused = it.isFocused },
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            for (ph in listOf("{prefix}", "{tool}")) {
                Text(
                    ph,
                    style = MaterialTheme.typography.labelMedium,
                    fontFamily = FontFamily.Monospace,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier
                        .clip(RoundedCornerShape(8.dp))
                        .background(MaterialTheme.colorScheme.surfaceVariant)
                        .clickable { insertBuiltinPlaceholder(ph) }
                        .padding(horizontal = 10.dp, vertical = 5.dp),
                )
            }
        }
        Text(
            "host 内置工具（bash/read_file…，裸名上游）的注册名模板；占位符：{prefix} = 前缀，" +
                "{tool} = 裸工具名；不含 {server}、不能有 __；留空保存恢复默认",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
        if (builtinValid) {
            Text(
                "示例：$builtinPreview",
                style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.outline,
            )
        } else {
            Text(
                "模板无效：必须含 {tool}，不能用 {server}，除占位符外仅限字母数字 _ . -，且不能含 __",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.error,
            )
        }
        CompactButton(
            text = "保存",
            enabled = !busy && effectiveBuiltin != null && btpl != effectiveBuiltin,
            container = mcpBtnContainer,
            content = mcpBtnContent,
        ) {
            busy = true
            scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                try {
                    val envelope = RuntimeBridge.setMcpBuiltinToolNameTemplate(btpl)
                    val obj = envelope?.let { runCatching { JSONObject(it) }.getOrNull() }
                    if (obj != null && obj.optBoolean("ok", false)) {
                        log("builtin 模板已保存，重连生效中…")
                        // 同上：FFI 不自动 reload，显式重连让 host 裸名按新
                        // 模板重挂（reload 同时交接 HostTool 让位/兜底）。
                        val r = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
                        val o = r?.let { runCatching { JSONObject(r) }.getOrNull() }
                        if (o != null && o.optBoolean("ok", false)) {
                            log("${o.optInt("tools", -1)} 个工具已按新模板注册")
                        } else {
                            val err = o?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                                ?: r?.take(120) ?: "runtime 未就绪"
                            log("工具重连失败：$err")
                        }
                        effectiveBuiltin = RuntimeBridge.getMcpBuiltinToolNameTemplate()
                    } else {
                        val err = obj?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                            ?: envelope?.take(120) ?: "runtime 未就绪"
                        log("builtin 模板保存失败：$err")
                    }
                } catch (e: Throwable) {
                    log("builtin 模板保存失败：${e.message?.take(160) ?: e.javaClass.simpleName}")
                } finally {
                    busy = false
                }
                onRecheck()
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
