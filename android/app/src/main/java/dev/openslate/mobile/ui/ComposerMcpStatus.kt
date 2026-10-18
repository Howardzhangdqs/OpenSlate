package dev.openslate.mobile.ui

import android.content.Context
import dev.openslate.mobile.bridge.McpHostManager
import dev.openslate.mobile.bridge.RuntimeBridge
import dev.openslate.mobile.bridge.TermuxDiagnostics
import dev.openslate.mobile.bridge.TermuxExec
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext

/**
 * composer 面板 MCP/Termux 状态显示的底层管线（纯数据，无 UI）：
 * 上提 Termux 检测阶梯（原 ToolManagementSection 私有定义，逻辑零变化，
 * private → internal），并新增实时快照 tracker
 * （[McpLiveSnapshot] / [ComposerMcpStatusTracker]）。
 */

/** Termux 依赖工具的可用状态；检测中/正常不产生警示标识（避免闪烁）。 */
internal sealed interface TermuxStatus {
    /** 检测进行中（含重入重置后的过渡态）：不显示任何标识。 */
    object Checking : TermuxStatus

    /** 通道 echo 往返通过：无需警示。 */
    object Ready : TermuxStatus

    /** Termux 未安装。 */
    object NotInstalled : TermuxStatus

    /** RUN_COMMAND 权限未授予。 */
    object NoPermission : TermuxStatus

    /** 通道探测失败；stopped = Termux 处于停止态（首装未打开/被强停）。 */
    data class ChannelDown(val summary: String, val stopped: Boolean) : TermuxStatus
}

/**
 * Termux 依赖工具的可用性检测阶梯（权威检测，无副作用、不自动拉起）：
 * **MCP host 健康（探活通过）→ 直接可用**（bash 经 MCP over HTTP 执行，
 * 与 RUN_COMMAND 无关）；host 不在线时才走 RUN_COMMAND 阶梯：未安装 →
 * 无权限 → `echo ok` 通道往返（通过 = RUN_COMMAND 兜底可用）。全程
 * IO 线程（PackageManager 查询、HTTP 探活与 RUN_COMMAND 派发都不该
 * 压主线程）。
 */
internal suspend fun detectTermuxStatus(
    context: Context,
    onStall: (() -> Unit)? = null,
): TermuxStatus =
    withContext(Dispatchers.IO) {
        // 主路径：MCP host 探活通过 → bash 走 MCP，RUN_COMMAND 无关紧要。
        val hostAlive = runCatching { McpHostManager.isHostAlive(context) }.getOrDefault(false)
        if (hostAlive) return@withContext TermuxStatus.Ready
        when {
            !TermuxExec.isTermuxInstalled(context) -> TermuxStatus.NotInstalled
            !TermuxExec.hasPermission(context) -> TermuxStatus.NoPermission
            else -> when (
                val r = TermuxDiagnostics.ensureTermuxUp(context, log = { }, onStall = onStall)
            ) {
                is TermuxDiagnostics.ChannelResult.Ok -> TermuxStatus.Ready
                is TermuxDiagnostics.ChannelResult.NoResponse ->
                    TermuxStatus.ChannelDown(TermuxDiagnostics.describe(r), stopped = r.stopped)
                else -> TermuxStatus.ChannelDown(TermuxDiagnostics.describe(r), stopped = false)
            }
        }
    }

/**
 * composer 面板 MCP/Termux 状态的一次探测快照。
 *
 * **口径**：App 只连接 1 个 MCP——Termux 里的聚合 host（HTTP）。因此
 * [connectedServers] 是 **App 清单（[servers.*]）配置、且当前在线供数的
 * 下游 server** 数量（对 host 发 tools/list，按工具名 `__` 前缀分组后
 * **再过滤到配置 slug 集**——host 内置 server 的裸名工具（bash /
 * read_file 等，无 `__` 各自成组）与未知前缀不计入：它们属于 Agent
 * 能力，已体现在 [agentTools]），**不是** App 发起的 MCP 连接数（恒
 * 1），也**不是**工具总数；[serverNames] 为对应下游名单（配置分组
 * 前缀）。仅 host 探活通过才可能有值：host 死 /
 * 未探测 / tools/list 自身失败时为 null（后者 [hostAlive] 仍为 true，
 * 可与 host 死区分）。
 *
 * 快照**非实时推送**：host 侧变化不会自动同步，只在
 * [ComposerMcpStatusTracker.refresh] 被调用时主动探测一次。
 *
 * @param probing 探测进行中（terminal 写入前恒 true；异常也会收敛，
 *   绝不永久卡 true）。
 * @param hostAlive host 探活结果；null = 尚未探测过。
 * @param hostDetail [McpHostManager.hostAliveDetail] 的诊断串（成功 =
 *   应答耗时；失败 = 超时 / HTTP 码 / 异常）。
 * @param agentTools 当前 Agent 可用工具总数（App 侧注册表，可见性过滤
 *   后的口径）；**与 [hostAlive] 相互独立**——host 死也有值（工具清单
 *   来自 FFI 注册表而非 host 探活）。null = 注册表不可用（运行时未
 *   就绪）；0 = 注册表可读但全被可见性过滤（含 visibility 拿不到的
 *   情况，视作全隐）。
 */
data class McpLiveSnapshot(
    val probing: Boolean = false,
    val hostAlive: Boolean? = null,        // null = 还没探过
    val hostDetail: String? = null,        // hostAliveDetail 的耗时/HTTP码/异常串
    val connectedServers: Int? = null,     // App 清单配置且在线供数的下游 server 数（内置裸名组已剔除）；host 死/未知 = null
    val serverNames: List<String> = emptyList(),
    val agentTools: Int? = null,           // 当前 Agent 可用工具总数（可见性过滤后）；null = 注册表不可用
)

/**
 * composer MCP 状态单例 tracker：持有最新 [McpLiveSnapshot]，供 UI 订阅。
 *
 * - [refresh] fire-and-forget：`hostAliveDetail` 探活 → 活着再
 *   `hostToolsByPrefix` 取下游分组（分组数 + 名单；此步自身失败则
 *   connectedServers/serverNames 置 null/空，但 hostAlive=true 保留）→
 *   一次性写终态；host 死则终态 `hostAlive=false, connectedServers=null`；
 * - agentTools 与 host 探活**独立**计算（[countAgentTools]，App 侧
 *   注册表同步 FFI 读取）：host 死的终态也带上值，仅注册表不可用
 *   （listTools null）为 null；异常兜底分支置 null；
 * - 幂等去重：probing 进行中重复 refresh 直接忽略（单飞，不叠加并发
 *   探测——CAS 抢占 probing=true，抢不到即返回）；
 * - 探测全程在单线程 IO 调度器串行执行（阻塞 HTTP 不压主线程，
 *   也不与在途探测交错）；object 单例作用域不随 UI 生命周期取消
 *   （无泄漏担忧）；
 * - 任何异常都收敛为终态（probing=false, hostAlive=false, 详情入
 *   hostDetail），绝不卡在探测中。
 */
object ComposerMcpStatusTracker {
    /** 单线程 IO：探测串行化，阻塞网络调用不压主线程。 */
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO.limitedParallelism(1))

    private val _state = MutableStateFlow(McpLiveSnapshot())

    /** 最新快照；初始值全默认（probing=false, hostAlive=null = 未探测）。 */
    val state: StateFlow<McpLiveSnapshot> = _state.asStateFlow()

    /** 触发一次探测（fire-and-forget）：在途探测中调用为无操作。 */
    fun refresh(context: Context) {
        val appCtx = context.applicationContext
        // CAS 抢占探测权：probing=true 抢到才发起新一轮；他人在探测中
        // （或本轮 CAS 败给并发对手后重读为 probing=true）→ 直接返回。
        while (true) {
            val cur = _state.value
            if (cur.probing) return
            if (_state.compareAndSet(cur, cur.copy(probing = true))) break
        }
        scope.launch {
            // agentTools 与 host 探活独立：注册表在 App 侧（FFI 同步
            // 读取），host 死也要有值；自身异常不牵连 host 探测。
            val agentTools = runCatching { countAgentTools() }.getOrNull()
            val terminal = try {
                val (alive, detail) = McpHostManager.hostAliveDetail(appCtx)
                if (alive) {
                    // host 活着才有意义：tools/list 取下游分组。此步自身
                    // 失败（null）只降级下游计数，不推翻探活结论。
                    // 口径过滤：只认 App 清单（[servers.*]）配置的 server——
                    // host 的 tools/list 还混有内置 server 的裸名工具（bash/
                    // read_file 等，无 `__`，各自成组）与未知前缀；它们属于
                    // Agent 能力（已体现在 agentTools），不计入「连了多少个
                    // MCP」，计数与名单一并剔除。
                    val configured = McpHostManager
                        .serversSlugMap(McpHostManager.loadServers(appCtx))
                        .map { it.second }.toSet()
                    val byPrefix = McpHostManager.hostToolsByPrefix(appCtx)
                        ?.filterKeys { it in configured }
                    McpLiveSnapshot(
                        probing = false,
                        hostAlive = true,
                        hostDetail = detail,
                        connectedServers = byPrefix?.size,
                        serverNames = byPrefix?.keys?.toList() ?: emptyList(),
                        agentTools = agentTools,
                    )
                } else {
                    McpLiveSnapshot(
                        probing = false,
                        hostAlive = false,
                        hostDetail = detail,
                        connectedServers = null,
                        serverNames = emptyList(),
                        agentTools = agentTools,
                    )
                }
            } catch (t: Throwable) {
                // 兜底：探活/取数理论上不抛（各自内部 runCatching），
                // 防御性收敛，绝不把 probing 卡在 true。
                McpLiveSnapshot(
                    probing = false,
                    hostAlive = false,
                    hostDetail = "异常: ${t.message ?: t.javaClass.simpleName}",
                    agentTools = null,
                )
            }
            // 单飞保证终态无并发写竞争，一次性落终态。
            _state.value = terminal
        }
    }

    /**
     * Agent 当前可用工具总数（可见性过滤后）：口径照搬旧
     * loadComposerPanelData 的 toolsVisible（HEAD 版 ComposerPanel.kt
     * 135-148 行）——listTools 为 null（运行时未就绪）→ 整体 null
     * （注册表不可用）；可见性判据与 ToolManagementSection.isVisible
     * 逐字同式（visibility 拿不到视作全隐 → 计 0，而非 null）。同步
     * FFI 调用，仅在本 tracker 的 IO scope 内执行。
     */
    private fun countAgentTools(): Int? {
        val tools = RuntimeBridge.listTools() ?: return null
        val vis = RuntimeBridge.getAgentToolVisibility()
        return tools.count { t ->
            vis != null && (vis.tools.isEmpty() || t.name in vis.tools) && t.name !in vis.hiddenTools
        }
    }
}
