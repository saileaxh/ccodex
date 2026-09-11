//! Named upstream-proxy pool managed via the admin API, persisted to proxies.json next to
//! the accounts dir. An account bound to a proxy gets its data-plane client built with that
//! proxy injected into the env vars reqwest's system-proxy logic reads (the same mechanism
//! the official client uses); unbound accounts use the ambient default (config
//! upstream_proxy). Note: token-refresh clients are rebuilt by the official AuthManager at
//! refresh time and therefore follow the ambient default, not the account binding.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

/// Assignment target meaning "no proxy even if a default is configured".
pub const DIRECT: &str = "direct";

const PROXY_SCHEMES: [&str; 4] = ["http://", "https://", "socks5://", "socks5h://"];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredProxy {
    name: String,
    url: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoredFile {
    #[serde(default)]
    proxies: Vec<StoredProxy>,
    #[serde(default)]
    assignments: HashMap<String, String>,
}

/// Result of a connectivity check through a proxy (recorded on the entry for the UI).
#[derive(Debug, Clone, Serialize)]
pub struct ProxyCheck {
    pub ok: bool,
    pub ip: Option<String>,
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
    pub checked_at_unix: u64,
}

pub struct ProxyEntry {
    pub name: String,
    pub url: String,
    check: RwLock<Option<ProxyCheck>>,
}

/// How one account's data plane egresses.
pub enum Binding {
    /// Ambient default (config upstream_proxy / process env).
    Default,
    /// Explicitly direct, overriding the ambient default.
    Direct,
    /// Bound to a named pool proxy.
    Proxy { name: String, url: String },
}

struct Inner {
    proxies: Vec<ProxyEntry>,
    assignments: HashMap<String, String>,
}

pub struct ProxyStore {
    path: PathBuf,
    inner: RwLock<Inner>,
}

impl ProxyStore {
    /// proxies.json lives next to the accounts dir (both are runtime state).
    pub fn path_for(accounts_dir: &Path) -> PathBuf {
        accounts_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("proxies.json")
    }

    pub fn load(path: &Path) -> Self {
        let stored: StoredFile = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let proxies = stored
            .proxies
            .into_iter()
            .map(|p| ProxyEntry {
                name: p.name,
                url: p.url,
                check: RwLock::new(None),
            })
            .collect();
        Self {
            path: path.to_path_buf(),
            inner: RwLock::new(Inner {
                proxies,
                assignments: stored.assignments,
            }),
        }
    }

    fn persist(&self, inner: &Inner) -> Result<(), String> {
        let stored = StoredFile {
            proxies: inner
                .proxies
                .iter()
                .map(|p| StoredProxy {
                    name: p.name.clone(),
                    url: p.url.clone(),
                })
                .collect(),
            assignments: inner.assignments.clone(),
        };
        let text = serde_json::to_string_pretty(&stored).map_err(|e| e.to_string())?;
        // Proxy URLs may embed credentials — same owner-only policy as keys.json.
        crate::keys::write_restricted(&self.path, &text)
    }

    pub fn validate_url(url: &str) -> Result<(), String> {
        let lower = url.to_ascii_lowercase();
        if PROXY_SCHEMES.iter().any(|s| lower.starts_with(s)) {
            Ok(())
        } else {
            Err(format!("仅支持 http/https/socks5/socks5h: {url}"))
        }
    }

    fn validate_name(name: &str) -> Result<(), String> {
        if name.is_empty() || name.len() > 32 {
            return Err("名称需为 1-32 字符".to_string());
        }
        if name == DIRECT {
            return Err(format!("{DIRECT} 是保留名（表示直连）"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            return Err("名称只允许字母数字和 . _ -".to_string());
        }
        Ok(())
    }

    pub fn add(&self, name: &str, url: &str) -> Result<(), String> {
        Self::validate_name(name)?;
        Self::validate_url(url)?;
        let mut inner = self.inner.write().unwrap();
        if inner.proxies.iter().any(|p| p.name == name) {
            return Err(format!("代理 {name} 已存在"));
        }
        inner.proxies.push(ProxyEntry {
            name: name.to_string(),
            url: url.to_string(),
            check: RwLock::new(None),
        });
        self.persist(&inner)
    }

    /// Removes the entry and every assignment pointing at it. Returns false if unknown.
    pub fn remove(&self, name: &str) -> Result<bool, String> {
        let mut inner = self.inner.write().unwrap();
        let before = inner.proxies.len();
        inner.proxies.retain(|p| p.name != name);
        if inner.proxies.len() == before {
            return Ok(false);
        }
        inner.assignments.retain(|_, target| target != name);
        self.persist(&inner)?;
        Ok(true)
    }

    /// Binds an account to a proxy name, to DIRECT, or (None) back to the default.
    pub fn assign(&self, account: &str, target: Option<&str>) -> Result<(), String> {
        let mut inner = self.inner.write().unwrap();
        match target {
            None => {
                inner.assignments.remove(account);
            }
            Some(DIRECT) => {
                inner
                    .assignments
                    .insert(account.to_string(), DIRECT.to_string());
            }
            Some(name) => {
                if !inner.proxies.iter().any(|p| p.name == name) {
                    return Err(format!("代理 {name} 不存在"));
                }
                inner
                    .assignments
                    .insert(account.to_string(), name.to_string());
            }
        }
        self.persist(&inner)
    }

    pub fn binding(&self, account: &str) -> Binding {
        let inner = self.inner.read().unwrap();
        match inner.assignments.get(account).map(String::as_str) {
            None => Binding::Default,
            Some(DIRECT) => Binding::Direct,
            Some(name) => inner
                .proxies
                .iter()
                .find(|p| p.name == name)
                .map(|p| Binding::Proxy {
                    name: p.name.clone(),
                    url: p.url.clone(),
                })
                .unwrap_or(Binding::Default),
        }
    }

    /// Snapshot for the admin UI: entries (with last check) plus the assignment map.
    pub fn snapshot(&self) -> (Vec<serde_json::Value>, HashMap<String, String>) {
        let inner = self.inner.read().unwrap();
        let proxies = inner
            .proxies
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "url": p.url,
                    "check": p.check.read().unwrap().clone(),
                })
            })
            .collect();
        (proxies, inner.assignments.clone())
    }

    pub fn set_check(&self, name: &str, check: ProxyCheck) {
        let inner = self.inner.read().unwrap();
        if let Some(entry) = inner.proxies.iter().find(|p| p.name == name) {
            *entry.check.write().unwrap() = Some(check);
        }
    }

    /// Checks connectivity through one proxy (or the ambient default when url is None):
    /// GET an IP echo service with a plain diagnostic reqwest client (deliberately not the
    /// official client — this never touches the upstream API).
    pub async fn check_proxy(url: Option<&str>) -> ProxyCheck {
        let started = std::time::Instant::now();
        let checked_at_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let result = check_proxy_inner(url).await;
        let latency_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(ip) => ProxyCheck {
                ok: true,
                ip: Some(ip),
                latency_ms: Some(latency_ms),
                error: None,
                checked_at_unix,
            },
            Err(e) => ProxyCheck {
                ok: false,
                ip: None,
                latency_ms: Some(latency_ms),
                error: Some(e),
                checked_at_unix,
            },
        }
    }
}

async fn check_proxy_inner(url: Option<&str>) -> Result<String, String> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(8));
    if let Some(url) = url {
        let proxy = reqwest::Proxy::all(url).map_err(|e| format!("代理地址无效: {e}"))?;
        builder = builder.proxy(proxy);
    }
    let client = builder.build().map_err(|e| e.to_string())?;
    let resp = client
        .get("https://api.ipify.org?format=json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    body.get("ip")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "ip 响应缺字段".to_string())
}

/// Serializes proxy-env mutation process-wide; every env-swapping client build and every
/// proxied one-off request (e.g. quota queries) holds it for the whole swap. A tokio Mutex
/// because the quota fetch must hold it across .await (the official route-aware pool builds
/// its reqwest client lazily at request time, reading the ambient proxy env then).
static PROXY_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const PROXY_VARS: [&str; 4] = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"];

/// How to set the proxy env vars for one swap section.
pub enum ProxyEnv {
    /// Leave the ambient process env untouched (still takes the lock to avoid racing swaps).
    Keep,
    /// Point all proxy vars at this URL.
    Set(String),
    /// Remove the vars (explicit direct).
    Clear,
}

/// RAII guard for a proxy-env swap section; restores the previous env on drop. Async-acquired
/// so the swap can cover a whole request, not just a synchronous client build.
pub struct ProxyEnvGuard {
    saved: Vec<Option<String>>,
    touched: bool,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl ProxyEnvGuard {
    pub async fn acquire(mode: ProxyEnv) -> Self {
        let guard = PROXY_ENV_LOCK.lock().await;
        let saved: Vec<Option<String>> = PROXY_VARS.iter().map(|v| std::env::var(v).ok()).collect();
        let touched = !matches!(mode, ProxyEnv::Keep);
        if touched {
            for var in PROXY_VARS {
                // SAFETY: the lock above serializes every proxy-env mutation in this process
                // (client builds and one-off requests alike) for the guard's lifetime.
                unsafe {
                    match &mode {
                        ProxyEnv::Set(url) => std::env::set_var(var, url),
                        ProxyEnv::Clear => std::env::remove_var(var),
                        ProxyEnv::Keep => {}
                    }
                }
            }
        }
        Self {
            saved,
            touched,
            _guard: guard,
        }
    }
}

impl Drop for ProxyEnvGuard {
    fn drop(&mut self) {
        if !self.touched {
            return;
        }
        for (var, old) in PROXY_VARS.iter().zip(&self.saved) {
            // SAFETY: the guard still holds the lock; restores the pre-swap state.
            unsafe {
                match old {
                    Some(v) => std::env::set_var(var, v),
                    None => std::env::remove_var(var),
                }
            }
        }
    }
}

/// The env-swap mode for one account binding — the single mapping shared by every path
/// that must egress like the account: the data-plane transport, quota fetches, and both
/// telemetry channels (analytics events + Statsig OTLP metrics).
pub fn env_mode(binding: &Binding) -> ProxyEnv {
    match binding {
        Binding::Default => ProxyEnv::Keep,
        Binding::Direct => ProxyEnv::Clear,
        Binding::Proxy { url, .. } => ProxyEnv::Set(url.clone()),
    }
}

/// Builds official clients that bake in the process proxy env at `ClientBuilder::build`
/// time — reqwest's system-proxy logic resolves the ambient env exactly then. The env is
/// swapped to this account's binding for the duration of `build`, then restored.
///
/// The official client builder itself is untouched: this is the same construction path as
/// the official binary, only the ambient proxy differs — which is precisely how the
/// official client picks its own proxy. Every client an account talks through (data plane
/// and telemetry alike) must be built in here, so nothing egresses from a different IP
/// than the conversation it belongs to.
pub async fn with_binding<T>(binding: &Binding, build: impl FnOnce() -> T) -> T {
    let _guard = ProxyEnvGuard::acquire(env_mode(binding)).await;
    build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &Path) -> ProxyStore {
        ProxyStore::load(&dir.join("proxies.json"))
    }

    #[test]
    fn add_assign_persist_reload() {
        let dir = tempfile::tempdir().unwrap();
        {
            let s = store(dir.path());
            s.add("b-jp", "socks5h://127.0.0.1:17890").unwrap();
            s.add("c-hk", "http://127.0.0.1:7890").unwrap();
            s.assign("acc1", Some("b-jp")).unwrap();
            s.assign("acc2", Some(DIRECT)).unwrap();
        }
        let s = store(dir.path());
        match s.binding("acc1") {
            Binding::Proxy { name, url } => {
                assert_eq!(name, "b-jp");
                assert_eq!(url, "socks5h://127.0.0.1:17890");
            }
            _ => panic!("acc1 should bind to b-jp"),
        }
        assert!(matches!(s.binding("acc2"), Binding::Direct));
        assert!(matches!(s.binding("acc3"), Binding::Default));
        let (proxies, assignments) = s.snapshot();
        assert_eq!(proxies.len(), 2);
        assert_eq!(assignments.len(), 2);
    }

    #[test]
    fn validation_and_remove() {
        let dir = tempfile::tempdir().unwrap();
        let s = store(dir.path());
        assert!(s.add("bad name!", "socks5h://x:1").is_err());
        assert!(s.add(DIRECT, "socks5h://x:1").is_err());
        assert!(s.add("ok", "ftp://x:1").is_err());
        assert!(s.add("ok", "socks5h://x:1").is_ok());
        assert!(s.add("ok", "socks5h://x:2").is_err());
        s.assign("acc1", Some("ok")).unwrap();
        assert!(s.assign("acc1", Some("missing")).is_err());
        assert!(s.remove("ok").unwrap());
        assert!(!s.remove("ok").unwrap());
        assert!(matches!(s.binding("acc1"), Binding::Default));
        s.assign("acc1", None).unwrap();
        assert!(matches!(s.binding("acc1"), Binding::Default));
    }
}
