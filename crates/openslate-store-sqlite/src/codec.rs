//! `messages.content_zst` 列的编解码（存储 v5，设计 §1/§8.3）。
//!
//! 空间热点集中在消息载荷（reasoning 全文 + 截断的工具输出），整体用
//! zstd level 3 压缩（与 mobile `registry.rs::zstd_level()` 一致；64KB
//! 文本压缩 <1ms，交互路径无感）。zstd 帧自带 magic（`0x28 B5 2F FD`），
//! 列内数据自描述，无需标志列。
//!
//! decode 失败**绝不静默**：错误信息携带消息 id（调用方传入），损坏的
//! 历史要显式暴露而不是悄悄丢行。

use openslate_core::error::StoreError;

/// 压缩等级（level 3：手机档位收益/耗时平衡点，见设计 §1）。
const ZSTD_LEVEL: i32 = 3;

/// 压缩消息 JSON 文本 → `content_zst` 列字节。
///
/// 纯内存编码（输出为 `Vec`，仅 OOM 才可能失败），不返回 `Result`。
pub fn encode_content(content: &str) -> Vec<u8> {
    // in-memory encode 到 Vec 不会产生 I/O 错误；expect 仅作 OOD 防线。
    zstd::encode_all(content.as_bytes(), ZSTD_LEVEL)
        .expect("zstd in-memory encode into Vec cannot fail")
}

/// 解压 `content_zst` 列字节 → 消息 JSON 文本。
///
/// `message_id` 仅用于错误信息定位（哪条消息损坏），不参与解码。
/// 失败（数据损坏 / 非 UTF-8）返回带消息 id 的 [`StoreError::QueryError`]。
pub fn decode_content(message_id: &str, data: &[u8]) -> Result<String, StoreError> {
    let decoded = zstd::decode_all(data).map_err(|e| {
        StoreError::QueryError(format!(
            "message '{message_id}' content_zst decode failed (corrupted zstd frame): {e}"
        ))
    })?;
    String::from_utf8(decoded).map_err(|e| {
        StoreError::QueryError(format!(
            "message '{message_id}' content_zst decoded to invalid utf-8: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_content() {
        let samples = [
            r#"{"role":"user","content":"hello"}"#,
            "",
            "你好世界 🌏 multiple languages",
            &"x".repeat(64 * 1024),
        ];
        for s in samples {
            let encoded = encode_content(s);
            let decoded = decode_content("msg-test", &encoded).expect("decode");
            assert_eq!(decoded, s);
        }
    }

    #[test]
    fn zstd_frame_magic_present() {
        let encoded = encode_content("hello");
        assert_eq!(&encoded[..4], &[0x28, 0xB5, 0x2F, 0xFD], "zstd magic");
    }

    #[test]
    fn repetitive_text_actually_shrinks() {
        let raw = "a".repeat(100_000);
        let encoded = encode_content(&raw);
        assert!(
            encoded.len() < raw.len() / 10,
            "highly repetitive text must compress hard: {} -> {}",
            raw.len(),
            encoded.len()
        );
    }

    #[test]
    fn corrupted_bytes_fail_with_message_id() {
        let mut encoded = encode_content(r#"{"role":"user"}"#);
        // 篡改 zstd magic（首字节）——解码必然失败
        encoded[0] = encoded[0].wrapping_add(1);
        let err = decode_content("msg-corrupt-42", &encoded).expect_err("must fail");
        let msg = err.to_string();
        assert!(msg.contains("msg-corrupt-42"), "error must carry message id, got: {msg}");
    }

    #[test]
    fn empty_input_rejected_on_decode() {
        let err = decode_content("msg-empty", &[]).expect_err("empty slice is not a zstd frame");
        assert!(err.to_string().contains("msg-empty"));
    }
}
