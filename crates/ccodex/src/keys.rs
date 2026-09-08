//! Managed API keys: generated/deleted via the admin API, persisted to keys.json next to
//! the accounts dir — the only downstream credential source (legacy config.toml api_keys
//! are migrated into this store at startup). The store keeps full key material so the
//! admin panel can show it back — the panel is the recovery path for a lost key. File is
//! written owner-only (0600).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredKey {
    pub name: String,
    pub key: String,
    pub created_at_unix: u64,
}

pub struct KeyStore {
    path: PathBuf,
    inner: RwLock<Vec<StoredKey>>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Writes a secrets file owner-only (0600 on unix; the dir ACL governs on Windows).
pub(crate) fn write_restricted(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("写入 {} 失败: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl KeyStore {
    /// keys.json lives next to the accounts dir (both are runtime state).
    pub fn path_for(accounts_dir: &Path) -> PathBuf {
        accounts_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("keys.json")
    }

    pub fn load(path: &Path) -> Self {
        let keys: Vec<StoredKey> = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            inner: RwLock::new(keys),
        }
    }

    fn persist(&self, inner: &[StoredKey]) -> Result<(), String> {
        let text = serde_json::to_string_pretty(inner).map_err(|e| e.to_string())?;
        write_restricted(&self.path, &text)
    }

    pub fn is_empty(&self) -> bool {
        self.inner.read().unwrap().is_empty()
    }

    pub fn contains(&self, key: &str) -> bool {
        !key.is_empty() && self.inner.read().unwrap().iter().any(|k| k.key == key)
    }

    pub fn list(&self) -> Vec<StoredKey> {
        self.inner.read().unwrap().clone()
    }

    /// Creates a managed key. `key` None = generate `sk-<96 hex>`.
    pub fn add(&self, name: &str, key: Option<&str>) -> Result<StoredKey, String> {
        let name = name.trim();
        if name.is_empty() || name.len() > 32 {
            return Err("名称需为 1-32 字符".to_string());
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err("名称只允许字母数字和 . _ -".to_string());
        }
        let key = match key {
            Some(k) => {
                let k = k.trim();
                if k.len() < 16 {
                    return Err("自带 key 至少 16 字符".to_string());
                }
                k.to_string()
            }
            None => format!(
                "sk-{}{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
        };
        let mut inner = self.inner.write().unwrap();
        if inner.iter().any(|k| k.name == name) {
            return Err(format!("密钥 {name} 已存在"));
        }
        if inner.iter().any(|k| k.key == key) {
            return Err("该 key 值已存在".to_string());
        }
        let stored = StoredKey {
            name: name.to_string(),
            key,
            created_at_unix: now_unix(),
        };
        inner.push(stored.clone());
        self.persist(&inner)?;
        Ok(stored)
    }

    pub fn remove(&self, name: &str) -> Result<Option<StoredKey>, String> {
        let mut inner = self.inner.write().unwrap();
        let Some(pos) = inner.iter().position(|k| k.name == name) else {
            return Ok(None);
        };
        let removed = inner.remove(pos);
        self.persist(&inner)?;
        Ok(Some(removed))
    }

    /// Display mask for config-file keys (managed keys are shown in full — the panel is
    /// the recovery path).
    pub fn mask(key: &str) -> String {
        let len = key.chars().count();
        if len <= 10 {
            return "***".to_string();
        }
        let prefix: String = key.chars().take(6).collect();
        let suffix: String = key.chars().skip(len - 4).collect();
        format!("{prefix}…{suffix}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_list_remove_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.json");
        {
            let s = KeyStore::load(&path);
            assert!(s.is_empty());
            let k = s.add("main", None).unwrap();
            assert!(k.key.starts_with("sk-") && k.key.len() > 90);
            assert!(s.contains(&k.key));
            assert!(s.add("main", None).is_err());
            assert!(s.add("bad name!", None).is_err());
            assert!(s.add("short", Some("tiny")).is_err());
        }
        let s = KeyStore::load(&path);
        assert_eq!(s.list().len(), 1);
        let k = s.remove("main").unwrap().unwrap();
        assert!(k.key.starts_with("sk-"));
        assert!(s.remove("main").unwrap().is_none());
        assert!(s.is_empty());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn mask_shape() {
        assert_eq!(KeyStore::mask("short"), "***");
        assert_eq!(KeyStore::mask("sk-abcdefgh123456789"), "sk-abc…6789");
    }
}
