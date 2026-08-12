//! stage_parser — extract a geometry model from an SSF2 stage `.ssf`.
//!
//! SSF2 stages are SWF resources: a root `stageMC` MovieClip holds a `terrain`
//! container (the collision geometry), named boundary clips (`deathBoundary`,
//! `camBoundary`, `smashBallBoundary`), and per-player `pN_Start` / `pN_Spawn`
//! beacon instances. We walk the placement tree accumulating the FULL affine
//! matrix down each branch, then read each instance's WORLD axis-aligned bounding
//! box (the referenced shape's local bounds transformed by the compounded matrix).
//!
//! Output is in **Fraymakers stage coordinates**: SSF2 world coords minus the
//! `stageMC` origin (FrayTools and SSF2 share a y-down pixel space at 1:1 scale —
//! the battlefield floor is ~520px wide in both). The FM stage emitter consumes
//! this model directly. Read-only on the input file.
//!
//! Reuses [`crate::ssf::decompress`] (the DAT-archive aware path; the raw `swf`
//! decompressor fails on the install's DATs).

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// A 2D affine matrix (SWF convention: x' = a·x + c·y + tx, y' = b·x + d·y + ty).
#[derive(Clone, Copy, Debug)]
struct Mat { a: f64, b: f64, c: f64, d: f64, tx: f64, ty: f64 }
impl Mat {
    fn id() -> Mat { Mat { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 } }
    /// `self ∘ other` — apply `other` first, then `self`.
    fn mul(&self, o: &Mat) -> Mat {
        Mat {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            tx: self.a * o.tx + self.c * o.ty + self.tx,
            ty: self.b * o.tx + self.d * o.ty + self.ty,
        }
    }
    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.tx, self.b * x + self.d * y + self.ty)
    }
    /// `+1.0` if this matrix preserves x-orientation, `-1.0` if mirrored (scaleX < 0).
    fn x_sign(&self) -> f64 { if self.a < 0.0 { -1.0 } else { 1.0 } }
    /// `(flip_x, flip_y)` for an AXIS-ALIGNED placement (a mirrored decorative element). A
    /// negative scale on an axis means the art is drawn reversed there; the raster path composites
    /// onto an axis-aligned AABB and so must mirror the tile to match. Rotated/skewed matrices (b/c
    /// nonzero) report `(false, false)` — a resize+blit can't represent a true affine warp, and
    /// background decals are effectively never rotated.
    fn flips(&self) -> (bool, bool) {
        let axis_aligned = self.b.abs() < 1e-3 && self.c.abs() < 1e-3;
        (axis_aligned && self.a < 0.0, axis_aligned && self.d < 0.0)
    }
}

/// An axis-aligned box in FM stage coordinates (x/y = top-left, w/h = size).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect { pub x: f64, pub y: f64, pub w: f64, pub h: f64 }
impl Rect {
    pub fn left(&self) -> f64 { self.x }
    pub fn top(&self) -> f64 { self.y }
    pub fn right(&self) -> f64 { self.x + self.w }
    pub fn bottom(&self) -> f64 { self.y + self.h }
}

/// A collision platform (the main floor or a soft platform).
#[derive(Clone, Debug)]
pub struct Platform {
    /// World AABB in FM coords.
    pub rect: Rect,
    /// `true` for a one-way (drop-through) soft platform; `false` for solid terrain.
    pub drop_through: bool,
    /// The walkable TOP surface as a polyline in FM coords (left to right), when the terrain
    /// is curved/sloped. `None` for a flat platform (use `rect.top()` as a level line).
    pub profile: Option<Vec<(f64, f64)>>,
    /// `true` if the SSF2 source moves this platform (its instance name carries `moving`).
    /// Emitted as a STATIC platform at its start position: SSF2 moving-platform motion is
    /// bespoke per stage (custom AS3 / timeline animation) and isn't ported yet.
    pub moving: bool,
    /// `true` if this platform needs a drawn grey surface (a hand-declared platform whose visual
    /// isn't in the stage art, e.g. bowserscastle's columns over lava). Parsed terrain platforms
    /// already have their art in the background, so they stay invisible collision.
    pub visible: bool,
    /// `true` for a standable floor that must not anchor the stage (a molten lake the fighter
    /// lands ON while its hazard hitbox burns them). Excluded from main-floor selection and
    /// engine-layer alignment; still real collision.
    pub hazard_floor: bool,
}

/// A stage hazard, emitted as a Fraymakers custom game object (a damaging hitbox volume the
/// stage spawns). Position is in FM coords; `w`/`h` size the hitbox; the rest are FM
/// HitboxStats values. `interval`/`active` drive a simple on/off pulse (0 interval = always on).
#[derive(Clone, Debug)]
pub struct Hazard {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub damage: f64,
    pub knockback: f64,
    pub angle: f64,
    /// Per-hitbox launch directions from the class's `getAttackStats` (the Thwomp's two boxes =
    /// 135 left / 45 right). Empty falls back to `angle`. Data-driven, so the emitter angles each
    /// HIT_BOX by what the SSF2 source declares instead of a hardcoded pair.
    pub hitbox_dirs: Vec<f64>,
    /// Animation labels the hazard class plays via `forceAttack(...)` — the code-referenced handle
    /// to its art clip (the clip whose frame labels match these), so the art comes from what the
    /// script animates, not a library keyword match. Empty falls back to the keyword path.
    pub anim_labels: Vec<String>,
    /// The engine-spawned faller cycle (entrance delay, landed wait, spawn period, column xs),
    /// stepped from the enemy + stage classes' own code. None for non-spawned hazards.
    pub faller: Option<crate::abc_parser::FallerCycle>,
    /// Behavior values stepped out of the hazard class's update()/initialize() — drives the emitted
    /// CGO script (shake amplitude, rise/fall speeds, self-platform, dust, sounds) from the hazard's
    /// own code rather than hardcoded template constants.
    pub behavior: crate::abc_parser::EnemyBehavior,
    /// The hazard class's update()/initialize() reconstructed as FM hscript via the character
    /// decompile→translate pipeline (gated reconstruction path; needs a field-state + FrameTimer
    /// pass before it runs). None when the class has no lifecycle methods.
    pub reconstructed_script: Option<String>,
    /// The SSF2 enemy class this hazard came from (`spawnEnemy(<Class>)`), e.g.
    /// `BowsersCastleLava` — used for readable emitted content ids. None for detected-only
    /// hazards with no class reference.
    pub class_name: Option<String>,
    /// The class's `getOwnStats` maxYSpeed (the SSF2 engine's fall-speed cap) — the terminal
    /// velocity the reconstruction's kinematics integrator honors. None = no cap declared.
    pub max_y_speed: Option<f64>,
    /// Period in frames of the on/off pulse (0 = always active).
    pub interval: u32,
    /// Frames the hitbox stays active within each `interval` (ignored when interval is 0).
    pub active: u32,
    /// Movement pattern: "static" / "oscillateX" / "oscillateY" / "circle" / "fall".
    pub motion: String,
    /// Movement amplitude (px) and period (frames) for the pattern.
    pub range: f64,
    pub period: u32,
    /// Frames between native-hitbox re-arms (so a lingering fighter keeps taking hits).
    /// = the SSF2 attack's `refreshRate`, frame-doubled.
    pub rehit: u32,
    /// FM knockbackGrowth (= the SSF2 attack's `kbConstant`, per the shared key map).
    pub kb_growth: f64,
    /// Display label (for the entity/layer names).
    pub label: String,
    /// The rasterized SSF2 hazard sprite (the real art), if recovered from the placement tree.
    /// `None` falls back to a generated placeholder box in the emitter.
    pub art: Option<StageArt>,
    /// The hazard's labelled sub-animations (e.g. a Thwomp's `entrance`/`fall`/`idle`), recovered
    /// from its SSF2 animation clip's frame labels. When present, the emitter builds a
    /// multi-animation custom game object whose Substate machine switches between them; empty falls
    /// back to the single-animation [`Hazard::art`].
    pub anims: Vec<ClipAnim>,
    /// The clip's SSF2 `CollisonBox`/`attackBox` shapes, clip-local and FM-scaled (one per box; the
    /// Thwomp's `fall` carries a left + right pair). The multi-animation emitter rides these on its
    /// damaging animation as HIT_BOX layers, so the hit volume is the real SSF2 attackBox geometry
    /// (recovered like the lava's) instead of the art canvas or a guessed size.
    pub attack_boxes: Vec<Rect>,
}

/// One labelled animation of an SSF2 movieclip (a frame-label segment, e.g. `idle`/`fall`), as a
/// sequence of rasterized frames. Universal: used for multi-animation hazard CGOs and any other
/// stage clip whose code switches animations by label. Frame lengths are SSF2 (30fps) units; the
/// emitter applies the same 30->60fps doubling as the rest of the stage.
#[derive(Clone, Debug)]
pub struct ClipAnim {
    /// FM animation name (the SSF2 frame label, camelCased + prefixed by the emitter).
    pub label: String,
    /// The animation's frames, in order (one rasterized image per source frame).
    pub frames: Vec<StageArt>,
    /// 0-based frame the SWF timeline stop()s on, if the sub-clip HOLDS instead of looping (its
    /// Flash-generated `_fla.` timeline class has a stopping frame script). `frames` is already
    /// truncated to this; the emitter maps it to AnimationEndType.NONE. None = the clip loops.
    pub stop_frame: Option<usize>,
    /// `setYSpeed` actions the sub-clip's frame scripts drive (1-based source frame, 30fps speed)
    /// — the thwomp entrance's descend/hover/rise bob. Empty for clips without such scripts.
    pub frame_velocities: Vec<(u32, f64)>,
}

impl Hazard {
    /// Re-arm cadence for the native hitbox, in frames (min 1).
    pub fn rehit(&self) -> u32 { self.rehit.max(1) }
}

/// Anchor points the placement walk records: the stage MC's world registration (the FM-space
/// origin) and the terrain MC's world registration (the GAME-space origin — the coordinate
/// space SSF2's AS3 spawn calls and runtime X/Y use).
#[derive(Default)]
struct WalkAnchors {
    origin: Option<(f64, f64)>,
    terrain: Option<(f64, f64)>,
}

/// A player beacon (match-start "entrance" or respawn point) in FM coords.
#[derive(Clone, Debug)]
pub struct SpawnPoint {
    /// 0-based player index (p1 -> 0 …).
    pub index: usize,
    pub x: f64,
    pub y: f64,
    /// `true` if the beacon faces left (SSF2 scaleX < 0).
    pub face_left: bool,
}

/// The parsed SSF2 stage geometry, in FM stage coordinates.
#[derive(Clone, Debug)]
pub struct StageModel {
    /// Content id (from `Main.id`, fallback file stem). The emitter may suffix this
    /// (`<id>ssf2`) so it can't shadow a built-in stage; `display_name` stays clean.
    pub id: String,
    /// Human display name for the stage-select screen (override map, else the SSF2 id
    /// title-cased).
    pub display_name: String,
    /// Source series (for the description), if known from the override map.
    pub series: Option<String>,
    /// The stage's original SSF2 soundtrack track ids (`bgm_*`), preserved in the
    /// description. Not playable in FM (the audio isn't shipped), recorded for the author.
    pub ssf2_music: Vec<String>,
    /// FM bgm resource ids the converted stage actually references (override map, else the
    /// configured default). Must be real public FM resources so the match can start.
    pub fm_music: Vec<String>,
    /// Collision platforms (floor + soft platforms), top surfaces are `rect.top()`.
    pub platforms: Vec<Platform>,
    /// Blast zone (KO boundary).
    pub death_box: Option<Rect>,
    /// Hard camera bounds.
    pub camera_box: Option<Rect>,
    /// Match-start beacons (SSF2 `pN_Start`), ordered by player index.
    pub entrances: Vec<SpawnPoint>,
    /// Respawn beacons (SSF2 `pN_Spawn`), ordered by player index.
    pub respawns: Vec<SpawnPoint>,
    /// Grabbable ledge x-positions `(left, right)` from the SSF2 `ledge_mc` instances,
    /// in FM coords. Used as the main floor's left/right endpoints.
    pub ledges: Option<(f64, f64)>,
    /// Stage hazards, each emitted as a Fraymakers custom game object the stage spawns. SSF2
    /// hazards are bespoke per-stage AS3 (lava, thwomps, …) so they aren't auto-ported; these
    /// are declared opt-in in `mappings/stage/metadata.jsonc` and become real, author-editable
    /// FM custom-game-object hazards (a damaging hitbox volume).
    pub hazards: Vec<Hazard>,
    /// Thwomp-style hazard target columns as FM x coords, converted from the GAME-space values
    /// in `metadata.jsonc` (read 1:1 from the stage class's `update` disasm). Empty when the
    /// stage has none; the emitter drives the falling-hazard cycle over them.
    pub sink_columns: Vec<f64>,
    /// The sinking-platform class's authored motion (sink/rise speeds, hold, depth), stepped from
    /// its own code; None when the stage has no SSF2Platform subclass.
    pub platform_behavior: Option<crate::abc_parser::PlatformBehavior>,
    /// Rendered stage art, split into depth layers (the painted backdrop, the main
    /// stage at character depth, and a foreground that draws in front of fighters).
    pub art: StageArtSet,
    /// Non-fatal validation notes: required SSF2 stage linkages that were missing from
    /// the source (e.g. no collision floor, no spawn beacons). Surfaced to the user.
    pub warnings: Vec<String>,
    /// SSF2 -> Fraymakers spatial scale (the `size_multiplier` knob, default 1.3). Fraymakers
    /// space is SSF2 space scaled up by this factor (characters are rendered at this scale,
    /// so the stage geometry + art must match it). Geometry coords are already scaled; the
    /// emitter renders the art IMAGE layers at this scale.
    pub scale: f64,
}

/// The stage art split by depth so the emitter can layer it around the characters
/// and parallax-scroll the background.
#[derive(Clone, Debug, Default)]
pub struct StageArtSet {
    /// Painted backdrop (SSF2 `<id>_bg` / `background`) as ORDERED per-element layers, back-to-
    /// front. SSF2 authors each animated backdrop element (lava, torches, embers, podoboos, ...)
    /// as its own movieclip on its own loop, so each becomes its own FM layer/animation rather
    /// than one baked composite. A layer with one frame is static (drawn once); more than one =
    /// that element animates and the emitter loops its frames at its own pace. The whole list
    /// moves 1:1 with the world (it carries the surface fighters stand on).
    pub background: Vec<BgLayer>,
    /// Camera-relative parallax layers (the SSF2 `<id>_bg` backdrop + the `_cambg` layers that
    /// `SSF2Stage.getCameraBackgrounds` returns), back-to-front. Each is its OWN plane with its
    /// OWN pan rate (SSF2 auto-derives it from the layer size). Empty for the 109/110 corpus
    /// stages with no camera backgrounds.
    pub parallax: Vec<ParallaxLayer>,
    /// The main stage art (terrain / platforms / props) at character depth. More than
    /// one frame when the source has animated clips (the emitter loops them).
    pub stage_frames: Vec<StageArt>,
    /// Art that draws in front of the fighters (SSF2 `foreground`), as a possibly-animated,
    /// semi-transparent overlay (bowserscastle's shimmering lava-glow sheet + lightmask tint). One
    /// frame = static, more = the emitter loops it. Empty = no foreground.
    pub foreground: Vec<StageArt>,
    /// OPAQUE foreground occluders that draw in front of the fighter but BEHIND the semi-transparent
    /// `foreground` tint: the near face of a standable structure SSF2 split off as a separate piece
    /// (bowscastle's bridge parapet) so a fighter standing on the deck is occluded from the front.
    /// Rendered at full alpha (a translucent occluder would show the fighter through solid brick).
    pub foreground_occluders: Vec<StageArt>,
}

impl StageArtSet {
    /// `true` if no layer rasterized (e.g. a stage with only bitmap fills we can't
    /// decode) — the emitter then falls back to a geometry placeholder.
    pub fn is_empty(&self) -> bool {
        self.background.is_empty() && self.parallax.is_empty()
            && self.stage_frames.is_empty() && self.foreground.is_empty()
    }
}

/// How a camera-background layer scrolls (mirrors SSF2 `VcamBGSettings.mode` =
/// Fraymakers `ParallaxMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParallaxMode {
    /// Straight parallax pan at `x_pan`/`y_pan` (`bg = camera * multiplier`). SSF2's `_cambg`
    /// discrete layers use this (the `autoPanMultiplier` feeds the pan).
    Pan,
    /// Anchored to the camera bounds, tiling to fill (SSF2 makes ±width copies). For a
    /// repeating/wrapping backdrop.
    Bounds,
}

/// One camera-background parallax layer: the composited art + its per-layer scroll mode and
/// pan rate (SSF2 auto-derives the rate from the layer's pixel size; see `parallax_pan`).
#[derive(Clone, Debug)]
pub struct ParallaxLayer {
    pub art: StageArt,
    pub mode: ParallaxMode,
    /// Fraction of the camera's movement this layer scrolls by (0 = screen-fixed).
    pub x_pan: f64,
    pub y_pan: f64,
}

/// One backdrop element as its own animated layer (the SSF2 movieclip model: each animated
/// backdrop object is separate, on its own loop). `name` is the SSF2 symbol id (e.g.
/// `bowsers_torches_lit_bg`) used to name the FM layer; `frames` is 1 (static) or the
/// element's RLE'd animation loop.
#[derive(Clone, Debug)]
pub struct BgLayer {
    pub name: String,
    pub frames: Vec<StageArt>,
}

/// A composited stage-art image ready to drop in as an IMAGE layer / parallax bg.
#[derive(Clone, Debug)]
pub struct StageArt {
    /// PNG bytes (RGBA).
    pub png: Vec<u8>,
    /// Top-left of the image in FM stage coords.
    pub x: f64,
    pub y: f64,
    /// Pixel dimensions (for parallax `originalBGWidth/Height`).
    pub w: u32,
    pub h: u32,
    /// Frames to display this image before advancing (FM 60fps frames). For an animated layer
    /// this is the run length of identical source frames times the 30->60fps doubling, so the
    /// loop matches the SSF2 duration and held frames read as pauses. 1 for a static layer.
    pub hold: u32,
}

impl StageModel {
    /// The main (solid) floor: the widest non-drop-through platform, if any.
    pub fn main_floor(&self) -> Option<&Platform> {
        // a hazard floor (a standable molten lake) never anchors the stage, however wide.
        self.platforms.iter().filter(|p| !p.drop_through && !p.hazard_floor)
            .max_by(|a, b| a.rect.w.total_cmp(&b.rect.w))
    }
}

/// One placed shape instance discovered during the tree walk.
#[derive(Clone)]
struct Instance {
    /// The placed `DefineShape` id (so its art can be rasterized later).
    shape_id: u16,
    /// PlaceObject instance name (e.g. `deathBoundary`), if any.
    inst_name: Option<String>,
    /// SWF SymbolClass name of the nearest named sprite ancestor (e.g.
    /// `battlefield_fla.battlefield_TerrainMC_5`). Fla-prefixed + auto-numbered, so it's a
    /// weak cross-stage signal; prefer [`Instance::plane`] for art and linkage suffixes for
    /// markers.
    sym_name: String,
    /// The stage-root plane this instance descends from, by INSTANCE name (the stable AS3
    /// slot: `terrain` / `background` / `foreground` / `shadowMask` / `reflectionMask`), or
    /// an extra root instance (`*_cambg` parallax, `stance`, decorations). The robust art
    /// classifier (instance names generalize across stages; symbol names do not).
    plane: Option<String>,
    /// World AABB in raw SSF2 coords.
    aabb: Rect,
    /// World center (raw SSF2 coords).
    cx: f64,
    cy: f64,
    /// `-1.0` if mirrored along x (facing left).
    x_sign: f64,
    /// Axis flips of this leaf's world placement (`scaleX`/`scaleY` < 0). The art is composited
    /// onto an axis-aligned AABB, so a flipped placement must mirror the rasterized tile to render
    /// the source orientation. Without this, a mirrored multi-frame element (e.g. a wall torch
    /// whose flame frames are cropped bitmaps) draws each frame un-mirrored and its content slides
    /// frame to frame (a false left/right wiggle).
    flip: (bool, bool),
    /// `true` if this shape descends from a `moving`-named ancestor (an SSF2 moving
    /// platform/foreground). The collision child is usually named `terrainGround_platform`,
    /// so the `moving` signal lives on the parent container and is propagated down.
    moving: bool,
    /// The hazard kind this shape descends from, if any. SSF2 hazards (lava/thwomp/…) are named
    /// sprite containers whose leaf shapes carry deeper auto-named symbols, so the hazard signal
    /// lives on an ancestor and is propagated down the subtree (like [`Instance::moving`]).
    hazard: Option<HazardKind>,
    /// World position of the nearest NAMED MovieClip ancestor (the placement point of this leaf's
    /// owning instance). Repeated placements of one symbol (e.g. 16 torch-ember emitters) share a
    /// `sym_name` but get DISTINCT anchors, so the art grouping can split them into one object per
    /// placement at its own position instead of merging them into one union-bounds image.
    inst_anchor: (f64, f64),
    /// The placement's SWF blend mode, inherited down the subtree (`None` = Normal). The
    /// compositor emulates the modes it can (HardLight) when baking.
    blend: Option<swf::BlendMode>,
    /// Hash of the placement-depth chain from the root to this leaf's owning clip. Two sibling
    /// clips placed at the SAME anchor (same symbol, same origin) still differ here, so the art
    /// grouping splits them into independent elements that each loop on their OWN period (a
    /// merged capture cuts every child whose cycle doesn't divide the parent's).
    inst_path: u64,
}

/// Parse the SSF2 stage at `path` into a [`StageModel`], rendering its art (read-only).
pub fn parse_stage(path: &Path) -> Result<StageModel> {
    parse_stage_opts(path, true)
}

/// Parse the SSF2 stage at `path`. `render_art` toggles the (relatively expensive)
/// art rasterization: the converter needs it, but a geometry-only pass (e.g. the
/// coverage test, or `--info`) skips decoding + compositing every shape/bitmap.
pub fn parse_stage_opts(path: &Path, render_art_flag: bool) -> Result<StageModel> {
    let raw = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let swf_data = ssf_decompress(&raw, path)?;
    let buf = swf::decompress_swf(&swf_data[..]).context("decompress SWF")?;
    let swf = swf::parse_swf(&buf).context("parse SWF")?;

    let meta = stage_package_metadata(&swf_data);
    let id = meta.as_ref().and_then(|m| m.id.clone()).unwrap_or_else(|| {
        path.file_stem().and_then(|s| s.to_str()).unwrap_or("stage").to_string()
    });
    let ssf2_music = meta.map(|m| m.music).unwrap_or_default();
    // the stage's AS3 plane assignments + spawned actors (authoritative; heuristic is the fallback).
    let abc_model = stage_abc_model(&swf_data);
    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
        match &abc_model {
            Some(m) => {
                eprintln!("[as3] doc_class={} planes={} actors={}", m.doc_class, m.planes.len(), m.actors.len());
                for (clip, plane) in &m.planes { eprintln!("  plane {:<10?} <- {clip}", plane); }
                for a in &m.actors { eprintln!("  actor {} @ (x={:?}, y={:?})", a.class_name, a.x, a.y); }
            }
            None => eprintln!("[as3] no SSF2Stage subclass found (heuristic fallback)"),
        }
    }
    // display name + FM music: the override map keyed by the SSF2 id, else title-case +
    // the default bgm. The original SSF2 soundtrack (ssf2_music) is preserved separately.
    let smeta = crate::mappings::stage_metadata();
    let entry = smeta.stages.get(&id);
    let display_name = entry.and_then(|e| e.name.clone()).unwrap_or_else(|| title_case(&id));
    let series = entry.and_then(|e| e.series.clone());
    let fm_music = match entry.map(|e| &e.music) {
        Some(m) if !m.is_empty() => m.clone(),
        _ => vec![smeta.default_music.clone()],
    };
    // hazards: a stage with declared hazards in metadata uses those verbatim (manual override,
    // full control); otherwise they're auto-detected from the placement tree below. SSF2 hazards
    // are bespoke AS3, so a hand-declared entry always wins over the heuristic. (built below, once
    // terrain_off + scale are known, so `game_coords` entries can be transformed to FM space.)
    let suppress_auto_hazards = entry.map(|e| e.no_hazards).unwrap_or(false);

    // SymbolClass id -> name; DefineShape bounds; DefineSprite tag lists.
    let mut sym_names: BTreeMap<u16, String> = BTreeMap::new();
    let mut shape_bounds: BTreeMap<u16, (f64, f64, f64, f64)> = BTreeMap::new();
    let mut sprites: BTreeMap<u16, &Vec<swf::Tag>> = BTreeMap::new();
    for tag in &swf.tags {
        match tag {
            swf::Tag::SymbolClass(links) => {
                for link in links {
                    let n = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                    if !n.is_empty() { sym_names.insert(link.id, n); }
                }
            }
            swf::Tag::DefineShape(s) => {
                let b = &s.shape_bounds;
                shape_bounds.insert(s.id, (
                    b.x_min.get() as f64 / 20.0, b.y_min.get() as f64 / 20.0,
                    b.x_max.get() as f64 / 20.0, b.y_max.get() as f64 / 20.0,
                ));
            }
            swf::Tag::DefineSprite(s) => { sprites.insert(s.id, &s.tags); }
            _ => {}
        }
    }
    // DefineShape registry, for rasterizing stage art.
    let mut shape_defs: BTreeMap<u16, &swf::Shape> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::DefineShape(s) = tag { shape_defs.insert(s.id, s); }
    }
    // debug (`PEPTIDE_DUMP_SHAPE=<ids>`): a shape's bounds + fill styles, gradients in full.
    if let Ok(ids) = std::env::var("PEPTIDE_DUMP_SHAPE") {
        for id in ids.split(',').filter_map(|x| x.trim().parse::<u16>().ok()) {
            let Some(sh) = shape_defs.get(&id) else { continue };
            let b = &sh.shape_bounds;
            eprintln!("[shape {id}] bounds px=({:.1},{:.1})..({:.1},{:.1})",
                b.x_min.to_pixels(), b.y_min.to_pixels(), b.x_max.to_pixels(), b.y_max.to_pixels());
            for (i, f) in sh.styles.fill_styles.iter().enumerate() {
                match f {
                    swf::FillStyle::Color(c) => eprintln!("  fill{i}: Color({},{},{},{})", c.r, c.g, c.b, c.a),
                    swf::FillStyle::LinearGradient(g) | swf::FillStyle::RadialGradient(g)
                    | swf::FillStyle::FocalGradient { gradient: g, .. } => {
                        let kind = match f { swf::FillStyle::LinearGradient(_) => "Linear", swf::FillStyle::RadialGradient(_) => "Radial", _ => "Focal" };
                        eprintln!("  fill{i}: {kind}Gradient matrix=[a={} b={} c={} d={} tx={:.1}px ty={:.1}px] spread={:?}",
                            g.matrix.a.to_f64(), g.matrix.b.to_f64(), g.matrix.c.to_f64(), g.matrix.d.to_f64(),
                            g.matrix.tx.to_pixels(), g.matrix.ty.to_pixels(), g.spread);
                        for r in &g.records {
                            eprintln!("    stop ratio={} rgba=({},{},{},{})", r.ratio, r.color.r, r.color.g, r.color.b, r.color.a);
                        }
                    }
                    swf::FillStyle::Bitmap { id: bid, .. } => eprintln!("  fill{i}: Bitmap({bid})"),
                }
            }
        }
    }
    // Bitmap registry (id -> (w, h, RGBA)) for shapes whose fill is a bitmap (stage
    // backgrounds are usually bitmaps, which `vector_raster` can't fill). Only decoded
    // when rendering art (decoding every stage's bitmaps is the slow part).
    let mut bitmaps: BTreeMap<u16, (u32, u32, Vec<u8>)> = BTreeMap::new();
    if render_art_flag {
        for tag in &swf.tags {
            match tag {
                swf::Tag::DefineBitsLossless(b) => {
                    if let Ok(rgba) = crate::image_extractor::decode_lossless(b) {
                        bitmaps.insert(b.id, (b.width as u32, b.height as u32, rgba));
                    }
                }
                swf::Tag::DefineBitsJpeg3(j) => {
                    if let Ok((w, h, rgba)) = crate::image_extractor::decode_jpeg3(j) {
                        bitmaps.insert(j.id, (w, h, rgba));
                    }
                }
                _ => {}
            }
        }
    }

    // PEPTIDE_STAGE_DEBUG dumps the SymbolClass linkage table; PEPTIDE_STAGE_TREE (in `walk`)
    // dumps the placement tree with each instance's resolved plane. Both gated, for stage RE.
    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
        eprintln!("=== SymbolClass linkage table ({} entries) for {} ===", sym_names.len(), id);
        for (cid, n) in &sym_names { eprintln!("  id={cid} linkage={n:?}"); }
    }

    // the AS3-derived plane map (empty if the stage has no parseable SSF2Stage subclass → the
    // name heuristic in plane_tag takes over).
    let planes: PlaneMap = abc_model.as_ref().map(|m| m.planes.clone()).unwrap_or_default();

    // Collect every placed shape instance (with world AABB) and find the stageMC origin.
    let mut instances: Vec<Instance> = Vec::new();
    let mut anchors = WalkAnchors::default();
    walk(&swf.tags, Mat::id(), &sym_names, &shape_bounds, &sprites, &planes, 0, None, None, false, None, &mut instances, &mut anchors);

    let (ox, oy) = anchors.origin.unwrap_or((275.0, 200.0)); // SWF stage center fallback
    // GAME-space origin: the terrain MC's registration relative to the stage MC. The stage's
    // own AS3 (spawnEnemy coords) and the engine's runtime X/Y are terrain-local, so
    // FM = (game + terrain_off) × scale. Verified live on bowserscastle: terrain at
    // stageMC-local (338.85, 381.7); ledge anchors confirm pure translation (scale 1.0).
    let terrain_off = anchors.terrain.map(|(tx, ty)| (tx - ox, ty - oy));
    // Fraymakers space = SSF2 space scaled up by `size_multiplier` (the same knob the
    // character converter scales sprites by, default 1.3), so the stage matches the
    // scaled-up fighters and the art fills the FM camera the way it did in SSF2.
    let scale = crate::mappings::character_stats().scaling.size_multiplier;
    let to_fm = |r: &Rect| Rect {
        x: (r.x - ox) * scale, y: (r.y - oy) * scale, w: r.w * scale, h: r.h * scale,
    };

    // declared hazards (built here so `game_coords` entries can use terrain_off + scale). a
    // game_coords hazard carries the verbatim SSF2 setX/setY + getOwnStats width/height
    // literals; convert to FM space the same way actor hazards / sink_columns do (terrain-local
    // + terrain_off, then × scale). without a terrain anchor we can't place it, so fall back to
    // the raw values treated as FM and let the bound-filter / a wrong position surface it.
    let meta_hazards: Vec<Hazard> = entry.map(|e| e.hazards.iter().enumerate().map(|(i, h)| {
        let (x, y, w, hh) = if h.game_coords {
            match terrain_off {
                Some(t) => ((h.x + t.0) * scale, (h.y + t.1) * scale, h.w * scale, h.h * scale),
                None => (h.x, h.y, h.w, h.h),
            }
        } else {
            (h.x, h.y, h.w, h.h)
        };
        Hazard {
            x, y, w, h: hh,
            damage: h.damage, knockback: h.knockback, angle: h.angle,
            interval: h.interval, active: h.active,
            motion: h.motion.clone().unwrap_or_else(|| "static".to_string()),
            range: h.range, period: h.period.max(1), rehit: h.rehit, kb_growth: 40.0,
            label: h.label.clone().unwrap_or_else(|| format!("Hazard {}", i + 1)),
            art: None, anims: vec![], attack_boxes: vec![], hitbox_dirs: vec![], anim_labels: vec![], faller: None, behavior: crate::abc_parser::EnemyBehavior::default(), reconstructed_script: None, class_name: None, max_y_speed: None,
        }
    }).collect()).unwrap_or_default();

    // --- platforms: `*platform*` = drop-through soft platform; otherwise any terrain /
    // collision shape (SSF2 stages name these inconsistently: TerrainMC, terrain_mc,
    // ChunkTerrain, CollisonBox [sic], ground) is a solid floor. Check `platform` first so
    // `terrainGround_platform` (both words) is classified drop-through.
    let mut platforms: Vec<Platform> = Vec::new();
    for inst in &instances {
        let sn = inst.sym_name.to_ascii_lowercase();
        // `back`/`fore`ground are ART, not collision — exclude them (they contain the
        // substring "ground", which would otherwise read as a floor).
        let is_art_bg = sn.contains("background") || sn.contains("foreground")
            // and exclude clearly-behind art PLANES: a background/backdrop/parallax shape named
            // with "ground"/"platform" (e.g. homeruncontest's `hrc_groundloop` field texture)
            // is scenery, not collision. (Foreground stays eligible — some stages put a
            // standable moving platform in the foreground plane.)
            || matches!(inst.plane.as_deref(), Some("background" | "backdrop" | "cambg"));
        // SSF2 moving platforms carry `moving` in the linkage (movingplatform_N,
        // tos_movingplatform_6, movingPlatformTerrain_14) — usually on the parent container,
        // not the collision child, so the walk propagates it down. We collide with them at
        // their start position; the motion itself is bespoke per stage and not ported yet.
        let moving = inst.moving;
        // a stage can flag terrain that is NOT a standable floor (lava/acid you fall into); skip it
        // from collision (it's handled as a hazard instead).
        if entry.map(|e| e.non_floor_terrain.iter().any(|s| sn.contains(&s.to_ascii_lowercase()))).unwrap_or(false) {
            continue;
        }
        // a molten lake (hazard floor) IS standable in SSF2 — the fighter lands ON it and the
        // lava hitbox above it does the damage — but must not anchor the stage (main floor /
        // engine-layer alignment), so it carries the flag.
        let hazard_floor = entry.map(|e| e.hazard_floors.iter().any(|s| sn.contains(&s.to_ascii_lowercase()))).unwrap_or(false);
        if !is_art_bg && sn.contains("platform") {
            platforms.push(Platform { rect: to_fm(&inst.aabb), drop_through: true, profile: None, moving, visible: false, hazard_floor });
        } else if !is_art_bg && ["terrain", "collison", "collision", "ground"].iter().any(|m| sn.contains(m)) {
            // solid terrain can be curved/sloped (e.g. a hilly island), so trace its top
            // surface as a polyline instead of a flat line.
            let profile = shape_defs.get(&inst.shape_id)
                .and_then(|s| floor_profile(s, &inst.aabb, ox, oy, scale));
            platforms.push(Platform { rect: to_fm(&inst.aabb), drop_through: false, profile, moving, visible: false, hazard_floor });
        }
    }
    // Dedupe near-coincident collision platforms: a moving-platform container MC (e.g.
    // `tos_movingplatform`) and the collision child inside it both match the platform/terrain
    // naming, so the same platform gets emitted twice (overlapping rects a few px apart). Drop
    // a platform when another of the same kind covers most of it; keep the larger. Distinct
    // platforms (battlefield's three soft platforms, stacked floors) don't overlap, so they stay.
    dedupe_platforms(&mut platforms);
    // hand-declared platforms (FM coords) for stages whose real standable surfaces are AS3-spawned
    // objects, not static terrain (e.g. bowserscastle's BowsersCastlePlatform columns over lava).
    for p in entry.map(|e| e.platforms.as_slice()).unwrap_or(&[]) {
        // collision is the top edge (rect.top()); the taller rect just gives the drawn grey block
        // visible height (a chunky stone platform, not a thin line).
        platforms.push(Platform {
            rect: Rect { x: p.x - p.w / 2.0, y: p.y, w: p.w, h: 64.0 },
            drop_through: p.drop_through, profile: None, moving: false, visible: true,
            hazard_floor: false,
        });
    }
    let moving_count = platforms.iter().filter(|p| p.moving).count();

    // --- boundaries: identified by the carried name (the boundary clip is placed with
    // a PlaceObject name like `deathBoundary`, which the walk carries down to its shape).
    let mut death_box = None;
    let mut camera_box = None;
    for inst in &instances {
        let label = inst.inst_name.as_deref().unwrap_or(&inst.sym_name).to_ascii_lowercase();
        if label.contains("deathboundary") { death_box = Some(to_fm(&inst.aabb)); }
        else if label.contains("camboundary") { camera_box = Some(to_fm(&inst.aabb)); }
    }

    // hazards: a hand-declared metadata entry wins (full manual control); otherwise auto-detect
    // placed hazards from the placement tree, unless the stage opts out (`no_hazards`). Auto-
    // detected hazards are filtered to the reachable area (inside the death box, with margin) so a
    // hazard clip parked off-screen at frame 0 (a thwomp resting below the pit) doesn't ship as a
    // phantom hitbox a fighter can never reach.
    let detected = detect_hazards(&instances, &to_fm, &shape_defs, &bitmaps, ox, oy, render_art_flag);
    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
        for (k, h) in &detected {
            eprintln!("[detected-hazard] {:?} ({:.1},{:.1}) {:.0}x{:.0} top_y={:.1} bottom_y={:.1}",
                k, h.x, h.y, h.w, h.h, h.y - h.h / 2.0, h.y + h.h / 2.0);
        }
    }
    // AS3-sourced hazards: the stage's own `spawnEnemy(<Class>)` calls (e.g. bowserscastle's
    // BowsersCastleLava + Thwomp). these win over the placement-tree heuristic when present.
    // Each actor's hazard clip (the SymbolClass whose linkage carries the kind keyword) supplies
    // its own collision box so the hitbox lands EXACTLY where SSF2 puts it (terrain-space).
    let actor_box = |class_name: &str| -> Option<Rect> {
        let kw = hazard_kind(class_name).map(|k| k.label().to_ascii_lowercase())?;
        sym_names.iter()
            .filter(|(id, name)| name.to_ascii_lowercase().contains(&kw) && sprites.contains_key(id))
            .find_map(|(id, _)| clip_attack_box(*id, &sprites, &sym_names, &shape_bounds))
    };
    let as3_hazards: Vec<Hazard> = abc_model.as_ref().map(|m| m.actors.iter()
        .filter_map(|a| actor_to_hazard(a, scale, terrain_off, actor_box(&a.class_name).as_ref())
            .or_else(|| {
                // a coordless AS3 actor — a random-drop / script-positioned hazard like the Thwomp
                // (`spawnEnemy(Thwomp)` with no literal setX/setY) — can't be placed from literals,
                // so actor_to_hazard yields None. Adopt the placement-tree DETECTED hazard of the
                // same kind so it still ships; its script positions it at runtime (the frame-0
                // parked spot is irrelevant). Without this it falls through every branch and the
                // whole hazard is silently dropped.
                let k = hazard_kind(&a.class_name)?;
                let mut h = detected.iter().find(|(dk, dh)| *dk == k && dh.art.is_some()).map(|(_, dh)| dh.clone())?;
                apply_enemy_stats(&mut h, a); // adopt the real declared hit params + animation labels
                Some(h)
            }))
        .collect()).unwrap_or_default();
    let hazards: Vec<Hazard> = if !meta_hazards.is_empty() {
        // hand-declared hazards win, but borrow a detected sprite of the same kind so a declared
        // thwomp renders as the real SSF2 thwomp (not a placeholder) at its declared position.
        meta_hazards.into_iter().map(|mut h| {
            if h.art.is_none() {
                if let Some(k) = hazard_kind(&h.label) {
                    h.art = detected.iter().find(|(dk, dh)| *dk == k && dh.art.is_some())
                        .and_then(|(_, dh)| dh.art.clone());
                }
            }
            h
        }).collect()
    } else if !as3_hazards.is_empty() {
        // AS3 spawnEnemy hazards: borrow a detected sprite of the same kind so each renders as the
        // real SSF2 actor, and keep only those inside the reachable area.
        let bound = death_box.or(camera_box);
        as3_hazards.into_iter().map(|mut h| {
            if let Some(k) = hazard_kind(&h.label) {
                h.art = detected.iter().find(|(dk, dh)| *dk == k && dh.art.is_some())
                    .and_then(|(_, dh)| dh.art.clone());
            }
            h
        }).filter(|h| h.motion != "static" || match &bound {
            // a MOVING hazard repositions itself at runtime (a Thwomp drops onto random columns; a
            // HyruleTornado sweeps across; a saw circles), so its frame-0 parked position is not
            // where it operates and must NOT gate it. Only a statically-placed hazard (lava sheet)
            // is reachability-checked, so a phantom off-screen static clip still doesn't ship.
            Some(b) => h.x >= b.x - 60.0 && h.x <= b.x + b.w + 60.0
                    && h.y >= b.y - 60.0 && h.y <= b.y + b.h + 60.0,
            None => true,
        }).collect()
    } else if suppress_auto_hazards {
        Vec::new()
    } else {
        // auto-detected: keep only hazards inside the reachable area (death box, with margin) so a
        // clip parked off-screen at frame 0 doesn't ship as a phantom hitbox.
        let bound = death_box.or(camera_box);
        detected.into_iter().map(|(_, h)| h).filter(|h| match &bound {
            Some(b) => h.x >= b.x - 60.0 && h.x <= b.x + b.w + 60.0
                    && h.y >= b.y - 60.0 && h.y <= b.y + b.h + 60.0,
            None => true,
        }).collect()
    };

    // --- hazard ANIMATIONS: recover each hazard's labelled sub-animations from its SSF2 clip so
    // the emitter can build a multi-animation Substate CGO (a Thwomp's entrance/fall/idle, not one
    // frozen pose). The source clip is the SymbolClass whose linkage matches the hazard's class
    // keyword and which carries frame labels; universal across hazards.
    let mut hazards = hazards;
    if render_art_flag {
        // the ABC blocks, for hold-vs-loop: a sub-clip that stop()s carries a Flash timeline class.
        let hz_abcs: Vec<crate::abc_parser::AbcFile> = crate::swf_parser::parse(&swf_data)
            .map(|s| s.abc_blocks.iter().filter_map(|b| crate::abc_parser::parse(b).ok()).collect())
            .unwrap_or_default();
        if let Ok(want) = std::env::var("PEPTIDE_DUMP_CLASS") {
            for abc in &hz_abcs { crate::abc_parser::dump_class(abc, &want); }
        }
        // a hazard's art clip is ENGINE-DRIVEN, not authored scenery: a clip placed inside another
        // SPRITE's timeline (the stage's background/terrain/foreground MCs) is scenery and must not
        // be adopted (bowserscastle's Bowser spectator, placed in the background MC, shares the lava
        // class's idle/wait/lose labels and would otherwise become the "art" of an invisible-region
        // hazard, doubling him onto the lava CGO). ROOT-timeline placements don't disqualify — an
        // engine-spawned hazard parks its clip at root frame 0 (the thwomp's below-stage rest spot).
        let scenery_ids: std::collections::BTreeSet<u16> = {
            let mut s = std::collections::BTreeSet::new();
            for tags in sprites.values() {
                for t in tags.iter() {
                    if let swf::Tag::PlaceObject(po) = t {
                        if let swf::PlaceObjectAction::Place(id) | swf::PlaceObjectAction::Replace(id) = po.action { s.insert(id); }
                    }
                }
            }
            s
        };
        for hz in hazards.iter_mut() {
            // CODE-DRIVEN art clip: the hazard CLASS plays its art via `forceAttack("<label>")`, so
            // the art clip is the one whose frame labels carry those labels (the thing the script
            // actually animates) — not a library symbol grabbed by keyword. Pick the clip matching
            // the MOST of the class's forceAttack labels. Only when the class declared none (or none
            // match) do we fall back to the old linkage-keyword heuristic.
            let clip = (!hz.anim_labels.is_empty()).then(|| {
                sprites.keys().filter(|id| !scenery_ids.contains(id)).filter_map(|id| {
                    let labels = crate::sprite_parser::extract_frame_labels_from_tags(sprites[id]);
                    let hits = hz.anim_labels.iter()
                        .filter(|l| labels.iter().any(|(fl, _)| fl.eq_ignore_ascii_case(l))).count();
                    (hits > 0).then_some((*id, hits))
                }).max_by_key(|(_, n)| *n).map(|(id, _)| id)
            }).flatten().or_else(|| {
                let kw = hz.label.to_ascii_lowercase();
                sym_names.iter()
                    .filter(|(id, name)| !scenery_ids.contains(id)
                        && name.to_ascii_lowercase().contains(&kw) && sprites.contains_key(id))
                    .map(|(id, _)| (*id, crate::sprite_parser::extract_frame_labels_from_tags(sprites[id]).len()))
                    .filter(|(_, n)| *n > 0)
                    .max_by_key(|(_, n)| *n)
                    .map(|(id, _)| id)
            });
            if let Some(cid) = clip {
                // recover the clip's real SSF2 attackBox shapes (clip-local), FM-scaled to ride the
                // damaging animation as HIT_BOXes — the data-driven hit volume, same recovery the
                // lava uses. The Thwomp's `fall` yields its left + right pair.
                hz.attack_boxes = clip_attack_boxes(cid, &sprites, &sym_names, &shape_bounds).into_iter()
                    .map(|r| Rect { x: r.x * scale, y: r.y * scale, w: r.w * scale, h: r.h * scale })
                    .collect();
                // origin 0,0: the frames stay in the clip's LOCAL space, so the emitter centres them
                // on the CGO (which is spawned at setX/setY) rather than the stage origin.
                let anims = extract_labeled_clip_anims(cid, &sprites, &sym_names, &shape_defs, &bitmaps, &planes, &hz_abcs, 0.0, 0.0);
                // only adopt a multi-animation CGO when it's genuinely animated (>1 label or any
                // multi-frame segment); a single static label is just the placeholder art.
                if anims.len() > 1 || anims.iter().any(|a| a.frames.len() > 1) {
                    hz.anims = anims;
                }
                if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
                    eprintln!("[hz-anims] {} clip={cid}: {:?}", hz.label,
                        hz.anims.iter().map(|a| (a.label.clone(), a.frames.len(), a.frames.first().map(|f| (f.w, f.h)))).collect::<Vec<_>>());
                }
            }
        }
    }

    // --- spawns: pN_Start -> entrances, pN_Spawn -> respawns (by symbol-class name).
    let mut entrances: Vec<SpawnPoint> = Vec::new();
    let mut respawns: Vec<SpawnPoint> = Vec::new();
    for inst in &instances {
        let sn = inst.sym_name.to_ascii_lowercase();
        if let Some(idx) = player_index(&sn, "_start_") {
            entrances.push(SpawnPoint { index: idx, x: (inst.cx - ox) * scale, y: (inst.cy - oy) * scale, face_left: inst.x_sign < 0.0 });
        } else if let Some(idx) = player_index(&sn, "_spawn_") {
            respawns.push(SpawnPoint { index: idx, x: (inst.cx - ox) * scale, y: (inst.cy - oy) * scale, face_left: inst.x_sign < 0.0 });
        }
    }
    entrances.sort_by_key(|s| s.index);
    respawns.sort_by_key(|s| s.index);

    // --- ledges: the SSF2 `ledge_mc_left` / `ledge_mc_right` beacons mark the floor's
    // grabbable edges (FM grabs ledges at the floor's left/right endpoints).
    let ledge_x = |needle: &str| instances.iter()
        .find(|i| i.sym_name.to_ascii_lowercase().contains(needle))
        .map(|i| (i.cx - ox) * scale);
    let ledges = match (ledge_x("ledge_mc_left"), ledge_x("ledge_mc_right")) {
        (Some(l), Some(r)) => Some((l.min(r), l.max(r))),
        _ => None,
    };

    if platforms.is_empty() {
        bail!("no collision geometry found in {} (not a recognised SSF2 stage?)", path.display());
    }

    // --- art: rasterize the stage's shapes, split into background / stage / foreground.
    // The PNGs stay native-resolution; placement is scaled here and the emitter renders the
    // IMAGE layers at `scale`, matching the geometry.
    let keep_foreground = entry.map(|e| e.keep_foreground).unwrap_or(false);
    // the main solid floor's top y (FM coords), so an engine-added standable bg layer (e.g.
    // bowserscastle's brick bridge) aligns its surface to where fighters actually stand.
    // hazard floors (a standable molten lake) never anchor, however wide.
    let main_floor_y = platforms.iter().filter(|p| !p.drop_through && !p.hazard_floor)
        .max_by(|a, b| a.rect.w.total_cmp(&b.rect.w)).map(|p| p.rect.y);
    let art = if render_art_flag {
        render_art_layers(&swf.tags, &sprites, &sym_names, &shape_defs, &bitmaps, ox, oy, scale, keep_foreground, &planes, main_floor_y)
    } else {
        StageArtSet::default()
    };

    // --- validate: the SSF2 source should carry the standard stage linkages, and the
    // parse should have recovered the playable bits. Non-fatal notes for the user.
    let mut warnings = validate_ssf2_linkages(&sym_names, &id);
    if death_box.is_none() { warnings.push("blast zone not parsed (no deathBoundary)".into()); }
    if camera_box.is_none() { warnings.push("camera bounds not parsed (no camBoundary)".into()); }
    if entrances.is_empty() { warnings.push("no match-start entrances parsed (pN_Start)".into()); }
    if !platforms.iter().any(|p| !p.drop_through) { warnings.push("no solid floor parsed".into()); }
    if moving_count > 0 {
        warnings.push(format!("{moving_count} moving platform(s) emitted as static (SSF2 \
            moving-platform motion is bespoke per stage and not ported yet)"));
    }

    // thwomp-style target columns: GAME (terrain-local) x values from the metadata (read 1:1
    // from the stage class's update disasm) -> FM x via the terrain origin.
    let sink_columns: Vec<f64> = match terrain_off {
        Some(t) => {
            // prefer the columns stepped from the stage class's own spawnEnemy code (the int-array
            // literal in its update); the hand-maintained metadata stays as the fallback.
            let from_code: Vec<f64> = hazards.iter()
                .filter_map(|h| h.faller.as_ref())
                .find(|f| !f.columns.is_empty())
                .map(|f| f.columns.iter().map(|x| (x + t.0) * scale).collect())
                .unwrap_or_default();
            if !from_code.is_empty() {
                if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
                    eprintln!("[sink-columns] from CODE: {from_code:?}");
                }
                from_code
            }
            else { entry.map(|e| e.sink_columns.iter().map(|x| (x + t.0) * scale).collect()).unwrap_or_default() }
        }
        None => Vec::new(),
    };

    // the sinking-platform class's authored motion (sink/rise/hold/depth), from its own code.
    let platform_behavior = crate::swf_parser::parse(&swf_data).ok().and_then(|sw| {
        sw.abc_blocks.iter()
            .filter_map(|b| crate::abc_parser::parse(b).ok())
            .find_map(|abc| crate::abc_parser::extract_platform_behavior(&abc))
    });
    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
        eprintln!("[platform-behavior] {platform_behavior:?}");
    }

    let mut model = StageModel { id, display_name, series, ssf2_music, fm_music, platforms, death_box, camera_box, entrances, respawns, ledges, hazards, sink_columns, platform_behavior, art, warnings, scale };
    extend_art_to_death_bounds(&mut model);
    Ok(model)
}

/// SSF2 fills the space between a painted hazard surface and the blast zone with runtime engine
/// effects (bowserscastle mounts an animated lava sheet + a `lavafade` gradient below the floor;
/// neither lives in the stage file), so an art layer that stops mid-hazard shows a hard edge with
/// the backdrop behind it in FM. Data-driven repair: any art layer whose bottom edge lands INSIDE
/// a static full-width hazard's declared region is that hazard's visible surface cut short —
/// extend it by repeating its bottom row (opaque down to the death bounds, then fading out), the
/// same visual role the runtime fade plays.
fn extend_art_to_death_bounds(model: &mut StageModel) {
    use image::{GenericImageView, RgbaImage};
    let Some(db) = model.death_box else { return };
    let scale = model.scale;
    let stage_w = model.platforms.iter().map(|p| p.rect.w).fold(0.0, f64::max);
    // static, stage-spanning hazard regions (the lava pool), FM coords centered on (x, y).
    let regions: Vec<Rect> = model.hazards.iter()
        .filter(|h| h.motion == "static" && h.w >= stage_w)
        .map(|h| Rect { x: h.x - h.w / 2.0, y: h.y - h.h / 2.0, w: h.w, h: h.h })
        .collect();
    if regions.is_empty() { return; }
    let fade_rows = 100u32;
    let extend = |art: &mut StageArt| {
        let bottom = art.y + art.h as f64 * scale;
        let inside = regions.iter().any(|r| bottom > r.top() + 4.0 && bottom < r.bottom() - 4.0
            && art.x < r.right() && art.x + art.w as f64 * scale > r.left());
        if !inside { return; }
        let gap_raw = ((db.bottom() - bottom) / scale).ceil();
        let opaque = gap_raw.clamp(0.0, 600.0) as u32;
        let add = opaque + fade_rows;
        let Ok(img) = image::load_from_memory(&art.png) else { return };
        let (w, h) = img.dimensions();
        let img = img.to_rgba8();
        let mut out = RgbaImage::new(w, h + add);
        image::imageops::overlay(&mut out, &img, 0, 0);
        for dy in 0..add {
            let f = if dy < opaque { 1.0 } else { 1.0 - (dy - opaque + 1) as f64 / fade_rows as f64 };
            for x in 0..w {
                let mut p = *img.get_pixel(x, h - 1);
                p[3] = (p[3] as f64 * f) as u8;
                out.put_pixel(x, h + dy, p);
            }
        }
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            if image::codecs::png::PngEncoder::new(&mut png)
                .write_image(out.as_raw(), w, h + add, image::ExtendedColorType::Rgba8).is_err() { return; }
        }
        art.png = png;
        art.h += add;
    };
    for a in model.art.foreground.iter_mut() { extend(a); }
    for l in model.art.background.iter_mut() {
        for a in l.frames.iter_mut() { extend(a); }
    }
}

/// Validate that the SSF2 source carries the linkages a playable stage needs. Returns a
/// note for each missing one (non-fatal: the converter fills fallbacks, but a gap usually
/// means a mis-parse or an unusual stage). Keyed off the SymbolClass linkage table.
fn validate_ssf2_linkages(sym_names: &BTreeMap<u16, String>, model_id: &str) -> Vec<String> {
    let all: Vec<String> = sym_names.values().map(|s| s.to_ascii_lowercase()).collect();
    let has = |needles: &[&str]| all.iter().any(|n| needles.iter().any(|m| n.contains(m)));
    let mut w = Vec::new();
    if !has(&["terrainmc", "terrain_mc", "collison", "collision"]) {
        w.push("SSF2 source has no collision-floor linkage (TerrainMC / CollisonBox)".into());
    }
    if !has(&["_start_"]) { w.push("SSF2 source has no pN_Start spawn beacons".into()); }
    if !has(&["boundary"]) { w.push("SSF2 source has no boundary_clip (blast/camera bounds)".into()); }
    if !has(&["_bg", "background"]) {
        w.push(format!("SSF2 source has no backdrop art linkage (`{model_id}_bg`)"));
    }
    w
}

/// Depth layer an art instance belongs to (back-to-front). `Backdrop` is the document-root
/// `<id>_bg` scene (the far sky), which composites WITH `Parallax` (the `_cambg` layers) into
/// one camera-relative background when parallax is present, so the rays/sun draw in front of
/// the sky instead of being occluded by it.
#[derive(Clone, Copy, PartialEq)]
enum ArtKind { Backdrop, Parallax, Background, Stage, Foreground }

/// Classify a *visual* art instance by its stage PLANE — the instance name of the stage-root
/// child it descends from (the stable AS3 slot: `terrain`/`background`/`foreground`/mask, or
/// an extra root instance). Instance names generalize across stages; fla-prefixed symbol
/// names do not. `*_cambg` = a camera-background parallax layer (SSF2
/// `SSF2Stage.getCameraBackgrounds`); `foreground`/`*_fg` = in front of fighters;
/// `background` = the fixed backdrop; decorations (clouds, lensflare, …) sit at stage depth.
/// Collision (`terrain`) and masks never reach here (filtered by [`is_non_art_plane`]).
fn art_kind(inst: &Instance) -> ArtKind {
    let plane = inst.plane.as_deref().unwrap_or("").to_ascii_lowercase();
    if plane.contains("cambg") {
        ArtKind::Parallax
    } else if plane == "foreground" || plane.contains("_fg") {
        ArtKind::Foreground
    } else if plane == "backdrop" {
        ArtKind::Backdrop
    } else if plane == "background" {
        ArtKind::Background
    } else {
        ArtKind::Stage
    }
}

/// Planes that are not visual art: collision (`terrain`), the shadow/reflection masks, and
/// spawn-pose beacons (`stance`). The plane is already normalized by [`plane_tag`].
fn is_non_art_plane(plane: Option<&str>) -> bool {
    matches!(plane, Some("terrain") | Some("mask") | Some("stance"))
}

/// The normalized stage plane an instance establishes for its subtree, from its INSTANCE
/// name (the AS3 slot vocabulary + the `_cambg` parallax / `_fg` foreground conventions),
/// or `None` to inherit the parent's plane. The one symbol-name exception: the document-
/// root backdrop container carries no instance name but the explicit linkage `<id>_bg`, so
/// an unnamed `*_bg` instance tags `background`. Instance/linkage signals generalize across
/// stages; fla-prefixed timeline symbol names do not.
fn plane_tag(inst_name: Option<&str>, sym: &str, planes: &PlaneMap) -> Option<&'static str> {
    let n = inst_name.unwrap_or("");
    // AS3-authoritative: if the stage's own `initialize` assigned this instance (or its symbol)
    // to a plane, that wins over the name heuristic (e.g. `bowsers_lightmask` -> foreground, not
    // the `mask` the heuristic would infer).
    if let Some(p) = planes.get(n).or_else(|| planes.get(sym)) {
        return Some(p.code());
    }
    let nl = n.to_ascii_lowercase();
    if nl.contains("cambg") { Some("cambg") }
    else if nl == "foreground" || nl.contains("_fg") { Some("foreground") }
    else if nl == "background" { Some("background") }
    else if nl == "terrain" { Some("terrain") }
    else if nl.contains("mask") { Some("mask") }
    else if nl == "stance" { Some("stance") }
    // the document-root backdrop container (`<id>_bg`, and `*_bg` sky props inside it) carries
    // no instance name. it sits BEHIND the stageMC `background` plane and holds the `_cambg`
    // parallax layers, so it gets its own `backdrop` tag (composited with the parallax when
    // present, otherwise drawn as the farthest fixed layer).
    else if n.is_empty() && sym.to_ascii_lowercase().ends_with("_bg") { Some("backdrop") }
    else { None }
}

// ── SSF2 stage linkage vocabulary ──
// A shipped SSF2 stage carries a consistent set of named symbols (linkage ids). Art
// classification and validation key off these names. Two families are NOT art:
//  - COLLISION masks: `*TerrainMC*`, `terrainGround_platform*`, `CollisonBox*` [sic],
//    `ledge_mc_*`. SSF2 renders them invisibly; Peptide maps them to FM collision boxes /
//    line segments. Drawing them as art double-draws a flat silhouette over the backdrop.
//  - SCAFFOLDING: `pN_Start`/`pN_Spawn` beacons, the death/camera `boundary_clip`,
//    off-screen `warningbounds_*`, the `itemGen_mc`, `shadowMask`/`reflectionMask`, the
//    `light_source_mc`, the `smashball` spawn. All non-visual.

/// SSF2 collision-geometry linkage (invisible in SSF2; becomes FM collision, never art).
fn is_collision_linkage(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    l.contains("terrainmc") || l.contains("terrain_mc") || l.contains("platform")
        || l.contains("collison") || l.contains("collision") || l.contains("ledge_mc")
}

/// SSF2 non-visual scaffolding linkage (spawn/boundary/warning/itemgen/mask/light/smashball).
fn is_scaffold_linkage(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    ["_start_", "_spawn_", "boundary", "warningbounds", "warning", "itemgen",
     "shadowmask", "reflectionmask", "light_source", "smashball"]
        .iter().any(|m| l.contains(m))
}

/// `true` if a placed instance is collision geometry or scaffolding (not stage art).
fn is_non_art(label: &str) -> bool {
    is_collision_linkage(label) || is_scaffold_linkage(label)
}

/// A damaging stage-hazard type, recognised from an SSF2 linkage name. Carries the FM hitbox
/// defaults (damage / knockback / angle / motion) the auto-detected hazard emits — tunable per
/// stage in `mappings/stage/metadata.jsonc`. SSF2 hazard behaviour is bespoke AS3; these are
/// best-effort defaults keyed off the hazard's nature.
#[derive(Clone, Copy, PartialEq, Debug)]
enum HazardKind { Lava, Acid, Spike, Saw, Thwomp, Podoboo, Tornado, Bumper, DamageZone, Piranha }

impl HazardKind {
    /// (motion, damage, knockback, angle) defaults for this hazard type.
    fn defaults(self) -> (&'static str, f64, f64, f64) {
        match self {
            // Lava/Thwomp: 1:1 from the bowserscastle getAttackStats disasm (the SSF2 key map:
            // power -> baseKnockback, direction -> angle). lava: damage 6, direction 90,
            // power 70. thwomp `fall`: damage 30, power 125 (two boxes, directions 135/45 —
            // the emitter splits them; 90 here is the single-box fallback).
            HazardKind::Lava       => ("static",      6.0, 70.0, 90.0),
            HazardKind::Acid       => ("static",     10.0, 50.0, 90.0),
            HazardKind::Spike      => ("static",      9.0, 60.0, 90.0),
            HazardKind::Saw        => ("circle",     12.0, 70.0, 45.0),
            HazardKind::Thwomp     => ("fall",       30.0, 125.0, 90.0),
            HazardKind::Podoboo    => ("oscillateY", 12.0, 70.0, 90.0),
            HazardKind::Tornado    => ("oscillateX",  8.0, 50.0, 80.0),
            HazardKind::Bumper     => ("static",      4.0, 90.0, 45.0),
            HazardKind::DamageZone => ("static",     12.0, 60.0, 90.0),
            HazardKind::Piranha    => ("oscillateY", 11.0, 60.0, 80.0),
        }
    }
    /// `(rehit frames @60fps, FM knockbackGrowth)` — from the SSF2 attack stats where known
    /// (refreshRate ×2, kbConstant 1:1 per the shared key map); generic 30/40 otherwise.
    fn hit_tuning(self) -> (u32, f64) {
        match self {
            HazardKind::Lava   => (30, 125.0), // refreshRate 15, kbConstant 125 (disasm)
            HazardKind::Thwomp => (60, 12.0),  // refreshRate 30, kbConstant 12 (disasm)
            _ => (30, 40.0),
        }
    }
    fn label(self) -> &'static str {
        match self {
            HazardKind::Lava => "Lava", HazardKind::Acid => "Acid", HazardKind::Spike => "Spikes",
            HazardKind::Saw => "Saw", HazardKind::Thwomp => "Thwomp", HazardKind::Podoboo => "Podoboo",
            HazardKind::Tornado => "Tornado", HazardKind::Bumper => "Bumper",
            HazardKind::DamageZone => "Damage Zone", HazardKind::Piranha => "Piranha Plant",
        }
    }
}

/// Classify an SSF2 linkage/instance name as a damaging hazard, or `None`. Cosmetic sub-clips
/// (sound, vfx, splash, the landing/impact/break/intro frames, embers/torches/particles) and
/// background art are excluded so one hazard isn't detected as several — the leaf shapes that
/// survive cluster into a single hazard by position. Deliberately conservative: only clearly
/// damaging hazards, since a misplaced or cosmetic false-positive ships a phantom hitbox.
fn hazard_kind(label: &str) -> Option<HazardKind> {
    let l = label.to_ascii_lowercase();
    // cosmetic / non-damaging sub-elements and scenery — never a hazard hitbox.
    const COSMETIC: &[&str] = &[
        "bg", "background", "foreground", "shadow", "mask", "sfx", "snd", "sound", "splash",
        "ember", "torch", "particle", "sparkle", "vfx", "glow", "hud", "cloud", "rain", "snow",
        "intro", "_land", "_hit", "_break", "_pop", "_wave", "_shake", "_entrance", "_entrace",
        "fill", "button", "door", "checkpoint", "screen", "building", "spectator", "balloon",
        "fairy", "sprite", "_egg", "explode", "_fly", "_wall", "bub", "_proj", "projectile",
        "effect", "_growl", "_roar", "_cry", "spit", "warp", "transition", "_boost", "_quake",
    ];
    if COSMETIC.iter().any(|m| l.contains(m)) { return None; }
    // damaging hazards by keyword (ordered: more specific first).
    let kind = if l.contains("thwomp") { HazardKind::Thwomp }
        else if l.contains("podobo") { HazardKind::Podoboo }
        else if l.contains("lava") || l.contains("magma") { HazardKind::Lava }
        else if l.contains("acid") { HazardKind::Acid }
        else if l.contains("spike") || l.contains("thorn") { HazardKind::Spike }
        else if l.contains("saw") { HazardKind::Saw }
        else if l.contains("tornado") || l.contains("nado") { HazardKind::Tornado }
        else if l.contains("bumper") { HazardKind::Bumper }
        else if l.contains("damagezone") || l.contains("damage_zone") { HazardKind::DamageZone }
        else if l.contains("piranha") { HazardKind::Piranha }
        else { return None };
    Some(kind)
}

/// Convert an AS3-spawned actor (`SSF2API.spawnEnemy(<Class>)`) into a Fraymakers hazard: the
/// class name picks the kind ([`hazard_kind`] -> damage/knockback/angle/motion), and the literal
/// `setX`/`setY` coords map to FM space. an actor with no literal coords (a random-drop Thwomp)
/// falls back to the top-center of the reachable bound so it still drops into play.
/// The collision box placed inside a hazard clip (the SSF2 `attackBox`: a `CollisonBox` child
/// sprite whose inner shape is the box), as a CLIP-LOCAL rect — the placement matrix applied to
/// the inner shape's bounds. Searched across all the clip's frames, one level of nesting deep
/// (SSF2 hazards put the box under a `stance` wrapper). Static data, live-verified: the
/// bowserscastle lava box computed here matches the running engine's attackBox exactly.
/// EVERY distinct `CollisonBox`/`attackBox` shape under a hazard clip (clip-local rects), in
/// depth-first placement order, deduped by rounded geometry (a box repeats across frames). A clip
/// can carry MORE than one — the Thwomp's `fall` has a left + right attackBox — so the multi-box
/// hazard emit can reproduce both instead of guessing.
fn clip_attack_boxes(
    clip_id: u16,
    sprites: &BTreeMap<u16, &Vec<swf::Tag>>,
    sym_names: &BTreeMap<u16, String>,
    shape_bounds: &BTreeMap<u16, (f64, f64, f64, f64)>,
) -> Vec<Rect> {
    fn walk(
        tags: &[swf::Tag], mat: Mat, depth: usize,
        sprites: &BTreeMap<u16, &Vec<swf::Tag>>,
        sym_names: &BTreeMap<u16, String>,
        shape_bounds: &BTreeMap<u16, (f64, f64, f64, f64)>,
        out: &mut Vec<Rect>,
    ) {
        if depth > 3 { return; }
        for frame in build_frames(tags) {
            for (id, local, name, _, _, _) in &frame {
                let m = mat.mul(local);
                let is_box = name.as_deref() == Some("attackBox")
                    || sym_names.get(id).is_some_and(|s| {
                        let l = s.to_ascii_lowercase();
                        l.contains("collison") || l.contains("collision")
                    });
                if is_box {
                    // the box sprite's inner shape, through its own placement matrix.
                    if let Some(btags) = sprites.get(id) {
                        for inner in build_frames(btags) {
                            for (sid, smat, _, _, _, _) in &inner {
                                if let Some((x0, y0, x1, y1)) = shape_bounds.get(sid) {
                                    let mm = m.mul(smat);
                                    let c = [mm.apply(*x0, *y0), mm.apply(*x1, *y0), mm.apply(*x1, *y1), mm.apply(*x0, *y1)];
                                    let xmn = c.iter().map(|p| p.0).fold(f64::MAX, f64::min);
                                    let xmx = c.iter().map(|p| p.0).fold(f64::MIN, f64::max);
                                    let ymn = c.iter().map(|p| p.1).fold(f64::MAX, f64::min);
                                    let ymx = c.iter().map(|p| p.1).fold(f64::MIN, f64::max);
                                    out.push(Rect { x: xmn, y: ymn, w: xmx - xmn, h: ymx - ymn });
                                }
                            }
                        }
                    }
                } else if let Some(child) = sprites.get(id) {
                    walk(child, m, depth + 1, sprites, sym_names, shape_bounds, out);
                }
            }
        }
    }
    let mut raw = Vec::new();
    if let Some(tags) = sprites.get(&clip_id) {
        walk(tags, Mat::id(), 0, sprites, sym_names, shape_bounds, &mut raw);
    }
    // dedup by rounded geometry, preserving first-seen (placement) order.
    let mut seen = std::collections::BTreeSet::new();
    let boxes: Vec<Rect> = raw.into_iter()
        .filter(|r| seen.insert((r.x.round() as i64, r.y.round() as i64, r.w.round() as i64, r.h.round() as i64)))
        .collect();
    if std::env::var("PEPTIDE_BOX_DEBUG").is_ok() {
        eprintln!("[boxes] clip={clip_id}: {}", boxes.iter().map(|r| format!("({:.0},{:.0} {:.0}x{:.0})", r.x, r.y, r.w, r.h)).collect::<Vec<_>>().join(" "));
    }
    boxes
}

/// The first attackBox under a hazard clip (the lava's single wide band). See [`clip_attack_boxes`].
fn clip_attack_box(
    clip_id: u16,
    sprites: &BTreeMap<u16, &Vec<swf::Tag>>,
    sym_names: &BTreeMap<u16, String>,
    shape_bounds: &BTreeMap<u16, (f64, f64, f64, f64)>,
) -> Option<Rect> {
    clip_attack_boxes(clip_id, sprites, sym_names, shape_bounds).into_iter().next()
}

fn actor_to_hazard(
    actor: &crate::stage_abc::SpawnedActor, scale: f64,
    terrain_off: Option<(f64, f64)>, attack_box: Option<&Rect>,
) -> Option<Hazard> {
    let kind = hazard_kind(&actor.class_name)?;
    let (motion, damage, knockback, angle) = kind.defaults();
    // The actor's setX/setY literals are GAME (terrain-local) coords; FM = (game + terrain_off)
    // × scale. With the clip's own collision box (clip-local), the hazard hitbox is placed
    // EXACTLY where SSF2 puts it. Verified live on bowserscastle (the computed lava box matches
    // the running engine's attackBox).
    // If any of those three reads is missing (no terrain anchor, a runtime/dynamic spawn position,
    // or no recoverable collision box), we CAN'T place the hazard exactly -> a declared gap, not an
    // invented box. a guessed band/default ships a wrong-but-plausible hitbox, which is worse than
    // none (nobody knows to fix it). such a hazard needs a live measurement to place; declare it in
    // the gap report instead.
    let (t, ax, ay, b) = (terrain_off?, actor.x?, actor.y?, attack_box?);
    let (x, y, w, h) = (
        (ax + b.x + b.w / 2.0 + t.0) * scale,
        (ay + b.y + b.h / 2.0 + t.1) * scale,
        b.w * scale,
        b.h * scale,
    );
    let (rehit, kb_growth) = kind.hit_tuning();
    let mut hz = Hazard {
        x, y, w, h, damage, knockback, angle,
        interval: 0, active: 20, motion: motion.to_string(),
        range: 0.0, period: 120, rehit, kb_growth, label: kind.label().to_string(), art: None, anims: vec![], attack_boxes: vec![], hitbox_dirs: vec![], anim_labels: vec![], faller: None, behavior: crate::abc_parser::EnemyBehavior::default(), reconstructed_script: None, class_name: None, max_y_speed: None,
    };
    apply_enemy_stats(&mut hz, actor);
    Some(hz)
}

/// Override a hazard's hit params + animation set with the spawned class's OWN declarations (the
/// authoritative SSF2 source) — `getAttackStats` (SSF2's key map: `power`→baseKnockback,
/// `kbConstant`→knockbackGrowth, `direction`→angle, `damage`→damage; per-box dirs kept for the dual
/// emit) and the `forceAttack` animation labels. A hazard whose class declares neither keeps its
/// per-kind default.
fn apply_enemy_stats(hz: &mut Hazard, actor: &crate::stage_abc::SpawnedActor) {
    hz.anim_labels = actor.anim_labels.clone();
    hz.behavior = actor.behavior.clone();
    hz.reconstructed_script = actor.reconstructed_script.clone();
    hz.faller = actor.faller.clone();
    hz.max_y_speed = actor.own_stats.get("maxYSpeed").copied();
    hz.class_name = Some(actor.class_name.clone());
    if let Some(h0) = actor.attack_hitboxes.first() {
        if let Some(&d) = h0.get("damage") { hz.damage = d; }
        if let Some(&p) = h0.get("power") { hz.knockback = p; }
        if let Some(&k) = h0.get("kbConstant") { hz.kb_growth = k; }
        if let Some(&dir) = h0.get("direction") { hz.angle = dir; }
        hz.hitbox_dirs = actor.attack_hitboxes.iter().filter_map(|h| h.get("direction").copied()).collect();
    }
}

/// Auto-detect placed hazards from the stage's shape instances: classify each by linkage
/// ([`hazard_kind`]), cluster the several leaf shapes of one hazard clip into a single hazard
/// (same-kind boxes that overlap or sit within `MERGE_GAP` px), and rasterize each cluster's real
/// SSF2 art (via [`composite_layer`]) as the hazard sprite. Positions/sizes come from the
/// placement tree (FM coords via `to_fm`); motion/damage from the kind. Returns the kind with each
/// hazard so a hand-declared hazard can borrow a detected sprite of the same kind.
fn detect_hazards(
    instances: &[Instance], to_fm: &impl Fn(&Rect) -> Rect,
    shape_defs: &BTreeMap<u16, &swf::Shape>, bitmaps: &BTreeMap<u16, (u32, u32, Vec<u8>)>,
    ox: f64, oy: f64, with_art: bool,
) -> Vec<(HazardKind, Hazard)> {
    // hazard leaf shapes (kind propagated from a named ancestor). Background/backdrop/parallax
    // planes are scenery (e.g. decorative bg podoboos), never a hitbox, so they're skipped.
    let hazard_insts: Vec<&Instance> = instances.iter().filter(|i| {
        i.hazard.is_some() && !matches!(i.plane.as_deref(), Some("background" | "backdrop" | "cambg"))
    }).collect();
    // cluster: group same-kind leaf shapes whose AABBs overlap or are within MERGE_GAP px (in FM
    // coords), keeping the instances so the cluster's art can be rasterized.
    const MERGE_GAP: f64 = 40.0;
    let mut clusters: Vec<(HazardKind, Rect, Vec<&Instance>)> = Vec::new();
    'next: for inst in hazard_insts {
        let k = inst.hazard.unwrap();
        let r = to_fm(&inst.aabb);
        for c in clusters.iter_mut() {
            if c.0 == k && rects_near(&c.1, &r, MERGE_GAP) {
                c.1 = union_rect(&c.1, &r); c.2.push(inst); continue 'next;
            }
        }
        clusters.push((k, r, vec![inst]));
    }
    clusters.into_iter().map(|(k, r, insts)| {
        let (motion, damage, knockback, angle) = k.defaults();
        let (rehit, kb_growth) = k.hit_tuning();
        let art = if with_art { composite_layer(&insts, shape_defs, bitmaps, ox, oy, None) } else { None };
        (k, Hazard {
            // hazard hitbox = the detected box center + size (FM coords; +y down).
            x: r.x + r.w / 2.0, y: r.y + r.h / 2.0, w: r.w.max(20.0), h: r.h.max(20.0),
            damage, knockback, angle, interval: 0, active: 20,
            motion: motion.to_string(), range: 60.0, period: 120, rehit, kb_growth,
            label: k.label().to_string(), art, anims: vec![], attack_boxes: vec![], hitbox_dirs: vec![], anim_labels: vec![], faller: None, behavior: crate::abc_parser::EnemyBehavior::default(), reconstructed_script: None, class_name: None, max_y_speed: None,
        })
    }).collect()
}

/// `true` if two rects overlap or are within `gap` px of each other on both axes.
fn rects_near(a: &Rect, b: &Rect, gap: f64) -> bool {
    a.x - gap < b.x + b.w && b.x - gap < a.x + a.w && a.y - gap < b.y + b.h && b.y - gap < a.y + a.h
}

/// Smallest rect covering both inputs.
fn union_rect(a: &Rect, b: &Rect) -> Rect {
    let x = a.x.min(b.x); let y = a.y.min(b.y);
    Rect { x, y, w: (a.x + a.w).max(b.x + b.w) - x, h: (a.y + a.h).max(b.y + b.h) - y }
}

/// One placed child in a sprite's timeline frame: `(character id, local matrix, name)`.
/// One placed child on a timeline frame.
/// The `Option<u16>` = the placement's `ratio` (the Flash IDE writes one on a sprite placed
/// as a GRAPHIC symbol for its frame-phase sync; a real MovieClip placement has none) — such a
/// child shows ONE fixed frame and never self-animates or runs its frame scripts.
/// (character id, local matrix, instance name, graphic frame, placement depth). The depth is the
/// SWF display-list slot — stable per child across frames, so it discriminates same-anchor
/// siblings (a clip whose children all sit at the clip origin, like a multi-emitter bubble set).
type PlacedChild = (u16, Mat, Option<String>, Option<u16>, u16, Option<swf::BlendMode>);

/// instance/symbol name -> AS3-assigned render plane (from `stage_abc::extract_stage`). empty when
/// the stage has no parseable SSF2Stage subclass, in which case `plane_tag` falls back to heuristics.
type PlaneMap = BTreeMap<String, crate::stage_abc::StagePlane>;

/// Build a sprite/root timeline: the placed-child state snapshotted at each `ShowFrame`
/// (Flash semantics — Place/Replace set a depth, Modify updates its matrix, Remove clears
/// it). At least one frame.
fn build_frames(tags: &[swf::Tag]) -> Vec<Vec<PlacedChild>> {
    let mut depth: std::collections::BTreeMap<u16, PlacedChild> = std::collections::BTreeMap::new();
    let mat_of = |po: &swf::PlaceObject| po.matrix.as_ref().map(|m| Mat {
        a: m.a.to_f64(), b: m.b.to_f64(), c: m.c.to_f64(), d: m.d.to_f64(),
        tx: m.tx.get() as f64 / 20.0, ty: m.ty.get() as f64 / 20.0,
    });
    let name_of = |po: &swf::PlaceObject| po.name.as_ref().map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252).to_string());
    let mut frames: Vec<Vec<PlacedChild>> = Vec::new();
    for tag in tags {
        match tag {
            swf::Tag::PlaceObject(po) => match &po.action {
                swf::PlaceObjectAction::Place(id) | swf::PlaceObjectAction::Replace(id) => {
                    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok()
                        && (po.clip_depth.is_some() || po.blend_mode.is_some() || po.color_transform.is_some()) {
                        eprintln!("[place-mod] id={} depth={} clip_depth={:?} blend={:?} cxform={}",
                            id, po.depth, po.clip_depth, po.blend_mode, po.color_transform.is_some());
                    }
                    // Replace (and move-style Place) without a matrix/name/ratio KEEPS the slot's
                    // existing state and only swaps the character — bowserscastle's bubble pool
                    // Replace()s 8 depth slots through bubble variants with mat=false; resetting
                    // to identity stacked all 8 at the clip origin.
                    let prev = depth.get(&po.depth);
                    let mat = mat_of(po).or_else(|| prev.map(|e| e.1)).unwrap_or(Mat::id());
                    let name = name_of(po).or_else(|| prev.and_then(|e| e.2.clone()));
                    // a `ratio` on a sprite placement = a Graphic symbol pinned to the FRAME the
                    // ratio selects (Flash's "Single Frame" setting); carried like the matrix.
                    let pinned = po.ratio.or_else(|| prev.and_then(|e| e.3));
                    let blend = po.blend_mode.or_else(|| prev.and_then(|e| e.5));
                    depth.insert(po.depth, (*id, mat, name, pinned, po.depth, blend));
                }
                swf::PlaceObjectAction::Modify => {
                    if let Some(e) = depth.get_mut(&po.depth) {
                        if let Some(m) = mat_of(po) { e.1 = m; }
                        if let Some(n) = name_of(po) { e.2 = Some(n); }
                    }
                }
            },
            swf::Tag::RemoveObject(r) => { depth.remove(&r.depth); }
            swf::Tag::ShowFrame => { frames.push(depth.values().cloned().collect()); }
            _ => {}
        }
    }
    if frames.is_empty() { frames.push(depth.values().cloned().collect()); }
    frames
}

/// Walk the placement tree at a fixed global frame (every animated sprite shows its
/// `global_frame % len` frame — Flash advances all clips together), collecting the placed
/// Collect the world position of every movieclip placement (for the even-desync rank map). Walks
/// the base layout (frame 0 of each clip) since clip POSITIONS are fixed across frames.
fn collect_clip_positions(
    children: &[PlacedChild], parent: Mat,
    sprite_frames: &BTreeMap<u16, Vec<Vec<PlacedChild>>>, out: &mut Vec<(i64, i64)>,
) {
    if out.len() > 100_000 { return; }
    for (id, local, _, _, _, _) in children {
        let world = parent.mul(local);
        if let Some(frames) = sprite_frames.get(id) {
            out.push((world.tx.round() as i64, world.ty.round() as i64));
            if let Some(f) = frames.first() {
                collect_clip_positions(f, world, sprite_frames, out);
            }
        }
    }
}

/// shape instances with world AABBs. Mirrors [`walk`] but frame-aware.
#[allow(clippy::too_many_arguments)]
fn walk_frame(
    children: &[PlacedChild], parent: Mat, global_frame: usize, carried_sym: Option<&str>,
    plane: Option<&str>, planes: &PlaneMap,
    sym_names: &BTreeMap<u16, String>, shape_defs: &BTreeMap<u16, &swf::Shape>,
    sprite_frames: &BTreeMap<u16, Vec<Vec<PlacedChild>>>, out: &mut Vec<Instance>, rec: usize,
    inst_anchor: (f64, f64), inst_path: u64, blend: Option<swf::BlendMode>,
    phase_rank: &std::collections::HashMap<(i64, i64), usize>,
) {
    if rec > 8 { return; }
    for (id, local, name, pinned, pdepth, pblend) in children {
        let world = parent.mul(local);
        let sym = sym_names.get(id).cloned().unwrap_or_default();
        // an instance establishes a plane for its subtree: the stage's AS3 plane map (authoritative)
        // first, then the instance-name heuristic, otherwise it inherits the parent's plane.
        let my_plane = plane_tag(name.as_deref(), &sym, planes).or(plane);
        if let Some(s) = shape_defs.get(id) {
            let b = &s.shape_bounds;
            let (x0, y0, x1, y1) = (b.x_min.get() as f64/20.0, b.y_min.get() as f64/20.0, b.x_max.get() as f64/20.0, b.y_max.get() as f64/20.0);
            let cs = [world.apply(x0, y0), world.apply(x1, y0), world.apply(x1, y1), world.apply(x0, y1)];
            let xmn = cs.iter().map(|c| c.0).fold(f64::MAX, f64::min);
            let xmx = cs.iter().map(|c| c.0).fold(f64::MIN, f64::max);
            let ymn = cs.iter().map(|c| c.1).fold(f64::MAX, f64::min);
            let ymx = cs.iter().map(|c| c.1).fold(f64::MIN, f64::max);
            out.push(Instance {
                shape_id: *id, inst_name: name.clone(), sym_name: carried_sym.unwrap_or("").to_string(),
                plane: my_plane.map(str::to_string),
                aabb: Rect { x: xmn, y: ymn, w: xmx - xmn, h: ymx - ymn },
                cx: world.tx, cy: world.ty, x_sign: world.x_sign(), flip: world.flips(),
                moving: false, // art-path instances classify by plane, not collision
                hazard: None,  // hazards come from the geometry walk, not the art-frame walk
                inst_anchor,
                inst_path,
                blend: pblend.or(blend),
            });
        }
        if let Some(frames) = sprite_frames.get(id) {
            let next = name.as_deref().or(if sym.is_empty() { carried_sym } else { Some(&sym) }).map(|s| s.to_string());
            if let Ok(pat) = std::env::var("PEPTIDE_WALK_TRACE") {
                if next.as_deref().is_some_and(|n| n.to_ascii_lowercase().contains(&pat.to_ascii_lowercase())) {
                    eprintln!("[walk-trace] clip id={id} pdepth={pdepth} path={inst_path} name={:?} sym={:?} plane={:?} frames={} at=({:.0},{:.0})",
                        name, sym, my_plane, frames.len(), world.tx, world.ty);
                }
            }
            // EVERY movieclip placement is its own instance: its world position becomes the anchor
            // for its subtree, so each repeated clip (the 16 ember emitters, the 8 wall torches --
            // even when unnamed) splits into its OWN object at its own position, instead of being
            // merged into the parent element. each then animates + stabilizes independently (a torch
            // can't drift left/right because the whole row's average shifts -- it's its own clip).
            let child_anchor = (world.tx, world.ty);
            let child_path = inst_path.wrapping_mul(31).wrapping_add(*pdepth as u64 + 1);
            // even desync: each clip's start frame from its position-rank via the golden ratio (a
            // low-discrepancy sequence), so a row of repeated clips is spread EVENLY across its loop
            // and their per-frame leans cancel (the row reads as still). see `phase_rank` above.
            let rank = phase_rank.get(&(world.tx.round() as i64, world.ty.round() as i64)).copied().unwrap_or(0);
            let phase = ((rank as f64 * 0.618_033_988_75).fract() * frames.len() as f64) as usize;
            // a graphic-pinned placement shows the frame its ratio SELECTS (Flash's Single Frame
            // choice), always — it never self-animates.
            let f = match pinned {
                Some(r) => &frames[*r as usize % frames.len()],
                None => &frames[(global_frame + phase) % frames.len()],
            };
            walk_frame(f, world, global_frame, next.as_deref(), my_plane, planes, sym_names, shape_defs, sprite_frames, out, rec + 1, child_anchor, child_path, pblend.or(blend), phase_rank);
        }
    }
}

/// Rasterize the stage's art into background / stage / foreground layers. The stage layer
/// is frame-aware: if the source has animated clips it renders multiple frames (the
/// emitter loops them). Background/foreground are single-frame (frame 0).
#[allow(clippy::too_many_arguments)]
fn render_art_layers(
    root_tags: &[swf::Tag],
    sprites: &BTreeMap<u16, &Vec<swf::Tag>>,
    sym_names: &BTreeMap<u16, String>,
    shape_defs: &BTreeMap<u16, &swf::Shape>,
    bitmaps: &BTreeMap<u16, (u32, u32, Vec<u8>)>,
    ox: f64, oy: f64, scale: f64,
    keep_foreground: bool,
    planes: &PlaneMap,
    surface_y_fm: Option<f64>,
) -> StageArtSet {
    // per-sprite + root frame timelines.
    let mut sprite_frames: BTreeMap<u16, Vec<Vec<PlacedChild>>> = BTreeMap::new();
    for (id, tags) in sprites { sprite_frames.insert(*id, build_frames(tags)); }
    let root_frames = build_frames(root_tags);

    if let Ok(want) = std::env::var("PEPTIDE_CLIP_ANIMS") {
        for (id, name) in sym_names.iter() {
            if !name.to_ascii_lowercase().contains(&want.to_ascii_lowercase()) { continue; }
            if let Some(tags) = sprites.get(id) {
                let labels = crate::sprite_parser::extract_frame_labels_from_tags(tags);
                let frames = build_frames(tags);
                eprintln!("[clip] {name} id={id}: {} frames; labels={labels:?}", frames.len());
            }
        }
    }

    // full animation length = longest timeline. Port EVERY source frame 1:1 (no sampling): each
    // SSF2 frame becomes one keyframe of length 1, and the shared 30fps->60fps doubling
    // (entity_gen::double_keyframe_lengths, exactly the character port's tool) stretches each to
    // length 2. Identical consecutive frames RLE into one longer hold. Shorter-looping elements
    // repeat within the global timeline, matching SSF2's per-clip frame advancement.
    let full_len = sprite_frames.values().map(|f| f.len())
        .chain(std::iter::once(root_frames.len())).max().unwrap_or(1);
    let n_samples = full_len;
    let sample_frame = |i: usize| -> usize { i };

    // The ROOT timeline usually has just ONE content frame (the backdrop + stageMC placements);
    // many SSF2 stages reserve frame 0 as an empty preloader and put the content on frame 1.
    // Sampling the root by `g % root_len` can then miss the content frame entirely (when the
    // global stride and root length are both even, every sample lands on the empty frame 0),
    // dropping the whole backdrop. So anchor the root at its RICHEST frame (the content layout);
    // the sprites INSIDE still cycle by the global frame, so animation is preserved.
    let root_idx = root_frames.iter().enumerate().max_by_key(|(_, f)| f.len()).map(|(i, _)| i).unwrap_or(0);

    // EVEN DESYNC: SSF2 starts each repeated decorative clip (the row of wall torches/lamps, the
    // embers) on a different frame so the row doesn't flicker in unison. rank every clip by world
    // position, then give each a low-discrepancy (golden-ratio) start phase. ranking + golden ratio
    // spreads a row of repeated clips EVENLY across its loop, so their per-frame leans cancel and
    // the row reads as still -- unlike syncing them (they all lean together) or a linear position
    // offset (adjacent clips one frame apart = a gradient that travels across the row as a wave).
    let phase_rank = {
        let mut pos: Vec<(i64, i64)> = Vec::new();
        collect_clip_positions(&root_frames[root_idx], Mat::id(), &sprite_frames, &mut pos);
        pos.sort_unstable();
        pos.dedup();
        pos.into_iter().enumerate().map(|(r, p)| (p, r)).collect::<std::collections::HashMap<(i64, i64), usize>>()
    };

    // instances at a given global frame, classified + composited per layer.
    let frame_instances = |g: usize| -> Vec<Instance> {
        let root = &root_frames[root_idx];
        let mut out = Vec::new();
        walk_frame(root, Mat::id(), g, None, None, planes, sym_names, shape_defs, &sprite_frames, &mut out, 0, (0.0, 0.0), 0, None, &phase_rank);
        // exclude non-art PLANES (terrain/masks/spawns, by instance name) and any stray
        // collision/scaffolding markers (by linkage suffix) that slipped into an art plane.
        if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
            for i in out.iter().filter(|i| !shape_defs.contains_key(&i.shape_id)) {
                eprintln!("[no-shapedef] id={} plane={:?} aabb=({:.0},{:.0} {:.0}x{:.0})",
                    i.shape_id, i.plane, i.aabb.x, i.aabb.y, i.aabb.w, i.aabb.h);
            }
        }
        out.retain(|i| shape_defs.contains_key(&i.shape_id)
            && !is_non_art_plane(i.plane.as_deref())
            && !is_non_art(i.inst_name.as_deref().unwrap_or("")) && !is_non_art(&i.sym_name));
        out
    };
    let composite = |insts: &[Instance], kinds: &[ArtKind]| -> Option<StageArt> {
        let group: Vec<&Instance> = insts.iter().filter(|i| kinds.contains(&art_kind(i))).collect();
        // PNG stays native-resolution; only the placement is scaled (the emitter renders the
        // IMAGE layer at `scale`), so the art and geometry share one scale with no upscaling.
        composite_layer(&group, shape_defs, bitmaps, ox, oy, None).map(|mut a| {
            a.x *= scale; a.y *= scale; a
        })
    };

    // sample every frame's instance set + the composited image.
    let sampled: Vec<(Vec<Instance>, Option<StageArt>)> = (0..n_samples).map(|i| {
        let insts = frame_instances(sample_frame(i));
        let img = composite(&insts, &[ArtKind::Stage]);
        (insts, img)
    }).collect();
    let stage_counts: Vec<usize> = sampled.iter()
        .map(|(insts, _)| insts.iter().filter(|x| art_kind(x) == ArtKind::Stage).count()).collect();
    let max_count = stage_counts.iter().copied().max().unwrap_or(0);

    // background / foreground / parallax from the richest frame — the one with the most ART of
    // ANY plane (an animated stage's frame 0 is often empty; many stages put ALL their art in
    // the backdrop/background plane, so counting only Stage-plane art would leave base_idx on
    // an empty frame and drop the whole backdrop — half the corpus rendered as a placeholder).
    let total_counts: Vec<usize> = sampled.iter().map(|(insts, _)| insts.len()).collect();
    let base_idx = total_counts.iter().enumerate().max_by_key(|(_, c)| **c).map(|(i, _)| i).unwrap_or(0);
    let base_insts = &sampled[base_idx].0;
    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
        eprintln!("=== art instances (richest frame {base_idx}, {} insts) ===", base_insts.len());
        for i in base_insts {
            let k = match art_kind(i) { ArtKind::Backdrop => "BKDRP", ArtKind::Parallax => "PARLX", ArtKind::Background => "BG", ArtKind::Stage => "STAGE", ArtKind::Foreground => "FG" };
            eprintln!("  [{k:5}] plane={:?} sym={:?} name={:?} shape={} aabb=({:.0},{:.0} {:.0}x{:.0})",
                i.plane, i.sym_name, i.inst_name, i.shape_id, i.aabb.x, i.aabb.y, i.aabb.w, i.aabb.h);
        }
    }
    // The SSF2 stageMC `foreground` plane is the structure's FRONT FACE, drawn over the same
    // `background` structure for SSF2's 2.5D depth. Drawn as an FM foreground (in front of
    // fighters) it re-draws the whole platform on top of them — reads as a duplicate platform.
    // So fold a foreground that substantially overlaps the background plane back INTO the
    // background (behind fighters, composited once). Distinct foreground PROPS (offset `*_fg`
    // trees, bushes) keep a low overlap and stay in front, where they belong.
    // build the union from the SIGNIFICANT background instances only — the main structure.
    // small scattered background props (e.g. junglehijinx's flying-bird sprites way off to the
    // sides) would otherwise inflate the union and make a distinct foreground prop read as
    // ">60% inside the structure" and wrongly fold. Keep background instances at least 25% of
    // the largest's area.
    let bg_union: Option<(f64, f64, f64, f64)> = {
        let bgs: Vec<&Instance> = base_insts.iter().filter(|i| art_kind(i) == ArtKind::Background).collect();
        let max_area = bgs.iter().map(|i| i.aabb.w * i.aabb.h).fold(0.0, f64::max);
        let big: Vec<&&Instance> = bgs.iter().filter(|i| i.aabb.w * i.aabb.h >= max_area * 0.25).collect();
        (!big.is_empty()).then(|| (
            big.iter().map(|i| i.aabb.left()).fold(f64::MAX, f64::min),
            big.iter().map(|i| i.aabb.top()).fold(f64::MAX, f64::min),
            big.iter().map(|i| i.aabb.right()).fold(f64::MIN, f64::max),
            big.iter().map(|i| i.aabb.bottom()).fold(f64::MIN, f64::max),
        ))
    };
    // fraction of `a`'s area that lies inside the background union.
    let frac_in_bg = move |a: &Rect| -> f64 {
        let Some((l, t, r, b)) = bg_union else { return 0.0 };
        let ix = (a.right().min(r) - a.left().max(l)).max(0.0);
        let iy = (a.bottom().min(b) - a.top().max(t)).max(0.0);
        let area = a.w * a.h;
        if area <= 0.0 { 0.0 } else { (ix * iy) / area }
    };
    // a foreground that substantially overlaps the structure is normally a re-drawn front face
    // (fold it behind fighters to avoid a duplicate platform); `keep_foreground` opts a stage out
    // (its foreground is a real overlay, e.g. bowserscastle's glowing lava sheet) so it stays in front.
    let is_dup_fg = move |i: &Instance| !keep_foreground && art_kind(i) == ArtKind::Foreground && frac_in_bg(&i.aabb) >= 0.6;
    // composite an explicit instance group (back-to-front = walk order), scaled like `composite`.
    let composite_grp = |group: Vec<&Instance>, bounds: Option<(f64, f64, f64, f64)>| -> Option<StageArt> {
        composite_layer(&group, shape_defs, bitmaps, ox, oy, bounds).map(|mut a| { a.x *= scale; a.y *= scale; a })
    };
    // union raw bounds of an instance set (None if empty) — the fixed canvas for an animated layer.
    let union_bounds = |insts: &[&Instance]| -> Option<(f64, f64, f64, f64)> {
        (!insts.is_empty()).then(|| (
            insts.iter().map(|i| i.aabb.left()).fold(f64::MAX, f64::min),
            insts.iter().map(|i| i.aabb.top()).fold(f64::MAX, f64::min),
            insts.iter().map(|i| i.aabb.right()).fold(f64::MIN, f64::max),
            insts.iter().map(|i| i.aabb.bottom()).fold(f64::MIN, f64::max),
        ))
    };

    // one SSF2 source frame = one hold unit (the 30fps->60fps doubling is applied uniformly by the
    // emitter, exactly like the character port). Consecutive identical frames RLE into one longer
    // hold so a held source frame reads as a pause.
    let base_hold = 1u32;
    let rle = |frames: Vec<StageArt>| -> Vec<StageArt> {
        if frames.len() <= 1 { return frames; }
        let mut out: Vec<StageArt> = Vec::new();
        for mut f in frames {
            f.hold = base_hold;
            match out.last_mut() {
                Some(prev) if prev.png == f.png => prev.hold += base_hold,
                _ => out.push(f),
            }
        }
        out
    };
    // The smallest period at which a captured frame sequence repeats. Each backdrop ELEMENT loops
    // on its OWN movieclip clock, but we sample every element over `n_samples` (the longest clip on
    // the stage), so a short element loop comes back repeated N times. Truncating to one period
    // gives the element's REAL loop length, so its emitted VFX loops at its own rate instead of the
    // stage's (a 6-frame ember stays 6 frames, not 142). Returns the full length if it never
    // repeats within the sample window.
    let loop_period = |frames: &[StageArt]| -> usize {
        let n = frames.len();
        (1..n).find(|&p| (0..n - p).all(|i| frames[i].png == frames[i + p].png)).unwrap_or(n)
    };
    // Group a set of bg-plane instances into ORDERED per-element layers (the SSF2 movieclip
    // model: each animated backdrop object is its own clip). `member` selects bg-plane art;
    // each distinct `sym_name` becomes one layer in first-appearance (back-to-front) draw order.
    // An element that changes across the sampled frames keeps its own animation loop; a static
    // one collapses to its single base-frame image.
    let group_bg_layers = |member: &dyn Fn(&Instance) -> bool| -> Vec<BgLayer> {
        // one layer per distinct PLACEMENT (sym_name + instance anchor), not per symbol: repeated
        // placements of one symbol (16 torch-ember emitters at 16 wall positions) become 16 objects
        // each at its own position, instead of one merged union-bounds image (which both desyncs
        // their independent loops and wastes a giant mostly-empty texture). a single-instance symbol
        // is one group, same as before.
        let akey = |i: &Instance| (i.sym_name.clone(), i.inst_anchor.0.round() as i64, i.inst_anchor.1.round() as i64, i.inst_path);
        let mut order: Vec<(String, i64, i64, u64)> = Vec::new();
        let mut seen: std::collections::BTreeSet<(String, i64, i64, u64)> = std::collections::BTreeSet::new();
        for i in base_insts.iter().filter(|i| member(i)) {
            let k = akey(i);
            if seen.insert(k.clone()) { order.push(k); }
        }
        order.iter().filter_map(|(sname, ax, ay, apath)| {
            let matches = |i: &Instance| member(i) && &i.sym_name == sname
                && i.inst_anchor.0.round() as i64 == *ax && i.inst_anchor.1.round() as i64 == *ay
                && i.inst_path == *apath;
            // fixed canvas = the union of THIS placement's bounds across ALL frames, so the layer
            // stays put while its content animates (no wiggle as the flame bbox flickers).
            let all: Vec<&Instance> = sampled.iter()
                .flat_map(|(insts, _)| insts.iter().filter(|i| matches(i))).collect();
            let bounds = union_bounds(&all);
            // a frame where the element is INVISIBLE (a bubble between pops, a podoboo between
            // leaps) is a real frame of its loop: emit it as a blank, not a gap. dropping it
            // both broke the animated test (collapsing the element to one static frame) and
            // compressed the quiet stretches out of the loop.
            let blank = || bounds.map(|(l, t, _, _)| StageArt {
                png: blank_png(), x: (l - ox) * scale, y: (t - oy) * scale, w: 1, h: 1, hold: 1,
            });
            let per_frame: Vec<StageArt> = sampled.iter().filter_map(|(insts, _)| {
                let grp: Vec<&Instance> = insts.iter().filter(|i| matches(i)).collect();
                composite_grp(grp, bounds).or_else(blank)
            }).collect();
            let animated = per_frame.len() == n_samples
                && per_frame.windows(2).any(|w| w[0].png != w[1].png);
            let frames = if animated {
                // truncate to the element's OWN loop period (it was sampled over the stage's
                // longest clip), so its VFX loops at its own length, not the whole stage's.
                let p = loop_period(&per_frame);
                rle(per_frame[..p].to_vec())
            } else {
                let grp: Vec<&Instance> = base_insts.iter().filter(|i| matches(i)).collect();
                composite_grp(grp, bounds).into_iter().collect()
            };
            (!frames.is_empty()).then(|| BgLayer { name: sname.clone(), frames })
        }).collect()
    };

    // foreground = the genuine in-front props (non-overlapping foreground), sampled PER FRAME so
    // an animated SSF2 foreground (bowserscastle's shimmering lava-glow sheet over the floor)
    // actually animates instead of freezing on one frame. static foregrounds collapse to one frame.
    let fg_member = |i: &Instance| art_kind(i) == ArtKind::Foreground && !is_dup_fg(i);
    let fg_all: Vec<&Instance> = sampled.iter().flat_map(|(insts, _)| insts.iter().filter(|i| fg_member(i))).collect();
    let fg_bounds = union_bounds(&fg_all);
    let fg_per_frame: Vec<StageArt> = sampled.iter()
        .filter_map(|(insts, _)| composite_grp(insts.iter().filter(|i| fg_member(i)).collect(), fg_bounds))
        .collect();
    let fg_animated = fg_per_frame.len() == n_samples
        && fg_per_frame.windows(2).any(|w| w[0].png != w[1].png);
    let foreground: Vec<StageArt> = if fg_animated {
        rle(fg_per_frame)
    } else {
        composite_grp(base_insts.iter().filter(|i| fg_member(i)).collect(), fg_bounds).into_iter().collect()
    };
    // when the backdrop carries `_cambg` parallax layers, each backdrop/cambg LAYER (grouped
    // by symbol) becomes its own camera-relative plane with its own auto-derived pan rate (so
    // the sky, sun rays, trees, ... scroll at different rates, reading as depth — and the rays
    // draw in front of the sky, not occluded by it). The stageMC `background` plane (the
    // island fighters stand near) stays a fixed near-layer in front. Without parallax, backdrop
    // + background fold into one fixed bg. The folded structure-foreground draws last (in front
    // of the background structure, still behind fighters).
    let has_parallax = base_insts.iter().any(|i| art_kind(i) == ArtKind::Parallax);
    let (background, parallax): (Vec<BgLayer>, Vec<ParallaxLayer>) = if has_parallax {
        let mut order: Vec<&str> = Vec::new();
        let mut groups: BTreeMap<&str, Vec<&Instance>> = BTreeMap::new();
        for i in base_insts.iter().filter(|i| matches!(art_kind(i), ArtKind::Backdrop | ArtKind::Parallax)) {
            let key = i.sym_name.as_str();
            if !groups.contains_key(key) { order.push(key); }
            groups.entry(key).or_default().push(i);
        }
        // SSF2 `_cambg` layers are discrete elements that PAN (the autoPanMultiplier feeds the
        // pan); BOUNDS is for a tiling/wrapping backdrop, which the corpus has none of. Default
        // PAN; `PEPTIDE_PARALLAX_BOUNDS` forces BOUNDS to exercise that path.
        let mode = if std::env::var("PEPTIDE_PARALLAX_BOUNDS").is_ok() { ParallaxMode::Bounds } else { ParallaxMode::Pan };
        let layers: Vec<ParallaxLayer> = order.iter().filter_map(|key| {
            composite_layer(&groups[*key], shape_defs, bitmaps, ox, oy, None).map(|mut art| {
                art.x *= scale; art.y *= scale;
                let (x_pan, y_pan) = parallax_pan(art.w, art.h);
                ParallaxLayer { art, mode, x_pan, y_pan }
            })
        }).collect();
        // parallax stages keep the fixed near-background (the cambg layers carry the motion),
        // split into per-element layers so any animated near-bg object stays on its own loop.
        let near_member = |i: &Instance| art_kind(i) == ArtKind::Background || is_dup_fg(i);
        (group_bg_layers(&near_member), layers)
    } else {
        // No parallax: each backdrop/background ELEMENT (lava, torches, embers, podoboos, ...)
        // becomes its own layer on its own loop, in back-to-front draw order, so an animated SSF2
        // backdrop element actually animates without forcing every other element onto one shared
        // composite timeline. The folded structure-foreground (e.g. bowserscastle's stone walkway)
        // rides along as its own element, drawn after the backdrop it sits on.
        let bg_member = |i: &Instance| matches!(art_kind(i), ArtKind::Backdrop | ArtKind::Background) || is_dup_fg(i);
        (group_bg_layers(&bg_member), Vec::new())
    };

    // Emit a multi-frame stage animation only when the samples form a CLEAN animation:
    // every frame well-populated (no blinking on/off) and at least two distinct images.
    // Otherwise (static, or a chaotic/long SSF2 timeline) use the single richest frame.
    let imgs: Vec<&StageArt> = sampled.iter().filter_map(|(_, img)| img.as_ref()).collect();
    let clean = max_count > 0
        && stage_counts.iter().all(|&c| c * 5 >= max_count * 4)   // >=80% of max in every frame
        && imgs.len() == n_samples
        && imgs.windows(2).any(|w| w[0].png != w[1].png);          // some motion
    let stage_frames: Vec<StageArt> = if clean {
        // truncate to the animation's OWN loop period (sampled over the stage's longest clip), so a
        // stage with a long looping timeline emits ONE loop, not the whole multi-thousand-frame
        // sweep (gangplankgalleon's sea animation was 2651 frames). loop_period returns the full
        // length if it never repeats, so a genuinely non-looping stage is unaffected.
        let frames: Vec<StageArt> = imgs.into_iter().cloned().collect();
        let p = loop_period(&frames);
        frames[..p].to_vec()
    } else {
        sampled[base_idx].1.clone().into_iter().collect()
    };

    // engine-added background layers: orphan library bitmaps the SSF2 engine instantiates as named
    // bg children (e.g. bowserscastle's standable brick bridge). Placed centred on the backdrop;
    // appended frontmost-of-background (in front of the painted lava, behind the fighters).
    let backdrop_bounds = {
        let bg: Vec<&Instance> = base_insts.iter()
            .filter(|i| matches!(art_kind(i), ArtKind::Backdrop | ArtKind::Background))
            .collect();
        (!bg.is_empty()).then(|| (
            bg.iter().map(|i| i.aabb.x).fold(f64::MAX, f64::min),
            bg.iter().map(|i| i.aabb.y).fold(f64::MAX, f64::min),
            bg.iter().map(|i| i.aabb.x + i.aabb.w).fold(f64::MIN, f64::max),
            bg.iter().map(|i| i.aabb.y + i.aabb.h).fold(f64::MIN, f64::max),
        ))
    };
    let mut background = background;
    // engine-added orphan bitmaps: standable bg layers (the bridge deck) PLUS any foreground
    // occluder pieces SSF2 splits off them (the near parapet that draws in front of the fighter).
    let (eng_bg, foreground_occluders) = engine_added_bg_layers(root_tags, shape_defs, bitmaps, sprites, base_insts, backdrop_bounds, surface_y_fm, ox, oy, scale);
    background.extend(eng_bg);

    // background is already grouped into per-element layers (each RLE'd at the 30->60fps doubling);
    // the stage plane still needs the same RLE pass so a held source frame reads as a break.
    StageArtSet { background, parallax, stage_frames: rle(stage_frames), foreground, foreground_occluders }
}

/// Engine-added background layers: bitmaps in the stage library that NO shape fill, PlaceObject,
/// or timeline references. The SSF2 engine instantiates these at runtime as named background
/// children (e.g. bowserscastle's standable `bowsers_bridge_bg` brick walkway) — they live in the
/// stage's library but never in any movieclip, so the placement walk structurally can't see them.
/// We detect the orphan bitmaps directly and emit each distinct one as its own background layer,
/// centered on the backdrop at its standable height. Same-size orphans are one layer's animation
/// frames; tiny orphans (animation swap-frames of a placed bitmap) are skipped. Generalizes to any
/// stage built this way. `backdrop` is the raw-coord union bounds of the placed background art.
#[allow(clippy::too_many_arguments)]
fn engine_added_bg_layers(
    root_tags: &[swf::Tag],
    shape_defs: &BTreeMap<u16, &swf::Shape>,
    bitmaps: &BTreeMap<u16, (u32, u32, Vec<u8>)>,
    sprites: &BTreeMap<u16, &Vec<swf::Tag>>,
    instances: &[Instance],
    backdrop: Option<(f64, f64, f64, f64)>,
    surface_y_fm: Option<f64>,
    ox: f64, oy: f64, scale: f64,
) -> (Vec<BgLayer>, Vec<StageArt>) {
    use std::collections::{BTreeMap as Map, BTreeSet};
    let Some((bx0, by0, bx1, by1)) = backdrop else { return (Vec::new(), Vec::new()) };
    // every character id referenced by a shape fill / sprite placement / root placement, and the
    // pixel dims of every referenced bitmap (so a placed clip's animation swap-frame is excluded).
    let mut referenced: BTreeSet<u16> = BTreeSet::new();
    let mut ref_dims: BTreeSet<(u32, u32)> = BTreeSet::new();
    let note = |id: u16, bitmaps: &BTreeMap<u16, (u32, u32, Vec<u8>)>, refd: &mut BTreeSet<u16>, dims: &mut BTreeSet<(u32, u32)>| {
        refd.insert(id);
        if let Some((w, h, _)) = bitmaps.get(&id) { dims.insert((*w, *h)); }
    };
    for s in shape_defs.values() {
        for f in &s.styles.fill_styles {
            if let swf::FillStyle::Bitmap { id, .. } = f { note(*id, bitmaps, &mut referenced, &mut ref_dims); }
        }
    }
    for tags in sprites.values() {
        for frame in build_frames(tags) { for (cid, _, _, _, _, _) in frame { note(cid, bitmaps, &mut referenced, &mut ref_dims); } }
    }
    for t in root_tags {
        if let swf::Tag::PlaceObject(po) = t {
            if let swf::PlaceObjectAction::Place(id) | swf::PlaceObjectAction::Replace(id) = po.action { note(id, bitmaps, &mut referenced, &mut ref_dims); }
        }
    }
    // orphan + large + not a swap-frame of a placed bitmap; group equal-size orphans (frames).
    let mut by_dim: Map<(u32, u32), Vec<u16>> = Map::new();
    for (id, (w, h, _)) in bitmaps.iter() {
        if !referenced.contains(id) && w * h > 50_000 && !ref_dims.contains(&(*w, *h)) {
            by_dim.entry((*w, *h)).or_default().push(*id);
        }
    }
    if let Ok(ids) = std::env::var("PEPTIDE_DUMP_BMPS") {
        for id in ids.split(',').filter_map(|s| s.trim().parse::<u16>().ok()) {
            if let Some((w, h, rgba)) = bitmaps.get(&id) {
                if let Some(im) = image::RgbaImage::from_raw(*w, *h, rgba.clone()) {
                    let _ = im.save(format!("/tmp/bmp_{id}.png"));
                }
            }
        }
    }
    if std::env::var("PEPTIDE_ORPHAN_DEBUG").is_ok() {
        eprintln!("=== bitmap inventory ({} total) ===", bitmaps.len());
        for (id, (w, h, _)) in bitmaps.iter() {
            let orphan = !referenced.contains(id);
            let swap = ref_dims.contains(&(*w, *h));
            eprintln!("  id={id} {w}x{h} ({} px)  orphan={orphan} swapframe-dim={swap}{}",
                w * h, if orphan && w * h > 50_000 && !swap { "  <- ENGINE LAYER" } else { "" });
            if std::env::var("PEPTIDE_ORPHAN_DUMP").is_ok() && orphan && w * h > 50_000 && !swap {
                if let Some(im) = image::RgbaImage::from_raw(*w, *h, bitmaps[id].2.clone()) {
                    let _ = im.save(format!("/tmp/orphan_{id}.png"));
                }
            }
        }
    }
    // horizontal centre of the backdrop, in FM coords (the emitter scales the IMAGE by `scale`).
    let center_x_fm = ((bx0 + bx1) / 2.0 - ox) * scale;
    // the TRUE placement of an engine-bound orphan: SSF2 authors a placed shape whose bitmap fill
    // is a PLACEHOLDER (id 0xFFFF) the engine binds the orphan into at runtime (bowserscastle's
    // bridge: the background-plane shape hosts the deck piece, the foreground-plane shape the
    // parapet — at DIFFERENT y's, which is why "pixel-align the pair" mis-stacked them). Recover
    // the placeholder region by rasterizing a mask of just that fill and mapping its bbox through
    // the placed instance's world AABB; the region's dims must match the orphan's.
    // bbox of the subpaths painted by a GIVEN bitmap fill, walked straight off the shape records
    // (same conventions as `rasterize_shape`: bounds-relative px, absolute moveTo, delta edges,
    // mid-shape new-style arrays reset the fill indices). These "orphan" bitmaps are in fact
    // referenced — by fills inside mid-shape new-styles groups, which the top-level `referenced`
    // scan (and the composite's single-bitmap render path) don't see. The region IS the bitmap's
    // authored placement.
    fn bitmap_fill_bbox(shape: &swf::Shape, want_id: u16) -> Option<(f64, f64, f64, f64)> {
        let is_want = |fills: &[swf::FillStyle], idx: usize|
            idx > 0 && matches!(fills.get(idx - 1), Some(swf::FillStyle::Bitmap { id, .. }) if *id == want_id);
        let min_x = shape.shape_bounds.x_min.to_pixels();
        let min_y = shape.shape_bounds.y_min.to_pixels();
        let mut fills: &[swf::FillStyle] = &shape.styles.fill_styles;
        let (mut fs0, mut fs1) = (0usize, 0usize);
        let (mut px, mut py) = (-min_x, -min_y);
        let mut bb: Option<(f64, f64, f64, f64)> = None;
        fn extend(bb: &mut Option<(f64, f64, f64, f64)>, x: f64, y: f64) {
            *bb = Some(match *bb {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
        for rec in &shape.shape {
            match rec {
                swf::ShapeRecord::StyleChange(sc) => {
                    if let Some(ns) = &sc.new_styles { fills = &ns.fill_styles; fs0 = 0; fs1 = 0; }
                    if let Some(f) = sc.fill_style_0 { fs0 = f as usize; }
                    if let Some(f) = sc.fill_style_1 { fs1 = f as usize; }
                    if let Some(mv) = &sc.move_to { px = mv.x.to_pixels() - min_x; py = mv.y.to_pixels() - min_y; }
                    if is_want(fills, fs0) || is_want(fills, fs1) { extend(&mut bb, px, py); }
                }
                swf::ShapeRecord::StraightEdge { delta } => {
                    px += delta.dx.to_pixels(); py += delta.dy.to_pixels();
                    if is_want(fills, fs0) || is_want(fills, fs1) { extend(&mut bb, px, py); }
                }
                swf::ShapeRecord::CurvedEdge { control_delta, anchor_delta } => {
                    let cx = px + control_delta.dx.to_pixels(); let cy = py + control_delta.dy.to_pixels();
                    px = cx + anchor_delta.dx.to_pixels(); py = cy + anchor_delta.dy.to_pixels();
                    if is_want(fills, fs0) || is_want(fills, fs1) { extend(&mut bb, cx, cy); extend(&mut bb, px, py); }
                }
            }
        }
        bb
    }
    // the authored FM placement of bitmap `id`: the fill region painting it in any placed shape,
    // mapped through that instance's world AABB.
    let bitmap_host = |id: u16| -> Option<(f64, f64)> {
        for inst in instances {
            let Some(shape) = shape_defs.get(&inst.shape_id) else { continue };
            let Some((x0, y0, _x1, _y1)) = bitmap_fill_bbox(shape, id) else { continue };
            let shape_w = (shape.shape_bounds.x_max - shape.shape_bounds.x_min).to_pixels();
            let shape_h = (shape.shape_bounds.y_max - shape.shape_bounds.y_min).to_pixels();
            if shape_w <= 0.0 || shape_h <= 0.0 { continue; }
            let (fx, fy) = (
                (inst.aabb.x + x0 * (inst.aabb.w / shape_w) - ox) * scale,
                (inst.aabb.y + y0 * (inst.aabb.h / shape_h) - oy) * scale,
            );
            if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
                eprintln!("[bmp-host] bitmap {id}: shape {} plane {:?} region@({x0:.0},{y0:.0}) -> FM ({fx:.1},{fy:.1})",
                    inst.shape_id, inst.plane);
            }
            return Some((fx, fy));
        }
        None
    };
    // opaque coverage of bitmap `id`'s row `y` (fraction of the width that's solid).
    let row_cov = |id: &u16, y: u32| -> f64 {
        let (bw, bh, rgba) = &bitmaps[id];
        if y >= *bh { return 0.0; }
        let op = (0..*bw).filter(|&x| rgba[((y * *bw + x) * 4 + 3) as usize] > 40).count();
        op as f64 / *bw as f64
    };
    // the topmost row of bitmap `id` opaque across most of the width (its standable surface).
    let surf_row = |id: &u16| -> u32 {
        let (_, bh, _) = &bitmaps[id];
        (0..*bh).find(|&y| row_cov(id, y) > 0.5).unwrap_or(0)
    };
    let to_art = |id: &u16, x_fm: f64, y_fm: f64| -> Option<StageArt> {
        let (bw, bh, rgba) = &bitmaps[id];
        let im = image::RgbaImage::from_raw(*bw, *bh, rgba.clone())?;
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(im).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
        Some(StageArt { png, x: x_fm, y: y_fm, w: *bw, h: *bh, hold: 1 })
    };
    let mut bg_layers: Vec<BgLayer> = Vec::new();
    let mut fg_arts: Vec<StageArt> = Vec::new();
    for (li, ((w, _h), ids)) in by_dim.into_iter().enumerate() {
        let x_fm = center_x_fm - w as f64 * scale / 2.0;
        // DETECT the walkway's standable surface: the topmost row opaque across most of the width
        // (the flat brick deck; the banner/throne above it are narrow). The BACKGROUND piece is the
        // one solid at that deck row (fighters stand ON it); align THAT to the main collision floor.
        // Fall back to the backdrop's vertical centre when there's no floor.
        // Pick the deck row from whichever same-size orphan has the highest (topmost) solid deck.
        let deck_row = ids.iter().map(&surf_row).min().unwrap_or(0);
        // SSF2 splits a standable structure (e.g. bowscastle's bridge) into a background piece
        // (solid deck, drawn behind the fighter) and a foreground piece (the near parapet, deck cut
        // OUT so the fighter shows through, drawn in front to occlude their feet). Both are orphan
        // bitmaps of the same size. Classify each by its coverage at the deck row: solid -> behind,
        // cut-out -> in front. Animation-frame groups (all solid) keep one background layer.
        let probe = (deck_row + 4).min(bitmaps[&ids[0]].1.saturating_sub(1));
        let mut bg_id = ids[0];
        let mut bg_cov = row_cov(&bg_id, probe);
        for id in &ids {
            let c = row_cov(id, probe);
            if c > bg_cov { bg_cov = c; bg_id = *id; }
        }
        // background = the deck-solid piece, at its AUTHORED fill-region placement when a placed
        // shape paints it (the authoritative position), else anchored to the collision floor.
        let (bg_x, bg_y) = bitmap_host(bg_id).unwrap_or_else(|| {
            let y_fm = match surface_y_fm {
                Some(s) => s - surf_row(&bg_id) as f64 * scale,
                None => ((by0 + by1) / 2.0 - oy) * scale,
            };
            (x_fm, y_fm)
        });
        if let Some(art) = to_art(&bg_id, bg_x, bg_y) {
            bg_layers.push(BgLayer { name: format!("engineLayer{li}"), frames: vec![art] });
        }
        // foreground = any same-size sibling whose deck row is clearly CUT OUT (a near parapet that
        // must draw IN FRONT of the fighter), at ITS OWN authored placement, falling back to the
        // background piece's.
        for id in &ids {
            if *id != bg_id && row_cov(id, probe) < 0.25 && bg_cov > 0.4 {
                let (fx, fy) = bitmap_host(*id).unwrap_or((bg_x, bg_y));
                if let Some(art) = to_art(id, fx, fy) { fg_arts.push(art); }
            }
        }
    }
    (bg_layers, fg_arts)
}

/// spanning their union bounds. `None` if nothing rasterized.
/// A 1x1 fully transparent PNG: the canvas for an element frame where nothing is visible.
fn blank_png() -> Vec<u8> {
    use image::ImageEncoder;
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&[0u8, 0, 0, 0], 1, 1, image::ExtendedColorType::Rgba8)
        .expect("encode 1x1 png");
    png
}

fn composite_layer(
    art: &[&Instance],
    shape_defs: &BTreeMap<u16, &swf::Shape>,
    bitmaps: &BTreeMap<u16, (u32, u32, Vec<u8>)>,
    ox: f64, oy: f64,
    fixed_bounds: Option<(f64, f64, f64, f64)>,
) -> Option<StageArt> {
    use image::{imageops, RgbaImage};

    // The bitmap id a shape fills with, if any (stage backgrounds are bitmap-filled
    // rects that `vector_raster` skips; we blit the bitmap into the shape's AABB).
    let shape_bitmap = |sid: u16| -> Option<u16> {
        let shape = shape_defs.get(&sid)?;
        shape.styles.fill_styles.iter().find_map(|f| match f {
            swf::FillStyle::Bitmap { id, .. } if bitmaps.contains_key(id) => Some(*id),
            _ => None,
        })
    };

    if art.is_empty() { return None; }

    // union bounds (raw world coords) -> canvas. cap to keep the PNG sane. `fixed_bounds` (the
    // union across ALL frames of an animated layer) keeps every frame on one canvas so the layer
    // doesn't slide as a flickering element's per-frame bbox shifts (the "wiggle").
    let (min_x, min_y, max_x, max_y) = fixed_bounds.unwrap_or_else(|| (
        art.iter().map(|i| i.aabb.left()).fold(f64::MAX, f64::min),
        art.iter().map(|i| i.aabb.top()).fold(f64::MAX, f64::min),
        art.iter().map(|i| i.aabb.right()).fold(f64::MIN, f64::max),
        art.iter().map(|i| i.aabb.bottom()).fold(f64::MIN, f64::max),
    ));
    let cw = ((max_x - min_x).ceil() as u32).clamp(1, 4096);
    let ch = ((max_y - min_y).ceil() as u32).clamp(1, 4096);
    let mut canvas = RgbaImage::new(cw, ch);

    let tw_th = |i: &Instance| (
        (i.aabb.w.round() as u32).clamp(1, 4096),
        (i.aabb.h.round() as u32).clamp(1, 4096),
    );
    let mut drew = false;
    for inst in art {
        let (tw, th) = tw_th(inst);
        // The placed tile at the AABB size: a bitmap fill clipped to the shape OUTLINE, else
        // the vector rasterization.
        let scaled: Option<RgbaImage> = if let Some(bid) = shape_bitmap(inst.shape_id) {
            let (w, h, rgba) = bitmaps.get(&bid).unwrap();
            let Some(bmp) = RgbaImage::from_raw(*w, *h, rgba.clone()) else { continue };
            let mut bmp = imageops::resize(&bmp, tw, th, imageops::FilterType::Triangle);
            // clip the bitmap to the shape's path (not its rectangular AABB): rasterize the
            // path with the bitmap fill swapped for solid white, and use that as an alpha
            // mask. Without this an irregular bitmap-filled shape (e.g. a tree canopy) renders
            // as a hard rectangle.
            let shape = shape_defs.get(&inst.shape_id).unwrap();
            let mask_fills: Vec<swf::FillStyle> = shape.styles.fill_styles.iter().map(|f| match f {
                swf::FillStyle::Bitmap { .. } => swf::FillStyle::Color(swf::Color { r: 255, g: 255, b: 255, a: 255 }),
                other => other.clone(),
            }).collect();
            if let Some(mask) = crate::vector_raster::rasterize_shape(
                &shape.shape_bounds, &mask_fills, &shape.styles.line_styles, &shape.shape,
            ).and_then(|r| RgbaImage::from_raw(r.width, r.height, r.rgba)) {
                let mask = imageops::resize(&mask, tw, th, imageops::FilterType::Triangle);
                for (p, m) in bmp.pixels_mut().zip(mask.pixels()) {
                    p[3] = ((p[3] as u32 * m[3] as u32) / 255) as u8;
                }
            }
            Some(bmp)
        } else {
            let shape = shape_defs.get(&inst.shape_id).unwrap();
            crate::vector_raster::rasterize_shape(
                &shape.shape_bounds, &shape.styles.fill_styles, &shape.styles.line_styles, &shape.shape,
            ).and_then(|r| RgbaImage::from_raw(r.width, r.height, r.rgba))
                .map(|t| imageops::resize(&t, tw, th, imageops::FilterType::Triangle))
        };
        let Some(mut scaled) = scaled else { continue };
        if scaled.width() == 0 || scaled.height() == 0 { continue; }
        // The tile is rasterized in source orientation and placed onto the axis-aligned AABB; a
        // mirrored placement (scaleX/scaleY < 0) must flip the tile so it lands the same way it
        // does in SSF2. For a single static shape this is invisible (the AABB is unchanged), but a
        // mirrored MULTI-FRAME element whose frames are off-center crops (a wall torch's flame
        // bitmaps) needs it: un-flipped, each frame's content sits at a different spot in its AABB
        // and the element appears to slide left/right instead of animating in place.
        let (fx, fy) = inst.flip;
        if fx { scaled = imageops::flip_horizontal(&scaled); }
        if fy { scaled = imageops::flip_vertical(&scaled); }
        let ox_px = (inst.aabb.left() - min_x).round() as i64;
        let oy_px = (inst.aabb.top() - min_y).round() as i64;
        if inst.blend == Some(swf::BlendMode::HardLight) {
            // SSF2 draws this placement with HardLight (bowserscastle's lightmask gradient over
            // the lava). FM stage layers have no blend modes, so emulate in the bake: hard-light
            // into pixels this composite already painted; where the canvas is empty (the layer
            // sits over the live scene) fall back to normal alpha as the approximation.
            let hl = |b: f64, t: f64| if t <= 0.5 { 2.0 * b * t } else { 1.0 - 2.0 * (1.0 - b) * (1.0 - t) };
            for (x, y, sp) in scaled.enumerate_pixels() {
                let (cx, cy) = (ox_px + x as i64, oy_px + y as i64);
                if cx < 0 || cy < 0 || cx >= cw as i64 || cy >= ch as i64 { continue; }
                let dp = canvas.get_pixel_mut(cx as u32, cy as u32);
                let sa = sp[3] as f64 / 255.0;
                if sa <= 0.0 { continue; }
                let da = dp[3] as f64 / 255.0;
                for ch_i in 0..3 {
                    let b = dp[ch_i] as f64 / 255.0;
                    let t = sp[ch_i] as f64 / 255.0;
                    // blend result over painted base, plain alpha over unpainted (da lerps them)
                    let blended = hl(b, t) * da + t * (1.0 - da);
                    dp[ch_i] = ((b * da * (1.0 - sa) + blended * sa).min(1.0) * 255.0
                        / (da * (1.0 - sa) + sa).max(1e-6)) as u8;
                }
                dp[3] = (((da * (1.0 - sa) + sa) * 255.0).min(255.0)) as u8;
            }
        } else {
            imageops::overlay(&mut canvas, &scaled, ox_px, oy_px);
        }
        drew = true;
    }
    if !drew { return None; }

    let mut png = Vec::new();
    {
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(canvas.as_raw(), cw, ch, image::ExtendedColorType::Rgba8).ok()?;
    }
    Some(StageArt { png, x: min_x - ox, y: min_y - oy, w: cw, h: ch, hold: 1 })
}

/// Extract a clip's labelled sub-animations as rasterized frame sequences. Each frame label in the
/// clip's own timeline (e.g. a Thwomp's `entrance`/`fall`/`idle`) becomes one [`ClipAnim`]: the clip
/// is held on that label's frame while the nested sub-clip plays through its full timeline,
/// composited per frame on ONE fixed canvas (the union across the sub-animation, so it doesn't
/// wiggle). Universal — any multi-animation stage clip (hazards, animated props) uses this.
#[allow(clippy::too_many_arguments)]
fn extract_labeled_clip_anims(
    clip_id: u16,
    sprites: &BTreeMap<u16, &Vec<swf::Tag>>,
    sym_names: &BTreeMap<u16, String>,
    shape_defs: &BTreeMap<u16, &swf::Shape>,
    bitmaps: &BTreeMap<u16, (u32, u32, Vec<u8>)>,
    planes: &PlaneMap,
    abcs: &[crate::abc_parser::AbcFile],
    ox: f64, oy: f64,
) -> Vec<ClipAnim> {
    let Some(tags) = sprites.get(&clip_id) else { return Vec::new() };
    if let Ok(ids) = std::env::var("PEPTIDE_CLIP_TAGS") {
        for id in ids.split(',').filter_map(|s| s.trim().parse::<u16>().ok()) {
            let Some(t) = sprites.get(&id) else { continue };
            eprintln!("[clip-tags] sprite {id} raw timeline:");
            let mut frame = 0usize;
            for tag in t.iter() {
                match tag {
                    swf::Tag::PlaceObject(po) => {
                        let act = match &po.action {
                            swf::PlaceObjectAction::Place(i) => format!("Place({i})"),
                            swf::PlaceObjectAction::Replace(i) => format!("Replace({i})"),
                            swf::PlaceObjectAction::Modify => "Modify".into(),
                        };
                        eprintln!("    f{frame} depth={} {act} mat={} ratio={:?}", po.depth, po.matrix.is_some(), po.ratio);
                    }
                    swf::Tag::RemoveObject(r) => eprintln!("    f{frame} depth={} REMOVE", r.depth),
                    swf::Tag::FrameLabel(l) => eprintln!("    f{frame} LABEL {:?}", l.label.to_str_lossy(encoding_rs::WINDOWS_1252)),
                    swf::Tag::ShowFrame => { frame += 1; }
                    _ => {}
                }
            }
            eprintln!("    total frames {frame}");
        }
    }
    let labels = crate::sprite_parser::extract_frame_labels_from_tags(tags);
    if labels.is_empty() { return Vec::new(); }
    let mut sprite_frames: BTreeMap<u16, Vec<Vec<PlacedChild>>> = BTreeMap::new();
    for (id, t) in sprites { sprite_frames.insert(*id, build_frames(t)); }
    let clip_frames = build_frames(tags);
    let no_phase: std::collections::HashMap<(i64, i64), usize> = std::collections::HashMap::new();
    let mut anims = Vec::new();
    for (label, frame) in &labels {
        let f = *frame as usize;
        let Some(children) = clip_frames.get(f) else { continue };
        // sub-animation length = the longest nested MOVIECLIP placed on this label's frame. a
        // graphic-pinned child (PlacedChild.3: the placement carries a ratio — a Graphic symbol,
        // e.g. the thwomp's Single Frame fall/idle) shows one fixed frame, never self-animates and
        // never runs its frame scripts, so it doesn't drive the length (1 if everything is static).
        let sub_len = children.iter()
            .filter(|(_, _, _, pinned, _, _)| pinned.is_none())
            .filter_map(|(id, _, _, _, _, _)| sprite_frames.get(id).map(|fr| fr.len()))
            .max().unwrap_or(1).max(1);
        // hold-vs-loop comes from the DRIVING nested clip's own timeline: a sub-clip that holds
        // carries a Flash-generated `_fla.` class (frame scripts) bound in SymbolClass. `stop()`
        // freezes where it fires; `gotoAndStop(target)` plays through then freezes at the target
        // (a label in the SUB-clip's own timeline, or a 1-based frame number).
        let driver_id = children.iter()
            .filter(|(_, _, _, pinned, _, _)| pinned.is_none())
            .filter_map(|(id, _, _, _, _, _)| sprite_frames.get(id).map(|fr| (*id, fr.len())))
            .max_by_key(|(_, n)| *n).map(|(id, _)| id);
        let hold = driver_id
            .and_then(|id| sym_names.get(&id))
            .and_then(|class| abcs.iter().find_map(|a| crate::abc_parser::extract_timeline_hold(a, class)));
        let frame_velocities: Vec<(u32, f64)> = driver_id
            .and_then(|id| sym_names.get(&id))
            .map(|class| abcs.iter().flat_map(|a| crate::abc_parser::extract_frame_velocities(a, class)).collect())
            .unwrap_or_default();
        // resolve to (play-until frame, freeze-at frame), both 0-based sub-frame indices.
        let hold_frames: Option<(usize, usize)> = hold.as_ref().and_then(|h| match h {
            crate::abc_parser::TimelineHold::StopAt(n) => {
                let f = (*n as usize).saturating_sub(1);
                Some((f, f))
            }
            crate::abc_parser::TimelineHold::GotoStop(target, n) => {
                let until = (*n as usize).saturating_sub(1);
                let at = if let Ok(num) = target.parse::<usize>() { num.saturating_sub(1) } else {
                    let sub_labels = driver_id.and_then(|id| sprites.get(&id))
                        .map(|t| crate::sprite_parser::extract_frame_labels_from_tags(t))
                        .unwrap_or_default();
                    sub_labels.iter().find(|(l, _)| l.eq_ignore_ascii_case(target))
                        .map(|(_, f)| *f as usize)?
                };
                Some((until, at))
            }
        });
        if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
            eprintln!("[hz-hold] label={label:?} driver={driver_id:?} class={:?} hold={hold:?} -> {hold_frames:?}",
                driver_id.and_then(|id| sym_names.get(&id)));
        }
        // walk every sub-frame, then composite all frames onto one fixed union canvas (no wiggle).
        // walk_frame itself pins graphic placements to their ratio-selected frame.
        let per_frame: Vec<Vec<Instance>> = (0..sub_len).map(|g| {
            let mut out = Vec::new();
            walk_frame(children, Mat::id(), g, None, None, planes, sym_names, shape_defs, &sprite_frames, &mut out, 0, (0.0, 0.0), 0, None, &no_phase);
            out.retain(|i| shape_defs.contains_key(&i.shape_id));
            out
        }).collect();
        let all: Vec<&Instance> = per_frame.iter().flatten().collect();
        let bounds = (!all.is_empty()).then(|| (
            all.iter().map(|i| i.aabb.left()).fold(f64::MAX, f64::min),
            all.iter().map(|i| i.aabb.top()).fold(f64::MAX, f64::min),
            all.iter().map(|i| i.aabb.right()).fold(f64::MIN, f64::max),
            all.iter().map(|i| i.aabb.bottom()).fold(f64::MIN, f64::max),
        ));
        let mut frames: Vec<StageArt> = per_frame.iter()
            .filter_map(|insts| composite_layer(&insts.iter().collect::<Vec<_>>(), shape_defs, bitmaps, ox, oy, bounds))
            .collect();
        // arrange the frames so FM's play-once-and-hold (endType NONE) freezes on the SAME frame
        // SSF2 freezes on: truncate at the play-until frame, and when the freeze target differs
        // (gotoAndStop to a label) append that frame as the final held one.
        let stop_frame = hold_frames.and_then(|(until, at)| {
            if until + 1 < frames.len() { frames.truncate(until + 1); }
            if at != until {
                let held = frames.get(at).or(frames.first()).cloned()?;
                frames.push(held);
            }
            Some(frames.len() - 1)
        });
        if !frames.is_empty() { anims.push(ClipAnim { label: label.clone(), frames, stop_frame, frame_velocities }); }
    }
    anims
}

/// Trace a solid terrain shape's walkable TOP surface as a polyline in FM coords (left to
/// right). Rasterizes the shape and scans each column for the topmost opaque pixel, then maps
/// to world via the placed AABB and simplifies. `None` if the shape won't rasterize, is
/// essentially flat (so the level `rect.top()` line suffices), or maps to < 2 points. Maps
/// through the AABB, so it's exact for translated/scaled terrain; rotated terrain would skew
/// (rare for stages).
fn floor_profile(shape: &swf::Shape, aabb: &Rect, ox: f64, oy: f64, scale: f64) -> Option<Vec<(f64, f64)>> {
    use image::{imageops, RgbaImage};
    let raster = crate::vector_raster::rasterize_shape(
        &shape.shape_bounds, &shape.styles.fill_styles, &shape.styles.line_styles, &shape.shape,
    )?;
    let mask = RgbaImage::from_raw(raster.width, raster.height, raster.rgba)?;
    let w = (aabb.w.round() as u32).clamp(1, 4096);
    let h = (aabb.h.round() as u32).clamp(1, 4096);
    if w < 2 || h < 2 { return None; }
    let mask = imageops::resize(&mask, w, h, imageops::FilterType::Triangle);
    let step = (w / 96).max(1); // ~96 columns sampled across the span
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut cx = 0u32;
    loop {
        let col = cx.min(w - 1);
        if let Some(ty) = (0..h).find(|&y| mask.get_pixel(col, y)[3] > 40) {
            pts.push((
                (aabb.left() + col as f64 - ox) * scale,
                (aabb.top() + ty as f64 - oy) * scale,
            ));
        }
        if col == w - 1 { break; }
        cx += step;
    }
    if pts.len() < 2 { return None; }
    let simplified = rdp_simplify(&pts, 2.0);
    // a (near-)flat profile adds nothing over the level floor line.
    let ys: Vec<f64> = simplified.iter().map(|p| p.1).collect();
    let span = ys.iter().cloned().fold(f64::MIN, f64::max) - ys.iter().cloned().fold(f64::MAX, f64::min);
    if span < 6.0 { return None; }
    Some(simplified)
}

/// Ramer-Douglas-Peucker polyline simplification (perpendicular-distance epsilon in px).
fn rdp_simplify(pts: &[(f64, f64)], eps: f64) -> Vec<(f64, f64)> {
    if pts.len() < 3 { return pts.to_vec(); }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let (mut max_d, mut idx) = (0.0_f64, 0);
    for (i, p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
        let d = ((p.0 - a.0) * dy - (p.1 - a.1) * dx).abs() / len;
        if d > max_d { max_d = d; idx = i; }
    }
    if max_d > eps {
        let mut left = rdp_simplify(&pts[..=idx], eps);
        let right = rdp_simplify(&pts[idx..], eps);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![a, b]
    }
}

/// SSF2's auto pan multiplier for a camera-background layer of `w x h` native px, vs the
/// 640x360 game view (the SSF2 `Vcam` formula): `(size - view) / (2 * size)`, clamped to
/// `[0, 0.5]` (a layer <= the view is screen-fixed; a very wide layer approaches 0.5).
fn parallax_pan(w: u32, h: u32) -> (f64, f64) {
    let x = ((w as f64 - 640.0) / (2.0 * w.max(1) as f64)).clamp(0.0, 0.5);
    let y = ((h as f64 - 360.0) / (2.0 * h.max(1) as f64)).clamp(0.0, 0.5);
    (x, y)
}

/// Decompress an `.ssf` to SWF bytes via the DAT-archive aware path.
fn ssf_decompress(raw: &[u8], path: &Path) -> Result<Vec<u8>> {
    crate::ssf::decompress(raw).with_context(|| format!("decompress {}", path.display()))
}

/// Read the stage DAT's `Main` package metadata (`id`, `guid`, `music`) from the first
/// ABC block that carries an `id` (the content id the engine knows the stage by).
/// the stage's AS3 layer assignments + spawned actors (the document class extending `SSF2Stage`).
/// `None` if the stage has no parseable SSF2Stage subclass (the heuristic path then takes over).
fn stage_abc_model(swf_data: &[u8]) -> Option<crate::stage_abc::StageAbcModel> {
    let swf = crate::swf_parser::parse(swf_data).ok()?;
    let blocks: Vec<crate::abc_parser::AbcFile> = swf.abc_blocks.iter()
        .filter_map(|b| crate::abc_parser::parse(b).ok()).collect();
    let mut model = blocks.iter().find_map(crate::stage_abc::extract_stage)?;
    // Resolve each spawned hazard's get*() declarations across ALL abc blocks, not just the stage's.
    // The enemy class is usually compiled into the same block as the stage (extract_stage already
    // filled it from there), but a stage CAN split it into another block — and we follow the
    // `spawnEnemy(<Class>)` reference (`a.class_name`) to find it wherever it lives, rather than
    // assuming co-location or scanning for SSF2Enemy subclasses by hand.
    for a in &mut model.actors {
        if a.attack_hitboxes.is_empty() {
            for abc in &blocks {
                let hb = crate::abc_parser::extract_attack_stats_for(abc, &a.class_name);
                if !hb.is_empty() { a.attack_hitboxes = hb; break; }
            }
        }
        if a.own_stats.is_empty() {
            for abc in &blocks {
                let os = crate::abc_parser::extract_own_stats_for(abc, &a.class_name);
                if !os.is_empty() { a.own_stats = os; break; }
            }
        }
        if a.anim_labels.is_empty() {
            for abc in &blocks {
                let al = crate::abc_parser::extract_force_attack_labels(abc, &a.class_name);
                if !al.is_empty() { a.anim_labels = al; break; }
            }
        }
        if a.faller.is_none() {
            for abc in &blocks {
                if let Some(fc) = crate::abc_parser::extract_faller_cycle(abc, &a.class_name) {
                    a.faller = Some(fc); break;
                }
            }
        }
        if a.behavior.shake.is_none() && a.behavior.self_platform.is_none() {
            for abc in &blocks {
                let b = crate::abc_parser::extract_enemy_behavior(abc, &a.class_name);
                if b.shake.is_some() || b.self_platform.is_some() || b.rise_yspeed.is_some() { a.behavior = b; break; }
            }
        }
        if a.reconstructed_script.is_none() {
            for abc in &blocks {
                if let Some(s) = crate::abc_parser::reconstruct_enemy_script(abc, &a.class_name) { a.reconstructed_script = Some(s); break; }
            }
        }
    }
    Some(model)
}

fn stage_package_metadata(swf_data: &[u8]) -> Option<crate::abc_parser::MainPackageMetadata> {
    let swf = crate::swf_parser::parse(swf_data).ok()?;
    for abc_bytes in &swf.abc_blocks {
        if let Ok(abc) = crate::abc_parser::parse(abc_bytes) {
            if let Some(md) = crate::abc_parser::extract_main_package_metadata(&abc) {
                if md.id.is_some() { return Some(md); }
            }
        }
    }
    None
}

/// Drop collision platforms that are near-duplicates of another (a moving-platform container
/// MC and its collision child both match the platform/terrain naming → the same platform twice).
/// A platform is removed when another platform of the SAME kind covers >=70% of its area; the
/// larger of an overlapping pair survives. Genuinely separate platforms don't overlap, so they
/// are untouched. A removed platform's `moving` flag is OR'd onto the survivor.
fn dedupe_platforms(platforms: &mut Vec<Platform>) {
    let area = |r: &Rect| (r.w * r.h).max(1.0);
    let covered = |a: &Rect, b: &Rect| -> f64 {
        let ix = (a.right().min(b.right()) - a.left().max(b.left())).max(0.0);
        let iy = (a.bottom().min(b.bottom()) - a.top().max(b.top())).max(0.0);
        (ix * iy) / area(a) // fraction of `a` inside `b`
    };
    let mut keep = vec![true; platforms.len()];
    for i in 0..platforms.len() {
        if !keep[i] { continue; }
        for j in 0..platforms.len() {
            if i == j || !keep[j] || platforms[i].drop_through != platforms[j].drop_through { continue; }
            // j is the smaller (or equal, broken by index) -> drop j into i.
            let (bigger, smaller) = if area(&platforms[i].rect) >= area(&platforms[j].rect) { (i, j) } else { continue };
            if covered(&platforms[smaller].rect, &platforms[bigger].rect) >= 0.7 {
                keep[smaller] = false;
                if platforms[smaller].moving { platforms[bigger].moving = true; }
            }
        }
    }
    let mut idx = 0;
    platforms.retain(|_| { let k = keep[idx]; idx += 1; k });
}

/// Title-case an SSF2 lowercase-concatenated id for a display name: capitalize the first
/// letter and any after an underscore/space ("battlefield" -> "Battlefield"). Multi-word
/// ids that aren't underscore-separated stay one word; add an override in the metadata map.
fn title_case(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    let mut cap = true;
    for c in id.chars() {
        if c == '_' || c == '-' { out.push(' '); cap = true; }
        else if cap { out.extend(c.to_uppercase()); cap = false; }
        else { out.push(c); }
    }
    out
}

/// Extract a 0-based player index from a symbol name like
/// `battlefield_fla.p1_Start_9` given the marker `"_start_"` / `"_spawn_"`.
/// Looks for `pN` immediately before the marker.
fn player_index(sym_lower: &str, marker: &str) -> Option<usize> {
    let pos = sym_lower.find(marker)?;
    // walk back over the `pN` token preceding the marker
    let prefix = &sym_lower[..pos];
    let digits: String = prefix.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
    if digits.is_empty() { return None; }
    // the char before the digits must be 'p'
    let p_idx = prefix.len() - digits.len();
    if prefix.as_bytes().get(p_idx.wrapping_sub(1)) != Some(&b'p') { return None; }
    digits.parse::<usize>().ok().and_then(|n| n.checked_sub(1))
}

/// Recurse the placement tree, recording each placed shape's world AABB and the
/// `stageMC` origin (the root instance whose name is `stageMC` or whose symbol
/// class starts with `stage_`).
#[allow(clippy::too_many_arguments)]
fn walk<'a>(
    tags: &'a [swf::Tag], parent: Mat,
    sym_names: &BTreeMap<u16, String>,
    shape_bounds: &BTreeMap<u16, (f64, f64, f64, f64)>,
    sprites: &BTreeMap<u16, &'a Vec<swf::Tag>>,
    planes: &PlaneMap,
    rec: usize, carried_sym: Option<&str>, plane: Option<&str>, moving_anc: bool,
    hazard_anc: Option<HazardKind>,
    out: &mut Vec<Instance>, anchors: &mut WalkAnchors,
) {
    if rec > 8 { return; }
    for tag in tags {
        let swf::Tag::PlaceObject(po) = tag else { continue };
        let id = match &po.action {
            swf::PlaceObjectAction::Place(id) | swf::PlaceObjectAction::Replace(id) => *id,
            swf::PlaceObjectAction::Modify => continue,
        };
        let local = po.matrix.as_ref().map(|m| Mat {
            a: m.a.to_f64(), b: m.b.to_f64(), c: m.c.to_f64(), d: m.d.to_f64(),
            tx: m.tx.get() as f64 / 20.0, ty: m.ty.get() as f64 / 20.0,
        }).unwrap_or(Mat::id());
        let world = parent.mul(&local);
        let inst_name = po.name.as_ref().map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252).to_string());
        let sym = sym_names.get(&id).cloned().unwrap_or_default();
        let my_plane = plane_tag(inst_name.as_deref(), &sym, planes).or(plane);
        // sticky once any ancestor (or this node) is `moving`-named — the collision child
        // is named separately, so the signal must flow down the subtree.
        let here_moving = moving_anc
            || inst_name.as_deref().is_some_and(|n| n.to_ascii_lowercase().contains("moving"))
            || sym.to_ascii_lowercase().contains("moving");
        // sticky hazard kind: once an ancestor (or this node) is named like a hazard, every
        // descendant shape inherits it (the leaf shapes carry deeper auto-named symbols).
        let here_hazard = hazard_anc
            .or_else(|| inst_name.as_deref().and_then(hazard_kind))
            .or_else(|| hazard_kind(&sym));

        if std::env::var("PEPTIDE_STAGE_TREE").is_ok() {
            let kind = if sprites.contains_key(&id) { "MC" } else if shape_bounds.contains_key(&id) { "shape" } else { "?" };
            // frames tells a MOVING object from a still one, which is the whole question
            // when auditing a stage: stage art is rasterized into one composite sprite, so
            // a 1-frame clip having no separate Fraymakers object is correct, while a
            // multi-frame clip baked into that composite has silently lost its animation.
            let frames = sprites.get(&id).map(|t| build_frames(t).len()).unwrap_or(1);
            eprintln!("{}d{} {kind} inst={:?} sym={:?} plane={:?} @({:.0},{:.0}) frames={}",
                "  ".repeat(rec), po.depth, inst_name.as_deref().unwrap_or(""), sym, my_plane.unwrap_or(""), world.tx, world.ty, frames);
        }

        // record the stage origin from the root stageMC instance
        if anchors.origin.is_none()
            && (inst_name.as_deref() == Some("stageMC") || sym.to_ascii_lowercase().starts_with("stage_"))
        {
            anchors.origin = Some((world.tx, world.ty));
        }
        // record the terrain MC's REGISTRATION (matrix translate) — the origin of the GAME
        // coordinate space: the stage's own AS3 (spawnEnemy/setX) and the engine's runtime
        // X/Y both use terrain-local coords (verified live: pure translation, scale 1).
        if anchors.terrain.is_none() && inst_name.as_deref() == Some("terrain") {
            anchors.terrain = Some((world.tx, world.ty));
        }

        if let Some((x0, y0, x1, y1)) = shape_bounds.get(&id) {
            let corners = [world.apply(*x0, *y0), world.apply(*x1, *y0), world.apply(*x1, *y1), world.apply(*x0, *y1)];
            let xmn = corners.iter().map(|c| c.0).fold(f64::MAX, f64::min);
            let xmx = corners.iter().map(|c| c.0).fold(f64::MIN, f64::max);
            let ymn = corners.iter().map(|c| c.1).fold(f64::MAX, f64::min);
            let ymx = corners.iter().map(|c| c.1).fold(f64::MIN, f64::max);
            out.push(Instance {
                shape_id: id,
                inst_name: inst_name.clone(),
                sym_name: carried_sym.unwrap_or("").to_string(),
                plane: my_plane.map(str::to_string),
                aabb: Rect { x: xmn, y: ymn, w: xmx - xmn, h: ymx - ymn },
                cx: world.tx, cy: world.ty,
                x_sign: world.x_sign(), flip: world.flips(),
                moving: here_moving,
                hazard: here_hazard,
                inst_anchor: (0.0, 0.0), inst_path: 0, blend: None, // geometry walk feeds collision/hazards, not art grouping
            });
        }
        if let Some(child) = sprites.get(&id) {
            // carry the most specific identity: instance name wins, else symbol class,
            // else inherit the parent's carried symbol.
            let next = inst_name.as_deref().or(if sym.is_empty() { carried_sym } else { Some(&sym) });
            let next = next.map(|s| s.to_string());
            walk(child, world, sym_names, shape_bounds, sprites, planes, rec + 1, next.as_deref(), my_plane, here_moving, here_hazard, out, anchors);
        }
    }
}

#[cfg(test)]
mod hazard_classifier_tests {
    use super::*;

    // The damaging-hazard vocabulary + cosmetic exclusions. These guard the auto-detection: a
    // missed keyword silently ships no hazard, a missed exclusion silently ships a phantom one.
    #[test]
    fn damaging_hazards_are_recognised() {
        for (label, want) in [
            ("bowserscastle_lava", HazardKind::Lava),
            ("MK3Lava", HazardKind::Lava),
            ("SR388_AcidWaterfall", HazardKind::Acid),
            ("thwomp_mc", HazardKind::Thwomp),
            ("PodobooMC", HazardKind::Podoboo),
            ("CNZBumper", HazardKind::Bumper),
            ("HyruleTornado", HazardKind::Tornado),
            ("DamageZone", HazardKind::DamageZone),
            ("spike_floor", HazardKind::Spike),
            ("PiranhaPlant", HazardKind::Piranha),
        ] {
            assert_eq!(hazard_kind(label), Some(want), "{label} should be a hazard");
        }
    }

    #[test]
    fn cosmetic_and_scenery_are_excluded() {
        // sound/vfx/impact sub-clips, decorative bg, and projectile/effect spawns are NOT hitboxes.
        for label in [
            "thwomp_land", "thwomp_vfx", "thwomp_shake", "lava_splash", "bowsers_podoboos_bg",
            "lavabub", "lava_land", "plant_fireball", "birdo_egg", "castlevania_thunder_sfx",
            "background", "ember", "torchembers",
        ] {
            assert_eq!(hazard_kind(label), None, "{label} should NOT be a hazard");
        }
    }

    #[test]
    fn same_kind_boxes_cluster_into_one() {
        // two adjacent lava leaf shapes (one hazard split across shapes) merge into a single box.
        let id = |r: &Rect| *r;
        let insts = vec![
            Instance { shape_id: 1, inst_name: None, sym_name: "x".into(), plane: None,
                aabb: Rect { x: 0.0, y: 0.0, w: 100.0, h: 20.0 }, cx: 50.0, cy: 10.0, x_sign: 1.0, flip: (false, false),
                moving: false, hazard: Some(HazardKind::Lava), inst_anchor: (0.0, 0.0), inst_path: 0, blend: None },
            Instance { shape_id: 2, inst_name: None, sym_name: "x".into(), plane: None,
                aabb: Rect { x: 110.0, y: 0.0, w: 100.0, h: 20.0 }, cx: 160.0, cy: 10.0, x_sign: 1.0, flip: (false, false),
                moving: false, hazard: Some(HazardKind::Lava), inst_anchor: (0.0, 0.0), inst_path: 0, blend: None },
        ];
        let shapes = std::collections::BTreeMap::new();
        let bitmaps = std::collections::BTreeMap::new();
        let hz = detect_hazards(&insts, &id, &shapes, &bitmaps, 0.0, 0.0, false);
        assert_eq!(hz.len(), 1, "adjacent same-kind hazards merge");
        assert!(hz[0].1.w >= 210.0, "merged box spans both: {}", hz[0].1.w);
    }
}
