//! A speed written into a move has to be converted, not copied.
//!
//! `setXSpeed(17)` in an SSF2 move means "move 17 source pixels this frame", and neither half of
//! that means the same thing after conversion: the frame is one of sixty rather than one of
//! thirty, and the pixel belongs to a world scaled by `size_multiplier`. Copying the number
//! through leaves the move covering the right ground per frame across twice as many frames, so it
//! travels half again as far as it should. Measured live on a side special before this was fixed:
//! 2.0x the source distance where 1.3x is correct.

use ssf2_converter::mappings::character_stats;

fn scale() -> f64 { character_stats().scaling.velocity_scale() }

#[test]
fn an_authored_speed_is_converted_to_fraymakers_units() {
    let out = ssf2_converter::decompiler::render_call_for_test("setXSpeed", &["17"]);
    let want = 17.0 * scale();
    let got: f64 = out.trim_start_matches("self.setXSpeed(").trim_end_matches(')')
        .parse().expect("emits a number");
    assert!((got - want).abs() < 0.001, "setXSpeed(17) should emit {want}, got {out}");
}

#[test]
fn a_computed_speed_is_left_alone() {
    // Anything read back from the engine is already in Fraymakers units. Scaling it would shrink
    // it again on every frame the move applies it, which is worse than not scaling at all.
    let out = ssf2_converter::decompiler::render_call_for_test("setXSpeed", &["self.getXSpeed()"]);
    assert!(out.contains("getXSpeed()"), "a computed speed must pass through untouched: {out}");
    assert!(!out.contains('*'), "and must not be rescaled: {out}");
}

#[test]
fn zero_stays_zero() {
    let out = ssf2_converter::decompiler::render_call_for_test("setXSpeed", &["0"]);
    assert_eq!(out, "self.setXSpeed(0)", "a stop is a stop in either engine");
}
