package dev.openslate.mobile.ui

import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.background
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.clickable
import androidx.compose.material3.HorizontalDivider
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Add
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.CheckCircle
import androidx.compose.material.icons.filled.Cancel
import androidx.compose.material.icons.filled.Delete
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.KeyboardArrowUp
import androidx.compose.material.icons.outlined.Warning
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.Key
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExtendedFloatingActionButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.FilterChip
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.text.BasicTextField
import androidx.compose.foundation.text.selection.LocalTextSelectionColors
import androidx.compose.foundation.text.selection.TextSelectionColors
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.size
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.async
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.launch
import org.json.JSONObject
import dev.openslate.mobile.bridge.LogExporter
import dev.openslate.mobile.bridge.ConfigUi
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.ModelUi
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.hazeSource
import dev.chrisbanes.haze.rememberHazeState
import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import dev.openslate.mobile.bridge.ProviderUi
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.TermuxDiagnostics

/**
 * 模型配置页：Provider / 模型条目 / 级别绑定 / API key。
 * 全部改动经 ClientMsg CRUD → Rust persist 层落盘 openslate.toml →
 * config_changed 事件回刷（协议复用，与桌面 TUI 同一条写回链）。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(
    onBack: () -> Unit,
    onOpenKeys: () -> Unit,
    onOpenRegistry: () -> Unit,
    themeMode: ThemeMode = ThemeMode.SYSTEM,
    onThemeModeChange: (ThemeMode) -> Unit = {},
) {
    val state by RuntimeBridge.state.collectAsState()
    val config = state.config
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    // Haze 内容源：会话页功能模型弹层（GlassDropdownMenu）对本页内容做局部
    // 真实模糊；挂在下方承载各域内容的内容列上（即弹层背后可见区域）。
    val hazeState = rememberHazeState()

    var editProvider by remember { mutableStateOf<ProviderUi?>(null) }
    var editProviderIsNew by remember { mutableStateOf(false) }
    var editModel by remember { mutableStateOf<ModelUi?>(null) }
    var editModelIsNew by remember { mutableStateOf(false) }
    /** Provider 页模型管理打开时锁定 provider（条目归属不可跨 provider 改）。 */
    var editModelLock by remember { mutableStateOf<String?>(null) }
    /** Provider 的模型管理弹窗（null = 关闭）。 */
    var modelsProvider by remember { mutableStateOf<ProviderUi?>(null) }
    var tab by androidx.compose.runtime.saveable.rememberSaveable { mutableStateOf(0) }
    var toolTab by androidx.compose.runtime.saveable.rememberSaveable { mutableStateOf(0) }
    var modelTab by androidx.compose.runtime.saveable.rememberSaveable { mutableStateOf(0) }

    // API key 存在性批量预取（IO 线程）：Keystore 解密是 binder IPC + AES-GCM，
    // 单次 5~50ms；此前在 provider 行组合期同步查、每次重组都查 → 进设置页必卡一帧。
    // 三态：区分「未配置」与「密文在但解密失败（需重新录入）」。
    var keyStatus by remember { mutableStateOf<Map<String, RuntimeBridge.KeyPresence>>(emptyMap()) }
    androidx.compose.runtime.LaunchedEffect(config.providers) {
        val providers = config.providers
        keyStatus = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            providers.associate { it.name to RuntimeBridge.keyPresence(it.name) }
        }
    }
    // 密钥持久化失败的 provider id（加密写入异常时弹窗提示，不置已配置）。
    var keySaveError by remember { mutableStateOf<String?>(null) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("设置") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "返回")
                    }
                },
                actions = {
                    // 统一密钥管理页入口（所有 Provider 的 API 密钥一处管理）。
                    IconButton(onClick = onOpenKeys) {
                        Icon(Icons.Filled.Key, contentDescription = "API 密钥管理")
                    }
                },
            )
        },
    ) { padding ->
        Column(
            Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            // ── Tab 栏：每个域独立负责一类设置 ──────────────────
            androidx.compose.material3.TabRow(selectedTabIndex = tab) {
                val tabs = listOf("外观" to 0, "模型" to 1, "工具" to 2)
                for ((label, idx) in tabs) {
                    androidx.compose.material3.Tab(
                        selected = tab == idx,
                        onClick = { tab = idx },
                        text = { Text(label) },
                    )
                }
            }

            // ── 模型域二级 Tab：会话 / 模型 / Provider（吸顶，同工具域）──
            if (tab == 1) {
                androidx.compose.material3.TabRow(selectedTabIndex = modelTab) {
                    for ((label, idx) in listOf("会话" to 0, "模型" to 1, "Provider" to 2)) {
                        androidx.compose.material3.Tab(
                            selected = modelTab == idx,
                            onClick = { modelTab = idx },
                            text = { Text(label) },
                        )
                    }
                }
            }

            // ── 工具域二级 Tab：吸顶固定（在滚动容器外），紧贴主 Tab ──
            if (tab == 2) {
                androidx.compose.material3.TabRow(selectedTabIndex = toolTab) {
                    for ((label, idx) in listOf("MCP" to 0, "命令行" to 1, "工具管理" to 2)) {
                        androidx.compose.material3.Tab(
                            selected = toolTab == idx,
                            onClick = { toolTab = idx },
                            text = { Text(label) },
                        )
                    }
                }
            }

            // ── 域内容（各自独立滚动）───────────────────────────
            // hazeSource：本内容列即弹层背后实际可见的内容区域（含键盘上方）。
            Column(
                Modifier
                    .fillMaxSize()
                    .hazeSource(hazeState)
                    .verticalScroll(rememberScrollState())
                    .padding(16.dp),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                when (tab) {
                    0 -> {
                        // ── 外观域：主题模式（持久化 + 全局生效）──────
                        Section(title = "主题") {
                            Text(
                                "选择应用的深浅色配色：跟随系统或固定浅色/深色。" +
                                    "即时生效，重启后保持。",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                            val modes = listOf(
                                "跟随系统" to ThemeMode.SYSTEM,
                                "浅色" to ThemeMode.LIGHT,
                                "深色" to ThemeMode.DARK,
                            )
                            SlidingSegmentedControl(
                                options = modes.map { it.first },
                                selected = modes.first { it.second == themeMode }.first,
                                onSelect = { label ->
                                    onThemeModeChange(modes.first { it.first == label }.second)
                                },
                            )
                        }
                    }
                    1 -> {
                        // ── 模型域：会话 / 模型 / Provider 三个二级 tab ──
                        when (modelTab) {
                            0 -> {
                        // ── 会话域：标题生成模型（功能 → 模型条目直绑）──
                        // 模型条目清单在「Provider」页管理（自动检测 + 手动
                        // 增删）；levels 代号映射层已从 UI 移除（存量表保留
                        // 不写，仅作解析兜底）。主对话模型已移至对话页输入
                        // 框聚焦面板的选择器直接选定（选了即落盘为启动默认，
                        // RuntimeBridge.selectModel 双写）；上下文压缩不再出
                        // 现在 UI（未绑定时 core 侧继承会话模型）。
                        Section(title = "标题生成模型") {
                            Text(
                                "新会话的标题用哪个模型条目生成，改动立即落盘生效。" +
                                    "主对话模型在对话页输入框面板里直接选择，选了即存为默认。",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                            CapabilityModelRow("title", "会话标题生成", config, hazeState)
                        }
                    }
                    1 -> {
                        // ── 模型域：条目管理入口指引 + 元数据数据源 ──────
                        // levels 代号列表/绑定/创建已随映射层一并移除；模型
                        // 条目在各 Provider 的「模型」管理里维护，功能绑定
                        // 在「会话」页完成。
                        Text(
                            "模型条目在各 Provider 的「模型」管理里增删改（自动检测 + 手动添加）；" +
                                "标题生成绑定在「会话」页，主对话模型在对话页输入框面板里直接选。",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                        // 元数据数据源管理（更新 / 本地缓存状态）。
                        Card(
                            modifier = Modifier.fillMaxWidth(),
                            onClick = onOpenRegistry,
                        ) {
                            Column(Modifier.padding(12.dp)) {
                                Text("模型数据源", style = MaterialTheme.typography.titleSmall)
                                Text(
                                    "上下文 / 输出限额 / 视觉 / 思考 / 计价的来源与本地缓存更新",
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                            }
                        }
                    }
                    2 -> {
                        // ── Provider 域：Provider CRUD + API key ─────────
                        Section(
                            title = "Provider",
                            action = {
                                IconButton(onClick = { editProvider = ProviderUi("", "", "", null); editProviderIsNew = true }) {
                                    Icon(Icons.Filled.Add, contentDescription = "新增 Provider")
                                }
                            },
                        ) {
                            if (config.providers.isEmpty()) Text("（空）", color = MaterialTheme.colorScheme.outline)
                            for (p in config.providers) {
                                Card(
                                    modifier = Modifier.fillMaxWidth(),
                                    onClick = { editProvider = p; editProviderIsNew = false },
                                ) {
                                    Row(
                                        Modifier.padding(12.dp),
                                        verticalAlignment = Alignment.CenterVertically,
                                    ) {
                                        Column(Modifier.weight(1f)) {
                                            Text(p.displayName, style = MaterialTheme.typography.titleSmall)
                                            Text(
                                                p.baseUrl,
                                                style = MaterialTheme.typography.bodySmall,
                                                fontFamily = MonoFont,
                                                color = MaterialTheme.colorScheme.outline,
                                            )
                                            // null = 预取进行中（避免先闪"未配置"再变"已存"）。
                                            val hasKey = keyStatus[p.name]
                                            Row(verticalAlignment = Alignment.CenterVertically) {
                                                if (hasKey == RuntimeBridge.KeyPresence.CONFIGURED) {
                                                    Icon(
                                                        imageVector = Icons.Filled.Check,
                                                        contentDescription = "已存",
                                                        tint = MaterialTheme.colorScheme.primary,
                                                        modifier = Modifier.height(12.dp).width(12.dp),
                                                    )
                                                }
                                                Text(
                                                    when (hasKey) {
                                                        RuntimeBridge.KeyPresence.CONFIGURED -> "  密钥已配置"
                                                        RuntimeBridge.KeyPresence.MISSING -> "密钥未配置"
                                                        RuntimeBridge.KeyPresence.DECRYPT_FAILED -> "密钥解密失败，请重新录入"
                                                        null -> "密钥状态检查中…"
                                                    },
                                                    style = MaterialTheme.typography.labelSmall,
                                                    color = when (hasKey) {
                                                        RuntimeBridge.KeyPresence.CONFIGURED -> MaterialTheme.colorScheme.primary
                                                        RuntimeBridge.KeyPresence.MISSING -> MaterialTheme.colorScheme.error
                                                        RuntimeBridge.KeyPresence.DECRYPT_FAILED -> MaterialTheme.colorScheme.error
                                                        null -> MaterialTheme.colorScheme.outline
                                                    },
                                                )
                                            }
                                        }
                                        // 该 Provider 的模型管理（自动检测 + 手动
                                        // 增删改）；卡片其余区域仍是编辑 Provider。
                                        val modelCount = config.models.count { it.provider == p.name }
                                        SoftButton(onClick = { modelsProvider = p }) {
                                            Text("模型（$modelCount）")
                                        }
                                    }
                                }
                            }
                        }
                        Text(
                            "配置写入 ${config.activeConfig.ifEmpty { "openslate.toml" }}；密钥存 Android Keystore（加密），不进配置文件。",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                            }
                        }
                    }
                    2 -> {
                        // ── 工具域（二级 Tab 在上方吸顶）：MCP / 命令行 / 工具管理 ──
                        when (toolTab) {
                            0 -> {
                                McpSection()
                                LogSection()
                            }
                            1 -> ExecBackendSection()
                            else -> ToolManagementSection()
                        }
                    }
                }
            }
        }
    }

    // ── 对话框 ─────────────────────────────────────────────────
    editProvider?.let { p ->
        EditProviderDialog(
            initial = p,
            isNew = editProviderIsNew,
            initialHasKey = keyStatus[p.name] == RuntimeBridge.KeyPresence.CONFIGURED,
            existingIds = config.providers.map { it.name }.toSet(),
            onDismiss = { editProvider = null },
            onSave = { id, title, baseUrl, env, adapter, apiKey ->
                RuntimeBridge.upsertProvider(id, baseUrl, env, adapter, title)
                // 编辑时原地填写的 Key：直接落 Keystore（按内部 ID 关联）。
                // 保存链路（Keystore IPC + FFI 注入）离主线程，完成后再回填 UI；
                // 失败不置已配置，改由 keySaveError 弹窗提示。
                val k = apiKey
                if (k != null) {
                    scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                        val ok = RuntimeBridge.setApiKeyAndPersist(id, k)
                        kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.Main) {
                            if (ok) {
                                keyStatus = keyStatus + (id to RuntimeBridge.KeyPresence.CONFIGURED)
                            } else {
                                keySaveError = id
                            }
                        }
                    }
                }
                editProvider = null
            },
        )
    }

    keySaveError?.let { id ->
        AlertDialog(
            onDismissRequest = { keySaveError = null },
            title = { Text("密钥保存失败") },
            text = {
                Text(
                    "「${config.providers.find { it.name == id }?.displayName ?: id}」的密钥" +
                        "已注入本次运行，但加密持久化失败（Keystore 写入异常），下次启动将丢失。" +
                        "请到 API 密钥管理页重新保存；若持续失败请检查设备安全硬件后重启应用。",
                )
            },
            confirmButton = {
                SoftButton(onClick = { keySaveError = null }) { Text("知道了") }
            },
        )
    }
    editModel?.let { m ->
        EditModelDialog(
            initial = m,
            isNew = editModelIsNew,
            providers = config.providers.map { it.displayName to it.name },
            lockProvider = editModelLock ?: m.provider,
            onDismiss = {
                editModel = null
                editModelLock = null
            },
            onSave = { ui ->
                // Provider 页新建且未填条目 ID → 自动生成（providerId/modelId）。
                val final = if (ui.entry.isBlank()) {
                    ui.copy(
                        entry = genModelEntryId(
                            ui.provider,
                            ui.modelId,
                            config.models.map { it.entry }.toSet(),
                        ),
                    )
                } else {
                    ui
                }
                // 查重：同 Provider 下不允许重复的模型 ID（否则键冲突
                // 会生成随机后缀新条目，列表出现重复模型）；同 entry 覆盖
                // 更新不受影响。服务端 upsert_model 守卫同款兜底。
                val dup = config.models.find {
                    it.provider == final.provider && it.modelId == final.modelId &&
                        it.entry != final.entry
                }
                if (dup != null) {
                    "该 Provider 下已存在模型「${final.modelId}」，请直接编辑原条目或删除重复条目"
                } else {
                    RuntimeBridge.upsertModel(final)
                    editModel = null
                    editModelLock = null
                    null
                }
            },
        )
    }

    // Provider 的模型管理（自动检测 + 手动增删改）。
    modelsProvider?.let { p ->
        ProviderModelsDialog(
            provider = p,
            models = config.models.filter { it.provider == p.name },
            capabilities = config.capabilities,
            onDismiss = { modelsProvider = null },
            onEdit = { m, isNew ->
                editModel = m
                editModelIsNew = isNew
                editModelLock = p.name
                modelsProvider = null
            },
        )
    }
}

/** token 数显示格式：131072 → "128K"、2000000 → "2M"、null → "—"。
 *  exact = true 时显示千分位完整数字（详情弹窗用）。 */
private fun fmtTokens(t: Long?, exact: Boolean = false): String {
    if (t == null || t <= 0) return "—"
    if (exact) return "%,d".format(t)
    val unit = when {
        t >= 1_000_000L -> "M" to t / 1_000_000.0
        t >= 1_000L -> "K" to t / 1_000.0
        else -> return t.toString()
    }
    val (suffix, v) = unit
    val s = if (v >= 100.0 || v == kotlin.math.floor(v)) "%.0f".format(v) else "%.1f".format(v)
    return "$s$suffix"
}

/** 计价显示（$/Mtok）：整数/大数不带小数、常见两位、极小值四位；null → "—"。 */
private fun fmtPrice(p: Double?): String {
    if (p == null) return "—"
    val s = when {
        p >= 100.0 || p == kotlin.math.floor(p) -> "%.0f".format(p)
        p < 0.01 -> "%.4f".format(p)
        else -> "%.2f".format(p)
    }
    return "\$$s / Mtok"
}

/** 详情弹窗用：简写 + 千分位全量（"128K（131,072）"）；null → "—"，简写
 *  已是全量（如 999）时不重复。 */
private fun fmtTokensFull(t: Long?): String {
    val short = fmtTokens(t)
    val exact = if (t != null && t > 0) fmtTokens(t, exact = true) else null
    return if (exact != null && exact != short) "$short（$exact）" else short
}

/** 布尔能力旗标：Material 图标 + 标签（图标规范见 AGENTS.md——勾叉
 *  一律用 Icon，不用 Unicode 字符，跨设备字体渲染不一致）。 */
@Composable
private fun CapsFlag(label: String, on: Boolean) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Icon(
            if (on) Icons.Filled.CheckCircle else Icons.Filled.Cancel,
            contentDescription = if (on) "$label：支持" else "$label：不支持",
            tint = if (on) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.outline,
            modifier = Modifier.height(13.dp).width(13.dp),
        )
        Text(
            label,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
    }
}

/** 模型能力摘要行（Provider 模型管理列表）：视觉 / 思考 / 上下文 / 输出。
 *  单行显示，放不下时末尾省略号截断（不换行）。内部条目 ID 不对外展示。 */
@Composable
private fun ModelCapsRow(m: ModelUi) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(10.dp),
    ) {
        CapsFlag(label = "视觉", on = m.supportsVision)
        CapsFlag(label = "思考", on = m.supportsReasoning)
        Text(
            "上下文 ${fmtTokens(m.maxContextTokens)}",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            softWrap = false,
        )
        Text(
            "输出 ${fmtTokens(m.maxOutputTokens)}",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            softWrap = false,
            modifier = Modifier.weight(1f, fill = false),
        )
    }
}

/** 详情弹窗的信息行：标签（灰、小字）靠左，值靠右。 */
@Composable
private fun InfoRow(label: String, value: String) {
    Row(Modifier.fillMaxWidth()) {
        Text(
            label,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline,
            modifier = Modifier.weight(1f),
        )
        Text(value, style = MaterialTheme.typography.bodySmall)
    }
}

/** 模型条目内部 ID：`<providerId>/<modelId>`（两段原样、仅 trim——与
 * models.dev 注册表键 `zhipuai/glm-4.7` 同形；toml 中含 `/` 的键由
 * toml_edit 自动加引号，合法且无碍）。键冲突（同键已被手写占用）时
 * 追加随机 hex 后缀。仅在 Provider 页自动检测/手动添加时生成；改名
 * 入口不存在，引用稳定。存量 `provider__model` slug 键由 RuntimeBridge
 * 启动迁移（migrateModelEntrySlash）统一改写，此处不再生成。 */
private fun genModelEntryId(providerId: String, modelId: String, existing: Set<String>): String {
    val p = providerId.trim()
    val m = modelId.trim()
    var id = "$p/$m"
    if (id !in existing) return id
    val chars = ('a'..'f') + ('0'..'9')
    while (id in existing) {
        id = "$p/$m-" + List(4) { chars.random() }.joinToString("")
    }
    return id
}

/** Provider 的模型管理：自动检测（远端 /models）+ 手动增删改。
 *  检测出的新模型以 chip 呈现，点击即入库（自动生成条目 ID）。 */
@Composable
private fun ProviderModelsDialog(
    provider: ProviderUi,
    models: List<ModelUi>, // 该 provider 名下的条目
    capabilities: Map<String, String>, // 功能 → 条目直绑值（删除时悬空提示；删除不再被引用拦截）
    onDismiss: () -> Unit,
    onEdit: (ModelUi, Boolean) -> Unit, // (条目, isNew)
) {
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    var probing by remember { mutableStateOf(false) }
    var probeError by remember { mutableStateOf<String?>(null) }
    /** 待删除确认的条目（null = 无弹窗）。 */
    var pendingDelete by remember { mutableStateOf<ModelUi?>(null) }
    /** 详情展示中的条目（null = 无弹窗）。 */
    var detailModel by remember { mutableStateOf<ModelUi?>(null) }
    var detected by remember { mutableStateOf<List<String>?>(null) }
    val existingIds = models.map { it.modelId }.toSet()

    AppDialog(
        onDismiss = onDismiss,
        title = "${provider.displayName} 的模型",
    ) {
        Column(
            Modifier.verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
                Text(
                    "此清单是该 Provider 的模型条目；「会话」页的功能模型最终都" +
                        "指向这里的条目。点条目左侧可查看详情与功能引用。",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                // 已有条目
                if (models.isEmpty()) {
                    Text("（暂无模型条目）", color = MaterialTheme.colorScheme.outline)
                }
                for (m in models) {
                    Row(
                        Modifier.fillMaxWidth(),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        // 点击左侧信息区查看完整详情（摘要行被省略号截断的补全）。
                        Column(Modifier.weight(1f).clickable { detailModel = m }) {
                            Text(m.modelId, style = MaterialTheme.typography.bodyMedium, fontFamily = MonoFont)
                            // 能力摘要（内部条目 ID 不对外展示）。
                            ModelCapsRow(m)
                        }
                        IconButton(onClick = { onEdit(m, false) }) {
                            Icon(
                                Icons.Filled.Edit,
                                contentDescription = "编辑",
                                tint = MaterialTheme.colorScheme.outline,
                            )
                        }
                        IconButton(onClick = { pendingDelete = m }) {
                            Icon(
                                Icons.Filled.Delete,
                                contentDescription = "删除",
                                tint = MaterialTheme.colorScheme.outline,
                            )
                        }
                    }
                }

                androidx.compose.material3.HorizontalDivider()

                if (probeError != null) {
                    Text(
                        probeError ?: "",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
                // 检测结果：尚未入库的模型 → 点击添加
                val fresh = detected?.filter { it !in existingIds }.orEmpty()
                if (fresh.isNotEmpty()) {
                    Text("检测到 ${fresh.size} 个新模型（点击添加）", style = MaterialTheme.typography.labelMedium)
                    Column {
                        for (id in fresh) {
                            FilterChip(
                                selected = false,
                                onClick = {
                                    val entry = genModelEntryId(provider.name, id, models.map { it.entry }.toSet())
                                    val base = ModelUi(entry = entry, provider = provider.name, modelId = id)
                                    // 先立即入库（即时反馈），再异步补全元数据
                                    // （本地 registry 优先，miss 时在线兜底）；
                                    // 补全后 config_changed 自动回刷列表。
                                    RuntimeBridge.upsertModel(base)
                                    scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                                        val meta = RuntimeBridge.lookupModelMeta(id)
                                        if (meta != null && meta.hasAny) {
                                            RuntimeBridge.upsertModel(
                                                base.copy(
                                                    maxContextTokens = meta.contextTokens,
                                                    maxOutputTokens = meta.maxOutputTokens,
                                                    supportsVision = meta.supportsVision ?: false,
                                                    supportsReasoning = meta.supportsReasoning ?: false,
                                                    supportsToolCall = meta.supportsToolCall ?: true,
                                                    inputPricePerMtok = meta.inputPricePerMtok,
                                                    outputPricePerMtok = meta.outputPricePerMtok,
                                                ),
                                            )
                                        }
                                    }
                                },
                                label = {
                                    Row(
                                        verticalAlignment = Alignment.CenterVertically,
                                        horizontalArrangement = Arrangement.spacedBy(4.dp),
                                    ) {
                                        Icon(
                                            Icons.Filled.Add,
                                            contentDescription = null,
                                            modifier = Modifier.height(13.dp).width(13.dp),
                                            tint = MaterialTheme.colorScheme.primary,
                                        )
                                        Text(id, fontFamily = MonoFont)
                                    }
                                },
                            )
                        }
                    }
                    if (detected.orEmpty().size == fresh.size) {
                        Text(
                            "（已全部入库）",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                }

                // 底部动作：检测 / 添加 同行（8dp 间距）；关闭右对齐新起
                // 一行，行距由 Column spacedBy(8dp) 控制，紧贴上方按钮。
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    horizontalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    SoftButton(
                        enabled = !probing,
                        onClick = {
                            probing = true
                            probeError = null
                            scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                                val r = RuntimeBridge.listProviderModels(provider.name)
                                kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.Main) {
                                    probing = false
                                    if (r.ok) {
                                        detected = r.models
                                        if (r.models.isEmpty()) probeError = "远端返回空清单"
                                    } else {
                                        probeError = r.error
                                    }
                                }
                            }
                        },
                    ) { Text(if (probing) "检测中…" else "自动检测可用模型") }
                    SoftButton(onClick = { onEdit(ModelUi("", provider.name, ""), true) }) {
                        Text("手动添加")
                    }
                }
                Row(Modifier.fillMaxWidth(), horizontalArrangement = Arrangement.End) {
                    SoftButton(onClick = onDismiss) { Text("关闭") }
                }
            }
    }

    // 删除确认。完全松耦合：删除永远放行，引用方悬空。功能悬空 →
    // 「会话」页该功能行显示未设置，重选条目即自愈（服务端 DeleteModel
    // 无引用拦截，同款语义）。
    pendingDelete?.let { m ->
        val capRefs = capabilities.filterValues { it == m.entry }.keys.toList()
        val dangling = if (capRefs.isEmpty()) "" else
            "功能 ${capRefs.joinToString("、")} 将悬空（到「会话」页或对话页模型选择器改绑）"
        AlertDialog(
            onDismissRequest = { pendingDelete = null },
            title = { Text("删除模型") },
            text = {
                Text(
                    if (dangling.isEmpty()) {
                        "删除「${m.modelId}」？此操作不可撤销。"
                    } else {
                        "删除「${m.modelId}」？$dangling。此操作不可撤销。"
                    },
                )
            },
            confirmButton = {
                SoftButton(onClick = {
                    RuntimeBridge.deleteModel(m.entry)
                    pendingDelete = null
                }) { Text("删除") }
            },
            dismissButton = {
                SoftButton(onClick = { pendingDelete = null }) { Text("取消") }
            },
        )
    }

    // 模型详情：点击条目左侧信息区弹出（摘要行省略号截断的信息在此完整展示；
    // 内部条目 ID 不对外）。功能引用是展示时从 capabilities 反查的（松耦合：
    // 功能 → 条目单向直绑，模型侧不存引用，换绑在「会话」页完成）。
    detailModel?.let { m ->
        val capRefs = capabilities.filterValues { it == m.entry }.keys.sorted()
        AlertDialog(
            onDismissRequest = { detailModel = null },
            title = { Text(m.modelId, fontFamily = MonoFont) },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Row(
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        CapsFlag(label = "视觉", on = m.supportsVision)
                        CapsFlag(label = "思考", on = m.supportsReasoning)
                        CapsFlag(label = "工具调用", on = m.supportsToolCall)
                    }
                    androidx.compose.material3.HorizontalDivider()
                    InfoRow("被功能引用", capRefs.joinToString("、").ifEmpty { "（无）" })
                    InfoRow("上下文窗口", fmtTokensFull(m.maxContextTokens))
                    InfoRow("最大输出", fmtTokensFull(m.maxOutputTokens))
                    InfoRow("输入价格", fmtPrice(m.inputPricePerMtok))
                    InfoRow("输出价格", fmtPrice(m.outputPricePerMtok))
                    InfoRow("所属 Provider", m.provider)
                    if (capRefs.isNotEmpty()) {
                        Text(
                            "功能绑定只是指向本模型的直绑引用；要整体换新模型，把对应功能" +
                                "改绑到新条目即可（「会话」页；主对话在对话页选择器重选）。",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                }
            },
            confirmButton = {
                SoftButton(onClick = { detailModel = null }) { Text("关闭") }
            },
        )
    }
}@Composable
private fun Section(
    title: String,
    subtitle: String? = null,
    action: (@Composable () -> Unit)? = null,
    content: @Composable androidx.compose.foundation.layout.ColumnScope.() -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            // 标题块整体居左（weight 占满），action 居右；subtitle 非空时
            // 主副标题紧凑成组，按钮对标题块垂直居中（双行时视觉平衡）。
            Column(Modifier.weight(1f)) {
                Text(
                    title,
                    style = MaterialTheme.typography.titleMedium,
                )
                if (subtitle != null) {
                    Text(
                        subtitle,
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(top = 2.dp),
                    )
                }
            }
            action?.invoke()
        }
        content()
    }
}

/**
 * 功能模型绑定行（会话页；现仅「标题生成」在用——主对话模型已移至对话页
 * 输入框聚焦面板的选择器直选，compact 不再暴露）：功能名居左、当前绑定
 * 模型居右、箭头指示可点；整行点击弹出共享玻璃弹层（[GlassDropdownMenu]，
 * 与对话页「本次对话模型」选择器同族磨砂语言），直选 models 条目，选中即
 * setCapability 落盘（main 改绑由 Rust 热切换会话模型并广播 model_changed，
 * 下回合即生效）。
 *
 * 当前值解析（与 Rust capability_alias + resolve_model 同序）：生效别名 =
 * capabilities[cap]，缺省回退 main→"main"、其余→"fast"；再经
 * [RuntimeBridge.modelEntryOf] 的 levels 遗留兜底解析到条目。解析不出条目
 * （悬空代号 / 条目已删）显示「未设置」——重选条目即自愈。
 */
@Composable
private fun CapabilityModelRow(cap: String, title: String, config: ConfigUi, hazeState: HazeState) {
    var expanded by remember { mutableStateOf(false) }
    val stored = config.capabilities[cap]
    val fallback = if (cap == "main") "main" else "fast"
    // 生效别名（镜像 capability_alias）→ 实际条目（镜像 resolve_model 首步）。
    val currentEntry = RuntimeBridge.modelEntryOf(stored ?: fallback, config)
    val entries = config.models.map { it.entry }.toSet()
    val bound = currentEntry.takeIf { it in entries }
    val items = config.models.map { m ->
        GlassDropdownItem(
            value = m.entry,
            label = RuntimeBridge.modelAliasDisplay(m.entry, config),
            selected = m.entry == bound,
        )
    }
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "capModelArrow",
    )
    Box {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(10.dp))
                .clickable(enabled = items.isNotEmpty()) { expanded = true }
                .padding(horizontal = 4.dp, vertical = 6.dp),
        ) {
            Text(
                title,
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.weight(1f),
            )
            Text(
                // 当前值 = 模型标识（modelAliasDisplay 输出）→ 内嵌 MonoFont
                // 子集；「先在…添加/未设置」等纯状态分支保持默认字体。
                text = when {
                    items.isEmpty() -> AnnotatedString("先在 Provider 页添加模型")
                    bound == null -> AnnotatedString("未设置")
                    else -> buildAnnotatedString {
                        withStyle(SpanStyle(fontFamily = MonoFont)) {
                            append(RuntimeBridge.modelAliasDisplay(bound, config))
                        }
                    }
                },
                style = MaterialTheme.typography.labelLarge,
                color = if (bound == null) {
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
                contentDescription = if (expanded) "收起${title}的模型选择" else "选择${title}的模型",
                tint = MaterialTheme.colorScheme.outline,
                modifier = Modifier.rotate(arrowAngle),
            )
        }
        GlassDropdownMenu(
            expanded = expanded,
            onDismissRequest = { expanded = false },
            hazeState = hazeState,
            items = items,
            onSelect = { RuntimeBridge.setCapability(cap, it.value) },
        )
    }
}

/**
 * Termux 放行外部应用（allow-external-apps）的一条式修复命令。
 * 写入内容前后各带一个换行：无论原文件末行是否有换行符，都不会与旧内容拼接；
 */
private const val TERMUX_ALLOW_EXTERNAL_CMD =
    "mkdir -p ~/.termux && printf '\\nallow-external-apps=true\\n' >> ~/.termux/termux.properties && termux-reload-settings"

/**
 * 滑动药丸分段选择器：所有选项平铺在一个圆角容器里，选中项背后是一块
 * 半透明着色区域；切换时色块以动画平滑滑动到目标位置。
 *
 * [label] 仅换显示文字（默认恒等）：选项的**值语义**（selected 匹配、
 * onSelect 回传）始终用原始 option，供调用方把内部键映射为显示串。
 */
@Composable
private fun SlidingSegmentedControl(
    options: List<String>,
    selected: String,
    onSelect: (String) -> Unit,
    modifier: Modifier = Modifier,
    label: (String) -> String = { it },
) {
    val selectedIndex = options.indexOf(selected).coerceAtLeast(0)
    // 色块位置用「选中索引」的浮点动画驱动，index 连续变化 → 色块连续滑动。
    val anim by animateFloatAsState(
        targetValue = selectedIndex.toFloat(),
        animationSpec = tween(durationMillis = 240, easing = FastOutSlowInEasing),
        label = "segmentSlide",
    )
    BoxWithConstraints(modifier.fillMaxWidth()) {
        val segWidth = maxWidth / options.size
        Box(
            Modifier
                .fillMaxWidth()
                .height(44.dp)
                .clip(RoundedCornerShape(22.dp))
                .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.45f)),
        ) {
            // 半透明高亮色块：x = 段宽 × 动画进度，4dp 内边距形成描边感。
            Box(
                Modifier
                    .offset(x = segWidth * anim)
                    .size(width = segWidth, height = 44.dp)
                    .padding(4.dp)
                    .clip(RoundedCornerShape(18.dp))
                    .background(MaterialTheme.colorScheme.secondary.copy(alpha = 0.35f)),
            )
            Row(Modifier.fillMaxSize()) {
                options.forEach { opt ->
                    val isSel = opt == selected
                    Box(
                        Modifier
                            .weight(1f)
                            .fillMaxHeight()
                            .clickable(
                                interactionSource = remember { MutableInteractionSource() },
                                indication = null, // 滑动色块本身就是反馈，不再叠加矩形涟漪
                            ) { onSelect(opt) },
                        contentAlignment = Alignment.Center,
                    ) {
                        Text(
                            label(opt),
                            style = MaterialTheme.typography.titleSmall,
                            color = if (isSel) MaterialTheme.colorScheme.primary
                            else MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                }
            }
        }
    }
}

/**
 * 代码块：header 带「复制」按钮（可见、可发现）+ 等宽代码体。
 * - 快路径：点代码体或「复制」按钮 → 直接写剪贴板，header 变「已复制」；
 * - 回退：复制失败 → 自动切手动模式：代码变可编辑块并全选，用户长按手动复制。
 */
@Composable
private fun CodeBlock(text: String, modifier: Modifier = Modifier) {
    val context = androidx.compose.ui.platform.LocalContext.current
    var copied by remember { mutableStateOf(false) }
    var manual by remember { mutableStateOf(false) }
    // 手动模式的可编辑块：初始即整段全选。
    var value by remember(text) { mutableStateOf(TextFieldValue(text, TextRange(0, text.length))) }
    var everFocused by remember { mutableStateOf(false) }
    val focusRequester = remember { FocusRequester() }
    LaunchedEffect(copied) {
        if (copied) {
            kotlinx.coroutines.delay(2000)
            copied = false
        }
    }
    LaunchedEffect(manual) {
        if (manual) focusRequester.requestFocus()
    }

    fun copyToClipboard(): Boolean = try {
        val cm = context.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
            as android.content.ClipboardManager
        cm.setPrimaryClip(android.content.ClipData.newPlainText("termux", text))
        true
    } catch (t: Throwable) {
        false
    }

    Column(modifier.fillMaxWidth()) {
        Column(
            Modifier
                .fillMaxWidth()
                .clip(RoundedCornerShape(10.dp))
                .background(MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.55f)),
        ) {
            // header：标识 + 复制按钮（可发现性）
            Row(
                Modifier
                    .fillMaxWidth()
                    .background(MaterialTheme.colorScheme.surfaceVariant)
                    .padding(horizontal = 10.dp, vertical = 2.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    "Termux 命令",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.weight(1f),
                )
                // 紧凑型复制按钮：淡色背景胶囊（不用 TextButton，其最小高度会把 header 撑高）。
                Box(
                    Modifier
                        .background(
                            MaterialTheme.colorScheme.primary.copy(alpha = 0.12f),
                            RoundedCornerShape(8.dp),
                        )
                        .clickable(
                            interactionSource = remember { MutableInteractionSource() },
                            indication = null,
                        ) {
                            if (manual) {
                                manual = false
                            } else if (copyToClipboard()) {
                                copied = true
                            } else {
                                manual = true
                            }
                        }
                        .padding(horizontal = 10.dp, vertical = 3.dp),
                ) {
                    Text(
                        when {
                            manual -> "完成"
                            copied -> "已复制"
                            else -> "复制"
                        },
                        style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            // 代码体
            Column(Modifier.padding(horizontal = 10.dp, vertical = 8.dp)) {
                if (manual) {
                    CompositionLocalProvider(
                        LocalTextSelectionColors provides TextSelectionColors(
                            handleColor = MaterialTheme.colorScheme.primary,
                            backgroundColor = MaterialTheme.colorScheme.primary.copy(alpha = 0.30f),
                        ),
                    ) {
                        BasicTextField(
                            value = value,
                            onValueChange = { value = it },
                            readOnly = true,
                            textStyle = TextStyle(
                                fontFamily = MonoFont,
                                fontSize = 12.sp,
                                lineHeight = 17.sp,
                                color = MaterialTheme.colorScheme.onSurface,
                            ),
                            modifier = Modifier
                                .fillMaxWidth()
                                .onFocusChanged {
                                    if (it.isFocused) {
                                        everFocused = true
                                    } else if (manual && everFocused) {
                                        // 失焦即自动退出手动模式，无需点「完成」。
                                        manual = false
                                        everFocused = false
                                    }
                                }
                                .focusRequester(focusRequester),
                        )
                    }
                    Text(
                        "内容已全选：长按点「复制」，或改完点右上角「完成」退出。",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(top = 4.dp),
                    )
                } else {
                    Text(
                        text,
                        style = TextStyle(
                            fontFamily = MonoFont,
                            fontSize = 12.sp,
                            lineHeight = 17.sp,
                            color = MaterialTheme.colorScheme.onSurface,
                        ),
                        modifier = Modifier.fillMaxWidth().clickable(
                            interactionSource = remember { MutableInteractionSource() },
                            indication = null,
                        ) {
                            // 点代码块 = 变成可全选的文本框（直接复制走 header 按钮）。
                            manual = true
                        },
                    )
                }
            }
        }
        // 复制反馈只体现在 header 按钮（复制 → 已复制），不再额外加提示行。
    }
}

// dismissKeyboardOnTap / ApiKeyDialog 已移至 KeysScreen.kt（internal，同包共用）。

/** Provider 新增/编辑对话框（表单复用 ProviderEditForm）。 */
@Composable
private fun EditProviderDialog(
    initial: ProviderUi,
    isNew: Boolean,
    initialHasKey: Boolean,
    existingIds: Set<String>,
    onDismiss: () -> Unit,
    onSave: (id: String, title: String?, baseUrl: String, apiKeyEnv: String, adapter: String?, apiKey: String?) -> Unit,
) {
    AppDialog(
        onDismiss = onDismiss,
        title = if (isNew) "新增 Provider" else "编辑 Provider",
    ) {
        ProviderEditForm(
            initial = initial,
            isNew = isNew,
            initialHasKey = initialHasKey,
            existingIds = existingIds,
            onSave = onSave,
            onCancel = onDismiss,
        )
    }
}

/** Provider 编辑表单（对话框新建 / 列表行内展开编辑 共用）。 */
@Composable
private fun ProviderEditForm(
    initial: ProviderUi,
    isNew: Boolean,
    initialHasKey: Boolean,
    existingIds: Set<String>,
    onSave: (id: String, title: String?, baseUrl: String, apiKeyEnv: String, adapter: String?, apiKey: String?) -> Unit,
    onCancel: () -> Unit,
) {
    // 显示名：人类可读（中文/空格/大小写均可，随时可改）。
    // 内部 ID：新建时由软件从显示名自动生成（ascii 词 → slug；非 ascii → 随机短 ID），
    // 编辑时保持不变——模型引用、env 派生、Keystore 均按 ID 关联，改名零迁移成本。
    var title by remember { mutableStateOf(initial.title ?: initial.name) }
    var baseUrl by remember { mutableStateOf(initial.baseUrl) }
    var adapter by remember { mutableStateOf(initial.adapter ?: "openai") }
    // 密钥：默认只展示状态（已配置/未配置），点「更换/添加」才进入输入态——
    // 避免"空输入框 = 已有值"的歧义（外部 UI 审查建议）。
    var editingKey by remember { mutableStateOf(false) }
    var apiKey by remember { mutableStateOf("") }
    // 长文本输入框（URL/密钥）：未聚焦时单行省空间；聚焦后关闭 singleLine，
    // 高度随内容换行自动展开（编辑长 URL/密钥不用左右滚动找光标）。
    var urlFocused by remember { mutableStateOf(false) }
    var keyFocused by remember { mutableStateOf(false) }
    val id = if (isNew) genProviderId(title, existingIds) else initial.name
    val modified = title.trim() != (initial.title ?: initial.name).trim() ||
        baseUrl.trim() != initial.baseUrl.trim() ||
        adapter != (initial.adapter ?: "openai") ||
        (editingKey && apiKey.isNotBlank())

    Column(
        Modifier.dismissKeyboardOnTap().verticalScroll(rememberScrollState()),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        OutlinedTextField(
            title, { title = it },
            label = { Text("显示名称") },
            supportingText = { Text("显示在应用中的名称，可随时修改") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )
        OutlinedTextField(
            baseUrl, { baseUrl = it },
            label = { Text("API 地址（Base URL）") },
            singleLine = !urlFocused,
            modifier = Modifier
                .fillMaxWidth()
                .onFocusChanged { urlFocused = it.isFocused },
        )
        DropdownChoice(
            options = listOf(
                "openai" to "OpenAI 兼容",
                "anthropic" to "Anthropic 兼容",
                "gemini" to "Gemini",
                "ollama" to "Ollama",
            ),
            selected = adapter,
            onSelect = { adapter = it },
            fieldLabel = "API 协议",
            supportingText = "选择此服务使用的请求格式",
        )
        if (!editingKey) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                if (initialHasKey) {
                    Icon(
                        Icons.Filled.Check, contentDescription = null,
                        tint = MaterialTheme.colorScheme.primary,
                        modifier = Modifier.height(14.dp).width(14.dp),
                    )
                    Text(
                        "  密钥已配置",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                } else {
                    Text(
                        "密钥未配置",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
                Spacer(Modifier.weight(1f))
                SoftButton(onClick = { editingKey = true }) {
                    Text(if (initialHasKey) "更换" else "添加")
                }
            }
        } else {
            OutlinedTextField(
                apiKey, { apiKey = it },
                label = { Text("API 密钥") },
                visualTransformation = PasswordVisualTransformation(),
                singleLine = !keyFocused,
                modifier = Modifier
                    .fillMaxWidth()
                    .onFocusChanged { keyFocused = it.isFocused },
            )
            Text(
                "加密存入 Android Keystore，注入运行时内存；不写入配置文件。",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        if (!isNew && initial.apiKeyEnv.isNotBlank()) {
            Text(
                "密钥引用（高级）：${initial.apiKeyEnv}",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        // 操作行：取消靠左留白、保存为主按钮靠右；无修改时禁用。
        Row(
            horizontalArrangement = Arrangement.spacedBy(8.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Spacer(Modifier.weight(1f))
            SoftButton(onClick = onCancel) { Text("取消") }
            SoftButton(
                enabled = modified && title.isNotBlank() && baseUrl.isNotBlank(),
                onClick = {
                    val env = initial.apiKeyEnv.ifBlank {
                        id.uppercase().filter { it.isLetterOrDigit() || it == '_' }
                            .let { e -> if (e.isBlank()) "PROVIDER_API_KEY" else "${e}_API_KEY" }
                    }
                    onSave(
                        id, title.trim(), baseUrl.trim(), env, adapter.ifBlank { null },
                        if (editingKey && apiKey.isNotBlank()) apiKey.trim() else null,
                    )
                },
            ) { Text("保存") }
        }
    }
}

/**
 * 从显示名生成内部 ID：小写 ascii 词以 `-` 连接（"Zhipu AI" → "zhipu-ai"）；
 * 无 ascii 词（如纯中文）或为空 → `p-<随机>`；与现有 ID 冲突时追加随机后缀。
 * ID 仅软件内部使用（模型引用 / env 派生 / Keystore 键），不要求人类可读。
 */
private fun genProviderId(title: String, existing: Set<String>): String {
    val chars = ('a'..'f') + ('0'..'9')
    fun rand(n: Int) = List(n) { chars.random() }.joinToString("")
    val slug = title.trim().lowercase()
        .split(Regex("[^a-z0-9]+"))
        .filter { it.isNotEmpty() }
        .joinToString("-")
        .take(24)
        .trim('-')
    var id = slug.ifEmpty { "p-${rand(5)}" }
    if (id in existing) id = "${slug.ifEmpty { "p" }}-${rand(3)}"
    return id
}

/** 选项不多的下拉（M3 ExposedDropdownMenu）：点击整个框直接弹出菜单，
 *  纯选择器语义——无文本编辑态、无焦点，选中项带 ✓。选项为 (协议值, 显示标签)。 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DropdownChoice(
    options: List<Pair<String, String>>,
    selected: String,
    onSelect: (String) -> Unit,
    fieldLabel: String,
    supportingText: String? = null,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = options.firstOrNull { it.first == selected }?.second ?: selected
    ExposedDropdownMenuBox(expanded = expanded, onExpandedChange = { expanded = it }) {
        OutlinedTextField(
            value = selectedLabel,
            onValueChange = {},
            readOnly = true,
            label = { Text(fieldLabel) },
            supportingText = supportingText?.let { s -> { Text(s) } },
            trailingIcon = { ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded) },
            modifier = Modifier
                .fillMaxWidth()
                .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryNotEditable),
        )
        ExposedDropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            for ((value, label) in options) {
                DropdownMenuItem(
                    text = { Text(label) },
                    onClick = {
                        onSelect(value)
                        expanded = false
                    },
                    trailingIcon = if (value == selected) {
                        { Icon(Icons.Filled.Check, contentDescription = null) }
                    } else {
                        null
                    },
                )
            }
        }
    }
}

@Composable
private fun EditModelDialog(
    initial: ModelUi,
    isNew: Boolean,
    providers: List<Pair<String, String>>, // (显示名, 内部 ID)
    lockProvider: String, // 归属 Provider 内部 ID（Provider 页模型管理进入时锁定，归属不可跨 provider 改）
    onDismiss: () -> Unit,
    onSave: (ModelUi) -> String?, // null = 已保存（调用方关弹窗）；非 null = 错误提示，弹窗保持打开
) {
    var entry by remember { mutableStateOf(initial.entry) }
    // Provider 归属锁定：条目归属不可在此修改（levels 别名自由路径已随
    // 映射层一并移除，模型条目一律挂在 Provider 下管理）。
    val provider = lockProvider
    var modelId by remember { mutableStateOf(initial.modelId) }
    var showAdvanced by remember { mutableStateOf(false) }
    var saveError by remember { mutableStateOf<String?>(null) }
    var toolCall by remember { mutableStateOf(initial.supportsToolCall) }
    var vision by remember { mutableStateOf(initial.supportsVision) }
    var reasoning by remember { mutableStateOf(initial.supportsReasoning) }
    var maxCtx by remember { mutableStateOf(initial.maxContextTokens?.toString() ?: "") }
    var maxOut by remember { mutableStateOf(initial.maxOutputTokens?.toString() ?: "") }
    var priceIn by remember { mutableStateOf(initial.inputPricePerMtok?.toString() ?: "") }
    var priceOut by remember { mutableStateOf(initial.outputPricePerMtok?.toString() ?: "") }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (isNew) "新增模型" else "编辑模型") },
        text = {
            Column(Modifier.dismissKeyboardOnTap().verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    entry,
                    { entry = it },
                    label = { Text("条目 ID（留空自动生成）") },
                    enabled = isNew,
                )
                // 归属锁定，仅展示。
                Text(
                    "Provider：${providers.find { it.second == lockProvider }?.first ?: lockProvider}",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                OutlinedTextField(modelId, { modelId = it }, label = { Text("模型 ID（如 glm-4.7）") })

                // 从 registry 补全元数据（本地数据源优先，miss 在线兜底；
                // 只填充空白字段，已填的不覆盖）。
                if (modelId.isNotBlank()) {
                    val fillScope = androidx.compose.runtime.rememberCoroutineScope()
                    SoftButton(onClick = {
                        fillScope.launch(kotlinx.coroutines.Dispatchers.IO) {
                            val meta = RuntimeBridge.lookupModelMeta(modelId.trim())
                            kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.Main) {
                                if (meta == null || !meta.hasAny) return@withContext
                                if (maxCtx.isBlank()) meta.contextTokens?.let { maxCtx = it.toString() }
                                if (maxOut.isBlank()) meta.maxOutputTokens?.let { maxOut = it.toString() }
                                if (priceIn.isBlank()) meta.inputPricePerMtok?.let { priceIn = it.toString() }
                                if (priceOut.isBlank()) meta.outputPricePerMtok?.let { priceOut = it.toString() }
                                meta.supportsVision?.let { vision = it }
                                meta.supportsReasoning?.let { reasoning = it }
                                meta.supportsToolCall?.let { toolCall = it }
                                showAdvanced = true
                            }
                        }
                    }) { Text("从 registry 补全") }
                }

                SoftButton(onClick = { showAdvanced = !showAdvanced }) {
                    Text(if (showAdvanced) "收起高级选项" else "高级选项（能力 / 限额 / 计价）")
                    Icon(
                        if (showAdvanced) Icons.Filled.KeyboardArrowUp else Icons.Filled.KeyboardArrowDown,
                        contentDescription = null,
                        modifier = Modifier.height(16.dp).width(16.dp),
                    )
                }
                if (showAdvanced) {
                    Text("能力开关", style = MaterialTheme.typography.labelMedium)
                    Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        Toggle("工具调用", toolCall) { toolCall = !toolCall }
                        Toggle("视觉", vision) { vision = !vision }
                        Toggle("推理", reasoning) { reasoning = !reasoning }
                    }
                    OutlinedTextField(maxCtx, { maxCtx = it.filter { c -> c.isDigit() } }, label = { Text("最大上下文 tokens（空=默认）") })
                    OutlinedTextField(maxOut, { maxOut = it.filter { c -> c.isDigit() } }, label = { Text("最大输出 tokens（空=默认）") })
                    OutlinedTextField(priceIn, { priceIn = it }, label = { Text("输入单价 USD/Mtok（空=0）") })
                    OutlinedTextField(priceOut, { priceOut = it }, label = { Text("输出单价 USD/Mtok（空=0）") })
                }
                // 保存被拒的原因（如查重失败），内联展示、弹窗不关。
                if (saveError != null) {
                    Text(
                        saveError ?: "",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
            }
        },
        confirmButton = {
            SoftButton(
                onClick = {
                    if (provider.isNotBlank() && modelId.isNotBlank()) {
                        saveError = onSave(
                            initial.copy(
                                entry = entry.trim(),
                                provider = provider,
                                modelId = modelId.trim(),
                                supportsToolCall = toolCall,
                                supportsVision = vision,
                                supportsReasoning = reasoning,
                                maxContextTokens = maxCtx.toLongOrNull(),
                                maxOutputTokens = maxOut.toLongOrNull(),
                                inputPricePerMtok = priceIn.toDoubleOrNull(),
                                outputPricePerMtok = priceOut.toDoubleOrNull(),
                            )
                        )
                    }
                },
            ) { Text("保存") }
        },
        dismissButton = { SoftButton(onClick = onDismiss) { Text("取消") } },
    )
}

@Composable
private fun Toggle(label: String, value: Boolean, onToggle: () -> Unit) {
    TextButton(onClick = onToggle) {
        Text((if (value) "● " else "○ ") + label)
    }
}

/** 方形复选框行（多选场景；Checkbox 圆形默认改为方角以区分单选语义）。
 *  subtitle 非空时在标题下加灰色小字说明行（labelSmall + outline，与
 *  区块内其他说明小字同款）。 */
@Composable
private fun CheckboxRow(
    label: String,
    checked: Boolean,
    onChange: (Boolean) -> Unit,
    subtitle: String? = null,
) {
    Row(
        Modifier
            .fillMaxWidth()
            .clickable { onChange(!checked) },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        androidx.compose.material3.Checkbox(
            checked = checked,
            onCheckedChange = onChange,
        )
        if (subtitle == null) {
            Text(label, style = MaterialTheme.typography.bodyMedium)
        } else {
            Column(Modifier.weight(1f)) {
                Text(label, style = MaterialTheme.typography.bodyMedium)
                Text(
                    subtitle,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        }
    }
}

/** 跳到本 App 的系统应用详情页（手动授予权限的兜底入口）。 */
private fun openOwnAppSettings(context: android.content.Context) {
    context.startActivity(
        android.content.Intent(
            android.provider.Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
            android.net.Uri.parse("package:" + context.packageName),
        ),
    )
}

/** 紧凑圆角按钮：淡色背景 + 深色文字 + 小上下留白（工具页统一风格）。
 *  internal：SettingsScreen 与 ToolManagementSection（模板小节）共用。 */
@Composable
internal fun CompactButton(
    text: String,
    enabled: Boolean = true,
    container: androidx.compose.ui.graphics.Color =
        MaterialTheme.colorScheme.surfaceVariant,
    content: androidx.compose.ui.graphics.Color =
        MaterialTheme.colorScheme.onSurfaceVariant,
    onClick: () -> Unit,
) {
    Button(
        onClick = onClick,
        enabled = enabled,
        shape = RoundedCornerShape(10.dp),
        contentPadding = PaddingValues(horizontal = 14.dp, vertical = 4.dp),
        colors = ButtonDefaults.buttonColors(containerColor = container, contentColor = content),
        modifier = Modifier.heightIn(min = 34.dp),
    ) {
        Text(text, style = MaterialTheme.typography.labelMedium)
    }
}

/** 状态行：成功 → 实心对勾圆（绿）；未就绪 → 实心红叉圆（error 色）。 */
@Composable
private fun McpStatusLine(ok: Boolean, text: String) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Icon(
            imageVector = if (ok) Icons.Filled.CheckCircle else Icons.Filled.Cancel,
            contentDescription = if (ok) "已就绪" else "未就绪",
            tint = if (ok) successGreen()
            else MaterialTheme.colorScheme.error,
            modifier = Modifier.height(14.dp).width(14.dp),
        )
        Text("  $text", style = MaterialTheme.typography.labelMedium)
    }
}

/**
 * bash 工具执行后端（独立运行模式，多选）。
 *
 * - native：进程内 Android 系统 sh（toybox），零依赖零授权。
 * - termux：Termux RUN_COMMAND，完整 Linux 环境。
 *
 * 命名（Rust 侧注册）：单选 → 工具名 `bash`；双选 → `bash`（native）+
 * `termux_bash`（termux）。
 */
@Composable
private fun ExecBackendSection() {
    val context = androidx.compose.ui.platform.LocalContext.current
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    // prefs 读取预取到 IO 线程（组合期同步读盘会卡帧）；null = 加载中。
    var backends by remember { mutableStateOf<String?>(null) }
    LaunchedEffect(Unit) {
        backends = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            RuntimeBridge.execBackends()
        }
    }
    fun isEnabled(name: String) =
        backends?.split(',')?.map { it.trim() }?.contains(name) == true
    /** 允许全不选（bash 功能整体下线）；空集持久化为空串。 */
    fun setEnabled(name: String, on: Boolean) {
        val set = linkedSetOf<String>()
        if (isEnabled("native") || name == "native" && on) set.add("native")
        if (isEnabled("termux") || name == "termux" && on) set.add("termux")
        if (name == "native" && !on) set.remove("native")
        if (name == "termux" && !on) set.remove("termux")
        val csv = set.joinToString(",")
        // 写操作（FFI 热切换 + prefs 落盘）包 IO 协程；完成后回主线程更新 UI。
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            RuntimeBridge.setExecBackendsAndPersist(csv)
            kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.Main) { backends = csv }
        }
    }
    val noneSelected = !isEnabled("native") && !isEnabled("termux")
    var hasPerm by remember {
        mutableStateOf(dev.openslate.mobile.bridge.TermuxExec.hasPermission(context))
    }
    var grantMsg by remember { mutableStateOf<String?>(null) }
    var granting by remember { mutableStateOf(false) }

    // 从系统设置授予/撤销权限后返回时自动刷新状态。
    val lifecycleOwner = androidx.compose.ui.platform.LocalLifecycleOwner.current
    androidx.compose.runtime.DisposableEffect(lifecycleOwner) {
        val obs = androidx.lifecycle.LifecycleEventObserver { _, event ->
            if (event == androidx.lifecycle.Lifecycle.Event.ON_RESUME) {
                hasPerm = dev.openslate.mobile.bridge.TermuxExec.hasPermission(context)
            }
        }
        lifecycleOwner.lifecycle.addObserver(obs)
        onDispose { lifecycleOwner.lifecycle.removeObserver(obs) }
    }

    Section(title = "bash 工具执行后端（可多选）") {
        if (backends == null) {
            // 预取进行中：占位一行，避免加载间隙闪"未选择任何后端"红色告警。
            Text("加载中…", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.outline)
        } else {
            CheckboxRow(
                label = "Native",
                subtitle = "Android 自带系统 shell（toybox 子集，零依赖零授权）",
                checked = isEnabled("native"),
                onChange = { setEnabled("native", it) },
            )
            CheckboxRow(
                label = "Termux",
                subtitle = "Termux 完整 Linux 环境（apt/python/…）",
                checked = isEnabled("termux"),
                onChange = { setEnabled("termux", it) },
            )
            when {
                noneSelected -> Row(verticalAlignment = Alignment.CenterVertically) {
                    Icon(
                        imageVector = Icons.Outlined.Warning,
                        contentDescription = "警告",
                        tint = MaterialTheme.colorScheme.error,
                        modifier = Modifier.height(14.dp).width(14.dp),
                    )
                    Text(
                        "  未选择任何后端：模型将没有 bash 工具，无法执行任何命令。",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                }
                isEnabled("native") && isEnabled("termux") -> Text(
                    "已多选：native 工具名为 bash，Termux 工具名为 termux_bash。" +
                        "MCP 服务在线时 Termux 命令经 MCP 执行（host 内置 bash），未在线走 RUN_COMMAND 兜底。",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                isEnabled("termux") -> Text(
                    "仅 termux：工具名为 bash（Termux 后端）。MCP 服务在线时命令走 MCP 路径，" +
                        "未在线时以 RUN_COMMAND Intent 兜底（完整环境需在 ~/.termux/termux.properties " +
                        "设置 allow-external-apps=true）。",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                else -> Text(
                    "仅 native：工具名为 bash，零依赖、零授权，无需 Termux（MCP 服务不影响该路径）。",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }

            if (isEnabled("termux")) {
                val installed = dev.openslate.mobile.bridge.TermuxExec.isTermuxInstalled(context)
                Text(
                    if (installed) "Termux：已安装" else "Termux：未安装（请先安装 Termux）",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                Text(
                    if (hasPerm) "RUN_COMMAND 权限：已授予" else "RUN_COMMAND 权限：未授予",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                // 部分 ROM（MIUI/HyperOS 等）在 Termux 从未启动时无法完成
                // 授予：提示先打开一次 Termux（PackageManager 查询轻量，同步调用）。
                if (!hasPerm && installed &&
                    dev.openslate.mobile.bridge.TermuxDiagnostics.isTermuxStopped(context)
                ) {
                    Text(
                        "Termux 尚未启动过（或被强制停止）。部分系统此状态下无法授予该权限：" +
                            "先打开一次 Termux，返回后再点「申请权限」；或直接用 Shizuku 一键授权。",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (!hasPerm) {
                        CompactButton(text = "申请权限", onClick = {
                            val activity = context as? android.app.Activity
                            if (activity != null) {
                                activity.requestPermissions(
                                    arrayOf(dev.openslate.mobile.bridge.TermuxExec.RUN_COMMAND_PERMISSION),
                                    1002,
                                )
                            } else {
                                openOwnAppSettings(context)
                            }
                        })
                        CompactButton(text = "打开应用设置", onClick = { openOwnAppSettings(context) })
                        CompactButton(text = "申请 Shizuku 授权", onClick = {
                            dev.openslate.mobile.bridge.ShizukuGrant.requestShizukuPermission()
                        })
                        CompactButton(text = "Shizuku 一键授权", enabled = !granting, onClick = {
                            granting = true
                            grantMsg = null
                            scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                                val err = dev.openslate.mobile.bridge.ShizukuGrant
                                    .grantTermuxPermission(context)
                                kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.Main) {
                                    granting = false
                                    hasPerm = dev.openslate.mobile.bridge.TermuxExec.hasPermission(context)
                                    grantMsg = err ?: "授权成功"
                                }
                            }
                        })
                    }
                }
                grantMsg?.let {
                    Text(it, style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.outline)
                }
            }
        }
    }
}

/** MCP 域状态快照（RUN_COMMAND / 端口探测）。 */
private data class McpStatus(
    val probing: Boolean = true,
    val termuxInstalled: Boolean = false,
    val termuxPerm: Boolean = false,
    /** = channel is TermuxDiagnostics.ChannelResult.Ok。 */
    val termuxAllowed: Boolean = false,
    /** RUN_COMMAND 通道精确诊断结果（probing / 未探测短路时为 null）。 */
    val channel: dev.openslate.mobile.bridge.TermuxDiagnostics.ChannelResult? = null,
    val hostInstalled: Boolean = false,
    val hostAlive: Boolean = false,
    /** Termux 中不存在的 server 启动命令首 token；探测失败/通道未放行为
     *  空集（= 不警示，避免探测间隙行内警示闪烁）。 */
    val missingCommands: Set<String> = emptySet(),
    /** 注册表内外部 MCP 工具按 host 前缀（slug）分组的计数（每服务器
     *  运行态的判定依据：agent 真实可见，比 host 侧探针更诚实）：
     *  null = runtime 未就绪/未获取（状态未知），空 map / 缺某 slug =
     *  该 server 的工具未注册（未起来，或起来但没注册 → 差异自愈会
     *  尝试重连一次）。 */
    val hostTools: Map<String, Int>? = null,
    /** 本轮检测已完成的项目（渐进渲染：检测完一项就显示一项；五个灯的 key）。 */
    val checked: Set<String> = emptySet(),
)

/** 五个状态灯的 key（渐进渲染的判定 + 摘要行进度统计共用；整轮结束时
 *  短路路径未检测的项也一次性补齐，保证五灯语义完整）。 */
private val MCP_LIGHT_KEYS = listOf(
    "termuxInstalled", "termuxPerm", "termuxAllowed", "hostInstalled", "hostAlive",
)

/**
 * MCP 状态的进程级快照：全量重检完成后写入，进页/切回先用快照渲染
 * （无快照才显示「检测中」），配合门控探测避免每次进页都全量重检。
 * 只缓存 [McpStatus]；servers 清单从 prefs 本地读零成本，不进缓存。
 */
private object McpStatusCache {
    @Volatile var snapshot: McpStatus? = null
    /** 快照完成时刻（门控翻转时用于日志展示快照保持了多久）。 */
    @Volatile var savedAtMs: Long = 0L

    fun update(s: McpStatus) {
        snapshot = s
        savedAtMs = System.currentTimeMillis()
    }
}

/** App 写入 openslate.toml 的 MCP server 名单：单键名，可经设置页改名
 *  （默认 termux）。计算属性透传 RuntimeBridge 缓存——改名保存后状态页
 *  聚合即时跟随，无快照滞后。 */
// 单一事实源在 RuntimeBridge（ToolManagementSection 聚合与状态页共用）。
private val MCP_SERVER_NAMES: List<String> get() = RuntimeBridge.MCP_SERVER_NAMES

/**
 * 差异自愈的防抖（进程级）：reload 在途 + 上次完成时刻（30s 内不重复
 * 触发）。inFlight 在 finally 里清除——页面离开取消协程也不会把自愈
 * 卡死在「在途」态。
 */
private object McpSelfHealGate {
    @Volatile var inFlight: Boolean = false
    @Volatile var lastDoneAtMs: Long = 0L
}

/**
 * MCP 域：Termux 侧 mcp-host 安装/清单/状态。
 *
 * - "自动化操作"（前置三绿后出现）：assets 内嵌的静态 host 经本机回环
 *   TCP 装进 Termux，写清单、启动、并把 [mcp.servers.termux] 写入
 *   openslate.toml；
 * - server 清单：App 侧管理，变更即推送清单并重启 host；
 *   工具名前缀 `slug__`（slug 由显示名或用户指定代号派生，
 *   见 McpHostManager.entrySlug），模型看到 `termux_fs__list_directory`。
 */
@Composable
/** 日志导出：复制本应用 logcat（三档时间范围），反馈问题时直接粘贴。 */
private fun LogSection() {
    val context = androidx.compose.ui.platform.LocalContext.current
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    var busy by remember { mutableStateOf(false) }
    var status by remember { mutableStateOf<String?>(null) }

    fun copy(range: LogExporter.Range, label: String) {
        if (busy) return
        busy = true
        status = "正在读取日志…"
        scope.launch {
            runCatching { LogExporter.collect(context, range) }.fold(
                onSuccess = { text ->
                    val clipboard = context.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                        as android.content.ClipboardManager
                    clipboard.setPrimaryClip(
                        android.content.ClipData.newPlainText("OpenSlate 日志", text)
                    )
                    status = "$label：已复制 ${text.lines().size} 行（${text.length / 1024}KB）"
                },
                onFailure = { status = "$label 失败：${it.message}" },
            )
            busy = false
        }
    }

    Section(title = "日志") {
        Text(
            "复制本应用的运行日志（含 MCP 诊断、核心库输出），反馈问题时直接粘贴。" +
                "受系统限制仅包含本应用自身的日志。",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            SoftButton(enabled = !busy, onClick = { copy(LogExporter.Range.SINCE_START, "启动以来") }) {
                Text("启动以来")
            }
            SoftButton(enabled = !busy, onClick = { copy(LogExporter.Range.LAST_HOUR, "最近 1 小时") }) {
                Text("最近 1 小时")
            }
            SoftButton(enabled = !busy, onClick = { copy(LogExporter.Range.LAST_DAY, "最近 24 小时") }) {
                Text("最近 24 小时")
            }
        }
        status?.let {
            // 系统等宽（不换 MonoFont 子集）：状态行混排中文与动态错误信息。
            Text(it, style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace, color = MaterialTheme.colorScheme.outline)
        }
    }
}

@Composable
private fun McpSection() {
    val context = androidx.compose.ui.platform.LocalContext.current
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    // MCP 按钮主题色（primary 12% 底 + primary 文字）：与「必填」徽标/
    // SoftButton 同款惯例，轻量不压卡片内容；disabled 态由 M3 按钮的
    // 默认弱化色承载（buttonColors 只覆盖 enabled 色），可读。
    val mcpBtnContainer = MaterialTheme.colorScheme.primary.copy(alpha = 0.12f)
    val mcpBtnContent = MaterialTheme.colorScheme.primary
    // 进页先渲染进程级快照（如有）：配合门控避免每次进页都从「检测中」
    // 全量重来；无快照（冷启动首次）才从 probing 态开始。
    var status by remember { mutableStateOf(McpStatusCache.snapshot ?: McpStatus()) }
    // server 清单（prefs JSON）预取到 IO 线程，避免组合期主线程读盘。
    var servers by remember { mutableStateOf<List<McpHostManager.McpServerEntry>>(emptyList()) }
    LaunchedEffect(Unit) {
        servers = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            McpHostManager.loadServers(context)
        }
    }
    // ── 诊断日志（多行、带时间戳）+ 当前步骤实时计时 ──────────────
    var logLines by remember { mutableStateOf<List<String>>(emptyList()) }
    var currentStep by remember { mutableStateOf<String?>(null) }
    var stepElapsedSec by remember { mutableStateOf(0) }
    val logFmt = remember { java.text.SimpleDateFormat("HH:mm:ss", java.util.Locale.US) }
    fun log(msg: String) {
        logLines = (logLines + "[${logFmt.format(java.util.Date())}] $msg").takeLast(300)
        // 同步镜像到 logcat：日志导出（LogExporter）走 logcat，不镜像就丢。
        android.util.Log.i("OpenSlateDiag", msg)
    }

    /** 步骤进度：name 非空 = 进入该步骤（计时开始）；null = 结束。 */
    fun step(name: String?) {
        currentStep = name
        stepElapsedSec = 0
    }

    // 秒级跳动：步骤进行中每秒 +1，驱动"已进行 Xs"重绘（卡住时可见
    // 停在哪一步、停了多久）。
    androidx.compose.runtime.LaunchedEffect(currentStep) {
        while (currentStep != null) {
            kotlinx.coroutines.delay(1000)
            stepElapsedSec += 1
        }
    }
    var busy by remember { mutableStateOf(false) }
    // 全量重检的在途 Job：新触发取消旧在途（最新优先，不排队堆积）。
    // 手动刷新/applyServers 收尾/门控触发的重检都经它收敛——「取消顶替」
    // 而非「在途忽略」是刻意的：applyServers 的收尾重检必须保证能起一轮
    // 新的（配置刚变过），纯「在途忽略」会把它吞掉。
    var refreshJob by remember { mutableStateOf<kotlinx.coroutines.Job?>(null) }
    // 添加/编辑对话框状态：first = 条目在 servers 中的索引（-1 = 添加模式），
    // second = 预填的 (name, slug, command)。编辑保存按索引定位替换——不用
    // 条目相等性查找，避免同内容条目（data class 相等）误替换到另一个。
    var editServer by remember { mutableStateOf<Pair<Int, Triple<String, String?, String>>?>(null) }

    /**
     * 单发差异自愈（替代旧的定时重试轮询——用户否了周期性全量刷新：
     * 运行链路本就有 core↔host 持久会话与热重载，轮询是状态面板自己
     * 开的平行通道）。host 在跑、server 启动命令在 Termux 存在（缺失
     * 是终态，重连无意义），但注册表里没有该 server 的工具（agent 看
     * 不见）→ 调一次 reloadMcpServers 重连；成功后从注册表重读 slug 级
     * 计数刷新状态 + 快照。失败只记一行——不重试、不循环：瞬时态
     * （host 掉线）由门控翻转检测覆盖，重连失败靠下次门控/手动刷新
     * 自然再试（30s 防抖防连发）。
     */
    suspend fun maybeSelfHeal() {
        if (McpSelfHealGate.inFlight) return
        if (System.currentTimeMillis() - McpSelfHealGate.lastDoneAtMs < 30_000) return
        val s = status
        if (s.probing || !s.hostAlive || !s.hostInstalled) return
        // 现读注册表而非用快照（快照可能滞后于刚完成的重连/刷新）；
        // runtime 未就绪 → 无法判定，不动。
        val reg = RuntimeBridge.mcpRegistryToolsByPrefix(MCP_SERVER_NAMES) ?: return
        val missing = McpHostManager.serversSlugMap(servers).mapNotNull { (entry, slug) ->
            val cmdToken = entry.command.trim().split(Regex("\\s+")).first()
            when {
                cmdToken in s.missingCommands -> null // 终态不自愈（行内已有警示指引）
                slug !in reg -> slug
                else -> null
            }
        }
        if (missing.isEmpty()) return
        McpSelfHealGate.inFlight = true
        log("检测到 ${missing.joinToString("、")} 工具未注册，自动重连…")
        try {
            val result = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
            val obj = result?.let { runCatching { JSONObject(it) }.getOrNull() }
            if (obj != null && obj.optBoolean("ok", false)) {
                // reload 响应的 servers 计数按配置 server 名聚合（键是
                // "termux"，不是 slug），hostTools 必须从注册表重读。
                val reg2 = RuntimeBridge.mcpRegistryToolsByPrefix(MCP_SERVER_NAMES)
                val updated = status.copy(hostTools = reg2)
                status = updated
                McpStatusCache.update(updated)
                val n = obj.optInt("tools", -1).takeIf { it >= 0 }
                    ?: reg2?.values?.sum()
                    ?: 0
                log("重连成功：$n 个工具")
            } else {
                val err = obj?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                    ?: result?.take(120)
                    ?: "runtime 未就绪"
                log("自动重连失败：$err")
            }
        } finally {
            McpSelfHealGate.inFlight = false
            McpSelfHealGate.lastDoneAtMs = System.currentTimeMillis()
        }
    }

    /**
     * 全量重检：只在用户想看全貌的时机跑（手动刷新 / 首载 / 门控翻转 /
     * applyServers 收尾）；自动路径只有门控的毫秒级探测与差异检查，不
     * 产生任何轮询。返回本轮 Job（在途判定用）。
     */
    fun refresh(): kotlinx.coroutines.Job {
        refreshJob?.cancel() // 最新触发优先：取消在途轮而非排队
        val job = scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            val t0 = System.currentTimeMillis()
            log("── 状态检查开始 ──")
            step("检查 Termux / 权限")
            val s = McpStatus(probing = false,
                termuxInstalled = dev.openslate.mobile.bridge.TermuxExec.isTermuxInstalled(context),
                termuxPerm = dev.openslate.mobile.bridge.TermuxExec.hasPermission(context),
                termuxAllowed = false, hostInstalled = false, hostAlive = false)
            log("Termux 已安装：${s.termuxInstalled}；RUN_COMMAND 权限：${s.termuxPerm}")
            ensureActive() // 被更新的触发取消 → 立即退出，不写状态
            // 渐进提交：每步完成即写回，检测完一项显示一项（灯区块只
            // 渲染已完成的项），不再等整轮结束才统一落地。probing 保持
            // true 到整轮结束——期间摘要行显示进度而非结论。
            status = s.copy(probing = true, checked = setOf("termuxInstalled", "termuxPerm"))
            var full = s
            var alive = false
            var missing = emptySet<String>()
            var tools: Map<String, Int>? = null
            if (s.termuxInstalled && s.termuxPerm) {
                kotlinx.coroutines.coroutineScope {
                    // 端口探活走独立 HTTP 通道，先并行发出（不占 Termux 通道）。
                    val aliveD = async { McpHostManager.hostAliveDetail(context) }

                    // ── 步骤 1：RUN_COMMAND 通道（echo ok 往返 + 精确诊断）──
                    // 独立成步：卡在这即通道问题（Termux 冷启动 / 被冻结解冻 / 放行未开）。
                    step("检测 RUN_COMMAND 通道（echo ok）")
                    val watcher = launch {
                        kotlinx.coroutines.delay(5000)
                        log("仍在等待 RUN_COMMAND 应答…（Termux 冷启动或被系统冻结后首次命令较慢；" +
                            "常开 Termux 通知常驻 + Wakelock 并对其关闭电池优化可避免）")
                    }
                    val chan = TermuxDiagnostics.ensureTermuxUp(context, log = { log(it) }, onStall = {
                        // 0.5s 无应答：不等整轮，先亮「打开 Termux」指引（复用
                        // NoResponse 渲染路径）。这是早写的显示态，refresh 协程
                        // 内部流程不受影响（allowed 来自返回值 chan）；整轮结束
                        // 后 2207 行处用真实结论覆盖（重试通过则变绿）。
                        log("RUN_COMMAND 已 ${TermuxDiagnostics.PROBE_STALL_HINT_MS}ms 无应答——先显示「打开 Termux」指引，检测仍在后台继续…")
                        status = status.copy(
                            channel = TermuxDiagnostics.ChannelResult.NoResponse(
                                stopped = TermuxDiagnostics.isTermuxStopped(context), ms = 0,
                            ),
                            termuxAllowed = false,
                            checked = status.checked + "termuxAllowed",
                        )
                    })
                    watcher.cancel()
                    log("RUN_COMMAND 通道最终结果：${TermuxDiagnostics.describe(chan)}")
                    val allowed = chan is TermuxDiagnostics.ChannelResult.Ok
                    // 前台拉起可能让部分 ROM（MIUI 等）补全 RUN_COMMAND 授权：复查一次。
                    val permNow = dev.openslate.mobile.bridge.TermuxExec.hasPermission(context)
                    if (!s.termuxPerm && permNow) {
                        log("Termux 启动后权限检查结果已更新：未授予 → 已授予")
                    }
                    ensureActive() // 取消轮不写中间态
                    status = status.copy(termuxPerm = permNow, termuxAllowed = allowed, channel = chan,
                        checked = status.checked + "termuxAllowed")

                    // ── 步骤 2：host 安装（通道已热，秒回）──
                    val inst = if (allowed) {
                        step("检查 host 安装")
                        McpHostManager.isHostInstalled(context)
                    } else false
                    log("host 已安装：$inst")
                    ensureActive() // 取消轮不写中间态
                    status = status.copy(hostInstalled = inst, checked = status.checked + "hostInstalled")

                    // ── 步骤 2.5：server 启动命令探测（通道放行才有意义）──
                    // 首 token 在 Termux 缺失（如未装 npx）的 server 必然无法
                    // 启动，行内直接警示 + 安装指引；通道未放行不探测（五灯
                    // 状态已引导修通道，不叠加命令级警示），探测失败返回空集
                    // 不误报。切分逻辑与 manifestToml 一致（首 token = 可执行名）。
                    if (allowed && servers.isNotEmpty()) {
                        step("探测 server 命令可用性")
                        val firstTokens = servers.map { it.command.trim().split(Regex("\\s+")).first() }
                        missing = McpHostManager.probeCommandAvailable(context, firstTokens)
                        if (missing.isNotEmpty()) {
                            log("Termux 缺少命令：${missing.joinToString("、")}（对应服务器无法启动，见行内提示）")
                        }
                        ensureActive() // 取消轮不写中间态
                        status = status.copy(missingCommands = missing)
                    }

                    // ── 步骤 3：端口探活（并行已发出，此时收结果）──
                    step("探活服务端口")
                    val (a, adetail) = aliveD.await()
                    alive = a
                    log("探活 ${McpHostManager.HOST_URL}：${if (a) "应答正常（$adetail）" else "无应答（$adetail）"}")
                    ensureActive() // 取消轮不写中间态
                    status = status.copy(hostAlive = alive, checked = status.checked + "hostAlive")

                    // ── 步骤 4：host 已装但未运行 → 自动拉起（无需点重装）──
                    if (allowed && inst && !alive && !busy) {
                        step("自动拉起 MCP 服务")
                        log("服务未运行，自动拉起（停止旧进程 → nohup 后台启动）…")
                        runCatching { McpHostManager.restartHost(context) { log(it) } }
                            .onSuccess {
                                val (a2, d2) = McpHostManager.hostAliveDetail(context)
                                alive = a2
                                ensureActive() // 取消轮不写中间态
                                status = status.copy(hostAlive = alive)
                                log(
                                    if (a2) "服务已自动恢复（$d2），无需重装"
                                    else "自动拉起后仍未应答（$d2）——可点「自动化操作（重新安装）」修复",
                                )
                            }
                            .onFailure {
                                log("自动拉起失败：${it.message?.take(160)}——可点「自动化操作（重新安装）」修复")
                            }
                    }
                    // ── 步骤 5：工具可见性。状态行的判定源 = 注册表计数
                    // （agent 真实可见，比 host 侧探针更诚实）；host 侧 HTTP
                    // 探针（hostToolsByPrefix）降级为深度诊断——区分「server
                    // 没起来」vs「host 有但未注册」，只在手动全量时跑一次。
                    if (alive) {
                        step("获取 host 工具列表")
                        val reg = RuntimeBridge.mcpRegistryToolsByPrefix(MCP_SERVER_NAMES)
                        tools = reg
                        ensureActive() // 取消轮不写中间态
                        status = status.copy(hostTools = reg)
                        log("注册表工具：" + (reg?.entries?.joinToString("、") { "${it.key}(${it.value}个)" }?.ifEmpty { "无" }
                            ?: "runtime 未就绪（状态未知）"))
                        val hostSide = McpHostManager.hostToolsByPrefix(context)
                        log("host 侧探针：" + (hostSide?.entries?.joinToString("、") { "${it.key}(${it.value}个)" }?.ifEmpty { "无" }
                            ?: "获取失败"))
                    }
                    full = s.copy(
                        termuxAllowed = allowed, channel = chan, hostInstalled = inst,
                        termuxPerm = permNow, missingCommands = missing, hostTools = tools,
                    )
                }
            } else {
                step("探活服务端口")
                val (a, adetail) = McpHostManager.hostAliveDetail(context)
                alive = a
                log("探活 ${McpHostManager.HOST_URL}：${if (a) "应答正常（$adetail）" else "无应答（$adetail）"}")
                ensureActive() // 取消轮不写中间态
                status = status.copy(hostAlive = alive, checked = status.checked + "hostAlive")
            }
            ensureActive() // 取消轮不落地：不覆盖更新一轮已写的新状态/快照
            // 五灯 key 一次性补齐：短路路径某些检测没跑，最终落地补全五灯
            // 语义（与改造前的最终态一致）。
            val final = full.copy(hostAlive = alive, checked = MCP_LIGHT_KEYS.toSet())
            status = final
            McpStatusCache.update(final)
            step(null)
            log("── 状态检查完成（总耗时 ${System.currentTimeMillis() - t0}ms）──")
            // 收尾差异自愈：注册表缺工具时单发重连（30s 防抖，不循环）。
            maybeSelfHeal()
        }
        refreshJob = job
        return job
    }
    /**
     * 进页 / ON_RESUME 的智能门控：
     * - 无快照（首次进页）→ 全量重检；
     * - 快照非全绿（上次有未通过项）→ 直接全量重检：典型场景是用户按
     *   指引打开 Termux 后返回——Termux 刚复活时 host 端口必然仍无应答
     *   （host 还没人重启），host-only 廉价比对看不见「Termux 已活」这个
     *   变化，会误判无变化而跳过重检、逼用户手动刷新；全量重检的步骤 4
     *   会顺带把 host 自动拉起，一步到位；
     * - 全绿快照 → 只做廉价变化探测——本机回环 HTTP（~ms 级，不碰
     *   Termux 通道），与快照 hostAlive 比对：一致 → 无感差异检查
     *   （注册表缺工具才单发自愈，健康时零日志零动作）；翻转 → 全量重检。
     * 廉价路径全程不置 probing（快照继续渲染）；全量路径由 refresh 自带的
     * 渐进提交接管（检测中 · 已完成 n/5）。
     */
    fun gateRefresh() {
        val snap = McpStatusCache.snapshot
        if (snap == null) {
            refresh()
            return
        }
        // 有操作/重检在途 → 它的结果本来就更新，门控不叠加。
        if (busy || refreshJob?.isActive == true) return
        val snapGreen = snap.termuxInstalled && snap.termuxPerm &&
            snap.termuxAllowed && snap.hostInstalled && snap.hostAlive
        if (!snapGreen) {
            log("上次检测有未通过项（如刚从 Termux 返回），自动全量重检…")
            refresh()
            return
        }
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            val (alive, _) = McpHostManager.hostAliveDetail(context)
            // 探测期间可能已有操作/重检启动（applyServers、手动刷新等）：
            // 它们的结果本来就更新，这次的翻转判定作废，不叠加。
            if (busy || refreshJob?.isActive == true) return@launch
            if (alive == snap.hostAlive) {
                // 一致 → 只做无感差异检查：注册表缺工具才单发自愈，健康时
                // 零日志零动作（不产生任何「获取 host 工具列表」类轮询）。
                maybeSelfHeal()
                return@launch
            }
            val ageSec = (System.currentTimeMillis() - McpStatusCache.savedAtMs) / 1000
            log(
                "服务状态变化（快照保持了 ${ageSec}s）：${if (snap.hostAlive) "运行中 → 未应答" else "未运行 → 已应答"}，自动重检…"
            )
            refresh()
        }
    }

    androidx.compose.runtime.LaunchedEffect(Unit) { gateRefresh() }

    // 五灯全绿（折叠/摘要行用，原语义：只看基础设施）。旧的「不健康
    // 定时重试循环」已移除——周期性全量刷新等于状态面板自开的轮询通道
    // （用户否了）：host 掉线这类瞬时态由门控翻转检测覆盖，工具未注册
    // 由 maybeSelfHeal 单发差异自愈处理，不再有任何后台定时器。
    val allGreen = !status.probing && status.termuxInstalled && status.termuxPerm &&
        status.termuxAllowed && status.hostInstalled && status.hostAlive

    // 用户按指引手动打开 Termux 唤醒后，回到本页（ON_RESUME）触发智能
    // 门控（仿 ExecBackendSection 的 ON_RESUME 模式）：无快照/状态翻转
    // 才全量重检，其余情况不动（gateRefresh 内部自会处理）。
    @Suppress("DEPRECATION") // 非 deprecated 替代在 lifecycle-runtime-compose（非本项目依赖），与 L1765 现有用法一致
    val lifecycleOwner = androidx.compose.ui.platform.LocalLifecycleOwner.current
    androidx.compose.runtime.DisposableEffect(lifecycleOwner) {
        val obs = androidx.lifecycle.LifecycleEventObserver { _, event ->
            if (event == androidx.lifecycle.Lifecycle.Event.ON_RESUME) {
                gateRefresh()
            }
        }
        lifecycleOwner.lifecycle.addObserver(obs)
        onDispose { lifecycleOwner.lifecycle.removeObserver(obs) }
    }

    fun applyServers(next: List<McpHostManager.McpServerEntry>) {
        servers = next
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            busy = true
            step("推送清单并重启服务")
            log("保存清单（${next.size} 个服务器）…")
            runCatching {
                McpHostManager.saveServers(context, next)
                if (status.hostInstalled) {
                    McpHostManager.pushManifest(context)
                    McpHostManager.restartHost(context) { log(it) }
                }
                // 服务名可改（设置页）：upsert 必须用当前生效名——继续
                // 用死字面量 "termux" 会把旧键建回来，[mcp.servers] 变成
                // 双 server（transport 重复、工具重复注册）。
                val srvName = RuntimeBridge.getMcpServerName() ?: "termux"
                RuntimeBridge.upsertMcpServer(srvName, McpHostManager.HOST_URL)
            }.fold(
                onSuccess = {
                    // 配置已变 → 热重连 MCP 工具（免重启生效）。未装 host 时
                    // 也调——清单变了就该重连（直连型 server 不依赖 host）。
                    // runtime 未就绪（null）时退回旧语义：重启会话后生效。
                    val result = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
                    if (result == null) {
                        log("已更新（新工具重启会话后生效）")
                    } else {
                        val obj = runCatching { JSONObject(result) }.getOrNull()
                        if (obj != null && obj.optBoolean("ok", false)) {
                            val tools = obj.optInt("tools", -1)
                            val serversObj = obj.optJSONObject("servers")
                            val serversSummary = serversObj?.keys()?.asSequence()
                                ?.joinToString("、") { "${it}(${serversObj.optInt(it)}个)" }
                                .orEmpty()
                            log(
                                if (tools >= 0 && serversSummary.isNotEmpty()) {
                                    "已更新（工具已热重连生效：$tools 个工具,服务器: $serversSummary）"
                                } else {
                                    "已更新（工具已热重连生效）"
                                },
                            )
                        } else {
                            val err = obj?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                                ?: result.take(120)
                            log("清单已更新，但工具热重连失败（$err）——重启应用后生效")
                        }
                    }
                },
                onFailure = { log("失败：${it.message}") },
            )
            step(null)
            busy = false
            refresh()
        }
    }

    /**
     * 单服务器「重启」= 智能修复。host 没有单服务器重启接口，此按钮
     * 先探测自愈：reloadMcpServers 热重连常能救回「进程其实活着/已自愈」
     * 的常见情况（不惊动其他服务器）；快路径未就绪才升级整 host 重启——
     * 对 1-3 个服务器的典型场景等价于单服务器重启（副作用是同 host 的
     * 其他服务器进程随之重启，触发日志已明示）。
     */
    fun restartServer(slug: String, name: String) {
        busy = true
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            try {
                // 快路径：热重连 + 注册表复核。reload 失败/runtime 未就绪
                // → counts 为 null → 直接走升级路径。
                val result = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
                val obj = result?.let { runCatching { JSONObject(it) }.getOrNull() }
                val counts = if (obj != null && obj.optBoolean("ok", false)) {
                    RuntimeBridge.mcpRegistryToolsByPrefix(MCP_SERVER_NAMES)
                } else null
                if (counts != null && slug in counts) {
                    log("$name：工具已就绪（${counts[slug]} 个），无需重启")
                    // 顺手把注册表计数写回状态与快照：状态行立即转绿，
                    // 不必等一轮全量（与 maybeSelfHeal 的刷新范式一致）。
                    val updated = status.copy(hostTools = counts)
                    status = updated
                    McpStatusCache.update(updated)
                    return@launch
                }
                // 升级路径：整 host 重启（全部服务器进程随之重启）。
                log("$name：工具未就绪，重启 MCP 服务（全部服务器进程随之重启）…")
                McpHostManager.restartHost(context) { log(it) }
                val r2 = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
                val o2 = r2?.let { runCatching { JSONObject(it) }.getOrNull() }
                if (o2 != null && o2.optBoolean("ok", false)) {
                    log("$name：已就绪（${o2.optInt("tools", -1)} 个工具）")
                } else {
                    val err = o2?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                        ?: r2?.take(120)
                        ?: "runtime 未就绪"
                    log("$name：重启后仍未就绪（$err）")
                }
            } catch (e: Throwable) {
                log("$name：重启失败：${e.message?.take(160) ?: e.javaClass.simpleName}")
            } finally {
                busy = false
            }
            // 升级路径收尾全量重检（快路径已提前 return，不重检）。
            refresh()
        }
    }

    Section(
        title = "MCP 服务",
        subtitle = "在 Termux 中运行",
        // 手动刷新/重启入口改文字按钮（用户反馈 icon 不直观）：用户主动
        // 要求的重检/重启永远允许，无条件执行（与在途轮互斥由 refresh
        // 内部收敛）。
        action = {
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                CompactButton(
                    text = "刷新",
                    enabled = !status.probing,
                    container = mcpBtnContainer,
                    content = mcpBtnContent,
                ) {
                    status = McpStatus(probing = true); refresh()
                }
                // 重启 host 进程：比「刷新」重（杀旧进程 → nohup 重启 → 端口
                // 就绪），给 host 假死/状态错乱时一个不进 Termux 的手动出口。
                CompactButton(
                    text = "重启",
                    enabled = !busy && !status.probing,
                    container = mcpBtnContainer,
                    content = mcpBtnContent,
                ) {
                    busy = true
                    scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                        try {
                            // restartHost 自带全部步骤日志（杀旧/nohup/轮询
                            // 端口），失败抛异常并带 Termux 侧证据。
                            McpHostManager.restartHost(context) { log(it) }
                            log("MCP 服务已重启，重连工具…")
                            // host 进程换了 → core↔host 持久会话需重建，
                            // reload 不能省；注册工具随新 host 重新聚合一轮。
                            val result = runCatching { RuntimeBridge.reloadMcpServers() }.getOrNull()
                            val obj = result?.let { runCatching { JSONObject(it) }.getOrNull() }
                            if (obj != null && obj.optBoolean("ok", false)) {
                                log("重连成功：${obj.optInt("tools", -1)} 个工具")
                            } else {
                                val err = obj?.optString("error")?.take(120)?.takeIf { it.isNotEmpty() }
                                    ?: result?.take(120)
                                    ?: "runtime 未就绪"
                                log("工具重连失败：$err")
                            }
                        } catch (e: Throwable) {
                            log("MCP 服务重启失败：${e.message?.take(160) ?: e.javaClass.simpleName}")
                        } finally {
                            busy = false
                        }
                        // 最后全量重检：tools/list 会触发下游懒 spawn（npx
                        // 冷启动可达数秒，由既有 15s 读超时覆盖），工具与
                        // 状态随新 host 重新聚合；重启失败也重检——让状态
                        // 行如实反映（服务未运行），自动拉起仍有机会兜底。
                        refresh()
                    }
                }
            }
        },
    ) {
        // ── 折叠逻辑：五灯全绿自动收起（一行摘要）；异常自动展开 ──
        //（allGreen 在 McpSection 顶层计算）
        var expanded by androidx.compose.runtime.saveable.rememberSaveable {
            mutableStateOf(false)
        }
        // 诊断日志折叠态：一切就绪自动收起，点击标题行重新打开。
        var logOpen by androidx.compose.runtime.saveable.rememberSaveable {
            mutableStateOf(true)
        }
        var prevGreen by androidx.compose.runtime.saveable.rememberSaveable {
            mutableStateOf(false)
        }
        androidx.compose.runtime.LaunchedEffect(allGreen) {
            if (allGreen && !prevGreen) { // 变绿瞬间收起
                expanded = false
                logOpen = false // 一切就绪：诊断日志一并折叠
            }
            if (!allGreen) { // 有异常自动展开
                expanded = true
                logOpen = true
            }
            prevGreen = allGreen
        }
        // 摘要行（始终显示；点击切换）。
        Row(
            Modifier
                .fillMaxWidth()
                .clickable { expanded = !expanded },
            verticalAlignment = Alignment.CenterVertically,
        ) {
            if (status.probing) {
                // 渐进进度：五灯 key 命中数（检测完一项 +1）。
                val done = MCP_LIGHT_KEYS.count { it in status.checked }
                Text("检测中 · 已完成 $done/5", style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.outline)
            } else {
                if (allGreen) {
                    Icon(
                        imageVector = Icons.Filled.CheckCircle,
                        contentDescription = "一切就绪",
                        tint = successGreen(),
                        modifier = Modifier.height(16.dp).width(16.dp),
                    )
                    Text("  一切就绪 · 服务运行中", style = MaterialTheme.typography.labelMedium,
                        color = successGreen())
                } else {
                    Text("状态详情", style = MaterialTheme.typography.labelMedium,
                        color = MaterialTheme.colorScheme.outline)
                }
            }
        }
        // 检测中也显示（expanded 即渲染）：区块内每个灯按 checked 命中
        // 才出现——检测完一项显示一项，未完成的项不占位。
        if (expanded) {
            if ("termuxInstalled" in status.checked) McpStatusLine(status.termuxInstalled, "Termux 已安装")
            if ("termuxPerm" in status.checked) McpStatusLine(status.termuxPerm, "RUN_COMMAND 权限")
            if ("termuxAllowed" in status.checked) McpStatusLine(status.termuxAllowed, "RUN_COMMAND 通道")
            status.channel?.let {
                Text(
                    TermuxDiagnostics.describe(it),
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
            // ── 通道未通过时按因给指引 ──────────────────────────
            val chan = status.channel
            when (chan) {
                is TermuxDiagnostics.ChannelResult.PolicyRejected -> {
                    Text(
                        "在 Termux 中粘贴执行下面这条命令即可放行（命令前后已带换行，不破坏原配置；执行后点「刷新」）：",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                    // 代码块样式：点击整块直接复制。
                    CodeBlock(TERMUX_ALLOW_EXTERNAL_CMD)
                    if (status.termuxInstalled) {
                        // 复制后一键跳到 Termux 粘贴执行。
                        CompactButton(text = "打开 Termux", onClick = {
                            context.packageManager.getLaunchIntentForPackage("com.termux")?.let { intent ->
                                intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK)
                                context.startActivity(intent)
                            }
                        })
                    }
                    Text(
                        "若 Termux 刚安装尚未打开过：先打开一次完成初始化，再粘贴执行上面命令。",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
                is TermuxDiagnostics.ChannelResult.NoResponse -> {
                    if (chan.stopped) {
                        Text(
                            "Termux 处于停止状态（安装后从未打开、被强制停止或被系统清理）。系统限制下无法后台直接拉起，只能打开一次 Termux 唤醒——点下方「打开 Termux」，打开后请手动回到 OpenSlate，返回时会自动重新检测。",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    } else {
                        Text(
                            "Termux 无应答：进程可能被系统冻结或后台受限。建议在 Termux 中执行 termux-wake-lock 开启常驻通知，并在系统设置中关闭 Termux 的电池优化。也可点下方「打开 Termux」手动唤醒——打开后请手动回到 OpenSlate，返回时会自动重新检测。",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    if (status.probing) {
                        // 0.5s stall 的早写状态：检测未完，注明非最终结论。
                        Text(
                            "通道检测仍在后台继续，若随后通过会自动更新。",
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                    // 按钮紧贴指引文案（原文案「点下方」即指此处），不放远处按钮排。
                    CompactButton(text = "打开 Termux", enabled = !busy) {
                        log("打开 Termux（请手动返回 OpenSlate，返回后自动重新检测）…")
                        if (!McpHostManager.startTermuxApp(context)) {
                            log("拉起失败：找不到 Termux 启动入口（未安装？）")
                        }
                    }
                }
                is TermuxDiagnostics.ChannelResult.Rejected -> {
                    Text(
                        "Termux 拒绝了命令：${chan.errmsg.take(160)}",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
                is TermuxDiagnostics.ChannelResult.DispatchError -> {
                    Text(
                        "命令派发失败：${chan.error.take(160)}",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
                // Ok / null（未探测短路，如未安装/无权限）：不显示指引块。
                is TermuxDiagnostics.ChannelResult.Ok, null -> {}
            }
            // checked 命中才渲染：避免权限检测完成前的瞬间闪现。
            if ("termuxPerm" in status.checked && !status.termuxPerm) {
                CompactButton(text = "申请 RUN_COMMAND 权限", onClick = {
                    val activity = context as? android.app.Activity
                    if (activity != null) {
                        activity.requestPermissions(
                            arrayOf(dev.openslate.mobile.bridge.TermuxExec.RUN_COMMAND_PERMISSION),
                            1002,
                        )
                    } else {
                        openOwnAppSettings(context)
                    }
                })
            }
            if ("hostInstalled" in status.checked) McpStatusLine(status.hostInstalled, "MCP 服务已安装")
            if ("hostAlive" in status.checked) McpStatusLine(status.hostAlive, "服务运行中")
        }
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            // 前置三绿（Termux/权限/放行）就绪才出现 —— 那时恰好只剩
            // "安装 + 运行"最后两步，由这个按钮一键完成。
            if (status.termuxInstalled && status.termuxPerm && status.termuxAllowed) {
                CompactButton(
                    text = if (status.hostInstalled) "自动化操作（重新安装）" else "自动化操作（一键安装）",
                    enabled = !busy,
                    container = successGreenContainer(),
                    content = successGreen(),
                ) {
                    busy = true
                    scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                        runCatching {
                            McpHostManager.installHost(context, log = { log(it) }, step = { step(it) })
                        }
                            .onSuccess { msg ->
                                log(msg)
                                // 服务名可改：同 applyServers——upsert 用当前
                                // 生效名，防止把旧键 "termux" 建成双 server。
                                val srvName = RuntimeBridge.getMcpServerName() ?: "termux"
                                RuntimeBridge.upsertMcpServer(srvName, McpHostManager.HOST_URL)
                            }
                            .onFailure { log("失败：${it.message}") }
                        busy = false
                        refresh()
                    }
                }
            }
            // 手动「刷新」入口已移到 Section 标题行（IconButton）——正文
            // 不再放重复入口。
        }
        // 当前步骤 + 已进行时长（每秒刷新；卡住时能看出停在哪一步、停了多久）。
        currentStep?.let { name ->
            Row(verticalAlignment = Alignment.CenterVertically) {
                androidx.compose.material3.CircularProgressIndicator(
                    modifier = Modifier.height(16.dp).width(16.dp),
                    strokeWidth = 2.dp,
                )
                Text(
                    "  $name（已进行 ${stepElapsedSec}s）",
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
        }
        if (logLines.isNotEmpty()) {
            // 标题行可点击：一切就绪自动折叠后，点这里重新打开（或再次收起）。
            Row(
                Modifier
                    .fillMaxWidth()
                    .clickable { logOpen = !logOpen },
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(
                    "诊断日志（${logLines.size} 条）",
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.weight(1f),
                )
                CompactButton(text = "清空", onClick = { logLines = emptyList() })
            }
            if (logOpen) {
                val listState = rememberLazyListState()
                androidx.compose.runtime.LaunchedEffect(logLines.size) {
                    if (logLines.isNotEmpty()) listState.animateScrollToItem(logLines.size - 1)
                }
                LazyColumn(
                    state = listState,
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 220.dp)
                        .clip(RoundedCornerShape(8.dp))
                        .background(MaterialTheme.colorScheme.surfaceVariant)
                        .padding(8.dp),
                ) {
                    items(logLines) { line ->
                        Text(
                            line,
                            style = MaterialTheme.typography.labelSmall,
                            // 系统等宽（不换 MonoFont 子集）：诊断日志是任意
                            // 内容（时间戳 + 多源诊断文本），需全字形覆盖。
                            fontFamily = FontFamily.Monospace,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    }
                }
            }
        }
        Text(
            "一键安装并启动 MCP 服务，之后重启会话即可使用。MCP 服务器在 " +
                "Termux 中运行；命令用 npx 时需先在 Termux 安装 Node.js（pkg install nodejs），" +
                "npm 包首次运行会自动下载。",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
    }

    Section(
        title = "MCP 服务器",
        action = {
            IconButton(onClick = { editServer = -1 to Triple("", null, "") }) {
                Icon(Icons.Filled.Add, contentDescription = "添加 MCP")
            }
        },
    ) {
        if (servers.isEmpty()) {
            Text("（空）添加一个 MCP 服务器，例如：", color = MaterialTheme.colorScheme.outline, style = MaterialTheme.typography.labelSmall)
            Text(
                "npx -y @modelcontextprotocol/server-filesystem /sdcard",
                style = MaterialTheme.typography.labelSmall, fontFamily = MonoFont,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        // 派生 slug 与清单生成（manifestToml）共用同一实现：行内展示的前缀
        // 就是 host 实际路由用的前缀，两处绝不能各算各的。
        val slugMap = remember(servers) { McpHostManager.serversSlugMap(servers) }
        for ((idx, entry) in slugMap.withIndex()) {
            val (s, slug) = entry
            Card(Modifier.fillMaxWidth()) {
                // 纵向布局：信息占满整行宽度，控制按钮单独一行居右——
                // 不与名称/命令/状态文本抢横向空间。
                // 底边收紧到 6dp：按钮行贴近卡片底边（视觉重心下沉），
                // 其余三边不变。
                Column(Modifier.fillMaxWidth().padding(start = 12.dp, end = 12.dp, top = 12.dp, bottom = 6.dp)) {
                    Text(s.name, style = MaterialTheme.typography.titleSmall)
                    Text(s.command, style = MaterialTheme.typography.bodySmall,
                        fontFamily = MonoFont, color = MaterialTheme.colorScheme.outline)
                    // ── 每服务器运行状态行：按 host 实际工具列表判定，
                    // 而非只看端口是否应答（端口活着 ≠ server 起来了）。
                    // 正常态的前缀片段用等宽字体（与对话框预览行同款做法）。
                    val cmdToken = s.command.trim().split(Regex("\\s+")).first()
                    val tools = status.hostTools
                    // hostAlive 与 tools 都渐进落地：检测中命中即转绿，
                    // 不必等 probing 翻 false。
                    if (status.hostAlive && tools != null && slug in tools) {
                        Text(
                            buildAnnotatedString {
                                append("正常运行（${tools[slug]} 个工具）· 前缀 ")
                                withStyle(SpanStyle(fontFamily = MonoFont)) {
                                    append("${slug}__")
                                }
                            },
                            style = MaterialTheme.typography.labelSmall,
                            color = successGreen(),
                        )
                    } else {
                        // 已落地数据优先于「检测中」：missingCommands 先于
                        // 探活结果写入，警示不等整轮；probing=false 时各
                        // 分支与旧逻辑等价（hostAlive=true 且 tools=null →
                        // 状态未知；hostAlive=false → 服务未运行）。
                        val (statusText, statusColor) = when {
                            tools != null && cmdToken in status.missingCommands ->
                                "无法启动：Termux 缺少 $cmdToken" to MaterialTheme.colorScheme.error
                            tools != null ->
                                "启动中或启动失败（npx 首次运行需下载依赖，可稍候刷新；" +
                                    "持续失败请查看 Termux 侧 ~/.openslate/host.log）" to MaterialTheme.colorScheme.error
                            !status.probing && !status.hostAlive -> "服务未运行" to MaterialTheme.colorScheme.outline
                            !status.probing -> "状态未知（工具列表获取失败）" to MaterialTheme.colorScheme.outline
                            else -> "检测中…" to MaterialTheme.colorScheme.outline
                        }
                        Text(statusText, style = MaterialTheme.typography.labelSmall, color = statusColor)
                    }
                    // 启动命令首 token 在 Termux 缺失 → 行内警示（规格与
                    // ExecBackendSection 的警告行一致；探测失败不会进
                    // missingCommands，不误报）。
                    if (cmdToken in status.missingCommands) {
                        Row(verticalAlignment = Alignment.CenterVertically) {
                            Icon(
                                imageVector = Icons.Outlined.Warning,
                                contentDescription = null, // 语义由文字承载
                                tint = MaterialTheme.colorScheme.error,
                                modifier = Modifier.height(14.dp).width(14.dp),
                            )
                            Text(
                                if (cmdToken == "npx") {
                                    "  Termux 未安装 npx：该服务器无法启动。" +
                                        "请在 Termux 中执行 pkg install nodejs（npx 随 Node.js 一起安装）。"
                                } else {
                                    "  Termux 未安装 $cmdToken：该服务器无法启动。请先在 Termux 中安装对应运行时。"
                                },
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.error,
                            )
                        }
                    }
                    // 控制按钮独立一行、居右；文字按钮（用户反馈 icon
                    // 不直观，与标题行刷新/重启统一），主题色同全局 MCP 按钮。
                    Row(
                        Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(8.dp, Alignment.End),
                    ) {
                        CompactButton(
                            text = "编辑",
                            enabled = !busy,
                            container = mcpBtnContainer,
                            content = mcpBtnContent,
                        ) {
                            editServer = idx to Triple(s.name, s.slug, s.command)
                        }
                        CompactButton(
                            text = "重启",
                            enabled = !busy,
                            container = mcpBtnContainer,
                            content = mcpBtnContent,
                        ) {
                            restartServer(slug, s.name)
                        }
                        CompactButton(
                            text = "删除",
                            enabled = !busy,
                            container = mcpBtnContainer,
                            content = mcpBtnContent,
                        ) {
                            applyServers(servers.filter { it != s })
                        }
                    }
                }
            }
        }
    }

    editServer?.let { initial ->
        val (editIdx, prefill) = initial
        val isEdit = editIdx >= 0
        var name by remember(initial) { mutableStateOf(prefill.first) }
        var userSlug by remember(initial) { mutableStateOf(prefill.second ?: "") }
        var command by remember(initial) { mutableStateOf(prefill.third) }
        // 前缀预览与真实派生同源（entrySlug）：编辑时只把该条目之前的
        // slug 视为已占用（清单按顺序派生，后条目不影响前条目）；添加时
        // 全部现有 slug 已占用。保证预览 = 保存后清单里的实际前缀。
        val takenSlugs = remember(servers, editIdx) {
            // 编辑：只算该条目之前的（清单顺序派生，后条目不影响前条目）；
            // 添加：算全部现有条目（否则撞名时预览漏掉 -2 后缀）。
            val upto = if (editIdx >= 0) editIdx else servers.size
            McpHostManager.serversSlugMap(servers.take(upto)).map { it.second }
        }
        // 名称留空时自动识别显示名（仅对话框建议用，不进清单派生链路）：
        // suggestServerName 识别不出时回退命令首 token（直接 bin 形态如
        // mcp-server-memory → 名称就是它）；命令为空时为 ""（确认按钮
        // 已禁用，placeholder 显示固定的机制提示文案）。
        val resolvedName = name.trim().ifEmpty {
            McpHostManager.suggestServerName(command.trim())
                ?: command.trim().split(Regex("\\s+")).first()
        }
        // 最终生效 slug：用户指定的代号优先（同样 sanitize），空则从名称
        // （名称留空时即自动识别结果）派生。
        val finalSlug = McpHostManager.entrySlug(resolvedName, userSlug, takenSlugs)
        val slugSpecified = userSlug.isNotBlank()
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { editServer = null },
            title = { Text(if (isEdit) "编辑 MCP 服务器" else "添加 MCP 服务器") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    // 名称可留空：留空时保存前自动从启动命令识别（保存
                    // 路径见 resolvedName），落库 name 永远非空。识别值
                    // 用 placeholder 动态呈现（随命令实时变化），不占
                    // 预览区行。
                    OutlinedTextField(
                        value = name,
                        onValueChange = { name = it },
                        label = { Text("名称（可留空）") },
                        placeholder = {
                            Text(
                                if (resolvedName.isEmpty()) "留空自动识别 npm/uvx 包名"
                                else "$resolvedName（自动识别）"
                            )
                        },
                    )
                    OutlinedTextField(
                        value = userSlug,
                        onValueChange = { userSlug = it },
                        label = { Text("代号（工具名前缀）") },
                        // 留空 = 自动派生；placeholder 实时给出将派生的值
                        //（同样基于解析后的名称，与最终前缀预览一致）。
                        placeholder = { Text("auto: " + McpHostManager.entrySlug(resolvedName, null, takenSlugs)) },
                    )
                    OutlinedTextField(value = command, onValueChange = { command = it }, label = { Text("启动命令") })
                    Text("完整命令行，如 npx -y @modelcontextprotocol/server-memory", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.outline)
                    Text(
                        buildAnnotatedString {
                            append("工具名前缀：")
                            withStyle(SpanStyle(fontFamily = MonoFont)) { append(finalSlug + "__") }
                            append(if (slugSpecified) "（已指定）" else "（自动派生）")
                        },
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
            },
            confirmButton = {
                // 只要求启动命令非空：名称可留空（保存时自动识别兜底）。
                SoftButton(enabled = command.isNotBlank() && !busy, onClick = {
                    val entry = McpHostManager.McpServerEntry(
                        // 落库 name 永远非空：resolvedName = 输入留空时已用
                        // suggestServerName / 命令首 token 兜底，slug 派生
                        // 与清单链路无需感知。
                        resolvedName,
                        userSlug.trim().takeIf { it.isNotEmpty() }, // 空 = 自动派生，不持久化空串
                        command.trim(),
                    )
                    editServer = null
                    // 编辑按索引原位替换（避免 data class 相等性误伤同内容
                    // 条目）；添加追加到尾部。
                    applyServers(
                        if (isEdit) {
                            servers.toMutableList().also { it[editIdx] = entry }
                        } else {
                            servers + entry
                        }
                    )
                }) { Text(if (isEdit) "保存" else "添加") }
            },
            dismissButton = { SoftButton(onClick = { editServer = null }) { Text("取消") } },
        )
    }
}
