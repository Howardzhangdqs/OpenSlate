package dev.openslate.mobile.ui

import androidx.compose.foundation.ExperimentalFoundationApi
import androidx.compose.foundation.combinedClickable
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
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
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
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.SessionSummaryUi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

private const val PAGE_SIZE = 50

/**
 * 历史会话页：分页列出数据库中的 runs（每页 50 条），点击切换续聊，
 * 长按删除非活动会话（DB 记录 + 本地 transcript 一并清理）。
 */
@OptIn(ExperimentalMaterial3Api::class, ExperimentalFoundationApi::class)
@Composable
fun HistoryScreen(onBack: () -> Unit, onOpened: () -> Unit) {
    val scope = androidx.compose.runtime.rememberCoroutineScope()
    var sessions by remember { mutableStateOf<List<SessionSummaryUi>>(emptyList()) }
    var loading by remember { mutableStateOf(true) }
    var loadError by remember { mutableStateOf(false) }
    var canLoadMore by remember { mutableStateOf(false) }
    var loadingMore by remember { mutableStateOf(false) }
    var pendingDelete by remember { mutableStateOf<SessionSummaryUi?>(null) }
    var activeRunId by remember { mutableStateOf<String?>(null) }

    fun refresh() {
        scope.launch {
            loading = true
            val page = withTimeoutOrNull(3000) {
                withContext(Dispatchers.IO) { RuntimeBridge.listSessions(0) }
            }
            if (page == null) {
                // 超时/失败：显式错误态 + 重试，不静默伪装成"无历史"。
                loadError = true
                sessions = emptyList()
                canLoadMore = false
            } else {
                loadError = false
                sessions = page
                canLoadMore = page.size >= PAGE_SIZE
            }
            activeRunId = withContext(Dispatchers.IO) { RuntimeBridge.currentRunId() }
            loading = false
        }
    }

    LaunchedEffect(Unit) { refresh() }

    // LLM 标题异步到达（run_title）→ 原位替换列表里的占位标题。
    // 只在已完成首次加载后触发刷新（首刷自己就有 LaunchedEffect(Unit)）。
    val state by RuntimeBridge.state.collectAsState()
    LaunchedEffect(state.titlesVersion) {
        if (state.titlesVersion > 0 && !loading) refresh()
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
        if (loadError) {
            Column(Modifier.padding(padding).padding(16.dp)) {
                Text("加载失败（运行时未就绪或查询超时）", color = MaterialTheme.colorScheme.error)
                SoftButton(onClick = { refresh() }) { Text("重试") }
            }
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
                    modifier = Modifier
                        .fillMaxWidth()
                        .combinedClickable(
                            onClick = {
                                // openSession 是同步 FFI 调用（DB 读取 + 会话装配），
                                // 移出主线程；成功后再回主线程回调导航。
                                scope.launch(Dispatchers.IO) {
                                    val ok = RuntimeBridge.openSession(s.id)
                                    withContext(Dispatchers.Main) {
                                        if (ok) onOpened()
                                    }
                                }
                            },
                            // 活动会话由 Rust 侧拒绝删除；长按仅对非活动会话弹确认。
                            onLongClick = { if (s.id != activeRunId) pendingDelete = s },
                        ),
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
                            if (s.id == activeRunId) "●" else "",
                            color = MaterialTheme.colorScheme.primary,
                        )
                    }
                }
            }
            if (sessions.isEmpty()) {
                item { Text("（还没有历史会话）", color = MaterialTheme.colorScheme.outline) }
            }
            if (canLoadMore) {
                item {
                    SoftButton(
                        modifier = Modifier.fillMaxWidth(),
                        enabled = !loadingMore,
                        onClick = {
                            scope.launch {
                                loadingMore = true
                                val page = withTimeoutOrNull(5000) {
                                    withContext(Dispatchers.IO) {
                                        RuntimeBridge.listSessions(sessions.size)
                                    }
                                } ?: emptyList()
                                sessions = sessions + page
                                canLoadMore = page.size >= PAGE_SIZE
                                loadingMore = false
                            }
                        },
                    ) {
                        Text(if (loadingMore) "加载中…" else "加载更多")
                    }
                }
            }
            if (sessions.isNotEmpty()) {
                item {
                    Text(
                        "长按会话可删除（活动会话除外）",
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.fillMaxWidth().padding(top = 4.dp),
                        textAlign = androidx.compose.ui.text.style.TextAlign.Center,
                    )
                }
            }
        }
    }

    // 删除确认：DB 级联删除 + 本地 transcript 清理，不可恢复，需显式确认。
    pendingDelete?.let { target ->
        AlertDialog(
            onDismissRequest = { pendingDelete = null },
            title = { Text("删除会话") },
            text = {
                Text("确定删除「${target.title.ifBlank { "(无标题)" }}」？对话记录与本地备份将一并删除，不可恢复。")
            },
            confirmButton = {
                SoftButton(
                    tint = MaterialTheme.colorScheme.error,
                    onClick = {
                        pendingDelete = null
                        scope.launch {
                            // deleteSession 内含 FFI 同步 DB 删除，离主线程执行；
                            // 无论成败都刷新列表（反映真实状态，失败项仍保留）。
                            withContext(Dispatchers.IO) { RuntimeBridge.deleteSession(target.id) }
                            refresh()
                        }
                    },
                ) {
                    Text("删除", color = MaterialTheme.colorScheme.error)
                }
            },
            dismissButton = {
                SoftButton(onClick = { pendingDelete = null }) { Text("取消") }
            },
        )
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
