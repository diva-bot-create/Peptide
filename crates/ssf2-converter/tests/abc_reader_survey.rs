//! A survey, not a gate: where do the two ABC readers DISAGREE across real content?
//!
//! Run with `--ignored --nocapture`. Every difference is a place the hand-rolled walk read the
//! file differently from Ruffle's, which is worth looking at one by one rather than asserting on.

mod common;

use ssf2_converter::{abc_parser, abc_ruffle};

fn abc_blocks(swf_bytes: &[u8]) -> Vec<Vec<u8>> {
    let Ok(buf) = swf::decompress_swf(swf_bytes) else { return vec![] };
    let Ok(parsed) = swf::parse_swf(&buf) else { return vec![] };
    parsed.tags.iter().filter_map(|t| match t {
        swf::Tag::DoAbc2(a) => Some(a.data.to_vec()),
        _ => None,
    }).collect()
}

#[test]
#[ignore]
fn survey_reader_differences() {
    let dir = common::ssfs_dir();
    if !dir.exists() { eprintln!("no corpus; skipping"); return; }

    let stages: Vec<String> = ["battlefield", "battlefield2", "bombfactory", "butterbuilding",
        "bowserscastle", "crateria", "crystalsmash", "flatzoneplus", "fourside", "huecomundo"]
        .iter().map(|s| format!("stages/{s}.ssf")).collect();
    let chars: Vec<String> = ["sandbag", "mario", "kirby", "zelda", "bowser", "captainfalcon"]
        .iter().map(|s| format!("{s}.ssf")).collect();

    let (mut files, mut blocks, mut diffs) = (0, 0, 0);
    for rel in stages.iter().chain(chars.iter()) {
        let path = dir.join(rel);
        let Ok(raw) = std::fs::read(&path) else { continue };
        let Ok(swf_bytes) = ssf2_converter::ssf::decompress(&raw) else { continue };
        files += 1;
        for abc in abc_blocks(&swf_bytes) {
            let (h, r) = match (abc_parser::parse_by_hand(&abc), abc_ruffle::parse(&abc)) {
                (Ok(h), Ok(r)) => (h, r),
                (Err(e), Ok(_)) => { println!("{rel}: HAND FAILED, ruffle ok: {e}"); diffs += 1; continue }
                (Ok(_), Err(e)) => { println!("{rel}: RUFFLE FAILED, hand ok: {e}"); diffs += 1; continue }
                _ => continue,
            };
            blocks += 1;
            let mut note = |what: &str| { println!("{rel}: {what}"); diffs += 1; };

            if h.strings.len() != r.strings.len() {
                note(&format!("strings {} vs {}", h.strings.len(), r.strings.len()));
            }
            if h.classes.len() != r.classes.len() {
                note(&format!("classes {} vs {}", h.classes.len(), r.classes.len()));
            }
            if h.method_bodies.len() != r.method_bodies.len() {
                note(&format!("bodies {} vs {}", h.method_bodies.len(), r.method_bodies.len()));
            }
            for (a, b) in h.classes.iter().zip(r.classes.iter()) {
                if a.name != b.name { note(&format!("class name {:?} vs {:?}", a.name, b.name)); }
                if a.super_name != b.super_name {
                    note(&format!("{}: super {:?} vs {:?}", a.name, a.super_name, b.super_name));
                }
                let m = |ts: &[abc_parser::Trait]| ts.iter()
                    .map(|t| (t.name.clone(), t.kind)).collect::<Vec<_>>();
                if m(&a.instance_methods) != m(&b.instance_methods) {
                    note(&format!("{}: instance traits differ", a.name));
                }
            }
            for (a, b) in h.method_bodies.iter().zip(r.method_bodies.iter()) {
                if a.bytecode != b.bytecode {
                    note(&format!("body {} bytecode differs", a.method_idx));
                }
            }
        }
    }
    println!("\n=== {files} files, {blocks} ABC blocks, {diffs} differences ===");
}
