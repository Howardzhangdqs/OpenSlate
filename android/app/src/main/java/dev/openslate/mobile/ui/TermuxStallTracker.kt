package dev.openslate.mobile.ui

import android.content.Context
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Termux 停滞连接面板的状态管线（纯数据，无 UI）。
 *
 * 驱动方：聊天页在 stall 激活（termuxStallSince 非 null）期间周期
 * `refresh`（约 2.5s 一次）+ ON_RESUME 立即重探（对齐设置页 gateRefresh
 * 范式——用户从 Termux 切回时最快确认恢复）。探测阶梯复用
 * [detectTermuxStatus]（MCP host 探活 → 安装/权限 → echo ok 通道往返，
 * 无副作用、不自动拉起）。
 *
 * - CAS 单飞（照 [ComposerMcpStatusTracker] 范式）：在途探测中重复
 *   refresh 直接忽略，不叠加并发探测；探测串行单线程 IO；
 * - 快照保留上轮结论（probing 只置标志不重置 status），面板在探测间隙
 *   不闪烁；
 * - **连接事件**只在「非 Ready → Ready」跳变时上报一次
 *   （[RuntimeBridge.onTermuxConnectionRestored]）：且需自上次 Ready 后
 *   观察到过通道中断（见过非 Ready）才携带重发语义——未见中断的 Ready
 *   （慢命令误报停滞）只解除等待豁免、不重发，防命令重复执行。
 */
internal object TermuxStallTracker {

    /** 面板快照：probing = 探测在途（status 保留上轮终态）。 */
    internal data class Snapshot(
        val probing: Boolean = false,
        val status: TermuxStatus = TermuxStatus.Checking,
    )

    /** 单线程 IO：探测串行化，RUN_COMMAND 往返不与在途探测交错。 */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO.limitedParallelism(1))

    private val _state = MutableStateFlow(Snapshot())

    /** 最新快照；初始 Checking（stall 面板出现后的首次探测前显示探测中）。 */
    val state: StateFlow<Snapshot> = _state.asStateFlow()

    /** 自上次 Ready 后是否观察到过通道中断（连接事件 → 是否重发续跑）。 */
    private val sawDownSinceReady = AtomicBoolean(false)

    /** 触发一次探测（fire-and-forget）：在途探测中调用为无操作。 */
    fun refresh(context: Context) {
        val appCtx = context.applicationContext
        // CAS 抢占探测权（照 ComposerMcpStatusTracker）。
        while (true) {
            val cur = _state.value
            if (cur.probing) return
            if (_state.compareAndSet(cur, cur.copy(probing = true))) break
        }
        scope.launch {
            val status = runCatching { detectTermuxStatus(appCtx) }.getOrElse {
                TermuxStatus.ChannelDown(it.message ?: it.javaClass.simpleName, stopped = false)
            }
            // 连接事件判定先于写终态：becameReady 基于上轮快照（CAS 时保留）。
            val becameReady = status is TermuxStatus.Ready &&
                _state.value.status !is TermuxStatus.Ready
            if (status !is TermuxStatus.Ready) sawDownSinceReady.set(true)
            _state.value = Snapshot(probing = false, status = status)
            if (becameReady) {
                RuntimeBridge.onTermuxConnectionRestored(sawDownSinceReady.getAndSet(false))
            }
        }
    }
}
