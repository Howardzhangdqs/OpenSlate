package dev.openslate.mobile.bridge

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Bundle
import android.util.Log
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.ConcurrentHashMap

/**
 * Termux 执行器（独立运行模式：无 PC、无 adb、无局域网中继）。
 *
 * 链路（Termux >= 0.109 官方插件 API）：
 *   RUN_COMMAND intent（background runner）
 *     + com.termux.RUN_COMMAND_PENDING_INTENT（指向本 App 的 TermuxResultReceiver）
 *   → Termux RunCommandService 执行
 *   → 结果 intent 回送：extra "result" = Bundle{stdout, stderr, exitCode, err, errmsg}
 *   → Receiver 回填 CompletableDeferred → resolveHostCall
 *
 * 前置条件（一次性，用户操作，无需 PC）：
 *   1. RUN_COMMAND 是 dangerous runtime permission：系统设置 → 应用 →
 *      OpenSlate → 权限 里授予，或经 Shizuku 一键 pm grant（见 ShizukuGrant）。
 *   2. Termux ~/.termux/termux.properties 中 allow-external-apps=true。
 */
object TermuxExec {

    private const val TAG = "OpenSlateTermux"

    const val TERMUX_PACKAGE = "com.termux"
    const val RUN_COMMAND_PERMISSION = "com.termux.permission.RUN_COMMAND"

    private const val RUN_COMMAND_ACTION = "com.termux.RUN_COMMAND"
    private const val RUN_COMMAND_SERVICE = "com.termux.app.RunCommandService"
    private const val EXTRA_PATH = "com.termux.RUN_COMMAND_PATH"
    private const val EXTRA_ARGUMENTS = "com.termux.RUN_COMMAND_ARGUMENTS"
    private const val EXTRA_WORKDIR = "com.termux.RUN_COMMAND_WORKDIR"
    private const val EXTRA_BACKGROUND = "com.termux.RUN_COMMAND_BACKGROUND"
    private const val EXTRA_PENDING_INTENT = "com.termux.RUN_COMMAND_PENDING_INTENT"

    /** host call id → 输出回传通道（TermuxResultReceiver 回填）。 */
    private val pending = ConcurrentHashMap<Long, CompletableDeferred<TermuxResult>>()

    /** 结果等待上限（host call 路由自身还有 120s 兜底）。 */
    private const val RESULT_TIMEOUT_MS = 90_000L

    fun hasPermission(ctx: Context): Boolean =
        ctx.checkSelfPermission(RUN_COMMAND_PERMISSION) == PackageManager.PERMISSION_GRANTED

    /** Termux 是否已安装（Android 11+ 需要 manifest queries 声明）。 */
    fun isTermuxInstalled(ctx: Context): Boolean = runCatching {
        ctx.packageManager.getPackageInfo(TERMUX_PACKAGE, 0)
        true
    }.getOrDefault(false)

    /**
     * 在 Termux 中执行命令并等待输出（挂起）。失败返回 null + 错误说明由
     * 调用方组装；永不抛出。
     */
    suspend fun run(context: Context, id: Long, command: String): Pair<String?, String> {
        if (command.isBlank()) return null to "termux.run: empty command"
        if (!hasPermission(context)) {
            return null to
                "termux.run: missing permission $RUN_COMMAND_PERMISSION. " +
                "Grant it in system Settings → Apps → OpenSlate → Permissions, " +
                "or via Shizuku in the app's settings page."
        }
        val deferred = CompletableDeferred<TermuxResult>()
        pending[id] = deferred
        try {
            // 带 id 的回传 intent（Termux send() 的 fill-in 不会覆盖我们的 extra key）。
            val resultIntent = Intent(context, TermuxResultReceiver::class.java)
                .putExtra("openslate_id", id)
            val pi = android.app.PendingIntent.getBroadcast(
                context,
                (id and 0x7FFFFFFF).toInt(),
                resultIntent,
                android.app.PendingIntent.FLAG_MUTABLE,
            )
            val intent = Intent(RUN_COMMAND_ACTION)
                .setClassName(TERMUX_PACKAGE, RUN_COMMAND_SERVICE)
                .putExtra(EXTRA_PATH, "/data/data/com.termux/files/usr/bin/bash")
                .putExtra(EXTRA_ARGUMENTS, arrayOf("-c", command))
                .putExtra(EXTRA_WORKDIR, "/data/data/com.termux/files/home")
                .putExtra(EXTRA_BACKGROUND, true)
                .putExtra(EXTRA_PENDING_INTENT, pi)
            context.startForegroundService(intent)

            val result = withTimeoutOrNull(RESULT_TIMEOUT_MS) { deferred.await() }
            return when {
                result == null ->
                    null to "termux.run: no result within ${RESULT_TIMEOUT_MS / 1000}s " +
                        "(check Termux is running and allow-external-apps=true is set)"
                result.errmsg != null -> null to "termux.run error: ${result.errmsg}"
                else -> result.combined() to ""
            }
        } catch (t: Throwable) {
            Log.e(TAG, "termux.run dispatch failed", t)
            return null to "termux.run error: ${t.message}"
        } finally {
            pending.remove(id)
        }
    }

    /** TermuxResultReceiver 的回填入口。 */
    internal fun complete(id: Long, result: TermuxResult) {
        pending[id]?.complete(result)
    }

    fun cancelAll() {
        for ((id, d) in pending) {
            d.complete(TermuxResult(errmsg = "runtime shutting down"))
            pending.remove(id)
        }
    }
}

/** Termux 回传结果（stdout/stderr/exitCode 之外还有错误通路）。 */
data class TermuxResult(
    val stdout: String? = null,
    val stderr: String? = null,
    val exitCode: Int? = null,
    val errmsg: String? = null,
) {
    fun combined(): String = buildString {
        append(stdout ?: "")
        if (!stderr.isNullOrBlank()) {
            if (isNotEmpty()) append('\n')
            append("--- stderr ---\n").append(stderr)
        }
        if (exitCode != null && exitCode != 0) {
            if (isNotEmpty()) append('\n')
            append("exit_code: ").append(exitCode)
        }
    }.take(8_000)
}

/**
 * 结果回传接收者（manifest 注册，exported=false；PendingIntent 临时授权
 * 使 Termux 可达）。解析 Termux 结果 intent 的 "result" Bundle。
 */
class TermuxResultReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        val id = intent.getLongExtra("openslate_id", -1L)
        if (id < 0) return
        val bundle: Bundle? = runCatching {
            intent.getBundleExtra("result")
                ?: intent.getBundleExtra("com.termux.execute.result")
        }.getOrNull()
        val result = if (bundle != null) {
            TermuxResult(
                stdout = bundle.getString("stdout"),
                stderr = bundle.getString("stderr"),
                exitCode = if (bundle.containsKey("exitCode")) bundle.getInt("exitCode") else null,
                errmsg = bundle.getString("errmsg") ?: bundle.getString("err"),
            )
        } else {
            // 无 result Bundle：Termux 直接给出了 resultCode 语义（send 的
            // resultCode 非 0 = 出错），至少把可得的文本带回。
            TermuxResult(errmsg = "no result bundle (resultCode=$resultCode)")
        }
        Log.d("OpenSlateTermux", "result for #$id: err=${result.errmsg} exit=${result.exitCode}")
        TermuxExec.complete(id, result)
    }
}
