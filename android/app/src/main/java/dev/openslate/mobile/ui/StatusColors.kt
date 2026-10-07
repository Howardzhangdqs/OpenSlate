package dev.openslate.mobile.ui

import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color

/**
 * 成功/就绪态的语义绿（跟随系统深浅色模式）。
 *
 * 此前各处写死浅色模式配色（前景 0xFF1E8A44 / 底色 0xFFDDF3DE 一类），
 * 深色模式下刺眼且不随主题变化；统一收敛到这里，深浅各一套。
 */

/** 成功态前景（图标/文字）。 */
@Composable
fun successGreen(): Color =
    if (LocalDarkTheme.current) Color(0xFF8FD6A4) else Color(0xFF1E8A44)

/** 成功态底色（chip / 按钮容器）。 */
@Composable
fun successGreenContainer(): Color =
    if (LocalDarkTheme.current) Color(0xFF1F3A29) else Color(0xFFDDF3DE)
