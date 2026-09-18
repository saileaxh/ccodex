//! Panel login key: a single admin credential stored apart from the downstream API keys
//! (admin.json next to the accounts dir, owner-only 0600). First run is setup mode: the
//! admin API refuses everything except the setup endpoint until a login key is chosen. The
//! login key never authorizes downstream /v1 traffic, and downstream keys never authorize
//! the panel — the two credential kinds stay separate by construction (different stores,
//! different verifiers).
//!
//! Hashing: argon2id (salted, memory-hard; PHC string stored). Legacy files holding a bare
//! unsalted SHA-256 hex digest still verify, and are transparently upgraded to argon2id on
//! the next successful login.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::util::{now_unix, sha256_hex};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAdmin {
    /// argon2id PHC string ("$argon2id$..."), or a legacy bare SHA-256 hex digest.
    token_hash: String,
    updated_at_unix: u64,
}

pub struct AdminAuth {
    path: PathBuf,
    inner: RwLock<Option<StoredAdmin>>,
}

fn is_legacy_sha256(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

fn hash_argon2(token: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    argon2::Argon2::default()
        .hash_password(token.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| format!("密钥哈希失败: {e}"))
}

fn verify_argon2(hash: &str, token: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    argon2::Argon2::default()
        .verify_password(token.as_bytes(), &parsed)
        .is_ok()
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
        let stored = self.inner.read().unwrap().clone();
        let Some(stored) = stored else {
            return false;
        };
        if is_legacy_sha256(&stored.token_hash) {
            // Legacy constant-time compare; upgrade to argon2id on success.
            let ok = crate::util::ct_eq(
                stored.token_hash.as_bytes(),
                sha256_hex(token).as_bytes(),
            );
            if ok && self.set(token).is_ok() {
                tracing::info!("admin login key upgraded to argon2id hashing");
            }
            return ok;
        }
        verify_argon2(&stored.token_hash, token)
    }

    /// Sets or replaces the login key. Validation only — the caller enforces the
    /// current-key check and the separation from downstream keys.
    pub fn set(&self, token: &str) -> Result<(), String> {
        let token = token.trim();
        if token.len() < 8 || token.len() > 128 {
            return Err("登录密钥需为 8-128 字符".to_string());
        }
        let stored = StoredAdmin {
            token_hash: hash_argon2(token)?,
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
            assert!(raw.contains("$argon2id$"));
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

    #[test]
    fn legacy_sha256_upgrades_on_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.json");
        // A pre-upgrade admin.json: bare unsalted SHA-256 hex digest.
        let legacy = serde_json::json!({
            "token_hash": sha256_hex("panel-secret-1"),
            "updated_at_unix": 1u64,
        });
        std::fs::write(&path, serde_json::to_string(&legacy).unwrap()).unwrap();

        let a = AdminAuth::load(&path);
        assert!(a.verify("panel-secret-1"));
        // Transparently re-hashed with argon2id after the successful legacy verify.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("$argon2id$"));
        assert!(!a.verify("wrong"));
        assert!(a.verify("panel-secret-1"));
    }
}
