package dev.openslate.mobile.bridge

import android.content.Context
import android.content.Intent
import android.content.pm.ApplicationInfo
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.atomic.AtomicLong

/**
 * Termux RUN_COMMAND 通道精确诊断 + 自动拉起。
 *
 * 一次 `echo ok` 往返的结局可精确分类（依据 RunCommandService 行为）：
 * - Ok：正常往返；
 * - PolicyRejected：Termux 快速回调（RESULT_OK + err=2、errmsg 含
 *   allow-external-apps）——allow-external-apps 未开启或 Termux 未初始化；
 * - Rejected：其他明确拒绝；
 * - DispatchError：startForegroundService 本身抛异常（intent 没发出去）；
 * - NoResponse：无任何回调 → 超时。Termux 处于 FLAG_STOPPED（首装从未
 *   打开 / 被强制停止）时 service intent 不会执行，只有 startActivity
 *   类 intent 能穿透；非 stopped 但被冻结时 startForegroundService 可
 *   自动冷启动进程。
 *
 * 恢复策略（真机验证定稿）：**取消自动拉起**——非 stopped 无应答静默
 * 重试一次（6s）；停止态无法后台唤醒（真机证实系统只允许打开 Termux
 * 界面唤醒，自动跳转打扰用户）→ 如实返回诊断，由 UI 引导用户手动打开
 * Termux，返回 OpenSlate 时 ON_RESUME 自动重检。`launchTermuxAndReturn`
 * 仅用于 installHost 首步（用户主动触发的一键安装流程）。
 */
object TermuxDiagnostics {

    /** 一次通道探测（echo ok 往返）的精确结局。 */
    sealed class ChannelResult {
        data class Ok(val ms: Long) : ChannelResult()
        /** Termux 快速拒绝：allow-external-apps 未开启（或 Termux 未初始化）。 */
        data class PolicyRejected(val errmsg: String, val ms: Long) : ChannelResult()
        /** 其他明确拒绝（ errmsg/错误码带回）。 */
        data class Rejected(val errmsg: String, val errCode: Int?, val ms: Long) : ChannelResult()
        /** 派发失败（startForegroundService 抛异常，intent 未送达）。 */
        data class DispatchError(val error: String) : ChannelResult()
        /** 无应答（超时）：stopped = Termux 是否处于 FLAG_STOPPED。 */
        data class NoResponse(val stopped: Boolean, val ms: Long) : ChannelResult()
    }

    /** 首探上限（用户拍板：1 秒无应答即定性，不陪跑）。 */
    const val PROBE_FIRST_MS = 1_000L
    /** 拉起 Termux 后等多久把自己切回前台。 */
    const val LAUNCH_RETURN_MS = 1_800L
    /** 无应答时的静默重试上限（startForegroundService 自动冷启动进程）。 */
    const val PROBE_AFTER_LAUNCH_MS = 6_000L
    /** 无应答早提示阈值（用户拍板：0.5s 无应答即亮「打开 Termux」指引，
     *  不等 1s/6s 探测阶梯跑完；探测自身照常继续，结论仍由返回值给出）。 */
    const val PROBE_STALL_HINT_MS = 500L

    /** 本对象专用 Termux 调用 id 段（host call 从 0 起；McpHostManager 用 900_000_000 段）。 */
    private val probeIds = AtomicLong(800_000_000L)

    /** Termux 包是否处于 FLAG_STOPPED（首装从未打开 / 被强制停止）。 */
    fun isTermuxStopped(ctx: Context): Boolean = runCatching {
        (ctx.packageManager.getApplicationInfo(TermuxExec.TERMUX_PACKAGE, 0).flags
            and ApplicationInfo.FLAG_STOPPED) != 0
    }.getOrDefault(false)

    /**
     * 单次通道探测：`echo ok` 往返并分类。超时（含 TermuxExec 内部 90s
     * 兜底被外层截断）→ NoResponse（附 stopped 状态）。
     *
     * [onStall]：发出 [PROBE_STALL_HINT_MS] 仍无应答时的早提示回调
     * （最多一次）——UI 借此在探测继续的同时先亮出指引。锁内双检
     * isPending：结果可能在 delay 期间已到达。
     */
    suspend fun probeChannel(
        ctx: Context,
        timeoutMs: Long,
        onStall: (() -> Unit)? = null,
    ): ChannelResult {
        val t0 = System.currentTimeMillis()
        val appCtx = ctx.applicationContext
        val probeId = probeIds.incrementAndGet()
        var stallFired = false
        val outcome: Pair<String?, String>? = coroutineScope {
            // stall 看门狗：随结果返回（finally）一并取消；探测阶梯不变。
            val watchdog = if (onStall != null) launch {
                delay(PROBE_STALL_HINT_MS)
                // 锁内定夺、锁外调用：回调里可能写 UI 状态，不持锁。
                val fire = synchronized(this@TermuxDiagnostics) {
                    if (!stallFired && TermuxExec.isPending(probeId)) {
                        stallFired = true
                        onStall
                    } else {
                        null
                    }
                }
                fire?.invoke()
            } else {
                null
            }
            try {
                withTimeoutOrNull(timeoutMs) { TermuxExec.run(appCtx, probeId, "echo ok") }
            } finally {
                watchdog?.cancel()
            }
        }
        val (out, err) = outcome ?: return ChannelResult.NoResponse(
            stopped = isTermuxStopped(ctx),
            ms = timeoutMs,
        )
        val ms = System.currentTimeMillis() - t0
        return when {
            out?.trim() == "ok" && err.isEmpty() -> ChannelResult.Ok(ms)
            // Termux 快速回调的 errmsg 含 allow-external-apps（忽略大小写）。
            err.contains("allow-external-apps", ignoreCase = true) ->
                ChannelResult.PolicyRejected(firstNonEmptyLine(err), ms)
            // TermuxExec.run 派发抛异常时不抛出，而是返回该前缀文本。
            err.startsWith("termux.run error:") -> ChannelResult.DispatchError(err)
            else -> ChannelResult.Rejected(err, null, ms)
        }
    }

    /** 一行中文摘要（不带时间戳；时间戳由调用方日志加）。 */
    fun describe(r: ChannelResult): String = when (r) {
        is ChannelResult.Ok -> "通过（${r.ms}ms）"
        is ChannelResult.PolicyRejected ->
            "未放行外部应用（allow-external-apps 未开启或 Termux 未初始化）"
        is ChannelResult.Rejected -> "Termux 拒绝：${r.errmsg.take(120)}"
        is ChannelResult.DispatchError -> "命令派发失败：${r.error.take(120)}"
        is ChannelResult.NoResponse -> if (r.stopped) {
            "无应答；Termux 处于停止状态（安装后从未打开或被强制停止）"
        } else {
            "无应答；Termux 可能被系统冻结或后台受限"
        }
    }

    /**
     * 拉起 Termux 前台完成初始化，约 1.8s 后把 OpenSlate 切回前台
     * （NEW_TASK|SINGLE_TOP|REORDER_TO_FRONT，复用已有任务栈不重走路由）。
     * App 有 AgentService 前台服务，通常具备后台启动 Activity 豁免；被
     * 系统拦截时 runCatching 静默失败不崩溃（用户仍停留在 Termux，无害，
     * 不影响 Termux 已被拉起的事实）。返回 false = 找不到 Termux 启动入口。
     */
    internal suspend fun launchTermuxAndReturn(ctx: Context): Boolean {
        if (!McpHostManager.startTermuxApp(ctx)) return false
        delay(LAUNCH_RETURN_MS)
        ctx.packageManager.getLaunchIntentForPackage(ctx.packageName)?.let { back ->
            back.addFlags(
                Intent.FLAG_ACTIVITY_NEW_TASK or
                    Intent.FLAG_ACTIVITY_SINGLE_TOP or
                    Intent.FLAG_ACTIVITY_REORDER_TO_FRONT,
            )
            runCatching { ctx.startActivity(back) }
        }
        return true
    }

    /**
     * 通道就绪保障（阶梯式，不做自动拉起）：
     * 1. 首次探测（1s 上限）通过 → 直接返回；
     * 2. 配置类失败（未放行 / 被拒 / 派发失败）→ 重试无用，原样返回；
     * 3. 无应答且非 stopped → 静默重试一次（startForegroundService 可
     *    自动冷启动 Termux 进程，6s 上限）；
     * 4. 无应答且 stopped → 直接返回诊断结果，不重试、不启动任何
     *    Activity：停止态 Termux 只有打开其界面才能唤醒（真机证实系统
     *    不允许后台拉起，自动跳转打扰用户），由 UI 引导用户手动打开
     *    Termux，返回 ON_RESUME 自动重检。
     */
    suspend fun ensureTermuxUp(
        ctx: Context,
        log: (String) -> Unit,
        onStall: (() -> Unit)? = null,
    ): ChannelResult {
        // onStall 只挂首探：配置类快速失败（PolicyRejected/Rejected/
        // DispatchError）毫秒级返回，无需早提示；重试阶段首探的 NoResponse
        // 指引已在场，也无需重复触发。
        val first = probeChannel(ctx, PROBE_FIRST_MS, onStall)
        if (first is ChannelResult.Ok) {
            log("RUN_COMMAND 通道：通过（${first.ms}ms）")
            return first
        }
        log("RUN_COMMAND 通道：${describe(first)}")
        // 配置类问题（未放行 / 被拒 / 派发失败）：重试无用。
        if (first !is ChannelResult.NoResponse) return first
        // 停止态：无法后台唤醒，不做任何拉起动作，如实返回诊断。
        if (first.stopped) return first
        // 非 stopped：进程可能被杀/冻结，RUN_COMMAND 会自动冷启动，重试一次。
        log("静默重试一次（RUN_COMMAND 可自动冷启动 Termux 进程）…")
        val retry = probeChannel(ctx, PROBE_AFTER_LAUNCH_MS)
        if (retry is ChannelResult.Ok) {
            log("RUN_COMMAND 通道：重试通过（${retry.ms}ms）")
        } else {
            log("RUN_COMMAND 通道：${describe(retry)}")
        }
        return retry
    }

    private fun firstNonEmptyLine(s: String): String =
        s.lines().firstOrNull { it.isNotBlank() }?.trim() ?: s
}
