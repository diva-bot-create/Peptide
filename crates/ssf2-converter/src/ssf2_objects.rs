//! Which of a package's classes are GAME OBJECTS, and what kind each one is.
//!
//! A stage that does anything has objects in it: a thwomp, a pool of lava, a fairy that heals you,
//! a balloon that hurts you. The question a port has to answer first is which classes those are,
//! and the answer is written down in the content: SSF2's api layer declares a hierarchy, every
//! package carries its own copy of it, and a stage's own objects extend it.
//!
//! ```text
//! SSF2BaseAPIObject
//!   └─ SSF2GameObject
//!        ├─ SSF2Item        ClockTownFairy
//!        ├─ SSF2Enemy       Thwomp, BowsersCastleLava, TingleBalloon, ClockTownFlamingRock
//!        ├─ SSF2Projectile
//!        ├─ SSF2Target
//!        ├─ SSF2Beacon
//!        └─ SSF2Character
//! ```
//!
//! So "is this a game object" is answered by ANCESTRY, and "what kind" by which api base it
//! reaches. Both are facts about the file.
//!
//! The alternative, matching class names against keyword lists, cannot work and did not: it read
//! `ClockTownFairy` as scenery because the name contains "fairy" (it is an SSF2Item -- a pickup),
//! it read `TingleBalloon` as scenery for "balloon" (it is an SSF2Enemy), and it dropped
//! `ClockTownFlamingRock` because "rock" was on no list at all. Names describe; the class graph
//! decides.
//!
//! Nor does this need to sort objects into hazards and decorations. That distinction is not the
//! porter's to make -- an object's own code says whether it has an attack box and what it does,
//! and porting that code faithfully reproduces the difference without anyone having to classify
//! it. What this module answers is only: which classes are objects, and what is each one's base.

use crate::abc_parser::Class;
use std::collections::BTreeMap;

/// The api root every game object descends from.
const GAME_OBJECT_ROOT: &str = "SSF2GameObject";

/// The api base for pickups.
///
/// Held back deliberately, and not because it is hard to FIND -- it is found exactly like every
/// other base. Fraymakers has no item system at all, so an SSF2Item has nothing to port ONTO: it
/// needs a whole custom subsystem built first (pickup, carry, throw, effect, respawn), which is a
/// project of its own rather than a conversion. Until that exists an item is reported as deferred
/// rather than shipped half-built.
pub const ITEM_BASE: &str = "SSF2Item";

/// How deep a class chain is followed before giving up (a malformed package could cycle).
const MAX_DEPTH: usize = 32;

/// One of the package's own game-object classes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GameObjectClass {
    /// The package's class, e.g. `Thwomp`.
    pub class_name: String,
    /// The api base it reaches under the root, e.g. `SSF2Enemy`. This is the KIND: what the game
    /// treats it as, spawns it as, and collides it as.
    pub base: String,
}

impl GameObjectClass {
    /// Whether porting this object is blocked on a subsystem Fraymakers does not have.
    ///
    /// See [`ITEM_BASE`]. This is a statement about the TARGET engine, not about the object: the
    /// class is read as completely as any other, and only the emission has nowhere to go.
    pub fn needs_unbuilt_subsystem(&self) -> bool {
        self.base == ITEM_BASE
    }
}

/// Strip any namespace so `foo.Thwomp` and `Thwomp` compare equal.
fn local(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// Every class in the package that IS a game object, with the api base that says what kind.
///
/// Only the package's OWN classes are returned: the api layer's declarations (the root and the
/// bases directly beneath it) describe the hierarchy rather than participating in it, so they are
/// not objects to port.
pub fn game_object_classes(classes: &[Class]) -> Vec<GameObjectClass> {
    let super_of: BTreeMap<&str, &str> = classes.iter()
        .map(|c| (local(&c.name), local(&c.super_name)))
        .collect();

    // the api bases: whatever sits directly under the root
    let bases: std::collections::BTreeSet<&str> = super_of.iter()
        .filter(|(_, sup)| **sup == GAME_OBJECT_ROOT)
        .map(|(name, _)| *name)
        .collect();

    let mut out: Vec<GameObjectClass> = Vec::new();
    for class in classes {
        let name = local(&class.name);
        if name == GAME_OBJECT_ROOT || bases.contains(name) { continue; }
        // walk up until the root, remembering the last step before it -- that step is the kind
        let (mut cur, mut base) = (name, None);
        for _ in 0..MAX_DEPTH {
            let Some(sup) = super_of.get(cur) else { break };
            if *sup == GAME_OBJECT_ROOT { base = Some(cur.to_string()); break; }
            cur = sup;
        }
        if let Some(base) = base {
            out.push(GameObjectClass { class_name: name.to_string(), base });
        }
    }
    out.sort_by(|a, b| a.class_name.cmp(&b.class_name));
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls(name: &str, sup: &str) -> Class {
        Class { name: name.to_string(), super_name: sup.to_string(),
                instance_methods: vec![], class_methods: vec![], constructor_idx: 0 }
    }

    /// The api layer as every package carries it, plus a stage's own objects on top.
    fn package(own: &[(&str, &str)]) -> Vec<Class> {
        let mut classes = vec![
            cls("SSF2BaseAPIObject", "Object"),
            cls("SSF2GameObject", "SSF2BaseAPIObject"),
            cls("SSF2Item", "SSF2GameObject"),
            cls("SSF2Enemy", "SSF2GameObject"),
            cls("SSF2Projectile", "SSF2GameObject"),
            cls("SSF2Stage", "SSF2BaseAPIObject"),
        ];
        classes.extend(own.iter().map(|(n, s)| cls(n, s)));
        classes
    }

    /// The case the keyword list got backwards in both directions: a "fairy" that is a pickup and
    /// a "balloon" that is an enemy.
    #[test]
    fn objects_are_found_by_what_they_extend() {
        let abc = package(&[
            ("ClockTownFairy", "SSF2Item"),
            ("TingleBalloon", "SSF2Enemy"),
            ("ClockTownFlamingRock", "SSF2Enemy"),
        ]);
        let got: Vec<(String, String)> = game_object_classes(&abc).into_iter()
            .map(|g| (g.class_name, g.base)).collect();
        assert_eq!(got, vec![
            ("ClockTownFairy".to_string(), "SSF2Item".to_string()),
            ("ClockTownFlamingRock".to_string(), "SSF2Enemy".to_string()),
            ("TingleBalloon".to_string(), "SSF2Enemy".to_string()),
        ]);
    }

    /// The api layer describes the hierarchy; it is not content to port.
    #[test]
    fn the_api_layer_is_not_an_object() {
        let abc = package(&[]);
        assert!(game_object_classes(&abc).is_empty());
    }

    /// A stage class is not a game object, however much else it is.
    #[test]
    fn other_api_descendants_are_not_objects() {
        let abc = package(&[("clocktown", "SSF2Stage")]);
        assert!(game_object_classes(&abc).is_empty());
    }

    /// An object several steps down still reports the api base as its kind, not its parent.
    #[test]
    fn the_kind_is_the_api_base_however_deep() {
        let abc = package(&[("FallingThing", "SSF2Enemy"), ("BurningThing", "FallingThing")]);
        let got = game_object_classes(&abc);
        assert_eq!(got.iter().find(|g| g.class_name == "BurningThing").map(|g| g.base.as_str()),
                   Some("SSF2Enemy"));
    }

    /// A namespaced class is the same class.
    #[test]
    fn namespaces_do_not_hide_ancestry() {
        let abc = package(&[("stage_fla.Thwomp", "SSF2Enemy")]);
        assert_eq!(game_object_classes(&abc).len(), 1);
    }

    /// A cycle in the chain must not hang the converter.
    #[test]
    fn a_cycle_terminates() {
        let abc = package(&[("A", "B"), ("B", "A")]);
        assert!(game_object_classes(&abc).is_empty());
    }
}
