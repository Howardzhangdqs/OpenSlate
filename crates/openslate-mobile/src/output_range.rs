//! 长输出落盘的 seekable 分块压缩层（v8 存储格式）。
//!
//! [`crate::output_storer`] 写侧 + 本模块读侧共同构成「查询永不整篇
//! 解压、内存 O(帧)」的 GB 级极端条件防线：
//!
//! - **写**（[`encode_seekable`]）：`zeekstd::RawEncoder` 增量压缩，按
//!   **行边界**手动断帧——喂入整行、累计 ≥[`FRAME_TARGET_BYTES`]（64KiB）
//!   且行完整时 `end_frame()`；压缩等级 3 + 每帧 XXH64 校验和；产物是
//!   zstd seekable 格式（帧数据 + 尾部 skippable 帧内嵌 seek table，
//!   文件尾 4 字节为小端魔数 `0x8F92EAB1`，读取端据此探测）；
//! - **行级帧索引**（[`FrameEntry`] / [`frame_index_blob`]）：每帧一条
//!   `(首行号 u32, 帧起始解压后字节偏移 u64)`，小端手写打包成 BLOB 随
//!   `tool_outputs.frame_index` 列入库（布局见 store `schema.rs`
//!   `DDL_TOOL_OUTPUTS` kdoc）。seek table 本身只有字节偏移——行号到帧
//!   的映射靠这份索引，`read_lines` 二分定位后只解压覆盖区间；
//! - **读**（[`OutputRangeReader`]）：`std::fs::File` 直接作 zeekstd
//!   `Decoder` 的 Seekable 源（std feature 的 blanket impl：`Read + Seek`
//!   → `Seekable`，GB 级压缩文件不进内存），`set_offset`/`set_offset_
//!   limit`（解压后字节偏移，帧内任意偏移由解码器 dummy 解压跳过）。
//!
//! ## legacy 兼容
//!
//! v8 之前的文件是普通单帧 zstd（无尾魔数）：[`OutputRangeReader::open`]
//! 探测失败返回 `Ok(None)`，调用方回落整篇解压路径（`zstd::decode_all`
//! 对 seekable 文件同样可用——libzstd 跳过 skippable 帧，故 C6 全文
//! 闭环不感知格式差异）。
//!
//! ## 帧边界不变量（正确性根基）
//!
//! 索引里记录的帧边界**恒为行边界**（写侧只在行完整处 `end_frame`），
//! 因此「从条目 k 的偏移开始解压」必然从某行行首开始，行号映射无歧
//! 义。单行超过 [`zeekstd::SEEKABLE_MAX_FRAME_SIZE`]（1GiB）时编码器
//! 自动断帧（帧边界落在行中间）——该物理断帧发生在条目覆盖范围
//! **内部**（条目偏移仍是行边界），字节流连续性由解码器保证，正确性
//! 不受影响；代价是该条目窗口变大（内存 O(条目字节数)），仅影响
//! ≥1GiB 单行的病态输入。
//!
//! ## blocking 约定
//!
//! 对齐 [`crate::output_storer`]：纯 CPU + 有界窗口 fs 读（≤ 覆盖帧
//! 解压量），交互路径毫秒级；grep 是 O(全量) CPU（与写侧压缩同级），
//! 按既有先例不单独 spawn_blocking。

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
// 切分语义），为 output_query 五算子与本文档的窗口化读取改为
// 「跳过 + 拷贝」形态。
// ─────────────────────────────────────────────────────────────────────────────

use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom};

use zeekstd::{DecodeOptions, Decoder};



/// 断帧目标：累计**未压缩**字节数达到该值且行完整时 `end_frame()`。
///
/// 64KiB = 行迭代读块（[`READ_CHUNK`]）同量级：单帧解压内存 + dummy 跳过
/// CPU 都是恒定量；GB 级文件约 16k 帧，帧索引 ≈ 16k × 12B ≈ 192KiB
/// （BLOB 入库可接受）。
pub(crate) const FRAME_TARGET_BYTES: usize = 64 * 1024;

/// 64KB 读块（uutils head `BUF_SIZE` 同值；行迭代核心在本模块，legacy
/// 整篇路径（output_query）与窗口化读取共用）。
pub(crate) const READ_CHUNK: usize = 65_536;

/// 压缩等级（level 3：与 store `codec.rs` / `registry.rs` 同一先例，
/// 手机档位收益/耗时平衡点）。output_storer 委托本模块后此常量移驻。
pub(crate) const ZSTD_LEVEL: i32 = 3;

/// seekable 格式文件尾 4 字节的小端魔数（`zeekstd::SEEKABLE_MAGIC_NUMBER`）。
const SEEKABLE_MAGIC_NUMBER: u32 = 0x8F92_EAB1;

/// 帧索引 BLOB 的当前版本号（布局见 [`frame_index_blob`]）。
const FRAME_INDEX_VERSION: u8 = 1;

// ─────────────────────────────────────────────────────────────────────────────
// 行迭代核心（head / tail / lines / grep / wc 一份共用；legacy 整篇路径与
// 窗口化读取共用同一实现）
// ─────────────────────────────────────────────────────────────────────────────

/// 64KB 块式行迭代器。
///
/// 边界保证（uutils take.rs 改造，见模块文档署名）：
/// - 读块恒定 64KB、`fill_chunk` 里 resize 复用（超长单行不膨胀读块）；
/// - `read` 返回 [`ErrorKind::Interrupted`] 时重试（EINTR）；
/// - 行跨块由调用方 `line` 缓冲拼接（行长即其上界）；
/// - 末行无换行亦产出一行（返回 `true` 且 `line` 为该行内容）。
pub(crate) struct LineScanner<R: Read> {
    reader: R,
    chunk: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read> LineScanner<R> {
    pub(crate) fn new(reader: R) -> Self {
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
    pub(crate) fn next_line(&mut self, line: &mut Vec<u8>) -> std::io::Result<bool> {
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
    pub(crate) fn skip_lines(&mut self, n: usize, line: &mut Vec<u8>) -> std::io::Result<usize> {
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
// 写侧：seekable 编码 + 帧索引
// ─────────────────────────────────────────────────────────────────────────────

/// 帧索引条目：`(首行号, 帧起始解压后字节偏移)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameEntry {
    /// 1-based；该帧首字节所在行的行号（帧边界恒为行边界 ⇒ 恰是帧内
    /// 第一行的行号）。
    pub first_line: u32,
    /// 帧起始「解压后」字节偏移（条目 0 恒为 0；严格递增）。
    pub dstart: u64,
}

/// 把 `content` 压成 seekable 分块格式，返回 `(压缩字节, 帧索引 BLOB)`。
///
/// 断帧规则见模块文档（行边界 + 64KiB 目标）；末尾不足目标的余量单独
/// 成帧；空内容 = 一个合法 zstd 空帧 + **N=0 条帧索引**（读取端对空
/// 内容短路，无需窗口）。每帧开 XXH64 校验和（解码器逐帧验证，崩坏
/// 帧报错不静默）。
pub(crate) fn encode_seekable(content: &str) -> zeekstd::Result<(Vec<u8>, Vec<u8>)> {
    use zeekstd::{EncodeOptions, FrameSizePolicy, SEEKABLE_MAX_FRAME_SIZE};

    // 帧大小策略取 seekable 格式上限（1GiB）：自动断帧仅对 ≥1GiB 单行
    // 生效（正确性不受影响，见模块文档「帧边界不变量」），常规输入的
    // 断帧完全由行边界手动驱动。
    let mut encoder = EncodeOptions::new()
        .checksum_flag(true)
        .compression_level(ZSTD_LEVEL)
        .frame_size_policy(FrameSizePolicy::Uncompressed(SEEKABLE_MAX_FRAME_SIZE as u32))
        .into_raw_encoder()?;

    let mut out: Vec<u8> = Vec::new();
    // 输出缓冲：常规行（≪ 缓冲）一次吞下；巨行按 in_progress 循环推进，
    // 缓冲不足时翻倍（zstd 流式压缩最坏膨胀远低于 2×，这里只做防御）。
    let mut buf: Vec<u8> = vec![0; FRAME_TARGET_BYTES * 2];

    let mut entries: Vec<FrameEntry> = Vec::new();
    let mut frame_bytes: usize = 0; // 当前帧已喂入的未压缩字节数
    let mut frame_first_line: u32 = 1; // 当前帧首行号（帧边界是行边界）
    let mut fed_bytes: u64 = 0; // 全文已喂入字节数（下一帧 dstart = fed_bytes - frame_bytes）

    for (idx, line) in content.split_inclusive('\n').enumerate() {
        let line_no = idx + 1;
        let mut pos = 0;
        while pos < line.len() {
            let prog = encoder.compress(&line.as_bytes()[pos..], &mut buf)?;
            out.extend_from_slice(&buf[..prog.out_progress()]);
            pos += prog.in_progress();
            // 防御：输出缓冲满且输入零消耗 → 翻倍重试（正常不可达）。
            if prog.in_progress() == 0 && prog.out_progress() == buf.len() {
                buf.resize(buf.len() * 2, 0);
            }
        }
        fed_bytes += line.len() as u64;
        frame_bytes += line.len();
        if frame_bytes >= FRAME_TARGET_BYTES {
            end_frame_into(&mut encoder, &mut out, &mut buf)?;
            entries.push(FrameEntry {
                first_line: frame_first_line,
                dstart: fed_bytes - frame_bytes as u64,
            });
            frame_bytes = 0;
            frame_first_line = u32::try_from(line_no + 1).unwrap_or(u32::MAX);
        }
    }
    // 末帧：有余量才成帧（空内容不记条目——读取端按 N=0 短路）。
    if frame_bytes > 0 {
        end_frame_into(&mut encoder, &mut out, &mut buf)?;
        entries.push(FrameEntry {
            first_line: frame_first_line,
            dstart: fed_bytes - frame_bytes as u64,
        });
    } else if entries.is_empty() {
        // 空内容（或恰好全部在行边界断完）：保证至少一个空帧使文件合法。
        end_frame_into(&mut encoder, &mut out, &mut buf)?;
    }

    // seek table 序列化为尾部 skippable 帧（Foot 格式 → 文件尾 4 字节魔数）。
    let mut ser = encoder.into_seek_table().into_serializer();
    loop {
        let n = ser.write_into(&mut buf);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }

    Ok((out, frame_index_blob(&entries)))
}

/// `end_frame()` 循环到 epilogue 全部写完（`data_left() == 0`）。
fn end_frame_into(
    encoder: &mut zeekstd::RawEncoder,
    out: &mut Vec<u8>,
    buf: &mut [u8],
) -> zeekstd::Result<()> {
    loop {
        let prog = encoder.end_frame(buf)?;
        out.extend_from_slice(&buf[..prog.out_progress()]);
        if prog.data_left() == 0 {
            return Ok(());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 帧索引 BLOB（反）序列化
// ─────────────────────────────────────────────────────────────────────────────

/// 帧索引序列化为 BLOB（小端；布局与 store `DDL_TOOL_OUTPUTS` kdoc 一致）：
///
/// ```text
/// u8    版本 = 1
/// u32   条目数 N（空内容 = 0）
/// N × ( u32 首行号; u64 帧起始解压后偏移 )
/// ```
pub(crate) fn frame_index_blob(entries: &[FrameEntry]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(5 + entries.len() * 12);
    blob.push(FRAME_INDEX_VERSION);
    blob.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        blob.extend_from_slice(&e.first_line.to_le_bytes());
        blob.extend_from_slice(&e.dstart.to_le_bytes());
    }
    blob
}

/// 解析 + 校验帧索引 BLOB。空 BLOB 仅当 `orig_bytes == 0` 合法（空内容
/// 写入方记 N=0；读取端所有算子对空内容直接短路）。
pub(crate) fn parse_frame_index(blob: &[u8], orig_bytes: u64) -> Result<Vec<FrameEntry>, String> {
    let bad = |msg: &str| format!("invalid frame index blob: {msg}");
    if blob.is_empty() {
        return if orig_bytes == 0 {
            Ok(Vec::new())
        } else {
            Err(bad("non-empty output must carry a frame index"))
        };
    }
    if blob[0] != FRAME_INDEX_VERSION {
        return Err(bad(&format!("unsupported version {}", blob[0])));
    }
    if blob.len() < 5 {
        return Err(bad("truncated header"));
    }
    let n = u32::from_le_bytes([blob[1], blob[2], blob[3], blob[4]]) as usize;
    if blob.len() != 5 + n * 12 {
        return Err(bad("length does not match entry count"));
    }
    let mut entries = Vec::with_capacity(n);
    for i in 0..n {
        let o = 5 + i * 12;
        let first_line = u32::from_le_bytes([blob[o], blob[o + 1], blob[o + 2], blob[o + 3]]);
        let dstart = u64::from_le_bytes([
            blob[o + 4], blob[o + 5], blob[o + 6], blob[o + 7], blob[o + 8], blob[o + 9],
            blob[o + 10], blob[o + 11],
        ]);
        entries.push(FrameEntry { first_line, dstart });
    }
    // 不变量校验（写入方保证；违反 = 行/文件错位，宁可报错不可静默错读）。
    if let Some(first) = entries.first() {
        if first.first_line != 1 || first.dstart != 0 {
            return Err(bad("first entry must be (line 1, offset 0)"));
        }
        for w in entries.windows(2) {
            if w[1].dstart <= w[0].dstart {
                return Err(bad("offsets must be strictly increasing"));
            }
            if w[1].first_line < w[0].first_line {
                return Err(bad("first-line numbers must be non-decreasing"));
            }
        }
        if entries.last().expect("non-empty").dstart >= orig_bytes {
            return Err(bad("last entry offset must be < orig_bytes"));
        }
    }
    Ok(entries)
}

// ─────────────────────────────────────────────────────────────────────────────
// 读侧：窗口化读取器
// ─────────────────────────────────────────────────────────────────────────────

/// 窗口化读取的错误（output_query 映射为既有错误文案形态）。
#[derive(Debug)]
pub(crate) enum RangeError {
    Io(std::io::Error),
    Zeek(zeekstd::Error),
    /// 解压出的行不是合法 UTF-8（写入方存的是 String，防御分支）。
    Utf8,
    /// 帧索引 BLOB 非法（行/文件错位风险，拒绝服务）。
    Index(String),
    /// full-pass 字节核对失败（grep 全量扫描顺带校验）。
    Integrity { decoded: u64, expected: u64 },
}

impl std::fmt::Display for RangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RangeError::Io(e) => write!(f, "{e}"),
            RangeError::Zeek(e) => write!(f, "{e}"),
            RangeError::Utf8 => write!(f, "stored-output content is not valid UTF-8"),
            RangeError::Index(msg) => write!(f, "{msg}"),
            RangeError::Integrity { decoded, expected } => write!(
                f,
                "decoded {decoded} bytes, expected {expected}"
            ),
        }
    }
}

impl From<std::io::Error> for RangeError {
    fn from(e: std::io::Error) -> Self {
        RangeError::Io(e)
    }
}

impl From<zeekstd::Error> for RangeError {
    fn from(e: zeekstd::Error) -> Self {
        RangeError::Zeek(e)
    }
}

/// grep 扫描结果：保留的命中（`行号:行文本`，字节预算内）+ 全量命中总数。
pub(crate) struct GrepScan {
    pub matches: Vec<String>,
    pub total: usize,
}

/// seekable 文件的窗口化读取器。
///
/// 构造时探测文件尾魔数：非 seekable（legacy 单帧 zstd）→ `Ok(None)`，
/// 调用方回落整篇路径。文件句柄由 zeekstd `Decoder` 持有（std feature
/// 下 `File: Read + Seek` blanket 实现 `Seekable`，GB 级文件不进内存；
/// 构造即从文件尾解析 seek table）。
pub(crate) struct OutputRangeReader {
    decoder: Decoder<'static, File>,
    entries: Vec<FrameEntry>,
    /// 原文行数（`str::lines()` 语义；wc 零解压直读源）。
    line_count: usize,
    /// 原文字节数（帧索引校验 + wc）。
    orig_bytes: u64,
}

impl OutputRangeReader {
    /// 打开 + 探测 + 解析帧索引。`Ok(None)` = legacy 单帧文件（回落整篇
    /// 解压）；`Err(_)` = 探测为 seekable 但打开/索引失败（按解码错误
    /// 上报，不静默）。
    pub(crate) fn open(
        path: &std::path::Path,
        frame_index: &[u8],
        line_count: usize,
        orig_bytes: u64,
    ) -> Result<Option<Self>, RangeError> {
        let mut file = File::open(path).map_err(RangeError::Io)?;
        let len = file.seek(SeekFrom::End(0)).map_err(RangeError::Io)?;
        if len < 4 {
            return Ok(None); // 空/残文件：交给 legacy 分支报正经解码错误
        }
        file.seek(SeekFrom::Start(len - 4))
            .map_err(RangeError::Io)?;
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic).map_err(RangeError::Io)?;
        if u32::from_le_bytes(magic) != SEEKABLE_MAGIC_NUMBER {
            return Ok(None); // legacy 单帧 zstd
        }
        // 魔数命中但构造失败（1/2^32 误报或文件损坏）→ 报错而非回落
        // （回落会把损坏误报成 legacy 解码失败，文案误导）。
        let decoder = DecodeOptions::new(file).into_decoder().map_err(RangeError::Zeek)?;
        let entries =
            parse_frame_index(frame_index, orig_bytes).map_err(RangeError::Index)?;
        Ok(Some(Self {
            decoder,
            entries,
            line_count,
            orig_bytes,
        }))
    }

    /// 行数 / 字节数直读（wc 零解压）。
    pub(crate) fn line_count(&self) -> usize {
        self.line_count
    }
    pub(crate) fn orig_bytes(&self) -> u64 {
        self.orig_bytes
    }

    /// 覆盖行 `a..=b` 的条目区间（二分）：`first` = 最后一个首行号 ≤ a
    /// 的条目（其起始偏移 ≤ 行 a 行首且为行边界），`last` = 最后一个
    /// 首行号 ≤ b 的条目（行 b 的尾部至多延展到 next 条目起点——条目
    /// 边界是行边界，行 b 必然完整落在 ≤ last 的覆盖内）。
    fn covering_entries(&self, a: usize, b: usize) -> (usize, usize) {
        let le = |line: usize| {
            self.entries
                .partition_point(|e| (e.first_line as usize) <= line)
                - 1
        };
        (le(a), le(b))
    }

    /// 条目 `i` 的解压起始偏移 / 覆盖区间终点（末条目 → 文件总解压量）。
    fn entry_start(&self, i: usize) -> u64 {
        self.entries[i].dstart
    }
    fn entry_limit(&self, i: usize) -> u64 {
        self.entries
            .get(i + 1)
            .map(|e| e.dstart)
            .unwrap_or_else(|| self.decoder.seek_table().size_decomp())
    }

    /// 读取行区间 `[a, b]`（1-based 闭区间，越界钳制），行以 `\n` join。
    /// 内存 = 覆盖条目解压量 + 结果（帧边界与行边界不对齐的残余由
    /// 「从行边界起扫 + 裁剪到 [a,b]」吸收）。
    pub(crate) fn read_lines(&mut self, a: usize, b: usize) -> Result<String, RangeError> {
        if self.line_count == 0 || a > self.line_count || b < a {
            return Ok(String::new());
        }
        let b = b.min(self.line_count);
        let (first, last) = self.covering_entries(a, b);
        self.decoder.set_offset(self.entry_start(first))?;
        self.decoder.set_offset_limit(self.entry_limit(last))?;

        let start_line = self.entries[first].first_line as usize;
        let mut scanner = LineScanner::new(&mut self.decoder);
        let mut line = Vec::new();
        // 跳到行 a（覆盖条目可能从更早的行开始）。
        scanner.skip_lines(a - start_line, &mut line)?;
        let mut collected: Vec<String> = Vec::new();
        let mut no = a;
        while no <= b {
            if !scanner.next_line(&mut line)? {
                break; // EOF 防御（line_count 与实际不一致时提前止步）
            }
            collected.push(
                String::from_utf8(std::mem::take(&mut line)).map_err(|_| RangeError::Utf8)?,
            );
            no += 1;
        }
        Ok(collected.join("\n"))
    }

    /// 前 `n` 行（read_lines 特化：帧索引定位到首条目即可）。
    pub(crate) fn read_head(&mut self, n: usize) -> Result<String, RangeError> {
        self.read_lines(1, n)
    }

    /// 后 `n` 行（read_lines 特化：从帧索引尾部定位）。
    pub(crate) fn read_tail(&mut self, n: usize) -> Result<String, RangeError> {
        if self.line_count == 0 || n == 0 {
            return Ok(String::new());
        }
        // 闭区间起点 = total-n+1（n ≥ total 时饱和为 1 → 全文）。
        let start = self.line_count.saturating_sub(n) + 1;
        self.read_lines(start, self.line_count)
    }

    /// 末行是否以 `\n` 结尾（wc 的 trailing-newline 注记；O(1) 内存——
    /// 解压最后 1 字节，CPU 最坏 dummy 跳过末帧 ≈ 一帧）。
    /// 空内容按「无注记」语义返回 `true`。
    pub(crate) fn last_line_terminated(&mut self) -> Result<bool, RangeError> {
        if self.orig_bytes == 0 {
            return Ok(true);
        }
        self.decoder.set_offset(self.orig_bytes - 1)?;
        self.decoder.set_offset_limit(self.orig_bytes)?;
        let mut byte = [0u8; 1];
        let mut filled = 0;
        while filled < 1 {
            let n = self.decoder.decompress(&mut byte[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            return Err(RangeError::Integrity {
                decoded: self.orig_bytes - 1,
                expected: self.orig_bytes,
            });
        }
        Ok(byte[0] == b'\n')
    }

    /// 逐条目顺序流式 grep：每命中一行记 `行号:行文本`，保留集受
    /// `retain_budget` 字节预算封顶（预算外只计数）；全量扫描顺带核对
    /// 解压总字节数 == orig_bytes（完整性：替代 legacy 的整篇 len 校验，
    /// 帧校验和由解码器逐帧自动验证）。内存 O(帧 + 保留结果)。
    pub(crate) fn scan_grep(
        &mut self,
        regex: &regex::Regex,
        retain_budget: usize,
    ) -> Result<GrepScan, RangeError> {
        let mut result = GrepScan {
            matches: Vec::new(),
            total: 0,
        };
        let mut retained_bytes = 0usize;
        let mut bytes_seen: u64 = 0;
        let mut line = Vec::new();

        // 空内容（entries 为空）：单轮 [0, size_decomp) 全量扫描（size
        // 为 0，立即 EOF）。
        let rounds: Vec<(u32, u64, u64)> = if self.entries.is_empty() {
            vec![(1, 0, self.decoder.seek_table().size_decomp())]
        } else {
            (0..self.entries.len())
                .map(|i| {
                    (
                        self.entries[i].first_line,
                        self.entries[i].dstart,
                        self.entry_limit(i),
                    )
                })
                .collect()
        };

        for (first_line, dstart, limit) in rounds {
            self.decoder.set_offset(dstart)?;
            self.decoder.set_offset_limit(limit)?;
            let mut scanner = LineScanner::new(CountingRead {
                inner: &mut self.decoder,
                count: &mut bytes_seen,
            });
            let mut line_no = first_line as usize;
            while scanner.next_line(&mut line)? {
                // 行级 UTF-8 校验（全文来自 String 落盘，正常恒过；防
                // 御分支与 legacy 路径同文案）。
                let text = String::from_utf8(std::mem::take(&mut line))
                    .map_err(|_| RangeError::Utf8)?;
                if regex.is_match(&text) {
                    result.total += 1;
                    if retained_bytes < retain_budget {
                        retained_bytes += text.len() + 1;
                        result.matches.push(format!("{line_no}:{text}"));
                    }
                }
                line_no += 1;
            }
        }

        if bytes_seen != self.orig_bytes {
            return Err(RangeError::Integrity {
                decoded: bytes_seen,
                expected: self.orig_bytes,
            });
        }
        Ok(result)
    }
}

/// 解压字节计数器（grep full-pass 完整性核对的旁路通道）。
struct CountingRead<'a> {
    inner: &'a mut Decoder<'static, File>,
    count: &'a mut u64,
}

impl Read for CountingRead<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        *self.count += n as u64;
        Ok(n)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 落盘 + 打开 reader 的测试脚手架：返回 (TempDir, path, blob, reader)。
    /// TempDir 必须由调用方持有到测试结束。
    fn write_and_open(content: &str) -> (tempfile::TempDir, OutputRangeReader) {
        let dir = tempfile::tempdir().expect("temp dir");
        let (zst, blob) = encode_seekable(content).expect("encode");
        let path = dir.path().join("t.zst");
        std::fs::write(&path, &zst).expect("write");
        let reader = OutputRangeReader::open(
            &path,
            &blob,
            content.lines().count(),
            content.len() as u64,
        )
        .expect("open")
        .expect("seekable");
        (dir, reader)
    }

    /// 参考模型：全文按 `\n` 切行（保留 `\r`、末行无换行也算一行），
    /// 返回 [a,b]（1-based 闭区间）join("\n")——与 output_query 语义一致。
    fn ref_lines(content: &str, a: usize, b: usize) -> String {
        let lines: Vec<&str> = content.split('\n').collect();
        // 末行无换行：split('\n') 的末元素是最后那行（有换行时是空串，
        // 需剥掉——与 str::lines() 计数语义对齐）。
        let lines: Vec<&str> = if content.ends_with('\n') || content.is_empty() {
            lines[..lines.len() - 1].to_vec()
        } else {
            lines
        };
        let a = a.min(lines.len().max(1)); // clamp 由调用方语义决定，这里按 read_lines 契约
        if a > lines.len() || b < a {
            return String::new();
        }
        let b = b.min(lines.len());
        lines[a - 1..b].join("\n")
    }

    /// 多帧文档：300k 行 × 26B ≈ 7.8MB → ~120 帧。
    fn multi_frame_doc() -> String {
        (0..300_000)
            .map(|i| format!("line-{i:06}-paddingpadding\n"))
            .collect()
    }

    // ── 写读往返等价性 ─────────────────────────────────────────────

    /// 多帧文档：read_lines / head / tail 与整篇参考逐字节相等（跨 ~120
    /// 帧采样多个区间：首帧内、跨帧边界、文件尾部、全文）。
    #[test]
    fn roundtrip_multiframe_read_lines_matches_reference() {
        let content = multi_frame_doc();
        let total = content.lines().count();
        let (_dir, mut r) = write_and_open(&content);

        // 帧边界附近：26B/行 → 帧 0/1 边界 ≈ 行 2521（65536/26）。取跨
        // 边界窗口、单帧内窗口、尾部窗口、全文。
        for (a, b) in [
            (1, 10),
            (2_500, 2_600), // 跨帧 0/1 边界
            (65_000, 65_050),
            (total - 5, total),
            (1, total),
        ] {
            assert_eq!(
                r.read_lines(a, b).expect("read"),
                ref_lines(&content, a, b),
                "区间 [{a},{b}]"
            );
        }
        assert_eq!(r.read_head(50).expect("head"), ref_lines(&content, 1, 50));
        assert_eq!(
            r.read_tail(50).expect("tail"),
            ref_lines(&content, total - 49, total)
        );
    }

    /// 64KiB 断帧阈值落在行中间字节（行字节跨阈值，帧边界延到行尾才
    /// 断）：阈值两侧的行完整无损。
    #[test]
    fn roundtrip_threshold_falls_mid_line() {
        // 行 1 = 65530B（含 \n）→ 帧累计 65530 < 65536；行 2 = 200B →
        // 累计 65730 ≥ 65536，断帧发生在行 2 之后（阈值点落在行 2 中间）。
        let content = format!(
            "{}\n{}\n{}\n",
            "a".repeat(65_529),
            "b".repeat(199),
            "tail-line"
        );
        let (_dir, mut r) = write_and_open(&content);
        assert_eq!(content.lines().count(), 3);
        assert_eq!(r.read_lines(1, 3).expect("read"), ref_lines(&content, 1, 3));
        assert_eq!(r.read_lines(2, 2).expect("read"), "b".repeat(199));
        assert_eq!(r.read_tail(1).expect("tail"), "tail-line");
        // 帧索引：单帧起点后余量成第二帧或同帧，条目首行号正确。
        let (zst, blob) = encode_seekable(&content).expect("encode");
        let entries = parse_frame_index(&blob, content.len() as u64).expect("parse");
        assert!(entries.len() == 1 || entries.len() == 2);
        if entries.len() == 2 {
            assert_eq!(entries[1].first_line, 3, "第二帧从行 3 开始");
        }
        let _ = zst;
    }

    /// 末行无换行：末行也算一行，tail/wc 注记语义正确。
    #[test]
    fn roundtrip_unterminated_last_line() {
        let content = format!("{}\n{}\nfinal-no-newline", "x".repeat(100), "y".repeat(100));
        let (_dir, mut r) = write_and_open(&content);
        assert_eq!(r.line_count(), 3);
        assert_eq!(r.read_tail(1).expect("tail"), "final-no-newline");
        assert_eq!(
            r.read_lines(3, 3).expect("read"),
            "final-no-newline",
            "末行无换行也算一行"
        );
        assert!(!r.last_line_terminated().expect("terminated"), "无尾注记");
    }

    /// CRLF：行内 `\r` 保留（切分只按 `\n`）。
    #[test]
    fn roundtrip_crlf_keeps_carriage_returns() {
        let content: String = (0..5000)
            .map(|i| format!("crlf-line-{i:04}\r\n"))
            .collect();
        let (_dir, mut r) = write_and_open(&content);
        assert_eq!(
            r.read_lines(100, 100).expect("read"),
            format!("crlf-line-{:04}\r", 99),
            "\\r 留在行尾"
        );
        assert!(r.last_line_terminated().expect("terminated"));
    }

    /// 单超长行（300KB ≫ 64KiB 帧阈值）：整行跨过阈值，帧只在行尾断
    /// （或全文一帧）；read_lines(1,1) 整行完整。
    #[test]
    fn roundtrip_single_giant_line() {
        let content = format!("header\n{}", "g".repeat(300_000));
        let (_dir, mut r) = write_and_open(&content);
        assert_eq!(r.line_count(), 2);
        let got = r.read_lines(2, 2).expect("read");
        assert_eq!(got.len(), 300_000);
        assert!(got.starts_with("gggg") && got.ends_with("gggg"));
        assert_eq!(got, "g".repeat(300_000));
    }

    /// 空内容：所有算子优雅空返回。
    #[test]
    fn roundtrip_empty_content() {
        let (_dir, mut r) = write_and_open("");
        assert_eq!(r.line_count(), 0);
        assert_eq!(r.orig_bytes(), 0);
        assert_eq!(r.read_lines(1, 10).expect("read"), "");
        assert_eq!(r.read_head(5).expect("head"), "");
        assert_eq!(r.read_tail(5).expect("tail"), "");
        let scan = r
            .scan_grep(&regex::Regex::new(".").expect("re"), 1024)
            .expect("scan");
        assert_eq!(scan.total, 0);
        assert!(r.last_line_terminated().expect("terminated"));
    }

    // ── grep 逐帧扫描 vs 整篇参考 ─────────────────────────────────

    /// 多帧文档 grep：命中集合 + 行号 + 总数与整篇参考一致；保留预算
    /// 封顶后 total 仍为全量。
    #[test]
    fn grep_scan_matches_full_text_reference() {
        let content = multi_frame_doc();
        let re = regex::Regex::new("line-000(123|999|500)").expect("re");
        // 整篇参考
        let expect: Vec<String> = content
            .split_inclusive('\n')
            .enumerate()
            .filter(|(_, l)| re.is_match(l.trim_end_matches('\n')))
            .map(|(i, l)| format!("{}:{}", i + 1, l.trim_end_matches('\n')))
            .collect();
        assert!(!expect.is_empty());

        let (_dir, mut r) = write_and_open(&content);
        let scan = r.scan_grep(&re, usize::MAX).expect("scan");
        assert_eq!(scan.total, expect.len(), "命中总数 = 整篇参考");
        assert_eq!(scan.matches, expect, "命中集合 + 行号逐字节一致");
    }

    /// grep 保留预算封顶：matches 截到预算内，total 仍是全量计数。
    #[test]
    fn grep_scan_retention_cap_counts_all() {
        let content: String = (0..10_000)
            .map(|i| format!("match-{i:05}-body\n"))
            .collect();
        let re = regex::Regex::new("match-").expect("re");
        let (_dir, mut r) = write_and_open(&content);
        let scan = r.scan_grep(&re, 512).expect("scan");
        assert_eq!(scan.total, 10_000, "全量计数不因封顶丢失");
        let retained: usize = scan.matches.iter().map(|m| m.len() + 1).sum();
        assert!(retained <= 512 + 200, "保留集受预算封顶（单条可超冲）");
        assert!(!scan.matches.is_empty());
        assert!(scan.matches.len() < 10_000);
    }

    /// wc 零解压：line_count / orig_bytes 与参考一致（值直读行计量，
    /// 不经解压扫描）。
    #[test]
    fn wc_reads_metrics_without_decompression() {
        let content = multi_frame_doc();
        let (_dir, r) = write_and_open(&content);
        assert_eq!(r.line_count(), content.lines().count());
        assert_eq!(r.orig_bytes(), content.len() as u64);
    }

    // ── 帧索引二分边界 ────────────────────────────────────────────

    /// 请求区间恰跨帧边界 / 恰在帧内 / 越界 clamp。
    #[test]
    fn read_lines_boundary_clamps() {
        // 26B/行 → 帧边界 ≈ 每 2521 行。
        let content = multi_frame_doc();
        let total = content.lines().count();
        let dir = tempfile::tempdir().expect("temp dir");
        let (zst, blob) = encode_seekable(&content).expect("encode");
        let path = dir.path().join("t.zst");
        std::fs::write(&path, &zst).expect("write");
        let entries = parse_frame_index(&blob, content.len() as u64).expect("parse");
        let boundary_line = entries[1].first_line as usize; // 帧 1 的首行号

        let mut r = OutputRangeReader::open(&path, &blob, total, content.len() as u64)
            .expect("open")
            .expect("seekable");

        // 恰在帧内（远离边界）
        assert_eq!(
            r.read_lines(10, 20).expect("read"),
            ref_lines(&content, 10, 20)
        );
        // 恰跨帧边界（[boundary-1, boundary]）
        assert_eq!(
            r.read_lines(boundary_line - 1, boundary_line).expect("read"),
            ref_lines(&content, boundary_line - 1, boundary_line)
        );
        // 边界行本身
        assert_eq!(
            r.read_lines(boundary_line, boundary_line).expect("read"),
            ref_lines(&content, boundary_line, boundary_line)
        );
        // 越界 clamp：end 超总行数、start 超总行数、b < a
        assert_eq!(
            r.read_lines(total - 2, total + 1000).expect("read"),
            ref_lines(&content, total - 2, total)
        );
        assert_eq!(r.read_lines(total + 1, total + 5).expect("read"), "");
        assert_eq!(r.read_lines(5, 3).expect("read"), "");
        // head/tail 超量 → 全文
        assert_eq!(
            r.read_head(1 << 30).expect("read"),
            ref_lines(&content, 1, total)
        );
        assert_eq!(
            r.read_tail(1 << 30).expect("read"),
            ref_lines(&content, 1, total)
        );
    }

    // ── legacy 探测回退 ───────────────────────────────────────────

    /// 手工普通 zstd 文件：open() → Ok(None)（调用方回落整篇路径）。
    #[test]
    fn legacy_plain_zstd_probes_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        let content = "legacy\nplain\nzstd\n";
        let path = dir.path().join("legacy.zst");
        std::fs::write(&path, zstd::encode_all(content.as_bytes(), 3).expect("zst"))
            .expect("write");
        assert!(OutputRangeReader::open(&path, &[], 3, content.len() as u64)
            .expect("open 不报错")
            .is_none());
    }

    /// seekable 文件但帧索引 BLOB 为空且内容非空 → Index 错误（行/文件
    /// 错位风险，拒绝服务而非静默错读）。
    #[test]
    fn seekable_file_with_empty_index_errors() {
        let content = multi_frame_doc();
        let dir = tempfile::tempdir().expect("temp dir");
        let (zst, _blob) = encode_seekable(&content).expect("encode");
        let path = dir.path().join("t.zst");
        std::fs::write(&path, &zst).expect("write");
        let err = match OutputRangeReader::open(&path, &[], 42, content.len() as u64) {
            Err(e) => e,
            Ok(_) => panic!("空索引 + 非空内容必须报错"),
        };
        assert!(matches!(err, RangeError::Index(_)), "got: {err}");
    }

    // ── 帧校验和：崩坏帧报错不静默 ────────────────────────────────

    /// 翻转压缩数据中段一位 → 窗口读取跨该帧时报错（XXH64 帧校验和由
    /// 解码器验证），且 full-pass grep 亦报完整性/解码错误。
    #[test]
    fn corrupted_frame_fails_loudly() {
        let content = multi_frame_doc();
        let (zst, blob) = encode_seekable(&content).expect("encode");
        // 压缩数据中段（避开首尾帧头与尾部 seek table）
        let mut corrupt = zst.clone();
        let mid = corrupt.len() / 2;
        corrupt[mid] ^= 0xFF;

        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("corrupt.zst");
        std::fs::write(&path, &corrupt).expect("write");
        let mut r = OutputRangeReader::open(
            &path,
            &blob,
            content.lines().count(),
            content.len() as u64,
        )
        .expect("open（seek table 完好，构造成功）")
        .expect("seekable");

        // full-pass 必然跨过崩坏帧 → 报错（解码错误或字节核对失败）
        let scan = r.scan_grep(&regex::Regex::new("line").expect("re"), 1024);
        assert!(scan.is_err(), "崩坏帧必须报错而非静默错读");
        // 从头读取跨中段帧的窗口同样报错（帧校验和 / dummy 解压失败）
        let res = r.read_lines(1, content.lines().count());
        assert!(res.is_err(), "跨崩坏帧的窗口读取必须报错");
    }

    // ── LineScanner 单元直测（随迁移自 output_query 移驻）─────────

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

    // ── 帧索引 BLOB 解析边界 ──────────────────────────────────────

    #[test]
    fn frame_index_blob_roundtrip_and_validation() {
        let entries = vec![
            FrameEntry { first_line: 1, dstart: 0 },
            FrameEntry { first_line: 2_522, dstart: 65_536 },
            FrameEntry { first_line: 5_043, dstart: 131_072 },
        ];
        let blob = frame_index_blob(&entries);
        assert_eq!(parse_frame_index(&blob, 196_608).expect("parse"), entries);

        // 版本号错误 / 截断 / 长度不符
        let mut bad = blob.clone();
        bad[0] = 2;
        assert!(parse_frame_index(&bad, 196_608).is_err());
        assert!(parse_frame_index(&blob[..4], 196_608).is_err());
        let mut short = blob.clone();
        short.truncate(short.len() - 1);
        assert!(parse_frame_index(&short, 196_608).is_err());

        // 不变量违反：首条目非 (1,0)、偏移非严格递增、行号降序、末条
        // 偏移 ≥ orig_bytes。
        let bad_first = frame_index_blob(&[FrameEntry { first_line: 2, dstart: 0 }]);
        assert!(parse_frame_index(&bad_first, 100).is_err());
        let non_increasing = frame_index_blob(&[
            FrameEntry { first_line: 1, dstart: 0 },
            FrameEntry { first_line: 5, dstart: 0 },
        ]);
        assert!(parse_frame_index(&non_increasing, 100).is_err());
        let desc_lines = frame_index_blob(&[
            FrameEntry { first_line: 1, dstart: 0 },
            FrameEntry { first_line: 5, dstart: 10 },
            FrameEntry { first_line: 3, dstart: 20 },
        ]);
        assert!(parse_frame_index(&desc_lines, 100).is_err());
        let last_over = frame_index_blob(&[FrameEntry { first_line: 1, dstart: 50 }]);
        assert!(parse_frame_index(&last_over, 50).is_err());

        // 空 BLOB：仅 orig_bytes == 0 合法。
        assert!(parse_frame_index(&[], 0).is_ok());
        assert!(parse_frame_index(&[], 1).is_err());
    }
}
