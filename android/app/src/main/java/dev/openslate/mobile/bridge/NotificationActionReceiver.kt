package dev.openslate.mobile.bridge

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import androidx.core.app.RemoteInput
import dev.openslate.mobile.service.AgentService

/**
 * notify 通知动作接收器（Manifest 注册 `exported=false`，§7.5——外部应用无法
 * 伪造通知动作注入用户消息/伪造 resolve；照 TermuxResultReceiver 先例）。
 *
 * onReceive 内**不做任何耗时操作**（§5.3）：取 extras / RemoteInput 文本 →
 * 分流入 RuntimeBridge（runtime 未就绪则入冷启动缓冲，§4.7）→ return。
 *
 * 分流（§4.4 / D5 wire）：
 * - 动作按钮：wait → 先试在途 `resolveHostCall(request_id, {outcome:"action",
 *   label, value})`（resolve 返回 false = 失效 id 无害 no-op）→ 迟到上行
 *   `notification_reply`；非 wait → 作为新用户消息回流（submitFromNotification，
 *   runId 对齐）。
 * - `behavior=cancel_turn`：wait → resolve/迟到上行 cancelled；随后 cancel()
 *   停止当前回合；非 wait → 直接 cancel()（即时生效）。
 * - RemoteInput 内联回复：outcome="replied"、text=原文（原文注入，零噪音）；
 *   非 wait → 同样回流为用户消息。
 * - deleteIntent（仅 wait）：outcome="dismissed"（同分流）；非 wait 无操作。
 * - ask_user 伴生唤回「不回答」：RuntimeBridge.declineQuestionCard（live →
 *   resolve declined；恢复卡 → question_reply decline，函数内双通路）。
 */
class NotificationActionReceiver : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        val ctx = context.applicationContext
        when (intent.action) {
            AgentNotifier.ACTION_BUTTON -> handleButton(ctx, intent)
            AgentNotifier.ACTION_REPLY -> handleReply(ctx, intent)
            AgentNotifier.ACTION_DELETE -> handleDelete(ctx, intent)
            AgentNotifier.ACTION_ASK_USER_DECLINE -> handleAskUserDecline(ctx, intent)
            else -> Unit
        }
    }

    /** 统一分发：runtime 就绪即执行；未就绪入队 + 拉起 AgentService（§4.7）。 */
    private fun dispatch(ctx: Context, action: () -> Unit) {
        if (!RuntimeBridge.runOrDeferNotificationAction(action)) {
            runCatching { AgentService.start(ctx) }
                .onFailure { Log.w(TAG, "notify: start AgentService failed", it) }
        }
    }

    private fun handleButton(ctx: Context, intent: Intent) {
        val requestId = intent.getLongExtra(AgentNotifier.EXTRA_REQUEST_ID, -1L)
        val wait = intent.getBooleanExtra(AgentNotifier.EXTRA_WAIT, false)
        val runId = intent.getStringExtra(AgentNotifier.EXTRA_RUN_ID)
        val agentId = intent.getStringExtra(AgentNotifier.EXTRA_AGENT_ID)
        val label = intent.getStringExtra(AgentNotifier.EXTRA_LABEL).orEmpty()
        // 缺省文案与工具 Schema 约定一致（Rust 侧已归一化，此处兜底）。
        val value = intent.getStringExtra(AgentNotifier.EXTRA_VALUE)
            ?: "用户在通知上点了「$label」"
        val behavior = intent.getStringExtra(AgentNotifier.EXTRA_BEHAVIOR)
        dispatch(ctx) {
            when {
                behavior == "cancel_turn" ->
                    // wait 挂起：先 resolve/迟到上行 cancelled，再停回合；非 wait 直接停。
                    RuntimeBridge.notificationCancelTurn(if (wait) requestId else -1L, runId)
                wait && requestId >= 0 ->
                    RuntimeBridge.resolveNotificationOutcome(requestId, runId, "action", label, value)
                else -> RuntimeBridge.submitFromNotification(value, runId)
            }
            // 动作已消费：撤下通知（wait resolve 后；非 wait 回流后），防重复点击双发。
            agentId?.let { AgentNotifier.cancelRegistered(ctx, it) }
        }
    }

    private fun handleReply(ctx: Context, intent: Intent) {
        // RemoteInput 取文本（可能抛异常：结果 Bundle 形态异常）。
        val text = runCatching {
            RemoteInput.getResultsFromIntent(intent)
                ?.getCharSequence(AgentNotifier.REMOTE_INPUT_KEY)?.toString()?.trim()
        }.getOrNull().orEmpty()
        if (text.isEmpty()) return // 取消回复（空输入）
        val requestId = intent.getLongExtra(AgentNotifier.EXTRA_REQUEST_ID, -1L)
        val wait = intent.getBooleanExtra(AgentNotifier.EXTRA_WAIT, false)
        val runId = intent.getStringExtra(AgentNotifier.EXTRA_RUN_ID)
        val agentId = intent.getStringExtra(AgentNotifier.EXTRA_AGENT_ID)
        dispatch(ctx) {
            if (wait && requestId >= 0) {
                RuntimeBridge.resolveNotificationOutcome(requestId, runId, "replied", text = text)
            } else {
                RuntimeBridge.submitFromNotification(text, runId)
            }
            agentId?.let { AgentNotifier.cancelRegistered(ctx, it) }
        }
    }

    private fun handleDelete(ctx: Context, intent: Intent) {
        val requestId = intent.getLongExtra(AgentNotifier.EXTRA_REQUEST_ID, -1L)
        if (!intent.getBooleanExtra(AgentNotifier.EXTRA_WAIT, false) || requestId < 0) {
            return // 非 wait 划掉：无操作
        }
        val runId = intent.getStringExtra(AgentNotifier.EXTRA_RUN_ID)
        val agentId = intent.getStringExtra(AgentNotifier.EXTRA_AGENT_ID)
        // 通知本体已由用户移除，仅除名（不重复 cancel）。
        agentId?.let { AgentNotifier.unregister(it) }
        dispatch(ctx) {
            RuntimeBridge.resolveNotificationOutcome(requestId, runId, "dismissed")
        }
    }

    private fun handleAskUserDecline(ctx: Context, intent: Intent) {
        val requestId = intent.getLongExtra(AgentNotifier.EXTRA_REQUEST_ID, -1L)
        if (requestId < 0) return
        dispatch(ctx) {
            // decline 双通路（live resolve declined / 恢复卡 question_reply decline）
            // 由 declineQuestionCard 内部按 restored 分流；卡不在 pending 相位时 no-op。
            RuntimeBridge.declineQuestionCard(requestId)
        }
    }

    private companion object {
        private const val TAG = "OpenSlateNotifyRx"
    }
}
