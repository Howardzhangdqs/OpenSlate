package dev.openslate.mobile.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Build
import androidx.compose.material.icons.filled.Psychology
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.RegistryEntryUi
import dev.openslate.mobile.bridge.RegistrySearchUi
import dev.openslate.mobile.bridge.RegistrySourceUi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 模型元数据数据源管理页：
 *
 * - 列出全部数据源（models.dev / LiteLLM / CloudPrice）与本地状态
 *   （条目数、更新时间、压缩后体积）；
 * - 点源卡片进入**源详情**：浏览该源条目（前 50 条）或源内搜索；
 * - 列表页顶部**跨源搜索**：按模型名查全部本地已缓存源，结果带来源
 *   标签（可直接对比同一模型在不同源的能力/计价数据）；
 * - 每源一个「更新」按钮（手动触发，ZSTD 压缩落盘手机本地）；
 * - 应用启动时对过期源自动更新（models.dev 一天、其余一周，Rust 侧
 *   registry_schedule_auto_update）。
 *
 * 元数据消费方：Provider 模型管理里的"自动检测入库后补全"与模型编辑
 * 弹窗的「从 registry 补全」。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ModelRegistryScreen(onBack: () -> Unit) {
    val scope = rememberCoroutineScope()
    var sources by remember { mutableStateOf<List<RegistrySourceUi>>(emptyList()) }
    var loaded by remember { mutableStateOf(false) }
    /** 正在更新的源 id 集合。 */
    var updating by remember { mutableStateOf<Set<String>>(emptySet()) }
    var message by remember { mutableStateOf<String?>(null) }
    /** 打开的源（null = 源列表 + 跨源搜索视图）。 */
    var openSource by remember { mutableStateOf<RegistrySourceUi?>(null) }
    var query by remember { mutableStateOf("") }
    var result by remember { mutableStateOf<RegistrySearchUi?>(null) }

    fun refresh() {
        scope.launch {
            val list = withContext(Dispatchers.IO) { RuntimeBridge.registrySources() }
            sources = list
            loaded = true
        }
    }

    LaunchedEffect(Unit) { refresh() }

    // 防抖 300ms 搜索：源详情内 = 该源；列表页 = 跨源。query 空 =
    // 浏览模式（前 50 条，键字典序）。
    LaunchedEffect(openSource?.id, query) {
        delay(300)
        result = withContext(Dispatchers.IO) {
            RuntimeBridge.registrySearch(openSource?.id ?: "", query.trim(), 50)
        }
    }

    val fmt = remember { SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()) }

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(openSource?.name ?: "模型数据源") },
                navigationIcon = {
                    IconButton(onClick = {
                        if (openSource != null) {
                            openSource = null
                            query = ""
                            result = null
                        } else {
                            onBack()
                        }
                    }) {
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
                .padding(horizontal = 16.dp),
        ) {
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                label = {
                    Text(
                        if (openSource == null) "搜索模型（全部本地数据源）"
                        else "在 ${openSource?.name} 内搜索",
                    )
                },
                singleLine = true,
                modifier = Modifier.fillMaxWidth().padding(top = 12.dp),
            )

            val showResults = openSource != null || query.isNotBlank()
            if (!showResults) {
                // ── 源列表视图 ──────────────────────────────────────
                Column(
                    Modifier
                        .fillMaxSize()
                        .verticalScroll(rememberScrollState()),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    Text(
                        "大模型元数据（上下文长度 / 输出限额 / 视觉 / 思考 / 计价）来自下面的" +
                            "公开数据源，拉取后压缩存于手机本地：添加或编辑模型时自动补全，" +
                            "查询不联网。点数据源可查看/搜索其条目。应用启动时，超过更新周期" +
                            "（models.dev 1 天、其余 1 周）的源会自动刷新。",
                        style = MaterialTheme.typography.bodySmall,
                        color = MaterialTheme.colorScheme.outline,
                        modifier = Modifier.padding(top = 8.dp),
                    )

                    if (!loaded) {
                        Row(
                            Modifier.fillMaxWidth().padding(top = 24.dp),
                            horizontalArrangement = Arrangement.Center,
                        ) { CircularProgressIndicator() }
                    }

                    for (s in sources) {
                        Card(
                            onClick = {
                                openSource = s
                                query = ""
                                result = null
                            },
                            enabled = s.fetchedAtMs != null,
                            modifier = Modifier.fillMaxWidth(),
                        ) {
                            Column(
                                Modifier.padding(12.dp),
                                verticalArrangement = Arrangement.spacedBy(6.dp),
                            ) {
                                Row(verticalAlignment = Alignment.CenterVertically) {
                                    Text(
                                        s.name,
                                        style = MaterialTheme.typography.titleSmall,
                                        modifier = Modifier.weight(1f),
                                    )
                                    if (s.id in updating) {
                                        CircularProgressIndicator(
                                            modifier = Modifier
                                                .padding(end = 8.dp)
                                                .height(16.dp)
                                                .width(16.dp),
                                            strokeWidth = 2.dp,
                                        )
                                    }
                                    SoftButton(
                                        enabled = s.id !in updating,
                                        onClick = {
                                            updating = updating + s.id
                                            message = null
                                            scope.launch {
                                                val r = withContext(Dispatchers.IO) {
                                                    RuntimeBridge.updateRegistrySource(s.id)
                                                }
                                                withContext(Dispatchers.Main) {
                                                    updating = updating - s.id
                                                    message = if (r.ok) {
                                                        "${s.name} 已更新" +
                                                            "（${r.models.firstOrNull() ?: "?"} 条）"
                                                    } else {
                                                        "${s.name} 更新失败：${r.error}"
                                                    }
                                                    refresh()
                                                }
                                            }
                                        },
                                    ) { Text("更新") }
                                }
                                Text(
                                    s.desc,
                                    style = MaterialTheme.typography.bodySmall,
                                    color = MaterialTheme.colorScheme.outline,
                                )
                                val fetched = s.fetchedAtMs
                                Text(
                                    if (fetched != null) {
                                        val entries = s.entries?.toString() ?: "—"
                                        val kb = s.zstBytes?.let { "（${it / 1024}KB 压缩存储）" } ?: ""
                                        "已缓存 $entries 条 · 更新于 ${fmt.format(Date(fetched))}$kb · 点按查看"
                                    } else {
                                        "尚未拉取（点「更新」下载到本地后可查看）"
                                    },
                                    style = MaterialTheme.typography.labelSmall,
                                    color = if (fetched != null) {
                                        MaterialTheme.colorScheme.primary
                                    } else {
                                        MaterialTheme.colorScheme.outline
                                    },
                                )
                            }
                        }
                    }

                    if (message != null) {
                        Text(
                            message ?: "",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.primary,
                        )
                    }
                }
            } else {
                // ── 搜索 / 浏览结果 ─────────────────────────────────
                val r = result
                Text(
                    if (r == null) "搜索中…"
                    else "共 ${r.total} 条" +
                        if (r.total > r.results.size) "，显示前 ${r.results.size}" else "",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                    modifier = Modifier.padding(top = 8.dp, bottom = 4.dp),
                )
                LazyColumn(
                    Modifier.fillMaxSize(),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    items(r?.results ?: emptyList()) { e ->
                        RegistryEntryRow(e, showSource = openSource == null)
                    }
                }
            }
        }
    }
}

/** registry 条目行：原始键（保留 provider 前缀）+ 来源标签 + 配置摘要。
 *  字段 null = 源未提供（不显示对应部分，不臆造）。 */
@Composable
private fun RegistryEntryRow(e: RegistryEntryUi, showSource: Boolean) {
    Card(Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    e.id,
                    style = MaterialTheme.typography.bodyMedium,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    softWrap = false,
                    modifier = Modifier.weight(1f, fill = false),
                )
                if (showSource) {
                    Spacer(Modifier.width(8.dp))
                    Text(
                        e.source,
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.primary,
                    )
                }
            }
            // 能力旗标（Material 图标，规范见 AGENTS.md；null = 源未提供 → 不显示）。
            Row(
                horizontalArrangement = Arrangement.spacedBy(10.dp),
                verticalAlignment = Alignment.CenterVertically,
            ) {
                e.vision?.let { v -> RegistryFlag(Icons.Filled.Visibility, "视觉", v) }
                e.reasoning?.let { v -> RegistryFlag(Icons.Filled.Psychology, "思考", v) }
                e.tool?.let { v -> RegistryFlag(Icons.Filled.Build, "工具调用", v) }
            }
            val parts = mutableListOf<String>()
            if (e.ctx != null) parts += "上下文 ${fmtTokens(e.ctx)}"
            if (e.out != null) parts += "输出 ${fmtTokens(e.out)}"
            if (e.priceIn != null || e.priceOut != null) {
                parts += "\$${fmtPrice(e.priceIn)} / \$${fmtPrice(e.priceOut)} 每百万 token"
            }
            Text(
                parts.joinToString(" · ").ifEmpty { "（该源未提供此模型的元数据）" },
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
    }
}

/** 能力旗标：Material 图标 + 标签（true = 主题色，false = 灰）。 */
@Composable
private fun RegistryFlag(icon: ImageVector, label: String, on: Boolean) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(2.dp),
    ) {
        Icon(
            icon,
            contentDescription = "$label：${if (on) "支持" else "不支持"}",
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

/** token 简写：131072 → "128K"、2000000 → "2M"（本文件私有）。 */
private fun fmtTokens(t: Long?): String {
    if (t == null || t <= 0) return "—"
    val unit = when {
        t >= 1_000_000L -> "M" to t / 1_000_000.0
        t >= 1_000L -> "K" to t / 1_000.0
        else -> return t.toString()
    }
    val (suffix, v) = unit
    val s = if (v >= 100.0 || v == kotlin.math.floor(v)) "%.0f".format(v) else "%.1f".format(v)
    return "$s$suffix"
}

/** 计价简写（$/Mtok，本文件私有；null → "—"）。 */
private fun fmtPrice(p: Double?): String {
    if (p == null) return "—"
    val s = when {
        p >= 100.0 || p == kotlin.math.floor(p) -> "%.0f".format(p)
        p < 0.01 -> "%.4f".format(p)
        else -> "%.2f".format(p)
    }
    return s
}
