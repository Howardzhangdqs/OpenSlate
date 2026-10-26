package dev.openslate.mobile.ui

import android.app.Activity
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.Settings
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.TermuxDiagnostics
import dev.openslate.mobile.bridge.TermuxExec
import kotlin.coroutines.coroutineContext
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import java.util.concurrent.atomic.AtomicBoolean

/**
 * MCP 五灯探测管线（设置页 MCP 区与 composer 聚焦面板 Termux 子面板
 * 共用的唯一口径）：
 *
 * - [MCP_LIGHT_KEYS]：五灯 key 及检测序（termuxInstalled → termuxPerm →
 *   termuxAllowed → hostInstalled → hostAlive）；
 * - [probeMcpLights]：设置页 refresh() 原始探测序的提炼（顺序 / 短路 /
 *   语义逐字保持），渐进 emit 供两处消费方做过程显示；
 * - [McpLightsResult] / [mcpLightRow]：探测结果 → [StatusCheckRow] 的
 *   一灯一行映射（语义源唯一，设置页与子面板共用；含 restartingHost
 *   拉起窗口的「MCP host 启动中…」过渡态分支）；
 * - [McpLightsTracker]：子面板数据源（CAS 单飞探测，[McpLightsUiState]
 *   快照 + probing 持有；[McpLightsTracker.gateRefresh] 提供与设置页
 *   同款的 ON_RESUME/展开智能门控）。
 *
 * 与旧 detectTermuxStatus 四行阶梯的本质区别：这里五项独立判定成败、
 * 终态失败如实落 Failed（配置类通道问题不再映射成永不失败的
 * 「等待中」）、「打开 Termux」按钮只在通道无应答（NoResponse）时出现
 * ——与设置页 MCP 区的判定与指引时机完全一致。
 */

/** 五个状态灯的 key（渐进渲染的判定 + 摘要行进度统计共用；顺序 = 检测序）。
 *  整轮结束时短路路径未检测的项也一次性补齐，保证五灯语义完整。 */
internal val MCP_LIGHT_KEYS = listOf(
    "termuxInstalled", "termuxPerm", "termuxAllowed", "hostInstalled", "hostAlive",
)

/**
 * 一轮五灯探测的结果快照：五项独立布尔 + RUN_COMMAND 通道精确诊断 +
 * 本轮已完成的 key 集合（渐进渲染：未含 = 未检测/检测中）。
 *
 * @param restartedHost 探测时是否已尝试自动拉起 host（终态标记：hostAlive 行的
 *   失败摘要分支用——已尝试 →「稍候自动重检」，未尝试 →「到设置页重启」）。
 * @param restartingHost 「host 自动拉起进行中」过渡态（步骤 ⑦ 调 restartHost
 *   前置 true、收尾 emit / 最终返回归位 false）：拉起窗口内 hostAlive 仍为
 *   false，消费方据此把 hostAlive 行 / 首行状态显示为「MCP host 启动中…」
 *   而非误报未运行——与终态标记 restartedHost 语义区分（一个说「正在拉」，
 *   一个说「拉过了」）。
 */
internal data class McpLightsResult(
    val installed: Boolean = false,
    val perm: Boolean = false,
    val allowed: Boolean = false,
    /** RUN_COMMAND 通道精确诊断（未探测短路时为 null）。 */
    val channel: TermuxDiagnostics.ChannelResult? = null,
    val hostInstalled: Boolean = false,
    val hostAlive: Boolean = false,
    val checked: Set<String> = emptySet(),
    val restartedHost: Boolean = false,
    val restartingHost: Boolean = false,
)

/**
 * 五灯探测（设置页 refresh() 原始探测序的提炼，顺序/短路/语义逐字保持）：
 *
 * ① Termux 安装 + RUN_COMMAND 权限（PackageManager 查询，毫秒级）→ emit；
 * ② 短路：未装 / 无权限 → Termux 通道不可用，跳过通道/host 安装探测，
 *    直接探活服务端口（host 可能经 MCP 独立于 RUN_COMMAND 在跑）→ emit；
 * ③ 端口探活（独立 HTTP 通道）并行先发（不占 Termux 通道）；
 * ④ RUN_COMMAND 通道（ensureTermuxUp：1s 首探 → 配置类失败原样返回 /
 *    无应答非 stopped 静默重试 6s / stopped 不拉起如实返回；0.5s stall
 *    早 emit NoResponse 预显「打开 Termux」指引）→ emit；随后复查权限
 *    （前台拉起可能让部分 ROM 补全授权）；
 * ⑤ 通道放行才查 host 安装 → emit；
 * ⑥ 收并行探活结果 → emit；
 * ⑦ 放行 && host 已装 && 未运行 && [autoRestartHost] → **先 emit
 *    restartingHost=true 过渡态**（拉起窗口内消费方显示「MCP host 启动中…」
 *    而非误报未运行）→ restartHost + 复探回写 → emit（restartingHost
 *    归位 false）；
 * ⑧ 终态：checked 补齐五键 → 返回（restartingHost 恒 false）。
 *
 * @param autoRestartHost 步骤 ⑦ 的门控（设置页传 !busy：用户操作在途时
 *   不自动拉起；子面板 tracker 传 true）。
 * @param log 探测明细出口（设置页接诊断日志面板 + logcat 镜像；子面板
 *   tracker 直接进 logcat）。
 * @param onStage 每步完成时的渐进回调（emit 顺序见上），消费方据此做
 *   过程显示；回调在同协程内同步执行。
 */
internal suspend fun probeMcpLights(
    context: Context,
    autoRestartHost: Boolean,
    log: (String) -> Unit,
    onStage: (McpLightsResult) -> Unit,
): McpLightsResult {
    val appCtx = context.applicationContext
    // ── ① Termux 安装 + RUN_COMMAND 权限 ──
    val installed = TermuxExec.isTermuxInstalled(appCtx)
    val perm = TermuxExec.hasPermission(appCtx)
    log("Termux 已安装：$installed；RUN_COMMAND 权限：$perm")
    coroutineContext.ensureActive() // 被更新的触发取消 → 立即退出，不写状态
    var st = McpLightsResult(
        installed = installed,
        perm = perm,
        checked = setOf("termuxInstalled", "termuxPerm"),
    )
    onStage(st)
    var alive = false
    var restarted = false
    if (installed && perm) {
        coroutineScope {
            // ── ③ 端口探活并行先发（不占 Termux 通道）──
            val aliveD = async { McpHostManager.hostAliveDetail(appCtx) }

            // ── ④ RUN_COMMAND 通道（echo ok 往返 + 精确诊断）──
            // 独立成步：卡在这即通道问题（Termux 冷启动 / 被冻结解冻 / 放行未开）。
            val watcher = launch {
                delay(5000)
                log("仍在等待 RUN_COMMAND 应答…（Termux 冷启动或被系统冻结后首次命令较慢；" +
                    "常开 Termux 通知常驻 + Wakelock 并对其关闭电池优化可避免）")
            }
            val chan = TermuxDiagnostics.ensureTermuxUp(appCtx, log = log, onStall = {
                // 0.5s 无应答：不等整轮，先亮「打开 Termux」指引（复用
                // NoResponse 渲染路径）。这是早 emit 的显示态，探测流程
                // 不受影响；随后 ④ 的最终 emit 用真实结论覆盖。
                log("RUN_COMMAND 已 ${TermuxDiagnostics.PROBE_STALL_HINT_MS}ms 无应答——先显示「打开 Termux」指引，检测仍在后台继续…")
                st = st.copy(
                    channel = TermuxDiagnostics.ChannelResult.NoResponse(
                        stopped = TermuxDiagnostics.isTermuxStopped(appCtx), ms = 0,
                    ),
                    allowed = false,
                    checked = st.checked + "termuxAllowed",
                )
                onStage(st)
            })
            watcher.cancel()
            log("RUN_COMMAND 通道最终结果：${TermuxDiagnostics.describe(chan)}")
            val allowed = chan is TermuxDiagnostics.ChannelResult.Ok
            // 前台拉起可能让部分 ROM（MIUI 等）补全 RUN_COMMAND 授权：复查一次。
            val permNow = TermuxExec.hasPermission(appCtx)
            if (!perm && permNow) {
                log("Termux 启动后权限检查结果已更新：未授予 → 已授予")
            }
            coroutineContext.ensureActive()
            st = st.copy(perm = permNow, allowed = allowed, channel = chan,
                checked = st.checked + "termuxAllowed")
            onStage(st)

            // ── ⑤ host 安装（通道已热，秒回；未放行不探测）──
            val inst = if (allowed) McpHostManager.isHostInstalled(appCtx) else false
            log("host 已安装：$inst")
            coroutineContext.ensureActive()
            st = st.copy(hostInstalled = inst, checked = st.checked + "hostInstalled")
            onStage(st)

            // ── ⑥ 端口探活（并行已发出，此时收结果）──
            val (a, adetail) = aliveD.await()
            alive = a
            log("探活 ${McpHostManager.HOST_URL}：${if (a) "应答正常（$adetail）" else "无应答（$adetail）"}")
            coroutineContext.ensureActive()
            st = st.copy(hostAlive = alive, checked = st.checked + "hostAlive")
            onStage(st)

            // ── ⑦ host 已装但未运行 → 自动拉起（调用方门控）──
            if (allowed && inst && !alive && autoRestartHost) {
                log("服务未运行，自动拉起（停止旧进程 → nohup 后台启动）…")
                restarted = true
                // 拉起是秒级长窗口（停旧进程 + nohup 启动 + 复探）：先行
                // emit 过渡态 restartingHost=true，消费方把 hostAlive 行 /
                // 首行状态显示为「MCP host 启动中…」；收尾 emit 与最终
                // 返回一律归位 false。
                st = st.copy(restartingHost = true)
                onStage(st)
                runCatching { McpHostManager.restartHost(appCtx) { log(it) } }
                    .onSuccess {
                        val (a2, d2) = McpHostManager.hostAliveDetail(appCtx)
                        alive = a2
                        coroutineContext.ensureActive()
                        st = st.copy(hostAlive = alive, restartingHost = false)
                        onStage(st)
                        log(
                            if (a2) "服务已自动恢复（$d2），无需重装"
                            else "自动拉起后仍未应答（$d2）——可点「自动化操作（重新安装）」修复",
                        )
                    }
                    .onFailure {
                        log("自动拉起失败：${it.message?.take(160)}——可点「自动化操作（重新安装）」修复")
                        coroutineContext.ensureActive()
                        st = st.copy(restartingHost = false)
                        onStage(st)
                    }
            }
        }
    } else {
        // ── ② 短路：Termux 通道不可用 → 直接探活服务端口 ──
        val (a, adetail) = McpHostManager.hostAliveDetail(appCtx)
        alive = a
        log("探活 ${McpHostManager.HOST_URL}：${if (a) "应答正常（$adetail）" else "无应答（$adetail）"}")
        coroutineContext.ensureActive()
        st = st.copy(hostAlive = alive, checked = st.checked + "hostAlive")
        onStage(st)
    }
    coroutineContext.ensureActive() // 取消轮不落地：消费方不落终态/快照
    // ── ⑧ 终态：checked 补齐五键（短路路径未检测的项一次性补齐语义）；
    // restartingHost 恒归位 false（过渡态只在拉起窗口内存在）──
    return st.copy(
        hostAlive = alive,
        restartedHost = restarted,
        restartingHost = false,
        checked = MCP_LIGHT_KEYS.toSet(),
    )
}

/**
 * 子面板五灯 UI 状态：探测在途标记 + 最新结果快照。探测期间 result
 * 保留上轮结论（onStage 渐进回灌，过程不闪烁），probing 供首行状态
 * 显示「检测中…」与 gateRefresh 在途判定共用。
 */
internal data class McpLightsUiState(
    /** 最新探测结果；null = 尚未探测过（按 checked 空集渲染渐进行）。 */
    val result: McpLightsResult? = null,
    /** 探测在途（refresh CAS 抢到后置 true，finally 归 false）。 */
    val probing: Boolean = false,
)

/**
 * 子面板五灯数据源：单例 tracker，CAS 单飞探测（照 TermuxStallTracker
 * 范式——在途探测中重复 refresh 直接忽略），快照保留上轮结论（onStage
 * 渐进回灌，过程不闪烁；probing 单列于 [McpLightsUiState]）。autoRestartHost
 * 恒 true：子面板探测与设置页同语义——host 已装未运行时顺手自动拉起，
 * 重检即见恢复。探测明细进 logcat（OpenSlateDiag），不进任何 UI 日志面板。
 */
internal object McpLightsTracker {

    /** 单线程 IO：探测串行化，RUN_COMMAND 往返不与在途探测交错。 */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO.limitedParallelism(1))

    private val _state = MutableStateFlow(McpLightsUiState())

    /** 最新 UI 状态；result=null = 尚未探测过（首轮由 ChatScreen 初载/
     *  ON_RESUME 的 gateRefresh 触发；子面板手动展开亦经 gateRefresh，
     *  自动展开只读快照不重检），期间按 checked 空集渲染「正在检测…/
     *  待检测」渐进行）。 */
    val state: StateFlow<McpLightsUiState> = _state.asStateFlow()

    private val probing = AtomicBoolean(false)

    /** 触发一次探测（fire-and-forget）：在途探测中调用为无操作。抢到
     *  CAS 后置 probing=true（result 保留旧值不闪烁），onStage 渐进写
     *  .result，finally 置 probing=false。 */
    fun refresh(context: Context) {
        val appCtx = context.applicationContext
        if (!probing.compareAndSet(false, true)) return
        _state.value = _state.value.copy(probing = true)
        scope.launch {
            try {
                _state.value = _state.value.copy(
                    result = probeMcpLights(
                        appCtx,
                        autoRestartHost = true,
                        log = { android.util.Log.i("OpenSlateDiag", it) },
                        onStage = { st -> _state.value = _state.value.copy(result = st) },
                    ),
                )
            } finally {
                probing.set(false)
                _state.value = _state.value.copy(probing = false)
            }
        }
    }

    /**
     * 进页 / ON_RESUME / 子面板展开的智能门控（语义照抄设置页
     * McpSection 的 gateRefresh，适配到 tracker）：
     * - 无快照（result=null，冷启动首轮）→ 全量探测；
     * - 探测在途 → 门控不叠加（在途轮的结果本来就更新）；
     * - 快照非全绿（checked 未齐或五布尔任一 false）→ 直接全量重检：
     *   典型场景是用户按指引打开 Termux 后返回——Termux 刚复活时 host
     *   端口必然仍无应答（host 还没人重启），廉价比对看不见「Termux
     *   已活」这个变化，全量重检的步骤 ⑦ 会顺带把 host 自动拉起；
     * - 全绿快照 → 只做廉价变化探测（本机回环 HTTP ~ms 级，不碰
     *   Termux 通道），与快照 hostAlive 比对：一致 → 无动作；翻转 →
     *   全量重检（launch 内复查 probing 防竞态）。
     */
    fun gateRefresh(context: Context) {
        val snap = _state.value.result ?: run { refresh(context); return }
        if (probing.get()) return
        val snapGreen = MCP_LIGHT_KEYS.all { it in snap.checked } &&
            snap.installed && snap.perm && snap.allowed && snap.hostInstalled && snap.hostAlive
        if (!snapGreen) {
            android.util.Log.i(
                "OpenSlateDiag",
                "McpLightsTracker：上次检测有未通过项（如刚从 Termux 返回），自动全量重检…",
            )
            refresh(context)
            return
        }
        scope.launch {
            // 复查 probing：廉价探测期间可能已有全量轮/手动 refresh 启动，
            // 其结果本来就更新，本次翻转判定作废不叠加。
            if (probing.get()) return@launch
            val (alive, _) = McpHostManager.hostAliveDetail(context.applicationContext)
            if (probing.get()) return@launch
            if (alive == snap.hostAlive) return@launch
            android.util.Log.i(
                "OpenSlateDiag",
                "McpLightsTracker：服务状态变化（" +
                    "${if (snap.hostAlive) "运行中 → 未应答" else "未运行 → 已应答"}），自动重检…",
            )
            refresh(context)
        }
    }
}

/** 子面板动作集：三处卡点按钮的回调（null = 不出该按钮）。 */
internal data class McpLightActions(
    /** 「去安装」：开 Termux 的 F-Droid 详情页。 */
    val install: (() -> Unit)? = null,
    /** 「去授权」：系统权限弹窗（Activity 上下文）/ 应用详情页兜底。 */
    val grantPerm: (() -> Unit)? = null,
    /** 「打开 Termux」：拉起 Termux 应用（不自动切回，靠返回时重检闭环）。 */
    val openTermux: (() -> Unit)? = null,
)

/** 标准动作集（从 Context 构建三处卡点按钮，行为与设置页指引块一致）。 */
internal fun standardMcpLightActions(context: Context): McpLightActions = McpLightActions(
    install = {
        // Termux 不在 Play 上架：开 F-Droid 详情页（浏览器兜底；无浏览器
        // 静默失败不崩溃）。
        runCatching {
            context.startActivity(
                Intent(Intent.ACTION_VIEW, Uri.parse("https://f-droid.org/en/packages/com.termux/")),
            )
        }
    },
    grantPerm = {
        // 优先系统权限弹窗（Activity 上下文可达）；非 Activity 上下文兜底
        // 直达本应用详情页。
        val activity = context as? Activity
        if (activity != null) {
            activity.requestPermissions(arrayOf(TermuxExec.RUN_COMMAND_PERMISSION), 1002)
        } else {
            runCatching {
                context.startActivity(
                    Intent(
                        Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                        Uri.parse("package:${context.packageName}"),
                    ),
                )
            }
        }
    },
    openTermux = { McpHostManager.startTermuxApp(context.applicationContext) },
)

/** 检测中的行标题（InProgress：checked 序中第一个未完成的 key）。 */
private fun lightProgressTitle(key: String): String = when (key) {
    "termuxInstalled" -> "正在检测 Termux 安装…"
    "termuxPerm" -> "正在检测 RUN_COMMAND 权限…"
    "termuxAllowed" -> "正在检测 RUN_COMMAND 通道…"
    "hostInstalled" -> "正在检测 MCP 服务安装…"
    else -> "正在检测 MCP 服务…"
}

/** 待检测的行标题（Pending：checked 序中尚未轮到的 key）。 */
private fun lightPendingTitle(key: String): String = when (key) {
    "termuxInstalled" -> "Termux 安装 · 待检测"
    "termuxPerm" -> "RUN_COMMAND 权限 · 待检测"
    "termuxAllowed" -> "RUN_COMMAND 通道 · 待检测"
    "hostInstalled" -> "MCP 服务安装 · 待检测"
    else -> "MCP 服务 · 待检测"
}

/**
 * 五灯 → 检查清单行的共享映射（一灯一行，语义源唯一）：
 *
 * - **终态**（key ∈ result.checked）：Done/Failed 独立判定 + 状态化标题
 *   （「Termux 已安装 / Termux 未安装」——红叉不再配歧义标题）；
 * - **失败 + displayDetails**（子面板语境）：附引导摘要与卡点按钮——
 *   「打开 Termux」仅通道 NoResponse 时出现（stopped/冻结由 describe
 *   区分；配置类终态失败不误导等待）；host 安装/运行失败只给指引摘要，
 *   重流程（安装/重启）不在小面板触发；
 * - **未检测**（key ∉ checked）：displayDetails 时第一个未完成 key =
 *   InProgress「正在检测…」、其余 Pending「X · 待检测」；否则 null
 *   （设置页的渐进渲染由调用方「checked 命中才 add」承担，与本分支一致）。
 *
 * displayDetails=false（设置页）：只出 state + 状态化标题，不出
 * summary/action——设置页有自己的通道指引块与按钮排，避免重复。
 */
internal fun mcpLightRow(
    key: String,
    result: McpLightsResult,
    displayDetails: Boolean,
    actions: McpLightActions = McpLightActions(),
): StatusCheckRow? {
    if (key !in result.checked) {
        if (!displayDetails) return null
        // 第一个未完成的 key = 当前步骤（InProgress），其余待检测（Pending）。
        val firstUnchecked = MCP_LIGHT_KEYS.first { it !in result.checked }
        return if (key == firstUnchecked) {
            StatusCheckRow(StatusCheckState.InProgress, lightProgressTitle(key))
        } else {
            StatusCheckRow(StatusCheckState.Pending, lightPendingTitle(key))
        }
    }
    // hostAlive 拉起过渡态（步骤 ⑦ restartHost 前的先行 emit：探活仍未过
    // 且 restartingHost=true）→ InProgress「MCP host 启动中…」，不误报
    // 未运行；displayDetails true/false 两态都走此分支——设置页与子面板
    // 同享该过渡态。放在正常 ok/state 判定之前。
    if (key == "hostAlive" && !result.hostAlive && result.restartingHost) {
        return StatusCheckRow(StatusCheckState.InProgress, "MCP host 启动中…")
    }
    val ok = when (key) {
        "termuxInstalled" -> result.installed
        "termuxPerm" -> result.perm
        "termuxAllowed" -> result.allowed
        "hostInstalled" -> result.hostInstalled
        "hostAlive" -> result.hostAlive
        else -> return null
    }
    val state = statusCheckOf(ok)
    val title = when (key) {
        "termuxInstalled" -> if (ok) "Termux 已安装" else "Termux 未安装"
        "termuxPerm" -> if (ok) "RUN_COMMAND 权限已授予" else "RUN_COMMAND 权限未授予"
        "termuxAllowed" -> if (ok) "RUN_COMMAND 通道可用" else "RUN_COMMAND 通道未放行"
        "hostInstalled" -> if (ok) "MCP 服务已安装" else "MCP 服务未安装"
        else -> if (ok) "MCP 服务运行中" else "MCP 服务未运行"
    }
    if (!displayDetails || ok) return StatusCheckRow(state, title)
    // ── 失败 + 子面板语境：引导摘要 + 卡点按钮 ──
    var summary: String? = null
    var actionLabel: String? = null
    var onAction: (() -> Unit)? = null
    when (key) {
        "termuxInstalled" -> {
            summary = "bash 依赖 Termux；装好打开一次再回来"
            actionLabel = "去安装"
            onAction = actions.install
        }
        "termuxPerm" -> {
            actionLabel = "去授权"
            onAction = actions.grantPerm
        }
        "termuxAllowed" -> {
            val chan = result.channel
            summary = when (chan) {
                // 配置类终态失败：给指向性提示（完整命令指引在设置页 MCP 区）。
                is TermuxDiagnostics.ChannelResult.PolicyRejected ->
                    "需在 Termux 开启 allow-external-apps，完整指引见设置页 MCP 区"
                null -> null
                else -> TermuxDiagnostics.describe(chan)
            }
            // 「打开 Termux」只在无应答时出现（stopped/冻结两态由 describe
            // 区分）；startTermuxApp 不自动切回，靠返回时重检闭环——与设置
            // 页指引块同款时机。配置类失败（PolicyRejected/Rejected/
            // DispatchError）重试无用，不挂该按钮。
            if (chan is TermuxDiagnostics.ChannelResult.NoResponse) {
                actionLabel = "打开 Termux"
                onAction = actions.openTermux
            }
        }
        "hostInstalled" -> {
            summary = "到 设置 → MCP 区可一键安装/重装"
        }
        else -> {
            summary = if (result.restartedHost) {
                "已尝试自动拉起，稍候自动重检"
            } else {
                "可到设置页重启服务"
            }
        }
    }
    return StatusCheckRow(state, title, summary, actionLabel, onAction)
}
