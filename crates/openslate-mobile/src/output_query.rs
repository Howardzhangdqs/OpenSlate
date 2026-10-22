//! output_query 内置工具（长输出落盘 + output_query 设计 C4）。
//!
//! 长工具输出落盘后（[`crate::output_storer`]，C3），对话内只有头尾预览
//! （`[STORED OUTPUT ref="out-…" …]` 标记，core `build_preview`）。本模块
//! 是模型侧的取回通道：按 `ref` 查 `tool_outputs` 行 → 读
//! `files/outputs/{sha256}.zst` → 校验完整性 → 按 `op` 切片返回
//! （head / tail / lines / grep / wc 五算子）。
//!
//! ## 数据链（与 C6 详情弹窗 `tool_output_by_call_id` 同源）
//!
//! `ref` 校验（`out-` + 16 hex）→ `find_tool_output(ref16, 当前 run 优先)`
//! → 无行返回 status=Error 文案（含期望格式提示）→
//! `read_tool_output_bytes(sha64)` → zstd 解压（纯内存操作，量级 ≤1MiB
//! 传输上限，对齐 output_storer 的「不单独 spawn_blocking」约定）→
//! **校验 `len == orig_bytes`**（防半写文件/内容错位被静默错读）→ 切片。
//!
//! ## 行迭代核心（一份共用，改造自 uutils/coreutils `head`）
//!
//! 五个算子共用同一个 64KB 块式行迭代核心 [`LineScanner`]。边界处理
//! 改造自 uutils coreutils `uu/head/src/take.rs`（MIT 许可，下文常量区
//! 有署名与版权声明）：恒定 64KB 读块复用不膨胀（超长单行的累积只发生
//! 在行缓冲，读块尺寸不变）、读侧 EINTR 重试、行跨块拼接、末行无换行
//! 亦算一行。行的切分**只按 `\n`**：行内 `\r` 保留（CRLF 文件的 grep
//! 输出行尾带 `\r`，工具 description 已注明；与 Rust `str::lines()` 会
//! 剥掉 `\r\n` 的 `\r` 不同——行**计数**两种约定恒等，行**内容**我们保
//! 留原字节，工具描述注明）。
//!
//! 注：设计初稿把 tail 记为 `copy_all_but_n_lines(total-n)`，但该函数
//! 的 uutils 语义是「丢弃末 n 行、输出前缀」（`head -n -K`），直接套用
//! 会返回 head 而不是 tail；tail 正确实现 = 跳过前 `total-n` 行 + 拷贝
//! 余下全部，与 lines 算子同一条「跳过 + 拷贝」通路（见
//! [`LineScanner::skip_lines`]），故未引入 `copy_all_but_n_lines` /
//! `copy_all_but_n_bytes`（无算子对应，保留只会成为 dead code）。
//!
//! ## 审批
//!
//! 无副作用（纯读本会话已脱敏落盘的数据）：mobile 装配链最外层的
//! [`crate::askuser::SideEffectFreeApproval`] 免审（ask_user 同款先例），
//! Auto 缺省下也不会为每次查询付一次 AI 审查调用。
//!
//! ## 生命周期
//!
//! store 经 [`crate::attachments::SharedStore`]（OnceLock 后注，
//! output_storer 同款接缝）注入；`AppState` 弱引用在 `assemble_state`
//! 之后由 `create_with` 注入（当前 run 优先 + `[limits].max_output_bytes`
//! 生效值读取，AskUserContext.wire 同款防环模式）。装配前调用返回明确
//! 错误文案（正常流程不可达：注入先于首个回合）。

// ─────────────────────────────────────────────────────────────────────────────
// 行迭代核心：改造自 uutils/coreutils（MIT License）。
//
// Copyright (c) 2024 uutils developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
//
// adapted from uutils/coreutils src/uu/head/src/take.rs（TakeAllBuffer 的
// 64KB 读块 / EINTR 重试 / 缓冲复用模式 + TakeAllLinesBuffer 的分隔符
// 切分语义），为 output_query 的五个算子改为「跳过 + 拷贝」形态。
// ─────────────────────────────────────────────────────────────────────────────

use std::io::{ErrorKind, Read};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use async_trait::async_trait;
use openslate_core::error::ToolError;
use openslate_core::runtime::RuntimeLimits;
use openslate_core::tool::Tool;
use openslate_core::types::{ToolOutput, ToolOutputStatus};
use openslate_session::state::AppState;
use serde_json::{json, Value};

use crate::attachments::SharedStore;

/// 工具注册名。
pub const OUTPUT_QUERY_TOOL: &str = "output_query";

/// head/tail 的行数缺省。
const DEFAULT_N: usize = 50;

/// head/tail 的行数上限。
const MAX_N: usize = 2000;

/// 64KB 读块（uutils head `BUF_SIZE` 同值）。
const READ_CHUNK: usize = 65_536;

/// 工具 description（设计定稿文案 + CRLF 行为注明）。
const OUTPUT_QUERY_DESCRIPTION: &str = "Query the full text of a stored tool output. Long tool outputs are not shown \
inline; instead the tool result contains a marker like \
[STORED OUTPUT ref=\"out-...\" size=N lines=M] with a head/tail preview, and the \
full text is stored on disk. \
Use this tool when a previous tool result contained that marker and you need \
more than the preview: find errors in the middle of build logs (grep), read a \
specific section (lines), check totals (wc), or see context around the preview \
(head/tail). \
Workflow tips: \
- grep first to locate interesting line numbers, then lines with a window \
around them (e.g. start=120, end=140) for context. \
- wc gives total lines/bytes to choose sensible ranges. \
- Line numbers are 1-based. ref comes from the [STORED OUTPUT ...] marker. \
- Lines are split on \\n only; a trailing \\r (CRLF file) stays at the end of \
each returned line.";

// ─────────────────────────────────────────────────────────────────────────────
// 行迭代核心（head / tail / lines / grep / wc 一份共用）
// ─────────────────────────────────────────────────────────────────────────────

/// 64KB 块式行迭代器。
///
/// 边界保证（uutils take.rs 改造，见模块文档署名）：
/// - 读块恒定 64KB、`fill_chunk` 里 resize 复用（超长单行不膨胀读块）；
/// - `read` 返回 [`ErrorKind::Interrupted`] 时重试（EINTR）；
/// - 行跨块由调用方 `line` 缓冲拼接（行长即其上界）；
/// - 末行无换行亦产出一行（返回 `true` 且 `line` 为该行内容）。
struct LineScanner<R: Read> {
    reader: R,
    chunk: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read> LineScanner<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            chunk: Vec::new(),
            pos: 0,
            eof: false,
        }
    }

    /// 读下一块到 `chunk`（EINTR 重试；uutils `TakeAllBuffer::fill_buffer`
    /// 同款），返回读取字节数（0 = EOF）。
    fn fill_chunk(&mut self) -> std::io::Result<usize> {
        self.chunk.resize(READ_CHUNK, 0);
        self.pos = 0;
        loop {
            match self.reader.read(&mut self.chunk[..]) {
                Ok(n) => {
                    self.chunk.truncate(n);
                    return Ok(n);
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    /// 下一行写入 `line`（不含 `\n`，**含行内 `\r`**）。返回 `false` =
    /// EOF。空内容立即 `false`；末行无换行时该行照常产出。
    fn next_line(&mut self, line: &mut Vec<u8>) -> std::io::Result<bool> {
        line.clear();
        loop {
            if self.pos >= self.chunk.len() {
                if self.eof {
                    return Ok(!line.is_empty());
                }
                if self.fill_chunk()? == 0 {
                    self.eof = true;
                }
                continue;
            }
            let rest = &self.chunk[self.pos..];
            match rest.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    line.extend_from_slice(&rest[..i]);
                    self.pos += i + 1;
                    return Ok(true);
                }
                None => {
                    line.extend_from_slice(rest);
                    self.pos = self.chunk.len();
                }
            }
        }
    }

    /// 跳过 `n` 行，返回实际跳过数（内容不足则提前停在 EOF）。
    fn skip_lines(&mut self, n: usize, line: &mut Vec<u8>) -> std::io::Result<usize> {
        let mut skipped = 0;
        for _ in 0..n {
            if !self.next_line(line)? {
                break;
            }
            skipped += 1;
        }
        Ok(skipped)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 参数解析
// ─────────────────────────────────────────────────────────────────────────────

/// 归一化后的算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Head,
    Tail,
    Lines,
    Grep,
    Wc,
}

/// 校验 + 归一化后的查询（错误文案直接回喂模型自纠）。
struct ParsedQuery {
    /// `ref` 去 `out-` 前缀后的 16 hex。
    ref16: String,
    op: Op,
    /// head/tail 行数（已含缺省 50 与 1..=2000 钳制校验）。
    n: usize,
    /// lines 区间（1-based，闭区间；已校验 start ≥ 1 且 start ≤ end）。
    start: usize,
    end: usize,
    /// grep 编译后的正则（ignore_case 已折叠进去）。
    pattern: Option<regex::Regex>,
}

/// `ref` 期望格式提示（参数错误与未知 ref 共用）。
const REF_HINT: &str = "expected \"out-\" followed by exactly 16 hex characters (e.g. \"out-ab12cd34ef56ab78\") \
taken from the '[STORED OUTPUT ref=\"out-...\" ...]' marker in a previous tool result";

fn parse_int_field(args: &Value, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_i64()
            .map(Some)
            .ok_or_else(|| format!("'{key}' must be an integer")),
    }
}

/// 校验 + 归一化工具入参。`op` 与字段的匹配关系逐算子校验（lines 缺
/// start/end、非法 regex 编译错误原文回显等）。
fn parse_query(args: &Value) -> Result<ParsedQuery, String> {
    let ref_id = args
        .get("ref")
        .and_then(|r| r.as_str())
        .ok_or_else(|| format!("'ref' is required; {REF_HINT}"))?
        .to_owned();
    let ref16 = ref_id
        .strip_prefix("out-")
        .filter(|hex| hex.len() == 16 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| format!("'ref' has an invalid format ({ref_id:?}); {REF_HINT}"))?
        .to_owned();

    let op = match args.get("op").and_then(|o| o.as_str()) {
        Some("head") => Op::Head,
        Some("tail") => Op::Tail,
        Some("lines") => Op::Lines,
        Some("grep") => Op::Grep,
        Some("wc") => Op::Wc,
        Some(other) => {
            return Err(format!(
                "'op' must be one of head|tail|lines|grep|wc (got {other:?})"
            ))
        }
        None => return Err("'op' is required (head|tail|lines|grep|wc)".to_owned()),
    };

    let n = match parse_int_field(args, "n")? {
        None => DEFAULT_N,
        Some(v) if (1..=MAX_N as i64).contains(&v) => v as usize,
        Some(v) => {
            return Err(format!(
                "'n' must be an integer between 1 and {MAX_N} (got {v})"
            ))
        }
    };

    let (start, end) = if op == Op::Lines {
        let start = parse_int_field(args, "start")?
            .ok_or_else(|| "'start' is required for op=lines (1-based, inclusive)".to_owned())?;
        let end = parse_int_field(args, "end")?
            .ok_or_else(|| "'end' is required for op=lines (1-based, inclusive)".to_owned())?;
        if start < 1 {
            return Err(format!("'start' must be >= 1 (got {start})"));
        }
        if end < start {
            return Err(format!(
                "'start' must be <= 'end' (got start={start}, end={end})"
            ));
        }
        (start as usize, end as usize)
    } else {
        (0, 0)
    };

    let pattern = if op == Op::Grep {
        let raw = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .ok_or_else(|| "'pattern' is required for op=grep".to_owned())?;
        let ignore_case = args
            .get("ignore_case")
            .and_then(|i| i.as_bool())
            .unwrap_or(false);
        Some(
            regex::RegexBuilder::new(raw)
                .case_insensitive(ignore_case)
                .build()
                .map_err(|e| format!("invalid regex {raw:?}: {e}"))?,
        )
    } else {
        None
    };

    Ok(ParsedQuery {
        ref16,
        op,
        n,
        start,
        end,
        pattern,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// 输出封顶
// ─────────────────────────────────────────────────────────────────────────────

/// 字节预算内的最大字符边界下标（向后回退；对齐 core output_store /
/// mcp-builtin truncate 的 head 先例——std 同名 API 未稳定，本地小拷贝）。
fn floor_char_boundary(text: &str, mut idx: usize) -> usize {
    idx = idx.min(text.len());
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

/// 结果截断尾注（设计定稿文案）。
fn truncation_notice(cap: usize) -> String {
    format!("[output_query: result truncated at {cap} bytes — narrow start/end, reduce n, or use grep]")
}

/// 累计输出封顶（`cap == 0` = 不限）：超限在字符边界截断并追加尾注
/// （尾注本身超预算是必然——截断提示必须可读）。
fn cap_with_notice(mut text: String, cap: usize) -> String {
    if cap == 0 || text.len() <= cap {
        return text;
    }
    let cut = floor_char_boundary(&text, cap);
    text.truncate(cut);
    text.push('\n');
    text.push_str(&truncation_notice(cap));
    text
}

/// grep 尾注预算（"showing M of T matches" 上界，含换行）。
const GREP_NOTE_RESERVE: usize = 48;

/// grep 输出：命中行（恒带行号）+ 尾注 `showing M of T matches`（M =
/// 实际保留行数；封顶时 M < T 即截断提示）。`cap == 0` = 不限。
fn grep_output(matches: &[String], cap: usize) -> String {
    let total = matches.len();
    let mut out = String::new();
    let mut shown = 0;
    for m in matches {
        let sep = if out.is_empty() { 0 } else { 1 };
        if cap != 0 && out.len() + sep + m.len() + GREP_NOTE_RESERVE > cap {
            break;
        }
        if sep == 1 {
            out.push('\n');
        }
        out.push_str(m);
        shown += 1;
    }
    let note = format!("showing {shown} of {total} matches");
    if cap != 0 && out.len() + 1 + note.len() > cap {
        // 极端小 cap（配置异常）：整段在边界截断，尾注尽力保留。
        let cut = floor_char_boundary(&out, cap.saturating_sub(note.len() + 1));
        out.truncate(cut);
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&note);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// 工具本体
// ─────────────────────────────────────────────────────────────────────────────

/// output_query 工具（宿主直连 `Tool`，AskUserTool/HostTool 同款形态）。
///
/// 无副作用纯读：store 未注入（装配窗口，正常不可达）返回明确错误文案
/// 而非 Err/panic。
pub struct OutputQueryTool {
    /// 数据源（output_storer 同款 `SharedStore` 接缝，create_with 在
    /// assemble_state 后注入）。
    store: SharedStore,
    /// `AppState` 弱引用（AskUserContext.wire 同款防环接缝）：当前 run
    /// 优先 + `[limits].max_output_bytes` 生效值。
    state: Arc<OnceLock<Weak<AppState>>>,
}

impl OutputQueryTool {
    pub fn new(store: SharedStore) -> Self {
        Self {
            store,
            state: Arc::new(OnceLock::new()),
        }
    }

    /// 装配后接线句柄（工具本体随即 move 进 registry，接线经共享
    /// OnceLock 完成）。
    pub fn state_handle(&self) -> Arc<OnceLock<Weak<AppState>>> {
        self.state.clone()
    }

    /// `(当前 run_id, max_output_bytes 生效值)`。未接线 / AppState 已
    /// 释放（shutdown 后）→ `(None, RuntimeLimits 缺省 64KiB)`；cap 为 0
    /// = 不限（与 core limit 的 0 = unlimited 约定一致）。
    fn session_context(&self) -> (Option<String>, usize) {
        let cap = RuntimeLimits::default().max_output_bytes as usize;
        let Some(state) = self.state.get().and_then(Weak::upgrade) else {
            return (None, cap);
        };
        let inner = state.core.lock();
        let run_id = inner.session_run.as_ref().map(|r| r.run_id.0.clone());
        let effective = RuntimeLimits::from_config(&inner.config).max_output_bytes as usize;
        (run_id, effective)
    }
}

#[async_trait]
impl Tool for OutputQueryTool {
    fn name(&self) -> &str {
        OUTPUT_QUERY_TOOL
    }

    fn description(&self) -> &str {
        OUTPUT_QUERY_DESCRIPTION
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "ref": {
                    "type": "string",
                    "description": "stored-output reference, e.g. \"out-ab12cd34ef56ab78\" (from the '[STORED OUTPUT ref=...]' marker in a previous tool result)"
                },
                "op": {
                    "type": "string",
                    "enum": ["head", "tail", "lines", "grep", "wc"],
                    "description": "head=first N lines; tail=last N lines; lines=line range (1-based, inclusive); grep=regex search with line numbers; wc=line/byte counts"
                },
                "n": {
                    "type": "integer",
                    "description": "line count for head/tail (default 50, max 2000)"
                },
                "start": {
                    "type": "integer",
                    "description": "first line number for op=lines (1-based, inclusive)"
                },
                "end": {
                    "type": "integer",
                    "description": "last line number for op=lines (inclusive)"
                },
                "pattern": {
                    "type": "string",
                    "description": "Rust regex for op=grep (per-line match)"
                },
                "ignore_case": {
                    "type": "boolean",
                    "description": "case-insensitive grep (default false)"
                }
            },
            "required": ["ref", "op"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolOutput, ToolError> {
        let started = Instant::now();
        let output = |content: String, status: ToolOutputStatus| ToolOutput {
            attachments: None,
            bytes: content.len(),
            content,
            duration_ms: started.elapsed().as_millis() as u64,
            status,
        };
        let error = |content: String| output(content, ToolOutputStatus::Error);

        // ① 参数校验（普通文本 Error 回喂自纠）。
        let query = match parse_query(args) {
            Ok(q) => q,
            Err(msg) => return Ok(error(format!("Invalid output_query arguments: {msg}"))),
        };

        // ② store 未注入（装配窗口；正常流程注入先于首个回合）。
        let Some(store) = self.store.get().cloned() else {
            return Ok(error(
                "output_query is not available: the output store is not wired yet \
                 (this should resolve right after session start; if it persists, restart the app)"
                    .to_owned(),
            ));
        };

        // ③ 行查找（当前 run 优先）。
        let (prefer_run, cap) = self.session_context();
        let record = match store.find_tool_output(&query.ref16, prefer_run.as_deref()).await {
            Ok(Some(record)) => record,
            Ok(None) => {
                return Ok(error(format!(
                    "unknown stored-output ref \"out-{}\": no stored output matches this reference. {REF_HINT}",
                    query.ref16
                )))
            }
            Err(e) => return Ok(error(format!("stored-output lookup failed: {e}"))),
        };

        // ④ 读压缩文件 + 解压 + 完整性校验（len == orig_bytes，防半写
        //    文件 / 内容错位被静默错读）。
        let zst = match store.read_tool_output_bytes(&record.sha256).await {
            Ok(Some(bytes)) => bytes,
            Ok(None) => {
                return Ok(error(
                    "stored-output file is missing (it may have been cleaned up); \
                     only the preview in the original tool result remains"
                        .to_owned(),
                ))
            }
            Err(e) => return Ok(error(format!("stored-output file read failed: {e}"))),
        };
        // 纯内存解压，≤1MiB 量级（三腿传输上限），对齐 output_storer 的
        // 「不单独 spawn_blocking」约定。
        let decoded = match zstd::decode_all(zst.as_slice()) {
            Ok(bytes) => bytes,
            Err(e) => {
                crate::alog!("output_query: zstd 解压失败 (ref=out-{}): {e}", query.ref16);
                return Ok(error(format!(
                    "stored-output decompression failed: {e}"
                )));
            }
        };
        if decoded.len() != record.orig_bytes as usize {
            crate::alog!(
                "output_query: 完整性校验失败 (ref=out-{}): decoded={} recorded={}",
                query.ref16,
                decoded.len(),
                record.orig_bytes
            );
            return Ok(error(format!(
                "stored-output integrity check failed (decoded {} bytes, expected {}): \
                 the stored file may be corrupted; do not trust partial reads",
                decoded.len(),
                record.orig_bytes
            )));
        }
        let text = match String::from_utf8(decoded) {
            Ok(text) => text,
            Err(e) => {
                crate::alog!("output_query: 落盘内容非 UTF-8 (ref=out-{}): {e}", query.ref16);
                return Ok(error("stored-output content is not valid UTF-8".to_owned()));
            }
        };

        // ⑤ 按 op 切片（五算子共用一条「跳过 + 拷贝 / 逐块扫描」通路）。
        // grep 在算子内自带 M-of-T 封顶尾注；head/tail/lines/wc 的累计
        // 封顶在 ⑥ 统一处理。
        let result = match run_op(&query, &text, record.line_count as usize, cap) {
            Ok(text) => text,
            Err(msg) => return Ok(error(msg)),
        };

        // ⑥ 累计输出封顶 = max_output_bytes（0 = 不限）。
        Ok(output(cap_with_notice(result, cap), ToolOutputStatus::Success))
    }
}

/// 在全文上执行算子。`total_lines_hint` = 落盘行的 line_count（与已通过
/// 完整性校验的原文同源，tail 的跳过量据此计算）；`cap` 仅 grep 用
/// （M-of-T 尾注需要知道封顶）。
fn run_op(query: &ParsedQuery, text: &str, total_lines_hint: usize, cap: usize) -> Result<String, String> {
    // 全文已验证为 UTF-8 ⇒ 按 b'\n' 切出的行必然也是合法 UTF-8 子串。
    let mut scanner = LineScanner::new(text.as_bytes());
    let mut line = Vec::new();
    let mut collected: Vec<String> = Vec::new();

    match query.op {
        Op::Head | Op::Lines | Op::Tail => {
            // 统一的「跳过 + 拷贝」通路：head = 跳 0 拷 n；lines = 跳
            // start-1 拷 end-start+1；tail = 跳 total-n 拷余下全部（末行
            // 无换行也算一行——scanner 语义保证）。
            let (skip, take) = match query.op {
                Op::Head => (0, Some(query.n)),
                Op::Lines => (query.start - 1, Some(query.end - query.start + 1)),
                // tail：n ≥ 总行数时跳过量饱和为 0（返回全文）。
                Op::Tail => (total_lines_hint.saturating_sub(query.n), None),
                _ => unreachable!("matched arm filters to three ops"),
            };
            scanner
                .skip_lines(skip, &mut line)
                .map_err(|e| format!("stored-output read failed: {e}"))?;
            let mut taken = 0;
            while take.is_none_or(|t| taken < t) {
                if !scanner
                    .next_line(&mut line)
                    .map_err(|e| format!("stored-output read failed: {e}"))?
                {
                    break;
                }
                collected.push(
                    String::from_utf8(std::mem::take(&mut line))
                        .map_err(|_| "stored-output content is not valid UTF-8".to_owned())?,
                );
                taken += 1;
            }
            Ok(collected.join("\n"))
        }
        Op::Grep => {
            // 逐行扫描全量收集，封顶 + 尾注在 grep_output。
            let regex = query.pattern.as_ref().expect("parse_query 已编译");
            let mut matches: Vec<String> = Vec::new();
            let mut line_no: usize = 0;
            while scanner
                .next_line(&mut line)
                .map_err(|e| format!("stored-output read failed: {e}"))?
            {
                line_no += 1;
                let text_line = String::from_utf8(std::mem::take(&mut line))
                    .map_err(|_| "stored-output content is not valid UTF-8".to_owned())?;
                if regex.is_match(&text_line) {
                    matches.push(format!("{line_no}:{text_line}"));
                }
            }
            Ok(grep_output(&matches, cap))
        }
        Op::Wc => {
            let mut lines: usize = 0;
            while scanner
                .next_line(&mut line)
                .map_err(|e| format!("stored-output read failed: {e}"))?
            {
                lines += 1;
            }
            let mut out = format!("lines: {}, bytes: {}", lines, text.len());
            if !text.is_empty() && !text.ends_with('\n') {
                out.push_str("\nnote: no trailing newline");
            }
            Ok(out)
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use openslate_store_sqlite::store::SqliteStore;
    use sha2::{Digest, Sha256};

    /// 建文件库 + run-1（tool_outputs 需要文件库）。
    async fn setup_store() -> (tempfile::TempDir, Arc<SqliteStore>) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Arc::new(
            SqliteStore::new(dir.path().join("oq.sqlite").to_str().expect("utf-8"))
                .await
                .expect("store"),
        );
        store.run_migrations().await.expect("migrations");
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run");
        (dir, store)
    }

    /// 落盘一条输出（orig/lines 按原文真实值），返回 ref_id。
    async fn store_output(store: &SqliteStore, content: &str) -> String {
        store_output_raw(store, content, content.len(), content.lines().count()).await
    }

    /// 落盘一条输出（计量字段可伪造——完整性校验测试用）。
    async fn store_output_raw(
        store: &SqliteStore,
        content: &str,
        orig_bytes: usize,
        line_count: usize,
    ) -> String {
        let sha = format!("{:x}", Sha256::digest(content.as_bytes()));
        let data_zst = zstd::encode_all(content.as_bytes(), 3).expect("zstd");
        // 行 id 是 PRIMARY KEY（行级唯一），测试用 uuid 防冲突。
        let id = format!("out-{}", uuid::Uuid::new_v4().simple());
        store
            .insert_tool_output(
                &id,
                "run-1",
                &sha,
                "bash",
                orig_bytes as i64,
                line_count as i64,
                &data_zst,
                1000,
            )
            .await
            .expect("insert");
        format!("out-{}", &sha[..16])
    }

    /// 未接线 AppState 的工具（prefer_run=None、cap=64KiB 缺省）。
    fn tool(store: &Arc<SqliteStore>) -> OutputQueryTool {
        let shared: SharedStore = Arc::new(OnceLock::new());
        let _ = shared.set(store.clone());
        OutputQueryTool::new(shared)
    }

    async fn run(tool: &OutputQueryTool, args: Value) -> ToolOutput {
        tool.execute(&args).await.expect("execute ok")
    }

    fn args(ref_id: &str, op: &str, extra: Value) -> Value {
        let mut base = json!({ "ref": ref_id, "op": op });
        if let (Some(obj), Some(extra)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                obj.insert(k.clone(), v.clone());
            }
        }
        base
    }

    // ── 五算子 happy path ─────────────────────────────────────────────

    #[tokio::test]
    async fn ops_matrix_on_known_content() {
        let (_dir, store) = setup_store().await;
        let content = "l1\nl2\nl3\nl4\nl5\n"; // 5 行（末行有换行）
        let ref_id = store_output(&store, content).await;
        let t = tool(&store);

        // head 默认 n=50 → 全部 5 行。
        let out = run(&t, args(&ref_id, "head", json!({}))).await;
        assert_eq!(out.status, ToolOutputStatus::Success);
        assert_eq!(out.content, "l1\nl2\nl3\nl4\nl5");
        assert_eq!(out.bytes, out.content.len());

        // head n=2。
        let out = run(&t, args(&ref_id, "head", json!({ "n": 2 }))).await;
        assert_eq!(out.content, "l1\nl2");

        // tail n=2 → 末两行。
        let out = run(&t, args(&ref_id, "tail", json!({ "n": 2 }))).await;
        assert_eq!(out.content, "l4\nl5");

        // lines 2..=4。
        let out = run(&t, args(&ref_id, "lines", json!({ "start": 2, "end": 4 }))).await;
        assert_eq!(out.content, "l2\nl3\nl4");

        // grep 恒带行号 + 尾注。
        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "l[24]" }))).await;
        assert_eq!(out.content, "2:l2\n4:l4\nshowing 2 of 2 matches");

        // wc。
        let out = run(&t, args(&ref_id, "wc", json!({}))).await;
        assert_eq!(out.content, format!("lines: 5, bytes: {}", content.len()));
    }

    // ── 边界：末行无换行 / CRLF / 空输出 ─────────────────────────────

    #[tokio::test]
    async fn tail_counts_unterminated_last_line() {
        let (_dir, store) = setup_store().await;
        // 3 行，末行无换行（str::lines() 仍记 3 行）。
        let content = "a\nb\nc";
        let ref_id = store_output(&store, content).await;
        let t = tool(&store);

        let out = run(&t, args(&ref_id, "tail", json!({ "n": 2 }))).await;
        assert_eq!(out.content, "b\nc", "末行无换行也算一行");

        let out = run(&t, args(&ref_id, "wc", json!({}))).await;
        assert_eq!(
            out.content,
            format!("lines: 3, bytes: {}\nnote: no trailing newline", content.len())
        );

        // tail n 超过总行数 → 全文。
        let out = run(&t, args(&ref_id, "tail", json!({ "n": 50 }))).await;
        assert_eq!(out.content, "a\nb\nc");
    }

    #[tokio::test]
    async fn crlf_keeps_carriage_return_in_lines() {
        let (_dir, store) = setup_store().await;
        let content = "one\r\ntwo\r\nthree\r\n";
        let ref_id = store_output(&store, content).await;
        let t = tool(&store);

        // \r 留在行内（不按 str::lines() 剥离）。
        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "two" }))).await;
        assert_eq!(out.content, "2:two\r\nshowing 1 of 1 matches");

        let out = run(&t, args(&ref_id, "head", json!({ "n": 1 }))).await;
        assert_eq!(out.content, "one\r");
    }

    #[tokio::test]
    async fn empty_content_degrades_cleanly() {
        let (_dir, store) = setup_store().await;
        let ref_id = store_output(&store, "").await;
        let t = tool(&store);

        let out = run(&t, args(&ref_id, "wc", json!({}))).await;
        assert_eq!(out.content, "lines: 0, bytes: 0");

        let out = run(&t, args(&ref_id, "head", json!({}))).await;
        assert_eq!(out.content, "");

        let out = run(&t, args(&ref_id, "tail", json!({ "n": 5 }))).await;
        assert_eq!(out.content, "");
    }

    // ── 边界：1MB 单行 + 64KB 块边界跨越 ─────────────────────────────

    #[tokio::test]
    async fn single_megabyte_line_survives_all_ops() {
        let (_dir, store) = setup_store().await;
        let big = format!("x{}", "y".repeat(1_048_576)); // 1MiB + 1 单行
        let ref_id = store_output(&store, &big).await;
        let t = tool(&store);

        // head/tail n=1 → 整行（读块恒 64KB，行缓冲按行长累积）。
        let out = run(&t, args(&ref_id, "head", json!({ "n": 1 }))).await;
        // 注意：默认 cap（64KiB，未接线 AppState）会封顶——这里恰好同时
        // 覆盖「多字节无关的纯 ASCII 封顶」路径。
        assert!(out.content.contains(&truncation_notice(65_536)));
        assert!(out.content.len() < big.len());

        let out = run(&t, args(&ref_id, "tail", json!({ "n": 1 }))).await;
        assert!(out.content.starts_with("xy"));
        assert!(out.content.contains(&truncation_notice(65_536)));

        let out = run(&t, args(&ref_id, "wc", json!({}))).await;
        assert_eq!(
            out.content,
            format!("lines: 1, bytes: {}\nnote: no trailing newline", big.len())
        );

        // grep 在超长行上命中：单行 1MiB 超 cap——命中被计数但放不进
        // 输出（M=0），行为钉死。
        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "^x" }))).await;
        assert_eq!(out.content, "showing 0 of 1 matches");
    }

    #[tokio::test]
    async fn line_straddling_64k_chunk_boundary_is_intact() {
        let (_dir, store) = setup_store().await;
        // line1 占到 65530 字节；line2 从 65531 起跨 65536 读块边界。
        // （head n=2 的累计输出会超 64KiB cap，故跨块完整性经 tail /
        // grep / head n=1 三面钉住。）
        let content = format!("{}\n{}\n", "a".repeat(65_530), "b".repeat(100));
        let ref_id = store_output(&store, &content).await;
        let t = tool(&store);

        // head n=1：line1 完整（首块内）。
        let out = run(&t, args(&ref_id, "head", json!({ "n": 1 }))).await;
        assert_eq!(out.content, "a".repeat(65_530));

        // tail n=1：line2 跨块重组完整。
        let out = run(&t, args(&ref_id, "tail", json!({ "n": 1 }))).await;
        assert_eq!(out.content, "b".repeat(100));

        // grep：跨块行带行号命中。
        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "^b+$" }))).await;
        assert_eq!(
            out.content,
            format!("2:{}\nshowing 1 of 1 matches", "b".repeat(100))
        );
    }

    // ── 封顶：notice 文案 + 多字节字符边界 ───────────────────────────

    #[tokio::test]
    async fn cap_appends_notice_and_is_char_boundary_safe() {
        let (_dir, store) = setup_store().await;
        // 单行 90_000B 中文（3B/字）→ head n=1 超 64KiB cap。
        let content = "中".repeat(30_000);
        let ref_id = store_output(&store, &content).await;
        let t = tool(&store);

        let out = run(&t, args(&ref_id, "head", json!({ "n": 1 }))).await;
        assert_eq!(out.status, ToolOutputStatus::Success);
        // 65_536 = 3×21_845 + 1 → 边界回退到 65_535（String 保证合法
        // UTF-8，这里显式钉住截点与 notice 位置）。
        let notice = truncation_notice(65_536);
        assert_eq!(out.content.len(), 65_535 + 1 + notice.len());
        assert!(out.content.ends_with(&notice));
        assert!(out.content.starts_with("中中中"));
        assert_eq!(out.content.chars().filter(|c| *c == '中').count(), 21_845);

        // cap_with_notice 直测：不限档（0）原样返回。
        assert_eq!(cap_with_notice("abc".to_owned(), 0), "abc");
    }

    #[tokio::test]
    async fn grep_cap_shows_m_of_t() {
        let (_dir, store) = setup_store().await;
        // 5000 行 × ~40B ≈ 200KB → 超 64KiB。
        let content: String = (0..5000).map(|i| format!("match-line-{i:04}-paddingpadding\n")).collect();
        let ref_id = store_output(&store, &content).await;
        let t = tool(&store);

        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "match" }))).await;
        assert!(out.content.contains("showing "));
        let note_line = out.content.lines().last().expect("note");
        let (m, t_) = note_line
            .strip_prefix("showing ")
            .and_then(|s| s.strip_suffix(" matches"))
            .and_then(|s| s.split_once(" of "))
            .map(|(m, t)| (m.parse::<usize>(), t.parse::<usize>()))
            .expect("parsable note");
        let (m, t_) = (m.expect("m"), t_.expect("t"));
        assert_eq!(t_, 5000, "全量收集");
        assert!(m < 5000, "封顶生效");
        assert!(out.content.len() <= 65_536 + GREP_NOTE_RESERVE);
    }

    // ── grep ignore_case ──────────────────────────────────────────────

    #[tokio::test]
    async fn grep_ignore_case_flag() {
        let (_dir, store) = setup_store().await;
        let content = "ERROR: boom\ninfo: ok\n";
        let ref_id = store_output(&store, content).await;
        let t = tool(&store);

        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "error" }))).await;
        assert_eq!(out.content, "showing 0 of 0 matches", "默认大小写敏感");

        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "error", "ignore_case": true }))).await;
        assert_eq!(out.content, "1:ERROR: boom\nshowing 1 of 1 matches");
    }

    // ── 参数错误文案 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn argument_errors_are_plain_text_error_outputs() {
        let (_dir, store) = setup_store().await;
        let ref_id = store_output(&store, "x\n").await;
        let t = tool(&store);

        let cases: Vec<(Value, &str)> = vec![
            // ref 缺失 / 格式非法。
            (json!({ "op": "wc" }), "'ref' is required"),
            (args("not-a-ref", "wc", json!({})), "invalid format"),
            (args("out-zzzz", "wc", json!({})), "invalid format"),
            // op 非法。
            (args(&ref_id, "middle", json!({})), "must be one of"),
            (json!({ "ref": ref_id }), "'op' is required"),
            // n 越界。
            (args(&ref_id, "head", json!({ "n": 0 })), "between 1 and 2000"),
            (args(&ref_id, "head", json!({ "n": 2001 })), "between 1 and 2000"),
            (args(&ref_id, "head", json!({ "n": 1.5 })), "must be an integer"),
            // lines 缺 start/end、start>end、start<1。
            (args(&ref_id, "lines", json!({ "end": 3 })), "'start' is required"),
            (args(&ref_id, "lines", json!({ "start": 3 })), "'end' is required"),
            (args(&ref_id, "lines", json!({ "start": 0, "end": 3 })), "must be >= 1"),
            (args(&ref_id, "lines", json!({ "start": 5, "end": 3 })), "must be <= 'end'"),
            // grep 缺 pattern / 非法 regex（编译错误原文回显）。
            (args(&ref_id, "grep", json!({})), "'pattern' is required"),
            (
                args(&ref_id, "grep", json!({ "pattern": "([unclosed" })),
                "invalid regex",
            ),
        ];
        for (bad, needle) in cases {
            let out = run(&t, bad.clone()).await;
            assert_eq!(out.status, ToolOutputStatus::Error, "args: {bad}");
            assert!(
                out.content.starts_with("Invalid output_query arguments: "),
                "prefix: {}",
                out.content
            );
            assert!(out.content.contains(needle), "want {needle:?} in {}", out.content);
        }

        // 非法 regex 的编译错误原文回显。
        let out = run(&t, args(&ref_id, "grep", json!({ "pattern": "([unclosed" }))).await;
        assert!(
            out.content.contains("([unclosed"),
            "regex 编译错误原文回显: {}",
            out.content
        );
    }

    // ── 数据链错误：未知 ref / 完整性 / 装配前 ─────────────────────────

    #[tokio::test]
    async fn unknown_ref_error_mentions_marker_hint() {
        let (_dir, store) = setup_store().await;
        let t = tool(&store);

        // 合法格式但无行。
        let out = run(&t, args("out-0000000000000000", "wc", json!({}))).await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(out.content.contains("unknown stored-output ref \"out-0000000000000000\""));
        assert!(
            out.content.contains("[STORED OUTPUT ref="),
            "提示从标记取 ref: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn orig_bytes_mismatch_reports_integrity_error() {
        let (_dir, store) = setup_store().await;
        // orig_bytes 伪造 +42 → 解压后 len != orig_bytes。
        let content = "hello\nworld\n";
        let ref_id = store_output_raw(&store, content, content.len() + 42, 2).await;
        let t = tool(&store);

        let out = run(&t, args(&ref_id, "wc", json!({}))).await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(out.content.contains("integrity check failed"));
        assert!(
            out.content
                .contains(&format!("decoded {} bytes, expected {}", content.len(), content.len() + 42)),
            "got: {}",
            out.content
        );
    }

    #[tokio::test]
    async fn store_not_wired_returns_explicit_error() {
        let t = OutputQueryTool::new(Arc::new(OnceLock::new()));
        let out = run(&t, args("out-0000000000000000", "wc", json!({}))).await;
        assert_eq!(out.status, ToolOutputStatus::Error);
        assert!(
            out.content.contains("not available"),
            "装配前调用的明确错误文案: {}",
            out.content
        );
    }

    // ── LineScanner 单元直测（EINTR 之外的结构性边界）──────────────

    #[test]
    fn scanner_handles_empty_and_unterminated() {
        let mut line = Vec::new();
        let mut s = LineScanner::new(&b""[..]);
        assert!(!s.next_line(&mut line).unwrap());
        assert!(line.is_empty());

        let mut s = LineScanner::new(&b"a\n\nb"[..]);
        assert!(s.next_line(&mut line).unwrap());
        assert_eq!(line, b"a");
        assert!(s.next_line(&mut line).unwrap());
        assert_eq!(line, b"", "空行也是一行");
        assert!(s.next_line(&mut line).unwrap());
        assert_eq!(line, b"b");
        assert!(!s.next_line(&mut line).unwrap());
    }

    #[test]
    fn scanner_skip_lines_stops_at_eof() {
        let mut line = Vec::new();
        let mut s = LineScanner::new(&b"1\n2\n3\n"[..]);
        assert_eq!(s.skip_lines(2, &mut line).unwrap(), 2);
        assert!(s.next_line(&mut line).unwrap());
        assert_eq!(line, b"3");
        assert_eq!(s.skip_lines(10, &mut line).unwrap(), 0, "EOF 后跳 0");
    }
}
