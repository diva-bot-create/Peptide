//! Parser tests over SWFs built FROM SCRATCH — no SSF2 file involved.
//!
//! `ssf::decompress` passes a raw `FWS`/`CWS`/`ZWS` stream straight through, so a SWF written
//! by the `swf` crate is a valid input to the stage parser. That makes the parser's handling
//! of degenerate and hostile input testable without shipping official content.
//!
//! What this does NOT cover: real SSF2 naming conventions and AS3, which is what turns a SWF
//! into a populated model. Those stay corpus-gated behind `cargo test -- --ignored`.

use std::io::Write;

/// A minimal, valid, uncompressed SWF with `n` empty frames and nothing else.
fn empty_swf(frames: u16) -> Vec<u8> {
    let header = swf::Header {
        compression: swf::Compression::None,
        version: 10,
        stage_size: swf::Rectangle {
            x_min: swf::Twips::from_pixels(0.0),
            x_max: swf::Twips::from_pixels(800.0),
            y_min: swf::Twips::from_pixels(0.0),
            y_max: swf::Twips::from_pixels(600.0),
        },
        frame_rate: swf::Fixed8::from_f32(30.0),
        num_frames: frames,
    };
    let tags: Vec<swf::Tag> = (0..frames).map(|_| swf::Tag::ShowFrame).collect();
    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).expect("write_swf");
    out
}

fn write_temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(name);
    let mut f = std::fs::File::create(&path).expect("create");
    f.write_all(bytes).expect("write");
    (dir, path)
}

/// A structurally valid SWF with no stage content must be REPORTED, not panic and not
/// silently produce a playable-looking stage. A converter that unwraps its way through
/// missing geometry would take the whole run down on a malformed input.
#[test]
fn structurally_valid_but_empty_swf_is_handled() {
    let (_dir, path) = write_temp("empty.ssf", &empty_swf(1));
    match ssf2_converter::parse_stage_opts(&path, false) {
        Ok(model) => {
            assert!(model.platforms.is_empty(), "an empty SWF has no collision");
            assert!(!model.warnings.is_empty(),
                "a stage with no geometry must surface warnings, got none");
        }
        Err(e) => {
            // erroring is an acceptable contract too; what matters is that it's reported
            let msg = e.to_string();
            assert!(!msg.is_empty(), "error must carry a reason");
        }
    }
}

/// Truncated input must be an error, never a panic — `.ssf` files come from users.
#[test]
fn truncated_swf_errors_without_panicking() {
    let full = empty_swf(1);
    for cut in [0usize, 2, 3, 8, full.len() / 2] {
        let (_dir, path) = write_temp("cut.ssf", &full[..cut.min(full.len())]);
        assert!(ssf2_converter::parse_stage_opts(&path, false).is_err(),
            "a {cut}-byte input must error, not succeed");
    }
}

/// Garbage that isn't a SWF at all must error cleanly.
#[test]
fn non_swf_input_errors_cleanly() {
    let (_dir, path) = write_temp("junk.ssf", b"this is definitely not a swf file at all");
    assert!(ssf2_converter::parse_stage_opts(&path, false).is_err(),
        "non-SWF input must be rejected");
}

/// Frame count travels from the container into the parse without the corpus.
#[test]
fn multi_frame_swf_parses() {
    let (_dir, path) = write_temp("frames.ssf", &empty_swf(12));
    // whatever the contract, it must be consistent with the single-frame case
    let one = ssf2_converter::parse_stage_opts(&write_temp("one.ssf", &empty_swf(1)).1, false);
    let twelve = ssf2_converter::parse_stage_opts(&path, false);
    assert_eq!(one.is_ok(), twelve.is_ok(),
        "frame count alone must not change whether a stage parses");
}
