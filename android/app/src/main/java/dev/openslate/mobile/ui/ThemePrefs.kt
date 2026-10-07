package dev.openslate.mobile.ui

import android.content.Context
import androidx.compose.runtime.staticCompositionLocalOf

/** 主题模式（设置 → 外观）；SharedPreferences 持久化，启动时全局读取。 */
enum class ThemeMode { SYSTEM, LIGHT, DARK }

/** 当前生效的"深色"判定（含手动覆盖）。绕过 MaterialTheme 配色的组件
 *  （如状态色 StatusColors）用它对齐外观设置，不直读 isSystemInDarkTheme()。
 *  由 [dev.openslate.mobile.MainActivity] 的 OpenSlateTheme 提供。 */
val LocalDarkTheme = staticCompositionLocalOf { false }

object ThemePrefs {
    private const val FILE = "appearance"
    private const val KEY = "theme_mode"

    fun read(ctx: Context): ThemeMode =
        runCatching {
            when (ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).getString(KEY, null)) {
                "light" -> ThemeMode.LIGHT
                "dark" -> ThemeMode.DARK
                else -> ThemeMode.SYSTEM
            }
        }.getOrDefault(ThemeMode.SYSTEM)

    fun write(ctx: Context, mode: ThemeMode) {
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
            .edit()
            .putString(KEY, mode.name.lowercase())
            .apply()
    }
}
