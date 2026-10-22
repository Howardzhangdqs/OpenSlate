//! Filesystem MCP server: `read_file` / `write_file` sandboxed to a workspace
//! root.
//!
//! Faithful port of the legacy `ReadFileTool` / `WriteFileTool` from
//! `openslate-core::tool` (parameters, output formats and error messages are
//! preserved verbatim). Differences inherent to MCP:
//!
//! - Business failures (missing file, sandbox rejection, I/O errors) return
//!   `Ok(CallToolResult::error(...))` so the error text is fed back to the
//!   LLM instead of surfacing as a protocol error.
//! - Structurally invalid arguments (e.g. an empty `path`) return
//!   `Err(McpError::invalid_params(...))`.
//!
//! `read_file` 读图（2026-10）：字节读 + magic-byte 嗅探——图像
//! （JPEG/PNG/WebP/GIF）经归一化（长边 ≤2048 / ≤5MB / JPEG q85，小 PNG
//! passthrough）后以 `[text 说明, image block]` 双块返回；非 UTF-8 的
//! 视频/二进制给结构化错误提示（视频不支持直接读取）。归一化在本
//! crate（host 侧）执行：mcp-host 与 CLI in-process 共用同一实现，避免
//! 原图 base64 走 JSON-RPC。

use std::path::{Path, PathBuf};

use base64::Engine as _;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::tool::ToolRouter,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
};

// ── sandbox path resolution (ported from openslate-core tool.rs) ────────────

/// Sandbox violation or execution failure while resolving a target path.
/// Mirrors `ToolError::SecurityError` / `ToolError::ExecutionError` from the
/// legacy tools; both map to a tool-level error result.
#[derive(Debug)]
enum PathError {
    /// Sandbox violation (path traversal, outside workspace).
    Security(&'static str),
    /// I/O or resolution failure.
    Execution(String),
}

impl PathError {
    fn message(&self) -> String {
        match self {
            PathError::Security(msg) => (*msg).to_owned(),
            PathError::Execution(msg) => msg.clone(),
        }
    }
}

/// Validate that a target path is within the workspace root and return the
/// resolved full path.
///
/// Rules:
/// 1. Any `..` component → rejected ("Path traversal not allowed").
/// 2. Unresolvable workspace root → execution error.
/// 3. Absolute targets are used as-is; relative targets are joined onto the
///    canonical root (absolute paths inside the workspace are allowed).
/// 4. If the target does not exist yet, the longest *existing* ancestor is
///    canonicalized and the remaining components appended (this is what lets
///    `write_file` create new files — while still resolving symlinks in the
///    existing part of the path, so a symlinked directory inside the
///    workspace cannot smuggle a write past the containment check).
/// 5. Targets not under the canonical root → "Path is outside workspace".
fn resolve_workspace_path(workspace_root: &Path, target_path: &str) -> Result<PathBuf, PathError> {
    if target_path.contains("..") {
        return Err(PathError::Security("Path traversal not allowed"));
    }

    let canonical_root = workspace_root
        .canonicalize()
        .map_err(|e| PathError::Execution(format!("Failed to resolve workspace root: {}", e)))?;

    let full_path = if target_path.starts_with('/') {
        PathBuf::from(target_path)
    } else {
        canonical_root.join(target_path)
    };

    let resolved = match full_path.canonicalize() {
        Ok(canonical) => canonical,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => canonicalize_via_ancestor(&full_path)
            .map_err(|e| {
                PathError::Execution(format!("Failed to resolve path '{}': {}", target_path, e))
            })?,
        Err(e) => {
            return Err(PathError::Execution(format!(
                "Failed to resolve path '{}': {}",
                target_path, e
            )));
        }
    };

    if !resolved.starts_with(&canonical_root) {
        return Err(PathError::Security("Path is outside workspace"));
    }

    Ok(resolved)
}

/// Canonicalize the longest existing ancestor of `path` and append the
/// remaining components.
///
/// Unlike a plain lexical fallback, this resolves symlinks in the part of
/// the path that already exists, so containment checks see where the path
/// *actually* leads. Example: with `ws/link -> /tmp/evil` (a symlinked
/// directory), `ws/link/new.txt` resolves to `/tmp/evil/new.txt` and is
/// rejected instead of passing the check lexically.
fn canonicalize_via_ancestor(path: &Path) -> std::io::Result<PathBuf> {
    let mut existing = path.to_path_buf();
    let mut remainder: Vec<std::ffi::OsString> = Vec::new();
    loop {
        match existing.canonicalize() {
            Ok(canonical) => {
                let mut resolved = canonical;
                for component in remainder.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Climb one component up; the filesystem root always exists,
                // so the loop terminates.
                let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                    return Err(e);
                };
                remainder.push(name);
                if !existing.pop() {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

// ── MCP server ──────────────────────────────────────────────────────────────

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileParams {
    #[schemars(description = "File path (absolute or relative to workspace)")]
    pub path: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WriteFileParams {
    #[schemars(description = "File path (absolute or relative to workspace)")]
    pub path: String,
    #[schemars(description = "Content to write to the file")]
    pub content: String,
}

/// In-process MCP server exposing `read_file` / `write_file` inside a
/// workspace sandbox.
#[derive(Clone)]
pub struct FsServer {
    root: PathBuf,
    tool_router: ToolRouter<Self>,
}

fn error_text(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(msg.into())])
}

fn success_text(content: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(content)])
}

// ── read_file 读图（2026-10）：magic-byte 嗅探 + 精简归一化 ─────────────────
//
// 归一化上移到 host 侧（mcp-host / CLI in-process 同一实现）：相机原图
// （动辄 10MB+）先缩放重编码再出口，避免 5MB 级 base64 走 JSON-RPC。
// 语义与 openslate-mobile attachments::normalize_image 对齐（常量同源），
// 但本 crate 不能依赖 openslate-mobile（host/Termux 进程），故精简复刻。

/// 归一化长边上限（mobile 侧 `max_image_dim` 默认值）。
const MAX_IMAGE_DIM: u32 = 2048;
/// 归一化出口字节 / PNG passthrough 上限。
const MAX_NORMALIZED_IMAGE_BYTES: usize = 5 * 1024 * 1024;
/// 归一化出口像素总数上限（防呆断言）。
const MAX_IMAGE_PIXELS: u64 = 6000 * 6000;
/// JPEG 重编码质量（与 mobile 侧一致）。
const JPEG_QUALITY: u8 = 85;

/// magic-byte 嗅探：仅识别 read_file 能处理的图像格式（jpeg/png/webp/gif）。
/// 返回 `None` = 非图像（调用方再走 UTF-8 文本 / 二进制提示分支）。
/// 视频（mp4/mkv/mov）在本期不支持读取，嗅出来只用于提示文案。
fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 && data[2] == 0xFF {
        return Some("image/jpeg");
    }
    if data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    None
}

/// 嗅探视频容器（mp4/mkv/mov）——仅用于二进制提示文案，不读取。
fn sniff_video(data: &[u8]) -> Option<&'static str> {
    if data.len() >= 4 && data[..4] == [0x1A, 0x45, 0xDF, 0xA3] {
        return Some("video/x-matroska");
    }
    if data.len() >= 12 && &data[4..8] == b"ftyp" {
        let brand = &data[8..12];
        return if brand == b"qt  " {
            Some("video/quicktime")
        } else {
            Some("video/mp4")
        };
    }
    None
}

/// 图像归一化（对齐 mobile attachments::normalize_image）：
///
/// - **快速通道（零解码零编码零重采样）**：JPEG/PNG header-only 读尺寸
///   （不碰像素），长边 ≤2048 且 ≤5MB → **字节原样 passthrough**
///   （JPEG 本就在出口白名单内；EXIF 方向不处理，字节原样保留，方向
///   解释交给 provider——全解码路径同样不处理，行为一致）；
/// - 其余（超尺寸/超体积的 jpeg/png、webp、gif）→ 缩到长边 ≤2048
///   （只缩不放，`CatmullRom` 快档——纯 Rust 无 SIMD 下比 Lanczos3 快
///   数倍，2048 降采样画质对视觉模型够用）+ JPEG q85 重编码；动图取
///   首帧；
/// - 出口硬校验：≤5MB、像素 ≤6000²（两条路径都执行；2048+q85 实际
///   ~0.3–2MB，断言防呆）。
///
/// 分段耗时打 stderr（本 crate 零日志依赖；stdio transport 下 stderr
/// 是安全的诊断通道，stdout 才是协议通道）：`attachment timing: …`，
/// 与 mobile 侧 alog 格式统一。
fn normalize_image(bytes: &[u8], sniffed: &'static str) -> Result<(&'static str, Vec<u8>), String> {
    let t_start = std::time::Instant::now();

    // ── 快速通道：JPEG/PNG header 判定后直接 passthrough ──────────
    if matches!(sniffed, "image/jpeg" | "image/png") {
        if let Some((w, h)) = header_dimensions(bytes) {
            let t_header = t_start.elapsed();
            if w.max(h) <= MAX_IMAGE_DIM && bytes.len() <= MAX_NORMALIZED_IMAGE_BYTES {
                if (w as u64) * (h as u64) > MAX_IMAGE_PIXELS {
                    return Err(format!("图像像素超限（{w}×{h} > 6000²）"));
                }
                eprintln!(
                    "attachment timing: sniff_header={}ms decode=0ms resize=0ms encode=0ms \
                     fast_path={sniffed}_passthrough {w}x{h} total={}ms",
                    t_header.as_millis(),
                    t_start.elapsed().as_millis()
                );
                return Ok((sniffed, bytes.to_vec()));
            }
        }
        // header 解析失败（畸形/截断）→ 全解码路径给准确错误。
    }

    // ── 全解码路径（超尺寸/超体积 JPEG、PNG，webp/gif 转码）────────
    let t_header = t_start.elapsed();
    let img = image::load_from_memory(bytes)
        .map_err(|e| format!("图像解码失败（{sniffed}）：{e}"))?;
    let t_decode = t_start.elapsed();
    let max_dim = img.width().max(img.height());

    let out = if max_dim > MAX_IMAGE_DIM {
        // CatmullRom（见函数文档）：纯 Rust resize 快档，真机（P10 lite
        // 级 SoC）上 Lanczos3 数秒太贵。
        img.resize(
            MAX_IMAGE_DIM,
            MAX_IMAGE_DIM,
            image::imageops::FilterType::CatmullRom,
        )
    } else {
        img
    };
    let t_resize = t_start.elapsed();
    let mut jpeg = Vec::with_capacity(256 * 1024);
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY);
    out.to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| format!("JPEG 重编码失败：{e}"))?;
    let t_encode = t_start.elapsed();

    let (w, h) = (out.width() as u64, out.height() as u64);
    if w * h > MAX_IMAGE_PIXELS {
        return Err(format!("归一化后像素超限（{w}×{h} > 6000²）"));
    }
    if jpeg.len() > MAX_NORMALIZED_IMAGE_BYTES {
        return Err(format!(
            "归一化后图像超 5MB（{:.1} MB）",
            jpeg.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    eprintln!(
        "attachment timing: sniff_header={}ms decode={}ms resize={}ms encode={}ms \
         transcode {sniffed} {w}x{h} {}B→{}B total={}ms",
        t_header.as_millis(),
        (t_decode - t_header).as_millis(),
        (t_resize - t_decode).as_millis(),
        (t_encode - t_resize).as_millis(),
        bytes.len(),
        jpeg.len(),
        t_start.elapsed().as_millis()
    );
    Ok(("image/jpeg", jpeg))
}

/// header-only 尺寸读取（不解码像素；SOF/IHDR/容器头）。畸形/截断返回
/// `None`——调用方回落全解码路径（解码器给准确错误）。
fn header_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

#[tool_router]
impl FsServer {
    /// Create a server confined to the given workspace root. The root is
    /// canonicalized once; if that fails (root missing) the raw path is kept
    /// so the failure surfaces on first use, like the legacy tools.
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root
            .as_ref()
            .canonicalize()
            .unwrap_or_else(|_| root.as_ref().to_path_buf());
        Self {
            root,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Read a file at the given path (within workspace). UTF-8 text files return their content; image files (JPEG/PNG/WebP/GIF) are normalized (≤2048px, ≤5MB) and returned as image content the model can see directly")]
    async fn read_file(
        &self,
        Parameters(args): Parameters<ReadFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params("'path' must not be empty", None));
        }

        let full_path = match resolve_workspace_path(&self.root, &args.path) {
            Ok(p) => p,
            Err(e) => return Ok(error_text(e.message())),
        };

        // 字节读（不再 read_to_string）：UTF-8 校验挪到文本分支，图像/
        // 二进制按 magic-byte 嗅探分流。
        let bytes = match tokio::fs::read(&full_path).await {
            Ok(b) => b,
            Err(e) => {
                return Ok(error_text(format!("Failed to read '{}': {}", args.path, e)));
            }
        };

        // ── 图像分支：归一化 + [text, image] 双 block ──────────────────
        if let Some(sniffed) = sniff_image(&bytes) {
            let filename = full_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(args.path.as_str());
            return match normalize_image(&bytes, sniffed) {
                Ok((mime, data)) => {
                    let text = format!(
                        "[image file: {}, {}, {} bytes (normalized from {} bytes) \
— the image itself is attached below; no need to read it again]",
                        filename, mime, data.len(), bytes.len()
                    );
                    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                    Ok(CallToolResult::success(vec![
                        ContentBlock::text(text),
                        ContentBlock::image(b64, mime),
                    ]))
                }
                Err(e) => Ok(error_text(format!(
                    "Failed to read '{}' as image: {}",
                    args.path, e
                ))),
            };
        }

        // ── 文本分支：UTF-8 校验（对齐旧 read_to_string 行为）──────────
        match String::from_utf8(bytes) {
            Ok(content) => Ok(success_text(content)),
            Err(err) => {
                let bytes = err.into_bytes();
                let hint = match sniff_video(&bytes) {
                    Some(mime) => format!("binary file ({} bytes, {})", bytes.len(), mime),
                    None => format!("binary or non-UTF-8 file ({} bytes)", bytes.len()),
                };
                Ok(error_text(format!(
                    "Failed to read '{}': {} — only UTF-8 text and images \
(JPEG/PNG/WebP/GIF) can be read directly; videos are not supported",
                    args.path, hint
                )))
            }
        }
    }

    #[tool(description = "Write content to a file within the workspace")]
    async fn write_file(
        &self,
        Parameters(args): Parameters<WriteFileParams>,
    ) -> Result<CallToolResult, McpError> {
        if args.path.trim().is_empty() {
            return Err(McpError::invalid_params("'path' must not be empty", None));
        }

        let full_path = match resolve_workspace_path(&self.root, &args.path) {
            Ok(p) => p,
            Err(e) => return Ok(error_text(e.message())),
        };

        // Create parent directories if they don't exist (legacy behavior).
        if let Some(parent) = full_path.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return Ok(error_text(format!("Failed to create directories: {}", e)));
        }

        if let Err(e) = tokio::fs::write(&full_path, &args.content).await {
            return Ok(error_text(format!(
                "Failed to write '{}': {}",
                args.path, e
            )));
        }

        Ok(success_text(format!(
            "Successfully wrote {} bytes to '{}'",
            args.content.len(),
            args.path
        )))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FsServer {}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{first_text, is_error, spawn_server};
    use rmcp::model::CallToolRequestParams;

    async fn client_in(
        dir: &tempfile::TempDir,
    ) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
        spawn_server(FsServer::new(dir.path())).await
    }

    async fn call(
        client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
        tool: &str,
        args: rmcp::model::JsonObject,
    ) -> rmcp::model::CallToolResult {
        client
            .call_tool(CallToolRequestParams::new(tool.to_owned()).with_arguments(args))
            .await
            .expect("call_tool should not be a protocol error")
    }

    // ── read_file ──

    #[tokio::test]
    async fn read_file_success() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "test.txt"})).await;
        assert!(!is_error(&result));
        assert_eq!(first_text(&result), "hello world");
    }

    #[tokio::test]
    async fn read_file_absolute_path_in_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("abs.txt"), "abs content").unwrap();

        let client = client_in(&dir).await;
        let abs = dir.path().join("abs.txt");
        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": abs.to_str().unwrap()}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(first_text(&result), "abs content");
    }

    #[tokio::test]
    async fn read_file_nested_subdir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), "deep").unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "a/b/c.txt"})).await;
        assert!(!is_error(&result));
        assert_eq!(first_text(&result), "deep");
    }

    #[tokio::test]
    async fn read_file_nonexistent_is_business_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": "nonexistent_file.txt"}),
        )
        .await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).contains("Failed to read"),
            "{}",
            first_text(&result)
        );
    }

    #[tokio::test]
    async fn read_file_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": "../../../etc/passwd"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path traversal not allowed");
    }

    #[tokio::test]
    async fn read_file_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "read_file",
            rmcp::object!({"path": "/etc/hostname"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
    }

    #[tokio::test]
    async fn read_file_missing_path_param_is_tool_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        // rmcp's parameter wrapper turns a missing required field into a
        // tool-level error result (message visible to the LLM), not a
        // protocol error.
        let result = call(&client, "read_file", rmcp::object!({})).await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).contains("missing field `path`"),
            "{}",
            first_text(&result)
        );
    }

    #[tokio::test]
    async fn read_file_empty_path_is_invalid_params() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let err = client
            .call_tool(
                CallToolRequestParams::new("read_file").with_arguments(rmcp::object!({"path": ""})),
            )
            .await
            .expect_err("empty path must be invalid params");
        assert!(format!("{err}").contains("path"));
    }

    // ── read_file：读图（2026-10）────────────────────────────────────

    /// 提取第一个 image block（base64 数据 + mime）。
    fn first_image(result: &rmcp::model::CallToolResult) -> Option<(String, String)> {
        result.content.iter().find_map(|c| match c {
            rmcp::model::ContentBlock::Image(img) => {
                Some((img.data.clone(), img.mime_type.clone()))
            }
            _ => None,
        })
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([9, 140, 60]));
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode png");
        buf
    }

    fn jpeg_bytes(w: u32, h: u32, q: u8) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30]));
        let mut buf = Vec::new();
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q);
        image::DynamicImage::ImageRgb8(img)
            .write_with_encoder(enc)
            .expect("encode jpeg");
        buf
    }

    fn b64_decode(s: &str) -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(s).expect("valid base64")
    }

    /// 小 PNG：passthrough（mime 保持 png、字节逐字保留），结果 =
    /// [text 说明, image block] 双块。
    #[tokio::test]
    async fn read_file_small_png_returns_image_block_passthrough() {
        let dir = tempfile::TempDir::new().unwrap();
        let png = png_bytes(64, 64);
        std::fs::write(dir.path().join("shot.png"), &png).unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "shot.png"})).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(result.content.len(), 2, "text 说明 + image block");
        let text = first_text(&result);
        assert!(text.contains("[image file: shot.png"), "{text}");
        assert!(text.contains("image/png"), "{text}");
        let (data, mime) = first_image(&result).expect("image block present");
        assert_eq!(mime, "image/png", "小 PNG passthrough 保 PNG");
        assert_eq!(b64_decode(&data), png, "passthrough 字节逐字保留");
    }

    /// 超尺寸 JPEG（3000×2000）：归一化——mime 转 jpeg、长边 ≤2048、
    /// ≤5MB、可解码。
    #[tokio::test]
    async fn read_file_oversize_jpeg_normalized_to_2048_jpeg() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("big.jpg"), jpeg_bytes(3000, 2000, 95)).unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "big.jpg"})).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        let (data, mime) = first_image(&result).expect("image block present");
        assert_eq!(mime, "image/jpeg");
        let bytes = b64_decode(&data);
        assert!(bytes.len() <= 5 * 1024 * 1024, "{} bytes ≤ 5MB", bytes.len());
        let decoded = image::load_from_memory(&bytes).expect("归一化产物可解码");
        assert_eq!(decoded.width().max(decoded.height()), 2048);
    }

    /// 视频（伪 ftyp 头）：结构化错误提示（mime + 字节数），无 image block。
    #[tokio::test]
    async fn read_file_video_reports_binary_notice() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut mp4 = vec![0, 0, 0, 0];
        mp4.extend_from_slice(b"ftypisom");
        // 0xFF 填充：真实视频 payload 必然非 UTF-8（顺带保证走二进制分支，
        // 而不是全零字节被当成合法 UTF-8 文本）。
        mp4.extend(std::iter::repeat(0xFF).take(1024));
        std::fs::write(dir.path().join("clip.mp4"), &mp4).unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "clip.mp4"})).await;
        assert!(is_error(&result));
        let text = first_text(&result);
        assert!(text.contains("video/mp4"), "{text}");
        assert!(text.contains(&mp4.len().to_string()), "{text}");
        assert!(first_image(&result).is_none());
    }

    /// 未知二进制（非 UTF-8、无图像/视频魔数）：同样结构化错误提示。
    #[tokio::test]
    async fn read_file_unknown_binary_reports_notice() {
        let dir = tempfile::TempDir::new().unwrap();
        let blob: Vec<u8> = (0..256u32).map(|i| (i % 256) as u8).collect();
        std::fs::write(dir.path().join("blob.bin"), &blob).unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "blob.bin"})).await;
        assert!(is_error(&result));
        assert!(first_text(&result).contains("binary"), "{}", first_text(&result));
    }

    /// 归一化出口断言（防呆）：纯函数直测——webp/gif 入口交给 image crate
    /// 解码路径（同 normalize_image 主干），这里锁定常量护栏。
    #[test]
    fn normalize_output_bounds_hold() {
        // 小 PNG passthrough。
        let png = png_bytes(32, 32);
        let (mime, out) = normalize_image(&png, "image/png").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(out, png);
        // 大 JPEG → jpeg ≤2048。
        let (mime, out) = normalize_image(&jpeg_bytes(4000, 3000, 95), "image/jpeg").unwrap();
        assert_eq!(mime, "image/jpeg");
        let decoded = image::load_from_memory(&out).unwrap();
        assert_eq!(decoded.width().max(decoded.height()), 2048);
        assert!(out.len() <= 5 * 1024 * 1024);
    }

    /// 快速通道（零转码，与 mobile attachments 同款修复）：小 JPEG
    /// （长边 ≤2048、≤5MB）字节原样 passthrough。
    #[test]
    fn normalize_small_jpeg_passthrough_zero_transcode() {
        let jpeg = jpeg_bytes(640, 480, 90);
        let (mime, out) = normalize_image(&jpeg, "image/jpeg").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(out, jpeg, "小 JPEG 快速通道必须逐字节保留（零转码）");
    }

    /// 畸形 JPEG（header 读不出尺寸）→ 回落全解码路径给准确错误。
    #[test]
    fn normalize_truncated_jpeg_falls_back_to_decoder_error() {
        let broken = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00];
        let err = normalize_image(&broken, "image/jpeg").unwrap_err();
        assert!(err.contains("图像解码失败"), "{err}");
    }

    // ── write_file ──

    #[tokio::test]
    async fn write_file_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "test.txt", "content": "hello world"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(
            first_text(&result),
            "Successfully wrote 11 bytes to 'test.txt'"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("test.txt")).unwrap(),
            "hello world"
        );
    }

    #[tokio::test]
    async fn write_file_absolute_path_in_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;
        let target = dir.path().join("subdir/test.txt");

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": target.to_str().unwrap(), "content": "absolute"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "absolute");
    }

    #[tokio::test]
    async fn write_file_creates_parent_directories() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "a/b/c/nested.txt", "content": "nested content"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c/nested.txt")).unwrap(),
            "nested content"
        );
    }

    #[tokio::test]
    async fn write_file_overwrites_existing() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("existing.txt"), "original").unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "existing.txt", "content": "new content"}),
        )
        .await;
        assert!(!is_error(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
            "new content"
        );
    }

    #[tokio::test]
    async fn write_file_rejects_path_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "../escape.txt", "content": "bad"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path traversal not allowed");
    }

    #[tokio::test]
    async fn write_file_rejects_double_dot_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "foo/../../bar.txt", "content": "bad"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path traversal not allowed");
    }

    #[tokio::test]
    async fn write_file_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "/tmp/definitely-outside-openslate.txt", "content": "x"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
    }

    #[tokio::test]
    async fn write_file_missing_content_param_is_tool_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let result = call(&client, "write_file", rmcp::object!({"path": "test.txt"})).await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).contains("missing field `content`"),
            "{}",
            first_text(&result)
        );
    }

    // ── listing ──

    #[tokio::test]
    async fn list_tools_exposes_read_and_write() {
        let dir = tempfile::TempDir::new().unwrap();
        let client = client_in(&dir).await;

        let tools = client.peer().list_tools(None).await.unwrap();
        let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.contains(&"read_file".to_string()));
        assert!(names.contains(&"write_file".to_string()));
    }

    // ── sandbox helper unit tests (ports of the tool.rs semantics) ──

    #[test]
    fn resolve_rejects_traversal() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = resolve_workspace_path(dir.path(), "../x").unwrap_err();
        assert!(matches!(
            err,
            PathError::Security("Path traversal not allowed")
        ));
    }

    #[test]
    fn resolve_rejects_outside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = resolve_workspace_path(dir.path(), "/etc/hosts").unwrap_err();
        assert!(matches!(
            err,
            PathError::Security("Path is outside workspace")
        ));
    }

    #[test]
    fn resolve_allows_absolute_inside_workspace() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let abs = dir.path().join("f.txt");
        let resolved = resolve_workspace_path(dir.path(), abs.to_str().unwrap()).unwrap();
        assert_eq!(resolved, abs);
    }

    #[test]
    fn resolve_nonexistent_target_falls_back_to_lexical() {
        let dir = tempfile::TempDir::new().unwrap();
        // Key path for write_file: a not-yet-existing file inside the root
        // must resolve fine (lexical fallback).
        let resolved = resolve_workspace_path(dir.path(), "new/deep/file.txt").unwrap();
        assert_eq!(
            resolved,
            dir.path().canonicalize().unwrap().join("new/deep/file.txt")
        );
    }

    #[test]
    fn resolve_fails_when_workspace_root_missing() {
        // Rule 2 of the sandbox: an unresolvable root is an execution error
        // (FsServer::new keeps the raw path, so this fires on first use).
        let missing = tempfile::TempDir::new().unwrap();
        let root = missing.path().join("does/not/exist");
        drop(missing);

        let err = resolve_workspace_path(&root, "a.txt").unwrap_err();
        assert!(
            matches!(err, PathError::Execution(ref msg) if msg.starts_with("Failed to resolve workspace root")),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn nonexistent_root_reports_execution_error_on_first_use() {
        let missing = tempfile::TempDir::new().unwrap();
        let root = missing.path().join("gone");
        drop(missing);

        let client = spawn_server(FsServer::new(&root)).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "a.txt"})).await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).starts_with("Failed to resolve workspace root"),
            "{}",
            first_text(&result)
        );

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "a.txt", "content": "x"}),
        )
        .await;
        assert!(is_error(&result));
        assert!(
            first_text(&result).starts_with("Failed to resolve workspace root"),
            "{}",
            first_text(&result)
        );
    }

    // ── symlink sandbox tests (symlink creation is Unix-only in std) ──

    #[cfg(unix)]
    #[tokio::test]
    async fn read_file_rejects_symlink_to_outside_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let evil_dir = tempfile::TempDir::new().unwrap();
        std::fs::write(evil_dir.path().join("secret.txt"), "stolen").unwrap();
        std::os::unix::fs::symlink(
            evil_dir.path().join("secret.txt"),
            dir.path().join("alias.txt"),
        )
        .unwrap();

        let client = client_in(&dir).await;
        let result = call(&client, "read_file", rmcp::object!({"path": "alias.txt"})).await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_rejects_new_file_via_symlinked_dir() {
        // The write-side escape: `link -> /tmp/evil`, target `link/new.txt`
        // does not exist yet, so only the ancestor walk catches it.
        let dir = tempfile::TempDir::new().unwrap();
        let evil_dir = tempfile::TempDir::new().unwrap();
        std::os::unix::fs::symlink(evil_dir.path(), dir.path().join("link")).unwrap();

        let client = client_in(&dir).await;
        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "link/new.txt", "content": "escaped"}),
        )
        .await;
        assert!(is_error(&result));
        assert_eq!(first_text(&result), "Path is outside workspace");
        // The outside target must not have been created.
        assert!(!evil_dir.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn in_root_symlinked_file_read_and_write_still_work() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("real.txt"), "original").unwrap();
        std::os::unix::fs::symlink("real.txt", dir.path().join("alias.txt")).unwrap();

        let client = client_in(&dir).await;

        let result = call(&client, "read_file", rmcp::object!({"path": "alias.txt"})).await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(first_text(&result), "original");

        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "alias.txt", "content": "updated"}),
        )
        .await;
        assert!(!is_error(&result), "{}", first_text(&result));
        // The write lands on the real file (symlink dereferenced).
        assert_eq!(
            std::fs::read_to_string(dir.path().join("real.txt")).unwrap(),
            "updated"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_new_file_via_in_root_symlinked_dir_works() {
        // Legit use of the fallback path: symlinked directory that stays
        // inside the workspace — new files through it are allowed.
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("realdir")).unwrap();
        std::os::unix::fs::symlink("realdir", dir.path().join("dirlink")).unwrap();

        let client = client_in(&dir).await;
        let result = call(
            &client,
            "write_file",
            rmcp::object!({"path": "dirlink/new.txt", "content": "inside"}),
        )
        .await;
        assert!(!is_error(&result), "{}", first_text(&result));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("realdir/new.txt")).unwrap(),
            "inside"
        );
    }
}
