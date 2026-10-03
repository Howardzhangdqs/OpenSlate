package dev.openslate.mobile.bridge

/**
 * 应用 SharedPreferences 文件名统一入口。
 *
 * 此前 "openslate_prefs" 字面量散落在 RuntimeBridge / McpHostManager 多处，
 * 易拼写漂移；集中到这里管理（密钥密文另有独立文件，见 [SecretStore]）。
 */
internal object AppPrefs {
    /** 主 prefs 文件：运行时杂项（exec 后端 / HTTP 代理 / MCP server 清单）。 */
    const val FILE_MAIN = "openslate_prefs"
}
