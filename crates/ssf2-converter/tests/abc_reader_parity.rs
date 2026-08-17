//! The two ABC readers must agree, on real files.
//!
//! `abc_parser` walks the bytes by hand; `abc_ruffle` maps Ruffle's parse into the same shape.
//! Swapping one for the other is only safe if they produce the same thing, and "the same thing"
//! has to be checked against actual content rather than reasoned about -- the pools are 1-based
//! with a reserved slot, the two readers keep that differently per pool, and an off-by-one there
//! renames every symbol in the file without failing anything.

mod common;

use ssf2_converter::{abc_parser, abc_ruffle};

/// Pull every ABC block out of a SWF.
fn abc_blocks(swf_bytes: &[u8]) -> Vec<Vec<u8>> {
    let Ok(buf) = swf::decompress_swf(swf_bytes) else { return vec![] };
    let Ok(parsed) = swf::parse_swf(&buf) else { return vec![] };
    parsed.tags.iter().filter_map(|t| match t {
        swf::Tag::DoAbc2(a) => Some(a.data.to_vec()),
        _ => None,
    }).collect()
}

#[test]
fn both_readers_agree_on_the_corpus() {
    let dir = common::ssfs_dir();
    if !dir.exists() {
        eprintln!("no SSF2 corpus at {}; skipping", dir.display());
        return;
    }
    let mut checked = 0;
    for name in ["sandbag", "mario", "kirby"] {
        let path = dir.join(format!("{name}.ssf"));
        let Ok(raw) = std::fs::read(&path) else { continue };
        let Ok(swf_bytes) = ssf2_converter::ssf::decompress(&raw) else { continue };
        for abc in abc_blocks(&swf_bytes) {
            let (Ok(hand), Ok(ruffle)) = (abc_parser::parse_by_hand(&abc), abc_ruffle::parse(&abc))
                else { continue };

            assert_eq!(hand.strings.len(), ruffle.strings.len(), "{name}: string pool size");
            assert_eq!(hand.strings, ruffle.strings, "{name}: string pool contents");
            assert_eq!(hand.ints, ruffle.ints, "{name}: int pool");
            assert_eq!(hand.uints, ruffle.uints, "{name}: uint pool");
            // index 0 is the reserved slot, filled with NaN by both -- and NaN is not equal to
            // itself, so compare the real entries and assert the pad separately
            assert!(hand.doubles[0].is_nan() && ruffle.doubles[0].is_nan(), "{name}: double pad");
            assert_eq!(hand.doubles[1..], ruffle.doubles[1..], "{name}: double pool");

            // multinames: the resolved NAME is what the crate reads, so compare that
            let names = |f: &abc_parser::AbcFile| -> Vec<String> {
                f.multinames.iter().map(|m| m.name.to_string()).collect()
            };
            assert_eq!(names(&hand), names(&ruffle), "{name}: multiname names");

            assert_eq!(hand.classes.len(), ruffle.classes.len(), "{name}: class count");
            for (a, b) in hand.classes.iter().zip(ruffle.classes.iter()) {
                assert_eq!(a.name, b.name, "{name}: class name");
                assert_eq!(a.super_name, b.super_name, "{name}: {} supertype", a.name);
                // the trait KIND is the thing that was wrong by hand, so compare kind by kind
                let sig = |ts: &[abc_parser::Trait]| -> Vec<(String, u8, u32)> {
                    ts.iter().map(|t| (t.name.clone(), t.kind, t.method_idx)).collect()
                };
                assert_eq!(sig(&a.instance_methods), sig(&b.instance_methods),
                    "{name}: {} instance traits", a.name);
            }

            assert_eq!(hand.method_bodies.len(), ruffle.method_bodies.len(), "{name}: body count");
            for (a, b) in hand.method_bodies.iter().zip(ruffle.method_bodies.iter()) {
                assert_eq!(a.method_idx, b.method_idx, "{name}: body method index");
                assert_eq!(a.bytecode, b.bytecode, "{name}: body bytecode");
            }
            checked += 1;
        }
    }
    assert!(checked > 0, "no ABC blocks were compared");
    eprintln!("compared {checked} ABC blocks");
}
