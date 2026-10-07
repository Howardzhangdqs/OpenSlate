//! mobile secret provider（PLAN §18 第一阶段形态）。
//!
//! Android 上 API key 由宿主持有：Keystore(AES-GCM) 持久化 → 启动时经
//! FFI `set_api_key(provider, value)` 注入内存 map。Rust 侧不碰 .env /
//! config（桌面 `apply_set_api_key` 写 .env 的路径不适用于 mobile，
//! PLAN §18：secret 绝不落明文配置）。
//!
//! 查找顺序：内存 map（provider 名精确匹配）→ 环境变量（宿主进程注入
//! 或测试场景兜底）→ 报错。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use openslate_app::provider::build_genai_provider;
use openslate_core::config::OpenSlateConfig;
use openslate_core::provider::ModelProvider;

/// provider 名 → API key（内存；进程重启由宿主重新注入）。
#[derive(Default)]
pub struct MobileSecrets {
    keys: RwLock<HashMap<String, String>>,
}

impl MobileSecrets {
    pub fn set(&self, provider: &str, value: &str) {
        self.keys
            .write()
            .expect("mobile secrets lock poisoned")
            .insert(provider.to_owned(), value.to_owned());
        tracing::info!(target: "openslate_mobile", "api key injected for provider '{provider}'");
    }

    /// 取 key（runtime 的模型检测等同步路径复用；null = 未注入）。
    pub fn get(&self, provider: &str) -> Option<String> {
        self.keys
            .read()
            .expect("mobile secrets lock poisoned")
            .get(provider)
            .cloned()
    }
}

/// 构造 mobile 的 per-turn provider factory：注入 key 优先，env 兜底。
pub fn mobile_provider_factory(
    secrets: Arc<MobileSecrets>,
) -> openslate_session::state::ProviderFactory {
    Arc::new(move |config: &OpenSlateConfig, alias: &str| {
        let provider_name = openslate_core::model_config::resolve_model(config, alias)
            .map(|r| r.provider_name.clone())
            .or_else(|_| {
                // 别名解析失败也要尽量报出缺 key 的 provider 名——
                // 常见场景：宿主先 set_api_key 再配置模型。
                config
                    .models
                    .get(alias)
                    .map(|m| m.provider.clone())
                    .ok_or_else(|| anyhow::anyhow!("未知模型别名 '{alias}'"))
            })?;
        let key = secrets
            .get(&provider_name)
            .or_else(|| {
                config
                    .providers
                    .get(&provider_name)
                    .and_then(|p| std::env::var(&p.api_key_env).ok())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "provider '{provider_name}' 的 API key 未配置（宿主请先 set_api_key）"
                )
            })?;
        build_genai_provider(config, alias, Some(key))
            .map(|p| p as Box<dyn ModelProvider>)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_set_get_roundtrip() {
        let s = MobileSecrets::default();
        assert!(s.get("zhipu").is_none());
        s.set("zhipu", "sk-test");
        assert_eq!(s.get("zhipu").as_deref(), Some("sk-test"));
    }
}
