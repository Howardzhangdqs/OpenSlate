package dev.openslate.mobile.bridge

import android.content.Context
import android.content.Intent
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import org.json.JSONArray
import org.json.JSONObject
import java.net.HttpURLConnection
import java.net.URL
import java.util.concurrent.atomic.AtomicLong

/**
 * MCP Host 管理（手机端 MCP 方案的 App 侧执行器）。
 *
 * 职责：
 * - "自动化操作"：assets 内嵌的 mcp-host（aarch64 musl 静态）经
 *   /sdcard/Download 中转安装到 Termux ~/.openslate/mcp-host；
 * - 生成并推送 mcp-host.toml 清单（token + 下游 server 列表）；
 * - 启动/重启 host（nohup 后台）；
 * - 探活（HTTP initialize → 127.0.0.1:8765/mcp）。
 *
 * 通道：RUN_COMMAND（TermuxExec）执行 Termux 侧命令；UI 专用 id 段与
 * host call 路由隔离。清单是小文本，直接 base64 内嵌命令写入（不经
 * /sdcard）；host 二进制太大走 /sdcard 中转（需 Termux 存储权限，
 * 即用户执行过一次 termux-setup-storage）。
 */
object McpHostManager {

    const val HOST_PORT = 8765
    const val HOST_URL = "http://127.0.0.1:$HOST_PORT/mcp"
    private const val PREF_SERVERS = "mcp_host_servers"
    /** SecretStore 中的固定 provider 键（token 加密持久化；不再明文入 prefs）。 */
    private const val SECRET_KEY_TOKEN = "mcp_host_token"
    private const val ASSET_HOST = "mcp-host-aarch64"

    /** UI 发起的 Termux 调用 id 段（host call 的 runtime id 从 0 递增）。 */
    private val uiCallIds = AtomicLong(900_000_000L)

    /**
     * 一个 MCP server 条目（清单 [servers.<slug>] 的来源）。
     *
     * @param name 人类可读显示名（中文/空格随意，只用于展示）。
     * @param slug 代号（工具名前缀）；null/空 = 自动从 name 派生。用户
     *   指定值同样会经 [serverSlug] sanitize——这是安全边界不是可选
     *   美化：坏值（含 `__` 等）会直接破坏 host 路由与清单键。
     */
    data class McpServerEntry(
        val name: String,
        val slug: String?,
        /** 完整启动命令行（如 "npx -y @modelcontextprotocol/server-filesystem /sdcard"）。 */
        val command: String,
    )

    private fun prefs(ctx: Context) =
        ctx.applicationContext.getSharedPreferences(AppPrefs.FILE_MAIN, Context.MODE_PRIVATE)

    /**
     * 共享 token：首次生成（随机 hex）经 SecretStore 加密持久化，同时用于
     * host 清单与 App 侧连接头。解密失败（Keystore 密钥变更/密文损坏）视为
     * 不可用 → 轮换新 token 并覆盖存储（清单每次推送都带当前 token，host
     * 重启后自愈）。加密持久化失败时本次仅内存生效，重启后同样轮换。
     */
    fun token(ctx: Context): String = synchronized(this) {
        val appCtx = ctx.applicationContext
        when (val saved = SecretStore.load(appCtx, SECRET_KEY_TOKEN)) {
            is SecretStore.KeyLoadResult.Ok -> saved.value
            else -> {
                if (saved is SecretStore.KeyLoadResult.Failed) {
                    android.util.Log.w(
                        "McpHost",
                        "token 解密失败，轮换新 token（${saved.error?.message ?: "unknown"}）",
                    )
                }
                val t = java.util.UUID.randomUUID().toString().replace("-", "") +
                    java.util.UUID.randomUUID().toString().replace("-", "")
                if (!SecretStore.save(appCtx, SECRET_KEY_TOKEN, t)) {
                    android.util.Log.w("McpHost", "token 加密持久化失败，仅内存生效（重启后轮换）")
                }
                t
            }
        }
    }

    // ── server 清单（UI 状态源；JSON 存 prefs）──────────────────────────

    fun loadServers(ctx: Context): List<McpServerEntry> {
        val raw = prefs(ctx).getString(PREF_SERVERS, null) ?: return emptyList()
        return runCatching {
            val arr = JSONArray(raw)
            (0 until arr.length()).map { i ->
                val o = arr.getJSONObject(i)
                // slug 可空：旧数据缺该键（或 null/空串）= 自动从 name 派生，
                // 存量 prefs 无需迁移。键名沿用历史的 "alias"（存的是显示
                // 名；改键名需迁移存量，无收益）。
                val slug = if (o.isNull("slug")) null
                else o.optString("slug").trim().takeIf { it.isNotEmpty() }
                McpServerEntry(o.getString("alias"), slug, o.getString("command"))
            }
        }.getOrDefault(emptyList())
    }

    fun saveServers(ctx: Context, servers: List<McpServerEntry>) {
        val arr = JSONArray()
        for (s in servers) {
            val o = JSONObject().put("alias", s.name).put("command", s.command)
            // 空串/空白不持久化（缺省 = 自动派生），别存空串污染数据。
            s.slug?.trim()?.takeIf { it.isNotEmpty() }?.let { o.put("slug", it) }
            arr.put(o)
        }
        prefs(ctx).edit().putString(PREF_SERVERS, arr.toString()).apply()
    }

    // ── Termux 通道辅助 ────────────────────────────────────────────────

    private suspend fun termux(ctx: Context, command: String): Pair<String?, String> =
        TermuxExec.run(ctx.applicationContext, uiCallIds.incrementAndGet(), command)

    /** 调试通道（DebugReceiver）：在 Termux 执行任意命令，结果进 logcat。 */
    suspend fun debugRun(ctx: Context, command: String): Pair<String?, String> =
        termux(ctx, command)

    /** 拉起 Termux 应用（前台可见）。Termux 进程被杀后经 RUN_COMMAND
     *  后台拉服务在部分 ROM（HyperOS 等）上很慢甚至不可靠，先把应用
     *  打开、让它完成前台初始化，再走命令通道最稳。 */
    fun startTermuxApp(ctx: Context): Boolean = runCatching {
        val intent = ctx.packageManager.getLaunchIntentForPackage(TermuxExec.TERMUX_PACKAGE)
            ?: return@runCatching false
        intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        ctx.startActivity(intent)
        true
    }.getOrDefault(false)

    /** Termux 就绪三要素：已装、有 RUN_COMMAND 权限、allow-external-apps。 */
    suspend fun checkTermux(ctx: Context): Triple<Boolean, Boolean, Boolean> {
        val installed = TermuxExec.isTermuxInstalled(ctx.applicationContext)
        val perm = TermuxExec.hasPermission(ctx.applicationContext)
        // Termux 处于 stopped 态时 service intent 不会执行 → 永无应答；
        // 上限 20s 截断 TermuxExec 的 90s 兜底，超时按未放行处理。
        val allowed = if (installed && perm) {
            withTimeoutOrNull(20_000) {
                val (out, err) = termux(ctx, "echo ok")
                out?.trim() == "ok" && err.isEmpty()
            } ?: false
        } else false
        return Triple(installed, perm, allowed)
    }

    /**
     * 单次 Termux 往返同时探测：放行（echo ok）+ host 已安装（test -x）。
     * 预热（无 UI）路径专用——往返次数最少；UI 状态刷新走分步检测
     * （TermuxDiagnostics / isHostInstalled），便于定位卡点。
     *
     * 返回 (allowed, hostInstalled, 耗时或错误说明)。
     */
    suspend fun probeTermux(ctx: Context): Triple<Boolean, Boolean, String> {
        val t0 = System.currentTimeMillis()
        val (out, err) = termux(
            ctx,
            "echo ok; test -x \$HOME/.openslate/mcp-host && echo host-installed",
        )
        val ms = System.currentTimeMillis() - t0
        val lines = out?.lines()?.map { it.trim() } ?: emptyList()
        val allowed = lines.contains("ok") && err.isEmpty()
        val hostInstalled = lines.contains("host-installed")
        val detail = if (out == null && err.isNotEmpty()) {
            "错误 ${err.take(120)}（${ms}ms）"
        } else {
            "${ms}ms"
        }
        return Triple(allowed, hostInstalled, detail)
    }

    /** host 二进制是否已安装到 Termux（可执行）。 */
    suspend fun isHostInstalled(ctx: Context): Boolean {
        val (out, _) = termux(ctx, "test -x \$HOME/.openslate/mcp-host && echo yes || echo no")
        return out?.trim() == "yes"
    }

    /**
     * 探测一批命令名在 Termux 中是否可用（command -v）。
     * 返回「不存在」的命令集合；通道异常/超时返回空集（不误报：探测失败
     * 不等于命令缺失，宁可不警示）。
     *
     * 单次 RUN_COMMAND 往返探测全部命令（shell for 循环逐个 echo 结果），
     * 避免每命令一次往返拖慢状态刷新；名字含引号/反斜杠/空白的 token 无法
     * 安全嵌入单行 shell，直接跳过不探测（可能性极低，不值得为它引入
     * 转义复杂度）。
     */
    suspend fun probeCommandAvailable(ctx: Context, commands: List<String>): Set<String> =
        withContext(Dispatchers.IO) {
            val tokens = commands.map { it.trim() }.filter {
                it.isNotEmpty() && !it.contains('"') && !it.contains('\\') && !it.contains(' ')
            }.distinct()
            if (tokens.isEmpty()) return@withContext emptySet()
            val quoted = tokens.joinToString(" ") { "\"$it\"" }
            val cmd = "for c in $quoted; do " +
                "command -v \"\$c\" >/dev/null 2>&1 && echo \"ok|\$c\" || echo \"missing|\$c\"; done"
            // 照 checkTermux：20s 截断 TermuxExec 的 90s 兜底；超时按探测
            // 失败处理（空集），「缺失」结论只在通道正常应答时给出。
            withTimeoutOrNull(20_000) {
                val (out, err) = termux(ctx, cmd)
                if (out == null || err.isNotEmpty()) {
                    emptySet()
                } else {
                    out.lineSequence()
                        .map { it.trim() }
                        .filter { it.startsWith("missing|") }
                        .map { it.removePrefix("missing|") }
                        .toSet()
                }
            } ?: emptySet()
        }

    /** host 进程是否存活 + 端口应答（带 token 的 initialize）。 */
    suspend fun isHostAlive(ctx: Context): Boolean = hostAliveDetail(ctx).first

    /**
     * 后台预热（app 启动时调用，fire-and-forget）：配置了 MCP 服务器时
     * 提前拉起 Termux（把冷启动提前消化掉）；若 host 未运行则自动重启
     * 并开启常驻。全程日志进 logcat（OpenSlateDiag），UI 无感。
     */
    suspend fun prewarm(ctx: Context) = withContext(Dispatchers.IO) {
        val appCtx = ctx.applicationContext
        val tag = "OpenSlateDiag"
        try {
            val servers = loadServers(appCtx)
            if (servers.isEmpty() ||
                !TermuxExec.isTermuxInstalled(appCtx) ||
                !TermuxExec.hasPermission(appCtx)
            ) return@withContext
            android.util.Log.i(tag, "MCP 预热：拉起 Termux…")
            var (allowed, hostInstalled, detail) = probeTermux(appCtx)
            if (!allowed) {
                if (TermuxDiagnostics.isTermuxStopped(appCtx)) {
                    android.util.Log.i(
                        tag,
                        "MCP 预热：Termux 处于停止状态，后台无法唤醒，跳过（用户打开一次 Termux 后自愈）",
                    )
                    return@withContext
                }
                // 进程被杀但非 stopped：RUN_COMMAND 会自动冷启动，重试一次。
                val retry = probeTermux(appCtx)
                allowed = retry.first
                hostInstalled = retry.second
                detail = retry.third
            }
            if (!allowed || !hostInstalled) {
                android.util.Log.i(tag, "MCP 预热：未就绪（allowed=$allowed host=$hostInstalled $detail），跳过")
                return@withContext
            }
            val (alive, _) = hostAliveDetail(appCtx)
            if (alive) {
                android.util.Log.i(tag, "MCP 预热：Termux 与 host 均在运行（$detail），无需处理")
                return@withContext
            }
            android.util.Log.i(tag, "MCP 预热：host 未运行，自动重启…")
            restartHost(appCtx) { android.util.Log.i(tag, "MCP 预热｜$it") }
            android.util.Log.i(tag, "MCP 预热：host 已恢复")
        } catch (t: Throwable) {
            android.util.Log.i(tag, "MCP 预热失败：${t.message}")
        }
    }

    /** 探活并带回诊断：成功 = 应答耗时；失败 = 原因（超时 / HTTP 码 /
     *  异常），供诊断日志展示"卡在哪"。 */
    suspend fun hostAliveDetail(ctx: Context): Pair<Boolean, String?> =
        withContext(Dispatchers.IO) {
            val t0 = System.currentTimeMillis()
            runCatching {
                val conn = URL(HOST_URL).openConnection() as HttpURLConnection
                conn.connectTimeout = 1500
                conn.readTimeout = 1500
                conn.requestMethod = "POST"
                conn.doOutput = true
                conn.setRequestProperty("Content-Type", "application/json")
                conn.setRequestProperty("Accept", "application/json, text/event-stream")
                conn.setRequestProperty("Authorization", "Bearer ${token(ctx)}")
                val body = """{"jsonrpc":"2.0","id":1,"method":"initialize","params":""" +
                    """{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}"""
                conn.outputStream.use { it.write(body.toByteArray()) }
                val code = conn.responseCode
                conn.disconnect()
                val ok = code == 200
                ok to if (ok) {
                    "${System.currentTimeMillis() - t0}ms"
                } else {
                    "HTTP $code"
                }
            }.getOrElse { e -> false to "${e.javaClass.simpleName}: ${e.message ?: "无详细信息"}" }
        }

    /**
     * 向 host 发 JSON-RPC tools/list，按工具名第一个 __ 前的前缀分组计数。
     * host 不可达 / 响应异常 → null（与「host 运行但该 server 无工具」区分：
     * null = 探测本身失败，空 map / 缺某前缀 = host 明确没有该 server 的工具）。
     */
    suspend fun hostToolsByPrefix(ctx: Context): Map<String, Int>? =
        withContext(Dispatchers.IO) {
            val text = runCatching {
                val conn = URL(HOST_URL).openConnection() as HttpURLConnection
                // connectTimeout 保持 1500：host 是本机回环，活着就必须秒连，
                // 连不上说明进程死了，等再久也没用。
                // readTimeout 放宽到 15s：tools/list 会触发下游懒 spawn
                // （downstream.rs peer() 慢路径），npx 冷启动（node 引导 +
                // 包解析）在手机上轻松超 1.5s——旧值导致读超时 → null →
                // 「状态未知」（实验：4s 握手下游 → 4.02s 应答仍 200+JSON）。
                conn.connectTimeout = 1500
                conn.readTimeout = 15_000
                conn.requestMethod = "POST"
                conn.doOutput = true
                conn.setRequestProperty("Content-Type", "application/json")
                conn.setRequestProperty("Accept", "application/json, text/event-stream")
                conn.setRequestProperty("Authorization", "Bearer ${token(ctx)}")
                val body = """{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"""
                conn.outputStream.use { it.write(body.toByteArray()) }
                val code = conn.responseCode
                if (code != 200) {
                    conn.disconnect()
                    return@runCatching null
                }
                conn.inputStream.use { it.readBytes().toString(Charsets.UTF_8) }
            }.getOrNull()
            // 请求层失败（连接拒绝/超时/非 200）→ null：端口探活刚过就取不到，
            // 说明 host 行为异常，不能当「无工具」处理。
            text ?: return@withContext null
            runCatching { parseToolsByPrefix(text) }.getOrNull()
        }

    /**
     * 解析 tools/list 响应体 → 前缀 → 工具数。响应可能是纯 JSON 或 SSE
     * （`data: {...}` 行）——host 的 Content-Type 不作信任依据，先按 SSE
     * 逐行提取解析，失败再整串解析；两种都解析不出 → null（响应异常）。
     */
    private fun parseToolsByPrefix(text: String): Map<String, Int>? {
        for (line in text.lineSequence()) {
            if (!line.startsWith("data:")) continue
            val json = runCatching { JSONObject(line.removePrefix("data:").trim()) }.getOrNull() ?: continue
            val tools = json.optJSONObject("result")?.optJSONArray("tools") ?: continue
            return countToolsByPrefix(tools)
        }
        val tools = runCatching {
            JSONObject(text).getJSONObject("result").getJSONArray("tools")
        }.getOrNull() ?: return null
        return countToolsByPrefix(tools)
    }

    /** tools[] 按名字第一个 __ 前的前缀分组计数（无 __ 的名字自成一组）。 */
    private fun countToolsByPrefix(tools: JSONArray): Map<String, Int> {
        val counts = linkedMapOf<String, Int>()
        for (i in 0 until tools.length()) {
            val name = tools.optJSONObject(i)?.optString("name").orEmpty()
            if (name.isEmpty()) continue
            val prefix = name.substringBefore("__")
            counts[prefix] = (counts[prefix] ?: 0) + 1
        }
        return counts
    }

    // ── 安装流程（"自动化操作"）────────────────────────────────────────

    /**
     * 全自动安装：assets → /sdcard/Download 中转 → Termux 安装 → 推清单 →
     * 启动。返回逐步日志（失败时抛出并带步骤说明）。
     *
     * step 回调：name = 当前步骤名（驱动 UI"进行中 + 已耗时"），null = 结束。
     */
    suspend fun installHost(
        ctx: Context,
        log: (String) -> Unit,
        step: (String?) -> Unit = {},
    ): String = withContext(Dispatchers.IO) {
        try {
            step("拉起 Termux 应用")
            log("打开 Termux 完成初始化（会短暂切过去，约 2 秒后自动返回本页）…")
            if (TermuxDiagnostics.launchTermuxAndReturn(ctx)) {
                log("Termux 已拉起，继续后续步骤…")
            } else {
                log("无法拉起 Termux（可能未安装），改走命令通道…")
            }

            step("检查 Termux 连通")
            val t0 = System.currentTimeMillis()
            val (installed, perm, allowed) = checkTermux(ctx)
            log("Termux 连通（${System.currentTimeMillis() - t0}ms）：安装=$installed 权限=$perm 放行=$allowed")
            if (!installed) throw IllegalStateException("未安装 Termux")
            if (!perm) throw IllegalStateException(
                "缺少 RUN_COMMAND 权限（系统设置 → 应用 → OpenSlate → 权限；" +
                "部分系统需先打开一次 Termux 才能授予，也可用 Shizuku 一键授权）"
            )
            if (!allowed) throw IllegalStateException(
                if (TermuxDiagnostics.isTermuxStopped(ctx)) {
                    "Termux 处于停止状态（安装后从未打开或被强制停止）：" +
                        "请手动打开一次 Termux 完成初始化后重试"
                } else {
                    "Termux 未放行外部应用：在 ~/.termux/termux.properties 设置 " +
                        "allow-external-apps=true 后重启 Termux（首次安装需先打开一次 " +
                        "Termux 完成初始化，配置文件才会存在）"
                }
            )

            step("传输 host 二进制")
            log("经本机回环传输 host 二进制（零存储权限）…")
            transferHostOverTcp(ctx, log)

            step("写入 host 清单")
            log("写入 host 清单…")
            pushManifest(ctx)
            log("清单已写入")

            step("启动 host")
            log("启动 host…")
            restartHost(ctx, log)

            step("探活")
            log("探活…")
            val (alive, detail) = hostAliveDetail(ctx)
            if (!alive) throw IllegalStateException("host 已启动但端口未应答（$detail），查看 Termux 侧 ~/.openslate/host.log")
            log("探活通过（$detail）")
            step(null)
            "安装完成：$HOST_URL"
        } catch (e: Throwable) {
            step(null)
            throw e
        }
    }

    /**
     * assets → Termux 的 host 二进制传输（推荐路径：本机回环 TCP）。
     *
     * App 在 127.0.0.1 起一次性 ServerSocket，RUN_COMMAND 让 Termux 用
     * bash 内置 `/dev/tcp` 拉取并落盘 + chmod + 字节数校验：
     *
     * - 零存储权限（不经 /sdcard，无需 termux-setup-storage）；
     * - 零 Termux 侧依赖（不需要 curl/wget）；
     * - Android 各版本通吃（API 26 与分区存储后的 29+ 一视同仁）。
     *
     * 失败时错误信息带回；传输完成（或超时）后服务端自动关闭。
     */
    private suspend fun transferHostOverTcp(ctx: Context, log: (String) -> Unit) {
        val appCtx = ctx.applicationContext
        // 1) assets → App cache（私有目录，无需任何权限）。
        val tmp = java.io.File(appCtx.cacheDir, "mcp-host.stage")
        appCtx.assets.open(ASSET_HOST).use { input ->
            tmp.outputStream().use { input.copyTo(it) }
        }
        val size = tmp.length()

        // 2) 一次性回环监听 + 后台发送线程（termux() 等命令结束，发送在
        //    独立线程进行，无死锁：cat 直到流关闭才返回结果）。
        val server = java.net.ServerSocket(0, 1, java.net.InetAddress.getByName("127.0.0.1"))
        server.soTimeout = 60_000
        val port = server.localPort
        log("二进制 ${size / 1024}KB 已暂存，回环端口 $port，等待 Termux 拉取（超时 60s）…")
        val sender = Thread {
            runCatching {
                server.accept().use { sock ->
                    sock.getOutputStream().use { out ->
                        tmp.inputStream().use { it.copyTo(out) }
                    }
                }
            }.onFailure { android.util.Log.w("McpHost", "tcp transfer: ${it.message}") }
            runCatching { server.close() }
        }
        sender.start()

        // 3) Termux 侧拉取 + 校验字节数。注意：不能直接 `> mcp-host`——
        //    旧 host 进程可能正在执行该文件（open for write 报 ETXTBSY
        //    "Text file busy"）。先写 .new 再 mv：同文件系统 rename 可
        //    原子替换运行中的二进制，旧进程继续跑旧 inode，互不干扰。
        val cmd = "mkdir -p \$HOME/.openslate && " +
            "cat < /dev/tcp/127.0.0.1/$port > \$HOME/.openslate/mcp-host.new && " +
            "chmod +x \$HOME/.openslate/mcp-host.new && " +
            "[ \$(wc -c < \$HOME/.openslate/mcp-host.new) -eq $size ] && " +
            "mv \$HOME/.openslate/mcp-host.new \$HOME/.openslate/mcp-host && echo installed"
        val t0 = System.currentTimeMillis()
        val (out, err) = termux(ctx, cmd)
        log("Termux 拉取命令返回（${System.currentTimeMillis() - t0}ms）")
        runCatching { sender.join(1_000) }
        runCatching { tmp.delete() }
        if (out?.contains("installed") != true) {
            throw IllegalStateException(
                "host 二进制传输失败：${err.take(150)}${out?.take(150) ?: ""}"
            )
        }
    }

    /**
     * 从原始输入（显示名或用户指定代号）派生安全的工具名前缀（slug）：
     * 白名单 [A-Za-z0-9_-]、连续非法字符压缩为单个 _、绝不产出 __（与
     * host 工具名分隔符冲突会破坏路由）、空结果回退 srv；taken 已占用时
     * 追加 -2/-3… 去重。
     *
     * 为什么必须收敛到一个实现：slug 同时是清单 `[servers.<slug>]` 键与
     * host 工具名 `<slug>__<tool>` 的路由前缀，slug 含 `__` 会让
     * downstream 的 split_once("__") 路由直接损坏；两处（清单生成 / UI
     * 展示）各自派生必然漂移，故 UI 一律经 [serversSlugMap] 取值。
     */
    fun serverSlug(alias: String, taken: Collection<String>): String {
        // 非法字符（中文/空格/点等）连续出现算一段，压成单个 _；再单独
        // 压缩下划线连跑——用户输入的合法 "__" 同样会撞路由分隔符。
        var slug = alias.trim()
            .replace(Regex("[^A-Za-z0-9_-]+"), "_")
            .replace(Regex("_+"), "_")
        // 纯非 ASCII 别名（如全中文）会退化成单 "_"，与空结果同样无意义，
        // 一并回退（旧实现对中文别名正是产出 "_"，属已知缺陷）。
        if (slug.isEmpty() || slug == "_") slug = "srv"
        var candidate = slug
        var n = 2
        while (candidate in taken) {
            candidate = "$slug-$n"
            n += 1
        }
        return candidate
    }

    /**
     * 条目最终 slug 派生：用户指定的代号（slug 非空）优先，否则从显示名
     * 自动派生。**用户指定值也走 [serverSlug] 同一 sanitize**——白名单/
     * 禁 `__`/回退/去重对用户输入同样生效，这是安全边界不是可选美化：
     * 坏值会直接破坏路由与清单键。serversSlugMap 与对话框预览共用此实现，
     * 保证预览 = 保存后清单里的实际前缀。
     */
    fun entrySlug(name: String, slug: String?, taken: Collection<String>): String =
        serverSlug(slug?.trim()?.takeIf { it.isNotEmpty() } ?: name, taken)

    /**
     * 共用派生映射：按列表顺序为每个条目派生 slug（先到先得，同 slug 的
     * 后到者追加 -2/-3…）。混合条目（有的指定代号、有的自动）也按列表
     * 顺序确定性派生。manifestToml 与 UI 都从这里取，防逻辑漂移。
     */
    fun serversSlugMap(servers: List<McpServerEntry>): List<Pair<McpServerEntry, String>> {
        val taken = mutableSetOf<String>()
        return servers.map { s ->
            val slug = entrySlug(s.name, s.slug, taken)
            taken += slug
            s to slug
        }
    }

    /**
     * 从启动命令识别服务器显示名：npx / uvx / pnpm dlx 的包名参数。
     * 包名剥掉 @scope/ 前缀（如 @modelcontextprotocol/server-memory →
     * server-memory——作用域前缀对显示名是噪音）；识别不出返回 null。
     *
     * 规则：跳过任意 `-` 开头的 flag（-y、--yes 等）；`--from <pkg>` 特殊
     * （uvx 常用形态，其值才是包名，取到即停）；第一个非 flag token = 包名。
     * 其他可执行形态（直接 bin 路径、python 等）→ null。
     *
     * 仅供添加/编辑对话框在名称留空时建议显示名用，不进清单派生链路
     * （落库 name 由对话框兜底保证非空，slug 派生不受影响）。
     */
    fun suggestServerName(command: String): String? {
        val tokens = command.trim().split(Regex("\\s+"))
        // 三类可识别形态；uvx / pnpm dlx 的包名参数整体前移一位
        // （pnpm 必须带 dlx 子命令，其余子命令不装包，不适用）。
        val args = when {
            tokens.first() == "npx" -> tokens.drop(1)
            tokens.first() == "uvx" -> tokens.drop(1)
            tokens.first() == "pnpm" && tokens.getOrNull(1) == "dlx" -> tokens.drop(2)
            else -> return null
        }
        for (i in args.indices) {
            val t = args[i]
            if (!t.startsWith("-")) return stripScope(t)
            if (t == "--from") {
                // --from 的值才是包名（uvx --from mcp-server-git mcp-server-git）；
                // 取到即停，缺值（畸形命令行）按识别失败处理。
                return args.getOrNull(i + 1)?.let { stripScope(it) }
            }
        }
        return null
    }

    /** 剥 @scope/ 前缀（取 split('/') 末段）；剥完为空 = 识别失败。 */
    private fun stripScope(pkg: String): String? =
        pkg.split('/').last().takeIf { it.isNotEmpty() }

    /** 生成当前清单（token + 全部 server 条目）。 */
    fun manifestToml(ctx: Context): String {
        val sb = StringBuilder()
        sb.append("bind = \"127.0.0.1:$HOST_PORT\"\n")
        // token 明文写入 Termux 侧清单（保持现状）：mcp-host 是校验方，其
        // home 目录本就是该 token 的信任边界，属于不同信任域，App 侧加密
        // 对它无意义；App 侧的存取已改走 SecretStore（见 token()）。
        sb.append("token = \"${token(ctx)}\"\n")
        for ((s, slug) in serversSlugMap(loadServers(ctx))) {
            sb.append("\n[servers.$slug]\n")
            // 命令行 → command + args（mcp-host spawn 子进程不经过 shell，
            // 不展开变量；Termux PATH 会随 host 进程传给子进程）。
            val parts = s.command.trim().split(Regex("\\s+"))
            sb.append("command = \"${escapeToml(parts.first())}\"\n")
            if (parts.size > 1) {
                sb.append("args = [" + parts.drop(1).joinToString(", ") { "\"${escapeToml(it)}\"" } + "]\n")
            }
        }
        return sb.toString()
    }

    private fun escapeToml(s: String) = s.replace("\\", "\\\\").replace("\"", "\\\"")

    /** 清单推送：base64 内嵌命令写入（小文本，不经 /sdcard）。 */
    suspend fun pushManifest(ctx: Context) {
        val b64 = android.util.Base64.encodeToString(manifestToml(ctx).toByteArray(), android.util.Base64.NO_WRAP)
        val cmd = "mkdir -p \$HOME/.openslate && printf %s '$b64' | base64 -d > \$HOME/.openslate/mcp-host.toml && echo ok"
        val (out, err) = termux(ctx, cmd)
        if (out?.trim() != "ok") throw IllegalStateException("清单推送失败：${err.take(200)}")
    }

    /** 启动/重启 host（kill 旧进程 → nohup 后台启动 → 等待端口）。 */
    suspend fun restartHost(ctx: Context, log: (String) -> Unit = {}) {
        // ── 第 1 条：杀旧进程（独立命令行，只含 [o] 转义模式字面）─────
        // 不能和启动合并：启动命令行必然携带 host 路径字面（$HOME/.openslate/
        // mcp-host），pkill -f 的正则 [o]penslate/mcp-host 会匹配到那个字面
        // → bash 自杀（真机实测 exit 143）。-x（按 comm 精确匹配）在该设备
        // Termux 的 pkill 实现上匹配不到（真机实测），不可用。
        log("停止旧 host 进程…")
        termux(ctx, "pkill -f '[o]penslate/mcp-host' 2>/dev/null; sleep 0.3; echo killed")

        // ── 第 2 条：启动（setsid 尽力而为，缺省退化 nohup 裸启）──────
        // LD_PRELOAD=libtermux-exec：npm 装的第三方 bin（如 mcp-server-memory）
        // 的 shebang 是 #!/usr/bin/env node，而 Termux 没有 /usr/bin——交互
        // shell 全靠 termux-exec 垫片在 execve 时改写 shebang；RUN_COMMAND
        // 非交互环境没有这个 LD_PRELOAD，host spawn 的下游会 ENOENT 秒死
        // （host.log 表现为 "handshake: connection closed"，真机实证）。
        // host 自身是静态 musl 二进制不受 LD_PRELOAD 影响，仅随环境传递给
        // 子进程（npx/node 动态链接，垫片在其 execve 时生效）。垫片不存在
        // （非 Termux 环境）则跳过，不影响其他平台。
        val cmd = ": > \$HOME/.openslate/host.log; " +
            "SETS=\$(command -v setsid || true); " +
            "LDEXEC=/data/data/com.termux/files/usr/lib/libtermux-exec.so; " +
            "[ -f \"\$LDEXEC\" ] && export LD_PRELOAD=\"\$LDEXEC\"; " +
            "nohup \$SETS \$HOME/.openslate/mcp-host --config \$HOME/.openslate/mcp-host.toml " +
            ">> \$HOME/.openslate/host.log 2>&1 < /dev/null & " +
            "sleep 1; pgrep -f '[o]penslate/mcp-host' >/dev/null && echo running || echo dead"
        log("启动 host 进程（nohup 后台）…")
        val (out, err) = termux(ctx, cmd)
        log("启动命令返回：进程状态 ${out?.trim() ?: "?"}")
        if (err.isNotEmpty() && !err.contains("Terminated")) {
            throw IllegalStateException("host 启动失败：${err.take(200)}")
        }
        // 轮询端口就绪（最多 ~8s；mcp-host 绑定即应答）。
        log("等待端口就绪（每 0.5s 探测一次，最多约 8s）…")
        var ready = false
        var tries = 0
        repeat(16) {
            if (ready) return@repeat
            tries += 1
            if (isHostAlive(ctx)) ready = true else Thread.sleep(500)
        }
        if (!ready) {
            // 探活失败自动带回 Termux 侧证据（host.log 尾部 + 进程状态）。
            val (diag, _) = termux(
                ctx,
                "tail -15 \$HOME/.openslate/host.log 2>/dev/null; echo ===PROC===; " +
                    "pgrep -af '[o]penslate/mcp-host' || echo no-process",
            )
            throw IllegalStateException(
                "host 启动后端口未就绪（$tries 次探测无应答，进程 ${out?.trim() ?: "?"}）：\n${diag?.take(700)}"
            )
        }
        log("端口就绪（第 $tries 次探测通过）")
        // 常驻：wake-lock 让 Termux 通知常驻，降低被系统冻结/回收的概率
        //（失败静默——不影响主流程）。
        termux(ctx, "termux-wake-lock 2>/dev/null || true")
        log("已尝试开启 Termux 常驻（wake-lock，Termux 通知栏可见）")
    }
}
