//! Character-pipeline tests over a SWF built FROM SCRATCH — no SSF2 file involved.
//!
//! `run_conversion` with an explicit `name` skips character DETECTION (which needs a real
//! `Main` class in ABC bytecode) and converts whatever the SWF holds, so a hand-written SWF
//! drives the whole emit path: project layout, per-character scripts, manifest, entity,
//! conversion log. That's the shape most of the old corpus tests actually asserted on.
//!
//! What still needs the real corpus, and stays `#[ignore]`d: anything read out of AS3 —
//! character detection, `ssf2_source` provenance, stat extraction, transformation forms.
//! Synthesising valid ABC bytecode is a bigger lift than those assertions are worth.

use std::io::Write;
use std::path::{Path, PathBuf};

/// A structurally valid, uncompressed SWF with `frames` empty frames.
///
/// This is a real SWF written by the `swf` crate, not a fixture blob: `ssf::decompress`
/// passes a raw `FWS` stream straight through, so the converter sees it as it would any
/// input, and nothing copyrighted is involved.
fn synthetic_character_swf(frames: u16) -> Vec<u8> {
    let header = swf::Header {
        compression: swf::Compression::None,
        version: 10,
        stage_size: swf::Rectangle {
            x_min: swf::Twips::from_pixels(0.0),
            x_max: swf::Twips::from_pixels(400.0),
            y_min: swf::Twips::from_pixels(0.0),
            y_max: swf::Twips::from_pixels(400.0),
        },
        frame_rate: swf::Fixed8::from_f32(30.0),
        num_frames: frames,
    };
    let tags: Vec<swf::Tag> = (0..frames).map(|_| swf::Tag::ShowFrame).collect();
    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).expect("write_swf");
    out
}

/// Convert a synthetic character named `id` into a fresh temp dir. Returns the tempdir
/// (kept alive by the caller) and the emitted project root.
fn convert_synthetic(id: &str) -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
    let src = tempfile::tempdir().expect("tempdir");
    let ssf = src.path().join(format!("{id}.ssf"));
    std::fs::File::create(&ssf)
        .expect("create ssf")
        .write_all(&synthetic_character_swf(1))
        .expect("write ssf");

    let out = tempfile::tempdir().expect("tempdir");
    let mut opts = ssf2_converter::ConvertOptions::new(&ssf);
    opts.output = out.path().to_path_buf();
    // explicit name: skips detection, which is the only part that needs real AS3
    opts.name = Some(id.to_string());
    ssf2_converter::run_conversion(opts).expect("run_conversion");

    let project = out.path().join(id);
    (src, out, project)
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() { stack.push(p) } else { found.push(p) }
        }
    }
    found.sort();
    found
}

/// The emitted project must have the layout FrayTools expects: a `.fraytools` at the root,
/// a PascalCase entity, and the four per-character script files under `scripts/<Pascal>/`.
#[test]
fn emits_the_fraytools_project_layout() {
    let (_src, _out, project) = convert_synthetic("synthchar");

    assert!(project.join("synthchar.fraytools").exists(), ".fraytools at project root");
    assert!(project.join("library/manifest.json").exists(), "library manifest");
    assert!(project.join("library/entities/Synthchar.entity").exists(),
        "entity is PascalCase-named");

    let scripts = project.join("library/scripts/Synthchar");
    for f in ["CharacterStats.hx", "AnimationStats.hx", "HitboxStats.hx", "Script.hx"] {
        assert!(scripts.join(f).exists(), "scripts/Synthchar/{f} must exist");
    }
    // every emitted script needs its FrayTools sidecar or the import silently drops it
    for f in ["CharacterStats.hx", "AnimationStats.hx", "HitboxStats.hx", "Script.hx"] {
        assert!(scripts.join(format!("{f}.meta")).exists(), "{f}.meta sidecar must exist");
    }
}

/// The manifest must wire every content id the engine resolves at load time. A missing or
/// misspelled id here is a load failure in-game, not a compile error.
#[test]
fn manifest_wires_the_character_content_ids() {
    let (_src, _out, project) = convert_synthetic("synthchar");
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join("library/manifest.json")).unwrap())
            .expect("manifest is json");

    let entry = manifest["content"].as_array().expect("content array")
        .iter().find(|c| c["id"] == "synthchar").expect("an entry for the character");

    for (field, expected) in [
        ("animationStatsId", "synthcharAnimationStats"),
        ("hitboxStatsId", "synthcharHitboxStats"),
        ("costumesId", "synthcharCostumes"),
        ("aiId", "synthcharAi"),
    ] {
        assert_eq!(entry[field].as_str(), Some(expected), "manifest {field}");
    }
}

/// The conversion log is the user-facing record of what didn't port. It must name the
/// character and carry the warning channels even when there's nothing to report.
#[test]
fn writes_a_conversion_log_for_the_character() {
    let (_src, _out, project) = convert_synthetic("synthchar");
    let log: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(project.join("conversion_log.json")).unwrap())
            .expect("conversion_log is json");

    assert_eq!(log["character"].as_str(), Some("synthchar"));
    for channel in ["ssf2_only", "unknown", "validation_warnings"] {
        assert!(log[channel].is_array(), "log must carry a `{channel}` array");
    }
}

/// Every `"guid"` in the emitted package must be unique — duplicates make FrayTools drop
/// content silently.
#[test]
fn emitted_guids_are_unique() {
    let (_src, _out, project) = convert_synthetic("guidchar");

    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut dupes = Vec::new();
    for p in walk(&project) {
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
        if !matches!(ext, "entity" | "json" | "meta" | "fraytools" | "palettes") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let rel = p.strip_prefix(&project).unwrap_or(&p).display().to_string();
        for chunk in text.split("\"guid\"").skip(1) {
            let Some(open) = chunk.find('"') else { continue };
            let rest = &chunk[open + 1..];
            let Some(close) = rest.find('"') else { continue };
            let guid = &rest[..close];
            if guid.len() < 8 { continue }
            if let Some(first) = seen.insert(guid.to_string(), rel.clone()) {
                dupes.push(format!("{guid} in {first} and {rel}"));
            }
        }
    }
    assert!(!seen.is_empty(), "expected GUIDs in the emitted package");
    assert!(dupes.is_empty(), "duplicate GUIDs:\n{}", dupes.join("\n"));
}

/// Converting the SAME input TWICE in ONE process must produce byte-identical output.
///
/// This is the property `inprocess_reuse` existed for, on the character path where it
/// actually mattered: thread-local caches, the shared conversion log and the regex caches all
/// outlive a single call, so run two can inherit run one's state. It needs no SSF2 file —
/// the state is the converter's, not the input's.
#[test]
fn converting_twice_in_one_process_is_deterministic() {
    let hash_tree = |root: &Path| -> std::collections::BTreeMap<String, u64> {
        walk(root).into_iter().filter_map(|p| {
            let bytes = std::fs::read(&p).ok()?;
            let mut h: u64 = 0xcbf29ce484222325;
            for b in &bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x100000001b3);
            }
            Some((p.strip_prefix(root).ok()?.display().to_string(), h))
        }).collect()
    };

    let (_s1, _o1, p1) = convert_synthetic("determinism");
    let first = hash_tree(&p1);
    let (_s2, _o2, p2) = convert_synthetic("determinism");
    let second = hash_tree(&p2);

    assert!(!first.is_empty(), "expected output from the first conversion");
    if first != second {
        let mut diffs = Vec::new();
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
        panic!(
            "second in-process conversion diverged from the first — process-global state \
             leaking across run_conversion calls\n{}",
            diffs.join("\n")
        );
    }
}

/// A character id that isn't already PascalCase must still produce a valid entity + script
/// dir, since the Pascal form is derived, not taken from the file name.
#[test]
fn lowercase_id_is_pascal_cased_consistently() {
    let (_src, _out, project) = convert_synthetic("mrgameandwatch");
    assert!(project.join("library/entities/Mrgameandwatch.entity").exists(),
        "entity name is derived from the id");
    assert!(project.join("library/scripts/Mrgameandwatch/Script.hx").exists(),
        "script dir uses the same derived name as the entity");
}
