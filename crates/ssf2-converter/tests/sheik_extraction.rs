//! End-to-end Sheik extraction test. Sheik has no `SheikExt` class in
//! zelda.ssf — only a `Main::getSheik` bundle and her own `sheik`
//! MovieClip. Path 2 picks her up automatically.
//!
//! Asserts she gets a sane output package (non-empty CharacterStats /
//! AnimationStats / HitboxStats / Script). Skipped silently if the
//! corpus isn't on disk (matches `golden_sandbag.rs` pattern).

use std::path::PathBuf;

mod common;

fn zelda_ssf_path() -> PathBuf {
    common::ssf("zelda")
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn sheik_emits_full_package_from_zelda_ssf() {
    let ssf = zelda_ssf_path();
    if !common::present(&ssf) { return; }

    // Default (no --name) full SSF conversion, shared with the other corpus tests: Stage B
    // emits zelda+sheik into ONE project at characters/zelda/. The --name override would
    // route to single-character mode; we exercise the default multi-char path here.
    let Some(out) = common::shared_conversion("zelda") else { return };

    // Stage B: zelda.ssf → ONE characters/zelda/ project containing both
    // characters. characters/sheik/ does NOT exist.
    let project = out.join("zelda");
    assert!(project.exists(), "characters/zelda must exist (multi-char project)");
    assert!(project.join("zelda.fraytools").exists(), "project .fraytools missing");
    assert!(!out.join("sheik").exists(),
        "characters/sheik must NOT exist as a standalone project");

    // Sheik's scripts live at library/scripts/Sheik/ inside the project.
    let sheik = project.join("library/scripts/Sheik");
    for f in &["CharacterStats.hx", "AnimationStats.hx", "HitboxStats.hx", "Script.hx"] {
        let p = sheik.join(f);
        assert!(p.exists(), "expected {} to exist", p.display());
        let body = std::fs::read_to_string(&p).expect("read");
        assert!(body.len() > 100, "{} is suspiciously short ({} bytes)", f, body.len());
    }
    // Sheik's character entity at library/entities/Sheik.entity.
    let sheik_entity = project.join("library/entities/Sheik.entity");
    assert!(sheik_entity.exists(), "expected {} to exist", sheik_entity.display());

    // Both characters' menu entities live at <Pascal>_Menu.entity per
    // multi-char convention.
    assert!(project.join("library/entities/Zelda_Menu.entity").exists()
            || project.join("library/entities/Sheik_Menu.entity").exists(),
        "at least one of the per-character Menu entities must exist (depending on head-sprite detection)");

    // CharacterStats.hx must NOT carry the transformation banner — Sheik's
    // cData.normalStats_id is `sheik` (matches her derived id).
    let stats_body = std::fs::read_to_string(sheik.join("CharacterStats.hx")).unwrap();
    assert!(!stats_body.contains("TRANSFORMATION FORM"),
        "Sheik must not have the transformation TODO banner");

    // Project-level conversion_log.json with characters: [...] array
    // (Stage B). Both zelda and sheik live in it. Sheik must NOT carry
    // the transformation overlay since her normalStats_id matches her id.
    let log = std::fs::read_to_string(project.join("conversion_log.json")).unwrap();
    assert!(log.contains("\"characters\""),
        "multi-char log must have a characters array");
    assert!(log.contains("\"zelda\""), "log must reference zelda");
    assert!(log.contains("\"sheik\""), "log must reference sheik");
    assert!(log.contains("Main::getSheik"),
        "log must include Main::getSheik for Sheik");
    assert!(log.contains("\"package_id\": \"zelda\""),
        "package_id is `zelda` — the SSF they ship in");
    assert!(!log.contains("parent_normal_stats_id"),
        "Neither zelda nor sheik are transformation forms");

    // HitboxStats.hx should mention one of Sheik's signature attacks
    // (needle / chain / lightarrow are her canonical moves).
    let hb = std::fs::read_to_string(sheik.join("HitboxStats.hx")).unwrap().to_lowercase();
    assert!(hb.contains("needle") || hb.contains("chain") || hb.contains("lightarrow")
            || hb.contains("vanish") || hb.contains("sheik"),
        "Sheik's HitboxStats.hx should reference at least one signature move");
}
