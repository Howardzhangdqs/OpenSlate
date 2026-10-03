package dev.openslate.mobile.ui

import androidx.compose.foundation.gestures.detectTapGestures
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalFocusManager
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge

/**
 * API Key 统一管理页：所有 Provider 的密钥状态一览，点击任一条设置/更新密钥。
 * 密钥加密存 Android Keystore，按 Provider 内部 ID 关联（与显示名改名无关）。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun KeysScreen(onBack: () -> Unit) {
    val state by RuntimeBridge.state.collectAsState()
    val config = state.config
    // 批量预取（IO 线程）：Keystore 解密是 binder IPC + AES-GCM，主线程调用会卡帧。
    var keyStatus by remember { mutableStateOf<Map<String, Boolean>>(emptyMap()) }
    LaunchedEffect(config.providers) {
        val providers = config.providers
        keyStatus = kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
            providers.associate { it.name to RuntimeBridge.hasPersistedKey(it.name) }
        }
    }
    var editing by remember { mutableStateOf<String?>(null) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("API 密钥管理") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "返回")
                    }
                },
            )
        },
    ) { padding ->
        if (config.providers.isEmpty()) {
            Column(Modifier.padding(padding).padding(16.dp)) {
                Text("还没有 Provider。先到 设置 → Provider 新增后再配置密钥。", color = MaterialTheme.colorScheme.outline)
            }
            return@Scaffold
        }
        LazyColumn(
            Modifier.fillMaxSize().padding(padding),
            contentPadding = PaddingValues(horizontal = 12.dp, vertical = 8.dp),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(config.providers, key = { it.name }) { p ->
                // null = 预取进行中（避免先闪"未配置"再变"已存"）。
                val hasKey = keyStatus[p.name]
                Card(
                    modifier = Modifier.fillMaxWidth(),
                    onClick = { editing = p.name },
                ) {
                    Row(
                        Modifier.padding(12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(p.displayName, style = MaterialTheme.typography.titleSmall)
                            Text(
                                when (hasKey) {
                                    true -> "已配置 · 点击更换"
                                    false -> "未配置 · 点击添加"
                                    null -> "检查中…"
                                },
                                style = MaterialTheme.typography.labelSmall,
                                color = when (hasKey) {
                                    true -> MaterialTheme.colorScheme.primary
                                    false -> MaterialTheme.colorScheme.error
                                    null -> MaterialTheme.colorScheme.outline
                                },
                            )
                        }
                        if (hasKey == true) {
                            Icon(
                                Icons.Filled.Check,
                                contentDescription = "已存",
                                tint = MaterialTheme.colorScheme.primary,
                            )
                        }
                    }
                }
            }
        }
    }

    editing?.let { name ->
        ApiKeyDialog(
            providerId = name,
            display = config.providers.find { it.name == name }?.displayName ?: name,
            onDismiss = { editing = null },
            onSave = { key ->
                RuntimeBridge.setApiKeyAndPersist(name, key)
                keyStatus = keyStatus + (name to true)
                editing = null
            },
        )
    }
}

/** 单个 Provider 的密钥设置对话框（KeysScreen 与 Provider 编辑共用语义）。 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
internal fun ApiKeyDialog(
    providerId: String,
    display: String,
    onDismiss: () -> Unit,
    onSave: (String) -> Unit,
) {
    var key by remember { mutableStateOf("") }
    // 未聚焦单行；聚焦后随内容换行自动展开高度（长密钥编辑不用横向滚动）。
    var keyFocused by remember { mutableStateOf(false) }
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("$display 的 API 密钥") },
        text = {
            Column(
                Modifier.dismissKeyboardOnTap(),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                OutlinedTextField(
                    key, { key = it },
                    label = { Text("API 密钥") },
                    visualTransformation = PasswordVisualTransformation(),
                    singleLine = !keyFocused,
                    modifier = Modifier
                        .fillMaxWidth()
                        .onFocusChanged { keyFocused = it.isFocused },
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

/** 点击空白区域收起输入法并清除输入框焦点（Dialog 独立窗口，页面级手势覆盖不到）。 */
@Composable
internal fun Modifier.dismissKeyboardOnTap(): Modifier {
    val focusManager = LocalFocusManager.current
    val keyboard = LocalSoftwareKeyboardController.current
    return pointerInput(Unit) {
        detectTapGestures(onTap = {
            keyboard?.hide()
            focusManager.clearFocus()
        })
    }
}
