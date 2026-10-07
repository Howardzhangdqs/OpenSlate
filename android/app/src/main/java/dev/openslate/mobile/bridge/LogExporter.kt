package dev.openslate.mobile.bridge

import android.content.Context
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

/**
 * 导出本应用自身的 logcat。无需 READ_LOGS 权限：logd 只放行自身 UID 的
 * 日志；跨进程重启的历史属于同一 UID，仍在环形缓冲区内的同样能读到
 * （"一天"档受缓冲区容量限制，只能尽力而为）。
 */
object LogExporter {

    enum class Range { SINCE_START, LAST_HOUR, LAST_DAY }

    /** 复制上限：超出则丢弃更早的部分，只保留最近的（clipboard 体量可控）。 */
    private const val MAX_BYTES = 400 * 1024

    /** 读 logcat 并拼装带设备 / 版本头的文本（IO 线程执行，返回即完整内容）。 */
    suspend fun collect(ctx: Context, range: Range): String =
        withContext(Dispatchers.IO) {
            val appCtx = ctx.applicationContext
            val pid = android.os.Process.myPid()
            val uid = android.os.Process.myUid()
            val timeFmt = SimpleDateFormat("MM-dd HH:mm:ss.SSS", Locale.US)
            val now = System.currentTimeMillis()

            val cmd = mutableListOf("logcat", "-d")
            val desc = when (range) {
                Range.SINCE_START -> {
                    cmd += "--pid=$pid"
                    "本次启动以来（PID $pid）"
                }
                Range.LAST_HOUR -> {
                    cmd += listOf("--uid=$uid", "-T", timeFmt.format(Date(now - 3_600_000L)))
                    "最近 1 小时"
                }
                Range.LAST_DAY -> {
                    cmd += listOf("--uid=$uid", "-T", timeFmt.format(Date(now - 86_400_000L)))
                    "最近 24 小时（受缓冲区容量限制）"
                }
            }

            val raw = runCatching {
                val proc = ProcessBuilder(cmd).start()
                val out = proc.inputStream.readBytes().toString(Charsets.UTF_8)
                runCatching { proc.waitFor() }
                out
            }.getOrElse { "（读取失败：${it.message}）" }

            val truncated = raw.length > MAX_BYTES
            val body = if (truncated) raw.takeLast(MAX_BYTES) else raw
            val ver = runCatching {
                @Suppress("DEPRECATION")
                appCtx.packageManager.getPackageInfo(appCtx.packageName, 0).versionName
            }.getOrNull() ?: "?"
            val head = buildString {
                appendLine("OpenSlate 日志｜$desc")
                appendLine("导出时间：${SimpleDateFormat("yyyy-MM-dd HH:mm:ss", Locale.US).format(Date())}")
                appendLine("版本：$ver｜设备：${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}｜Android ${android.os.Build.VERSION.RELEASE} (API ${android.os.Build.VERSION.SDK_INT})")
                appendLine("行数：${body.lines().size}｜大小：${body.length / 1024}KB${if (truncated) "（已截断，保留最近部分）" else ""}")
                appendLine("说明：仅本应用（UID $uid）日志，不含系统与其他应用")
                appendLine("────────────────────────────────────────")
            }
            head + body
        }
}
