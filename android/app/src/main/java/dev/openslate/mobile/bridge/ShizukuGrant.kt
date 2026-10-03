package dev.openslate.mobile.bridge

import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import android.os.Process
import android.util.Log
import kotlinx.coroutines.suspendCancellableCoroutine
import rikka.shizuku.Shizuku
import kotlin.coroutines.resume

/**
 * Shizuku 一次性授权（独立运行模式的 ROM 兜底）。
 *
 * 定位：RUN_COMMAND 是 dangerous runtime permission，绝大多数 ROM 可在
 * 系统设置里手动授予；部分 ROM 不给第三方 custom permission 的授予入口，
 * 此时用 Shizuku（shell uid）执行一次 `pm grant` 即可。授权持久生效，
 * 日常执行不经过 Shizuku，重启也无需重新授权（Shizuku 服务停了也不影响）。
 *
 * 实现：Shizuku UserService（API 13.1.5 已把 newProcess 转私有，UserService
 * 是官方推荐路径）——授权代码以 shell 身份运行在 Shizuku 托管的独立进程。
 * Binder 协议为本文件内手写的极简双 String 接口（本机 aidl 工具链是
 * 占位 stub，见 android/README「已知限制」；两端同源，无需 AIDL 生成）。
 *
 * 用户前置（Android 11+ 全程手机完成）：开发者选项 → 无线调试开启 →
 * Shizuku App「通过无线调试启动」（首次需配对码配对）。
 */
object ShizukuGrant {

    private const val TAG = "OpenSlateShizuku"

    /** Shizuku 服务是否在运行（App 已装 ≠ 服务已启动）。 */
    fun isShizukuRunning(): Boolean = runCatching { Shizuku.pingBinder() }.getOrDefault(false)

    /** 本 App 是否已被允许使用 Shizuku。 */
    fun hasShizukuPermission(): Boolean = runCatching {
        Shizuku.checkSelfPermission() == PackageManager.PERMISSION_GRANTED
    }.getOrDefault(false)

    fun requestShizukuPermission() {
        runCatching { Shizuku.requestPermission(1001) }
            .onFailure { Log.w(TAG, "requestPermission failed", it) }
    }

    /**
     * 经 Shizuku 执行 `pm grant <pkg> com.termux.permission.RUN_COMMAND`
     * （挂起，IO 安全）。返回 null = 成功（已二次验证权限到位），否则为
     * 错误说明。永不抛出。
     */
    suspend fun grantTermuxPermission(context: Context): String? = try {
        if (!isShizukuRunning()) {
            "Shizuku 服务未运行：请先在 Shizuku App 中启动（无线调试）"
        } else if (!hasShizukuPermission()) {
            "尚未获得 Shizuku 授权：请先点击「申请 Shizuku 授权」"
        } else {
            val err = runGrantService(context, context.packageName, TermuxExec.RUN_COMMAND_PERMISSION)
            if (err == null && TermuxExec.hasPermission(context)) {
                Log.i(TAG, "RUN_COMMAND granted via shizuku ✓")
                null
            } else {
                err ?: "pm grant 成功但权限仍未生效（ROM 限制），请到系统设置手动授予"
            }
        }
    } catch (t: Throwable) {
        Log.e(TAG, "shizuku grant failed", t)
        "Shizuku 授权失败: ${t.message}"
    }

    /** 绑定 UserService 并执行 pm grant（一次性、不 daemon）。 */
    private suspend fun runGrantService(context: Context, packageName: String, permission: String): String? =
        suspendCancellableCoroutine { cont ->
            val args = Shizuku.UserServiceArgs(
                ComponentName(
                    "dev.openslate.mobile",
                    GrantUserService::class.java.name,
                ),
            )
                .processNameSuffix("grant")
                .tag("openslate_grant")
                .version(1)
                .daemon(false)
            lateinit var connection: ServiceConnection
            connection = object : ServiceConnection {
                override fun onServiceConnected(name: ComponentName?, binder: IBinder?) {
                    if (binder == null) {
                        if (cont.isActive) cont.resume("Shizuku UserService 绑定失败（binder 为空）")
                        return
                    }
                    threadIo {
                        val err = grantViaBinder(binder, packageName, permission)
                        runCatching { context.unbindService(connection) }
                        if (cont.isActive) cont.resume(err)
                    }
                }

                override fun onServiceDisconnected(name: ComponentName?) {}
            }
            runCatching { Shizuku.bindUserService(args, connection) }
                .onFailure {
                    Log.e(TAG, "bindUserService failed", it)
                    if (cont.isActive) cont.resume("Shizuku UserService 绑定失败: ${it.message}")
                }
        }

    /** 手写 Binder 调用：grant(pkg, perm) → void，异常经 writeException 回传。 */
    private fun grantViaBinder(binder: IBinder, packageName: String, permission: String): String? {
        if (binder.interfaceDescriptor != GrantUserService.DESCRIPTOR) {
            return "Shizuku UserService 返回了未知 binder（版本不匹配，请重装 App 后重试）"
        }
        val data = Parcel.obtain()
        val reply = Parcel.obtain()
        return try {
            data.writeInterfaceToken(GrantUserService.DESCRIPTOR)
            data.writeString(packageName)
            data.writeString(permission)
            binder.transact(GrantUserService.TRANSACTION_grant, data, reply, 0)
            reply.readException()
            null
        } catch (t: Throwable) {
            "pm grant 失败: ${t.message ?: t.javaClass.simpleName}"
        } finally {
            data.recycle()
            reply.recycle()
        }
    }

    private fun threadIo(block: () -> Unit) {
        Thread(block, "shizuku-grant").start()
    }
}

/**
 * 以 shell 身份运行的授权服务（Shizuku UserService；仅执行 pm grant，
 * 参数带白名单校验）。协议见 [ShizukuGrant.grantViaBinder]。
 */
class GrantUserService : Binder() {

    override fun getInterfaceDescriptor(): String = DESCRIPTOR

    override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
        if (code != TRANSACTION_grant) return super.onTransact(code, data, reply, flags)
        data.enforceInterface(DESCRIPTOR)
        val packageName = data.readString()
        val permission = data.readString()
        try {
            require(!packageName.isNullOrBlank() && packageName.matches(Regex("[A-Za-z0-9._]+"))) {
                "bad package name"
            }
            require(!permission.isNullOrBlank() && permission.matches(Regex("[A-Za-z0-9._]+"))) {
                "bad permission name"
            }
            val process = ProcessBuilder(
                "/system/bin/pm", "grant", packageName, permission,
            ).redirectErrorStream(true).start()
            val output = process.inputStream.bufferedReader().use { it.readText() }
            val exit = process.waitFor()
            if (exit != 0) error("pm grant failed (exit=$exit): ${output.take(400)}")
            reply?.writeNoException()
        } catch (t: Throwable) {
            reply?.writeException(t as? Exception ?: RuntimeException(t.message))
        }
        return true
    }

    companion object {
        const val DESCRIPTOR = "dev.openslate.mobile.bridge.IGrantService"
        const val TRANSACTION_grant = Binder.FIRST_CALL_TRANSACTION
    }
}
