package dev.openslate.mobile.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.SessionSummaryUi
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 历史会话页：列出数据库中的 runs（最近 50 条），点击切换续聊。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun HistoryScreen(onBack: () -> Unit, onOpened: () -> Unit) {
    var sessions by remember { mutableStateOf<List<SessionSummaryUi>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }

    // 加载（FFI 同步调用，轻量查询走后台线程足够快，这里简单阻塞一帧内）。
    androidx.compose.runtime.LaunchedEffect(Unit) {
        // FFI 阻塞查询（50 runs × 消息摘要）必须离主线程。
        sessions = kotlinx.coroutines.withTimeoutOrNull(3000) {
            kotlinx.coroutines.withContext(kotlinx.coroutines.Dispatchers.IO) {
                RuntimeBridge.listSessions()
            }
        } ?: emptyList()
        loading = false
    }

    val fmt = remember { SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text("历史会话（${sessions.size}）") },
                navigationIcon = {
                    IconButton(onClick = onBack) {
                        Icon(Icons.AutoMirrored.Filled.ArrowBack, contentDescription = "返回")
                    }
                },
            )
        },
    ) { padding ->
        if (loading) {
            Column(Modifier.padding(padding).padding(16.dp)) { Text("加载中…") }
            return@Scaffold
        }
        LazyColumn(
            Modifier.fillMaxSize().padding(padding),
            contentPadding = androidx.compose.foundation.layout.PaddingValues(
                horizontal = 12.dp, vertical = 8.dp,
            ),
            verticalArrangement = Arrangement.spacedBy(8.dp),
        ) {
            items(sessions, key = { it.id }) { s ->
                Card(
                    modifier = Modifier.fillMaxWidth(),
                    onClick = {
                        if (RuntimeBridge.openSession(s.id)) onOpened()
                    },
                ) {
                    Row(
                        Modifier.padding(12.dp),
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(
                                s.title.ifBlank { "(无标题)" },
                                style = MaterialTheme.typography.titleSmall,
                                maxLines = 1,
                            )
                            Text(
                                "${fmt.format(Date(s.startedMs))} · ${statusLabel(s.status)}" +
                                    if (s.costUsd > 0.0) " · $${"%.4f".format(s.costUsd)}" else "",
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                        Text(
                            if (s.status == "running") "●" else "",
                            color = MaterialTheme.colorScheme.primary,
                        )
                    }
                }
            }
            if (sessions.isEmpty()) {
                item { Text("（还没有历史会话）", color = MaterialTheme.colorScheme.outline) }
            }
        }
    }
}

private fun statusLabel(status: String): String = when (status) {
    "running" -> "进行中"
    "completed" -> "已完成"
    "interrupted" -> "被中断"
    "cancelled" -> "已取消"
    "failed" -> "失败"
    else -> status
}
