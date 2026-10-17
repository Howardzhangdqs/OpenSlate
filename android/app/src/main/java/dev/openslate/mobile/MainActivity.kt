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
import androidx.navigation.NavType
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.rememberNavController
import androidx.navigation.navArgument
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
import dev.openslate.mobile.ui.ModelRegistrySourceScreen
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

/** 页面导航栈：chat（起点）/ settings / history / registry-source/{sourceId}（模型数据源三级页），回退键沿栈逐级返回。 */
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
                onOpenSource = { id ->
                    nav.navigate("registry-source/$id") { launchSingleTop = true }
                },
                themeMode = themeMode,
                onThemeModeChange = onThemeModeChange,
            )
        }
        // 模型数据源独占整屏（三级）：系统返回键/手势与顶栏返回都走
        // popBackStack；转场同其他二级页家族（右滑入/原路滑出）。
        composable(
            "registry-source/{sourceId}",
            arguments = listOf(navArgument("sourceId") { type = NavType.StringType }),
            enterTransition = { slideInHorizontally(tween(280)) { it } + fadeIn(tween(280)) },
            exitTransition = { slideOutHorizontally(tween(280)) { -it / 4 } + fadeOut(tween(280)) },
            popEnterTransition = { slideInHorizontally(tween(280)) { -it / 4 } + fadeIn(tween(280)) },
            popExitTransition = { slideOutHorizontally(tween(280)) { it } + fadeOut(tween(280)) },
        ) { backStackEntry ->
            ModelRegistrySourceScreen(
                sourceId = backStackEntry.arguments?.getString("sourceId").orEmpty(),
                onBack = { nav.popBackStack() },
            )
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
            SettingsScreen(onBack = {}, onOpenKeys = {})
            KeysScreen(onBack = {})
            HistoryScreen(onBack = {}, onOpened = {})
        }
    }
}

// Cyan 主题配色(Material 3 tonal palette,种子色 cyan #00BCD4)。
// surface 全家必须整套给齐：material3 1.4 中默认 Card 容器取
// colorScheme.surfaceContainerHighest、AppDialog 取 surfaceContainerHigh，
// 未显式提供的角色会回落 lightColorScheme()/darkColorScheme() 的 Material
// 基线紫种子中性色（浅 Highest=0xFFE6E0E9、深=0xFF36343B），在冷蓝
// surface 底上显淡红/紫灰——container 阶梯一律从本套 surface/surfaceVariant
// 同轨道插值，禁止紫基线值混入。
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
    // ── surface container 阶梯（由 surface F6FAFE → surfaceVariant DCE3E9
    // 冷蓝灰轨道等距插值；每步 R−7/G−6/B−6，与既有锚点同偏量）──
    // Lowest=纯白（对齐基线 Lowest=Neutral100 语义）；
    // Highest=variant 终点（material3 1.4 默认 Card 容器色，取代紫基线
    // E6E0E9）；High/Container/AppDialog 用的 surfaceContainerHigh 依次插值。
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFF0F4FA),
    surfaceContainer = Color(0xFFE9EEF4),
    surfaceContainerHigh = Color(0xFFE2E8EE),
    surfaceContainerHighest = Color(0xFFDCE3E9),
    // dim/bright：基线浅色语义为 dim=Neutral87（比 variant N90 深一档）、
    // bright=Neutral98（= surface）→ dim 在 variant 下再深一阶、bright=surface。
    surfaceDim = Color(0xFFD7DFE6),
    surfaceBright = Color(0xFFF6FAFE),
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
    // ── surface container 阶梯（以 surface 0F1417 为底向 surfaceVariant
    // 41484D 方向递进；每步 R+5~6/G+6/B+6~7，与既有锚点同偏量）──
    // Lowest 比 surface 再深一档（对齐基线 Lowest=Neutral4 在 surface
    // N6 之下）；Highest=2A3138（material3 1.4 默认 Card 容器色，取代紫
    // 基线 36343B）；Low→Highest 由 1A2025 族等距递进。
    surfaceContainerLowest = Color(0xFF0A0F12),
    surfaceContainerLow = Color(0xFF1A2025),
    surfaceContainer = Color(0xFF1F252B),
    surfaceContainerHigh = Color(0xFF252C32),
    surfaceContainerHighest = Color(0xFF2A3138),
    // dim/bright：基线深色语义为 dim=Neutral6（= surface）、bright=Neutral24
    // （比 Highest N22 亮一阶）→ dim=surface、bright 在 Highest 上再加一档。
    surfaceDim = Color(0xFF0F1417),
    surfaceBright = Color(0xFF2F363E),
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
