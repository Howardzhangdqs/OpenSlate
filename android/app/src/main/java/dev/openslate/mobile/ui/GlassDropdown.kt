package dev.openslate.mobile.ui

import android.content.Context
import android.os.Build
import android.view.View
import android.view.WindowManager
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
import androidx.compose.runtime.DisposableEffect
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
import androidx.compose.ui.platform.LocalView
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
import kotlin.math.roundToInt

/**
 * 玻璃质感弹层（共享件：ComposerPanel「本次对话模型」+ 设置页功能模型绑定）。
 *
 * 为什么不用 M3 DropdownMenu：其面板是实心 surface（不透明容器 + 独立窗口），
 * 与 ChatScreen 周围的半透明磨砂语言（GlassPanel）割裂；容器色不可定制
 * （M3 未暴露 containerColor）。为什么不复用 GlassPanel：GlassPanel 的真模糊
 * 依赖应用内共享内容捕获层（glass.source GraphicsLayer），而弹层是独立窗口
 * （PopupWindow 子窗口，独立 Surface），跨窗口引用他窗 RenderNode 不可行。
 *
 * 磨砂方案（两层）：
 * - 基底 = GlassPanel 同族模拟：surfaceVariant 半透明 tint + 0.5dp 细边框 +
 *   轻阴影 + 16dp 圆角；不透明度按有无真模糊分档（0.6 / 0.92，与聚焦面板同
 *   阈值）——API 31 以下或系统关掉跨窗模糊时，靠近实底蒙层模拟玻璃拟态。
 * - API 31+ 增强 = 窗口级 FLAG_BLUR_BEHIND：弹层窗口经 WindowManager 抬起，
 *   取 contentView 根视图的 WindowManager.LayoutParams 追加 blur 标志与半径
 *   后 updateViewLayout 回写，系统对窗口背后的真实内容（聊天列表/设置页）
 *   做真模糊——不依赖应用内捕获层，任何窗口都适用。模糊能力被系统关闭
 *   （省电模式/不支持的 ROM，`isCrossWindowBlurEnabled`）时跳过，自然落到
 *   蒙层档。窗口随弹层销毁，无需还原。
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
 * 玻璃弹层本体。调用方把它放在锚点行的同一 Box 内（Popup 以父布局锚定），
 * 展开时在锚点上/下方弹出条目列。空 items 不弹（调用方一般已禁用触发行）。
 */
@Composable
internal fun GlassDropdownMenu(
    expanded: Boolean,
    onDismissRequest: () -> Unit,
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
        Popup(
            onDismissRequest = onDismissRequest,
            properties = PopupProperties(
                focusable = true,
                dismissOnBackPress = true,
                dismissOnClickOutside = true,
            ),
            popupPositionProvider = AnchorFlipPositionProvider(marginPx),
        ) {
            val blurBehind = rememberPopupWindowBlur()
            val tint = MaterialTheme.colorScheme.surfaceVariant.copy(
                alpha = if (blurBehind) 0.6f else 0.92f,
            )
            Box(
                modifier = modifier
                    .widthIn(max = 320.dp)
                    .shadow(elevation = 2.dp, shape = shape, clip = false)
                    .graphicsLayer {
                        this.shape = shape
                        clip = true
                    }
                    .background(tint)
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

/**
 * 给当前（Popup）窗口叠加 FLAG_BLUR_BEHIND 真背景模糊，返回是否实际生效。
 * API < 31 直接 false（玻璃拟态蒙层档）；31+ 需系统跨窗模糊开启。拿窗口的
 * 方式：弹层内容根视图（LocalView）回溯 rootView，其 layoutParams 即抬窗时
 * 的 WindowManager.LayoutParams，追加标志与 blurBehindRadius 后经
 * WindowManager.updateViewLayout 回写。视图尚未 attach（理论不可达，Popup
 * 先抬窗后组内容）时挂 attach 监听补做。窗口随弹层销毁，无需还原。
 */
@Composable
private fun rememberPopupWindowBlur(): Boolean {
    val view = LocalView.current
    val blurRadiusPx = with(LocalDensity.current) { 20.dp.toPx() }.roundToInt()
    var applied by remember { mutableStateOf(false) }
    DisposableEffect(view) {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.S) return@DisposableEffect onDispose { }
        val apply: () -> Unit = apply@{
            if (applied) return@apply
            val root = view.rootView
            val lp = root.layoutParams as? WindowManager.LayoutParams ?: return@apply
            val wm = view.context.getSystemService(Context.WINDOW_SERVICE) as WindowManager
            if (!wm.isCrossWindowBlurEnabled) return@apply
            if (lp.flags and WindowManager.LayoutParams.FLAG_BLUR_BEHIND == 0) {
                lp.flags = lp.flags or WindowManager.LayoutParams.FLAG_BLUR_BEHIND
                lp.blurBehindRadius = blurRadiusPx
                runCatching { wm.updateViewLayout(root, lp) }.onFailure { return@apply }
            }
            applied = true
        }
        if (view.isAttachedToWindow) {
            apply()
        } else {
            val listener = object : View.OnAttachStateChangeListener {
                override fun onViewAttachedToWindow(v: View) {
                    view.removeOnAttachStateChangeListener(this)
                    apply()
                }

                override fun onViewDetachedFromWindow(v: View) {}
            }
            view.addOnAttachStateChangeListener(listener)
            return@DisposableEffect onDispose { view.removeOnAttachStateChangeListener(listener) }
        }
        onDispose { }
    }
    return applied
}
