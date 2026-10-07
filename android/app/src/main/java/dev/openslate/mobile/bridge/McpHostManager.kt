package dev.openslate.mobile.bridge

import android.content.Context
import android.content.Intent
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.withContext
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

    /** 一个 MCP server 条目（清单 [servers.<alias>] 的来源）。 */
    data class McpServerEntry(
        val alias: String,
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
                McpServerEntry(o.getString("alias"), o.getString("command"))
            }
        }.getOrDefault(emptyList())
    }

    fun saveServers(ctx: Context, servers: List<McpServerEntry>) {
        val arr = JSONArray()
        for (s in servers) arr.put(JSONObject().put("alias", s.alias).put("command", s.command))
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
        val allowed = if (installed && perm) {
            val (out, err) = termux(ctx, "echo ok")
            out?.trim() == "ok" && err.isEmpty()
        } else false
        return Triple(installed, perm, allowed)
    }

    /**
     * 仅检测 RUN_COMMAND 放行链路（echo ok 单次往返）。UI 状态刷新专用：
     * 独立成步，卡住时能明确定位是本通道问题（冷启动 / 冻结解冻 / 放行未开）。
     * 返回 (是否通过, 诊断：成功 = 耗时；失败 = 原因)。
     */
    suspend fun checkRunCommandAllowed(ctx: Context): Pair<Boolean, String> {
        val t0 = System.currentTimeMillis()
        val (out, err) = termux(ctx, "echo ok")
        val ms = System.currentTimeMillis() - t0
        val ok = out?.trim() == "ok" && err.isEmpty()
        return ok to if (ok) {
            "${ms}ms"
        } else {
            err.ifBlank { out ?: "无输出" }.take(160) + "（${ms}ms）"
        }
    }

    /**
     * 单次 Termux 往返同时探测：放行（echo ok）+ host 已安装（test -x）。
     * 预热（无 UI）路径专用——往返次数最少；UI 状态刷新走分步检测
     * （checkRunCommandAllowed / isHostInstalled），便于定位卡点。
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
            val (allowed, hostInstalled, detail) = probeTermux(appCtx)
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
            log("打开 Termux 前台完成初始化（屏幕会切过去，约 2 秒后自动继续，可返回本页看进度）…")
            if (startTermuxApp(ctx)) {
                delay(2000)
                log("Termux 已拉起，继续后续步骤…")
            } else {
                log("无法拉起 Termux（可能未安装），改走命令通道…")
            }

            step("检查 Termux 连通")
            val t0 = System.currentTimeMillis()
            val (installed, perm, allowed) = checkTermux(ctx)
            log("Termux 连通（${System.currentTimeMillis() - t0}ms）：安装=$installed 权限=$perm 放行=$allowed")
            if (!installed) throw IllegalStateException("未安装 Termux")
            if (!perm) throw IllegalStateException("缺少 RUN_COMMAND 权限（系统设置 → 应用 → OpenSlate → 权限）")
            if (!allowed) throw IllegalStateException("Termux 未放行外部应用（~/.termux/termux.properties 设 allow-external-apps=true 后重启 Termux）")

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

    /** 生成当前清单（token + 全部 server 条目）。 */
    fun manifestToml(ctx: Context): String {
        val sb = StringBuilder()
        sb.append("bind = \"127.0.0.1:$HOST_PORT\"\n")
        // token 明文写入 Termux 侧清单（保持现状）：mcp-host 是校验方，其
        // home 目录本就是该 token 的信任边界，属于不同信任域，App 侧加密
        // 对它无意义；App 侧的存取已改走 SecretStore（见 token()）。
        sb.append("token = \"${token(ctx)}\"\n")
        for (s in loadServers(ctx)) {
            val alias = s.alias.ifBlank { "srv" }.replace(Regex("[^A-Za-z0-9_-]"), "_")
            sb.append("\n[servers.$alias]\n")
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
        val cmd = ": > \$HOME/.openslate/host.log; " +
            "SETS=\$(command -v setsid || true); " +
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
