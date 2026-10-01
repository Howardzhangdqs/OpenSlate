package dev.openslate.mobile.bridge

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log

/**
 * 调试用 adb 注入通道（Phase 1 临时，设置页上线后移除）：
 *
 *   # 注入 API key（进程内存，重启失效）
 *   adb shell am broadcast \
 *     -a dev.openslate.mobile.SET_KEY --es provider zhipu --es key "xxxx"
 *
 *   # 发送任意 ClientMsg JSON
 *   adb shell am broadcast \
 *     -a dev.openslate.mobile.SEND --es msg '{"type":"submit","text":"hi"}'
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
        }
    }

    companion object {
        private const val TAG = "OpenSlateBridge"
        const val ACTION_SET_KEY = "dev.openslate.mobile.SET_KEY"
        const val ACTION_SET_PROXY = "dev.openslate.mobile.SET_PROXY"
        const val ACTION_SEND = "dev.openslate.mobile.SEND"
    }
}
