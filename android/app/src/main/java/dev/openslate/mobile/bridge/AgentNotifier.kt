package dev.openslate.mobile.bridge

import android.Manifest
import android.app.Activity
import android.app.Application
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.RemoteInput
import androidx.core.content.ContextCompat
import dev.openslate.mobile.MainActivity
import dev.openslate.mobile.R
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * notify 工具的 Android 通知执行器（object 单例，提案 notify-tool-design.md §5）。
 *
 * - 三渠道幂等注册（§5.2）：`agent_messages`（DEFAULT）/ `agent_important`
 *   （HIGH）/ `agent_progress`（LOW），与 AgentService 的 agent_runtime
 *   （FGS 1001）并存互不触碰；
 * - post/update（同 tag,id 原生覆盖）/ cancel / cancelAllRegistered /
 *   settleOngoing（turn 终态防僵尸常驻，§4.6；wait 挂起不参与）；
 * - agent-tag ↔ (tag,id) 登记表（§5.5：tag = "an-"+sanitize、id =
 *   stableHash32 纯函数派生，重启后同 id 仍能命中）；**禁用
 *   NotificationManager.cancelAll()**——会连 AgentService FGS 常驻通知（1001）
 *   一起取消，cancel(all) 只枚举本进程登记表逐条 cancel；
 * - 权限/渠道可用性检测（API 33 `POST_NOTIFICATIONS` 运行时授予 +
 *   `areNotificationsEnabled` + 渠道 `IMPORTANCE_NONE`）；
 * - App 前后台跟踪（ask_user 伴生唤回 timer 的门控，§4.8）；
 * - ask_user 伴生唤回通知入口（固定 id `ask-user-nudge`，**不入工具登记表**，
 *   与 notify 工具域解耦）。
 *
 * 业务校验/限额全部在 Rust 侧发信封前完成；本层只做防御性兜底（§5.6：
 * 不信任入参，长度再截、枚举再滤、异常兜 posted:false）。
 */
object AgentNotifier {

    private const val TAG = "OpenSlateNotify"

    /** 通知小图标：复用 AgentService FGS 通知现有 drawable 约定（§5.3）。 */
    private const val SMALL_ICON = android.R.drawable.stat_notify_chat

    /** agent 通知 tag 前缀（§5.5）。 */
    private const val TAG_PREFIX = "an-"

    /** ask_user 伴生唤回通知的固定 agent id（§4.8，同问卷覆写复用）。 */
    private const val COMPANION_AGENT_ID = "ask-user-nudge"

    /**
     * 活跃通知登记上限（提案决策 #7）：post 新 id 且登记数达到上限时，按插入序
     * 自动 cancel 最早的非 ongoing、非 waitPending 条目（跳过 ongoing 防打断
     * 常驻任务展示，跳过 waitPending 防打断等待回填）；全部不可淘汰时放行
     * （Rust 侧 12 条/分钟限流闸已兜底）。
     */
    private const val MAX_ACTIVE_NOTIFICATIONS = 16

    // ── receiver / PendingIntent 契约（NotificationActionReceiver 共用）──

    const val ACTION_BUTTON = "dev.openslate.mobile.NOTIFY_BUTTON"
    const val ACTION_REPLY = "dev.openslate.mobile.NOTIFY_REPLY"
    const val ACTION_DELETE = "dev.openslate.mobile.NOTIFY_DELETE"
    const val ACTION_ASK_USER_DECLINE = "dev.openslate.mobile.NOTIFY_ASK_USER_DECLINE"

    /** PendingIntent extras（§5.4/§7.5）：agent_id / run_id / request_id / wait。 */
    const val EXTRA_AGENT_ID = "notify_agent_id"
    const val EXTRA_RUN_ID = "notify_run_id"
    const val EXTRA_REQUEST_ID = "notify_request_id"
    const val EXTRA_WAIT = "notify_wait"
    const val EXTRA_LABEL = "notify_label"
    const val EXTRA_VALUE = "notify_value"
    const val EXTRA_BEHAVIOR = "notify_behavior"
    /** MainActivity 深链标记：导航到 question-fullscreen（伴生唤回「去回答」）。 */
    const val EXTRA_OPEN_ASK_USER = "notify_open_ask_user"

    /** RemoteInput 内联回复取文本 key。 */
    const val REMOTE_INPUT_KEY = "notify_reply_text"

    // ── 登记表（agent-tag ↔ tag,id + 重建快照）─────────────────────────

    /** notify 工具通知的解析投影（RuntimeBridge.handleNotify 从信封 args 装配）。 */
    internal data class NotificationSpec(
        val agentId: String,
        val title: String,
        val message: String,
        /** normal | important | progress（渠道映射见 [channelFor]）。 */
        val kind: String,
        /** 仅 progress：0..100 确定进度（null = 未给）。 */
        val progress: Int? = null,
        val progressIndeterminate: Boolean = false,
        val ongoing: Boolean = false,
        val completed: Boolean = false,
        val autoCancel: Boolean = true,
        val publicVisible: Boolean = false,
        val wait: Boolean = false,
        /** 发起 host call id（wait 回填路由用；wait=false 时无意义）。 */
        val requestId: Long = -1L,
        /** D9：发起时活动 run（信封 args.run_id 内嵌，点击/迟到回复路由用）。 */
        val runId: String? = null,
        val actions: List<ActionSpec> = emptyList(),
    )

    internal data class ActionSpec(
        val label: String,
        /** 缺省由消费方补「用户在通知上点了「label」」（Rust 侧已归一化，兜底）。 */
        val value: String?,
        /** default | positive | destructive（仅 UI 语义；NotificationCompat 无
         *  对应 API，不参与构建）。 */
        val style: String,
        /** cancel_turn：立即停止当前回合（§4.4）。 */
        val behavior: String?,
        val reply: Boolean,
        val placeholder: String?,
    )

    /** 登记表条目：settleOngoing 重建 / cancel 枚举 / wait 关卷用。 */
    internal data class RegisteredEntry(
        val spec: NotificationSpec,
        val tag: String,
        val notifId: Int,
        /** 落地后的常驻位（spec.ongoing 经 progress 终态消解）。 */
        val ongoing: Boolean,
        /** wait 模式挂起中：不参与 settleOngoing（其 ongoing 属等待语义，§4.6）。 */
        val waitPending: Boolean,
    )

    internal sealed interface PostResult {
        data class Posted(val channel: String) : PostResult
        object PermissionDenied : PostResult
        data class ChannelDisabled(val channel: String) : PostResult
    }

    /** cancel 的 wait 关卷信息（requestId 非 null = wait 挂起，需在途 resolve 或迟到上行）。 */
    internal data class CancelledEntry(val agentId: String, val requestId: Long?, val runId: String?)

    private val registry = java.util.concurrent.ConcurrentHashMap<String, RegisteredEntry>()

    /**
     * 登记表的插入序（agentId；仅 post 新 id 时 append，更新不重排）。淘汰
     * （§6 / 决策 #7）按此序找最早可淘汰条目；cancelRegistered / unregister /
     * cancelAllRegistered 同步移除。COW 适合 16 上限内读多写少 + 迭代快照安全。
     */
    private val insertionOrder = java.util.concurrent.CopyOnWriteArrayList<String>()

    private val autoSeq = java.util.concurrent.atomic.AtomicLong(0)

    /** Rust 缺省自动生成一次性 id 的兜底（正常信封恒带 id，此处仅防御）。 */
    internal fun nextAutoId(): String = "auto-${autoSeq.incrementAndGet()}"

    // ── 渠道（§5.2）────────────────────────────────────────────────────

    private fun nm(ctx: Context): NotificationManager =
        ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

    /** 幂等创建三渠道（minSdk 26，NotificationChannel 恒可用）。 */
    private fun ensureChannels(ctx: Context) {
        val nm = nm(ctx)
        listOf(
            Triple(ctx.getString(R.string.agent_channel_messages), "Agent 消息", NotificationManager.IMPORTANCE_DEFAULT),
            Triple(ctx.getString(R.string.agent_channel_important), "Agent 重要提醒", NotificationManager.IMPORTANCE_HIGH),
            Triple(ctx.getString(R.string.agent_channel_progress), "Agent 任务进度", NotificationManager.IMPORTANCE_LOW),
        ).forEach { (id, name, importance) ->
            nm.createNotificationChannel(
                NotificationChannel(id, name, importance).apply {
                    // 渠道级锁屏默认隐藏（§5.2）；public:true 用通知级 VISIBILITY_PUBLIC 覆盖
                    lockscreenVisibility = Notification.VISIBILITY_PRIVATE
                },
            )
        }
    }

    /** 渠道选择按 kind（normal/important/progress）。 */
    internal fun channelFor(ctx: Context, kind: String): String = when (kind) {
        "important" -> ctx.getString(R.string.agent_channel_important)
        "progress" -> ctx.getString(R.string.agent_channel_progress)
        else -> ctx.getString(R.string.agent_channel_messages)
    }

    /** 通知权限：API 33+ 需 POST_NOTIFICATIONS 运行时授予；全系统开关另查 areNotificationsEnabled。 */
    internal fun areNotificationsEnabled(ctx: Context): Boolean {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            ContextCompat.checkSelfPermission(ctx, Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            return false
        }
        return nm(ctx).areNotificationsEnabled()
    }

    /** 渠道被用户单独关闭（IMPORTANCE_NONE）→ 如实回 channel_disabled。 */
    internal fun isChannelEnabled(ctx: Context, channelId: String): Boolean {
        val ch = nm(ctx).getNotificationChannel(channelId) ?: return true
        return ch.importance != NotificationManager.IMPORTANCE_NONE
    }

    // ── 寻址（§5.5 纯函数稳定派生）────────────────────────────────────

    private fun tagFor(agentId: String): String =
        TAG_PREFIX + agentId.replace(Regex("[^A-Za-z0-9_-]"), "_").take(48)

    internal fun stableHash32(s: String): Int = if (s.isEmpty()) 0 else s.hashCode()

    // ── post / update（同 tag,id 覆盖 = Android 原生替换语义，§4.3）────

    internal fun post(ctx: Context, spec: NotificationSpec): PostResult {
        if (!areNotificationsEnabled(ctx)) return PostResult.PermissionDenied
        ensureChannels(ctx)
        val channel = channelFor(ctx, spec.kind)
        if (!isChannelEnabled(ctx, channel)) return PostResult.ChannelDisabled(channel)
        val tag = tagFor(spec.agentId)
        val notifId = stableHash32(spec.agentId)
        val update = registry.containsKey(spec.agentId)
        // 新 id 且登记满员：按插入序淘汰最早可淘汰条目（决策 #7；全不可淘汰时放行）。
        if (!update && registry.size >= MAX_ACTIVE_NOTIFICATIONS) {
            evictOldestEvictable(ctx)
        }
        val ongoing = spec.ongoing && !(spec.kind == "progress" && spec.completed)
        nm(ctx).notify(tag, notifId, buildNotification(ctx, spec, update))
        registry[spec.agentId] = RegisteredEntry(spec, tag, notifId, ongoing, spec.wait)
        if (!update) {
            insertionOrder.remove(spec.agentId) // 防御：清可能的插入序残留
            insertionOrder.add(spec.agentId)
        }
        Log.i(
            TAG,
            "posted ${spec.agentId} (kind=${spec.kind}, channel=$channel, update=$update," +
                " ongoing=$ongoing, wait=${spec.wait}, actions=${spec.actions.size})",
        )
        return PostResult.Posted(channel)
    }

    private fun buildNotification(ctx: Context, spec: NotificationSpec, update: Boolean): Notification {
        val channel = channelFor(ctx, spec.kind)
        val ongoing = spec.ongoing && !(spec.kind == "progress" && spec.completed)
        val builder = NotificationCompat.Builder(ctx, channel)
            .setSmallIcon(SMALL_ICON)
            .setContentTitle(spec.title)
            .setContentText(spec.message)
            // 长文自动展开（§1 #3）：折叠态截断，展开态全文。
            .setStyle(NotificationCompat.BigTextStyle().bigText(spec.message))
            // 同 id 更新不重复响铃（§4.3）。
            .setOnlyAlertOnce(update)
            .setOngoing(ongoing)
            .setAutoCancel(spec.autoCancel && !ongoing)
            // 锁屏默认隐藏内容；public:true 通知级覆盖（§5.2）。
            .setVisibility(
                if (spec.publicVisible) NotificationCompat.VISIBILITY_PUBLIC
                else NotificationCompat.VISIBILITY_PRIVATE,
            )
            // 点击 → MainActivity + runId 路由（§4.5）。
            .setContentIntent(activityIntent(ctx, "content:${spec.agentId}", spec.runId, openAskUser = false))
        if (spec.kind == "progress") {
            when {
                // 进度终态：去进度条（§1 #6）。
                spec.completed -> builder.setProgress(0, 0, false)
                spec.progressIndeterminate -> builder.setProgress(0, 0, true)
                spec.progress != null -> builder.setProgress(100, spec.progress.coerceIn(0, 100), false)
            }
        }
        spec.actions.take(3).forEachIndexed { i, a -> builder.addAction(buildAction(ctx, spec, a, i)) }
        if (spec.wait) {
            // wait 挂起：划掉 → dismissed 回填（§4.2）；deleteIntent 用 MUTABLE（§7.4）。
            val seed = "delete:${spec.agentId}"
            builder.setDeleteIntent(
                PendingIntent.getBroadcast(
                    ctx, stableHash32(seed), receiverIntent(ctx, ACTION_DELETE, spec), pendingFlags(mutable = true),
                ),
            )
        }
        return builder.build()
    }

    private fun buildAction(ctx: Context, spec: NotificationSpec, a: ActionSpec, index: Int): NotificationCompat.Action {
        val seed = "action:${spec.agentId}:$index"
        return if (a.reply) {
            // 内联回复（RemoteInput 必须 FLAG_MUTABLE + 显式组件，§5.3）。
            val intent = receiverIntent(ctx, ACTION_REPLY, spec).putExtra(EXTRA_LABEL, a.label)
            val pi = PendingIntent.getBroadcast(ctx, stableHash32(seed), intent, pendingFlags(mutable = true))
            val remote = RemoteInput.Builder(REMOTE_INPUT_KEY).setLabel(a.placeholder ?: "回复").build()
            NotificationCompat.Action.Builder(0, a.label, pi).addRemoteInput(remote).build()
        } else {
            val intent = receiverIntent(ctx, ACTION_BUTTON, spec)
                .putExtra(EXTRA_LABEL, a.label)
                .apply {
                    a.value?.let { putExtra(EXTRA_VALUE, it) }
                    a.behavior?.let { putExtra(EXTRA_BEHAVIOR, it) }
                }
            val pi = PendingIntent.getBroadcast(ctx, stableHash32(seed), intent, pendingFlags(mutable = false))
            NotificationCompat.Action.Builder(0, a.label, pi).build()
        }
    }

    /** 显式组件 receiver intent（§5.4：agent_id / run_id / request_id / wait）。 */
    private fun receiverIntent(ctx: Context, action: String, spec: NotificationSpec): Intent =
        Intent(ctx, NotificationActionReceiver::class.java).apply {
            setAction(action)
            putExtra(EXTRA_AGENT_ID, spec.agentId)
            spec.runId?.let { putExtra(EXTRA_RUN_ID, it) }
            putExtra(EXTRA_REQUEST_ID, if (spec.wait) spec.requestId else -1L)
            putExtra(EXTRA_WAIT, spec.wait)
        }

    /** 显式组件 activity intent（contentIntent；FLAG_ACTIVITY_NEW_TASK|SINGLE_TOP，§5.3）。 */
    private fun activityIntent(ctx: Context, seed: String, runId: String?, openAskUser: Boolean): PendingIntent {
        val i = Intent(ctx, MainActivity::class.java)
            .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP)
        runId?.takeIf { it.isNotEmpty() }?.let { i.putExtra(EXTRA_RUN_ID, it) }
        if (openAskUser) i.putExtra(EXTRA_OPEN_ASK_USER, true)
        return PendingIntent.getActivity(ctx, stableHash32(seed), i, pendingFlags(mutable = false))
    }

    private fun pendingFlags(mutable: Boolean): Int =
        PendingIntent.FLAG_UPDATE_CURRENT or
            if (mutable) PendingIntent.FLAG_MUTABLE else PendingIntent.FLAG_IMMUTABLE

    // ── cancel（禁 cancelAll，只枚举登记表，§5.5）──────────────────────

    /**
     * 满员淘汰（决策 #7）：按插入序找首个 `!ongoing && !waitPending` 条目
     * cancelRegistered 淘汰。跳过 ongoing（打断常驻任务展示）与 waitPending
     * （打断等待回填）；顺手清理插入序中已不在登记表的残留。全部不可淘汰时
     * 放行本次 post——Rust 侧 12 条/分钟限流闸已兜底，瞬时超限可接受。
     */
    private fun evictOldestEvictable(ctx: Context) {
        for (agentId in insertionOrder.toList()) {
            val e = registry[agentId] ?: run {
                insertionOrder.remove(agentId)
                continue
            }
            if (!e.ongoing && !e.waitPending) {
                cancelRegistered(ctx, agentId)
                Log.i(TAG, "evicted $agentId (registry cap=$MAX_ACTIVE_NOTIFICATIONS reached)")
                return
            }
        }
        Log.i(TAG, "registry cap=$MAX_ACTIVE_NOTIFICATIONS reached but no entry evictable (all ongoing/waitPending); admitting")
    }

    internal fun cancelRegistered(ctx: Context, agentId: String): CancelledEntry? {
        val e = registry.remove(agentId) ?: return null
        insertionOrder.remove(agentId)
        nm(ctx).cancel(e.tag, e.notifId)
        Log.i(TAG, "cancelled $agentId (wait=${e.waitPending})")
        return e.toCancelled()
    }

    internal fun cancelAllRegistered(ctx: Context): List<CancelledEntry> {
        val entries = registry.values.toList()
        registry.clear()
        insertionOrder.clear()
        val nm = nm(ctx)
        return entries.map { e ->
            nm.cancel(e.tag, e.notifId)
            e.toCancelled()
        }.also { Log.i(TAG, "cancel all: ${it.size} agent notification(s)") }
    }

    /** 仅除名不 cancel OS 通知（deleteIntent 已发生，通知本体已由用户移除）。 */
    internal fun unregister(agentId: String) {
        registry.remove(agentId)
        insertionOrder.remove(agentId)
    }

    private fun RegisteredEntry.toCancelled() = CancelledEntry(
        agentId = spec.agentId,
        requestId = if (waitPending) spec.requestId else null,
        runId = spec.runId,
    )

    // ── settleOngoing（§4.6：turn 终态防僵尸常驻；wait 挂起不参与）────

    internal fun settleOngoing(ctx: Context) {
        for (e in registry.values.toList()) {
            if (!e.ongoing || e.waitPending) continue
            val rebuilt = e.spec.copy(ongoing = false, autoCancel = true)
            runCatching {
                nm(ctx).notify(e.tag, e.notifId, buildNotification(ctx, rebuilt, update = true))
                registry[e.spec.agentId] = e.copy(spec = rebuilt, ongoing = false)
            }.onFailure { Log.w(TAG, "settleOngoing: rebuild ${e.spec.agentId} failed", it) }
        }
    }

    // ── ask_user 伴生唤回（§4.8；固定 id，不入工具登记表）─────────────

    private val companionTag = tagFor(COMPANION_AGENT_ID)
    private val companionId = stableHash32(COMPANION_AGENT_ID)

    /**
     * 伴生唤回通知：agent_messages 渠道、固定 id（同问卷覆写复用）。
     * 「去回答」= contentIntent 同源深链（MainActivity → question-fullscreen）；
     * 「不回答」= receiver → RuntimeBridge.declineQuestionCard（live → resolve
     * declined；恢复卡 → question_reply decline，函数内双通路）。
     */
    internal fun postAskUserCompanion(ctx: Context, requestId: Long, summary: String) {
        if (!areNotificationsEnabled(ctx)) return
        ensureChannels(ctx)
        val channel = ctx.getString(R.string.agent_channel_messages)
        val open = activityIntent(ctx, "ask-user-nudge", null, openAskUser = true)
        val decline = PendingIntent.getBroadcast(
            ctx, stableHash32("ask-user-nudge:decline"),
            Intent(ctx, NotificationActionReceiver::class.java)
                .setAction(ACTION_ASK_USER_DECLINE)
                .putExtra(EXTRA_REQUEST_ID, requestId),
            pendingFlags(mutable = false),
        )
        val n = NotificationCompat.Builder(ctx, channel)
            .setSmallIcon(SMALL_ICON)
            .setContentTitle("Agent 在等你回答")
            .setContentText(summary)
            .setStyle(NotificationCompat.BigTextStyle().bigText(summary))
            .setAutoCancel(true)
            .setVisibility(NotificationCompat.VISIBILITY_PRIVATE)
            .setContentIntent(open)
            .addAction(NotificationCompat.Action.Builder(0, "去回答", open).build())
            .addAction(NotificationCompat.Action.Builder(0, "不回答", decline).build())
            .build()
        nm(ctx).notify(companionTag, companionId, n)
        Log.i(TAG, "ask_user companion nudge posted (request=$requestId)")
    }

    internal fun cancelCompanionNudge(ctx: Context) {
        nm(ctx).cancel(companionTag, companionId)
    }

    // ── App 前后台跟踪（伴生唤回 timer 门控 + 回前台撤通知，§4.8）────

    private val _inForeground = MutableStateFlow(false)
    val inForeground: StateFlow<Boolean> = _inForeground.asStateFlow()

    fun isAppInForeground(): Boolean = _inForeground.value

    @Volatile private var trackingRegistered = false
    private val foregroundActivities = java.util.concurrent.atomic.AtomicInteger(0)

    /** 注册 Activity 生命周期跟踪（幂等；RuntimeBridge.start 时调用）。 */
    fun init(appContext: Context) {
        if (trackingRegistered) return
        synchronized(this) {
            if (trackingRegistered) return
            val app = appContext.applicationContext as? Application ?: run {
                Log.w(TAG, "init: applicationContext is not an Application; foreground tracking disabled")
                return
            }
            app.registerActivityLifecycleCallbacks(object : Application.ActivityLifecycleCallbacks {
                override fun onActivityStarted(activity: Activity) {
                    if (foregroundActivities.incrementAndGet() == 1) _inForeground.value = true
                }

                override fun onActivityStopped(activity: Activity) {
                    if (foregroundActivities.decrementAndGet() == 0) _inForeground.value = false
                }

                override fun onActivityCreated(activity: Activity, savedInstanceState: Bundle?) {}
                override fun onActivityResumed(activity: Activity) {}
                override fun onActivityPaused(activity: Activity) {}
                override fun onActivitySaveInstanceState(activity: Activity, outState: Bundle) {}
                override fun onActivityDestroyed(activity: Activity) {}
            })
            trackingRegistered = true
        }
    }
}
