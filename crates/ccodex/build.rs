//! Embeds the official models.json / base instructions into the binary and records the
//! upstream commit. Data comes straight from the vendored official source, so
//! instructions always match the locked version.
use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let vendor_root = manifest.join("../../vendor/codex");
    let models_manager = vendor_root.join("codex-rs/models-manager");
    let models_json = models_manager.join("models.json");
    let prompt_md = models_manager.join("prompt.md");

    if !models_json.exists() || !prompt_md.exists() {
        panic!(
            "官方源码尚未同步（缺少 {}）。请先运行: python tools/sync_upstream.py",
            models_json.display()
        );
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    fs::copy(&models_json, out_dir.join("models.json")).unwrap();
    fs::copy(&prompt_md, out_dir.join("base_instructions.md")).unwrap();

    // Upstream commit and release version: sync-script stamp first, then .git.
    let stamp = vendor_root.join(".ccodex-sync.json");
    let stamp_text = fs::read_to_string(&stamp).unwrap_or_default();
    let extract = |key: &str| -> Option<String> {
        let needle = format!("\"{key}\":");
        let start = stamp_text.find(&needle)? + needle.len();
        let rest = stamp_text[start..].trim_start().strip_prefix('"')?;
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    };
    let commit = extract("commit").unwrap_or_else(|| "unknown".to_string());
    let codex_version = extract("codex_version").unwrap_or_else(|| "0.0.0".to_string());

    println!("cargo:rustc-env=CCODEX_UPSTREAM_COMMIT={commit}");
    println!("cargo:rustc-env=CCODEX_BAKED_VERSION={codex_version}");
    println!("cargo:rerun-if-changed={}", models_json.display());
    println!("cargo:rerun-if-changed={}", prompt_md.display());
    println!("cargo:rerun-if-changed={}", stamp.display());
}
