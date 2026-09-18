//! Per-key / per-account usage + cost accounting. Attribution happens at the SSE tap on
//! stream completion (usage from the response.completed event); cost comes from the
//! pricing table (equivalent API spend — subscription accounts are not billed per token).
//! Keys are identified by a SHA-256 fingerprint (16 hex chars) so raw key material is
//! never duplicated into the stats file. Persisted to usage.json (0600) next to the
//! accounts dir.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use crate::util::now_unix;

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Totals {
    pub requests: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl Totals {
    fn add(&mut self, tokens: (u64, u64, u64), cost: f64) {
        self.requests += 1;
        self.input_tokens += tokens.0;
        self.cached_input_tokens += tokens.1;
        self.output_tokens += tokens.2;
        self.cost_usd += cost;
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct KeyUsage {
    // Nested rather than #[serde(flatten)]: flatten + f64 breaks serde_json
    // deserialization ("invalid type: map, expected f64", serde-rs/serde#1183).
    pub totals: Totals,
    /// Display mask of the key (full material never enters this file).
    pub mask: String,
    pub last_seen_unix: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodUsage {
    pub start_unix: u64,
    pub end_unix: u64,
    pub totals: Totals,
    #[serde(default = "legacy_period_is_partial")]
    pub partial: bool,
}

fn legacy_period_is_partial() -> bool {
    true
}

pub fn same_period(first: (u64, u64), second: (u64, u64)) -> bool {
    first.1.saturating_sub(first.0) == second.1.saturating_sub(second.0)
        && first.0.abs_diff(second.0) <= 60
        && first.1.abs_diff(second.1) <= 60
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct AccountUsage {
    pub totals: Totals,
    pub last_seen_unix: u64,
    /// Usage inside the current upstream rate-limit window (aligned via quota snapshots).
    pub period: Option<PeriodUsage>,
}

#[derive(Default, Serialize, Deserialize)]
struct UsageData {
    keys: BTreeMap<String, KeyUsage>,
    accounts: BTreeMap<String, AccountUsage>,
}

pub struct UsageStore {
    path: PathBuf,
    inner: RwLock<UsageData>,
}

/// Stable identity for stats rows; first 16 hex chars of SHA-256.
pub fn key_fingerprint(key: &str) -> String {
    crate::util::sha256_hex(key)[..16].to_string()
}

/// Extracts (input, cached_input, output) from a Responses usage object.
pub fn parse_usage_tokens(usage: &Value) -> (u64, u64, u64) {
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    (input, cached, output)
}

impl UsageStore {
    pub fn path_for(accounts_dir: &Path) -> PathBuf {
        accounts_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("usage.json")
    }

    pub fn load(path: &Path) -> Self {
        let data: UsageData = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            inner: RwLock::new(data),
        }
    }

    fn persist(&self, data: &UsageData) {
        let text = match serde_json::to_string_pretty(data) {
            Ok(t) => t,
            Err(_) => return,
        };
        if let Err(e) = crate::keys::write_restricted(&self.path, &text) {
            tracing::warn!(error = %e, "usage.json 落盘失败");
        }
    }

    /// Records one completed turn. `key` is None in open mode (no downstream keys) —
    /// account accounting still runs. `window` is the account's current upstream primary
    /// window (start, end); when it changes the period counters roll over.
    pub fn record(
        &self,
        key: Option<(String, String)>,
        account: &str,
        window: Option<(u64, u64)>,
        tokens: (u64, u64, u64),
        cost: f64,
    ) {
        let now = now_unix();
        self.record_at(key, account, window, tokens, cost, now);
    }

    fn record_at(
        &self,
        key: Option<(String, String)>,
        account: &str,
        window: Option<(u64, u64)>,
        tokens: (u64, u64, u64),
        cost: f64,
        now: u64,
    ) {
        let mut data = self.inner.write().unwrap();
        if let Some((fp, mask)) = key {
            let entry = data.keys.entry(fp).or_default();
            entry.totals.add(tokens, cost);
            entry.mask = mask;
            entry.last_seen_unix = now;
        }
        let acc = data.accounts.entry(account.to_string()).or_default();
        let had_unassigned_usage = acc.totals.requests > 0 && acc.period.is_none();
        acc.totals.add(tokens, cost);
        acc.last_seen_unix = now;
        let window = window
            .filter(|(start, end)| *start <= now && now < *end)
            .or_else(|| {
                acc.period.as_ref().and_then(|period| {
                    (period.start_unix <= now && now < period.end_unix)
                        .then_some((period.start_unix, period.end_unix))
                })
            });
        if let Some((start, end)) = window {
            match &mut acc.period {
                Some(p) if same_period((p.start_unix, p.end_unix), (start, end)) => {
                    p.totals.add(tokens, cost);
                }
                _ => {
                    let mut totals = Totals::default();
                    totals.add(tokens, cost);
                    acc.period = Some(PeriodUsage {
                        start_unix: start,
                        end_unix: end,
                        totals,
                        partial: had_unassigned_usage,
                    });
                }
            }
        }
        self.persist(&data);
    }

    pub fn key_usage(&self, fingerprint: &str) -> Option<KeyUsage> {
        self.inner.read().unwrap().keys.get(fingerprint).cloned()
    }

    pub fn account_usage(&self, account: &str) -> Option<AccountUsage> {
        self.inner.read().unwrap().accounts.get(account).cloned()
    }

    pub fn account_usage_in_window(
        &self,
        account: &str,
        window: Option<(u64, u64)>,
        now: u64,
    ) -> Option<AccountUsage> {
        let mut usage = self.account_usage(account)?;
        usage.period = usage.period.filter(|period| {
            period.start_unix <= now
                && now < period.end_unix
                && window
                    .is_none_or(|window| same_period((period.start_unix, period.end_unix), window))
        });
        Some(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tokens_shape() {
        let (i, c, o) = parse_usage_tokens(&serde_json::json!({
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 30},
            "output_tokens": 45
        }));
        assert_eq!((i, c, o), (100, 30, 45));
        assert_eq!(parse_usage_tokens(&serde_json::json!({})), (0, 0, 0));
    }

    #[test]
    fn reset_timestamp_jitter_and_restart_do_not_erase_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let store = UsageStore::load(&path);
        let start = 1_789_289_017;
        let end = start + 604_800;
        for offset in [0, 1, 0, 1, 0] {
            store.record_at(
                None,
                "acc",
                Some((start + offset, end + offset)),
                (100, 80, 10),
                0.001,
                start + 100,
            );
        }
        let reloaded = UsageStore::load(&path);
        reloaded.record_at(None, "acc", None, (100, 80, 10), 0.001, start + 101);
        let usage = reloaded
            .account_usage_in_window("acc", Some((start + 1, end + 1)), start + 102)
            .unwrap();
        let period = usage.period.unwrap();
        assert_eq!(period.totals.requests, 6);
        assert_eq!(period.totals.input_tokens, 600);
        assert!((period.totals.cost_usd - 0.006).abs() < 1e-10);
        assert_eq!((period.start_unix, period.end_unix), (start, end));
        assert!(!period.partial);
        assert!(
            reloaded
                .account_usage_in_window("acc", None, end)
                .unwrap()
                .period
                .is_none()
        );
        reloaded.record_at(
            None,
            "acc",
            Some((end, end + 604_800)),
            (7, 0, 3),
            0.002,
            end + 1,
        );
        let usage = reloaded.account_usage("acc").unwrap();
        assert_eq!(usage.totals.requests, 7);
        assert_eq!(usage.period.unwrap().totals.requests, 1);
    }

    #[test]
    fn legacy_period_is_marked_partial_without_changing_totals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let data = serde_json::json!({"keys": {}, "accounts": {"acc": {
            "totals": Totals {requests: 500, input_tokens: 10000, ..Totals::default()},
            "last_seen_unix": 1500,
            "period": {"start_unix": 1000, "end_unix": 2000,
                "totals": Totals {requests: 2, input_tokens: 100, ..Totals::default()}}
        }}});
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        let store = UsageStore::load(&path);
        store.record_at(None, "acc", Some((1001, 2001)), (10, 0, 5), 0.01, 1501);
        let usage = UsageStore::load(&path).account_usage("acc").unwrap();
        assert_eq!(usage.totals.requests, 501);
        let period = usage.period.unwrap();
        assert!(period.partial);
        assert_eq!(period.totals.requests, 3);
        assert_eq!(period.totals.input_tokens, 110);
    }

    #[test]
    fn record_accumulates_and_rolls_period() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.json");
        let store = UsageStore::load(&path);
        let key = Some(("fp1".to_string(), "sk-abc…6789".to_string()));
        store.record_at(
            key.clone(),
            "acc",
            Some((1000, 2000)),
            (100, 0, 50),
            0.01,
            1500,
        );
        store.record_at(
            key.clone(),
            "acc",
            Some((1000, 2000)),
            (200, 10, 60),
            0.02,
            1501,
        );
        let k = store.key_usage("fp1").unwrap();
        assert_eq!(k.totals.requests, 2);
        assert_eq!(k.totals.input_tokens, 300);
        let a = store.account_usage("acc").unwrap();
        assert_eq!(a.totals.output_tokens, 110);
        let p = a.period.as_ref().unwrap();
        assert_eq!((p.start_unix, p.end_unix), (1000, 2000));
        assert_eq!(p.totals.requests, 2);
        // New window (resets_at moved) rolls the period, keeps all-time totals.
        store.record_at(key, "acc", Some((2000, 3000)), (5, 0, 5), 0.001, 2500);
        let a = store.account_usage("acc").unwrap();
        assert_eq!(a.totals.requests, 3);
        let p = a.period.as_ref().unwrap();
        assert_eq!(
            (p.start_unix, p.end_unix, p.totals.requests),
            (2000, 3000, 1)
        );
        // Open mode: account-only accounting.
        store.record(None, "acc", None, (1, 0, 1), 0.0);
        assert_eq!(store.account_usage("acc").unwrap().totals.requests, 4);
        // Persisted (fp1 saw records 1-3; the open-mode record was account-only).
        let reloaded = UsageStore::load(&path);
        assert_eq!(reloaded.key_usage("fp1").unwrap().totals.requests, 3);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
