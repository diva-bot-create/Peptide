//! Tests for the constructor-walk detection
//! ([docs/constructor_walk_detection.md](docs/constructor_walk_detection.md)).
//!
//! - End-to-end across the corpus: every SSF's detected character set
//!   matches `Main`'s `register("characters", [...])` array.
//! - Metadata: `id` / `guid` extracted; `package_id` / `package_guid` /
//!   `source_method` appear in every character's `conversion_log.json`.
//! - misc.ssf (no `Main`): handled cleanly, no panic.
//! - Validation warnings emit for empty stats (we exercise this with a
//!   `--name foo-bogus` against a real SSF, which extracts nothing but
//!   shouldn't crash).
//!
//! Skipped silently if `../ssf2-ssfs/` isn't on disk.

use ssf2_converter::*;
use std::path::PathBuf;

mod common;

fn ssfs_dir() -> PathBuf {
    common::ssfs_dir()
}

#[test]
fn id_derivation_rule_matches_corpus_method_names() {
    // Cross-check the derive_id_from_bundle_method_name rule against
    // every observed method-name shape. Kept alongside the in-main.rs
    // `derive_id_from_getter` test for path 2 — they're the same rule;
    // when path 2 enumeration is deleted in a follow-up, this becomes
    // the canonical test.
    let cases: &[(&str, &str)] = &[
        ("getMario",         "mario"),
        ("getBandanaDee",    "bandanadee"),
        ("getCaptainFalcon", "captainfalcon"),
        ("getgameandwatch",  "gameandwatch"),
        ("getGigaBowser",    "gigabowser"),
        ("getWario_Man",     "wario_man"),
        ("getSheik",         "sheik"),
    ];
    for (m, expected) in cases {
        let got = abc_parser::derive_id_from_bundle_method_name(m);
        assert_eq!(got.as_deref(), Some(*expected),
            "derive_id_from_bundle_method_name({:?}) = {:?}, expected Some({:?})",
            m, got, expected);
    }
    assert_eq!(abc_parser::derive_id_from_bundle_method_name("get"),    None);
    assert_eq!(abc_parser::derive_id_from_bundle_method_name("init"),   None);
    assert_eq!(abc_parser::derive_id_from_bundle_method_name(""),       None);
}

#[test]
fn pascal_form_covers_every_corpus_shape() {
    // Drives entity filenames (`<Pascal>.entity`) and the scripts subdir
    // (`library/scripts/<Pascal>/`) per
    // docs/multi_character_projects_plan.md §1. Coverage is the same
    // 13 method-name shapes derive_id_from_bundle_method_name handles,
    // plus the fallback path (no `get` prefix) used by --name overrides
    // and filename fallbacks.
    let cases: &[(&str, &str)] = &[
        // Primary path: from Main::getX() method names.
        ("getMario",         "Mario"),
        ("getSandbag",       "Sandbag"),
        ("getSheik",         "Sheik"),
        ("getBandanaDee",    "BandanaDee"),
        ("getCaptainFalcon", "CaptainFalcon"),
        ("getChibiRobo",     "ChibiRobo"),
        ("getDonkeyKong",    "DonkeyKong"),
        ("getMegaMan",       "MegaMan"),
        ("getMetaKnight",    "MetaKnight"),
        ("getPacMan",        "PacMan"),
        ("getBlackMage",     "BlackMage"),
        ("getGigaBowser",    "GigaBowser"),
        ("getWario_Man",     "WarioMan"),
        ("getgameandwatch",  "Gameandwatch"),
        // Fallback path (no `get` prefix): act on the id directly.
        ("sandbag",          "Sandbag"),
        ("mario",            "Mario"),
        ("wario_man",        "Warioman"),  // fallback can't recover SSF2 case info
    ];
    for (input, expected) in cases {
        let got = abc_parser::pascal_form(input);
        assert_eq!(got, *expected,
            "pascal_form({:?}) = {:?}, expected {:?}", input, got, expected);
    }
    // Degenerate inputs return empty rather than crashing.
    assert_eq!(abc_parser::pascal_form("get"), "");
    assert_eq!(abc_parser::pascal_form(""),    "");
}

#[test]
fn corpus_constructor_walk_matches_path2_enumeration() {
    // For every .ssf in the corpus, the constructor walker's
    // declared-characters list should match the path 2 enumeration's
    // discovered character set 1:1. Doubles as the "no orphan get*"
    // gate — caught in CI if a future SSF2 build ships a dev-leftover.
    let dir = ssfs_dir();
    if !common::present(&dir) { return; }

    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "ssf").unwrap_or(false))
        .collect();
    files.sort();

    let mut mismatches: Vec<String> = Vec::new();
    let mut total_chars_via_constructor = 0;
    let mut total_no_main = 0;

    for path in &files {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let Ok(bytes) = std::fs::read(path) else { continue };
        let Ok(swf_bytes) = ssf::decompress(&bytes) else { continue };
        let Ok(swf) = swf_parser::parse(&swf_bytes) else { continue };

        for abc_bytes in &swf.abc_blocks {
            let Ok(abc) = abc_parser::parse(abc_bytes) else { continue };
            let md = abc_parser::extract_main_package_metadata(&abc);
            let main = abc.classes.iter().find(|c| c.name == "Main");

            match (md, main) {
                (Some(md), Some(main)) => {
                    let constructor_ids: Vec<&str> =
                        md.characters.iter().map(|(i, _)| i.as_str()).collect();
                    let enum_ids: Vec<String> = main.instance_methods.iter()
                        .filter_map(|t| t.name.strip_prefix("get")
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_lowercase()))
                        .collect();
                    if constructor_ids.iter().map(|s| s.to_string()).collect::<Vec<_>>() != enum_ids {
                        mismatches.push(format!(
                            "{}: constructor={:?} vs enumeration={:?}",
                            stem, constructor_ids, enum_ids));
                    }
                    total_chars_via_constructor += constructor_ids.len();

                    // id should match filename stem. a few corpus files ship under a
                    // placeholder filename (e.g. DAT0.ssf is luffy); the converter keys
                    // off Main.id, not the filename, so those are expected aliases.
                    let alias = matches!(stem.to_lowercase().as_str(), "dat0");
                    if let Some(id) = &md.id {
                        if !alias {
                            assert_eq!(id.to_lowercase(), stem.to_lowercase(),
                                "{}: Main.id {:?} disagrees with filename stem", stem, id);
                        }
                    } else {
                        mismatches.push(format!("{}: no Main.id", stem));
                    }
                    assert!(md.guid.is_some(), "{}: no Main.guid", stem);
                }
                (None, None) => { total_no_main += 1; }
                (md, main) => {
                    mismatches.push(format!(
                        "{}: md.is_some={} but main.is_some={}",
                        stem, md.is_some(), main.is_some()));
                }
            }
        }
    }

    eprintln!("constructor walk total characters: {}", total_chars_via_constructor);
    eprintln!("SSFs without Main: {}", total_no_main);
    if !mismatches.is_empty() {
        for m in &mismatches { eprintln!("  {}", m); }
        panic!("{} SSF(s) had constructor-vs-enumeration mismatches", mismatches.len());
    }
    assert!(total_chars_via_constructor >= 40,
        "corpus should yield ≥40 characters; got {}", total_chars_via_constructor);
}

#[test]
#[ignore = "needs the real SSF2 corpus (official content, never in this repo): not part of the default gate. Run with `cargo test -- --ignored`."]
fn package_metadata_lands_in_conversion_log() {
    // Run the converter against sandbag.ssf and assert ssf2_source has
    // package_id / package_guid / source_method.
    let sandbag = ssfs_dir().join("sandbag.ssf");
    if !common::present(&sandbag) { return; }
    let Some(out) = common::shared_conversion("sandbag") else { return };

    let log = std::fs::read_to_string(out.join("sandbag/conversion_log.json")).unwrap();
    assert!(log.contains("\"ssf2_source\""),
        "sandbag conversion_log.json must include ssf2_source");
    assert!(log.contains("\"package_id\": \"sandbag\""),
        "sandbag package_id must be \"sandbag\"");
    assert!(log.contains("\"source_method\": \"Main::getSandbag\""),
        "sandbag source_method must be \"Main::getSandbag\"");
    // guid present (some 32-hex-with-dashes string).
    assert!(log.contains("\"package_guid\""),
        "sandbag package_guid must be present");
    // sandbag is a peer character, not a transformation.
    assert!(!log.contains("parent_normal_stats_id"),
        "sandbag must NOT carry the transformation overlay");
}
