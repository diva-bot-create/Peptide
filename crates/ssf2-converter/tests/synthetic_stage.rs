//! Stage emitter tests built from a SCRATCH stage model — no SSF2 file involved.
//!
//! The corpus tests that used to cover this had to parse a real SSF2 stage, which means
//! official McLeodGaming content that this repo can never ship. That made them
//! corpus-gated, slow, and unrunnable on a fresh checkout. Everything asserted here is a
//! property of the EMITTER (`emit_stage`), and the emitter takes a `StageModel` — so the
//! model is constructed by hand and the source format never enters into it.
//!
//! What a real stage parse still covers, and this deliberately doesn't: turning SWF shapes
//! and AS3 into that model. Those assertions live in the corpus-gated tests behind
//! `cargo test -- --ignored`.

use ssf2_converter::{emit_stage, Platform, Rect, SpawnPoint, StageModel};

/// The smallest stage the emitter will accept: one floor, four spawns, blast + camera boxes.
/// Deliberately art-free — `emit_stage` renders a placeholder when there's no art, which is
/// the cheap path and still exercises the full entity build.
fn synthetic_stage(id: &str) -> StageModel {
    let spawns = |face_left: bool| {
        (0..4)
            .map(|i| SpawnPoint { index: i, x: -150.0 + (i as f64) * 100.0, y: 0.0, face_left })
            .collect::<Vec<_>>()
    };
    StageModel {
        id: id.to_string(),
        display_name: "Synthetic".to_string(),
        platforms: vec![
            Platform { rect: Rect { x: -400.0, y: 0.0, w: 800.0, h: 40.0 }, drop_through: false, ..Default::default() },
            Platform { rect: Rect { x: -180.0, y: -220.0, w: 360.0, h: 20.0 }, drop_through: true, ..Default::default() },
        ],
        death_box: Some(Rect { x: -1200.0, y: -900.0, w: 2400.0, h: 2000.0 }),
        camera_box: Some(Rect { x: -700.0, y: -520.0, w: 1400.0, h: 900.0 }),
        entrances: spawns(false),
        respawns: spawns(false),
        // the parser always sets a real multiplier; Default leaves it 0.0
        scale: 1.3,
        ..Default::default()
    }
}

/// The emitted `.entity` must carry the layer stack a Fraymakers stage needs: all eleven
/// named depth containers in back-to-front order, plus collision, spawns and blast bounds.
/// This is the contract `emit_stage` bails on, so it's the one worth pinning.
#[test]
fn emitted_entity_has_the_fraymakers_stage_layer_stack() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = synthetic_stage("synthstage");
    let (dir, _fraytools) = emit_stage(&model, tmp.path()).expect("emit_stage");

    let entity_path = dir.join(format!("library/entities/{}.entity", model.id));
    let entity: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&entity_path).expect("read entity"))
            .expect("entity is json");
    let layers = entity["layers"].as_array().expect("layers array");

    let container_of = |l: &serde_json::Value| -> Option<String> {
        l.pointer("/pluginMetadata/com.fraymakers.FraymakersMetadata/containerType")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let containers: Vec<String> = layers.iter().filter_map(container_of).collect();

    // back-to-front: the order IS the depth model, so assert the sequence, not just presence
    let expected = [
        "BACKGROUND_BEHIND_CONTAINER", "BACKGROUND_EFFECTS_CONTAINER",
        "BACKGROUND_SHADOWS_CONTAINER", "BACKGROUND_STRUCTURES_CONTAINER",
        "CHARACTERS_BACK_CONTAINER", "CHARACTERS_CONTAINER", "CHARACTERS_FRONT_CONTAINER",
        "FOREGROUND_STRUCTURES_CONTAINER", "FOREGROUND_SHADOWS_CONTAINER",
        "FOREGROUND_EFFECTS_CONTAINER", "FOREGROUND_FRONT_CONTAINER",
    ];
    assert_eq!(containers, expected, "stage depth containers, back to front");

    let has_type = |t: &str| layers.iter().any(|l| l["type"].as_str() == Some(t));
    assert!(has_type("LINE_SEGMENT"), "a floor line segment must be emitted");
    assert!(has_type("COLLISION_BOX"), "blast zone / camera box must be emitted");
    assert!(has_type("POINT"), "entrance + respawn points must be emitted");
}

/// Every `"guid"` the emitter writes must be unique within the package. Duplicate GUIDs make
/// FrayTools silently drop content, and the ids are derived from the stage id + role, so a
/// scratch model exercises the same derivation a real one does.
#[test]
fn emitted_guids_are_unique() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (dir, _) = emit_stage(&synthetic_stage("guidstage"), tmp.path()).expect("emit_stage");

    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut dupes: Vec<String> = Vec::new();
    let mut stack = vec![dir.clone()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
            if !matches!(ext, "entity" | "json" | "meta" | "fraytools") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            let rel = p.strip_prefix(&dir).unwrap_or(&p).display().to_string();
            for chunk in text.split("\"guid\"").skip(1) {
                let Some(open) = chunk.find('"') else { continue };
                let rest = &chunk[open + 1..];
                let Some(close) = rest.find('"') else { continue };
                let guid = &rest[..close];
                if guid.len() < 8 {
                    continue;
                }
                if let Some(first) = seen.insert(guid.to_string(), rel.clone()) {
                    dupes.push(format!("{guid} in {first} and {rel}"));
                }
            }
        }
    }
    assert!(!seen.is_empty(), "expected the emitted package to contain GUIDs");
    assert!(dupes.is_empty(), "duplicate GUIDs:\n{}", dupes.join("\n"));
}

/// The package is named by the model's `id`, while the human-facing `display_name` travels
/// independently — so a caller can rename the package (the CLI suffixes `ssf2` onto the id so
/// it can't shadow a built-in stage, see `peptide ssf2`) without touching what players read.
#[test]
fn package_takes_the_model_id_and_display_name_travels_separately() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut model = synthetic_stage("battlefieldssf2");
    model.display_name = "Battlefield".to_string();
    let (dir, fraytools) = emit_stage(&model, tmp.path()).expect("emit_stage");

    assert_eq!(dir.file_name().unwrap().to_string_lossy(), "battlefieldssf2",
        "package dir is named by the model id");
    assert!(fraytools.exists(), "a .fraytools project must be emitted");

    let manifest = std::fs::read_to_string(dir.join("library/manifest.json")).expect("manifest");
    assert!(manifest.contains("Battlefield"),
        "the unsuffixed display name must survive into the manifest");
}

/// Emitting the SAME model twice in ONE process must produce byte-identical packages.
///
/// This is the property `inprocess_reuse` was written for — thread-local caches, the shared
/// conversion log, and the regex caches all live for the life of the process, so a second run
/// can silently inherit the first one's state. It needs no SSF2 file to check: the emitter is
/// the code that reads that state, and it takes a model.
///
/// Compares CONTENT, not paths, since the two runs land in different directories.
#[test]
fn emitting_twice_in_one_process_is_deterministic() {
    let hash_tree = |root: &std::path::Path| -> std::collections::BTreeMap<String, u64> {
        let mut out = std::collections::BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else { continue };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                let Ok(bytes) = std::fs::read(&p) else { continue };
                // FNV-1a: enough to catch a divergence, no dependency needed
                let mut h: u64 = 0xcbf29ce484222325;
                for b in &bytes {
                    h ^= *b as u64;
                    h = h.wrapping_mul(0x100000001b3);
                }
                let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
                out.insert(rel, h);
            }
        }
        out
    };

    let emit_once = |tag: &str| {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (dir, _) = emit_stage(&synthetic_stage("determinism"), tmp.path()).expect("emit_stage");
        let hashes = hash_tree(&dir);
        assert!(!hashes.is_empty(), "{tag}: emitted package should not be empty");
        hashes
    };

    let first = emit_once("first");
    let second = emit_once("second");

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
            "second in-process emit diverged from the first — process-global state leaking \
             across calls\n{}",
            diffs.join("\n")
        );
    }
}
