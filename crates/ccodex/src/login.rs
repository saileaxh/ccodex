//! Login flows, all upstream-facing parts driven by official codex-login code.
//!
//! Primary: OAuth authorization-code flow (identical to `codex login` in a browser).
//! The relay is headless, so instead of the official localhost callback listener the user
//! pastes back the final redirect URL (browser lands on a dead localhost:1455 page); the
//! code exchange, API-key obtainment and auth.json persistence are all official code.
//!
//! Fallback: device-code flow (kept for environments where paste-back is impractical;
//! note it is more likely to trip upstream risk control than the browser flow).

use codex_login::{
    AuthCredentialsStoreMode, AuthKeyringBackendKind, AuthRouteConfig, DeviceCode, ServerOptions,
};
use std::path::PathBuf;

/// Official redirect target: the Codex CLI Hydra allow-list only permits localhost:1455
/// (and the 1457 fallback). Nothing listens there for a headless relay — the user copies
/// the URL out of the dead browser tab.
pub const OAUTH_REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

/// An in-flight OAuth login (admin API state machine).
pub struct OAuthPending {
    pub codex_home: PathBuf,
    pub pkce: codex_login::PkceCodes,
    pub state: String,
}

/// Builds the official authorize URL and keeps the PKCE/state material for the completion.
pub fn start_oauth_login(codex_home: PathBuf) -> (String, OAuthPending) {
    let pkce = codex_login::generate_pkce();
    let state = codex_login::generate_state();
    let url = codex_login::build_authorize_url(
        codex_login::DEFAULT_ISSUER,
        codex_login::CLIENT_ID,
        OAUTH_REDIRECT_URI,
        &pkce,
        &state,
        /*forced_chatgpt_workspace_ids*/ None,
    );
    (
        url,
        OAuthPending {
            codex_home,
            pkce,
            state,
        },
    )
}

/// Extracts (code, state) from a pasted callback URL; also tolerates a bare query string.
fn parse_callback_params(pasted: &str) -> Option<(String, String)> {
    let query = pasted
        .trim()
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or(pasted.trim());
    let mut code = None;
    let mut state = None;
    for pair in query.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let v = percent_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => state = Some(v.split('#').next().unwrap_or(&v).to_string()),
            _ => {}
        }
    }
    match (code, state) {
        (Some(c), Some(s)) if !c.is_empty() && !s.is_empty() => Some((c, s)),
        _ => None,
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Ok(hex) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(hex);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Completes the OAuth login from the pasted callback URL: verifies state, then the
/// official exchange / api-key / persist chain writes auth.json into the account dir.
pub async fn finish_oauth_login(
    pending: &OAuthPending,
    route: &AuthRouteConfig,
    pasted_url: &str,
) -> anyhow::Result<()> {
    let Some((code, state)) = parse_callback_params(pasted_url) else {
        anyhow::bail!("粘贴的内容里没找到 code/state，请复制地址栏完整 URL");
    };
    if state != pending.state {
        anyhow::bail!("state 不匹配（可能粘贴了另一次登录的链接），请重新发起");
    }
    let tokens = codex_login::exchange_code_for_tokens(
        codex_login::DEFAULT_ISSUER,
        codex_login::CLIENT_ID,
        OAUTH_REDIRECT_URI,
        &pending.pkce,
        &code,
        route,
    )
    .await
    .map_err(|e| anyhow::anyhow!("token 交换失败: {e}"))?;
    let api_key = codex_login::obtain_api_key(
        codex_login::DEFAULT_ISSUER,
        codex_login::CLIENT_ID,
        &tokens.id_token,
        route,
    )
    .await
    .ok();
    std::fs::create_dir_all(&pending.codex_home)?;
    codex_login::persist_tokens_async(
        &pending.codex_home,
        api_key,
        tokens.id_token,
        tokens.access_token,
        tokens.refresh_token,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("凭证落盘失败: {e}"))?;
    Ok(())
}

/// A device-login session (admin API state machine).
#[derive(Clone)]
pub struct LoginSession {
    pub verification_url: String,
    pub user_code: String,
    pub status: LoginStatus,
}

#[derive(Clone)]
pub enum LoginStatus {
    Pending,
    Done,
    Error(String),
}

pub fn server_options(codex_home: PathBuf, route: &AuthRouteConfig) -> ServerOptions {
    let mut opts = ServerOptions::new(
        codex_home,
        codex_login::CLIENT_ID.to_string(),
        /*forced_chatgpt_workspace_id*/ None,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
        route.clone(),
    );
    // A relay server has no browser; the device flow needs no local callback port.
    opts.open_browser = false;
    opts
}

/// CLI login: prints the official authorize URL, waits for the pasted callback URL, and
/// writes auth.json into codex_home — the same browser flow as `codex login`.
pub async fn oauth_login_cli(codex_home: PathBuf, route: &AuthRouteConfig) -> anyhow::Result<()> {
    let (url, pending) = start_oauth_login(codex_home.clone());
    println!();
    println!("  请在浏览器打开授权链接（与官方 codex login 同一流程）:");
    println!("  {url}");
    println!();
    println!("授权完成后浏览器会跳转到一个打不开的 localhost:1455 页面，");
    println!("把地址栏里的完整 URL 粘贴到这里并回车:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    finish_oauth_login(&pending, route, &line).await?;
    println!(
        "登录成功，凭证已保存到 {}",
        codex_home.join("auth.json").display()
    );
    Ok(())
}

/// Admin API: only requests the device code and returns display info; the authorization
/// poll runs in a background task.
pub async fn start_device_login(
    codex_home: PathBuf,
    route: &AuthRouteConfig,
) -> anyhow::Result<DeviceCode> {
    std::fs::create_dir_all(&codex_home)?;
    let opts = server_options(codex_home, route);
    codex_login::request_device_code(&opts)
        .await
        .map_err(|e| anyhow::anyhow!("请求设备码失败: {e}"))
}

/// Admin API: completes the device login in the background (official code polls on the
/// interval, up to 15 minutes).
pub async fn finish_device_login(
    codex_home: PathBuf,
    route: &AuthRouteConfig,
    device: DeviceCode,
) -> anyhow::Result<()> {
    let opts = server_options(codex_home, route);
    codex_login::complete_device_code_login(opts, device)
        .await
        .map_err(|e| anyhow::anyhow!("设备码登录失败: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_full_url() {
        let url = "http://localhost:1455/auth/callback?code=abc%2F123%3D&state=xyz789";
        let (code, state) = parse_callback_params(url).unwrap();
        assert_eq!(code, "abc/123=");
        assert_eq!(state, "xyz789");
    }

    #[test]
    fn parse_callback_bare_query_and_fragment() {
        let (code, state) = parse_callback_params("code=c%40d&state=s1#ignored-fragment").unwrap();
        assert_eq!(code, "c@d");
        assert_eq!(state, "s1");
    }

    #[test]
    fn parse_callback_rejects_garbage() {
        assert!(parse_callback_params("https://example.com/").is_none());
        assert!(parse_callback_params("code=only").is_none());
        assert!(parse_callback_params("state=only").is_none());
    }
}
