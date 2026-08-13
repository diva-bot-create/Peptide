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
