package dev.openslate.mobile.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.automirrored.filled.ArrowBack
import androidx.compose.material.icons.filled.Build
import androidx.compose.material.icons.filled.KeyboardArrowDown
import androidx.compose.material.icons.filled.Psychology
import androidx.compose.material.icons.filled.Visibility
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RegistryEntryUi
import dev.openslate.mobile.bridge.RegistrySearchUi
import dev.openslate.mobile.bridge.RegistrySourceUi
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 模型数据源首屏（设置 > 模型 > 模型数据源，内嵌二级 tab）：
 *
 * - 「数据源管理」卡片区常开置顶（每源手动更新 + 缓存元信息 + 启动自动
 *   刷新说明）+ 卡片下方一行提示；**无跨源合并列表，也不保留跨源搜索**
 *   （用户拍板：三个源各自独立呈现）；
 * - 点某张源卡 → `onOpenSource(sourceId)` 经 NavController 跳转该源独占
 *   整屏 [ModelRegistrySourceScreen]（路由 `registry-source/{sourceId}`），
 *   系统返回键 / 手势 / 顶栏返回都走导航栈 pop；点按与「更新」按钮事件
 *   隔离：SoftButton 自带 clickable 消费点按，不会穿透触发卡片 onClick。
 *
 * 元数据消费方：Provider 模型管理里的「自动检测入库后补全」与模型编辑
 * 弹窗的「从 registry 补全」。
 */
@Composable
fun ModelRegistrySection(onOpenSource: (String) -> Unit) {
    val scope = rememberCoroutineScope()
    /** 数据版本键：数据源手动更新成功后 +1，重拉源状态。 */
    var reloadKey by remember { mutableStateOf(0) }
    var sources by remember { mutableStateOf<List<RegistrySourceUi>>(emptyList()) }

    LaunchedEffect(reloadKey) {
        sources = withContext(Dispatchers.IO) { RuntimeBridge.registrySources() }
    }

    /** 正在更新的源 id 集合与结果消息（数据源管理小节）。 */
    var updating by remember { mutableStateOf<Set<String>>(emptySet()) }
    var message by remember { mutableStateOf<String?>(null) }

    fun updateSource(s: RegistrySourceUi) {
        if (s.id in updating) return
        updating = updating + s.id
        message = null
        scope.launch {
            val r = withContext(Dispatchers.IO) { RuntimeBridge.updateRegistrySource(s.id) }
            withContext(Dispatchers.Main) {
                updating = updating - s.id
                message = if (r.ok) {
                    "${s.name} 已更新（${r.models.firstOrNull() ?: "?"} 条）"
                } else {
                    "${s.name} 更新失败：${r.error}"
                }
                // 新数据生效：重拉源状态。源页是独立导航目的地，条目在进入
                // 时自取（见 [ModelRegistrySourceScreen]），返回后重进即新数据。
                reloadKey++
            }
        }
    }

    LazyColumn(
        Modifier.fillMaxSize(),
        // 顶部 19dp = 16dp 滚动列节奏 + 3dp 光学补偿：本页首行标题是
        // titleSmall（14sp/20sp 行高），比会话/Provider 页的 titleMedium
        // （16sp/24sp 行高）首行基线高约 2.7~2.9dp（随字体度量浮动），
        // 补一笔让三个子页首行标题到二级 TabRow 的目视距离一致。
        contentPadding = PaddingValues(start = 16.dp, top = 19.dp, end = 16.dp, bottom = 16.dp),
        verticalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        item(key = "manage", contentType = "manage") {
            SourceManageSection(
                sources = sources,
                updating = updating,
                message = message,
                onUpdate = ::updateSource,
                onOpen = onOpenSource,
            )
        }
        item(key = "hint", contentType = "hint") {
            Text(
                "点数据源查看其条目",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
    }
}

/**
 * 数据源独占整屏（导航目的地 `registry-source/{sourceId}`；首屏点源卡进入，
 * 系统返回键 / 手势与顶栏返回箭头都走 NavController pop，标准 Android 形态）。
 *
 * 数据经路由参数 sourceId 自取（零 Rust/FFI 改动，IO 线程）：
 * `registrySources()` 取源元信息（名称等）；`registrySearch(sourceId, "",
 * 10万上限)` 空 query 字典序浏览通道当全量用。该源条目按 provider 前缀
 * 分组（`providerOf` 取 `/` 之前），组默认收起，点组头展开/收起，组内条目
 * 以 LazyColumn 逐行虚拟化（分组头与条目是平级 item，不是整组
 * AnimatedVisibility——单组可达数百行，整组动画会一次性全部组合）；
 * 源内搜索（防抖 300ms）只查该源条目，命中组自动展开（之后仍可手动收起），
 * 清空回该源浏览（复用全量数据，不再发查询）；点条目行 → 详情弹层（null
 * 字段如实标「源未提供」，不臆造）。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ModelRegistrySourceScreen(sourceId: String, onBack: () -> Unit) {
    // 源元信息（本地读，进屏即达；读到前标题先落 sourceId 兜底）。
    var source by remember { mutableStateOf<RegistrySourceUi?>(null) }
    LaunchedEffect(sourceId) {
        if (sourceId.isEmpty()) return@LaunchedEffect
        val list = withContext(Dispatchers.IO) { RuntimeBridge.registrySources() }
        source = list.firstOrNull { it.id == sourceId }
    }

    // 该源全量条目（null = 首载进行中；空 query = 字典序浏览，limit 给大值
    // 一次拉全量，数千条本地读，手机可承受）。
    var entries by remember { mutableStateOf<List<RegistryEntryUi>?>(null) }
    LaunchedEffect(sourceId) {
        if (sourceId.isEmpty()) return@LaunchedEffect
        entries = null
        entries = withContext(Dispatchers.IO) {
            RuntimeBridge.registrySearch(sourceId, "", 100_000).results
        }
    }

    var query by remember { mutableStateOf("") }
    var searchResult by remember { mutableStateOf<RegistrySearchUi?>(null) }
    /** 展开的 provider 组（键 = provider 前缀；默认收起）。 */
    var expandedProviders by remember { mutableStateOf<Set<String>>(emptySet()) }

    val q = query.trim()
    val searching = q.isNotEmpty()

    // 防抖 300ms 源内搜索（沿用原页面节奏）；清空即回该源浏览（复用全量
    // 数据，不再发查询）。
    LaunchedEffect(sourceId, q) {
        if (sourceId.isEmpty() || q.isEmpty()) {
            searchResult = null
            return@LaunchedEffect
        }
        delay(300)
        searchResult = withContext(Dispatchers.IO) {
            RuntimeBridge.registrySearch(sourceId, q, 100_000)
        }
    }

    // 搜索命中的组自动展开（之后仍可手动收起）。
    LaunchedEffect(searchResult) {
        searchResult?.let { r ->
            expandedProviders = expandedProviders + r.results.map { providerOf(it.id) }
        }
    }

    /** 详情弹层中的条目（null = 关闭）。 */
    var detail by remember { mutableStateOf<RegistryEntryUi?>(null) }

    // 展示条目：搜索态用搜索结果，浏览态用该源全量；按 provider 前缀分组，
    // 组与组内条目均按字典序（空 query 的 FFI 浏览结果本就有序，重排兜底）。
    val shown: List<RegistryEntryUi>? = if (searching) searchResult?.results else entries
    val groups: List<Pair<String, List<RegistryEntryUi>>> = shown
        ?.groupBy { providerOf(it.id) }
        ?.map { (p, list) -> p to list.sortedBy { it.id.lowercase() } }
        ?.sortedBy { it.first.lowercase() }
        .orEmpty()

    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(source?.name ?: sourceId.ifEmpty { "数据源" }) },
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
            OutlinedTextField(
                value = query,
                onValueChange = { query = it },
                label = { Text("在 ${source?.name ?: "该数据源"} 内搜索") },
                singleLine = true,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 12.dp),
            )
            LazyColumn(
                Modifier.fillMaxWidth().weight(1f),
                contentPadding = PaddingValues(start = 16.dp, end = 16.dp, bottom = 16.dp),
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                when {
                    shown == null -> item(key = "loading") {
                        Row(
                            Modifier.fillMaxWidth().padding(top = 24.dp),
                            horizontalArrangement = Arrangement.Center,
                        ) { CircularProgressIndicator() }
                    }
                    groups.isEmpty() -> item(key = "empty") {
                        Text(
                            if (searching) "无匹配「$q」的条目"
                            else "该源还没有本地缓存：返回后点「更新」拉取，再进入即可浏览。",
                            style = MaterialTheme.typography.bodySmall,
                            color = MaterialTheme.colorScheme.outline,
                            modifier = Modifier.padding(top = 8.dp),
                        )
                    }
                    else -> {
                        item(key = "summary", contentType = "summary") {
                            Text(
                                if (searching) {
                                    "匹配 ${searchResult?.total ?: 0} 条 · ${groups.size} 个 provider"
                                } else {
                                    "共 ${shown.size} 条 · ${groups.size} 个 provider"
                                },
                                style = MaterialTheme.typography.labelSmall,
                                color = MaterialTheme.colorScheme.outline,
                            )
                        }
                        for ((provider, list) in groups) {
                            item(key = "p:$provider", contentType = "header") {
                                ProviderGroupHeader(
                                    name = provider,
                                    count = list.size,
                                    expanded = provider in expandedProviders,
                                    onToggle = {
                                        expandedProviders = if (provider in expandedProviders) {
                                            expandedProviders - provider
                                        } else {
                                            expandedProviders + provider
                                        }
                                    },
                                )
                            }
                            if (provider in expandedProviders) {
                                items(
                                    list,
                                    key = { "e:${it.sourceId}:${it.id}" },
                                    contentType = { "entry" },
                                ) { e ->
                                    RegistryEntryRow(e, onClick = { detail = e })
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── 条目详情弹层（每源条目各自点开看该源数据；不跨源合并）────────
    detail?.let { e ->
        AlertDialog(
            onDismissRequest = { detail = null },
            title = { Text(e.id, fontFamily = MonoFont) },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    InfoRow("来源", e.source)
                    HorizontalDivider()
                    InfoRow("上下文窗口", limitText(e.ctx))
                    InfoRow("最大输出", limitText(e.out))
                    InfoRow("输入价格", priceText(e.priceIn))
                    InfoRow("输出价格", priceText(e.priceOut))
                    HorizontalDivider()
                    RegistryCapsLine(e)
                }
            },
            confirmButton = {
                SoftButton(onClick = { detail = null }) { Text("关闭") }
            },
        )
    }
}

/** 条目键的 provider 前缀（`/` 之前，如 `zai/glm-5.3` → `zai`）；无 `/` 的
 *  键归入以其自身键名为组名的兜底组。 */
private fun providerOf(id: String): String = id.substringBefore('/', id)

/**
 * provider 组头：provider 前缀 + 条目数 + 展开箭头。折叠语言对齐
 * ToolManagementSection 的「工具命名模板」小节（标题行点击展开/收起，
 * KeyboardArrowDown 随态旋转）；组内条目是 LazyColumn 平级 item（见
 * [ModelRegistrySourceScreen] KDoc），组头自身不包条目。
 */
@Composable
private fun ProviderGroupHeader(
    name: String,
    count: Int,
    expanded: Boolean,
    onToggle: () -> Unit,
) {
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "providerGroupArrow",
    )
    Row(
        Modifier
            .fillMaxWidth()
            .clip(RoundedCornerShape(8.dp))
            .clickable(onClick = onToggle)
            .padding(horizontal = 4.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Column(Modifier.weight(1f)) {
            // provider 前缀是 registry 键的一段 = 标识类文本 → MonoFont。
            Text(name, style = MaterialTheme.typography.titleSmall, fontFamily = MonoFont)
            Text(
                "$count 个模型条目",
                style = MaterialTheme.typography.labelSmall,
                color = MaterialTheme.colorScheme.outline,
            )
        }
        Icon(
            imageVector = Icons.Filled.KeyboardArrowDown,
            contentDescription = if (expanded) "收起 $name 的条目" else "展开 $name 的条目",
            tint = MaterialTheme.colorScheme.outline,
            modifier = Modifier.rotate(arrowAngle),
        )
    }
}

/**
 * 「数据源管理」区（常开置顶，无折叠/入口点击——打开子页即直接看到各源
 * 卡片）：标题行（右侧缓存摘要）+ 说明灰字 + 每源卡片（点卡片经
 * [onOpen] 跳转该源条目整屏 + 手动更新 + 缓存元信息）+ 更新结果消息；
 * 随内容一起滚动。
 */
@Composable
private fun SourceManageSection(
    sources: List<RegistrySourceUi>,
    updating: Set<String>,
    message: String?,
    onUpdate: (RegistrySourceUi) -> Unit,
    onOpen: (String) -> Unit,
) {
    val cached = sources.filter { it.fetchedAtMs != null }
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        // 标题行：标题居左，缓存摘要（源清单未读到时不渲染，防闪烁）居右。
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Text(
                "数据源管理",
                style = MaterialTheme.typography.titleSmall,
                modifier = Modifier.weight(1f),
            )
            if (sources.isNotEmpty()) {
                Text(
                    "${cached.size}/${sources.size} 个源已缓存 · " +
                        "${cached.sumOf { it.entries ?: 0 }} 条条目",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                )
            }
        }
        Text(
            "大模型元数据（上下文 / 输出限额 / 视觉 / 思考 / 计价）来自下列公开数据源，" +
                "拉取后压缩存于手机本地：添加或编辑模型时自动补全，查询不联网。" +
                "应用启动时，超过更新周期（models.dev 1 天、其余 1 周）的源会自动刷新。",
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.outline,
        )
        for (s in sources) {
            SourceCard(
                s,
                updating = s.id in updating,
                onUpdate = { onUpdate(s) },
                onOpen = { onOpen(s.id) },
            )
        }
        if (message != null) {
            Text(
                message ?: "",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.primary,
            )
        }
    }
}

/** 单个数据源卡片：点卡片跳转该源条目整屏 + 手动更新 + 缓存元信息（条目
 *  数 / 更新时间 / 压缩体积）。点按与「更新」按钮事件隔离：SoftButton 自带
 *  clickable 消费点按，不会穿透触发卡片 onClick（跳转）。 */
@Composable
private fun SourceCard(
    s: RegistrySourceUi,
    updating: Boolean,
    onUpdate: () -> Unit,
    onOpen: () -> Unit,
) {
    val fmt = remember { SimpleDateFormat("MM-dd HH:mm", Locale.getDefault()) }
    Card(onClick = onOpen, modifier = Modifier.fillMaxWidth()) {
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
                if (updating) {
                    CircularProgressIndicator(
                        modifier = Modifier
                            .padding(end = 8.dp)
                            .height(16.dp)
                            .width(16.dp),
                        strokeWidth = 2.dp,
                    )
                }
                SoftButton(enabled = !updating, onClick = onUpdate) { Text("更新") }
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
                    "已缓存 $entries 条 · 更新于 ${fmt.format(Date(fetched))}$kb"
                } else {
                    "尚未拉取（点「更新」下载到本地后可浏览其条目）"
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

/** registry 条目行：原始键（保留 provider 前缀，等宽）+ 来源标签 + 能力
 *  旗标 + 限额/计价摘要。点击进详情弹层。字段 null = 源未提供（摘要行
 *  略去对应部分，不臆造）。 */
@Composable
private fun RegistryEntryRow(e: RegistryEntryUi, onClick: () -> Unit) {
    Card(onClick = onClick, modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    // registry 原始键（如 zhipuai/glm-4.7）= 标识类文本 →
                    // 内嵌 MonoFont 子集。
                    e.id,
                    style = MaterialTheme.typography.bodyMedium,
                    fontFamily = MonoFont,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    softWrap = false,
                    modifier = Modifier.weight(1f, fill = false),
                )
                Spacer(Modifier.width(8.dp))
                // 来源标签常显：标注该条目出自哪个源（条目行所属视图即该源）。
                Text(
                    e.source,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.primary,
                )
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

/** 详情弹层的能力行：源提供的能力用 Material 旗标；源未提供的能力如实
 *  标注「源未提供」（不臆造——能力布尔不采信聚合兜底，见 AGENTS.md）。 */
@Composable
private fun RegistryCapsLine(e: RegistryEntryUi) {
    Row(
        horizontalArrangement = Arrangement.spacedBy(10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        e.vision?.let { v -> RegistryFlag(Icons.Filled.Visibility, "视觉", v) }
        e.reasoning?.let { v -> RegistryFlag(Icons.Filled.Psychology, "思考", v) }
        e.tool?.let { v -> RegistryFlag(Icons.Filled.Build, "工具调用", v) }
    }
    val missing = buildList {
        if (e.vision == null) add("视觉")
        if (e.reasoning == null) add("思考")
        if (e.tool == null) add("工具调用")
    }
    if (missing.isNotEmpty()) {
        Text(
            "${missing.joinToString("、")}：源未提供",
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
        )
    }
}

/** token 简写：131072 → "128K"、2000000 → "2M"（本文件私有）；
 *  exact = true 时显示千分位完整数字（详情弹层用）。 */
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

/** 计价简写（$/Mtok 数值段，本文件私有；null → "—"）。 */
private fun fmtPrice(p: Double?): String {
    if (p == null) return "—"
    val s = when {
        p >= 100.0 || p == kotlin.math.floor(p) -> "%.0f".format(p)
        p < 0.01 -> "%.4f".format(p)
        else -> "%.2f".format(p)
    }
    return s
}

/** 详情弹层用：简写 + 千分位全量（"128K（131,072）"）；简写已是全量
 *  （如 999）时不重复。 */
private fun fmtTokensFull(t: Long?): String {
    val short = fmtTokens(t)
    val exact = if (t != null && t > 0) fmtTokens(t, exact = true) else null
    return if (exact != null && exact != short) "$short（$exact）" else short
}

/** 详情弹层的限额文本；null/非正数 = 源未提供（如实标注，不臆造数值）。 */
private fun limitText(t: Long?): String =
    if (t == null || t <= 0) "源未提供" else fmtTokensFull(t)

/** 详情弹层的计价文本（$/Mtok）；null = 源未提供。 */
private fun priceText(p: Double?): String =
    if (p == null) "源未提供" else "$${fmtPrice(p)} / Mtok"
