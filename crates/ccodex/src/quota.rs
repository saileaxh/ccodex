//! On-demand upstream plan/quota query via the official codex-backend-client
//! (`GET <chatgpt base>/wham/usage`), as opposed to the passive snapshots learned from
//! response traffic. Follows the account's proxy binding like its data-plane traffic:
//! the official route-aware pool resolves the proxy route and builds its reqwest client
//! lazily at request time, so the swap must cover the request, not just construction.

use crate::accounts::Account;
use crate::proxies::{Binding, ProxyEnv, ProxyEnvGuard};
use codex_http_client::{HttpClientFactory, OutboundProxyPolicy};
use std::time::Duration;

const QUOTA_QUERY_TIMEOUT: Duration = Duration::from_secs(12);

/// Fetches the account's rate-limit/plan snapshot from the official usage endpoint and
/// stores it on the account (same JSON shape the traffic path records, so the admin UI
/// renders both sources identically).
pub async fn fetch_account_quotas(
    account: &Account,
    backend_base: &str,
    binding: Binding,
) -> anyhow::Result<serde_json::Value> {
    let auth = account
        .auth_manager
        .auth()
        .await
        .ok_or_else(|| anyhow::anyhow!("凭证不可用，请重新登录"))?;
    let mode = match &binding {
        Binding::Default => ProxyEnv::Keep,
        Binding::Direct => ProxyEnv::Clear,
        Binding::Proxy { url, .. } => ProxyEnv::Set(url.clone()),
    };
    let _guard = ProxyEnvGuard::acquire(mode).await;
    // Official constructor: CF-cookie pool + auth headers (Authorization + ChatGPT-Account-Id).
    let client = codex_backend_client::Client::from_auth(
        backend_base,
        &auth,
        HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
    );
    // Passive reader: supports_luna_reserve stays false per the official API contract.
    let result = tokio::time::timeout(
        QUOTA_QUERY_TIMEOUT,
        client.get_rate_limits_with_reset_credits(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("查询上游配额超时"))?
    .map_err(|e| anyhow::anyhow!("查询上游配额失败: {e}"))?;
    let quotas = serde_json::to_value(&result.rate_limits)?;
    account.set_quotas(quotas.clone());
    Ok(quotas)
}

/// The relay's provider base is the codex API root (.../backend-api/codex); the usage
/// endpoint lives one level up (.../backend-api/wham/usage).
pub fn backend_base_from_provider(provider_base: &str) -> String {
    let base = provider_base.trim_end_matches('/');
    base.strip_suffix("/codex").unwrap_or(base).to_string()
}

#[cfg(test)]
mod tests {
    use super::backend_base_from_provider;

    #[test]
    fn strips_codex_suffix() {
        assert_eq!(
            backend_base_from_provider("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            backend_base_from_provider("https://chatgpt.com/backend-api/codex/"),
            "https://chatgpt.com/backend-api"
        );
        assert_eq!(
            backend_base_from_provider("http://127.0.0.1:9999"),
            "http://127.0.0.1:9999"
        );
    }
}
