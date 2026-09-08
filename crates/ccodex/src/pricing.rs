//! Reference pricing table (USD per 1M tokens) for cost estimation. ChatGPT-subscription
//! accounts are not billed per token — these are the official API list prices for the same
//! model slugs (https://developers.openai.com/api/docs/pricing, short-context standard
//! rates), so "cost" means equivalent API spend, not an actual charge. Exact slug match,
//! "*" fallback; admin overrides persist to pricing.json and win over the defaults.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PriceEntry {
    pub input_per_m: f64,
    pub cached_input_per_m: f64,
    pub output_per_m: f64,
}

pub const FALLBACK_MATCH: &str = "*";

const fn e(input: f64, cached: f64, output: f64) -> PriceEntry {
    PriceEntry {
        input_per_m: input,
        cached_input_per_m: cached,
        output_per_m: output,
    }
}

/// Official list prices, checked 2026-09 (models without public cached pricing reuse the
/// input rate — conservative). "*" is the GPT-5-class fallback for unknown slugs.
static DEFAULTS: &[(&str, PriceEntry)] = &[
    ("gpt-6-astra", e(10.0, 1.0, 50.0)),
    ("gpt-5.6-sol", e(4.0, 0.40, 20.0)),
    ("gpt-5.6-terra", e(2.0, 0.20, 12.0)),
    ("gpt-5.6-luna", e(0.20, 0.02, 1.20)),
    ("gpt-5.6-cyber", e(12.50, 1.25, 75.0)),
    ("gpt-5.5-cyber", e(12.50, 1.25, 75.0)),
    ("gpt-daybreak-blue-latest", e(12.50, 1.25, 75.0)),
    ("gpt-daybreak-red-latest", e(12.50, 1.25, 75.0)),
    ("gpt-5.5", e(5.0, 0.50, 30.0)),
    ("gpt-5.5-pro", e(30.0, 30.0, 180.0)),
    ("gpt-5.4", e(2.50, 0.25, 15.0)),
    ("gpt-5.4-mini", e(0.75, 0.075, 4.50)),
    ("gpt-5.4-nano", e(0.20, 0.02, 1.25)),
    ("gpt-5.2", e(1.75, 0.175, 14.0)),
    ("gpt-5.2-pro", e(21.0, 21.0, 168.0)),
    ("gpt-5.1", e(1.25, 0.125, 10.0)),
    ("gpt-5", e(1.25, 0.125, 10.0)),
    ("gpt-5-mini", e(0.25, 0.025, 2.0)),
    ("gpt-5-nano", e(0.05, 0.005, 0.40)),
    (FALLBACK_MATCH, e(1.25, 0.125, 10.0)),
];

pub struct PricingStore {
    path: PathBuf,
    overrides: RwLock<BTreeMap<String, PriceEntry>>,
}

impl PricingStore {
    pub fn path_for(accounts_dir: &Path) -> PathBuf {
        accounts_dir
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("pricing.json")
    }

    pub fn load(path: &Path) -> Self {
        let overrides: BTreeMap<String, PriceEntry> = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            overrides: RwLock::new(overrides),
        }
    }

    fn persist(&self, overrides: &BTreeMap<String, PriceEntry>) -> Result<(), String> {
        let text = serde_json::to_string_pretty(overrides).map_err(|e| e.to_string())?;
        crate::keys::write_restricted(&self.path, &text)
    }

    fn default_for(model: &str) -> Option<PriceEntry> {
        DEFAULTS
            .iter()
            .find(|(slug, _)| *slug == model)
            .map(|(_, entry)| *entry)
    }

    /// Override exact → default exact → override "*" → default "*".
    pub fn lookup(&self, model: &str) -> PriceEntry {
        let overrides = self.overrides.read().unwrap();
        overrides
            .get(model)
            .copied()
            .or_else(|| Self::default_for(model))
            .or_else(|| overrides.get(FALLBACK_MATCH).copied())
            .or_else(|| Self::default_for(FALLBACK_MATCH))
            .expect("DEFAULTS always carries a fallback entry")
    }

    pub fn cost_usd(
        &self,
        model: &str,
        uncached_input: u64,
        cached_input: u64,
        output: u64,
    ) -> f64 {
        let p = self.lookup(model);
        (uncached_input as f64 * p.input_per_m
            + cached_input as f64 * p.cached_input_per_m
            + output as f64 * p.output_per_m)
            / 1_000_000.0
    }

    /// Merged view for the admin UI: (match, effective entry, is_override).
    pub fn list(&self) -> Vec<(String, PriceEntry, bool)> {
        let overrides = self.overrides.read().unwrap();
        let mut out: Vec<(String, PriceEntry, bool)> = DEFAULTS
            .iter()
            .map(|(slug, default)| {
                if let Some(o) = overrides.get(*slug) {
                    (slug.to_string(), *o, true)
                } else {
                    (slug.to_string(), *default, false)
                }
            })
            .collect();
        for (slug, entry) in overrides.iter() {
            if DEFAULTS.iter().all(|(d, _)| d != slug) {
                out.push((slug.clone(), *entry, true));
            }
        }
        out
    }

    pub fn set_override(&self, model: &str, entry: PriceEntry) -> Result<(), String> {
        let model = model.trim();
        if model.is_empty() || model.len() > 64 || model.chars().any(char::is_whitespace) {
            return Err("模型名无效".to_string());
        }
        for v in [
            entry.input_per_m,
            entry.cached_input_per_m,
            entry.output_per_m,
        ] {
            if !(v.is_finite() && (0.0..=1000.0).contains(&v)) {
                return Err("价格需为 0-1000 之间的数字".to_string());
            }
        }
        let mut overrides = self.overrides.write().unwrap();
        overrides.insert(model.to_string(), entry);
        self.persist(&overrides)
    }

    pub fn remove_override(&self, model: &str) -> Result<bool, String> {
        let mut overrides = self.overrides.write().unwrap();
        let removed = overrides.remove(model).is_some();
        if removed {
            self.persist(&overrides)?;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_and_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let store = PricingStore::load(&dir.path().join("pricing.json"));
        assert_eq!(store.lookup("gpt-5.6-terra").input_per_m, 2.0);
        // Unknown slug falls back to "*".
        assert_eq!(store.lookup("gpt-future-x"), store.lookup("*"));
    }

    #[test]
    fn cost_math() {
        let dir = tempfile::tempdir().unwrap();
        let store = PricingStore::load(&dir.path().join("pricing.json"));
        // terra: 1M uncached in + 1M cached in + 1M out = 2.0 + 0.2 + 12.0
        let cost = store.cost_usd("gpt-5.6-terra", 1_000_000, 1_000_000, 1_000_000);
        assert!((cost - 14.2).abs() < 1e-9);
    }

    #[test]
    fn overrides_win_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        {
            let store = PricingStore::load(&path);
            store
                .set_override("gpt-5.6-terra", e(9.0, 9.0, 9.0))
                .unwrap();
            store.set_override("my-model", e(1.0, 0.1, 2.0)).unwrap();
            assert_eq!(store.lookup("gpt-5.6-terra").input_per_m, 9.0);
            assert_eq!(store.lookup("my-model").output_per_m, 2.0);
            assert!(store.set_override("bad model", e(1.0, 1.0, 1.0)).is_err());
            assert!(store.set_override("ok", e(-1.0, 1.0, 1.0)).is_err());
        }
        let store = PricingStore::load(&path);
        assert_eq!(store.lookup("gpt-5.6-terra").input_per_m, 9.0);
        store.remove_override("gpt-5.6-terra").unwrap();
        assert_eq!(store.lookup("gpt-5.6-terra").input_per_m, 2.0);
        assert!(!store.remove_override("gpt-5.6-terra").unwrap());
    }
}
