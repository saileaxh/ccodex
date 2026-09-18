pub(crate) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// SHA-256 hex digest. Used for opaque fingerprints (usage.json joins) and the legacy
/// admin-key hash format — never for new password storage (argon2id, see admin_auth).
pub(crate) fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time byte equality for credential comparisons. The fold has no early exit, so
/// the comparison time does not depend on how many leading bytes match.
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Strips `user:pass@` credentials from a URL for logging (proxy URLs commonly embed
/// them; the full URL must never land in logs or error text).
pub(crate) fn redact_url_credentials(url: &str) -> String {
    if crate::shadowsocks_proxy::is_shadowsocks(url) {
        return "ss://***".to_string();
    }
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    match rest.find('@') {
        Some(at) if !rest[..at].contains('/') => {
            format!("{}://***@{}", &url[..scheme_end], &rest[at + 1..])
        }
        _ => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_shape() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn redact_shape() {
        assert_eq!(
            redact_url_credentials("http://user:pass@proxy.local:8080"),
            "http://***@proxy.local:8080"
        );
        assert_eq!(
            redact_url_credentials("socks5h://u:p@host:1/path"),
            "socks5h://***@host:1/path"
        );
        assert_eq!(
            redact_url_credentials("http://proxy.local:8080"),
            "http://proxy.local:8080"
        );
        assert_eq!(redact_url_credentials("no-scheme"), "no-scheme");
    }
}
