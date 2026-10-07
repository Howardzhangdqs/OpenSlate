package dev.openslate.mobile

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInHorizontally
import androidx.compose.animation.slideOutHorizontally
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.withFrameNanos
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import dev.openslate.mobile.service.AgentService
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch
import androidx.compose.runtime.CompositionLocalProvider
import dev.openslate.mobile.ui.LocalDarkTheme
import dev.openslate.mobile.ui.ThemeMode
import dev.openslate.mobile.ui.ThemePrefs
import dev.openslate.mobile.ui.ChatScreen
import dev.openslate.mobile.ui.HistoryScreen
import dev.openslate.mobile.ui.KeysScreen
import dev.openslate.mobile.ui.ModelRegistryScreen
import dev.openslate.mobile.ui.SettingsScreen

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        AgentService.start(this)
        // MCP 预热：配置了服务器时后台拉起 Termux / 恢复 host（冷启动提前消化）。
        lifecycleScope.launch(kotlinx.coroutines.Dispatchers.IO) {
            runCatching { dev.openslate.mobile.bridge.McpHostManager.prewarm(this@MainActivity) }
        }
        setContent {
            val ctx = applicationContext
            var themeMode by remember { mutableStateOf(ThemePrefs.read(ctx)) }
            OpenSlateTheme(mode = themeMode) {
                Surface {
                    AppNavHost(
                        themeMode = themeMode,
                        onThemeModeChange = { m ->
                            themeMode = m
                            ThemePrefs.write(ctx, m)
                        },
                    )
                }
            }
        }
    }
}

/** 页面导航栈：chat（起点）/ settings / history，回退键沿栈逐级返回。 */
@Composable
private fun AppNavHost(themeMode: ThemeMode, onThemeModeChange: (ThemeMode) -> Unit) {
    val nav = rememberNavController()
    WarmUpSecondaryScreens()
    NavHost(navController = nav, startDestination = "chat") {
        composable("chat") {
            ChatScreen(
                onOpenHistory = { nav.navigate("history") { launchSingleTop = true } },
                onOpenSettings = { nav.navigate("settings") { launchSingleTop = true } },
            )
        }
        // 二级页面：从右侧滑入、按返回时原路滑出（原生推入语义）。
        composable(
            "settings",
            enterTransition = { slideInHorizontally(tween(280)) { it } + fadeIn(tween(280)) },
            exitTransition = { slideOutHorizontally(tween(280)) { -it / 4 } + fadeOut(tween(280)) },
            popEnterTransition = { slideInHorizontally(tween(280)) { -it / 4 } + fadeIn(tween(280)) },
            popExitTransition = { slideOutHorizontally(tween(280)) { it } + fadeOut(tween(280)) },
        ) {
            SettingsScreen(
                onBack = { nav.popBackStack() },
                onOpenKeys = { nav.navigate("keys") { launchSingleTop = true } },
                onOpenRegistry = { nav.navigate("registry") { launchSingleTop = true } },
                themeMode = themeMode,
                onThemeModeChange = onThemeModeChange,
            )
        }
        composable(
            "registry",
            enterTransition = { slideInHorizontally(tween(280)) { it } + fadeIn(tween(280)) },
            exitTransition = { slideOutHorizontally(tween(280)) { -it / 4 } + fadeOut(tween(280)) },
            popEnterTransition = { slideInHorizontally(tween(280)) { -it / 4 } + fadeIn(tween(280)) },
            popExitTransition = { slideOutHorizontally(tween(280)) { it } + fadeOut(tween(280)) },
        ) {
            ModelRegistryScreen(onBack = { nav.popBackStack() })
        }
        composable(
            "keys",
            enterTransition = { slideInHorizontally(tween(280)) { it } + fadeIn(tween(280)) },
            exitTransition = { slideOutHorizontally(tween(280)) { -it / 4 } + fadeOut(tween(280)) },
            popEnterTransition = { slideInHorizontally(tween(280)) { -it / 4 } + fadeIn(tween(280)) },
            popExitTransition = { slideOutHorizontally(tween(280)) { it } + fadeOut(tween(280)) },
        ) {
            KeysScreen(onBack = { nav.popBackStack() })
        }
        composable(
            "history",
            enterTransition = { slideInHorizontally(tween(280)) { it } + fadeIn(tween(280)) },
            exitTransition = { slideOutHorizontally(tween(280)) { -it / 4 } + fadeOut(tween(280)) },
            popEnterTransition = { slideInHorizontally(tween(280)) { -it / 4 } + fadeIn(tween(280)) },
            popExitTransition = { slideOutHorizontally(tween(280)) { it } + fadeOut(tween(280)) },
        ) {
            HistoryScreen(
                onBack = { nav.popBackStack() },
                onOpened = { nav.popBackStack() },
            )
        }
    }
}

/**
 * 二级页预热：启动完成（过两帧、避开启动高峰）后，把设置/历史页在 0 尺寸容器里
 * 组合一帧再卸载。debug 包无预编译优化，首次导航需现场加载/校验类并首建整棵
 * UI 树（用户感知为"第一次进设置卡一下"）；预热把这笔成本提前到启动期支付。
 */
@Composable
private fun WarmUpSecondaryScreens() {
    var warming by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        withFrameNanos {} // 等主画面首帧
        withFrameNanos {} // 再等一帧，错开启动渲染高峰
        warming = true
        withFrameNanos {} // 预热页恰好存活一帧（组合阶段同步完成类加载）
        warming = false
    }
    if (warming) {
        Box(Modifier.size(0.dp)) {
            SettingsScreen(onBack = {}, onOpenKeys = {}, onOpenRegistry = {})
            KeysScreen(onBack = {})
            HistoryScreen(onBack = {}, onOpened = {})
        }
    }
}

// Cyan 主题配色(Material 3 tonal palette,种子色 cyan #00BCD4)
private val LightCyanColors = lightColorScheme(
    primary = Color(0xFF006689),
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = Color(0xFFC1E8FF),
    onPrimaryContainer = Color(0xFF001E2C),
    secondary = Color(0xFF4E616D),
    onSecondary = Color(0xFFFFFFFF),
    secondaryContainer = Color(0xFFD1E5F4),
    onSecondaryContainer = Color(0xFF0A1E29),
    tertiary = Color(0xFF006A60),
    onTertiary = Color(0xFFFFFFFF),
    tertiaryContainer = Color(0xFF9CF1E4),
    onTertiaryContainer = Color(0xFF00201C),
    error = Color(0xFFBA1A1A),
    onError = Color(0xFFFFFFFF),
    errorContainer = Color(0xFFFFDAD6),
    onErrorContainer = Color(0xFF410002),
    background = Color(0xFFF6FAFE),
    onBackground = Color(0xFF171C1F),
    surface = Color(0xFFF6FAFE),
    onSurface = Color(0xFF171C1F),
    surfaceVariant = Color(0xFFDCE3E9),
    onSurfaceVariant = Color(0xFF41484D),
    outline = Color(0xFF71787E),
    inverseSurface = Color(0xFF2B3135),
    inverseOnSurface = Color(0xFFEDF1F5),
    inversePrimary = Color(0xFF7AD0F7),
)

private val DarkCyanColors = darkColorScheme(
    primary = Color(0xFF7AD0F7),
    onPrimary = Color(0xFF00344C),
    primaryContainer = Color(0xFF004B6C),
    onPrimaryContainer = Color(0xFFC1E8FF),
    secondary = Color(0xFFB5C9D7),
    onSecondary = Color(0xFF20333E),
    secondaryContainer = Color(0xFF374955),
    onSecondaryContainer = Color(0xFFD1E5F4),
    tertiary = Color(0xFF80D5C8),
    onTertiary = Color(0xFF003731),
    tertiaryContainer = Color(0xFF005048),
    onTertiaryContainer = Color(0xFF9CF1E4),
    error = Color(0xFFFFB4AB),
    onError = Color(0xFF690005),
    errorContainer = Color(0xFF93000A),
    onErrorContainer = Color(0xFFFFDAD6),
    background = Color(0xFF0F1417),
    onBackground = Color(0xFFDEE3E7),
    surface = Color(0xFF0F1417),
    onSurface = Color(0xFFDEE3E7),
    surfaceVariant = Color(0xFF41484D),
    onSurfaceVariant = Color(0xFFC1C7CD),
    outline = Color(0xFF8B9297),
    inverseSurface = Color(0xFFDEE3E7),
    inverseOnSurface = Color(0xFF2B3135),
    inversePrimary = Color(0xFF006689),
)

@Composable
fun OpenSlateTheme(mode: ThemeMode = ThemeMode.SYSTEM, content: @Composable () -> Unit) {
    val dark = when (mode) {
        ThemeMode.SYSTEM -> isSystemInDarkTheme()
        ThemeMode.LIGHT -> false
        ThemeMode.DARK -> true
    }
    CompositionLocalProvider(LocalDarkTheme provides dark) {
        MaterialTheme(
            colorScheme = if (dark) DarkCyanColors else LightCyanColors,
            content = content,
        )
    }
}
