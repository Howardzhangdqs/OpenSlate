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
import androidx.compose.foundation.horizontalScroll
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
import androidx.compose.material.icons.outlined.Circle
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
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import kotlinx.coroutines.async
import kotlinx.coroutines.launch
import dev.openslate.mobile.bridge.LogExporter
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.ModelUi
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

    var editProvider by remember { mutableStateOf<ProviderUi?>(null) }
    var editProviderIsNew by remember { mutableStateOf(false) }
    var editModel by remember { mutableStateOf<ModelUi?>(null) }
    var editModelIsNew by remember { mutableStateOf(false) }
    /** Provider 页模型管理打开时锁定 provider（条目归属不可跨 provider 改）。 */
    var editModelLock by remember { mutableStateOf<String?>(null) }
    /** 代号绑定编辑（null = 关闭；name 空 = 新建）。 */
    var editAlias by remember { mutableStateOf<AliasDraft?>(null) }
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
                    for ((label, idx) in listOf("MCP" to 0, "命令行" to 1)) {
                        androidx.compose.material3.Tab(
                            selected = toolTab == idx,
                            onClick = { toolTab = idx },
                            text = { Text(label) },
                        )
                    }
                }
            }

            // ── 域内容（各自独立滚动）───────────────────────────
            Column(
                Modifier
                    .fillMaxSize()
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
                        // ── 会话域：功能模型（功能 → 代号）──────────
                        // 代号与具体模型的绑定在「模型」页管理；模型清单
                        // 本身在「Provider」页管理（自动检测 + 手动增删）。
                        Section(title = "功能模型（哪个功能用哪档模型）") {
                            Text(
                                "会话与模型的关联：每个功能在这里选一个代号，代号再经「模型」页" +
                                    "绑定到具体模型。换新模型只需在「模型」页把代号改绑到新模型，" +
                                    "这里不用动。",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                            val aliases = (config.levels.keys + config.capabilities.values)
                                .distinct().sorted()
                            val capRows = listOf(
                                Triple("main", "主对话（正常 API 请求）", "main"),
                                Triple("compact", "上下文压缩（超限自动摘要）", "fast"),
                                Triple("title", "会话标题生成", "fast"),
                            )
                            for ((cap, label, dft) in capRows) {
                                Column {
                                    Text(
                                        label,
                                        style = MaterialTheme.typography.bodySmall,
                                        color = MaterialTheme.colorScheme.outline,
                                    )
                                    SlidingSegmentedControl(
                                        options = aliases,
                                        selected = config.capabilities[cap] ?: dft,
                                        onSelect = { RuntimeBridge.setCapability(cap, it) },
                                    )
                                }
                            }
                        }
                    }
                    1 -> {
                        // ── 模型域：纯映射（代号 → Provider 的具体模型）──
                        // 模型清单本身在 Provider 页管理（编辑 Provider 时
                        // 自动检测 + 手动增删）；本页只做"代号绑定哪只模型"。
                        Section(
                            title = "模型代号（代号 → 具体模型）",
                            action = {
                                IconButton(onClick = { editAlias = AliasDraft() }) {
                                    Icon(Icons.Filled.Add, contentDescription = "新增代号")
                                }
                            },
                        ) {
                            Text(
                                "代号是「会话」页功能与模型之间的桥梁：功能指向代号，代号在这里" +
                                    "绑定到 Provider 的具体模型条目（条目清单在 Provider 页维护）。" +
                                    "新模型发布后把代号改绑到新条目，引用它的功能即刻切换。",
                                style = MaterialTheme.typography.bodySmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                            if (config.levels.isEmpty()) {
                                Text("（空）", color = MaterialTheme.colorScheme.outline)
                            }
                            for (level in config.levels.keys.sorted()) {
                                val bound = config.levels[level]?.let { e ->
                                    config.models.find { it.entry == e }
                                }
                                val providerName = bound?.let {
                                    config.providers.find { p -> p.name == it.provider }?.displayName
                                        ?: it.provider
                                }
                                Card(
                                    modifier = Modifier.fillMaxWidth(),
                                    onClick = { editAlias = AliasDraft(level, config.levels[level] ?: "") },
                                ) {
                                    Row(
                                        Modifier.padding(12.dp),
                                        verticalAlignment = Alignment.CenterVertically,
                                    ) {
                                        Column(Modifier.weight(1f)) {
                                            Text(level, style = MaterialTheme.typography.titleSmall)
                                            Text(
                                                bound
                                                    ?.let { "$providerName · ${it.modelId}" }
                                                    ?: "绑定的模型条目缺失",
                                                style = MaterialTheme.typography.bodySmall,
                                                color = if (bound != null) {
                                                    MaterialTheme.colorScheme.outline
                                                } else {
                                                    MaterialTheme.colorScheme.error
                                                },
                                            )
                                        }
                                        Icon(
                                            Icons.Filled.Edit,
                                            contentDescription = "改绑",
                                            tint = MaterialTheme.colorScheme.outline,
                                        )
                                        // main/fast 必需、被功能引用的代号不可删
                                        // （服务端同款守卫，这里不显示入口）。
                                        val deletable = level != "main" && level != "fast" &&
                                            level !in config.capabilities.values
                                        if (deletable) {
                                            IconButton(onClick = { RuntimeBridge.deleteLevel(level) }) {
                                                Icon(
                                                    Icons.Filled.Delete,
                                                    contentDescription = "删除代号",
                                                    tint = MaterialTheme.colorScheme.outline,
                                                )
                                            }
                                        }
                                    }
                                }
                            }
                        }
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
                                                fontFamily = FontFamily.Monospace,
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
                        // ── 工具域（二级 Tab 在上方吸顶）：MCP / 命令行 ──
                        if (toolTab == 0) {
                            McpSection()
                            LogSection()
                        } else ExecBackendSection()
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
            lockProvider = editModelLock,
            onDismiss = {
                editModel = null
                editModelLock = null
            },
            onSave = { ui ->
                // Provider 页新建且未填条目 ID → 自动生成（provider__model slug）。
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
                // 查重：同 Provider 下不允许重复的模型 ID（否则 slug 冲突
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

    // 代号绑定编辑（模型页）。entry 为 [models] 条目名；选择器按
    // provider → model 两级级联，保存即 set_level 落盘。
    editAlias?.let { draft ->
        AliasBindingDialog(
            initial = draft,
            existingAliases = config.levels.keys.toSet(),
            providers = config.providers.map { it.displayName to it.name },
            models = config.models,
            onDismiss = { editAlias = null },
            onSave = { name, entry ->
                RuntimeBridge.setLevel(name, entry)
                editAlias = null
            },
        )
    }

    // Provider 的模型管理（自动检测 + 手动增删改）。
    modelsProvider?.let { p ->
        ProviderModelsDialog(
            provider = p,
            models = config.models.filter { it.provider == p.name },
            levels = config.levels,
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

/** 代号绑定草稿（name 空 = 新建代号）。entry = 目标 [models] 条目名。 */
data class AliasDraft(val name: String = "", val entry: String = "")

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

/** 模型条目内部 ID：`<provider>__<model>` slug 化（TOML 键安全：
 * 非 [a-z0-9-] 折成 `-`），冲突时追加随机后缀。仅在 Provider 页自动
 * 检测/手动添加时生成；改名入口不存在，引用稳定。 */
private fun genModelEntryId(providerId: String, modelId: String, existing: Set<String>): String {
    fun slug(s: String) = s.lowercase().replace(Regex("[^a-z0-9-]+"), "-")
        .trim('-').ifEmpty { "m" }
    var id = "${slug(providerId)}__${slug(modelId)}"
    if (id !in existing) return id
    val chars = ('a'..'f') + ('0'..'9')
    while (id in existing) {
        id = "${slug(providerId)}__${slug(modelId)}-" +
            List(4) { chars.random() }.joinToString("")
    }
    return id
}

/** 代号绑定对话框：代号名（新建时可填）+ Provider 选择 + 该 provider
 *  下的模型选择（级联）。保存 = set_level(alias → entry)。 */
@Composable
private fun AliasBindingDialog(
    initial: AliasDraft,
    existingAliases: Set<String>,
    providers: List<Pair<String, String>>, // (显示名, 内部 ID)
    models: List<ModelUi>,
    onDismiss: () -> Unit,
    onSave: (alias: String, entry: String) -> Unit,
) {
    val isNew = initial.name.isEmpty()
    var name by remember { mutableStateOf(initial.name) }
    // 初值：已绑条目的 provider；新建默认第一个 provider。
    val initialEntry = models.find { it.entry == initial.entry }
    var provider by remember {
        mutableStateOf(initialEntry?.provider ?: providers.firstOrNull()?.second ?: "")
    }
    var entry by remember { mutableStateOf(initial.entry) }
    // provider 切换后目标条目可能不属于它 → 清空重选。
    if (models.find { it.entry == entry }?.provider != provider) entry = ""

    val trimmed = name.trim()
    val providerModels = models.filter { it.provider == provider }
    val valid = (!isNew || (trimmed.isNotEmpty() && trimmed !in existingAliases)) &&
        entry.isNotEmpty() && entry in providerModels.map { it.entry }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (isNew) "新增模型代号" else "改绑代号「${initial.name}」") },
        text = {
            Column(
                Modifier.verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                if (isNew) {
                    OutlinedTextField(
                        trimmed,
                        { name = it },
                        label = { Text("代号名（如 fast、vision、deep）") },
                        isError = trimmed.isNotEmpty() && trimmed in existingAliases,
                        supportingText = if (trimmed.isNotEmpty() && trimmed in existingAliases) {
                            { Text("代号已存在") }
                        } else {
                            null
                        },
                    )
                }
                if (providers.isEmpty()) {
                    Text(
                        "先到「Provider」页添加至少一个 Provider。",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.error,
                    )
                } else {
                    Text("Provider", style = MaterialTheme.typography.labelMedium)
                    Row(
                        Modifier.horizontalScroll(rememberScrollState()),
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        for ((title, id) in providers) {
                            FilterChip(
                                selected = provider == id,
                                onClick = { provider = id },
                                label = { Text(title) },
                            )
                        }
                    }
                    Text("模型", style = MaterialTheme.typography.labelMedium)
                    if (providerModels.isEmpty()) {
                        Text(
                            "该 Provider 下还没有模型，先在其「模型」管理里添加或自动检测。",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                        )
                    } else {
                        Row(
                            Modifier.horizontalScroll(rememberScrollState()),
                            horizontalArrangement = Arrangement.spacedBy(6.dp),
                        ) {
                            for (m in providerModels) {
                                FilterChip(
                                    selected = entry == m.entry,
                                    onClick = { entry = m.entry },
                                    label = { Text(m.modelId) },
                                )
                            }
                        }
                    }
                }
            }
        },
        confirmButton = {
            SoftButton(enabled = valid, onClick = { onSave(if (isNew) trimmed else initial.name, entry) }) {
                Text("保存")
            }
        },
        dismissButton = { SoftButton(onClick = onDismiss) { Text("取消") } },
    )
}

/** Provider 的模型管理：自动检测（远端 /models）+ 手动增删改。
 *  检测出的新模型以 chip 呈现，点击即入库（自动生成条目 ID）。 */
@Composable
private fun ProviderModelsDialog(
    provider: ProviderUi,
    models: List<ModelUi>, // 该 provider 名下的条目
    levels: Map<String, String>, // 代号 → 条目（删除时悬空提示；删除不再被代号拦截）
    capabilities: Map<String, String>, // 功能 → 代号（删除时悬空提示；删除不再被引用拦截）
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
                    "此清单是该 Provider 的模型条目；「模型」页的代号与「会话」页的功能最终都" +
                        "指向这里的条目。点条目左侧可查看详情与代号引用。",
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
                            Text(m.modelId, style = MaterialTheme.typography.bodyMedium)
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
                                        Text(id)
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

    // 删除确认。完全松耦合：删除永远放行，引用方悬空。代号悬空 →「模型」
    // 页标红可改绑，用该代号时报错；功能悬空 →「会话」页可改绑，运行时
    // 自动回退默认档（服务端 DeleteModel 无引用拦截，同款语义）。
    pendingDelete?.let { m ->
        val levelRefs = levels.filterValues { it == m.entry }.keys.toList()
        val capRefs = capabilities.filterValues { it == m.entry }.keys.toList()
        val dangling = buildList {
            if (levelRefs.isNotEmpty()) {
                add("代号 ${levelRefs.joinToString("、")} 将悬空（到「模型」页改绑）")
            }
            if (capRefs.isNotEmpty()) {
                add("功能 ${capRefs.joinToString("、")} 将悬空（到「会话」页改绑）")
            }
        }.joinToString("；")
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
    // 内部条目 ID 不对外）。代号是展示时从 levels 反查的（松耦合：代号 →
    // 模型单向映射，模型侧不存 alias，换新模型只需在「模型」页改绑代号）。
    detailModel?.let { m ->
        val aliases = levels.filterValues { it == m.entry }.keys.sorted()
        AlertDialog(
            onDismissRequest = { detailModel = null },
            title = { Text(m.modelId) },
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
                    InfoRow("代号", aliases.joinToString("、").ifEmpty { "（无）" })
                    InfoRow("上下文窗口", fmtTokensFull(m.maxContextTokens))
                    InfoRow("最大输出", fmtTokensFull(m.maxOutputTokens))
                    InfoRow("输入价格", fmtPrice(m.inputPricePerMtok))
                    InfoRow("输出价格", fmtPrice(m.outputPricePerMtok))
                    InfoRow("所属 Provider", m.provider)
                    if (aliases.isNotEmpty()) {
                        Text(
                            "代号只是指向本模型的映射，不随模型存储；要换新模型，到「模型」页把对应代号改绑即可。",
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
    action: (@Composable () -> Unit)? = null,
    content: @Composable androidx.compose.foundation.layout.ColumnScope.() -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Row(verticalAlignment = Alignment.CenterVertically) {
            Text(
                title,
                style = MaterialTheme.typography.titleMedium,
                modifier = Modifier.weight(1f),
            )
            action?.invoke()
        }
        content()
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
 */
@Composable
private fun SlidingSegmentedControl(
    options: List<String>,
    selected: String,
    onSelect: (String) -> Unit,
    modifier: Modifier = Modifier,
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
                            opt,
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
                                fontFamily = FontFamily.Monospace,
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
                            fontFamily = FontFamily.Monospace,
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
    lockProvider: String? = null, // Provider 页进入时锁定归属
    onDismiss: () -> Unit,
    onSave: (ModelUi) -> String?, // null = 已保存（调用方关弹窗）；非 null = 错误提示，弹窗保持打开
) {
    var entry by remember { mutableStateOf(initial.entry) }
    var provider by remember {
        mutableStateOf(lockProvider ?: initial.provider.ifBlank { providers.firstOrNull()?.second ?: "" })
    }
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
                    label = { Text(if (lockProvider != null) "条目 ID（留空自动生成）" else "别名（如 glm-main）") },
                    enabled = isNew,
                )
                if (lockProvider != null) {
                    // Provider 页模型管理进入：归属锁定，仅展示。
                    Text(
                        "Provider：${providers.find { it.second == lockProvider }?.first ?: lockProvider}",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                    )
                } else if (providers.isNotEmpty()) {
                    Text("Provider", style = MaterialTheme.typography.labelMedium)
                    Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        for ((label, pid) in providers) {
                            TextButton(onClick = { provider = pid }) {
                                Text(label)
                                // 选中标记用 Material 图标（图标规范见 AGENTS.md）。
                                if (pid == provider) {
                                    Icon(
                                        Icons.Filled.Check,
                                        contentDescription = "已选中",
                                        tint = MaterialTheme.colorScheme.primary,
                                        modifier = Modifier.height(14.dp).width(14.dp),
                                    )
                                }
                            }
                        }
                    }
                } else {
                    OutlinedTextField(provider, { provider = it }, label = { Text("Provider ID") })
                }
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
                    if ((entry.isNotBlank() || lockProvider != null) && provider.isNotBlank() && modelId.isNotBlank()) {
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

/** 方形复选框行（多选场景；Checkbox 圆形默认改为方角以区分单选语义）。 */
@Composable
private fun CheckboxRow(label: String, checked: Boolean, onChange: (Boolean) -> Unit) {
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
        Text(label, style = MaterialTheme.typography.bodyMedium)
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

/** 紧凑圆角按钮：淡色背景 + 深色文字 + 小上下留白（工具页统一风格）。 */
@Composable
private fun CompactButton(
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

/** 状态行：成功 → 实心对勾圆（绿）；未就绪 → 空心圆（灰）。 */
@Composable
private fun McpStatusLine(ok: Boolean, text: String) {
    Row(verticalAlignment = Alignment.CenterVertically) {
        Icon(
            imageVector = if (ok) Icons.Filled.CheckCircle else Icons.Outlined.Circle,
            contentDescription = if (ok) "已就绪" else "未就绪",
            tint = if (ok) successGreen()
            else MaterialTheme.colorScheme.outline,
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
                label = "native — Android 自带系统 shell（toybox 子集，零依赖零授权）",
                checked = isEnabled("native"),
                onChange = { setEnabled("native", it) },
            )
            CheckboxRow(
                label = "termux — Termux 完整 Linux 环境（apt/python/…）",
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
                    "已多选：native 工具名为 bash，Termux 工具名为 termux_bash。",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                isEnabled("termux") -> Text(
                    "仅 termux：工具名为 bash（Termux 后端）。完整环境需在 " +
                        "~/.termux/termux.properties 设置 allow-external-apps=true。",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                else -> Text(
                    "仅 native：工具名为 bash，零依赖、零授权，无需 Termux。",
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
    val termuxAllowed: Boolean = false,
    val hostInstalled: Boolean = false,
    val hostAlive: Boolean = false,
)

/**
 * MCP 域：Termux 侧 mcp-host 安装/清单/状态。
 *
 * - "自动化操作"（前置三绿后出现）：assets 内嵌的静态 host 经本机回环
 *   TCP 装进 Termux，写清单、启动、并把 [mcp.servers.termux] 写入
 *   openslate.toml；
 * - server 清单：App 侧管理，变更即推送清单并重启 host；
 *   工具名前缀 `alias__`，模型看到 `termux_fs__list_directory`。
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
            Text(it, style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace, color = MaterialTheme.colorScheme.outline)
        }
    }
}

@Composable
private fun McpSection() {
    val context = androidx.compose.ui.platform.LocalContext.current
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    var status by remember { mutableStateOf(McpStatus()) }
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
    var editServer by remember { mutableStateOf<Pair<String, String>?>(null) } // alias to command

    fun refresh() {
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            val t0 = System.currentTimeMillis()
            log("── 状态检查开始 ──")
            step("检查 Termux / 权限")
            val s = McpStatus(probing = false,
                termuxInstalled = dev.openslate.mobile.bridge.TermuxExec.isTermuxInstalled(context),
                termuxPerm = dev.openslate.mobile.bridge.TermuxExec.hasPermission(context),
                termuxAllowed = false, hostInstalled = false, hostAlive = false)
            log("Termux 已安装：${s.termuxInstalled}；RUN_COMMAND 权限：${s.termuxPerm}")
            // 快检查立即写回：慢通道（RUN_COMMAND 往返最长 12s）探测期间，
            // 按钮状态（如「打开 Termux」）就该正确亮起，不能等整轮结束。
            status = s.copy(probing = true)
            var full = s
            var alive = false
            if (s.termuxInstalled && s.termuxPerm) {
                kotlinx.coroutines.coroutineScope {
                    // 端口探活走独立 HTTP 通道，先并行发出（不占 Termux 通道）。
                    val aliveD = async { McpHostManager.hostAliveDetail(context) }

                    // ── 步骤 1：RUN_COMMAND 通道（echo ok 单次往返）──
                    // 独立成步：卡在这即通道问题（Termux 冷启动 / 被冻结解冻 / 放行未开）。
                    step("检测 RUN_COMMAND 通道（echo ok）")
                    val watcher = launch {
                        kotlinx.coroutines.delay(4000)
                        log("仍在等待 RUN_COMMAND 应答…（Termux 冷启动或被系统冻结后首次命令较慢；" +
                            "常开 Termux 通知常驻 + Wakelock 并对其关闭电池优化可避免）")
                    }
                    // 状态检查 12s 上限：不陪跑 TermuxExec 的 90s 兜底。
                    // 超时 = 通道死（Termux 未运行/被冻结），给出明确动作指引。
                    val (allowed, rdetail) = kotlinx.coroutines.withTimeoutOrNull(12_000) {
                        McpHostManager.checkRunCommandAllowed(context)
                    } ?: (false to "12s 无应答（Termux 可能未运行或被系统冻结）")
                    watcher.cancel()
                    log("RUN_COMMAND 通道：${if (allowed) "通过（$rdetail）" else "未通过：$rdetail —— 可点「打开 Termux」拉起后重试，并检查 ~/.termux/termux.properties 的 allow-external-apps=true"}")

                    // ── 步骤 2：host 安装（通道已热，秒回）──
                    val inst = if (allowed) {
                        step("检查 host 安装")
                        McpHostManager.isHostInstalled(context)
                    } else false
                    log("host 已安装：$inst")

                    // ── 步骤 3：端口探活（并行已发出，此时收结果）──
                    step("探活服务端口")
                    val (a, adetail) = aliveD.await()
                    alive = a
                    log("探活 ${McpHostManager.HOST_URL}：${if (a) "应答正常（$adetail）" else "无应答（$adetail）"}")
                    full = s.copy(termuxAllowed = allowed, hostInstalled = inst)
                }
            } else {
                step("探活服务端口")
                val (a, adetail) = McpHostManager.hostAliveDetail(context)
                alive = a
                log("探活 ${McpHostManager.HOST_URL}：${if (a) "应答正常（$adetail）" else "无应答（$adetail）"}")
            }
            status = full.copy(hostAlive = alive)
            step(null)
            log("── 状态检查完成（总耗时 ${System.currentTimeMillis() - t0}ms）──")
        }
    }
    androidx.compose.runtime.LaunchedEffect(Unit) { refresh() }

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
                RuntimeBridge.upsertMcpServer("termux", McpHostManager.HOST_URL)
            }.fold(
                onSuccess = { log("已更新（新工具重启会话后生效）") },
                onFailure = { log("失败：${it.message}") },
            )
            step(null)
            busy = false
            refresh()
        }
    }

    Section(title = "MCP 服务（在 Termux 中运行）") {
        // ── 折叠逻辑：五灯全绿自动收起（一行摘要）；异常自动展开 ──
        val allGreen = !status.probing && status.termuxInstalled && status.termuxPerm &&
            status.termuxAllowed && status.hostInstalled && status.hostAlive
        var expanded by androidx.compose.runtime.saveable.rememberSaveable {
            mutableStateOf(false)
        }
        var prevGreen by androidx.compose.runtime.saveable.rememberSaveable {
            mutableStateOf(false)
        }
        androidx.compose.runtime.LaunchedEffect(allGreen) {
            if (allGreen && !prevGreen) expanded = false // 变绿瞬间收起
            if (!allGreen) expanded = true // 有异常自动展开
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
                Text("探测状态…", style = MaterialTheme.typography.labelMedium,
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
        if (expanded && !status.probing) {
            McpStatusLine(status.termuxInstalled, "Termux 已安装")
            McpStatusLine(status.termuxPerm, "RUN_COMMAND 权限")
            McpStatusLine(status.termuxAllowed, "外部应用放行（allow-external-apps）")
            // ── 未放行时给一键指引：复制命令到 Termux 执行 ──────
            if (!status.termuxAllowed) {
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
            }
            if (!status.termuxPerm) {
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
            McpStatusLine(status.hostInstalled, "MCP 服务已安装")
            McpStatusLine(status.hostAlive, "服务运行中")
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
                                RuntimeBridge.upsertMcpServer("termux", McpHostManager.HOST_URL)
                            }
                            .onFailure { log("失败：${it.message}") }
                        busy = false
                        refresh()
                    }
                }
            }
            CompactButton(text = "打开 Termux", enabled = !busy && status.termuxInstalled) {
                log("拉起 Termux 应用（前台）…")
                if (!McpHostManager.startTermuxApp(context)) {
                    log("拉起失败：找不到 Termux 启动入口（未安装？）")
                }
            }
            CompactButton(text = "刷新", enabled = !status.probing) {
                status = McpStatus(probing = true); refresh()
            }
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
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    "诊断日志（${logLines.size} 条）",
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.weight(1f),
                )
                CompactButton(text = "清空", onClick = { logLines = emptyList() })
            }
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
                        fontFamily = FontFamily.Monospace,
                        color = MaterialTheme.colorScheme.outline,
                    )
                }
            }
        }
        Text(
            "一键安装并启动 MCP 服务，之后重启会话即可使用。MCP 服务器在 " +
                "Termux 中运行（添加时填 npx 命令即可，首次使用会自动下载）。",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
    }

    Section(
        title = "MCP 服务器",
        action = {
            IconButton(onClick = { editServer = "" to "" }) {
                Icon(Icons.Filled.Add, contentDescription = "添加 MCP")
            }
        },
    ) {
        if (servers.isEmpty()) {
            Text("（空）添加一个 MCP 服务器，例如：", color = MaterialTheme.colorScheme.outline, style = MaterialTheme.typography.labelSmall)
            Text(
                "npx -y @modelcontextprotocol/server-filesystem /sdcard",
                style = MaterialTheme.typography.labelSmall, fontFamily = FontFamily.Monospace,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        for (s in servers) {
            Card(Modifier.fillMaxWidth()) {
                Row(Modifier.padding(12.dp), verticalAlignment = Alignment.CenterVertically) {
                    Column(Modifier.weight(1f)) {
                        Text(s.alias, style = MaterialTheme.typography.titleSmall)
                        Text(s.command, style = MaterialTheme.typography.bodySmall,
                            fontFamily = FontFamily.Monospace, color = MaterialTheme.colorScheme.outline)
                        Text("工具名前缀：${s.alias}__ · ${if (status.hostAlive) "运行中" else "未运行"}",
                            style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.outline)
                    }
                    IconButton(onClick = { applyServers(servers.filter { it != s }) }) {
                        Icon(Icons.Filled.Delete, contentDescription = "删除")
                    }
                }
            }
        }
    }

    editServer?.let { initial ->
        var alias by remember(initial) { mutableStateOf(initial.first) }
        var command by remember(initial) { mutableStateOf(initial.second) }
        androidx.compose.material3.AlertDialog(
            onDismissRequest = { editServer = null },
            title = { Text("添加 MCP 服务器") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(value = alias, onValueChange = { alias = it }, label = { Text("名称") })
                    OutlinedTextField(value = command, onValueChange = { command = it }, label = { Text("启动命令") })
                    Text("完整命令行，如 npx -y @modelcontextprotocol/server-memory", style = MaterialTheme.typography.labelSmall, color = MaterialTheme.colorScheme.outline)
                }
            },
            confirmButton = {
                SoftButton(enabled = alias.isNotBlank() && command.isNotBlank() && !busy, onClick = {
                    editServer = null
                    applyServers(servers + McpHostManager.McpServerEntry(alias.trim(), command.trim()))
                }) { Text("添加") }
            },
            dismissButton = { SoftButton(onClick = { editServer = null }) { Text("取消") } },
        )
    }
}
