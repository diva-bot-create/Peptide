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
        // --- collision geometry, inside a `terrain` clip (the parser's collision container) ---
        rect_shape(1, -FLOOR_HALF_W, FLOOR_Y, FLOOR_HALF_W, FLOOR_Y + 40.0, grey),
        rect_shape(2, -200.0, PLATFORM_Y, 200.0, PLATFORM_Y + 20.0, blue),
        Tag::DefineSprite(swf::Sprite {
            id: 10,
            num_frames: 1,
            tags: vec![place(1, Some("floor"), 1), place(2, Some("platform"), 2), Tag::ShowFrame],
        }),
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

    tags.push(place(10, Some("terrain"), 1));
    tags.push(place(3, Some("deathBoundary"), 2));
    tags.push(place(4, Some("camBoundary"), 3));
    tags.extend(placements);
    tags.push(Tag::SymbolClass(
        beacons.iter().map(|(id, name)| swf::SymbolClassLink {
            id: *id,
            class_name: swf::SwfStr::from_utf8_str(Box::leak(name.clone().into_boxed_str())),
        }).collect(),
    ));
    tags.push(Tag::ShowFrame);

    let header = swf::Header {
        compression: swf::Compression::None,
        version: 10,
        stage_size: rect(-800.0, -600.0, 800.0, 600.0),
        frame_rate: Fixed8::from_f32(30.0),
        num_frames: 1,
    };
    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).expect("fixture swf");
    out
}
