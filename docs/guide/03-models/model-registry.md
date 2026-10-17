# 03 · 模型元数据注册表（Model Registry）

> 面向接手本项目的 AI agent。**问题**：Provider 的 `/models` 端点只返回 id 清单，
> 不含 context window / 限额 / vision / reasoning / 计价。这些数据从三个公开数据源
> 拉取合并（背景与大陆可达性实测见根目录 `AGENTS.md`「模型元数据数据源」节）。
> 本文讲定义、解析、匹配、合并、缓存、更新、兜底的全套规则。

---

## 1. 职责切分（改代码先看这个）

| 模块 | 职责 |
|------|------|
| `crates/openslate-core/src/model_registry.rs`（573 行） | **纯逻辑，无 IO 状态**：数据源定义（`SOURCES`，L56）、两种格式解析（`SourceFormat::{ModelsDev, LiteLLM}`，L30）、id 归一化与匹配（`norm_id` L92 / `id_candidates` L102）、按源优先级合并（`merge_metas` L218）、CloudPrice 在线单查（`probe_cloudprice_by_id` L383）、`http_get_json`（L336，Android TLS + 代理） |
| `crates/openslate-mobile/src/registry.rs`（541 行） | **存储与更新**：`RegistryStore`（L46）——本地 ZSTD 持久化、手动/自动更新、三源合并查询、搜索/浏览 |
| 设置页「模型数据源」（mobile，内嵌子页） | UI：provider 聚合浏览/跨源搜索 + 条目详情弹层；每源独立「更新」按钮在页顶「数据源管理」区（常开直接可见） |

## 2. 数据源（`SOURCES` 常量，L56-83；声明序 = 优先级序）

| 序 | id | 源 | URL | 格式 | stale_after | 备注 |
|----|----|----|-----|------|-------------|------|
| 0 | `models_dev` | models.dev | `https://models.dev/models.json` | ModelsDev | 24h | 体积小 ~400KB，一天一更；键规范 `provider/model` |
| 1 | `litellm` | LiteLLM | `https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json` | LiteLLM | 7d | 全量 ~3MB；计价最细（per-token） |
| 2 | `cloudprice` | CloudPrice | `https://ai.cloudprice.net/api/v1/litellm_model_prices.json` | LiteLLM | 7d | LiteLLM 兼容导出；响应偏慢（1-2s+） |

```rust
pub struct SourceDef {           // L41-53
    pub id: &'static str,        // 稳定 id（存储文件名 / FFI 参数）
    pub name: &'static str,      // 展示名
    pub desc: &'static str,      // 数据源页面一句话说明
    pub url: &'static str,       // 全量拉取 URL
    pub format: SourceFormat,
    pub stale_after: Duration,   // 过期阈值：启动时自动刷新判据
}
pub fn source_by_id(id: &str) -> Option<&'static SourceDef>;  // L85
```

~~ModelRadar.cn~~ 已剔除（可信度存疑，用户拍板）。可达性注意：开发机可能挂代理，
本机 curl 实测不能作为大陆可达性判据；以真机全国多节点实测为准（2026-10，
用户验证 models.dev 全国可用）。

## 3. id 归一化与匹配

### 3.1 `norm_id`（L92-97）

小写、只留 ASCII 字母数字：`glm-4.7` / `GLM_4.7` → `glm47`。

### 3.2 `id_candidates(model_id) -> Vec<String>`（L102-125）

生成候选序列（全部归一化后逐个探测索引）：

1. 原始 id 归一化；
2. 去 `@变体` 后缀（`glm-4.7@flash` → `glm-4.7`）；
3. 取最后一个 `/` 后的尾段（去 provider 前缀，`zhipu/glm-4.7` → `glm-4.7`）；
4. 尾段再去尾部日期快照：`-20251222`（8 位）或 `-251222`（6 位）纯数字段
   （`claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`）。

去重保序（`!out.contains(&n)`）。

### 3.3 `parse_source_map(format, v) -> HashMap<String, Value>`（L246-269）

解析为「归一化键 → 原始条目」索引：

- **ModelsDev**：双索引——裸尾段（`norm_id(k.rsplit('/'))`，`entry(tail).or_insert` 即
  **尾段 first-wins**：同尾段先声明的 provider 胜）+ 全键（`insert` 覆盖式）。
- **LiteLLM**：只索引真正的模型条目（`is_model_entry` 过滤），尾段为键。

`is_model_entry(format, entry)`（L233-241）：ModelsDev 全 true；LiteLLM 要求
`max_input_tokens` 或 `input_cost_per_token` 存在（`sample_spec` 之类被过滤）。
索引与搜索/浏览共用本谓词。

## 4. 归一化元数据 `ModelMeta` 与合并

```rust
// L130-143：单源返回的归一化元数据，全 Option（源没给的留空交给合并）
pub struct ModelMeta {
    pub display_name: Option<String>,
    pub context_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_vision: Option<bool>,
    pub supports_reasoning: Option<bool>,
    pub supports_tool_call: Option<bool>,
    pub input_price_per_mtok: Option<f64>,   // USD / 百万 token
    pub output_price_per_mtok: Option<f64>,
    pub source: Option<&'static str>,        // 命中的源（调试/展示）
}
```

`completeness()`（L148）数 7 个有效字段（display_name/source 不计）；
`is_empty()` = completeness == 0；`fill_from(&other)`（L168）逐字段填自己的 None。

### 4.1 合并策略（用户拍板 2026-10，**不要改回完整度优先**）

`merge_metas(metas: Vec<ModelMeta>) -> ModelMeta`（L218-226）：

1. 按 `source_priority(m.source)`（L203-211：`SOURCES` 声明序下标；按**展示名忽略大小写**
   匹配——`probe_cloudprice_by_id` 产出小写 `"cloudprice"`，与 `SourceDef.name`
   `"CloudPrice"` 不一致；未知 / None 源 `usize::MAX` 排最后）**稳定**排序；
2. 从空 `ModelMeta` 开始逐个 `fill_from`——逐字段 **fill-None**：高优先源的非空值胜出
   （**哪怕 `vision=false` 胜过更全源的 `vision=true`**），它没给的字段才由低优先源补齐。

**理由**：更全的源（如带计价的 LiteLLM）可能带更错的能力布尔，「完整度」不能赢得覆盖权。

计价统一为 **USD / 百万 token**：LiteLLM per-token 值 × 1e6
（`extract_litellm` 的 `pt` 闭包，L312-318，且 `.filter(|x| *x > 0.0)` 丢掉 0 值）。

钉死语义的单测：`merge_prefers_source_priority_and_fills_gaps`（L439，含「到达序倒排
结果不变」断言）、`merge_priority_matches_source_names_and_unknowns`（L486，大小写不敏感
+ 未知源排序 + 只有未知源时稳定保持到达序）。

### 4.2 单条目提取

- `extract_models_dev(source_name, entry)`（L287-307）：`limit.context` /
  `limit.output` 取限额；`modalities.input` 数组含 `"image"` → `supports_vision = Some(true)`
  （只有 `["text"]` → `Some(false)`；无 modalities → None）；`reasoning` / `tool_call`
  布尔直读；**无计价字段**（恒 None）。
- `extract_litellm(source_name, entry)`（L311-332，CloudPrice 全量导出同格式）：
  `max_input_tokens` / `max_output_tokens`；`supports_vision` / `supports_reasoning` /
  `supports_function_calling`（注意：tool call 的键名与 models.dev 不同）；
  `input_cost_per_token` / `output_cost_per_token` × 1e6；**无 display_name**。

### 4.3 `lookup_in_map(format, source_name, map, model_id)`（L272-284）

`id_candidates` 逐候选 `map.get(c)`，第一个命中即提取。

## 5. CloudPrice 在线兜底（能力布尔不采信）

`probe_cloudprice_by_id(model_id) -> Option<ModelMeta>`（L383-420）：

- 触发条件：本地三源全 miss（`lookup_local().is_empty()`）。
- 请求：`GET https://ai.cloudprice.net/api/v1/models/{percent-encoded-id}?include=pricing`，
  10s 超时（`tokio::time::timeout` 包 `http_get_json`）。id 做手工 percent-encode
  （保留 `A-Za-z0-9-_.~`，其余 `%XX`）。resolver 支持 canonical/LiteLLM/模糊名。
- **只采数值字段**：`data.context_window` / `data.max_output_tokens` /
  `pricing.input_cost_per_token` / `pricing.output_cost_per_token`（× 1e6，>0 过滤）、
  `data.display_name`。
- **能力布尔一律留 `None`**（`supports_vision/reasoning/tool_call` 硬编码 None，L413-415）。
  理由：聚合数据无交叉验证，实证案例 2026-10 GLM-5.3 base 被标 image 输入（flash 变体的
  能力被并进 base），models.dev 与 LiteLLM 多数源均为纯文本。**错误能力开关会被
  「从 registry 补全」直接写入条目，危害大于留空**；留空时用户可手动设置，
  或等本地源（1 天 TTL）收录后覆盖。

`http_get_json(url)`（L336-372）：8s connect + 60s total 超时；
`#[cfg(target_os = "android")]` webpki 静态根证书 TLS（与 model-genai 同款）；
`OPENSLATE_HTTP_PROXY` 显式代理否则 `no_proxy()`；UA = `openslate/<version>`。

## 6. 本地存储与更新（openslate-mobile/src/registry.rs）

`RegistryStore::new(config_dir)`（L70）管理 `{config_dir}/registry/`：

- `<id>.json.zst`：每源一个 ZSTD 压缩 JSON（`zstd_level() = 3`，L65-67）。
  数据常驻手机，查询零网络。
- `meta.json`：`MetaFile { v: 1, sources: {id → SourceStatus} }`（L36-41）。
  `SourceStatus`（L27-34）：`fetched_at_ms` / `entries` / `raw_bytes` / `zst_bytes`
  全 Option。tmp+rename 原子写（`write_meta`，L96-104）；**损坏按空处理**（`read_meta`
  L86-94，alog 一条日志不 panic）。`v` 字段是预留的迁移版本号。

### 6.1 关键方法

| 方法 | 行号 | 语义 |
|------|------|------|
| `update_source(id) -> Result<usize>` | L131 | 拉取 → `parse_source_map` 计数 → ZSTD 压缩 → tmp+rename 原子写 `<id>.json.zst` → 更新 meta → **失效两层内存缓存**（parsed + raw）。返回条目数。数据源子页「数据源管理」小节「更新」按钮的后端 |
| `auto_update_if_stale()` | L176 | 启动时对缺失 / 超过 `stale_after` 的源**串行**刷新（for 循环逐个 await，避免移动网络带宽尖峰）；失败静默（alog，下次启动再试），本地旧数据继续用 |
| `lookup_local(model_id) -> ModelMeta` | L240 | 按 `SOURCES` 序遍历本地索引（`parsed_map` 懒加载磁盘 zst），非空命中收进 vec，`merge_metas` 合并；全 miss → 空 `ModelMeta` |
| `lookup_with_online_fallback(model_id)` | L256 | `lookup_local` 非空即返回；空 → `probe_cloudprice_by_id` 兜底，仍 None → default |
| `sources_json()` | L107 | 页面渲染：每源 id/name/desc/url + meta 状态 + `stale_after_hrs` |
| `search_json(source_id, query, limit)` | L281 | 见 §6.3 |

### 6.2 内存缓存两层（都是 `Mutex<HashMap>`，更新后一并失效）

- `parsed`（L48）：归一化索引 map（查询用，`parsed_map` L198 构建：磁盘 ZSTD → JSON →
  `parse_source_map`，成功后常驻；3MB JSON → 索引后构建期 ~十几 MB 峰值，移动端可承受，
  构建一次终身复用）。
- `raw`（L55）：解码后的**原始 JSON**（`Arc<Value>`，`raw_value` L220 懒加载——搜索/浏览
  要原始键含 provider 前缀与天然键序，索引 map 已折叠丢失。Arc 共享，查询方零整树克隆）。

### 6.3 `search_json`（数据源子页「provider 聚合浏览 / 跨源搜索」）

纯本地（磁盘 ZSTD + 内存 raw 缓存）、同步执行。返回
`{"total":N,"results":[{source,sourceId,id,ctx,out,vision,reasoning,tool,priceIn,priceOut}]}`。

- `source_id` 空 = 跨全部**本地已缓存**源搜索（无 zst 数据的源跳过，不误报）；
  非空 = 只搜该源；未知 id → `{"total":0,"results":[]}`（契约）。
- `query` 空（或归一化后为空，如纯符号）= **浏览模式**：按 JSON 键天然顺序
  （serde_json 默认 BTreeMap 字典序，确定性）取前 `limit` 条；非空 = 双侧归一化子串匹配
  `norm_id(原始键).contains(&norm_id(query))`（键含 provider 前缀，尾段子串天然覆盖）。
- `limit <= 0` 按 50；`total` = 匹配总数，`results` 截断到 `limit`（mobile 数据源子页
  不用分页：浏览与搜索都传大 limit（100_000）一次拉全量，客户端按 provider 前缀分组）。
- `id` = 源 JSON 的**原始键**（保留 provider 前缀）；数值 / 能力 / 计价字段源没给就为
  null（不硬造）。LiteLLM 非模型条目按 `is_model_entry` 跳过。
- 测试：`search_browse_mode_and_field_mapping` / `search_limit_truncation_keeps_total` /
  `search_query_matches_and_source_filter` / `search_unknown_or_uncached_source`（L455-540）。

## 7. FFI 面（Kotlin 宿主入口）

`crates/openslate-mobile-ffi/src/lib.rs`（JSON 字符串返回）+ 桥接
`crates/openslate-mobile/src/runtime.rs`：

| FFI | runtime 桥接 | 说明 |
|-----|--------------|------|
| `registry_sources()` | `registry_sources_json`（runtime.rs:439） | 源列表 + 状态 |
| `registry_update_source(id)` | `registry_update_source_json`（L445） | 手动更新 |
| `registry_schedule_auto_update()` | `registry_schedule_auto_update`（L468） | 启动时后台调 `auto_update_if_stale` |
| `registry_search(source_id, query, limit)` | `registry_search_json`（L487） | 搜索/浏览 |
| 元数据补全查询 | `lookup_with_online_fallback`（L509） | 外包 10s 超时 + `meta_to_json` |

## 8. 改动检查清单

- 动了数据源 / 合并策略 / 兜底规则 → 同步更新 `AGENTS.md`「模型元数据数据源」节 + 本文档；
  `model_registry.rs` 的单测是合并语义的钉子，**先改测试预期再改实现**。
- 动了 registry 存储 schema → `MetaFile.v` 字段是预留的迁移版本号。
- 动了 HTTP 栈（TLS/代理/UA）→ 与 `model-genai`、core `provider.rs::list_remote_models`
  三处是同款处理，一起改。
