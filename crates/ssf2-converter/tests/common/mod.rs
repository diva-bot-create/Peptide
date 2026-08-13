//! Shared test helper: locate the SSF2 `.ssf` corpus.
//!
//! The corpus is the developer's own SSF2 files. it's never committed (it's
//! McLeodGaming's copyrighted content, and `.gitignore` excludes `*.ssf`), so
//! tests that need it check `.exists()` and skip cleanly when it's absent. that
//! keeps a fresh checkout's `cargo build` and `cargo test` green for anyone.
//!
//! Resolution order:
//!   1. `$SSF2_SSFS_DIR` if set -- point this at wherever you keep the corpus.
//!   2. otherwise the sibling `ssf2-ssfs/` of the repo root (the corpus lives
//!      next to the repo, not inside it).

#![allow(dead_code)]

use std::path::PathBuf;

/// Single source of truth for the engine-internal symbol tokens that must not
/// leak into the tracked repo, for BOTH engines. Two trip-wires share it so a
/// new symbol updates both at once:
///   - `doc_freshness.rs :: no_engine_internals_in_tracked_docs` scans Markdown.
///   - `conventions.rs :: no_engine_internals_in_code_comments` scans `.rs` comments.
///
/// These are distinctive non-hscript engine/bundle symbols with no legitimate
/// place in a doc or a comment (outside the patcher source that IS the symbol
/// map's home: `src/main.rs`/`src/manifest.rs`/`src/bin/` for Fraymakers,
/// `abc_inject.rs` for the SSF2 AVM2 injector).
///
/// The SSF2 entries are the RUNNING-ENGINE internals RE'd from SSF2.swf (the live
/// match/menu object graph). SSF2 INPUT-SIDE symbols -- the `.ssf`/SWF format and
/// the character modding API the converter reads -- are intentionally NOT here;
/// that side is ours to name. See AGENT_CONTEXT.md "engine-side knowledge is not
/// in this repo" and CONTRIBUTING.md "special case".
pub const ENGINE_SYMBOL_NEEDLES: &[&str] = &[
    // Fraymakers / FrayTools (HashLink)
    "fraymakers.Main",
    "Main.onLoaded",
    "MatchController",
    "PXFResource",
    "getPXF", // covers getPXFResource + getPXFSpriteEntity
    "spawnPlayer",
    "cacheSpriteEntityData",
    "characterPxfContentMap",
    "ThreadTaskManager",
    "set_DataAsPxf",
    "fetchThreaded",
    "poolHash",
    "importManifest",
    "calculateAbsolutePivotPosition",
    "Tildebugger",
    "hxd.System",
    "launchScreen",
    "loadingScreenFactory",
    "FraymakersClassFactory",
    "queueRequiredResources",
    // SSF2 running-engine internals (RE'd from SSF2.swf)
    "GameController",
    "MenuController",
    "ResourceManager",
    "showInitialMenu",
    "disposeAllMenus",
    "disclaimerMenu",
    "loadingMenu",
    "queueResources",
];

/// Directory holding the `.ssf` corpus (see module docs for resolution order).
pub fn ssfs_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SSF2_SSFS_DIR") {
        return PathBuf::from(dir);
    }
    // CARGO_MANIFEST_DIR = <repo>/crates/ssf2-converter; the corpus is the repo
    // root's sibling, so go up three levels then into `ssf2-ssfs`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from(".."))
        .join("ssf2-ssfs")
}

/// Path to one corpus file by character id (no extension), e.g. `ssf("sandbag")`.
pub fn ssf(name: &str) -> PathBuf {
    ssfs_dir().join(format!("{name}.ssf"))
}

/// `true` if `path` exists. When it doesn't, prints a one-line, self-documenting
/// skip note (so anyone running `cargo test` learns how to point at the corpus)
/// and returns `false`. Use it as the skip guard: `if !common::present(&p) { return; }`.
pub fn present(path: &std::path::Path) -> bool {
    if path.exists() {
        return true;
    }
    eprintln!(
        "skip: SSF2 corpus not found at {} -- set $SSF2_SSFS_DIR or place your \
         SSF2 .ssf files at ../ssf2-ssfs/ (they're not committed; bring your own).",
        path.display()
    );
    false
}

/// A small, structurally DIVERSE sample of stages instead of all 110.
///
/// The point of the corpus tests is to catch a converter change that breaks a whole CLASS of
/// stage, and one stage of each class does that as well as thirty do — while a full sweep costs
/// minutes on every `cargo test`. Each entry is here because it is the representative of a
/// distinct structure, so removing one loses real coverage:
///
///   battlefield       plain static stage, no hazards / no moving parts (the baseline)
///   finaldestination  plain static, single-platform (no soft platforms)
///   towerofsalvation  moving platforms
///   crateria          overlapping collision that must survive the dedup pass
///   bowserscastle     hazards + animated backdrop elements + a baked foreground
///   centralhighway    ANIMATED foreground (the promote-to-vfx path)
///   clocktown         multi-layer camera-relative parallax backgrounds
///   junglehijinx      parallax + drop-through terrain
///
/// Set `PEPTIDE_TEST_FULL_CORPUS=1` to sweep all of them instead (what the corpus-wide
/// `just sweep-stages` does); a missing entry is skipped, so a partial corpus still runs.
pub const SAMPLE_STAGES: &[&str] = &[
    "battlefield", "finaldestination", "towerofsalvation", "crateria",
    "bowserscastle", "centralhighway", "clocktown", "junglehijinx",
];

/// The stage `.ssf` paths a corpus test should run over: the diverse sample by default,
/// every stage in the corpus when `PEPTIDE_TEST_FULL_CORPUS` is set. Entries that aren't
/// present on this machine are dropped, so a partial corpus never fails the test.
pub fn stage_sample() -> Vec<PathBuf> {
    let dir = ssfs_dir().join("stages");
    if std::env::var("PEPTIDE_TEST_FULL_CORPUS").is_ok() {
        let mut all: Vec<PathBuf> = std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ssf"))
            .collect();
        all.sort();
        return all;
    }
    SAMPLE_STAGES.iter().map(|n| dir.join(format!("{n}.ssf"))).filter(|p| p.exists()).collect()
}

/// Convert `name` ONCE per (corpus file, converter build) and share the result across every
/// test binary that needs it.
///
/// `cargo test` runs integration-test binaries SEQUENTIALLY, and eight of them each converted
/// the same character from scratch — so the suite paid the same multi-second conversion eight
/// times over and blew well past a usable "validate my build" runtime. A conversion is
/// deterministic for a given converter build, so it only has to happen once.
///
/// Freshness: the cache key is the newest mtime under the converter's `src/` and `mappings/`
/// plus the corpus file itself, so ANY converter or mapping change mints a new key and the
/// next test converts for real. A stale cache can't survive a code edit.
///
/// Concurrency: the conversion is written to a unique temp directory and moved into place with
/// a single rename, so binaries racing for the same character produce one winner and never a
/// half-written cache. Callers get a read-only view — a test that needs to mutate the output,
/// or that is specifically testing conversion itself (see `inprocess_reuse`), must keep running
/// its own conversion.
///
/// Returns `None` when the corpus file is absent, matching the `present()` skip convention.
pub fn shared_conversion(name: &str) -> Option<PathBuf> {
    use std::time::UNIX_EPOCH;

    let ssf = ssf(name);
    if !present(&ssf) {
        return None;
    }

    // newest mtime across the converter sources + mappings + this corpus file
    fn newest(dir: &std::path::Path, best: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                newest(&p, best);
            } else if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
                let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                *best = (*best).max(secs);
            }
        }
    }
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut stamp = 0u64;
    newest(&crate_dir.join("src"), &mut stamp);
    newest(&crate_dir.join("mappings"), &mut stamp);
    if let Ok(t) = std::fs::metadata(&ssf).and_then(|m| m.modified()) {
        stamp = stamp.max(t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    }

    // <target>/test-fixtures/<stamp>/<name>/ — under the build dir so `cargo clean` clears it
    let root = crate_dir
        .ancestors()
        .nth(2)
        .map(|p| p.join("build"))
        .unwrap_or_else(std::env::temp_dir)
        .join("test-fixtures")
        .join(stamp.to_string());
    let ready = root.join(name);
    if ready.join(name).exists() {
        return Some(ready);
    }

    let staging = root.join(format!(".staging-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).ok()?;
    let mut opts = ssf2_converter::ConvertOptions::new(&ssf);
    opts.output = staging.clone();
    ssf2_converter::run_conversion(opts).expect("shared_conversion: run_conversion");
    // another binary may have won the race; its result is equivalent, so ignore the error
    if std::fs::rename(&staging, &ready).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    Some(ready)
}

/// A stage model parsed WITHOUT art rendering, memoized per stage id for the binary's life.
///
/// Rendering a stage's art (rasterize + resize + composite every placed instance) is where
/// essentially all of a stage parse's time goes, and the geometry/hazard/platform assertions
/// these tests make don't look at art at all. Skipping the render turns a ~40s parse into a
/// fast one. A test that genuinely asserts on ART must parse it itself and be marked
/// `#[ignore]` — see the note in `stage_porting.rs`.
///
/// Memoized because `stage_porting` asks for battlefield four separate times. The model is
/// read-only here, so one parse serves them all. Returns `None` when the corpus file is
/// absent, matching the `present()` skip convention.
pub fn parsed_stage(name: &str) -> Option<std::sync::Arc<ssf2_converter::StageModel>> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Option<Arc<ssf2_converter::StageModel>>>>> =
        OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().unwrap().get(name) {
        return hit.clone();
    }
    let path = ssfs_dir().join("stages").join(format!("{name}.ssf"));
    let parsed = if present(&path) {
        Some(Arc::new(ssf2_converter::parse_stage_opts(&path, false).expect("parse_stage_opts")))
    } else {
        None
    };
    cache.lock().unwrap().insert(name.to_string(), parsed.clone());
    parsed
}
