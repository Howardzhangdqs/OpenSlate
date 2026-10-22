package dev.openslate.mobile.ui

import android.net.Uri
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.text.InlineTextContent
import androidx.compose.foundation.text.appendInlineContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.statusBarsPadding
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.ArrowForward
import androidx.compose.material.icons.outlined.Tune
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.Placeholder
import androidx.compose.ui.text.PlaceholderVerticalAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import dev.openslate.mobile.bridge.RuntimeBridge
import java.io.ByteArrayInputStream
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import me.saket.telephoto.subsamplingimage.SubSamplingImage
import me.saket.telephoto.subsamplingimage.SubSamplingImageSource
import me.saket.telephoto.subsamplingimage.rememberSubSamplingImageState
import me.saket.telephoto.zoomable.ZoomSpec
import me.saket.telephoto.zoomable.rememberZoomableState
import me.saket.telephoto.zoomable.zoomable
import okio.source

/**
 * 附件全屏查看器（2026-10 chips 迭代第 3 项；同月二轮：手势层从手写
 * detectTransformGestures 换 **Telephoto**——手写 pan 与库级缩放叠加打架是
 * 用户实测反馈的问题根源）。视频卡不接查看器（无解码路径，点击无动作）。
 *
 * - 渲染：`SubSamplingImage` 按可视区域分块解码大图，2048px 级 JPEG 不再
 *   整张进内存；双指缩放 / 平移 / 双击放大复位 / 边界钳制全交
 *   `Modifier.zoomable`（`ZoomSpec(maxZoomFactor = 5f)`）。
 * - 来源两路径：
 *   - composer 草稿（Draft）：`SubSamplingImageSource.contentUri(uri)` 直读
 *     photo picker 原 URI，分辨率不受 FFI 限制（查看器专用的大图解码路径
 *     已删；chips 小缩略图的 `decodeUriThumbnail` 保留在
 *     ui/AttachmentThumbnails.kt 不动）；
 *   - transcript 引用（Stored）：FFI `getAttachmentThumbnail(id, maxPx=2048)`
 *     反射取 base64 JPEG → ByteArray → `rawSource`（okio 流子采样；
 *     **不进 chips 的 LruCache**——大图分块后常驻内存仍可观，随 Dialog
 *     组合持有、关闭即释放）。
 * - 关闭：单击空白（zoomable 的 `onClick`）+ 系统返回键（Dialog）。
 *   手写下滑关闭已删——与库手势叠自定义 pan 会冲突，不做。
 * - 已知坑位规避（官方 issue）：`rememberZoomableState` 每屏独立（#108 共享
 *   state 错乱）；不与 `Modifier.wrapContentSize()` 叠用（#111），容器恒
 *   `fillMaxSize()`。
 * - 左上角元信息标签（statusBarsPadding 避开状态栏）：三态——无处理数据
 *   （transcript 引用 / 旧版导入响应）= 文件名 + 文件大小单行（缺项省行）；
 *   草稿源带处理对比（fix-11 契约）时 = 文件名 + 处理对比块（transcode
 *   单行「原图 → 处理后」ArrowForward inlineContent；passthrough/
 *   video_passthrough 单行）+ 原因副文案与「跳转设置」小圆钮（见
 *   AttachmentProcessedCaption）。
 */

/** 存储源 FFI 大图请求边长（FFI 内部缩放并 cap 2048）。 */
private const val VIEWER_MAX_PX = 2048

/** 查看器图片来源。 */
internal sealed class AttachmentViewerSource {
    /** 标签行文件名（null = 协议未携带）。 */
    abstract val filename: String?

    /** 标签行大小（null = 数据源不提供——transcript 引用协议无 size）。 */
    abstract val sizeBytes: Long?

    /** composer 草稿：photo picker URI 直读（子采样按需分块解码）。 */
    data class Draft(
        val uri: Uri,
        override val filename: String?,
        override val sizeBytes: Long,
        /**
         * 导入处理对比（fix-11；null = 导入响应缺键——信息区退回「文件名 +
         * 大小」现状）。transcript 存储源无原图数据，恒 null。
         */
        val processed: AttachmentProcessedInfo? = null,
    ) : AttachmentViewerSource()

    /** transcript 引用：FFI 按附件 id 取 JPEG 字节（子采样源）。 */
    data class Stored(
        val id: String,
        override val filename: String?,
        override val sizeBytes: Long? = null,
    ) : AttachmentViewerSource()
}

/** 附件全屏查看器（Dialog 全屏黑底；返回键经 onDismissRequest 关闭）。 */
@Composable
internal fun AttachmentViewerDialog(
    source: AttachmentViewerSource,
    onDismiss: () -> Unit,
    /** 对比块「跳转设置」：关闭查看器并跳设置页「处理图片」（草稿源带处理数据时显示按钮）。 */
    onOpenProcessingSettings: () -> Unit = {},
) {
    Dialog(
        onDismissRequest = onDismiss,
        properties = DialogProperties(usePlatformDefaultWidth = false, decorFitsSystemWindows = false),
    ) {
        AttachmentViewerContent(
            source = source,
            onDismiss = onDismiss,
            onOpenProcessingSettings = onOpenProcessingSettings,
        )
    }
}

@Composable
private fun AttachmentViewerContent(
    source: AttachmentViewerSource,
    onDismiss: () -> Unit,
    onOpenProcessingSettings: () -> Unit,
) {
    // 图源装配：Draft 同步 contentUri；Stored 先 FFI（IO）取字节再 rawSource。
    var model by remember(source) { mutableStateOf<SubSamplingImageSource?>(null) }
    var failed by remember(source) { mutableStateOf(false) }
    LaunchedEffect(source) {
        when (source) {
            is AttachmentViewerSource.Draft ->
                model = SubSamplingImageSource.contentUri(source.uri)
            is AttachmentViewerSource.Stored -> {
                val bytes = withContext(Dispatchers.IO) {
                    RuntimeBridge.attachmentThumbnail(source.id, VIEWER_MAX_PX)
                }
                model = bytes?.let { b ->
                    SubSamplingImageSource.rawSource(
                        source = { ByteArrayInputStream(b).source() },
                        preview = null,
                    )
                }
                failed = bytes == null
            }
        }
    }
    // 每屏独立 state（telephoto #108：共享 state 跨屏错乱）。
    val zoomState = rememberZoomableState(zoomSpec = ZoomSpec(maxZoomFactor = 5f))

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black.copy(alpha = 0.92f)),
    ) {
        val src = model
        if (src != null) {
            // 子采样 state 绑定 zoomable state（0.19 API：瓦片按视口解码，
            // 缩放变换由 zoomable 托管，二者共享变换信息）。
            val imageState = rememberSubSamplingImageState(
                imageSource = src,
                zoomableState = zoomState,
            )
            SubSamplingImage(
                state = imageState,
                contentDescription = "附件图片",
                // 单击空白关闭；双指缩放/平移/双击复位由 zoomable 托管。
                // 容器 fillMaxSize（telephoto #111：wrapContentSize 叠用会错乱）。
                modifier = Modifier
                    .fillMaxSize()
                    .zoomable(zoomState, onClick = { onDismiss() }),
            )
        } else if (failed) {
            Text(
                "无法加载图片",
                style = MaterialTheme.typography.labelMedium,
                color = Color.White.copy(alpha = 0.7f),
                modifier = Modifier.align(Alignment.Center),
            )
        } else {
            // Stored FFI 在途 / 草稿首块瓦片解码前的加载反馈（图源就绪后
            // 被 SubSamplingImage 覆盖）。
            CircularProgressIndicator(
                color = Color.White,
                strokeWidth = 2.5.dp,
                modifier = Modifier
                    .align(Alignment.Center)
                    .size(28.dp),
            )
        }
        // 左上角元信息标签（半透明底；状态栏 inset 让位）。三态：
        // ① 无处理数据（transcript 引用 / 旧版导入响应）= 文件名 + 大小单行
        //    （现状）；
        // ② transcode = 文件名 + 原图行 → ArrowDownward 图标 + 处理后行 +
        //    原因副文案；
        // ③ passthrough / video_passthrough = 文件名 + 单行数据（原图与处理
        //    后相同不重复两行）+ 原因副文案。
        val processedInfo = (source as? AttachmentViewerSource.Draft)?.processed
        if (source.filename != null || source.sizeBytes != null || processedInfo != null) {
            Column(
                verticalArrangement = Arrangement.spacedBy(3.dp),
                modifier = Modifier
                    .align(Alignment.TopStart)
                    .statusBarsPadding()
                    .padding(12.dp)
                    .clip(RoundedCornerShape(8.dp))
                    .background(Color.Black.copy(alpha = 0.45f))
                    .padding(horizontal = 10.dp, vertical = 6.dp),
            ) {
                source.filename?.let {
                    Text(
                        it,
                        style = MaterialTheme.typography.labelSmall,
                        color = Color.White,
                        maxLines = 1,
                        overflow = TextOverflow.Ellipsis,
                        modifier = Modifier.widthIn(max = 240.dp),
                    )
                }
                if (processedInfo != null) {
                    AttachmentProcessedCaption(
                        info = processedInfo,
                        processedBytes = source.sizeBytes ?: 0L,
                        onOpenSettings = onOpenProcessingSettings,
                    )
                } else {
                    source.sizeBytes?.let {
                        Text(
                            formatAttachmentSize(it),
                            style = MaterialTheme.typography.labelSmall,
                            color = Color.White.copy(alpha = 0.75f),
                        )
                    }
                }
            }
        }
    }
}

/** 对比行内箭头 inlineContent 占位 id（AnnotatedString）。 */
private const val ARROW_INLINE_ID = "arrow"

/** 尺寸段紧凑形态（"12.3MB"——transcode 单行对比行省空间，查看器其余处不复用）。 */
private fun compactAttachmentSize(bytes: Long): String =
    formatAttachmentSize(bytes).replace(" ", "")

/**
 * 处理对比块（信息区第二段起；仅草稿源带 [AttachmentProcessedInfo] 时出现）。
 * 视觉：labelSmall 白字族，[formatAttachmentSize] 复用 chip 同款大小摘要；
 * transcode **单行**「原图 W×H · 大小 → 处理后 W×H · 大小」——箭头 =
 * ArrowForward 12dp inlineContent（60% 白，占位随文字基线居中、随行自然
 * 换行），尺寸段紧凑化（单位前空格去除，[compactAttachmentSize]）优先保
 * 一行，240dp 内放不下自然折行不截断；原图段 75% 白/处理后段纯白保留原
 * 两行的主次。原图/处理后相同（passthrough / video_passthrough / 未知值）
 * 只显一行不紧凑。末行原因副文案（60% 白、240dp 内换行）**右缘「跳转设
 * 置」小圆钮**：Icons.Outlined.Tune 14dp 白 70%、24dp 热区半透明黑圆底
 * （同草稿删除钮语言）→ 关闭查看器跳设置页「处理图片」。图标一律
 * Material（ui.md 硬规范）。
 */
@Composable
private fun AttachmentProcessedCaption(
    info: AttachmentProcessedInfo,
    processedBytes: Long,
    onOpenSettings: () -> Unit,
) {
    if (info.processed == "transcode") {
        Text(
            buildAnnotatedString {
                withStyle(SpanStyle(color = Color.White.copy(alpha = 0.75f))) {
                    append("原图 ${info.origWidth}×${info.origHeight} · ${compactAttachmentSize(info.origBytes)}")
                }
                append("  ")
                appendInlineContent(ARROW_INLINE_ID, "[→]")
                append("  ")
                withStyle(SpanStyle(color = Color.White)) {
                    append("处理后 ${info.width}×${info.height} · ${compactAttachmentSize(processedBytes)}")
                }
            },
            style = MaterialTheme.typography.labelSmall,
            modifier = Modifier.widthIn(max = 240.dp),
            inlineContent = mapOf(
                ARROW_INLINE_ID to InlineTextContent(
                    Placeholder(12.sp, 12.sp, PlaceholderVerticalAlign.TextCenter),
                ) {
                    Icon(
                        Icons.Filled.ArrowForward,
                        contentDescription = null,
                        tint = Color.White.copy(alpha = 0.6f),
                        modifier = Modifier.size(12.dp),
                    )
                },
            ),
        )
    } else {
        // passthrough / video_passthrough：原图与处理后相同，只显一行。
        Text(
            "原图 ${info.origWidth}×${info.origHeight} · ${formatAttachmentSize(info.origBytes)}",
            style = MaterialTheme.typography.labelSmall,
            color = Color.White,
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
        )
    }
    val reason = when (info.processed) {
        "passthrough" -> "已符合模型限制，未做处理"
        "transcode" -> "超出 2048px / 5MB 限制，已缩放并转码 JPEG 以符合模型 API 要求、节省 token 与流量"
        "video_passthrough" -> "视频原样保留，不做转码"
        else -> null
    }
    reason?.let {
        Row(
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(4.dp),
            modifier = Modifier.widthIn(max = 264.dp),
        ) {
            Text(
                it,
                style = MaterialTheme.typography.labelSmall,
                color = Color.White.copy(alpha = 0.6f),
                modifier = Modifier.weight(1f, fill = false),
            )
            Box(
                contentAlignment = Alignment.Center,
                modifier = Modifier
                    .size(24.dp)
                    .clip(CircleShape)
                    .background(Color.Black.copy(alpha = 0.45f))
                    .clickable(onClick = onOpenSettings),
            ) {
                Icon(
                    Icons.Outlined.Tune,
                    contentDescription = "图片处理设置",
                    tint = Color.White.copy(alpha = 0.7f),
                    modifier = Modifier.size(14.dp),
                )
            }
        }
    }
}
