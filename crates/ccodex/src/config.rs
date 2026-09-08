use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Listen address, default 127.0.0.1:8317.
    pub listen: Option<String>,
    /// Accounts directory: each subdirectory is an account (contains auth.json, same
    /// format as ~/.codex/auth.json).
    pub accounts_dir: Option<PathBuf>,
    /// Outbound proxy (e.g. http://127.0.0.1:7890). When set, written into HTTPS_PROXY
    /// etc., matching how the official client reads proxies (reqwest system-proxy behavior).
    pub upstream_proxy: Option<String>,
    #[serde(default)]
    pub identity: IdentityConfig,
    #[serde(default)]
    pub relay: RelayConfig,
}

/// Legacy `api_keys` from a pre-managed-keys config file. The Config struct no longer
/// deserializes them (unknown fields are ignored); this peeks at the raw TOML solely for
/// the one-time startup migration into keys.json.
pub fn legacy_api_keys(config_path: &Path) -> Vec<String> {
    // toml 0.9: Value::from_str parses a bare value, not a document — must go through
    // toml::from_str (a document deserializes into Value::Table).
    std::fs::read_to_string(config_path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|v| v.get("api_keys")?.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|k| k.as_str().map(str::to_string))
        .filter(|k| !k.is_empty())
        .collect()
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct IdentityConfig {
    /// Codex version reported upstream; defaults to the compile-time baked upstream
    /// version (resolved by the sync script) — normally leave unset.
    pub version: Option<String>,
    /// originator override; defaults to the official codex_cli_rs.
    pub originator: Option<String>,
    /// UA platform persona: "windows" (default, fixed Windows desktop fingerprint) or
    /// "native" (no UA segment overrides at all — official dynamic detection; required
    /// for Linux deployments so the UA stays consistent with the OpenSSL TLS stack and
    /// byte-identical to official Linux clients).
    pub ua_platform: Option<String>,
    /// UA OS segment; default Windows (applies in ua_platform = "windows" mode).
    pub ua_os_type: Option<String>,
    /// UA OS version segment; default 10.0.26200.
    pub ua_os_version: Option<String>,
    /// UA arch segment; default x86_64.
    pub ua_arch: Option<String>,
    /// UA terminal segment; default WindowsTerminal/1.21.
    pub ua_terminal: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RelayConfig {
    /// Max account switches per request (excluding the first attempt); default 3.
    pub max_account_switches: Option<usize>,
    /// Derive session-id per downstream API key so multi-user sessions can't be
    /// correlated upstream; default true.
    pub session_isolation: Option<bool>,
    /// zstd request-body compression (official clients enable it by default); default true.
    pub request_compression: Option<bool>,
    /// Sticky-session TTL in seconds; default 3600.
    pub sticky_ttl_secs: Option<u64>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("读取配置文件 {} 失败: {e}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("解析配置文件 {} 失败: {e}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if let Some(proxy) = &self.upstream_proxy {
            let lower = proxy.to_ascii_lowercase();
            let schemes = ["http://", "https://", "socks5://", "socks5h://"];
            if !schemes.iter().any(|s| lower.starts_with(s)) {
                anyhow::bail!(
                    "upstream_proxy 仅支持 http/https/socks5/socks5h（可内嵌 user:pass@ 认证），当前: {proxy}"
                );
            }
        }
        if let Some(platform) = &self.identity.ua_platform
            && !matches!(platform.as_str(), "windows" | "native")
        {
            anyhow::bail!("identity.ua_platform 仅支持 \"windows\" / \"native\"，当前: {platform}");
        }
        Ok(())
    }

    pub fn listen(&self) -> &str {
        self.listen.as_deref().unwrap_or("127.0.0.1:8317")
    }

    pub fn accounts_dir(&self) -> PathBuf {
        self.accounts_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from("./accounts"))
    }

    pub fn max_account_switches(&self) -> usize {
        self.relay.max_account_switches.unwrap_or(3)
    }

    pub fn session_isolation(&self) -> bool {
        self.relay.session_isolation.unwrap_or(true)
    }

    pub fn request_compression(&self) -> bool {
        self.relay.request_compression.unwrap_or(true)
    }

    pub fn sticky_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.relay.sticky_ttl_secs.unwrap_or(3600))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_peek() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.toml");
        std::fs::write(&p, "listen = \"127.0.0.1:1\"\napi_keys = [\"sk-a\", \"sk-b\"]\naccounts_dir = \"./x\"\n\n[identity]\nversion = \"1\"\n").unwrap();
        let keys = super::legacy_api_keys(&p);
        assert_eq!(keys, vec!["sk-a".to_string(), "sk-b".to_string()]);
    }
}
