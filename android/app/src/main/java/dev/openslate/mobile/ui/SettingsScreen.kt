package dev.openslate.mobile.ui

import androidx.compose.foundation.horizontalScroll
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
import androidx.compose.material.icons.filled.Edit
import androidx.compose.material.icons.filled.Key
import androidx.compose.material3.AlertDialog
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

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("模型配置") },
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
                .padding(padding)
                .verticalScroll(rememberScrollState())
                .padding(16.dp),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            // ── 当前模型 ─────────────────────────────────────────
            Section(title = "当前模型（会话即时切换）") {
                val aliasChoices = (config.levels.keys + config.models.map { it.entry }).distinct()
                ChoiceRow(
                    label = "使用别名",
                    options = aliasChoices,
                    selected = state.modelAlias,
                    onSelect = { RuntimeBridge.setModelAlias(it) },
                )
            }

            // ── 级别绑定 ─────────────────────────────────────────
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

            // ── 模型列表 ─────────────────────────────────────────
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

            // ── 提供商 ───────────────────────────────────────────
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
                                Text(
                                    if (hasKey) "API key ✓ 已存（Keystore）" else "API key 未配置",
                                    style = MaterialTheme.typography.labelSmall,
                                    color = if (hasKey) MaterialTheme.colorScheme.primary
                                    else MaterialTheme.colorScheme.error,
                                )
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
