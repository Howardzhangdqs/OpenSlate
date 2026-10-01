package dev.openslate.mobile.ui

import androidx.compose.animation.animateContentSize
import androidx.compose.foundation.clickable
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.ExpandLess
import androidx.compose.material.icons.filled.ExpandMore
import androidx.compose.material.icons.outlined.Psychology
import androidx.compose.material.icons.automirrored.filled.Send
import androidx.compose.material.icons.filled.History
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Stop
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.UiEntry

/**
 * Phase 1 聊天主界面：transcript 流 + 输入框 + 审批横幅。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ChatScreen() {
    val state by RuntimeBridge.state.collectAsState()
    val listState = rememberLazyListState()
    var showSettings by remember { mutableStateOf(false) }
    var showHistory by remember { mutableStateOf(false) }
    // 相邻的连续工具调用聚合为一组（圆角矩形容器内多个 chip），
    // 其余条目原样单行渲染。
    val renderItems: List<RenderItem> = remember(state.entries) { groupEntries(state.entries) }

    LaunchedEffect(state.entries.size) {
        if (renderItems.isNotEmpty()) {
            listState.animateScrollToItem(renderItems.lastIndex)
        }
    }

    if (showSettings) {
        SettingsScreen(onBack = { showSettings = false })
        return
    }
    if (showHistory) {
        HistoryScreen(
            onBack = { showHistory = false },
            onOpened = { showHistory = false },
        )
        return
    }

    Scaffold(
        topBar = {
            TopAppBar(
                title = {
                    Column {
                        Text("OpenSlate", style = MaterialTheme.typography.titleLarge)
                        Text(
                            text = when {
                                !state.ready -> "runtime 启动中…"
                                state.running ->
                                    "${state.modelAlias} · Step ${state.step} · 工具 ${state.toolCalls}"
                                else -> state.modelAlias.ifEmpty { "就绪" }
                            },
                            style = MaterialTheme.typography.labelSmall,
                        )
                    }
                },
                actions = {
                    IconButton(onClick = { showHistory = true }) {
                        Icon(
                            androidx.compose.material.icons.Icons.Filled.History,
                            contentDescription = "历史会话",
                        )
                    }
                    IconButton(onClick = { showSettings = true }) {
                        Icon(
                            androidx.compose.material.icons.Icons.Filled.Settings,
                            contentDescription = "模型配置",
                        )
                    }
                    TextButton(onClick = { RuntimeBridge.newSession() }) { Text("新会话") }
                },
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.surface,
                ),
            )
        },
        bottomBar = {
            if (state.pendingApproval != null) {
                ApprovalBanner(
                    pending = state.pendingApproval!!,
                    onAnswer = { id, choice -> RuntimeBridge.answerApproval(id, choice) },
                )
            }
        },
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding)
                .imePadding(),
        ) {
            Box(Modifier.weight(1f)) {
                if (!state.ready) {
                    Box(Modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
                        CircularProgressIndicator()
                    }
                } else {
                    LazyColumn(
                        state = listState,
                        modifier = Modifier.fillMaxSize(),
                        contentPadding = androidx.compose.foundation.layout.PaddingValues(
                            horizontal = 12.dp, vertical = 8.dp,
                        ),
                        verticalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        // key 稳定 → 滚动复用组合（Markdown 不重解析，老设备关键）。
                        // 组 key 用组首条目在 entries 中的索引（turn 进行中 entries
                        // 只追加、不改写既有段 → 稳定）。
                        itemsIndexed(renderItems, key = { _, item ->
                            when (item) {
                                is RenderItem.Single -> "s-${item.index}"
                                is RenderItem.ToolGroup -> "g-${item.startIdx}"
                            }
                        }) { _, item ->
                            when (item) {
                                is RenderItem.Single -> EntryRow(item.entry)
                                is RenderItem.ToolGroup -> ToolGroupCard(item.items)
                            }
                        }
                    }
                }
            }
            Composer(
                running = state.running,
                onSend = { text ->
                    RuntimeBridge.submit(text)
                },
                onCancel = { RuntimeBridge.cancel() },
            )
        }
    }
}

/** 渲染投影：相邻的连续工具调用聚合为一组，其余条目单行。 */
private sealed class RenderItem {
    data class Single(val entry: UiEntry, val index: Int) : RenderItem()
    data class ToolGroup(val items: List<UiEntry.ToolCall>, val startIdx: Int) : RenderItem()
}

private fun groupEntries(entries: List<UiEntry>): List<RenderItem> {
    val out = mutableListOf<RenderItem>()
    var i = 0
    while (i < entries.size) {
        val e = entries[i]
        if (e is UiEntry.ToolCall) {
            val start = i
            val group = mutableListOf<UiEntry.ToolCall>()
            while (i < entries.size && entries[i] is UiEntry.ToolCall) {
                group.add(entries[i] as UiEntry.ToolCall)
                i++
            }
            out.add(RenderItem.ToolGroup(group, start))
        } else {
            out.add(RenderItem.Single(e, i))
            i++
        }
    }
    return out
}

@Composable
private fun EntryRow(entry: UiEntry) {
    when (entry) {
        is UiEntry.User -> Bubble(entry.text, mine = true)
        is UiEntry.Assistant -> MarkdownBubble(entry.text)
        is UiEntry.Reasoning -> ReasoningBlock(entry.text, entry.meta)
        is UiEntry.Meta -> Text(
            entry.text,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.tertiary,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        // 分组投影后正常不会到达；保留兜底（sealed when 完整性）。
        is UiEntry.ToolCall -> ToolGroupCard(listOf(entry))
        is UiEntry.Delegate -> Text(
            (if (entry.done) "↳ agent " else "↻ agent ") + entry.agent,
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        is UiEntry.Approval -> Text(
            "⚑ ${entry.toolName} → ${entry.decision}",
            style = MaterialTheme.typography.labelMedium,
            modifier = Modifier.padding(horizontal = 8.dp),
        )
        UiEntry.StepBreak -> Spacer(Modifier.widthIn(min = 4.dp))
    }
}

/**
 * 工具组容器：圆角矩形包裹相邻工具 chip；单个 chip 点击各自展开
 * （命令 + 输出），容器尺寸随展开动画过渡。
 */
@Composable
private fun ToolGroupCard(items: List<UiEntry.ToolCall>) {
    Surface(
        shape = RoundedCornerShape(14.dp),
        color = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.45f),
        border = androidx.compose.foundation.BorderStroke(
            0.5.dp,
            MaterialTheme.colorScheme.outlineVariant.copy(alpha = 0.6f),
        ),
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp)
            .animateContentSize(),
    ) {
        Column(
            Modifier.padding(horizontal = 8.dp, vertical = 6.dp),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            items.forEach { tc -> ToolChipRow(tc) }
        }
    }
}

/** 单个工具 chip（与思维链 chip 同风格）：状态图标 + 名称 + 参数摘要，点击展开。 */
@Composable
private fun ToolChipRow(tc: UiEntry.ToolCall) {
    Column(Modifier.animateContentSize()) {
        Surface(
            shape = RoundedCornerShape(50),
            color = MaterialTheme.colorScheme.secondaryContainer.copy(alpha = 0.4f),
            border = androidx.compose.foundation.BorderStroke(
                0.5.dp,
                MaterialTheme.colorScheme.secondary.copy(alpha = 0.4f),
            ),
            onClick = { RuntimeBridge.toggleToolExpanded(tc) },
        ) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier.padding(start = 10.dp, end = 6.dp, top = 4.dp, bottom = 4.dp),
            ) {
                when (tc.status) {
                    "running" -> CircularProgressIndicator(
                        strokeWidth = 1.5.dp,
                        modifier = Modifier.size(13.dp),
                    )
                    "failed" -> Icon(
                        Icons.Filled.Close,
                        contentDescription = null,
                        modifier = Modifier.size(14.dp),
                        tint = MaterialTheme.colorScheme.error,
                    )
                    else -> Icon(
                        Icons.Filled.Check,
                        contentDescription = null,
                        modifier = Modifier.size(14.dp),
                        tint = MaterialTheme.colorScheme.secondary,
                    )
                }
                Spacer(Modifier.widthIn(min = 6.dp))
                Text(
                    tc.name,
                    style = MaterialTheme.typography.labelMedium,
                    color = MaterialTheme.colorScheme.secondary,
                )
                Spacer(Modifier.widthIn(min = 6.dp))
                Text(
                    tc.argsPreview,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                    maxLines = 1,
                    overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                    modifier = Modifier.weight(1f),
                )
                Icon(
                    if (tc.expanded) Icons.Filled.ExpandLess else Icons.Filled.ExpandMore,
                    contentDescription = if (tc.expanded) "收起" else "展开",
                    modifier = Modifier.size(16.dp),
                    tint = MaterialTheme.colorScheme.secondary,
                )
            }
        }
        if (tc.expanded) {
            Column(Modifier.padding(start = 10.dp, end = 4.dp, top = 2.dp)) {
                Text(
                    "命令",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
                Text(
                    tc.fullArgs.ifBlank { "（无参数）" },
                    style = MaterialTheme.typography.bodySmall.copy(
                        fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                    ),
                )
                Text(
                    "结果",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
                Text(
                    when {
                        tc.status == "running" -> "（执行中…）"
                        tc.output != null && tc.output!!.isNotBlank() -> tc.output!!
                        else -> "（无输出）"
                    },
                    style = MaterialTheme.typography.bodySmall.copy(
                        fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                    ),
                    maxLines = 16,
                    overflow = androidx.compose.ui.text.style.TextOverflow.Ellipsis,
                )
            }
        }
    }
}

/** 思维链：默认折叠（超长文本布局昂贵 + 干扰正文），点击展开。 */
@Composable
private fun ReasoningBlock(text: String, meta: String? = null) {
    var expanded by remember { mutableStateOf(false) }
    // 展开/收起平滑动画。
    Column(
        Modifier
            .padding(horizontal = 4.dp)
            .animateContentSize()
    ) {
        androidx.compose.material3.AssistChip(
            onClick = { expanded = !expanded },
            label = {
                Text(
                    meta?.takeIf { it.isNotBlank() }?.let { "已思考 $it" } ?: "思维链 ${text.length} 字",
                    style = MaterialTheme.typography.labelSmall,
                )
            },
            leadingIcon = {
                Icon(
                    Icons.Outlined.Psychology,
                    contentDescription = null,
                    modifier = Modifier.size(16.dp),
                    tint = MaterialTheme.colorScheme.tertiary,
                )
            },
            trailingIcon = {
                Icon(
                    if (expanded) {
                        Icons.Filled.ExpandLess
                    } else {
                        Icons.Filled.ExpandMore
                    },
                    contentDescription = if (expanded) "收起" else "展开",
                    modifier = Modifier.size(18.dp),
                    tint = MaterialTheme.colorScheme.tertiary,
                )
            },
            colors = androidx.compose.material3.AssistChipDefaults.assistChipColors(
                containerColor = MaterialTheme.colorScheme.tertiaryContainer.copy(alpha = 0.35f),
                labelColor = MaterialTheme.colorScheme.tertiary,
            ),
            border = androidx.compose.foundation.BorderStroke(
                0.5.dp,
                MaterialTheme.colorScheme.tertiary.copy(alpha = 0.4f),
            ),
        )
        if (expanded) {
            Text(
                text,
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
                modifier = Modifier.padding(top = 2.dp, start = 8.dp, end = 8.dp),
            )
        }
    }
}

/** 模型回复气泡：Markdown 渲染（代码块/粗体/列表/标题等，紧凑字号）。 */
@Composable
private fun MarkdownBubble(text: String) {
    val clipboard = LocalClipboardManager.current
    // 标题降档：聊天场景 H1≈titleLarge(20sp)，逐级收紧（默认 headlineMedium
    // 32sp 起太占屏）；正文 bodyMedium。
    val typography = (
        com.mikepenz.markdown.m3.markdownTypography()
            as com.mikepenz.markdown.model.DefaultMarkdownTypography
        ).copy(
            h1 = MaterialTheme.typography.titleLarge,
            h2 = MaterialTheme.typography.titleMedium,
            h3 = MaterialTheme.typography.titleSmall,
            h4 = MaterialTheme.typography.titleSmall,
            h5 = MaterialTheme.typography.bodyLarge,
            h6 = MaterialTheme.typography.bodyLarge,
            text = MaterialTheme.typography.bodyMedium,
            paragraph = MaterialTheme.typography.bodyMedium,
        )
    Surface(
        shape = RoundedCornerShape(
            topStart = 4.dp,
            topEnd = 16.dp,
            bottomStart = 16.dp,
            bottomEnd = 16.dp,
        ),
        color = MaterialTheme.colorScheme.surfaceVariant,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp),
        onClick = { clipboard.setText(AnnotatedString(text)) },
    ) {
        com.mikepenz.markdown.m3.Markdown(
            content = text,
            typography = typography,
            modifier = Modifier.padding(horizontal = 12.dp, vertical = 8.dp),
        )
    }
}

@Composable
private fun Bubble(text: String, mine: Boolean) {
    val clipboard = LocalClipboardManager.current
    Surface(
        shape = RoundedCornerShape(
            topStart = 16.dp,
            topEnd = 16.dp,
            bottomStart = if (mine) 16.dp else 4.dp,
            bottomEnd = if (mine) 4.dp else 16.dp,
        ),
        color = if (mine) MaterialTheme.colorScheme.primaryContainer
        else MaterialTheme.colorScheme.surfaceVariant,
        modifier = Modifier
            .fillMaxWidth()
            .padding(horizontal = 4.dp),
        onClick = { clipboard.setText(AnnotatedString(text)) },
    ) {
        Text(
            text,
            modifier = Modifier.padding(horizontal = 14.dp, vertical = 10.dp),
            style = MaterialTheme.typography.bodyMedium,
        )
    }
}

@Composable
private fun Composer(running: Boolean, onSend: (String) -> Unit, onCancel: () -> Unit) {
    var input by remember { mutableStateOf("") }
    Surface(tonalElevation = 3.dp) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 10.dp, vertical = 8.dp),
        ) {
            OutlinedTextField(
                value = input,
                onValueChange = { input = it },
                placeholder = { Text("发消息…") },
                maxLines = 5,
                modifier = Modifier.weight(1f),
            )
            IconButton(
                onClick = {
                    if (running) onCancel() else {
                        val text = input.trim()
                        if (text.isNotEmpty()) {
                            onSend(text)
                            input = ""
                        }
                    }
                },
            ) {
                if (running) {
                    Icon(Icons.Filled.Stop, contentDescription = "取消")
                } else {
                    Icon(Icons.AutoMirrored.Filled.Send, contentDescription = "发送")
                }
            }
        }
    }
}

@Composable
private fun ApprovalBanner(
    pending: dev.openslate.mobile.bridge.PendingApprovalUi,
    onAnswer: (Long, String) -> Unit,
) {
    Surface(tonalElevation = 6.dp) {
        Column(Modifier.fillMaxWidth().padding(12.dp)) {
            Text(
                "审批请求：${pending.toolName}（${pending.riskLevel}）",
                style = MaterialTheme.typography.titleSmall,
            )
            if (pending.reason.isNotEmpty()) {
                Text(
                    pending.reason,
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                TextButton(onClick = { onAnswer(pending.id, "approve_all") }) { Text("全部允许") }
                TextButton(onClick = { onAnswer(pending.id, "approve") }) { Text("允许") }
                TextButton(onClick = { onAnswer(pending.id, "deny") }) { Text("拒绝") }
            }
        }
    }
}
