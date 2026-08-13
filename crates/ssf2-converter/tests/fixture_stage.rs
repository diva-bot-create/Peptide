//! The physics fixture must parse back as the geometry it was authored with.
//!
//! This is the stage every parity measurement is meant to stand on, so the thing worth pinning is
//! that the numbers survive the round trip. A fixture that silently drifts is worse than no
//! fixture: measurements taken on it would look clean and mean nothing.
//!
//! Absolute coordinates are NOT asserted. The parser re-origins a stage (it subtracts an origin
//! and applies the 1.3 scale), so the invariants that matter are RELATIVE: spacings, spans and
//! offsets, each of which must come back multiplied by the scale and nothing else.

use ssf2_converter::test_fixture::{build_fixture_swf, FLOOR_HALF_W};

const SCALE: f64 = 1.3;

fn parse_fixture() -> ssf2_converter::StageModel {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("fixture.ssf");
    std::fs::write(&path, build_fixture_swf()).expect("write fixture");
    ssf2_converter::parse_stage(&path).expect("the fixture must parse")
}

#[test]
fn fixture_floor_spans_the_authored_width() {
    let m = parse_fixture();
    let floor = m.platforms.iter()
        .filter(|p| !p.drop_through)
        .max_by(|a, b| a.rect.w.total_cmp(&b.rect.w))
        .expect("the fixture has a solid floor");
    // authored -FLOOR_HALF_W..FLOOR_HALF_W, so the span is twice the half-width, scaled
    let want = FLOOR_HALF_W * 2.0 * SCALE;
    assert!((floor.rect.w - want).abs() < 1.0,
        "floor width {} should be the authored {want}", floor.rect.w);
}

#[test]
fn fixture_has_four_starts_and_four_respawns_evenly_spaced() {
    let m = parse_fixture();
    assert_eq!(m.entrances.len(), 4, "four start beacons (SSF2 numbers players from 1)");
    assert_eq!(m.respawns.len(), 4, "four respawn beacons");
    // authored 200px apart; spacing is the invariant, absolute position is re-origined
    for w in m.entrances.windows(2) {
        let gap = w[1].x - w[0].x;
        assert!((gap - 200.0 * SCALE).abs() < 1.0, "start spacing {gap} should be 200*scale");
    }
    // respawns sit 500px ABOVE the starts (y decreases upward), matching the real convention
    // where a respawn platform hangs over the stage.
    let rise = m.entrances[0].y - m.respawns[0].y;
    assert!((rise - 500.0 * SCALE).abs() < 1.0,
        "respawns should sit 500*scale above the starts, got {rise}");
}

#[test]
fn fixture_blast_box_is_big_enough_for_a_long_fall() {
    let m = parse_fixture();
    let death = m.death_box.expect("the fixture declares a blast box");
    // The point of the fixture is that a fall lasts long enough to measure per frame. SSF2 falls
    // at 30px/frame, so anything under a few hundred frames of drop is not worth having.
    let frames = (death.h / SCALE) / 30.0;
    assert!(frames > 150.0, "blast box gives only {frames:.0} frames of fall; want a long drop");
}

#[test]
fn fixture_ships_in_the_container_format_the_game_reads() {
    // The game only ever opens `DAT<n>.ssf` archives, so the packaged form has to survive the same
    // reader the rest of the corpus goes through and come back as the identical SWF.
    let packed = ssf2_converter::test_fixture::build_fixture_dat();
    assert_ne!(&packed[..3], b"FWS", "the packaged fixture must be a container, not a bare SWF");
    let inner = ssf2_converter::ssf::decompress(&packed).expect("the container must unwrap");
    assert_eq!(inner, build_fixture_swf(), "unwrapping must yield the fixture unchanged");
}

// ── the art fallback ─────────────────────────────────────────────────────────
//
// A stage that converts with no art of its own gets a placeholder drawn from its collision. The
// only thing that makes such a picture worth drawing is that it lands ON the collision, so these
// pin exactly that, plus the two ways it used to produce something unusable.

/// The rectangle a placeholder occupies once the emitter has placed it, in FM coordinates. The
/// emitter positions every stage image at `x, y` and scales its PIXELS by the stage scale, so this
/// is what a player actually sees.
fn placed_rect(art: &ssf2_converter::StageArt, scale: f64) -> (f64, f64, f64, f64) {
    (art.x, art.y, art.x + art.w as f64 * scale, art.y + art.h as f64 * scale)
}

fn model_with(platforms: Vec<ssf2_converter::Platform>) -> ssf2_converter::StageModel {
    let mut m = parse_fixture();
    m.platforms = platforms;
    m
}

fn flat(left: f64, top: f64, w: f64, h: f64, drop_through: bool) -> ssf2_converter::Platform {
    ssf2_converter::Platform {
        rect: ssf2_converter::Rect { x: left, y: top, w, h },
        drop_through, profile: None, moving: false, visible: true, hazard_floor: false,
    }
}

#[test]
fn placeholder_art_lands_on_the_collision_it_was_drawn_from() {
    // The failure this catches: the raster was measured in FM units while the emitter also scales
    // it, so the picture came out `scale` too wide and slid off its own floor. On screen that
    // reads as collision covering only part of the shape, which is a confusing way to be told
    // about a units bug.
    let m = model_with(vec![flat(-780.0, 520.0, 1560.0, 52.0, false)]);
    let art = ssf2_converter::render_placeholder_for_test(&m);
    let (l, t, r, b) = placed_rect(&art, m.scale);
    let margin = 8.0;
    assert!((l - (-780.0 - margin)).abs() < 2.0, "left edge {l} should sit on the collision");
    assert!((r - (780.0 + margin)).abs() < 2.0, "right edge {r} should sit on the collision");
    assert!((t - (520.0 - margin)).abs() < 2.0, "top edge {t} should sit on the collision");
    assert!((b - (572.0 + margin)).abs() < 2.0, "bottom edge {b} should sit on the collision");
}

#[test]
fn placeholder_draws_a_surface_thinner_than_one_pixel() {
    // A thin platform divided by the stage scale used to round to a zero-height span and vanish,
    // leaving collision a fighter stands on with nothing drawn under their feet.
    let m = model_with(vec![flat(-100.0, 0.0, 200.0, 1.0, true)]);
    let art = ssf2_converter::render_placeholder_for_test(&m);
    let img = image::load_from_memory(&art.png).expect("decodes").to_rgba8();
    let painted = img.pixels().filter(|p| p.0[3] > 0).count();
    assert!(painted > 0, "a thin surface must still be drawn, got an empty image");
}

#[test]
fn placeholder_with_no_collision_is_placed_somewhere_real() {
    // With nothing to measure, the bounds come back inverted from the empty folds and the image
    // lands at 1e308. Anything downstream that reads that is dealing with a number, not a stage.
    let m = model_with(vec![]);
    let art = ssf2_converter::render_placeholder_for_test(&m);
    assert!(art.x.is_finite() && art.y.is_finite(), "placed at ({}, {})", art.x, art.y);
    assert!(art.w >= 1 && art.h >= 1);
}
