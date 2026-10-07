//! 模型元数据注册表的本地存储与更新（数据源页面后端）。
//!
//! - 存储：`{config_dir}/registry/` 下每源一个 ZSTD 压缩 JSON
//!   （`<id>.json.zst`）+ `meta.json`（拉取时间 / 条目数 / 体积）。
//!   数据一旦拉取**常驻手机本地**，查询零网络。
//! - 更新：手动（页面按钮，[`update_source`）+ 启动时按各源
//!   `stale_after` 自动触发（[`auto_update_if_stale`]：models.dev 一天、
//!   LiteLLM / CloudPrice 一周）。
//! - 查询：[`lookup_local`] 三源本地合并（源优先级 models.dev >
//!   LiteLLM > CloudPrice，逐字段取最高优先源的非空值，见 core
//!   `merge_metas`）；全部 miss 时可选在线单查 CloudPrice 兜底
//!   （[`lookup_with_online_fallback`]）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use openslate_core::model_registry::{
    extract_litellm, extract_models_dev, http_get_json, is_model_entry, lookup_in_map, merge_metas,
    norm_id, parse_source_map, probe_cloudprice_by_id, source_by_id, ModelMeta, SourceDef,
    SourceFormat, SOURCES,
};

/// meta.json 里单个源的状态。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SourceStatus {
    pub fetched_at_ms: Option<u64>,
    pub entries: Option<usize>,
    /// 原始 JSON 字节数（压缩前）。
    pub raw_bytes: Option<u64>,
    /// 压缩后字节数。
    pub zst_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct MetaFile {
    /// 格式版本（未来 schema 变更时迁移用）。
    v: u32,
    sources: HashMap<String, SourceStatus>,
}

/// 注册表存储（文件 + 解析缓存的看门人）。解析后的索引 map 常驻内存
/// （3MB JSON → 索引后 ~十几 MB 峰值构建期，移动端可承受；构建一次
/// 终身复用，更新时重建）。
pub struct RegistryStore {
    dir: PathBuf,
    parsed: Mutex<HashMap<String, HashMap<String, serde_json::Value>>>,
    /// 解码后的**原始 JSON** 缓存。搜索 / 浏览要遍历原始键（保留
    /// provider 前缀），而 `parsed` 是折叠后的归一化索引（尾段键、
    /// first-wins），原始键已丢，不能复用。与 `parsed` 同生命周期
    /// （`update_source` 成功后一并失效）；首次搜索才加载——
    /// ~3MB JSON 的 Value 树内存开销不菲，按需付出。Arc 共享，
    /// 查询方零整树克隆。
    raw: Mutex<HashMap<String, Arc<serde_json::Value>>>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn zstd_level() -> i32 {
    3
}

impl RegistryStore {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            dir: config_dir.join("registry"),
            parsed: Mutex::new(HashMap::new()),
            raw: Mutex::new(HashMap::new()),
        }
    }

    fn data_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json.zst"))
    }

    fn meta_path(&self) -> PathBuf {
        self.dir.join("meta.json")
    }

    fn read_meta(&self) -> MetaFile {
        let Ok(text) = std::fs::read_to_string(self.meta_path()) else {
            return MetaFile { v: 1, sources: HashMap::new() };
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            crate::alog!("registry: meta.json 损坏，按空处理（{e}）");
            MetaFile { v: 1, sources: HashMap::new() }
        })
    }

    fn write_meta(&self, meta: &MetaFile) {
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.dir.join("meta.json.tmp");
        if let Ok(text) = serde_json::to_string_pretty(meta) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, self.meta_path());
            }
        }
    }

    /// 各源状态（页面渲染）。
    pub fn sources_json(&self) -> String {
        let meta = self.read_meta();
        let arr: Vec<serde_json::Value> = SOURCES
            .iter()
            .map(|s| {
                let st = meta.sources.get(s.id).cloned().unwrap_or_default();
                serde_json::json!({
                    "id": s.id,
                    "name": s.name,
                    "desc": s.desc,
                    "url": s.url,
                    "fetched_at_ms": st.fetched_at_ms,
                    "entries": st.entries,
                    "raw_bytes": st.raw_bytes,
                    "zst_bytes": st.zst_bytes,
                    "stale_after_hrs": s.stale_after.as_secs() / 3600,
                })
            })
            .collect();
        serde_json::json!({ "sources": arr }).to_string()
    }

    /// 手动/自动更新一个源：拉取 → 解析计数 → ZSTD 压缩 → 原子写 →
    /// 更新 meta → 失效内存解析缓存。返回条目数。
    pub async fn update_source(&self, id: &str) -> anyhow::Result<usize> {
        let def = source_by_id(id)
            .ok_or_else(|| anyhow::anyhow!("未知数据源 '{id}'"))?;
        let v = http_get_json(def.url).await?;
        let map = parse_source_map(def.format, &v)?;
        let entries = map.len();
        let raw = serde_json::to_vec(&v)?;
        let zst = zstd::encode_all(raw.as_slice(), zstd_level())?;
        let _ = std::fs::create_dir_all(&self.dir);
        let tmp = self.dir.join(format!("{id}.json.zst.tmp"));
        std::fs::write(&tmp, &zst)?;
        std::fs::rename(&tmp, self.data_path(id))?;
        let mut meta = self.read_meta();
        meta.v = 1;
        meta.sources.insert(
            id.to_owned(),
            SourceStatus {
                fetched_at_ms: Some(now_ms()),
                entries: Some(entries),
                raw_bytes: Some(raw.len() as u64),
                zst_bytes: Some(zst.len() as u64),
            },
        );
        self.write_meta(&meta);
        self.parsed
            .lock()
            .expect("registry parsed lock poisoned")
            .remove(id);
        self.raw
            .lock()
            .expect("registry raw lock poisoned")
            .remove(id);
        crate::alog!(
            "registry: updated '{}' → {entries} entries ({}B → {}B zst{})",
            def.name,
            raw.len(),
            zst.len(),
            ""
        );
        Ok(entries)
    }

    /// 启动时自动更新：本地缺失或超过 stale_after 的源**串行**刷新
    /// （避免移动网络并发带宽尖峰）。失败静默（下次启动再试），本地
    /// 旧数据继续可用。
    pub async fn auto_update_if_stale(&self) {
        let meta = self.read_meta();
        for def in SOURCES {
            let fresh = meta
                .sources
                .get(def.id)
                .and_then(|s| s.fetched_at_ms)
                .map(|at| {
                    now_ms().saturating_sub(at) < def.stale_after.as_millis() as u64
                })
                .unwrap_or(false);
            if fresh {
                continue;
            }
            crate::alog!("registry: auto update '{}' (stale/missing)", def.name);
            if let Err(e) = self.update_source(def.id).await {
                crate::alog!("registry: auto update '{}' FAILED (keep old): {e:#}", def.name);
            }
        }
    }

    /// 取某源的解析索引 map（磁盘 ZSTD → JSON → 索引；成功后常驻内存）。
    fn parsed_map(&self, id: &str) -> Option<HashMap<String, serde_json::Value>> {
        {
            let cache = self.parsed.lock().expect("registry parsed lock poisoned");
            if let Some(m) = cache.get(id) {
                return Some(m.clone());
            }
        }
        let def = source_by_id(id)?;
        let zst = std::fs::read(self.data_path(id)).ok()?;
        let raw = zstd::decode_all(zst.as_slice()).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
        let map = parse_source_map(def.format, &v).ok()?;
        self.parsed
            .lock()
            .expect("registry parsed lock poisoned")
            .insert(id.to_owned(), map.clone());
        Some(map)
    }

    /// 取某源解码后的原始 JSON（磁盘 ZSTD → JSON；成功后常驻内存，
    /// Arc 共享）。与 `parsed_map` 分开缓存：搜索要原始键（含 provider
    /// 前缀）与天然键序，索引 map 已折叠丢失。无本地数据返回 None。
    fn raw_value(&self, id: &str) -> Option<Arc<serde_json::Value>> {
        {
            let cache = self.raw.lock().expect("registry raw lock poisoned");
            if let Some(v) = cache.get(id) {
                return Some(v.clone());
            }
        }
        let zst = std::fs::read(self.data_path(id)).ok()?;
        let bytes = zstd::decode_all(zst.as_slice()).ok()?;
        let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let v = Arc::new(v);
        self.raw
            .lock()
            .expect("registry raw lock poisoned")
            .insert(id.to_owned(), v.clone());
        Some(v)
    }

    /// 三源本地合并查询（源优先级 models.dev > LiteLLM > CloudPrice，
    /// 由 `merge_metas` 排序，与遍历序无关）。全 miss → 空 ModelMeta。
    pub fn lookup_local(&self, model_id: &str) -> ModelMeta {
        let mut metas = Vec::new();
        for def in SOURCES {
            if let Some(map) = self.parsed_map(def.id) {
                if let Some(m) = lookup_in_map(def.format, def.name, &map, model_id) {
                    if !m.is_empty() {
                        metas.push(m);
                    }
                }
            }
        }
        merge_metas(metas)
    }

    /// 本地查询；三源全 miss 时在线单查 CloudPrice 兜底（模糊 resolver
    /// 能命中本地库没有的新模型）。
    pub async fn lookup_with_online_fallback(&self, model_id: &str) -> ModelMeta {
        let local = self.lookup_local(model_id);
        if !local.is_empty() {
            return local;
        }
        probe_cloudprice_by_id(model_id)
            .await
            .unwrap_or_default()
    }

    /// 本地条目搜索 / 浏览（数据源页面「查看条目 / 跨源搜索」）。
    /// 纯本地（磁盘 ZSTD + 内存原始 JSON 缓存），同步执行。返回
    /// `{"total":N,"results":[{source,sourceId,id,ctx,out,vision,reasoning,tool,priceIn,priceOut},…]}`。
    ///
    /// - `source_id` 空 = 跨全部**本地已缓存**源搜索（无 zst 数据的源
    ///   跳过）；非空 = 只搜该源；未知 id → `{"total":0,"results":[]}`。
    /// - `query` 空（或归一化后为空，如纯符号）= 浏览模式：按 JSON 键
    ///   的天然顺序（serde_json 默认 BTreeMap 字典序，确定性）取前
    ///   `limit` 条；非空 = 子串匹配 `norm_id(原始键).contains(&norm_id(query))`
    ///   （键含 provider 前缀，尾段子串天然覆盖）。
    /// - `limit <= 0` 按 50 处理；`total` = 匹配总数（分页「共 N 条」），
    ///   `results` 截断到 `limit`。
    /// - `id` = 源 JSON 的**原始键**（保留 provider 前缀）；数值 / 能力
    ///   / 计价字段源没给就为 null（不硬造）。LiteLLM 格式的非模型
    ///   条目（`sample_spec` 之类）按 [`is_model_entry`] 跳过。
    pub fn search_json(&self, source_id: &str, query: &str, limit: i32) -> String {
        let limit = if limit <= 0 { 50 } else { limit as usize };
        let needle = norm_id(query);
        let browse = needle.is_empty();
        let defs: Vec<&'static SourceDef> = if source_id.is_empty() {
            // 只搜有本地数据（zst 在盘）的源；raw_value 会顺带预热缓存。
            SOURCES
                .iter()
                .filter(|s| self.raw_value(s.id).is_some())
                .collect()
        } else if let Some(def) = source_by_id(source_id) {
            vec![def]
        } else {
            return serde_json::json!({ "total": 0, "results": [] }).to_string();
        };

        let mut total = 0usize;
        let mut results: Vec<serde_json::Value> = Vec::new();
        for def in defs {
            let Some(v) = self.raw_value(def.id) else { continue };
            let Some(obj) = v.as_object() else { continue };
            for (key, entry) in obj {
                if !is_model_entry(def.format, entry) {
                    continue;
                }
                if !browse && !norm_id(key).contains(&needle) {
                    continue;
                }
                total += 1;
                if results.len() >= limit {
                    continue; // total 继续累计，results 截断。
                }
                let m = match def.format {
                    SourceFormat::ModelsDev => extract_models_dev(def.name, entry),
                    SourceFormat::LiteLLM => extract_litellm(def.name, entry),
                }
                .unwrap_or_default();
                results.push(serde_json::json!({
                    "source": def.name,
                    "sourceId": def.id,
                    "id": key,
                    "ctx": m.context_tokens,
                    "out": m.max_output_tokens,
                    "vision": m.supports_vision,
                    "reasoning": m.supports_reasoning,
                    "tool": m.supports_tool_call,
                    "priceIn": m.input_price_per_mtok,
                    "priceOut": m.output_price_per_mtok,
                }));
            }
        }
        serde_json::json!({ "total": total, "results": results }).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn update_lookup_and_persistence_roundtrip() {
        // 无网络依赖的存储回路：直接写一个伪 models.dev 数据文件再查。
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path());
        let v = serde_json::json!({
            "zhipuai/glm-4.7": {
                "name": "GLM-4.7",
                "reasoning": true,
                "tool_call": true,
                "limit": {"context": 204800, "output": 131072}
            }
        });
        let raw = serde_json::to_vec(&v).unwrap();
        let zst = zstd::encode_all(raw.as_slice(), zstd_level()).unwrap();
        std::fs::create_dir_all(dir.path().join("registry")).unwrap();
        std::fs::write(dir.path().join("registry/models_dev.json.zst"), zst).unwrap();

        let m = store.lookup_local("glm-4.7");
        assert_eq!(m.context_tokens, Some(204800));
        assert_eq!(m.supports_reasoning, Some(true));

        // 进程内换一个 store 实例（清内存缓存）仍可查（磁盘持久）。
        let store2 = RegistryStore::new(dir.path());
        assert_eq!(store2.lookup_local("glm-4.7").context_tokens, Some(204800));

        // sources_json 结构完整。
        let sj: serde_json::Value =
            serde_json::from_str(&store2.sources_json()).unwrap();
        let arr = sj["sources"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert!(arr.iter().any(|s| s["id"] == "models_dev"));
    }

    #[test]
    fn stale_logic_via_meta() {
        let dir = tempfile::tempdir().unwrap();
        let store = RegistryStore::new(dir.path());
        // 无 meta：视为过期（自动更新会处理全部源）。
        let meta = store.read_meta();
        assert!(meta.sources.is_empty());
    }

    // ── 搜索 / 浏览 ─────────────────────────────────────────────

    /// 直写一个源的伪数据文件（复用 update 测试的无网络直写模式）。
    fn write_source(base: &Path, id: &str, v: &serde_json::Value) {
        let raw = serde_json::to_vec(v).unwrap();
        let zst = zstd::encode_all(raw.as_slice(), zstd_level()).unwrap();
        std::fs::create_dir_all(base.join("registry")).unwrap();
        std::fs::write(
            base.join("registry").join(format!("{id}.json.zst")),
            zst,
        )
        .unwrap();
    }

    /// 搜索测试夹具：models_dev 3 条（glm × 2 + gpt × 1）+
    /// litellm 1 条 glm（带计价）+ 1 条非模型条目；cloudprice 无本地
    /// 数据（跨源搜索应跳过该源）。
    fn search_fixture(base: &Path) -> RegistryStore {
        write_source(
            base,
            "models_dev",
            &serde_json::json!({
                "zhipuai/glm-4.7": {
                    "name": "GLM-4.7",
                    "reasoning": true,
                    "tool_call": true,
                    "modalities": {"input": ["text"], "output": ["text"]},
                    "limit": {"context": 204800, "output": 131072}
                },
                "zhipuai/glm-4.6v": {
                    "name": "GLM-4.6V",
                    "reasoning": true,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 128000, "output": 32768}
                },
                "openai/gpt-5": {
                    "name": "GPT-5",
                    "limit": {"context": 1000000, "output": 128000}
                }
            }),
        );
        write_source(
            base,
            "litellm",
            &serde_json::json!({
                "zai/glm-4.7": {
                    "max_input_tokens": 204800,
                    "max_output_tokens": 131072,
                    "input_cost_per_token": 6e-7,
                    "output_cost_per_token": 2.2e-6
                },
                "sample_spec": {"note": "非模型条目，应被跳过"}
            }),
        );
        RegistryStore::new(base)
    }

    fn run_search(
        store: &RegistryStore,
        source_id: &str,
        query: &str,
        limit: i32,
    ) -> (u64, Vec<serde_json::Value>) {
        let v: serde_json::Value =
            serde_json::from_str(&store.search_json(source_id, query, limit)).unwrap();
        (
            v["total"].as_u64().unwrap(),
            v["results"].as_array().unwrap().clone(),
        )
    }

    #[test]
    fn search_browse_mode_and_field_mapping() {
        let dir = tempfile::tempdir().unwrap();
        let store = search_fixture(dir.path());
        // 浏览模式：query 空、limit<=0（按 50 处理）→ 全部条目，
        // 键字典序（serde_json 默认 BTreeMap）。
        let (total, results) = run_search(&store, "models_dev", "", 0);
        assert_eq!(total, 3);
        assert_eq!(results.len(), 3);
        let ids: Vec<&str> = results.iter().map(|r| r["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            ["openai/gpt-5", "zhipuai/glm-4.6v", "zhipuai/glm-4.7"],
            "原始键保留 provider 前缀，字典序"
        );
        // 字段映射 + 缺失为 null（不硬造）。
        let glm = &results[2];
        assert_eq!(glm["source"], "models.dev");
        assert_eq!(glm["sourceId"], "models_dev");
        assert_eq!(glm["ctx"], 204800);
        assert_eq!(glm["out"], 131072);
        assert_eq!(glm["reasoning"], true);
        assert_eq!(glm["tool"], true);
        assert_eq!(glm["vision"], false, "text 输入 → 非 vision");
        assert!(glm["priceIn"].is_null(), "models.dev 无计价 → null");
        let gpt = &results[0];
        assert!(gpt["reasoning"].is_null() && gpt["tool"].is_null() && gpt["vision"].is_null());
        // 第二次调用走内存原始缓存，结果一致。
        let (total2, results2) = run_search(&store, "models_dev", "", 0);
        assert_eq!((total2, results2.len()), (3, 3));
    }

    #[test]
    fn search_limit_truncation_keeps_total() {
        let dir = tempfile::tempdir().unwrap();
        let store = search_fixture(dir.path());
        let (total, results) = run_search(&store, "models_dev", "", 2);
        assert_eq!(total, 3, "total = 匹配总数，不受 limit 影响");
        assert_eq!(results.len(), 2);
        // 跨源浏览：SOURCES 顺序取满即止（前 2 条都来自首个有数据的源）。
        let (total, results) = run_search(&store, "", "", 2);
        assert_eq!(total, 4, "3 (models_dev) + 1 (litellm，sample_spec 不算)");
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r["sourceId"] == "models_dev"));
    }

    #[test]
    fn search_query_matches_and_source_filter() {
        let dir = tempfile::tempdir().unwrap();
        let store = search_fixture(dir.path());
        // 指定源 + 子串匹配（尾段子串）。
        let (total, results) = run_search(&store, "models_dev", "glm", 50);
        assert_eq!(total, 2);
        assert!(results.iter().all(|r| r["id"].as_str().unwrap().starts_with("zhipuai/glm-")));
        // 双侧归一化：大小写 / 分隔符不敏感；尾段与全前缀键都能命中。
        assert_eq!(run_search(&store, "models_dev", "GLM 4.7", 50).0, 1);
        assert_eq!(run_search(&store, "models_dev", "zhipuai/glm-4.7", 50).0, 1);
        // 无命中。
        assert_eq!(run_search(&store, "models_dev", "claude", 50).0, 0);
        // 空 source_id 跨源：命中 litellm 条目（计价字段 USD/Mtok）。
        let (total, results) = run_search(&store, "", "glm", 50);
        assert_eq!(total, 3, "models_dev ×2 + litellm ×1（cloudprice 无数据跳过）");
        let lt: Vec<&serde_json::Value> =
            results.iter().filter(|r| r["sourceId"] == "litellm").collect();
        assert_eq!(lt.len(), 1);
        assert_eq!(lt[0]["id"], "zai/glm-4.7", "原始键保留 litellm 前缀");
        assert_eq!(lt[0]["source"], "LiteLLM");
        assert_eq!(lt[0]["ctx"], 204800);
        assert_eq!(lt[0]["priceIn"], 0.6, "per-token × 1e6");
        assert_eq!(lt[0]["priceOut"], 2.2);
        assert!(lt[0]["reasoning"].is_null(), "litellm 条目未给能力 → null");
    }

    #[test]
    fn search_unknown_or_uncached_source() {
        let dir = tempfile::tempdir().unwrap();
        let store = search_fixture(dir.path());
        // 未知 source_id → 空结果（契约）。
        let (total, results) = run_search(&store, "no_such_source", "glm", 50);
        assert_eq!(total, 0);
        assert!(results.is_empty());
        // 已知但无本地数据的源（cloudprice 未拉取）→ 同样空结果。
        let (total, results) = run_search(&store, "cloudprice", "", 50);
        assert_eq!((total, results.len()), (0, 0));
        // 跨源模式跳过无数据源，不误报。
        assert_eq!(run_search(&store, "", "cloudprice-only-nothing", 50).0, 0);
    }
}
