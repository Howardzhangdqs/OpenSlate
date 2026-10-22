//! 长输出落盘接缝（output_query 配套，设计 C2）。
//!
//! core 引擎的工具输出三段处理（mask → fork → limit）中「fork」的支撑
//! 模块：超过 `[limits].output_store_threshold` 的工具输出经宿主注册的
//! [`ToolOutputStorer`] 全文落盘，对话内替换为头尾预览
//! （[`build_preview`]），模型随后用 `output_query` 工具按 `ref_id`
//! 取回全文/切片。
//!
//! ## 进程级单槽注册（与 `set_shell_env_source` / `set_secret_masker`
//! 同款接缝）
//!
//! core 不依赖具体存储实现（SQLite / 文件 / 内存均由宿主决定）；
//! mobile 装配期 `set_tool_output_storer(Some(...))` 一次注册全进程
//! 生效。**未注册（CLI/TUI/server 桌面形态）时 `current_storer()`
//! 返回 `None`，引擎分叉点直通，行为与旧截断路径完全一致**——本模块
//! 对桌面零影响。
//!
//! ## 落盘内容约定
//!
//! 引擎在分叉点先对全文脱敏（`mask_tool_output`）再落盘——vault 凭据
//! 绝不落盘（见 runtime.rs 分叉点注释）。`store` 返回 `None` = 存储
//! 不可用/失败，引擎回落旧 limit 路径。

use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;

/// 落盘回执（core 用于构造预览）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOutput {
    /// 全文引用 id（`output_query` 按 id 取回；由存储层生成，全局唯一）。
    pub ref_id: String,
    /// 原文字节数（预览头行的 `size=` 与省略量诚实值计算都用它）。
    pub orig_bytes: usize,
    /// 原文行数（预览头行 `lines=`；行计数约定 = `\n` 分割段数，
    /// 存储层与预览构造共用同一约定即可，此处只透传）。
    pub line_count: usize,
}

/// 宿主注册的输出存储器。`store` 返回 `None` = 存储不可用/失败，
/// core 回落旧截断路径（limit_tool_output）。
#[async_trait]
pub trait ToolOutputStorer: Send + Sync {
    /// 落盘一条工具输出全文，返回回执。实现必须吞掉并记录自身错误
    /// （持久化失败绝不Kill运行），以 `None` 表达不可用。
    async fn store(&self, run_id: &str, tool_name: &str, content: &str) -> Option<StoredOutput>;
}

/// 进程级单槽注册表（默认 `None`——桌面端行为完全不变）。
static TOOL_OUTPUT_STORER: OnceLock<RwLock<Option<Arc<dyn ToolOutputStorer>>>> = OnceLock::new();

fn storer_cell() -> &'static RwLock<Option<Arc<dyn ToolOutputStorer>>> {
    TOOL_OUTPUT_STORER.get_or_init(|| RwLock::new(None))
}

/// 注册/清除进程级输出存储器（mobile 装配期调用；`None` = 清除）。
pub fn set_tool_output_storer(storer: Option<Arc<dyn ToolOutputStorer>>) {
    let mut guard = storer_cell().write().expect("tool output storer 锁中毒");
    *guard = storer;
}

/// 当前注册的存储器（未注册 → `None`，引擎分叉点直通）。
pub fn current_storer() -> Option<Arc<dyn ToolOutputStorer>> {
    storer_cell()
        .read()
        .expect("tool output storer 锁中毒")
        .clone()
}

/// 预览标记（格式冻结：UI 恢复链与 mobile 全文读取都按它识别落盘预览）。
/// 注意这是头行**前缀**，完整头行形如
/// `[STORED OUTPUT ref="..." size=... lines=...]`。
pub const STORED_OUTPUT_MARKER: &str = "[STORED OUTPUT";

/// 预览头尾字节预算（各 ≤1024B，UTF-8 字符边界安全截断）。
pub const PREVIEW_EDGE_BYTES: usize = 1024;

/// 字节预算内的最大字符边界下标（向后回退，对齐 mcp-builtin shell.rs
/// `truncate_output` 的 head 先例：劈进多字节字符就退到前一边界）。
fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 字节预算内的最小字符边界下标（向前推进，对齐同先例的 tail：劈进
/// 多字节字符就进到后一边界）。
fn ceil_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// 构造落盘预览：首行元数据 + head + 省略行 + tail。
///
/// 精确格式（**冻结**，测试断言钉死；UI 恢复链与 mobile 全文读取都
/// 解析它，改动即破坏协议）：
///
/// ```text
/// [STORED OUTPUT ref="{ref_id}" size={orig_bytes} lines={line_count}]
/// {head ≤1024B}
/// [... {omitted} bytes omitted ({line_count} lines) — use output_query to inspect ...]
/// {tail ≤1024B}
/// ```
///
/// - `omitted = orig_bytes - head - tail`（诚实值：head/tail 是字符边界
///   修正后的实际保留字节，省略量随之计算，不做四舍五入的估算）；
/// - 省略行的 `lines` = **总行数**（`line_count`，与头行一致——模型
///   关心的是全文规模，不是被省略的行数）；
/// - 字节截断按 UTF-8 字符边界回退/前进（中文/emoji 不会被劈成
///   乱码，对齐 mcp-builtin shell.rs truncate 先例）。
///
/// `tool_name` 当前不进预览文本（格式冻结），保留在签名里供调试日志
/// 与未来演进（mobile 恢复链可能需要按工具名分类展示）。
pub fn build_preview(tool_name: &str, stored: &StoredOutput, content: &str) -> String {
    tracing::debug!(
        tool_name,
        ref_id = %stored.ref_id,
        orig_bytes = stored.orig_bytes,
        "building stored-output preview"
    );

    let total = content.len();
    // 头尾不重叠（分叉点保证 content 远大于 2×预算，此处防御性钳制）。
    let head_end = floor_char_boundary(content, PREVIEW_EDGE_BYTES);
    let tail_start = ceil_char_boundary(content, total.saturating_sub(PREVIEW_EDGE_BYTES)).max(head_end);
    let head = &content[..head_end];
    let tail = &content[tail_start..];
    let omitted = stored.orig_bytes.saturating_sub(head.len() + tail.len());

    format!(
        "{STORED_OUTPUT_MARKER} ref=\"{}\" size={} lines={}]\n{}\n[... {} bytes omitted ({} lines) — use output_query to inspect ...]\n{}",
        stored.ref_id,
        stored.orig_bytes,
        stored.line_count,
        head,
        omitted,
        stored.line_count,
        tail,
    )
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn stored(ref_id: &str, orig_bytes: usize, line_count: usize) -> StoredOutput {
        StoredOutput {
            ref_id: ref_id.to_owned(),
            orig_bytes,
            line_count,
        }
    }

    // ── 注册槽 ───────────────────────────────────────────────────────────

    struct NopStorer;
    #[async_trait]
    impl ToolOutputStorer for NopStorer {
        async fn store(
            &self,
            _run_id: &str,
            _tool_name: &str,
            _content: &str,
        ) -> Option<StoredOutput> {
            None
        }
    }

    #[test]
    fn storer_slot_defaults_to_none_and_round_trips() {
        // 注意：进程级槽位——先清空再断言，测完复位（并行测试互不干扰）。
        set_tool_output_storer(None);
        assert!(current_storer().is_none());
        set_tool_output_storer(Some(Arc::new(NopStorer)));
        assert!(current_storer().is_some());
        set_tool_output_storer(None);
        assert!(current_storer().is_none());
    }

    // ── 预览格式（冻结，防漂移）─────────────────────────────────────────

    /// ASCII 已知输入 → 逐字符钉死完整预览（格式冻结锚点测试：任何
    /// 空格/字段顺序/标点变化都会在此爆）。
    #[test]
    fn preview_format_is_pinned() {
        // head 与 tail 各恰好 1024B（ASCII，无边界修正）。
        let head = "H".repeat(PREVIEW_EDGE_BYTES);
        let tail = "T".repeat(PREVIEW_EDGE_BYTES);
        let middle = "M".repeat(5000);
        let content = format!("{head}{middle}{tail}");
        let s = stored("ref-abc", content.len(), 42);

        let preview = build_preview("bash", &s, &content);
        let omitted = content.len() - PREVIEW_EDGE_BYTES * 2;
        let expected = format!(
            "[STORED OUTPUT ref=\"ref-abc\" size={} lines=42]\n{}\n[... {} bytes omitted (42 lines) — use output_query to inspect ...]\n{}",
            content.len(),
            head,
            omitted,
            tail,
        );
        assert_eq!(preview, expected);
        // 前缀常量与头行绑定（UI 恢复链按它识别）。
        assert!(preview.starts_with(STORED_OUTPUT_MARKER));
        assert!(preview.starts_with("[STORED OUTPUT ref=\"ref-abc\""));
        // 诚实值：omitted 恰为被省略的中段字节数。
        assert_eq!(omitted, middle.len());
    }

    /// 省略行的 lines = 总行数（不是被省略的行数）。
    #[test]
    fn preview_omitted_line_reports_total_lines() {
        // head 内含换行（"line\n" 前缀）→ 按前缀定位省略行，不按行号。
        let content = format!("line\n{}", "x".repeat(8_000));
        let lines = content.matches('\n').count() + 1;
        let s = stored("r", content.len(), lines);
        let preview = build_preview("bash", &s, &content);
        let omitted_line = preview
            .lines()
            .find(|l| l.starts_with("[... "))
            .expect("omitted line present");
        assert_eq!(
            omitted_line,
            format!(
                "[... {} bytes omitted ({} lines) — use output_query to inspect ...]",
                content.len() - 2 * PREVIEW_EDGE_BYTES,
                lines
            )
        );
    }

    // ── 多字节字符劈裂安全 ────────────────────────────────────────────────

    /// 中文（3B/字）落在 1024B 切点：边界回退/前进不 panic、不劈字。
    #[test]
    fn preview_multibyte_cut_points_are_char_safe_chinese() {
        // 前缀 1B 使 1024 切点恰好落在某个「中」的中间字节。
        let content = format!("a{}", "中".repeat(10_000)); // 1 + 30_000B
        let s = stored("r", content.len(), 1);
        let preview = build_preview("bash", &s, &content);

        let mut lines = preview.lines();
        let head = lines.nth(1).expect("head 行");
        let tail = preview.lines().last().expect("tail 行");
        // head/tail 都是完整字符序列（&str 切片本身保证，断言显式钉住）。
        assert!(head.starts_with('a'));
        assert!(head.chars().all(|c| c == 'a' || c == '中'));
        assert!(tail.chars().all(|c| c == '中'));
        // 头尾预算不因边界修正超支。
        assert!(head.len() <= PREVIEW_EDGE_BYTES);
        assert!(tail.len() <= PREVIEW_EDGE_BYTES);
        // 诚实值与 head/tail 实际保留量一致（省略量 ≥ 原文 - 2×预算）。
        let omitted_line = preview
            .lines()
            .find(|l| l.starts_with("[... "))
            .expect("omitted line present");
        let omitted: usize = omitted_line
            .split_whitespace()
            .find_map(|t| t.parse::<usize>().ok())
            .expect("省略行含省略字节数");
        assert_eq!(omitted, content.len() - head.len() - tail.len());
    }

    /// emoji（4B/字，astral plane）同样安全。
    #[test]
    fn preview_multibyte_cut_points_are_char_safe_emoji() {
        let content = format!("b{}", "😀".repeat(5000)); // 1 + 20_000B
        let s = stored("r", content.len(), 7);
        let preview = build_preview("termux_bash", &s, &content);
        let head = preview.lines().nth(1).unwrap();
        let tail = preview.lines().last().unwrap();
        assert!(head.starts_with('b'));
        assert!(head.chars().all(|c| c == 'b' || c == '😀'));
        assert!(tail.chars().all(|c| c == '😀'));
        // 1024 % 4 == 0，但前缀 1B 把切点推进字符内部 → head 实际 1020B。
        assert_eq!(head.len(), 1 + 1020);
    }

    /// 防御路径：内容不足头尾预算之和时不 panic、不重叠。
    #[test]
    fn preview_tiny_content_does_not_overlap() {
        let content = "short".repeat(100); // 500B < 2×1024
        let s = stored("r", content.len(), 1);
        let preview = build_preview("bash", &s, &content);
        // head 吃满全文、tail 为空 → 省略 0。
        assert!(preview.contains(&format!(
            "[... 0 bytes omitted (1 lines) — use output_query to inspect ...]"
        )));
        assert!(!preview.contains("shortshortshortshort\n[... 0 bytes\n"));
    }
}
