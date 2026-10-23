package dev.openslate.mobile.ui

import android.os.Build
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.FastOutSlowInEasing
import androidx.compose.animation.core.MutableTransitionState
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.scaleIn
import androidx.compose.animation.scaleOut
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
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
import androidx.compose.ui.graphics.TransformOrigin
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextMeasurer
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.rememberTextMeasurer
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Density
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
 * 与 IME 的共存关系与既有 DropdownMenu 完全一致。进出场 = 玻璃 Box 外层的
 * 纯绘制相位过渡（fadeIn + scaleIn 自底 origin / fadeOut + scaleOut）：玻璃
 * 面布局尺寸首帧定型，Haze 模糊区域不随动画逐帧伸缩（布局尺寸与动画解耦
 * ——尺寸驱动的进出场会让模糊逐帧重配，收起帧全丢）；退场期间 Popup 保持
 * 组合至动画结束（M3 DropdownMenu 同款 MutableTransitionState 模式）。
 */

/** 弹层选项：value 回传原值（models 条目键），label 主文字，subtitle 可选副行。 */
internal data class GlassDropdownItem(
    val value: String,
    val label: String,
    val subtitle: String? = null,
    val selected: Boolean = false,
    /** 可选前置图标（Material ImageVector；null = 无——既有调用方零变化，
     *  上下文菜单等场景传入以强化条目语义）。 */
    val leadingIcon: ImageVector? = null,
)

/** 弹层离锚点的间距 + 面板最大高度（超出内部滚动）。 */
private val GlassMenuMargin = 8.dp
private val GlassMenuMaxHeight = 360.dp

/**
 * 面板宽度（内容自适应）：最终宽 = max(条目实测单行宽) 收口到
 * [GlassMenuMinWidth, 上限]，Modifier.width 显式钉在玻璃面（Box）上。
 *
 * 为什么实测而不用 IntrinsicSize.Min：条目文字列挂 weight(1f)，Compose
 * intrinsic 遍历中 weighted 子项不贡献主轴宽（其主轴尺寸由剩余空间定义，
 * intrinsic 查询时不可知）→ 列 minIntrinsic ≈ 图标+Check+padding ≈ 74dp，
 * 被 widthIn(min) 钉死成常宽、文字全截断（2026-10 真机翻车记录）。文字宽
 * 改用 TextMeasurer 按渲染同款样式实测（同 EnhancedMarkdownTable 列宽思路）。
 */
private val GlassMenuMinWidth = 120.dp

/** 屏宽推算上限时两侧各留的安全边距。 */
private val GlassMenuScreenMargin = 16.dp

/** 绝对上限兜底：平板/分屏等大屏场景菜单不无限跟屏宽。 */
private val GlassMenuAbsoluteMaxWidth = 420.dp

/** 条目行内固定件规格（布局与 TextMeasurer 实测共用，保证两侧一致）。 */
private val GlassItemIconSize = 18.dp
private val GlassItemIconGap = 10.dp
private val GlassItemPaddingHorizontal = 14.dp

/** 真实模糊半径：18dp 局部磨砂（与 GlassPanel 的 12dp 同族、弹层面略重）。 */
private val GlassMenuBlurRadius = 18.dp

/**
 * 锚点定位：横向左缘对齐锚点并夹取到窗口内；纵向优先展开在锚点下方，
 * 放不下翻转到上方（composer 贴屏幕底部 → 自动向上开），再夹取兜底。
 */
private class AnchorFlipPositionProvider(
    private val marginPx: Int,
    private val alignEnd: Boolean = false,
) : PopupPositionProvider {
    override fun calculatePosition(
        anchorBounds: IntRect,
        windowSize: IntSize,
        layoutDirection: LayoutDirection,
        popupContentSize: IntSize,
    ): IntOffset {
        // alignEnd：弹层右缘对锚点右缘（值控件贴行右缘时弹层从右侧展开）；
        // 默认 Start：弹层左缘对锚点左缘。超窗宽一律收进窗口内。
        val x = (
            if (alignEnd) anchorBounds.right - popupContentSize.width
            else anchorBounds.left
            ).coerceIn(0, (windowSize.width - popupContentSize.width).coerceAtLeast(0))
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
 * 方一般已禁用触发行）。[labelFontFamily] 可选注入条目主文字字体（如模型
 * 菜单传 MonoFont 等宽子集），null = 默认字体，行为不变。
 *
 * 宽度内容自适应：面板宽 = 最宽条目的实测单行自然宽（TextMeasurer，条目
 * 行全体等宽对齐），上限随屏宽收口、下限 [GlassMenuMinWidth] 只兜极短菜单
 * ——上限之内文字完整显示（最宽条目天然贴边），超上限才 Ellipsis。见常量
 * 定义处注释。
 *
 * [focusable]（默认 true，既有调用方零变化）：Popup 是否抢窗口焦点。
 * 抢焦点的副作用——底层输入框失焦，系统**瞬间无动画**收起 IME（composer
 * 面板整体在 imePadding 之上，键盘一塌面板跟着跳）。传 false 弹层不持
 * 焦点、键盘保持；代价是非聚焦窗口收不到 BACK 键（dismissOnBackPress
 * 失效，返回键作用于底层界面——先收 IME 再走导航，弹层保持打开）；
 * 点外部 dismiss（dismissOnClickOutside 不依赖窗口焦点）与菜单项点击
 * （Compose clickable 不需要窗口焦点）均正常。composer 双下拉场景
 * 可接受该取舍。
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
    labelFontFamily: FontFamily? = null,
    alignEnd: Boolean = false,
    focusable: Boolean = true,
    onSelect: (GlassDropdownItem) -> Unit,
) {
    // 退场动画期间保持 Popup 组合（currentState 在退场结束前仍为 true）。
    val expandedState = remember { MutableTransitionState(false) }
    expandedState.targetState = expanded
    if (expandedState.currentState || expandedState.targetState) {
        val density = LocalDensity.current
        val marginPx = with(density) { GlassMenuMargin.toPx().roundToInt() }
        val style = rememberGlassMenuStyle()
        // 宽度上限 = 屏宽 − 两侧安全边距，再夹绝对上限（平板/分屏兜底）；
        // screenWidthDp 异常为 0 时兜回下限，防 coerceIn 反向区间崩溃。
        val menuMaxWidth = minOf(
            LocalConfiguration.current.screenWidthDp.dp - GlassMenuScreenMargin * 2,
            GlassMenuAbsoluteMaxWidth,
        ).coerceAtLeast(GlassMenuMinWidth)
        // 实测内容宽：主/副行按渲染同款样式量单行自然宽取大者，加
        // leadingIcon/Check/行水平 padding（见 measuredWidthDp）；
        // 清单刷新 → items 变化触发 recomposition → 宽度天然重算。
        val labelStyle = MaterialTheme.typography.bodyMedium.let {
            if (labelFontFamily != null) it.copy(fontFamily = labelFontFamily) else it
        }
        val textMeasurer = rememberTextMeasurer()
        val menuWidth = (
            items.maxOfOrNull {
                it.measuredWidthDp(
                    textMeasurer = textMeasurer,
                    labelStyle = labelStyle,
                    subtitleStyle = MaterialTheme.typography.labelSmall,
                    density = density,
                )
            } ?: 0.dp
            ).coerceIn(GlassMenuMinWidth, menuMaxWidth)
        Popup(
            onDismissRequest = onDismissRequest,
            properties = PopupProperties(
                // 默认 true（弹层菜单语义：抢焦点、BACK 关弹层）；composer
                // 双下拉传 false 保住键盘，取舍见上方 KDoc。
                focusable = focusable,
                dismissOnBackPress = true,
                dismissOnClickOutside = true,
            ),
            popupPositionProvider = AnchorFlipPositionProvider(marginPx, alignEnd),
        ) {
            // 进出场 = 纯绘制相位（scale + fade），套在玻璃 Box 外层：
            // Box 布局尺寸首帧即定型为最终内容尺寸，hazeBlur 的模糊区域
            // 一次成型——动画期只是对已模糊整层做绘制变换（免逐帧重配
            // RenderEffect）。原 expandVertically/shrinkVertically 直接
            // 驱动玻璃面布局尺寸逐帧伸缩：展开＝模糊区逐帧扩大（残余
            // 卡顿），收起＝缩至 0 时帧全丢（观感「没动画直接消失」），
            // 且 AnchorFlipPositionProvider 按内容尺寸定位会跟着逐帧
            // 重算（翻上分支收起时 y 跳动）。transformOrigin 取底部
            // 中心，等价原 expandFrom/shrinkTowards Bottom 的自底展开
            // 语言（向上翻开的分支原实现也无 origin 联动，保持等价）。
            AnimatedVisibility(
                visibleState = expandedState,
                enter = fadeIn(tween(160, easing = FastOutSlowInEasing)) +
                    scaleIn(
                        animationSpec = tween(160, easing = FastOutSlowInEasing),
                        initialScale = 0.9f,
                        transformOrigin = TransformOrigin(0.5f, 1f),
                    ),
                exit = fadeOut(tween(120, easing = FastOutSlowInEasing)) +
                    scaleOut(
                        animationSpec = tween(120, easing = FastOutSlowInEasing),
                        targetScale = 0.95f,
                        transformOrigin = TransformOrigin(0.5f, 1f),
                    ),
            ) {
                Box(
                    modifier = modifier
                        // 宽随内容：实测宽已在上文钉好（menuWidth）。上限之内
                        // 文字完整显示（最宽条目天然贴边），只有超过屏宽上限
                        // 的极端长文才 Ellipsis；各行 fillMaxWidth → 等宽对齐。
                        .width(menuWidth)
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
                    Column(
                        Modifier
                            .heightIn(max = maxHeight)
                            .verticalScroll(rememberScrollState()),
                    ) {
                        for (item in items) {
                            GlassDropdownMenuItem(
                                item = item,
                                labelFontFamily = labelFontFamily,
                                onClick = {
                                    onSelect(item)
                                    onDismissRequest()
                                },
                            )
                        }
                    }
                }
            }
        }
    }
}

/**
 * 条目单行自然宽（dp）：主/副行按渲染同款样式 TextMeasurer 实测取大者
 * （px→dp），加行内固定件——leadingIcon（18 + 间距 10）、选中 Check
 * （间距 10 + 18；勾前 Spacer 与此处同款常量，量画同源，改一处必改另一
 * 处）、行水平 padding（14×2）。softWrap=false + maxLines=1 + 无约束宽 →
 * 不换行单行布局，宽 = 该文本完整单行渲染宽（样式/字体与条目行实际渲染
 * 一致）。
 */
private fun GlassDropdownItem.measuredWidthDp(
    textMeasurer: TextMeasurer,
    labelStyle: TextStyle,
    subtitleStyle: TextStyle,
    density: Density,
): Dp {
    val textWidthPx = maxOf(
        textMeasurer.measure(
            AnnotatedString(label),
            style = labelStyle,
            softWrap = false,
            maxLines = 1,
        ).size.width,
        subtitle?.let {
            textMeasurer.measure(
                AnnotatedString(it),
                style = subtitleStyle,
                softWrap = false,
                maxLines = 1,
            ).size.width
        } ?: 0,
    )
    var width = with(density) { textWidthPx.toDp() } + GlassItemPaddingHorizontal * 2
    if (leadingIcon != null) width += GlassItemIconSize + GlassItemIconGap
    if (selected) width += GlassItemIconGap + GlassItemIconSize
    return width
}

/** 弹层条目行：主文字（+可选副行）居左，选中带 Check 图标（ui.md 基线）。
 *  [labelFontFamily] 由菜单级注入（null = 默认字体），仅作用于主文字。 */
@Composable
private fun GlassDropdownMenuItem(
    item: GlassDropdownItem,
    labelFontFamily: FontFamily?,
    onClick: () -> Unit,
) {
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            // 面板宽已实测钉死（见 measuredWidthDp）：fillMaxWidth = 各行等宽
            // 对齐 + 等宽点击区，不参与宽度决定。
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(
                horizontal = GlassItemPaddingHorizontal,
                vertical = 10.dp,
            ),
    ) {
        item.leadingIcon?.let {
            Icon(
                it,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.onSurfaceVariant,
                modifier = Modifier.size(GlassItemIconSize),
            )
            Spacer(Modifier.width(GlassItemIconGap))
        }
        Column(Modifier.weight(1f)) {
            // 超宽主/副文字尾缘淡出（同 SoftPickerCapsule 值文本视觉，
            // 见 ui/TextFade.kt）：仅真溢出（如极端长文超屏幕上限）才挂。
            var labelOverflowed by remember { mutableStateOf(false) }
            Text(
                item.label,
                style = MaterialTheme.typography.bodyMedium,
                fontFamily = labelFontFamily,
                color = if (item.selected) {
                    MaterialTheme.colorScheme.primary
                } else {
                    MaterialTheme.colorScheme.onSurface
                },
                maxLines = 1,
                overflow = TextOverflow.Clip,
                onTextLayout = { labelOverflowed = it.didOverflowWidth },
                modifier = Modifier.fadeOutTrailingEdgeIf(labelOverflowed),
            )
            item.subtitle?.let {
                var subtitleOverflowed by remember { mutableStateOf(false) }
                Text(
                    it,
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.outline,
                    maxLines = 1,
                    overflow = TextOverflow.Clip,
                    onTextLayout = { subtitleOverflowed = it.didOverflowWidth },
                    modifier = Modifier.fadeOutTrailingEdgeIf(subtitleOverflowed),
                )
            }
        }
        if (item.selected) {
            // 勾与文字间的固定间距（与左侧图标同族 10dp 节奏）：文字列
            // weight(1f) 会吸掉全部富余宽，不设这个 Spacer，最宽选中条目
            // 的文字会贴死勾位（2026-10 真机反馈）。
            Spacer(Modifier.width(GlassItemIconGap))
            Icon(
                Icons.Filled.Check,
                contentDescription = null,
                tint = MaterialTheme.colorScheme.primary,
                modifier = Modifier.size(GlassItemIconSize),
            )
        }
    }
}
