package dev.openslate.mobile.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.outlined.Code
import androidx.compose.material.icons.outlined.ContentCopy
import androidx.compose.material.icons.outlined.Visibility
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalClipboardManager
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.mikepenz.markdown.annotator.AnnotatorSettings
import com.mikepenz.markdown.annotator.annotatorSettings
import com.mikepenz.markdown.annotator.buildMarkdownAnnotatedString
import com.mikepenz.markdown.compose.LocalMarkdownColors
import com.mikepenz.markdown.compose.LocalMarkdownDimens
import com.mikepenz.markdown.compose.LocalMarkdownTypography
import com.mikepenz.markdown.compose.components.MarkdownComponentModel
import com.mikepenz.markdown.compose.components.markdownComponents
import com.mikepenz.markdown.compose.elements.MarkdownHighlightedCodeBlock
import com.mikepenz.markdown.compose.elements.MarkdownHighlightedCodeFence
import com.mikepenz.markdown.compose.elements.MarkdownTableBasicText
import com.mikepenz.markdown.coil3.Coil3ImageTransformerImpl
import com.mikepenz.markdown.model.DefaultMarkdownTypography
import com.mikepenz.markdown.m3.Markdown
import com.mikepenz.markdown.m3.markdownTypography
import com.gallatinapps.syntaxmp.role.SyntaxRole
import com.gallatinapps.syntaxmp.tokenizer.SyntaxTokenizer
import org.intellij.markdown.IElementType
import org.intellij.markdown.MarkdownElementTypes
import org.intellij.markdown.ast.ASTNode
import org.intellij.markdown.flavours.gfm.GFMElementTypes
import org.intellij.markdown.flavours.gfm.GFMTokenTypes
import java.util.Locale

/**
 * 聊天气泡增强 Markdown 渲染，在裸 m3 Markdown 之上追加：
 * - 代码块语法高亮 + 行号（自绘 [LineNumberedCode]）+ 语言名/复制头部；
 *   SVG 围栏额外带「预览」切换（见 [EnhancedCodeFence]）
 * - 图片经 Coil3 加载（全局 ImageLoader 已注册 SvgDecoder，支持 SVG）
 * - HTML 块降级渲染：简单标签走 Compose Text，复杂结构走 WebView
 *   （见 [HtmlFallback]）
 * - 表格自绘渲染：实测列宽 + fit/横滚两分支 + 列对齐，任何内容不
 *   省略号截断（见 [EnhancedMarkdownTable]）
 *
 * 字号沿用聊天场景的标题降档（默认 headlineMedium 32sp 起太占屏）。
 */
@Composable
fun EnhancedMarkdown(
    text: String,
    modifier: Modifier = Modifier,
) {
    // 标题降档：H1≈titleLarge(20sp) 逐级收紧；正文 bodyMedium。
    val typography = (
        markdownTypography() as DefaultMarkdownTypography
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
    val components = markdownComponents(
        // 行内 `code`：无头部，仅高亮。
        codeBlock = { model: MarkdownComponentModel ->
            MarkdownHighlightedCodeBlock(
                content = model.content,
                node = model.node,
                showHeader = false,
            )
        },
        // ``` 围栏代码块：自绘头部（语言名 + 复制，SVG 加预览切换）+ 带行号
        // 的高亮代码区；不再嵌套库的 MarkdownHighlightedCodeFence（其内部
        // 在代码背景外再叠 vertical 8dp padding + codeBlockPadding，嵌套后
        // 会造成 header/代码/块底之间双重空白）。
        codeFence = { model: MarkdownComponentModel ->
            val raw = model.content.substring(model.node.startOffset, model.node.endOffset)
            val parts = parseCodeFence(raw)
            if (parts != null) {
                EnhancedCodeFence(
                    language = parts.language,
                    body = parts.body,
                    isSvg = parts.body.trim().startsWith("<svg", ignoreCase = true),
                )
            } else {
                MarkdownHighlightedCodeFence(
                    content = model.content,
                    node = model.node,
                    showHeader = true,
                )
            }
        },
        // 表格：自绘列宽/对齐/换行渲染。库默认按「列数 × 固定单元格宽」
        // 布局且单行省略号截断，超宽内容不可见，见 [EnhancedMarkdownTable]。
        table = { model: MarkdownComponentModel ->
            EnhancedMarkdownTable(content = model.content, node = model.node)
        },
        // 接住原生不支持的块级元素（HTML 块）转降级渲染。
        custom = { type: IElementType, model: MarkdownComponentModel ->
            if (type == MarkdownElementTypes.HTML_BLOCK) {
                HtmlFallback(
                    content = model.content.substring(
                        model.node.startOffset,
                        model.node.endOffset,
                    ),
                )
            }
        },
    )
    Markdown(
        content = text,
        imageTransformer = Coil3ImageTransformerImpl,
        components = components,
        typography = typography,
        modifier = modifier,
    )
}

/** 代码围栏解析结果：语言标注 + 正文（已剥围栏行）。 */
private data class CodeFenceParts(val language: String, val body: String)

/** 开栏行：``` 或 ~~~ 后跟语言标注等 info string（不吃换行）。 */
private val FENCE_OPEN = Regex("""^[ \t]*(`{3,}|~{3,})[ \t]*([^\n]*)""")

/** 闭栏行（流式更新中可能尚未出现）。 */
private val FENCE_CLOSE = Regex("""\n?[ \t]*(?:`{3,}|~{3,})[ \t]*\Z""")

private fun parseCodeFence(raw: String): CodeFenceParts? {
    val open = FENCE_OPEN.find(raw) ?: return null
    val language = open.groupValues[2].trim()
    val body = raw.removePrefix(open.value)
        .replace(FENCE_CLOSE, "")
        .dropWhile { it == '\n' }
    return CodeFenceParts(language, body)
}

/**
 * 自绘代码围栏（SVG 与普通块共用）：Surface(codeBackground) 内
 * Column { 头部行（语言名 + [SVG: 预览切换] + 复制）；代码或 SVG 预览 }。
 * 代码区与头部行/块底之间只留 ~8dp 内边距（不嵌套库实现，避免双重
 * padding）；Surface 外的 vertical 8dp 为与相邻 Markdown 内容的间距
 * （对齐库默认行为）。
 */
@Composable
private fun EnhancedCodeFence(language: String, body: String, isSvg: Boolean) {
    // 流式更新（body 变化）时重置预览态。
    var preview by remember(body) { mutableStateOf(false) }
    val colors = LocalMarkdownColors.current
    val dimens = LocalMarkdownDimens.current
    val clipboard = LocalClipboardManager.current
    Surface(
        color = colors.codeBackground,
        shape = RoundedCornerShape(dimens.codeBackgroundCornerSize),
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 8.dp),
    ) {
        Column {
            // 复刻库内 MarkdownCodeTopBar（0.45.0 中为 internal）：语言名 +
            // 预览切换 + 复制按钮。
            Row(
                verticalAlignment = Alignment.CenterVertically,
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(start = 12.dp, top = 6.dp, bottom = 6.dp, end = 8.dp),
            ) {
                Text(
                    text = language.ifBlank { "code" }.uppercase(Locale.ROOT),
                    color = colors.text,
                    fontSize = 12.sp,
                    fontFamily = FontFamily.Monospace,
                    modifier = Modifier.weight(1f),
                )
                if (isSvg) {
                    Box(
                        contentAlignment = Alignment.Center,
                        modifier = Modifier
                            .padding(start = 4.dp)
                            .size(28.dp)
                            .clip(CircleShape)
                            .clickable { preview = !preview },
                    ) {
                        // 预览态 → 显示「回到代码」；代码态 → 显示「预览」。
                        Icon(
                            imageVector = if (preview) Icons.Outlined.Code else Icons.Outlined.Visibility,
                            contentDescription = if (preview) "显示代码" else "预览 SVG",
                            tint = colors.text,
                            modifier = Modifier.size(18.dp),
                        )
                    }
                }
                Box(
                    contentAlignment = Alignment.Center,
                    modifier = Modifier
                        .padding(start = 4.dp)
                        .size(28.dp)
                        .clip(CircleShape)
                        .clickable { clipboard.setText(AnnotatedString(body)) },
                ) {
                    Icon(
                        imageVector = Icons.Outlined.ContentCopy,
                        contentDescription = "复制代码",
                        tint = colors.text,
                        modifier = Modifier.size(18.dp),
                    )
                }
            }
            if (isSvg && preview) {
                HtmlFallback(content = body, modifier = Modifier.padding(8.dp))
            } else {
                LineNumberedCode(
                    body = body,
                    language = language,
                    modifier = Modifier.padding(
                        start = 12.dp, end = 12.dp, top = 2.dp, bottom = 10.dp,
                    ),
                )
            }
        }
    }
}

// ===== 表格 =====

/** 表格单元格：AST 节点（渲染用，行内链接/图片/行内代码不丢）+ 实测用文本。 */
private data class TableCell(val node: ASTNode?, val text: AnnotatedString)

/** 表格解析结果：列数 / 各列对齐 / 是否有表头 / 表头与数据行（已对齐到列数）。 */
private data class TableModel(
    val columns: Int,
    val alignments: List<TextAlign>,
    val hasHeader: Boolean,
    val header: List<TableCell>,
    val rows: List<List<TableCell>>,
)

/**
 * 自绘表格渲染（覆写库默认 [com.mikepenz.markdown.compose.elements.MarkdownTable]）：
 * 库默认按「列数 × 固定单元格宽」布局、单元格单行 + 省略号截断，超宽内容
 * 完全不可见。这里改为：
 * - 列内在宽 = 该列最宽单元格按渲染同款样式用 TextMeasurer 单行实测
 *   （上限 min(200dp, 可用宽 × 1.6)，超限列内换行；下限 48dp 防塌缩）；
 * - 总内在宽 ≤ 可用宽 → 各列 weight(内在宽)，富余空间按内容比例分配，
 *   不滚动；超出 → 整表 horizontalScroll（与代码块横滚交互一致），列保
 *   内在宽；
 * - 单元格 softWrap 竖向生长 + Clip 不限行数 → 任何内容不省略号截断；
 *   纵向自然生长跟随气泡高度，无纵向滚动。
 * 样式：圆角 Surface（tableBackground，对齐代码块）+ 表头 SemiBold 加粗、
 * surfaceVariant 半透明浅底 + 1dp 分隔线 + 行间发丝线 + 对齐标记生效 +
 * bodySmall 紧凑字号。
 */
@Composable
private fun EnhancedMarkdownTable(content: String, node: ASTNode) {
    val colors = LocalMarkdownColors.current
    val dimens = LocalMarkdownDimens.current
    val settings = annotatorSettings()
    // 紧凑字号：正文 bodySmall 量级；表头加粗。各列对齐写入 style.textAlign
    // （MarkdownTableBasicText → MarkdownBasicText 的 textAlign 参数缺省时
    // 回退到 style），表头与数据行共用同一列对齐。
    val headerStyle = MaterialTheme.typography.bodySmall.copy(fontWeight = FontWeight.SemiBold)
    val bodyStyle = MaterialTheme.typography.bodySmall

    // 流式更新（块文本变化 → AST 重建）以 content+node 为键整表重解析。
    val table = remember(content, node, settings, headerStyle, bodyStyle) {
        parseTableModel(content, node, headerStyle, bodyStyle, settings)
    }
    val density = LocalDensity.current
    val textMeasurer = rememberTextMeasurer()
    // 各列单行内在宽：该列全部单元格（表头用表头样式、数据行用正文样式）
    // 实测取最宽 + 左右内边距。与列宽上限无关（上限只影响换行）。
    val intrinsicWidths = remember(table, headerStyle, bodyStyle, textMeasurer, density) {
        (0 until table.columns).map { column ->
            val headerPx = table.header.getOrNull(column)?.let {
                textMeasurer.measure(it.text, headerStyle, softWrap = false, maxLines = 1).size.width
            } ?: 0
            val bodyPx = table.rows.maxOfOrNull { row ->
                row.getOrNull(column)?.let {
                    textMeasurer.measure(it.text, bodyStyle, softWrap = false, maxLines = 1).size.width
                } ?: 0
            } ?: 0
            with(density) { maxOf(headerPx, bodyPx).toDp() + CELL_PAD_H * 2 }
        }
    }

    BoxWithConstraints(
        modifier = Modifier
            .fillMaxWidth()
            // 与相邻 Markdown 块的间距（对齐代码块的 vertical 8dp）。
            .padding(vertical = 8.dp),
    ) {
        // 列宽 = 内在宽夹进 [MIN_COLUMN_WIDTH, min(MAX_COLUMN_WIDTH, 可用宽 × 1.6)]。
        val widthCap = (maxWidth * 1.6f).coerceAtMost(MAX_COLUMN_WIDTH)
        val columnWidths = intrinsicWidths.map {
            it.coerceIn(MIN_COLUMN_WIDTH, widthCap.coerceAtLeast(MIN_COLUMN_WIDTH))
        }
        val totalWidth = columnWidths.fold(0.dp) { acc, width -> acc + width }
        // fit / overflow 分支：总内在宽超出可用宽（或父级无界宽兜底）→ 整表
        // 横向滚动；否则列按内容比例吃满可用宽。
        val needsScroll = maxWidth == Dp.Infinity || totalWidth > maxWidth
        val scrollState = rememberScrollState()
        val shape = RoundedCornerShape(dimens.tableCornerSize)
        // 表头浅底：surfaceVariant 半透明，明暗主题下都比表体略深一档。
        val headerBackground = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.45f)

        Surface(
            color = colors.tableBackground,
            shape = shape,
            modifier = Modifier.fillMaxWidth(),
        ) {
            Column(
                modifier = Modifier
                    // 表头底色/分隔线铺到圆角内（Surface 的背景带圆角，
                    // 子内容再显式裁一次保证不被方形底色穿出）。
                    .clip(shape)
                    .then(if (needsScroll) Modifier.horizontalScroll(scrollState) else Modifier),
            ) {
                if (table.hasHeader) {
                    TableCellRow(
                        content = content,
                        cells = table.header,
                        style = headerStyle,
                        alignments = table.alignments,
                        columnWidths = columnWidths,
                        needsScroll = needsScroll,
                        settings = settings,
                        modifier = Modifier.background(headerBackground),
                    )
                    HorizontalDivider(thickness = 1.dp, color = colors.dividerColor)
                }
                table.rows.forEachIndexed { index, row ->
                    if (index > 0) {
                        HorizontalDivider(
                            thickness = Dp.Hairline,
                            color = colors.dividerColor.copy(alpha = 0.5f),
                        )
                    }
                    TableCellRow(
                        content = content,
                        cells = row,
                        style = bodyStyle,
                        alignments = table.alignments,
                        columnWidths = columnWidths,
                        needsScroll = needsScroll,
                        settings = settings,
                    )
                }
            }
        }
    }
}

/**
 * 表格单行：各列按固定/加权宽排布（单元格垂直顶对齐，多行换行时阅读
 * 起线一致）。滚动模式下列宽 = 内在宽（超出部分靠整表横滚）；fit 模式下
 * weight(内在宽) 把富余空间按内容比例分给各列。单元格文本 softWrap
 * 竖向生长（显式 Clip + 不限行数，无省略号截断）。
 */
@Composable
private fun TableCellRow(
    content: String,
    cells: List<TableCell>,
    style: TextStyle,
    alignments: List<TextAlign>,
    columnWidths: List<Dp>,
    needsScroll: Boolean,
    settings: AnnotatorSettings,
    modifier: Modifier = Modifier,
) {
    Row(modifier = modifier, verticalAlignment = Alignment.Top) {
        cells.forEachIndexed { column, cell ->
            Box(
                modifier = Modifier
                    .then(
                        if (needsScroll) Modifier.width(columnWidths[column])
                        else Modifier.weight(columnWidths[column].value),
                    )
                    .padding(horizontal = CELL_PAD_H, vertical = CELL_PAD_V),
            ) {
                cell.node?.let { node ->
                    MarkdownTableBasicText(
                        content = content,
                        cell = node,
                        style = style.copy(
                            textAlign = alignments.getOrElse(column) { TextAlign.Left },
                        ),
                        maxLines = Int.MAX_VALUE,
                        overflow = TextOverflow.Clip,
                        annotatorSettings = settings,
                    )
                }
            }
        }
    }
}

/** 单元格内边距（计入列宽，底色/分隔线铺满列宽）。 */
private val CELL_PAD_H = 10.dp
private val CELL_PAD_V = 6.dp

/** 列宽上下限：下限防空列塌缩；上限防单列吃满整表（超限列内换行）。 */
private val MIN_COLUMN_WIDTH = 48.dp
private val MAX_COLUMN_WIDTH = 200.dp

/** 分隔行劈列：按未转义的 `|` 劈（`\|` 转义管道不劈列）。 */
private val SEPARATOR_SPLIT = Regex("""(?<!\\)\|""")

/** 合法对齐标记：`---` / `:---` / `---:` / `:---:`（前后冒号可选）。 */
private val COLUMN_ALIGN_MARKER = Regex("""^(:?)-+-(:?)$""")

/**
 * 表格解析（复用 GFM 解析器的 AST，不重写管道解析）：
 * - TABLE 的孩子为 HEADER / TABLE_SEPARATOR / ROW，单元格是 CELL 叶子；
 *   `\|` 转义管道由解析器处理（CELL 边界不会劈错）；
 * - 表头定列数（表头缺失时取最宽数据行兜底），数据行按列数截断/补空；
 * - 对齐标记从分隔行原文逐列解析（`:--:` 居中 / `--:` 右 / `:--` 左，
 *   不合法段按左对齐），再截断/补齐到列数；分隔行缺失（流式极端情况）
 *   全部左对齐；
 * - 单元格测量文本经库的 buildMarkdownAnnotatedString 生成（保留行内
 *   标记，实测宽度接近真实渲染）。
 */
private fun parseTableModel(
    content: String,
    node: ASTNode,
    headerStyle: TextStyle,
    bodyStyle: TextStyle,
    settings: AnnotatorSettings,
): TableModel {
    var separatorText: String? = null
    val headerNodes = mutableListOf<ASTNode>()
    val rowNodes = mutableListOf<List<ASTNode>>()
    node.children.forEach { child ->
        when (child.type) {
            GFMElementTypes.HEADER ->
                headerNodes += child.children.filter { it.type == GFMTokenTypes.CELL }
            GFMElementTypes.ROW ->
                rowNodes += child.children.filter { it.type == GFMTokenTypes.CELL }
            GFMTokenTypes.TABLE_SEPARATOR ->
                if (child.startOffset >= 0 && child.endOffset <= content.length) {
                    separatorText = content.substring(child.startOffset, child.endOffset)
                }
        }
    }
    val columns = maxOf(headerNodes.size, rowNodes.maxOfOrNull { it.size } ?: 0)
        .coerceAtLeast(1)
    fun cellOf(cellNode: ASTNode?, style: TextStyle) = TableCell(
        node = cellNode,
        text = cellNode
            // 0.45.0 中为 String 扩展：content.buildMarkdownAnnotatedString(...)。
            ?.let { content.buildMarkdownAnnotatedString(it, style, settings) }
            ?: AnnotatedString(""),
    )
    return TableModel(
        columns = columns,
        alignments = parseColumnAlignments(separatorText.orEmpty(), columns),
        hasHeader = headerNodes.isNotEmpty(),
        header = List(columns) { cellOf(headerNodes.getOrNull(it), headerStyle) },
        rows = rowNodes.map { row -> List(columns) { cellOf(row.getOrNull(it), bodyStyle) } },
    )
}

/**
 * 解析分隔行各列对齐标记：去首尾管道后按未转义管道劈段，逐段匹配
 * `:?-+:?`（`:--:` 居中 / `--:` 右 / 其余左）；列数不足以左对齐补齐、
 * 超出截断到列数。
 */
private fun parseColumnAlignments(raw: String, columns: Int): List<TextAlign> {
    if (columns <= 0) return emptyList()
    var body = raw.trim()
    if (body.startsWith("|")) body = body.substring(1)
    if (body.endsWith("|")) body = body.removeSuffix("|")
    val flags = body.split(SEPARATOR_SPLIT).map { segment ->
        val marker = COLUMN_ALIGN_MARKER.matchEntire(segment.trim())
        when {
            marker == null -> TextAlign.Left
            marker.groupValues[1] == ":" && marker.groupValues[2] == ":" -> TextAlign.Center
            marker.groupValues[2] == ":" -> TextAlign.Right
            else -> TextAlign.Left
        }
    }
    return List(columns) { flags.getOrElse(it) { TextAlign.Left } }
}

/**
 * 带行号的高亮代码区：SyntaxMP tokenizer 生成带偏移的 token（角色分层
 * Tag/Attribute/String/Comment/Keyword/Number…，支持 xml/html，svg 走
 * xml），布局为 Row { 固定行号列 | 共享横向滚动的代码列 }：
 * - 按逻辑行编号（行号对齐该逻辑行首行，软换行已禁用无此问题）；
 * - 行号列宽度用最大行号本身按渲染同款 style 实测（TextMeasurer），
 *   不猜位数/字宽，杜绝两位数行号被挤换行；
 * - 代码每行 softWrap = false，超宽经一个共享 ScrollState 整体横向
 *   滚动（各逻辑行同步滚动），行号列固定在左侧不随内容滚走。
 */
@Composable
private fun LineNumberedCode(body: String, language: String, modifier: Modifier = Modifier) {
    val colors = LocalMarkdownColors.current
    val style = LocalMarkdownTypography.current.code
    val dark = isSystemInDarkTheme()
    // SyntaxMP tokenizer：默认含全部内置语言（含 xml/html，svg 由别名转 xml）。
    val tokenizer = remember { SyntaxTokenizer() }
    // 角色配色随明暗主题切换（GitHub light/dark 风格）。
    val theme = remember(dark) { syntaxRoleColors(dark) }
    // 异步计算高亮（首帧先显示纯文本）；tokenizer 与 theme 均为只读输入，
    // 不再有共享可变 Builder 的竞态。
    val highlighted = produceState(
        initialValue = remember(body) { AnnotatedString(body) },
        body, language, tokenizer, theme,
    ) {
        value = buildSyntaxAnnotatedString(body, language, tokenizer, theme)
    }.value

    // lines 必须与被切的 highlighted 同源派生：produceState 在 body 变更
    // 后的首帧仍持有上一代高亮串（异步重算中），若 lines 取自新 body，
    // 新偏移会切在旧串上 → StringIndexOutOfBoundsException（流式增量
    // 首帧的必现窗口，曾致对话结束渲染时主线程崩溃退场）。同源派生下
    // 偏移与被切串恒为同一代，旧一代仅表现为内容晚一帧刷新。
    val lines = remember(highlighted) {
        highlighted.text.removeSuffix("\n").split("\n")
    }
    val textMeasurer = rememberTextMeasurer()
    val density = LocalDensity.current
    // 行号列宽：以最大行号字符串（与渲染完全相同的 style）实测宽度；
    // 间距用独立 Spacer，避免 padding 挤占固定宽度导致换行。
    val maxLineNumber = lines.size.toString()
    val numberWidth = remember(textMeasurer, density, maxLineNumber, style) {
        with(density) {
            textMeasurer.measure(maxLineNumber, style).size.width.toDp()
        }
    }
    // 所有代码行共享一个滚动状态，横向滚动整列同步。
    val codeScroll = rememberScrollState()

    // SelectionContainer 会拦截代码文本上的横向拖拽（与 horizontalScroll 手势
    // 冲突导致滑动失效）；库内代码块本就不支持选择，复制走头部按钮，故不包裹。
    Row(
        modifier = modifier.fillMaxWidth(),
        verticalAlignment = Alignment.Top,
    ) {
        // 固定行号列：不参与横向滚动。
        Column {
            lines.forEachIndexed { index, _ ->
                Text(
                    text = "${index + 1}",
                    style = style,
                    color = colors.text.copy(alpha = 0.5f),
                    textAlign = TextAlign.Right,
                    softWrap = false,
                    modifier = Modifier.width(numberWidth),
                )
            }
        }
        Spacer(Modifier.width(8.dp))
        // 代码列：weight(1f) 定宽为视口，horizontalScroll 允许子行按自然
        // 宽度（softWrap = false，无 fillMaxWidth）超出并整体同步滚动。
        Column(
            modifier = Modifier
                .weight(1f)
                .horizontalScroll(codeScroll),
        ) {
            var start = 0
            lines.forEachIndexed { _, line ->
                val lineStart = start
                start += line.length + 1
                // subSequence 会保留并平移该行内的高亮 span；空行占位保持行高。
                // 双端 clamp 兜底：同源派生下恒不越界，此为对未来派生方式
                // 漂移的防御（越界即主线程 FATAL，代价必须无限高）。
                val from = lineStart.coerceAtMost(highlighted.length)
                val to = (lineStart + line.length).coerceAtMost(highlighted.length)
                val lineText = highlighted.subSequence(from, to)
                    .ifEmpty { AnnotatedString(" ") }
                Text(
                    text = lineText,
                    style = style,
                    color = colors.text,
                    softWrap = false,
                )
            }
        }
    }
}

/**
 * SyntaxMP 版高亮构建：tokenize 拿到带偏移的 [SyntaxTokenSpan]，把角色
 * 解析为颜色（逐级回退：keyword.declaration → keyword）后映射为 SpanStyle。
 * 输入与输出均为只读数据，可在 produceState 中安全异步计算。
 */
internal fun buildSyntaxAnnotatedString(
    body: String,
    language: String,
    tokenizer: SyntaxTokenizer,
    theme: Map<String, Color>,
): AnnotatedString {
    if (body.isEmpty()) return AnnotatedString(body)
    val label = resolveLanguageLabel(language) ?: return AnnotatedString(body)
    val spans = tokenizer.tokenize(body, label)
    if (spans.isEmpty()) return AnnotatedString(body)
    return AnnotatedString.Builder().apply {
        append(body)
        spans.forEach { span ->
            val color = theme.pickRoleColor(span.role) ?: return@forEach
            addStyle(SpanStyle(color = color), span.start, span.endExclusive)
        }
    }.toAnnotatedString()
}

/** 角色颜色查找：从最具体角色逐级回退到根角色（rolesFromRoot 为根→叶序）。 */
private fun Map<String, Color>.pickRoleColor(role: SyntaxRole): Color? {
    role.rolesFromRoot().asReversed().forEach { candidate ->
        get(candidate.value)?.let { return it }
    }
    return null
}

/**
 * GitHub light/dark 风格的角色配色（tag/attribute/string/comment/keyword/
 * number/function 等），isSystemInDarkTheme() 切换。未列出的角色（如
 * operator/punctuation 有列出；其余）回退到 Text 的基础色。
 *
 * markdown 专属角色（仅 markdown tokenizer 产生）：fence/list/table/
 * thematic-break/strikethrough 归 punctuation 灰（结构性标记弱化），
 * language（fence 信息串 string.language 的语言段）用 variable 橙与
 * string 蓝区分。internal：SourceCodeDialog（源码查看器）与代码块共用。
 */
internal fun syntaxRoleColors(dark: Boolean): Map<String, Color> = if (dark) mapOf(
    "comment" to Color(0xFF8B949E),
    "string" to Color(0xFFA5D6FF),
    "keyword" to Color(0xFFFF7B72),
    "number" to Color(0xFF79C0FF),
    "function" to Color(0xFFD2A8FF),
    "tag" to Color(0xFF7EE787),
    "attribute" to Color(0xFF79C0FF),
    "variable" to Color(0xFFFFA657),
    "type" to Color(0xFFFFA657),
    "constant" to Color(0xFF79C0FF),
    "property" to Color(0xFF79C0FF),
    "annotation" to Color(0xFFFF7B72),
    "escape" to Color(0xFF79C0FF),
    "markup" to Color(0xFF79C0FF),
    "operator" to Color(0xFFFF7B72),
    "punctuation" to Color(0xFF8B949E),
    "fence" to Color(0xFF8B949E),
    "list" to Color(0xFF8B949E),
    "table" to Color(0xFF8B949E),
    "thematic-break" to Color(0xFF8B949E),
    "strikethrough" to Color(0xFF8B949E),
    "language" to Color(0xFFFFA657),
) else mapOf(
    "comment" to Color(0xFF6E7781),
    "string" to Color(0xFF0A3069),
    "keyword" to Color(0xFFCF222E),
    "number" to Color(0xFF0550AE),
    "function" to Color(0xFF8250DF),
    "tag" to Color(0xFF116329),
    "attribute" to Color(0xFF0550AE),
    "variable" to Color(0xFF953800),
    "type" to Color(0xFF953800),
    "constant" to Color(0xFF0550AE),
    "property" to Color(0xFF0550AE),
    "annotation" to Color(0xFFCF222E),
    "escape" to Color(0xFF0550AE),
    "markup" to Color(0xFF0550AE),
    "operator" to Color(0xFFCF222E),
    "punctuation" to Color(0xFF57606A),
    "fence" to Color(0xFF57606A),
    "list" to Color(0xFF57606A),
    "table" to Color(0xFF57606A),
    "thematic-break" to Color(0xFF57606A),
    "strikethrough" to Color(0xFF57606A),
    "language" to Color(0xFF953800),
)

/**
 * 语言标签归一化：SyntaxMP 的 tokenizer 自带大量别名（js/py/kt/rs/sh/
 * bash/zsh/golang/ts/c++/c#/yaml…），这里只补它没有的映射；
 * null = 纯文本（不加 token 高亮）。未列出的标签原样透传，tokenizer
 * 对未知标签返回空 token 列表、不抛异常。
 */
private val LANGUAGE_ALIASES: Map<String, String?> = mapOf(
    // SVG 不在 SyntaxMP 语言表 → 走 xml（标记语言高亮：tag/attribute/string）。
    "svg" to "xml",
    "mjs" to "js",
    "python3" to "python",
    // SyntaxMP 无 CoffeeScript tokenizer → 纯文本
    "coffee" to null, "coffeescript" to null,
    // 纯文本系：null = 不高亮
    "text" to null, "txt" to null, "plain" to null, "plaintext" to null,
)

private fun resolveLanguageLabel(language: String): String? {
    val key = language.trim().lowercase()
    return if (LANGUAGE_ALIASES.containsKey(key)) {
        LANGUAGE_ALIASES[key] // null = 纯文本
    } else {
        key
    }
}
