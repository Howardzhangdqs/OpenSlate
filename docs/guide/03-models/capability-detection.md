# 03 · 能力检测（auto-detect 远端模型清单）

> 面向接手本项目的 AI agent。Provider 设置页的「自动检测模型清单」：拉取 provider 的
> `/models`（各协议族端点/鉴权不同），返回 id 列表供 UI 勾选建模型条目。
> 检测到 id 之后的能力字段补全走 model registry（见 `model-registry.md`）。

---

## 1. 入口函数

`crates/openslate-core/src/provider.rs:104-208`：

```rust
pub async fn list_remote_models(
    base_url: &str,
    adapter: &str,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<String>
```

- `adapter` 为空按 `"openai"` 处理（L110，与 `ProviderConfig::adapter` 的缺省语义一致）。
- `base_url` 先 `trim_end_matches('/')`（L109）。

## 2. adapter 感知的端点/鉴权矩阵

| adapter | 端点 | 鉴权头 |
|---------|------|--------|
| openai（及一切兼容端点，如 zhipu） | `GET {base}/models`（L154） | `Authorization: Bearer {key}`（L171） |
| anthropic | `GET {base}/v1/models`；base 已带 `/v1` 则不重复拼（L143-147，anthropic 代理常配 `…/v1`，直拼会变 `/v1/v1/models` → 404） | `x-api-key: {key}` + `anthropic-version: 2023-06-01`（L157-161） |
| gemini | `GET {base}/v1beta/models`；base 带 `/v1beta` 先剥（L149-151） | `x-goog-api-key: {key}`（L163-166） |
| ollama | `GET {base}/api/tags`（L153） | 无（L168） |

## 3. HTTP 栈（与 model-genai 同款处理）

- 10s connect + **10s total** 超时（L113-114，`/models` 是小请求，总超时合理——与流式
  模型请求的 idle 语义不同）。
- UA = `openslate/<version>`（L112）。
- `#[cfg(target_os = "android")]`：webpki 静态根证书预配置 rustls（L117-128）——
  `rustls-platform-verifier` 需要 JNI 宿主初始化，纯 .so 场景不可用。
- 代理：`OPENSLATE_HTTP_PROXY` 环境变量显式启用，否则 `no_proxy()`（L131-138）。

## 4. 响应解析（刻意宽松）

响应按 JSON 对象处理，两处逐个探测（L180-198）：

- `data[].id`：OpenAI `{"data":[{"id":...}]}` / Anthropic 同形态；
- `models[].name` 或 `models[].id`：Gemini `{"models":[{"name":"models/xxx"}]}` /
  Ollama `/api/tags` `{"models":[{"name":...}]}`。Gemini 的 `models/` 前缀剥掉（L195）。

排序去重（`ids.sort(); ids.dedup();` L205-206）。**一个都识别不出则报错**（L199-204），
错误文案带 adapter 与响应片段前 200 字符（`serde_json::to_string` 后 `chars().take(200)`），
方便排查网关返回了 HTML/错误页的情况。

`error_for_status()`（L175）：非 2xx 直接报错（reqwest 状态文本 + body）。

## 5. 消费方

- **mobile**：`crates/openslate-mobile/src/runtime.rs::list_provider_models(provider)`（L521）
  ——FFI `list_provider_models(provider)`（`openslate-mobile-ffi/src/lib.rs:198`）→
  返回 `{"ok":true,"models":[...]}`；失败 `{"ok":false,"error":"..."}`（provider 不存在 /
  key 未配置 / 网络 / 协议错误均走此路，UI 直接展示 error 文案）。key 优先取 FFI 注入的
  内存 secrets map，`env(api_key_env)` 兜底；ollama 等本地端点允许无 key。
- **desktop 暂无入口**（cli/tui/server 未接线；要加的话走 core 这个函数，别再复制一份矩阵）。

## 6. mobile 建条目的 entry 键命名（`providerId/modelId`）

- UI 勾选/手动添加入库（SettingsScreen `genModelEntryId`）生成的 `[models.<entry>]`
  键为 **`providerId/modelId`**（两段原样、仅 trim），与 models.dev 注册表键
  （`zhipuai/glm-4.7`）同形。toml 中含 `/` 的键由 toml_edit 自动加引号
  （`[models."zhipuai/glm-4.7"]`），合法且无碍；键冲突时追加 `-` + 4 位 hex 后缀。
- **存量 `provider__model` slug 键**（旧生成器：小写 + 非 `[a-z0-9-]` 折叠 `-`，
  冲突加 hex 后缀）由 mobile 端启动迁移统一改写（RuntimeBridge
  `migrateModelEntrySlash`：精确公式匹配判定自动生成条目 → **先删旧键、
  后 upsert 新键**——避让 core `upsert_model` 的 (provider, model) 全局
  唯一守卫 → levels/capabilities 引用同步改指；发完不置标记，config 更新
  事件路径上逐条核对（旧键消失 && 新键存在 && 引用已改指）**校验通过才写**
  SharedPreferences 标记 `migrated_model_entry_slash_v2`（v1 被修复前旧代码
  未校验就置位、不可信，已弃用），不通过则下次启动
  幂等重试）。手写 entry 与 levels KEY 不动。
- Rust 全链路（resolve_model/persist/registry/session）把 entry 当**不透明串**，
  键格式仅是 mobile 侧约定；UI 显示层经 `RuntimeBridge.modelAliasDisplay` 把
  条目键换「Provider显示名/modelId」，entry 裸串不直出。

## 7. 检测之后：能力字段哪里来

`list_remote_models` 只给 id。`[models.X]` 条目的 `supports_tool_call / supports_vision /
supports_reasoning` 及限额 / 计价来自：手动配置 > mobile 端「从 registry 补全」
（`lookup_with_online_fallback`，见 `model-registry.md` §7）。

## 8. 改动检查清单

- 新增 adapter 端点形态 → 矩阵 + 解析都要动；错误文案保持带响应片段的习惯。
- 动 TLS / 代理 → 与 model-genai、model_registry 三处同款，一起改。
- mobile 侧行为变化 → FFI 返回 JSON 的 `ok`/`error` 契约别破坏（Kotlin 侧直接展示）。
