package dev.openslate.mobile.ui

import android.os.Build
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Check
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.graphics.Shape
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntRect
import androidx.compose.ui.unit.IntSize
import androidx.compose.ui.unit.LayoutDirection
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Popup
import androidx.compose.ui.window.PopupPositionProvider
import androidx.compose.ui.window.PopupProperties
import dev.chrisbanes.haze.HazeInput
import dev.chrisbanes.haze.HazeState
import dev.chrisbanes.haze.blur.HazeBlurStyle
import dev.chrisbanes.haze.blur.HazeColorEffect
import dev.chrisbanes.haze.blur.hazeBlur
import kotlin.math.roundToInt

/**
 * 玻璃质感弹层（共享件：ComposerPanel「本次对话模型」+ 设置页功能模型绑定）。
 *
 * 为什么不用 M3 DropdownMenu：其面板是实心 surface（不透明容器 + 独立窗口），
 * 与 ChatScreen 周围的半透明磨砂语言（GlassPanel）割裂；容器色不可定制
 * （M3 未暴露 containerColor）。
 *
 * 磨砂方案 = Haze 2.0.1 弹层局部内容源真实模糊（hazeSource + hazeBlur）：
 * - 宿主屏在弹层背后的内容区标 `Modifier.hazeSource(hazeState)`（ChatScreen
 *   聊天内容区 / SettingsScreen 会话页内容列，覆盖键盘上方与列表底部）；
 * - 弹层表面挂 `Modifier.hazeBlur(HazeInput.Sources(hazeState), style)`，
 *   只有弹层那一块区域被真实模糊（糊的是真实宿主内容），玻璃参数（tint/
 *   0.5dp 边框/轻阴影/16dp 圆角）不变；
 * - 模糊不可用路径（API < 31 或系统关闭）由 style 的 `blurEnabled` +
 *   `fallbackColorEffect` 承接：高不透明（0.92）柔和蒙层，观感与旧版蒙层档
 *   一致；31+ 真实模糊路径用 0.6 轻主题色叠加。此处显式按 SDK_VERSION 门控
 *   blurEnabled（haze 31- 有 RenderScript 委托路径，不赌其可用性）。
 * - 旧版窗口 FLAG_BLUR_BEHIND 路径已移除（整屏后景模糊过重，且会与 haze
 *   局部模糊叠加成双重模糊）。
 *
 * 弹层窗口是 Popup 子窗口且 focusable：点外部/返回键触发 onDismissRequest、
 * 与 IME 的共存关系与既有 DropdownMenu 完全一致。进出场与面板同族
 * （fadeIn + expandVertically 自底 / fadeOut + shrinkVertically），退场期间
 * Popup 保持组合至动画结束（M3 DropdownMenu 同款 MutableTransitionState 模式）。
 */

/** 弹层选项：value 回传原值（models 条目键），label 主文字，subtitle 可选副行。 */
internal data class GlassDropdownItem(
    val value: String,
    val label: String,
    val subtitle: String? = null,
    val selected: Boolean = false,
)

/** 弹层离锚点的间距 + 面板最大高度（超出内部滚动）。 */
private val GlassMenuMargin = 8.dp
private val GlassMenuMaxHeight = 360.dp

/** 真实模糊半径：18dp 局部磨砂（与 GlassPanel 的 12dp 同族、弹层面略重）。 */
private val GlassMenuBlurRadius = 18.dp

/**
 * 锚点定位：横向左缘对齐锚点并夹取到窗口内；纵向优先展开在锚点下方，
 * 放不下翻转到上方（composer 贴屏幕底部 → 自动向上开），再夹取兜底。
 */
private class AnchorFlipPositionProvider(private val marginPx: Int) : PopupPositionProvider {
    override fun calculatePosition(
        anchorBounds: IntRect,
        windowSize: IntSize,
        layoutDirection: LayoutDirection,
        popupContentSize: IntSize,
    ): IntOffset {
        val x = anchorBounds.left.coerceIn(0, (windowSize.width - popupContentSize.width).coerceAtLeast(0))
        val yBelow = anchorBounds.bottom + marginPx
        val fitsBelow = yBelow + popupContentSize.height <= windowSize.height - marginPx
        val y = if (fitsBelow) yBelow else anchorBounds.top - marginPx - popupContentSize.height
        return IntOffset(x, y.coerceIn(0, (windowSize.height - popupContentSize.height).coerceAtLeast(0)))
    }
}

/**
 * 弹层玻璃 style（Haze）：blurEnabled 按 SDK 门控（31+ 真实局部模糊，
 * 31- 走 fallbackColorEffect 高不透明蒙层）；模糊路径 tint 0.6 / fallback
 * 0.92 与聚焦面板的不透明度分档同阈值。backgroundColor 给 blur 合成提供
 * 不透明底（弹层背后的页面底色），保证模糊输入可预期。
 */
@Composable
private fun rememberGlassMenuStyle(): HazeBlurStyle {
    val surfaceVariant = MaterialTheme.colorScheme.surfaceVariant
    val base = MaterialTheme.colorScheme.surface
    val blurEnabled = Build.VERSION.SDK_INT >= Build.VERSION_CODES.S
    return remember(surfaceVariant, base, blurEnabled) {
        HazeBlurStyle {
            blurEnabled(blurEnabled)
            blurRadius(GlassMenuBlurRadius)
            backgroundColor(base)
            colorEffects(listOf(HazeColorEffect.tint(surfaceVariant.copy(alpha = 0.6f))))
            fallbackColorEffect(HazeColorEffect.tint(surfaceVariant.copy(alpha = 0.92f)))
        }
    }
}

/**
 * 玻璃弹层本体。调用方把它放在锚点行的同一 Box 内（Popup 以父布局锚定），
 * 展开时在锚点上/下方弹出条目列。[hazeState] 为宿主屏标注过 hazeSource 的
 * 状态（ChatScreen / SettingsScreen 各自持有并传入）。空 items 不弹（调用
 * 方一般已禁用触发行）。
 */
@Composable
internal fun GlassDropdownMenu(
    expanded: Boolean,
    onDismissRequest: () -> Unit,
    hazeState: HazeState,
    items: List<GlassDropdownItem>,
    modifier: Modifier = Modifier,
    shape: Shape = RoundedCornerShape(16.dp),
    maxHeight: Dp = GlassMenuMaxHeight,
    onSelect: (GlassDropdownItem) -> Unit,
) {
    // 退场动画期间保持 Popup 组合（currentState 在退场结束前仍为 true）。
    val expandedState = remember { MutableTransitionState(false) }
    expandedState.targetState = expanded
    if (expandedState.currentState || expandedState.targetState) {
        val density = LocalDensity.current
        val marginPx = with(density) { GlassMenuMargin.toPx().roundToInt() }
        val style = rememberGlassMenuStyle()
        Popup(
            onDismissRequest = onDismissRequest,
            properties = PopupProperties(
                focusable = true,
                dismissOnBackPress = true,
                dismissOnClickOutside = true,
            ),
            popupPositionProvider = AnchorFlipPositionProvider(marginPx),
        ) {
            Box(
                modifier = modifier
                    .widthIn(max = 320.dp)
                    .shadow(elevation = 2.dp, shape = shape, clip = false)
                    .graphicsLayer {
                        this.shape = shape
                        clip = true
                    }
                    // 玻璃面 = Haze 局部模糊（含 fallback 蒙层路径），整块
                    // 面板背景由 haze 绘制；圆角裁剪交给上层 graphicsLayer。
                    .hazeBlur(
                        HazeInput.Sources(hazeState),
                        style = style,
                    )
                    .border(0.5.dp, MaterialTheme.colorScheme.outline.copy(alpha = 0.4f), shape),
            ) {
                AnimatedVisibility(
                    visibleState = expandedState,
                    enter = fadeIn() + expandVertically(expandFrom = Alignment.Bottom),
                    exit = fadeOut() + shrinkVertically(shrinkTowards = Alignment.Bottom),
                ) {
                    Column(
                        Modifier
                            .heightIn(max = maxHeight)
                            .verticalScroll(rememberScrollState()),
                    ) {
                        for (item in items) {
                            GlassDropdownMenuItem(item = item, onClick = {
                                onSelect(item)
                                onDismissRequest()
                            })
                        }
                    }
                }
            }
        }
    }
}

/** 弹层条目行：主文字（+可选副行）居左，选中带 Check 图标（ui.md 基线）。 */
@Composable
private fun GlassDropdownMenuItem(item: GlassDropdownItem, onClick: () -> Unit) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(horizontal = 14.dp, vertical = 10.dp),
    ) {
        Column(Modifier.weight(1f)) {
            Text(
                item.label,
                style = MaterialTheme.typography.bodyMedium,
                color = if (item.selected) {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.onSurface
                },
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
            )
            item.subtitle?.let {
                Text(
                    it,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                )
            }
        }
        if (item.selected) {
            Icon(
                Icons.Filled.Check,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.primary,
                modifier = Modifier.size(18.dp),
            )
        }
    }
}
