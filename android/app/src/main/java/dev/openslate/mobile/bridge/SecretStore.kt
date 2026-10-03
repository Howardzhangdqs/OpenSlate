package dev.openslate.mobile.bridge

import android.content.Context
import android.os.Build
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.security.keystore.StrongBoxUnavailableException
import android.util.Base64
import android.util.Log
import java.security.KeyStore
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/**
 * API key 安全存储（PLAN §18）：Android Keystore 内生成 AES-256 密钥，
 * GCM 加密后的密文存应用私有 SharedPreferences；明文只在内存/runtime。
 *
 * - key 不出安全硬件：API>=28 优先 StrongBox（独立安全芯片），不可用时
 *   自动回退普通 TEE；API 26/27 无该能力，直接走 TEE。
 * - 密文格式：base64(iv[12] || ciphertext+tag)。
 * - 读取三态（[KeyLoadResult]）：调用方必须区分「未存储」与「密文在但
 *   解密失败（密钥变更/密文损坏，需重新录入）」，不得静默混作未配置。
 */
object SecretStore {
    private const val TAG = "OpenSlateBridge"
    private const val KS_ALIAS = "openslate_api_keys"
    private const val PREFS = "openslate_secrets"

    /** load() 的三态结果：未存储 / 解密成功 / 解密失败。 */
    sealed class KeyLoadResult {
        /** 该 provider 从未写入过密文。 */
        object Missing : KeyLoadResult()
        /** 解密成功，[value] 为明文。 */
        data class Ok(val value: String) : KeyLoadResult()
        /** 密文存在但解不开（Keystore 密钥失效、密文损坏等），需重新录入。 */
        data class Failed(val error: Throwable?) : KeyLoadResult()
    }

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(KS_ALIAS, null) as? SecretKey)?.let { return it }
        // API>=28：先试 StrongBox，不可用（含 StrongBoxUnavailableException
        // 及厂商实现的各种异常）回退普通 TEE；26/27 跳过直走 TEE。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            try {
                return generateKey(strongBox = true)
            } catch (e: StrongBoxUnavailableException) {
                Log.w(TAG, "StrongBox unavailable, fallback to TEE", e)
            } catch (e: Exception) {
                Log.w(TAG, "StrongBox keygen failed (${e.javaClass.simpleName}), fallback to TEE", e)
            }
        }
        return generateKey(strongBox = false)
    }

    private fun generateKey(strongBox: Boolean): SecretKey {
        val builder = KeyGenParameterSpec.Builder(
            KS_ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .setRandomizedEncryptionRequired(true)
        if (strongBox && Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
            builder.setIsStrongBoxBacked(true)
        }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(builder.build())
        return gen.generateKey()
    }

    /** 加密持久化；返回 false = 失败（调用方不得置「已配置」状态）。 */
    @Synchronized
    fun save(context: Context, provider: String, plain: String): Boolean = try {
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.ENCRYPT_MODE, key())
        val iv = cipher.iv
        val ct = cipher.doFinal(plain.toByteArray(Charsets.UTF_8))
        val blob = iv + ct
        prefs(context).edit()
            .putString(provider, Base64.encodeToString(blob, Base64.NO_WRAP))
            .apply()
        Log.i(TAG, "secret saved for provider=$provider (encrypted, ${blob.size}B blob)")
        true
    } catch (e: Throwable) {
        Log.e(TAG, "secret save failed for $provider", e)
        false
    }

    @Synchronized
    fun load(context: Context, provider: String): KeyLoadResult = try {
        val b64 = prefs(context).getString(provider, null)
            ?: return KeyLoadResult.Missing
        val blob = Base64.decode(b64, Base64.NO_WRAP)
        require(blob.size > 12) { "corrupted secret blob" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, blob, 0, 12))
        KeyLoadResult.Ok(String(cipher.doFinal(blob, 12, blob.size - 12), Charsets.UTF_8))
    } catch (e: Throwable) {
        Log.e(TAG, "secret load failed for provider=$provider", e)
        KeyLoadResult.Failed(e)
    }

    @Synchronized
    fun allProviders(context: Context): List<String> =
        prefs(context).all.keys.toList()

    @Synchronized
    fun clear(context: Context, provider: String) {
        prefs(context).edit().remove(provider).apply()
    }

    private fun prefs(context: Context) =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)
}
