package dev.openslate.mobile.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
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
import androidx.compose.material.icons.filled.Delete
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
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.ModelUi
import dev.openslate.mobile.bridge.ProviderUi
import dev.openslate.mobile.bridge.RuntimeBridge

/**
 * 模型配置页：提供商 / 模型条目 / 级别绑定 / API key。
 * 全部改动经 ClientMsg CRUD → Rust persist 层落盘 openslate.toml →
 * config_changed 事件回刷（协议复用，与桌面 TUI 同一条写回链）。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SettingsScreen(onBack: () -> Unit) {
    val state by RuntimeBridge.state.collectAsState()
    val config = state.config

    var editProvider by remember { mutableStateOf<ProviderUi?>(null) }
    var editProviderIsNew by remember { mutableStateOf(false) }
    var editModel by remember { mutableStateOf<ModelUi?>(null) }
    var editModelIsNew by remember { mutableStateOf(false) }
    var keyProvider by remember { mutableStateOf<String?>(null) }
    var tab by androidx.compose.runtime.saveable.rememberSaveable { mutableStateOf(0) }
    var toolTab by androidx.compose.runtime.saveable.rememberSaveable { mutableStateOf(0) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("设置") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "返回")
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
                val tabs = listOf("会话" to 0, "模型" to 1, "提供商" to 2, "工具" to 3)
                for ((label, idx) in tabs) {
                    androidx.compose.material3.Tab(
                        selected = tab == idx,
                        onClick = { tab = idx },
                        text = { Text(label) },
                    )
                }
            }

            // ── 工具域二级 Tab：吸顶固定（在滚动容器外），紧贴主 Tab ──
            if (tab == 3) {
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
                        // ── 会话域：当前模型 + 级别绑定 ─────────────
                        Section(title = "当前模型（会话即时切换）") {
                            val aliasChoices = (config.levels.keys + config.models.map { it.entry }).distinct()
                            ChoiceRow(
                                label = "使用别名",
                                options = aliasChoices,
                                selected = state.modelAlias,
                                onSelect = { RuntimeBridge.setModelAlias(it) },
                            )
                        }
                        Section(title = "级别绑定（main = 对话主力 / fast = 摘要压缩）") {
                            for (level in config.levels.keys.sorted()) {
                                ChoiceRow(
                                    label = level,
                                    options = config.models.map { it.entry },
                                    selected = config.levels[level] ?: "",
                                    onSelect = { RuntimeBridge.setLevel(level, it) },
                                )
                            }
                        }
                    }
                    1 -> {
                        // ── 模型域：模型条目 CRUD ───────────────────
                        Section(
                            title = "模型",
                            action = {
                                IconButton(onClick = { editModel = ModelUi("", "", ""); editModelIsNew = true }) {
                                    Icon(Icons.Filled.Add, contentDescription = "新增模型")
                                }
                            },
                        ) {
                            if (config.models.isEmpty()) Text("（空）", color = MaterialTheme.colorScheme.outline)
                            for (m in config.models) {
                                Card(
                                    modifier = Modifier.fillMaxWidth(),
                                    onClick = { editModel = m; editModelIsNew = false },
                                ) {
                                    Row(
                                        Modifier.padding(12.dp),
                                        verticalAlignment = Alignment.CenterVertically,
                                    ) {
                                        Column(Modifier.weight(1f)) {
                                            Text(m.entry, style = MaterialTheme.typography.titleSmall)
                                            Text(
                                                "${m.provider} · ${m.modelId}",
                                                style = MaterialTheme.typography.bodySmall,
                                                color = MaterialTheme.colorScheme.outline,
                                            )
                                        }
                                        Icon(Icons.Filled.Edit, contentDescription = "编辑", tint = MaterialTheme.colorScheme.outline)
                                    }
                                }
                            }
                        }
                    }
                    2 -> {
                        // ── 提供商域：提供商 CRUD + API key ─────────
                        Section(
                            title = "提供商",
                            action = {
                                IconButton(onClick = { editProvider = ProviderUi("", "", "", null); editProviderIsNew = true }) {
                                    Icon(Icons.Filled.Add, contentDescription = "新增提供商")
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
                                            Text(p.name, style = MaterialTheme.typography.titleSmall)
                                            Text(
                                                p.baseUrl,
                                                style = MaterialTheme.typography.bodySmall,
                                                fontFamily = FontFamily.Monospace,
                                                color = MaterialTheme.colorScheme.outline,
                                            )
                                            val hasKey = RuntimeBridge.hasPersistedKey(p.name)
                                            Row(verticalAlignment = Alignment.CenterVertically) {
                                                if (hasKey) {
                                                    Icon(
                                                        imageVector = Icons.Filled.Check,
                                                        contentDescription = "已存",
                                                        tint = MaterialTheme.colorScheme.primary,
                                                        modifier = Modifier.height(12.dp).width(12.dp),
                                                    )
                                                }
                                                Text(
                                                    if (hasKey) "  API key 已存（Keystore）" else "API key 未配置",
                                                    style = MaterialTheme.typography.labelSmall,
                                                    color = if (hasKey) MaterialTheme.colorScheme.primary
                                                    else MaterialTheme.colorScheme.error,
                                                )
                                            }
                                        }
                                        IconButton(onClick = { keyProvider = p.name }) {
                                            Icon(Icons.Filled.Key, contentDescription = "设置 API Key")
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
                    else -> {
                        // ── 工具域（二级 Tab 在上方吸顶）：MCP / 命令行 ──
                        if (toolTab == 0) McpSection() else ExecBackendSection()
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
            onDismiss = { editProvider = null },
            onSave = { name, baseUrl, env, adapter ->
                RuntimeBridge.upsertProvider(name, baseUrl, env, adapter)
                editProvider = null
            },
        )
    }
    editModel?.let { m ->
        EditModelDialog(
            initial = m,
            isNew = editModelIsNew,
            providers = config.providers.map { it.name },
            onDismiss = { editModel = null },
            onSave = { ui ->
                RuntimeBridge.upsertModel(ui)
                editModel = null
            },
        )
    }
    keyProvider?.let { name ->
        ApiKeyDialog(
            provider = name,
            onDismiss = { keyProvider = null },
            onSave = { key ->
                RuntimeBridge.setApiKeyAndPersist(name, key)
                keyProvider = null
            },
        )
    }
}

@Composable
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

@Composable
private fun ChoiceRow(label: String, options: List<String>, selected: String, onSelect: (String) -> Unit) {
    var expanded by remember { mutableStateOf(false) }
    Column {
        Text(label, style = MaterialTheme.typography.labelMedium, color = MaterialTheme.colorScheme.outline)
        Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
            for (opt in options) {
                val isSel = opt == selected
                TextButton(onClick = { onSelect(opt); expanded = false }) {
                    Text(
                        opt + if (isSel) " ✓" else "",
                        color = if (isSel) MaterialTheme.colorScheme.primary else MaterialTheme.colorScheme.onSurface,
                    )
                }
            }
        }
    }
}

@Composable
private fun EditProviderDialog(
    initial: ProviderUi,
    isNew: Boolean,
    onDismiss: () -> Unit,
    onSave: (name: String, baseUrl: String, apiKeyEnv: String, adapter: String?) -> Unit,
) {
    var name by remember { mutableStateOf(initial.name) }
    var baseUrl by remember { mutableStateOf(initial.baseUrl) }
    var apiKeyEnv by remember { mutableStateOf(initial.apiKeyEnv) }
    var adapter by remember { mutableStateOf(initial.adapter ?: "openai") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (isNew) "新增提供商" else "编辑提供商") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(name, { name = it }, label = { Text("名称（如 zhipu / deepseek）") }, enabled = isNew)
                OutlinedTextField(baseUrl, { baseUrl = it }, label = { Text("Base URL") })
                OutlinedTextField(
                    apiKeyEnv, { apiKeyEnv = it },
                    label = { Text("API Key 环境变量名（占位，密钥在 Key 图标处设置）") },
                    enabled = false,
                )
                // 协议适配：下拉选择（openai/anthropic/gemini/ollama）。
                Text("协议适配", style = MaterialTheme.typography.labelMedium)
                DropdownChoice(
                    options = listOf("openai", "anthropic", "gemini", "ollama"),
                    selected = adapter,
                    onSelect = { adapter = it },
                )
            }
        },
        confirmButton = {
            TextButton(
                onClick = { if (name.isNotBlank() && baseUrl.isNotBlank()) onSave(name.trim(), baseUrl.trim(), apiKeyEnv.ifBlank { (name.uppercase() + "_API_KEY") }, adapter.ifBlank { null }) },
            ) { Text("保存") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("取消") } },
    )
}

/** 选项不多的下拉（Material3 菜单风格，单行胶囊）。 */
@Composable
private fun DropdownChoice(options: List<String>, selected: String, onSelect: (String) -> Unit) {
    var expanded by remember { mutableStateOf(false) }
    Column {
        OutlinedTextField(
            value = selected,
            onValueChange = {},
            readOnly = true,
            label = { Text(selected) },
            trailingIcon = {
                TextButton(onClick = { expanded = !expanded }) { Text(if (expanded) "▲" else "▼") }
            },
            modifier = Modifier.fillMaxWidth(),
        )
        if (expanded) {
            Card(Modifier.fillMaxWidth()) {
                Column {
                    for (opt in options) {
                        TextButton(
                            onClick = { onSelect(opt); expanded = false },
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Text(opt + if (opt == selected) " ✓" else "")
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun EditModelDialog(
    initial: ModelUi,
    isNew: Boolean,
    providers: List<String>,
    onDismiss: () -> Unit,
    onSave: (ModelUi) -> Unit,
) {
    var entry by remember { mutableStateOf(initial.entry) }
    var provider by remember { mutableStateOf(initial.provider.ifBlank { providers.firstOrNull() ?: "" }) }
    var modelId by remember { mutableStateOf(initial.modelId) }
    var showAdvanced by remember { mutableStateOf(false) }
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
            Column(Modifier.verticalScroll(rememberScrollState()), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(entry, { entry = it }, label = { Text("别名（如 glm-main）") }, enabled = isNew)
                if (providers.isNotEmpty()) {
                    Text("提供商", style = MaterialTheme.typography.labelMedium)
                    Row(Modifier.horizontalScroll(rememberScrollState()), horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                        for (p in providers) {
                            TextButton(onClick = { provider = p }) {
                                Text(p + if (p == provider) " ✓" else "")
                            }
                        }
                    }
                } else {
                    OutlinedTextField(provider, { provider = it }, label = { Text("提供商名") })
                }
                OutlinedTextField(modelId, { modelId = it }, label = { Text("模型 ID（如 glm-4.7）") })

                TextButton(onClick = { showAdvanced = !showAdvanced }) {
                    Text(if (showAdvanced) "收起高级选项 ▲" else "高级选项（能力 / 限额 / 计价） ▼")
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
            }
        },
        confirmButton = {
            TextButton(
                onClick = {
                    if (entry.isNotBlank() && provider.isNotBlank() && modelId.isNotBlank()) {
                        onSave(
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
        dismissButton = { TextButton(onClick = onDismiss) { Text("取消") } },
    )
}

@Composable
private fun Toggle(label: String, value: Boolean, onToggle: () -> Unit) {
    TextButton(onClick = onToggle) {
        Text((if (value) "● " else "○ ") + label)
    }
}

@Composable
private fun ApiKeyDialog(provider: String, onDismiss: () -> Unit, onSave: (String) -> Unit) {
    var key by remember { mutableStateOf("") }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("设置 $provider 的 API Key") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    key, { key = it },
                    label = { Text("API Key") },
                    visualTransformation = PasswordVisualTransformation(),
                )
                Text(
                    "加密存入 Android Keystore，注入 Rust 运行时内存；不会写入配置文件。",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        },
        confirmButton = {
            TextButton(onClick = { if (key.isNotBlank()) onSave(key.trim()) }) { Text("保存并生效") }
        },
        dismissButton = { TextButton(onClick = onDismiss) { Text("取消") } },
    )
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
            tint = if (ok) androidx.compose.ui.graphics.Color(0xFF1E8A44)
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
    var backends by remember {
        mutableStateOf(RuntimeBridge.execBackends())
    }
    fun isEnabled(name: String) = backends.split(',').map { it.trim() }.contains(name)
    /** 允许全不选（bash 功能整体下线）；空集持久化为空串。 */
    fun setEnabled(name: String, on: Boolean) {
        val set = linkedSetOf<String>()
        if (isEnabled("native") || name == "native" && on) set.add("native")
        if (isEnabled("termux") || name == "termux" && on) set.add("termux")
        if (name == "native" && !on) set.remove("native")
        if (name == "termux" && !on) set.remove("termux")
        val csv = set.joinToString(",")
        RuntimeBridge.setExecBackendsAndPersist(csv)
        backends = csv
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
private fun McpSection() {
    val context = androidx.compose.ui.platform.LocalContext.current
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    var status by remember { mutableStateOf(McpStatus()) }
    var servers by remember { mutableStateOf(McpHostManager.loadServers(context)) }
    var installLog by remember { mutableStateOf<String?>(null) }
    var busy by remember { mutableStateOf(false) }
    var editServer by remember { mutableStateOf<Pair<String, String>?>(null) } // alias to command

    fun refresh() {
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            val s = McpStatus(probing = false,
                termuxInstalled = dev.openslate.mobile.bridge.TermuxExec.isTermuxInstalled(context),
                termuxPerm = dev.openslate.mobile.bridge.TermuxExec.hasPermission(context),
                termuxAllowed = false, hostInstalled = false, hostAlive = McpHostManager.isHostAlive(context))
            val full = if (s.termuxInstalled && s.termuxPerm) {
                val triple = McpHostManager.checkTermux(context)
                s.copy(termuxAllowed = triple.third, hostInstalled = McpHostManager.isHostInstalled(context))
            } else s
            status = full
        }
    }
    androidx.compose.runtime.LaunchedEffect(Unit) { refresh() }

    fun applyServers(next: List<McpHostManager.McpServerEntry>) {
        servers = next
        scope.launch(kotlinx.coroutines.Dispatchers.IO) {
            busy = true
            installLog = "推送清单并重启服务…"
            runCatching {
                McpHostManager.saveServers(context, next)
                if (status.hostInstalled) {
                    McpHostManager.pushManifest(context)
                    McpHostManager.restartHost(context)
                }
                RuntimeBridge.upsertMcpServer(
                    "termux", McpHostManager.HOST_URL, McpHostManager.authHeaders(context),
                )
            }.fold(
                onSuccess = { installLog = "已更新（新工具重启会话后生效）" },
                onFailure = { installLog = "失败：${it.message}" },
            )
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
                        tint = androidx.compose.ui.graphics.Color(0xFF1E8A44),
                        modifier = Modifier.height(16.dp).width(16.dp),
                    )
                    Text("  一切就绪 · 服务运行中", style = MaterialTheme.typography.labelMedium,
                        color = androidx.compose.ui.graphics.Color(0xFF1E8A44))
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
                    "在 Termux 中粘贴执行下面这条命令即可放行（执行后点「刷新」）：",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
                Row(verticalAlignment = Alignment.CenterVertically) {
                    Text(
                        "mkdir -p ~/.termux && echo \"allow-external-apps=true\" >> ~/.termux/termux.properties && termux-reload-settings",
                        style = MaterialTheme.typography.labelSmall,
                        fontFamily = FontFamily.Monospace,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.weight(1f),
                    )
                    CompactButton(text = "复制命令", onClick = {
                        val cm = context.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                            as android.content.ClipboardManager
                        cm.setPrimaryClip(
                            android.content.ClipData.newPlainText(
                                "termux",
                                "mkdir -p ~/.termux && echo \"allow-external-apps=true\" >> ~/.termux/termux.properties && termux-reload-settings",
                            )
                        )
                        installLog = "命令已复制，粘贴到 Termux 执行后点「刷新」"
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
                    container = androidx.compose.ui.graphics.Color(0xFFDCEFDD),
                    content = androidx.compose.ui.graphics.Color(0xFF1E8A44),
                ) {
                    busy = true
                    scope.launch(kotlinx.coroutines.Dispatchers.IO) {
                        runCatching { McpHostManager.installHost(context) { installLog = it } }
                            .onSuccess { msg ->
                                installLog = msg
                                RuntimeBridge.upsertMcpServer(
                                    "termux", McpHostManager.HOST_URL, McpHostManager.authHeaders(context),
                                )
                            }
                            .onFailure { installLog = "失败：${it.message}" }
                        busy = false
                        refresh()
                    }
                }
            }
            CompactButton(text = "刷新", enabled = !status.probing) {
                status = McpStatus(probing = true); refresh()
            }
        }
        installLog?.let {
            Text(it, style = MaterialTheme.typography.labelSmall,
                fontFamily = FontFamily.Monospace, color = MaterialTheme.colorScheme.outline)
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
                TextButton(enabled = alias.isNotBlank() && command.isNotBlank() && !busy, onClick = {
                    editServer = null
                    applyServers(servers + McpHostManager.McpServerEntry(alias.trim(), command.trim()))
                }) { Text("添加") }
            },
            dismissButton = { TextButton(onClick = { editServer = null }) { Text("取消") } },
        )
    }
}
