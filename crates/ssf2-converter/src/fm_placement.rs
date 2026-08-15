//! Where a piece of SSF2 art lands in Fraymakers.
//!
//! SSF2 content is SSF2 content: a character's arm and a stage's clock hand are both a shape placed
//! on a timeline by a matrix, and Fraymakers draws both as an IMAGE symbol with a position, a
//! signed scale and a rotation. So the conversion between the two lives here once, rather than
//! being re-derived per content type -- the character path had it and the stage path did not,
//! which is why stage art baked a fresh bitmap per angle instead of turning one.
//!
//! Two details this has to get right, both learned by measurement:
//!
//! * **The pivot moves with the matrix.** A shape's raster is cropped to its own bounds, so the
//!   art's top-left sits at the shape's pivot offset, not at the placement origin. That offset has
//!   to be carried THROUGH the placement matrix (`a·off_x + c·off_y`), or a rotated piece drifts
//!   away from where the source draws it.
//! * **A bitmap fill has its own scale.** A shape filled with a bitmap composes the fill's matrix
//!   with the placement's, so the emitted scale is the product and the pivot is in fill-scaled
//!   pixels. Without it a bitmap-filled shape comes out at the wrong size -- the case that showed
//!   up as a 13x oversized particle.

/// A placement's world transform, as the SWF describes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct WorldPlacement {
    /// Translation in pixels (SSF2 y-down).
    pub tx: f64,
    pub ty: f64,
    /// Signed scale (negative = mirrored on that axis).
    pub sx: f64,
    pub sy: f64,
    /// Rotation in degrees.
    pub rotation: f64,
    /// The affine components, for carrying the pivot through the same transform.
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
}

impl WorldPlacement {
    /// The identity placement at a position: no rotation, unit scale.
    pub fn at(tx: f64, ty: f64) -> Self {
        WorldPlacement { tx, ty, sx: 1.0, sy: 1.0, rotation: 0.0, a: 1.0, b: 0.0, c: 0.0, d: 1.0 }
    }
}

/// What a Fraymakers IMAGE symbol needs: where its art goes and how it is oriented.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImagePlacement {
    pub x: f64,
    pub y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    /// Degrees, normalised to [0, 360).
    pub rotation: f64,
}

fn round2(v: f64) -> f64 { (v * 100.0).round() / 100.0 }

/// Convert a world placement into the IMAGE symbol fields that draw it.
///
/// `pivot` is the art's top-left within the shape's own bounds and `fill_scale` the bitmap fill's
/// scale (both `(0,0)` / `(1,1)` when they do not apply).
pub fn image_placement(w: &WorldPlacement, pivot: (f64, f64), fill_scale: (f64, f64)) -> ImagePlacement {
    let (fsx, fsy) = fill_scale;
    let (off_x, off_y) = (pivot.0 * fsx, pivot.1 * fsy);
    ImagePlacement {
        x: round2(w.tx + w.a * off_x + w.c * off_y),
        y: round2(w.ty + w.b * off_x + w.d * off_y),
        scale_x: round2(w.sx * fsx),
        scale_y: round2(w.sy * fsy),
        rotation: normalise_degrees(w.rotation),
    }
}

/// Reflect a placement about the vertical axis: what a MIRRORED copy of the same art looks like.
///
/// Reflection is `Rot(-θ)·Scale(-sx, sy)` with `x -> -x`. The rotation negates along with
/// everything else -- a turn animation mirrored without it faces the right way and leans the wrong
/// way.
pub fn mirrored(p: ImagePlacement) -> ImagePlacement {
    ImagePlacement {
        x: round2(-p.x),
        scale_x: -p.scale_x,
        rotation: normalise_degrees(-p.rotation),
        ..p
    }
}

pub fn normalise_degrees(deg: f64) -> f64 { ((deg % 360.0) + 360.0) % 360.0 }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_placement_is_its_position() {
        let p = image_placement(&WorldPlacement::at(10.0, 20.0), (0.0, 0.0), (1.0, 1.0));
        assert_eq!(p, ImagePlacement { x: 10.0, y: 20.0, scale_x: 1.0, scale_y: 1.0, rotation: 0.0 });
    }

    #[test]
    fn the_pivot_travels_through_the_matrix() {
        // quarter turn: a pivot offset along x comes out along y
        let w = WorldPlacement { tx: 0.0, ty: 0.0, sx: 1.0, sy: 1.0, rotation: 90.0,
                                 a: 0.0, b: 1.0, c: -1.0, d: 0.0 };
        let p = image_placement(&w, (10.0, 0.0), (1.0, 1.0));
        assert_eq!((p.x, p.y), (0.0, 10.0), "an offset that is rotated must land rotated");
    }

    #[test]
    fn a_bitmap_fill_scales_both_the_art_and_its_pivot() {
        let w = WorldPlacement::at(0.0, 0.0);
        let p = image_placement(&w, (10.0, 4.0), (2.0, 3.0));
        assert_eq!((p.scale_x, p.scale_y), (2.0, 3.0));
        assert_eq!((p.x, p.y), (20.0, 12.0), "the pivot is in fill-scaled pixels");
    }

    #[test]
    fn mirroring_negates_position_scale_and_rotation() {
        let p = ImagePlacement { x: 30.0, y: 5.0, scale_x: 1.19, scale_y: 1.0, rotation: 150.0 };
        let m = mirrored(p);
        assert_eq!((m.x, m.scale_x, m.rotation), (-30.0, -1.19, 210.0));
        assert_eq!((m.y, m.scale_y), (5.0, 1.0), "the vertical axis is untouched");
    }
}
