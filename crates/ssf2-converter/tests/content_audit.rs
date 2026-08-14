//! The audit that would have caught a converted move drawing the character's revival platform:
//! a script asking for content the package does not ship.
use ssf2_converter::content_audit::{audit, content_ids};

fn write(p: &std::path::Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}

#[test]
fn ids_come_from_code_not_comments() {
    let ids = content_ids(
        "match.createVfx(new VfxStats({ spriteContent: self.getResource().getContent(\"effect_land\") }));\n\
         // TODO: attachEffect(\"global_bubbles\") not spawned: self.getResource().getContent(\"global_bubbles\")\n");
    assert_eq!(ids, vec!["effect_land".to_string()], "a commented-out call is not a reference");
}

#[test]
fn flags_an_id_the_package_does_not_ship() {
    let dir = std::env::temp_dir().join(format!("peptide_audit_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let lib = dir.join("library");
    write(&lib.join("manifest.json"), r#"{"content":[{"id":"sandbag"}]}"#);
    write(&lib.join("audio/real_sound.wav"), "RIFF");
    write(&lib.join("scripts/Script.hx"),
          "AudioClip.play(self.getResource().getContent(\"real_sound\"));\n\
           match.createVfx(new VfxStats({ spriteContent: self.getResource().getContent(\"effect_land\") }));\n");

    let found = audit(&lib);
    let ids: Vec<&str> = found.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, vec!["effect_land"], "should flag only the id nothing ships: {found:?}");
    assert_eq!(found[0].asked_by, "Script.hx");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn manifest_only_content_counts_as_shipped() {
    // structures exist ONLY as manifest entries -- no file of their own
    let dir = std::env::temp_dir().join(format!("peptide_audit_m_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let lib = dir.join("library");
    write(&lib.join("manifest.json"), r#"{"content":[{"id":"stage_platform0"}]}"#);
    write(&lib.join("scripts/S.hx"), "match.createStructure(self.getResource().getContent(\"stage_platform0\"));\n");
    assert!(audit(&lib).is_empty(), "a manifest-declared structure ships even with no asset file");
    let _ = std::fs::remove_dir_all(&dir);
}
