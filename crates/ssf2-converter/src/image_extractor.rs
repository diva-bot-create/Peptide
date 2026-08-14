/// Extracts bitmap images from SWF tags and writes them as PNGs.
///
/// SWF structure for character sprites:
///   DefineSprite (animation) → PlaceObject (shape_id) → DefineShape (bitmap fill) → DefineBitsLossless
///
/// We extract the raw bitmaps and name the PNGs by their symbol name (e.g. mario_i0.png).
/// We also build a shape_id → bitmap_id mapping so callers can resolve
/// DefineSprite PlaceObject references to actual image files.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct ExtractedImage {
    pub bitmap_id: u16,
    pub symbol_name: String,
    pub width: u32,
    pub height: u32,
    pub png_path: String, // relative path within character output dir
    /// True if this image was rasterized from a vector DefineShape/Morph (usually an
    /// effect), false for a real character bitmap (DefineBits). The skew-bake palette
    /// re-snap only applies to character bitmaps, never vector-rendered effects.
    pub is_vector: bool,
}

/// Maps shape/character IDs to bitmap IDs (DefineShape → bitmap fill ID)
pub type ShapeToBitmapMap = BTreeMap<u16, u16>;

/// One frame's stacked image layers: (depth, symbol_name, local placement matrix).
type FrameLayers = Vec<(u16, String, ImageLocalMatrix)>;
/// Per-sprite frame table: sprite id → its frames, each a list of image layers.
type SpriteFrameTable = BTreeMap<u16, Vec<FrameLayers>>;

/// Local placement matrix for an image within its sub-sprite (in SSF2 local pixel space).
/// tx/ty are in pixels (already divided by 20 from twips).
/// sx/sy are scale magnitudes (always positive).
/// Full 2D affine matrix from SWF PlaceObject.
/// rotation is in degrees, derived from the matrix b/c shear components.
#[derive(Debug, Clone, Copy)]
pub struct ImageLocalMatrix {
    pub tx: f64,
    pub ty: f64,
    pub sx: f64,
    pub sy: f64,
    /// Rotation in degrees (from matrix a/b components). 0 = no rotation.
    pub rotation: f64,
    /// Full affine matrix components for skew detection & pre-rendering
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
}

impl Default for ImageLocalMatrix {
    fn default() -> Self { Self { tx: 0.0, ty: 0.0, sx: 1.0, sy: 1.0, rotation: 0.0, a: 1.0, b: 0.0, c: 0.0, d: 1.0 } }
}

impl ImageLocalMatrix {
    /// Construct from raw SWF matrix components.
    ///
    /// Scale/rotation decomposition:
    ///   sx = magnitude of x-axis column vector (always positive)
    ///   sy = magnitude of y-axis column vector, NEGATED when det<0 (encodes flip)
    ///   rotation = atan2(b, a) — angle of the x-axis column
    ///
    /// With sy negative (flip), FrayTools reconstruct:
    ///   x' = sx*cos(rot)*x - sy*sin(rot)*y
    ///   y' = sx*sin(rot)*x + sy*cos(rot)*y
    /// which exactly matches the original a/b/c/d when the flip is encoded in sy.
    pub fn from_abcd(a: f64, b: f64, c: f64, d: f64, tx: f64, ty: f64) -> Self {
        let sx_mag = (a*a + b*b).sqrt();
        let sy_mag = (c*c + d*d).sqrt();
        let det = a * d - b * c;
        // Encode the flip in sy: negative det means an odd number of reflections.
        // Convention: keep sx positive and put the flip sign into sy.
        let sy = if det < 0.0 { -sy_mag } else { sy_mag };
        Self {
            tx, ty,
            sx: sx_mag,
            sy,
            rotation: b.atan2(a).to_degrees(),
            a, b, c, d,
        }
    }

    /// Compose two matrices: apply `other` first, then `self`
    /// (i.e. self ∘ other). The SWF matrix maps (x,y) →
    /// (a·x + c·y + tx, b·x + d·y + ty).
    ///
    /// The previous implementation multiplied the wrong off-diagonal
    /// terms, which injected a false shear into every composed
    /// sub-sprite / effect matrix whenever rotation or non-uniform
    /// scale was involved.
    pub fn compose(&self, other: &ImageLocalMatrix) -> Self {
        let a = self.a * other.a + self.c * other.b;
        let b = self.b * other.a + self.d * other.b;
        let c = self.a * other.c + self.c * other.d;
        let d = self.b * other.c + self.d * other.d;
        let tx = self.a * other.tx + self.c * other.ty + self.tx;
        let ty = self.b * other.tx + self.d * other.ty + self.ty;
        Self::from_abcd(a, b, c, d, tx, ty)
    }

    /// Returns true if this matrix contains a skew (not just scale + rotation + flip).
    /// A pure rotation (with optional flip) has the x-axis and y-axis perpendicular.
    /// When sy < 0 (flip encoded), the y-axis column direction is negated.
    pub fn has_skew(&self) -> bool {
        let rot1 = self.b.atan2(self.a);
        // When sy is negative the y-axis direction is flipped, so compare against
        // the negated column to see if the axes are still perpendicular.
        let rot2 = if self.sy < 0.0 {
            self.c.atan2(-self.d) // negated y-axis
        } else {
            (-self.c).atan2(self.d)
        };
        // Normalize angle difference to (-π, π]
        let mut diff = rot1 - rot2;
        while diff > std::f64::consts::PI  { diff -= 2.0 * std::f64::consts::PI; }
        while diff < -std::f64::consts::PI { diff += 2.0 * std::f64::consts::PI; }
        diff.abs() > 0.02 // ~1 degree tolerance
    }
}

/// A single image layer placed at a specific depth in one frame.
#[derive(Debug, Clone)]
pub struct FrameImageEntry {
    pub depth: u16,
    pub shape_id: u16,
    pub symbol_name: String,
    /// Local placement matrix within the sub-sprite (before root MC transform)
    pub local_matrix: ImageLocalMatrix,
    /// World-space position after applying root MC transform (pixels, SSF2 y-down).
    /// Use these for Fraymakers IMAGE symbol x/y/scaleX/scaleY (after y-flip).
    pub world_tx: f64,
    pub world_ty: f64,
    pub world_sx: f64,
    pub world_sy: f64,
    /// World-space rotation in degrees (local rotation + root MC rotation, if any).
    pub world_rotation: f64,
    /// World-space affine matrix components (local matrix * root transform).
    /// Used to compute pivot-corrected positions.
    pub world_a: f64,
    pub world_b: f64,
    pub world_c: f64,
    pub world_d: f64,
    /// World-space position of the animation's LOCAL ORIGIN (0,0).
    /// In SSF2/Flash, the sub-sprite rotates around its local origin.
    /// This is simply root_xf.apply(0,0) = (root_xf.tx, root_xf.ty).
    /// This is the TRUE rotation center in world space.
    pub anim_origin_x: f64,
    pub anim_origin_y: f64,
}

/// Per-animation per-frame image references.
/// Each frame may have multiple entries (one per depth/layer).
/// Entries within a frame are ordered by depth (ascending = back-to-front).
#[derive(Debug, Clone)]
pub struct AnimFrameImages {
    /// frame_index → ordered list of (depth, shape_id, symbol_name)
    pub frames: BTreeMap<u16, Vec<FrameImageEntry>>,
    pub total_frames: u16,
    /// Number of distinct depth slots used across all frames (= number of IMAGE layers to create)
    pub max_depth_slots: usize,
}

/// Result of image extraction
pub struct ImageExtractionResult {
    /// bitmap_id → ExtractedImage (the raw PNG files)
    pub images: BTreeMap<u16, ExtractedImage>,
    /// shape_id → bitmap_id (for resolving PlaceObject refs)
    pub shape_to_bitmap: ShapeToBitmapMap,
    /// shape_id → (pivot_x, pivot_y) in bitmap pixel space.
    /// This is where the shape's local (0,0) origin lands in the bitmap,
    /// computed as (-fill_tx / (fill_a/20), -fill_ty / (fill_d/20)).
    /// Only populated for shapes with a non-trivial fill offset.
    pub shape_pivot: BTreeMap<u16, (f64, f64)>,
    /// shape_id → (fill_scale_x, fill_scale_y), the bitmap-fill matrix scale
    /// (signed; negative = flip). SSF2 references a high-res bitmap through a
    /// DefineShape whose fill matrix scales it (often WAY down, e.g. a shared
    /// magic-particle bitmap displayed at 0.08×). The PNG is extracted at native
    /// resolution, so this scale MUST multiply into the IMAGE symbol's scale or
    /// the sprite renders 1/fill_scale too big. Absent → 1.0 (vector shapes, or
    /// bitmaps placed 1:1). See entity_gen IMAGE placement.
    pub shape_fill_scale: BTreeMap<u16, (f64, f64)>,
    /// fm_anim_name → per-frame image references
    pub anim_images: BTreeMap<String, AnimFrameImages>,
}

/// Extract all bitmap images from the SWF, build mappings, and save as PNGs.
pub fn extract_images(
    swf_data: &[u8],
    output_dir: &Path,
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
) -> Result<ImageExtractionResult> {
    let swf_buf = swf::decompress_swf(swf_data)
        .context("Failed to decompress SWF")?;
    let swf = swf::parse_swf(&swf_buf)
        .context("Failed to parse SWF tags")?;
    let xform_map = crate::sprite_parser::extract_xframe_transforms_from_swf(
        &swf, char_name, ssf2_to_fm,
    ).unwrap_or_default();
    extract_images_from_swf(&swf, output_dir, char_name, ssf2_to_fm, &xform_map)
}

/// `extract_images` against an already-parsed SWF and a precomputed
/// xform_map. Used by main.rs::process_character so the SWF and the
/// xform_map are computed exactly once per character.
pub fn extract_images_from_swf(
    swf: &swf::Swf<'_>,
    output_dir: &Path,
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
    xform_map: &BTreeMap<String, crate::sprite_parser::XframeTransform>,
) -> Result<ImageExtractionResult> {
    // Sprite PNGs are queued and encoded across all cores at the end (see PngBatch) rather
    // than one at a time inline, which is where a third of a conversion's CPU used to go.
    let mut pngs = PngBatch::default();

    // Build symbol table: char_id → class_name
    let mut symbols: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = String::from_utf8_lossy(link.class_name.as_bytes()).to_string();
                symbols.insert(link.id, name);
            }
        }
    }

    // 1. Build shape_id → bitmap_id map from DefineShape tags.
    //    Also compute shape_pivot: where shape (0,0) lands in bitmap pixel space.
    //    pivot_x = -fill_tx / (fill_a / 20),  pivot_y = -fill_ty / (fill_d / 20)
    let mut shape_to_bitmap: ShapeToBitmapMap = BTreeMap::new();
    let mut shape_pivot: BTreeMap<u16, (f64, f64)> = BTreeMap::new();
    let mut shape_fill_scale: BTreeMap<u16, (f64, f64)> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::DefineShape(shape) = tag {
            // Look for bitmap fill in fill styles.
            // Skip id=65535 (SWF null/clipping bitmap) — take the first real bitmap.
            for fill in &shape.styles.fill_styles {
                if let swf::FillStyle::Bitmap { id, matrix, .. } = fill {
                    if *id != 65535 {
                        shape_to_bitmap.insert(shape.id, *id);
                        // fill matrix: a/d are in twips-per-20-pixels, tx/ty in twips/20.
                        // scale_x = a/20 (pixels per shape-unit); pivot_x = -tx / scale_x
                        let fa = matrix.a.to_f64();
                        let fd = matrix.d.to_f64();
                        let ftx = matrix.tx.get() as f64 / 20.0;
                        let fty = matrix.ty.get() as f64 / 20.0;
                        let scale_x = fa / 20.0;
                        let scale_y = fd / 20.0;
                        if scale_x.abs() > 0.001 && scale_y.abs() > 0.001 {
                            // Store the fill scale so the IMAGE placement can fold it in
                            // (the PNG is at native res; this is how much SSF2 shrinks it).
                            shape_fill_scale.insert(shape.id, (scale_x, scale_y));
                            // fill matrix: shape_pt = fill_matrix * bitmap_pt
                            // So: bitmap_pt = inv(fill_matrix) * shape_pt
                            // For shape (0,0): bitmap_pt = (-tx/a, -ty/d) in pixel space
                            // But we want the OPPOSITE: offset FROM bitmap origin TO shape origin
                            // bitmap origin (0,0) → shape coords via fill matrix: (tx, ty)
                            // In pixels: (tx/(a/20), ty/(d/20)) = (tx*20/a, ty*20/d)
                            let px = ftx / scale_x;  // no negation
                            let py = fty / scale_y;
                            if px.abs() > 0.5 || py.abs() > 0.5 {
                                shape_pivot.insert(shape.id, (px, py));
                            }
                        }
                        break;
                    }
                }
            }
        }
    }
    log::info!("Shape→bitmap mappings: {} ({} with non-zero pivot)", shape_to_bitmap.len(), shape_pivot.len());

    // 2. Extract all bitmaps to PNGs
    let sprites_dir = output_dir.join("library/sprites");
    fs::create_dir_all(&sprites_dir)?;

    let mut images: BTreeMap<u16, ExtractedImage> = BTreeMap::new();

    for tag in &swf.tags {
        match tag {
            swf::Tag::DefineBitsLossless(bmp) => {
                let id = bmp.id;
                let w = bmp.width as u32;
                let h = bmp.height as u32;

                // Find the symbol name: prefer the shape's symbol that references this bitmap
                let sym = find_symbol_for_bitmap(id, &symbols, &shape_to_bitmap)
                    .unwrap_or_else(|| symbols.get(&id).cloned()
                        .unwrap_or_else(|| format!("bitmap_{}", id)));

                match decode_lossless(bmp) {
                    Ok(rgba_data) => {
                        let filename = format!("{}.png", sanitize_name(&sym));
                        let png_path = sprites_dir.join(&filename);
                        pngs.push(png_path.clone(), w, h, rgba_data)?;

                        images.insert(id, ExtractedImage {
                            bitmap_id: id,
                            symbol_name: sym,
                            width: w,
                            height: h,
                            png_path: format!("library/sprites/{}", filename),
                            is_vector: false,
                        });
                    }
                    Err(e) => {
                        log::debug!("Failed to decode lossless bitmap {}: {}", id, e);
                    }
                }
            }
            swf::Tag::DefineBitsJpeg3(jpeg) => {
                let id = jpeg.id;
                let sym = find_symbol_for_bitmap(id, &symbols, &shape_to_bitmap)
                    .unwrap_or_else(|| symbols.get(&id).cloned()
                        .unwrap_or_else(|| format!("jpeg_{}", id)));

                match decode_jpeg3(jpeg) {
                    Ok((w, h, rgba_data)) => {
                        let filename = format!("{}.png", sanitize_name(&sym));
                        let png_path = sprites_dir.join(&filename);
                        pngs.push(png_path.clone(), w, h, rgba_data)?;

                        images.insert(id, ExtractedImage {
                            bitmap_id: id,
                            symbol_name: sym,
                            width: w,
                            height: h,
                            png_path: format!("library/sprites/{}", filename),
                            is_vector: false,
                        });
                    }
                    Err(e) => {
                        log::debug!("Failed to decode JPEG3 bitmap {}: {}", id, e);
                    }
                }
            }
            _ => {}
        }
    }

    // 2b. Rasterize VECTOR shapes (DefineShape with no bitmap fill, and
    //     DefineMorphShape) to PNGs so vector-drawn SSF2 VFX — dust swirls,
    //     sparks, trails, morphing waves — actually render in Fraymakers
    //     (which only displays bitmaps). They masquerade as bitmaps: keyed by
    //     the shape's own character id in `images` + `shape_to_bitmap`, with
    //     `shape_pivot` set so the existing placement math positions them.
    //
    //     Morph shapes: SSF2 places each morph at a *fixed* ratio (verified
    //     across chars — morphs are not ratio-tweened per frame; their
    //     animation comes from placing different morph shapes on different
    //     frames). So we rasterize each morph once at the ratio it is actually
    //     placed with (default 0 = start shape), which is exact for this data.
    let mut morph_ratio: BTreeMap<u16, u16> = BTreeMap::new();
    {
        let morph_ids: BTreeSet<u16> = swf.tags.iter().filter_map(|t| match t {
            swf::Tag::DefineMorphShape(m) => Some(m.id), _ => None,
        }).collect();
        if !morph_ids.is_empty() {
            for tag in &swf.tags {
                if let swf::Tag::DefineSprite(s) = tag {
                    for st in &s.tags {
                        if let swf::Tag::PlaceObject(po) = st {
                            if let swf::PlaceObjectAction::Place(cid) | swf::PlaceObjectAction::Replace(cid) = &po.action {
                                if morph_ids.contains(cid) {
                                    // First non-zero ratio wins; else 0.
                                    let r = po.ratio.unwrap_or(0);
                                    let e = morph_ratio.entry(*cid).or_insert(0);
                                    if *e == 0 { *e = r; }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut vec_count = 0usize;
    for tag in &swf.tags {
        match tag {
            swf::Tag::DefineShape(shape) if !shape_to_bitmap.contains_key(&shape.id) => {
                if let Some(r) = crate::vector_raster::rasterize_shape(
                    &shape.shape_bounds, &shape.styles.fill_styles, &shape.styles.line_styles, &shape.shape,
                ) {
                    let sym = format!("{}_vec_{}", char_name, shape.id);
                    let filename = format!("{}.png", sanitize_name(&sym));
                    pngs.push(sprites_dir.join(&filename), r.width, r.height, r.rgba.clone())?;
                    images.insert(shape.id, ExtractedImage {
                        bitmap_id: shape.id, symbol_name: sym, width: r.width, height: r.height,
                        png_path: format!("library/sprites/{}", filename),
                        is_vector: true,
                    });
                    shape_to_bitmap.insert(shape.id, shape.id);
                    shape_pivot.insert(shape.id, r.origin_px);
                    vec_count += 1;
                }
            }
            swf::Tag::DefineMorphShape(m) if !shape_to_bitmap.contains_key(&m.id) => {
                let ratio = *morph_ratio.get(&m.id).unwrap_or(&0) as f64 / 65535.0;
                if let Some(r) = crate::vector_raster::rasterize_morph(m, ratio) {
                    let sym = format!("{}_vec_{}", char_name, m.id);
                    let filename = format!("{}.png", sanitize_name(&sym));
                    pngs.push(sprites_dir.join(&filename), r.width, r.height, r.rgba.clone())?;
                    images.insert(m.id, ExtractedImage {
                        bitmap_id: m.id, symbol_name: sym, width: r.width, height: r.height,
                        png_path: format!("library/sprites/{}", filename),
                        is_vector: true,
                    });
                    shape_to_bitmap.insert(m.id, m.id);
                    shape_pivot.insert(m.id, r.origin_px);
                    vec_count += 1;
                }
            }
            _ => {}
        }
    }
    log::info!("Rasterized {} vector shapes/morphs to PNGs", vec_count);

    log::info!("Extracted {} images to {}", images.len(), sprites_dir.display());

    // 3. xform_map is supplied by the caller so it's computed exactly
    //    once per character (was previously re-extracted here AND inside
    //    parse_sprite_boxes — three SWF parses across the two call sites).
    //
    // 4. Build per-animation per-frame image references from DefineSprite tags
    let mut anim_images = build_anim_frame_images(swf, char_name, ssf2_to_fm, &symbols, &shape_to_bitmap, xform_map, &images);
    // Apply same fallbacks as sprite_parser for animations with no image data
    apply_image_fallbacks(&mut anim_images);
    log::info!("Animation image mappings: {} animations (after fallbacks)", anim_images.len());

    // Every queued sprite must be on disk BEFORE the shear pre-render: that pass re-opens
    // the sprites it bakes from, so anything still sitting in the batch would be invisible
    // to it and its baked output would silently go missing.
    pngs.flush()?;

    // 5. Pre-render frames whose world matrix contains shear — FrayTools
    //    keyframes can't express shear, so the shear is baked into a new
    //    bitmap and the frame becomes a plain translation.
    let skew_count = prerender_skewed_frames(
        &mut anim_images, &mut images, &shape_to_bitmap, &sprites_dir, char_name,
    );
    if skew_count > 0 {
        log::info!("Pre-rendered {} sheared frame placement(s) as baked bitmaps", skew_count);
    }

    Ok(ImageExtractionResult {
        images,
        shape_to_bitmap,
        shape_pivot,
        shape_fill_scale,
        anim_images,
    })
}

/// Pre-render frames whose WORLD placement matrix contains shear.
///
/// FrayTools' IMAGE keyframe expresses translation, rotation and
/// scaleX/scaleY but NOT shear. When a SWF places a sprite with a sheared
/// matrix, decomposing it to scale+rotation is lossy — the sprite appears to
/// shrink/balloon instead of stretching. For those frames we bake the full
/// linear part of the world matrix into a fresh bitmap and rewrite the entry
/// as a plain translation, which FrayTools can reproduce exactly.
///
/// Non-sheared frames (pure rotation + scale, with or without flip) are left
/// untouched — only genuine shear is baked.
fn prerender_skewed_frames(
    anim_images: &mut BTreeMap<String, AnimFrameImages>,
    images: &mut BTreeMap<u16, ExtractedImage>,
    shape_to_bitmap: &ShapeToBitmapMap,
    sprites_dir: &std::path::Path,
    char_name: &str,
) -> usize {
    use image::RgbaImage;

    // Collect (anim, frame, entry_idx) for entries whose world matrix shears.
    let mut work: Vec<(String, u16, usize)> = Vec::new();
    for (anim_name, anim_data) in anim_images.iter() {
        for (frame, entries) in &anim_data.frames {
            for (idx, e) in entries.iter().enumerate() {
                // Only bake direct-bitmap placements. When a DefineShape wraps a
                // bitmap fill (e.g. sandbag's trail sprites), the entry's world
                // matrix is in shape-coordinate space, not bitmap-pixel space —
                // baking the bitmap by that matrix would mis-place it. Those
                // stay on the faithful scale+rotation path.
                if shape_to_bitmap.contains_key(&e.shape_id) {
                    continue;
                }
                let wm = ImageLocalMatrix::from_abcd(
                    e.world_a, e.world_b, e.world_c, e.world_d, e.world_tx, e.world_ty);
                if wm.has_skew() {
                    work.push((anim_name.clone(), *frame, idx));
                }
            }
        }
    }
    if work.is_empty() { return 0; }

    // Synthetic bitmap ids for the baked PNGs — start above every real id.
    // u16 namespace, so we cap at u16::MAX. Past ~5,500 cache misses we
    // bail loudly instead of silently wrapping back into the real-id range
    // and colliding with existing shapes.
    let mut next_id: u16 = images.keys().max().copied().unwrap_or(0).max(59_999) + 1;
    // exact world linear part + source id → (id, sym, w, h, min_x, min_y)
    let mut cache: BTreeMap<String, (u16, String, u32, u32, f64, f64)> = BTreeMap::new();
    let mut count = 0usize;

    for (anim_name, frame, entry_idx) in &work {
        // Snapshot the fields we need (immutable borrow ends here).
        let (wa, wb, wc, wd, wtx, wty, shape_id, symbol_name) = {
            let e = &anim_images[anim_name].frames[frame][*entry_idx];
            (e.world_a, e.world_b, e.world_c, e.world_d, e.world_tx, e.world_ty,
             e.shape_id, e.symbol_name.clone())
        };

        // Resolve the source bitmap: shape_id → bitmap, else by symbol name.
        let bitmap_id = shape_to_bitmap.get(&shape_id).copied().unwrap_or(shape_id);
        let src_img = match images.get(&bitmap_id)
            .or_else(|| images.values().find(|img| img.symbol_name == symbol_name))
        {
            Some(img) => img.clone(),
            None => {
                log::debug!("prerender_skewed: no source image for sym='{}' shape={}", symbol_name, shape_id);
                continue;
            }
        };

        // Bake once per distinct (linear part, source bitmap). The key uses the
        // exact f64 bit patterns of the linear matrix: only frames whose matrix
        // is bit-for-bit identical share a baked bitmap, so two animations with
        // merely near-identical matrices can never collide and reuse each
        // other's bake. Truly identical placements still dedup, so this does
        // not bloat the output.
        let cache_key = format!("{:016x}_{:016x}_{:016x}_{:016x}_{}",
            wa.to_bits(), wb.to_bits(), wc.to_bits(), wd.to_bits(), src_img.bitmap_id);
        let (new_id, new_sym, _nw, _nh, min_x, min_y) = if let Some(c) = cache.get(&cache_key) {
            c.clone()
        } else {
            // png_path is "library/sprites/X.png" — resolve from the char output dir.
            let char_output_dir = sprites_dir.parent().and_then(|p| p.parent()).unwrap_or(sprites_dir);
            let src = match image::open(char_output_dir.join(&src_img.png_path)) {
                Ok(img) => img.to_rgba8(),
                Err(e) => {
                    log::debug!("prerender_skewed: failed to open '{}': {}", src_img.png_path, e);
                    continue;
                }
            };
            let (sw, sh) = (src.width() as f64, src.height() as f64);

            // Bounding box of the source rectangle under the linear map A_w.
            let corners = [(0.0, 0.0), (sw, 0.0), (0.0, sh), (sw, sh)];
            let tx: Vec<(f64, f64)> = corners.iter()
                .map(|(x, y)| (wa * x + wc * y, wb * x + wd * y))
                .collect();
            let min_x = tx.iter().map(|p| p.0).fold(f64::MAX, f64::min);
            let min_y = tx.iter().map(|p| p.1).fold(f64::MAX, f64::min);
            let max_x = tx.iter().map(|p| p.0).fold(f64::MIN, f64::max);
            let max_y = tx.iter().map(|p| p.1).fold(f64::MIN, f64::max);
            let dst_w = (max_x - min_x).ceil().max(1.0) as u32;
            let dst_h = (max_y - min_y).ceil().max(1.0) as u32;
            if dst_w > 4096 || dst_h > 4096 { continue; }

            // Inverse of [[wa, wc], [wb, wd]] for backward sampling.
            // Near-singular world matrix → no inverse, can't bake. Warn so
            // the user knows a sheared frame fell back to the (visually
            // wrong) scale+rotation path instead of being baked.
            let det = wa * wd - wb * wc;
            if det.abs() < 1e-9 {
                log::warn!(
                    "prerender_skewed_frames: skipping anim='{}' frame={} \
                     entry={} — world matrix is near-singular (det={:.3e}); \
                     the placement will use the scale+rotation fallback",
                    anim_name, frame, entry_idx, det
                );
                continue;
            }
            let (inv_a, inv_c) = ( wd / det, -wc / det);
            let (inv_b, inv_d) = (-wb / det,  wa / det);

            let mut dst = RgbaImage::new(dst_w, dst_h);
            let swi = src.width() as i64;
            let shi = src.height() as i64;

            // Sample the source with premultiplied alpha; out-of-bounds reads
            // return transparent black so sheared edges fade out cleanly.
            let sample = |xi: i64, yi: i64| -> [f64; 4] {
                if xi >= 0 && xi < swi && yi >= 0 && yi < shi {
                    let p = src.get_pixel(xi as u32, yi as u32).0;
                    let a = p[3] as f64 / 255.0;
                    [p[0] as f64 * a, p[1] as f64 * a, p[2] as f64 * a, p[3] as f64]
                } else {
                    [0.0; 4]
                }
            };
            // Catmull-Rom cubic weights for a fractional offset in [0,1).
            let cubic = |t: f64| -> [f64; 4] {
                let t2 = t * t;
                let t3 = t2 * t;
                [
                    -0.5 * t3 + t2 - 0.5 * t,
                     1.5 * t3 - 2.5 * t2 + 1.0,
                    -1.5 * t3 + 2.0 * t2 + 0.5 * t,
                     0.5 * t3 - 0.5 * t2,
                ]
            };
            // Bicubic (Catmull-Rom) sample of the source at a continuous point.
            // This interpolating kernel stays much sharper than bilinear and
            // its mild negative lobes preserve contrast, so the sprites' thin
            // dark outlines keep their definition instead of being averaged
            // toward the lighter interior.
            let bicubic = |sx: f64, sy: f64| -> [f64; 4] {
                let xf = sx.floor();
                let yf = sy.floor();
                let wx = cubic(sx - xf);
                let wy = cubic(sy - yf);
                let (x0, y0) = (xf as i64 - 1, yf as i64 - 1);
                let mut acc = [0.0f64; 4];
                // Anti-ringing: track the per-channel min/max of the 16 source taps and clamp
                // the result into that range. Catmull-Rom's negative lobes overshoot at
                // light/dark edges, producing pixels LIGHTER than any nearby source pixel; once
                // palette-snapped those become the lightest costume colour → scattered light
                // "noise" speckles all over a recoloured sprite (seen on jab). Clamping kills it.
                let mut mn = [f64::MAX; 4];
                let mut mx = [f64::MIN; 4];
                for (j, &wyj) in wy.iter().enumerate() {
                    for (i, &wxi) in wx.iter().enumerate() {
                        let w = wxi * wyj;
                        let c = sample(x0 + i as i64, y0 + j as i64);
                        for k in 0..4 { acc[k] += c[k] * w; mn[k] = mn[k].min(c[k]); mx[k] = mx[k].max(c[k]); }
                    }
                }
                for k in 0..4 { acc[k] = acc[k].clamp(mn[k], mx[k]); }
                acc
            };
            // Bicubic-resample every destination pixel into a premultiplied
            // float buffer. One interpolating tap per pixel — no supersampling,
            // which would blur thin dark outlines back toward their lighter
            // neighbours. Pixel (i,j) is centred on integer coord (i,j),
            // matching the previous sampler's convention (no positional shift).
            let npx = (dst_w as usize) * (dst_h as usize);
            let mut buf: Vec<[f64; 4]> = Vec::with_capacity(npx);
            for dy in 0..dst_h {
                for dx in 0..dst_w {
                    let fx = dx as f64 + min_x;
                    let fy = dy as f64 + min_y;
                    let sx = inv_a * fx + inv_c * fy;
                    let sy = inv_b * fx + inv_d * fy;
                    // Bicubic + unsharp is the base bake — it gives the clearest, smoothest
                    // sheared image. Recolorability is handled downstream (anti-ringing clamp
                    // above + the palette snap/despeckle in palette_gen), not by degrading the
                    // sampler.
                    buf.push(bicubic(sx, sy));
                }
            }

            // Unsharp mask: lifts edge contrast for a crisper base image. Its overshoot is
            // bounded by the anti-ringing clamp on the bicubic taps, and the recolor speckle it
            // would otherwise cause is cleaned by the palette despeckle in palette_gen.
            const UNSHARP_AMOUNT: f64 = 0.45;
            let gauss: Vec<f64> = {
                let sigma = 0.9_f64;
                let radius = (sigma * 3.0).ceil() as i64;
                let mut k: Vec<f64> = (-radius..=radius)
                    .map(|i| (-((i * i) as f64) / (2.0 * sigma * sigma)).exp())
                    .collect();
                let sum: f64 = k.iter().sum();
                for w in &mut k { *w /= sum; }
                k
            };
            let gradius = (gauss.len() / 2) as i64;
            let w = dst_w as i64;
            let h = dst_h as i64;
            let idx = |x: i64, y: i64| (y * w + x) as usize;
            // Separable Gaussian, zero-padded (premultiplied → transparent
            // outside): horizontal pass into `tmp`, then vertical into `blurred`.
            let mut tmp = vec![[0.0f64; 4]; npx];
            for y in 0..h {
                for x in 0..w {
                    let mut acc = [0.0f64; 4];
                    for (t, &gw) in gauss.iter().enumerate() {
                        let sx = x + t as i64 - gradius;
                        if sx < 0 || sx >= w { continue; }
                        let c = buf[idx(sx, y)];
                        for k in 0..4 { acc[k] += c[k] * gw; }
                    }
                    tmp[idx(x, y)] = acc;
                }
            }
            let mut blurred = vec![[0.0f64; 4]; npx];
            for y in 0..h {
                for x in 0..w {
                    let mut acc = [0.0f64; 4];
                    for (t, &gw) in gauss.iter().enumerate() {
                        let sy = y + t as i64 - gradius;
                        if sy < 0 || sy >= h { continue; }
                        let c = tmp[idx(x, sy)];
                        for k in 0..4 { acc[k] += c[k] * gw; }
                    }
                    blurred[idx(x, y)] = acc;
                }
            }

            // Finalize: apply the unsharp mask, then un-premultiply. Clamp
            // alpha to [0,255] and premultiplied colour to [0,alpha] so the
            // sharpen overshoot can't manufacture a bright or dark fringe.
            for y in 0..h {
                for x in 0..w {
                    let p = idx(x, y);
                    let mut out = [0.0f64; 4];
                    for k in 0..4 {
                        out[k] = buf[p][k] + UNSHARP_AMOUNT * (buf[p][k] - blurred[p][k]);
                    }
                    let alpha = out[3].clamp(0.0, 255.0);
                    if alpha > 0.5 {
                        let inv = 255.0 / alpha; // un-premultiply
                        dst.put_pixel(x as u32, y as u32, image::Rgba([
                            (out[0].clamp(0.0, alpha) * inv).round().clamp(0.0, 255.0) as u8,
                            (out[1].clamp(0.0, alpha) * inv).round().clamp(0.0, 255.0) as u8,
                            (out[2].clamp(0.0, alpha) * inv).round().clamp(0.0, 255.0) as u8,
                            alpha.round() as u8,
                        ]));
                    }
                }
            }

            // NOTE: the baked skew bitmap's bicubic/unsharp output is palette-snapped LATER,
            // in palette_gen::snap_skew_bitmaps_to_palette, which snaps every skew_*.png pixel
            // to the COSTUME color array (the swappable palette) — not the source bitmap's
            // sampled colours. Doing it there (a) targets the actual swap palette so no
            // non-costume colour survives to flicker, and (b) runs once after the costume
            // palette is finalized. So no per-bake snap here.

            let new_id = next_id;
            // Bail out cleanly if we'd overflow the u16 id namespace.
            // Wrapping back into 0 would collide with real shape ids.
            match next_id.checked_add(1) {
                Some(n) => next_id = n,
                None => {
                    log::warn!(
                        "prerender_skewed_frames: exhausted u16 synthetic-id \
                         namespace at id={}; subsequent sheared placements \
                         will fall back to the faithful scale+rotation path",
                        new_id
                    );
                    // Don't continue baking; emit nothing for this and any
                    // later entries. The faithful path is still correct,
                    // just visually wrong for shears.
                    break;
                }
            }
            let new_sym = format!("skew_{}_{}", char_name, new_id);
            let filename = format!("{}.png", sanitize_name(&new_sym));
            if let Err(e) = dst.save(sprites_dir.join(&filename)) {
                log::warn!("Failed to save pre-rendered skew bitmap: {}", e);
                continue;
            }
            images.insert(new_id, ExtractedImage {
                bitmap_id: new_id,
                symbol_name: new_sym.clone(),
                width: dst_w,
                height: dst_h,
                png_path: format!("library/sprites/{}", filename),
                is_vector: src_img.is_vector,
            });
            let v = (new_id, new_sym, dst_w, dst_h, min_x, min_y);
            cache.insert(cache_key, v.clone());
            v
        };

        // Rewrite the entry as a plain translation of the baked bitmap.
        // The baked bitmap's pixel (0,0) holds world content A_w·L at
        // (min_x, min_y); placed with an identity transform at
        // (world_tx + min_x, world_ty + min_y) it lands exactly where the
        // sheared placement intended.
        let e = anim_images.get_mut(anim_name).unwrap()
            .frames.get_mut(frame).unwrap().get_mut(*entry_idx).unwrap();
        e.shape_id = new_id;
        e.symbol_name = new_sym;
        e.world_tx = wtx + min_x;
        e.world_ty = wty + min_y;
        e.world_a = 1.0; e.world_b = 0.0; e.world_c = 0.0; e.world_d = 1.0;
        e.world_sx = 1.0; e.world_sy = 1.0; e.world_rotation = 0.0;
        e.local_matrix = ImageLocalMatrix::from_abcd(1.0, 0.0, 0.0, 1.0, e.world_tx, e.world_ty);
        count += 1;
    }

    count
}

/// Find the symbol name for a bitmap by looking up which shape references it
fn find_symbol_for_bitmap(
    bitmap_id: u16,
    symbols: &BTreeMap<u16, String>,
    shape_to_bitmap: &ShapeToBitmapMap,
) -> Option<String> {
    // Find a shape that references this bitmap and has a symbol name
    for (shape_id, bmp_id) in shape_to_bitmap {
        if *bmp_id == bitmap_id {
            if let Some(sym) = symbols.get(shape_id) {
                return Some(sym.clone());
            }
        }
    }
    None
}

/// Build per-animation per-frame image references by walking DefineSprite display lists.
/// For each animation sprite, track which shape (image) is placed on each frame.
fn build_anim_frame_images(
    swf: &swf::Swf,
    char_name: &str,
    ssf2_to_fm: &BTreeMap<String, String>,
    symbols: &BTreeMap<u16, String>,
    shape_to_bitmap: &ShapeToBitmapMap,
    xform_map: &BTreeMap<String, crate::sprite_parser::XframeTransform>,
    images: &BTreeMap<u16, ExtractedImage>,
) -> BTreeMap<String, AnimFrameImages> {
    let char_lower = char_name.to_lowercase();
    // Animation identity comes from the TIMELINE (xframe FrameLabel + `stance`
    // instance), never from the sprite's symbol name. THE SAME shared map the box
    // extractor uses — images, boxes and transforms name the SAME animation per sprite.
    let anim_by_sprite = crate::sprite_parser::build_anim_sprite_map(swf, symbols, &char_lower, ssf2_to_fm);
    let mut result = BTreeMap::new();

    // Pre-build a lookup of all DefineSprite tags by id (including unnamed ones)
    let all_sprites: BTreeMap<u16, &swf::Sprite> = swf.tags.iter().filter_map(|t| {
        if let swf::Tag::DefineSprite(s) = t { Some((s.id, s)) } else { None }
    }).collect();

    // Pre-build effect sprite frame tables: sprite_id → Vec<frame> → Vec<(shape_id, sym, mat)>
    // Effect sprites are _fla. named sprites that are NOT top-level animation containers.
    // Recursively expands unnamed inner sprites.
    // `effect_sprites` holds the named "_fla." effect-sprite frame tables;
    // `unnamed_frames` holds every unnamed sub-sprite's frame table. Both are
    // needed downstream: a named effect placed in an animation expands via
    // `effect_sprites`, but an *unnamed* sub-sprite placed directly in an
    // animation timeline (e.g. sandbag's item_homerun body movieclips) must
    // expand via `unnamed_frames`.
    let (effect_sprites, unnamed_frames): (SpriteFrameTable, SpriteFrameTable) = {
        // Two-pass approach:
        // Pass 1: build inner unnamed sprite frame tables
        // Pass 2: for named effect sprites, inline the unnamed sub-sprites

        // First build all unnamed sprite frame tables
        let mut unnamed_frames: SpriteFrameTable = BTreeMap::new();
        for (&sid, sprite) in &all_sprites {
            if symbols.contains_key(&sid) { continue; } // skip named ones
            let mut disp: BTreeMap<u16, (u16, String, ImageLocalMatrix)> = BTreeMap::new();
            let mut frames: Vec<Vec<(u16, String, ImageLocalMatrix)>> = Vec::new();
            for stag in &sprite.tags {
                match stag {
                    swf::Tag::ShowFrame => {
                        frames.push(disp.values().map(|(id, sym, mat)| (*id, sym.clone(), *mat)).collect());
                    }
                    swf::Tag::PlaceObject(po) => {
                        let local_mat = po.matrix.map(|m| {
                            let a = m.a.to_f64(); let b = m.b.to_f64();
                            let c = m.c.to_f64(); let d = m.d.to_f64();
                            ImageLocalMatrix::from_abcd(a, b, c, d,
                                m.tx.get() as f64 / 20.0, m.ty.get() as f64 / 20.0)
                        }).unwrap_or_default();
                        match &po.action {
                            swf::PlaceObjectAction::Place(cid) | swf::PlaceObjectAction::Replace(cid) => {
                                if let Some(sname) = symbols.get(cid) {
                                    let lower = sname.to_lowercase();
                                    if !lower.contains("collisonbox") && !lower.contains("collisionbox") {
                                        disp.insert(po.depth, (*cid, sname.clone(), local_mat));
                                    }
                                } else if let Some(&bitmap_id) = shape_to_bitmap.get(cid) {
                                    if let Some(img) = images.get(&bitmap_id) {
                                        disp.insert(po.depth, (*cid, img.symbol_name.clone(), local_mat));
                                    }
                                }
                            }
                            swf::PlaceObjectAction::Modify => {
                                // A MODIFY with no matrix only re-states the object —
                                // keep its existing transform instead of resetting it.
                                if po.matrix.is_some() {
                                    if let Some(e) = disp.get_mut(&po.depth) { e.2 = local_mat; }
                                }
                            }
                        }
                    }
                    swf::Tag::RemoveObject(ro) => { disp.remove(&ro.depth); }
                    _ => {}
                }
            }
            if !frames.is_empty() { unnamed_frames.insert(sid, frames); }
        }

        // Now build named effect sprite tables, inlining unnamed sub-sprites
        let mut map = BTreeMap::new();
        for tag in &swf.tags {
            if let swf::Tag::DefineSprite(sprite) = tag {
                let sym = match symbols.get(&sprite.id) { Some(s) => s.as_str(), None => continue };
                if !sym.contains("_fla.") { continue; }
                // Skip character ANIMATION sprites (those placed on the root timeline);
                // this block handles the remaining non-anim effect/trail sprites.
                if anim_by_sprite.contains_key(&sprite.id) { continue; }

                let mut disp: BTreeMap<u16, (u16, String, ImageLocalMatrix)> = BTreeMap::new();
                // depth → (unnamed_sprite_id, place_parent_mat, frame_started, graphic_frame)
                // graphic_frame = the placement's ratio (a Graphic-symbol placement: it shows the
                // ONE frame the ratio selects — Flash's "Single Frame" — and never self-animates).
                let mut unnamed_placements: BTreeMap<u16, (u16, ImageLocalMatrix, u16, Option<u16>)> = BTreeMap::new();
                let mut cur_frame: u16 = 0;
                let mut effect_frames: Vec<Vec<(u16, String, ImageLocalMatrix)>> = Vec::new();

                for stag in &sprite.tags {
                    match stag {
                        swf::Tag::ShowFrame => {
                            let mut snap: Vec<(u16, String, ImageLocalMatrix)> = disp.values()
                                .map(|(id, sym, mat)| (*id, sym.clone(), *mat)).collect();
                            // Expand unnamed sub-sprites: pick their frame at (cur_frame - place_frame)
                            for (&_depth, (unnamed_id, parent_mat, place_frame, pinned)) in &unnamed_placements {
                                if let Some(uframes) = unnamed_frames.get(unnamed_id) {
                                    // a graphic-pinned placement shows the ratio-selected frame, always.
                                    let uf_idx = match pinned {
                                        Some(r) => (*r as usize).min(uframes.len().saturating_sub(1)),
                                        None => (cur_frame.saturating_sub(*place_frame) as usize)
                                            .min(uframes.len().saturating_sub(1)),
                                    };
                                    for (inner_id, inner_sym, inner_mat) in &uframes[uf_idx] {
                                        let composed = parent_mat.compose(inner_mat);
                                        snap.push((*inner_id, inner_sym.clone(), composed));
                                    }
                                }
                            }
                            effect_frames.push(snap);
                            cur_frame += 1;
                        }
                        swf::Tag::PlaceObject(po) => {
                            let local_mat = po.matrix.map(|m| {
                                let a = m.a.to_f64(); let b = m.b.to_f64();
                                let c = m.c.to_f64(); let d = m.d.to_f64();
                                ImageLocalMatrix::from_abcd(a, b, c, d,
                                    m.tx.get() as f64 / 20.0, m.ty.get() as f64 / 20.0)
                            }).unwrap_or_default();
                            match &po.action {
                                swf::PlaceObjectAction::Place(cid) | swf::PlaceObjectAction::Replace(cid) => {
                                    if let Some(sname) = symbols.get(cid) {
                                        let lower = sname.to_lowercase();
                                        if !lower.contains("collisonbox") && !lower.contains("collisionbox") {
                                            disp.insert(po.depth, (*cid, sname.clone(), local_mat));
                                        }
                                    } else if let Some(&bitmap_id) = shape_to_bitmap.get(cid) {
                                        if let Some(img) = images.get(&bitmap_id) {
                                            disp.insert(po.depth, (*cid, img.symbol_name.clone(), local_mat));
                                        }
                                    } else if unnamed_frames.contains_key(cid) {
                                        unnamed_placements.insert(po.depth, (*cid, local_mat, cur_frame, po.ratio));
                                    }
                                }
                                swf::PlaceObjectAction::Modify => {
                                    // A MODIFY with no matrix only re-states the object —
                                    // keep its existing transform.
                                    if po.matrix.is_some() {
                                        if let Some(e) = disp.get_mut(&po.depth) { e.2 = local_mat; }
                                        if let Some(e) = unnamed_placements.get_mut(&po.depth) { e.1 = local_mat; }
                                    }
                                }
                            }
                        }
                        swf::Tag::RemoveObject(ro) => {
                            disp.remove(&ro.depth);
                            unnamed_placements.remove(&ro.depth);
                        }
                        _ => {}
                    }
                }
                if !effect_frames.is_empty() {
                    log::debug!("image_extractor: effect sprite id={} '{}' has {} frames",
                        sprite.id, sym, effect_frames.len());
                    map.insert(sprite.id, effect_frames);
                }
            }
        }
        (map, unnamed_frames)
    };

    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            let sym = symbols.get(&sprite.id)
                .map(|s| s.as_str())
                .unwrap_or("");

            // Process character animation sprites. These are usually named
            // "{char}_fla.{Anim}_{N}", but a root-timeline `stance` sub-sprite can be
            // UNNAMED (sym="") and still be a real animation — the box pass processes
            // it via anim_by_sprite, so the image pass must too, or that animation comes
            // out with hurtboxes but no visual (e.g. zelda's `falling`/tumble stance).
            if !sym.contains("_fla.") && !anim_by_sprite.contains_key(&sprite.id) { continue; }

            let fm_name = match anim_by_sprite.get(&sprite.id) {
                // The shared map already resolved this sprite's FM animation name.
                Some((_ssf2, fm)) => fm.clone(),
                None => {
                    // Not a root-timeline animation — a sub-sprite carrying trail/effect
                    // images. Keep the raw symbol as the key (NOT a symbol-derived ANIM
                    // name); the trail/effect system references these by symbol.
                    sym.to_string()
                }
            };
            log::debug!("image_extractor: sprite '{}' → fm='{}'", sym, fm_name);

            // Root MC transform for this animation
            let root_xf = xform_map.get(&fm_name).copied()
                .unwrap_or(crate::sprite_parser::XframeTransform::default());

            let mut current_frame: u16 = 0;
            // depth → (shape_id, symbol_name, local_matrix) — the active display list
            let mut display_list: BTreeMap<u16, (u16, String, ImageLocalMatrix)> = BTreeMap::new();
            // depth → (effect_sprite_id, place_frame, parent_matrix, graphic_frame) — sub-sprite
            // placements; graphic_frame = the placement's ratio (a Graphic-symbol placement shows
            // the ONE frame the ratio selects — Flash's "Single Frame" — never self-animating).
            let mut sub_sprite_placements: BTreeMap<u16, (u16, u16, ImageLocalMatrix, Option<u16>)> = BTreeMap::new();
            let mut frames: BTreeMap<u16, Vec<FrameImageEntry>> = BTreeMap::new();

            for stag in &sprite.tags {
                match stag {
                    swf::Tag::ShowFrame => {
                        let mut entries: Vec<FrameImageEntry> = Vec::new();

                        // Animation local origin (0,0) in world space = root_xf.apply(0,0)
                        // This is the TRUE rotation center: Flash rotates sub-sprites around (0,0).
                        let anim_origin_x = root_xf.tx;
                        let anim_origin_y = root_xf.ty;

                        // Regular display list entries
                        for (&depth, (id, sym, mat)) in &display_list {
                            let (world_tx, world_ty) = root_xf.apply(mat.tx, mat.ty);
                            let world_sx = mat.sx * root_xf.scale_x();
                            let world_sy = mat.sy * root_xf.scale_y();
                            // World matrix = root_xf * local_matrix (2x2 part)
                            let wa = root_xf.a * mat.a + root_xf.b * mat.c;
                            let wb = root_xf.a * mat.b + root_xf.b * mat.d;
                            let wc = root_xf.c * mat.a + root_xf.d * mat.c;
                            let wd = root_xf.c * mat.b + root_xf.d * mat.d;
                            entries.push(FrameImageEntry {
                                depth,
                                shape_id: *id,
                                symbol_name: sym.clone(),
                                local_matrix: *mat,
                                world_tx,
                                world_ty,
                                world_sx,
                                world_sy,
                                world_rotation: mat.rotation,
                                world_a: wa,
                                world_b: wb,
                                world_c: wc,
                                world_d: wd,
                                anim_origin_x,
                                anim_origin_y,
                            });
                        }

                        // Expand sub-sprite placements into their current inner
                        // frame. Named "_fla." effects live in effect_sprites;
                        // unnamed sub-sprites placed directly in the timeline
                        // (e.g. item_homerun's body movieclips) live in
                        // unnamed_frames.
                        for (&depth, (effect_id, place_frame, parent_mat, pinned)) in &sub_sprite_placements {
                            if let Some(effect_frames) = effect_sprites.get(effect_id)
                                .or_else(|| unnamed_frames.get(effect_id))
                            {
                                // a graphic-pinned placement shows the ratio-selected frame, always.
                                let eff_frame = match pinned {
                                    Some(r) => (*r as usize).min(effect_frames.len().saturating_sub(1)),
                                    None => (current_frame.saturating_sub(*place_frame) as usize)
                                        .min(effect_frames.len().saturating_sub(1)),
                                };
                                // The effect sub-sprite rotates around its own local origin (0,0).
                                // In world space that's root_xf.apply(parent_mat.tx, parent_mat.ty).
                                let (effect_origin_x, effect_origin_y) = root_xf.apply(parent_mat.tx, parent_mat.ty);
                                for (inner_id, inner_sym, inner_mat) in &effect_frames[eff_frame] {
                                    let composed = parent_mat.compose(inner_mat);
                                    let (world_tx, world_ty) = root_xf.apply(composed.tx, composed.ty);
                                    let world_sx = composed.sx * root_xf.scale_x();
                                    let world_sy = composed.sy * root_xf.scale_y();
                                    let wa = root_xf.a * composed.a + root_xf.b * composed.c;
                                    let wb = root_xf.a * composed.b + root_xf.b * composed.d;
                                    let wc = root_xf.c * composed.a + root_xf.d * composed.c;
                                    let wd = root_xf.c * composed.b + root_xf.d * composed.d;
                                    let effect_depth = depth + 1000;
                                    entries.push(FrameImageEntry {
                                        depth: effect_depth,
                                        shape_id: *inner_id,
                                        symbol_name: inner_sym.clone(),
                                        local_matrix: composed,
                                        world_tx,
                                        world_ty,
                                        world_sx,
                                        world_sy,
                                        world_rotation: composed.rotation,
                                        world_a: wa,
                                        world_b: wb,
                                        world_c: wc,
                                        world_d: wd,
                                        anim_origin_x: effect_origin_x,
                                        anim_origin_y: effect_origin_y,
                                    });
                                }
                            }
                        }

                        if !entries.is_empty() {
                            // Sort by depth so layers are ordered back-to-front
                            entries.sort_by_key(|e| e.depth);
                            frames.insert(current_frame, entries);
                        }
                        current_frame += 1;
                    }
                    swf::Tag::PlaceObject(po) => {
                        let inst_name = po.name.as_ref()
                            .map(|n| String::from_utf8_lossy(n.as_bytes()).to_string())
                            .unwrap_or_default();

                        // Skip collision box instances
                        if crate::sprite_parser::BoxType::from_instance_name(&inst_name).is_some() {
                            continue;
                        }

                        let depth = po.depth;
                        let local_mat = po.matrix.map(|m| {
                            let a = m.a.to_f64();
                            let b = m.b.to_f64();
                            let c = m.c.to_f64();
                            let d = m.d.to_f64();
                            ImageLocalMatrix::from_abcd(a, b, c, d,
                                m.tx.get() as f64 / 20.0, m.ty.get() as f64 / 20.0)
                        }).unwrap_or_default();

                        match &po.action {
                            swf::PlaceObjectAction::Place(char_id)
                            | swf::PlaceObjectAction::Replace(char_id) => {
                                if let Some(sym_name) = symbols.get(char_id) {
                                    let lower = sym_name.to_lowercase();
                                    if lower.contains("collisonbox") || lower.contains("collisionbox") {
                                        continue;
                                    }
                                    // Effect sprite (nested _fla. movieclip)? Track as sub-sprite.
                                    if lower.contains("_fla.") {
                                        if effect_sprites.contains_key(char_id) {
                                            sub_sprite_placements.insert(depth, (*char_id, current_frame, local_mat, po.ratio));
                                        }
                                        continue;
                                    }
                                    display_list.insert(depth, (*char_id, sym_name.clone(), local_mat));
                                } else if effect_sprites.contains_key(char_id) {
                                    // Unnamed sub-sprite that was pre-built as an effect sprite
                                    sub_sprite_placements.insert(depth, (*char_id, current_frame, local_mat, po.ratio));
                                } else if shape_to_bitmap.contains_key(char_id) {
                                    // Unnamed shape with a bitmap fill — resolve to bitmap symbol
                                    let bitmap_id = shape_to_bitmap[char_id];
                                    if let Some(img) = images.get(&bitmap_id) {
                                        display_list.insert(depth, (*char_id, img.symbol_name.clone(), local_mat));
                                    }
                                } else if all_sprites.contains_key(char_id) {
                                    // Unnamed sub-sprite not pre-built — track as effect
                                    sub_sprite_placements.insert(depth, (*char_id, current_frame, local_mat, po.ratio));
                                }
                            }
                            swf::PlaceObjectAction::Modify => {
                                // A MODIFY with no matrix only re-states the object (a
                                // "keep" frame) — preserve its existing transform rather
                                // than resetting it to identity, which would teleport it.
                                if po.matrix.is_some() {
                                    if let Some(entry) = display_list.get_mut(&depth) {
                                        entry.2 = local_mat;
                                    } else if let Some(entry) = sub_sprite_placements.get_mut(&depth) {
                                        // Update the parent matrix of a sub-sprite placement
                                        entry.2 = local_mat;
                                    }
                                }
                            }
                        }
                    }
                    swf::Tag::RemoveObject(ro) => {
                        display_list.remove(&ro.depth);
                        sub_sprite_placements.remove(&ro.depth);
                    }
                    _ => {}
                }
            }

            log::debug!("image_extractor: fm='{}' raw_frames={} from {} sprite frames", fm_name, frames.len(), sprite.num_frames);
            // Per-frame symbol trace, for asking what the SOURCE actually draws on a given frame
            // (PEPTIDE_TRACE_ANIM=tilt_up). The emitted keyframes are these, so a frame that looks
            // wrong in Fraymakers is answered here: either it came in wrong, or it was always so.
            if std::env::var("PEPTIDE_TRACE_ANIM").ok().as_deref() == Some(fm_name.as_str()) {
                let mut ks: Vec<_> = frames.keys().copied().collect();
                ks.sort();
                for f in ks {
                    let names: Vec<&str> = frames[&f].iter().map(|e| e.symbol_name.as_str()).collect();
                    eprintln!("[trace-anim] {fm_name} frame {f}: {names:?}");
                }
            }
            if !frames.is_empty() {
                // Fill in frames that didn't get an explicit ShowFrame snapshot (inherit previous)
                let total = sprite.num_frames;
                let mut last_entry: Option<Vec<FrameImageEntry>> = None;
                for f in 0..total {
                    if let Some(entry) = frames.get(&f) {
                        last_entry = Some(entry.clone());
                    } else if let Some(ref entry) = last_entry {
                        frames.insert(f, entry.clone());
                    }
                }
                // Compute max depth slots across all frames
                let max_depth_slots = frames.values().map(|v| v.len()).max().unwrap_or(1);

                // Check if this animation should be split into sub-animations
                // (same split table as sprite_parser)
                let frame_labels = crate::sprite_parser::extract_frame_labels_from_tags(&sprite.tags);
                let sub_splits = crate::sprite_parser::sub_anim_image_splits(&fm_name, &frame_labels, total);

                if sub_splits.is_empty() {
                    // Now that the image pass also processes unnamed stance sprites, two
                    // sprites can map to one fm name (a named visual + an image-less
                    // stance). Never let an image-less sprite clobber an entry that
                    // already has a visual.
                    let new_has_image = frames.values()
                        .any(|v| v.iter().any(|e| !e.symbol_name.is_empty()));
                    let existing_has_image = result.get(&fm_name)
                        .map(|p: &AnimFrameImages| p.frames.values().any(|v| v.iter().any(|e| !e.symbol_name.is_empty())))
                        .unwrap_or(false);
                    if new_has_image || !existing_has_image {
                    result.insert(fm_name, AnimFrameImages {
                        frames,
                        total_frames: total,
                        max_depth_slots,
                    });
                    }
                } else {
                    for (sub_fm_name, start_frame, end_frame) in sub_splits {
                        let slice_len = end_frame.saturating_sub(start_frame);
                        let sliced: BTreeMap<u16, Vec<FrameImageEntry>> = frames.iter()
                            .filter(|(&f, _)| f >= start_frame && f < end_frame)
                            .map(|(&f, v)| (f - start_frame, v.clone()))
                            .collect();
                        let sub_max = sliced.values().map(|v| v.len()).max().unwrap_or(1);
                        log::debug!("image_extractor: sub-anim '{}': frames {}..{} ({} img frames, {} depth slots)",
                            sub_fm_name, start_frame, end_frame, sliced.len(), sub_max);
                        result.insert(sub_fm_name, AnimFrameImages {
                            frames: sliced,
                            total_frames: slice_len,
                            max_depth_slots: sub_max,
                        });
                    }
                }
            }
        }
    }

    result
}

/// Apply image fallbacks for procedural/synthetic animations.
fn apply_image_fallbacks(result: &mut BTreeMap<String, AnimFrameImages>) {
    let mut to_insert: Vec<(String, AnimFrameImages)> = Vec::new();
    for (missing, donor) in crate::sprite_parser::ANIM_FALLBACKS {
        // Override if missing entirely OR if present but has no actual image frames
        let needs_fallback = match result.get(*missing) {
            None => true,
            Some(existing) => existing.frames.is_empty() || existing.frames.values().all(|entries| entries.iter().all(|e| e.symbol_name.starts_with("id_"))),
        };
        if !needs_fallback { continue; }
        if let Some(donor_data) = result.get(*donor) {
            to_insert.push((missing.to_string(), donor_data.clone()));
        }
    }
    for (missing, donor) in crate::sprite_parser::ANIM_FALLBACKS_REVERSED {
        let needs_fallback = match result.get(*missing) {
            None => true,
            Some(existing) => existing.frames.is_empty()
                || existing.frames.values().all(|entries| entries.iter().all(|e| e.symbol_name.starts_with("id_"))),
        };
        if !needs_fallback { continue; }
        if let Some(donor_data) = result.get(*donor) {
            let mut cloned = donor_data.clone();
            let n = cloned.total_frames;
            if n > 1 {
                let old = std::mem::take(&mut cloned.frames);
                for (f, entries) in old {
                    if f < n { cloned.frames.insert(n - 1 - f, entries); }
                }
            }
            to_insert.push((missing.to_string(), cloned));
        }
    }

    for (name, data) in to_insert {
        result.insert(name, data);
    }
}

/// Decode DefineBitsLossless/DefineBitsLossless2 → RGBA pixels
pub(crate) fn decode_lossless(bmp: &swf::DefineBitsLossless) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let mut decoder = ZlibDecoder::new(&bmp.data[..]);
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed)?;

    let w = bmp.width as usize;
    let h = bmp.height as usize;

    match bmp.format {
        swf::BitmapFormat::ColorMap8 { num_colors } => {
            let nc = num_colors as usize + 1;
            let has_alpha = bmp.version == 2;
            let bytes_per_color = if has_alpha { 4 } else { 3 };
            let palette_size = nc * bytes_per_color;

            if decompressed.len() < palette_size {
                anyhow::bail!("Palette data too short");
            }

            let palette = &decompressed[..palette_size];
            let pixel_data = &decompressed[palette_size..];

            // Row stride padded to 4-byte boundary
            let row_stride = (w + 3) & !3;
            let mut rgba = Vec::with_capacity(w * h * 4);

            for y in 0..h {
                let row_start = y * row_stride;
                for x in 0..w {
                    let idx = pixel_data.get(row_start + x).copied().unwrap_or(0) as usize;
                    let ci = idx.min(nc - 1);
                    let base = ci * bytes_per_color;
                    if has_alpha {
                        // DefineBitsLossless2 ColorMap8 palette is RGBA (not ARGB).
                        // SWF spec: format=3 with alpha uses swf_rgba = {R, G, B, A}.
                        // Only format=5 (Rgb32) uses ARGB order.
                        let r = palette[base];
                        let g = palette[base + 1];
                        let b = palette[base + 2];
                        let a = palette[base + 3];
                        rgba.extend_from_slice(&[r, g, b, a]);
                    } else {
                        let r = palette[base];
                        let g = palette[base + 1];
                        let b = palette[base + 2];
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                }
            }
            Ok(rgba)
        }
        swf::BitmapFormat::Rgb15 => {
            anyhow::bail!("RGB15 format not supported");
        }
        swf::BitmapFormat::Rgb32 => {
            let mut rgba = Vec::with_capacity(w * h * 4);
            if bmp.version == 2 {
                // ARGB premultiplied
                for pixel in decompressed.chunks_exact(4) {
                    let a = pixel[0];
                    let r = pixel[1];
                    let g = pixel[2];
                    let b = pixel[3];
                    if a == 0 {
                        rgba.extend_from_slice(&[0, 0, 0, 0]);
                    } else {
                        let r = ((r as u16 * 255) / a as u16).min(255) as u8;
                        let g = ((g as u16 * 255) / a as u16).min(255) as u8;
                        let b = ((b as u16 * 255) / a as u16).min(255) as u8;
                        rgba.extend_from_slice(&[r, g, b, a]);
                    }
                }
            } else {
                for pixel in decompressed.chunks_exact(4) {
                    let r = pixel[1];
                    let g = pixel[2];
                    let b = pixel[3];
                    rgba.extend_from_slice(&[r, g, b, 255]);
                }
            }
            Ok(rgba)
        }
    }
}

/// Decode DefineBitsJpeg3 → (width, height, RGBA pixels)
pub(crate) fn decode_jpeg3(jpeg: &swf::DefineBitsJpeg3) -> Result<(u32, u32, Vec<u8>)> {
    use image::ImageReader;
    use std::io::Cursor;

    let reader = ImageReader::new(Cursor::new(&jpeg.data))
        .with_guessed_format()?;
    let img = reader.decode()?;
    let rgb = img.to_rgba8();
    let w = rgb.width();
    let h = rgb.height();
    let mut rgba = rgb.into_raw();

    if !jpeg.alpha_data.is_empty() {
        use flate2::read::ZlibDecoder;
        use std::io::Read;
        let mut decoder = ZlibDecoder::new(jpeg.alpha_data);
        let mut alpha = Vec::new();
        decoder.read_to_end(&mut alpha)?;

        for (i, a) in alpha.iter().enumerate() {
            if let Some(px) = rgba.get_mut(i * 4 + 3) {
                *px = *a;
            }
        }
    }

    Ok((w, h, rgba))
}

/// Write RGBA pixel data as PNG
fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
    use image::{ImageBuffer, Rgba};
    let img: ImageBuffer<Rgba<u8>, _> = ImageBuffer::from_raw(width, height, rgba.to_vec())
        .context("Failed to create image buffer")?;
    img.save(path)?;
    Ok(())
}

/// Sprite PNGs queued up and encoded across all cores instead of one at a time.
///
/// PNG encode (deflate + row filtering) is about a THIRD of a conversion's CPU and every
/// sprite is independent, so encoding them serially leaves every other core idle. Deferring
/// is safe because nothing reads a sprite back while the SWF walk is still running.
///
/// Queued pixel data is raw RGBA, which is far larger than the encoded file, so the batch
/// flushes itself once it is holding more than `FLUSH_BYTES` — a big character can't balloon
/// memory, and the parallelism is preserved because each flush still has many images in it.
#[derive(Default)]
pub(crate) struct PngBatch {
    jobs: Vec<(std::path::PathBuf, u32, u32, Vec<u8>)>,
    queued_bytes: usize,
}

impl PngBatch {
    /// Roughly 64 MB of raw RGBA in flight before an automatic flush.
    const FLUSH_BYTES: usize = 64 * 1024 * 1024;

    fn push(&mut self, path: std::path::PathBuf, width: u32, height: u32, rgba: Vec<u8>) -> Result<()> {
        self.queued_bytes += rgba.len();
        self.jobs.push((path, width, height, rgba));
        if self.queued_bytes >= Self::FLUSH_BYTES {
            self.flush()?;
        }
        Ok(())
    }

    /// Encode + write everything queued, across `available_parallelism` threads. Workers pull
    /// from a shared cursor so one enormous sprite can't leave the other threads idle.
    fn flush(&mut self) -> Result<()> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        if self.jobs.is_empty() {
            return Ok(());
        }
        let jobs = std::mem::take(&mut self.jobs);
        self.queued_bytes = 0;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(jobs.len());
        let cursor = AtomicUsize::new(0);
        let failure: std::sync::Mutex<Option<anyhow::Error>> = std::sync::Mutex::new(None);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                scope.spawn(|| loop {
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some((path, w, h, rgba)) = jobs.get(i) else { break };
                    if let Err(e) = write_png(path, *w, *h, rgba) {
                        let mut slot = failure.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                    }
                });
            }
        });
        match failure.into_inner().unwrap() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

fn sanitize_name(name: &str) -> String {
    name.replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_")
}

// ─── Projectile image frame extraction ────────────────────────────────────────────────────────

/// Per-frame image data for a single projectile animation.
#[derive(Debug, Clone)]
pub struct ProjectileFrameImages {
    /// frame index → symbol name of the image placed on that frame (if any)
    pub frames: Vec<Option<String>>,
    /// frame index → the placed image's SSF2 local transform (scale/pos/rotation).
    /// Parallel to `frames`. Without applying this the image renders at its full
    /// native bitmap size (SSF2 scales it down) — i.e. WAY too big.
    pub matrices: Vec<Option<ImageLocalMatrix>>,
    /// symbol_name → meta GUID
    pub image_guids: BTreeMap<String, String>,
}

/// Extract per-frame image data for a projectile's inner sprite.
///
/// Uses the same two-pass effect-sprite flattening as `extract_images`.
/// Given the inner sprite ID (e.g. 194 for mario_fireball_mc), returns
/// the image symbol name placed on each frame.
pub fn extract_projectile_frame_images(
    swf_data: &[u8],
    char_id: &str,
    inner_sprite_id: u16,
    img_result: &ImageExtractionResult,
) -> Result<ProjectileFrameImages> {
    use std::io::Cursor;
    let swf_buf = swf::decompress_swf(Cursor::new(swf_data)).context("decompress")?;
    let swf = swf::parse_swf(&swf_buf).context("parse")?;
    extract_projectile_frame_images_from_swf(&swf, char_id, inner_sprite_id, img_result)
}

/// `extract_projectile_frame_images` against an already-parsed SWF. Used
/// per projectile / per effect by main.rs / haxe_gen.rs to amortise the
/// SWF parse across all calls within one character's conversion.
pub fn extract_projectile_frame_images_from_swf(
    swf: &swf::Swf<'_>,
    // Image GUIDs are now symbol-only (see image_meta_guid), so the owner id is
    // no longer part of the seed. Kept in the signature for caller compatibility.
    _char_id: &str,
    inner_sprite_id: u16,
    img_result: &ImageExtractionResult,
) -> Result<ProjectileFrameImages> {
    // Build symbol map
    let mut symbols: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                if !name.is_empty() { symbols.insert(link.id, name); }
            }
        }
    }

    // Collect all sprites
    let mut all_sprites: BTreeMap<u16, &swf::Sprite> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(s) = tag { all_sprites.insert(s.id, s); }
    }

    // Pass 1: build sprite frame tables for ALL sprites (named and unnamed)
    // For unnamed sprites: resolve shape_id → bitmap_id → symbol_name via img_result.shape_to_bitmap.
    // For named sprites: use symbol names directly; these form inner animation sprites for projectiles.
    let mut unnamed_frames: SpriteFrameTable = BTreeMap::new();
    for (&sid, sprite) in &all_sprites {
        let mut disp: BTreeMap<u16, (u16, String, ImageLocalMatrix)> = BTreeMap::new();
        let mut frames: Vec<Vec<(u16, String, ImageLocalMatrix)>> = Vec::new();
        for stag in &sprite.tags {
            match stag {
                swf::Tag::ShowFrame => {
                    frames.push(disp.values().map(|(id, sym, mat)| (*id, sym.clone(), *mat)).collect());
                }
                swf::Tag::PlaceObject(po) => {
                    let mat = po_to_mat(po);
                    match &po.action {
                        swf::PlaceObjectAction::Place(cid) | swf::PlaceObjectAction::Replace(cid) => {
                            if let Some(sname) = symbols.get(cid) {
                                // Named symbol
                                disp.insert(po.depth, (*cid, sname.clone(), mat));
                            } else if let Some(&bitmap_id) = img_result.shape_to_bitmap.get(cid) {
                                // Unnamed shape with a bitmap fill — resolve to symbol name
                                if let Some(img) = img_result.images.get(&bitmap_id) {
                                    disp.insert(po.depth, (*cid, img.symbol_name.clone(), mat));
                                }
                            }
                        }
                        swf::PlaceObjectAction::Modify => {
                            // A MODIFY with no matrix only re-states the object —
                            // keep its existing transform.
                            if po.matrix.is_some() {
                                if let Some(e) = disp.get_mut(&po.depth) { e.2 = mat; }
                            }
                        }
                    }
                }
                swf::Tag::RemoveObject(ro) => { disp.remove(&ro.depth); }
                _ => {}
            }
        }
        if !frames.is_empty() { unnamed_frames.insert(sid, frames); }
    }

    // Pass 2: flatten target sprite
    let sprite = match all_sprites.get(&inner_sprite_id) {
        Some(s) => s,
        None => return Ok(ProjectileFrameImages { frames: vec![], matrices: vec![], image_guids: BTreeMap::new() }),
    };

    let mut disp: BTreeMap<u16, (u16, String, ImageLocalMatrix)> = BTreeMap::new();
    // depth -> (sprite id, parent matrix, place frame, graphic frame): a placement with a ratio
    // is a Graphic symbol showing the ONE frame the ratio selects (Flash's "Single Frame").
    let mut unnamed_placements: BTreeMap<u16, (u16, ImageLocalMatrix, u16, Option<u16>)> = BTreeMap::new();
    let mut cur_frame: u16 = 0;
    let mut effect_frames: Vec<Vec<(u16, String, ImageLocalMatrix)>> = Vec::new();

    for stag in &sprite.tags {
        match stag {
            swf::Tag::ShowFrame => {
                let mut snap: Vec<(u16, String, ImageLocalMatrix)> =
                    disp.values().map(|(id, sym, mat)| (*id, sym.clone(), *mat)).collect();
                for (&_depth, (unnamed_id, parent_mat, place_frame, pinned)) in &unnamed_placements {
                    if let Some(uframes) = unnamed_frames.get(unnamed_id) {
                        let uf_idx = match pinned {
                            Some(r) => (*r as usize).min(uframes.len().saturating_sub(1)),
                            None => (cur_frame.saturating_sub(*place_frame) as usize)
                                .min(uframes.len().saturating_sub(1)),
                        };
                        for (inner_id, inner_sym, inner_mat) in &uframes[uf_idx] {
                            let composed = parent_mat.compose(inner_mat);
                            snap.push((*inner_id, inner_sym.clone(), composed));
                        }
                    }
                }
                effect_frames.push(snap);
                cur_frame += 1;
            }
            swf::Tag::PlaceObject(po) => {
                let mat = po_to_mat(po);
                match &po.action {
                    swf::PlaceObjectAction::Place(cid) | swf::PlaceObjectAction::Replace(cid) => {
                        let named = symbols.get(cid);
                        let is_collisonbox = named.map(|s| {
                            let l = s.to_lowercase();
                            l.contains("collisonbox") || l.contains("collisionbox")
                        }).unwrap_or(false);
                        if is_collisonbox {
                            // collision box — not a visual, skip
                        } else if unnamed_frames.contains_key(cid) {
                            // A sub-sprite (named OR unnamed) → recurse into its
                            // frames. Checking this BEFORE the named-symbol case
                            // is essential: a NAMED nested sprite (e.g.
                            // mario_fla.MarioCut_InPATriangle) is an animation,
                            // not an image — treating it as an image emits a
                            // dangling imageAsset (no PNG is ever written for it).
                            unnamed_placements.insert(po.depth, (*cid, mat, cur_frame, po.ratio));
                        } else if let Some(sname) = named {
                            // Named bitmap/shape placed directly → an image.
                            disp.insert(po.depth, (*cid, sname.clone(), mat));
                        } else if let Some(&bitmap_id) = img_result.shape_to_bitmap.get(cid) {
                            // Direct shape placement with bitmap fill
                            if let Some(img) = img_result.images.get(&bitmap_id) {
                                disp.insert(po.depth, (*cid, img.symbol_name.clone(), mat));
                            }
                        }
                    }
                    swf::PlaceObjectAction::Modify => {
                        // A MODIFY with no matrix only re-states the object —
                        // keep its existing transform.
                        if po.matrix.is_some() {
                            if let Some(e) = disp.get_mut(&po.depth) { e.2 = mat; }
                            if let Some(e) = unnamed_placements.get_mut(&po.depth) { e.1 = mat; }
                        }
                    }
                }
            }
            swf::Tag::RemoveObject(ro) => {
                disp.remove(&ro.depth);
                unnamed_placements.remove(&ro.depth);
            }
            _ => {}
        }
    }

    // Convert effect_frames → per-frame symbol names
    let mut frames: Vec<Option<String>> = Vec::new();
    let mut matrices: Vec<Option<ImageLocalMatrix>> = Vec::new();
    let mut image_guids: BTreeMap<String, String> = BTreeMap::new();

    for frame_entries in &effect_frames {
        // Each entry now has the resolved symbol_name already (from Pass 1 & 2 fixes).
        // Take the first non-empty symbol name in the frame, WITH its local matrix.
        let entry = frame_entries.iter().find(|(_id, sym, _mat)| !sym.is_empty());
        let sym_name = entry.map(|(_id, sym, _mat)| sym.clone());
        let mat = entry.map(|(_id, _sym, mat)| *mat);
        if let Some(ref sym) = sym_name {
            // Symbol-only GUID (matches get_image_meta_guids) so projectile/effect
            // entity refs resolve to the shared sprite .meta regardless of which
            // char owns them — multi-char projectile/effect sprites no longer
            // dangle into placeholders.
            let meta_guid = crate::uuid_gen::image_meta_guid(sym);
            image_guids.insert(sym.clone(), meta_guid);
        }
        frames.push(sym_name);
        matrices.push(mat);
    }

    Ok(ProjectileFrameImages { frames, matrices, image_guids })
}

fn po_to_mat(po: &swf::PlaceObject) -> ImageLocalMatrix {
    po.matrix.map(|m| {
        let a = m.a.to_f64(); let b = m.b.to_f64();
        let c = m.c.to_f64(); let d = m.d.to_f64();
        ImageLocalMatrix::from_abcd(a, b, c, d,
            m.tx.get() as f64 / 20.0, m.ty.get() as f64 / 20.0)
    }).unwrap_or_default()
}

// ─── Projectile and menu sprite discovery ───────────────────────────────────────

/// A single named state within a multi-state projectile outer wrapper.
/// SSF2 uses frame labels on the outer sprite to switch between inner sprites.
#[derive(Debug, Clone)]
pub struct ProjectileState {
    /// SSF2 frame label (e.g. "attack_hold", "attack_idle", "attack_toss")
    pub label: String,
    /// Inner animation sprite ID for this state
    pub inner_sprite_id: u16,
    /// Inner animation sprite name (e.g. "link_fla.BombHeld_152")
    pub inner_sprite_name: String,
    /// Frame count of the inner sprite
    pub inner_frame_count: u16,
}

/// A projectile sprite discovered in the SWF.
#[derive(Debug, Clone)]
pub struct DiscoveredProjectile {
    /// Root projectile sprite ID
    pub sprite_id: u16,
    /// Root projectile name (e.g. "mario_fireball")
    pub name: String,
    /// Inner animation sprite ID (the 'stance' child), if any (single-state projectiles)
    pub inner_sprite_id: Option<u16>,
    /// Inner animation sprite name (e.g. "mario_fla.mario_fireball_mc_210")
    pub inner_sprite_name: Option<String>,
    /// Frame count of the inner animation sprite
    pub inner_frame_count: u16,
    /// For multi-state projectiles (outer wrapper has multiple frame labels),
    /// each entry is one SSF2 state with its own inner sprite.
    /// Empty for single-state projectiles.
    pub states: Vec<ProjectileState>,
    /// SSF2 frame labels found INSIDE the inner animation sprite, in
    /// timeline order, as `(frame_1_based, label)`. These are the
    /// projectile's own animation labels — extracted in parallel to the
    /// character's xframe extraction. Empty when the inner sprite has
    /// no FrameLabel tags (in which case the generators fall back to
    /// the FM template's `projectileSpawn`/`projectileIdle`/`projectileDestroy`).
    pub inner_labels: Vec<(u16, String)>,
}

/// A visual effect sprite discovered in the SWF root. Effects are like
/// projectiles in that they live on the root with a SymbolClass linkage
/// ID, but they don't carry an `attack_idle` FrameLabel or a `stance`
/// PlaceObject — they're pure visual playback. Converted to standalone
/// `library/entities/<name>.entity` files with no scripts/stats; the
/// character's Script.hx spawns them via `match.createVfx(...)`.
#[derive(Debug, Clone)]
pub struct DiscoveredEffect {
    /// Sprite ID of the effect's root timeline
    pub sprite_id: u16,
    /// SymbolClass name (e.g. `effect_land`, `dee_fs_sparkle`).
    /// Becomes the entity `id` and the `getContent()` lookup key.
    pub name: String,
    /// Frame count of the effect's timeline
    pub frame_count: u16,
    /// SSF2 FrameLabel tags found inside the effect sprite, in
    /// timeline order: `(1-based frame, label)`. Used to name the
    /// emitted entity animations; falls back to a single `vfx`
    /// animation when there are no labels.
    pub inner_labels: Vec<(u16, String)>,
}

/// The head/portrait sprite discovered in the SWF.
#[derive(Debug, Clone)]
pub struct DiscoveredHead {
    /// Sprite ID of the head sprite
    pub sprite_id: u16,
    /// Name of the head sprite (e.g. "mario_head")
    pub name: String,
    /// The image symbol placed inside (e.g. "mario_dm0")
    pub image_symbol: Option<String>,
    /// Shape ID that the head sprite places
    pub image_shape_id: Option<u16>,
}

/// Scan the SWF for projectile, effect, and head/portrait sprites.
///
///   - Projectiles → have an `attack_idle` FrameLabel + a `stance`
///     PlaceObject inside.
///   - Effects → a root-level SymbolClass'd sprite with NEITHER of
///     those, AND not matched by the head/UI carve-outs (`*_head`,
///     `*_hud`, `*_icon`, `*_Symbol`, the character's own sprite, or
///     auto-generated `_fla.*` timelines).
///   - Head → matched by `*_head` etc.; see `find_receiver_start`-style
///     name heuristics for menu.entity rendering.
///
/// Returns (projectiles, effects, head_sprite).
pub fn discover_projectiles_and_head(
    swf_data: &[u8],
    char_name: &str,
) -> Result<(Vec<DiscoveredProjectile>, Vec<DiscoveredEffect>, Option<DiscoveredHead>)> {
    use std::io::Cursor;
    let swf_buf = swf::decompress_swf(Cursor::new(swf_data)).context("decompress SWF")?;
    let swf = swf::parse_swf(&swf_buf).context("parse SWF")?;
    discover_projectiles_and_head_from_swf(&swf, char_name)
}

/// `discover_projectiles_and_head` against an already-parsed SWF.
pub fn discover_projectiles_and_head_from_swf(
    swf: &swf::Swf<'_>,
    char_name: &str,
) -> Result<(Vec<DiscoveredProjectile>, Vec<DiscoveredEffect>, Option<DiscoveredHead>)> {
    // Build SymbolClass map: id → name
    let mut symbols: BTreeMap<u16, String> = BTreeMap::new();
    for tag in &swf.tags {
        if let swf::Tag::SymbolClass(links) = tag {
            for link in links {
                let name = link.class_name.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                if !name.is_empty() {
                    symbols.insert(link.id, name);
                }
            }
        }
    }

    // Pre-compute which IDs are bitmaps. The head sprite usually places a
    // mix of Shapes (outlines/backgrounds) and one Bitmap (the actual pixel
    // art portrait). We need to prefer the bitmap when scanning inner tags.
    let mut bitmap_ids: BTreeSet<u16> = BTreeSet::new();
    for tag in &swf.tags {
        match tag {
            swf::Tag::DefineBitsLossless(b) => { bitmap_ids.insert(b.id); }
            swf::Tag::DefineBitsJpeg2 { id, .. } => { bitmap_ids.insert(*id); }
            swf::Tag::DefineBitsJpeg3(d) => { bitmap_ids.insert(d.id); }
            _ => {}
        }
    }

    // Scan DefineSprite tags
    let mut projectiles: Vec<DiscoveredProjectile> = Vec::new();
    let mut head: Option<DiscoveredHead> = None;
    let char_lower = char_name.to_lowercase();

    // Build candidate-head ranking. Every SSF2 character has pixel-art at
    // the SWF root, but the naming varies — the cross-character survey
    // showed `*_head` (most chars: dee_head, blackmage_head, fox_head,
    // bomberman_head, etc.) and `*_icon` (sandbag_icon). `*_hud` is the
    // in-game damage-counter HUD (animated, hundreds of frames), NOT a
    // menu portrait — explicitly excluded. Head sprites named without an
    // underscore (dkhead, dkicon) only contain shapes — they need shape
    // rasterization which is a separate code path.
    let mut candidates: Vec<&swf::Sprite> = Vec::new();
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            let sprite_name = symbols.get(&sprite.id).cloned().unwrap_or_default();
            if sprite_name.is_empty() { continue; }
            let nl = sprite_name.to_lowercase();
            // Skip names that look like in-game gameplay assets (cutins,
            // weapons, HUDs).
            if nl.contains("cutin") || nl.contains("trail") || nl.contains("fs_")
                || nl.ends_with("_hud") || nl == format!("{}_hud", char_lower)
            {
                continue;
            }
            let head_like = nl.ends_with("_head")
                || nl == format!("{}_icon", char_lower);
            if head_like {
                candidates.push(sprite);
            }
        }
    }
    // Stable preference: _head > _icon. _head is the standard convention;
    // _icon is the CSS thumbnail (sandbag uses this since it has no _head).
    candidates.sort_by_key(|s| {
        let n = symbols.get(&s.id).map(|s| s.to_lowercase()).unwrap_or_default();
        if n.ends_with("_head") { 0 } else { 1 }
    });

    for sprite in candidates {
        let sprite_name = symbols.get(&sprite.id).cloned().unwrap_or_default();
        // Walk all inner PlaceObject tags. Prefer a placement whose target
        // is a Bitmap with a SymbolClass name — the actual portrait. Fall
        // back to any placement with a symbol name, then to anything.
        let mut best_bitmap: Option<(u16, String)> = None;
        let mut best_named: Option<(u16, String)> = None;
        let mut first_placement: Option<u16> = None;
        for stag in &sprite.tags {
            if let swf::Tag::PlaceObject(po) = stag {
                if let swf::PlaceObjectAction::Place(id) = &po.action {
                    if first_placement.is_none() { first_placement = Some(*id); }
                    let sym = symbols.get(id).cloned().unwrap_or_default();
                    if !sym.is_empty() {
                        if bitmap_ids.contains(id) && best_bitmap.is_none() {
                            best_bitmap = Some((*id, sym.clone()));
                        }
                        if best_named.is_none() {
                            best_named = Some((*id, sym));
                        }
                    }
                }
            }
        }
        let (image_shape_id, image_symbol) = match (best_bitmap, best_named, first_placement) {
            (Some((id, sym)), _, _) => (Some(id), Some(sym)),
            (None, Some((id, sym)), _) => (Some(id), Some(sym)),
            (None, None, Some(id)) => (Some(id), None),
            _ => (None, None),
        };

        if image_symbol.is_some() {
            log::debug!("Head sprite '{}': resolved image symbol '{}' (id={:?})",
                sprite_name, image_symbol.as_deref().unwrap_or(""), image_shape_id);
            head = Some(DiscoveredHead {
                sprite_id: sprite.id,
                name: sprite_name,
                image_symbol,
                image_shape_id,
            });
            break;
        }
    }

    // The remainder of the scan (projectile detection) walks the same
    // DefineSprite tags but doesn't depend on `head`.
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            let sprite_name = symbols.get(&sprite.id).cloned().unwrap_or_default();
            if sprite_name.is_empty() { continue; }

            // Check for projectile: has 'attack_idle' label
            let has_attack_idle = sprite.tags.iter().any(|t| {
                if let swf::Tag::FrameLabel(fl) = t {
                    fl.label.to_str_lossy(encoding_rs::WINDOWS_1252) == "attack_idle"
                } else {
                    false
                }
            });
            if !has_attack_idle { continue; }

            // Skip the root character sprite
            if sprite_name.to_lowercase() == char_lower { continue; }

            // Walk the outer sprite timeline to collect all frame-label → stance placements.
            // Single-state: one label ('attack_idle') + one PlaceObject(stance).
            // Multi-state: multiple labels each followed by a new PlaceObject(stance).
            let mut states: Vec<ProjectileState> = Vec::new();
            let mut cur_label: Option<String> = None;
            for stag in &sprite.tags {
                match stag {
                    swf::Tag::FrameLabel(fl) => {
                        cur_label = Some(fl.label.to_str_lossy(encoding_rs::WINDOWS_1252).to_string());
                    }
                    swf::Tag::PlaceObject(po) => {
                        let po_name = po.name.as_ref()
                            .map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252).to_string());
                        if po_name.as_deref() == Some("stance") {
                            if let swf::PlaceObjectAction::Place(id) = &po.action {
                                let inner_sym = symbols.get(id).cloned().unwrap_or_default();
                                let inner_frames = swf.tags.iter().find_map(|t| {
                                    if let swf::Tag::DefineSprite(s) = t {
                                        if s.id == *id { return Some(s.num_frames); }
                                    }
                                    None
                                }).unwrap_or(1);
                                if let Some(label) = cur_label.take() {
                                    states.push(ProjectileState {
                                        label,
                                        inner_sprite_id: *id,
                                        inner_sprite_name: inner_sym,
                                        inner_frame_count: inner_frames,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }

            // Single-state shortcut: find the attack_idle entry
            let idle_state = states.iter().find(|s| s.label == "attack_idle");
            let inner_sprite_id = idle_state.map(|s| s.inner_sprite_id);
            let inner_sprite_name = idle_state.map(|s| s.inner_sprite_name.clone());
            let inner_frame_count = idle_state.map(|s| s.inner_frame_count)
                .unwrap_or_else(|| sprite.num_frames.max(1));

            // Only keep states vec if it has more than just attack_idle
            let states = if states.len() > 1 { states } else { Vec::new() };

            // Walk the inner sprite's tags for its own FrameLabel pings —
            // these are the projectile's per-frame SSF2 animation labels
            // (parallel to character xframe labels). Order by frame.
            let mut inner_labels: Vec<(u16, String)> = Vec::new();
            if let Some(inner_id) = inner_sprite_id {
                if let Some(inner_sprite) = swf.tags.iter().find_map(|t| {
                    if let swf::Tag::DefineSprite(s) = t {
                        if s.id == inner_id { return Some(s); }
                    }
                    None
                }) {
                    let mut frame: u16 = 1;
                    for tag in &inner_sprite.tags {
                        match tag {
                            swf::Tag::FrameLabel(fl) => {
                                let label = fl.label.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                                if !label.is_empty() {
                                    inner_labels.push((frame, label));
                                }
                            }
                            swf::Tag::ShowFrame => { frame += 1; }
                            _ => {}
                        }
                    }
                }
            }

            projectiles.push(DiscoveredProjectile {
                sprite_id: sprite.id,
                name: sprite_name,
                inner_sprite_id,
                inner_sprite_name,
                inner_frame_count,
                states,
                inner_labels,
            });
        }
    }

    // ── Effects pass ────────────────────────────────────────────────────
    // A second walk picks up root-level SymbolClass'd sprites that aren't
    // projectiles, the character itself, or UI carve-outs. Effects are
    // pure visual sprites (no `attack_idle`, no `stance`).
    //
    // We exclude BOTH the projectile root sprite IDs AND each projectile's
    // inner animation sprite IDs. The inner sprite is where the projectile
    // actually renders its frames (e.g. `deeFinalSmashProjectile` is the
    // inner sprite under `dee_finalsmash`'s `stance` PlaceObject). Without
    // this exclusion, those inner sprites get re-discovered here and
    // emitted as phantom effect entities — and their ids tend to
    // case-collide with the projectile entity's `<name>Projectile` id
    // (e.g. `deeFinalSmashProjectile` vs `deefinalsmashProjectile`),
    // which FrayTools rejects as a duplicate-id error when it normalizes
    // ids for cross-resource lookup.
    let mut projectile_ids: std::collections::BTreeSet<u16> =
        projectiles.iter().map(|p| p.sprite_id).collect();
    for p in &projectiles {
        if let Some(inner) = p.inner_sprite_id {
            projectile_ids.insert(inner);
        }
        for s in &p.states {
            projectile_ids.insert(s.inner_sprite_id);
        }
    }
    let head_id = head.as_ref().map(|h| h.sprite_id);
    let mut effects: Vec<DiscoveredEffect> = Vec::new();
    for tag in &swf.tags {
        if let swf::Tag::DefineSprite(sprite) = tag {
            let name = symbols.get(&sprite.id).cloned().unwrap_or_default();
            if name.is_empty() { continue; }
            // Auto-generated timeline classes — never effects.
            if name.contains("_fla.") { continue; }
            // Already classified.
            if projectile_ids.contains(&sprite.id) { continue; }
            if Some(sprite.id) == head_id { continue; }
            // The character's own sprite — has a `stance` PlaceObject but
            // no `attack_idle` (which is why the projectile pass skipped
            // it). Skip by name match.
            if name.to_lowercase() == char_lower { continue; }
            // UI carve-outs: HUDs, franchise symbols, head/icon helpers
            // that menu.entity already handles. Match by suffix or by
            // shared SSF2 convention.
            let nl = name.to_lowercase();
            if nl.ends_with("_hud") || nl.ends_with("_symbol")
                || nl.ends_with("_icon") || nl.ends_with("icon")
                || nl.ends_with("_head") || nl == "head"
                || nl == "sparkle"                       // generic 1-frame placeholder
            { continue; }
            // Also skip sprites that carry a `stance` PlaceObject — these
            // are sub-characters or sub-projectiles (rare but observed).
            let has_stance = sprite.tags.iter().any(|t| {
                if let swf::Tag::PlaceObject(po) = t {
                    po.name.as_ref()
                        .map(|n| n.to_str_lossy(encoding_rs::WINDOWS_1252) == "stance")
                        .unwrap_or(false)
                } else { false }
            });
            if has_stance { continue; }

            // Collect inner-sprite FrameLabel tags for the entity's
            // animation names. Same shape as projectile inner_labels.
            let mut inner_labels: Vec<(u16, String)> = Vec::new();
            let mut frame: u16 = 1;
            for t in &sprite.tags {
                match t {
                    swf::Tag::FrameLabel(fl) => {
                        let label = fl.label.to_str_lossy(encoding_rs::WINDOWS_1252).to_string();
                        if !label.is_empty() { inner_labels.push((frame, label)); }
                    }
                    swf::Tag::ShowFrame => { frame += 1; }
                    _ => {}
                }
            }

            effects.push(DiscoveredEffect {
                sprite_id: sprite.id,
                name,
                frame_count: sprite.num_frames.max(1),
                inner_labels,
            });
        }
    }

    Ok((projectiles, effects, head))
}
