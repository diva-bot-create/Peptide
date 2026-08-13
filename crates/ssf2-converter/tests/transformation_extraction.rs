//! Transformation-character extraction tests. Verifies that bowser.ssf
//! and wario.ssf emit both their normal forms AND their Final-Smash
//! transformations as separate character packages, with the appropriate
//! TODO banner + `ssf2_source` metadata.
//!
//! CORPUS-GATED: these parse real SSF2 characters, so they're `#[ignore]`d out of the default
//! test gate (official content never lives in this repo). Run with `cargo test -- --ignored`.

use std::path::{Path, PathBuf};

mod common;

fn ssf_path(name: &str) -> PathBuf {
    common::ssf(name)
}

/// Asserts a transformation character's package is present in the
/// multi-char project at `out/<parent>/`. `project_id` is the SSF stem
/// (= parent character id in all observed cases).
fn assert_transformation_package(out: &Path, project_id: &str, parent: &str, transformation: &str, pascal: &str, source_method: &str) {
    let project = out.join(project_id);
    // Stage B: scripts live at <project>/library/scripts/<Pascal>/ per
    // docs/multi_character_projects_plan.md §2.
    let dir = project.join(format!("library/scripts/{}", pascal));
    for f in &["CharacterStats.hx", "AnimationStats.hx", "HitboxStats.hx", "Script.hx"] {
        let p = dir.join(f);
        assert!(p.exists(), "{} should exist for {}", p.display(), transformation);
    }
    // Stage B: character entity at <project>/library/entities/<Pascal>.entity.
    let entity_path = project.join(format!("library/entities/{}.entity", pascal));
    assert!(entity_path.exists(), "{} should exist for {}", entity_path.display(), transformation);

    let stats = std::fs::read_to_string(dir.join("CharacterStats.hx")).unwrap();
    assert!(stats.contains("TRANSFORMATION FORM"),
        "{}'s CharacterStats.hx must carry the TODO transformation banner", transformation);
    assert!(stats.contains(parent),
        "{}'s banner must mention parent {:?}", transformation, parent);
    assert!(stats.contains(source_method),
        "{}'s banner must mention source method {:?}", transformation, source_method);

    // Stage B: the conversion log is project-scoped with a characters[]
    // array. The transformation's block lives inside that array.
    let log = std::fs::read_to_string(project.join("conversion_log.json")).unwrap();
    assert!(log.contains(transformation),
        "project conversion_log.json must reference the transformation character {:?}", transformation);
    assert!(log.contains(source_method),
        "project conversion_log.json must include source_method {:?}", source_method);
    assert!(log.contains(parent),
        "project conversion_log.json must reference parent {:?}", parent);
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn bowser_ssf_emits_bowser_and_gigabowser() {
    let ssf = ssf_path("bowser");
    if !common::present(&ssf) { return; }
    let Some(out) = common::shared_conversion(ssf.file_stem().unwrap().to_str().unwrap()) else { return };

    // Stage B: ONE project per multi-char SSF; gigabowser lives inside it.
    let project = &out.join("bowser");
    assert!(project.exists(), "characters/bowser project must exist");
    assert!(project.join("bowser.fraytools").exists(),
        "project must have one bowser.fraytools (not per-character)");
    assert!(!&out.join("gigabowser").exists(),
        "characters/gigabowser must NOT exist as a standalone project");

    // Parent does NOT have the transformation banner / ssf2_source.
    let parent_stats = std::fs::read_to_string(
        project.join("library/scripts/Bowser/CharacterStats.hx")).unwrap();
    assert!(!parent_stats.contains("TRANSFORMATION FORM"),
        "Bowser (parent) must not carry the TODO banner");

    // Transformation does (inside the shared project).
    assert_transformation_package(&out, "bowser", "bowser", "gigabowser", "GigaBowser", "Main::getGigaBowser");

    // Differentiating data: Giga's projectile pipeline must produce
    // GigaFireBreath{,Blue,Purple} stat/hitbox files — these come from
    // getGigaBowser's bundle's pData, NOT from getBowser's
    // fireBreath{,Blue,Purple}. Scripts share the project's
    // library/scripts/Projectile/ dir.
    let giga_proj_dir = project.join("library/scripts/Projectile");
    assert!(giga_proj_dir.join("GigaFireBreathHitboxStats.hx").exists(),
        "Giga's GigaFireBreathHitboxStats.hx must exist");
    assert!(giga_proj_dir.join("GigaFireBreathBlueHitboxStats.hx").exists(),
        "Giga's GigaFireBreathBlueHitboxStats.hx must exist");
    assert!(giga_proj_dir.join("GigaFireBreathPurpleHitboxStats.hx").exists(),
        "Giga's GigaFireBreathPurpleHitboxStats.hx must exist");
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn wario_ssf_emits_wario_and_wario_man() {
    let ssf = ssf_path("wario");
    if !common::present(&ssf) { return; }
    let Some(out) = common::shared_conversion(ssf.file_stem().unwrap().to_str().unwrap()) else { return };

    let project = &out.join("wario");
    assert!(project.exists(), "characters/wario project must exist");
    assert!(project.join("wario.fraytools").exists(),
        "project must have one wario.fraytools (not per-character)");
    assert!(!&out.join("wario_man").exists(),
        "characters/wario_man must NOT exist as a standalone project");

    let parent_stats = std::fs::read_to_string(
        project.join("library/scripts/Wario/CharacterStats.hx")).unwrap();
    assert!(!parent_stats.contains("TRANSFORMATION FORM"),
        "Wario (parent) must not carry the TODO banner");

    assert_transformation_package(&out, "wario", "wario", "wario_man", "WarioMan", "Main::getWario_Man");
}
