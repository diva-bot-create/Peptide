//! GUID-conflict guard.
//!
//! Every FrayTools object the converter emits gets a deterministic GUID from
//! `det_uuid("{char_id}::{context}")`. Two objects colliding on the same
//! `context` (or a missing `char_id` namespace) would assign one GUID to two
//! different definitions — which corrupts the project (and, in a merged
//! multi-character `.fraytools`, silently cross-links unrelated entities).
//!
//! This converts sandbag and asserts every `"guid": "..."` value in the text
//! output is unique. A manual sweep of 12 characters (~15k GUIDs) found zero
//! collisions; this locks that in so a future codegen change that reuses a
//! context seed fails loudly.
//!
//! Skipped silently if `../ssf2-ssfs/sandbag.ssf` isn't on disk.

use std::collections::HashMap;
use std::path::Path;

mod common;

fn collect_guids(dir: &Path, out: &mut Vec<(String, String)>) {
    let exts = ["hx", "entity", "json", "meta", "palettes", "fraytools"];
    let entries = match std::fs::read_dir(dir) { Ok(e) => e, Err(_) => return };
    for ent in entries.flatten() {
        let p = ent.path();
        if p.is_dir() { collect_guids(&p, out); continue; }
        let take = p.extension().and_then(|e| e.to_str())
            .map(|e| exts.iter().any(|x| x.eq_ignore_ascii_case(e))).unwrap_or(false);
        if !take { continue; }
        let text = match std::fs::read_to_string(&p) { Ok(t) => t, Err(_) => continue };
        let rel = p.strip_prefix(dir).unwrap_or(&p).display().to_string();
        // Match `"guid": "<uuid>"` (the definition field; references use other keys).
        let mut rest = text.as_str();
        while let Some(i) = rest.find("\"guid\"") {
            rest = &rest[i + 6..];
            if let Some(c1) = rest.find('"') {
                let after = &rest[c1 + 1..];
                if let Some(c2) = after.find('"') {
                    out.push((after[..c2].to_string(), rel.clone()));
                    rest = &after[c2 + 1..];
                    continue;
                }
            }
            break;
        }
    }
}

#[test]
#[ignore = "needs the real SSF2 corpus (official content, never in this repo): not part of the default gate. Run with `cargo test -- --ignored`."]
fn sandbag_guids_are_unique() {
    // shared with the other corpus tests — one conversion serves the whole suite
    let Some(tempdir) = common::shared_conversion("sandbag") else { return };

    let mut guids: Vec<(String, String)> = Vec::new();
    collect_guids(&tempdir.join("sandbag"), &mut guids);

    assert!(!guids.is_empty(), "expected to find GUIDs in converted output");

    // Build guid -> files it appears in; flag any used more than once.
    let mut seen: HashMap<&str, Vec<&str>> = HashMap::new();
    for (g, f) in &guids { seen.entry(g.as_str()).or_default().push(f.as_str()); }
    let dupes: Vec<_> = seen.iter().filter(|(_, v)| v.len() > 1).collect();
    assert!(dupes.is_empty(),
        "duplicate GUID definitions found (conflict):\n{}",
        dupes.iter().map(|(g, files)| format!("  {} in {:?}", g, files))
            .collect::<Vec<_>>().join("\n"));

    eprintln!("sandbag: {} unique GUID definitions, no conflicts", guids.len());
}
