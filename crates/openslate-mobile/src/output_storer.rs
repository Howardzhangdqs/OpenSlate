//! 长输出落盘 storer（长输出落盘 + output_query 设计 C3）。
//!
//! [`MobileOutputStorer`] 实现 core 的 `ToolOutputStorer` 接缝（C2）：
//! 超过 `[limits].output_store_threshold` 的工具输出全文压缩落盘
//! （store `tool_outputs` 行 + `files/outputs/{sha256}.zst`），对话内
//! 由 core 替换为头尾预览（`build_preview`），详情弹窗全文闭环（C6，
//! `runtime.rs::tool_output_by_call_id`）与 `output_query` 工具按
//! `ref` 取回。ref 是**会话内唯一**的自增短 id（`out-N`，本模块分配；
//! 严格会话隔离——见 `store` 实现）。
//!
//! ## 失败语义（绝不失败回合）
//!
//! `store` 任何一步失败（store 未注入 / 压缩 / 写文件 / 插行）都返回
//! `None` 并 `alog!` 告警——core 回落旧 limit 截断路径，回合照常进行。
//! 落盘是**增益路径**，不是依赖路径。
//!
//! ## blocking 约定
//!
//! 对齐 attachments 先例（`insert_attachment` 在 store 方法内同步执行
//! fs 写、mobile 侧直接 await）：压缩为纯内存操作（zstd-3，与
//! `registry.rs::zstd_level` / store `codec.rs` 同级），fs 写经
//! [`SqliteStore::insert_tool_output`] 同步完成，单次落盘在交互路径的
//! 开销为毫秒级，不单独上 spawn_blocking。
//!
//! ## 生命周期
//!
//! store 句柄经 `SharedStore`（`OnceLock` 后注，AttachmentDataProxy 同款
//! 接缝）在 `assemble_state` 之后由 `create_with` 注入；进程级单槽
//! （`set_tool_output_storer`）随进程生灭，**shutdown 无需清理**（mobile
//! 单进程 App，重启即重建）。

use std::sync::Arc;

use async_trait::async_trait;
use openslate_core::output_store::{StoredOutput, ToolOutputStorer};
use openslate_store_sqlite::store::SqliteStore;
use sha2::{Digest, Sha256};

/// 压缩等级（level 3：与 store `codec.rs` / `registry.rs` 同一先例，
/// 手机档位收益/耗时平衡点）。
const ZSTD_LEVEL: i32 = 3;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// mobile 落盘实现：sha256 content-addressing → zstd-3 压缩 →
/// `insert_tool_output`（文件 + 行）。
///
/// **ref 分配 = 会话内自增序号**（用户拍板 2026-10-22）：先按
/// `(run_id, sha256)` 查既有行——命中**复用其 ref**（同内容同 ref，序号
/// 不因幂等跳号）；未命中分配 `out-{max+1}`（`max_tool_output_ref_num`
/// 取该 run 现有最大序号，`idx_tool_outputs_run_ref` 唯一索引兜底）。
/// ref 只在本 run 内有效/可解析（store `find_tool_output` 严格
/// `(run_id, ref)` 匹配）。
///
/// store 注入前（装配窗口内理论上不可达——注入在引擎首个回合前完成）
/// `store` 返回 `None`，core 直通旧路径。
pub struct MobileOutputStorer {
    /// `assemble_state` 后注（`AttachmentDataProxy` 同款接缝，共享
    /// `attachments::SharedStore` 类型：工厂闭包在装配前构造）。
    store: crate::attachments::SharedStore,
}

impl MobileOutputStorer {
    pub fn new(store: crate::attachments::SharedStore) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ToolOutputStorer for MobileOutputStorer {
    async fn store(&self, run_id: &str, tool_name: &str, content: &str) -> Option<StoredOutput> {
        let store: Arc<SqliteStore> = self.store.get().cloned()?;
        let sha_hex = Sha256::digest(content.as_bytes());
        let sha_hex = format!("{sha_hex:x}");
        let line_count = content.lines().count();
        let orig_bytes = content.len();

        // 幂等短路：同 run 同 sha 已落盘 → 复用既有行的 ref（不重复插
        // 行、不消耗新序号）。查询失败按「不可用」处理回落旧路径。
        match store.find_tool_output_by_sha(run_id, &sha_hex).await {
            Ok(Some(existing)) => {
                return Some(StoredOutput {
                    ref_id: existing.ref_id,
                    orig_bytes,
                    line_count,
                });
            }
            Ok(None) => {}
            Err(e) => {
                crate::alog!("output_storer: 按 sha 查既有行失败（run={run_id}），回落旧截断路径: {e}");
                return None;
            }
        }

        let data_zst = match zstd::encode_all(content.as_bytes(), ZSTD_LEVEL) {
            Ok(z) => z,
            Err(e) => {
                crate::alog!("output_storer: zstd 压缩失败，回落旧截断路径: {e}");
                return None;
            }
        };
        // 行 id 与 ref 载体不同：行 id 是 uuid（行级唯一），ref 是会话内
        // 自增序号（`out-N`，消息 content 里的 `[STORED OUTPUT ref="out-…"`
        // 标记携带它）。max+1 分配在 storer 的串行路径上（单回合内工具
        // 结果顺序落盘）；真并发撞号由 store 的唯一索引兜底（insert 报错
        // → 回落旧截断路径）。
        let n = match store.max_tool_output_ref_num(run_id).await {
            Ok(n) => n + 1,
            Err(e) => {
                crate::alog!("output_storer: 读 ref 序号失败（run={run_id}），回落旧截断路径: {e}");
                return None;
            }
        };
        let ref_id = format!("out-{n}");
        // 行 id 是 uuid（行级唯一）。
        let id = format!("out-{}", uuid::Uuid::new_v4().simple());
        // insert 返回**真实生效的 ref**（sha 竞态命中既有行时是其 ref，
        // 而非我们算出的序号）——回执必须与库内行一致。
        let ref_id = match store
            .insert_tool_output(
                &id,
                run_id,
                &ref_id,
                &sha_hex,
                tool_name,
                orig_bytes as i64,
                line_count as i64,
                &data_zst,
                now_ms(),
            )
            .await
        {
            Ok(effective) => effective,
            Err(e) => {
                crate::alog!("output_storer: 落盘失败（run={run_id} tool={tool_name}），回落旧截断路径: {e}");
                return None;
            }
        };
        Some(StoredOutput {
            ref_id,
            orig_bytes,
            line_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 落盘往返：store() → find_tool_output(run, ref) →
    /// read_tool_output_bytes → 解压 == 原文；ref 为会话内自增短 id
    /// （首条 out-1、次条 out-2、同内容幂等复用 out-1）。
    #[tokio::test]
    async fn storer_roundtrip_through_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("storer.sqlite");
        let store = Arc::new(
            SqliteStore::new(path.to_str().expect("utf-8"))
                .await
                .expect("store"),
        );
        store.run_migrations().await.expect("migrations");
        store
            .insert_run("run-1", None, "root", "running", "{}", 1000)
            .await
            .expect("run");

        let shared: crate::attachments::SharedStore = Arc::new(std::sync::OnceLock::new());
        let _ = shared.set(store.clone());
        let storer = MobileOutputStorer::new(shared.clone());

        let content = "line-1\nline-2\n中文行\n".to_owned() + &"x".repeat(9000);
        let receipt = storer
            .store("run-1", "bash", &content)
            .await
            .expect("store 成功");
        assert_eq!(receipt.orig_bytes, content.len());
        assert_eq!(receipt.line_count, content.lines().count());
        assert_eq!(receipt.ref_id, "out-1", "会话内首条 → out-1");

        // 不同内容 → 自增 out-2
        let other = "another\n".to_owned() + &"y".repeat(9000);
        let receipt2 = storer.store("run-1", "bash", &other).await.expect("store 2");
        assert_eq!(receipt2.ref_id, "out-2", "次条自增 → out-2");

        // 同内容幂等 → 复用既有 ref（序号不跳号）
        let again = storer.store("run-1", "bash", &content).await.expect("again");
        assert_eq!(again.ref_id, "out-1", "同 run 同 sha → 复用既有 ref");

        // ref 反查（run 内精确匹配）→ 读文件 → 解压 == 原文
        let record = store
            .find_tool_output("run-1", &receipt.ref_id)
            .await
            .expect("find")
            .expect("row");
        assert_eq!(record.orig_bytes as usize, content.len());
        let zst = store
            .read_tool_output_bytes(&record.sha256)
            .await
            .expect("read")
            .expect("file");
        let decoded = zstd::decode_all(zst.as_slice()).expect("decompress");
        assert_eq!(decoded, content.as_bytes(), "解压零损");
    }

    /// 跨会话序号独立 + 隔离：run-2 的首条也从 out-1 起，且 run-1 的
    /// ref 在 run-2 查不到（store 侧严格隔离语义）。
    #[tokio::test]
    async fn storer_refs_are_per_run_scoped() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("storer_runs.sqlite");
        let store = Arc::new(
            SqliteStore::new(path.to_str().expect("utf-8"))
                .await
                .expect("store"),
        );
        store.run_migrations().await.expect("migrations");
        for run in ["run-a", "run-b"] {
            store
                .insert_run(run, None, "root", "running", "{}", 1000)
                .await
                .expect("run");
        }

        let shared: crate::attachments::SharedStore = Arc::new(std::sync::OnceLock::new());
        let _ = shared.set(store.clone());
        let storer = MobileOutputStorer::new(shared);

        let content = "shared-body\n".to_owned() + &"z".repeat(9000);
        let a = storer.store("run-a", "bash", &content).await.expect("a");
        let b = storer.store("run-b", "bash", &content).await.expect("b");
        assert_eq!(a.ref_id, "out-1", "run-a 首条 out-1");
        assert_eq!(b.ref_id, "out-1", "run-b 序号独立，也从 out-1 起");
        assert_eq!(a.ref_id, b.ref_id, "同 ref 不同 run（跨 run 同 sha 各自一行）");

        // run-b 里查 run-a 的行 → None（严格会话隔离）
        assert!(
            store
                .find_tool_output("run-b", "out-2")
                .await
                .expect("find")
                .is_none()
        );
        let hit_b = store
            .find_tool_output("run-b", &b.ref_id)
            .await
            .expect("find")
            .expect("hit");
        assert_eq!(hit_b.run_id, "run-b", "同 ref 值在 run-b 解析到 run-b 自己的行");
    }

    /// 失败回落 None：
    /// ① store 未注入（OnceLock 空）；② 内存库（无 files_dir）insert 报错。
    /// 两者都必须返回 None 而不是 Err/panic——core 回落旧路径。
    /// （目录不可写场景与 ② 同一失败路径——insert_tool_output 内 fs 写
    /// 失败同样映射为 None，此处以内存库覆盖，不依赖权限技巧。）
    #[tokio::test]
    async fn storer_falls_back_to_none_on_failures() {
        // ① 未注入
        let storer = MobileOutputStorer::new(Arc::new(std::sync::OnceLock::new()));
        assert!(
            storer.store("run-x", "bash", "whatever").await.is_none(),
            "store 未注入 → None"
        );

        // ② 内存库 insert fail-fast
        let store = Arc::new(SqliteStore::new_in_memory().await.expect("mem store"));
        store.run_migrations().await.expect("migrations");
        store
            .insert_run("run-1", None, "root", "running", "{}", 1)
            .await
            .expect("run");
        let shared: crate::attachments::SharedStore = Arc::new(std::sync::OnceLock::new());
        let _ = shared.set(store);
        let storer = MobileOutputStorer::new(shared);
        assert!(
            storer
                .store("run-1", "bash", &"y".repeat(5000))
                .await
                .is_none(),
            "内存库写文件失败 → None"
        );
    }
}
