//! Tests for `classify_ssf` (character / stage / other discrimination).
//! No-op cleanly when the SSF2 corpus isn't present (fresh checkout).

use ssf2_converter::{classify_ssf, AssetKind};

mod common;

#[test]
#[ignore = "needs the real SSF2 corpus (official content, never in this repo): not part of the default gate. Run with `cargo test -- --ignored`."]
fn battlefield_classifies_as_stage() {
    // corpus stages live under <corpus>/stages/<id>.ssf (copied by `peptide ssf2 identify`).
    let bf = common::ssfs_dir().join("stages").join("battlefield.ssf");
    if !bf.exists() {
        eprintln!("skip: {} not present (no corpus)", bf.display());
        return;
    }
    let c = classify_ssf(&bf).expect("classify battlefield");
    assert_eq!(c.kind, AssetKind::Stage, "battlefield must classify as a stage (got {:?})", c.kind);
    assert!(c.markers.iter().any(|m| m.to_lowercase().contains("boundary")),
            "a stage must expose boundary markers; got {:?}", c.markers);
    assert_eq!(c.id.as_deref(), Some("battlefield"));
}

#[test]
#[ignore = "needs the real SSF2 corpus (official content, never in this repo): not part of the default gate. Run with `cargo test -- --ignored`."]
fn a_character_does_not_classify_as_stage() {
    // any named character .ssf in the corpus root must be a Character, never a Stage.
    let dir = common::ssfs_dir();
    if !dir.exists() { eprintln!("skip: no corpus"); return; }
    let Some(char_file) = std::fs::read_dir(&dir).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok().map(|e| e.path()))
          .find(|p| p.extension().and_then(|x| x.to_str()) == Some("ssf")
                 && p.file_stem().and_then(|s| s.to_str()).map(|s| !s.starts_with("DAT")).unwrap_or(false))
    }) else { eprintln!("skip: no named character .ssf in corpus"); return; };
    let c = classify_ssf(&char_file).expect("classify character");
    assert!(matches!(c.kind, AssetKind::Character(_)),
            "{} should classify as a character, got {:?}", char_file.display(), c.kind);
}
