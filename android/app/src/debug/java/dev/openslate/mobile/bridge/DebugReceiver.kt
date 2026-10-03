package dev.openslate.mobile.bridge

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import kotlinx.coroutines.launch

/**
 * 调试用 adb 注入通道（仅 debug 变体携带，release 不包含本类与注册）：
 *
 *   # 注入 API key（进程内存，重启失效）
 *   adb shell am broadcast \
 *     -a dev.openslate.mobile.SET_KEY --es provider zhipu --es key "xxxx"
 *
 *   # 发送任意 ClientMsg JSON
 *   adb shell am broadcast \
 *     -a dev.openslate.mobile.SEND --es msg '{"type":"submit","text":"hi"}'
 *
 *   # 在 Termux 中执行命令（RUN_COMMAND，结果进 logcat）
 *   adb shell am broadcast \
 *     -a dev.openslate.mobile.TERMUX --es cmd 'ls ~/.openslate'
 */
class DebugReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        Log.i(TAG, "debug broadcast: ${intent.action}")
        when (intent.action) {
            ACTION_SET_KEY -> {
                val provider = intent.getStringExtra("provider") ?: return
                val key = intent.getStringExtra("key") ?: return
                RuntimeBridge.setApiKeyAndPersist(provider, key)
                Log.i(TAG, "api key injected+persisted for provider=$provider (${key.length} chars)")
            }
            ACTION_SET_PROXY -> {
                val url = intent.getStringExtra("url") ?: return
                RuntimeBridge.setHttpProxy(url)
                Log.i(TAG, "http proxy set+persisted: $url")
            }
            ACTION_SEND -> {
                val msg = intent.getStringExtra("msg") ?: return
                val ok = RuntimeBridge.sendJson(msg)
                Log.i(TAG, "send via broadcast: ok=$ok")
            }
            ACTION_TERMUX -> {
                // 不用 goAsync：其 pendingResult 有 ~10s 强制窗口，而
                // RUN_COMMAND 往返可达 90s，超窗即 ANR（真机踩坑实测）。
                // 直接投后台协程，receiver 立即返回；App 在前台时进程
                // 存活，任务照常跑完（调试通道，可接受）。
                val cmd = intent.getStringExtra("cmd") ?: return
                kotlinx.coroutines.CoroutineScope(kotlinx.coroutines.Dispatchers.IO).launch {
                    try {
                        val (out, err) = McpHostManager.debugRun(context, cmd)
                        Log.i(TAG, "termux debug result: out=${out?.take(2000)} err=${err.take(500)}")
                    } catch (t: Throwable) {
                        Log.e(TAG, "termux debug failed", t)
                    }
                }
            }
        }
    }

    companion object {
        private const val TAG = "OpenSlateBridge"
        const val ACTION_SET_KEY = "dev.openslate.mobile.SET_KEY"
        const val ACTION_SET_PROXY = "dev.openslate.mobile.SET_PROXY"
        const val ACTION_SEND = "dev.openslate.mobile.SEND"
        const val ACTION_TERMUX = "dev.openslate.mobile.TERMUX"
    }
}
