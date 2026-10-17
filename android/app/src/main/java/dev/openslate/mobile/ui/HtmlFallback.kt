package dev.openslate.mobile.ui

import android.graphics.Color as AndroidColor
import android.view.View
import android.view.ViewGroup
import android.webkit.WebResourceRequest
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.fromHtml
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView

/** 结构性 / 复杂标签：Compose Text 无法表达，走 WebView。 */
private val COMPLEX_TAGS = listOf(
    "<svg", "<table", "<style", "<script", "<iframe", "<form",
    "<video", "<audio", "<canvas", "<object", "<embed",
)

/** 简单行内标签白名单（可被 Spanned→AnnotatedString 表达）。 */
private val SIMPLE_TAG = Regex(
    pattern = """</?(?:b|strong|i|em|u|ins|s|strike|del|a|br|hr|p|span|code|pre|blockquote|ul|ol|li|h[1-6]|sub|sup|small|big|font|tt|img)\b[^<>]*>""",
    option = RegexOption.IGNORE_CASE,
)

/** 任意标签（用于白名单校验）。 */
private val ANY_TAG = Regex("""<[a-zA-Z!/][^<>]*>""")

/** `<img>` 降级：简单分支不加载图片，替换为链接文本。 */
private val IMG_TAG = Regex(
    pattern = """<img\b[^>]*?src\s*=\s*["']([^"']*)["'][^>]*>""",
    option = RegexOption.IGNORE_CASE,
)

/** 嵌套 `<div>`（两个及以上视为结构性容器）。 */
private val DIV_TAG = Regex("""<div\b""", RegexOption.IGNORE_CASE)

private fun isComplexHtml(html: String): Boolean {
    if (COMPLEX_TAGS.any { html.contains(it, ignoreCase = true) }) return true
    if (DIV_TAG.findAll(html).count() > 1) return true
    // 白名单之外的标签 → 交给 WebView 兜底。
    return ANY_TAG.findAll(html).any { !SIMPLE_TAG.matches(it.value) }
}

/**
 * Markdown 中 HTML 块的降级渲染：
 * - 简单标签（b/i/u/a/br/img…）→ Compose 原生 Text（HtmlCompat +
 *   Spanned.toAnnotatedString），`<img>` 降级为链接文本；
 * - 复杂 HTML（svg/table/style/嵌套 div…）→ WebView 渲染（禁 JS/文件访问，
 *   高度自适应无内部滚动，配色跟随主题不白底突兀）。
 */
@Composable
fun HtmlFallback(
    content: String,
    modifier: Modifier = Modifier,
) {
    val html = content.trim()
    if (html.isEmpty()) return
    if (isComplexHtml(html)) HtmlWebView(html, modifier) else SimpleHtmlText(html, modifier)
}

/** 简单 HTML：AnnotatedString.fromHtml（内部即 HtmlCompat），原生 Text 渲染。 */
@Composable
private fun SimpleHtmlText(html: String, modifier: Modifier) {
    val annotated = remember(html) {
        val degraded = IMG_TAG.replace(html) { m -> "[图片] ${m.groupValues[1]}" }
        AnnotatedString.fromHtml(degraded)
    }
    Text(
        text = annotated,
        style = MaterialTheme.typography.bodyMedium,
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        modifier = modifier.fillMaxWidth(),
    )
}

/** 复杂 HTML：WebView 渲染，页面量得内容高后驱动 Compose 高度（无内部滚动）。 */
@Composable
private fun HtmlWebView(html: String, modifier: Modifier) {
    val context = LocalView.current.context
    // 页面内容高度（CSS px ≈ dp）；0 表示尚未量得，先用最小占位高。
    val contentHeight = remember { mutableIntStateOf(0) }
    val textColor = MaterialTheme.colorScheme.onSurfaceVariant
    val strongColor = MaterialTheme.colorScheme.onSurface
    val outlineColor = MaterialTheme.colorScheme.outlineVariant
    val textHex = remember(textColor) { "%06X".format(textColor.toArgb() and 0xFFFFFF) }
    val strongHex = remember(strongColor) { "%06X".format(strongColor.toArgb() and 0xFFFFFF) }
    val borderHex = remember(outlineColor) { "%06X".format(outlineColor.toArgb() and 0xFFFFFF) }
    // 表头 tonal 底：onSurface 的 4% alpha 转成 rgba()，跟随主题取色、无硬编码值。
    val headerTint = remember(strongColor) {
        val argb = strongColor.toArgb()
        "rgba(${(argb shr 16) and 0xFF},${(argb shr 8) and 0xFF},${argb and 0xFF},0.04)"
    }

    // 透明背景 + 主题文字色，避免深色模式下白底黑字突兀。
    // 表格：表头极轻 tonal 底 + outlineVariant 发丝边框，正文行不铺底色。
    val styled = remember(html, textHex, strongHex, borderHex, headerTint) {
        """
        <!DOCTYPE html><html><head><meta charset="utf-8">
        <meta name="viewport" content="width=device-width, initial-scale=1">
        <style>
        html,body{margin:0;padding:0;background:transparent;color:#$textHex;
        font-family:sans-serif;font-size:14px;line-height:1.5}
        table{border-collapse:collapse}
        td,th{border:1px solid #$borderHex;padding:4px 8px;text-align:left}
        th{background:$headerTint;color:#$strongHex;font-weight:600}
        img,svg,video{max-width:100%;height:auto}
        *{max-width:100%}
        </style></head><body>$html</body></html>
        """.trimIndent()
    }

    val webView = remember {
        WebView(context).apply {
            layoutParams = ViewGroup.LayoutParams(
                ViewGroup.LayoutParams.MATCH_PARENT,
                ViewGroup.LayoutParams.WRAP_CONTENT,
            )
            setBackgroundColor(AndroidColor.TRANSPARENT)
            isVerticalScrollBarEnabled = false
            isHorizontalScrollBarEnabled = false
            overScrollMode = View.OVER_SCROLL_NEVER
            settings.javaScriptEnabled = false
            settings.allowFileAccess = false
            settings.allowContentAccess = false
            webViewClient = object : WebViewClient() {
                override fun onPageFinished(view: WebView, url: String) {
                    // evaluateJavascript 属宿主注入 API，不受 javaScriptEnabled=false 影响。
                    view.evaluateJavascript(
                        "(function(){return document.body.scrollHeight;})()",
                    ) { value ->
                        value.toFloatOrNull()?.let { contentHeight.intValue = it.toInt() }
                    }
                }

                override fun shouldOverrideUrlLoading(
                    view: WebView,
                    request: WebResourceRequest,
                ): Boolean = true // 禁止任何跳转
            }
        }
    }
    DisposableEffect(webView) {
        onDispose {
            webView.stopLoading()
            webView.destroy()
        }
    }

    AndroidView(
        factory = { webView },
        update = { wv ->
            // tag 记录已加载内容，避免重组触发重复 loadData。
            if (wv.tag != styled) {
                wv.tag = styled
                wv.loadDataWithBaseURL(null, styled, "text/html", "utf-8", null)
            }
        },
        modifier = modifier
            .fillMaxWidth()
            .then(
                if (contentHeight.intValue > 0) {
                    // scrollHeight 为 CSS px，与 dp 1:1；加 2dp 余量防裁切。
                    Modifier.height(contentHeight.intValue.dp + 2.dp)
                } else {
                    Modifier.heightIn(min = 2.dp)
                },
            ),
    )
}
