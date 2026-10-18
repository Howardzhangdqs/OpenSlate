//! ask_user secret 子系统（持久化凭据库 + env 注入源 + file 写入 + 脱敏值源）。
//!
//! 设计真相源：`docs/proposals/ask-user-tool-design.md` §0（r3 拍板）与
//! `docs/guide/04-tools/ask-user.md` §8。上一 lane 的内存版接缝
//! （[`crate::askuser::AskUserSecretVault`]）在此替换为
//! [`PersistentSecretVault`]：
//!
//! - **会话级凭据**（`remember=false`）：仅内存（进程存活期），语义与
//!   上一 lane 的 `InMemorySecretVault` 完全一致（requestId 域、同名
//!   最新写入优先）；
//! - **持久凭据**（`remember=true`）：store v4 `credentials` 表 + 内存
//!   镜像（启动时加载，此后读路径全走内存——env 注入是同步闭包，不能
//!   每次命令 spawn 都打库）；同名再写覆盖；
//! - **env 注入**：[`Self::env_pairs`] 供 shell/bash 执行环境取最新值
//!   （mobile `NativeShellTool` 直接消费；mcp-builtin 经进程级
//!   `set_shell_env_source` 消费）；
//! - **file 写入**：`file:<path>` 键在 store 时同步写出文件（0600），
//!   回填模型仍只给路径引用；
//! - **脱敏值源**：[`Self::known_secret_values`] 供 core 的
//!   `set_secret_masker` 闭包做子串打码。
//!
//! ## 加密边界（已拍板，见 store `DDL_CREDENTIALS` 注释）
//!
//! 明文存 app 私有 sqlite（Android 沙箱保护）；不做 Keystore 级加密。
//! 防意外泄漏（回显/日志/转写），不防 root 与备份提取，不防模型诱导
//! shell 间接拼接输出。升级路径：Android Keystore via FFI 包裹对称密钥，
//! 明文只在 vault 读写两侧出现（`Debug`/`Display` 均不输出值）。
//!
//! ## 路径边界（file 形态）
//!
//! 不对写入路径做额外限制：路径来自模型声明、提交前已在 UI 完整展示
//! （secret 题型的 storage 行），用户确认即授权。`~` 前缀展开到进程
//! HOME（Android app 私有目录）；父目录按需创建；权限恒 0600。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use openslate_store_sqlite::query::CredentialRecord;
use openslate_store_sqlite::store::SqliteStore;

use crate::askuser::AskUserSecretVault;

/// 凭据键 → 落地形态拆解。env：`GITHUB_TOKEN`；file：`file:/path`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialKind {
    Env { name: String },
    File { path: String },
}

impl CredentialKind {
    /// 从 vault 键拆解（`file:` 前缀 → 文件形态；其余视为 env 变量名）。
    pub fn from_key(key: &str) -> Self {
        match key.strip_prefix("file:") {
            Some(path) => Self::File {
                path: path.to_owned(),
            },
            None => Self::Env {
                name: key.to_owned(),
            },
        }
    }

    fn kind_str(&self) -> &'static str {
        match self {
            Self::Env { .. } => "env",
            Self::File { .. } => "file",
        }
    }

    fn target(&self) -> &str {
        match self {
            Self::Env { name } => name,
            Self::File { path } => path,
        }
    }
}

/// 会话/持久共用的单条凭据（明文仅在 vault 内部流转，绝不 derive Debug）。
struct StoredSecret {
    /// 会话条目的 requestId 域键；持久镜像行为 `None`。
    request_id: Option<String>,
    kind: CredentialKind,
    value: String,
    /// 会话条目携带（持久镜像行恒 true——行存在即被记住）。
    remember: bool,
}

/// ask_user 凭据库（会话内存 + 持久 sqlite 镜像双层）。
///
/// 构造于 `MobileRuntime::create_with` 装配期（store 尚不可用 → 持久层
/// 惰性），装配完成后 [`Self::attach_store`] 注入 store 并加载镜像。
pub struct PersistentSecretVault {
    /// 会话级条目（requestId 域语义与 `InMemorySecretVault` 一致：
    /// `(request_id, name)` 唯一，后写覆盖）。
    session: Mutex<Vec<StoredSecret>>,
    /// 持久行镜像（name → 行）。读路径全内存；写路径同步落库 + 镜像。
    persistent: RwLock<HashMap<String, StoredSecret>>,
    /// 持久层（None = 未接线：remember 条目暂按会话级暂存）。
    store: RwLock<Option<SqliteStore>>,
}

/// 手写 Debug：只输出计数，绝不泄露明文/键名。
impl std::fmt::Debug for PersistentSecretVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PersistentSecretVault(session={}, persistent={})",
            self.session.lock().map(|s| s.len()).unwrap_or(0),
            self.persistent.read().map(|p| p.len()).unwrap_or(0),
        )
    }
}

impl Default for PersistentSecretVault {
    fn default() -> Self {
        Self::new()
    }
}

/// 现在时间（毫秒；store 列用）。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl PersistentSecretVault {
    pub fn new() -> Self {
        Self {
            session: Mutex::new(Vec::new()),
            persistent: RwLock::new(HashMap::new()),
            store: RwLock::new(None),
        }
    }

    /// 装配后接线：注入 store 并加载持久镜像（幂等：重复调用仅刷新
    /// 镜像——以库内行为准，覆盖启动早于接线的暂存差异）。
    pub async fn attach_store(&self, store: SqliteStore) {
        match store.load_credentials_with_values().await {
            Ok(rows) => {
                let mut mirror = self
                    .persistent
                    .write()
                    .unwrap_or_else(|e| e.into_inner());
                mirror.clear();
                for row in rows {
                    let key = row.name;
                    let kind = CredentialKind::from_key(&key);
                    mirror.insert(
                        key,
                        StoredSecret {
                            request_id: None,
                            kind,
                            value: row.value,
                            remember: true,
                        },
                    );
                }
                drop(mirror);
                *self.store.write().unwrap_or_else(|e| e.into_inner()) = Some(store);
            }
            Err(e) => {
                // 镜像加载失败不阻断启动：vault 退化为会话级（注入/脱敏
                // 仍工作于本会话新存凭据），仅持久层不可用。
                crate::alog!("ask_user vault: load credentials failed (non-fatal): {e}");
                *self.store.write().unwrap_or_else(|e| e.into_inner()) = Some(store);
            }
        }
    }

    /// 持久凭据清单（设置页管理面；无明文）。
    pub fn list_persistent(&self) -> Vec<CredentialRecord> {
        let mirror = self.persistent.read().unwrap_or_else(|e| e.into_inner());
        let mut records: Vec<CredentialRecord> = mirror
            .values()
            .map(|entry| CredentialRecord {
                name: key_of(entry),
                kind: entry.kind.kind_str().to_owned(),
                target: entry.kind.target().to_owned(),
                created_at: 0,
                updated_at: 0,
            })
            .collect();
        records.sort_by(|a, b| a.name.cmp(&b.name));
        records
    }

    /// 删除一条持久凭据（设置页）：库行 + 镜像 + 同名会话条目；file
    /// 形态顺带 best-effort 删除已写出的文件（该文件由本系统写入；
    /// 删除失败仅告警——用户可手动清理）。
    pub async fn delete_persistent(&self, name: &str) -> bool {
        // 1) 库行（幂等；无 store 视为失败——持久层不可用）。
        let deleted = match self.store.read().unwrap_or_else(|e| e.into_inner()).clone() {
            Some(store) => matches!(store.delete_credential(name).await, Ok(true)),
            None => false,
        };
        if !deleted {
            return false;
        }
        // 2) 镜像 + 会话条目。
        let removed = self
            .persistent
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|e| key_of(e) != name);
        // 3) file 形态：best-effort 删文件。
        if let Some(entry) = removed {
            if let CredentialKind::File { path } = entry.kind {
                let expanded = expand_home(&path);
                if let Err(e) = std::fs::remove_file(&expanded) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        crate::alog!("ask_user vault: remove secret file failed (non-fatal): {e}");
                    }
                }
            }
        }
        true
    }

    /// 已知密钥值集合（env + file 全部；脱敏值源）。去重，含会话与持久。
    pub fn known_secret_values(&self) -> Vec<String> {
        let mut values: Vec<String> = Vec::new();
        {
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            for entry in session.iter() {
                if !values.contains(&entry.value) {
                    values.push(entry.value.clone());
                }
            }
        }
        {
            let mirror = self.persistent.read().unwrap_or_else(|e| e.into_inner());
            for entry in mirror.values() {
                if !values.contains(&entry.value) {
                    values.push(entry.value.clone());
                }
            }
        }
        values
    }
}

/// 镜像行的键（`StoredSecret.name` 在镜像行被置空——键即 HashMap key，
/// 从 kind 派生，避免存两份）。
fn key_of(entry: &StoredSecret) -> String {
    match &entry.kind {
        CredentialKind::Env { name } => name.clone(),
        CredentialKind::File { path } => format!("file:{path}"),
    }
}

/// `~` 前缀展开到 HOME（无 HOME 时原样返回——写入失败会以告警暴露）。
fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// 写出 secret 文件（0600；父目录按需创建；已存在则覆盖内容并收敛权限
/// ——`OpenOptions::mode` 只在新建时生效，umask 亦会削位，故显式
/// `set_permissions` 兜底）。
fn write_secret_file(path: &str, value: &str) -> std::io::Result<()> {
    let expanded = expand_home(path);
    if let Some(parent) = expanded.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&expanded)?;
        // 新建走 mode(0o600)（受 umask 影响），覆盖已存在文件则完全
        // 不受它管——统一显式收敛到 0600。
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(value.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&expanded, value)
    }
}

/// 对文本做已知密钥值子串打码（`***`）。
///
/// 仅替换长度 ≥ 4 的值：过短值（如单字符口令）的子串替换会把正常文本
/// 大面积破坏，而如此短的密钥本身也几乎不可能作为凭据被「意外回显」
/// 定位——工程取舍，注释在此。
pub fn mask_with_known_values(known_values: &[String], text: &str) -> String {
    let mut masked = text.to_owned();
    for value in known_values {
        if value.chars().count() >= 4 && text.contains(value.as_str()) {
            masked = masked.replace(value.as_str(), "***");
        }
    }
    masked
}

/// 环境注入源闭包（spawn 时取最新值；`NativeShellTool` /
/// mcp-builtin `set_shell_env_source` 共用）。
pub fn env_source(vault: Arc<PersistentSecretVault>) -> crate::exec::EnvSource {
    Arc::new(move || vault.env_pairs())
}

#[async_trait::async_trait]
impl AskUserSecretVault for PersistentSecretVault {
    async fn store(&self, request_id: &str, name: &str, value: &str, remember: bool) {
        let kind = CredentialKind::from_key(name);

        // 1) 会话条目（requestId 域；后写覆盖）。
        {
            let mut session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            session.retain(|e| !(key_of(e) == name && e.request_id.as_deref() == Some(request_id)));
            session.push(StoredSecret {
                request_id: Some(request_id.to_owned()),
                kind: kind.clone(),
                value: value.to_owned(),
                remember,
            });
        }

        // 2) file 形态：无论 remember 都写文件（落地形态是工具契约，
        //    remember 只决定凭据库持久化）。失败不阻断回填，仅告警。
        if let CredentialKind::File { path } = &kind {
            if let Err(e) = write_secret_file(path, value) {
                crate::alog!("ask_user vault: write secret file failed (non-fatal): {e}");
            }
        }

        // 3) remember → 持久化（库行 + 镜像）。失败仅告警：会话内仍可用。
        //    先 clone store（guard 不跨 await——async trait 需要 Send future）。
        if remember {
            let store_opt = self
                .store
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let persisted = match store_opt {
                Some(store) => store
                    .upsert_credential(name, kind.kind_str(), kind.target(), value, now_ms())
                    .await
                    .is_ok(),
                None => false,
            };
            if persisted {
                self.persistent
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(
                        name.to_owned(),
                        StoredSecret {
                            request_id: None,
                            kind,
                            value: value.to_owned(),
                            remember: true,
                        },
                    );
            } else if self.store.read().map(|s| s.is_some()).unwrap_or(false) {
                crate::alog!("ask_user vault: persist credential '{name}' failed (non-fatal)");
            }
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        // 会话最新优先（本会话后写覆盖持久值），否则持久镜像。
        {
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = session.iter().rev().find(|e| key_of(e) == name) {
                return Some(entry.value.clone());
            }
        }
        self.persistent
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .map(|e| e.value.clone())
    }

    fn is_remembered(&self, name: &str) -> bool {
        {
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(entry) = session.iter().rev().find(|e| key_of(e) == name) {
                return entry.remember;
            }
        }
        self.persistent
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(name)
    }

    fn len(&self) -> usize {
        let session = self.session.lock().unwrap_or_else(|e| e.into_inner()).len();
        let persistent = self
            .persistent
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len();
        session + persistent
    }

    fn env_pairs(&self) -> Vec<(String, String)> {
        // 持久镜像打底，会话条目按写入顺序覆盖（最新胜出）；仅 env 形态。
        let mut pairs: HashMap<String, String> = HashMap::new();
        {
            let mirror = self.persistent.read().unwrap_or_else(|e| e.into_inner());
            for entry in mirror.values() {
                if let CredentialKind::Env { name } = &entry.kind {
                    pairs.insert(name.clone(), entry.value.clone());
                }
            }
        }
        {
            let session = self.session.lock().unwrap_or_else(|e| e.into_inner());
            for entry in session.iter() {
                if let CredentialKind::Env { name } = &entry.kind {
                    pairs.insert(name.clone(), entry.value.clone());
                }
            }
        }
        pairs.into_iter().collect()
    }

    fn known_secret_values(&self) -> Vec<String> {
        // 动态分发覆盖（trait 默认实现为空）：与 inherent 方法同实现。
        PersistentSecretVault::known_secret_values(self)
    }
}

// ── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_store() -> SqliteStore {
        let store = SqliteStore::new_in_memory().await.unwrap();
        store.run_migrations().await.unwrap();
        store
    }

    #[tokio::test]
    async fn session_vs_persistent_scoping() {
        let store = mem_store().await;
        let vault = PersistentSecretVault::new();
        vault.attach_store(store).await;

        // 会话级（remember=false）：可用、不落库。
        vault.store("1", "A_TOKEN", "v1", false).await;
        assert_eq!(vault.lookup("A_TOKEN").as_deref(), Some("v1"));
        assert!(!vault.is_remembered("A_TOKEN"));
        assert_eq!(vault.list_persistent().len(), 0);

        // 持久级（remember=true）：落库 + 镜像。
        vault.store("2", "A_TOKEN", "v2", true).await;
        assert!(vault.is_remembered("A_TOKEN"), "最新写入 remember=true");
        assert_eq!(vault.lookup("A_TOKEN").as_deref(), Some("v2"));
        assert_eq!(vault.list_persistent().len(), 1);

        // 会话写覆盖持久值（lookup 最新优先），持久行不受影响。
        vault.store("3", "A_TOKEN", "v3", false).await;
        assert_eq!(vault.lookup("A_TOKEN").as_deref(), Some("v3"));
        assert!(!vault.is_remembered("A_TOKEN"), "会话覆盖不继承 remember");
    }

    #[tokio::test]
    async fn persistence_survives_new_vault_instance() {
        let store = mem_store().await;
        let vault = PersistentSecretVault::new();
        vault.attach_store(store.clone()).await;
        vault
            .store("1", "GITHUB_TOKEN", "sk-remembered", true)
            .await;

        // 新实例（模拟重启）从库加载镜像。
        let revived = PersistentSecretVault::new();
        revived.attach_store(store).await;
        assert_eq!(
            revived.lookup("GITHUB_TOKEN").as_deref(),
            Some("sk-remembered")
        );
        assert!(revived.is_remembered("GITHUB_TOKEN"));
        // env 注入含持久值。
        let pairs = revived.env_pairs();
        assert!(pairs.contains(&("GITHUB_TOKEN".to_owned(), "sk-remembered".to_owned())));
    }

    #[tokio::test]
    async fn env_pairs_merge_and_override() {
        let store = mem_store().await;
        let vault = PersistentSecretVault::new();
        vault.attach_store(store).await;
        vault.store("1", "PERSIST_TOKEN", "pv", true).await;
        vault.store("2", "SESSION_TOKEN", "sv", false).await;
        // file 键不进 env。
        vault
            .store("3", "file:/tmp/openslate-test-netrc", "fv", true)
            .await;

        let pairs = vault.env_pairs();
        assert_eq!(pairs.len(), 2, "file 形态不入 env: {pairs:?}");
        assert!(pairs.contains(&("PERSIST_TOKEN".to_owned(), "pv".to_owned())));
        assert!(pairs.contains(&("SESSION_TOKEN".to_owned(), "sv".to_owned())));
    }

    #[tokio::test]
    async fn file_write_permissions_and_content() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("creds").join("netrc");
            let path_str = path.display().to_string();
            write_secret_file(&path_str, "machine github.com login u password p\n").unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!(
                meta.permissions().mode() & 0o777,
                0o600,
                "secret 文件必须 0600"
            );
            assert!(std::fs::read_to_string(&path).unwrap().contains("password p"));

            // 覆盖已存在（0777 的宽松文件）也收敛回 0600。
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
            write_secret_file(&path_str, "rotated").unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "rotated");
        }
    }

    #[tokio::test]
    async fn delete_persistent_removes_row_mirror_and_file() {
        #[cfg(unix)]
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let file_path = dir.path().join("netrc");

        let store = mem_store().await;
        let vault = PersistentSecretVault::new();
        vault.attach_store(store.clone()).await;
        vault.store("1", "GITHUB_TOKEN", "v", true).await;
        #[cfg(unix)]
        {
            let path_str = file_path.display().to_string();
            vault
                .store("2", &format!("file:{path_str}"), "fv", true)
                .await;
            assert!(file_path.exists());
        }

        assert!(vault.delete_persistent("GITHUB_TOKEN").await);
        assert!(!vault.delete_persistent("GITHUB_TOKEN").await, "幂等二删 false");
        assert_eq!(vault.lookup("GITHUB_TOKEN"), None);
        assert_eq!(store.get_credential_value("GITHUB_TOKEN").await.unwrap(), None);

        #[cfg(unix)]
        {
            assert!(vault.delete_persistent(&format!("file:{}", file_path.display())).await);
            assert!(!file_path.exists(), "file 形态删除时联动删文件");
        }
    }

    #[test]
    fn mask_replaces_known_values_only() {
        let values = vec!["sk-long-secret-value".to_owned(), "ab".to_owned()];
        let out = mask_with_known_values(&values, "leak sk-long-secret-value but ab stays");
        assert!(out.contains("***"));
        assert!(!out.contains("sk-long-secret-value"));
        assert!(out.contains("ab stays"), "过短值不参与打码");
    }

    #[test]
    fn debug_never_leaks_values() {
        let vault = PersistentSecretVault::new();
        // 同步路径：直接塞会话条目需要 async store；这里用 block_on 单线程。
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            vault.store("1", "DEBUG_TOKEN", "sk-debug-canary", true).await;
        });
        let debug = format!("{vault:?}");
        assert!(!debug.contains("sk-debug-canary"), "{debug}");
        assert!(!debug.contains("DEBUG_TOKEN"), "键名也不出（计数 only）: {debug}");
    }
}
