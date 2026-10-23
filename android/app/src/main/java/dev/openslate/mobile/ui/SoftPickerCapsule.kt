package dev.openslate.mobile.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.KeyboardArrowDown
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
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.ui.semantics.clearAndSetSemantics
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.chrisbanes.haze.HazeState

/**
 * 软底选择器胶囊（下拉控件的最小共用内核）：设置页与输入面板四处下拉
 * 控件（SettingsScreen [ModelEffortPickerRow] 模型胶囊 / ThinkingEffortRow、
 * ComposerPanel ModelPickerRow / EffortPickerRow）共用的「胶囊本体」——
 * SoftButton 同族软底（primary 12% 透明底 + 10dp 圆角 + 34dp 最小触达高度
 * + 12/4dp 内边距）+ 左当前值文本（weight 吃满剩余宽、超长尾缘淡出
 *   ——渐变掩膜替代省略号，见 ui/TextFade.kt）+
 * 右 KeyboardArrowDown（展开 180° 旋转，graphicsLayer 绘制相位读取——旋转
 * 不逐帧重组整行）+ 本体 Box 锚弹 [GlassDropdownMenu]（alignEnd 右对齐，
 * 四处消费点均右对齐故不参数化）。
 *
 * 契约（内核只管胶囊渲染，不含任何状态与写入逻辑）：
 * - [expanded] 开合状态归宿主持有：胶囊点击走 [onExpand]、弹层 dismiss 走
 *   [onDismissRequest]，两回调间宿主差异逻辑（输入面板的 dismiss 后同手势
 *   重开抑制窗口、设置页的纯开关）全留在宿主，内核零感知；
 * - [value] = 当前值的展示富文本（[AnnotatedString]——模型名内嵌 MonoFont
 *   子集之类的构建逻辑留在宿主），内核原样上屏；
 * - 语义二选一（读屏口径，两宿主原本就不同）：
 *   [controlContentDescription] 非 null = 控件级语义——描述挂在胶囊本体、
 *   值文本清空自身语义、箭头不播报（输入面板双控件同款）；为 null = 值
 *   文本保留自身语义、[arrowContentDescription]（随展开态换措辞）播报在
 *   箭头上（设置页两控件同款）；
 * - [valueModifier] 注入值文本的**前置**修饰（内核随后追加 `weight(1f)`，
 *   控件级语义模式下再追加 clearAndSetSemantics——与替换前的修饰链同序）：
 *   输入面板的 sharedBounds 位移动画锚点经此挂进胶囊内的值 Text，内核不
 *   感知共享转场。
 *
 * 宿主差异面（内核不管，留在宿主）：
 * - 输入面板（ComposerPanel）：sharedBounds 位移动画（经 [valueModifier]
 *   注入）、面板专属双控件行布局、非聚焦弹层（[focusable] = false 保住
 *   键盘）、重开抑制与控件级语义；
 * - 设置页（SettingsScreen）：撑满行布局——模型/档位两组的 weight 分配
 *   （6:4）在宿主行内做，档位写盘（onSelect 回调里宿主自走 upsertModel）
 *   与 snackbar 通道都在宿主。
 *
 * 规格参数化仅两处（逐处保持替换前视觉）：
 * - [textStyle]：模型控件 labelLarge / 档位控件 labelMedium（两宿主的
 *   字号档位差是既有设计，不收口）；
 * - [disabledBackgroundAlpha]：禁用态底色——默认 12%（设置页模型控件
 *   禁用时底色不降），输入面板模型控件照 SoftButton 禁用态降到 5%。
 */
@Composable
internal fun SoftPickerCapsule(
    value: AnnotatedString,
    valueColor: Color,
    textStyle: TextStyle,
    expanded: Boolean,
    onExpand: () -> Unit,
    onDismissRequest: () -> Unit,
    hazeState: HazeState,
    items: List<GlassDropdownItem>,
    onSelect: (GlassDropdownItem) -> Unit,
    modifier: Modifier = Modifier,
    valueModifier: Modifier = Modifier,
    enabled: Boolean = true,
    disabledBackgroundAlpha: Float = 0.12f,
    controlContentDescription: String? = null,
    arrowContentDescription: String? = null,
    labelFontFamily: FontFamily? = null,
    focusable: Boolean = true,
) {
    val arrowAngle by animateFloatAsState(
        targetValue = if (expanded) 180f else 0f,
        label = "softPickerCapsuleArrow",
    )
    Box(modifier) {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            modifier = Modifier
                // 胶囊撑满宿主给定的 Box 宽（两宿主均为「外层定宽、胶囊
                // 吃满」），值文本再 weight 吃满胶囊宽——箭头钉右缘。
                .fillMaxWidth()
                .heightIn(min = 34.dp)
                .clip(RoundedCornerShape(10.dp))
                .background(
                    MaterialTheme.colorScheme.primary.copy(
                        alpha = if (enabled) 0.12f else disabledBackgroundAlpha,
                    ),
                )
                .clickable(enabled = enabled) { onExpand() }
                // 控件级语义模式：描述上移胶囊本体（值文本语义清空见下），
                // TalkBack 只播报这一句；否则不挂（值文本/箭头各自播报）。
                .then(
                    if (controlContentDescription != null) {
                        Modifier.semantics { contentDescription = controlContentDescription }
                    } else {
                        Modifier
                    },
                )
                .padding(horizontal = 12.dp, vertical = 4.dp),
        ) {
            // 值文本超宽处理：尾缘淡出替代省略号（2026-10 拍板）——
            // onTextLayout 检测溢出后才挂渐变掩膜（短文本不淡出），
            // overflow 用 Clip 保证掩膜作用在完整截断文本上（Ellipsis
            // 会先截成「…」再淡出，语义重复）。
            var valueOverflowed by remember { mutableStateOf(false) }
            Text(
                text = value,
                style = textStyle,
                color = valueColor,
                maxLines = 1,
                // softWrap 必须关：默认折行 + maxLines=1 会把超宽文本折到
                // 第二行整行裁掉（真机实测「模型名消失」），关掉才是水平
                // 截断——尾缘淡出掩膜的正确作用面。
                softWrap = false,
                overflow = TextOverflow.Clip,
                onTextLayout = { valueOverflowed = it.didOverflowWidth },
                modifier = valueModifier
                    .weight(1f)
                    .fadeOutTrailingEdgeIf(valueOverflowed)
                    .then(
                        if (controlContentDescription != null) {
                            // 控件级语义已含当前值，清掉裸文本语义避免
                            // TalkBack 把值读两遍。
                            Modifier.clearAndSetSemantics { }
                        } else {
                            Modifier
                        },
                    ),
            )
            Spacer(Modifier.width(4.dp))
            Icon(
                Icons.Filled.KeyboardArrowDown,
                contentDescription = if (controlContentDescription != null) {
                    // 描述语义已上移控件级，箭头自身不再播报。
                    null
                } else {
                    arrowContentDescription
                },
                tint = MaterialTheme.colorScheme.outline,
                modifier = Modifier.graphicsLayer { rotationZ = arrowAngle },
            )
        }
        GlassDropdownMenu(
            expanded = expanded,
            onDismissRequest = onDismissRequest,
            hazeState = hazeState,
            items = items,
            labelFontFamily = labelFontFamily,
            alignEnd = true,
            focusable = focusable,
            onSelect = onSelect,
        )
    }
}
