package dev.openslate.mobile.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp

/**
 * 软色底紧凑文字按钮（全项目统一动作按钮样式）。
 *
 * 几何与工具页 [CompactButton] 同款：10dp 圆角矩形、水平 14dp /
 * 竖向 4dp 内边距、34dp 最小高度；文字略大一号（labelLarge 14sp，
 * CompactButton 为 labelMedium 12sp）。
 *
 * 不基于 TextButton 自绘（Row + clickable）：TextButton 内部的
 * 最小交互尺寸强制会撑高按钮，导致尺寸不可控（实测改 padding/
 * 高度视觉上无差异的根因）。背景 = 语义色 12% 透明度（默认主题
 * 色），文字颜色 = 语义色（与原 TextButton 默认 primary 一致；
 * 调用方显式指定 color 的 Text 不受影响）。
 *
 * tint：语义色（背景 12% 透明 + 文字同色）；破坏性按钮可传
 * `MaterialTheme.colorScheme.error`。
 */
@Composable
fun SoftButton(
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    tint: Color = MaterialTheme.colorScheme.primary,
    content: @Composable RowScope.() -> Unit,
) {
    val bgAlpha = if (enabled) 0.12f else 0.05f
    val contentColor = if (enabled) tint else tint.copy(alpha = 0.45f)
    Row(
        modifier = modifier
            .heightIn(min = 34.dp)
            .clip(RoundedCornerShape(10.dp))
            .background(tint.copy(alpha = bgAlpha))
            .clickable(enabled = enabled, onClick = onClick)
            .padding(paddingValues = PaddingValues(horizontal = 14.dp, vertical = 4.dp)),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        CompositionLocalProvider(
            LocalContentColor provides contentColor,
            LocalTextStyle provides MaterialTheme.typography.labelLarge,
        ) {
            content()
        }
    }
}
