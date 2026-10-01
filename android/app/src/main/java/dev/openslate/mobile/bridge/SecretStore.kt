package dev.openslate.mobile.bridge

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
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
 * - key 永不出安全硬件（StrongBox 可用时自动用）。
 * - 密文格式：base64(iv[12] || ciphertext+tag)。
 */
object SecretStore {
    private const val TAG = "OpenSlateBridge"
    private const val KS_ALIAS = "openslate_api_keys"
    private const val PREFS = "openslate_secrets"

    private fun key(): SecretKey {
        val ks = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (ks.getKey(KS_ALIAS, null) as? SecretKey)?.let { return it }
        val gen = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        gen.init(
            KeyGenParameterSpec.Builder(
                KS_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .setRandomizedEncryptionRequired(true)
                .build()
        )
        return gen.generateKey()
    }

    @Synchronized
    fun save(context: Context, provider: String, plain: String) {
        runCatching {
            val cipher = Cipher.getInstance("AES/GCM/NoPadding")
            cipher.init(Cipher.ENCRYPT_MODE, key())
            val iv = cipher.iv
            val ct = cipher.doFinal(plain.toByteArray(Charsets.UTF_8))
            val blob = iv + ct
            prefs(context).edit()
                .putString(provider, Base64.encodeToString(blob, Base64.NO_WRAP))
                .apply()
            Log.i(TAG, "secret saved for provider=$provider (encrypted, ${blob.size}B blob)")
        }.onFailure { Log.e(TAG, "secret save failed for $provider", it) }
    }

    @Synchronized
    fun load(context: Context, provider: String): String? = runCatching {
        val b64 = prefs(context).getString(provider, null) ?: return@runCatching null
        val blob = Base64.decode(b64, Base64.NO_WRAP)
        require(blob.size > 12) { "corrupted secret blob" }
        val cipher = Cipher.getInstance("AES/GCM/NoPadding")
        cipher.init(Cipher.DECRYPT_MODE, key(), GCMParameterSpec(128, blob, 0, 12))
        String(cipher.doFinal(blob, 12, blob.size - 12), Charsets.UTF_8)
    }.getOrNull()

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
