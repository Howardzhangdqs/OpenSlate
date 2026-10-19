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
 *
 * 停滞续跑（Termux 未启动/被冻结时结果 PendingIntent 永不回传）：
 * 宿主 stall 看门狗（RuntimeBridge）标记「等待连接」[setWaitingConnection]
 * → 豁免 [RESULT_TIMEOUT_MS] 计时（面板引导用户恢复 Termux）；连接恢复
 * 后宿主经 [resend] 用**原 id** 重发 RUN_COMMAND——pending 表与 deferred
 * 原样复用，结果仍走 TermuxResultReceiver → [complete] → 原挂起调用，
 * 回填链路零新增。全程受硬上限（[HARD_WAIT_TIMEOUT_MS]，与 Rust 侧
 * termux HostTool 900s 上限配套）兜底，不允许无限等。
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
    private class PendingCall(
        val deferred: CompletableDeferred<TermuxResult>,
        /** 原命令：等待连接后续跑的重发要用原 id + 原命令。 */
        val command: String,
    ) {
        /** 已派发次数（首发 1 + 重发 N）。每次派发取新序号，PendingIntent
         *  request code 随之错开——FLAG_ONE_SHOT 的旧记录可能已被消费或
         *  仍挂着，同码重建会拿到同一记录导致回传错绑。 */
        val dispatchCount = java.util.concurrent.atomic.AtomicInteger(0)
    }

    private val pending = ConcurrentHashMap<Long, PendingCall>()

    /**
     * 「等待 Termux 连接」模式（豁免 [RESULT_TIMEOUT_MS] 计时）的 id 集
     * 合：宿主 stall 看门狗标记（命令发出 0.5s 无应答 = Termux 未启动/
     * 被冻结，结果 PendingIntent 永不回传），重发时解除。等待期间结果
     * 照常回填（轮询等待不设时限，由 [HARD_WAIT_TIMEOUT_MS] 全局兜底）。
     */
    private val waitingConnection: MutableSet<Long> =
        ConcurrentHashMap.newKeySet()

    /** 等待轮询周期：等待模式下的复查粒度（豁免解除 → 恢复计时的切换
     *  延迟上限）。 */
    private const val WAITING_POLL_MS = 1_000L

    /**
     * 单次调用总等待硬上限：与 Rust 侧 termux HostTool 的 900s 上限
     * （exec.rs `TERMUX_HOST_CALL_TIMEOUT`）配套——等待连接模式豁免
     * 90s 后仍要有界（用户始终不恢复时让工具报错、回合可继续），取略
     * 低于 900s 的值，超时定性由本侧先给出。
     */
    private const val HARD_WAIT_TIMEOUT_MS = 880_000L

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
     * 调用方组装；永不抛出。停滞豁免：进入「等待 Termux 连接」模式
     * （[setWaitingConnection]）的调用不计 90s 超时，等待连接后续跑；
     * 全程仍有 [HARD_WAIT_TIMEOUT_MS] 硬上限。
     */
    suspend fun run(context: Context, id: Long, command: String): Pair<String?, String> {
        if (command.isBlank()) return null to "termux.run: empty command"
        if (!hasPermission(context)) {
            return null to
                "termux.run: RUN_COMMAND permission not granted. " +
                "Grant it in system Settings → Apps → OpenSlate → Permissions, " +
                "or via Shizuku on the settings page. On some ROMs Termux must " +
                "be opened once before this permission can be granted."
        }
        val call = PendingCall(CompletableDeferred(), command)
        pending[id] = call
        try {
            dispatch(context, id, call)
            val result = awaitResult(id, call.deferred)
            return when {
                result == null ->
                    null to "termux.run: no result within ${RESULT_TIMEOUT_MS / 1000}s " +
                        "(or ${HARD_WAIT_TIMEOUT_MS / 1000}s while waiting for Termux to " +
                        "reconnect). Termux is likely in stopped state (never opened since " +
                        "install / force-stopped) or frozen by the system. Open Termux once, " +
                        "enable its wakelock, and disable battery optimization for Termux."
                result.errmsg != null -> null to "termux.run error: ${result.errmsg}"
                else -> result.combined() to ""
            }
        } catch (c: kotlinx.coroutines.CancellationException) {
            throw c
        } catch (t: Throwable) {
            Log.e(TAG, "termux.run dispatch failed", t)
            return null to "termux.run error: ${t.message}"
        } finally {
            pending.remove(id)
            waitingConnection.remove(id)
        }
    }

    /**
     * 结果等待：正常路径按 [RESULT_TIMEOUT_MS] 限时；「等待连接」模式
     * （id 在 [waitingConnection]）豁免计时，短周期轮询复查——豁免解除
     * （重发）后重起完整 90s 窗口（重发后恢复正常超时计时）。超时返回
     * null；总时长触 [HARD_WAIT_TIMEOUT_MS] 硬上限同样返回 null（等待
     * 连接不允许无限等）。
     */
    private suspend fun awaitResult(
        id: Long,
        deferred: CompletableDeferred<TermuxResult>,
    ): TermuxResult? {
        val hardDeadline = System.currentTimeMillis() + HARD_WAIT_TIMEOUT_MS
        var deadline: Long? = null // 限时窗口截止；null = 未起表（等待连接中）
        while (true) {
            if (id in waitingConnection) {
                withTimeoutOrNull(WAITING_POLL_MS) { deferred.await() }?.let { return it }
                deadline = null
            } else {
                val now = System.currentTimeMillis()
                if (deadline == null) deadline = now + RESULT_TIMEOUT_MS
                if (now >= deadline || now >= hardDeadline) return null
                val slice = minOf(WAITING_POLL_MS, deadline - now, hardDeadline - now)
                withTimeoutOrNull(slice) { deferred.await() }?.let { return it }
            }
        }
    }

    /**
     * 派发 RUN_COMMAND（首发与重发共用）。request code 以派发序号分段
     * （每段 0x1000_0000 = 268M）：工具调用 id 从 1 起、探测段 8 亿 /
     * UI 段 9 亿，各段与重发段互不重叠，PendingIntent 记录不会错绑。
     * 派发失败抛出（由调用方处置）。
     */
    private fun dispatch(context: Context, id: Long, call: PendingCall) {
        val attempt = call.dispatchCount.incrementAndGet()
        // 带 id 的回传 intent（Termux send() 的 fill-in 不会覆盖我们的 extra key）。
        val resultIntent = Intent(context, TermuxResultReceiver::class.java)
            .putExtra("openslate_id", id)
        val pi = android.app.PendingIntent.getBroadcast(
            context,
            (id + (attempt - 1) * 0x1000_0000L).toInt(),
            resultIntent,
            android.app.PendingIntent.FLAG_MUTABLE or android.app.PendingIntent.FLAG_ONE_SHOT,
        )
        val intent = Intent(RUN_COMMAND_ACTION)
            .setClassName(TERMUX_PACKAGE, RUN_COMMAND_SERVICE)
            .putExtra(EXTRA_PATH, "/data/data/com.termux/files/usr/bin/bash")
            .putExtra(EXTRA_ARGUMENTS, arrayOf("-c", call.command))
            .putExtra(EXTRA_WORKDIR, "/data/data/com.termux/files/home")
            .putExtra(EXTRA_BACKGROUND, true)
            .putExtra(EXTRA_PENDING_INTENT, pi)
        context.startForegroundService(intent)
    }

    /**
     * 用原 id 重发 RUN_COMMAND（等待连接后续跑）：**复用 pending 表里
     * 既有的 deferred**（结果回传链路零变化——TermuxResultReceiver.
     * complete 填回原 deferred，RuntimeBridge.runInTermux 的挂起等待原样
     * 唤醒），不新建 pending 项。返回 false = 该 id 已不在途（结果已到 /
     * 已取消），无需重发；派发抛异常时以错误回填 deferred（调用方以错误
     * 收尾）并返回 true。
     */
    internal fun resend(context: Context, id: Long): Boolean {
        val call = pending[id] ?: return false
        return try {
            dispatch(context, id, call)
            Log.d(TAG, "resend RUN_COMMAND for #$id (attempt ${call.dispatchCount.get()})")
            true
        } catch (t: Throwable) {
            Log.e(TAG, "termux.run resend dispatch failed for #$id", t)
            call.deferred.complete(TermuxResult(errmsg = "resend dispatch failed: ${t.message}"))
            true
        }
    }

    /** 标记/解除「等待 Termux 连接」模式（豁免 90s 计时；见 run）。 */
    fun setWaitingConnection(id: Long, waiting: Boolean) {
        if (waiting) waitingConnection.add(id) else waitingConnection.remove(id)
    }

    /** TermuxResultReceiver 的回填入口。 */
    internal fun complete(id: Long, result: TermuxResult) {
        pending[id]?.deferred?.complete(result)
    }

    /** 该 id 是否仍有一条未应答的命令（UI stall 看门狗探测用）。 */
    fun isPending(id: Long): Boolean = pending.containsKey(id)

    fun cancelAll() {
        waitingConnection.clear()
        for ((id, d) in pending) {
            d.deferred.complete(TermuxResult(errmsg = "runtime shutting down"))
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
    /** Termux 错误码（result Bundle 的整型 "err"，如 allow-external-apps 未开 = 2）。 */
    val errCode: Int? = null,
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
 * 使 Termux 可达）。解析 Termux 结果 intent 的 result Bundle：依次试
 * "result"、"com.termux.execute.result" 两个 key，都不像则扫描 extras 里
 * 第一个满足结果 Bundle 形状（含 stdout/stderr/errmsg/err 任一）的值
 * （不同 Termux 版本/分支的 extra key 有差异）。
 */
class TermuxResultReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        val id = intent.getLongExtra("openslate_id", -1L)
        if (id < 0) return
        val bundle: Bundle? = runCatching { findResultBundle(intent) }.getOrNull()
        val result = if (bundle != null) {
            TermuxResult(
                stdout = bundle.getString("stdout"),
                stderr = bundle.getString("stderr"),
                exitCode = if (bundle.containsKey("exitCode")) bundle.getInt("exitCode") else null,
                errmsg = bundle.getString("errmsg") ?: bundle.getString("err"),
                errCode = if (bundle.containsKey("err")) {
                    runCatching { bundle.getInt("err") }.getOrNull()
                } else null,
            )
        } else {
            // 无 result Bundle：Termux 直接给出了 resultCode 语义（send 的
            // resultCode 非 0 = 出错），至少把可得的文本带回。
            TermuxResult(errmsg = "no result bundle (resultCode=$resultCode)")
        }
        Log.d("OpenSlateTermux", "result for #$id: err=${result.errmsg} exit=${result.exitCode}")
        TermuxExec.complete(id, result)
    }

    @Suppress("DEPRECATION") // Bundle.get(String)：扫描 extras 兼容旧 API（33+ 弃用但可用）
    private fun findResultBundle(intent: Intent): Bundle? {
        fun looksLikeResult(b: Bundle?): Boolean =
            b != null && (b.containsKey("stdout") || b.containsKey("stderr") ||
                b.containsKey("errmsg") || b.containsKey("err"))
        val extras = intent.extras ?: return null
        return when {
            looksLikeResult(extras.getBundle("result")) -> extras.getBundle("result")
            looksLikeResult(extras.getBundle("com.termux.execute.result")) ->
                extras.getBundle("com.termux.execute.result")
            else -> extras.keySet().firstNotNullOfOrNull { k ->
                (extras.get(k) as? Bundle)?.takeIf { looksLikeResult(it) }
            }
        }
    }
}
