//! LLM 模型元数据注册表（数据源定义 + 纯解析）。
//!
//! 用途：输入任意 model id → 补全 context / max output / vision /
//! reasoning / tool call / 计价等元数据（Provider 的 `/models` 只返回
//! id 清单，能力字段从这里来）。
//!
//! 数据源决策（见 AGENTS.md「模型元数据数据源」）：
//! - **models.dev** `models.json`：键规范（`zhipuai/glm-4.7`）、字段
//!   干净；用户全国多节点实测可达（2026-10）。主力源，体积小（~400KB）。
//! - **CloudPrice**：全量 LiteLLM-compatible 导出 + 按 id 单查（模糊
//!   resolver），字段全但响应慢。补充源。
//! - **LiteLLM** 全量 JSON：计价最细（per-token），稳定性居中；键名
//!   provider 前缀化，需归一化匹配。
//! - ModelRadar.cn 已剔除（可信度存疑，用户拍板）。
//!
//! 本模块只做纯逻辑（无 IO 状态）：解析、归一化匹配、按源优先级合并
//! （models.dev > LiteLLM > CloudPrice，逐字段取最高优先源的非空值）。
//! 拉取/存储/更新策略在 openslate-mobile 的 registry 模块（本地 ZSTD
//! 持久化 + 手动/定期更新）。

use std::collections::HashMap;
use std::time::Duration;

use serde_json::Value;

// ── 数据源定义 ───────────────────────────────────────────────────

/// 数据源条目格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    /// models.dev `models.json`：`{id, name, reasoning, tool_call,
    /// modalities, limit:{context,output}}`，键为 `provider/model`。
    ModelsDev,
    /// LiteLLM 兼容 map：`{max_input_tokens, max_output_tokens,
    /// supports_*, *_cost_per_token}`，键常带 provider 前缀。
    /// CloudPrice 全量导出也是这个格式。
    LiteLLM,
}

/// 单个数据源的静态定义。
pub struct SourceDef {
    /// 稳定 id（存储文件名 / FFI 参数）。
    pub id: &'static str,
    /// 展示名。
    pub name: &'static str,
    /// 一句话说明（数据源页面展示）。
    pub desc: &'static str,
    /// 全量拉取 URL。
    pub url: &'static str,
    pub format: SourceFormat,
    /// 过期阈值：本地数据超过该时长未更新，启动时自动刷新。
    pub stale_after: Duration,
}

/// 全部数据源（页面顺序即展示顺序）。
pub const SOURCES: &[SourceDef] = &[
    SourceDef {
        id: "models_dev",
        name: "models.dev",
        desc: "模型身份 / 上下文 / 输出限额 / 能力开关；键规范、更新及时，主力源",
        url: "https://models.dev/models.json",
        format: SourceFormat::ModelsDev,
        // 体积小（~400KB）：一天一更。
        stale_after: Duration::from_secs(24 * 3600),
    },
    SourceDef {
        id: "litellm",
        name: "LiteLLM",
        desc: "计价最细（per-token）+ limits / capabilities；键带 provider 前缀，归一化匹配",
        url: "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
        format: SourceFormat::LiteLLM,
        // 全量 ~3MB：一周一更。
        stale_after: Duration::from_secs(7 * 24 * 3600),
    },
    SourceDef {
        id: "cloudprice",
        name: "CloudPrice",
        desc: "LiteLLM-compatible 全量导出（另支持按 id 单查兜底）；响应偏慢，补充源",
        url: "https://ai.cloudprice.net/api/v1/litellm_model_prices.json",
        format: SourceFormat::LiteLLM,
        stale_after: Duration::from_secs(7 * 24 * 3600),
    },
];

pub fn source_by_id(id: &str) -> Option<&'static SourceDef> {
    SOURCES.iter().find(|s| s.id == id)
}

// ── id 归一化与匹配 ──────────────────────────────────────────────

/// 归一化 id：小写、只留字母数字（`glm-4.7` / `GLM_4.7` → `glm47`）。
pub fn norm_id(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// 匹配候选：原始 id、去 provider 前缀（最后一个 `/` 后）、去 `@...`
/// 变体后缀、去尾部日期快照（`-20251222` / `-251222`）。全部归一化后
/// 比对。
pub fn id_candidates(model_id: &str) -> Vec<String> {
    let mut out = vec![norm_id(model_id)];
    let base = model_id.split('@').next().unwrap_or(model_id);
    let tail = base.rsplit('/').next().unwrap_or(base);
    let trimmed = {
        let mut t = tail;
        if let Some(pos) = t.rfind('-') {
            let seg = &t[pos + 1..];
            let digits = seg.chars().all(|c| c.is_ascii_digit());
            let len = seg.chars().count();
            if digits && (len == 6 || len == 8) {
                t = &t[..pos];
            }
        }
        t
    };
    for part in [model_id, base, tail, trimmed] {
        let n = norm_id(part);
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

// ── 归一化元数据与合并 ───────────────────────────────────────────

/// 单源返回的归一化元数据。全 Option：源没给的留空，交给合并。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelMeta {
    pub display_name: Option<String>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_vision: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub supports_tool_call: Option<bool>,
    /// USD / 百万 token（LiteLLM per-token × 1e6）。
    pub input_price_per_mtok: Option<f64>,
    pub output_price_per_mtok: Option<f64>,
    /// 命中的源（调试/展示）。
    pub source: Option<&'static str>,
}

impl ModelMeta {
    /// 有效字段数（`is_empty` 判定用；display_name/source 不计）。
    /// 不参与合并排序——合并按源优先级（见 [`merge_metas`]）。
    pub fn completeness(&self) -> usize {
        [
            self.context_tokens.is_some(),
            self.max_output_tokens.is_some(),
            self.supports_vision.is_some(),
            self.supports_reasoning.is_some(),
            self.supports_tool_call.is_some(),
            self.input_price_per_mtok.is_some(),
            self.output_price_per_mtok.is_some(),
        ]
        .into_iter()
        .filter(|b| *b)
        .count()
    }

    pub fn is_empty(&self) -> bool {
        self.completeness() == 0
    }

    /// 用 `other` 填补自己的空字段。
    pub fn fill_from(&mut self, other: &ModelMeta) {
        if self.display_name.is_none() {
            self.display_name = other.display_name.clone();
        }
        if self.context_tokens.is_none() {
            self.context_tokens = other.context_tokens;
        }
        if self.max_output_tokens.is_none() {
            self.max_output_tokens = other.max_output_tokens;
        }
        if self.supports_vision.is_none() {
            self.supports_vision = other.supports_vision;
        }
        if self.supports_reasoning.is_none() {
            self.supports_reasoning = other.supports_reasoning;
        }
        if self.supports_tool_call.is_none() {
            self.supports_tool_call = other.supports_tool_call;
        }
        if self.input_price_per_mtok.is_none() {
            self.input_price_per_mtok = other.input_price_per_mtok;
        }
        if self.output_price_per_mtok.is_none() {
            self.output_price_per_mtok = other.output_price_per_mtok;
        }
        if self.source.is_none() {
            self.source = other.source;
        }
    }
}

/// 源优先级：`SOURCES` 声明序下标（models.dev=0 > LiteLLM=1 >
/// CloudPrice=2）。按 `ModelMeta.source`（源展示名）匹配，**忽略大小写**
/// ——`probe_cloudprice_by_id` 产出的 source 是小写 `"cloudprice"`，与
/// `SourceDef.name` `"CloudPrice"` 大小写不一致。未知 / None 排最后。
fn source_priority(source: Option<&str>) -> usize {
    let Some(name) = source else {
        return usize::MAX;
    };
    SOURCES
        .iter()
        .position(|s| s.name.eq_ignore_ascii_case(name))
        .unwrap_or(usize::MAX)
}

/// 按源优先级（models.dev > LiteLLM > CloudPrice，即 `SOURCES` 声明
/// 序；稳定排序保持到达序）做 per-field fill-None 合并：逐字段取最高
/// 优先源的非空值，高优先源没给的字段才由低优先源补齐。用户拍板
/// （2026-10）：更全的源（如带计价的 LiteLLM）可能带更错的能力布尔，
/// 不能靠完整度赢得覆盖权。
pub fn merge_metas(metas: Vec<ModelMeta>) -> ModelMeta {
    let mut sorted = metas;
    sorted.sort_by_key(|m| source_priority(m.source));
    let mut merged = ModelMeta::default();
    for m in &sorted {
        merged.fill_from(m);
    }
    merged
}

// ── 各格式解析（Value → 索引 map / 条目 → ModelMeta）────────────

/// 该条目是否为可展示的模型条目。LiteLLM 格式混有 `sample_spec`
/// 之类的非模型元数据（有限额或定价键才算模型）；models.dev 的条目
/// 全部是模型。索引（[`parse_source_map`]）与搜索 / 浏览共用本谓词。
pub fn is_model_entry(format: SourceFormat, entry: &Value) -> bool {
    match format {
        SourceFormat::ModelsDev => true,
        SourceFormat::LiteLLM => {
            entry.get("max_input_tokens").is_some()
                || entry.get("input_cost_per_token").is_some()
        }
    }
}

/// 解析为「归一化键 → 原始条目」索引。models.dev：裸尾段优先 +
/// 全键双索引；LiteLLM：只索引真正的模型条目（有 limits 或定价键），
/// 尾段为键（前缀入不了归一化域）。
pub fn parse_source_map(format: SourceFormat, v: &Value) -> anyhow::Result<HashMap<String, Value>> {
    let obj = v
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("registry json 顶层不是对象"))?;
    let mut map = HashMap::with_capacity(obj.len() * 2);
    for (k, entry) in obj {
        match format {
            SourceFormat::ModelsDev => {
                let tail = norm_id(k.rsplit('/').next().unwrap_or(k));
                map.entry(tail).or_insert_with(|| entry.clone());
                let full = norm_id(k);
                map.insert(full, entry.clone());
            }
            SourceFormat::LiteLLM => {
                if !is_model_entry(format, entry) {
                    continue;
                }
                let tail = norm_id(k.rsplit('/').next().unwrap_or(k));
                map.entry(tail).or_insert_with(|| entry.clone());
            }
        }
    }
    Ok(map)
}

/// 在索引 map 中按候选键查找并提取元数据。
pub fn lookup_in_map(
    format: SourceFormat,
    source_name: &'static str,
    map: &HashMap<String, Value>,
    model_id: &str,
) -> Option<ModelMeta> {
    let cands = id_candidates(model_id);
    let entry = cands.iter().find_map(|c| map.get(c))?;
    match format {
        SourceFormat::ModelsDev => extract_models_dev(source_name, entry),
        SourceFormat::LiteLLM => extract_litellm(source_name, entry),
    }
}

/// models.dev 单条目 → 元数据（`pub` 供 mobile 搜索/浏览复用）。
pub fn extract_models_dev(source_name: &'static str, entry: &Value) -> Option<ModelMeta> {
    let g = |k: &str| entry.get("limit").and_then(|l| l.get(k)).and_then(Value::as_u64);
    Some(ModelMeta {
        display_name: entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        context_tokens: g("context"),
        max_output_tokens: g("output"),
        supports_vision: entry
            .get("modalities")
            .and_then(|m| m.get("input"))
            .and_then(Value::as_array)
            .map(|a| a.iter().any(|x| x.as_str() == Some("image"))),
        supports_reasoning: entry.get("reasoning").and_then(Value::as_bool),
        supports_tool_call: entry.get("tool_call").and_then(Value::as_bool),
        input_price_per_mtok: None,
        output_price_per_mtok: None,
        source: Some(source_name),
    })
}

/// LiteLLM 单条目 → 元数据（`pub` 供 mobile 搜索/浏览复用；
/// CloudPrice 全量导出同格式）。
pub fn extract_litellm(source_name: &'static str, entry: &Value) -> Option<ModelMeta> {
    let pt = |k: &str| -> Option<f64> {
        entry
            .get(k)
            .and_then(Value::as_f64)
            .map(|x| x * 1e6)
            .filter(|x| *x > 0.0)
    };
    Some(ModelMeta {
        display_name: None,
        context_tokens: entry.get("max_input_tokens").and_then(Value::as_u64),
        max_output_tokens: entry.get("max_output_tokens").and_then(Value::as_u64),
        supports_vision: entry.get("supports_vision").and_then(Value::as_bool),
        supports_reasoning: entry.get("supports_reasoning").and_then(Value::as_bool),
        supports_tool_call: entry
            .get("supports_function_calling")
            .and_then(Value::as_bool),
        input_price_per_mtok: pt("input_cost_per_token"),
        output_price_per_mtok: pt("output_cost_per_token"),
        source: Some(source_name),
    })
}

// ── HTTP（Android：webpki 预配置 TLS，同 provider.rs）────────────

pub async fn http_get_json(url: &str) -> anyhow::Result<Value> {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("openslate/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(60));
    #[cfg(target_os = "android")]
    {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = rustls::crypto::aws_lc_rs::default_provider();
        let tls = rustls::ClientConfig::builder_with_provider(provider.into())
            .with_safe_default_protocol_versions()
            .map_err(|e| anyhow::anyhow!("tls config: {e}"))?
            .with_root_certificates(roots)
            .with_no_client_auth();
        builder = builder.use_preconfigured_tls(tls);
    }
    match std::env::var("OPENSLATE_HTTP_PROXY")
        .ok()
        .filter(|u| !u.is_empty())
    {
        Some(proxy_url) => {
            let proxy = reqwest::Proxy::all(&proxy_url)
                .map_err(|e| anyhow::anyhow!("invalid proxy url '{proxy_url}': {e}"))?;
            builder = builder.proxy(proxy);
        }
        None => builder = builder.no_proxy(),
    }
    Ok(builder
        .build()?
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// CloudPrice 按 id 单查（本地三源都 miss 时的在线兜底；resolver 支持
/// 模糊名）。10s 超时。
///
/// 只采数值字段（限额 / 计价）。能力布尔（vision / reasoning /
/// tool_call）**不采信**：其聚合数据无法与主力源交叉验证，且有实证
/// 错误案例（2026-10：GLM-5.3 base 被标 image 输入，把 flash 变体的
/// 能力并进了 base；models.dev 与 LiteLLM 多数转售商均为纯文本）。
/// 错误能力开关会被"从 registry 补全"直接写入条目，危害大于留空；
/// 留空时用户可手动设置，或等本地源（1 天 TTL）收录后覆盖。
pub async fn probe_cloudprice_by_id(model_id: &str) -> Option<ModelMeta> {
    let mut out = String::with_capacity(model_id.len());
    for b in model_id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    let url = format!("https://ai.cloudprice.net/api/v1/models/{out}?include=pricing");
    let v = tokio::time::timeout(Duration::from_secs(10), http_get_json(&url))
        .await
        .ok()?
        .ok()?;
    let d = v.get("data")?;
    let pt = |key: &str| -> Option<f64> {
        d.get("pricing")
            .and_then(|p| p.get(key))
            .and_then(Value::as_f64)
            .map(|x| x * 1e6)
            .filter(|x| *x > 0.0)
    };
    Some(ModelMeta {
        display_name: d
            .get("display_name")
            .and_then(Value::as_str)
            .map(str::to_owned),
        context_tokens: d.get("context_window").and_then(Value::as_u64),
        max_output_tokens: d.get("max_output_tokens").and_then(Value::as_u64),
        supports_vision: None,
        supports_reasoning: None,
        supports_tool_call: None,
        input_price_per_mtok: pt("input_cost_per_token"),
        output_price_per_mtok: pt("output_cost_per_token"),
        source: Some("cloudprice"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn norm_and_candidates() {
        assert_eq!(norm_id("glm-4.7"), "glm47");
        assert_eq!(norm_id("GLM_4.7"), "glm47");
        let c = id_candidates("zhipu/glm-4.7@flash");
        assert!(c.contains(&norm_id("glm-4.7")));
        assert!(c.contains(&norm_id("zhipu/glm-4.7")));
        let d = id_candidates("claude-sonnet-4-5-20250929");
        assert!(d.contains(&norm_id("claude-sonnet-4-5")));
    }

    #[test]
    fn merge_prefers_source_priority_and_fills_gaps() {
        // 优先级：models.dev > LiteLLM > CloudPrice（SOURCES 声明序）。
        // litellm 条目更全（带计价 + vision=true + max_output），但
        // models.dev 优先级更高：其非空字段（含 vision=false）胜出；
        // 它没给的字段才由 litellm 补齐——"更全"不再赢得覆盖权（更全
        // 的源可能带更错的能力布尔）。
        let litellm = ModelMeta {
            context_tokens: Some(200000),
            max_output_tokens: Some(131072),
            supports_vision: Some(true),
            supports_reasoning: Some(true),
            input_price_per_mtok: Some(0.6),
            output_price_per_mtok: Some(2.2),
            source: Some("litellm"),
            ..Default::default()
        };
        let models_dev = ModelMeta {
            context_tokens: Some(204800),
            supports_vision: Some(false),
            source: Some("models.dev"),
            ..Default::default()
        };
        // 到达序倒排（litellm 在前）也应得到同一结果：排序按优先级，
        // 不按到达序 / 完整度。
        let merged = merge_metas(vec![litellm.clone(), models_dev.clone()]);
        assert_eq!(
            merged.context_tokens,
            Some(204800),
            "高优先源的值优先（哪怕低优先源更完整）"
        );
        assert_eq!(
            merged.supports_vision, Some(false),
            "models.dev 的 vision=false 胜过更全的 LiteLLM vision=true"
        );
        assert_eq!(
            merged.supports_reasoning,
            Some(true),
            "高优先源缺失的字段由低优先源补齐"
        );
        assert_eq!(merged.max_output_tokens, Some(131072));
        assert_eq!(merged.input_price_per_mtok, Some(0.6), "只有 LiteLLM 给的计价保留");
        assert_eq!(merged.output_price_per_mtok, Some(2.2));
        let merged2 = merge_metas(vec![models_dev, litellm]);
        assert_eq!(merged, merged2, "与到达序无关（稳定排序仅影响同优先级）");
    }

    #[test]
    fn merge_priority_matches_source_names_and_unknowns() {
        // probe_cloudprice_by_id 产出小写 "cloudprice"（与 SourceDef.name
        // "CloudPrice" 大小写不一致）——匹配须忽略大小写，CloudPrice 仍
        // 排在 LiteLLM 之后。
        let cloudprice = ModelMeta {
            context_tokens: Some(1000000),
            max_output_tokens: Some(65536),
            source: Some("cloudprice"),
            ..Default::default()
        };
        let litellm = ModelMeta {
            context_tokens: Some(200000),
            source: Some("LiteLLM"),
            ..Default::default()
        };
        let merged = merge_metas(vec![cloudprice, litellm.clone()]);
        assert_eq!(merged.context_tokens, Some(200000), "LiteLLM（次优先）胜过 CloudPrice");
        assert_eq!(merged.max_output_tokens, Some(65536), "CloudPrice 补 LiteLLM 没给的字段");

        // 未知 / None source 排最后（优先级最低），不抢已知源的值。
        let unknown = ModelMeta {
            context_tokens: Some(999999),
            source: Some("whatever-registry"),
            ..Default::default()
        };
        let no_source = ModelMeta {
            context_tokens: Some(888888),
            ..Default::default()
        };
        let merged = merge_metas(vec![unknown.clone(), no_source.clone(), litellm]);
        assert_eq!(merged.context_tokens, Some(200000), "未知 / None 源不抢已知源的值");
        // 只有未知源时稳定排序保持到达序（先到先用）。
        let merged = merge_metas(vec![no_source, unknown]);
        assert_eq!(merged.context_tokens, Some(888888));
    }

    #[test]
    fn parse_models_dev_and_lookup() {
        let v = json!({
            "zhipuai/glm-4.7": {
                "id": "zhipuai/glm-4.7",
                "name": "GLM-4.7",
                "reasoning": true,
                "tool_call": true,
                "modalities": {"input": ["text"], "output": ["text"]},
                "limit": {"context": 204800, "output": 131072}
            },
            "zhipuai/glm-4.6v": {
                "name": "GLM-4.6V",
                "reasoning": true,
                "tool_call": true,
                "modalities": {"input": ["text", "image"], "output": ["text"]},
                "limit": {"context": 128000, "output": 32768}
            }
        });
        let map = parse_source_map(SourceFormat::ModelsDev, &v).unwrap();
        let m = lookup_in_map(SourceFormat::ModelsDev, "models.dev", &map, "glm-4.7").unwrap();
        assert_eq!(m.context_tokens, Some(204800));
        assert_eq!(m.max_output_tokens, Some(131072));
        assert_eq!(m.supports_reasoning, Some(true));
        assert_eq!(m.supports_vision, Some(false));
        // 前缀查询也能命中（裸尾段索引）。
        let v2 = lookup_in_map(SourceFormat::ModelsDev, "models.dev", &map, "zhipuai/glm-4.6v").unwrap();
        assert_eq!(v2.supports_vision, Some(true), "image 输入 → vision");
        assert_eq!(v2.context_tokens, Some(128000));
    }

    #[test]
    fn parse_litellm_and_pricing() {
        let v = json!({
            "zai/glm-4.7": {
                "max_input_tokens": 200000,
                "max_output_tokens": 128000,
                "supports_reasoning": true,
                "input_cost_per_token": 6e-7,
                "output_cost_per_token": 2.2e-6
            },
            "sample_spec": {"note": "非模型条目应被跳过"}
        });
        let map = parse_source_map(SourceFormat::LiteLLM, &v).unwrap();
        assert_eq!(map.len(), 1, "非模型条目被过滤");
        let m = lookup_in_map(SourceFormat::LiteLLM, "litellm", &map, "glm-4.7").unwrap();
        assert_eq!(m.context_tokens, Some(200000));
        assert_eq!(m.input_price_per_mtok, Some(0.6), "per-token × 1e6");
        assert_eq!(m.output_price_per_mtok, Some(2.2));
        assert_eq!(m.supports_reasoning, Some(true));
    }
}
