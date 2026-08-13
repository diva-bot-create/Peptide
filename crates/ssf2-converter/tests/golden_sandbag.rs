//! End-to-end golden snapshot test for the `sandbag` character.
//!
//! Re-runs the converter against `../ssf2-ssfs/sandbag.ssf`, hashes every
//! text output (`*.hx`, `*.entity`, `*.json`, `*.meta`, `*.palettes`,
//! `*.fraytools`) with SHA-256, and diffs against the baseline hashes in
//! `tests/golden/sandbag_hashes.txt`.
//!
//! Binary outputs (sprite PNGs, palette_preview.png, .wav audio) are not
//! deterministically reproducible across machines (`image` crate version
//! drift, ffmpeg version drift) so they're excluded — the per-PNG `.meta`
//! sidecar GUIDs *are* part of the snapshot since they're deterministic.
//!
//! Skipped silently if `sandbag.ssf` isn't available — that lets a fresh
//! checkout on a machine without the SSF2 roster still pass `cargo test`.
//!
//! On a hash mismatch, prints a per-file diff so a developer can immediately
//! see which outputs changed.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

mod common;

fn sandbag_ssf_path() -> PathBuf {
    common::ssf("sandbag")
}

fn golden_hashes_path() -> PathBuf {
    manifest_dir().join("tests/golden/sandbag_hashes.txt")
}

/// Hash every text-output file under `dir` with SHA-256. Returns a map
/// keyed by repo-relative path (e.g. `./library/scripts/Character/Script.hx`)
/// → lowercase hex digest.
fn hash_text_outputs(dir: &Path) -> BTreeMap<String, String> {
    let exts: &[&str] = &["hx", "entity", "json", "meta", "palettes", "fraytools"];
    let mut out = BTreeMap::new();
    walk(dir, dir, exts, &mut out);
    out
}

fn walk(root: &Path, cur: &Path, exts: &[&str], out: &mut BTreeMap<String, String>) {
    let entries = match std::fs::read_dir(cur) {
        Ok(e) => e,
        Err(_) => return,
    };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_dir() {
            walk(root, &p, exts, out);
            continue;
        }
        let take = p.extension()
            .and_then(|e| e.to_str())
            .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e)))
            .unwrap_or(false);
        if !take { continue; }
        let bytes = match std::fs::read(&p) { Ok(b) => b, Err(_) => continue };
        let mut h = Sha256::new();
        h.update(&bytes);
        let digest = h.finalize();
        let rel = p.strip_prefix(root).unwrap_or(&p);
        // Match the shell `find . -exec shasum -a 256 {}` format: paths start with `./`.
        let key = format!("./{}", rel.display());
        out.insert(key, format!("{:x}", digest));
    }
}

/// Parse the baseline file. Each line is `<hex_digest>  <relative_path>`.
fn parse_golden(text: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        // shasum format: digest, two spaces, path.
        if let Some((digest, path)) = line.split_once("  ") {
            out.insert(path.to_string(), digest.to_string());
        }
    }
    out
}

/// Recursively delete a directory. Used for tempdir cleanup.
fn rm_rf(p: &Path) {
    let _ = std::fs::remove_dir_all(p);
}

// The ONE test in the default gate allowed to touch a real SSF2 file. Everything else that
// needs the corpus is `#[ignore]`d — see TESTING.md §0. This one earns its place because the
// golden hashes are the only thing that catches an unintended change to real converted output,
// and no synthetic fixture can stand in for that. It still skips cleanly with no corpus on
// disk, so a fresh checkout is unaffected.
#[test]
fn sandbag_conversion_matches_golden_hashes() {
    let ssf = sandbag_ssf_path();
    if !common::present(&ssf) { return; }
    let golden_path = golden_hashes_path();
    let golden_text = std::fs::read_to_string(&golden_path)
        .expect("must be able to read tests/golden/sandbag_hashes.txt");
    let golden = parse_golden(&golden_text);
    assert!(!golden.is_empty(),
        "golden hash file must contain at least one entry");

    // Shared with the other corpus tests: the conversion is deterministic for a given
    // converter build, so it runs once for the whole suite instead of once per binary.
    let Some(tempdir) = common::shared_conversion("sandbag") else { return };

    let out_root = tempdir.join("sandbag");
    let observed = hash_text_outputs(&out_root);

    // Diff per file.
    let mut missing: Vec<String> = Vec::new();   // in golden, not produced
    let mut extra:   Vec<String> = Vec::new();   // produced, not in golden
    let mut changed: Vec<String> = Vec::new();   // produced + in golden, different hash

    for (path, gold_hash) in &golden {
        match observed.get(path) {
            None => missing.push(path.clone()),
            Some(h) if h != gold_hash => changed.push(path.clone()),
            _ => {}
        }
    }
    for path in observed.keys() {
        if !golden.contains_key(path) { extra.push(path.clone()); }
    }

    // Clean up only on success; leave tempdir around on failure for inspection.
    if missing.is_empty() && extra.is_empty() && changed.is_empty() {
        rm_rf(&tempdir);
        return;
    }

    let mut msg = String::new();
    msg.push_str("sandbag conversion deviated from golden snapshot.\n\n");
    msg.push_str(&format!("Tempdir kept for inspection: {}\n\n", tempdir.display()));
    if !missing.is_empty() {
        msg.push_str(&format!("MISSING (in golden but not produced) [{}]:\n", missing.len()));
        for p in missing.iter().take(20) { msg.push_str(&format!("  {}\n", p)); }
        if missing.len() > 20 { msg.push_str(&format!("  ... and {} more\n", missing.len() - 20)); }
        msg.push('\n');
    }
    if !extra.is_empty() {
        msg.push_str(&format!("EXTRA (produced but not in golden) [{}]:\n", extra.len()));
        for p in extra.iter().take(20) { msg.push_str(&format!("  {}\n", p)); }
        if extra.len() > 20 { msg.push_str(&format!("  ... and {} more\n", extra.len() - 20)); }
        msg.push('\n');
    }
    if !changed.is_empty() {
        msg.push_str(&format!("CHANGED (hash differs) [{}]:\n", changed.len()));
        for p in changed.iter().take(20) { msg.push_str(&format!("  {}\n", p)); }
        if changed.len() > 20 { msg.push_str(&format!("  ... and {} more\n", changed.len() - 20)); }
        msg.push('\n');
    }
    msg.push_str("To accept the new output as the baseline, regenerate hashes:\n");
    msg.push_str("  cargo run -p peptide -- convert ../ssf2-ssfs/sandbag.ssf --output /tmp/sb && \\\n");
    msg.push_str("  cd /tmp/sb/sandbag && \\\n");
    msg.push_str("  find . -type f \\( -name '*.hx' -o -name '*.entity' -o -name '*.json' -o -name '*.meta' \\\n");
    msg.push_str("       -o -name '*.palettes' -o -name '*.fraytools' \\) \\\n");
    msg.push_str("    -exec shasum -a 256 {} \\; | sort -k 2 > tests/golden/sandbag_hashes.txt\n");
    panic!("{}", msg);
}
