//! Mario physics-stat parity guard. Locks in the movement stats that were verified
//! 1:1 against both the SSF2 ground-truth model (`peptide ssf2 stats mario.ssf`) and
//! the live Fraymakers engine (gravity/friction read off `physics`, walk-terminal off
//! `physics.currentVelocityX`). If a `stats.jsonc` multiplier or the scaling math drifts,
//! these break before the converter silently ships a Mario that moves differently.
//!
//! Skipped silently when the SSF2 corpus isn't on disk (matches `golden_sandbag.rs`).

use std::path::PathBuf;

mod common;

fn mario_ssf_path() -> PathBuf {
    common::ssf("mario")
}

/// Pull `field: <number>,` out of the generated CharacterStats.hx (tolerant of a
/// trailing `/*TODO*/` marker and whitespace). Returns the parsed f64.
fn stat(body: &str, field: &str) -> f64 {
    for line in body.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix(field) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(':') {
                // take everything up to the first ',' then strip a possible /*...*/
                let val = rest.split(',').next().unwrap_or("");
                let val = val.split("/*").next().unwrap_or("").trim();
                return val.parse::<f64>()
                    .unwrap_or_else(|_| panic!("CharacterStats.{field}: cannot parse {val:?}"));
            }
        }
    }
    panic!("CharacterStats.hx has no `{field}:` line");
}

#[test]
#[ignore = "full-conversion/art test: not part of the fast gate. Run with `cargo test -- --ignored`."]
fn mario_movement_stats_match_ssf2_ground_truth() {
    let ssf = mario_ssf_path();
    if !common::present(&ssf) { return; }

    let Some(out) = common::shared_conversion("mario") else { return };

    let cs = out.join("mario/library/scripts/Mario/CharacterStats.hx");
    let body = std::fs::read_to_string(&cs)
        .unwrap_or_else(|e| panic!("read {}: {e}", cs.display()));

    // (field, expected, tolerance). Derived from the SSF2 physics model at the
    // current size_multiplier (1.3). The 1:1 live-Fraymakers verification was done
    // at 1.9; every stat scales linearly with the knob, so these are the 1.9 values
    // times 1.3/1.9.
    let checks: &[(&str, f64, f64)] = &[
        ("gravity",          0.42,  0.02),
        ("shortHopSpeed",    6.17,  0.05),
        ("jumpSpeed",        11.31, 0.05),
        ("terminalVelocity", 8.45,  0.05),
        ("fastFallSpeed",    10.4,  0.05),
        ("friction",         0.34,  0.02),
        ("walkSpeedCap",     2.6,   0.05),
        ("dashSpeed",        7.15,  0.05),
    ];
    let mut errs: Vec<String> = Vec::new();
    for (field, expected, tol) in checks {
        let got = stat(&body, field);
        if (got - expected).abs() > *tol {
            errs.push(format!("  {field}: got {got}, expected {expected} (±{tol})"));
        }
    }
    assert!(errs.is_empty(),
        "mario physics drifted from the SSF2-verified ground truth:\n{}\n\
         if this is an intentional re-tune of stats.jsonc, update the expected values here.",
        errs.join("\n"));
}
