//! In-process reuse guard.
//!
//! `run_conversion` is called repeatedly by a long-running process (the Peptide
//! GUI), not just once per CLI invocation. The conversion log is a process-global
//! `Mutex` (reset per character) and some extractors use `thread_local!` caches.
//! This converts the same `.ssf` twice in one process and asserts the second run
//! produces a byte-identical tree to the first — i.e. no state leaks across runs.
//!
//! Skipped silently if `../../ssf2-ssfs/sandbag.ssf` isn't on disk.

use sha2::{Digest, Sha256};
use ssf2_converter::{run_conversion, ConvertOptions};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod common;

fn sandbag_ssf() -> PathBuf {
    common::ssf("sandbag")
}

/// SHA-256 every deterministic text output under `dir`, keyed by relative path.
fn hash_text_outputs(dir: &Path) -> BTreeMap<String, String> {
    let exts = ["hx", "entity", "json", "meta", "palettes", "fraytools"];
    let mut out = BTreeMap::new();
    fn walk(dir: &Path, root: &Path, exts: &[&str], out: &mut BTreeMap<String, String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() { walk(&p, root, exts, out); continue; }
            let take = p.extension().and_then(|x| x.to_str())
                .map(|x| exts.iter().any(|e| e.eq_ignore_ascii_case(x))).unwrap_or(false);
            if !take { continue; }
            let Ok(bytes) = std::fs::read(&p) else { continue };
            let mut h = Sha256::new();
            h.update(&bytes);
            let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
            out.insert(rel, format!("{:x}", h.finalize()));
        }
    }
    walk(dir, dir, &exts, &mut out);
    out
}

#[test]
#[ignore = "runs a full sandbag conversion TWICE by design: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn two_conversions_in_one_process_match() {
    let ssf = sandbag_ssf();
    if !common::present(&ssf) { return; }

    let convert_into = |tag: &str| -> BTreeMap<String, String> {
        let dir = std::env::temp_dir().join(format!("ssf2_reuse_{}_{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut opts = ConvertOptions::new(&ssf);
        opts.output = dir.clone();
        run_conversion(opts).expect("run_conversion");
        let hashes = hash_text_outputs(&dir.join("sandbag"));
        let _ = std::fs::remove_dir_all(&dir);
        hashes
    };

    let first = convert_into("a");
    let second = convert_into("b");

    assert!(!first.is_empty(), "expected text outputs from the first conversion");
    // Name the files that diverged rather than just asserting inequality: this failure is
    // INTERMITTENT, so a run that catches it has to leave behind enough to act on.
    if first != second {
        let mut diffs: Vec<String> = Vec::new();
        for (k, v) in &first {
            match second.get(k) {
                None => diffs.push(format!("MISSING in 2nd: {k}")),
                Some(v2) if v2 != v => diffs.push(format!("DIFFERS: {k}")),
                _ => {}
            }
        }
        for k in second.keys().filter(|k| !first.contains_key(*k)) {
            diffs.push(format!("EXTRA in 2nd: {k}"));
        }
        panic!("second in-process conversion diverged from the first — likely thread-local / \
                global state leaking across run_conversion calls\n{}", diffs.join("\n"));
    }
}
