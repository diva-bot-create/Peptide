//! A physics test stage built FROM SCRATCH, with geometry we choose and therefore know exactly.
//!
//! Every parity measurement so far has been taken on a real SSF2 stage, where the geometry is
//! whatever the original authors drew and hazards are live. That contaminates results in ways that
//! are easy to miss: an "air drift" sample on bowserscastle turned out to include lava knockback,
//! which produced a speed multiplier that looked like a finding and wasn't. A fixture removes that
//! whole class of error, because nothing is in it that we didn't put there.
//!
//! The three situations it has to support, and the geometry that gives each one:
//!
//! * GROUND FOREVER — one wide flat floor, no hazards, and the walkable span ends well inside the
//!   camera box, so a character can be driven left/right indefinitely without falling off.
//! * AIR FOREVER — a deliberately enormous blast box. A drop from the top runs for hundreds of
//!   frames before anything happens, which is what makes per-frame air measurements possible at
//!   all; on a normal stage the fall is over in ~40 frames and half of those are the transition.
//! * HYBRID — a soft (drop-through) platform above the floor, with the ledge beacons at the
//!   floor's ends, so ledge-grab and platform-drop behaviour have a known reference.
//!
//! Coordinates are SSF2 units (the converter scales them on the way out). The floor's top surface
//! is y = 0 so that "on the ground" is the origin and heights read directly as distance fallen.

use swf::{
    Fixed8, Rectangle, ShapeRecord, ShapeStyles, StyleChangeData, Tag, Twips,
};

/// Floor top surface. Everything vertical is measured from here.
pub const FLOOR_Y: f64 = 0.0;
/// Floor half-width: the walkable span is `-FLOOR_HALF_W ..= FLOOR_HALF_W`.
pub const FLOOR_HALF_W: f64 = 600.0;
/// Soft platform's top surface, above the floor.
pub const PLATFORM_Y: f64 = -260.0;
/// How high above the floor a character can be dropped from and still be inside the blast box.
pub const DROP_CEILING: f64 = -2800.0;

fn px(v: f64) -> Twips { Twips::from_pixels(v) }

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Rectangle<Twips> {
    Rectangle { x_min: px(x0), x_max: px(x1), y_min: px(y0), y_max: px(y1) }
}

/// A solid-colour rectangle shape. The colour only matters for eyeballing a screenshot; the
/// converter reads geometry, not paint.
fn rect_shape(id: u16, x0: f64, y0: f64, x1: f64, y1: f64, colour: swf::Color) -> Tag<'static> {
    // A single edge delta is bit-limited (measured: ~3276px is the ceiling before the writer
    // rejects it with "excessive value for bits written"), and the blast box is deliberately far
    // larger than that. Emit each side as a run of shorter segments so the box can be any size.
    const MAX_SEG: f64 = 1500.0;
    let run = |len: f64, horizontal: bool| -> Vec<ShapeRecord> {
        let n = (len.abs() / MAX_SEG).ceil().max(1.0);
        let step = len / n;
        (0..n as usize).map(|_| ShapeRecord::StraightEdge {
            delta: if horizontal { swf::PointDelta::new(px(step), Twips::ZERO) }
                   else { swf::PointDelta::new(Twips::ZERO, px(step)) },
        }).collect()
    };
    let (w, h) = (x1 - x0, y1 - y0);
    Tag::DefineShape(swf::Shape {
        version: 3,
        id,
        shape_bounds: rect(x0, y0, x1, y1),
        edge_bounds: rect(x0, y0, x1, y1),
        flags: swf::ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![swf::FillStyle::Color(colour)],
            line_styles: vec![],
        },
        shape: vec![
            ShapeRecord::StyleChange(Box::new(StyleChangeData {
                move_to: Some(swf::Point::new(px(x0), px(y0))),
                fill_style_0: None,
                fill_style_1: Some(1),
                line_style: None,
                new_styles: None,
            })),
        ]
        .into_iter()
        .chain(run(w, true))
        .chain(run(h, false))
        .chain(run(-w, true))
        .chain(run(-h, false))
        .collect(),
    })
}

/// Wrap a shape in its own sprite so it can be placed with an instance name and given a
/// SymbolClass linkage (which is how SSF2 marks spawns and ledges).
fn sprite_of(sprite_id: u16, shape_id: u16) -> Tag<'static> {
    Tag::DefineSprite(swf::Sprite {
        id: sprite_id,
        num_frames: 1,
        tags: vec![place(shape_id, None, 1), Tag::ShowFrame],
    })
}

fn place(id: u16, name: Option<&'static str>, depth: u16) -> Tag<'static> {
    place_at(id, name, depth, 0.0, 0.0)
}

/// Place with a TRANSLATION. A beacon's position is read from its placement matrix, not from
/// where its art happens to sit: the walk records `cx/cy` as the placement's `world.tx/ty`, so a
/// shape drawn at an offset and placed at identity reports position (0,0) like every other one.
fn place_at(id: u16, name: Option<&'static str>, depth: u16, x: f64, y: f64) -> Tag<'static> {
    let name = name.map(swf::SwfStr::from_utf8_str);
    let mut matrix = swf::Matrix::IDENTITY;
    matrix.tx = px(x);
    matrix.ty = px(y);
    Tag::PlaceObject(Box::new(swf::PlaceObject {
        version: 3,
        action: swf::PlaceObjectAction::Place(id),
        depth,
        matrix: Some(matrix),
        color_transform: None,
        ratio: None,
        name,
        clip_depth: None,
        class_name: None,
        filters: None,
        background_color: None,
        blend_mode: None,
        clip_actions: None,
        has_image: false,
        is_bitmap_cached: None,
        is_visible: Some(true),
        amf_data: None,
    }))
}

/// Build the fixture as a raw SWF. `ssf::decompress` passes an uncompressed `FWS` stream straight
/// through, so this is directly loadable by the stage parser without any packaging step.
///
/// Marker naming follows SSF2's conventions because that is what the parser keys on: boundary
/// clips by PlaceObject NAME (`deathBoundary`, `camBoundary`), spawns and ledges by SymbolClass
/// LINKAGE (`..._start_N` / `..._spawn_N`, `ledge_mc_left` / `ledge_mc_right`).
pub fn build_fixture_swf() -> Vec<u8> {
    let grey = swf::Color { r: 90, g: 95, b: 105, a: 255 };
    let blue = swf::Color { r: 70, g: 110, b: 180, a: 255 };
    let mark = swf::Color { r: 255, g: 0, b: 255, a: 0 }; // invisible beacons

    let mut tags: Vec<Tag> = vec![
        // Must come first, and must say ActionScript 3: without it the player treats the whole
        // file as AVM1, skips the ABC, and every symbol link silently binds to nothing.
        Tag::FileAttributes(swf::FileAttributes::IS_ACTION_SCRIPT_3),
        // --- collision geometry, inside a `terrain` clip (the parser's collision container) ---
        rect_shape(1, -FLOOR_HALF_W, FLOOR_Y, FLOOR_HALF_W, FLOOR_Y + 40.0, grey),
        rect_shape(2, -200.0, PLATFORM_Y, 200.0, PLATFORM_Y + 20.0, blue),
        // --- boundaries. The blast box is huge on purpose: that is what buys a long fall. ---
        rect_shape(3, -3000.0, -3000.0, 3000.0, 3000.0, mark),
        rect_shape(4, -800.0, -600.0, 800.0, 600.0, mark),
    ];

    // --- beacons: 4 starts, 4 respawns, 2 ledges ---
    let mut next_shape = 20u16;
    let mut next_sprite = 40u16;
    let mut beacons: Vec<(u16, String)> = Vec::new();
    let mut placements: Vec<Tag> = Vec::new();
    let mut depth = 20u16;
    for (i, x) in [-300.0f64, -100.0, 100.0, 300.0].into_iter().enumerate() {
        for (kind, y) in [("start", -100.0f64), ("spawn", -600.0)] {
            tags.push(rect_shape(next_shape, -4.0, -4.0, 4.0, 4.0, mark));
            tags.push(sprite_of(next_sprite, next_shape));
            // SSF2 numbers players from ONE: `player_index` subtracts 1, so a `p0_` beacon is
            // silently dropped rather than becoming player 0.
            beacons.push((next_sprite, format!("fixture_fla.p{}_{kind}_{next_sprite}", i + 1)));
            placements.push(place_at(next_sprite, None, depth, x, y));
            next_shape += 1;
            next_sprite += 1;
            depth += 1;
        }
    }
    for (side, x) in [("left", -FLOOR_HALF_W), ("right", FLOOR_HALF_W)] {
        tags.push(rect_shape(next_shape, -4.0, -4.0, 4.0, 4.0, mark));
        tags.push(sprite_of(next_sprite, next_shape));
        beacons.push((next_sprite, format!("fixture_fla.ledge_mc_{side}__{next_sprite}")));
        placements.push(place_at(next_sprite, None, depth, x, FLOOR_Y));
        next_shape += 1;
        next_sprite += 1;
        depth += 1;
    }

    // The clip tree a stage is REQUIRED to have, not a tree of our choosing. The game walks it by
    // name and does not check as it goes, so a missing layer is not an empty layer: it is a read
    // off nothing, mid-match-start, with no indication of which name was missing.
    //
    //   stageMC          the whole stage, linked `stage_<id>`
    //     background     art behind the fighters
    //     terrain        collision, boundaries and the spawn beacons
    //     foreground     art in front
    //
    // Boundaries and beacons belong INSIDE terrain, which is where shipped stages keep them.
    let mut terrain = vec![
        place(1, Some("floor"), 1),
        place(2, Some("platform"), 2),
        place(3, Some("deathBoundary"), 3),
        place(4, Some("camBoundary"), 4),
    ];
    terrain.extend(placements);
    terrain.push(Tag::ShowFrame);
    tags.push(Tag::DefineSprite(swf::Sprite { id: 10, num_frames: 1, tags: terrain }));

    // A fixture has no art on purpose, but both layers still have to EXIST.
    tags.push(Tag::DefineSprite(swf::Sprite { id: 12, num_frames: 1, tags: vec![Tag::ShowFrame] }));
    tags.push(Tag::DefineSprite(swf::Sprite { id: 13, num_frames: 1, tags: vec![Tag::ShowFrame] }));
    tags.push(Tag::DefineSprite(swf::Sprite {
        id: 11,
        num_frames: 1,
        tags: vec![
            place(12, Some("background"), 1),
            place(10, Some("terrain"), 2),
            place(13, Some("foreground"), 3),
            Tag::ShowFrame,
        ],
    }));
    tags.push(place(11, Some("stageMC"), 1));
    tags.push(Tag::ShowFrame);

    let stage_symbol = format!("stage_{FIXTURE_ID}");
    beacons.push((11, stage_symbol));
    // Symbol 0 is the file's own root, and binding it to `Main` is what makes the loaded content
    // a PACKAGE rather than an anonymous clip. Without it the game gets something that does not
    // answer to the package API, rejects it, and stops -- the geometry and the id are irrelevant
    // at that point because nothing ever asks for them.
    beacons.push((0, "Main".to_string()));
    tags.push(Tag::SymbolClass(
        beacons.iter().map(|(id, name)| swf::SymbolClassLink {
            id: *id,
            class_name: swf::SwfStr::from_utf8_str(Box::leak(name.clone().into_boxed_str())),
        }).collect(),
    ));
    // the AS3 half: without it the package has no identity and SSF2 cannot see it at all
    let abc = build_fixture_abc(&beacons);
    tags.push(Tag::DoAbc2(swf::DoAbc2 {
        flags: swf::DoAbc2Flag::LAZY_INITIALIZE,
        name: swf::SwfStr::from_utf8_str("fixture"),
        data: Box::leak(abc.into_boxed_slice()),
    }));
    tags.push(Tag::ShowFrame);

    let header = swf::Header {
        compression: swf::Compression::None,
        version: 21,
        stage_size: rect(-800.0, -600.0, 800.0, 600.0),
        frame_rate: Fixed8::from_f32(30.0),
        num_frames: 2,
    };
    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).expect("fixture swf");
    out
}

// ─────────────────────────── the AS3 half ────────────────────────────────────
//
// A stage is not just geometry: SSF2 identifies a package by its AS3. The id comes from a class
// named `Main` whose constructor registers it (`extract_main_package_metadata` reads exactly
// that), so a shapes-only SWF converts fine and is invisible to the engine.
//
// This authors that ABC from scratch rather than copying one out of an existing stage, which
// matters for two reasons: a donor's bytecode is McLeodGaming content this repo can't ship, and a
// donor would drag in its own id, class graph and behaviour -- the opposite of a fixture whose
// contents are entirely known.

use crate::abc_codec::{
    Abc, ClassInfo, InstanceInfo, MethodBody, MethodInfo, ScriptInfo, Trait,
    TraitKindData,
};

/// The id SSF2 and the converter both see this stage as.
pub const FIXTURE_ID: &str = "peptidefixture";

const OP_GETLOCAL0: u8 = 0xd0;
const OP_GETLOCAL1: u8 = 0xd1;
const OP_GETLOCAL2: u8 = 0xd2;
const OP_GETSCOPEOBJECT: u8 = 0x65;
const OP_PUSHFALSE: u8 = 0x27;
const OP_GETPROPERTY: u8 = 0x66;
const OP_SETPROPERTY: u8 = 0x61;
const OP_RETURNVALUE: u8 = 0x48;
const OP_PUSHSCOPE: u8 = 0x30;
const OP_POPSCOPE: u8 = 0x1d;
const OP_PUSHSTRING: u8 = 0x2c;
const OP_PUSHBYTE: u8 = 0x24;
const OP_DUP: u8 = 0x2a;
const OP_NEWCLASS: u8 = 0x58;
const OP_NEWOBJECT: u8 = 0x55;
const OP_NEWARRAY: u8 = 0x56;
const OP_INITPROPERTY: u8 = 0x68;
const OP_FINDPROPSTRICT: u8 = 0x5d;
const OP_CALLPROPVOID: u8 = 0x4f;
const OP_CONSTRUCTSUPER: u8 = 0x49;
const OP_RETURNVOID: u8 = 0x47;
const NS_PACKAGE: u8 = 0x16;

/// A stable guid for the fixture. Shipped packages carry one and the reader looks for it, so
/// leaving it out would make the fixture the only package in the corpus without an identity.
/// Where a package keeps what it registered about itself, and what it declares to the game.
const PROPS_SLOT: &str = "m_peptideProps";
const META_SLOT: &str = "MetaData";

/// The two names this package is free to choose. Everything else it declares has to match what
/// the game looks up, name for name.
const PKG_BASE_CLASS: &str = "PeptideBaseObject";
const PKG_ASSET_CLASS: &str = "PeptideAsset";


/// Every class a package must declare to the game, by the game's own names. A package built with
/// the official tooling carries an implementation of each; the fixture carries the shape and only
/// the stage base does anything, which is enough to be checked but not enough to be driven.
const API_CLASSES: [&str; 14] = [
    "SSF2GameObject", "SSF2Character", "SSF2Enemy", "SSF2Item", "SSF2Platform", "SSF2Projectile",
    "SSF2Stage", "SSF2CollisionBoundary", "SSF2Target", "SSF2CustomMatch", "SSF2CustomMode",
    "SSF2Camera", "SSF2GameTimer", "SSF2Beacon",
];

/// The name of the stage base a package declares to the game. The game looks this exact name up
/// in the package's own class map, so it is not ours to choose.
const PKG_STAGE_BASE: &str = "SSF2Stage";

/// What the game calls on a stage while a match runs.
const STAGE_CALLBACKS: [&str; 2] = ["initialize", "update"];

/// The package API version this fixture is written against, as the game declares it.
const API_VERSION_MAJOR: u8 = 0;
const API_VERSION_MINOR: u8 = 56;
const API_VERSION_REVISION: u8 = 0;

pub const FIXTURE_GUID: &str = "9f3c1e7a-2b64-4d18-a5f0-6c81d47e2b90";

/// Append a u30 in AVM2's variable-length encoding.
fn u30(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 { out.push(b); break; }
        out.push(b | 0x80);
    }
}

/// Emit `op` followed by one u30 operand.
fn op1(out: &mut Vec<u8>, op: u8, a: u32) { out.push(op); u30(out, a); }
/// Emit `op` followed by two u30 operands (property multiname + arg count).
fn op2(out: &mut Vec<u8>, op: u8, a: u32, b: u32) { out.push(op); u30(out, a); u30(out, b); }

/// Everything needed to author one class: what it is called, what it extends, and its
/// constructor. `param_count` exists because the base a class extends dictates its signature --
/// a stage class is constructed with one argument and passes it straight to `super`.
struct ClassSpec {
    name: &'static str,
    /// dotted package, `""` for the top level
    package: String,
    super_mn: u32,
    param_count: u32,
    ctor: Vec<u8>,
    max_stack: u32,
    local_count: u32,
    /// named instance slots, declared so the constructor can initialise them
    slots: Vec<&'static str>,
    methods: Vec<MethodSpec>,
}

/// One instance method on an authored class.
struct MethodSpec {
    name: &'static str,
    param_count: u32,
    code: Vec<u8>,
    max_stack: u32,
    local_count: u32,
}

/// Build the fixture's ABC from scratch.
///
/// The shape is dictated by what actually happens when the game opens a package, each part of
/// which was learned by watching a load fail:
///
/// * Every class named by a `SymbolClass` link must EXIST here. A link to a class the ABC does not
///   define is not a soft failure -- the player throws while binding symbols, and because that
///   happens as the data directory is being read, the whole boot dies rather than just this one
///   package. Nothing in the game's own logs points at the file responsible.
/// * The identity comes from a class named `Main` whose constructor registers it, so the package
///   is invisible without one no matter how correct the geometry is.
/// * `Main` extends `SSF2Asset` and the stage class extends `SSF2Stage`, neither of which this
///   file can define for real. Declaring them as stubs is exactly what shipped packages do: the
///   loader resolves a name that already exists to the definition that is already loaded, so the
///   stubs are replaced by the real classes and never run.
fn build_fixture_abc(symbols: &[(u16, String)]) -> Vec<u8> {
    let mut abc = Abc {
        minor: 16, major: 46,
        ints: vec![], uints: vec![], doubles: vec![],
        strings: vec![], strings_raw: vec![],
        namespaces: vec![], ns_sets: vec![], multinames: vec![],
        methods: vec![], metadata: vec![], instances: vec![], classes: vec![],
        scripts: vec![], bodies: vec![],
    };

    let s_empty = abc.intern_string("");
    let pub_ns = abc.intern_namespace(NS_PACKAGE, s_empty);
    let mn_movieclip = {
        let p = abc.intern_string("flash.display");
        let ns = abc.intern_namespace(NS_PACKAGE, p);
        let n = abc.intern_string("MovieClip");
        abc.intern_qname(ns, n)
    };
    let mn_object = { let n = abc.intern_string("Object"); abc.intern_qname(pub_ns, n) };
    let mn_register = { let n = abc.intern_string("register"); abc.intern_qname(pub_ns, n) };

    // Names the constructor of `Main` pushes. Interning them up front keeps the emit below flat.
    let str_ = |abc: &mut Abc, s: &str| abc.intern_string(s);
    let s_id = str_(&mut abc, "id");
    let s_fixture = str_(&mut abc, FIXTURE_ID);
    let s_guid = str_(&mut abc, "guid");
    let s_guid_v = str_(&mut abc, FIXTURE_GUID);
    let s_resources = str_(&mut abc, "resources");
    let s_movieclips = str_(&mut abc, "movieclips");
    let s_sounds = str_(&mut abc, "sounds");
    let s_stage_mc = str_(&mut abc, &format!("stage_{FIXTURE_ID}"));
    let s_music = str_(&mut abc, "music");
    let s_track = str_(&mut abc, "bgm_battlefield");
    let s_stage = str_(&mut abc, "stage");
    let s_camera = str_(&mut abc, "camera");
    let s_x_start = str_(&mut abc, "x_start");
    let s_y_start = str_(&mut abc, "y_start");
    let s_autopan = str_(&mut abc, "autoPanMultiplier");
    let s_backgrounds = str_(&mut abc, "backgrounds");

    // ── the stub hierarchy the real definitions take over at load time ──
    let mut specs: Vec<ClassSpec> = Vec::new();
    let plain_ctor = |argc: u32| {
        let mut c = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_GETLOCAL0];
        if argc == 1 { c.push(OP_GETLOCAL1); }
        op1(&mut c, OP_CONSTRUCTSUPER, argc);
        c.push(OP_RETURNVOID);
        c
    };

    let declare = |specs: &mut Vec<ClassSpec>, name, package: &str, super_mn, param_count, ctor: Vec<u8>, max_stack, local_count| {
        specs.push(ClassSpec { name, package: package.to_string(), super_mn, param_count, ctor,
            max_stack, local_count, slots: vec![], methods: vec![] });
    };

    declare(&mut specs, PKG_BASE_CLASS, "", mn_object, 0, plain_ctor(0), 2, 1);

    // The asset base is NOT a stub. A package carries its own copy of the small API layer the
    // game talks to, in the top-level namespace, so the game's same-named classes never stand in
    // for it: what this file declares is what actually runs. `Main` calls `register` on it from
    // its own constructor, so an empty base means the package throws before it has said what it
    // is. A property bag is the whole job -- `register` writes, `getProp` reads.
    let s_props = str_(&mut abc, PROPS_SLOT);
    let mn_props = { let ns = abc.intern_namespace(NS_PACKAGE, s_empty); abc.intern_qname(ns, s_props) };
    let key_nsset = abc.intern_ns_set(vec![pub_ns]);
    let mn_key = abc.intern_multinamel(key_nsset);   // obj[<runtime key>]

    let s_meta = str_(&mut abc, META_SLOT);
    let mn_meta = { let ns = abc.intern_namespace(NS_PACKAGE, s_empty); abc.intern_qname(ns, s_meta) };
    let s_base = str_(&mut abc, "BASE_CLASSES");
    let s_vmaj = str_(&mut abc, "VERSION_MAJOR");
    let s_vmin = str_(&mut abc, "VERSION_MINOR");
    let s_vrev = str_(&mut abc, "VERSION_REVISION");

    // ctor: super(); this.<props> = {}; this.<meta> = { BASE_CLASSES: [], VERSION_*: ... }
    //
    // The game reads the version off a package before it will talk to it, and a package that
    // cannot answer is one it refuses. The numbers are the API version this file is written
    // against, taken from the game's own declaration of it.
    let mut asset_ctor = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_GETLOCAL0];
    op1(&mut asset_ctor, OP_CONSTRUCTSUPER, 0);
    asset_ctor.push(OP_GETLOCAL0);
    op1(&mut asset_ctor, OP_NEWOBJECT, 0);
    op1(&mut asset_ctor, OP_INITPROPERTY, mn_props);
    // BASE_CLASSES is a MAP from class name to the package's own class object, not a list. The
    // game checks a package against its own list of these, and reads entries straight out without
    // looking first, so every name has to be present: a missing one comes back undefined and is
    // then used. Each is looked up late, for the usual reason -- naming a class directly binds it
    // before its own script has run.
    asset_ctor.push(OP_GETLOCAL0);
    op1(&mut asset_ctor, OP_PUSHSTRING, s_base);
    for name in API_CLASSES {
        let sn = str_(&mut abc, name);
        let mn = { let ns = abc.intern_namespace(NS_PACKAGE, s_empty); abc.intern_qname(ns, sn) };
        op1(&mut asset_ctor, OP_PUSHSTRING, sn);
        op1(&mut asset_ctor, OP_FINDPROPSTRICT, mn);
        op1(&mut asset_ctor, OP_GETPROPERTY, mn);
    }
    op1(&mut asset_ctor, OP_NEWOBJECT, API_CLASSES.len() as u32);
    op1(&mut asset_ctor, OP_PUSHSTRING, s_vmaj);
    asset_ctor.push(OP_PUSHBYTE); asset_ctor.push(API_VERSION_MAJOR);
    op1(&mut asset_ctor, OP_PUSHSTRING, s_vmin);
    asset_ctor.push(OP_PUSHBYTE); asset_ctor.push(API_VERSION_MINOR);
    op1(&mut asset_ctor, OP_PUSHSTRING, s_vrev);
    asset_ctor.push(OP_PUSHBYTE); asset_ctor.push(API_VERSION_REVISION);
    op1(&mut asset_ctor, OP_NEWOBJECT, 4);
    op1(&mut asset_ctor, OP_INITPROPERTY, mn_meta);
    asset_ctor.push(OP_RETURNVOID);
    let _ = mn_meta;

    // register(k, v): this.<props>[k] = v
    let mut reg = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_GETLOCAL0];
    op1(&mut reg, OP_GETPROPERTY, mn_props);
    reg.push(OP_GETLOCAL1);
    reg.push(OP_GETLOCAL2);
    op1(&mut reg, OP_SETPROPERTY, mn_key);
    reg.push(OP_RETURNVOID);

    // getProp(k): return this.<props>[k]
    let mut getp = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_GETLOCAL0];
    op1(&mut getp, OP_GETPROPERTY, mn_props);
    getp.push(OP_GETLOCAL1);
    op1(&mut getp, OP_GETPROPERTY, mn_key);
    getp.push(OP_RETURNVALUE);

    let noop = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_RETURNVOID];
    specs.push(ClassSpec {
        name: PKG_ASSET_CLASS, package: String::new(), super_mn: mn_movieclip, param_count: 0,
        ctor: asset_ctor, max_stack: 48, local_count: 1,
        slots: vec![PROPS_SLOT, META_SLOT],
        methods: vec![
            MethodSpec { name: "register", param_count: 2, code: reg, max_stack: 4, local_count: 3 },
            MethodSpec { name: "getProp", param_count: 1, code: getp, max_stack: 3, local_count: 2 },
            // the game checks these exist before it will talk to a package
            MethodSpec { name: "initAPI", param_count: 1, code: noop.clone(), max_stack: 2, local_count: 2 },
            MethodSpec { name: "deinitAPI", param_count: 0, code: noop.clone(), max_stack: 2, local_count: 1 },
            MethodSpec { name: "getAPIVersion", param_count: 0, code: noop, max_stack: 2, local_count: 1 },
        ],
    });

    // The stage's own display symbol, plus one class per beacon. All plain clips.
    let stage_symbol: &'static str = Box::leak(format!("stage_{FIXTURE_ID}").into_boxed_str());
    declare(&mut specs, stage_symbol, "", mn_movieclip, 0, plain_ctor(0), 2, 1);
    for (_, sym) in symbols {
        // both are declared in full below; the loop only covers the plain clips
        if sym == stage_symbol || sym == "Main" { continue; }
        let (package, local) = match sym.rsplit_once('.') {
            Some((p, l)) => (p.to_string(), l.to_string()),
            None => (String::new(), sym.clone()),
        };
        let local: &'static str = Box::leak(local.into_boxed_str());
        declare(&mut specs, local, &package, mn_movieclip, 0, plain_ctor(0), 2, 1);
    }

    // These two need forward references, so reserve their multinames before the loop that
    // creates classes -- a class must name its base before that base exists as a class object.
    let mn_base = { let n = abc.intern_string(PKG_BASE_CLASS); abc.intern_qname(pub_ns, n) };
    let mn_asset = { let n = abc.intern_string(PKG_ASSET_CLASS); abc.intern_qname(pub_ns, n) };
    // The stage base, under the game's own name for it: the game reads this exact key out of the
    // package's class map, so it is not a name we get to pick. The class itself is ours, and a
    // stub -- the official tooling compiles a real implementation in, and that is the one gap this
    // file cannot author around. Naming the game's own class instead does NOT work either: a
    // loaded package cannot see it, so the reference resolves to nothing and the package is
    // refused outright.
    let mn_ssf2stage = { let n = abc.intern_string(PKG_STAGE_BASE); abc.intern_qname(pub_ns, n) };
    let mn_fixture_cls = { let n = abc.intern_string(FIXTURE_ID); abc.intern_qname(pub_ns, n) };

    for name in API_CLASSES {
        let sup = if name == PKG_STAGE_BASE { mn_base } else { mn_object };
        declare(&mut specs, name, "", sup, 0, plain_ctor(0), 2, 1);
    }

    // A stage is constructed WITH an argument and hands it to super; a 0-arg version is rejected.
    //
    // The callbacks are where this stops. A shipped stage class is a ~70 byte shim: its
    // `initialize` calls `getBackground()`, `getForeground()` and `getCameraBackgrounds()` on the
    // base it inherits, and it is the BASE that wires the stage into the game. That base is the
    // package-side api layer, which the official tooling compiles into every package and which
    // this file has no copy of. So the fixture can be loaded, validated and cached -- all of which
    // it now is -- and still not be drivable, because the callbacks it answers have nothing under
    // them to answer with.
    let noop_m = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_RETURNVOID];
    specs.push(ClassSpec {
        name: FIXTURE_ID, package: String::new(), super_mn: mn_ssf2stage, param_count: 1,
        ctor: plain_ctor(1), max_stack: 3, local_count: 2, slots: vec![],
        methods: STAGE_CALLBACKS.iter().map(|n| MethodSpec {
            name: n, param_count: 0, code: noop_m.clone(), max_stack: 2, local_count: 1,
        }).collect(),
    });

    // ── Main: the package's identity, registered field by field ──
    let mut m = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_GETLOCAL0];
    op1(&mut m, OP_CONSTRUCTSUPER, 0);

    let pair = |m: &mut Vec<u8>, key: u32, emit: &dyn Fn(&mut Vec<u8>)| {
        op1(m, OP_FINDPROPSTRICT, mn_register);
        op1(m, OP_PUSHSTRING, key);
        emit(m);
        op2(m, OP_CALLPROPVOID, mn_register, 2);
    };

    pair(&mut m, s_id, &|m| op1(m, OP_PUSHSTRING, s_fixture));
    pair(&mut m, s_guid, &|m| op1(m, OP_PUSHSTRING, s_guid_v));
    // resources: { movieclips: [ "stage_<id>" ], sounds: [] }
    pair(&mut m, s_resources, &|m| {
        op1(m, OP_PUSHSTRING, s_movieclips);
        op1(m, OP_PUSHSTRING, s_stage_mc);
        op1(m, OP_NEWARRAY, 1);
        op1(m, OP_PUSHSTRING, s_sounds);
        op1(m, OP_NEWARRAY, 0);
        op1(m, OP_NEWOBJECT, 2);
    });
    // music: [ { id: "<track>" } ]
    pair(&mut m, s_music, &|m| {
        op1(m, OP_PUSHSTRING, s_id);
        op1(m, OP_PUSHSTRING, s_track);
        op1(m, OP_NEWOBJECT, 1);
        op1(m, OP_NEWARRAY, 1);
    });
    // Late-bound on purpose. Naming the class directly is what a compiler emits, but it binds at
    // VERIFY time, and this constructor is verified before the class's own script has run: the
    // player rejects the package outright rather than reporting a missing name. Looking it up
    // through the scope chain defers that to the moment it actually runs, by which point it exists.
    pair(&mut m, s_stage, &|m| {
        op1(m, OP_FINDPROPSTRICT, mn_fixture_cls);
        op1(m, OP_GETPROPERTY, mn_fixture_cls);
    });
    // camera: the fixture is centred on its own origin and has no parallax layers. The empty
    // fields are not padding: the game reaches into them without checking, so a camera block
    // that omits them is one it walks off the end of.
    pair(&mut m, s_camera, &|m| {
        op1(m, OP_PUSHSTRING, s_x_start);
        m.push(OP_PUSHBYTE); m.push(0);
        op1(m, OP_PUSHSTRING, s_y_start);
        m.push(OP_PUSHBYTE); m.push(0);
        op1(m, OP_PUSHSTRING, s_autopan);
        m.push(OP_PUSHFALSE);
        op1(m, OP_PUSHSTRING, s_backgrounds);
        op1(m, OP_NEWARRAY, 0);
        op1(m, OP_NEWOBJECT, 4);
    });
    m.push(OP_RETURNVOID);
    declare(&mut specs, "Main", "", mn_asset, 0, m, 12, 1);

    // ── realise every spec as instance + class, each in a script of its own ──
    //
    // One class per script, which is what shipped packages do and is load-bearing rather than
    // stylistic. Three of these names (the asset and stage bases) already exist in the game, and a
    // script initialiser that trips over an existing name stops there. With every class sharing one
    // script that takes `Main` down with it and the package goes silently unregistered; with a
    // script each, only the stub's own script is affected and it was never going to run anyway.
    let empty_body = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_RETURNVOID];

    for spec in specs.iter() {
        let mn = {
            let p = abc.intern_string(&spec.package);
            let ns = abc.intern_namespace(NS_PACKAGE, p);
            let n = abc.intern_string(spec.name);
            abc.intern_qname(ns, n)
        };
        let name_idx = abc.intern_string(spec.name);
        let iinit = abc.add_method(MethodInfo {
            param_types: vec![0; spec.param_count as usize], return_type: 0,
            name: name_idx, flags: 0, options: vec![], param_names: vec![],
        });
        abc.add_body(MethodBody {
            method: iinit, max_stack: spec.max_stack, local_count: spec.local_count,
            init_scope_depth: 0, max_scope_depth: 1,
            code: spec.ctor.clone(), exceptions: vec![], traits: vec![],
        });
        let cinit = abc.add_method(MethodInfo {
            param_types: vec![], return_type: 0, name: 0, flags: 0, options: vec![], param_names: vec![],
        });
        abc.add_body(MethodBody {
            method: cinit, max_stack: 1, local_count: 1, init_scope_depth: 0, max_scope_depth: 1,
            code: empty_body.clone(), exceptions: vec![], traits: vec![],
        });

        let mut inst_traits: Vec<Trait> = Vec::new();
        for (slot, slot_name) in spec.slots.iter().enumerate() {
            let n = abc.intern_string(slot_name);
            let smn = abc.intern_qname(pub_ns, n);
            inst_traits.push(Trait {
                name: smn, kind_byte: 0x00, // Slot
                data: TraitKindData::Slot { slot_id: slot as u32 + 1, type_name: 0, vindex: 0, vkind: 0 },
                metadata: vec![],
            });
        }
        for ms in &spec.methods {
            let n = abc.intern_string(ms.name);
            let mmn = abc.intern_qname(pub_ns, n);
            let m = abc.add_method(MethodInfo {
                param_types: vec![0; ms.param_count as usize], return_type: 0,
                name: n, flags: 0, options: vec![], param_names: vec![],
            });
            abc.add_body(MethodBody {
                method: m, max_stack: ms.max_stack, local_count: ms.local_count,
                init_scope_depth: 0, max_scope_depth: 1,
                code: ms.code.clone(), exceptions: vec![], traits: vec![],
            });
            inst_traits.push(Trait {
                name: mmn, kind_byte: 0x01, // Method
                data: TraitKindData::Method { disp_id: 0, method: m },
                metadata: vec![],
            });
        }
        abc.instances.push(InstanceInfo {
            name: mn, super_name: spec.super_mn, flags: 0, protected_ns: 0,
            interfaces: vec![], iinit, traits: inst_traits,
        });
        abc.classes.push(ClassInfo { cinit, traits: vec![] });
        let classi = (abc.classes.len() - 1) as u32;

        // Declaring the trait reserves the name; the class OBJECT only exists once this script's
        // initialiser builds it against its base.
        // `initproperty` at the end stores INTO something, so the thing it stores into has to be
        // on the stack under the value: the script's own global object, which `getscopeobject 0`
        // pushes. Leaving it out is a stack underflow, and the player rejects the package for it
        // with no clue which method is at fault -- the whole file simply never finishes loading.
        let mut init = vec![OP_GETLOCAL0, OP_PUSHSCOPE, OP_GETSCOPEOBJECT, 0];
        op1(&mut init, OP_FINDPROPSTRICT, spec.super_mn);
        op1(&mut init, OP_GETPROPERTY, spec.super_mn);
        init.push(OP_DUP);
        init.push(OP_PUSHSCOPE);
        op1(&mut init, OP_NEWCLASS, classi);
        init.push(OP_POPSCOPE);
        op1(&mut init, OP_INITPROPERTY, mn);
        init.push(OP_RETURNVOID);

        let script_init = abc.add_method(MethodInfo {
            param_types: vec![], return_type: 0, name: 0, flags: 0, options: vec![], param_names: vec![],
        });
        abc.add_body(MethodBody {
            method: script_init, max_stack: 4, local_count: 1, init_scope_depth: 0, max_scope_depth: 2,
            code: init, exceptions: vec![], traits: vec![],
        });
        abc.scripts.push(ScriptInfo {
            init: script_init,
            traits: vec![Trait {
                name: mn, kind_byte: 0x04, // Class
                data: TraitKindData::Class { slot_id: 1, classi },
                metadata: vec![],
            }],
        });
    }

    crate::abc_codec::write(&abc)
}

/// Package the fixture the way SSF2 ships its own content: a raw zlib stream wrapping
///
/// ```text
/// u32 BE  inner SWF length
/// u32 BE  index entry count
/// N x u32 BE  index entries
/// <inner SWF>
/// ```
///
/// `build_fixture_swf` alone is enough for the converter, because `ssf::decompress` passes a bare
/// `FWS` stream through untouched. The game is stricter: it only ever sees `DAT<n>.ssf` in this
/// container, so a raw SWF dropped in its data directory is simply not a file it can open. Ship the
/// container and the same bytes serve both sides.
///
/// The index carries no entries. Every shipped archive lists some, but nothing in the load path
/// needs them for a stage this small, and an empty list keeps the packaging honest about what we
/// actually know rather than inventing plausible ids.
pub fn build_fixture_dat() -> Vec<u8> {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    let swf = build_fixture_swf();
    let mut raw = Vec::with_capacity(swf.len() + 8);
    raw.extend_from_slice(&(swf.len() as u32).to_be_bytes());
    raw.extend_from_slice(&0u32.to_be_bytes()); // index entry count
    raw.extend_from_slice(&swf);

    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&raw).expect("zlib write to a Vec cannot fail");
    enc.finish().expect("zlib finish to a Vec cannot fail")
}
