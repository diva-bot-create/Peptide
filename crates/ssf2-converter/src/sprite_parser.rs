/// sprite_parser — extracts per-animation, per-frame collision box geometry from SWF DefineSprite tags.
///
/// SSF2 encodes hitbox data entirely in the SWF timeline, not in AS3 code:
///   - Each animation lives in a named DefineSprite (e.g. `mario_fla.Jab_21`)
///   - Each frame of that sprite has PlaceObject tags for collision MovieClips (CollisonBox_6)
///   - The PlaceObject matrix encodes the box's position (tx/ty in twips) and size (a/d = scale)
///   - The PlaceObject instance name tells us the box type:
///       attackBox / attackBox2 / attackBox3 → Hitbox (active attack)
///       hitBox / hitBox2 / hitBox3 / hitBox4 / hitBox5 → Hurtbox (can be hit)
///       hurtBox → Hurtbox
///       grabBox / grabbox → GrabBox
///       itemBox → ItemBox (item pickup range)
///       shieldBox / shieldbox → ShieldBox
///       reflectBox → ReflectBox
///       absorbBox → AbsorbBox
///       ledgeBox / ledgegrabbox → LedgeBox
///       (anything else with "Box" suffix) → Hurtbox (fallback)
///
/// The base CollisonBox_6 shape is typically a 100×100 unit square centered at 0,0.
/// scale_x * BASE_SIZE = box width, scale_y * BASE_SIZE = box height.
/// tx/ty give the top-left (or center, depending on registration point) in pixels.

use std::collections::BTreeMap;
use std::io::Cursor;
use serde::{Deserialize, Serialize};

/// Base size of the CollisonBox shape in SSF2 (pixels after /20 twip conversion).
/// Measured from the actual shape bounds of the CollisonBox_6 DefineShape.
/// Fallback to 100.0 if shape not found.
const DEFAULT_BASE_SIZE: f64 = 100.0;

// ─── Output types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoxType {
    /// Active attack hitbox (dealg damage)
    Hitbox,
    /// Hurtbox (can receive damage)
    Hurtbox,
    /// Grab range
    GrabBox,
    /// Item pickup range
    ItemBox,
    /// Shield/parry box
    ShieldBox,
    /// Reflector hitbox
    ReflectBox,
    /// Absorb hitbox
    AbsorbBox,
    /// Ledge grab box
    LedgeBox,
    /// Grab hold point (where grabbed opponent is held)
    GrabHoldBox,
}

impl BoxType {
    /// Map SSF2 instance name → BoxType
    pub fn from_instance_name(name: &str) -> Option<BoxType> {
        let lower = name.to_lowercase();
        if lower.starts_with("attackbox") {
            Some(BoxType::Hitbox)
        } else if lower.starts_with("hitbox") || lower.starts_with("hurtbox") {
            Some(BoxType::Hurtbox)
        } else if lower.starts_with("grabbox")
            || (lower.starts_with("grab") && lower.ends_with("box"))
        {
            Some(BoxType::GrabBox)
        } else if lower.starts_with("itembox") || lower == "itembox" {
            Some(BoxType::ItemBox)
        } else if lower.starts_with("shieldbox") {
            Some(BoxType::ShieldBox)
        } else if lower.starts_with("reflectbox") {
            Some(BoxType::ReflectBox)
        } else if lower.starts_with("absorbbox") {
            Some(BoxType::AbsorbBox)
        } else if lower.starts_with("ledgebox") || lower.starts_with("ledgegrab") {
            Some(BoxType::LedgeBox)
        } else if lower.starts_with("touchbox") {
            Some(BoxType::GrabHoldBox)
        } else if lower.ends_with("box") {
            // generic fallback — treat unknown *box as hurtbox
            Some(BoxType::Hurtbox)
        } else {
            None
        }
    }

    /// Whether Fraymakers honors a non-zero `rotation` on this box type.
    /// FM only rotates `ItemBox` and custom boxes; every other collision
    /// box (hurt / hit / grab / shield / reflect / absorb / ledge /
    /// grab-hold) is axis-aligned in the engine, so a rotated source box
    /// must be collapsed into its axis-aligned bounding box with
    /// rotation = 0 before emission. We never emit a custom-box type, so
    /// `ItemBox` is the only rotation-honoring case today.
    pub fn supports_rotation(&self) -> bool {
        matches!(self, BoxType::ItemBox)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            BoxType::Hitbox    => "HITBOX",
            BoxType::Hurtbox   => "HURTBOX",
            BoxType::GrabBox   => "GRAB_BOX",
            BoxType::ItemBox   => "ITEM_BOX",
            BoxType::ShieldBox => "SHIELD_BOX",
            BoxType::ReflectBox => "REFLECT_BOX",
            BoxType::AbsorbBox => "ABSORB_BOX",
            BoxType::LedgeBox  => "LEDGE_BOX",
            BoxType::GrabHoldBox => "GRAB_HOLD_BOX",
        }
    }
}

/// A single collision box placed on one frame of one animation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameBox {
    /// Box type derived from instance name
    pub box_type: BoxType,
    /// Original SSF2 instance name (e.g. "attackBox", "hitBox3")
    pub instance_name: String,
    /// X position in pixels (top-left, relative to character origin)
    pub x: f64,
    /// Y position in pixels (top-left, relative to character origin)
    /// Note: SWF Y axis points DOWN; negative = above foot.
    pub y: f64,
    /// Box width in pixels
    pub width: f64,
    /// Box height in pixels
    pub height: f64,
    /// Rotation in degrees (always 0 for hit/hurtboxes; itemBox can rotate)
    pub rotation: f64,
}

/// All collision boxes for one animation, indexed by frame number (0-based).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationBoxData {
    /// SSF2 animation name (e.g. "a_air", "stand")
    pub ssf2_name: String,
    /// Fraymakers animation name (e.g. "aerial_neutral", "idle")
    pub fm_name: String,
    /// Total frames in this animation (after sub-anim slicing if applicable)
    pub total_frames: u16,
    /// per-frame boxes: frame_index (0-based within this sub-anim) → list of boxes active on that frame
    pub frames: BTreeMap<u16, Vec<FrameBox>>,
    /// Frame offset into the original SSF2 sprite where this sub-anim starts
    pub sprite_frame_offset: u16,
    /// Frame labels within this animation: (label_name, frame_index relative to sub-anim start)
    pub frame_labels: Vec<(String, u16)>,
    /// Frames where the displayed CONTENT changes (a Place/Replace/Remove altered the display
    /// set since the previous frame) — matrix-only Modify ticks don't count. Lets the splitter
    /// find phase boundaries in label-less clips (SSF2's falling: the hurt-image sequence
    /// switches every few frames, then one tumble image spins by matrix alone).
    pub content_switches: Vec<u16>,
}

/// Root MovieClip placement transform for one xframe animation.
/// The root character MC places each animation sub-sprite at a specific
/// offset and scale. These must be applied to all sub-sprite-local coords
/// (boxes, image pivots) to produce correct world-space positions.
#[derive(Debug, Clone, Copy)]
pub struct XframeTransform {
    /// Root placement offset X in pixels (SSF2 coordinate space)
    pub tx: f64,
    /// Root placement offset Y in pixels (SSF2 coordinate space, y-down)
    pub ty: f64,
    /// Root placement scaleX
    pub sx: f64,
    /// Root placement scaleY
    pub sy: f64,
    /// Full affine matrix components for correct world-space composition
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
}

impl Default for XframeTransform {
    fn default() -> Self {
        Self { tx: 0.0, ty: 0.0, sx: 1.0, sy: 1.0, a: 1.0, b: 0.0, c: 0.0, d: 1.0 }
    }
}

impl XframeTransform {
    /// Apply this root transform to a local point (lx, ly), returning world coords.
    pub fn apply(&self, lx: f64, ly: f64) -> (f64, f64) {
        (self.a * lx + self.b * ly + self.tx,
         self.c * lx + self.d * ly + self.ty)
    }

    /// Scale magnitude for a local scale value (ignores rotation/skew components).
    pub fn scale_x(&self) -> f64 { self.sx.abs() }
    pub fn scale_y(&self) -> f64 { self.sy.abs() }
}

// ─── Main entry point ────────────────────────────────────────────────────────

/// Extract per-animation root-MovieClip placement transforms.
///
/// The SSF2 root character sprite (e.g. "mario") places each animation
/// sub-sprite ("stance") on successive frames with a PlaceObject that
/// carries a translation (tx, ty) and scale (sx, sy). These transform
/// all local coordinates inside the sub-sprite into the character's
/// world space (where y=0 is the foot of the character).
///
/// Returns a map: FM animation name → XframeTransform.
/// For sub-animations (jab1/jab2 etc) the parent animation's transform is used.
pub fn extract_xframe_transforms(
    swf_data: &[u8],
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, XframeTransform>> {
    let swf_buf = swf::decompress_swf(Cursor::new(swf_data))?;
    let swf = swf::parse_swf(&swf_buf)?;
    extract_xframe_transforms_from_swf(&swf, char_name, ssf2_to_fm)
}

/// Count `"stance"`-named PlaceObjects in a sprite's timeline. The root
/// character MovieClip places its `stance` sub-sprite once per animation
/// frame, so this count is ~70-80 for the root and ≤1 for projectiles/effects.
fn count_stance_placements(sprite: &swf::Sprite) -> usize {
    sprite.tags.iter().filter(|t| {
        if let swf::Tag::PlaceObject(po) = t {
            po.name.as_ref()
                .map(|s| s.as_bytes() == b"stance")
                .unwrap_or(false)
        } else {
            false
        }
    }).count()
}

/// Identify the root character timeline MovieClip's sprite id.
///
/// Most characters export it under a symbol whose name equals the character id
/// (e.g. `mario`, `Sandbag`), but several use a variant the exact-match misses:
/// `sonicremastered`, `nessbase`, `meta_knight`, `dkong`, `falcon`,
/// `black_mage`, `giga_bowser`, `Wario_Man`. The signal that a sprite IS a root
/// timeline is its number of `"stance"` PlaceObjects — the root places the
/// stance sub-sprite once per animation frame (~70-80), far more than any
/// projectile or effect.
///
/// A root timeline places its stance sub-sprite once per animation frame, so it
/// carries dozens of `"stance"` placements (the observed roster range is 40-80).
/// Flash-generated `_fla.` sub-animation clips, even long ones like
/// `captainfalcon_fla.Revival_32` (150 frames), place stance ~once. This hard
/// threshold filters those out before any name matching — without it, a sub-clip
/// whose name starts with the character id would out-score the real root.
const MIN_ROOT_STANCES: usize = 15;
///
/// Among the surviving genuine roots, pick the best by a name-affinity score so
/// multi-character SSFs (bowser+giga_bowser, zelda+sheik, wario+Wario_Man)
/// assign each form its OWN root rather than whichever has the most stances:
///   3 = exact name, or name with `_`/spaces stripped (meta_knight→metaknight,
///       giga_bowser→gigabowser, Wario_Man→warioman)
///   2 = one name is a prefix of the other (ness→nessbase, sonic→sonicremastered)
///   1 = no name relation (dkong, falcon) — wins only as the lone root in its SWF
/// Ties at equal score are broken by stance count.
fn find_root_sprite_id(
    swf: &swf::Swf<'_>,
    sym_names: &BTreeMap<u16, String>,
    char_lower: &str,
) -> Option<u16> {
    // (score, stance_count, id), maximised lexicographically.
    let mut best: Option<(u8, usize, u16)> = None;
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            let count = count_stance_placements(sprite);
            if count < MIN_ROOT_STANCES { continue; }
            let sym_lower = sym_names.get(&sprite.id).cloned().unwrap_or_default().to_lowercase();
            let sym_norm: String = sym_lower.chars().filter(|c| *c != '_' && *c != ' ').collect();
            let score: u8 = if sym_lower == char_lower || sym_norm == char_lower {
                3
            } else if sym_lower.starts_with(char_lower) || char_lower.starts_with(&sym_lower) {
                2
            } else {
                1
            };
            let cand = (score, count, sprite.id);
            if best.map(|b| cand > b).unwrap_or(true) {
                best = Some(cand);
            }
        }
    }
    best.map(|(_, _, id)| id)
}

/// Same as `extract_xframe_transforms` but operates on an already-parsed
/// SWF. Use this when the caller has access to the parsed `swf::Swf` and
/// wants to avoid the redundant `decompress_swf` + `parse_swf` round-trip.
/// `main.rs::process_character` parses the SWF once and threads it through
/// every per-character extractor; the `_from_swf` variants are how.
pub fn extract_xframe_transforms_from_swf(
    swf: &swf::Swf<'_>,
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, XframeTransform>> {
    let mut sym_names: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                sym_names.insert(link.id, name);
            }
        }
    }

    let char_lower = char_name.to_lowercase();
    let root_id = find_root_sprite_id(swf, &sym_names, &char_lower);
    let mut result: BTreeMap<String, XframeTransform> = BTreeMap::new();

    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            if Some(sprite.id) != root_id { continue; }

            // Walk the root MC, tracking the current xframe label and the last
            // stance PlaceObject matrix seen.
            let mut current_frame: u16 = 0;
            let mut current_ssf2_label: Option<String> = None;
            // depth → last known matrix for stance
            let mut stance_matrix: Option<swf::Matrix> = None;

            for stag in &sprite.tags {
                match stag {
                    swf::Tag::FrameLabel(fl) => {
                        let label = fl.label.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                        current_ssf2_label = Some(label);
                        // Reset stance matrix for this new label frame
                        stance_matrix = None;
                    }
                    swf::Tag::PlaceObject(po) => {
                        let inst = po.name.as_ref()
                            .map(|s| String::from_utf8_lossy(s.as_bytes()).to_string())
                            .unwrap_or_default();
                        if inst == "stance" {
                            if let Some(m) = &po.matrix {
                                stance_matrix = Some(*m);
                            }
                        }
                    }
                    swf::Tag::ShowFrame => {
                        // On ShowFrame: record the transform for the current label
                        if let (Some(label), Some(m)) = (&current_ssf2_label, &stance_matrix) {
                            let tx = m.tx.get() as f64 / 20.0;
                            let ty = m.ty.get() as f64 / 20.0;
                            // Store full affine matrix for correct world-space composition
                            let a = m.a.to_f64();
                            let b = m.b.to_f64();
                            let c = m.c.to_f64();
                            let d = m.d.to_f64();
                            let sx = (a*a + b*b).sqrt();
                            let sy = (c*c + d*d).sqrt();

                            // Map SSF2 label → FM name
                            let fm_name = ssf2_to_fm.get(label.as_str())
                                .cloned()
                                .or_else(|| static_ssf2_to_fm(label))
                                .unwrap_or_else(|| label.clone());

                            // Also insert for sub-animations (jab1/jab2/jab3/jab4, taunt_up/taunt_down)
                            let xform = XframeTransform { tx, ty, sx, sy, a, b, c, d };
                            result.entry(fm_name.clone()).or_insert(xform);

                            // Seed sub-anim names with the same transform
                            for sub in crate::extractor::expand_split_anim(&fm_name) {
                                result.entry(sub).or_insert(xform);
                            }
                        }
                        current_frame += 1;
                        // After ShowFrame, clear label so next ShowFrame (same label re-entry) doesn't re-record
                        current_ssf2_label = None;
                    }
                    _ => {}
                }
            }
            let _ = current_frame;
            break;
        }
    }

    log::info!("extract_xframe_transforms: {} animations mapped", result.len());
    Ok(result)
}

/// Parse the SWF file and extract per-animation per-frame collision box data.
/// `ssf2_to_fm` maps SSF2 animation names to Fraymakers animation names.
pub fn parse_sprite_boxes(
    swf_data: &[u8],
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
) -> anyhow::Result<BTreeMap<String, AnimationBoxData>> {
    let swf_buf = swf::decompress_swf(Cursor::new(swf_data))?;
    let swf = swf::parse_swf(&swf_buf)?;
    let xform_map = extract_xframe_transforms_from_swf(&swf, char_name, ssf2_to_fm)
        .unwrap_or_default();
    parse_sprite_boxes_from_swf(&swf, char_name, ssf2_to_fm, &xform_map)
}

/// Map each animation's INNER sprite id → its SSF2 xframe label, by walking the
/// root MC timeline. THIS is the only correct source of an animation's identity:
/// an animation is the xframe `FrameLabel` on the root timeline, and the `stance`
/// PlaceObject under that label places the animation's inner sprite. We must NEVER
/// derive an animation name from the inner sprite's SYMBOL name — two animations
/// can share a symbol (and across a multi-char .ssf the symbol names collide), and
/// SSF2 routes by timeline placement, not by symbol. (Mirrors what dump_proj_states
/// observes and how extract_xframe_transforms already reads the same timeline.)
pub(crate) fn build_xframe_sprite_map(
    swf: &swf::Swf<'_>,
    sym_names: &BTreeMap<u16, String>,
    char_lower: &str,
) -> BTreeMap<u16, String> {
    let mut map: BTreeMap<u16, String> = BTreeMap::new();
    let root_id = find_root_sprite_id(swf, sym_names, char_lower);
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            if Some(sprite.id) != root_id { continue; }
            let mut cur_label = String::new();
            for stag in &sprite.tags {
                match stag {
                    swf::Tag::FrameLabel(fl) => {
                        cur_label = fl.label.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                    }
                    swf::Tag::PlaceObject(po) => {
                        let inst = po.name.as_ref()
                            .map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252).to_string());
                        if inst.as_deref() == Some("stance") {
                            if let swf::PlaceObjectAction::Place(cid) = &po.action {
                                if !cur_label.is_empty() {
                                    // First label that places a given inner sprite wins.
                                    map.entry(*cid).or_insert_with(|| cur_label.clone());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            break;
        }
    }
    map
}

/// THE single source of truth for "what animation is this sprite": sprite id →
/// (ssf2 anim name, fm anim name). Built from the timeline (build_xframe_sprite_map)
/// then run through ssf2_to_fm (static fallback). The box extractor AND the image
/// extractor BOTH call this, so they can never disagree about a sprite's animation —
/// if they computed it independently they could (and did) drift.
pub(crate) fn build_anim_sprite_map(
    swf: &swf::Swf<'_>,
    sym_names: &BTreeMap<u16, String>,
    char_lower: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
) -> BTreeMap<u16, (String, String)> {
    build_xframe_sprite_map(swf, sym_names, char_lower)
        .into_iter()
        .map(|(id, ssf2)| {
            let fm = ssf2_to_fm.get(&ssf2).cloned()
                .or_else(|| static_ssf2_to_fm(&ssf2))
                .unwrap_or_else(|| ssf2.clone());
            (id, (ssf2, fm))
        })
        .collect()
}

/// Frames (0-based) where a sprite's displayed content changes: any Place/Replace/Remove since
/// the previous ShowFrame. Matrix-only Modify tags don't count, so an image spinning in place
/// (the SSF2 tumble) reads as a single content run.
fn extract_content_switch_frames(sprite: &swf::Sprite) -> Vec<u16> {
    let mut out = Vec::new();
    let mut frame: u16 = 0;
    let mut changed = false;
    for tag in &sprite.tags {
        match tag {
            swf::Tag::PlaceObject(po) => match &po.action {
                swf::PlaceObjectAction::Place(_) | swf::PlaceObjectAction::Replace(_) => changed = true,
                swf::PlaceObjectAction::Modify => {}
            },
            swf::Tag::RemoveObject(_) => changed = true,
            swf::Tag::ShowFrame => {
                if changed { out.push(frame); }
                changed = false;
                frame += 1;
            }
            _ => {}
        }
    }
    out
}

/// Same as `parse_sprite_boxes` but operates on an already-parsed SWF and
/// a precomputed xform_map. Main.rs parses each character's SWF once and
/// computes its xform_map once, then threads both here to skip the
/// redundant decompress + parse + xform-extraction round-trip.
pub fn parse_sprite_boxes_from_swf(
    swf: &swf::Swf<'_>,
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
    xform_map: &BTreeMap<String, XframeTransform>,
) -> anyhow::Result<BTreeMap<String, AnimationBoxData>> {
    // Build id → symbol name map
    let mut sym_names: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                sym_names.insert(link.id, name);
            }
        }
    }

    // Find the base size of the collision box shape
    let box_base_size = find_collision_box_base_size(swf, &sym_names);
    log::info!("CollisonBox base size: {:.1}px", box_base_size);
    log::info!("Root MC transforms: {} animations", xform_map.len());

    // Find all character animation sprites
    // Pattern: "{char}_fla.{AnimName}_{index}" or just "{char}_{animtype}"
    let char_lower = char_name.to_lowercase();
    let mut result: BTreeMap<String, AnimationBoxData> = BTreeMap::new();

    // Shared, timeline-derived sprite→animation map (the SAME one the image
    // extractor uses — never derived from the sprite's symbol name).
    let anim_by_sprite = build_anim_sprite_map(swf, &sym_names, &char_lower, ssf2_to_fm);

    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            let sym = sym_names.get(&sprite.id).cloned().unwrap_or_default();

            // (ssf2 name, fm name) for this sprite, or skip if it's not a timeline animation.
            let (ssf2_name, fm_name) = match anim_by_sprite.get(&sprite.id) {
                Some((s, f)) => (s.clone(), f.clone()),
                None => continue,
            };

            // Look up root MC transform for this animation
            let xform = xform_map.get(&fm_name).copied().unwrap_or_default();

            // Collect internal frame labels (for sub-animation splitting)
            let frame_labels = extract_frame_labels_from_tags(&sprite.tags);

            let frames = extract_frame_boxes(sprite, &sym_names, box_base_size, xform);
            let content_switches = extract_content_switch_frames(sprite);

            log::debug!("Sprite '{}' → ssf2='{}' fm='{}': {} frames with boxes, {} labels",
                sym, ssf2_name, fm_name, frames.len(), frame_labels.len());

            // Check if this animation should be split into sub-animations
            let sub_splits = sub_anim_splits(&fm_name, &frame_labels, sprite.num_frames);

            if sub_splits.is_empty() {
                // Single animation — insert as-is
                if frames.is_empty() { continue; }
                result.insert(fm_name.clone(), AnimationBoxData {
                    ssf2_name,
                    fm_name,
                    total_frames: sprite.num_frames,
                    frames,
                    sprite_frame_offset: 0,
                    frame_labels: frame_labels.clone(),
                    content_switches,
                });
            } else {
                // Split into multiple FM animations
                for (sub_fm_name, start_frame, end_frame) in sub_splits {
                    let slice_len = end_frame.saturating_sub(start_frame);
                    // Remap frame indices: subtract start_frame so they are 0-based within sub-anim
                    let sliced_frames: BTreeMap<u16, Vec<FrameBox>> = frames.iter()
                        .filter(|(&f, _)| f >= start_frame && f < end_frame)
                        .map(|(&f, boxes)| (f - start_frame, boxes.clone()))
                        .collect();
                    // Remap frame labels the same way
                    let sliced_labels: Vec<(String, u16)> = frame_labels.iter()
                        .filter(|(_, f)| *f >= start_frame && *f < end_frame)
                        .map(|(name, f)| (name.clone(), f - start_frame))
                        .collect();

                    log::debug!("  sub-anim '{}': frames {}..{} ({} frames with boxes, {} labels)",
                        sub_fm_name, start_frame, end_frame, sliced_frames.len(), sliced_labels.len());

                    result.insert(sub_fm_name.clone(), AnimationBoxData {
                        ssf2_name: ssf2_name.clone(),
                        fm_name: sub_fm_name,
                        total_frames: slice_len,
                        frames: sliced_frames,
                        sprite_frame_offset: start_frame,
                        frame_labels: sliced_labels,
                        content_switches: content_switches.iter()
                            .filter(|&&f| f >= start_frame && f < end_frame)
                            .map(|f| f - start_frame).collect(),
                    });
                }
            }
        }
    }

    log::info!("sprite_parser: extracted box data for {}/{} animations before fallbacks",
        result.len(), ssf2_to_fm.len());

    // Apply fallbacks: for animations with no sprite data, clone from the closest related state.
    // These are procedural states in SSF2 that reuse another animation's pose.
    apply_fallbacks(&mut result);

    log::info!("sprite_parser: {}/{} animations have box data after fallbacks",
        result.len(), ssf2_to_fm.len());

    // debug (`PEPTIDE_DUMP_BOX_FRAMES`): the SOURCE active-frame range of each animation's
    // attack boxes, the counterpart of the emitted entity's HIT_BOX keyframes. Animation
    // LENGTHS are checked by timing_parity.py; this is the other half of frame data — which
    // frames a move can actually hit on, i.e. its startup / active / recovery. Reported as
    // source (30fps) frames, so the emitted Fraymakers range should be twice it.
    if std::env::var("PEPTIDE_DUMP_BOX_FRAMES").is_ok() {
        for (fm_name, d) in &result {
            // Count BOX-frames, not frames: a frame carrying two hitboxes contributes two.
            // The emitted entity sums keyframe lengths across EVERY HIT_BOX layer, so
            // counting unique frames here compares two different units and reports a
            // two-box move as 4.00x when it is exactly right.
            let per_frame: Vec<(u16, usize)> = d.frames.iter()
                .map(|(f, boxes)| (*f, boxes.iter().filter(|b| b.box_type == BoxType::Hitbox).count()))
                .filter(|(_, n)| *n > 0)
                .collect();
            if per_frame.is_empty() { continue; }
            let box_frames: usize = per_frame.iter().map(|(_, n)| *n).sum();
            eprintln!("[box-frames] {fm_name} total={} active={} first={} last={}",
                d.total_frames, box_frames, per_frame[0].0, per_frame[per_frame.len() - 1].0);
        }
    }

    Ok(result)
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

// ─── Sub-animation splitting ─────────────────────────────────────────────────

/// Extract all FrameLabel tags from a SWF tag list, returning (label,
/// frame_number) pairs sorted by frame number. Public so `image_extractor`
/// can reuse the same implementation instead of carrying a duplicate copy.
pub fn extract_frame_labels_from_tags(tags: &[swf::Tag]) -> Vec<(String, u16)> {
    let mut frame_num: u16 = 0;
    let mut labels: Vec<(String, u16)> = Vec::new();
    for tag in tags {
        match tag {
            swf::Tag::ShowFrame => { frame_num += 1; }
            swf::Tag::FrameLabel(fl) => {
                let label = fl.label.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                labels.push((label, frame_num));
            }
            _ => {}
        }
    }
    labels.sort_by_key(|(_, f)| *f);
    labels
}

/// Public alias for sub_anim_splits — used by image_extractor to apply the same split.
pub fn sub_anim_image_splits(
    fm_name: &str,
    frame_labels: &[(String, u16)],
    total_frames: u16,
) -> Vec<(String, u16, u16)> {
    sub_anim_splits(fm_name, frame_labels, total_frames)
}

/// SSF2 frame labels that are control-flow markers, not real sub-animation boundaries.
/// These appear in SSF2 ActionScript as `gotoAndPlay("again")` / `gotoAndPlay("finish")`
/// to implement looping/chaining; they don't correspond to distinct Fraymakers animations.
const CONTROL_FLOW_LABELS: &[&str] = &[
    "goback",   // loop back to start of current hit
    "again",    // restart the combo chain if button pressed
    "finish",   // tail-end recovery animation
    "loop",     // generic loop point
    "afterHit", // post-hit recovery
    "endlag",   // end-lag frames
    "continue", // generic continue marker
    "done",     // end of sub-sequence (used in Jump, not jab)
];

/// Taunt sprite label names that map to the three Fraymakers taunt slots.
const TAUNT_LABEL_NAMES: &[&str] = &[
    "taunt_side", "taunt_neutral", "taunt_updown",
];
const TAUNT_FM_NAMES: &[&str] = &["taunt", "taunt_up", "taunt_down"];

/// Given an FM animation name and the internal frame labels of its SSF2 sprite,
/// return a list of (fm_sub_anim_name, start_frame_inclusive, end_frame_exclusive).
/// Returns empty vec if no splitting is needed (animation maps 1:1).
///
/// # Jab splitting strategy
/// Every character uses different label names for jab hits (begin/hit2/hit3,
/// combo1/combo2/combo3, jab2/jab3, kick2, hitHold, etc.).
/// Rather than matching by name, we:
///   1. Filter out known control-flow labels (again, finish, loop, goback, ...)
///   2. Whatever real labels remain — map them positionally to jab1/jab2/jab3/...
///   3. Each real label marks the START of its sub-animation slice.
///   4. If only one real label exists (or the first label starts well into the sprite)
///      we include everything from frame 0 as jab1, starting at the first real label.
fn sub_anim_splits(
    fm_name: &str,
    frame_labels: &[(String, u16)],
    total_frames: u16,
) -> Vec<(String, u16, u16)> {
    match fm_name {
        "jab" => split_jab(frame_labels, total_frames),
        "taunt" => split_taunt(frame_labels, total_frames),
        _ => vec![],
    }
}

/// Split a jab sprite into jab1/jab2/jab3/... by filtering control-flow labels
/// and mapping remaining labels positionally.
fn split_jab(frame_labels: &[(String, u16)], total_frames: u16) -> Vec<(String, u16, u16)> {
    // Filter to only real sub-animation boundary labels
    let real_labels: Vec<(u16, &str)> = frame_labels.iter()
        .filter(|(label, _)| !CONTROL_FLOW_LABELS.contains(&label.as_str()))
        .map(|(label, frame)| (*frame, label.as_str()))
        .collect();

    if real_labels.is_empty() {
        // No labels at all — single jab1 covering the whole sprite
        return vec![("jab1".to_string(), 0, total_frames)];
    }

    // Build ranges: each real label starts a sub-animation that ends at the next one
    let mut splits = Vec::new();
    for (i, &(start_frame, _label)) in real_labels.iter().enumerate() {
        let end_frame = real_labels.get(i + 1)
            .map(|&(f, _)| f)
            .unwrap_or(total_frames);
        let fm_name = format!("jab{}", i + 1);
        splits.push((fm_name, start_frame, end_frame));
        log::trace!("  jab split: '{}' (ssf2 label='{}') frames {}..{}",
            splits.last().unwrap().0, _label, start_frame, end_frame);
    }
    splits
}

/// Split a taunt sprite using the three known SSF2 taunt label names.
/// Unlike jab, taunts have fixed semantic label names across all characters.
fn split_taunt(frame_labels: &[(String, u16)], total_frames: u16) -> Vec<(String, u16, u16)> {
    let label_map: std::collections::HashMap<&str, u16> = frame_labels.iter()
        .map(|(l, f)| (l.as_str(), *f))
        .collect();

    // All three taunt labels must be present
    if TAUNT_LABEL_NAMES.iter().any(|l| !label_map.contains_key(l)) {
        // This character doesn't have the standard 3-taunt layout — don't split
        return vec![];
    }

    let starts: Vec<u16> = TAUNT_LABEL_NAMES.iter()
        .map(|l| *label_map.get(l).unwrap())
        .collect();

    starts.iter().enumerate()
        .map(|(i, &start)| {
            let end = starts.get(i + 1).copied().unwrap_or(total_frames);
            (TAUNT_FM_NAMES[i].to_string(), start, end)
        })
        .collect()
}

// ─── Fallbacks ────────────────────────────────────────────────────────────────

/// For animations with no extracted sprite data, clone box data from the most
/// appropriate related animation. The cloned data keeps the same box shapes but
/// marks the animation name correctly so it lands in the right entity layer.
/// Missing FM anim name → best donor FM anim name. One shared table for both the
/// collision-box fallbacks (`apply_fallbacks`) and the image fallbacks
/// (`image_extractor::apply_image_fallbacks`) so the two can't drift apart.
pub(crate) const ANIM_FALLBACKS: &[(&str, &str)] = &[
    // Damage / launched states
    ("stunned",           "hurt"),
    ("star_ko",           "hurt"),
    ("starko",            "hurt"),
    ("screenko",          "hurt"),
    ("buried",            "crouch"),
    ("tumble",            "fall"),
    ("frozen",            "stand"),
    // Airborne/misc states
    ("fly",               "jump_aerial"),
    ("swim",              "fall"),
    ("ladder",            "stand"),
    ("wall_stick",        "fall"),
    ("special",           "stand"),
    ("carry",             "grab"),
    // Landing variants
    ("land_heavy",        "land"),
    ("ledge_lean",        "ledge_hang"),
    // Win/lose/respawn
    ("victory",           "taunt"),
    ("defeat",            "hurt"),
    ("respawn",           "stand"),
    // Special air variants
    ("special_down_air",  "special_down"),
    ("special_neutral_air", "special_neutral"),
    ("special_side_air",  "special_side"),
    ("special_up_air",    "special_up"),
    // Item variants
    ("item_float",        "stand"),
    ("item_screw",        "special_up"),
];

fn apply_fallbacks(result: &mut BTreeMap<String, AnimationBoxData>) {
    let mut to_insert: Vec<AnimationBoxData> = Vec::new();

    for (missing, donor) in ANIM_FALLBACKS {
        if result.contains_key(*missing) { continue; }
        if let Some(donor_data) = result.get(*donor) {
            log::debug!("Fallback: '{}' ← '{}' ({} frames)", missing, donor, donor_data.total_frames);
            let mut cloned = donor_data.clone();
            cloned.fm_name = missing.to_string();
            to_insert.push(cloned);
        } else {
            log::debug!("Fallback: '{}' ← '{}' (donor also missing)", missing, donor);
        }
    }

    for data in to_insert {
        result.insert(data.fm_name.clone(), data);
    }
}

/// Find the pixel size of one side of the CollisionBox base shape.
/// SSF2 uses a square shape; we want the width (= height for square).
/// Measure a collision-box character's intrinsic size in pixels.
/// The box character is usually a DefineSprite wrapping a square DefineShape;
/// descend through sprites, folding in each placement matrix's scale so a
/// non-identity inner transform is accounted for.
fn measure_box_char(swf: &swf::Swf, char_id: u16, depth: u8) -> Option<f64> {
    if depth > 4 { return None; }
    for tag in &swf.tags {
        match tag {
            swf::Tag::DefineShape(shape) if shape.id == char_id => {
                let b = &shape.shape_bounds;
                let w = (b.x_max.get() - b.x_min.get()) as f64 / 20.0;
                let h = (b.y_max.get() - b.y_min.get()) as f64 / 20.0;
                return Some((w + h) / 2.0);
            }
            swf::Tag::DefineSprite(sprite) if sprite.id == char_id => {
                for stag in &sprite.tags {
                    if let swf::Tag::PlaceObject(po) = stag {
                        let child = match &po.action {
                            swf::PlaceObjectAction::Place(id)
                            | swf::PlaceObjectAction::Replace(id) => *id,
                            _ => continue,
                        };
                        let scale = po.matrix
                            .map(|m| (m.a.to_f64().abs() + m.d.to_f64().abs()) / 2.0)
                            .unwrap_or(1.0);
                        return measure_box_char(swf, child, depth + 1).map(|s| s * scale);
                    }
                }
                return None;
            }
            _ => {}
        }
    }
    None
}

/// Determine the base (unscaled, pre-PlaceObject-matrix) size of the
/// collision-box shape in pixels.
///
/// SSF2 places every collision box as the same character — a small square
/// shape scaled per instance by the PlaceObject matrix. We find that
/// character by tallying which char id the box-named instances
/// (attackBox / hitBox / grabBox / …) actually place, then measure its
/// shape. The symbol is not reliably named "CollisonBox" (sandbag's isn't),
/// so name-based lookup is only a last-resort fallback.
fn find_collision_box_base_size(swf: &swf::Swf, sym_names: &BTreeMap<u16, String>) -> f64 {
    // Tally char ids placed under box-typed instance names (excluding
    // itemBox, which is a separate character with its own geometry).
    let mut tally: BTreeMap<u16, u32> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            for stag in &sprite.tags {
                if let swf::Tag::PlaceObject(po) = stag {
                    let child = match &po.action {
                        swf::PlaceObjectAction::Place(id)
                        | swf::PlaceObjectAction::Replace(id) => *id,
                        _ => continue,
                    };
                    let Some(name) = po.name.as_ref()
                        .map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252).to_string())
                    else { continue };
                    if let Some(bt) = BoxType::from_instance_name(&name) {
                        if bt != BoxType::ItemBox {
                            *tally.entry(child).or_insert(0) += 1;
                        }
                    }
                }
            }
        }
    }
    if let Some((&char_id, _)) = tally.iter().max_by_key(|(_, &count)| count) {
        if let Some(size) = measure_box_char(swf, char_id, 0) {
            log::info!("CollisonBox char id={}: measured base size {:.1}px", char_id, size);
            return size;
        }
    }

    // Fallback: locate the box character by symbol name.
    let collison_sprite_id = sym_names.iter()
        .find(|(_, name)| {
            let l = name.to_lowercase();
            l.contains("collisonbox") || l.contains("collisionbox")
        })
        .map(|(id, _)| *id);
    if let Some(sprite_id) = collison_sprite_id {
        if let Some(size) = measure_box_char(swf, sprite_id, 0) {
            log::info!("CollisonBox (by name) id={}: base size {:.1}px", sprite_id, size);
            return size;
        }
    }

    log::warn!("CollisonBox shape not found; using default base size {:.0}px", DEFAULT_BASE_SIZE);
    DEFAULT_BASE_SIZE
}

/// Extract SSF2 animation name from a symbol name like "mario_fla.NAir_40".
/// Uses the ssf2_to_fm map to validate/match against known animation names.
pub fn extract_ssf2_anim_name(
    sym: &str,
    char_lower: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
) -> Option<String> {
    // Symbol format: "{char}_fla.{AnimLabel}_{index}"
    // e.g. "mario_fla.NAir_40" → local = "NAir_40" → strip suffix → "NAir" → normalize → "a_air"
    let local = if sym.contains('.') {
        sym.split('.').next_back()?
    } else {
        sym
    };

    // Strip trailing _NNN
    let stripped = strip_numeric_suffix(local);

    // Some characters name their move sprites with a REDUNDANT char prefix inside the
    // label instead of "{char}_fla.{Move}" (mario's "mario_fla.NAir"):
    //   fox:        "fox_fla.fox_airN"   → strip "fox_"  → "airN"
    //   donkeykong: "donkeykong_fla.dkbair" → strip "dk" → "bair" (in-game short code)
    // Strip the longest matching prefix so the label resolves. Case-insensitive; only
    // strips when actually present, so mario-style labels are unaffected.
    let stripped = {
        let lc = stripped.to_ascii_lowercase();
        // Candidate prefixes, longest first: "{char}_", then the char's short code.
        let cl = char_lower.to_ascii_lowercase();
        let mut prefixes = vec![format!("{cl}_")];
        // In-game short codes for characters whose sprite labels use them (the label
        // has no underscore separator, e.g. "dkbair"). Extend as more are found.
        if let Some(sc) = match cl.as_str() {
            "donkeykong" => Some("dk"),
            _ => None,
        } { prefixes.push(sc.to_string()); }
        let mut s = stripped;
        for pfx in &prefixes {
            if lc.starts_with(pfx.as_str()) && stripped.len() > pfx.len() {
                s = &stripped[pfx.len()..];
                break;
            }
        }
        s
    };

    // Try direct match against SSF2 anim names (case-insensitive)
    // Build a normalized version: "NAir" → "nair", then match against known patterns
    let normalized = normalize_anim_label(stripped);

    // First try exact match
    if ssf2_to_fm.contains_key(&normalized) {
        return Some(normalized);
    }

    // Try the label → SSF2 name table (loaded from
    // mappings/character/animations.json — see crate::mappings).
    crate::mappings::character_animations()
        .label_to_ssf2
        .get(&normalized)
        .cloned()
}

/// Static SSF2 xframe name → Fraymakers animation name lookup.
/// Consults the same `ssf2_to_fm` table as extractor::build_ssf2_to_fm_anim
/// (loaded from mappings/character/animations.json) without needing the
/// dynamic xframe map. Used when a character's bytecode doesn't call
/// setXFrame for a given animation (e.g. their jab sprite exists but they
/// use a custom label instead of 'a').
pub fn static_ssf2_to_fm(ssf2_name: &str) -> Option<String> {
    crate::mappings::character_animations()
        .ssf2_to_fm
        .get(ssf2_name)
        .cloned()
}

fn strip_numeric_suffix(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    // Strip trailing _NNN
    if let Some(pos) = s.rfind('_') {
        let suffix = &s[pos + 1..];
        if suffix.chars().all(|c| c.is_ascii_digit()) {
            end = pos;
        }
    }
    &s[..end]
}

fn normalize_anim_label(s: &str) -> String {
    // CamelCase → lowercase, remove underscores for matching
    s.to_lowercase().replace('_', "")
}

/// Extract per-frame collision box data from a single DefineSprite.
/// Returns a map of frame_index → list of boxes.
///
/// Strategy: simulate the Flash display list.
/// PlaceObject::Place(id) with name = new object at depth
/// PlaceObject::Modify = move/transform existing object at depth (no character change)
/// PlaceObject::Replace(id) = replace object at depth
/// Extract collision boxes for a specific DefineSprite by ID.
/// Used for projectile inner sprites (e.g. mario_fireball_mc).
/// Returns AnimationBoxData with frames keyed 0-based.
pub fn extract_boxes_for_sprite_id(
    swf_data: &[u8],
    sprite_id: u16,
) -> anyhow::Result<Option<AnimationBoxData>> {
    let swf_buf = swf::decompress_swf(Cursor::new(swf_data))?;
    let swf = swf::parse_swf(&swf_buf)?;
    extract_boxes_for_sprite_id_from_swf(&swf, sprite_id)
}

/// `extract_boxes_for_sprite_id` against an already-parsed SWF. Used per
/// projectile / per effect in main.rs to avoid the per-call SWF parse.
pub fn extract_boxes_for_sprite_id_from_swf(
    swf: &swf::Swf<'_>,
    sprite_id: u16,
) -> anyhow::Result<Option<AnimationBoxData>> {
    let mut sym_names: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                sym_names.insert(link.id, name);
            }
        }
    }

    let box_base_size = find_collision_box_base_size(swf, &sym_names);
    let identity = XframeTransform::default();

    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            if sprite.id != sprite_id { continue; }
            let frames = extract_frame_boxes(sprite, &sym_names, box_base_size, identity);
            if frames.is_empty() { return Ok(None); }
            return Ok(Some(AnimationBoxData {
                ssf2_name: String::new(),
                fm_name: String::new(),
                total_frames: sprite.num_frames,
                frames,
                sprite_frame_offset: 0,
                frame_labels: vec![],
                content_switches: extract_content_switch_frames(sprite),
            }));
        }
    }
    Ok(None)
}

/// RemoveObject = remove from display list
///
/// We track the display list across frames and snapshot the current boxes each frame.
fn extract_frame_boxes(
    sprite: &swf::Sprite,
    sym_names: &BTreeMap<u16, String>,
    base_size: f64,
    root_xform: XframeTransform,
) -> BTreeMap<u16, Vec<FrameBox>> {
    // Display list: depth → (char_id, instance_name, matrix)
    let mut display_list: BTreeMap<u16, DisplayItem> = BTreeMap::new();
    let mut current_frame: u16 = 0;
    let mut result: BTreeMap<u16, Vec<FrameBox>> = BTreeMap::new();

    // Pre-identify the collision box char id from symbol names
    // Any symbol whose name contains "collisonbox" or "collisionbox"
    let is_collision_char: std::collections::HashSet<u16> = sym_names.iter()
        .filter(|(_, n)| {
            let l = n.to_lowercase();
            l.contains("collisonbox") || l.contains("collisionbox")
        })
        .map(|(id, _)| *id)
        .collect();

    for tag in &sprite.tags {
        match tag {
            swf::Tag::ShowFrame => {
                // Snapshot collision boxes active this frame
                let boxes: Vec<FrameBox> = display_list.values()
                    .filter(|item| {
                        // Include if it's a known collision char id
                        // OR if the instance name looks like a box
                        is_collision_char.contains(&item.char_id)
                            || BoxType::from_instance_name(&item.instance_name).is_some()
                    })
                    .filter_map(|item| {
                        let box_type = BoxType::from_instance_name(&item.instance_name)?;
                        let sx = root_xform.sx;
                        let sy = root_xform.sy;

                        if box_type == BoxType::ItemBox {
                            // itemBox (id=991): the PlaceObject tx/ty is the hand attachment
                            // point, which is at the BOTTOM of the shape (origin of id=991).
                            // The shape hangs upward from this point.
                            let (lcx, lcy, lw, lh, rot) = matrix_to_itembox(&item.matrix);
                            // Hand position in world space
                            let whx = root_xform.tx + lcx * sx;
                            let why = root_xform.ty + lcy * sy;
                            let ww = lw * sx.abs();
                            let wh = lh * sy.abs();
                            // Top-left: hand is at bottom-center, so top = hand_y - height
                            return Some(FrameBox {
                                box_type,
                                instance_name: item.instance_name.clone(),
                                x: whx - ww / 2.0,
                                y: why - wh,
                                width: ww,
                                height: wh,
                                rotation: rot,
                            });
                        }

                        // CollisonBox_6: centered at origin. matrix_to_box
                        // recovers the box dims (w, h) and rotation θ from
                        // the full matrix decomposition (√(a²+b²), √(c²+d²),
                        // atan2(b,a)) — correct even for ~90° boxes whose
                        // scale lives in the off-diagonal b/c terms.
                        let (local_x, local_y, local_w, local_h, rot) = matrix_to_box(&item.matrix, base_size);

                        // Per-BoxType rotation decision (see
                        // finalize_box_geometry). FM honors rotation only on
                        // rotation-capable box types; every other type
                        // (hurt / hit / grab / shield / reflect / absorb /
                        // ledge / grab-hold) collapses the rotated box into
                        // the AABB that contains it and emits rotation = 0.
                        // The ItemBox path is handled in the dedicated branch
                        // above (it has its own hand-anchor extractor), so
                        // here `box_type` is never rotation-capable today and
                        // the helper returns the AABB; the call is routed
                        // through it anyway so a future CustomBox emitted with
                        // a real FM custom-box type keeps its rotation
                        // automatically.
                        let (fin_w, fin_h, fin_rot) =
                            finalize_box_geometry(box_type, local_w, local_h, rot);
                        // The AABB / kept box shares the source box's center.
                        let cx = local_x + local_w / 2.0;
                        let cy = local_y + local_h / 2.0;
                        let box_x = cx - fin_w / 2.0;
                        let box_y = cy - fin_h / 2.0;

                        let world_x = root_xform.tx + box_x * sx;
                        let world_y = root_xform.ty + box_y * sy;
                        let world_w = (fin_w * sx).abs();
                        let world_h = (fin_h * sy).abs();
                        Some(FrameBox {
                            box_type,
                            instance_name: item.instance_name.clone(),
                            x: world_x,
                            y: world_y,
                            width: world_w,
                            height: world_h,
                            rotation: fin_rot,
                        })
                    })
                    .collect();

                if !boxes.is_empty() {
                    // Invariant: a frame's box instance names are unique. The display
                    // list is depth-keyed and Flash authoring keeps instance names
                    // unique per timeline, so each name maps to exactly one box here.
                    // entity_gen relies on this — it keys one FM layer (and one
                    // sym_box_{anim}_{inst}_{frame} symbol) per instance name and
                    // takes the first box per name. If the invariant is ever violated
                    // the extra box would be silently dropped and the symbol seeds
                    // would alias, so warn loudly instead of failing quietly.
                    let mut seen = std::collections::HashSet::new();
                    for b in &boxes {
                        if !seen.insert(b.instance_name.as_str()) {
                            log::warn!(
                                "frame {} has duplicate box instance name '{}'; only the first is kept",
                                current_frame, b.instance_name
                            );
                        }
                    }
                    result.insert(current_frame, boxes);
                }
                current_frame += 1;
            }

            swf::Tag::PlaceObject(po) => {
                let inst_name = po.name
                    .map(|s| s.to_str_lossy(encoding_rs::WINDOWS_1252).to_string())
                    .unwrap_or_default();

                match &po.action {
                    swf::PlaceObjectAction::Place(char_id) => {
                        // Only track if it's a collision box char OR has a box-like name
                        let is_box = is_collision_char.contains(char_id)
                            || BoxType::from_instance_name(&inst_name).is_some();
                        if is_box || !inst_name.is_empty() {
                            display_list.insert(po.depth, DisplayItem {
                                char_id: *char_id,
                                instance_name: inst_name,
                                matrix: po.matrix.unwrap_or(swf::Matrix::IDENTITY),
                            });
                        }
                    }
                    swf::PlaceObjectAction::Replace(char_id) => {
                        let is_box = is_collision_char.contains(char_id)
                            || BoxType::from_instance_name(&inst_name).is_some();
                        if is_box || !inst_name.is_empty() {
                            let entry = display_list.entry(po.depth).or_insert(DisplayItem {
                                char_id: *char_id,
                                instance_name: inst_name.clone(),
                                matrix: swf::Matrix::IDENTITY,
                            });
                            entry.char_id = *char_id;
                            if !inst_name.is_empty() { entry.instance_name = inst_name; }
                            if let Some(m) = po.matrix { entry.matrix = m; }
                        }
                    }
                    swf::PlaceObjectAction::Modify => {
                        // Update position of existing object
                        if let Some(entry) = display_list.get_mut(&po.depth) {
                            if let Some(m) = po.matrix { entry.matrix = m; }
                            if !inst_name.is_empty() { entry.instance_name = inst_name; }
                        }
                    }
                }
            }

            swf::Tag::RemoveObject(ro) => {
                display_list.remove(&ro.depth);
            }

            _ => {}
        }
    }

    result
}

struct DisplayItem {
    char_id: u16,
    instance_name: String,
    matrix: swf::Matrix,
}

/// Convert a SWF matrix to (top_left_x, top_left_y, width, height, rotation_deg)
/// in pixels / degrees. The CollisonBox shape is base_size × base_size pixels,
/// **centered at (0,0)**; tx/ty give the **center** position.
///
/// Scale must come from the full column-vector magnitudes, NOT just `a`/`d`.
/// A box rotated by θ stores its scale in the off-diagonal `b`/`c` terms:
///   a = sx·cosθ   b = sx·sinθ
///   c = -sy·sinθ  d = sy·cosθ
/// so an `a`/`d`-only read collapses a 90°-rotated box to 0×0 (the
/// sandbag aerial_down frames 7-8 hurtbox bug). Mirror the IMAGE path's
/// decomposition (AGENT_CONTEXT §"SWF Matrix Decomposition"):
///   scale_x = √(a²+b²)   scale_y = √(c²+d²)   rotation = atan2(b, a)
///
/// Rotation is emitted so entity_gen can orient the box; a collision
/// rectangle is symmetric under reflection/180°, so magnitude scales +
/// atan2(b,a) reproduce the covered area faithfully even when the SWF
/// matrix carries a flip.
/// Axis-aligned bounding box `(width, height)` that fully contains a
/// `w × h` rectangle rotated by `rotation_deg` about its center.
///
/// Fraymakers does not honor rotation on hurt / hit / grab / shield /
/// reflect / absorb / ledge / grab-hold boxes (only `ItemBox` + custom
/// boxes rotate), so the converter collapses a rotated source box into
/// this AABB and emits `rotation = 0`. For `rotation_deg ≈ 0` the AABB
/// equals `(w, h)`, leaving axis-aligned boxes unchanged.
///
///   AABB_w = |w·cosθ| + |h·sinθ|
///   AABB_h = |w·sinθ| + |h·cosθ|
fn aabb_of_rotated_box(w: f64, h: f64, rotation_deg: f64) -> (f64, f64) {
    let rad = rotation_deg.to_radians();
    let (cos, sin) = (rad.cos().abs(), rad.sin().abs());
    (w * cos + h * sin, w * sin + h * cos)
}

/// Decide a collision box's final `(width, height, rotation)` from its
/// decomposed source size + rotation and its `BoxType`.
///
/// Fraymakers honors a non-zero rotation only on rotation-capable box
/// types (`BoxType::supports_rotation()` — `ItemBox`, and `CustomBox`
/// once we emit a real FM custom-box type). For those, the box is kept
/// as-is `(w, h, θ)`. For every other type the rotation is collapsed
/// into the axis-aligned bounding box that contains the rotated
/// rectangle, emitted with `rotation = 0`. For `θ ≈ 0` the AABB equals
/// `(w, h)`, so axis-aligned boxes are byte-identical either way.
fn finalize_box_geometry(box_type: BoxType, w: f64, h: f64, rotation_deg: f64) -> (f64, f64, f64) {
    if box_type.supports_rotation() {
        (w, h, rotation_deg)
    } else {
        let (aabb_w, aabb_h) = aabb_of_rotated_box(w, h, rotation_deg);
        (aabb_w, aabb_h, 0.0)
    }
}

fn matrix_to_box(m: &swf::Matrix, base_size: f64) -> (f64, f64, f64, f64, f64) {
    // tx/ty are in twips (1/20 pixel) → convert to pixels
    let cx = m.tx.get() as f64 / 20.0;  // center x
    let cy = m.ty.get() as f64 / 20.0;  // center y

    let a = m.a.to_f64();
    let b = m.b.to_f64();
    let c = m.c.to_f64();
    let d = m.d.to_f64();

    let scale_x = (a * a + b * b).sqrt();
    let scale_y = (c * c + d * d).sqrt();

    // Only emit a rotation when the matrix is genuinely off-axis (b/c
    // non-negligible). For an axis-aligned box — including a horizontal
    // or vertical flip (negative a/d, but b≈c≈0) — keep rotation 0 so
    // the output stays byte-identical to the pre-fix axis-aligned path.
    // A collision rectangle is symmetric under reflection, so dropping
    // the flip's would-be 180° rotation changes nothing geometrically
    // while avoiding churn across every flipped box in the corpus.
    const OFF_AXIS_EPS: f64 = 1e-4;
    let rotation_deg = if b.abs() < OFF_AXIS_EPS && c.abs() < OFF_AXIS_EPS {
        0.0
    } else {
        b.atan2(a).to_degrees()
    };

    let w = scale_x * base_size;
    let h = scale_y * base_size;

    // The CollisonBox shape is centered at the origin, so tx/ty is the
    // CENTER. Rotation in entity_gen pivots about the box center, so the
    // top-left of the un-rotated box is still center − (w/2, h/2).
    let x = cx - w / 2.0;
    let y = cy - h / 2.0;

    (x, y, w, h, rotation_deg)
}

/// Extract itemBox geometry from its PlaceObject matrix.
/// The itemBox (id=991) contains itemPlaceholder_mc_5 (id=990) at offset (-1.45, -20.95),
/// which contains a thin ~3.7x21.9 shape. The PlaceObject places id=991 with scale+rotation.
/// The relevant geometry for Fraymakers is: center = (tx, ty), effective size from scale.
/// Returns (center_x, center_y, scaled_width, scaled_height, rotation_degrees).
fn matrix_to_itembox(m: &swf::Matrix) -> (f64, f64, f64, f64, f64) {
    let cx = m.tx.get() as f64 / 20.0;
    let cy = m.ty.get() as f64 / 20.0;

    // The inner placeholder shape is roughly 3.7w x 21.9h centered near its origin.
    // The itemBox inner offset is (-1.45, -20.95) relative to id=991's origin,
    // meaning the shape's midpoint is approximately at (-1.45 + 1.85, -20.95 + 10.95) = (0.4, -10.0)
    // relative to id=991's origin. Close enough to (0, -10.0) for practical purposes.
    //
    // In SSF2, the item box represents the grab range for holding items.
    // It's placed with scale controlling overall size and b/c matrix for rotation.
    // We extract: scale magnitude → size, atan2(b, a) → rotation.
    let sx = m.a.to_f64();
    let _sy = m.d.to_f64();
    let bx = m.b.to_f64();
    let _by = m.c.to_f64();

    // Rotation from the matrix (degrees, CCW positive in SWF = CW positive visually)
    let rotation_rad = bx.atan2(sx);  // atan2(b, a) gives rotation
    let rotation_deg = rotation_rad.to_degrees();

    // Scale magnitude (handles rotation shear)
    let scale = (sx * sx + bx * bx).sqrt();

    // The placeholder inner shape: ~3.7w x 21.9h
    // After scale: these are approximate — the visual grab box.
    let inner_w = 3.7;
    let inner_h = 21.9;
    let w = inner_w * scale;
    let h = inner_h * scale;

    (cx, cy, w, h, rotation_deg)
}

// ─── Root MovieClip xframe scale extraction ──────────────────────────────────

/// Extract the median scaleX and scaleY from the root character MovieClip's
/// xframe PlaceObject entries (the `stance` sub-sprites placed on each frame).
///
/// SSF2 characters have a root DefineSprite whose SymbolClass name matches the
/// character name exactly (e.g. "mario"). Each frame of this sprite places
/// animation sub-sprites via PlaceObject with instance name "stance".
/// The matrix on these PlaceObjects contains the visual scale of the character.
///
/// We use the median (absolute value) to ignore outliers like "flying" (near-zero
/// to hide the sprite) or "frozen"/"egg" (oversized).
///
/// Returns `(median_scale_x, median_scale_y)`, or `(1.0, 1.0)` if no data found.
pub fn extract_xframe_scale(
    swf_data: &[u8],
    char_name: &str,
) -> anyhow::Result<(f64, f64)> {
    let swf_buf = swf::decompress_swf(Cursor::new(swf_data))?;
    let swf = swf::parse_swf(&swf_buf)?;
    extract_xframe_scale_from_swf(&swf, char_name)
}

/// `extract_xframe_scale` against an already-parsed SWF.
pub fn extract_xframe_scale_from_swf(
    swf: &swf::Swf<'_>,
    char_name: &str,
) -> anyhow::Result<(f64, f64)> {
    let mut sym_names: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                sym_names.insert(link.id, name);
            }
        }
    }

    let char_lower = char_name.to_lowercase();
    let root_id = find_root_sprite_id(swf, &sym_names, &char_lower);
    let mut scale_xs: Vec<f64> = Vec::new();
    let mut scale_ys: Vec<f64> = Vec::new();

    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            // Match the root character sprite (identified by stance-placement
            // count, since some chars name it e.g. `dkong`/`falcon` not the id).
            if Some(sprite.id) != root_id { continue; }
            let sym = sym_names.get(&sprite.id).cloned().unwrap_or_default();

            log::info!("Found root character MovieClip: id={} sym='{}' ({} frames)",
                sprite.id, sym, sprite.num_frames);

            for stag in &sprite.tags {
                if let swf::Tag::PlaceObject(po) = stag {
                    let inst = po.name.as_ref()
                        .map(|s| String::from_utf8_lossy(s.as_bytes()).to_string())
                        .unwrap_or_default();
                    // Only collect scale from "stance" placements (the xframe sub-sprites)
                    if inst != "stance" { continue; }

                    if let Some(m) = &po.matrix {
                        let sx = m.a.to_f64().abs();
                        let sy = m.d.to_f64().abs();
                        // Skip near-zero scales (hidden sprites, e.g. "flying")
                        if sx > 0.01 && sy > 0.01 {
                            scale_xs.push(sx);
                            scale_ys.push(sy);
                        }
                    } else {
                        // No matrix = identity
                        scale_xs.push(1.0);
                        scale_ys.push(1.0);
                    }
                }
            }
            break; // Only one root sprite per character
        }
    }

    if scale_xs.is_empty() {
        log::warn!("No xframe stance placements found for '{}', defaulting to 1.0", char_name);
        return Ok((1.0, 1.0));
    }

    scale_xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
    scale_ys.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let med_x = median_f64(&scale_xs);
    let med_y = median_f64(&scale_ys);

    log::info!("xframe scale for '{}': median scaleX={:.4}, scaleY={:.4} ({} samples)",
        char_name, med_x, med_y, scale_xs.len());

    Ok((med_x, med_y))
}

fn median_f64(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 { return 0.0; }
    if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 }
}

#[cfg(test)]
mod matrix_to_box_tests {
    use super::{matrix_to_box, aabb_of_rotated_box, finalize_box_geometry, BoxType};
    use swf::{Matrix, Fixed16, Twips};

    fn mat(a: f32, b: f32, c: f32, d: f32, tx: f32, ty: f32) -> Matrix {
        Matrix {
            a: Fixed16::from_f32(a),
            b: Fixed16::from_f32(b),
            c: Fixed16::from_f32(c),
            d: Fixed16::from_f32(d),
            tx: Twips::from_pixels(tx as f64),
            ty: Twips::from_pixels(ty as f64),
        }
    }

    fn approx(a: f64, b: f64) -> bool { (a - b).abs() < 0.5 }

    #[test]
    fn axis_aligned_box_unchanged() {
        // a/d on the diagonal, b=c=0: width=|a|*bs, height=|d|*bs, rotation=0.
        let (x, y, w, h, rot) = matrix_to_box(&mat(0.8, 0.0, 0.0, 0.5, 10.0, 20.0), 100.0);
        assert!(approx(w, 80.0),  "w={}", w);
        assert!(approx(h, 50.0),  "h={}", h);
        assert!(approx(rot, 0.0), "rot={}", rot);
        // center (10,20) → top-left (10-40, 20-25)
        assert!(approx(x, -30.0), "x={}", x);
        assert!(approx(y, -5.0),  "y={}", y);
    }

    #[test]
    fn rotated_90_box_keeps_size_via_bc() {
        // The sandbag aerial_down frames 7-8 bug: a≈0, d≈0, scale lives
        // in b/c. The old a/d-only read returned 0×0; the fix recovers
        // width=√(a²+b²)·bs, height=√(c²+d²)·bs.
        // f7 hitBox: a=0, b=0.316, c=-0.745, d=0 → w=31.6 h=74.5 rot=90°.
        let (_x, _y, w, h, rot) = matrix_to_box(&mat(0.0, 0.316, -0.745, 0.0, 0.0, 0.0), 100.0);
        assert!(approx(w, 31.6), "w={}", w);
        assert!(approx(h, 74.5), "h={}", h);
        assert!(approx(rot, 90.0), "rot={}", rot);
        assert!(w > 0.0 && h > 0.0, "box must NOT be degenerate");
    }

    #[test]
    fn horizontal_flip_stays_rotation_zero() {
        // Negative a with b=c=0 is a horizontal flip, not a rotation.
        // The epsilon guard keeps rotation 0 (a rectangle is symmetric
        // under reflection) so flipped boxes don't churn to rotation 180.
        let (_x, _y, w, h, rot) = matrix_to_box(&mat(-0.8, 0.0, 0.0, 0.5, 0.0, 0.0), 100.0);
        assert!(approx(w, 80.0), "w={}", w);
        assert!(approx(h, 50.0), "h={}", h);
        assert!(approx(rot, 0.0), "flip must stay rotation 0, got {}", rot);
    }

    #[test]
    fn partial_rotation_decomposes() {
        // 45°-ish rotation: a=d=cos, b=sin, c=-sin, uniform scale 1.0.
        let s = std::f32::consts::FRAC_1_SQRT_2; // cos45 = sin45 ≈ 0.707
        let (_x, _y, w, h, rot) = matrix_to_box(&mat(s, s, -s, s, 0.0, 0.0), 100.0);
        assert!(approx(w, 100.0), "w={}", w);
        assert!(approx(h, 100.0), "h={}", h);
        assert!(approx(rot, 45.0), "rot={}", rot);
    }

    // ── AABB collapse (FM only rotates ItemBox / custom boxes) ──────────

    #[test]
    fn aabb_axis_aligned_is_unchanged() {
        // θ = 0 → AABB equals the original box. Critical: keeps every
        // non-rotated collision box byte-identical (no golden churn).
        let (w, h) = aabb_of_rotated_box(80.0, 50.0, 0.0);
        assert!(approx(w, 80.0), "w={}", w);
        assert!(approx(h, 50.0), "h={}", h);
    }

    #[test]
    fn aabb_90_degrees_swaps_dimensions() {
        // A 31.6×74.5 box rotated 90° fits an AABB of 74.5×31.6 — the
        // dimensions swap. (sandbag aerial_down hurtbox case.)
        let (w, h) = aabb_of_rotated_box(31.6, 74.5, 90.0);
        assert!(approx(w, 74.5), "w={}", w);
        assert!(approx(h, 31.6), "h={}", h);
    }

    #[test]
    fn aabb_45_degrees_expands_both_dims() {
        // A 100×100 box at 45° needs a √2·100 ≈ 141.4 square AABB.
        let (w, h) = aabb_of_rotated_box(100.0, 100.0, 45.0);
        assert!(approx(w, 141.4), "w={}", w);
        assert!(approx(h, 141.4), "h={}", h);
        assert!(w > 100.0 && h > 100.0, "45° AABB must be larger than the box");
    }

    #[test]
    fn aabb_never_degenerate_for_nonzero_box() {
        // No angle collapses a non-zero box to 0 — preserves the
        // earlier degenerate-box fix.
        for deg in [0.0, 30.0, 60.0, 90.0, 123.4, 270.0] {
            let (w, h) = aabb_of_rotated_box(31.6, 74.5, deg);
            assert!(w > 0.0 && h > 0.0, "AABB degenerate at {}°: {}x{}", deg, w, h);
        }
    }

    #[test]
    fn only_itembox_supports_rotation() {
        // The BoxType gate driving the AABB-vs-keep-rotation decision.
        assert!(BoxType::ItemBox.supports_rotation());
        for bt in [BoxType::Hurtbox, BoxType::Hitbox, BoxType::GrabBox,
                   BoxType::ShieldBox, BoxType::ReflectBox, BoxType::AbsorbBox,
                   BoxType::LedgeBox, BoxType::GrabHoldBox] {
            assert!(!bt.supports_rotation(), "{:?} must NOT support rotation", bt);
        }
    }

    // ── Per-BoxType geometry finalization (the integration of the rule) ──

    #[test]
    fn finalize_hurtbox_axis_aligned_unchanged() {
        // θ=0 hurtbox: AABB == (w,h), rotation 0. Byte-identical to old.
        let (w, h, rot) = finalize_box_geometry(BoxType::Hurtbox, 80.0, 50.0, 0.0);
        assert!(approx(w, 80.0) && approx(h, 50.0) && approx(rot, 0.0),
            "got ({}, {}, {})", w, h, rot);
    }

    #[test]
    fn finalize_hurtbox_90_collapses_to_aabb_rotation_zero() {
        // 90° hurtbox → AABB swaps dims, rotation forced to 0.
        let (w, h, rot) = finalize_box_geometry(BoxType::Hurtbox, 31.6, 74.5, 90.0);
        assert!(approx(w, 74.5), "w={}", w);
        assert!(approx(h, 31.6), "h={}", h);
        assert!(approx(rot, 0.0), "hurtbox rotation must collapse to 0, got {}", rot);
    }

    #[test]
    fn finalize_hurtbox_45_collapses_to_aabb_rotation_zero() {
        let (w, h, rot) = finalize_box_geometry(BoxType::Hurtbox, 100.0, 100.0, 45.0);
        assert!(approx(w, 141.4) && approx(h, 141.4), "got {}x{}", w, h);
        assert!(approx(rot, 0.0), "rotation must be 0, got {}", rot);
    }

    #[test]
    fn finalize_itembox_keeps_rotation() {
        // ItemBox is rotation-capable: dims + rotation pass through as-is
        // (no AABB collapse). This is the path the earlier itemBox fix
        // depends on.
        let (w, h, rot) = finalize_box_geometry(BoxType::ItemBox, 31.6, 74.5, 90.0);
        assert!(approx(w, 31.6), "w must be unchanged, got {}", w);
        assert!(approx(h, 74.5), "h must be unchanged, got {}", h);
        assert!(approx(rot, 90.0), "itembox must keep rotation, got {}", rot);

        let (w2, h2, rot2) = finalize_box_geometry(BoxType::ItemBox, 40.0, 20.0, 45.0);
        assert!(approx(w2, 40.0) && approx(h2, 20.0) && approx(rot2, 45.0),
            "itembox 45° must pass through, got ({}, {}, {})", w2, h2, rot2);
    }

    #[test]
    fn finalize_hitbox_and_grabbox_also_collapse() {
        // Every non-rotation-capable type collapses identically.
        for bt in [BoxType::Hitbox, BoxType::GrabBox, BoxType::ShieldBox,
                   BoxType::ReflectBox, BoxType::AbsorbBox, BoxType::LedgeBox,
                   BoxType::GrabHoldBox] {
            let (_w, _h, rot) = finalize_box_geometry(bt, 31.6, 74.5, 90.0);
            assert!(approx(rot, 0.0), "{:?} must collapse rotation to 0, got {}", bt, rot);
        }
    }
}
