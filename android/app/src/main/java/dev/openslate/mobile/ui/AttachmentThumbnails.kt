package dev.openslate.mobile.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import android.graphics.ImageDecoder
import android.graphics.Matrix
import android.media.ExifInterface
import android.net.Uri
import android.os.Build
import android.util.LruCache
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.RuntimeBridge
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/**
 * 附件缩略图解码与缓存（chips 卡片预览位，2026-10 改版：chip 从「图标+文件名」
 * 横条升级为「上预览图 + 下文件名/尺寸」小卡片）。
 *
 * 两个来源：
 * - composer 草稿：photo picker content URI 直接解码
 *   （[rememberUriThumbnail]；URI 授权随 App 进程存活，草稿 chips 本就只在
 *   进程内存在，无需持久化）；
 * - transcript 只读引用：`getAttachmentThumbnail` FFI（反射调用，binding
 *   未生成 / 调用失败 / 无缩略图一律返回 null → 调用方回退图标占位，见
 *   [RuntimeBridge.attachmentThumbnail]），base64 JPEG → BitmapFactory。
 *
 * 缓存只服务 FFI 来源（[AttachmentThumbnailCache]，按附件 id；URI 解码结果
 * 随 chip 组合 remember，chip 数量少、位图 ~百像素级，无需再缓存）。容量按
 * bitmap 字节数计 ~4MiB，滚动出屏的 chip 回来时 LruCache 命中即秒出；已知
 * 取不到缩略图的 id 进 missing 集，避免转写滚动反复打 FFI。
 */

/** 解码目标边长（预览位长边 88dp 的 ~1.25 倍，密度缩放后解码，余量防糊）。 */
internal val AttachmentThumbTargetDp: Dp = 112.dp

/** transcript 附件缩略图缓存（按附件 id；容量按字节数 ~4MiB）。 */
private object AttachmentThumbnailCache {
    private const val MAX_BYTES = 4 * 1024 * 1024
    private val lru = object : LruCache<String, Bitmap>(MAX_BYTES) {
        override fun sizeOf(key: String, value: Bitmap): Int = value.byteCount
    }

    /** FFI 确认取不到缩略图的附件 id（重试无益；进程内记忆）。 */
    private val missing = java.util.concurrent.ConcurrentHashMap.newKeySet<String>()

    fun peek(id: String): Bitmap? = lru.get(id)
    fun isMissing(id: String): Boolean = id in missing

    fun put(id: String, bitmap: Bitmap) {
        lru.put(id, bitmap)
        missing.remove(id)
    }

    fun markMissing(id: String) {
        missing.add(id)
    }
}

/** composer 草稿 chip 预览：URI 异步解码（解出前返回 null → 调用方渲染占位）。 */
@Composable
internal fun rememberUriThumbnail(uri: Uri, targetPx: Int): Bitmap? {
    val context = LocalContext.current
    var bitmap by remember(uri) { mutableStateOf<Bitmap?>(null) }
    LaunchedEffect(uri, targetPx) {
        bitmap = decodeUriThumbnail(context, uri, targetPx)
    }
    return bitmap
}

/** transcript 只读 chip 预览：FFI + LruCache（视频/取不到 → null 回退图标）。 */
@Composable
internal fun rememberRefThumbnail(id: String, isVideo: Boolean, targetPx: Int): Bitmap? {
    if (isVideo) return null
    var bitmap by remember(id) { mutableStateOf(AttachmentThumbnailCache.peek(id)) }
    LaunchedEffect(id, targetPx) {
        if (bitmap != null || AttachmentThumbnailCache.isMissing(id)) return@LaunchedEffect
        val bytes = withContext(Dispatchers.IO) { RuntimeBridge.attachmentThumbnail(id, targetPx) }
        if (bytes == null) {
            AttachmentThumbnailCache.markMissing(id)
            return@LaunchedEffect
        }
        val decoded = runCatching { BitmapFactory.decodeByteArray(bytes, 0, bytes.size) }.getOrNull()
        if (decoded != null) {
            AttachmentThumbnailCache.put(id, decoded)
            bitmap = decoded
        } else {
            AttachmentThumbnailCache.markMissing(id)
        }
    }
    return bitmap
}

/**
 * content URI → 降采样位图（IO 线程调用；失败返回 null，调用方回退占位图标）。
 * 只解出 ~[AttachmentThumbTargetDp] 级别的小图，全尺寸 bitmap 不留内存。
 */
internal suspend fun decodeUriThumbnail(context: Context, uri: Uri, targetPx: Int): Bitmap? =
    withContext(Dispatchers.IO) {
        runCatching {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
                // ImageDecoder：EXIF 自动回正 + setTargetSampleSize 保比例降采样
                //（内部取 2 的幂，与 import-<ts> HEIC 预转码同款写法）。
                ImageDecoder.decodeBitmap(ImageDecoder.createSource(context.contentResolver, uri)) { decoder, info, _ ->
                    val maxDim = maxOf(info.size.width, info.size.height)
                    if (maxDim > targetPx) decoder.setTargetSampleSize(maxDim / targetPx)
                }
            } else {
                decodeUriThumbnailLegacy(context, uri, targetPx)
            }
        }.getOrNull()
    }

/** API 26/27：BitmapFactory 两段解码（bounds → inSampleSize）+ EXIF 回正。 */
private fun decodeUriThumbnailLegacy(context: Context, uri: Uri, targetPx: Int): Bitmap? {
    val resolver = context.contentResolver
    val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
    resolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) }
    if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
    var sample = 1
    while (maxOf(bounds.outWidth, bounds.outHeight) / (sample * 2) >= targetPx) sample *= 2
    val opts = BitmapFactory.Options().apply { inSampleSize = sample }
    val bitmap = resolver.openInputStream(uri)?.use {
        BitmapFactory.decodeStream(it, null, opts)
    } ?: return null
    return applyExifRotation(context, uri, bitmap)
}

/**
 * 旧平台 EXIF 方向回正（ImageDecoder 仅 API 28+ 自带；26/27 的 BitmapFactory
 * 不读 EXIF，竖拍 JPEG 会横躺）。框架 ExifInterface 无 rotationDegrees 便捷
 * 属性（那是 androidx 包的），手动读 TAG_ORIENTATION 映射常用三个旋转角；
 * 转置/翻转类罕见方向不处理（保持原样好过解码失败）。
 */
private fun applyExifRotation(context: Context, uri: Uri, bitmap: Bitmap): Bitmap {
    val orientation = runCatching {
        context.contentResolver.openInputStream(uri)?.use {
            ExifInterface(it).getAttributeInt(
                ExifInterface.TAG_ORIENTATION,
                ExifInterface.ORIENTATION_NORMAL,
            )
        }
    }.getOrNull() ?: return bitmap
    val degrees = when (orientation) {
        ExifInterface.ORIENTATION_ROTATE_90 -> 90
        ExifInterface.ORIENTATION_ROTATE_180 -> 180
        ExifInterface.ORIENTATION_ROTATE_270 -> 270
        else -> return bitmap
    }
    if (degrees == 0) return bitmap
    val matrix = Matrix().apply { postRotate(degrees.toFloat()) }
    return runCatching {
        Bitmap.createBitmap(bitmap, 0, 0, bitmap.width, bitmap.height, matrix, true)
    }.getOrDefault(bitmap)
}
