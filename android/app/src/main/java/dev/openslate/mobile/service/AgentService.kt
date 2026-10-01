package dev.openslate.mobile.service

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
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
 */
class AgentService : LifecycleService() {

    override fun onBind(intent: Intent): IBinder? {
        super.onBind(intent)
        return null
    }

    override fun onCreate() {
        super.onCreate()
        android.util.Log.i("OpenSlateBridge", "AgentService: onCreate")
        startForegroundWithNotification()
        lifecycleScope.launch {
            RuntimeBridge.start(this@AgentService)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        super.onStartCommand(intent, flags, startId)
        return Service.START_STICKY
    }

    override fun onDestroy() {
        lifecycleScope.launch {
            RuntimeBridge.shutdown()
        }
        super.onDestroy()
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

        fun start(context: Context) {
            val intent = Intent(context, AgentService::class.java)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, AgentService::class.java))
        }
    }
}
