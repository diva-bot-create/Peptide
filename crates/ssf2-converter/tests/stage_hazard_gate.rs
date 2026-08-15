//! The hazards switch is a per-stage decision made in the SSF2 source, so the converter reads it
//! rather than applying a rule of its own. These pin that reading against real stages.
mod common;

use ssf2_converter::abc_parser;

/// The gate a stage's own class declares, or None when the corpus isn't present.
fn gate_for(stage: &str) -> Option<abc_parser::HazardGate> {
    let path = common::ssfs_dir().join("stages").join(format!("{stage}.ssf"));
    if !path.exists() { return None; }
    let swf_data = ssf2_converter::ssf::decompress(&std::fs::read(&path).ok()?).ok()?;
    let sw = ssf2_converter::swf_parser::parse(&swf_data).ok()?;
    Some(sw.abc_blocks.iter()
        .filter_map(|b| abc_parser::parse(b).ok())
        .map(|abc| abc_parser::extract_hazard_gate(&abc, stage))
        .find(|g| g.checked)
        .unwrap_or_default())
}

#[test]
fn reads_the_gated_class_out_of_the_stage() {
    let Some(gate) = gate_for("bowserscastle") else { return };
    assert!(gate.checked, "bowserscastle asks the switch in its update: {gate:?}");
    // The faller is spawned inside the branch; the lava and the embers are not.
    assert!(gate.gated_classes.iter().any(|c| c == "Thwomp"),
            "the spawned faller should be gated: {gate:?}");
    assert!(!gate.gated_classes.iter().any(|c| c.to_lowercase().contains("lava")),
            "the lava is placed regardless, so it is not gated: {gate:?}");
}

#[test]
fn a_stage_that_never_asks_gates_nothing() {
    let Some(gate) = gate_for("finaldestination") else { return };
    assert!(!gate.checked, "a stage with no hazards has nothing to ask about: {gate:?}");
    assert!(gate.gated_classes.is_empty());
}
