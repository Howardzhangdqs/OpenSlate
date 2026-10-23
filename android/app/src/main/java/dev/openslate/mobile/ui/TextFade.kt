package dev.openslate.mobile.ui

import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.drawWithContent
import androidx.compose.ui.graphics.BlendMode
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.CompositingStrategy
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * 尾缘淡出掩膜：单行文本超宽时以渐变收尾，替代 [androidx.compose.ui.text.style.TextOverflow.Ellipsis]
 * 的「…」——文字在尾缘平滑淡出而非截断符提示（下拉胶囊值文本与弹层
 * 条目主/副文字的既定视觉，2026-10 用户拍板）。
 *
 * 实现要点：
 * - [BlendMode.DstIn] 让内容 alpha 乘上遮罩 alpha（黑→透明的水平渐变）：
 *   尾缘内容渐隐、遮罩未覆盖的区域原样——对任意背景成立（软底/玻璃均
 *   半透明，不能走「叠背景色渐变」的老办法）；
 * - [CompositingStrategy.Offscreen] 隔离合成层：DstIn 只与本节点已画
 *   内容混合，不漏到底下背景或兄弟节点；
 * - **仅溢出时挂载**（调用方经 Text `onTextLayout` 的 `didOverflowWidth`
 *   检测后用 [fadeOutTrailingEdgeIf] 条件挂上）：短文本不淡出，避免
 *   完整内容被无端吃掉一截；
 * - 配套 Text 参数：overflow 必须是 [androidx.compose.ui.text.style.TextOverflow.Clip]
 *   （Ellipsis 会先截成「…」再淡出，视觉语义重复）**且 softWrap = false**
 *   （默认折行 + maxLines=1 会把超宽文本折到第二行整行裁掉，不是水平
 *   截断——2026-10 真机实测「provider 名后的模型名整个消失」即此因）。
 */
internal fun Modifier.fadeOutTrailingEdge(fadeWidth: Dp = 20.dp): Modifier =
    graphicsLayer(compositingStrategy = CompositingStrategy.Offscreen)
        .drawWithContent {
            drawContent()
            val fadePx = fadeWidth.toPx().coerceAtMost(size.width)
            drawRect(
                brush = Brush.horizontalGradient(
                    colors = listOf(Color.Black, Color.Transparent),
                    startX = size.width - fadePx,
                    endX = size.width,
                ),
                blendMode = BlendMode.DstIn,
            )
        }

/** [fadeOutTrailingEdge] 的条件挂载形式：未溢出原样返回（见类级 kdoc
 *  的「仅溢出时挂载」约定）。 */
internal fun Modifier.fadeOutTrailingEdgeIf(
    overflowed: Boolean,
    fadeWidth: Dp = 20.dp,
): Modifier = if (overflowed) fadeOutTrailingEdge(fadeWidth) else this
