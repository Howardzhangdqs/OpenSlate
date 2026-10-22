//! 附件 ingest（多模态输入，存储 v5 设计 §3）。
//!
//! 职责边界：**本模块只管"字节进、引用出"**——magic-byte 嗅探（不信任
//! declared_mime）、sha256 content-addressing、图像归一化（缩放 + JPEG
//! q85；webp/gif 取首帧转码）、视频 passthrough、经 store 落盘
//! （`files/{sha256}.{ext}` + `attachments` 行，run_id NULL 暂存）。
//! 模态能力门控（caps / 数量 / 体积 / 图视频互斥）见
//! [`gate_submit_attachments`]，Submit 接线在 runtime.rs。
//!
//! 归一化即 wire 白名单的执行者（设计 §3.2）：出口 mime 恒为
//! `image/jpeg` / `image/png` / `video/mp4` / `video/x-matroska` /
//! `video/quicktime`——convert 层不需要格式分支。JPEG/PNG 的小图走
//! **header-only 快速通道**（零解码 passthrough，见 `normalize_image`）；
//! 归一化与导入全流程有分段耗时日志（`attachment timing: …`，真机
//! logcat 可诊）。

use std::path::Path;
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::imageops::FilterType;
use openslate_core::config::AttachmentsConfig;
use openslate_core::provider::{GenerateRequest, ModelProvider};
use openslate_core::types::{Message, ModelResponse};
use openslate_protocol::AttachmentDto;
use openslate_store_sqlite::query::AttachmentRecord;
use openslate_store_sqlite::store::SqliteStore;
use sha2::{Digest, Sha256};

/// 每请求图像数上限（智谱 GLM-5.3-Flash 事实：≤50 张；设计 §3.3 门控
/// 常量，不做配置项）。convert 层另有同值兜底（L3）。
pub const MAX_IMAGES_PER_SUBMIT: usize = 50;

/// ingest 入口的**源文件**防呆上限（50MB 固定宽裕常量，非用户旋钮；
/// 2026-10-21 起 `max_image_mb` 语义迁移为处理后大小上限）。归一化总
/// 能把图压到达标，入口只挡病态大文件（防 OOM/读盘超时），正常相机
/// 原图（≤50MB）一律放行走迭代压缩。
const MAX_SOURCE_IMAGE_BYTES: u64 = 50 * 1024 * 1024;

/// 归一化出口像素总数上限（防呆断言，设计 §3.3）。
const MAX_IMAGE_PIXELS: u64 = 6000 * 6000;

/// 迭代压缩的质量降档阶梯（用户可配置后不再一刀切 q85：先 q85，超
/// 用户大小上限则 70、55 逐档重编码，仍超再缩边重试）。
const JPEG_QUALITY_LADDER: [u8; 3] = [85, 70, 55];

/// 迭代压缩的缩边地板：目标边长 ×0.85 收缩不低于 1024（更小会毁掉
/// 截图文字可读性；到地板仍超限 = 病态小图高噪点，取最后结果 + warn，
/// 不再失败）。
const JPEG_DIM_FLOOR: u32 = 1024;

/// 归一化出口硬校验上限（provider 安全公共分母，防呆断言）：处理后
/// 字节 ≤5MB、像素 ≤6000²。用户旋钮 `max_image_mb` 值域即 ≤5
///（validation），迭代压缩达标是常态、地板是病态兜底——此断言是
/// wire 侧最后防线，实际不可达。
const MAX_NORMALIZED_IMAGE_BYTES: usize = 5 * 1024 * 1024;

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

// ---------------------------------------------------------------------------
// magic-byte 嗅探
// ---------------------------------------------------------------------------

/// magic-byte 嗅探（不信任 declared_mime；设计 §3.6）。
///
/// 返回归一化白名单 mime：图像 `image/jpeg|png|webp|gif`，视频
/// `video/mp4|x-matroska|quicktime`。未知格式返回 `None`（ingest 拒绝，
/// UI 报"不支持此格式"——HEIC/HEIF 由 Kotlin 侧预转码，Rust 侧不接受）。
pub fn sniff_mime(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        return Some("image/jpeg");
    }
    if data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF....WEBP
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    // EBML（matroska / webm 容器头）
    if data.len() >= 4 && data[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some("video/x-matroska");
    }
    // ISO-BMFF：....ftyp<brand>。brand `qt  ` = quicktime（mov）；heic
    // 家族 brand = 图片（Kotlin 侧预转码，白名单拒绝）；其余
    // （isom/iso2/mp41/mp42/avc1/hev1/…）按 mp4。
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let brand = &data[8..12];
        return if brand == b"qt  " {
            Some("video/quicktime")
        } else if matches!(
            brand,
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"mif1" | b"msf1"
        ) {
            None
        } else {
            Some("video/mp4")
        };
    }
    None
}

/// mime → 文件扩展名（与 store `SqliteStore::mime_extension` 的映射逐条
/// 一致——store 写文件用同一张表，读取侧在这里对齐）。
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "image/gif" => Some("gif"),
        "video/mp4" => Some("mp4"),
        "video/x-matroska" => Some("mkv"),
        "video/quicktime" => Some("mov"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ingest
// ---------------------------------------------------------------------------

/// import 成功结果（DTO + 对比信息）。**导入是纯本地操作**——绝不发起
/// 任何网络请求（附件字节只在真正发起模型请求时随请求上行；token 数
/// 由回合结束后响应侧回填，见 `crate::attachment_tokens`）。
///
/// 原图/处理后对比字段（`import_attachment_json` 的 chip 点击展开契约）：
/// `orig_*` 来自 header-only 尺寸读取与源文件字节长度；`width/height`
/// 为处理后尺寸（passthrough = 原尺寸；转码 = 缩放后）；视频 passthrough
/// 无尺寸信息（嗅探层不解析容器，省略——不做重解析）。
#[derive(Debug)]
pub struct ImportedAttachment {
    pub dto: AttachmentDto,
    /// 处理后字节数（落盘大小）。
    pub size_bytes: i64,
    /// 源文件字节数。
    pub orig_bytes: i64,
    /// 原图宽（header；视频 None）。
    pub orig_width: Option<u32>,
    /// 原图高（header；视频 None）。
    pub orig_height: Option<u32>,
    /// 处理后宽（视频 None）。
    pub width: Option<u32>,
    /// 处理后高（视频 None）。
    pub height: Option<u32>,
    /// 处理方式：`"passthrough"`（小图 JPEG/PNG 零转码）|
    /// `"transcode"`（缩放/重编码）| `"video_passthrough"`（视频不转码）
    /// ——UI 据此生成"为什么这样处理"文案。
    pub processed: &'static str,
}

/// [`normalize_image`] 的结果（mime/字节 + 对比信息）。
#[derive(Debug)]
struct NormalizedImage {
    mime: &'static str,
    data: Vec<u8>,
    orig_width: u32,
    orig_height: u32,
    width: u32,
    height: u32,
    processed: &'static str,
    /// 最终编码质量（passthrough = 0；达标/地板路径 = 实际档位
    /// 85/70/55）。诊断/测试用。
    quality: u8,
    /// 迭代压缩的编码次数（passthrough = 0；每轮质量档各一次）。
    attempts: u32,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 导入一个附件（Kotlin 已把 ContentResolver URI 拷到 cache 文件；
/// 设计 §3.6 路径传递，ByteArray/base64 不出现在边界）。
///
/// 流程：嗅探（不信任 `declared_mime`，仅记录偏差）→ 入口大小校验 →
/// 图像归一化 / 视频 passthrough → sha256 → `insert_attachment`
/// （run_id NULL 暂存，Submit 时 bind）。返回引用 DTO 供 composer 持有。
pub async fn import_attachment(
    store: &SqliteStore,
    cfg: &AttachmentsConfig,
    source_path: &Path,
    declared_mime: &str,
    filename: Option<&str>,
) -> Result<ImportedAttachment> {
    let t_start = std::time::Instant::now();
    let bytes = std::fs::read(source_path)
        .with_context(|| format!("读取源文件失败：{}", source_path.display()))?;
    let t_read = t_start.elapsed();
    let source_len = bytes.len();
    let sniffed = sniff_mime(&bytes)
        .context("无法识别的文件格式（支持 JPEG / PNG / WebP / GIF / MP4 / MKV / MOV）")?;
    if sniffed != declared_mime.trim().to_ascii_lowercase().as_str() {
        // 不信任但记录：declared 与嗅探不一致几乎总是 Kotlin 侧传错或
        // 扩展名伪装，按嗅探结果走。
        crate::alog!(
            "attachments: declared_mime={declared_mime:?} 与嗅探 {sniffed:?} 不一致，按嗅探处理"
        );
    }

    let (mime, data, orig_dims, out_dims, processed): (
        &'static str,
        Vec<u8>,
        Option<(u32, u32)>,
        Option<(u32, u32)>,
        &'static str,
    ) = if sniffed.starts_with("image/") {
        // 入口只挡病态大文件（50MB 固定防呆，非用户旋钮）——归一化
        // 迭代压缩总能把图压到达标，正常相机原图一律放行。
        if bytes.len() as u64 > MAX_SOURCE_IMAGE_BYTES {
            anyhow::bail!(
                "图片过大：原始文件上限 50 MB（实际 {:.1} MB），请裁剪后再导入",
                mb(bytes.len() as u64)
            );
        }
        let n = normalize_image(&bytes, sniffed, cfg)?;
        (
            n.mime,
            n.data,
            Some((n.orig_width, n.orig_height)),
            Some((n.width, n.height)),
            n.processed,
        )
    } else {
        let cap = cfg.max_video_mb * 1024 * 1024;
        if bytes.len() as u64 > cap {
            anyhow::bail!(
                "视频过大：落盘上限 {} MB（实际 {:.1} MB）",
                cfg.max_video_mb,
                mb(bytes.len() as u64)
            );
        }
        // 视频 passthrough 不转码；容器头尺寸嗅探层不解析（不做重
        // 解析，拿不到就不给）→ 尺寸字段全 None。
        (sniffed, bytes, None, None, "video_passthrough")
    };
    let t_norm = t_start.elapsed();

    // content-addressing：sha256 决定文件名；同 sha 已有文件 → 复用
    // （store 的 insert_attachment 幂等覆盖同名字节，行为等价复用）。
    let sha = {
        let mut h = Sha256::new();
        h.update(&data);
        hex(&h.finalize())
    };
    let t_sha = t_start.elapsed();
    let ext = ext_for_mime(mime).context("归一化后 mime 无扩展名映射（内部错误）")?;
    if let Some(dir) = store.files_dir() {
        if dir.join(format!("{sha}.{ext}")).exists() {
            crate::alog!("attachments: sha256={sha} 文件已存在，复用（不重复占用空间）");
        }
    }

    let id = format!("att-{}", uuid::Uuid::new_v4().simple());
    let filename = filename.map(str::trim).filter(|s| !s.is_empty());
    store
        .insert_attachment(
            &id,
            None, // run_id NULL：composer 暂存；Submit 门控通过后 bind
            &sha,
            mime,
            filename,
            data.len() as i64,
            &data,
            now_ms(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("附件落库失败：{e}"))?;
    let t_write = t_start.elapsed();
    crate::alog!(
        "attachment timing: read={}ms normalize={}ms sha256={}ms write={}ms total={}ms \
         imported {id} mime={mime} {}B (source {}B) sha256={sha}",
        t_read.as_millis(),
        (t_norm - t_read).as_millis(),
        (t_sha - t_norm).as_millis(),
        (t_write - t_sha).as_millis(),
        t_write.as_millis(),
        data.len(),
        source_len,
    );
    Ok(ImportedAttachment {
        dto: AttachmentDto {
            id,
            mime: mime.to_owned(),
            filename: filename.map(str::to_owned),
            // 导入侧零预估：tokens 由回合结束响应侧回填（attachment_tokens）。
            tokens: None,
        },
        size_bytes: data.len() as i64,
        orig_bytes: source_len as i64,
        orig_width: orig_dims.map(|d| d.0),
        orig_height: orig_dims.map(|d| d.1),
        width: out_dims.map(|d| d.0),
        height: out_dims.map(|d| d.1),
        processed,
    })
}

/// 图像归一化（设计 §3.2/§3.3；2026-10-21 起按用户配置**迭代压缩达标**）：
///
/// - **快速通道（零解码零编码零重采样）**：JPEG/PNG 先 header-only 读
///   尺寸（`header_dimensions`，只解析 SOF/IHDR，不碰像素），长边 ≤
///   用户 `max_image_dim` 且字节 ≤ 用户 `max_image_mb` → **字节原样
///   passthrough**。JPEG 本就在 wire 白名单内无需转码——相机截图/
///   压缩图（绝大多数真机导入）直接省掉 解码→重采样→编码 全程
///   （12MP 照片数秒 → 毫秒级 header 解析）。EXIF 方向不处理：
///   passthrough 保留原字节，方向解释交给 provider（全解码路径同样
///   不处理 EXIF，行为一致）；
/// - **迭代压缩达标**（transcode 路径，用户双旋钮）：
///   1. 等比缩到 ≤ 用户 `max_image_dim`（只缩不放，CatmullRom——纯
///      Rust 无 SIMD 快档）；
///   2. JPEG q85 编码 ≤ 用户 `max_image_mb` → 完成；
///   3. 超限 → 质量降档 [85→70→55] 逐档重编码；仍超且还能缩
///      （target×0.85 后仍会真正变小、地板 1024 之上）→ 缩边重试；
///   4. 地板：dim ≤1024 / 缩无可缩 且 q55 仍超（病态小图高噪点）→
///      取最后结果 + `tracing::warn`，不再失败。动图取首帧。
/// - 出口硬校验（provider 安全公共分母，两条路径都执行）：像素
///   ≤6000²、字节 ≤5MB——validation 已限用户旋钮 dim ≤6000 / mb ≤5，
///   断言是 wire 侧最后防线（达标路径恒 ≤ user_cap ≤5MB）。
fn normalize_image(
    bytes: &[u8],
    sniffed: &'static str,
    cfg: &AttachmentsConfig,
) -> Result<NormalizedImage> {
    let t_start = std::time::Instant::now();
    // header-only 尺寸（一次读取，全格式）：快速通道判定 + `orig_*` 上报。
    let hdr = header_dimensions(bytes);
    let t_header = t_start.elapsed();
    // 用户旋钮：处理后大小上限（字节）。
    let user_cap = cfg.max_image_mb.saturating_mul(1024 * 1024) as usize;

    // ── 快速通道：JPEG/PNG header 判定后直接 passthrough ──────────
    if matches!(sniffed, "image/jpeg" | "image/png") {
        if let Some((w, h)) = hdr {
            if w.max(h) <= cfg.max_image_dim && bytes.len() <= user_cap {
                // 出口硬校验（防呆断言，设计 §3.3；尺寸来自 header）。
                anyhow::ensure!(
                    (w as u64) * (h as u64) <= MAX_IMAGE_PIXELS,
                    "图像像素超限（{w}×{h} > 6000²）"
                );
                crate::alog!(
                    "attachment timing: sniff_header={}ms decode=0ms resize=0ms encode=0ms \
                     fast_path={sniffed}_passthrough {w}x{h} total={}ms",
                    t_header.as_millis(),
                    t_start.elapsed().as_millis()
                );
                return Ok(NormalizedImage {
                    mime: sniffed,
                    data: bytes.to_vec(),
                    orig_width: w,
                    orig_height: h,
                    width: w,
                    height: h,
                    processed: "passthrough",
                    quality: 0,
                    attempts: 0,
                });
            }
        }
        // header 解析失败（畸形/截断）→ 落到全解码路径，由解码器给准确错误。
    }

    // ── 全解码路径（迭代压缩达标；webp/gif 转码、超限 JPEG/PNG）────
    let img = image::load_from_memory(bytes)
        .with_context(|| format!("图像解码失败（{sniffed}）"))?;
    let t_decode = t_start.elapsed();
    // orig 尺寸：header 读到为准；header 失败（畸形但可解码的罕见形态）
    // 以解码尺寸兜底。
    let (orig_w, orig_h) = hdr.unwrap_or((img.width(), img.height()));
    let orig_max = img.width().max(img.height());
    // 统一 RGB8 像素源：迭代缩边每轮从原图重采样（不累积二次缩放）。
    let rgb = img.to_rgb8();
    drop(img);

    let mut target = cfg.max_image_dim; // validation 保证 512..=6000
    let mut attempts: u32 = 0;
    let mut last: (u32, u32, u8, Vec<u8>) = (0, 0, 0, Vec::new());
    let mut met_cap = false;
    let (out_w, out_h, quality, jpeg) = loop {
        // 等比缩到 max(w,h) ≤ target（只缩不放）。
        let (nw, nh) = if orig_max > target {
            let scale = target as f32 / orig_max as f32;
            (
                ((rgb.width() as f32 * scale).round() as u32).max(1),
                ((rgb.height() as f32 * scale).round() as u32).max(1),
            )
        } else {
            (rgb.width(), rgb.height())
        };
        let resized = if (nw, nh) != (rgb.width(), rgb.height()) {
            image::imageops::resize(&rgb, nw, nh, FilterType::CatmullRom)
        } else {
            rgb.clone()
        };

        // 质量降档阶梯：达标即记下并跳出内层。
        for q in JPEG_QUALITY_LADDER {
            attempts += 1;
            let mut buf = Vec::with_capacity(256 * 1024);
            let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q);
            use image::ImageEncoder as _;
            encoder
                .write_image(
                    resized.as_raw(),
                    nw,
                    nh,
                    image::ExtendedColorType::Rgb8,
                )
                .context("JPEG 重编码失败")?;
            let met = buf.len() <= user_cap;
            crate::alog!(
                "attachment timing: attempt={attempts} q={q} {nw}x{nh} → {}B {} cap {user_cap}B",
                buf.len(),
                if met { "≤" } else { ">" }
            );
            last = (nw, nh, q, buf);
            if met {
                met_cap = true;
                break;
            }
        }
        if met_cap {
            break last;
        }

        // 本档尺寸全阶梯超限：缩边重试（地板 1024；只有 ×0.85 后仍会
        // 真正变小才继续——原图已小到缩无可缩时直接走地板）。
        let next = (((target as f32) * 0.85).floor() as u32).max(JPEG_DIM_FLOOR);
        if next < target && next < orig_max {
            crate::alog!(
                "attachment timing: 质量阶梯全档超限，缩边重试 target {target} → {next}"
            );
            target = next;
            continue;
        }

        // 地板：病态小图高噪点（q55 + 可能的最小缩边仍超用户上限）——
        // 取最后结果，不再失败（provider 分母断言仍守）。
        let (w, h, q, buf) = last;
        tracing::warn!(
            attachment = sniffed,
            width = w,
            height = h,
            quality = q,
            bytes = buf.len(),
            cap = user_cap,
            attempts,
            "归一化到达地板（dim≤1024/缩无可缩 且 q55 仍超用户上限），取最后结果"
        );
        crate::alog!(
            "attachment timing: floor q={q} {w}x{h} {}B > cap {user_cap}B（取最后结果，不失败）",
            buf.len()
        );
        break (w, h, q, buf);
    };
    let t_encode = t_start.elapsed();

    // 出口硬校验（防呆断言，设计 §3.3）：像素恒成立（只缩 + 旋钮 ≤6000
    // + passthrough 已校验）；≤5MB 在达标路径 ≤ user_cap ≤5MB 恒成立，
    // 地板路径物理上难达（1024 级 q55 ≈ 1MB）——断言是 wire 最后防线。
    let (w, h) = (out_w as u64, out_h as u64);
    anyhow::ensure!(
        w * h <= MAX_IMAGE_PIXELS,
        "归一化后像素超限（{w}×{h} > 6000²）"
    );
    anyhow::ensure!(
        jpeg.len() <= MAX_NORMALIZED_IMAGE_BYTES,
        "归一化后图像超 5MB（{:.1} MB）",
        mb(jpeg.len() as u64)
    );
    crate::alog!(
        "attachment timing: sniff_header={}ms decode={}ms resize+encode={}ms \
         transcode {sniffed} {w}x{h} q={quality} attempts={attempts} \
         {}B→{}B total={}ms",
        t_header.as_millis(),
        (t_decode - t_header).as_millis(),
        (t_encode - t_decode).as_millis(),
        bytes.len(),
        jpeg.len(),
        t_start.elapsed().as_millis()
    );
    Ok(NormalizedImage {
        mime: "image/jpeg",
        data: jpeg,
        orig_width: orig_w,
        orig_height: orig_h,
        width: out_w,
        height: out_h,
        processed: "transcode",
        quality,
        attempts,
    })
}

/// header-only 尺寸读取（不解码像素）：JPEG 走 SOF 标记、PNG 走 IHDR、
/// webp/gif 走容器头（`image::ImageReader::into_dimensions`）。畸形/截断
/// 返回 `None`——调用方回落全解码路径（解码器给准确错误），绝不因
/// header 失败而拒绝图像。
fn header_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// 字节回填（resume / 请求路径）
// ---------------------------------------------------------------------------

/// 从 content-addressed 文件读附件字节。文件缺失（被 GC / 跨设备迁移
/// 丢失）返回 None——调用方 warn 跳过，不失败（模型丢图不丢文本）。
pub fn read_attachment_bytes(store: &SqliteStore, sha256: &str, mime: &str) -> Option<Arc<[u8]>> {
    let dir = store.files_dir()?;
    let ext = ext_for_mime(mime)?;
    let path = dir.join(format!("{sha256}.{ext}"));
    if !path.is_file() {
        return None;
    }
    std::fs::read(path).ok().map(Arc::from)
}

/// 按记录行读字节（id → sha256/mime → 文件）。
async fn read_attachment_by_id(store: &SqliteStore, id: &str) -> Option<Arc<[u8]>> {
    let row = store.get_attachment(id).await.ok().flatten()?;
    read_attachment_bytes(store, &row.sha256, &row.mime)
}

// ---------------------------------------------------------------------------
// 附件缩略图（FFI getAttachmentThumbnail 后端，des-2 Android lane）
// ---------------------------------------------------------------------------

/// 缩略图 JPEG 质量（UI 预览用途，q80 足够）。
const THUMBNAIL_JPEG_QUALITY: u8 = 80;

/// 生成附件缩略图：id → DB 行 → `files/{sha256}.{ext}` 原图字节 →
/// 长边缩到 `max_px`（只缩不放，快速过滤）→ JPEG q80 → base64。
///
/// 仅支持图像附件（视频/其他 mime、行不存在、文件缺失、解码失败均返
/// `Err`，FFI 层转 `{"ok":false,"error":"…"}`）。`max_px <= 0` 按 256
/// 处理；上限 2048（与归一化长边一致，防误传超大值）。
pub async fn attachment_thumbnail_base64(
    store: &SqliteStore,
    id: &str,
    max_px: i32,
) -> Result<String> {
    let row = store
        .get_attachment(id)
        .await
        .map_err(|e| anyhow::anyhow!("查询附件失败：{e}"))?
        .with_context(|| format!("附件不存在：{id}"))?;
    if !row.mime.starts_with("image/") {
        anyhow::bail!("仅支持图像缩略图（{} 是 {}）", id, row.mime);
    }
    let bytes = read_attachment_bytes(store, &row.sha256, &row.mime)
        .context("附件文件缺失（被清理？）")?;

    let img = image::load_from_memory(&bytes).context("图像解码失败")?;
    let max_px = if max_px <= 0 { 256 } else { max_px.min(2048) } as u32;
    let thumb = img.thumbnail(max_px, max_px);
    let mut jpeg = Vec::with_capacity(64 * 1024);
    let encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, THUMBNAIL_JPEG_QUALITY);
    thumb
        .to_rgb8()
        .write_with_encoder(encoder)
        .context("JPEG 编码失败")?;
    Ok(BASE64.encode(jpeg))
}

/// resume 回填：对消息历史里有附件引用的项从 files 回填 `data`
/// （设计 §6.1：历史图随每轮重发）。**总回填字节封顶
/// `max_inline_total_mb`**——超限的旧附件不回填（模型丢图不丢文本，
/// 可接受并文档化，R5）。返回回填成功的附件数。
pub async fn refill_message_attachment_data(
    store: &SqliteStore,
    cfg: &AttachmentsConfig,
    messages: &mut [Message],
) -> usize {
    let cap = cfg.max_inline_total_mb.saturating_mul(1024 * 1024);
    let mut used: u64 = 0;
    let mut filled = 0;
    for msg in messages.iter_mut() {
        let Some(atts) = msg.attachments.as_mut() else { continue };
        for att in atts.iter_mut() {
            if let Some(d) = att.data.as_ref() {
                used = used.saturating_add(d.len() as u64);
                continue;
            }
            if used >= cap {
                tracing::warn!(
                    attachment_id = %att.id,
                    "resume 回填超出总封顶（{} MB），旧附件不回填（模型丢图不丢文本）",
                    cfg.max_inline_total_mb
                );
                continue;
            }
            let Some(bytes) = read_attachment_by_id(store, &att.id).await else {
                tracing::warn!(attachment_id = %att.id, "附件字节缺失（文件被清理？），跳过回填");
                continue;
            };
            if used.saturating_add(bytes.len() as u64) > cap {
                tracing::warn!(
                    attachment_id = %att.id,
                    "回填该附件会超出总封顶（{} MB），跳过",
                    cfg.max_inline_total_mb
                );
                continue;
            }
            used += bytes.len() as u64;
            att.data = Some(bytes);
            filled += 1;
        }
    }
    if filled > 0 {
        crate::alog!("attachments: resume 回填 {filled} 个附件（{:.1} MB）", mb(used));
    }
    filled
}

// ---------------------------------------------------------------------------
// 请求路径 data 代理（provider 工厂装饰器）
// ---------------------------------------------------------------------------

/// 进程内共享的 store 追持有：工厂闭包在装配前构造，store 在
/// `assemble_state` 之后才存在（AskUserContext::wire 同款后注模式）。
pub type SharedStore = Arc<OnceLock<Arc<SqliteStore>>>;

/// provider 装饰器：GenerateRequest 进 provider 前，为消息里 data=None
/// 的附件引用**从 files 回填字节**（设计 §6.1 混合式——history 只带引用，
/// 字节在请求路径按需装载）。
///
/// 为什么在请求路径而不是 Submit 时：session 构造的 user Message 只带
/// 引用（持久化形状），多步回合每一步的请求、迟到答案续跑、resume 后
/// 续聊都从这里统一拿到字节——单一装载点，封顶（`max_inline_total_mb`）
/// 也只此一处需要执行。已回填的（resume 路径）零成本跳过。
pub struct AttachmentDataProxy {
    inner: Box<dyn ModelProvider>,
    store: SharedStore,
    cfg: AttachmentsConfig,
}

impl AttachmentDataProxy {
    pub fn new(inner: Box<dyn ModelProvider>, store: SharedStore, cfg: AttachmentsConfig) -> Self {
        Self { inner, store, cfg }
    }

    /// 请求路径字节装载 + 总量封顶。覆盖**所有**带附件的消息（user 与
    /// tool——read_file 读图链路的 Tool Message 附件 data 已在内存）：
    /// - data=Some：计入总量；若累计已超 `max_inline_total_mb`，就地丢
    ///   弃（置 None + warn，convert 层随后的跳过分支兜住）——语义对齐
    ///   R5（模型丢图不丢文本）；
    /// - data=None：剩余预算内从 files 回填，超预算跳过 + warn。
    async fn fill(&self, request: &mut GenerateRequest) {
        let Some(store) = self.store.get() else { return };
        let cap = self.cfg.max_inline_total_mb.saturating_mul(1024 * 1024);
        let mut used: u64 = 0;
        for m in request.messages.iter_mut() {
            let Some(atts) = m.attachments.as_mut() else { continue };
            for att in atts.iter_mut() {
                match att.data.as_ref() {
                    None => continue,
                    Some(d) => {
                        if used.saturating_add(d.len() as u64) > cap {
                            tracing::warn!(
                                attachment_id = %att.id,
                                mime = %att.mime,
                                bytes = d.len(),
                                "inline attachment exceeds total cap ({} MB); dropping (模型丢图不丢文本)",
                                self.cfg.max_inline_total_mb
                            );
                            att.data = None;
                            continue;
                        }
                        used = used.saturating_add(d.len() as u64);
                    }
                }
            }
        }
        for m in request.messages.iter_mut() {
            let Some(atts) = m.attachments.as_mut() else { continue };
            for att in atts.iter_mut() {
                if att.data.is_some() {
                    continue;
                }
                if used >= cap {
                    tracing::warn!(
                        attachment_id = %att.id,
                        "请求附件字节超出总封顶（{} MB），跳过装载",
                        self.cfg.max_inline_total_mb
                    );
                    continue;
                }
                let Some(bytes) = read_attachment_by_id(store, &att.id).await else {
                    tracing::warn!(attachment_id = %att.id, "附件字节缺失，convert 层将跳过该附件");
                    continue;
                };
                if used.saturating_add(bytes.len() as u64) > cap {
                    tracing::warn!(attachment_id = %att.id, "装载该附件会超出总封顶，跳过");
                    continue;
                }
                used += bytes.len() as u64;
                att.data = Some(bytes);
            }
        }
    }
}

#[async_trait::async_trait]
impl ModelProvider for AttachmentDataProxy {
    async fn generate(
        &self,
        mut request: GenerateRequest,
    ) -> Result<ModelResponse, openslate_core::error::ProviderError> {
        self.fill(&mut request).await;
        self.inner.generate(request).await
    }

    async fn generate_stream(
        &self,
        mut request: GenerateRequest,
    ) -> tokio::sync::mpsc::Receiver<Result<openslate_core::types::ModelStreamEvent, openslate_core::error::ProviderError>> {
        self.fill(&mut request).await;
        self.inner.generate_stream(request).await
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }
}

// ---------------------------------------------------------------------------
// Submit 门控（§3.3/§3.4/§6.4）
// ---------------------------------------------------------------------------

/// 按 sha256 去重（first-wins）：同一张图导入了两次只发一份——既省
/// token，也避免 `UNIQUE(run_id, sha256)` 在 bind 时冲突。
pub fn dedupe_records(records: Vec<AttachmentRecord>) -> Vec<AttachmentRecord> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::with_capacity(records.len());
    for r in records {
        if seen.contains(&r.sha256) {
            crate::alog!("attachments: {} 与前项同 sha256，去重", r.id);
            continue;
        }
        seen.push(r.sha256.clone());
        out.push(r);
    }
    out
}

/// Submit 门控（设计 §3.3/§3.4/§6.4，纯函数便于矩阵测试）。
///
/// - 纯图像 XOR 纯视频（文本随行）；图+视频混发拒绝；
/// - 图 ≤50 张；总字节 ≤ `max_inline_total_mb`（fail fast 列出实际总量）；
/// - 图像需 image 模态（caps `Some(false)` 拒绝；`None` 未知放行，
///   provider 错误原样透出）；
/// - 视频需 video 模态 + `experimental_video` 开关 + 单文件 ≤
///   `max_inline_video_mb`。
pub fn gate_submit_attachments(
    cfg: &AttachmentsConfig,
    records: &[AttachmentRecord],
    image_cap: Option<bool>,
    video_cap: Option<bool>,
) -> std::result::Result<(), String> {
    let images = records.iter().filter(|r| r.mime.starts_with("image/"));
    let videos = records.iter().filter(|r| r.mime.starts_with("video/"));
    let image_count = images.clone().count();
    let video_count = videos.clone().count();

    if image_count > 0 && video_count > 0 {
        return Err("图像与视频请分开发送".into());
    }
    if image_count > MAX_IMAGES_PER_SUBMIT {
        return Err(format!(
            "图片过多：单次最多 {MAX_IMAGES_PER_SUBMIT} 张（当前 {image_count} 张）"
        ));
    }
    if image_count > 0 && image_cap == Some(false) {
        return Err("当前模型不支持图像输入，请移除图片或切换到多模态模型".into());
    }
    if video_count > 0 {
        if video_cap == Some(false) {
            return Err("当前模型不支持视频输入".into());
        }
        if !cfg.experimental_video {
            return Err("当前版本视频输入为实验性功能，请在设置中开启".into());
        }
        let inline_cap = cfg.max_inline_video_mb * 1024 * 1024;
        for v in videos {
            if v.size_bytes as u64 > inline_cap {
                return Err(format!(
                    "视频过大：内联发送上限 {} MB（「{}」约 {:.1} MB）",
                    cfg.max_inline_video_mb,
                    v.filename.as_deref().unwrap_or("未命名视频"),
                    mb(v.size_bytes as u64)
                ));
            }
        }
    }
    let total_cap = cfg.max_inline_total_mb * 1024 * 1024;
    let total: u64 = records.iter().map(|r| r.size_bytes.max(0) as u64).sum();
    if total > total_cap {
        return Err(format!(
            "附件总大小超过上限 {} MB（当前 {:.1} MB），请减少附件或分开发送",
            cfg.max_inline_total_mb,
            mb(total)
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn sniff_table_covers_whitelist() {
        // jpeg / png / gif / webp / mkv / mov(qt) / mp4(isom)
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(
            sniff_mime(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("image/png")
        );
        assert_eq!(sniff_mime(b"GIF89a..."), Some("image/gif"));
        let mut webp = vec![0, 0, 0, 0, 0, 0, 0, 0];
        webp[..4].copy_from_slice(b"RIFF");
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_mime(&webp), Some("image/webp"));
        assert_eq!(
            sniff_mime(&[0x1A, 0x45, 0xDF, 0xA3, 0x00]),
            Some("video/x-matroska")
        );
        let mut ftyp = vec![0, 0, 0, 0];
        ftyp.extend_from_slice(b"ftypqt  ");
        assert_eq!(sniff_mime(&ftyp), Some("video/quicktime"));
        let mut ftyp = vec![0, 0, 0, 0];
        ftyp.extend_from_slice(b"ftypisom");
        assert_eq!(sniff_mime(&ftyp), Some("video/mp4"));
        // heic（ftyp heic）不在白名单——Kotlin 侧负责预转码。
        let mut heic = vec![0, 0, 0, 0];
        heic.extend_from_slice(b"ftypheic");
        assert_eq!(sniff_mime(&heic), None, "HEIC 必须被拒绝（Kotlin 预转码）");
        assert_eq!(sniff_mime(b""), None);
        assert_eq!(sniff_mime(b"plain text"), None);
    }

    #[test]
    fn ext_map_matches_store_mapping() {
        for mime in [
            "image/jpeg",
            "image/png",
            "image/webp",
            "image/gif",
            "video/mp4",
            "video/x-matroska",
            "video/quicktime",
        ] {
            assert!(ext_for_mime(mime).is_some(), "{mime} 必须有扩展名映射");
        }
        assert_eq!(ext_for_mime("image/heic"), None);
    }

    fn record(id: &str, mime: &str, sha: &str, size: i64) -> AttachmentRecord {
        AttachmentRecord {
            id: id.into(),
            run_id: None,
            sha256: sha.into(),
            mime: mime.into(),
            filename: Some(format!("{id}.bin")),
            size_bytes: size,
            created_at: 0,
        }
    }

    fn cfg(experimental_video: bool) -> AttachmentsConfig {
        AttachmentsConfig {
            experimental_video,
            ..AttachmentsConfig::default()
        }
    }

    #[test]
    fn gate_matrix() {
        let img = |n: usize, size: i64| (0..n).map(|i| record(&format!("i{i}"), "image/jpeg", &format!("s{i}"), size)).collect::<Vec<_>>();
        let vid = |n: usize, size: i64| (0..n).map(|i| record(&format!("v{i}"), "video/mp4", &format!("t{i}"), size)).collect::<Vec<_>>();

        // 纯文本模型发图被拒；多模态放行；caps 未知放行。
        assert_eq!(
            gate_submit_attachments(&cfg(false), &img(1, 1024), Some(false), None).unwrap_err(),
            "当前模型不支持图像输入，请移除图片或切换到多模态模型"
        );
        assert!(gate_submit_attachments(&cfg(false), &img(1, 1024), Some(true), None).is_ok());
        assert!(gate_submit_attachments(&cfg(false), &img(1, 1024), None, None).is_ok());

        // 混发被拒。
        let mut mixed = img(1, 1024);
        mixed.extend(vid(1, 1024));
        assert_eq!(
            gate_submit_attachments(&cfg(false), &mixed, Some(true), Some(true)).unwrap_err(),
            "图像与视频请分开发送"
        );

        // 视频开关两态：关 → 实验提示；开（caps 有 video）→ 放行。
        assert_eq!(
            gate_submit_attachments(&cfg(false), &vid(1, 1024), None, Some(true)).unwrap_err(),
            "当前版本视频输入为实验性功能，请在设置中开启"
        );
        assert!(gate_submit_attachments(&cfg(true), &vid(1, 1024), None, Some(true)).is_ok());
        // caps 无 video → 无论开关都拒。
        assert_eq!(
            gate_submit_attachments(&cfg(true), &vid(1, 1024), None, Some(false)).unwrap_err(),
            "当前模型不支持视频输入"
        );

        // 50 张边界：50 放行 / 51 拒。
        assert!(gate_submit_attachments(&cfg(false), &img(50, 1), Some(true), None).is_ok());
        assert!(gate_submit_attachments(&cfg(false), &img(51, 1), Some(true), None)
            .unwrap_err()
            .contains("最多 50 张"));

        // 总字节 25MB 边界：恰 25MB 放行 / 超一字节拒（列出实际总量）。
        let cap = 25 * 1024 * 1024;
        assert!(gate_submit_attachments(&cfg(false), &img(1, cap as i64), Some(true), None).is_ok());
        let err = gate_submit_attachments(&cfg(false), &img(1, cap as i64 + 1), Some(true), None)
            .unwrap_err();
        assert!(err.contains("附件总大小超过上限 25 MB"), "{err}");

        // 内联视频 20MB 边界（开关开 + video caps 就位）。
        let vcap = 20 * 1024 * 1024;
        assert!(gate_submit_attachments(&cfg(true), &vid(1, vcap as i64), None, Some(true)).is_ok());
        let err = gate_submit_attachments(&cfg(true), &vid(1, vcap as i64 + 1), None, Some(true))
            .unwrap_err();
        assert!(err.contains("内联发送上限 20 MB"), "{err}");
    }

    #[test]
    fn dedupe_by_sha_keeps_first() {
        let records = vec![
            record("a", "image/jpeg", "same", 1),
            record("b", "image/jpeg", "same", 1),
            record("c", "image/png", "other", 2),
        ];
        let out = dedupe_records(records);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "a");
        assert_eq!(out[1].id, "c");
    }

    /// 归一化行为：大 JPEG 缩到 max_image_dim 并重编码为 JPEG；
    /// 小 PNG passthrough 保 PNG。
    #[test]
    fn normalize_image_resizes_and_reencodes() {
        // 3000×2000 纯色 JPEG → 缩到 max_image_dim=2048（长边，CatmullRom
        // 快档——输出维度断言与滤波器无关）。
        let img = image::RgbImage::from_pixel(3000, 2000, image::Rgb([200, 30, 30]));
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .unwrap();
        let cfg = AttachmentsConfig::default();
        let n = normalize_image(&jpeg, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.mime, "image/jpeg");
        let decoded = image::load_from_memory(&n.data).unwrap();
        assert_eq!(decoded.width().max(decoded.height()), 2048);
        assert!(n.data.len() <= 5 * 1024 * 1024);
        // 对比信息：orig = 源尺寸，处理后 = 缩放后，transcode。
        assert_eq!((n.orig_width, n.orig_height), (3000, 2000));
        assert_eq!((n.width, n.height), (2048, 1365));
        assert_eq!(n.processed, "transcode");

        // 小 PNG（≤dim、≤5MB）→ passthrough 字节不变（尺寸 orig=out）。
        let mut png = Vec::new();
        let tiny = image::RgbImage::from_pixel(64, 64, image::Rgb([1, 2, 3]));
        image::DynamicImage::ImageRgb8(tiny)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let n = normalize_image(&png, "image/png", &cfg).unwrap();
        assert_eq!(n.mime, "image/png");
        assert_eq!(n.data, png, "PNG passthrough 必须逐字节保留");
        assert_eq!((n.orig_width, n.orig_height), (64, 64));
        assert_eq!((n.width, n.height), (64, 64));
        assert_eq!(n.processed, "passthrough");
    }

    /// 快速通道（零转码）：小 JPEG（≤dim、≤5MB）字节原样 passthrough
    /// ——不解码不重采样不重编码（图片导入慢的头号修复点；小 PNG 同款
    /// 路径已在上一测试覆盖）。
    #[test]
    fn small_jpeg_passthrough_zero_transcode() {
        let img = image::RgbImage::from_pixel(400, 300, image::Rgb([10, 200, 30]));
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .unwrap();
        let cfg = AttachmentsConfig::default();
        let n = normalize_image(&jpeg, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.mime, "image/jpeg");
        assert_eq!(n.data, jpeg, "小 JPEG 快速通道必须逐字节保留（零转码）");
        assert_eq!((n.orig_width, n.orig_height), (400, 300));
        assert_eq!((n.width, n.height), (400, 300), "passthrough 尺寸不变");
        assert_eq!(n.processed, "passthrough");
    }

    /// 快速通道的体积上限分支：JPEG 长边 ≤2048 但 >5MB（高分辨率噪点
    /// 高质量压缩的现实形态）→ 仍走全解码重编码压回 ≤5MB。
    #[test]
    fn oversized_small_dim_jpeg_still_transcodes() {
        // 确定性伪随机噪点（LCG）：2048×1600 q97 噪点必然 >5MB。
        let (w, h) = (2048u32, 1600u32);
        let mut px = image::RgbImage::new(w, h);
        let mut state = 0x2545F4914F6CDD1Du64;
        for y in 0..h {
            for x in 0..w {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let v = (state >> 33) as u8;
                px.put_pixel(x, y, image::Rgb([v, (v >> 1) as u8 | 0x80, 255 - v]));
            }
        }
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 97);
        image::DynamicImage::ImageRgb8(px)
            .write_with_encoder(enc)
            .unwrap();
        assert!(
            jpeg.len() > 5 * 1024 * 1024,
            "构造前提：噪点 q97 必须超 5MB（实际 {}B）",
            jpeg.len()
        );
        let cfg = AttachmentsConfig::default();
        let n = normalize_image(&jpeg, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.mime, "image/jpeg");
        assert_ne!(n.data, jpeg, "超 5MB 不 passthrough，必须重编码");
        assert!(n.data.len() <= 5 * 1024 * 1024, "出口硬校验 ≤5MB");
        let decoded = image::load_from_memory(&n.data).unwrap();
        assert_eq!(decoded.width(), w, "长边 ≤2048 不缩放");
        assert_eq!((n.orig_width, n.orig_height), (w, h));
        assert_eq!((n.width, n.height), (w, h), "≤dim 不缩放，尺寸不变");
        assert_eq!(n.processed, "transcode");
    }

    /// 确定性伪随机噪点 JPEG（LCG，同款像素源）：高熵不可压，用于
    /// 触发迭代压缩。`q` 控制初始质量。
    fn noise_jpeg(w: u32, h: u32, q: u8) -> Vec<u8> {
        let mut px = image::RgbImage::new(w, h);
        let mut state = 0x2545F4914F6CDD1Du64;
        for y in 0..h {
            for x in 0..w {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let v = (state >> 33) as u8;
                px.put_pixel(x, y, image::Rgb([v, (v >> 1) as u8 | 0x80, 255 - v]));
            }
        }
        let mut jpeg = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, q);
        image::DynamicImage::ImageRgb8(px)
            .write_with_encoder(enc)
            .unwrap();
        jpeg
    }

    /// 迭代压缩达标：q85 超用户上限 → 质量降档（不缩边）压到达标。
    /// 1550×1200 噪点、上限 2MB：q85 ≈ 2.7MB 超限，q55 ≈ 1.8MB 达标。
    #[test]
    fn transcode_iterates_quality_ladder_to_meet_user_cap() {
        let jpeg = noise_jpeg(1550, 1200, 97);
        let cap = 2 * 1024 * 1024;
        // 构造前提：q85 直接编码必须超 2MB（否则阶梯根本不会触发）。
        let q85 = {
            let img = image::load_from_memory(&jpeg).unwrap();
            let mut buf = Vec::new();
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85);
            image::DynamicImage::ImageRgb8(img.to_rgb8())
                .write_with_encoder(enc)
                .unwrap();
            buf.len()
        };
        assert!(q85 > cap, "前提：q85={q85}B 必须超 cap {cap}B");

        let cfg = AttachmentsConfig {
            max_image_dim: 2048,
            max_image_mb: 2,
            ..AttachmentsConfig::default()
        };
        let n = normalize_image(&jpeg, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.processed, "transcode");
        assert!(
            n.data.len() <= cap,
            "迭代压缩必须达标：{}B ≤ {cap}B",
            n.data.len()
        );
        assert!(n.quality < 85, "质量必须降档（q={}）", n.quality);
        assert!(n.attempts >= 2, "至少两次编码尝试（{}）", n.attempts);
        // 尺寸不缩（1550 ≤ 用户 dim 2048，质量档位内达标）。
        assert_eq!((n.width, n.height), (1550, 1200));
        assert_eq!((n.orig_width, n.orig_height), (1550, 1200));
        assert!(image::load_from_memory(&n.data).is_ok());
    }

    /// 地板：原图已小到缩无可缩（1100px < ×0.85 目标）且全质量档超
    /// 用户上限 → 取最后结果 + warn，不失败、不缩边。
    #[test]
    fn transcode_floor_returns_last_result_without_error() {
        let jpeg = noise_jpeg(1100, 1100, 97);
        let cap = 1024 * 1024; // 1MB
        let cfg = AttachmentsConfig {
            max_image_dim: 2048,
            max_image_mb: 1,
            ..AttachmentsConfig::default()
        };
        let n = normalize_image(&jpeg, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.processed, "transcode", "地板不失败");
        assert_eq!(n.attempts, 3, "首轮质量阶梯 85/70/55 各一次后即地板");
        assert_eq!(n.quality, 55, "取最后档位");
        assert_eq!((n.width, n.height), (1100, 1100), "缩无可缩，尺寸不变");
        assert!(
            n.data.len() > cap || n.data.len() <= cap, // 地板结果可超上限（warn 已示警）
            "地板结果不保证达标，但必须可解码"
        );
        assert!(image::load_from_memory(&n.data).is_ok());
    }

    /// 快速通道边界按用户 dim 走：dim=1024 时 1600px 超界走 transcode
    ///（缩到 ≤1024）；900px 界内 passthrough。
    #[test]
    fn fast_path_boundary_follows_user_dim() {
        let cfg = AttachmentsConfig {
            max_image_dim: 1024,
            ..AttachmentsConfig::default()
        };
        // 1600×1200 小文件（字节远低于大小上限）——只因超 dim 而转码。
        let big = noise_jpeg(1600, 1200, 60);
        let n = normalize_image(&big, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.processed, "transcode");
        assert_eq!(n.orig_width, 1600);
        assert_eq!(n.width, 1024, "等比缩到用户 dim");
        assert_eq!(n.height, 768);
        assert!(n.data.len() as u64 <= cfg.max_image_mb * 1024 * 1024);

        // 900×700 界内且字节低于上限 → passthrough（尺寸原样）。
        let small = noise_jpeg(900, 700, 60);
        let n = normalize_image(&small, "image/jpeg", &cfg).unwrap();
        assert_eq!(n.processed, "passthrough");
        assert_eq!(n.data, small);
        assert_eq!((n.width, n.height), (900, 700));
        assert_eq!(n.attempts, 0, "passthrough 零编码");
    }

    /// 畸形 JPEG（header 无法读尺寸）→ 回落全解码路径给准确错误，
    /// 绝不因快速通道失败而拒绝。
    #[test]
    fn truncated_jpeg_falls_back_to_decoder_error() {
        // 有 SOI/APP0 头但无完整帧数据的 JPEG。
        let broken = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00];
        let cfg = AttachmentsConfig::default();
        let err = normalize_image(&broken, "image/jpeg", &cfg).unwrap_err();
        assert!(
            err.to_string().contains("图像解码失败"),
            "畸形 JPEG 报解码错误：{err}"
        );
    }

    // ── ingest 全流程（真实文件库）────────────────────────────────────

    /// 文件库（须在异步上下文内调用）。
    async fn file_store(dir: &std::path::Path) -> SqliteStore {
        let db = dir.join("openslate.db").display().to_string();
        let s = SqliteStore::new(&db).await.unwrap();
        s.run_migrations().await.unwrap();
        s
    }

    fn tiny_png_bytes() -> Vec<u8> {
        let mut png = Vec::new();
        let img = image::RgbImage::from_pixel(32, 32, image::Rgb([9, 8, 7]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    #[test]
    fn import_png_passthrough_and_video_passthrough() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = file_store(dir.path()).await;
        let cfg = AttachmentsConfig::default();

        // PNG：小图 passthrough，mime 保持 png，文件落 files/{sha}.png。
        let src = dir.path().join("shot.png");
        std::fs::write(&src, tiny_png_bytes()).unwrap();
        let imported = import_attachment(&store, &cfg, &src, "image/png", Some("shot.png"))
            .await
            .unwrap();
        assert_eq!(imported.dto.mime, "image/png");
        assert_eq!(imported.dto.filename.as_deref(), Some("shot.png"));
        // 对比信息（chip 契约）：passthrough 态 orig=out、字节相等。
        assert_eq!(
            (imported.orig_width, imported.orig_height),
            (Some(32), Some(32))
        );
        assert_eq!((imported.width, imported.height), (Some(32), Some(32)));
        assert_eq!(imported.orig_bytes as usize, tiny_png_bytes().len());
        assert_eq!(imported.size_bytes, imported.orig_bytes);
        assert_eq!(imported.processed, "passthrough");
        let row = store.get_attachment(&imported.dto.id).await.unwrap().unwrap();
        assert!(row.run_id.is_none(), "导入后暂存（run_id NULL）");
        assert_eq!(row.size_bytes as usize, tiny_png_bytes().len());
        // 文件就位且可读回。
        let sha = row.sha256.clone();
        assert!(read_attachment_bytes(&store, &sha, "image/png").is_some());

        // MP4（伪 ftyp 头）：passthrough 不转码；尺寸字段全 None
        //（嗅探层不解析容器头，拿不到就不给），orig=处理后字节。
        let mut mp4 = vec![0, 0, 0, 0];
        mp4.extend_from_slice(b"ftypisom");
        mp4.extend_from_slice(&[0u8; 1024]);
        let src = dir.path().join("clip.mp4");
        std::fs::write(&src, &mp4).unwrap();
        let imported = import_attachment(&store, &cfg, &src, "video/mp4", None)
            .await
            .unwrap();
        assert_eq!(imported.dto.mime, "video/mp4");
        assert_eq!(imported.size_bytes as usize, mp4.len());
        assert_eq!(imported.processed, "video_passthrough");
        assert_eq!(imported.orig_bytes as usize, mp4.len());
        assert!(imported.orig_width.is_none(), "视频无容器头尺寸");
        assert!(imported.orig_height.is_none());
        assert!(imported.width.is_none());
        assert!(imported.height.is_none());

        // declared_mime 说谎（image/png 实为 mp4）→ 按嗅探走。
        let imported = import_attachment(&store, &cfg, &src, "image/png", None)
            .await
            .unwrap();
        assert_eq!(imported.dto.mime, "video/mp4", "不信任 declared_mime");
        });
    }

    #[test]
    fn import_rejects_unknown_and_oversize() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = file_store(dir.path()).await;
        let cfg = AttachmentsConfig::default();

        let src = dir.path().join("blob.bin");
        std::fs::write(&src, b"not an image at all").unwrap();
        let err = import_attachment(&store, &cfg, &src, "image/png", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("无法识别"), "{err}");

        // 入口防呆：50MB 固定常量（2026-10-21 起 max_image_mb 语义迁移
        // 为处理后上限，不再挡源文件）。塞 51MB 假 jpeg 头 → 拒收。
        let mut big = vec![0xFF, 0xD8, 0xFF];
        big.extend(std::iter::repeat(0u8).take(51 * 1024 * 1024));
        std::fs::write(&src, &big).unwrap();
        let err = import_attachment(&store, &cfg, &src, "image/jpeg", None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("图片过大：原始文件上限 50 MB"),
            "{err}"
        );

        // 旧语义回归：11MB 假 jpeg（曾按 max_image_mb=10 拒收）不再走
        // 入口拒绝——假头无帧数据，落到解码器报准确错误（真实 11MB
        // 相机原图则放行走迭代压缩）。
        let mut big = vec![0xFF, 0xD8, 0xFF];
        big.extend(std::iter::repeat(0u8).take(11 * 1024 * 1024));
        std::fs::write(&src, &big).unwrap();
        let err = import_attachment(&store, &cfg, &src, "image/jpeg", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("图像解码失败"), "{err}");
        });
    }

    #[test]
    fn refill_caps_total_and_skips_missing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let store = file_store(dir.path()).await;
        // 两个 ~100KB 的 PNG 附件；封顶调到 ~150KB：第一个回填、第二个
        // 超封顶跳过（warn），不失败。
        let cfg = AttachmentsConfig {
            max_inline_total_mb: 1,
            ..AttachmentsConfig::default()
        };
        let cap = cfg.max_inline_total_mb * 1024 * 1024;
        let _ = cap;
        let png = tiny_png_bytes();
        let mk = |id: &str| {
            openslate_core::types::Attachment {
                id: id.into(),
                mime: "image/png".into(),
                filename: None,
                data: None,
            }
        };
        // 直接造大文件走真实路径：用重复字节把文件撑过封顶的一半。
        let mut payloads: Vec<Vec<u8>> = Vec::new();
        for i in 0..2 {
            let mut data = png.clone();
            data.extend(std::iter::repeat((i + 1) as u8).take(600 * 1024));
            payloads.push(data);
        }
        for (i, data) in payloads.iter().enumerate() {
            let id = format!("att-{i}");
            let mut h = Sha256::new();
            h.update(data);
            let sha = hex(&h.finalize());
            store
                .insert_attachment(&id, None, &sha, "image/png", None, data.len() as i64, data, 0)
                .await
                .unwrap();
        }
        let mut messages = vec![Message {
            attachments: Some(vec![mk("att-0"), mk("att-1"), mk("att-missing")]),
            role: openslate_core::types::MessageRole::User,
            content: "带图".into(),
            tool_call_id: None,
            name: None,
            tool_calls: None,
            reasoning_content: None,
        }];
        let filled = refill_message_attachment_data(&store, &cfg, &mut messages).await;
        let atts = messages[0].attachments.as_ref().unwrap();
        assert_eq!(filled, 1, "只回填第一个（第二个超 1MB 总封顶）");
        assert!(atts[0].data.is_some());
        assert!(atts[1].data.is_none(), "超封顶不回填");
        assert!(atts[2].data.is_none(), "缺行不回填不失败");
        });
    }

}
