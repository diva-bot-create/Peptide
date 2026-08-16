//! Reading a SWF timeline: what is on the display list, frame by frame.
//!
//! SSF2 content is SSF2 content. A character's arm and a stage's clock hand are both a character
//! placed on a depth by a matrix, moved and turned and faded by the frames that follow, and both
//! engines' converters need the same answer: what was on screen, where, and how, at frame N.
//!
//! That answer used to be worked out twice -- once in the stage parser and once (several times
//! over) in the character path -- and the two disagreed about how much of a placement to read. The
//! stage side read the matrix, the ratio, the blend, the colour transform and the visibility flag;
//! the character side read the matrix. So a fade in a stage backdrop converted as a fade and the
//! same fade on a character converted as a hard cut, and the bug had to be found once per content
//! type. This module is the one reader, so a placement detail learned anywhere is available
//! everywhere.
//!
//! What it does NOT do is decide what any of it MEANS: which depths are art, which are collision,
//! which clip is a hazard. That is content-specific and stays with the caller.

use std::collections::BTreeMap;

/// A placement's transform, in pixels (SWF stores twips).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Matrix {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Default for Matrix {
    fn default() -> Self {
        Matrix { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 }
    }
}

impl Matrix {
    fn of(m: &swf::Matrix) -> Self {
        Matrix {
            a: m.a.to_f64(), b: m.b.to_f64(), c: m.c.to_f64(), d: m.d.to_f64(),
            tx: m.tx.get() as f64 / 20.0, ty: m.ty.get() as f64 / 20.0,
        }
    }
}

/// One object on the display list: everything a placement carries.
///
/// The fields beyond the matrix are the ones that were being dropped. They are not decoration:
/// `alpha` is how Flash writes a fade or a flash, `visible` is how it hides something without
/// removing it, `ratio` pins a graphic to one of its frames, and `blend` changes how it composites.
#[derive(Clone, Debug, PartialEq)]
pub struct Placed {
    /// The placed character (shape or sprite) id.
    pub char_id: u16,
    /// The display-list slot. Stable across frames, which is what makes an object an OBJECT rather
    /// than a fresh thing each frame.
    pub depth: u16,
    pub matrix: Matrix,
    /// The PlaceObject instance name, when the source gave it one.
    pub name: Option<String>,
    /// A `ratio` on a sprite placement pins it to that frame ("Single Frame" in Flash).
    pub ratio: Option<u16>,
    pub blend: Option<swf::BlendMode>,
    /// The colour transform's alpha, as a multiplier in `0..=1` (1.0 when it has none).
    pub alpha: f64,
    /// The placement's visibility flag; `true` unless the source turned it off.
    pub visible: bool,
}

/// Snapshot the display list at every `ShowFrame`.
///
/// Flash semantics, which is the part worth stating once: `Place`/`Replace` set a depth,
/// `Modify` UPDATES whatever that depth already holds (and carries only the fields it is
/// changing -- a tween is a run of Modifies each with a new matrix or colour transform),
/// `RemoveObject` clears it, and `ShowFrame` is when the frame is what it is. A depth keeps its
/// state until something changes it, so a placement made on frame 1 is still there on frame 500.
///
/// Always at least one frame: a sprite with no `ShowFrame` still has content.
pub fn frames(tags: &[swf::Tag]) -> Vec<Vec<Placed>> {
    let mut depth: BTreeMap<u16, Placed> = BTreeMap::new();
    let mut out: Vec<Vec<Placed>> = Vec::new();
    for tag in tags {
        match tag {
            swf::Tag::PlaceObject(po) => {
                let name = po.name.as_ref()
                    .map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252).to_string());
                let alpha_of = |ct: &swf::ColorTransform| {
                    let m = f32::from(ct.a_multiply) as f64;
                    (m + f32::from(ct.a_add) as f64 / 255.0).clamp(0.0, 1.0)
                };
                match po.action {
                    swf::PlaceObjectAction::Place(cid) | swf::PlaceObjectAction::Replace(cid) => {
                        // A Replace (or a move-style Place) that carries no matrix/name/ratio KEEPS
                        // the slot's state and only swaps the character -- a pool of variants
                        // cycling through one depth. Resetting to identity stacks them all at the
                        // clip origin.
                        let prev = depth.get(&po.depth);
                        let e = Placed {
                            char_id: cid,
                            depth: po.depth,
                            matrix: po.matrix.as_ref().map(Matrix::of)
                                .or_else(|| prev.map(|p| p.matrix)).unwrap_or_default(),
                            name: name.or_else(|| prev.and_then(|p| p.name.clone())),
                            ratio: po.ratio.or_else(|| prev.and_then(|p| p.ratio)),
                            blend: po.blend_mode.or_else(|| prev.and_then(|p| p.blend)),
                            alpha: po.color_transform.as_ref().map(alpha_of)
                                .or_else(|| prev.map(|p| p.alpha)).unwrap_or(1.0),
                            visible: po.is_visible.or_else(|| prev.map(|p| p.visible)).unwrap_or(true),
                        };
                        depth.insert(po.depth, e);
                    }
                    swf::PlaceObjectAction::Modify => {
                        if let Some(e) = depth.get_mut(&po.depth) {
                            if let Some(m) = po.matrix.as_ref() { e.matrix = Matrix::of(m); }
                            if let Some(n) = name { e.name = Some(n); }
                            if let Some(r) = po.ratio { e.ratio = Some(r); }
                            if let Some(b) = po.blend_mode { e.blend = Some(b); }
                            if let Some(ct) = po.color_transform.as_ref() { e.alpha = alpha_of(ct); }
                            if let Some(v) = po.is_visible { e.visible = v; }
                        }
                    }
                }
            }
            swf::Tag::RemoveObject(r) => { depth.remove(&r.depth); }
            swf::Tag::ShowFrame => out.push(depth.values().cloned().collect()),
            _ => {}
        }
    }
    if out.is_empty() { out.push(depth.values().cloned().collect()); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use swf::{Tag, PlaceObject, PlaceObjectAction, Twips, Fixed16, Matrix as SwfMatrix, ColorTransform, Fixed8};

    fn place(depth: u16, id: u16) -> Tag<'static> {
        Tag::PlaceObject(Box::new(PlaceObject {
            version: 3, action: PlaceObjectAction::Place(id), depth,
            matrix: Some(SwfMatrix { a: Fixed16::ONE, b: Fixed16::ZERO, c: Fixed16::ZERO, d: Fixed16::ONE,
                                     tx: Twips::from_pixels(10.0), ty: Twips::from_pixels(20.0) }),
            color_transform: None, ratio: None, name: None, clip_depth: None, class_name: None,
            filters: None, background_color: None, blend_mode: None, clip_actions: None,
            has_image: false, is_bitmap_cached: None, is_visible: None, amf_data: None,
        }))
    }

    fn modify_alpha(depth: u16, a: f32) -> Tag<'static> {
        Tag::PlaceObject(Box::new(PlaceObject {
            version: 3, action: PlaceObjectAction::Modify, depth,
            matrix: None,
            color_transform: Some(ColorTransform {
                r_multiply: Fixed8::ONE, g_multiply: Fixed8::ONE, b_multiply: Fixed8::ONE,
                a_multiply: Fixed8::from_f32(a),
                r_add: 0, g_add: 0, b_add: 0, a_add: 0,
            }),
            ratio: None, name: None, clip_depth: None, class_name: None, filters: None,
            background_color: None, blend_mode: None, clip_actions: None, has_image: false,
            is_bitmap_cached: None, is_visible: None, amf_data: None,
        }))
    }

    fn hide(depth: u16) -> Tag<'static> {
        Tag::PlaceObject(Box::new(PlaceObject {
            version: 3, action: PlaceObjectAction::Modify, depth,
            matrix: None, color_transform: None, ratio: None, name: None, clip_depth: None,
            class_name: None, filters: None, background_color: None, blend_mode: None,
            clip_actions: None, has_image: false, is_bitmap_cached: None,
            is_visible: Some(false), amf_data: None,
        }))
    }

    /// A depth keeps its placement until something changes it.
    #[test]
    fn placement_persists_across_frames() {
        let f = frames(&[place(1, 7), Tag::ShowFrame, Tag::ShowFrame, Tag::ShowFrame]);
        assert_eq!(f.len(), 3);
        for frame in &f {
            assert_eq!(frame.len(), 1);
            assert_eq!(frame[0].char_id, 7);
            assert_eq!(frame[0].matrix.tx, 10.0);
        }
    }

    /// A Modify carrying a colour transform is how Flash tweens a fade -- the thing the character
    /// path was dropping.
    #[test]
    fn modify_carries_the_fade() {
        let f = frames(&[
            place(1, 7), Tag::ShowFrame,
            modify_alpha(1, 0.5), Tag::ShowFrame,
            modify_alpha(1, 0.0), Tag::ShowFrame,
        ]);
        assert_eq!(f[0][0].alpha, 1.0);
        assert!((f[1][0].alpha - 0.5).abs() < 0.01, "got {}", f[1][0].alpha);
        assert_eq!(f[2][0].alpha, 0.0);
        // and it does not disturb the transform it was not changing
        assert_eq!(f[2][0].matrix.tx, 10.0);
    }

    /// Hidden is not removed: the object stays on its depth, marked invisible.
    #[test]
    fn visibility_is_read_and_kept() {
        let f = frames(&[place(1, 7), Tag::ShowFrame, hide(1), Tag::ShowFrame]);
        assert!(f[0][0].visible);
        assert!(!f[1][0].visible);
        assert_eq!(f[1].len(), 1, "hiding is not removing");
    }

    /// RemoveObject clears the slot.
    #[test]
    fn remove_clears_the_depth() {
        let f = frames(&[
            place(1, 7), Tag::ShowFrame,
            Tag::RemoveObject(swf::RemoveObject { depth: 1, character_id: None }), Tag::ShowFrame,
        ]);
        assert_eq!(f[0].len(), 1);
        assert!(f[1].is_empty());
    }

    /// Content with no ShowFrame is still one frame of content.
    #[test]
    fn always_at_least_one_frame() {
        assert_eq!(frames(&[place(1, 7)]).len(), 1);
    }
}
