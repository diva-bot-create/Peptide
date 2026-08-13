//! Multi-character project emission tests
//! (docs/multi_character_projects_plan.md §2). Asserts the merged
//! project shape, collision suffix rule, per-character menu naming,
//! and project-level conversion log.

use ssf2_converter::{run_conversion, ConvertOptions};
use std::path::{Path, PathBuf};

mod common;

fn ssf_path(name: &str) -> PathBuf {
    common::ssf(name)
}
/// Run the converter in-process (was: spawn the removed `ssf2_converter` binary).
/// `extra` accepts the old CLI flags `--name <X>` and `--per-character-projects`.
fn run_converter(ssf: &Path, out: &Path, extra: &[&str]) {
    let mut opts = ConvertOptions::new(ssf);
    opts.output = out.to_path_buf();
    let mut it = extra.iter();
    while let Some(a) = it.next() {
        match *a {
            "--name" | "-n" => opts.name = it.next().map(|s| s.to_string()),
            "--per-character-projects" => opts.per_character_projects = true,
            _ => {}
        }
    }
    run_conversion(opts).expect("run_conversion");
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn zelda_ssf_emits_one_merged_project() {
    let ssf = ssf_path("zelda");
    if !common::present(&ssf) { return; }
    let Some(out) = common::shared_conversion("zelda") else { return };

    let project = out.join("zelda");
    assert!(project.exists(),                                "characters/zelda must exist");
    assert!(project.join("zelda.fraytools").exists(),        ".fraytools at project root");
    assert!(!out.join("sheik").exists(),              "sheik must NOT exist as a standalone project");
    assert!(!project.join("sheik.fraytools").exists(),       "sheik.fraytools must NOT exist");

    // Both characters in entities/ under <Pascal>.entity.
    let entities = project.join("library/entities");
    assert!(entities.join("Zelda.entity").exists(), "Zelda.entity missing");
    assert!(entities.join("Sheik.entity").exists(), "Sheik.entity missing");

    // Per-character menu entities at <Pascal>_Menu.entity (at least one
    // is expected since both have head sprites).
    let zelda_menu = entities.join("Zelda_Menu.entity").exists();
    let sheik_menu = entities.join("Sheik_Menu.entity").exists();
    assert!(zelda_menu || sheik_menu,
        "expected at least one <Pascal>_Menu.entity to exist");

    // Per-character scripts subdirs.
    for sub in &["Zelda", "Sheik"] {
        let dir = project.join(format!("library/scripts/{}", sub));
        for f in &["CharacterStats.hx", "AnimationStats.hx", "HitboxStats.hx", "Script.hx"] {
            assert!(dir.join(f).exists(),
                "{}/library/scripts/{}/{} must exist",
                project.display(), sub, f);
        }
    }

    // Collision suffix rule: Zelda (constructor-walk slot 0) keeps the
    // unsuffixed costumes.palettes; Sheik (slot 1) gets `2` on the BASE name
    // (`costumes2.palettes`), since a `.palettes2` extension is malformed.
    let lib = project.join("library");
    assert!(lib.join("costumes.palettes").exists(),           "slot 0 (zelda) costumes.palettes must exist");
    assert!(lib.join("costumes2.palettes").exists(),          "slot 1 (sheik) costumes2.palettes must exist");
    assert!(lib.join("costumes.palettes.meta").exists(),      "slot 0 .meta must exist");
    assert!(lib.join("costumes2.palettes.meta").exists(),     "slot 1 .meta must exist");
    assert!(!lib.join("costumes.palettes2").exists(),         "the old malformed .palettes2 name must NOT be emitted");
    // Palette previews are CHAR-PREFIXED (not palette_preview.png{N}) so the two
    // chars' previews don't collide on base filename in the shared sprites/ dir —
    // FrayTools derives a sprite GUID from its path, so a shared base name collided.
    let sprites = project.join("library/sprites");
    assert!(sprites.join("zelda_palette_preview.png").exists(), "zelda palette preview must exist");
    assert!(sprites.join("sheik_palette_preview.png").exists(), "sheik palette preview must exist");
    assert!(!sprites.join("palette_preview.png2").exists(),
        "the old malformed .png2 preview name must NOT be emitted");

    // Manifest has BOTH characters with distinct asset ids + per-char menu pointers.
    let manifest_raw = std::fs::read_to_string(lib.join("manifest.json")).unwrap();
    let m: serde_json::Value = serde_json::from_str(&manifest_raw).expect("manifest parses");
    let chars: Vec<&serde_json::Value> = m["content"].as_array().unwrap().iter()
        .filter(|c| c["type"] == "character").collect();
    assert_eq!(chars.len(), 2, "manifest must have two character entries; got {:?}", chars);
    let ids: Vec<&str> = chars.iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"zelda") && ids.contains(&"sheik"),
        "manifest characters must be zelda+sheik; got {:?}", ids);
    for c in &chars {
        let id = c["id"].as_str().unwrap();
        let expected_entity_id = format!("{}_menu", id);
        assert_eq!(c["metadata"]["ui"]["entityId"], expected_entity_id,
            "{}: entityId must be {:?}", id, expected_entity_id);
        assert_eq!(c["objectStatsId"], format!("{}CharacterStats", id),
            "{}: objectStatsId must be character-namespaced", id);
        assert_eq!(c["scriptId"], format!("{}Script", id),
            "{}: scriptId must be character-namespaced", id);
    }

    // Project-level conversion log with characters: [...] array.
    let log_raw = std::fs::read_to_string(project.join("conversion_log.json")).unwrap();
    let log: serde_json::Value = serde_json::from_str(&log_raw).expect("log parses");
    assert_eq!(log["project"], "zelda");
    let chars_in_log: Vec<&str> = log["characters"].as_array().unwrap().iter()
        .map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(chars_in_log, vec!["zelda", "sheik"]);
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn multi_char_project_uses_per_character_audio_subdirs() {
    // Stage C (docs/multi_character_projects_plan.md §3): in multi-char
    // projects library/audio/ becomes a folder per character. Single-char
    // projects keep the flat library/audio/*.wav layout (golden_sandbag).
    let ssf = ssf_path("zelda");
    if !common::present(&ssf) { return; }
    let Some(out) = common::shared_conversion("zelda") else { return };

    let audio = out.join("zelda/library/audio");
    assert!(audio.join("zelda").is_dir(), "library/audio/zelda/ subdir must exist");
    assert!(audio.join("sheik").is_dir(), "library/audio/sheik/ subdir must exist");

    // Each subdir must contain at least one .wav AND its .meta sidecar.
    for sub in &["zelda", "sheik"] {
        let entries: Vec<_> = std::fs::read_dir(audio.join(sub)).unwrap()
            .filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        let wavs:  usize = entries.iter().filter(|n| n.ends_with(".wav"))      .count();
        let metas: usize = entries.iter().filter(|n| n.ends_with(".wav.meta")) .count();
        assert!(wavs  >= 1, "library/audio/{}/ must contain at least one .wav",       sub);
        assert!(metas >= 1, "library/audio/{}/ must contain at least one .wav.meta",  sub);
    }

    // The flat layout (no subdir) must NOT exist for the multi-char project.
    let flat_wavs: Vec<_> = std::fs::read_dir(&audio).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".wav"))
        .collect();
    assert!(flat_wavs.is_empty(),
        "library/audio/ must NOT contain flat .wav files in multi-char mode; got {:?}",
        flat_wavs.iter().map(|e| e.file_name()).collect::<Vec<_>>());
}

#[test]
#[ignore = "needs the real SSF2 corpus (official content, never in this repo): not part of the default gate. Run with `cargo test -- --ignored`."]
fn single_char_project_keeps_flat_audio_layout() {
    // Stage C contract: golden_sandbag's flat library/audio/*.wav layout
    // is unchanged for single-character projects. Verified directly via
    // sandbag here so the contract is tested independently of the
    // golden-hash check.
    let ssf = ssf_path("sandbag");
    if !common::present(&ssf) { return; }
    let Some(out) = common::shared_conversion("sandbag") else { return };

    let audio = out.join("sandbag/library/audio");
    let entries: Vec<_> = std::fs::read_dir(&audio).unwrap()
        .filter_map(|e| e.ok()).map(|e| (e.file_type().unwrap(), e.file_name())).collect();
    // Should have .wav files directly, no per-char subdirs.
    let has_wav = entries.iter().any(|(_, n)| n.to_string_lossy().ends_with(".wav"));
    let has_subdir = entries.iter().any(|(t, _)| t.is_dir());
    assert!(has_wav,    "library/audio/*.wav must exist for single-char projects");
    assert!(!has_subdir,"library/audio/ must NOT have subdirs for single-char projects");
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn rollback_flag_emits_per_character_projects() {
    // --per-character-projects reverts to the pre-Stage-B layout: each
    // character gets its own .fraytools project. This is the rollback
    // escape hatch retained for one release per the plan.
    let ssf = ssf_path("zelda");
    if !common::present(&ssf) { return; }
    let out = tempfile::tempdir().expect("tempdir");
    run_converter(&ssf, out.path(), &["--per-character-projects"]);

    for char_id in &["zelda", "sheik"] {
        let dir = out.path().join(char_id);
        assert!(dir.exists(),                                       "{}/ must exist under rollback", char_id);
        assert!(dir.join(format!("{}.fraytools", char_id)).exists(),"{}.fraytools must exist under rollback", char_id);
        assert!(dir.join("library/manifest.json").exists(),         "{} manifest must exist under rollback", char_id);
        assert!(dir.join("conversion_log.json").exists(),           "{} conversion_log must exist under rollback", char_id);
    }
}
