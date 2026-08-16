//! stage_abc — recover a stage's layer assignments + spawned actors from its AS3.
//!
//! the stage analog of the character `extract_character` path. every shipped SSF2 stage has a
//! document class (named = the stage id) that extends `SSF2Stage`; its `initialize` method is the
//! "get()" the author wrote: it reads named child clips off the plane accessors
//! (`getBackground()/getForeground()/getMidground()/getCameraBackgrounds()`) and bundles them for
//! the engine, and it engine-spawns the hazards/props (`SSF2API.spawnEnemy(<Class>)` + setX/setY).
//!
//! we recover both by running ONE stack-sim pass (the shared `abc_parser::scan_method` + the
//! property/lex/locals hooks) over `initialize` (and `update`, for hazards spawned lazily there).
//! the result is authoritative: clip-linkage-name -> plane, replacing the name heuristics, and a
//! list of spawned actors with their literal coords, replacing hand-declared hazard metadata.

use crate::abc_parser::{AbcFile, AbcVisitor, Class, MethodBody, StackVal, scan_method};
use std::collections::BTreeMap;

/// Which render plane a stage clip belongs to (from the `getX()` accessor it was read off).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagePlane {
    Background,
    Foreground,
    Midground,
    /// camera-background parallax (`getCameraBackgrounds()[i].mc.<clip>`).
    CameraBg,
}

impl StagePlane {
    /// the lowercase plane name the stage parser's `plane_tag`/`art_kind` use
    /// (`background`/`foreground`/`midground`/`cambg`).
    pub fn code(self) -> &'static str {
        match self {
            StagePlane::Background => "background",
            StagePlane::Foreground => "foreground",
            StagePlane::Midground => "midground",
            StagePlane::CameraBg => "cambg",
        }
    }
    fn from_code(s: &str) -> Option<StagePlane> {
        Some(match s {
            "background" => StagePlane::Background,
            "foreground" => StagePlane::Foreground,
            "midground" => StagePlane::Midground,
            "cambg" => StagePlane::CameraBg,
            _ => return None,
        })
    }
    /// the plane an `SSF2Stage` accessor method returns, if it is one.
    fn of_accessor(method: &str) -> Option<StagePlane> {
        Some(match method {
            "getBackground" => StagePlane::Background,
            "getForeground" => StagePlane::Foreground,
            "getMidground" => StagePlane::Midground,
            "getCameraBackgrounds" => StagePlane::CameraBg,
            _ => return None,
        })
    }
}

/// An engine-spawned actor (`SSF2API.spawnEnemy(<Class>)`), with its literal spawn coords if the
/// AS3 set them with constant `setX`/`setY` (dynamic/random positions stay `None`).
#[derive(Clone, Debug)]
pub struct SpawnedActor {
    pub class_name: String,
    /// The api base this class extends -- `SSF2Enemy`, `SSF2Item`, `SSF2Projectile` and so on.
    /// This is WHAT THE OBJECT IS, taken from the class graph rather than guessed from its name.
    pub base: String,
    pub x: Option<f64>,
    pub y: Option<f64>,
    /// The hazard CLASS's own declarations (the "get()" methods the author wrote): each
    /// `getAttackStats` hitbox map (damage/direction/power/kbConstant — the authoritative hit
    /// params, not a generic per-kind default) and the `getOwnStats` scalars (width/height/speeds).
    /// Empty when the class has neither getter.
    pub attack_hitboxes: Vec<std::collections::BTreeMap<String, f64>>,
    pub own_stats: std::collections::BTreeMap<String, f64>,
    /// Animation labels the class plays via `forceAttack("<label>")` (Thwomp: entrance/idle/fall;
    /// HyruleTornado: stand) — the code-referenced handle to its art clip (the clip carrying those
    /// frame labels), so the art is found by what the script animates, not a library keyword match.
    pub anim_labels: Vec<String>,
    /// Behavior values stepped out of the class's update()/initialize() (shake amplitude, rise
    /// speed, fall gravity, self-platform, dust, sounds) — drives the FM CGO script from the
    /// hazard's own code instead of a template of constants.
    pub behavior: crate::abc_parser::EnemyBehavior,
    /// The class's lifecycle methods reconstructed as FM hscript via the character/projectile
    /// decompile→translate pipeline (the real update() state machine). Still needs a field-state +
    /// FrameTimer pass to run; used by the gated reconstruction path. None if no such methods.
    pub reconstructed_script: Option<String>,
    /// The engine-spawned faller cycle (delays/period/columns) stepped from the enemy + stage code.
    pub faller: Option<crate::abc_parser::FallerCycle>,
}

/// What the AS3 says about a stage: the authoritative plane map + the spawned actors.
#[derive(Clone, Debug, Default)]
pub struct StageAbcModel {
    /// clip linkage/instance name -> the plane it was assigned to in `initialize`.
    pub planes: BTreeMap<String, StagePlane>,
    /// engine-spawned hazards/props from `initialize` + `update`.
    pub actors: Vec<SpawnedActor>,
    /// the document class extending `SSF2Stage` (= the stage id).
    pub doc_class: String,
}

/// the document class is the one extending `SSF2Stage`. (mirror of the character path's
/// "locate the entry class" step in `find_bundle_method`.)
pub fn find_stage_class(abc: &AbcFile) -> Option<&Class> {
    abc.classes.iter().find(|c| c.super_name == "SSF2Stage")
}

/// resolve an instance method's body by name (mirror of `find_bundle_method`'s trait->body step).
fn method_body<'a>(abc: &'a AbcFile, class: &Class, name: &str) -> Option<&'a MethodBody> {
    let t = class.instance_methods.iter().find(|t| t.name == name)?;
    abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx)
}

/// run the plane + actor recovery over a stage's `initialize` (+ `update`).
pub fn extract_stage(abc: &AbcFile) -> Option<StageAbcModel> {
    let class = find_stage_class(abc)?;
    let mut v = StageVisitor::default();
    if let Some(body) = method_body(abc, class, "initialize") {
        scan_method(&body.bytecode, abc, &mut v);
    }
    // hazards spawned lazily (e.g. bowserscastle's Thwomp) live in `update`.
    if let Some(body) = method_body(abc, class, "update") {
        scan_method(&body.bytecode, abc, &mut v);
    }
    // Every game object the package DECLARES is an object to port. The spawn-site scan above can
    // only see an actor when the class reference and the `spawnEnemy` call are close enough for the
    // stack sim to carry the tag between them, so it finds clocktown's TingleBalloon (three opcodes
    // apart) and misses its ClockTownFairy (two hundred). The class graph has no such gap: it says
    // what the package's objects ARE. The scan is kept for what only it knows -- the literal spawn
    // coordinates at the call site.
    let declared = crate::ssf2_objects::game_object_classes(&abc.classes);
    for go in &declared {
        match v.actors.iter_mut().find(|a| a.class_name == go.class_name) {
            Some(found) => found.base = go.base.clone(),
            None => v.actors.push(SpawnedActor {
                class_name: go.class_name.clone(), base: go.base.clone(), x: None, y: None,
                attack_hitboxes: Vec::new(), own_stats: Default::default(), anim_labels: Vec::new(),
                behavior: Default::default(), reconstructed_script: None, faller: None }),
        }
    }

    // Pull each spawned hazard's OWN declarations from its class's get* methods — the authoritative
    // damage/hit params + size/speeds, parsed straight from the SSF2 source instead of guessed.
    let mut actors = v.actors;
    for a in &mut actors {
        a.attack_hitboxes = crate::abc_parser::extract_attack_stats_for(abc, &a.class_name);
        a.own_stats = crate::abc_parser::extract_own_stats_for(abc, &a.class_name);
        a.anim_labels = crate::abc_parser::extract_force_attack_labels(abc, &a.class_name);
        a.behavior = crate::abc_parser::extract_enemy_behavior(abc, &a.class_name);
        a.reconstructed_script = crate::abc_parser::reconstruct_enemy_script(abc, &a.class_name);
        a.faller = crate::abc_parser::extract_faller_cycle(abc, &a.class_name);
    }
    Some(StageAbcModel { planes: v.planes, actors, doc_class: class.name.clone() })
}

/// stack-sim visitor: tags plane-accessor results + getlex class names so the chained
/// `getProperty <clip>` reads bind a clip to a plane, and `spawnEnemy` + `setX/setY` record actors.
#[derive(Default)]
struct StageVisitor {
    planes: BTreeMap<String, StagePlane>,
    actors: Vec<SpawnedActor>,
}

impl AbcVisitor for StageVisitor {
    fn track_locals(&self) -> bool { true }

    fn on_getlex(&mut self, name: &str) -> Option<StackVal> {
        // carry the resolved name so a following `spawnEnemy(<Class>)` can read its argument.
        Some(StackVal::Tag(format!("lex:{name}")))
    }

    fn on_callproperty(&mut self, method: &str, args: &[StackVal], receiver: &StackVal) -> Option<StackVal> {
        // a plane accessor -> tag its result so the chained getproperty binds clips to this plane.
        if let Some(plane) = StagePlane::of_accessor(method) {
            return Some(StackVal::Tag(format!("plane:{}", plane.code())));
        }
        // SSF2API.spawnEnemy(<Class>) -> a new actor; tag the result for the following setX/setY.
        if method == "spawnEnemy" || method == "spawnProjectile" {
            if let Some(StackVal::Tag(t)) = args.first() {
                if let Some(class) = t.strip_prefix("lex:") {
                    let idx = self.actors.len();
                    self.actors.push(SpawnedActor { class_name: class.to_string(), base: String::new(), x: None, y: None,
                        attack_hitboxes: Vec::new(), own_stats: std::collections::BTreeMap::new(), anim_labels: Vec::new(),
                        behavior: crate::abc_parser::EnemyBehavior::default(), reconstructed_script: None, faller: None });
                    return Some(StackVal::Tag(format!("actor:{idx}")));
                }
            }
            return None;
        }
        // <actor>.setX(literal) / .setY(literal): record the spawn coords.
        if (method == "setX" || method == "setY") && !args.is_empty() {
            if let (StackVal::Tag(t), StackVal::Num(n)) = (receiver, &args[0]) {
                if let Some(idx) = t.strip_prefix("actor:").and_then(|s| s.parse::<usize>().ok()) {
                    if let Some(a) = self.actors.get_mut(idx) {
                        if method == "setX" { a.x = Some(*n); } else { a.y = Some(*n); }
                    }
                }
            }
        }
        None
    }

    fn on_getproperty(&mut self, prop: &str, receiver: &StackVal) -> Option<StackVal> {
        // a getproperty off a plane-tagged receiver binds that clip to the plane. propagate the tag
        // so a chain (`getCameraBackgrounds()[0].mc.<clip>`) keeps the plane through `[0]`/`.mc`.
        if let StackVal::Tag(t) = receiver {
            if let Some(plane) = t.strip_prefix("plane:").and_then(StagePlane::from_code) {
                if !prop.is_empty() {
                    // record only real clip linkage names, not array indexes / `mc` containers.
                    if prop != "mc" {
                        self.planes.entry(prop.to_string()).or_insert(plane);
                    }
                }
                return Some(StackVal::Tag(t.clone()));
            }
        }
        None
    }
}
