//! Panel login key: a single admin credential stored apart from the downstream API keys
//! (admin.json next to the accounts dir, SHA-256 hashed, owner-only 0600). First run is
//! setup mode: the admin API refuses everything except the setup endpoint until a login
//! key is chosen. The login key never authorizes downstream /v1 traffic, and downstream
//! keys never authorize the panel — the two credential kinds stay separate by construction
//! (different stores, different verifiers).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAdmin {
    token_hash: String,
    updated_at_unix: u64,
}

pub struct AdminAuth {
    path: PathBuf,
    inner: RwLock<Option<StoredAdmin>>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl AdminAuth {
    /// admin.json lives next to the accounts dir (same runtime-state dir as keys.json).
    pub fn path_for(accounts_dir: &Path) -> PathBuf {
        accounts_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("admin.json")
    }

    pub fn load(path: &Path) -> Self {
        let stored: Option<StoredAdmin> = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok());
        Self {
            path: path.to_path_buf(),
            inner: RwLock::new(stored),
        }
    }

    pub fn is_set(&self) -> bool {
        self.inner.read().unwrap().is_some()
    }

    pub fn updated_at_unix(&self) -> Option<u64> {
        self.inner
            .read()
            .unwrap()
            .as_ref()
            .map(|s| s.updated_at_unix)
    }

    pub fn verify(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let inner = self.inner.read().unwrap();
        match inner.as_ref() {
            Some(stored) => stored.token_hash == hash_token(token),
            None => false,
        }
    }

    /// Sets or replaces the login key. Validation only — the caller enforces the
    /// current-key check and the separation from downstream keys.
    pub fn set(&self, token: &str) -> Result<(), String> {
        let token = token.trim();
        if token.len() < 8 || token.len() > 128 {
            return Err("登录密钥需为 8-128 字符".to_string());
        }
        let stored = StoredAdmin {
            token_hash: hash_token(token),
            updated_at_unix: now_unix(),
        };
        let text = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
        crate::keys::write_restricted(&self.path, &text)?;
        *self.inner.write().unwrap() = Some(stored);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_verify_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.json");
        {
            let a = AdminAuth::load(&path);
            assert!(!a.is_set());
            assert!(!a.verify("anything"));
            assert!(a.set("short").is_err());
            a.set("panel-secret-1").unwrap();
            assert!(a.is_set());
            assert!(a.verify("panel-secret-1"));
            assert!(!a.verify("panel-secret-2"));
            assert!(!a.verify(""));
            // Stored hashed, never in clear.
            let raw = std::fs::read_to_string(&path).unwrap();
            assert!(!raw.contains("panel-secret-1"));
        }
        let a = AdminAuth::load(&path);
        assert!(a.verify("panel-secret-1"));
        a.set("panel-secret-2").unwrap();
        assert!(a.verify("panel-secret-2"));
        assert!(!a.verify("panel-secret-1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
