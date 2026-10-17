package dev.openslate.mobile.ui

import android.content.Context
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color

/** 主题模式（设置 → 外观）；SharedPreferences 持久化，启动时全局读取。 */
enum class ThemeMode { SYSTEM, LIGHT, DARK }

/** 当前生效的"深色"判定（含手动覆盖）。绕过 MaterialTheme 配色的组件
 *  （如状态色 StatusColors）用它对齐外观设置，不直读 isSystemInDarkTheme()。
 *  由 [dev.openslate.mobile.MainActivity] 的 OpenSlateTheme 提供。 */
val LocalDarkTheme = staticCompositionLocalOf { false }

object ThemePrefs {
    private const val FILE = "appearance"
    private const val KEY = "theme_mode"
    private const val KEY_ACCENT = "accent_color"

    /** 主题色默认 id（现有 cyan 配色，老用户零视觉变化）。 */
    const val DEFAULT_ACCENT = "cyan"

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

    /** 主题色 id 存取（与模式同一 preferences 文件；未知 id 由
     *  [accentOf] 回落 cyan，这里只管存取不校验）。 */
    fun readAccent(ctx: Context): String =
        runCatching {
            ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE).getString(KEY_ACCENT, null)
        }.getOrNull() ?: DEFAULT_ACCENT

    fun writeAccent(ctx: Context, id: String) {
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
            .edit()
            .putString(KEY_ACCENT, id)
            .apply()
    }
}

/** 强调色单侧（浅或深）的 primary 四角色。按 M3 tonal 结构手写：
 *  浅侧 primary=40 阶 / container=90 阶 / onContainer=10 阶，深侧
 *  primary=80 阶 / onPrimary=20 阶 / container=30 阶 / onContainer=90 阶。 */
data class AccentSide(
    val primary: Color,
    val onPrimary: Color,
    val primaryContainer: Color,
    val onPrimaryContainer: Color,
)

/** 一个主题色 = id（持久化键）+ 中文标签（无障碍）+ 浅/深两侧强调色族。 */
data class AccentPalette(
    val id: String,
    val label: String,
    val light: AccentSide,
    val dark: AccentSide,
)

/**
 * 主题色集合（设置 → 外观「主题色」圆形选择器；覆盖色相环主要区间，
 * 共 7 色）。全部手写、零调色库；首项 cyan 是应用既有配色，值与历史
 * scheme 一字不差（老用户零视觉变化）。
 *
 * 取舍：只有 primary 族随 accent 走；中性面（surface 全家）与
 * secondary/tertiary/error 族是全色共享的冷中性/低彩度辅助系（见
 * MainActivity 的 scheme 工厂注释）——辅助色大量用于 ChatScreen 消息
 * 分组等语义场景，保持稳定比跟随强调色更协调，也避免 7 色手写 100+
 * 低质量值。
 */
val ACCENTS: List<AccentPalette> = listOf(
    AccentPalette(
        id = "cyan",
        label = "青色",
        // 应用默认配色（种子 #00BCD4，历史原值）。
        light = AccentSide(
            primary = Color(0xFF006689),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFFC1E8FF),
            onPrimaryContainer = Color(0xFF001E2C),
        ),
        dark = AccentSide(
            primary = Color(0xFF7AD0F7),
            onPrimary = Color(0xFF00344C),
            primaryContainer = Color(0xFF004B6C),
            onPrimaryContainer = Color(0xFFC1E8FF),
        ),
    ),
    AccentPalette(
        id = "blue",
        label = "蓝色",
        light = AccentSide(
            primary = Color(0xFF0061A4),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFFD1E4FF),
            onPrimaryContainer = Color(0xFF001D36),
        ),
        dark = AccentSide(
            primary = Color(0xFF9ECAFF),
            onPrimary = Color(0xFF003258),
            primaryContainer = Color(0xFF00497D),
            onPrimaryContainer = Color(0xFFD1E4FF),
        ),
    ),
    AccentPalette(
        id = "green",
        label = "绿色",
        light = AccentSide(
            primary = Color(0xFF006E1C),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFF9BF6A0),
            onPrimaryContainer = Color(0xFF002204),
        ),
        dark = AccentSide(
            primary = Color(0xFF7AD96F),
            onPrimary = Color(0xFF00390F),
            primaryContainer = Color(0xFF005316),
            onPrimaryContainer = Color(0xFF9BF6A0),
        ),
    ),
    AccentPalette(
        id = "amber",
        label = "琥珀",
        light = AccentSide(
            primary = Color(0xFF745B00),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFFFFE08D),
            onPrimaryContainer = Color(0xFF241B00),
        ),
        dark = AccentSide(
            primary = Color(0xFFE6C449),
            onPrimary = Color(0xFF3F2E00),
            primaryContainer = Color(0xFF5B4300),
            onPrimaryContainer = Color(0xFFFFE08D),
        ),
    ),
    AccentPalette(
        id = "orange",
        label = "橙色",
        light = AccentSide(
            primary = Color(0xFF8B5000),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFFFFDBC8),
            onPrimaryContainer = Color(0xFF2E1500),
        ),
        dark = AccentSide(
            primary = Color(0xFFFFB77C),
            onPrimary = Color(0xFF4E2600),
            primaryContainer = Color(0xFF6F3A00),
            onPrimaryContainer = Color(0xFFFFDBC8),
        ),
    ),
    AccentPalette(
        id = "purple",
        label = "紫色",
        light = AccentSide(
            primary = Color(0xFF6750A4),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFFEADDFF),
            onPrimaryContainer = Color(0xFF21005D),
        ),
        dark = AccentSide(
            primary = Color(0xFFD0BCFF),
            onPrimary = Color(0xFF381E72),
            primaryContainer = Color(0xFF4F378B),
            onPrimaryContainer = Color(0xFFEADDFF),
        ),
    ),
    AccentPalette(
        id = "rose",
        label = "玫红",
        light = AccentSide(
            primary = Color(0xFF8E4957),
            onPrimary = Color(0xFFFFFFFF),
            primaryContainer = Color(0xFFFFD9DE),
            onPrimaryContainer = Color(0xFF3A0C16),
        ),
        dark = AccentSide(
            primary = Color(0xFFFFB1C8),
            onPrimary = Color(0xFF511324),
            primaryContainer = Color(0xFF6D2F3C),
            onPrimaryContainer = Color(0xFFFFD9DE),
        ),
    ),
)

/** id → 调色板（存储值被删改 / 版本增删色后的兜底：未知 id 回落 cyan）。 */
fun accentOf(id: String): AccentPalette =
    ACCENTS.firstOrNull { it.id == id } ?: ACCENTS.first()
