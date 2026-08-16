//! Where a piece of SSF2 art lands in Fraymakers.
//!
//! SSF2 content is SSF2 content: a character's arm and a stage's clock hand are both a shape placed
//! on a timeline by a matrix, and Fraymakers draws both as an IMAGE symbol with a position, a
//! signed scale and a rotation. So the conversion between the two lives here once, rather than
//! being re-derived per content type.
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

    /// Decompose an affine matrix into the position, signed scale and rotation that draw it.
    ///
    /// There is one convention and this is it: **`sx` stays positive and a mirror goes in `sy`**,
    /// chosen by the determinant's sign. The tempting alternative -- sign each axis by its own
    /// diagonal term (`a < 0`, `d < 0`) -- is wrong, and wrong in a way that hides: `atan2(b, a)`
    /// ALREADY returns an angle past 90 degrees for a negated x column, so signing `sx` as well
    /// counts the same flip twice. A half turn decomposes as "180 degrees, both axes mirrored",
    /// which reconstructs to the identity. It looks correct for every gentle angle and falls apart
    /// past a quarter turn, which reads as art that is fine at some angles and displaced at others.
    pub fn from_affine(a: f64, b: f64, c: f64, d: f64, tx: f64, ty: f64) -> Self {
        WorldPlacement {
            tx, ty, a, b, c, d,
            sx: (a * a + b * b).sqrt(),
            sy: (c * c + d * d).sqrt() * if a * d - b * c < 0.0 { -1.0 } else { 1.0 },
            rotation: b.atan2(a).to_degrees(),
        }
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

    /// The whole point of turning one picture instead of pre-drawing many: a shape rasterised
    /// UNROTATED at native size, placed by this module, must land exactly where the source matrix
    /// puts it -- at every angle, mirrored or not.
    ///
    /// Fraymakers turns an IMAGE about its stored `(x, y)` (measured), so the image's corners are
    /// `(x,y) + R(rot)·(scale·corner)`. The source's corners are the shape's own bounds through the
    /// placement matrix. This checks those two agree, which is the invariant a runtime rotation
    /// depends on and pre-drawn per-angle cels do not.
    #[test]
    fn one_unrotated_raster_lands_where_the_matrix_puts_it() {
        // a shape whose own bounds do NOT start at the origin, since that offset is what a naive
        // placement drops
        let (bx0, by0, bx1, by1) = (-12.0, 7.0, 30.0, 25.0);

        for &mirror in &[false, true] {
            for step in 0..36 {
                let deg = step as f64 * 10.0;
                let (t, s) = (deg.to_radians().cos(), deg.to_radians().sin());
                let (scale_x, scale_y) = (1.7, 1.7);
                // SWF y-down: e_x -> (sx·cos, sx·sin), e_y -> (-sy·sin, sy·cos); a mirror
                // negates the x column.
                let m = if mirror { -1.0 } else { 1.0 };
                let (a, b) = (m * scale_x * t, m * scale_x * s);
                let (c, d) = (-scale_y * s, scale_y * t);
                let (tx, ty) = (140.0, -60.0);

                let w = WorldPlacement::from_affine(a, b, c, d, tx, ty);
                // the raster is the shape's own bounds, drawn unrotated at native size, so its
                // top-left IS the shape's top-left
                let p = image_placement(&w, (bx0, by0), (1.0, 1.0));

                for &(u, v) in &[(0.0, 0.0), (bx1 - bx0, 0.0), (0.0, by1 - by0), (bx1 - bx0, by1 - by0)] {
                    // where the source draws this corner
                    let want = (a * (bx0 + u) + c * (by0 + v) + tx,
                                b * (bx0 + u) + d * (by0 + v) + ty);
                    // where Fraymakers draws it: turned about the stored position
                    let r = p.rotation.to_radians();
                    let (ux, vy) = (p.scale_x * u, p.scale_y * v);
                    let got = (p.x + ux * r.cos() - vy * r.sin(),
                               p.y + ux * r.sin() + vy * r.cos());
                    assert!((want.0 - got.0).abs() < 0.02 && (want.1 - got.1).abs() < 0.02,
                        "deg {deg} mirror {mirror} corner ({u},{v}): want {want:?} got {got:?}");
                }
            }
        }
    }

    /// Decompose, then rebuild the matrix from the pieces: they must agree at every angle,
    /// mirrored or not. This is the test that catches signing each axis by its own diagonal term,
    /// where a half turn rebuilds as the identity.
    #[test]
    fn decomposition_rebuilds_the_matrix() {
        for &mirror in &[false, true] {
            for step in 0..36 {
                let deg = step as f64 * 10.0;
                let (t, s) = (deg.to_radians().cos(), deg.to_radians().sin());
                let m = if mirror { -1.0 } else { 1.0 };
                let (want_sx, want_sy) = (1.7, 0.8);
                let (a, b) = (m * want_sx * t, m * want_sx * s);
                let (c, d) = (-want_sy * s, want_sy * t);

                let w = WorldPlacement::from_affine(a, b, c, d, 0.0, 0.0);
                let r = w.rotation.to_radians();
                // how a renderer rebuilds it: scale, then turn
                let (ra, rb) = (w.sx * r.cos(), w.sx * r.sin());
                let (rc, rd) = (-w.sy * r.sin(), w.sy * r.cos());
                for (got, want, what) in [(ra, a, "a"), (rb, b, "b"), (rc, c, "c"), (rd, d, "d")] {
                    assert!((got - want).abs() < 1e-9,
                        "deg {deg} mirror {mirror}: {what} rebuilt {got} want {want}");
                }
            }
        }
    }

    #[test]
    fn mirroring_negates_position_scale_and_rotation() {
        let p = ImagePlacement { x: 30.0, y: 5.0, scale_x: 1.19, scale_y: 1.0, rotation: 150.0 };
        let m = mirrored(p);
        assert_eq!((m.x, m.scale_x, m.rotation), (-30.0, -1.19, 210.0));
        assert_eq!((m.y, m.scale_y), (5.0, 1.0), "the vertical axis is untouched");
    }
}

impl WorldPlacement {
    /// Where this placement sends a point given in the SHAPE's own space.
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (self.a * x + self.c * y + self.tx, self.b * x + self.d * y + self.ty)
    }

    /// The point in the SHAPE's own space that this placement sends to `(wx, wy)`.
    ///
    /// The inverse is what lets one raster be re-placed by a different frame's matrix: the cel was
    /// cropped from the frame it was rasterised in, so to know where it goes in another frame you
    /// first ask which part of the shape it was.
    pub fn unapply(&self, wx: f64, wy: f64) -> Option<(f64, f64)> {
        let det = self.a * self.d - self.b * self.c;
        if det.abs() < 1e-9 { return None; }
        let (dx, dy) = (wx - self.tx, wy - self.ty);
        Some(((self.d * dx - self.c * dy) / det, (-self.b * dx + self.a * dy) / det))
    }
}
