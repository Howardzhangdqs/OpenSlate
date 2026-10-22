package dev.openslate.mobile.service

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import androidx.core.app.NotificationCompat
import androidx.lifecycle.LifecycleService
import androidx.lifecycle.lifecycleScope
import dev.openslate.mobile.R
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.launch

/**
 * Agent 前台服务：持有 Rust MobileRuntime 的生命周期。
 *
 * - runtime 在 onCreate 起后台协程里创建（磁盘 bootstrap 首启稍慢）。
 * - onStartCommand 幂等；主界面通过 [RuntimeBridge] 单例共享状态。
 * - onDestroy 优雅关停 runtime（host call fail-all + 审批全拒）。
 * - 回合期唤醒锁（wakeLock/wifiLock）：running 期间持 CPU + Wi-Fi 锁，见字段注释。
 */
class AgentService : LifecycleService() {

    /**
     * 回合期唤醒锁（PARTIAL_WAKE_LOCK + WifiLock，tag 均 "openslate:turn"）。
     *
     * 为什么需要：前台服务只保**进程**不被杀，**不保 CPU 不休眠、不保网卡
     * 不断连**——App 切后台后系统照常进 doze/省电，模型流式连接被掐断
     * （"error decoding response body"）。因此由**回合运行态**驱动：
     * 收集 [RuntimeBridge.state] 的 `running` 字段，true → acquire（回合期
     * 持锁），false → release（回合结束即还，空闲不长持耗电）。
     *
     * - PARTIAL_WAKE_LOCK：与屏幕无关，灭屏也保持 CPU 运行；
     * - WifiLock(WIFI_MODE_FULL_HIGH_PERF)：抑制 Wi-Fi 省电模式休眠断连
     *   （high-perf 档：比 LOW_LATENCY 温和、比默认省电档激进，长流式
     *   场景吞吐/延迟稳定的平衡点）；
     * - 两者均 setReferenceCounted(false)，ensure/release 幂等（isHeld 守卫）；
     * - onCreate 创建（不获取），onDestroy 强制释放兜底；
     * - 局限：MIUI 类激进省电（无锁持有白名单时冻结进程/断网）不保证
     *   100% 拦截，回合级重试是另一轨的兜底，不在此处解决。
     */
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null

    override fun onBind(intent: Intent): IBinder? {
        super.onBind(intent)
        return null
    }

    override fun onCreate() {
        super.onCreate()
        android.util.Log.i("OpenSlateBridge", "AgentService: onCreate")
        startForegroundWithNotification()
        createTurnLocks()
        lifecycleScope.launch {
            RuntimeBridge.start(this@AgentService)
        }
        // 回合运行态驱动锁的获取/释放：running=true 持锁（后台保网络/CPU），
        // false 立即释放（空闲不耗电）。collect 挂 lifecycleScope，服务销毁
        // 自动停；onDestroy 的强制释放兜底覆盖协程被截断的场景。
        lifecycleScope.launch {
            RuntimeBridge.state.collect { s ->
                if (s.running) ensureTurnLocks() else releaseTurnLocks()
            }
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        super.onStartCommand(intent, flags, startId)
        return Service.START_STICKY
    }

    override fun onDestroy() {
        // 兜底：收集协程可能被 DESTROYED 截断，此处同步强制释放，
        // 防止泄漏持有唤醒锁（系统会告警 + 耗电）。
        releaseTurnLocks()
        wakeLock = null
        wifiLock = null
        // 进程即将销毁：此前经 lifecycleScope.launch 执行 shutdown，DESTROYED
        // 竞态会截断协程、Rust 侧 run 终态收尾与 SQLite 收束做不完。这里宁可
        // 短暂阻塞主线程（上限 2.5s）也要同步等它完成。
        kotlinx.coroutines.runBlocking {
            kotlinx.coroutines.withTimeoutOrNull(2_500) { RuntimeBridge.shutdown() }
        }
        super.onDestroy()
    }

    /** 创建锁对象（不获取）。onCreate 一次，之后 acquire/release 复用。 */
    private fun createTurnLocks() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, TURN_LOCK_TAG)
        wakeLock?.setReferenceCounted(false)
        val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        // WIFI_MODE_FULL_HIGH_PERF 在 API 34 起标记 deprecated（行为趋同
        // WIFI_MODE_FULL），但 minSdk 26 一条通路用它最稳，继续使用。
        @Suppress("DEPRECATION")
        wifiLock = wm.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, TURN_LOCK_TAG)
        wifiLock?.setReferenceCounted(false)
    }

    /** running=true：确保双锁持有（幂等）。 */
    private fun ensureTurnLocks() {
        if (wakeLock?.isHeld != true) {
            wakeLock?.acquire()
            android.util.Log.i("OpenSlateBridge", "turn locks: acquired (running)")
        }
        if (wifiLock?.isHeld != true) {
            wifiLock?.acquire()
        }
    }

    /** running=false / 服务销毁：释放双锁（幂等）。 */
    private fun releaseTurnLocks() {
        if (wakeLock?.isHeld == true) {
            wakeLock?.release()
            android.util.Log.i("OpenSlateBridge", "turn locks: released (idle)")
        }
        if (wifiLock?.isHeld == true) {
            wifiLock?.release()
        }
    }

    private fun startForegroundWithNotification() {

        val channelId = getString(R.string.agent_service_channel)
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(
                channelId,
                "Agent Runtime",
                NotificationManager.IMPORTANCE_LOW,
            )
        )
        val notification = NotificationCompat.Builder(this, channelId)
            .setContentTitle(getString(R.string.agent_service_notification_title))
            .setContentText(getString(R.string.agent_service_notification_text))
            .setSmallIcon(android.R.drawable.stat_notify_chat)
            .setOngoing(true)
            .build()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            startForeground(
                NOTIFICATION_ID,
                notification,
                ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE,
            )
        } else {
            startForeground(NOTIFICATION_ID, notification)
        }
    }

    companion object {
        private const val NOTIFICATION_ID = 1001

        /** WakeLock / WifiLock 共用 tag（adb dumpsys power 可见归属）。 */
        private const val TURN_LOCK_TAG = "openslate:turn"

        fun start(context: Context) {
            val intent = Intent(context, AgentService::class.java)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, AgentService::class.java))
        }
    }
}
