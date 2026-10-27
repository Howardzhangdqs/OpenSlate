package dev.openslate.mobile.ui

import android.content.Context

/**
 * 工具详情显示偏好（设置 → 工具 → 内置工具；SharedPreferences 持久化）。
 * 照 [ThemePrefs] 模式：无 StateFlow / 重组通知，消费点（工具详情弹窗）
 * 打开时读一次作为初始值，弹窗内圆按钮本地切换；设置页开关乐观写。
 */
object ToolDisplayPrefs {
    private const val FILE = "appearance"
    private const val KEY_JSON_AUTO_FORMAT = "tool_json_auto_format"

    /** JSON 自动格式化默认开：多数工具参数是紧凑 JSON，展开+高亮更可读。 */
    fun readJsonAutoFormat(ctx: Context): Boolean =
        runCatching {
            ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
                .getBoolean(KEY_JSON_AUTO_FORMAT, true)
        }.getOrDefault(true)

    fun writeJsonAutoFormat(ctx: Context, value: Boolean) {
        ctx.getSharedPreferences(FILE, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(KEY_JSON_AUTO_FORMAT, value)
            .apply()
    }
}
