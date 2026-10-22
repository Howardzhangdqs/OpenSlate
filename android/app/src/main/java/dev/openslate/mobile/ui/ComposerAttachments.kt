package dev.openslate.mobile.ui

import android.content.Context
import android.graphics.Bitmap
import android.graphics.ImageDecoder
import android.net.Uri
import android.os.Build
import android.provider.OpenableColumns
import androidx.compose.animation.Crossfade
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Image
import androidx.compose.material.icons.filled.Videocam
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import dev.openslate.mobile.bridge.AttachmentRefUi
import dev.openslate.mobile.bridge.RuntimeBridge
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/**
 * Composer 附件草稿（多模态输入设计 §3.6 Kotlin 数据流的 UI 侧）：
 * 选中 URI → 拷贝到 cache → HEIC 预转码 → `importAttachment` FFI →
 * Ready chip（id 入草稿，Submit 时拼进 JSON）。
 *
 * 两态：[Importing]（拷贝/转码/ingest 在途）与 [Ready]（id 已入 Rust 存储；
 * mime 为归一化值，chip 判类型用它）。两态都携带源 [previewUri]（photo
 * picker content URI）供预览位异步解码小图（ui/AttachmentThumbnails.kt）；
 * 视频无缩略图解码，previewUri 传 null 由 UI 出 Videocam 占位。
 */
internal sealed class ComposerAttachment {
    /** 草稿内唯一键（本地生成；导入完成后原位替换为 Ready）。 */
    abstract val key: String
    abstract val displayName: String

    /** 预览解码源（photo picker URI）；null = 无预览（视频/未知）。 */
    abstract val previewUri: Uri?

    data class Importing(
        override val key: String,
        override val displayName: String,
        override val previewUri: Uri? = null,
    ) : ComposerAttachment()

    data class Ready(
        override val key: String,
        val id: String,
        /** Rust 归一化后 mime（webp/gif→jpeg；UI 判类型用它）。 */
        val mime: String,
        val filename: String?,
        val sizeBytes: Long,
        /** 处理对比（fix-11；null = 响应缺键——查看器退回「文件名+大小」现状）。 */
        val processedInfo: AttachmentProcessedInfo? = null,
        override val previewUri: Uri? = null,
    ) : ComposerAttachment() {
        override val displayName: String get() = filename ?: id
        val isVideo: Boolean get() = mime.startsWith("video/")
    }
}

/**
 * 导入处理对比信息（fix-11 契约，`importAttachment` 成功 JSON 新增键）：
 * 原图尺寸/字节 vs 处理后尺寸 + 处理方式。草稿卡点击 → 全屏查看器时随
 * [AttachmentViewerSource.Draft] 展示「原图 vs 处理后」对比块；transcript
 * 存储附件无原图数据，不组装此对象。
 */
internal data class AttachmentProcessedInfo(
    val origWidth: Int,
    val origHeight: Int,
    val origBytes: Long,
    val width: Int,
    val height: Int,
    /** passthrough | transcode | video_passthrough。 */
    val processed: String,
) {
    companion object {
        /**
         * 全键齐备才组装（可选契约——旧版 Rust 响应无这些键；半缺状态一律
         * 整体视为无数据，不做残缺对比块）。
         */
        fun from(
            origWidth: Int?,
            origHeight: Int?,
            origBytes: Long?,
            width: Int?,
            height: Int?,
            processed: String?,
        ): AttachmentProcessedInfo? =
            if (origWidth != null && origHeight != null && origBytes != null &&
                width != null && height != null && processed != null
            ) {
                AttachmentProcessedInfo(origWidth, origHeight, origBytes, width, height, processed)
            } else {
                null
            }
    }
}

/** 单个附件导入结果（[importComposerAttachment] 返回）。 */
internal sealed interface AttachmentImportResult {
    data class Ok(val ready: ComposerAttachment.Ready) : AttachmentImportResult
    data class Failed(val message: String) : AttachmentImportResult
}

/** 附件 chip 尺寸摘要（"1.2 MB" / "340 KB"；<1KB 显字节数）。 */
internal fun formatAttachmentSize(bytes: Long): String = when {
    bytes >= 1 shl 20 -> "%.1f MB".format(bytes / 1048576.0)
    bytes >= 1 shl 10 -> "%.0f KB".format(bytes / 1024.0)
    else -> "$bytes B"
}

/** ContentResolver 显示名核心查询（回退路径段；null = 全部落空，调用方自定兜底）。 */
private suspend fun queryAttachmentDisplayName(context: Context, uri: Uri): String? =
    withContext(Dispatchers.IO) {
        runCatching {
            context.contentResolver.query(
                uri,
                arrayOf(OpenableColumns.DISPLAY_NAME),
                null, null, null,
            )?.use { c ->
                val idx = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (idx >= 0 && c.moveToFirst()) c.getString(idx)?.takeIf { it.isNotBlank() }
                else null
            } ?: uri.lastPathSegment?.takeLastFromSegment()
        }.getOrNull()
    }

/**
 * 批量显示名（多选导入「占位先行」用）：一次 IO 串行查完整批（单查毫秒级，
 * 上限 50 张总耗时无感）；查询失败给序号名兜底（`图片 N` / `视频 N`，N 为
 * 批内 1 起序号，[isVideo] 与 [uris] 对位）。
 */
internal suspend fun attachmentDisplayNames(
    context: Context,
    uris: List<Uri>,
    isVideo: List<Boolean>,
): List<String> = uris.mapIndexed { idx, uri ->
    queryAttachmentDisplayName(context, uri)
        ?: if (isVideo[idx]) "视频 ${idx + 1}" else "图片 ${idx + 1}"
}

private fun String?.takeLastFromSegment(): String? =
    this?.substringAfterLast('/')?.takeIf { it.isNotBlank() }

/**
 * 导入一个附件（全 IO；拷贝 → HEIC 检测/预转码 → FFI ingest → 清理临时
 * 文件）。契约（L4）：
 * - `importAttachment` 只接受 jpeg/png/webp/gif——HEIC/HEIF 必须在此预
 *   转码 JPEG（API 28+ `ImageDecoder`；API 26/27 无解码器 → 明确报
 *   「不支持此格式」，不调 FFI）；
 * - declaredMime 给 ContentResolver 真值（Rust 以 magic byte 嗅探为准）；
 * - 错误文案（FFI error 字段）直接展示。
 */
internal suspend fun importComposerAttachment(
    context: Context,
    uri: Uri,
    displayName: String,
): AttachmentImportResult = withContext(Dispatchers.IO) {
    val resolver = context.contentResolver
    val declaredMime = runCatching { resolver.getType(uri) }.getOrNull() ?: ""
    val stamp = System.currentTimeMillis()
    var raw: File? = null
    var converted: File? = null
    try {
        // 1. 拷贝到 cache（视频 100MB+ 走路径传递，绝不 base64——设计 §3.6）。
        val ext = displayName.substringAfterLast('.', "").lowercase()
            .takeIf { it.isNotEmpty() && it.length <= 5 }?.let { ".$it" } ?: ""
        val dst = File(context.cacheDir, "import-$stamp$ext")
        runCatching {
            resolver.openInputStream(uri)?.use { input ->
                dst.outputStream().use { output -> input.copyTo(output) }
            } ?: throw IllegalStateException("无法读取所选内容")
        }.getOrElse { return@withContext AttachmentImportResult.Failed("无法读取所选内容：${it.message ?: "未知错误"}") }
        raw = dst

        // 2. HEIC/HEIF 检测（声明 mime 或 ftyp magic 任一命中）→ 预转码。
        var importPath = dst.absolutePath
        var importMime = declaredMime
        var importName = displayName
        if (isHeicMime(declaredMime) || fileLooksHeic(dst)) {
            if (Build.VERSION.SDK_INT < Build.VERSION_CODES.P) {
                // API 26/27 无 HEIF 解码：明确拒绝，不调 FFI（设计 §3.6-2）。
                return@withContext AttachmentImportResult.Failed(
                    "不支持此格式（系统过旧，无法转换 HEIC/HEIF 图片）",
                )
            }
            val jpeg = runCatching { transcodeHeicToJpeg(context, dst, stamp) }.getOrElse {
                return@withContext AttachmentImportResult.Failed("图片转换失败：${it.message ?: "未知错误"}")
            }
            converted = jpeg
            importPath = jpeg.absolutePath
            importMime = "image/jpeg"
            // 转码后扩展名如实改 .jpg（chip 显示名与实际字节一致）。
            importName = displayName.substringBeforeLast('.', displayName) + ".jpg"
        }

        // 3. FFI ingest（blocking，已在 IO 线程）。
        when (val r = RuntimeBridge.importAttachment(importPath, importMime, importName)) {
            is RuntimeBridge.AttachmentImportUi.Ok -> AttachmentImportResult.Ok(
                ComposerAttachment.Ready(
                    key = "att-${r.id}",
                    id = r.id,
                    mime = r.mime,
                    filename = r.filename ?: importName,
                    sizeBytes = r.sizeBytes,
                    // 处理对比（fix-11 可选契约；任一键缺失整体为 null）。
                    processedInfo = AttachmentProcessedInfo.from(
                        origWidth = r.origWidth,
                        origHeight = r.origHeight,
                        origBytes = r.origBytes,
                        width = r.width,
                        height = r.height,
                        processed = r.processed,
                    ),
                    // 源 URI 留在 chip 状态里供预览位解码（photo picker 授权
                    // 随进程存活；临时文件此处照旧清理，不走延迟删除）。
                    previewUri = uri,
                ),
            )
            is RuntimeBridge.AttachmentImportUi.Failed -> AttachmentImportResult.Failed(r.message)
        }
    } finally {
        // 原图不保留（附件是输入的拷贝；Rust 已 content-address 落盘）。
        runCatching { raw?.delete() }
        runCatching { converted?.delete() }
    }
}

private fun isHeicMime(mime: String): Boolean = mime in HEIC_MIMES

private val HEIC_MIMES = setOf(
    "image/heic", "image/heif", "image/heic-sequence", "image/heif-sequence",
)

/** ftyp box 品牌嗅探（major brand + compatible brands 任一命中即 HEIF 家族）。 */
private fun fileLooksHeic(file: File): Boolean = runCatching {
    file.inputStream().use { ins ->
        val head = ByteArray(64)
        var n = 0
        while (n < head.size) {
            val r = ins.read(head, n, head.size - n)
            if (r < 0) break
            n += r
        }
        if (n < 12) return@runCatching false
        if (String(head, 4, 4, Charsets.US_ASCII) != "ftyp") return@runCatching false
        val brands = mutableSetOf(String(head, 8, 4, Charsets.US_ASCII).lowercase())
        var off = 16 // 4(size)+4(ftyp)+4(major)+4(minor) 之后是 compatible brands
        while (off + 4 <= n) {
            brands.add(String(head, off, 4, Charsets.US_ASCII).lowercase())
            off += 4
        }
        brands.any { it in HEIC_BRANDS }
    }
}.getOrDefault(false)

private val HEIC_BRANDS = setOf(
    "heic", "heix", "hevc", "hevx", "heim", "heis", "hevm", "hevs", "mif1", "msf1",
)

/**
 * HEIC/HEIF → JPEG（API 28+）。`ImageDecoder` 按 EXIF 自动回正方向；
 * 降采样上限对齐归一化目标 2048（Rust 侧还会再校验/降采样，这里只是
 * 控制大图解码内存）。
 */
private fun transcodeHeicToJpeg(context: Context, src: File, stamp: Long): File {
    val bitmap = ImageDecoder.decodeBitmap(ImageDecoder.createSource(src)) { decoder, info, _ ->
        val maxDim = maxOf(info.size.width, info.size.height)
        if (maxDim > 2048) decoder.setTargetSampleSize(maxDim / 2048)
    }
    val out = File(context.cacheDir, "import-$stamp-conv.jpg")
    java.io.FileOutputStream(out).use { bitmap.compress(Bitmap.CompressFormat.JPEG, 90, it) }
    bitmap.recycle()
    return out
}

// ── chips 组件 ─────────────────────────────────────────────────────────────
// 视觉（2026-10 改版）：横向小条 → **纵向预览小卡片**——上方 ~56dp 圆角预览位
//（图片异步解出小图 ContentScale.Crop；视频/解码前 = 占位底色 + Material 图标），
// 下方文件名 + 尺寸两行小字；删除钮 = 预览位右上角半透明圆底 Close 叠加。
// 图标一律 Material（ui.md 硬规范）。出现/消失过渡沿用 composer 列的
// animateContentSize（ChatScreen 侧），卡片本体不另设动画。

/** 卡片宽 / 预览位高 / 卡片圆角（chips 家族共用，读写两处排版一致）。 */
private val ChipCardWidth = 88.dp
private val ChipPreviewHeight = 56.dp
private val ChipCardShape = RoundedCornerShape(12.dp)

/**
 * 草稿 chips 行（composer 输入区上方；水平滚动，可删除）。
 * 卡片标签：恒显示文件大小（token 估算已撤——导入侧不再产生 tokens 数据，
 * 2026-10 迭代；文件名移入全屏查看器）；
 * [onOpen] = 图片卡点击打开查看器（视频/导入中卡无点击）。
 */
@Composable
internal fun ComposerAttachmentChipsRow(
    items: List<ComposerAttachment>,
    onRemove: (String) -> Unit,
    onOpen: (ComposerAttachment.Ready) -> Unit = {},
    modifier: Modifier = Modifier,
) {
    if (items.isEmpty()) return
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.Top,
        modifier = modifier
            .fillMaxWidth()
            .horizontalScroll(rememberScrollState()),
    ) {
        for (item in items) {
            ComposerAttachmentChip(item = item, onRemove = { onRemove(item.key) }, onOpen = onOpen)
        }
    }
}

@Composable
private fun ComposerAttachmentChip(
    item: ComposerAttachment,
    onRemove: () -> Unit,
    onOpen: (ComposerAttachment.Ready) -> Unit,
) {
    val targetPx = with(LocalDensity.current) { AttachmentThumbTargetDp.toPx().toInt() }
    val isVideo = item is ComposerAttachment.Ready && item.isVideo
    // 图片：源 URI 异步解小图（Importing 期间也会解出——spinner 叠在预览上）。
    val bitmap = item.previewUri?.let { uri ->
        if (isVideo) null else rememberUriThumbnail(uri, targetPx)
    }
    Column(
        modifier = Modifier
            .width(ChipCardWidth)
            .clip(ChipCardShape)
            .background(MaterialTheme.colorScheme.surface.copy(alpha = 0.55f))
            .border(
                width = 0.5.dp,
                color = MaterialTheme.colorScheme.outline.copy(alpha = 0.4f),
                shape = ChipCardShape,
            ),
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(ChipPreviewHeight)
                // 图片卡预览位点击 → 全屏查看器（视频/导入中卡不可点；
                // 删除钮是本 Box 的子元素、自消费点击不受影响）。
                .clickable(
                    enabled = item is ComposerAttachment.Ready && !isVideo &&
                        item.previewUri != null,
                    onClick = { (item as? ComposerAttachment.Ready)?.let(onOpen) },
                ),
        ) {
            AttachmentChipPreviewContent(
                bitmap = bitmap,
                isVideo = isVideo,
                iconTint = MaterialTheme.colorScheme.secondary,
                placeholderBg = MaterialTheme.colorScheme.surfaceVariant.copy(alpha = 0.6f),
            )
            if (item is ComposerAttachment.Importing) {
                // 导入中：深色圆底 + 白 spinner，压在占位/预览上都可读。
                Box(
                    modifier = Modifier
                        .align(Alignment.Center)
                        .size(22.dp)
                        .clip(CircleShape)
                        .background(Color.Black.copy(alpha = 0.4f)),
                    contentAlignment = Alignment.Center,
                ) {
                    CircularProgressIndicator(
                        strokeWidth = 2.dp,
                        color = Color.White,
                        modifier = Modifier.size(13.dp),
                    )
                }
            }
            // 删除 ×（预览位右上角叠加；半透明圆底 + Material Close）。
            Box(
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .padding(3.dp)
                    .size(22.dp)
                    .clip(CircleShape)
                    .background(Color.Black.copy(alpha = 0.45f))
                    .clickable(onClick = onRemove),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    Icons.Filled.Close,
                    contentDescription = "移除附件 ${item.displayName}",
                    tint = Color.White,
                    modifier = Modifier.size(12.dp),
                )
            }
        }
        // 标签：恒显示文件大小（token 估算已撤，导入侧不再产生 tokens 数据；
        // 处理中显示「处理中…」——含 HEIC 转码/归一化/落盘，不止导入）。
        // 图片/视频卡同规则（共用此分支）。
        Text(
            when (item) {
                is ComposerAttachment.Importing -> "处理中…"
                is ComposerAttachment.Ready -> formatAttachmentSize(item.sizeBytes)
            },
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.outline,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.padding(start = 7.dp, end = 7.dp, top = 5.dp, bottom = 6.dp),
        )
    }
}

/**
 * 预览位内容（composer 草稿与 transcript 只读 chip 共用）：有位图 →
 * Crop 满铺（Crossfade 淡入防跳变）；无 → 占位底色 + Image/Videocam 图标。
 */
@Composable
private fun AttachmentChipPreviewContent(
    bitmap: Bitmap?,
    isVideo: Boolean,
    iconTint: Color,
    placeholderBg: Color,
) {
    Crossfade(targetState = bitmap, label = "attachmentPreview") { bmp ->
        if (bmp != null) {
            Image(
                bitmap = bmp.asImageBitmap(),
                contentDescription = null,
                contentScale = ContentScale.Crop,
                modifier = Modifier.fillMaxSize(),
            )
        } else {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .background(placeholderBg),
                contentAlignment = Alignment.Center,
            ) {
                Icon(
                    if (isVideo) Icons.Filled.Videocam else Icons.Filled.Image,
                    contentDescription = if (isVideo) "视频附件" else "图片附件",
                    tint = iconTint,
                    modifier = Modifier.size(20.dp),
                )
            }
        }
    }
}

/**
 * 只读附件卡片行（transcript 用户消息气泡内；无删除钮）。标签规则与草稿卡
 * 同源（2026-10 迭代：不显示文件名）——但 transcript 协议引用（AttachmentDto）
 * 只有 id/mime/filename、无 size 字段，Rust 不动的约束下卡片**纯缩略图**
 * （尺寸/文件名都在查看器标签里）；例外：`tokens != null` 时缩略图右下角
 * 叠小徽标 `N tok`（响应侧可选契约，见 [AttachmentRefUi.tokens]）。
 * [onOpen] = 图片卡点击打开查看器（视频卡无点击）。
 */
@Composable
internal fun AttachmentRefChipsRow(
    refs: List<AttachmentRefUi>,
    onOpen: (AttachmentRefUi) -> Unit = {},
    modifier: Modifier = Modifier,
) {
    if (refs.isEmpty()) return
    Row(
        horizontalArrangement = Arrangement.spacedBy(8.dp),
        verticalAlignment = Alignment.Top,
        modifier = modifier
            .fillMaxWidth()
            .horizontalScroll(rememberScrollState()),
    ) {
        for (ref in refs) {
            AttachmentRefChip(ref = ref, onOpen = onOpen)
        }
    }
}

@Composable
private fun AttachmentRefChip(
    ref: AttachmentRefUi,
    onOpen: (AttachmentRefUi) -> Unit,
) {
    val targetPx = with(LocalDensity.current) { AttachmentThumbTargetDp.toPx().toInt() }
    val bitmap = rememberRefThumbnail(id = ref.id, isVideo = ref.isVideo, targetPx = targetPx)
    Column(
        modifier = Modifier
            .width(ChipCardWidth)
            .clip(ChipCardShape)
            .background(MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.08f))
            .border(
                width = 0.5.dp,
                color = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.25f),
                shape = ChipCardShape,
            ),
    ) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(ChipPreviewHeight)
                // 图片卡预览位点击 → 全屏查看器（视频卡无动作）。
                .clickable(enabled = !ref.isVideo) { onOpen(ref) },
        ) {
            AttachmentChipPreviewContent(
                bitmap = bitmap,
                isVideo = ref.isVideo,
                iconTint = MaterialTheme.colorScheme.onPrimaryContainer,
                placeholderBg = MaterialTheme.colorScheme.onPrimaryContainer.copy(alpha = 0.06f),
            )
            // 响应侧 token 徽标（右下角；契约：仅该回合恰好 1 图且模型
            // usage 顺带含图片 token（OpenAI 系）时 Rust 才写该键——智谱
            // 下恒 null 无徽标）。半透明黑底圆角 + labelSmall 白字，视觉
            // 克制不抢缩略图主体；数字不大，千分位不分组。
            ref.tokens?.let { tokens ->
                Text(
                    "$tokens tok",
                    style = MaterialTheme.typography.labelSmall,
                    color = Color.White,
                    modifier = Modifier
                        .align(Alignment.BottomEnd)
                        .padding(3.dp)
                        .clip(RoundedCornerShape(5.dp))
                        .background(Color.Black.copy(alpha = 0.45f))
                        .padding(horizontal = 4.dp, vertical = 1.dp),
                )
            }
        }
    }
}
