//! vocab — the ONE place the `commands.hsx` vocabulary is declared and where the
//! Fraymakers↔SSF2 differences are reconciled. This sits at the interpreter
//! level (a sibling of [`crate::interpreter`]), ABOVE either engine backend, so a
//! command's meaning is defined once and both engines agree on it:
//!
//!   * Fraymakers ([`crate::debug_target::FraymakersTarget`]) forwards the names
//!     verbatim as hscript; `commands.hsx`, loaded into the engine interpreter,
//!     is what DEFINES `match` / `getCharacters()` / `p0` / `.body.x` there.
//!   * SSF2 ([`crate::ssf2_target::Ssf2Target`]) has no script interpreter, so it
//!     LOWERS the same names to AVM2 reflection verbs using the tables here.
//!
//! Because the mapping lives here and not inside the SSF2 backend, adding a
//! command or renaming a field is a one-line change that keeps both engines in
//! lockstep — the same property `commands.hsx` calls `getStateName()` resolves to
//! `GET State` on SSF2 from a single table, never a divergent hand-written arm.

/// A `commands.hsx` root identifier and the SSF2 reflection verbs that navigate
/// the engine's `peptideCur` register to the equivalent object. `names[0]` is the
/// canonical spelling; the rest are accepted aliases (matched case-insensitively).
pub struct Root {
    pub names: &'static [&'static str],
    /// SSF2 verb sequence (tab-encoded, e.g. `"GET\tstageData"`).
    pub ssf2: &'static [&'static str],
}

/// The roots `commands.hsx` binds per eval (`match`, `p0`..`p3`) plus the engine
/// singletons the SSF2 reflection bridge exposes (`gc`/`rm`/`stats`/`root`).
/// On Fraymakers these are hscript globals; on SSF2 each lowers to the verb chain
/// that reaches the same live object (the per-player character node in the live match, …).
pub const ROOTS: &[Root] = &[
    Root { names: &["match"],                  ssf2: &["GC", "GET\tstageData"] },
    Root { names: &["p0", "self"],             ssf2: &["GC", "GET\tstageData", "GET\tCharacters", "IDX\t0"] },
    Root { names: &["p1"],                     ssf2: &["GC", "GET\tstageData", "GET\tCharacters", "IDX\t1"] },
    Root { names: &["p2"],                     ssf2: &["GC", "GET\tstageData", "GET\tCharacters", "IDX\t2"] },
    Root { names: &["p3"],                     ssf2: &["GC", "GET\tstageData", "GET\tCharacters", "IDX\t3"] },
    Root { names: &["gamecontroller", "gc"],   ssf2: &["GC"] },
    Root { names: &["resourcemanager", "rm"],  ssf2: &["RM"] },
    Root { names: &["menucontroller", "mc"],   ssf2: &["MC"] },
    Root { names: &["stats"],                  ssf2: &["STATS"] },
    Root { names: &["root", "main", "this"],   ssf2: &["ROOT"] },
];

/// Fraymakers passthrough wrappers. In FM a character exposes `.body.x` and
/// `.physics.currentVelocityX`; SSF2 reads the field straight off the character,
/// so these wrapper members are skipped and the NEXT member is aliased instead.
pub const PASSTHROUGH: &[&str] = &["body", "physics"];

/// Fraymakers field name → SSF2 field name (the engines spell the same concept
/// differently). Folded after a [`PASSTHROUGH`] wrapper or on a bare member.
pub const MEMBER_ALIASES: &[(&str, &str)] = &[
    ("currentVelocityX", "XSpeed"),
    ("currentVelocityY", "YSpeed"),
    ("x", "X"),
    ("y", "Y"),
];

/// How a `commands.hsx` method call lowers to SSF2 reflection verbs.
pub enum CallLowering {
    /// A fixed verb sequence, ignoring args (e.g. `getCharacters()` → `GET Characters`).
    Ops(&'static [&'static str]),
    /// The verbs, then an `IDX <n>` from the first numeric arg (e.g. `getCharacter(0)`).
    OpsThenIndex(&'static [&'static str]),
}

/// A named `commands.hsx`/Fraymakers method and its SSF2 lowering.
pub struct CallSpec {
    pub names: &'static [&'static str],
    pub lowering: CallLowering,
}

/// The character-access methods `commands.hsx` defines (`getCharacters`,
/// `getCharacter`, `characterCount`) plus the FM character method `getStateName`.
/// `characterCount()` has no SSF2 `LEN` verb but AVM2 arrays expose `.length`, so
/// it lowers to `GET Characters` then `GET length`.
pub const CALLS: &[CallSpec] = &[
    CallSpec { names: &["getCharacters"],            lowering: CallLowering::Ops(&["GET\tCharacters"]) },
    CallSpec { names: &["getCharacter", "getPlayer"], lowering: CallLowering::OpsThenIndex(&["GET\tCharacters"]) },
    CallSpec { names: &["getStateName", "getState"],  lowering: CallLowering::Ops(&["GET\tState"]) },
    CallSpec { names: &["characterCount"],            lowering: CallLowering::Ops(&["GET\tCharacters", "GET\tlength"]) },
    // Read getters the debug macros (info/kill) call. FM spells these as methods
    // (getX/getY/getTeam); SSF2 reads the same concept off the character as a field.
    CallSpec { names: &["getX"],                      lowering: CallLowering::Ops(&["GET\tX"]) },
    CallSpec { names: &["getY"],                      lowering: CallLowering::Ops(&["GET\tY"]) },
    CallSpec { names: &["getTeam"],                   lowering: CallLowering::Ops(&["GET\tTeam"]) },
    CallSpec { names: &["getXSpeed", "getXVelocity"], lowering: CallLowering::Ops(&["GET\tXSpeed"]) },
    CallSpec { names: &["getYSpeed", "getYVelocity"], lowering: CallLowering::Ops(&["GET\tYSpeed"]) },
];

/// The `damage._damage` idiom. FM reads/writes a character's damage percent through
/// the `damage` wrapper's `_damage` field; SSF2 exposes it as getter/setter methods.
/// So `p.damage._damage` lowers to `getDamage()` (read) / `setDamage(v)` (write).
pub const DAMAGE_GETTER: &str = "getDamage";
pub const DAMAGE_SETTER: &str = "setDamage";

/// Fraymakers position/velocity SETTER method → the SSF2 property it writes. FM
/// exposes setX/setY/setXVelocity/setYVelocity as methods; SSF2 has no such methods,
/// but the same concept is a writable property (X/Y/XSpeed/YSpeed), so a setter call
/// lowers to a property SET. (`setXSpeed`/`setYSpeed` accepted as aliases.)
pub const SETTERS: &[(&str, &str)] = &[
    ("setX", "X"), ("setY", "Y"),
    ("setXVelocity", "XSpeed"), ("setYVelocity", "YSpeed"),
    ("setXSpeed", "XSpeed"), ("setYSpeed", "YSpeed"),
];

/// The SSF2 property a Fraymakers setter method writes, or `None` if `name` isn't a
/// position/velocity setter (then it's a plain method call).
pub fn setter_field(name: &str) -> Option<&'static str> {
    SETTERS.iter().find(|(k, _)| *k == name).map(|(_, v)| *v)
}

/// The method SSF2 uses to change a character's state (FM uses `toState`).
pub const STATE_SETTER: &str = "setState";

/// Fraymakers `CState` name → the SSF2 numeric `State` value. SSF2 changes state via
/// `setState(<n>)`; the neutral STAND state is 0. Accepts `CState.STAND` or `STAND`.
pub const CSTATES: &[(&str, i64)] = &[("STAND", 0)];

/// Map a `CState.<NAME>` (or bare `<NAME>`) to its SSF2 numeric state, or `None` if
/// unknown. A bare integer passes through as itself.
pub fn cstate_value(arg: &str) -> Option<i64> {
    let a = arg.trim();
    if let Ok(n) = a.parse::<i64>() { return Some(n); }
    let name = a.rsplit('.').next().unwrap_or(a);
    CSTATES.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| *v)
}

/// Host control bit (the [`crate::interpreter::CONTROLS`] layout the `hold`/`seq`
/// parser produces) → SSF2 `ControlsObject.controls` bit. The two engines number
/// their control bits differently, so the host mask is translated to SSF2's layout
/// before it goes on the wire (Fraymakers consumes the host layout verbatim, SSF2
/// needs this remap). SSF2 bits are `1 << pos` from `ControlsObject`'s static
/// constants (UP=1<<11, BUTTON1=1<<6, …); `action` maps to SSF2 GRAB.
const HOST_TO_SSF2_BITS: &[(u32, u32)] = &[
    (0x01, 1 << 11), // up    → UP
    (0x02, 1 << 10), // down  → DOWN
    (0x04, 1 << 9),  // left  → LEFT
    (0x08, 1 << 8),  // right → RIGHT
    // BUTTON1/BUTTON2 are NOT A/B in that order. Verified live with the engine-side
    // frame recorder: driving BUTTON1 (1<<6) plays mario's `b` animation and BUTTON2
    // (1<<5) plays `a`, so BUTTON1 is the special button and BUTTON2 is attack. The bit
    // positions themselves match ControlsObject's constants; it's the naming that reads
    // backwards. Mapping them the other way silently pressed the wrong button on every
    // SSF2 input test.
    (0x10, 1 << 5),  // attack → BUTTON2 (SSF2's A)
    (0x20, 1 << 6),  // special→ BUTTON1 (SSF2's B)
    (0x40, 1 << 4),  // action→ GRAB
    (0x80, 1 << 7),  // jump  → JUMP
    // Found the same way as the buttons above: drive one bit, see which animation the character
    // plays. Bit 1 puts it in DEFEND, and with a direction gives DODGEROLL / SIDESTEP, so it is
    // the shield. Bit 2 is the taunt. Without these, `shield+right` reached SSF2 as a bare `right`
    // and the character walked -- every dodge, roll and shield row in a cross-engine sweep was
    // comparing a roll against a walk.
    (0x100, 1 << 1), // shield → DEFEND
    (0x800, 1 << 2), // taunt  → TAUNT
];

// SSF2 has no dash BUTTON: it dashes on a double-tap of the direction, where Fraymakers has a
// dedicated one. Anything driving both engines has to say that twice; `move_sweep.py` carries the
// two idioms per move.

/// Translate a host control mask ([`crate::interpreter::controls_mask`] output) to
/// the SSF2 `ControlsObject.controls` bitmask the engine-side input applicator
/// writes each frame. Unknown high bits are dropped (only the 8 mapped controls
/// cross the seam).
pub fn fm_mask_to_ssf2(mask: u32) -> u32 {
    HOST_TO_SSF2_BITS.iter().fold(0, |acc, (host, ssf2)| {
        if mask & host != 0 { acc | ssf2 } else { acc }
    })
}

/// SSF2 verbs for a root identifier (case-insensitive), or `None` to fall back to
/// a generic member read off the document root.
pub fn root_ssf2(name: &str) -> Option<&'static [&'static str]> {
    let l = name.to_ascii_lowercase();
    ROOTS.iter().find(|r| r.names.iter().any(|n| *n == l)).map(|r| r.ssf2)
}

/// Map a Fraymakers field name to its SSF2 spelling (identity if no alias).
pub fn member_alias(name: &str) -> &str {
    MEMBER_ALIASES.iter().find(|(k, _)| *k == name).map(|(_, v)| *v).unwrap_or(name)
}

/// Is this a Fraymakers passthrough wrapper (`body`/`physics`) to skip on SSF2?
pub fn is_passthrough(name: &str) -> bool {
    PASSTHROUGH.contains(&name)
}

/// The SSF2 lowering for a known `commands.hsx`/FM method call, or `None` for a
/// generic reflection call by name.
pub fn call_lowering(name: &str) -> Option<&'static CallLowering> {
    CALLS.iter().find(|c| c.names.contains(&name)).map(|c| &c.lowering)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_cover_commands_hsx_bindings() {
        assert_eq!(root_ssf2("match"), Some(["GC", "GET\tstageData"].as_slice()));
        assert_eq!(root_ssf2("MATCH"), Some(["GC", "GET\tstageData"].as_slice())); // case-insensitive
        assert_eq!(root_ssf2("p0"), root_ssf2("self"));
        assert_eq!(root_ssf2("p1").unwrap().last(), Some(&"IDX\t1"));
        // p0..p3 all resolve (SSF2 parity with the Fraymakers p0-p3 binding); each indexes
        // the matching live-character slot.
        assert_eq!(root_ssf2("p2").unwrap().last(), Some(&"IDX\t2"));
        assert_eq!(root_ssf2("p3").unwrap().last(), Some(&"IDX\t3"));
        assert!(root_ssf2("nonsense").is_none());
    }

    #[test]
    fn member_and_passthrough_match_commands_hsx() {
        assert_eq!(member_alias("currentVelocityX"), "XSpeed");
        assert_eq!(member_alias("x"), "X");
        assert_eq!(member_alias("damage"), "damage"); // identity for unmapped
        assert!(is_passthrough("body"));
        assert!(is_passthrough("physics"));
        assert!(!is_passthrough("damage"));
    }

    #[test]
    fn host_mask_translates_to_ssf2_bits() {
        // single controls land on the SSF2 ControlsObject bit positions
        assert_eq!(fm_mask_to_ssf2(0x08), 1 << 8);  // right → RIGHT
        assert_eq!(fm_mask_to_ssf2(0x80), 1 << 7);  // jump  → JUMP
        // BUTTON1 is SSF2's B and BUTTON2 its A — confirmed live via the frame recorder
        assert_eq!(fm_mask_to_ssf2(0x20), 1 << 6);  // special→BUTTON1
        assert_eq!(fm_mask_to_ssf2(0x10), 1 << 5);  // attack →BUTTON2
        // combos OR together (down+special)
        assert_eq!(fm_mask_to_ssf2(0x22), (1 << 10) | (1 << 6));
        assert_eq!(fm_mask_to_ssf2(0), 0);
    }

    #[test]
    fn call_lowering_covers_character_access() {
        assert!(matches!(call_lowering("getCharacters"), Some(CallLowering::Ops(_))));
        assert!(matches!(call_lowering("getCharacter"), Some(CallLowering::OpsThenIndex(_))));
        assert!(matches!(call_lowering("characterCount"), Some(CallLowering::Ops(_))));
        // read getters the info/kill macros use lower to the matching SSF2 field
        assert!(matches!(call_lowering("getX"), Some(CallLowering::Ops(_))));
        assert!(matches!(call_lowering("getTeam"), Some(CallLowering::Ops(_))));
        assert!(call_lowering("toState").is_none()); // a state change, not a read → handled specially
    }

    #[test]
    fn setters_lower_to_properties() {
        assert_eq!(setter_field("setX"), Some("X"));
        assert_eq!(setter_field("setXVelocity"), Some("XSpeed"));
        assert_eq!(setter_field("setYVelocity"), Some("YSpeed"));
        assert_eq!(setter_field("setDamage"), None); // a real method, not a property set
        assert_eq!(setter_field("toState"), None);
    }

    #[test]
    fn cstate_neutral_lowers_to_zero() {
        assert_eq!(cstate_value("CState.STAND"), Some(0));
        assert_eq!(cstate_value("STAND"), Some(0));
        assert_eq!(cstate_value("5"), Some(5)); // bare int passes through
        assert_eq!(cstate_value("CState.JAB"), None); // unmapped → caller declares the gap
    }
}
