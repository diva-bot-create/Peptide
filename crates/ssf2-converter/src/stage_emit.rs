//! stage_emit — emit a Fraymakers stage package from a parsed [`StageModel`].
//!
//! Mirrors the converted-character layout but for a `type:"stage"` resource:
//! ```text
//! stages/<id>/
//!   <id>.fraytools
//!   library/
//!     manifest.json (+ .meta)
//!     entities/<id>.entity
//!     scripts/stage/<id>Script.hx (+ .meta)
//!     scripts/stage/<id>StageStats.hx (+ .meta)
//! ```
//! The `.entity` is the normalized FrayTools graph (symbols + keyframes + layers +
//! one `stage` animation) reversed from the public `Fraymakers/stage-template`:
//! COLLISION_BOX death/camera boxes, LINE_SEGMENT floor + soft platforms,
//! ENTRANCE/RESPAWN points, and an IMAGE layer carrying the stage sprite (the SSF2
//! vector art rasterized + composited by the parser, or a geometry placeholder for
//! bitmap-only stages). Parallax backgrounds + hazards are follow-ups.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use crate::stage_parser::{ParallaxMode, Rect, StageArt, StageModel};
use crate::uuid_gen::det_uuid;

/// Emit the FM stage package for `model` under `out_root/<id>/`. Returns the
/// project dir and the `.fraytools` path (for the FrayTools publish step).
pub fn emit_stage(model: &StageModel, out_root: &Path) -> Result<(PathBuf, PathBuf)> {
    let id = &model.id;
    let dir = out_root.join(id);
    let lib = dir.join("library");
    // the library is fully converter-generated: start clean so renamed content (a hazard id
    // derived from its class, an element id from its symbol) doesn't leave a stale generation
    // behind that FrayTools would bundle alongside the new one.
    if lib.exists() { std::fs::remove_dir_all(&lib).context("clean library")?; }
    std::fs::create_dir_all(lib.join("entities")).context("mkdir entities")?;
    std::fs::create_dir_all(lib.join("scripts").join("stage")).context("mkdir scripts/stage")?;

    // Stage art — split into depth layers: a parallax background, the main stage at
    // character depth, and a foreground in front of fighters. A stage needs visible
    // content to be playable (the engine sizes the stage + places players from the sprite
    // bounds), so if nothing rasterized we fall back to a geometry placeholder as the
    // stage layer. Each layer is a PNG + `.meta`; the `.png` is gitignored (regenerated).
    let sprites = lib.join("sprites").join("Stage");
    std::fs::create_dir_all(&sprites).context("mkdir sprites/Stage")?;
    let write_layer = |suffix: &str, art: &StageArt| -> Result<ArtRef> {
        let guid = det_uuid(&format!("stage::{id}::{suffix}"));
        std::fs::write(sprites.join(format!("{id}_{suffix}.png")), &art.png)?;
        write_json(&sprites.join(format!("{id}_{suffix}.png.meta")), &json!({
            "export": false, "guid": guid, "id": "", "pluginMetadata": {}, "plugins": [], "tags": [], "version": 2
        }))?;
        Ok(ArtRef { guid, x: art.x, y: art.y, w: art.w, h: art.h, hold: art.hold.max(1) as usize })
    };
    let stage_fallback;
    let stage_frames: Vec<&StageArt> = if !model.art.stage_frames.is_empty() {
        model.art.stage_frames.iter().collect()
    } else if model.art.is_empty() {
        stage_fallback = render_placeholder(model);
        vec![&stage_fallback]
    } else {
        vec![]
    };
    let stage_refs: Vec<ArtRef> = stage_frames.iter().enumerate()
        .map(|(i, a)| write_layer(&format!("stage{i}"), a))
        .collect::<Result<_>>()?;
    let parallax_refs: Vec<ParallaxRef> = model.art.parallax.iter().enumerate()
        .map(|(i, layer)| write_layer(&format!("parallax{i}"), &layer.art)
            .map(|r| ParallaxRef { art: r, mode: layer.mode, x_pan: layer.x_pan, y_pan: layer.y_pan }))
        .collect::<Result<_>>()?;
    // each backdrop ELEMENT is its own layer (the SSF2 movieclip model). a static element is one
    // frame (`bgN`); an animated element is its loop (`bgN_0..M`). draw order = list order.
    if std::env::var("PEPTIDE_STAGE_DEBUG").is_ok() {
        for (i, layer) in model.art.background.iter().enumerate() {
            let f0 = &layer.frames[0];
            eprintln!("[bg-emit] {} name={:?} frames={} canvas={}x{} at=({:.1},{:.1}) holds={:?}",
                element_ids(&model.art.background)[i], layer.name, layer.frames.len(), f0.w, f0.h, f0.x, f0.y,
                layer.frames.iter().map(|f| f.hold).collect::<Vec<_>>());
        }
    }
    let elem_ids = element_ids(&model.art.background);
    let bg_refs: Vec<BgLayerRef> = model.art.background.iter().enumerate()
        .map(|(i, layer)| -> Result<BgLayerRef> {
            let eid = &elem_ids[i];
            let frames: Vec<ArtRef> = if layer.frames.len() == 1 {
                vec![write_layer(eid, &layer.frames[0])?]
            } else {
                layer.frames.iter().enumerate()
                    .map(|(j, a)| write_layer(&format!("{eid}_{j}"), a))
                    .collect::<Result<_>>()?
            };
            Ok(BgLayerRef {
                name: bg_layer_name(&layer.name, i), eid: eid.clone(), frames,
                plane: BgPlane::Background,
                segments: layer.segments.iter()
                    .map(|s| (s.label.clone(), s.frames.len(), s.next.clone())).collect(),
            })
        })
        .collect::<Result<_>>()?;
    // UNIVERSAL: promote every ANIMATED backdrop element (the weather embers, jumping podoboos,
    // lava bubbles/splashes, flickering torches, a spectating Bowser, …) to its own looping VFX
    // the stage spawns, instead of baking it as a layer inside the single `stage` animation where
    // every layer shares one master clock. that shared clock is the architectural bug: a long
    // element loop (a 260-frame Bowser) gets tiled/truncated to the master length and phase-jumps
    // each restart, and Flash's independent nested-movieclip loops + same-layer multiples can't be
    // represented at all. an animated backdrop element is exactly that case; a static (1-frame)
    // element has no loop, so it stays baked (cheaper). this is automatic, not flagged: any
    // animated bg element on any stage is treated this way. `PEPTIDE_BG_INLINE` forces the old
    // all-baked behavior for debugging/diffing.
    let inline_bg = std::env::var("PEPTIDE_BG_INLINE").is_ok();
    let (baked_bg, mut promoted_bg): (Vec<BgLayerRef>, Vec<BgLayerRef>) =
        bg_refs.into_iter().partition(|l| inline_bg || l.frames.len() <= 1);
    // an ANIMATED foreground element hits the SAME shared-master-clock bug as an animated
    // background one: baked into the stage entity it gets tiled/truncated to the master length
    // instead of running its own loop. Promotion was background-only, so it never reached the
    // foreground plane. Promote it on the same rule (more than one frame = it animates), routed
    // by its plane into a foreground container so it keeps drawing in front of the fighters.
    let fg_refs: Vec<ArtRef> = model.art.foreground.iter().enumerate()
        .map(|(i, a)| write_layer(&format!("fg{i}"), a)).collect::<Result<_>>()?;
    let promote_fg = !inline_bg && fg_refs.len() > 1;
    if promote_fg {
        promoted_bg.push(BgLayerRef {
            name: "Foreground Art".to_string(), eid: "fg".to_string(),
            frames: fg_refs.clone(), plane: BgPlane::Foreground, segments: Vec::new(),
        });
    }
    // PER-ELEMENT foreground: the parser now splits the in-front plane the same way it splits the
    // backdrop, so each prop is promoted on its own instead of being flattened with the rest. The
    // old whole-plane composite above only ever promoted when the FLATTENED image animated, which
    // is a different (and much rarer) condition than "this plane contains something that animates"
    // — a plane of independently-moving props whose composite happens to be static stayed frozen.
    let fg_elem_ids = element_ids(&model.art.foreground_elements);
    if !inline_bg {
        for (i, layer) in model.art.foreground_elements.iter().enumerate() {
            // namespaced: ids come from the symbol name alone, and a plane's element can share a
            // name with a backdrop one — same id means same filenames, and one silently overwrites
            // the other's art.
            let eid = &format!("fge_{}", fg_elem_ids[i]);
            let frames: Vec<ArtRef> = if layer.frames.len() == 1 {
                vec![write_layer(eid, &layer.frames[0])?]
            } else {
                layer.frames.iter().enumerate()
                    .map(|(j, a)| write_layer(&format!("{eid}_{j}"), a))
                    .collect::<Result<_>>()?
            };
            promoted_bg.push(BgLayerRef {
                name: bg_layer_name(&layer.name, i), eid: eid.clone(), frames,
                plane: BgPlane::Foreground,
                segments: layer.segments.iter()
                    .map(|s| (s.label.clone(), s.frames.len(), s.next.clone())).collect(),
            });
        }
    }
    // declared platforms become MOVING STRUCTURES (like the official stage-template's moving
    // platform): one shared grey `platformSprite` (an IMAGE + a structure LINE_SEGMENT, in the
    // stage entity), and one structure CONTENT per platform that the stage spawns and that moves
    // itself in its own Script (the sink/rise cycle). emit_platform_structures writes the per-
    // platform Stats + the shared Script + the grey sprite, and returns what the manifest + stage
    // Script need.
    let (platform_sprites, structure_contents, structure_spawn_ids) =
        emit_platform_structures(model, &lib, &sprites)?;
    let art = ArtRefs {
        background: baked_bg,
        parallax: parallax_refs,
        stage: stage_refs,
        // emptied when the foreground was promoted to its own looping VFX above, so it isn't
        // drawn twice (once on the master clock, once on its own).
        foreground: if promote_fg { vec![] } else { fg_refs },
        foreground_is_blend_overlay: model.art.foreground_is_blend_overlay,
        foreground_alpha: model.art.foreground_alpha,
        foreground_occluders: model.art.foreground_occluders.iter().enumerate()
            .map(|(i, a)| write_layer(&format!("fgocc{i}"), a)).collect::<Result<_>>()?,
        platform_sprites,
    };

    let entity = build_entity(model, &art);
    // FM-side validation: the built entity must carry the layers a Fraymakers stage needs
    // (these are invariants of build_entity; the check guards against regressions).
    let missing = validate_fm_entity(&entity);
    if !missing.is_empty() {
        bail!("emitted {id}.entity is missing required Fraymakers stage layers: {}", missing.join(", "));
    }
    write_json(&lib.join("entities").join(format!("{id}.entity")), &entity)?;

    // hazards (custom game objects) the stage spawns, if any are declared for this stage.
    let hazard_entries = emit_hazards(model, &lib)?;
    // every animated backdrop element becomes its own looping VFX the stage spawns + reparents
    // into a background container. VFX carry no manifest entry (resolved by id via getContent),
    // so this only returns spawn lines.
    let (bg_spawns, bg_entries) = emit_bg_elements(model, &promoted_bg, &lib)?;

    write_json(&lib.join("manifest.json"), &build_manifest(model, &[hazard_entries.clone(), bg_entries.clone()].concat(), &structure_contents))?;
    write_meta(&lib.join("manifest.json.meta"), id, "manifest", "json", None, None)?;

    let scripts = lib.join("scripts").join("stage");
    // animated if ANY looping art layer has multiple frames — the background (lava/fire/Bowser on
    // bowserscastle) animates just as much as the stage plane, so checking only the stage plane
    // froze background-animated stages (the Script paused the whole timeline).
    // the foreground glow flickers (its own animated layer), so it also requires a playing timeline.
    let animated = art.background.iter().any(|l| l.frames.len() > 1) || art.stage.len() > 1
        || !art.foreground.is_empty();
    // the stage spawns its moving-platform structures + hazards in its Script.
    let mut spawns = structure_spawn_ids.iter()
        .map(|cid| format!("\t\t\tmatch.createStructure(self.getResource().getContent(\"{cid}\"));\n"))
        .collect::<String>();
    spawns.push_str(&hazard_spawn_lines(model));
    spawns.push_str(&bg_spawns);
    write_script(&scripts.join(format!("{id}Script.hx")), &script_hx(id, animated, &spawns))?;
    write_meta(&scripts.join(format!("{id}Script.hx.meta")), id, &format!("{id}Script"), "", Some("STAGE"), None)?;
    write_script(&scripts.join(format!("{id}StageStats.hx")), &stage_stats_hx(id, &art.parallax, model.scale))?;
    write_meta(&scripts.join(format!("{id}StageStats.hx.meta")), id, &format!("{id}StageStats"), "hscript", None, None)?;

    let fraytools = dir.join(format!("{id}.fraytools"));
    write_json(&fraytools, &build_fraytools())?;

    Ok((dir, fraytools))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(v)?).with_context(|| format!("write {}", path.display()))
}

// ─────────────────────────────── the .entity ────────────────────────────────

/// Builders accumulate symbols/keyframes/layers; each `add_*` returns the layer
/// `$id` so the animation can list them in order.
struct EntityBuilder<'a> {
    id: &'a str,
    seq: usize,
    symbols: Vec<Value>,
    keyframes: Vec<Value>,
    layers: Vec<Value>,
    /// Layer ids of the `stage` animation, in render order (first = back).
    anim_layers: Vec<String>,
    /// One `(animationName, layerId)` per parallax camera-background layer (`parallax0`,
    /// `parallax1`, …) — each `_cambg` layer scrolls at its own rate, so each is its own.
    parallax_anims: Vec<(String, String)>,
    /// Length of the stage animation in engine frames (static layers hold for this many).
    frame_len: usize,
    /// SSF2 -> FM art scale (`size_multiplier`): native-resolution art PNGs render at this
    /// scale so they match the scaled-up geometry + fighters.
    scale: f64,
    /// Extra animations beyond `stage`/parallax (e.g. `platformSprite` for moving structures).
    extra_anims: Vec<Value>,
}

impl<'a> EntityBuilder<'a> {
    fn new(id: &'a str) -> Self {
        EntityBuilder { id, seq: 0, symbols: vec![], keyframes: vec![], layers: vec![], anim_layers: vec![], parallax_anims: vec![], frame_len: 1, scale: 1.0, extra_anims: vec![] }
    }
    /// A stable per-entity uuid for `role` (e.g. `"layer:Floor"`).
    fn uid(&mut self, role: &str) -> String {
        self.seq += 1;
        det_uuid(&format!("stage::{}::{}::{}", self.id, role, self.seq))
    }

    /// A CONTAINER layer (Characters / effects depth groups). No symbol.
    fn add_container(&mut self, name: &str, container_type: &str) {
        let kf = self.uid(&format!("kf:{name}"));
        self.keyframes.push(json!({ "$id": kf, "length": self.frame_len, "pluginMetadata": {}, "type": "CONTAINER" }));
        let lid = self.uid(&format!("layer:{name}"));
        self.layers.push(json!({
            "$id": lid, "hidden": false, "locked": false, "name": name, "type": "CONTAINER",
            "keyframes": [kf],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "containerType": container_type } }
        }));
        self.anim_layers.push(lid);
    }

    /// A COLLISION_BOX layer (death / camera box). `rect` in FM coords.
    fn add_collision_box(&mut self, name: &str, box_type: &str, rect: &Rect) {
        let sym = self.uid(&format!("sym:{name}"));
        self.symbols.push(json!({
            "$id": sym, "type": "COLLISION_BOX", "alpha": null, "color": null, "pluginMetadata": {},
            "x": rect.left(), "y": rect.top(), "scaleX": rect.w, "scaleY": rect.h,
            "pivotX": rect.w / 2.0, "pivotY": 0, "rotation": 0
        }));
        let kf = self.uid(&format!("kf:{name}"));
        self.keyframes.push(json!({ "$id": kf, "length": self.frame_len, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "COLLISION_BOX" }));
        let lid = self.uid(&format!("layer:{name}"));
        self.layers.push(json!({
            "$id": lid, "hidden": false, "locked": false, "name": name, "type": "COLLISION_BOX",
            "defaultAlpha": 0.5, "defaultColor": "0xd1d1d1", "keyframes": [kf],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "collisionBoxType": box_type } }
        }));
        self.anim_layers.push(lid);
    }


    /// A LINE_SEGMENT layer (walkable surface). `points` is the polyline (>= 2 points; a
    /// flat floor is 2, a curved/sloped terrain traces its surface). `pm` is the per-symbol
    /// FraymakersMetadata (structureType + ledge/dropThrough flags).
    /// Emit a terrain surface polyline split into TYPED runs: a segment steeper than
    /// `WALL_STEEPNESS_DEG` from horizontal is a wall (the side a fighter is blocked from
    /// depends on travel direction: surfaces trace left to right with the solid below, so a
    /// descending steep segment is the landmass's right face = RIGHT_WALL, an ascending one
    /// its left face = LEFT_WALL); everything else is floor. Without the split the whole
    /// profile is FLOOR and fighters slide up steep side faces instead of being blocked.
    /// Ledge grabs attach only to the outer ends of the first/last floor run.
    fn add_typed_surface(&mut self, name: &str, points: &[(f64, f64)], ledges: (bool, bool), drop_through: bool) {
        const WALL_STEEPNESS_DEG: f64 = 75.0;
        let steep = WALL_STEEPNESS_DEG.to_radians().tan();
        // classify each segment, then merge consecutive same-type segments into runs.
        let mut runs: Vec<(&'static str, Vec<(f64, f64)>)> = Vec::new();
        for w in points.windows(2) {
            let (dx, dy) = (w[1].0 - w[0].0, w[1].1 - w[0].1);
            let ty = if dy.abs() > dx.abs() * steep {
                if dy > 0.0 { "RIGHT_WALL" } else { "LEFT_WALL" }
            } else {
                "FLOOR"
            };
            match runs.last_mut() {
                Some((t, run)) if *t == ty => run.push(w[1]),
                _ => runs.push((ty, vec![w[0], w[1]])),
            }
        }
        let floor_idx: Vec<usize> = runs.iter().enumerate()
            .filter(|(_, (t, _))| *t == "FLOOR").map(|(i, _)| i).collect();
        for (i, (ty, run)) in runs.iter().enumerate() {
            let left_ledge = ledges.0 && floor_idx.first() == Some(&i);
            let right_ledge = ledges.1 && floor_idx.last() == Some(&i);
            let label = if runs.len() == 1 { name.to_string() } else { format!("{name} {} {}", ty.to_lowercase().replace('_', " "), i) };
            self.add_line_segment(&label, run, json!({
                "structureType": ty, "leftLedge": left_ledge, "rightLedge": right_ledge,
                "dropThrough": drop_through
            }));
        }
    }

    fn add_line_segment(&mut self, name: &str, points: &[(f64, f64)], pm: Value) {
        let flat: Vec<f64> = points.iter().flat_map(|&(x, y)| [x, y]).collect();
        let sym = self.uid(&format!("sym:{name}"));
        self.symbols.push(json!({
            "$id": sym, "type": "LINE_SEGMENT", "alpha": 0.5, "color": "0xeeeeee",
            "points": flat,
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": pm }
        }));
        let kf = self.uid(&format!("kf:{name}"));
        self.keyframes.push(json!({ "$id": kf, "length": self.frame_len, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "LINE_SEGMENT" }));
        let lid = self.uid(&format!("layer:{name}"));
        self.layers.push(json!({
            "$id": lid, "hidden": false, "locked": false, "name": name, "type": "LINE_SEGMENT",
            "keyframes": [kf],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "lineSegmentType": "LINE_SEGMENT_STRUCTURE" } }
        }));
        self.anim_layers.push(lid);
    }

    /// An IMAGE layer referencing a sprite by its `.meta` guid (`image_asset`), placed
    /// at `(x, y)` in stage coords. A stage MUST have visible content: the engine sizes
    /// the stage sprite from its image bounds during match setup, and a stage with no
    /// IMAGE never places a player (verified live). So even the geometry MVP emits a
    /// placeholder sprite.
    /// Create an IMAGE layer (in the entity pools) at `img_scale` and return its layer id.
    /// The caller pushes the id into whichever animation it belongs to (stage vs parallax).
    fn make_image(&mut self, name: &str, image_asset: &str, x: f64, y: f64, img_scale: f64) -> String {
        self.make_image_alpha(name, image_asset, x, y, img_scale, 1.0)
    }
    /// `make_image` with an explicit symbol alpha (for a semi-transparent overlay).
    fn make_image_alpha(&mut self, name: &str, image_asset: &str, x: f64, y: f64, img_scale: f64, alpha: f64) -> String {
        let sym = self.uid(&format!("sym:{name}"));
        self.symbols.push(json!({
            "$id": sym, "type": "IMAGE", "imageAsset": image_asset, "alpha": alpha,
            "x": x, "y": y, "scaleX": img_scale, "scaleY": img_scale, "rotation": 0, "pivotX": 0, "pivotY": 0,
            "pluginMetadata": {}
        }));
        let kf = self.uid(&format!("kf:{name}"));
        self.keyframes.push(json!({ "$id": kf, "length": self.frame_len, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "IMAGE" }));
        let lid = self.uid(&format!("layer:{name}"));
        self.layers.push(json!({
            "$id": lid, "hidden": false, "locked": false, "name": name, "type": "IMAGE",
            "keyframes": [kf], "pluginMetadata": {}
        }));
        lid
    }
    /// Add an IMAGE layer to the `stage` animation at the current depth (rendered at the
    /// stage `scale` so the native-resolution art matches the scaled-up geometry).
    /// Build a `platformSprite` animation (an IMAGE + a structure LINE_SEGMENT, in LOCAL coords
    /// centered on the object origin) that a moving Structure references by animationId. The grey
    /// PNG is native size `w x h`; the standable line is the top edge. Stored in `extra_anims`.
    fn add_platform_animation(&mut self, idx: usize, grey_guid: &str, w: f64, _h: f64) {
        let img = self.make_image(&format!("platformImage{idx}"), grey_guid, -w / 2.0, 0.0, 1.0);
        // a solid floor line across the top, with grabbable ledges at both ends.
        let pm = json!({ "structureType": "FLOOR", "leftLedge": true, "rightLedge": true, "dropThrough": false });
        let sym = self.uid("sym:platformLine");
        self.symbols.push(json!({
            "$id": sym, "type": "LINE_SEGMENT", "alpha": 0.5, "color": "0xeeeeee",
            "points": [-w / 2.0, 0.0, w / 2.0, 0.0],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": pm }
        }));
        let kf = self.uid("kf:platformLine");
        self.keyframes.push(json!({ "$id": kf, "length": 1, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "LINE_SEGMENT" }));
        let line = self.uid("layer:platformLine");
        self.layers.push(json!({
            "$id": line, "hidden": false, "locked": false, "name": "Line Segment Layer", "type": "LINE_SEGMENT",
            "keyframes": [kf], "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "lineSegmentType": "LINE_SEGMENT_STRUCTURE" } }
        }));
        let aid = self.uid(&format!("anim:platformSprite{idx}"));
        self.extra_anims.push(json!({
            "$id": aid, "name": format!("platformSprite{idx}"), "pluginMetadata": {}, "layers": [img, line]
        }));
    }

    /// Build a `platformSprite` animation whose collision is SOLID ON ALL SIDES: one line-segment
    /// structure layer carrying a CLOSED clockwise rect outline (floor -> right wall -> ceiling ->
    /// left wall; segment winding sets each face's collision side, the same way the official
    /// content builds solid fences from wall/ceiling segments). Local coords: top edge at y=0
    /// (same convention as the floor-line variant, so a follower script's offsets are unchanged),
    /// spanning x -w/2..w/2 and down to y=h. No grabbable ledges — it's a solid body, not a deck.
    fn add_solid_structure_animation(&mut self, idx: usize, sprite_guid: &str, w: f64, h: f64) {
        let img = self.make_image(&format!("platformImage{idx}"), sprite_guid, -w / 2.0, 0.0, 1.0);
        let pm = json!({ "structureType": "FLOOR", "leftLedge": false, "rightLedge": false, "dropThrough": false });
        let sym = self.uid("sym:platformSolid");
        self.symbols.push(json!({
            "$id": sym, "type": "LINE_SEGMENT", "alpha": 0.5, "color": "0xeeeeee",
            "points": [
                -w / 2.0, 0.0,  w / 2.0, 0.0,   // floor (left -> right)
                 w / 2.0, h,                    // right wall (down)
                -w / 2.0, h,                    // ceiling (right -> left)
                -w / 2.0, 0.0                   // left wall (up, closing the loop)
            ],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": pm }
        }));
        let kf = self.uid("kf:platformSolid");
        self.keyframes.push(json!({ "$id": kf, "length": 1, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "LINE_SEGMENT" }));
        let line = self.uid("layer:platformSolid");
        self.layers.push(json!({
            "$id": line, "hidden": false, "locked": false, "name": "Solid Outline Layer", "type": "LINE_SEGMENT",
            "keyframes": [kf], "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "lineSegmentType": "LINE_SEGMENT_STRUCTURE" } }
        }));
        let aid = self.uid(&format!("anim:platformSprite{idx}"));
        self.extra_anims.push(json!({
            "$id": aid, "name": format!("platformSprite{idx}"), "pluginMetadata": {}, "layers": [img, line]
        }));
    }

    /// Build a `platformSprite` animation whose collision is a CEILING segment along the bottom
    /// edge of the box (y=h): a FLOOR-typed structure never blocks an upward mover (one-way
    /// platform semantics), so a solid body's underside must be its own CEILING-typed structure —
    /// the official content ships separate Ceiling structures for its solid fences the same way.
    fn add_ceiling_structure_animation(&mut self, idx: usize, sprite_guid: &str, w: f64, h: f64) {
        let img = self.make_image(&format!("platformImage{idx}"), sprite_guid, -w / 2.0, 0.0, 1.0);
        let pm = json!({ "structureType": "CEILING", "leftLedge": false, "rightLedge": false, "dropThrough": false });
        let sym = self.uid("sym:platformCeiling");
        self.symbols.push(json!({
            "$id": sym, "type": "LINE_SEGMENT", "alpha": 0.5, "color": "0xeeeeee",
            "points": [w / 2.0, h, -w / 2.0, h],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": pm }
        }));
        let kf = self.uid("kf:platformCeiling");
        self.keyframes.push(json!({ "$id": kf, "length": 1, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "LINE_SEGMENT" }));
        let line = self.uid("layer:platformCeiling");
        self.layers.push(json!({
            "$id": line, "hidden": false, "locked": false, "name": "Ceiling Layer", "type": "LINE_SEGMENT",
            "keyframes": [kf], "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "lineSegmentType": "LINE_SEGMENT_STRUCTURE" } }
        }));
        let aid = self.uid(&format!("anim:platformSprite{idx}"));
        self.extra_anims.push(json!({
            "$id": aid, "name": format!("platformSprite{idx}"), "pluginMetadata": {}, "layers": [img, line]
        }));
    }

    /// Add an IMAGE layer to the `stage` animation at the current depth (rendered at the
    /// stage `scale` so the native-resolution art matches the scaled-up geometry).
    fn add_image(&mut self, name: &str, image_asset: &str, x: f64, y: f64) {
        let lid = self.make_image(name, image_asset, x, y, self.scale);
        self.anim_layers.push(lid);
    }
    /// Add a parallax camera-background layer as its own `parallax{idx}` animation. The IMAGE
    /// symbol stays at scale 1: the camera's ParallaxBG sizes it from
    /// `originalBGWidth × scaleMultiplier` (set in StageStats), so scaling the symbol too
    /// would double it.
    fn add_image_parallax(&mut self, idx: usize, image_asset: &str, x: f64, y: f64) {
        let lid = self.make_image(&format!("Parallax {idx}"), image_asset, x, y, 1.0);
        self.parallax_anims.push((format!("parallax{idx}"), lid));
    }
    /// Add an animated IMAGE layer to the `stage` animation: one keyframe per frame, each
    /// referencing that frame's image, so the layer plays through them (loops).
    fn add_image_frames(&mut self, name: &str, frames: &[(String, f64, f64, usize)]) {
        self.add_image_frames_alpha(name, frames, 1.0);
    }
    /// `add_image_frames` with an explicit symbol alpha (a semi-transparent animated overlay).
    /// Every layer must SPAN the whole stage animation (`frame_len`): a layer whose keyframes end
    /// early goes blank/frozen for the rest of each loop (the "tint flashing" / dead-layer bug).
    /// A single frame becomes one static full-length keyframe; a shorter cycle TILES (repeats,
    /// truncating the last repeat) so it keeps playing for the whole loop like an SSF2 movieclip.
    fn add_image_frames_alpha(&mut self, name: &str, frames: &[(String, f64, f64, usize)], alpha: f64) {
        let mut syms = Vec::new();
        for (i, (guid, x, y, _)) in frames.iter().enumerate() {
            let sym = self.uid(&format!("sym:{name}:{i}"));
            self.symbols.push(json!({
                "$id": sym, "type": "IMAGE", "imageAsset": guid, "alpha": alpha,
                "x": x, "y": y, "scaleX": self.scale, "scaleY": self.scale, "rotation": 0, "pivotX": 0, "pivotY": 0,
                "pluginMetadata": {}
            }));
            syms.push(sym);
        }
        let cycle: usize = frames.iter().map(|(_, _, _, h)| (*h).max(1)).sum();
        let target = self.frame_len.max(1);
        let mut kfs = Vec::new();
        if frames.len() == 1 {
            // static: one keyframe spanning the whole animation.
            let kf = self.uid(&format!("kf:{name}:0"));
            self.keyframes.push(json!({ "$id": kf, "length": target, "pluginMetadata": {}, "symbol": syms[0], "tweenType": "LINEAR", "tweened": false, "type": "IMAGE" }));
            kfs.push(kf);
        } else {
            // animated: per-frame hold = the run length of identical source frames (RLE), so held
            // frames read as pauses and the loop runs at the SSF2 pace. Tile cycles to fill.
            let mut filled = 0usize;
            let mut rep = 0usize;
            'tile: while filled < target {
                for (i, (_, _, _, hold)) in frames.iter().enumerate() {
                    let mut len = (*hold).max(1);
                    if filled + len > target { len = target - filled; }
                    if len == 0 { break 'tile; }
                    let kf = self.uid(&format!("kf:{name}:{rep}:{i}"));
                    self.keyframes.push(json!({ "$id": kf, "length": len, "pluginMetadata": {}, "symbol": syms[i], "tweenType": "LINEAR", "tweened": false, "type": "IMAGE" }));
                    kfs.push(kf);
                    filled += len;
                    if filled >= target { break 'tile; }
                }
                rep += 1;
                // safety: a degenerate zero-length cycle can't fill anything.
                if cycle == 0 || rep > target { break; }
            }
        }
        let lid = self.uid(&format!("layer:{name}"));
        self.layers.push(json!({
            "$id": lid, "hidden": false, "locked": false, "name": name, "type": "IMAGE",
            "keyframes": kfs, "pluginMetadata": {}
        }));
        self.anim_layers.push(lid);
    }

    /// A POINT layer (entrance / respawn). `point_type` = ENTRANCE_POINT|RESPAWN_POINT.
    fn add_point(&mut self, name: &str, point_type: &str, index: usize, x: f64, y: f64, rotation: i64) {
        let sym = self.uid(&format!("sym:{name}"));
        self.symbols.push(json!({
            "$id": sym, "type": "POINT", "alpha": 1, "color": "0xff0000", "pluginMetadata": {},
            "x": x, "y": y, "rotation": rotation
        }));
        let kf = self.uid(&format!("kf:{name}"));
        self.keyframes.push(json!({ "$id": kf, "length": self.frame_len, "pluginMetadata": {}, "symbol": sym, "tweenType": "LINEAR", "tweened": false, "type": "POINT" }));
        let lid = self.uid(&format!("layer:{name}"));
        self.layers.push(json!({
            "$id": lid, "hidden": false, "locked": false, "name": name, "type": "POINT",
            "keyframes": [kf],
            "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "pointType": point_type, "index": index } }
        }));
        self.anim_layers.push(lid);
    }
}

/// A written art layer the entity references: the sprite `.meta` guid + placement + size.
#[derive(Clone)]
struct ArtRef { guid: String, x: f64, y: f64, w: u32, h: u32, hold: usize }

/// A parallax camera-background layer: the written sprite + its scroll mode + pan rate.
struct ParallaxRef { art: ArtRef, mode: ParallaxMode, x_pan: f64, y_pan: f64 }

/// One backdrop element as its own layer: a display name + its frame sequence (1 = static).
struct BgLayerRef {
    name: String, eid: String, frames: Vec<ArtRef>, plane: BgPlane,
    /// The element's label-delimited segments as `(label, frame count, jump)`, when its SSF2 clip
    /// branches between them instead of looping. Frame counts index into `frames`, which is the
    /// segments concatenated in timeline order. Empty for an ordinary looping element.
    segments: Vec<(String, usize, Option<crate::abc_parser::LabelJump>)>,
}

/// The source plane a promoted element came from. Decides which stage container it's
/// reparented into and at what alpha, so a promoted element lands at the same depth the
/// baked version occupied in the entity's layer stack (background art draws behind the
/// stage art; foreground art draws in front of the fighters, semi-transparent).
#[derive(Clone, Copy, PartialEq, Debug)]
enum BgPlane { Background, Foreground }

impl BgPlane {
    /// The Stage container API call that puts an element at this plane's depth.
    fn container(self) -> &'static str {
        match self {
            // behind the baked `Stage Art` layer
            BgPlane::Background => "getBackgroundBehindContainer",
            // the lowest of the four foreground containers, i.e. directly above
            // `Characters Front` — where the baked `Foreground Art` layer sat.
            BgPlane::Foreground => "getForegroundStructuresContainer",
        }
    }
    /// Matches the alpha the baked path used for this plane (add_image_frames_alpha).
    fn alpha(self) -> f64 {
        match self { BgPlane::Background => 1.0, BgPlane::Foreground => 0.5 }
    }
}

/// A readable FM layer name for a backdrop element from its SSF2 symbol id. Strips the
/// `_bg` suffix and any `<stage>_` prefix, title-cases the rest; falls back to a numbered
/// "Background Art" for the unnamed root backdrop.
fn bg_layer_name(sym: &str, idx: usize) -> String {
    let s = sym.trim().trim_end_matches("_bg").trim_end_matches("_BG");
    // drop a leading `<stageid>_` style prefix (everything up to the last meaningful token group).
    let cleaned: String = s.split(['_', '.']).filter(|p| !p.is_empty())
        .map(|p| {
            let mut c = p.chars();
            match c.next() { Some(f) => f.to_uppercase().collect::<String>() + c.as_str(), None => String::new() }
        })
        .collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        if idx == 0 { "Background Art".to_string() } else { format!("Background Art {idx}") }
    } else {
        cleaned
    }
}

/// The depth layers the entity lays out. `stage` is the frame sequence (1 = static);
/// `background` is the ordered per-element backdrop layers (each 1 = static).
struct ArtRefs { background: Vec<BgLayerRef>, parallax: Vec<ParallaxRef>, stage: Vec<ArtRef>, foreground: Vec<ArtRef>, foreground_occluders: Vec<ArtRef>, platform_sprites: Vec<(String, f64, f64, u8)>,
    /// see `StageArtSet::foreground_is_blend_overlay`
    foreground_is_blend_overlay: bool,
    /// see `StageArtSet::foreground_alpha`
    foreground_alpha: f64 }

/// Render the floor + soft platforms as filled rectangles on a transparent canvas
/// covering their bounding box (1px = 1 stage unit). Gives the stage visible content
/// (required for play) and shows the playable geometry.
/// Draw a stand-in for a stage that converted with no art of its own: its collision surfaces, as
/// coloured bands, so the stage is something you can see and stand on rather than an invisible
/// plane. Solid ground and drop-through platforms are drawn in different colours.
///
/// The one rule this has to keep is that the picture sits exactly on the collision it was drawn
/// from. Everything below is in service of that, and each part of it has been got wrong once.
fn render_placeholder(model: &StageModel) -> StageArt {
    use image::{Rgba, RgbaImage};

    // Nothing to draw from. A stage with neither art nor collision is degenerate, but it must not
    // produce a bogus placement: the bounds below would come back inverted (from the empty folds)
    // and land an image at 1e308, which is not a number any consumer of this recovers from.
    if model.platforms.is_empty() {
        let img = RgbaImage::new(1, 1);
        let mut png = Vec::new();
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), 1, 1, image::ExtendedColorType::Rgba8)
            .expect("encode placeholder png");
        return StageArt { png, x: 0.0, y: 0.0, w: 1, h: 1, hold: 1 };
    }

    // bounding box of all collision geometry, with a small margin.
    let rects: Vec<Rect> = model.platforms.iter().map(|p| p.rect).collect();
    let margin = 8.0;
    let min_x = rects.iter().map(|r| r.left()).fold(f64::MAX, f64::min) - margin;
    let min_y = rects.iter().map(|r| r.top()).fold(f64::MAX, f64::min) - margin;
    let max_x = rects.iter().map(|r| r.right()).fold(f64::MIN, f64::max) + margin;
    let max_y = rects.iter().map(|r| r.bottom()).fold(f64::MIN, f64::max) + margin;

    // Drawn in SOURCE units, not FM units. Every stage image is placed at the stage scale, so a
    // raster already measured in FM units gets that scale applied a SECOND time: the art comes out
    // `scale` too wide and slides off its own collision, which reads as collision covering only
    // part of the shape and a floor line that does not sit on the floor. The collision geometry is
    // in FM units, so divide by the scale here and let the one placement put it back.
    let s = if model.scale > 0.0 { model.scale } else { 1.0 };
    // Generous, because a raster that does not cover its own bounds is a raster drawn in the wrong
    // place: the position and scale say it spans the whole stage, so anything cropped off the far
    // edge is simply missing rather than merely smaller.
    let w = (((max_x - min_x) / s).ceil() as u32).clamp(1, PLACEHOLDER_MAX_PX);
    let h = (((max_y - min_y) / s).ceil() as u32).clamp(1, PLACEHOLDER_MAX_PX);
    let mut img = RgbaImage::new(w, h);
    for p in &model.platforms {
        let r = &p.rect;
        let color = if p.drop_through { Rgba([120, 160, 220, 235]) } else { Rgba([90, 100, 120, 255]) };
        // Rounded OUTWARD, and never empty. A surface thinner than one pixel once divided by the
        // scale would otherwise round to a zero-height span and vanish -- which is how a stage ends
        // up with collision a fighter stands on and nothing drawn under their feet.
        let x0 = (((r.left() - min_x) / s).floor().max(0.0) as u32).min(w - 1);
        let y0 = (((r.top() - min_y) / s).floor().max(0.0) as u32).min(h - 1);
        let x1 = (((r.right() - min_x) / s).ceil().max(0.0) as u32).clamp(x0 + 1, w);
        let y1 = (((r.bottom() - min_y) / s).ceil().max(0.0) as u32).clamp(y0 + 1, h);
        for y in y0..y1 {
            for x in x0..x1 {
                img.put_pixel(x, y, color);
            }
        }
    }
    let mut png = Vec::new();
    {
        use image::ImageEncoder;
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .expect("encode placeholder png");
    }
    // Position stays in FM units: a placement's x/y are already parent-space coordinates and only
    // the image's PIXELS are scaled. Size is what had to change, not where it sits.
    StageArt { png, x: min_x, y: min_y, w, h, hold: 1 }
}

/// How large a placeholder raster may get. Big enough for every stage in the corpus at source
/// resolution; the clamp exists so a corrupt model cannot ask for a terabyte of pixels.
const PLACEHOLDER_MAX_PX: u32 = 8192;

fn build_entity(model: &StageModel, art: &ArtRefs) -> Value {
    let id = &model.id;
    let mut b = EntityBuilder::new(id);
    b.scale = model.scale;
    // the stage animation loops for the SUM of the per-frame holds of the richest animated layer
    // (each frame carries its own hold = the run length of identical source frames at FM's
    // 60fps), so the loop matches the SSF2 duration. a static layer holds for the whole loop.
    let layer_len = |refs: &[ArtRef]| refs.iter().map(|a| a.hold).sum::<usize>();
    let bg_max_len = art.background.iter().map(|l| layer_len(&l.frames)).max().unwrap_or(0);
    b.frame_len = bg_max_len.max(layer_len(&art.stage)).max(1);

    // ── render order (first = back): the painted backdrop, background depth containers,
    // the stage art (behind fighters), the character containers, the foreground art (in
    // front of fighters), the foreground containers, then the invisible collision / spawns.
    // The backdrop is FIXED, not parallax-scrolled: the SSF2 `<id>_bg` plane includes the
    // surface fighters stand on, so it has to stay aligned with the collision. ──
    // each backdrop element is its own IMAGE layer/animation (SSF2 movieclip model), in
    // back-to-front list order. a one-frame element is a static image; a multi-frame one loops.
    for layer in &art.background {
        match layer.frames.as_slice() {
            [] => {}
            [a] => b.add_image(&layer.name, &a.guid, a.x, a.y),
            frames => b.add_image_frames(&layer.name, &frames.iter().map(|a| (a.guid.clone(), a.x, a.y, a.hold)).collect::<Vec<_>>()),
        }
    }
    b.add_container("Background Behind", "BACKGROUND_BEHIND_CONTAINER");
    b.add_container("Background Effects", "BACKGROUND_EFFECTS_CONTAINER");
    b.add_container("Background Shadows", "BACKGROUND_SHADOWS_CONTAINER");
    b.add_container("Background Structures", "BACKGROUND_STRUCTURES_CONTAINER");
    if !art.stage.is_empty() {
        let frames: Vec<(String, f64, f64, usize)> = art.stage.iter().map(|a| (a.guid.clone(), a.x, a.y, a.hold)).collect();
        b.add_image_frames("Stage Art", &frames);
    }
    // moving platforms reference the shared `platformSprite` animation (grey surface + a structure
    // line segment); the stage spawns them as Structures that move themselves. Add that animation.
    for (i, (grey, w, h, kind)) in art.platform_sprites.iter().enumerate() {
        if *kind == 1 { b.add_solid_structure_animation(i, grey, *w, *h); continue; }
        if *kind == 2 { b.add_ceiling_structure_animation(i, grey, *w, *h); continue; }
        b.add_platform_animation(i, grey, *w, *h);
    }
    b.add_container("Characters Back", "CHARACTERS_BACK_CONTAINER");
    b.add_container("Characters", "CHARACTERS_CONTAINER");
    b.add_container("Characters Front", "CHARACTERS_FRONT_CONTAINER");
    // opaque foreground occluders (a standable structure's near face SSF2 split off — bowscastle's
    // bridge parapet) draw IN FRONT of fighters at FULL alpha, so a fighter on the deck is occluded
    // from the front. Behind the semi-transparent tint below (the red light reddens the parapet too).
    if !art.foreground_occluders.is_empty() {
        b.add_image_frames("Foreground Occluder",
            &art.foreground_occluders.iter().map(|a| (a.guid.clone(), a.x, a.y, a.hold)).collect::<Vec<_>>());
    }
    // the foreground (bowserscastle's lava-glow sheet + lightmask, classified foreground by the
    // AS3 plane map) draws IN FRONT of fighters — the REAL clip art, one keyframe per real frame
    // (no synthesized flicker), at the opacity the SOURCE authored. SSF2 makes a plane translucent
    // by setting alpha on a parent movieclip, so that value is read off the placement chain per
    // stage; it is not a fixed convention and must not be hardcoded.
    if !art.foreground.is_empty() {
        // a SOLVED blend overlay ships at FULL layer alpha: its per-pixel alpha already IS the
        // blend, so any layer alpha on top of it scales the tint down (measured: the overlay
        // reached the fighters at a fraction of the intended strength). Authored art keeps the
        // source's own plane opacity.
        let alpha = if art.foreground_is_blend_overlay { 1.0 } else { art.foreground_alpha };
        b.add_image_frames_alpha("Foreground Art",
            &art.foreground.iter().map(|a| (a.guid.clone(), a.x, a.y, a.hold)).collect::<Vec<_>>(), alpha);
    }
    b.add_container("Foreground Structures", "FOREGROUND_STRUCTURES_CONTAINER");
    b.add_container("Foreground Shadows", "FOREGROUND_SHADOWS_CONTAINER");
    b.add_container("Foreground Effects", "FOREGROUND_EFFECTS_CONTAINER");
    b.add_container("Foreground Front", "FOREGROUND_FRONT_CONTAINER");

    // boundaries: blast zone + hard camera bounds. NO camera-anchor box: the engine adds an
    // anchor to the camera's target bounds, so any anchor forces the camera to zoom out to
    // keep it framed. A blast-zone-sized anchor pins the camera at max zoom-out and the art
    // never fills the view. Without one, the camera frames the players within the camera box
    // and sits at the StageStats `minZoomHeight` floor, where the scaled backdrop fills, the
    // same as a FM-native stage.
    if let Some(r) = &model.death_box { b.add_collision_box("Death Box", "DEATH_BOX", r); }
    if let Some(r) = &model.camera_box {
        b.add_collision_box("Camera Box", "CAMERA_BOX", r);
    }

    // collision: main floor (solid, with ledges at the SSF2 ledge positions) + soft
    // platforms (drop-through). Ledges grab at the floor's left/right endpoints.
    let main_floor = model.main_floor().cloned();
    let mut plat_n = 0usize;
    for p in &model.platforms {
        // visible platforms are spawned as moving Structures (their collision comes from the
        // structure's own line segment), so they get NO static collision here.
        if p.visible { continue; }
        let r = &p.rect;
        let (lx, rx) = model.ledges
            .filter(|_| main_floor.as_ref().map(|m| m.rect == *r).unwrap_or(false))
            .unwrap_or((r.left(), r.right()));
        // a curved/sloped terrain traces its top surface as a polyline; a flat one is the
        // level top line. ledges anchor at the surface endpoints.
        let surface = |default: Vec<(f64, f64)>| -> Vec<(f64, f64)> {
            p.profile.clone().filter(|pf| pf.len() >= 2).unwrap_or(default)
        };
        if !p.drop_through && main_floor.as_ref().map(|m| m.rect == *r).unwrap_or(false) {
            b.add_typed_surface("Floor", &surface(vec![(lx, r.top()), (rx, r.top())]), (true, true), false);
        } else if p.drop_through {
            plat_n += 1;
            // tag moving platforms in the label so an author opening the entity in FrayTools
            // can find the ones meant to move (their motion isn't ported, see StageModel.moving).
            let tag = if p.moving { " (SSF2 moving, static)" } else { "" };
            b.add_line_segment(&format!("Platform {plat_n} Floor{tag}"), &[(r.left(), r.top()), (r.right(), r.top())], json!({
                "structureType": "FLOOR", "leftLedge": false, "rightLedge": false, "dropThrough": true
            }));
        } else {
            plat_n += 1;
            let tag = if p.moving { " (SSF2 moving, static)" } else { "" };
            b.add_typed_surface(&format!("Solid {plat_n}{tag}"), &surface(vec![(r.left(), r.top()), (r.right(), r.top())]), (false, false), false);
        }
    }

    // spawns: entrances (match start) + respawns. Fill 4 of each (the engine expects
    // a full set); if SSF2 declared fewer, fall back to the main floor center.
    let floor_cx = main_floor.as_ref().map(|m| m.rect.left() + m.rect.w / 2.0).unwrap_or(0.0);
    let floor_top = main_floor.as_ref().map(|m| m.rect.top()).unwrap_or(0.0);
    for i in 0..4usize {
        let (x, y, rot) = model.entrances.iter().find(|s| s.index == i)
            .map(|s| (s.x, s.y, if s.face_left { 270 } else { 90 }))
            .unwrap_or((floor_cx + (i as f64 - 1.5) * 60.0, floor_top - 40.0, 90));
        b.add_point(&format!("Entrance {i}"), "ENTRANCE_POINT", i, x, y, rot);
    }
    for i in 0..4usize {
        let (x, y, rot) = model.respawns.iter().find(|s| s.index == i)
            .map(|s| (s.x, s.y, if s.face_left { 270 } else { 90 }))
            .unwrap_or((floor_cx + (i as f64 - 1.5) * 60.0, floor_top - 200.0, 90));
        b.add_point(&format!("Respawn {i}"), "RESPAWN_POINT", i, x, y, rot);
    }

    // each SSF2 camera-background layer -> its own `parallax{i}` animation (each scrolls at its
    // own rate, set in StageStats).
    for (i, p) in art.parallax.iter().enumerate() {
        b.add_image_parallax(i, &p.art.guid, p.art.x, p.art.y);
    }

    let mut animations = vec![json!({
        "$id": b.uid("anim:stage"), "name": "stage", "pluginMetadata": {}, "layers": b.anim_layers
    })];
    for (name, lid) in b.parallax_anims.clone() {
        let aid = b.uid(&format!("anim:{name}"));
        animations.push(json!({ "$id": aid, "name": name, "pluginMetadata": {}, "layers": [lid] }));
    }
    animations.extend(b.extra_anims.clone());

    // SSF2 30fps -> Fraymakers 60fps: double every keyframe length, exactly like the character
    // port (entity_gen::double_keyframe_lengths). every layer (bg clips, foreground, platforms,
    // containers, points, line segments) was built in SSF2 frame units, so this doubling keeps
    // them all in lockstep at the right real-time speed.
    crate::entity_gen::double_keyframe_lengths(&mut b.keyframes);

    json!({
        "version": 14,
        "id": id,
        "guid": det_uuid(&format!("stage::{id}::entity")),
        "export": true,
        "plugins": ["com.fraymakers.FraymakersMetadata"],
        "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "objectType": "STAGE", "version": "0.4.0" } },
        "symbols": b.symbols,
        "keyframes": b.keyframes,
        "layers": b.layers,
        "animations": animations,
        "tags": [],
        "terrains": [],
        "tilesets": [],
        "paletteMap": { "paletteCollection": null, "paletteMap": null }
    })
}

/// The Fraymakers stage layers a playable `.entity` must carry. Returns the names of any
/// that are missing (empty = valid). Mirrors the required-linkage check on the SSF2 side:
/// a stage needs a Characters container, a walkable floor, spawn/respawn points, and a
/// visible IMAGE (the engine sizes the stage + places fighters from the sprite bounds).
fn validate_fm_entity(entity: &Value) -> Vec<String> {
    let empty = vec![];
    let layers = entity.get("layers").and_then(|l| l.as_array()).unwrap_or(&empty);
    let meta = |l: &Value, key: &str| -> Option<String> {
        l.pointer(&format!("/pluginMetadata/com.fraymakers.FraymakersMetadata/{key}"))
            .and_then(|v| v.as_str()).map(str::to_string)
    };
    let has_container = |ct: &str| layers.iter().any(|l| meta(l, "containerType").as_deref() == Some(ct));
    let has_point = |pt: &str| layers.iter().any(|l| meta(l, "pointType").as_deref() == Some(pt));
    let has_type = |ty: &str| layers.iter().any(|l| l.get("type").and_then(|v| v.as_str()) == Some(ty));

    let mut missing = Vec::new();
    if !has_container("CHARACTERS_CONTAINER") { missing.push("Characters container".to_string()); }
    if !has_type("LINE_SEGMENT") { missing.push("a floor line segment".to_string()); }
    if !has_point("ENTRANCE_POINT") { missing.push("an entrance point".to_string()); }
    if !has_point("RESPAWN_POINT") { missing.push("a respawn point".to_string()); }
    if !has_type("IMAGE") { missing.push("a visible IMAGE layer".to_string()); }
    missing
}

// ───────────────────────── manifest / scripts / project ─────────────────────

// ───────────────────────── stage hazards (custom game objects) ──────────────
// Each declared hazard (mappings/stage/metadata.jsonc) becomes a Fraymakers CUSTOM_GAME_OBJECT:
// an entity with a sprite + a HIT_BOX, a HitboxStats giving the damage, a GameObjectStats, and a
// Script that keeps it active. The stage Script spawns each via match.createCustomGameObject.
// SSF2 hazards are bespoke per stage, so this is the framework + an author-editable hazard, not
// an auto-port of the original behavior.

use crate::stage_parser::{Hazard, Platform};

/// Emit the declared platforms as MOVING STRUCTURES (the FM idiom for a moving/sinking platform,
/// per the official stage-template): one shared grey `platformSprite` PNG (referenced by the
/// `platformSprite` animation in the stage entity), a shared sink/rise Script, and one structure
/// CONTENT (with its own Stats giving startX/startY) per platform. Returns the shared sprite
/// `(guid, w, h)` for the entity animation, the manifest structure-content entries, and the
/// content ids the stage Script spawns with `match.createStructure`.
#[allow(clippy::type_complexity)]
fn emit_platform_structures(model: &StageModel, lib: &Path, sprites: &Path)
    -> Result<(Vec<(String, f64, f64, u8)>, Vec<Value>, Vec<String>)>
{
    let vis: Vec<&Platform> = model.platforms.iter().filter(|p| p.visible).collect();
    if vis.is_empty() { return Ok((Vec::new(), Vec::new(), Vec::new())); }
    let id = &model.id;
    let scripts = lib.join("scripts").join("platform");
    std::fs::create_dir_all(&scripts).context("mkdir scripts/platform")?;
    // shared sink/rise structure Script. HALF_W gates which platform a Thwomp landed on; the
    // platforms sit far apart (columns ~330px), so a generous-but-sub-spacing half-width assigns
    // the Thwomp to exactly one without bleeding into its neighbour.
    let script_id = format!("{id}platformScript");
    // SSF2 BowsersCastlePlatform sinks 145 local px (rest 155 -> 300); scale to FM space.
    // HALF_W covers the widest declared platform so an edge landing still sinks its deck.
    let half_w = vis.iter().map(|p| p.rect.w / 2.0).fold(150.0, f64::max);
    // the platform class's authored motion, converted 30fps -> 60fps: speeds × scale/2 (per-frame
    // velocity), the hold × 2 (frame count), depth × scale. The previous hand-RE'd values stay as
    // the fallback for a stage whose platform class wasn't found.
    let pb = model.platform_behavior.clone().unwrap_or_default();
    let sink_speed = pb.sink_speed.unwrap_or(30.0) * model.scale * 0.5;
    let rise_speed = pb.rise_speed.unwrap_or(1.0) * model.scale * 0.5;
    let wait = pb.wait_frames.unwrap_or(390.0) * 2.0;
    let sink_depth = pb.sink_depth.unwrap_or(145.0) * model.scale;
    write_script(&scripts.join(format!("{script_id}.hx")), &platform_script_hx(half_w, sink_depth, sink_speed, rise_speed, wait))?;
    write_meta(&scripts.join(format!("{script_id}.hx.meta")), id, &script_id, "hscript", Some("LINE_SEGMENT_STRUCTURE"), None)?;
    // per-platform: a sprite sized to THIS platform (the SSF2 standing platforms are different
    // widths), its own `platformSprite{i}` animation, Stats (startX/startY), a structure content
    // entry, and the spawn id. The sprite is TRANSPARENT (collision-only): the visible standable
    // surface is the engine-added bg layer the stage renders (e.g. bowserscastle's brick bridge),
    // so the structure just carries the floor line segment at the platform's position.
    let mut sprite_dims = Vec::new();
    let mut contents = Vec::new();
    let mut spawn_ids = Vec::new();
    for (i, p) in vis.iter().enumerate() {
        let (pw, ph) = (p.rect.w.round().max(8.0) as u32, p.rect.h.round().max(10.0) as u32);
        let img: image::RgbaImage = image::RgbaImage::new(pw, ph); // transparent collision footprint
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .context("encode platform png")?;
        let guid = det_uuid(&format!("stage::{id}::platformSprite{i}"));
        std::fs::write(sprites.join(format!("{id}_platformSprite{i}.png")), &png)?;
        write_json(&sprites.join(format!("{id}_platformSprite{i}.png.meta")), &json!({
            "export": false, "guid": guid, "id": "", "pluginMetadata": {}, "plugins": [], "tags": [], "version": 2
        }))?;
        sprite_dims.push((guid, pw as f64, ph as f64, 0));

        let cid = format!("{id}platform{i}");
        let stats_id = format!("{cid}Stats");
        let (sx, sy) = (p.rect.x + p.rect.w / 2.0, p.rect.y);
        write_script(&scripts.join(format!("{stats_id}.hx")), &platform_stats_hx(id, sx, sy, i))?;
        write_meta(&scripts.join(format!("{stats_id}.hx.meta")), id, &stats_id, "hscript", None, None)?;
        contents.push(json!({ "id": cid, "type": "structure", "objectStatsId": stats_id, "scriptId": script_id }));
        spawn_ids.push(cid);
    }
    // a hazard that declares createSelfPlatform (the Thwomp) gets a DECK structure riding it: the
    // SSF2 self-platform is a SOLID box (floor + walls + ceiling — fighters can't pass through the
    // body), so it emits as a RECT_STRUCTURE sized to the whole declared box, riding the
    // falling/landed/rising body (riding it up is real SSF2 gameplay). the deck finds the thwomp
    // custom game object by matching its x against the spawn columns, engages only once the thwomp
    // has dropped below its spawn hover (SSF2 keeps fallthrough on during the entrance), and parks
    // off-world between cycles.
    // (the decompiler-reconstruction path creates these at the class's own createSelfPlatform
    // call site instead, so the pre-spawned follower pair is template-path only.)
    let decomp_selfplatform = std::env::var("PEPTIDE_DECOMP_HAZARD").is_ok();
    if let Some(hz) = model.hazards.iter().find(|h| h.behavior.self_platform.is_some() && !decomp_selfplatform) {
        let (bx, by, bw, bh) = hz.behavior.self_platform.unwrap();
        let s = model.scale;
        let deck_w = (bw * s).round().max(8.0);
        let deck_h = (bh * s).round().max(8.0);
        let off_x = (bx + bw / 2.0) * s; // box centre relative to the body origin
        let off_y = by * s;              // solid top edge (the rect extends deck_h below it)
        let cols_x: Vec<f64> = if !model.sink_columns.is_empty() { model.sink_columns.clone() }
            else { vis.iter().map(|p| p.rect.x + p.rect.w / 2.0).collect() };
        let spawn_y = model.death_box.as_ref().map(|b| b.y + 60.0)
            .unwrap_or_else(|| vis.iter().map(|p| p.rect.y).fold(f64::MAX, f64::min) - 520.0);
        let di = sprite_dims.len();
        let img: image::RgbaImage = image::RgbaImage::new(deck_w as u32, deck_h as u32); // transparent, collision-only
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .context("encode deck png")?;
        let guid = det_uuid(&format!("stage::{id}::platformSprite{di}"));
        std::fs::write(sprites.join(format!("{id}_platformSprite{di}.png")), &png)?;
        write_json(&sprites.join(format!("{id}_platformSprite{di}.png.meta")), &json!({
            "export": false, "guid": guid, "id": "", "pluginMetadata": {}, "plugins": [], "tags": [], "version": 2
        }))?;
        sprite_dims.push((guid.clone(), deck_w, deck_h, 1));
        let cid = format!("{id}thwompdeck");
        let stats_id = format!("{cid}Stats");
        write_script(&scripts.join(format!("{stats_id}.hx")), &platform_stats_hx(id, -2000.0, -3000.0, di))?;
        write_meta(&scripts.join(format!("{stats_id}.hx.meta")), id, &stats_id, "hscript", None, None)?;
        let deck_script_id = format!("{id}thwompdeckScript");
        write_script(&scripts.join(format!("{deck_script_id}.hx")), &thwomp_deck_script_hx(&cols_x, off_x, off_y, spawn_y + 80.0))?;
        write_meta(&scripts.join(format!("{deck_script_id}.hx.meta")), id, &deck_script_id, "hscript", Some("LINE_SEGMENT_STRUCTURE"), None)?;
        contents.push(json!({ "id": cid, "type": "structure", "objectStatsId": stats_id, "scriptId": deck_script_id }));
        spawn_ids.push(cid);
        // the body's UNDERSIDE: a FLOOR-typed structure never blocks an upward mover, so the
        // ceiling face is its own CEILING-typed follower structure (same script, same offsets;
        // its animation puts the segment at the box's bottom edge).
        let ci = sprite_dims.len();
        sprite_dims.push((guid.clone(), deck_w, deck_h, 2));
        let ceil_cid = format!("{id}thwompceiling");
        let ceil_stats_id = format!("{ceil_cid}Stats");
        write_script(&scripts.join(format!("{ceil_stats_id}.hx")), &platform_stats_hx(id, -2000.0, -3000.0, ci))?;
        write_meta(&scripts.join(format!("{ceil_stats_id}.hx.meta")), id, &ceil_stats_id, "hscript", None, None)?;
        contents.push(json!({ "id": ceil_cid, "type": "structure", "objectStatsId": ceil_stats_id, "scriptId": deck_script_id }));
        spawn_ids.push(ceil_cid);
    }
    Ok((sprite_dims, contents, spawn_ids))
}

/// The Thwomp's rideable deck — 1:1 from SSF2 `createSelfPlatform`: a line-segment structure that
/// follows the thwomp custom game object (matched by column x), standable once the thwomp drops
/// below its spawn hover, parked off-world between cycles.
fn thwomp_deck_script_hx(cols_x: &[f64], off_x: f64, off_y: f64, engage_y: f64) -> String {
    let cols_lit = cols_x.iter().map(|x| format!("{x:.1}")).collect::<Vec<_>>().join(", ");
    format!(
        "// Thwomp deck — the SSF2 createSelfPlatform box riding the thwomp (standable in\n\
         // fall/land/rise; parked off-world while the thwomp waits at its spawn point).\n\
         var COLS_X = [{cols_lit}];\nvar OFF_X = {off_x:.1};\nvar OFF_Y = {off_y:.1};\nvar ENGAGE_Y = {engage_y:.1};\n\
         var m_excluded = self.makeBool(false);\n\n\
         function findThwomp() {{\n\
         \tvar objs = match.getCustomGameObjects();\n\
         \tfor (i in 0...objs.length) {{\n\
         \t\tvar o = objs[i];\n\
         \t\tfor (j in 0...COLS_X.length) {{ if (Math.abs(o.getX() - COLS_X[j]) < 2) {{ return o; }} }}\n\
         \t}}\n\treturn null;\n}}\n\n\
         function update() {{\n\
         \tvar t = findThwomp();\n\
         \t// SSF2 never grounds an enemy on its OWN self-platform: with an ECB the falling thwomp\n\
         \t// would land on this follower mid-air, so exclude it from the structure's collision.\n\
         \tif (t != null && !m_excluded.get()) {{ self.addToBlacklist(t); m_excluded.set(true); }}\n\
         \tif (t != null && t.getY() > ENGAGE_Y) {{ self.setX(t.getX() + OFF_X); self.setY(t.getY() + OFF_Y); }}\n\
         \t// parking carries any remaining rider off-world -> KO, the SSF2 outcome for riding\n\
         \t// the thwomp to the top (it crosses the top blast bound there).\n\
         \telse {{ self.setX(-2000); self.setY(-3000); }}\n}}\n\
         function initialize() {{}}\nfunction onTeardown() {{}}\nfunction onKill() {{}}\nfunction onStale() {{}}\n\
         function afterPushState() {{}}\nfunction afterPopState() {{}}\nfunction afterFlushStates() {{}}\n")
}

/// Stats for one moving platform: which sprite + animation gives its geometry, and where it spawns.
fn platform_stats_hx(stage_id: &str, start_x: f64, start_y: f64, idx: usize) -> String {
    format!(
        "// Moving-platform stats (sink/rise) for {stage_id}\n{{\n\
         \tspriteContent: self.getResource().getContent(\"{stage_id}\"),\n\
         \tanimationId: \"platformSprite{idx}\",\n\tstartX: {start_x:.1},\n\tstartY: {start_y:.1}\n}}\n")
}

/// A STATIC standing-platform structure. SSF2's standing platforms don't sink (the Thwomp drops
/// with its own self-platform); the structure just holds its spawn position and provides the floor
/// line segment from its `platformSprite{i}` animation.
fn platform_script_hx(half_w: f64, sink_depth: f64, sink_speed: f64, rise_speed: f64, wait: f64) -> String {
    // values stepped from the stage's OWN SSF2Platform subclass (extract_platform_behavior): the
    // per-frame sink/rise speeds (class const slots), the post-sink hold (its FrameTimer), and the
    // sink depth (the update()'s rest/sunk Y caps) — converted 30fps -> 60fps (speeds × scale/2,
    // frame counts × 2, depth × scale). Idle until a heavy custom game object lands on this deck,
    // then sink (shaking ±1px), hold, rise back. Shared by all platforms (each captures its own
    // start position), so any column the faller drops on sinks.
    format!(
        "// Sinking platform — values stepped from the stage's SSF2Platform subclass, frame-doubled.\n\
         var SINK_SPEED = {sink_speed:.2};\nvar RISE_SPEED = {rise_speed:.2};\nvar SINK_DEPTH = {sink_depth:.1};\nvar WAIT = {wait:.0};\nvar HALF_W = {half_w:.1};\n\
         var m_init = self.makeBool(false);\nvar m_startY = self.makeFloat(0.0);\nvar m_startX = self.makeFloat(0.0);\n\
         var m_action = self.makeInt(0);\nvar m_timer = self.makeInt(0);\nvar m_shake = self.makeBool(false);\n\n\
         function thwompLanded() {{\n\
         \tvar objs = match.getCustomGameObjects();\n\tvar px = m_startX.get();\n\
         \tfor (i in 0...objs.length) {{\n\
         \t\tvar o = objs[i];\n\
         \t\tif (Math.abs(o.getX() - px) < HALF_W && Math.abs(o.getY() - m_startY.get()) < 90) {{ return true; }}\n\
         \t}}\n\treturn false;\n}}\n\n\
         function update() {{\n\
         \tif (!m_init.get()) {{ m_init.set(true); m_startY.set(self.getY()); m_startX.set(self.getX()); }}\n\
         \tvar a = m_action.get();\n\
         \tif (a == 0) {{ if (thwompLanded()) {{ m_action.set(1); }} }}\n\
         \telse if (a == 1) {{ // sink + shake\n\
         \t\tvar sh = -1.3; if (m_shake.get()) {{ sh = 1.3; }}\n\
         \t\tself.setX(m_startX.get() + sh); m_shake.set(!m_shake.get());\n\
         \t\tself.setY(self.getY() + SINK_SPEED);\n\
         \t\tif (self.getY() >= m_startY.get() + SINK_DEPTH) {{ self.setY(m_startY.get() + SINK_DEPTH); self.setX(m_startX.get()); m_action.set(2); m_timer.set(0); }}\n\
         \t}} else if (a == 2) {{ m_timer.inc(); if (m_timer.get() >= WAIT) {{ m_action.set(3); }} }}\n\
         \telse {{ // rise\n\
         \t\tself.setY(self.getY() - RISE_SPEED);\n\
         \t\tif (self.getY() <= m_startY.get()) {{ self.setY(m_startY.get()); m_action.set(0); }}\n\
         \t}}\n\
         }}\n\
         function initialize() {{}}\nfunction onTeardown() {{}}\nfunction onKill() {{}}\nfunction onStale() {{}}\n\
         function afterPushState() {{}}\nfunction afterPopState() {{}}\nfunction afterFlushStates() {{}}\n")
}


/// File/content-safe element id from the SSF2 source name (instance or linkage), unique across
/// the stage: `bowsers_bubbles_bg` stays readable instead of `bg37`; repeated placements of one
/// symbol (16 torch embers) get an ordinal suffix. Empty names fall back to `bg{idx}`.
fn element_ids(layers: &[crate::stage_parser::BgLayer]) -> Vec<String> {
    use std::collections::BTreeMap;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let base: Vec<String> = layers.iter().enumerate().map(|(i, l)| {
        let b: String = l.name.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
            .collect::<String>().trim_matches('_').to_string();
        let b = if b.is_empty() { format!("bg{i}") } else { b };
        *counts.entry(b.clone()).or_default() += 1;
        b
    }).collect();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    base.into_iter().map(|b| {
        if counts[&b] == 1 { return b; }
        let n = seen.entry(b.clone()).or_default();
        *n += 1;
        format!("{b}{n}")
    }).collect()
}

/// Content id for hazard `idx` of stage `id` — the SSF2 class name when the hazard came from a
/// `spawnEnemy(<Class>)` reference (`bowserscastlessf2BowsersCastleLava`), else `hazard{idx}`.
fn hazard_id(stage_id: &str, idx: usize, hz: &Hazard) -> String {
    match hz.class_name.as_deref().map(|c| c.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>()) {
        Some(c) if !c.is_empty() => format!("{stage_id}{c}"),
        _ => format!("{stage_id}hazard{idx}"),
    }
}

/// Emit every hazard custom game object (entity + Script/GameObjectStats/HitboxStats/
/// AnimationStats + sprite) under the library. Returns the manifest content entries.
fn emit_hazards(model: &StageModel, lib: &Path) -> Result<Vec<Value>> {
    let mut entries = Vec::new();
    if model.hazards.is_empty() { return Ok(entries); }
    let sprites = lib.join("sprites").join("Hazard");
    let scripts = lib.join("scripts").join("hazard");
    std::fs::create_dir_all(&sprites).context("mkdir sprites/Hazard")?;
    std::fs::create_dir_all(&scripts).context("mkdir scripts/hazard")?;
    // a `thwomp`-motion hazard cycles target columns: the stage's AS3-declared sink columns
    // (1:1 from its update disasm, already FM x) when present, each landing on the platform
    // under it; else fall back to the visible platforms' centers.
    let land_y_at = |x: f64| -> f64 {
        model.platforms.iter()
            .filter(|p| !p.hazard_floor && x >= p.rect.left() && x <= p.rect.right())
            .map(|p| p.rect.y).fold(f64::INFINITY, f64::min)
    };
    let cols: Vec<(f64, f64)> = if !model.sink_columns.is_empty() {
        model.sink_columns.iter().map(|&x| (x, land_y_at(x))).filter(|(_, y)| y.is_finite()).collect()
    } else {
        model.platforms.iter().filter(|p| p.visible)
            .map(|p| (p.rect.x + p.rect.w / 2.0, p.rect.y)).collect()
    };

    for (i, hz) in model.hazards.iter().enumerate() {
        let hid = hazard_id(&model.id, i, hz);
        // a hazard whose SSF2 clip has labelled sub-animations (e.g. a Thwomp's entrance/fall/idle)
        // becomes a MULTI-animation custom game object: one FM animation per label, the local state
        // machine switching between them, the HIT_BOX riding only the active (damaging) ones. This
        // is universal — any multi-animation hazard goes down this path; the single-art path below
        // handles a static/looping hazard with no labelled phases (lava, a spike, …).
        if !hz.anims.is_empty() {
            emit_multi_anim_hazard(&hid, hz, model, &sprites, &scripts, lib, &cols, &mut entries)?;
            continue;
        }
        // sprite: the real rasterized SSF2 hazard art if we recovered it, else a translucent red
        // hitbox-volume placeholder (w x h). real art renders at the stage scale, centered on the
        // hazard; the placeholder fills the hitbox 1:1.
        let (png, img_x, img_y, img_scale) = if let Some(art) = &hz.art {
            let (aw, ah) = (art.w as f64 * model.scale, art.h as f64 * model.scale);
            (art.png.clone(), -aw / 2.0, -ah / 2.0, model.scale)
        } else {
            // no own art: a STATIC region hazard's visual IS the stage background (the lava lake is
            // painted bg art; the hazard is just the damage volume) — emit a fully transparent
            // sprite. A MOVING hazard with no art keeps the translucent red debug volume so the
            // missing art is visible rather than an invisible killer.
            let (w, h) = (hz.w.max(8.0) as u32, hz.h.max(8.0) as u32);
            let mut img = image::RgbaImage::new(w, h);
            if hz.motion != "static" {
                for px in img.pixels_mut() { *px = image::Rgba([220, 40, 40, 130]); }
            }
            let mut p = Vec::new();
            image::DynamicImage::ImageRgba8(img).write_to(&mut std::io::Cursor::new(&mut p), image::ImageFormat::Png)
                .context("encode hazard png")?;
            (p, -hz.w / 2.0, -hz.h / 2.0, 1.0)
        };
        let sprite_guid = det_uuid(&format!("hazard::{hid}::sprite"));
        std::fs::write(sprites.join(format!("{hid}.png")), &png)?;
        write_json(&sprites.join(format!("{hid}.png.meta")), &json!({
            "export": false, "guid": sprite_guid, "id": "", "pluginMetadata": {}, "plugins": [], "tags": [], "version": 2
        }))?;

        write_json(&lib.join("entities").join(format!("{hid}.entity")), &hazard_entity(&hid, hz, &sprite_guid, img_x, img_y, img_scale))?;
        write_meta(&lib.join("entities").join(format!("{hid}.entity.meta")), &hid, &hid, "", Some("CUSTOM_GAME_OBJECT"), None)?;

        // a Thwomp (SSF2 falls onto platform columns, sinks them, rises, repeats) uses the real
        // column-cycling script; everything else uses the generic motion. Keyed by label since the
        // kind's default motion is "fall", not the legacy "thwomp" sentinel.
        let is_thwomp = hz.motion == "thwomp" || hz.motion == "fall" || hz.behavior.self_platform.is_some();
        let script = if is_thwomp && !cols.is_empty() {
            thwomp_script_hx(&cols)
        } else {
            hazard_script_hx(hz)
        };
        let files = [
            ("Script", script),
            ("GameObjectStats", hazard_gameobject_stats_hx(&hid)),
            ("HitboxStats", hazard_hitbox_stats_hx(hz)),
            ("AnimationStats", hazard_animation_stats_hx()),
        ];
        for (kind, body) in files {
            let fname = format!("{hid}{kind}");
            write_script(&scripts.join(format!("{fname}.hx")), &body)?;
            write_meta(&scripts.join(format!("{fname}.hx.meta")), &hid, &fname,
                if kind == "Script" { "" } else { "hscript" },
                if kind == "Script" { Some("CUSTOM_GAME_OBJECT") } else { None }, None)?;
        }
        entries.push(json!({
            "type": "customGameObject", "id": hid,
            "scriptId": format!("{hid}Script"),
            "objectStatsId": format!("{hid}GameObjectStats"),
            "hitboxStatsId": format!("{hid}HitboxStats"),
            "animationStatsId": format!("{hid}AnimationStats"),
            "name": hz.label.clone(),
        }));
    }
    Ok(entries)
}

/// The CUSTOM_GAME_OBJECT entity: one `gameObjectIdle` animation with an IMAGE + a HIT_BOX
/// (index 0), sized to the hazard. Mirrors a projectile entity (the proven hitbox carrier).
fn hazard_entity(hid: &str, hz: &Hazard, sprite_guid: &str, img_x: f64, img_y: f64, img_scale: f64) -> Value {
    let g = |s: &str| det_uuid(&format!("hazard::{hid}::{s}"));
    let (hw, hh) = (hz.w / 2.0, hz.h / 2.0);
    let img_sym = |s: &str| json!({ "$id": g(&format!("imgsym{s}")), "type": "IMAGE", "imageAsset": sprite_guid, "x": img_x, "y": img_y, "pivotX": 0.0, "pivotY": 0.0, "scaleX": img_scale, "scaleY": img_scale, "rotation": 0.0, "alpha": 1.0, "pluginMetadata": {} });
    let img_kf = |s: &str| json!({ "$id": g(&format!("imgkf{s}")), "symbol": g(&format!("imgsym{s}")), "length": 1, "tweened": false, "tweenType": "LINEAR", "type": "IMAGE", "pluginMetadata": {} });
    let img_layer = |s: &str| json!({ "$id": g(&format!("imglayer{s}")), "name": "art", "type": "IMAGE", "hidden": false, "locked": false, "keyframes": [g(&format!("imgkf{s}"))], "pluginMetadata": {} });
    let mut e = json!({
        "export": true, "guid": g("entity"), "id": hid, "version": 5,
        "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "objectType": "CUSTOM_GAME_OBJECT", "version": "0.4.0" } },
        "plugins": ["com.fraymakers.FraymakersTypes", "com.fraymakers.FraymakersMetadata"],
        "tags": [], "paletteMap": {}, "tilesets": [], "terrains": [],
        "symbols": [
            img_sym("A"), img_sym("B"),
            { "$id": g("boxsym"), "type": "COLLISION_BOX", "x": -hw, "y": -hh, "pivotX": hw, "pivotY": hh, "scaleX": hz.w, "scaleY": hz.h, "rotation": 0.0, "alpha": 0.5, "color": "0xff0000", "pluginMetadata": {} }
        ],
        "keyframes": [
            img_kf("A"), img_kf("B"),
            { "$id": g("boxkf"), "symbol": g("boxsym"), "length": 1, "tweened": false, "tweenType": "LINEAR", "type": "COLLISION_BOX", "pluginMetadata": {} }
        ],
        "layers": [
            img_layer("A"), img_layer("B"),
            { "$id": g("boxlayer"), "name": "hitbox0", "type": "COLLISION_BOX", "hidden": false, "locked": false, "defaultAlpha": 0.5, "defaultColor": "0xff0000", "keyframes": [g("boxkf")],
              "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "collisionBoxType": "HIT_BOX", "index": 0 } } }
        ],
        // gameObjectIdle = sprite + the HIT_BOX (damages); gameObjectInactive = sprite only (safe,
        // for the pulse off-phase). The Script's local state machine plays one or the other.
        "animations": [
            { "$id": g("anim"), "name": "gameObjectIdle", "layers": [g("imglayerA"), g("boxlayer")], "pluginMetadata": {} },
            { "$id": g("anim2"), "name": "gameObjectInactive", "layers": [g("imglayerB")], "pluginMetadata": {} }
        ]
    });
    // SSF2 30fps -> FM 60fps doubling, same as the stage entity + the character port.
    if let Some(kfs) = e.get_mut("keyframes").and_then(|k| k.as_array_mut()) {
        crate::entity_gen::double_keyframe_lengths(kfs);
    }
    e
}

/// One labelled FM animation of a multi-animation hazard CGO: the FM animation name (the SSF2
/// frame label), its per-frame sprite guids (in order), the shared image placement + canvas size
/// (scaled into FM world units), and whether it carries the HIT_BOX (a damaging phase).
struct HzAnim {
    name: String,
    frame_guids: Vec<String>,
    /// IMAGE placement (frame canvas top-left, already × stage scale).
    x: f64,
    y: f64,
    /// Canvas size (× stage scale) — the HIT_BOX volume for an active phase.
    w: f64,
    h: f64,
    /// Stage scale applied to the IMAGE symbol (native-res PNG → scaled geometry).
    scale: f64,
    /// `true` = a damaging phase (carries the HIT_BOX); `false` = a safe phase (sprite only).
    active: bool,
    /// `true` = the SWF sub-clip stop()s (Flash timeline class with a stopping frame script), so
    /// the FM animation plays once and HOLDS (endType NONE); `false` = the clip genuinely loops.
    hold: bool,
}

/// A valid FM animation/identifier name from an SSF2 frame label (alphanumerics only, lower-cased
/// first char-run kept; falls back to `anim` if it sanitizes to empty).
fn sanitize_anim(label: &str) -> String {
    let s: String = label.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if s.is_empty() { return "anim".to_string(); }
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) { format!("a{s}") } else { s }
}

/// Whether an animation label names a DAMAGING phase (so it carries the HIT_BOX). A Thwomp's
/// `fall`/`land`/`slam` hit; its `entrance`/`idle` don't. Universal keyword test.
fn is_active_anim(label: &str) -> bool {
    let l = label.to_ascii_lowercase();
    ["fall", "land", "slam", "attack", "strike", "active", "shake", "hit"].iter().any(|k| l.contains(k))
}

/// Emit a multi-animation hazard CGO: write each labelled animation's frames as sprite PNGs, build
/// a CUSTOM_GAME_OBJECT entity with one FM animation per label (HIT_BOX on the damaging ones), and
/// a local-state-machine Script that switches animations by phase. Universal across stages; the
/// Thwomp is the first user (entrance/fall/idle).
#[allow(clippy::too_many_arguments)]
fn emit_multi_anim_hazard(
    hid: &str, hz: &Hazard, model: &StageModel, sprites: &Path, scripts: &Path, lib: &Path,
    cols: &[(f64, f64)], entries: &mut Vec<Value>,
) -> Result<()> {
    let scale = model.scale;
    let mut hzanims: Vec<HzAnim> = Vec::new();
    for anim in &hz.anims {
        let fm_name = sanitize_anim(&anim.label);
        // a static hazard (lava) is a persistent hit volume — its damage rides every animation, since
        // none names a transient "fall/slam" phase the keyword test looks for. an intermittent hazard
        // (the thwomp) only hits on its active frames.
        let active = is_active_anim(&anim.label) || hz.motion == "static";
        let mut guids = Vec::new();
        for (fi, fr) in anim.frames.iter().enumerate() {
            let base = format!("{hid}_{fm_name}_{fi}");
            let guid = det_uuid(&format!("hazard::{hid}::{fm_name}::{fi}"));
            std::fs::write(sprites.join(format!("{base}.png")), &fr.png)?;
            write_json(&sprites.join(format!("{base}.png.meta")), &json!({
                "export": false, "guid": guid, "id": "", "pluginMetadata": {}, "plugins": [], "tags": [], "version": 2
            }))?;
            guids.push(guid);
        }
        // frames of one label share a fixed union canvas, so frame 0 carries the placement + size.
        let f0 = &anim.frames[0];
        hzanims.push(HzAnim {
            name: fm_name, frame_guids: guids,
            x: f0.x * scale, y: f0.y * scale,
            w: f0.w as f64 * scale, h: f0.h as f64 * scale,
            scale, active, hold: anim.stop_frame.is_some(),
        });
    }
    if hzanims.is_empty() { return Ok(()); }

    // the resting/default animation (idle), the entrance (played at spawn), and the damaging
    // falling animation (fall), resolved by keyword so the state machine and stats line up
    // regardless of the source's exact labels.
    let fall_name = hzanims.iter().find(|a| a.active)
        .map(|a| a.name.clone()).unwrap_or_else(|| hzanims[0].name.clone());
    let idle_name = hzanims.iter().find(|a| a.name.contains("idle") || a.name.contains("rest"))
        .or_else(|| hzanims.iter().find(|a| !a.active))
        .map(|a| a.name.clone()).unwrap_or_else(|| hzanims[0].name.clone());
    let entrance_name = hzanims.iter().find(|a| a.name.contains("entrance") || a.name.contains("intro"))
        .map(|a| a.name.clone()).unwrap_or_else(|| idle_name.clone());

    let is_thwomp = hz.motion == "thwomp" || hz.motion == "fall" || hz.behavior.self_platform.is_some();
    // The hit volume = the clip's REAL SSF2 attackBox shapes (clip-local, FM-scaled), recovered the
    // same way the lava's is — the Thwomp's `fall` carries a left + right pair at its slam face. Top-
    // left rects for the COLLISION_BOX emit. If a hazard has no recoverable box, the emitter falls
    // back to the art canvas. This is NOT a guessed/own-stats size: it's the engine attackBox tied
    // to the animation (a HIT_BOX layer that rides the damaging frames).
    let mut hit_boxes: Vec<(f64, f64, f64, f64)> = hz.attack_boxes.iter()
        .map(|r| (r.x, r.y, r.w, r.h)).collect();
    // a static hazard (lava) has no CollisionBox in its art clip — SSF2 checks it by region, not a
    // shape — so attack_boxes is empty and the art-canvas fallback would shrink the hit volume to one
    // small lava tile. its damage covers the whole detected lava AABB (hz.w x hz.h), centred on the
    // CGO origin. COLLISION_BOX (x,y) is the box's un-rotated TOP-LEFT (the pivot only matters for
    // rotation — the convention the character path verified against framy), so centring on the CGO
    // = minus half the size; (0,0) would hang the whole volume below-right of the hazard.
    if hit_boxes.is_empty() && hz.motion == "static" && hz.w > 0.0 && hz.h > 0.0 {
        hit_boxes.push((-hz.w / 2.0, -hz.h / 2.0, hz.w, hz.h));
    }
    let dual = hit_boxes.len() >= 2;
    write_json(&lib.join("entities").join(format!("{hid}.entity")), &hazard_entity_multi(hid, &hzanims, &hit_boxes))?;
    write_meta(&lib.join("entities").join(format!("{hid}.entity.meta")), hid, hid, "", Some("CUSTOM_GAME_OBJECT"), None)?;

    // EXPERIMENTAL (PEPTIDE_DECOMP_HAZARD): emit the hazard class's update()/initialize()
    // reconstructed through the character/projectile decompile→translate pipeline, instead of the
    // hand-written template. Not yet runnable (needs the field-state + FrameTimer pass) — gated so
    // it never regresses the working template; used to iterate the reconstruction toward 1:1.
    let decomp = std::env::var("PEPTIDE_DECOMP_HAZARD").is_ok();
    let script = if let (true, Some(rs)) = (decomp, hz.reconstructed_script.as_ref()) {
        // the entity's real animation names (what registerLocalState/playAnimation need) — the
        // full clip set, not just the forceAttack subset (the fall clip has no forceAttack).
        let entity_anims: Vec<String> = hz.anims.iter().map(|a| sanitize_anim(&a.label)).collect();
        // a column-falling hazard gets the synthesized stage spawn cycle (the part of its
        // lifecycle that lives in the STAGE class, not the enemy class) — same data the template
        // uses: the spawn machine's period + columns, the spawn height, the fall-speed cap, the
        // entrance clip's frame-script bob, and the camera-target flag.
        let wrap = (is_thwomp && !cols.is_empty()).then(|| {
            let fc = hz.faller.clone().unwrap_or_default();
            ReconWrap {
                cols: cols.to_vec(),
                spawn_y: model.death_box.as_ref().map(|b| b.y + 60.0)
                    .unwrap_or_else(|| cols.iter().map(|(_, y)| *y).fold(f64::MAX, f64::min) - 520.0),
                spawn_period: fc.spawn_period.unwrap_or(600.0) * 2.0,
                terminal: hz.max_y_speed.or(hz.behavior.fall_gravity).unwrap_or(30.0) * scale * 0.5,
                bob: hz.anims.iter().find(|a| sanitize_anim(&a.label) == entrance_name)
                    .map(|a| a.frame_velocities.clone()).unwrap_or_default(),
                camera: hz.behavior.camera_target,
                entrance_anim: entrance_name.clone(),
                self_platform: hz.behavior.self_platform,
            }
        });
        format!("// {} reconstructed from its SSF2 update()/initialize() via the character decompiler,\n\
                 // then made FM-CGO-runnable: field state -> named local states, FrameTimers -> the engine's\n\
                 // makeFrameTimer, physics -> a scripted kinematics integrator (30fps units converted), and\n\
                 // the STAGE class's spawn cycle synthesized around the enemy class's own state machine.\n\n{}",
                hz.label, cgo_runnable(rs, &entity_anims, scale, wrap.as_ref()))
    } else if is_thwomp && !cols.is_empty() {
        // hover/spawn height: SSF2 spawns the thwomp AT getDeathBounds().y with
        // surviveDeathBounds=true. FM kills a game object outside the blast zone, so park
        // just INSIDE the top bound — still above the camera ceiling, so visually identical.
        let spawn_y = model.death_box.as_ref().map(|b| b.y + 60.0)
            .unwrap_or_else(|| cols.iter().map(|(_, y)| *y).fold(f64::MAX, f64::min) - 520.0);
        // The thwomp's motion values come from stepping ITS OWN update()/initialize() (not a
        // hardcoded template): `getCamera().shake(N)` (the slam shake), `updateEnemyStats({gravity})`
        // (the capped fall speed), and `setYSpeed(-N)` (the rise). 30fps→60fps + the size scale =
        // ×scale/2 for a per-frame velocity; the shake amplitude is a screen effect, just ×scale.
        // `match.getCamera().shake` is the FM camera API (RE'd MatchApi::getCamera → CameraApi::shake;
        // the bare `cameraApi` binding is compiler-injected, absent in interpreted hscript). Per-kind
        // defaults only apply if the class declared no value.
        let b = &hz.behavior;
        let shake_amp = b.shake.unwrap_or(13.0) * scale;
        let fall_v = b.fall_gravity.unwrap_or(30.0) * scale * 0.5;
        let rise_v = b.rise_yspeed.map(f64::abs).unwrap_or(6.0) * scale * 0.5;
        // the landing dust poof: the global-vfx shape from the mappings' global_vfx_map, at the
        // scale the hazard's own attachEffect call declared (× stage scale, a screen effect).
        let dust_scale = b.dust.as_ref().map(|(_, sx, _)| *sx).unwrap_or(2.0) * scale;
        // the cycle cadence, stepped from the classes' own FrameTimers (enemy: entrance delay +
        // landed wait, in declaration order; stage: the spawn machine's two phases), x2 for 60fps.
        let fc = hz.faller.clone().unwrap_or_default();
        let spawn_period = fc.spawn_period.unwrap_or(600.0) * 2.0;
        let entrance_t = fc.entrance_delay.unwrap_or(60.0) * 2.0;
        let land_wait = fc.land_wait.unwrap_or(90.0) * 2.0;
        // the entrance bob: the entrance sub-clip's frame scripts drive a setYSpeed timeline
        // (descend into view, hover, rise) — converted 30->60fps (frame x2, speed x scale/2) into
        // a timer-keyed velocity ladder the ENTRANCE branch integrates.
        let entrance_bob: String = hz.anims.iter()
            .find(|a| sanitize_anim(&a.label) == entrance_name)
            .map(|a| a.frame_velocities.iter()
                .map(|(f, v)| format!("\t\tif (m_timer.get() == {}) {{ m_vy.set({:.2}); }}\n",
                    (f.saturating_sub(1)) * 2, v * scale * 0.5))
                .collect())
            .unwrap_or_default();
        let camera = hz.behavior.camera_target;
        thwomp_multi_script_hx(cols, spawn_y, shake_amp, fall_v, rise_v, dust_scale, spawn_period, entrance_t, land_wait, &entrance_bob, camera, &entrance_name, &idle_name, &fall_name)
    } else {
        hazard_anim_loop_script_hx(hz, &hzanims, &idle_name)
    };
    // ECB for the engine's terrain grounding, only for a hazard that moves: head = body height
    // above the origin, hips = half-width, from the class's own createSelfPlatform box (its real
    // SSF2 collision volume) or the detected body as fallback.
    let ecb = (hz.motion != "static").then(|| {
        let scale = model.scale;
        match &hz.behavior.self_platform {
            Some((_, y, w, _h)) => ((-y) * scale, (w / 2.0) * scale),
            None => (hz.h, hz.w / 2.0), // hz.w/h are already FM-scaled
        }
    });
    let files = [
        ("Script", script),
        ("GameObjectStats", hazard_gameobject_stats_multi(hid, &idle_name, ecb)),
        ("HitboxStats", hazard_hitbox_stats_multi(hz, &hzanims, dual)),
        ("AnimationStats", hazard_animation_stats_multi(&hzanims)),
    ];
    for (kind, body) in files {
        let fname = format!("{hid}{kind}");
        write_script(&scripts.join(format!("{fname}.hx")), &body)?;
        write_meta(&scripts.join(format!("{fname}.hx.meta")), hid, &fname,
            if kind == "Script" { "" } else { "hscript" },
            if kind == "Script" { Some("CUSTOM_GAME_OBJECT") } else { None }, None)?;
    }
    entries.push(json!({
        "type": "customGameObject", "id": hid,
        "scriptId": format!("{hid}Script"),
        "objectStatsId": format!("{hid}GameObjectStats"),
        "hitboxStatsId": format!("{hid}HitboxStats"),
        "animationStatsId": format!("{hid}AnimationStats"),
        "name": hz.label.clone(),
    }));
    Ok(())
}

/// The multi-animation CGO entity: one IMAGE layer per labelled animation (a keyframe per source
/// frame, 30→60fps doubled), and HIT_BOX layer(s) riding the damaging animations. Each FM animation
/// references its image layer (+ its hitbox layers when active). `hit_boxes` are the clip's real
/// SSF2 attackBox rects (clip-local FM top-left x/y, w, h) — one HIT_BOX layer per box (the thwomp's
/// left+right pair → index 0/1, which the stats angle apart). Empty falls back to the full art
/// canvas as a single hit volume.
fn hazard_entity_multi(hid: &str, anims: &[HzAnim], hit_boxes: &[(f64, f64, f64, f64)]) -> Value {
    let g = |s: &str| det_uuid(&format!("hazard::{hid}::{s}"));
    let mut symbols = Vec::new();
    let mut keyframes = Vec::new();
    let mut layers = Vec::new();
    let mut animations = Vec::new();
    for (ai, a) in anims.iter().enumerate() {
        let mut kf_ids = Vec::new();
        for (fi, guid) in a.frame_guids.iter().enumerate() {
            let symid = g(&format!("imgsym{ai}_{fi}"));
            symbols.push(json!({ "$id": symid, "type": "IMAGE", "imageAsset": guid, "x": a.x, "y": a.y, "pivotX": 0.0, "pivotY": 0.0, "scaleX": a.scale, "scaleY": a.scale, "rotation": 0.0, "alpha": 1.0, "pluginMetadata": {} }));
            let kfid = g(&format!("imgkf{ai}_{fi}"));
            keyframes.push(json!({ "$id": kfid, "symbol": symid, "length": 1, "tweened": false, "tweenType": "LINEAR", "type": "IMAGE", "pluginMetadata": {} }));
            kf_ids.push(kfid);
        }
        let img_layer = g(&format!("imglayer{ai}"));
        layers.push(json!({ "$id": img_layer, "name": "art", "type": "IMAGE", "hidden": false, "locked": false, "keyframes": kf_ids, "pluginMetadata": {} }));
        let mut anim_layers = vec![img_layer];
        if a.active {
            let span = a.frame_guids.len().max(1) as u64;
            // one HIT_BOX per recovered SSF2 attackBox (the thwomp's left/right pair = index 0/1);
            // no recovered box -> the art canvas as a single fallback hit volume.
            let boxes: Vec<(f64, f64, f64, f64)> = if hit_boxes.is_empty() {
                vec![(a.x, a.y, a.w, a.h)]
            } else {
                hit_boxes.to_vec()
            };
            for (bi, (bx, by, bw, bh)) in boxes.iter().enumerate() {
                let (hw, hh) = (bw / 2.0, bh / 2.0);
                let boxsym = g(&format!("boxsym{ai}_{bi}"));
                symbols.push(json!({ "$id": boxsym, "type": "COLLISION_BOX", "x": bx, "y": by, "pivotX": hw, "pivotY": hh, "scaleX": bw, "scaleY": bh, "rotation": 0.0, "alpha": 0.5, "color": "0xff0000", "pluginMetadata": {} }));
                let boxkf = g(&format!("boxkf{ai}_{bi}"));
                keyframes.push(json!({ "$id": boxkf, "symbol": boxsym, "length": span, "tweened": false, "tweenType": "LINEAR", "type": "COLLISION_BOX", "pluginMetadata": {} }));
                let boxlayer = g(&format!("boxlayer{ai}_{bi}"));
                layers.push(json!({ "$id": boxlayer, "name": format!("hitbox{bi}"), "type": "COLLISION_BOX", "hidden": false, "locked": false, "defaultAlpha": 0.5, "defaultColor": "0xff0000", "keyframes": [boxkf],
                    "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "collisionBoxType": "HIT_BOX", "index": bi } } }));
                anim_layers.push(boxlayer);
            }
        }
        animations.push(json!({ "$id": g(&format!("anim{ai}")), "name": a.name.clone(), "layers": anim_layers, "pluginMetadata": {} }));
    }
    let mut e = json!({
        "export": true, "guid": g("entity"), "id": hid, "version": 5,
        "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "objectType": "CUSTOM_GAME_OBJECT", "version": "0.4.0" } },
        "plugins": ["com.fraymakers.FraymakersTypes", "com.fraymakers.FraymakersMetadata"],
        "tags": [], "paletteMap": {}, "tilesets": [], "terrains": [],
        "symbols": symbols, "keyframes": keyframes, "layers": layers, "animations": animations
    });
    if let Some(kfs) = e.get_mut("keyframes").and_then(|k| k.as_array_mut()) {
        crate::entity_gen::double_keyframe_lengths(kfs);
    }
    e
}

/// Build the `LState` registration block + `_prepLocalState` helper for a hazard whose animations
/// are `anims` (registers each FM animation name as a local state, upper-cased; plus the standard
/// UNINITIALIZED sentinel). Shared by the thwomp + the generic animated-hazard scripts.
fn local_state_block(anims: &[HzAnim]) -> String {
    let mut regs = vec!["\tUNINITIALIZED: _prepLocalState(\"#n/a\", -1)".to_string()];
    for a in anims {
        regs.push(format!("\t{}: _prepLocalState(\"{}\")", a.name.to_ascii_uppercase(), a.name));
    }
    format!(
        "function _prepLocalState(animation:String, ?index:Int=Math.NaN):Int {{\n\
         \tif (!__hasInitLocalStateMachine) {{ Common.initLocalStateMachine(); __hasInitLocalStateMachine = true; }}\n\
         \tif (index != Math.NaN) {{ index = __localStatePrepIndex++; }}\n\
         \tCommon.registerLocalState(index, animation);\n\treturn index;\n}}\n\
         var __hasInitLocalStateMachine = false;\nvar __localStatePrepIndex = -1;\n\
         var LState = {{\n{}\n}};\n",
        regs.join(",\n"))
}

/// The Thwomp (converted from SSF2 `Thwomp` + `bowserscastle::update`): it cycles through the
/// stage's platform columns, accelerating down onto each (a gravity slam), holding, then rising
/// and moving to the next column. Its native HIT_BOX damages on contact, and the platform it lands
/// on detects it (a custom game object on its surface) and sinks, exactly as SSF2's thwomp calls
/// `platform.sink()`. `cols` are the platform `(x-center, standable-top-y)` pairs; the Thwomp
/// falls onto each in turn, landing at THAT platform's top (they sit at different heights).
fn thwomp_script_hx(cols: &[(f64, f64)]) -> String {
    let cols_lit = cols.iter().map(|(x, _)| format!("{x:.1}")).collect::<Vec<_>>().join(", ");
    let land_lit = cols.iter().map(|(_, y)| format!("{y:.1}")).collect::<Vec<_>>().join(", ");
    // rest off-screen above the stage (well above the highest platform top = smallest y), so the
    // Thwomp only appears as it drops in.
    let top_y = cols.iter().map(|(_, y)| *y).fold(f64::MAX, f64::min) - 520.0;
    format!(
        "// Thwomp (converted from SSF2). Falls onto a platform column -> that platform sinks; then\n\
         // rises and moves to the next column. Native HIT_BOX (HitboxStats) damages on contact.\n\n\
         function _prepLocalState(animation:String, ?index:Int=Math.NaN):Int {{\n\
         \tif (!__hasInitLocalStateMachine) {{ Common.initLocalStateMachine(); __hasInitLocalStateMachine = true; }}\n\
         \tif (index != Math.NaN) {{ index = __localStatePrepIndex++; }}\n\
         \tCommon.registerLocalState(index, animation);\n\treturn index;\n}}\n\
         var __hasInitLocalStateMachine = false;\nvar __localStatePrepIndex = -1;\n\
         var LState = {{\n\tUNINITIALIZED: _prepLocalState(\"#n/a\", -1),\n\tACTIVE: _prepLocalState(\"gameObjectIdle\"),\n\tINACTIVE: _prepLocalState(\"gameObjectInactive\")\n}};\n\n\
         var COLUMNS = [{cols_lit}];\nvar LAND_YS = [{land_lit}];\nvar TOP_Y = {top_y:.1};\n\
         // SSF2: the stage spawns a Thwomp every 300 frames @30fps (=600 @60fps) at a random column;\n\
         // it falls (gravity), lands (the platform sinks), waits, then rises off. We model one\n\
         // persistent Thwomp on that cadence: rest -> fall random column -> land -> wait -> rise.\n\
         var REST = 600;\nvar LAND_WAIT = 110;\nvar GRAV = 1.4;\nvar RISE = 8.0;\n\
         // persistent state (a plain var resets every frame on a custom game object).\n\
         var m_phase = self.makeInt(3);\nvar m_col = self.makeInt(0);\nvar m_fallV = self.makeFloat(0.0);\n\
         var m_timer = self.makeInt(0);\nvar m_cool = self.makeInt(0);\nvar m_init = self.makeBool(false);\n\n\
         function initialize() {{\n\tself.setState(PState.ACTIVE);\n\tCommon.toLocalState(LState.ACTIVE);\n}}\n\n\
         function update() {{\n\
         \tif (!m_init.get()) {{ m_init.set(true); m_col.set(Random.getInt(0, COLUMNS.length - 1)); self.setX(COLUMNS[m_col.get()]); self.setY(TOP_Y); }}\n\
         \t// keep the native hitbox live so it damages fighters it falls through.\n\
         \tif (m_cool.get() > 0) {{ m_cool.dec(); }} else {{ self.reactivateHitboxes(); m_cool.set(18); }}\n\
         \tvar p = m_phase.get();\n\
         \tif (p == 0) {{ // fall\n\
         \t\tm_fallV.set(m_fallV.get() + GRAV);\n\t\tself.setY(self.getY() + m_fallV.get());\n\
         \t\tif (self.getY() >= LAND_YS[m_col.get()]) {{ self.setY(LAND_YS[m_col.get()]); m_phase.set(1); m_timer.set(0); }}\n\
         \t}} else if (p == 1) {{ // landed (its platform sinks); wait\n\
         \t\tm_timer.set(m_timer.get() + 1);\n\t\tif (m_timer.get() >= LAND_WAIT) {{ m_phase.set(2); }}\n\
         \t}} else if (p == 2) {{ // rise back to the top\n\
         \t\tself.setY(self.getY() - RISE);\n\
         \t\tif (self.getY() <= TOP_Y) {{ self.setY(TOP_Y); m_phase.set(3); m_timer.set(0); m_fallV.set(0.0); }}\n\
         \t}} else {{ // rest above the stage, then drop on a fresh random column\n\
         \t\tm_timer.set(m_timer.get() + 1);\n\
         \t\tif (m_timer.get() >= REST) {{ m_col.set(Random.getInt(0, COLUMNS.length - 1)); self.setX(COLUMNS[m_col.get()]); m_phase.set(0); m_timer.set(0); }}\n\
         \t}}\n\
         }}\n")
}

/// The multi-animation Thwomp script — 1:1 from the SSF2 disasm (bowserscastle::update +
/// Thwomp::initialize/update), frame-doubled (×2 frames) and velocity-converted (×0.65 =
/// size 1.3 × half-rate):
///   stage:  spawnTimer 300f -> spawn at (random column, deathBounds top) -> waitTimer 300f
///           -> repeat  ⇒ one spawn every 600 SSF2 f = 1200 FM frames (recycled CGO).
///   thwomp: `entrance` anim, 60f hover (gravity 0)  ⇒ 120 FM frames at the spawn point;
///           then gravity 30 with max_ySpeed 30 ⇒ constant 30 px/f fall = 19.5 FM px/frame;
///           land -> `idle` anim + the column platform sinks, wait 90f ⇒ 180 FM;
///           rise at YSpeed -6 ⇒ 3.9 FM px/frame and despawn (here: hide + restart cycle).
/// Make a decompiled SSF2Enemy script runnable as an FM custom game object without erroring. The
/// decompiler gives the real `update()` state machine, but it references SSF2 instance fields
/// (`self.m_action`, FrameTimers) and SSF2-only calls an FM CGO can't run. This pass:
///   - detects the field driving the update() switch (the int `m_X` compared against the most integer
///     literals) and lowers it onto the engine's local-state subsystem: `initLocalStateMachine` +
///     `registerLocalState(value, anim)` per state, `m_X = N` -> `toLocalState(N)`, `m_X == N` ->
///     `inLocalState(N)`. the field name isn't assumed (SSF2 authors name it freely),
///   - models every other `self.m_X` field as FM persistent state (`self.makeInt`), a `FrameTimer(N)`
///     as a persistent frame counter (`.tick()`/`.completed`/`.reset()` -> counter ops),
///   - drops SSF2-only object-stat flags (bypassCollisionTesting/surviveDeathBounds) from
///     `updateGameObjectStats`,
///   - comments out any line that has no safe FM equivalent (a field bound to an `[SSF2-only]` call
///     like `createSelfPlatform`, plus `setCamBoxSize`/`addTarget`/`isOnFloor`/`AudioClip.play`/the
///     `createVfx` dust, which need verified resources) — conservatively, so it never throws.
/// `anims` are the entity's real animation names (the `forceAttack` labels) — a state is registered
/// against the label nearest its assignment, so `toLocalState` plays a clip that exists.
/// Returns the rewritten initialize()+update() body plus the persistent-state declarations.
/// The stage-side context the reconstruction can't get from the enemy class alone: SSF2 splits a
/// spawned hazard's lifecycle between the enemy class (the state machine `cgo_runnable` rebuilds)
/// and the STAGE class (the spawn machine: when/where it appears, when it's culled). These are the
/// same stepped values the template path uses.
struct ReconWrap {
    /// (x-center, landing-surface top y) per spawn column, FM coords.
    cols: Vec<(f64, f64)>,
    /// Park/spawn height (just inside the top blast bound).
    spawn_y: f64,
    /// Spawn-to-spawn period, FM frames (the stage spawn machine's full cycle).
    spawn_period: f64,
    /// Fall-speed cap (the class's getOwnStats maxYSpeed), FM px/frame.
    terminal: f64,
    /// The entrance clip's frame-script setYSpeed timeline, raw SSF2 (frame, speed).
    bob: Vec<(u32, f64)>,
    /// The class called addToCamera() — mirror with camera add/deleteTarget.
    camera: bool,
    /// FM name of the entrance animation (locates the entrance state for the bob gate).
    entrance_anim: String,
    /// The class's own createSelfPlatform box (raw SSF2 coords, relative to the body origin):
    /// lowered to runtime structures created at the call site.
    self_platform: Option<(f64, f64, f64, f64)>,
}

fn cgo_runnable(raw: &str, anims: &[String], scale: f64, wrap: Option<&ReconWrap>) -> String {
    use regex::Regex;
    use std::collections::{BTreeMap, BTreeSet};
    // FrameTimer fields -> duration expr; dead fields (bound to an [SSF2-only] call); int fields.
    let timer_re = Regex::new(r"self\.(m_\w+)\s*=\s*new FrameTimer\(([^;]+)\)").unwrap();
    let dead_re = Regex::new(r"//\s*\[SSF2-only:[^]]*\]\s*self\.(m_\w+)\s*=").unwrap();
    let field_re = Regex::new(r"self\.(m_\w+)").unwrap();

    let mut timers: BTreeMap<String, String> = BTreeMap::new();
    let mut dead: BTreeSet<String> = BTreeSet::new();
    for c in timer_re.captures_iter(raw) { timers.insert(c[1].to_string(), c[2].trim().to_string()); }
    for c in dead_re.captures_iter(raw) { dead.insert(c[1].to_string()); }
    // every other m_X (read or written, not a timer, not dead) becomes an int persistent slot.
    let mut ints: BTreeSet<String> = BTreeSet::new();
    for c in field_re.captures_iter(raw) {
        let f = c[1].to_string();
        if !timers.contains_key(&f) && !dead.contains(&f) { ints.insert(f); }
    }

    // STATE-MACHINE DETECTION: the int field compared against the most integer literals is the
    // update() switch discriminant (SSF2's `m_action`-style state var). its name isn't fixed.
    let cmp_re = Regex::new(r"self\.(m_\w+)\s*(?:==|!=)\s*(-?\d+)").unwrap();
    let mut cmp_counts: BTreeMap<String, usize> = BTreeMap::new();
    for c in cmp_re.captures_iter(raw) {
        if ints.contains(&c[1]) { *cmp_counts.entry(c[1].to_string()).or_default() += 1; }
    }
    let state_field = cmp_counts.into_iter().filter(|(_, n)| *n >= 1).max_by_key(|(_, n)| *n).map(|(f, _)| f);
    // the state values + the animation each plays. a value is registered against the forceAttack label
    // textually nearest its `= N` (initialize sets the entrance state next to forceAttack("entrance"),
    // a handler plays its anim next to its transition), constrained to a real entity animation.
    let mut state_regs: BTreeMap<i64, String> = BTreeMap::new();
    if let Some(st) = &state_field {
        ints.remove(st); // no makeInt slot — it lives in the local-state machine now.
        let raw_lines: Vec<&str> = raw.lines().collect();
        let label_re = Regex::new(r#"forceAttack\("([^"]+)"\)"#).unwrap();
        let asg_re = Regex::new(&format!(r"self\.{st}\s*=\s*(-?\d+)")).unwrap();
        let val_re = Regex::new(&format!(r"self\.{st}\s*(?:==|!=|=)\s*(-?\d+)")).unwrap();
        // a state plays the forceAttack label nearest its `= N` (constrained to a real entity anim).
        let mut labeled: BTreeMap<i64, String> = BTreeMap::new();
        for (i, line) in raw_lines.iter().enumerate() {
            let Some(c) = asg_re.captures(line) else { continue };
            let Ok(v) = c[1].parse::<i64>() else { continue };
            let lo = i.saturating_sub(3);
            let hi = (i + 4).min(raw_lines.len());
            let mut best: Option<(usize, String)> = None;
            for (j, near) in raw_lines.iter().enumerate().take(hi).skip(lo) {
                if let Some(lc) = label_re.captures(near) {
                    let lbl = lc[1].to_string();
                    if anims.iter().any(|a| a.eq_ignore_ascii_case(&lbl)) {
                        let d = i.abs_diff(j);
                        if best.as_ref().map(|(bd, _)| d < *bd).unwrap_or(true) { best = Some((d, lbl)); }
                    }
                }
            }
            if let Some((_, lbl)) = best { labeled.insert(v, lbl); }
        }
        // every value set or tested must be registered (toLocalState warns on an unregistered value).
        let mut all_vals: BTreeSet<i64> = BTreeSet::new();
        for c in val_re.captures_iter(raw) { if let Ok(v) = c[1].parse::<i64>() { all_vals.insert(v); } }
        // states SSF2 animates implicitly (no adjacent forceAttack, e.g. the gravity-on fall) get the
        // entity anims no labeled state claimed, handed out in state-value order; idle is the last resort.
        let claimed: BTreeSet<String> = labeled.values().map(|s| s.to_ascii_lowercase()).collect();
        let mut spare: Vec<String> = anims.iter().filter(|a| !claimed.contains(&a.to_ascii_lowercase())).cloned().collect();
        let default_anim = anims.iter().find(|a| a.eq_ignore_ascii_case("idle"))
            .or_else(|| anims.first()).cloned().unwrap_or_else(|| "idle".to_string());
        for v in all_vals {
            let (anim, guessed) = match labeled.get(&v) {
                Some(a) => (a.clone(), false),
                None if !spare.is_empty() => (spare.remove(0), true),
                None => (default_anim.clone(), true),
            };
            if guessed {
                // no forceAttack sat next to this state's assignment, so its animation is a guess (the
                // next unclaimed entity clip in value order, idle as the last resort). flag it so a
                // wrong-looking hazard animation is traceable to here, not assumed correct.
                eprintln!("  warning: hazard state {v} has no labelled animation; \
                           guessed \"{anim}\" by value order (verify the clip is right)");
            }
            state_regs.insert(v, anim);
        }
    }
    // state read/compare/assign -> local-state API (compiled once; the field name is fixed here).
    let state_lower = state_field.as_ref().map(|st| (
        Regex::new(&format!(r"self\.{st}\s*==\s*(-?\d+)")).unwrap(),
        Regex::new(&format!(r"self\.{st}\s*!=\s*(-?\d+)")).unwrap(),
        Regex::new(&format!(r"self\.{st}\s*=\s*(-?\d+)")).unwrap(),
        st.clone(),
    ));

    // isOnFloor/setYVelocity/getCamera/createVfx are REAL FM Entity APIs (fraymakers-api-docs
    // Entity.md) and pass through 1:1 — only their VALUES get unit-converted below. isOnGround
    // normalizes to the FM spelling. setCamBoxSize has no FM port (the camera frames its targets
    // itself), setFallthrough is ported by the deck-structure follower, and sounds lack resources.
    let risky_expr: &[(&str, &str)] = &[("self.isOnGround()", "self.isOnFloor()")];
    let risky = ["setCamBoxSize", "AudioClip.play", "setFallthrough"];
    // physics intents -> the scripted kinematics integrator (the wrapper appends the step). units:
    // a 30fps per-frame velocity halves (x scale x 0.5); a per-frame^2 acceleration quarters.
    let grav_re = Regex::new(r"^(\s*)self\.updateGameObjectStats\(\{ gravity: (-?\d+(?:\.\d+)?) \}\);").unwrap();
    let yvel_re = Regex::new(r"^(\s*)self\.setYVelocity\((-?\d+(?:\.\d+)?)\);").unwrap();
    let shake_re = Regex::new(r"\.shake\((-?\d+(?:\.\d+)?)\)").unwrap();
    let vfx_scale_re = Regex::new(r"(scaleX|scaleY): (-?\d+(?:\.\d+)?)").unwrap();
    let fallthrough_re = Regex::new(r"^(\s*)self\.m_\w+\.setFallthrough\((true|false)\);").unwrap();

    let mut out: Vec<String> = Vec::new();
    for line in raw.lines() {
        let trimmed = line.trim_start();
        let indent = &line[..line.len() - trimmed.len()];
        // already-commented decompiler gaps pass through untouched — except the ground-detach,
        // which has a 1:1 FM equivalent (Entity.unattachFromFloor).
        if trimmed.starts_with("//") {
            if trimmed.contains("[SSF2-only: unnattachFromGround]") {
                out.push(format!("{indent}self.unattachFromFloor(); // SSF2 unnattachFromGround"));
            } else if trimmed.contains("[SSF2-only: createSelfPlatform]")
                && wrap.is_some_and(|w| w.self_platform.is_some()) {
                out.push(format!("{indent}__createSelfPlatform(); // SSF2 createSelfPlatform: deck + ceiling structures (helpers below)"));
            } else {
                out.push(line.to_string());
            }
            continue;
        }
        // FrameTimer construction: the counter IS the state (starts at 0); drop the `new`.
        if timer_re.is_match(line) { out.push(format!("{indent}// FrameTimer construction -> the module-scope makeFrameTimer")); continue; }
        // the self-platform's fallthrough toggle lowers natively: SSF2 fallthrough(true) means
        // riders pass through entirely, which is FM's `disabled` stat (dropThrough would only
        // make it a soft platform).
        if wrap.is_some_and(|w| w.self_platform.is_some()) {
            if let Some(c) = fallthrough_re.captures(line) {
                out.push(format!("{}__selfPlatformDisabled({}); // SSF2 setFallthrough({})", &c[1], &c[2], &c[2]));
                continue;
            }
        }
        // a line touching a dead field or a risky unmapped call: neutralize.
        if dead.iter().any(|f| line.contains(&format!("self.{f}")))
            || risky.iter().any(|r| line.contains(r)) {
            out.push(format!("{indent}// [needs-port] {trimmed}"));
            continue;
        }
        let mut s = line.to_string();
        // risky query in a condition -> safe literal (keeps braces balanced).
        for (call, lit) in risky_expr { s = s.replace(call, lit); }
        // the state field -> local-state machine (before the generic int rewrite; it's not in `ints`).
        // the local-state methods live on EntityScriptCommon (the script's own base class), so they're
        // called BARE — `self.` resolves the api object, which doesn't carry them ("Invalid function null").
        if let Some((eq, ne, asg, st)) = &state_lower {
            s = asg.replace_all(&s, "toLocalState($1)").to_string();
            s = eq.replace_all(&s, "inLocalState($1)").to_string();
            s = ne.replace_all(&s, "!inLocalState($1)").to_string();
            s = s.replace(&format!("self.{st}"), "getLocalState()");
        }
        // timer ops lower in a post-flatten pass (the frames-in-state form needs the flat ladder
        // to PROVE per timer that all its accesses live in one state handler). without the
        // wrapper there's no frames-in-state counter, so lower onto per-slot counters here.
        // a FrameTimer duration is 30fps frames; the counters tick at 60fps, so x2.
        if wrap.is_none() {
            for f in timers.keys() {
                s = s.replace(&format!("self.{f}."), &format!("_t_{f}."));
            }
        }
        // int field write/read -> persistent get/set.
        for f in &ints {
            let wre = Regex::new(&format!(r"self\.{f}\s*=\s*")).unwrap();
            s = wre.replace_all(&s, format!("_s_{f}.__SET__ ")).to_string();
            s = s.replace(&format!("self.{f}"), &format!("_s_{f}.get()"));
        }
        // updateGameObjectStats: drop SSF2-only flags.
        s = s.replace(", bypassCollisionTesting: false", "").replace(", bypassCollisionTesting: true", "")
             .replace(", surviveDeathBounds: false", "").replace(", surviveDeathBounds: true", "");
        // SSF2 physics calls stay NATIVE (the engine integrates gravity/velocity and grounds the
        // object, so isOnFloor() is the real engine test) — only the VALUES convert from 30fps:
        // acceleration x scale/4, velocity x scale/2; the class's maxYSpeed caps as terminalVelocity.
        if let Some(c) = grav_re.captures(&s) {
            let g: f64 = c[2].parse().unwrap_or(0.0);
            let term = wrap.filter(|_| g > 0.0)
                .map(|w| format!(", terminalVelocity: {:.2}", w.terminal)).unwrap_or_default();
            s = format!("{}self.updateGameObjectStats({{ gravity: {:.2}{} }}); // SSF2 gravity {} @30fps",
                &c[1], g * scale * 0.25, term, g);
        } else if let Some(c) = yvel_re.captures(&s) {
            let v: f64 = c[2].parse().unwrap_or(0.0);
            s = format!("{}self.setYVelocity({:.2}); // SSF2 setYSpeed {} @30fps", &c[1], v * scale * 0.5, v);
        }
        // screen-space args (camera shake amplitude, the dust vfx scale) x the stage scale.
        if let Some(c) = shake_re.captures(&s) {
            let a: f64 = c[1].parse().unwrap_or(0.0);
            s = shake_re.replace(&s, format!(".shake({:.1})", a * scale).as_str()).to_string();
        }
        if s.contains("createVfx") {
            s = vfx_scale_re.replace_all(&s, |c: &regex::Captures| {
                format!("{}: {:.1}", &c[1], c[2].parse::<f64>().unwrap_or(1.0) * scale)
            }).to_string();
        }
        out.push(s);
    }
    let mut body = out.join("\n");
    // finalize the write marker into a .set(...) wrapping the rest of the statement.
    let set_re = Regex::new(r"_s_(m_\w+)\.__SET__ (.*);").unwrap();
    body = set_re.replace_all(&body, "_s_$1.set($2);").to_string();

    // with the spawn wrapper, the decompiled lifecycle becomes inner machinery: the wrapper owns
    // the engine entry points and re-runs __hazardInit on every spawn-cycle engage. (renaming
    // BEFORE the state registration below routes the registrations to the one-time module-scope
    // guard instead of re-registering per engage.)
    if wrap.is_some() {
        body = body.replace("function initialize() {", "function __hazardInit() {")
                   .replace("function update() {", "function __hazardUpdate() {");
    }

    // register the local-state machine + each state's animation at the top of initialize() — before the
    // decompiled body's first toLocalState, and once (initialize runs once per spawn). a hazard always
    // has initialize(); if a future projectile path lacks one, guard a module-scope init instead.
    // (with the wrapper, registration moves into the SYNTHESIZED initialize below — a make* slot
    // dereferenced at module scope null-accesses on the first eval, before the CGO api is live.)
    if !state_regs.is_empty() && wrap.is_none() {
        let mut reg = String::from("\n\tinitLocalStateMachine();");
        for (v, anim) in &state_regs { reg.push_str(&format!("\n\tregisterLocalState({v}, \"{anim}\");")); }
        let init_re = Regex::new(r"function initialize\(\) \{").unwrap();
        if init_re.is_match(&body) {
            body = init_re.replace(&body, format!("function initialize() {{{reg}").as_str()).to_string();
        } else {
            body = format!("var _sm_init = self.makeBool(false);\nif (!_sm_init.get()) {{\n\t_sm_init.set(true);{reg}\n}}\n\n{body}");
        }
    }

    // persistent-state declarations at MODULE scope, exactly like the verified template: the engine's
    // makeInt(default) returns a call-order-keyed persistent store, so the script re-running each frame
    // just rebinds the same var to the same store (default applied once). functions close over these by
    // bare name — never re-call makeInt inside a function (that would shift the call-order keying).
    let mut preamble = String::new();
    for f in &ints { preamble.push_str(&format!("var _s_{f} = self.makeInt(0);\n")); }
    // FrameTimers lower 1:1 onto the engine's own makeFrameTimer (the tick/completed/reset
    // vocabulary is identical; a duration is 30fps frames, so x2). The timer counts UP to its
    // length, exactly like SSF2's.
    for (f, dur) in &timers {
        preamble.push_str(&format!("var _t_{f} = self.makeFrameTimer(({dur}) * 2);\n"));
    }
    let mut body = flatten_state_ladder(&body);
    let Some(w) = wrap else { return format!("{preamble}\n{body}") };

    // wrap path: the same 1:1 FrameTimer lowering (the engage below resets each timer, the
    // fresh-instance semantics SSF2 gets by constructing new timers per spawn).
    for f in timers.keys() {
        body = body.replace(&format!("self.{f}."), &format!("_t_{f}."));
    }

    // the update() switch values become named states: SSF2 inlines the constants, so the name is
    // derived from the animation each state registers (the forceAttack label its handler plays).
    let mut state_consts: std::collections::BTreeMap<i64, String> = std::collections::BTreeMap::new();
    {
        let mut used: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for (v, anim) in &state_regs {
            let base: String = anim.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
            let n = used.entry(base.clone()).or_default();
            *n += 1;
            let name = if *n == 1 { format!("ST_{base}") } else { format!("ST_{base}_{n}") };
            state_consts.insert(*v, name);
        }
        let st_re = Regex::new(r"(toLocalState|inLocalState)\((-?\d+)\)").unwrap();
        body = st_re.replace_all(&body, |c: &regex::Captures| {
            let v: i64 = c[2].parse().unwrap();
            match state_consts.get(&v) {
                Some(n) => format!("{}({})", &c[1], n),
                None => c[0].to_string(),
            }
        }).to_string();
    }
    let cname = |v: &i64| state_consts.get(v).cloned().unwrap_or_else(|| v.to_string());
    let mut const_decls = String::new();
    for (v, name) in &state_consts { const_decls.push_str(&format!("var {name} = {v};\n")); }

    // ---- the synthesized stage spawn cycle + kinematics integrator ----
    // SSF2 splits this hazard across two classes: the enemy class (the state machine above) and
    // the stage class (spawn cadence/columns, the death-bounds cull). The wrapper rebuilds the
    // stage half around the decompiled enemy half, plus the parts the SSF2 ENGINE did implicitly:
    // gravity/yspeed integration with the maxYSpeed cap, landing on the column surface, and the
    // entrance clip's frame-script setYSpeed timeline (a Graphic-pinned clip never self-runs).
    let cols_lit = w.cols.iter().map(|(x, _)| format!("{x:.1}")).collect::<Vec<_>>().join(", ");
    let land_lit = w.cols.iter().map(|(_, y)| format!("{y:.1}")).collect::<Vec<_>>().join(", ");
    // every class timer restarts on engage (a fresh SSF2 instance constructs fresh timers).
    let timer_resets: String = timers.keys()
        .map(|f| format!("\t\t\t_t_{f}.reset();\n")).collect();
    // state registration runs once, behind the wrapper's first-update guard. NOT module scope
    // (make* slots aren't dereferenceable on the first eval, before the CGO api is live) and NOT
    // initialize() (the engine never calls it on a stage custom game object — the template's
    // m_init-in-update guard is the proven pattern).
    let mut state_reg = String::new();
    if !state_regs.is_empty() {
        state_reg.push_str("var __hasInitLocalStateMachine = false;\n\
                            if (!__hasInitLocalStateMachine) {\n\
                            \tinitLocalStateMachine();\n\
                            \t__hasInitLocalStateMachine = true;\n\
                            }\n");
        for (v, anim) in &state_regs {
            state_reg.push_str(&format!("registerLocalState({}, \"{anim}\");\n", cname(v)));
        }
    }
    // the class's own addToCamera (inside __hazardInit) fires per engage; the wrapper only needs
    // the matching deleteTarget at the recycle (SSF2's death-bounds cull dropped the target).
    let cam_del = if w.camera { "\t\tmatch.getCamera().deleteTarget(self);\n" } else { "" };
    // SSF2 createSelfPlatform lowering: the body's solid box as runtime structures created at
    // the class's own call site. hscript can't hold object handles across frames (make* slots
    // are scalars), so the structures' indices in match.getStructures() are stashed instead.
    let (sp_decls, sp_helpers, sp_follow, sp_park) = match w.self_platform {
        Some((bx, by, bw, bh)) => {
            let (l, t, r, b) = (bx * scale, by * scale, (bx + bw) * scale, (by + bh) * scale);
            (
                "var _sp_deck = self.makeInt(-1);\n".to_string(),
                format!(
                    "function __createSelfPlatform() {{\n\
                     \tif (_sp_deck.get() >= 0) {{ return; }}\n\
                     \tvar n = match.getStructures().length;\n\
                     \t// the solid body outline, AUTO-typed: the engine classifies each face by its\n\
                     \t// orientation (top = floor, steep sides = walls, underside = ceiling).\n\
                     \tmatch.createLineSegmentStructure([{l:.1}, {t:.1}, {r:.1}, {t:.1}, {r:.1}, {b:.1}, {l:.1}, {b:.1}, {l:.1}, {t:.1}], new StructureStats({{ startX: -2000, startY: -3000, structureType: StructureType.AUTO, leftLedge: false, rightLedge: false }}));\n\
                     \t_sp_deck.set(n);\n\
                     \t// the body never grounds on its own platform (SSF2 self-platform semantics)\n\
                     \tmatch.getStructures()[_sp_deck.get()].addToBlacklist(self);\n\
                     }}\n\n\
                     function __selfPlatformDisabled(b:Bool) {{\n\
                     \tif (_sp_deck.get() < 0) {{ return; }}\n\
                     \tmatch.getStructures()[_sp_deck.get()].updateStructureStats({{ disabled: b }});\n\
                     }}\n\n"),
                "\t// the self-platform rides the body (SSF2 moves it with the enemy)\n\
                 \tif (_sp_deck.get() >= 0) {\n\
                 \t\tmatch.getStructures()[_sp_deck.get()].setX(self.getX());\n\
                 \t\tmatch.getStructures()[_sp_deck.get()].setY(self.getY());\n\
                 \t}\n".to_string(),
                "\t\t// park the self-platform off-world (carrying a rider out = KO, the SSF2 outcome)\n\
                 \t\tif (_sp_deck.get() >= 0) {\n\
                 \t\t\tmatch.getStructures()[_sp_deck.get()].setX(-2000);\n\
                 \t\t\tmatch.getStructures()[_sp_deck.get()].setY(-3000);\n\
                 \t\t}\n".to_string(),
            )
        }
        None => (String::new(), String::new(), String::new(), String::new()),
    };
    // the bob plays during the entrance state — locate it by its registered animation. its keys
    // ride the frames-in-state counter (the entrance clip starts when the state does).
    let bob_block = state_regs.iter()
        .find(|(_, a)| a.eq_ignore_ascii_case(&w.entrance_anim))
        .filter(|_| !w.bob.is_empty())
        .map(|(v, _)| {
            let ladder: String = w.bob.iter()
                .map(|(f, sp)| format!("\t\tif (_w_state_t.get() == {}) {{ self.setYVelocity({:.2}); }}\n",
                    (f.saturating_sub(1)) * 2, sp * scale * 0.5))
                .collect();
            format!("\t// entrance bob: the entrance sub-clip's frame scripts (setYSpeed timeline, 30->60fps)\n\
                     \tif (inLocalState({})) {{\n{ladder}\t}}\n", cname(v))
        })
        .unwrap_or_default();
    // the bob ladder is the one consumer of a frames-in-state clock (its keys are frame indices,
    // which a FrameTimer's completed flag can't address) — only emit the tracker when it's used.
    let (bob_decls, bob_tracker) = if bob_block.is_empty() { ("", "") } else { (
        "var _w_prev = self.makeInt(-99);\nvar _w_state_t = self.makeInt(0);\n",
        "\t// frames-in-state clock for the entrance bob ladder\n\
         \tif (getLocalState() != _w_prev.get()) {\n\
         \t\t_w_prev.set(getLocalState());\n\
         \t\t_w_state_t.set(0);\n\
         \t} else {\n\
         \t\t_w_state_t.inc();\n\
         \t}\n",
    ) };
    let script = format!(
        "{const_decls}\
         // the local-state machine inits + registers at MODULE scope on every eval, the\n\
         // proven template idiom.\n\
         {state_reg}\
         {preamble}\n\
         // stage spawn machine constants (stepped from the stage + enemy classes)\n\
         var COLUMNS = [{cols_lit}];\nvar LAND_YS = [{land_lit}];\n\
         var SPAWN_Y = {spawn_y:.1};\nvar SPAWN_PERIOD = {spawn_period:.0};\n\
         var _w_init = self.makeBool(false);\n\
         var _w_active = self.makeBool(false);\nvar _w_col = self.makeInt(0);\n\
         // the stage's spawn machine (one full spawn-to-spawn period) + the rehit cadence\n\
         var _w_clock = self.makeFrameTimer(SPAWN_PERIOD);\nvar _w_cool = self.makeFrameTimer(60);\n\
         {bob_decls}{sp_decls}\n\
         {sp_helpers}\
         {body}\n\n\
         function update() {{\n\
         \t// one-time setup on the first update: the stage script positions this object right\n\
         \t// after createCustomGameObject (initialize() runs inside the create call, so a park\n\
         \t// set there would be overridden), and make* slots aren't live at module scope.\n\
         \tif (!_w_init.get()) {{\n\
         \t\t_w_init.set(true);\n\
         \t\tself.setState(PState.ACTIVE);\n\
         \t\tself.setX(COLUMNS[0]);\n\
         \t\tself.setY(SPAWN_Y);\n\
         \t\t// SSF2's first spawn lands half a period in: pre-advance the spawn clock.\n\
         \t\tfor (i in 0...{half_period:.0}) {{ _w_clock.tick(); }}\n\
         \t}}\n\
         \t// spawn-to-spawn clock (the stage spawn machine runs regardless of the enemy's phase)\n\
         \t_w_clock.tick();\n\
         \tif (!_w_active.get()) {{\n\
         \t\tif (_w_clock.completed) {{\n\
         \t\t\t_w_clock.reset();\n\
         \t\t\t_w_col.set(Random.getInt(0, COLUMNS.length - 1));\n\
         \t\t\tself.setX(COLUMNS[_w_col.get()]);\n\
         \t\t\tself.setY(SPAWN_Y);\n\
         \t\t\tself.setYVelocity(0);\n\
         \t\t\tself.updateGameObjectStats({{ gravity: 0 }});\n\
         {timer_resets}\
         \t\t\t__hazardInit();\n\
         \t\t\t_w_active.set(true);\n\
         \t\t}}\n\
         \t\treturn;\n\
         \t}}\n\
         {bob_tracker}\
         \t_w_cool.tick();\n\
         \tif (_w_cool.completed) {{ self.reactivateHitboxes(); _w_cool.reset(); }}\n\
         {bob_block}\
         \t__hazardUpdate();\n\
         {sp_follow}\
         \t// SSF2 culls it past the death bounds (surviveDeathBounds=false); recycle for the next spawn\n\
         \tif (self.getYVelocity() < 0 && self.getY() <= SPAWN_Y) {{\n\
         \t\tself.setY(SPAWN_Y);\n\
         \t\tself.setYVelocity(0);\n\
         {cam_del}\
         {sp_park}\
         \t\t_w_active.set(false);\n\
         \t}}\n\
         }}\n",
        spawn_y = w.spawn_y, spawn_period = w.spawn_period,
        half_period = w.spawn_period / 2.0);
    // the local-state subsystem is reached through the live-verified `Common.` binding (the bare
    // names are unproven in a CGO scope; one pass over the assembled script keeps the lowering,
    // the flattener, and the wrapper all working in bare-name terms).
    let mut script = script;
    for f in ["initLocalStateMachine(", "registerLocalState(", "toLocalState(", "inLocalState(", "getLocalState("] {
        script = script.replace(f, &format!("Common.{f}"));
    }
    script
}

/// Expand single-line statement blocks (`if (x) { a; b; }`, `} else { c; }`) into multi-line form
/// so every emitted script reads like hand-written code. Only a `{` preceded by `)`/`else`/`{`
/// with a top-level `;` inside is a statement block — object literals (`{ gravity: 0 }`) stay
/// inline. Applied to every script the stage emitter writes.
fn expand_blocks(code: &str) -> String {
    fn expand_line(line: &str) -> Option<Vec<String>> {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") { return None; }
        let indent: String = line.chars().take_while(|c| *c == '\t').collect();
        let chars: Vec<char> = line.chars().collect();
        for i in 0..chars.len() {
            if chars[i] != '{' { continue; }
            let before: String = chars[..i].iter().collect();
            let b = before.trim_end();
            if !(b.ends_with(')') || b.ends_with("else") || b.ends_with('{')) { continue; }
            // matching close on this line?
            let (mut d, mut j) = (1i32, i + 1);
            while j < chars.len() {
                match chars[j] { '{' => d += 1, '}' => d -= 1, _ => {} }
                if d == 0 { break; }
                j += 1;
            }
            if j >= chars.len() { continue; }
            let inner: String = chars[i + 1..j].iter().collect();
            if inner.trim().is_empty() { continue; }
            // statements = a `;` at top level inside the block
            let mut dd = 0i32;
            let mut has_stmt = false;
            for c in inner.chars() {
                match c { '{' | '(' => dd += 1, '}' | ')' => dd -= 1, ';' if dd == 0 => has_stmt = true, _ => {} }
            }
            if !has_stmt { continue; }
            let mut stmts: Vec<String> = Vec::new();
            let mut cur = String::new();
            dd = 0;
            for c in inner.chars() {
                match c {
                    '{' | '(' => { dd += 1; cur.push(c); }
                    '}' | ')' => { dd -= 1; cur.push(c); }
                    ';' if dd == 0 => { stmts.push(cur.trim().to_string()); cur.clear(); }
                    _ => cur.push(c),
                }
            }
            if !cur.trim().is_empty() { stmts.push(cur.trim().to_string()); }
            let head: String = chars[..=i].iter().collect();
            let tail: String = chars[j + 1..].iter().collect::<String>().trim_start().to_string();
            let mut out = vec![head.trim_end().to_string()];
            for st in stmts.into_iter().filter(|st| !st.is_empty()) {
                out.push(format!("{indent}\t{st};"));
            }
            if tail.is_empty() { out.push(format!("{indent}}}")); }
            else { out.push(format!("{indent}}} {tail}")); }
            return Some(out);
        }
        None
    }
    let mut lines: Vec<String> = code.lines().map(str::to_string).collect();
    loop {
        let mut changed = false;
        let mut next: Vec<String> = Vec::with_capacity(lines.len());
        for line in &lines {
            match expand_line(line) {
                Some(ex) => { next.extend(ex); changed = true; }
                None => next.push(line.clone()),
            }
        }
        lines = next;
        if !changed { break; }
    }
    lines.join("\n")
}

/// Write an emitted hscript file, expanded to one statement per line.
fn write_script(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    std::fs::write(path, expand_blocks(body))
}

/// Flatten the decompiler's nested-NEGATION state ladder into the canonical substate shape:
/// `if (!inLocalState(A)) { <deeper ladder> } else { <A handler> }` chains (the natural CFG of a
/// decompiled switch) become the flat banner form
/// `if (inLocalState(A)) { <A> } else if (inLocalState(B)) { <B> } ...`.
/// The deepest rung's fallthrough tail (the all-states-missed `return;`) is dropped. Operates on
/// lines; brace depth skips comment lines (the [needs-port] markers carry unbalanced braces).
fn flatten_state_ladder(code: &str) -> String {
    use regex::Regex;
    let open_re = Regex::new(r"^\t*if \(!inLocalState\((-?\d+)\)\) \{$").unwrap();
    let lines: Vec<String> = code.lines().map(|l| l.to_string()).collect();
    let Some(start) = lines.iter().position(|l| open_re.is_match(l)) else { return code.to_string() };
    // char-wise rung scan from the `if (!inLocalState(N)) {` line: the if-block's close brace can
    // share a line with the else opener (`} else {`), so depth is tracked per character. Returns
    // (if-close line, else-close line if an else block follows).
    let scan_rung = |open: usize| -> Option<(usize, Option<usize>)> {
        let mut depth = 0i32;
        let mut if_close: Option<usize> = None;
        for (j, l) in lines.iter().enumerate().skip(open) {
            if l.trim_start().starts_with("//") { continue; }
            for ch in l.chars() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            match if_close {
                                None => {
                                    if !l.contains("else") { return Some((j, None)); }
                                    if_close = Some(j); // the trailing `{` re-opens to depth 1
                                }
                                Some(ic) => return Some((ic, Some(j))),
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    };
    // walk the ladder: rung at `i` yields (state, handler line range), recursing into the if-block.
    let mut pairs: Vec<(String, Vec<String>)> = Vec::new();
    let mut i = start;
    let mut outer_end = start; // line AFTER the outermost rung (set on the first iteration)
    let mut first = true;
    while let Some(c) = open_re.captures(&lines[i]) {
        let state = c[1].to_string();
        let Some((if_close, else_close)) = scan_rung(i) else { return code.to_string() };
        let (handler, after): (Vec<String>, usize) = match else_close {
            Some(ec) => (lines[if_close + 1..ec].to_vec(), ec + 1),
            None => (Vec::new(), if_close + 1),
        };
        if first { outer_end = after; first = false; }
        pairs.push((state, handler));
        // descend into the if-block: the next rung is its first non-empty line.
        let inner_first = (i + 1..if_close).find(|&j| !lines[j].trim().is_empty());
        match inner_first {
            Some(j) if open_re.is_match(&lines[j]) => { i = j; }
            _ => break,
        }
    }
    if pairs.len() < 2 { return code.to_string(); }
    let indent = lines[start].len() - lines[start].trim_start().len();
    let ind = "\t".repeat(indent);
    let mut flat = vec![format!("{ind}// --------- SUBSTATE SYSTEM ----------")];
    for (k, (state, handler)) in pairs.iter().enumerate() {
        let kw = if k == 0 { format!("{ind}if") } else { format!("{ind}}} else if") };
        flat.push(format!("{kw} (inLocalState({state})) {{"));
        // re-indent the handler to one level under the branch.
        let min_tabs = handler.iter().filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start_matches('\t').len()).min().unwrap_or(0);
        for l in handler {
            if l.trim().is_empty() { flat.push(String::new()); continue; }
            let stripped = if l.len() >= min_tabs { &l[min_tabs..] } else { l.trim_start() };
            flat.push(format!("{ind}\t{stripped}"));
        }
    }
    flat.push(format!("{ind}}}"));
    let mut out: Vec<String> = lines[..start].to_vec();
    out.extend(flat);
    out.extend(lines[outer_end..].iter().cloned());
    out.join("\n")
}

/// Cross-frame state via `self.make*` (a plain `var` re-inits every frame on a game object).
#[allow(clippy::too_many_arguments)]
fn thwomp_multi_script_hx(cols: &[(f64, f64)], spawn_y: f64, shake_amp: f64, fall_v: f64, rise_v: f64, dust_scale: f64, spawn_period: f64, entrance_t: f64, land_wait: f64, entrance_bob: &str, camera: bool, entrance: &str, idle: &str, fall: &str) -> String {
    // camera targeting (SSF2 addToCamera/setCamBoxSize): the camera includes the body while it's
    // engaged; FM's camera frames targets itself, so only add/delete need porting.
    let cam_add = if camera { "\t\t\tmatch.getCamera().addTarget(self);\n" } else { "" };
    let cam_del = if camera { "\t\t\tmatch.getCamera().deleteTarget(self);\n" } else { "" };
    let bob = if entrance_bob.is_empty() { String::new() } else {
        format!("\t\t// entrance bob: the sub-clip's frame scripts (setYSpeed timeline), 30->60fps\n{entrance_bob}\t\tself.setY(self.getY() + m_vy.get());\n")
    };
    let cols_lit = cols.iter().map(|(x, _)| format!("{x:.1}")).collect::<Vec<_>>().join(", ");
    let land_lit = cols.iter().map(|(_, y)| format!("{y:.1}")).collect::<Vec<_>>().join(", ");
    format!(
        "// Thwomp — 1:1 from the SSF2 disasm (stage spawn cycle + Thwomp class), frame-doubled.\n\
         // Native HIT_BOXes (HitboxStats: two half boxes, angles 135/45) damage on contact.\n\n\
         function _prepLocalState(animation:String, ?index:Int=Math.NaN):Int {{\n\
         \tif (!__hasInitLocalStateMachine) {{ Common.initLocalStateMachine(); __hasInitLocalStateMachine = true; }}\n\
         \tif (index != Math.NaN) {{ index = __localStatePrepIndex++; }}\n\
         \tCommon.registerLocalState(index, animation);\n\treturn index;\n}}\n\
         var __hasInitLocalStateMachine = false;\nvar __localStatePrepIndex = -1;\n\
         // one local state per PHASE (several share an animation); toLocalState swaps the anim.\n\
         var LState = {{\n\
         \tUNINITIALIZED: _prepLocalState(\"#n/a\", -1),\n\
         \tREST: _prepLocalState(\"{entrance}\"),\n\
         \tENTRANCE: _prepLocalState(\"{entrance}\"),\n\
         \tFALL: _prepLocalState(\"{fall}\"),\n\
         \tLANDED: _prepLocalState(\"{idle}\"),\n\
         \tRISE: _prepLocalState(\"{idle}\")\n\
         }};\n\n\
         // SSF2 constants, frame-doubled / velocity-converted (see the header comment).\n\
         var COLUMNS = [{cols_lit}];\nvar LAND_YS = [{land_lit}];\nvar SPAWN_Y = {spawn_y:.1};\n\
         var SPAWN_PERIOD = {spawn_period:.0};\nvar ENTRANCE_T = {entrance_t:.0};\nvar FALL_V = {fall_v:.2};\nvar LAND_WAIT = {land_wait:.0};\nvar RISE_V = {rise_v:.2};\n\
         // persistent state (a plain var resets every frame on a custom game object).\n\
         var m_col = self.makeInt(0);\nvar m_timer = self.makeInt(0);\nvar m_vy = self.makeFloat(0.0);\n\
         var m_cycle = self.makeFrameTimer(SPAWN_PERIOD);\nvar m_cool = self.makeFrameTimer(60);\n\
         var m_init = self.makeBool(false);\n\n\
         function initialize() {{\n\tself.setState(PState.ACTIVE);\n\tCommon.toLocalState(LState.REST);\n}}\n\n\
         function update() {{\n\
         \t// match start: park at the spawn point; SSF2's first spawn lands at t=300f (=600 FM),\n\
         \t// so pre-advance the spawn clock by half a period.\n\
         \tif (!m_init.get()) {{\n\
         \t\tm_init.set(true);\n\
         \t\tself.setX(COLUMNS[0]);\n\
         \t\tself.setY(SPAWN_Y);\n\
         \t\tfor (i in 0...Math.floor(SPAWN_PERIOD / 2)) {{ m_cycle.tick(); }}\n\
         \t}}\n\
         \tm_cool.tick();\n\
         \tif (m_cool.completed) {{\n\
         \t\tself.reactivateHitboxes();\n\
         \t\tm_cool.reset();\n\
         \t}}\n\
         \t// spawn-to-spawn clock: SSF2 spawns every 600f (=1200 FM) regardless of phase timing.\n\
         \tm_cycle.tick();\n\n\
         \t// --------- SUBSTATE SYSTEM ----------\n\
         \tif (Common.inLocalState(LState.REST)) {{\n\
         \t\t// resting between spawns (parked at the spawn point above the stage)\n\
         \t\tif (m_cycle.completed) {{\n\
         \t\t\tm_col.set(Random.getInt(0, COLUMNS.length - 1));\n\
         \t\t\tself.setX(COLUMNS[m_col.get()]);\n\
         \t\t\tself.setY(SPAWN_Y);\n\
         \t\t\tm_timer.set(0);\n\
         \t\t\tm_cycle.reset();\n\
         \t\t\tm_vy.set(0);\n\
{cam_add}\
         \t\t\tCommon.toLocalState(LState.ENTRANCE);\n\
         \t\t}}\n\
         \t}} else if (Common.inLocalState(LState.ENTRANCE)) {{\n\
{bob}\
         \t\tm_timer.inc();\n\
         \t\tif (m_timer.get() >= ENTRANCE_T) {{\n\
         \t\t\tCommon.toLocalState(LState.FALL);\n\
         \t\t}}\n\
         \t}} else if (Common.inLocalState(LState.FALL)) {{\n\
         \t\t// constant terminal velocity (SSF2 gravity 30 capped at 30)\n\
         \t\tself.setY(self.getY() + FALL_V);\n\
         \t\tif (self.getY() >= LAND_YS[m_col.get()]) {{\n\
         \t\t\tself.setY(LAND_YS[m_col.get()]);\n\
         \t\t\tm_timer.set(0);\n\
         \t\t\tCommon.toLocalState(LState.LANDED);\n\
         \t\t\tmatch.getCamera().shake({shake_amp:.1});\n\
         \t\t\tmatch.createVfx(new VfxStats({{\n\
         \t\t\t\tspriteContent: \"global::vfx.vfx\",\n\
         \t\t\t\tanimation: GlobalVfx.DUST_POOF,\n\
         \t\t\t\tscaleX: {dust_scale:.1},\n\
         \t\t\t\tscaleY: {dust_scale:.1}\n\
         \t\t\t}}), self);\n\
         \t\t}}\n\
         \t}} else if (Common.inLocalState(LState.LANDED)) {{\n\
         \t\t// the column platform under it sinks; hold (SSF2 waitTimer 90f)\n\
         \t\tm_timer.inc();\n\
         \t\tif (m_timer.get() >= LAND_WAIT) {{\n\
         \t\t\t// LANDED and RISE share an animation, and toLocalState REPLAYS the registered\n\
         \t\t\t// animation from frame 0 — which restarted `idle` mid-playback the moment the\n\
         \t\t\t// thwomp left the ground. SSF2 changes no attack here, so the clip just keeps\n\
         \t\t\t// running; carry the frame across the phase change to match it.\n\
         \t\t\tvar _f = self.getCurrentFrame();\n\
         \t\t\tCommon.toLocalState(LState.RISE);\n\
         \t\t\tself.playFrame(_f);\n\
         \t\t}}\n\
         \t}} else if (Common.inLocalState(LState.RISE)) {{\n\
         \t\t// rise at SSF2 YSpeed -6 until past the spawn point, then rest\n\
         \t\tself.setY(self.getY() - RISE_V);\n\
         \t\tif (self.getY() <= SPAWN_Y) {{\n\
         \t\t\tself.setY(SPAWN_Y);\n\
         \t\t\tm_timer.set(0);\n\
{cam_del}\
         \t\t\tCommon.toLocalState(LState.REST);\n\
         \t\t}}\n\
         \t}}\n\
         }}\n")
}

/// A generic animated-hazard script (non-thwomp multi-animation hazards): registers every labelled
/// animation, plays the resting `idle` one, and keeps the native HIT_BOX re-armed so a lingering
/// fighter keeps taking hits. Universal fallback for any future multi-animation stage hazard.
fn hazard_anim_loop_script_hx(hz: &Hazard, anims: &[HzAnim], idle: &str) -> String {
    let idle_s = idle.to_ascii_uppercase();
    format!(
        "// Animated stage hazard (custom game object) — converted from SSF2. Local state machine\n\
         // across the labelled animations; native HIT_BOX (HitboxStats) on the active ones.\n\
         // Cross-frame state via self.make* (a plain var re-inits every frame on a game object).\n\n\
         {states}\n\
         var m_cooldown = self.makeFrameTimer({rehit});\n\n\
         function initialize() {{\n\tself.setState(PState.ACTIVE);\n\tCommon.toLocalState(LState.{idle_s});\n}}\n\n\
         function update() {{\n\
         \tm_cooldown.tick();\n\
         \tif (m_cooldown.completed) {{ self.reactivateHitboxes(); m_cooldown.reset(); }}\n\
         }}\n",
        states = local_state_block(anims), rehit = hz.rehit())
}

/// GameObjectStats for a multi-animation hazard: PState.ACTIVE plays the resting `idle` animation
/// (the local state machine swaps in the others), so the engine always has a current animation for
/// the collision detector to resolve the native HIT_BOX against.
fn hazard_gameobject_stats_multi(hid: &str, idle: &str, ecb: Option<(f64, f64)>) -> String {
    // a MOVING hazard needs an ECB for the engine to ground it on floors (without one a falling
    // game object passes straight through stage terrain — isOnFloor() never fires). sized from
    // the hazard's own SSF2 collision box: head = body height above the origin, hips = half-width.
    let ecb_block = ecb.map(|(head, hipw)| format!(
        "\tfloorFootPosition: 0,\n\tfloorHeadPosition: {head:.0},\n\tfloorHipWidth: {hipw:.0},\n\
         \taerialFootPosition: 0,\n\taerialHeadPosition: {head:.0},\n\taerialHipWidth: {hipw:.0},\n"))
        .unwrap_or_default();
    format!(
        "// GameObjectStats for {hid}\n{{\n\tspriteContent: self.getResource().getContent(\"{hid}\"),\n\tinitialState: PState.ACTIVE,\n\
         \tstateTransitionMapOverrides: [\n\t\tPState.ACTIVE => {{ animation: \"{idle}\" }}\n\t],\n\
         \tbaseScaleX: 1,\n\tbaseScaleY: 1,\n\tweight: 100,\n\tgravity: 0,\n\tfriction: 0,\n\timmovable: true,  // a stage hazard shoves fighters, never the reverse (windboxes included)\n{ecb_block}}}\n")
}

/// HitboxStats for a multi-animation hazard: entries on each ACTIVE (damaging) animation, empty on
/// the safe ones. Keyed by FM animation name, matching the entity's HIT_BOX layers. `dual` splits
/// the hit into left/right half boxes with mirrored away-from-center angles (the SSF2 thwomp's two
/// attackBoxes: direction 135 on the left half, 45 on the right).
fn hazard_hitbox_stats_multi(hz: &Hazard, anims: &[HzAnim], dual: bool) -> String {
    // per-box launch directions from the class's getAttackStats (the Thwomp's 135 left / 45 right),
    // falling back to the away-from-center default only if the source declared none.
    let dir = |i: usize, fallback: f64| hz.hitbox_dirs.get(i).copied().unwrap_or(fallback);
    let mut entries = Vec::new();
    for a in anims {
        if a.active {
            if dual {
                entries.push(format!(
                    "\t{n}: {{\n\
                     \t\thitbox0: {{ damage: {d}, angle: {a0}, baseKnockback: {kb}, knockbackGrowth: {g}, \
                     hitstop: 6, hitstun: 24, reversibleAngle: false, directionalInfluence: true, reflectable: false }},\n\
                     \t\thitbox1: {{ damage: {d}, angle: {a1}, baseKnockback: {kb}, knockbackGrowth: {g}, \
                     hitstop: 6, hitstun: 24, reversibleAngle: false, directionalInfluence: true, reflectable: false }}\n\t}}",
                    n = a.name, d = hz.damage, kb = hz.knockback, g = hz.kb_growth, a0 = dir(0, 135.0), a1 = dir(1, 45.0)));
            } else {
                entries.push(format!(
                    "\t{}: {{\n\t\thitbox0: {{ damage: {}, angle: {}, baseKnockback: {}, knockbackGrowth: {}, \
                     hitstop: 6, hitstun: 24, reversibleAngle: true, directionalInfluence: true, reflectable: false }}\n\t}}",
                    a.name, hz.damage, hz.angle, hz.knockback, hz.kb_growth));
            }
        } else {
            entries.push(format!("\t{}: {{}}", a.name));
        }
    }
    format!(
        "// HitboxStats for the stage hazard — 1:1 from the SSF2 getAttackStats disasm\n\
         // (power -> baseKnockback, kbConstant -> knockbackGrowth, direction -> angle).\n{{\n{}\n}}\n",
        entries.join(",\n"))
}

/// AnimationStats for a multi-animation hazard: cyclic phases (idle face, fall speed-lines) LOOP
/// like the SSF2 movieclips; a one-shot entrance/intro plays once and holds its last frame (the
/// script's local state machine does the switching).
fn hazard_animation_stats_multi(anims: &[HzAnim]) -> String {
    let entries: Vec<String> = anims.iter()
        .map(|a| {
            // hold-vs-loop is the SWF's own declaration: a sub-clip with a stop() frame script
            // holds (NONE), one without loops — read from the timeline class, not guessed. The
            // entrance/intro keyword stays only as a fallback for clips with no script data.
            let one_shot = a.hold || a.name.contains("entrance") || a.name.contains("intro");
            let end = if one_shot { "NONE" } else { "LOOP" };
            format!("\t{}: {{ endType: AnimationEndType.{end} }}", a.name)
        }).collect();
    format!("// AnimationStats for the stage hazard.\n{{\n{}\n}}\n", entries.join(",\n"))
}

fn hazard_script_hx(hz: &Hazard) -> String {
    // A stage hazard is a custom game object with NO fighter owner (null owner), which makes it
    // neutral — a null hitbox owner passes the engine's team-hit validation, so it damages everyone.
    // Damage/knockback come from the NATIVE HIT_BOX (HitboxStats), not a script overlap test. The
    // local state machine plays the animations: ACTIVE (gameObjectIdle, carries the HIT_BOX) and
    // INACTIVE (gameObjectInactive, safe) for the on/off pulse. Two stats are load-bearing for the
    // native hitbox to connect: GameObjectStats.stateTransitionMapOverrides (PState.ACTIVE ->
    // gameObjectIdle, so an animation actually plays and the collision detector's currentAnimation
    // resolves against the HitboxStats map) and spriteContent as a real content ref. The hitbox is
    // re-armed on a cadence so a lingering fighter keeps taking damage. `motion` moves the entity.
    let tau = "6.2831853";
    // motion reads persistent state through `.get()` (m_frame/m_baseX/m_baseY are `self.make*`).
    let motion = match hz.motion.as_str() {
        "oscillateX" => format!("\tself.setX(m_baseX.get() + {r} * Math.sin(m_frame.get() * {tau} / {p}));\n", r = hz.range, p = hz.period),
        "oscillateY" => format!("\tself.setY(m_baseY.get() + {r} * Math.sin(m_frame.get() * {tau} / {p}));\n", r = hz.range, p = hz.period),
        "circle" => format!("\tself.setX(m_baseX.get() + {r} * Math.cos(m_frame.get() * {tau} / {p}));\n\tself.setY(m_baseY.get() + {r} * Math.sin(m_frame.get() * {tau} / {p}));\n", r = hz.range, p = hz.period),
        // thwomp: hover at the top, accelerate DOWN (quadratic, a gravity slam), rest at the
        // bottom, then ease back up. SMOOTH per-frame Y (no teleport) so it doesn't flicker.
        "fall" => format!(
            "\tvar _t = m_frame.get() % {p};\n\
             \tif (_t < {p} * 0.5) {{ self.setY(m_baseY.get()); }}\n\
             \telse if (_t < {p} * 0.62) {{ var _f = (_t - {p} * 0.5) / ({p} * 0.12); self.setY(m_baseY.get() + {r} * _f * _f); }}\n\
             \telse if (_t < {p} * 0.78) {{ self.setY(m_baseY.get() + {r}); }}\n\
             \telse {{ var _f = (_t - {p} * 0.78) / ({p} * 0.22); self.setY(m_baseY.get() + {r} * (1.0 - _f)); }}\n",
            r = hz.range, p = hz.period),
        _ => String::new(),
    };
    // pulse: toggle the active (hitbox) and inactive (no hitbox) states on the duty cycle.
    let pulse = if hz.interval > 0 {
        format!(
            "\tvar on = (m_frame.get() % {iv}) < {ac};\n\
             \tif (on && Common.inLocalState(LState.INACTIVE)) {{ Common.toLocalState(LState.ACTIVE); }}\n\
             \telse if (!on && Common.inLocalState(LState.ACTIVE)) {{ Common.toLocalState(LState.INACTIVE); }}\n",
            iv = hz.interval, ac = hz.active)
    } else {
        String::new()
    };
    format!(
        "// Stage hazard (custom game object) — converted from SSF2.\n\
         // Local state machine (clean multi-animation on a non-character entity) + the native\n\
         // hitbox (HitboxStats). null owner is fine for damage. `motion` = the SSF2 movement.\n\n\
         function _prepLocalState(animation:String, ?index:Int=Math.NaN):Int {{\n\
         \tif (!__hasInitLocalStateMachine) {{ Common.initLocalStateMachine(); __hasInitLocalStateMachine = true; }}\n\
         \tif (index != Math.NaN) {{ index = __localStatePrepIndex++; }}\n\
         \tCommon.registerLocalState(index, animation);\n\
         \treturn index;\n\
         }}\n\
         var __hasInitLocalStateMachine = false;\n\
         var __localStatePrepIndex = -1;\n\
         var LState = {{\n\
         \tUNINITIALIZED: _prepLocalState(\"#n/a\", -1),\n\
         \tACTIVE: _prepLocalState(\"gameObjectIdle\"),\n\
         \tINACTIVE: _prepLocalState(\"gameObjectInactive\")\n\
         }};\n\n\
         var REHIT = {rehit};\n\
         // persistent state (a plain var resets every frame on a custom game object).\n\
         var m_frame = self.makeInt(0);\nvar m_baseX = self.makeFloat(0.0);\nvar m_baseY = self.makeFloat(0.0);\n\
         var m_init = self.makeBool(false);\nvar m_cooldown = self.makeInt(0);\n\n\
         function initialize() {{\n\tself.setState(PState.ACTIVE);\n\tCommon.toLocalState(LState.ACTIVE);\n}}\n\n\
         function update() {{\n\
         \tif (!m_init.get()) {{ m_baseX.set(self.getX()); m_baseY.set(self.getY()); m_init.set(true); }}\n\
         \tm_frame.set(m_frame.get() + 1);\n\
{motion}{pulse}\
         \t// re-arm the native HIT_BOX so a fighter standing in the hazard keeps taking hits\n\
         \t// (a hitbox hits each target once per attack id; reactivateHitboxes issues a fresh one).\n\
         \tif (Common.inLocalState(LState.ACTIVE)) {{\n\
         \t\tif (m_cooldown.get() > 0) {{ m_cooldown.dec(); }}\n\
         \t\telse {{ self.reactivateHitboxes(); m_cooldown.set(REHIT); }}\n\
         \t}}\n\
         }}\n",
        rehit = hz.rehit())
}

fn hazard_gameobject_stats_hx(hid: &str) -> String {
    // stateTransitionMapOverrides maps each PState to the animation the engine plays while in it.
    // Without it a custom game object in PState.ACTIVE plays NO animation, so the collision-box
    // detector's currentAnimation stays null and the native HIT_BOX never resolves against the
    // HitboxStats map (keyed by animation name) — the hit is silently dropped. ACTIVE plays the
    // damaging frame; the local state machine swaps in gameObjectInactive for the off-pulse.
    format!(
        "// GameObjectStats for {hid}\n{{\n\tspriteContent: self.getResource().getContent(\"{hid}\"),\n\tinitialState: PState.ACTIVE,\n\
         \tstateTransitionMapOverrides: [\n\t\tPState.ACTIVE => {{ animation: \"gameObjectIdle\" }}\n\t],\n\
         \tbaseScaleX: 1,\n\tbaseScaleY: 1,\n\tweight: 100,\n\tgravity: 0,\n\tfriction: 0,\n\timmovable: true,  // a stage hazard shoves fighters, never the reverse (windboxes included)\n}}\n")
}

fn hazard_hitbox_stats_hx(hz: &Hazard) -> String {
    format!(
        "// HitboxStats for the stage hazard — SSF2 attack stats (power -> baseKnockback,\n\
         // kbConstant -> knockbackGrowth, direction -> angle).\n\
         {{\n\tgameObjectIdle: {{\n\t\thitbox0: {{ damage: {}, angle: {}, baseKnockback: {}, knockbackGrowth: {}, \
         hitstop: 6, hitstun: 24, reversibleAngle: true, directionalInfluence: true, reflectable: false }}\n\t}},\n\
         \tgameObjectInactive: {{}}\n}}\n",
        hz.damage, hz.angle, hz.knockback, hz.kb_growth)
}

fn hazard_animation_stats_hx() -> String {
    "// AnimationStats for the stage hazard.\n{\n\tgameObjectIdle: { endType: AnimationEndType.NONE },\n\tgameObjectInactive: { endType: AnimationEndType.NONE }\n}\n".to_string()
}

// ── animated backdrop element -> independent-loop VFX, reparented into a background container ──
// the architectural fix for Flash's two timeline features the single baked `stage` animation
// can't represent: (1) a nested movieclip whose loop is INDEPENDENT of the parent (a 260-frame
// element plays all 260 regardless of the master length), and (2) multiple distinct objects on
// one layer+frame. each promoted element becomes its own VFX (objectType "VFX", like the character
// port's effects) whose `active` animation is just that element's frames on LOOP (its own clock),
// and the stage reparents its sprite into a background CONTAINER for depth:
//   var v = match.createVfx(new VfxStats({ spriteContent, animation: "active", loop: true,
//                                          timeout: -1, relativeWith: false }));
//   self.getBackgroundBehindContainer().addChild(v.getSprite());
// depth control is the Stage container API (getBackground*Container etc., each returns a Container)
// + Vfx.getSprite() (a DisplayObject) + Container.addChild — the same reparent the CGO version used,
// just getSprite() instead of getViewRootContainer(). a plain VfxLayer drew at the wrong depth, so
// the explicit container reparent is authoritative. VFX carry no scripts/stats/manifest entry.

/// One backdrop-element VFX entity: the element's frames as a single `active` animation on LOOP
/// (looping driven by VfxStats.loop at spawn). objectType "VFX" so createVfx's spriteContent
/// accepts it. references the bg sprite GUIDs already written by `write_layer` (no PNG re-write);
/// keyframe lengths are the per-frame holds (already FM 60fps frames, like the baked bg path).
/// The element's own origin: the top-left it is placed at on the stage.
///
/// Every frame of a static element carries the SAME stage coordinate, and an animated one
/// carries near-identical coords per frame, so this is the element's position rather than a
/// per-frame accident. Hoisting it out of the art is what gives the vfx a real position (see
/// `bg_element_entity`).
fn bg_element_origin(layer: &BgLayerRef) -> (f64, f64) {
    // The element's placement, lifted out of the art and carried on the VFX ITSELF
    // (`VfxStats.x/y`), which works because the stage reparents the vfx's VIEW ROOT CONTAINER
    // rather than its sprite (see `emit_bg_elements`).
    //
    // Reparenting `getSprite()` is what made an earlier attempt at this render wrong: it lifts
    // the sprite OUT of the entity's view hierarchy, so the entity's position stops moving it
    // and every element collapses onto the layer container's origin -- measured live at
    // absolute (-163,-277) for all of them, which on bowserscastle put the podoboos up in the
    // chandeliers instead of in the lava. Moving the view ROOT keeps the entity intact, so its
    // position means what it says and the element stays one coherent, movable object.
    let x = layer.frames.iter().map(|f| f.x).fold(f64::MAX, f64::min);
    let y = layer.frames.iter().map(|f| f.y).fold(f64::MAX, f64::min);
    if x.is_finite() && y.is_finite() { (x, y) } else { (0.0, 0.0) }
}

fn bg_element_entity(eid: &str, layer: &BgLayerRef, scale: f64) -> Value {
    let alpha = layer.plane.alpha();
    // Art is element-LOCAL: the stage offset is hoisted onto the vfx itself (VfxStats x/y)
    // instead of being baked into every symbol. Baking it in meant every element sat at the
    // world origin with its picture drawn far away — so it had no position to read, none to
    // move, and nothing to tell two elements apart by. Per-frame offsets are preserved as
    // deltas from this origin, so an element whose frames crop differently still animates.
    let (ox, oy) = bg_element_origin(layer);
    let g = |s: &str| det_uuid(&format!("bgelem::{eid}::{s}"));
    let symbols: Vec<Value> = layer.frames.iter().enumerate().map(|(j, f)| json!({
        "$id": g(&format!("sym{j}")), "type": "IMAGE", "imageAsset": f.guid,
        "x": f.x - ox, "y": f.y - oy, "pivotX": 0.0, "pivotY": 0.0,
        "scaleX": scale, "scaleY": scale, "rotation": 0.0, "alpha": alpha, "pluginMetadata": {}
    })).collect();
    let mut keyframes: Vec<Value> = layer.frames.iter().enumerate().map(|(j, f)| json!({
        "$id": g(&format!("kf{j}")), "symbol": g(&format!("sym{j}")), "length": f.hold.max(1),
        "tweened": false, "tweenType": "LINEAR", "type": "IMAGE", "pluginMetadata": {}
    })).collect();
    // 30fps SSF2 source -> 60fps engine: double every keyframe length, exactly like the main stage
    // entity (build_entity) and the hazards. Without this the promoted VFX backdrop elements (the
    // torches, embers) animate at half the speed of the rest of the stage.
    crate::entity_gen::double_keyframe_lengths(&mut keyframes);
    let kf_ids: Vec<String> = (0..layer.frames.len()).map(|j| g(&format!("kf{j}"))).collect();
    json!({
        "export": true, "guid": g("entity"), "id": eid, "version": 5,
        "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "objectType": "VFX", "version": "0.1.0" } },
        "plugins": ["com.fraymakers.FraymakersTypes", "com.fraymakers.FraymakersMetadata"],
        "tags": [], "paletteMap": {}, "tilesets": [], "terrains": [],
        "symbols": symbols,
        "keyframes": keyframes,
        "layers": [ { "$id": g("layer"), "name": "art", "type": "IMAGE", "hidden": false, "locked": false, "keyframes": kf_ids, "pluginMetadata": {} } ],
        "animations": [ { "$id": g("anim"), "name": "active", "layers": [g("layer")], "pluginMetadata": {} } ]
    })
}

/// Per-segment `(fm_animation_name, fm_frame_length)` for a segmented element, in timeline order.
/// Lengths are FM (60fps) frames, so they match what the emitted keyframes actually play.
fn bg_segment_spans(layer: &BgLayerRef) -> Vec<(String, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    for (label, n, _) in &layer.segments {
        let end = (off + n).min(layer.frames.len());
        // the emitted keyframes are doubled 30->60fps, so the animation runs twice the source holds
        let fm_len: usize = layer.frames[off..end].iter().map(|f| f.hold.max(1) * 2).sum();
        out.push((sanitize_anim(label), off, end, fm_len));
        off = end;
    }
    out
}

/// The CUSTOM_GAME_OBJECT entity for a label-segmented backdrop element: one FM animation per
/// reachable segment, each carrying only that segment's frames. A VFX can hold exactly one
/// animation and no script, so an element whose SSF2 timeline BRANCHES between labels cannot be
/// one — it needs an object that can switch what it plays.
fn bg_segment_entity(eid: &str, layer: &BgLayerRef, scale: f64) -> Value {
    let alpha = layer.plane.alpha();
    let (ox, oy) = bg_element_origin(layer);
    let g = |s: &str| det_uuid(&format!("bgseg::{eid}::{s}"));
    let symbols: Vec<Value> = layer.frames.iter().enumerate().map(|(j, f)| json!({
        "$id": g(&format!("sym{j}")), "type": "IMAGE", "imageAsset": f.guid,
        "x": f.x - ox, "y": f.y - oy, "pivotX": 0.0, "pivotY": 0.0,
        "scaleX": scale, "scaleY": scale, "rotation": 0.0, "alpha": alpha, "pluginMetadata": {}
    })).collect();
    let mut keyframes: Vec<Value> = Vec::new();
    let mut layers: Vec<Value> = Vec::new();
    let mut animations: Vec<Value> = Vec::new();
    for (name, start, end, _) in bg_segment_spans(layer) {
        let mut kfs: Vec<Value> = (start..end).map(|j| json!({
            "$id": g(&format!("kf{j}")), "symbol": g(&format!("sym{j}")),
            "length": layer.frames[j].hold.max(1),
            "tweened": false, "tweenType": "LINEAR", "type": "IMAGE", "pluginMetadata": {}
        })).collect();
        crate::entity_gen::double_keyframe_lengths(&mut kfs);
        keyframes.extend(kfs);
        let lid = g(&format!("layer_{name}"));
        layers.push(json!({ "$id": lid, "name": format!("art_{name}"), "type": "IMAGE",
            "hidden": false, "locked": false,
            "keyframes": (start..end).map(|j| g(&format!("kf{j}"))).collect::<Vec<_>>(),
            "pluginMetadata": {} }));
        animations.push(json!({ "$id": g(&format!("anim_{name}")), "name": name,
            "layers": [lid], "pluginMetadata": {} }));
    }
    json!({
        "export": true, "guid": g("entity"), "id": eid, "version": 5,
        "pluginMetadata": { "com.fraymakers.FraymakersMetadata": { "objectType": "CUSTOM_GAME_OBJECT", "version": "0.4.0" } },
        "plugins": ["com.fraymakers.FraymakersTypes", "com.fraymakers.FraymakersMetadata"],
        "tags": [], "paletteMap": {}, "tilesets": [], "terrains": [],
        "symbols": symbols, "keyframes": keyframes, "layers": layers, "animations": animations
    })
}

/// The state machine that replays a segmented clip's own branching, so the element shows what SSF2
/// shows and in the same proportions.
///
/// Each segment's animation LOOPS on its own, so the script only has to act at the end of a lap:
/// staying in the same segment is doing nothing, and a branch is one `toLocalState`. That keeps the
/// common case seamless (no restart hitch on the segment that just repeats) and means the frame
/// counter is the only state that has to survive between frames.
fn bg_segment_script_hx(eid: &str, layer: &BgLayerRef) -> String {
    use crate::abc_parser::LabelJump;
    let spans = bg_segment_spans(layer);
    let idx_of = |lbl: &str| spans.iter().position(|(n, ..)| *n == sanitize_anim(lbl));
    let lens: Vec<String> = spans.iter().map(|(_, _, _, l)| l.to_string()).collect();
    // one branch arm per segment, unrolled: which segment follows this one, and how often
    let mut arms = String::new();
    for (i, (_, _, _, _)) in spans.iter().enumerate() {
        let fall = if i + 1 < spans.len() { i + 1 } else { 0 };
        let body = match layer.segments.get(i).and_then(|(_, _, j)| j.clone()) {
            Some(LabelJump::Always(t)) => format!("return {};", idx_of(&t).unwrap_or(fall)),
            // the SSF2 frame script rolls the engine RNG here; a roll that misses simply lets the
            // timeline run on into the next segment, which is what the fallthrough is
            Some(LabelJump::Chance { pct, target }) => format!(
                "if (Random.getInt(0, 99) < {pct}) {{ return {}; }}\n\t\treturn {fall};",
                idx_of(&target).unwrap_or(fall)),
            None => format!("return {fall};"),
        };
        arms.push_str(&format!("\tif (i == {i}) {{\n\t\t{body}\n\t}}\n"));
    }
    let states: Vec<String> = spans.iter()
        .map(|(n, ..)| format!("\t{}: _prepLocalState(\"{n}\")", n.to_ascii_uppercase())).collect();
    format!(
        "// {eid}: a backdrop element whose SSF2 clip BRANCHES between frame labels rather than\n\
         // looping one timeline. One FM animation per label; this replays the branching.\n\n\
         function _prepLocalState(animation:String, ?index:Int=Math.NaN):Int {{\n\
         \tif (!__hasInitLocalStateMachine) {{ Common.initLocalStateMachine(); __hasInitLocalStateMachine = true; }}\n\
         \tif (index != Math.NaN) {{ index = __localStatePrepIndex++; }}\n\
         \tCommon.registerLocalState(index, animation);\n\treturn index;\n}}\n\
         var __hasInitLocalStateMachine = false;\nvar __localStatePrepIndex = -1;\n\
         var LState = {{\n\tUNINITIALIZED: _prepLocalState(\"#n/a\", -1),\n{}\n}};\n\n\
         // segment lengths in engine frames (the source's 30fps holds, doubled)\n\
         var LENS = [{}];\n\
         var STATES = [{}];\n\
         // persistent: a plain var re-initialises every frame on a custom game object.\n\
         var m_seg = self.makeInt(0);\nvar m_t = self.makeInt(0);\nvar m_init = self.makeBool(false);\n\n\
         // which segment follows `i` — the clip's own frame scripts, unrolled.\n\
         function _next(i:Int):Int {{\n{}\treturn 0;\n}}\n\n\
         function initialize() {{\n\tself.setState(PState.ACTIVE);\n}}\n\n\
         function update() {{\n\
         \tif (!m_init.get()) {{ m_init.set(true); Common.toLocalState(STATES[0]); }}\n\
         \tvar t = m_t.get() + 1;\n\
         \tif (t >= LENS[m_seg.get()]) {{\n\
         \t\tt = 0;\n\
         \t\tvar nxt = _next(m_seg.get());\n\
         \t\t// staying put needs no call: the animation already loops itself, and restarting it\n\
         \t\t// would visibly re-seat the element every lap.\n\
         \t\tif (nxt != m_seg.get()) {{ m_seg.set(nxt); Common.toLocalState(STATES[nxt]); }}\n\
         \t}}\n\tm_t.set(t);\n}}\n",
        states.join(",\n"), lens.join(", "),
        spans.iter().map(|(n, ..)| format!("LState.{}", n.to_ascii_uppercase())).collect::<Vec<_>>().join(", "),
        arms)
}

/// Emit each promoted backdrop element as a looping VFX content (a `.entity` of objectType "VFX",
/// no scripts/stats/manifest entry — resolved by id via getContent, like the character port's
/// effects) and return the stage-Script spawn lines: createVfx (loop forever, absolute position)
/// then reparent its sprite into the stage background container for depth.
fn emit_bg_elements(model: &StageModel, promoted: &[BgLayerRef], lib: &Path)
    -> Result<(String, Vec<Value>)> {
    let mut spawns = String::new();
    let mut entries: Vec<Value> = Vec::new();
    for (i, layer) in promoted.iter().enumerate() {
        let eid = format!("{}_{}", model.id, layer.eid);
        // an element whose clip branches between labels can't be a VFX: a VFX holds one animation
        // and carries no script, so there is nothing to switch and nothing to switch it.
        if layer.segments.len() > 1 {
            emit_bg_segmented(model, layer, &eid, i, lib, &mut spawns, &mut entries)?;
            continue;
        }
        write_json(&lib.join("entities").join(format!("{eid}.entity")), &bg_element_entity(&eid, layer, model.scale))?;
        write_meta(&lib.join("entities").join(format!("{eid}.entity.meta")), &eid, &eid, "", Some("VFX"), None)?;
        // createVfx loops the element forever at its baked-in position (relativeWith:false ->
        // absolute), then reparent its sprite into the background container so it draws in front
        // of the static background art and behind the fighters. no owner / no VfxLayer: the
        // container reparent is the authoritative depth control.
        let (ox, oy) = bg_element_origin(layer);
        spawns.push_str(&format!(
            "\t\t\tvar _bg{i} = match.createVfx(new VfxStats({{ spriteContent: self.getResource().getContent(\"{eid}\"), \
             animation: \"active\", x: {ox}, y: {oy}, loop: true, timeout: -1, relativeWith: false, resizeWith: false }}));\n\
             \t\t\tif (_bg{i} != null) {{ self.{container}().addChild(_bg{i}.getViewRootContainer()); }}\n",
            container = layer.plane.container()));
    }
    Ok((spawns, entries))
}

/// One segmented backdrop element: a CUSTOM_GAME_OBJECT (entity + state-machine Script + stats),
/// spawned and reparented into the SAME layer container the VFX form uses, so switching vehicles
/// doesn't change where it draws.
#[allow(clippy::too_many_arguments)]
fn emit_bg_segmented(
    model: &StageModel, layer: &BgLayerRef, eid: &str, i: usize, lib: &Path,
    spawns: &mut String, entries: &mut Vec<Value>,
) -> Result<()> {
    let scripts = lib.join("scripts").join("entities");
    std::fs::create_dir_all(&scripts)?;
    write_json(&lib.join("entities").join(format!("{eid}.entity")), &bg_segment_entity(eid, layer, model.scale))?;
    write_meta(&lib.join("entities").join(format!("{eid}.entity.meta")), eid, eid, "", Some("CUSTOM_GAME_OBJECT"), None)?;

    let spans = bg_segment_spans(layer);
    let anim_stats = spans.iter()
        .map(|(n, ..)| format!("\t{n}: {{ endType: AnimationEndType.LOOP }}"))
        .collect::<Vec<_>>().join(",\n");
    let files = [
        ("Script", bg_segment_script_hx(eid, layer)),
        ("GameObjectStats", format!(
            "// GameObjectStats for the segmented backdrop element {eid}.\n{{\n\
             \tspriteContent: self.getResource().getContent(\"{eid}\"),\n\tinitialState: PState.ACTIVE,\n\
             \tstateTransitionMapOverrides: [\n\t\tPState.ACTIVE => {{ animation: \"{}\" }}\n\t],\n\
             \tbaseScaleX: 1,\n\tbaseScaleY: 1,\n\tweight: 100,\n\tgravity: 0,\n\tfriction: 0,\n\
             \t// scenery: it is never pushed, and it never pushes anything.\n\timmovable: true,\n}}\n",
            spans.first().map(|(n, ..)| n.clone()).unwrap_or_default())),
        ("AnimationStats", format!(
            "// AnimationStats for the segmented backdrop element {eid}. Every segment LOOPS; the\n\
             // Script switches between them at a lap boundary.\n{{\n{anim_stats}\n}}\n")),
    ];
    for (kind, body) in files {
        let fname = format!("{eid}{kind}");
        write_script(&scripts.join(format!("{fname}.hx")), &body)?;
        write_meta(&scripts.join(format!("{fname}.hx.meta")), eid, &fname,
            if kind == "Script" { "" } else { "hscript" },
            if kind == "Script" { Some("CUSTOM_GAME_OBJECT") } else { None }, None)?;
    }
    entries.push(json!({
        "type": "customGameObject", "id": eid,
        "scriptId": format!("{eid}Script"),
        "objectStatsId": format!("{eid}GameObjectStats"),
        "animationStatsId": format!("{eid}AnimationStats"),
        "name": layer.name.clone(),
    }));
    let (ox, oy) = bg_element_origin(layer);
    spawns.push_str(&format!(
        "\t\t\tvar _bg{i} = match.createCustomGameObject(self.getResource().getContent(\"{eid}\"), owner);\n\
         \t\t\tif (_bg{i} != null) {{ _bg{i}.setX({ox}); _bg{i}.setY({oy}); \
         self.{container}().addChild(_bg{i}.getViewRootContainer()); }}\n",
        container = layer.plane.container()));
    Ok(())
}

/// hscript the stage Script runs to spawn its hazards (createCustomGameObject + position).
fn hazard_spawn_lines(model: &StageModel) -> String {
    let mut out = String::new();
    for (i, hz) in model.hazards.iter().enumerate() {
        let hid = hazard_id(&model.id, i, hz);
        // owned by a character (a GameObject) so the hitbox registers; setX/setY positions it.
        out.push_str(&format!(
            "\t\t\tvar _hz{i} = match.createCustomGameObject(self.getResource().getContent(\"{hid}\"), owner);\n\
             \t\t\tif (_hz{i} != null) {{ _hz{i}.setX({:.1}); _hz{i}.setY({:.1}); }}\n",
            hz.x, hz.y));
    }
    out
}

fn build_manifest(model: &StageModel, hazard_entries: &[Value], structure_entries: &[Value]) -> Value {
    let id = &model.id;
    // a match needs at least one music track to start, and it must be a real public FM bgm
    // (the SSF2 soundtrack isn't shipped with the engine). `fm_music` is the override-map
    // pick or the configured default; the original SSF2 track ids go in the description.
    let music: Vec<Value> = model.fm_music.iter().map(|bgm| json!({
        "namespace": "public", "resourceId": bgm, "contentId": bgm
    })).collect();
    let mut description = format!("{} — converted from Super Smash Flash 2", model.display_name);
    if let Some(series) = &model.series { description.push_str(&format!(" ({series})")); }
    if !model.ssf2_music.is_empty() {
        description.push_str(&format!(". Original SSF2 soundtrack: {}", model.ssf2_music.join(", ")));
    }
    let mut content = vec![json!({
        "id": id,
        "name": model.display_name,
        "description": description,
        "type": "stage",
        "objectStatsId": format!("{id}StageStats"),
        "scriptId": format!("{id}Script"),
        "music": music,
        "metadata": {
            "ui": { "entityId": id, "render": { "animation": "stage" } }
        }
    })];
    content.extend(hazard_entries.iter().cloned());
    content.extend(structure_entries.iter().cloned());
    json!({ "resourceId": id, "content": content })
}

fn build_fraytools() -> Value {
    json!({
        "frame_rate": 60,
        "snapToPixel": true,
        "paletteShaderMode": "RG_MAP",
        "plugins": [
            "com.fraymakers.ContentExporter",
            "com.fraymakers.FraymakersTypes",
            "com.fraymakers.FraymakersMetadata"
        ],
        "pluginMetadata": {},
        "publishFolders": [ { "id": "build0", "path": "./build" } ],
        "version": 12
    })
}

fn write_meta(path: &Path, _stage_id: &str, id: &str, language: &str, object_type: Option<&str>, _unused: Option<()>) -> Result<()> {
    let mut pm = json!({});
    if let Some(ot) = object_type {
        pm = json!({ "com.fraymakers.FraymakersMetadata": { "objectType": ot, "version": "0.4.0" } });
    }
    let plugins: Vec<&str> = if object_type.is_some() { vec!["com.fraymakers.FraymakersMetadata"] } else { vec![] };
    let v = json!({
        "export": true,
        "guid": det_uuid(&format!("stage::meta::{id}")),
        "id": id,
        "language": language,
        "pluginMetadata": pm,
        "plugins": plugins,
        "tags": [],
        "version": 1
    });
    write_json(path, &v)
}

/// StageStats: stage sprite + camera. The fixed SSF2 backdrop lives as an IMAGE layer in the
/// `stage` animation (it carries the surface fighters stand on, so it moves 1:1 with the
/// world). Each SSF2 camera-background layer is emitted as its own `parallax{i}` animation +
/// a `camera.backgrounds` entry that pans at the layer's own SSF2-derived `xPanMultiplier`.
fn stage_stats_hx(id: &str, parallax: &[ParallaxRef], scale: f64) -> String {
    // back-to-front: layer 0 is the farthest (highest depth). ParallaxBG sizes the layer as
    // `originalBGWidth × scaleMultiplier` (native png × the stage scale; the IMAGE symbol stays
    // at scale 1). PAN mode pans the layer at `xPanMultiplier` of the camera offset, matching
    // how SSF2's Vcam scrolls each camera background.
    let entries: Vec<String> = parallax.iter().enumerate().map(|(i, p)| {
        // PAN: straight parallax pan at the per-layer multiplier (the SSF2 `_cambg` behavior).
        // BOUNDS: anchored to the camera bounds + tiling to fill (loopWidth/Height = the tile
        // size), for a repeating backdrop.
        let (mode, hscroll, vscroll, loop_w, loop_h) = match p.mode {
            ParallaxMode::Pan => ("PAN", "false", "false", 0, 0),
            ParallaxMode::Bounds => ("BOUNDS", "true", "true", p.art.w, p.art.h),
        };
        format!(
        "\n\t\t\t{{\n\
\t\t\t\tspriteContent: self.getResource().getContent(\"{id}\"),\n\
\t\t\t\tanimationId: \"parallax{i}\",\n\
\t\t\t\tmode: ParallaxMode.{mode},\n\
\t\t\t\toriginalBGWidth: {w},\n\
\t\t\t\toriginalBGHeight: {h},\n\
\t\t\t\thorizontalScroll: {hscroll},\n\
\t\t\t\tverticalScroll: {vscroll},\n\
\t\t\t\tloopWidth: {loop_w},\n\
\t\t\t\tloopHeight: {loop_h},\n\
\t\t\t\txPanMultiplier: {xp},\n\
\t\t\t\tyPanMultiplier: {yp},\n\
\t\t\t\tscaleMultiplier: {scale},\n\
\t\t\t\tforeground: false,\n\
\t\t\t\tdepth: {depth}\n\
\t\t\t}}",
        w = p.art.w, h = p.art.h, xp = p.x_pan, yp = p.y_pan, depth = 2000 - (i as i64) * 10)
    }).collect();
    let backgrounds = if entries.is_empty() { String::new() } else { format!("{}\n\t\t", entries.join(",")) };
    format!(
        "// Stats for {id} (converted from SSF2)\n\n\
{{\n\
\tspriteContent: self.getResource().getContent(\"{id}\"),\n\
\tanimationId: \"stage\",\n\
\tambientColor: 0xffffffff,\n\
\tshadowLayers: [],\n\
\tcamera: {{\n\
\t\tstartX: 0,\n\
\t\tstartY: 0,\n\
\t\tzoomX: 0,\n\
\t\tzoomY: 0,\n\
\t\tcamEaseRate: 1 / 11,\n\
\t\tcamZoomRate: 1 / 15,\n\
\t\tminZoomHeight: 360,\n\
\t\tinitialHeight: 360,\n\
\t\tinitialWidth: 640,\n\
\t\tbackgrounds: [{backgrounds}]\n\
\t}}\n\
}}\n"
    )
}

/// Stage Script.hx — pause a static stage on frame 1; let an animated stage's timeline
/// play (the SSF2 animated clips loop). The parallax background is camera-scrolled by
/// StageStats, so no manual scroll is needed.
fn script_hx(id: &str, animated: bool, hazard_spawns: &str) -> String {
    let init = if animated { "\t// animated stage clips play + loop on the timeline" } else { "\tself.pause();" };
    // hazards spawn DEFERRED in update() once the match is live (one-shot via a flag). owner is
    // NULL: a stage hazard belongs to no fighter, so it damages everyone (a null hitbox owner
    // passes the engine's team-hit validation), and createCustomGameObject accepts a null owner.
    let (haz_var, haz_body) = if hazard_spawns.is_empty() {
        // no hazards: keep update() a clean empty body (byte-stable with hazardless stages).
        (String::new(), String::new())
    } else {
        ("var m_hazardsSpawned = false;\n".to_string(),
         format!("\tif (!m_hazardsSpawned) {{\n\
                  \t\tvar chars = match.getCharacters();\n\
                  \t\tif (chars.length > 0) {{\n\
                  \t\t\tm_hazardsSpawned = true;\n\
                  \t\t\tvar owner = null;\n\
{hazard_spawns}\
                  \t\t}}\n\
                  \t}}\n"))
    };
    let update_fn = if haz_body.is_empty() {
        "function update() {}\n".to_string()
    } else {
        format!("function update() {{\n{haz_body}}}\n")
    };
    format!(
        "// API Script for {id} (converted from SSF2)\n\n\
{haz_var}\
function initialize() {{\n\
{init}\n\
}}\n\
{update_fn}\
function onTeardown() {{}}\n\
function onKill() {{}}\n\
function onStale() {{}}\n\
function afterPushState() {{}}\n\
function afterPopState() {{}}\n\
function afterFlushStates() {{}}\n"
    )
}

#[cfg(test)]
mod hazard_tests {
    use super::*;

    fn demo_hazard() -> Hazard {
        Hazard {
            x: 0.0, y: 150.0, w: 700.0, h: 160.0,
            damage: 10.0, knockback: 0.0, angle: 45.0,
            interval: 0, active: 20, motion: "static".into(),
            range: 0.0, period: 120, rehit: 30, kb_growth: 40.0, label: "TestHazard".into(), art: None, anims: vec![], attack_boxes: vec![], hitbox_dirs: vec![], anim_labels: vec![], faller: None, behavior: crate::abc_parser::EnemyBehavior::default(), reconstructed_script: None, class_name: None, max_y_speed: None,
        }
    }

    // The native custom-game-object hitbox only connects when the engine is actually playing the
    // hitbox animation, so the collision detector's currentAnimation resolves against the
    // HitboxStats map. These three pieces are each individually load-bearing and fail SILENTLY
    // (0 damage, no error) if dropped — so assert them.
    #[test]
    fn hazard_gameobject_stats_drive_the_hitbox_animation() {
        let s = hazard_gameobject_stats_hx("demohazard0");
        // PState.ACTIVE must map to the animation that carries the HIT_BOX, or nothing plays and
        // the collision detector's currentAnimation stays null -> every hit is silently dropped.
        assert!(s.contains("stateTransitionMapOverrides"), "missing state->animation map: {s}");
        assert!(s.contains("PState.ACTIVE => { animation: \"gameObjectIdle\" }"), "wrong/absent ACTIVE animation: {s}");
        // spriteContent must be a real content ref (a bare string id does not load the sprite).
        assert!(s.contains("spriteContent: self.getResource().getContent(\"demohazard0\")"), "spriteContent not a content ref: {s}");
    }

    #[test]
    fn hazard_script_uses_native_hitbox_with_rearm() {
        let s = hazard_script_hx(&demo_hazard());
        // a hitbox hits each target once per attack id; re-arm so a lingering fighter keeps taking
        // hits (continuous SSF2-style hazard damage).
        assert!(s.contains("reactivateHitboxes()"), "no native-hitbox re-arm: {s}");
        // the local state machine plays the hitbox animation.
        assert!(s.contains("Common.toLocalState(LState.ACTIVE)"), "no local-state activation: {s}");
        // no script-overlap damage fallback — damage comes from the native HIT_BOX.
        assert!(!s.contains("addDamage"), "script-overlap damage should be gone: {s}");
    }

    // A plain `var counter = 0` in a custom-game-object script RE-INITIALIZES every frame, so any
    // cross-frame accumulator silently never advances (the thwomp froze at spawn, dealt 0 damage).
    // Cross-frame state MUST be `self.make*` + .get()/.set(). Assert no naked mutable counter leaks
    // back into the hazard/thwomp scripts — this failure is invisible at convert time and only shows
    // as a dead hazard in-engine.
    #[test]
    fn hazard_scripts_use_persistent_state_not_plain_vars() {
        let cols = [(438.0, 660.0), (770.0, 616.0), (1100.0, 660.0)];
        let mut hz = demo_hazard();
        hz.motion = "oscillateX".into();
        hz.range = 40.0;
        for (name, s) in [
            ("hazard_script", hazard_script_hx(&hz)),
            ("thwomp_single", thwomp_script_hx(&cols)),
            ("thwomp_multi", thwomp_multi_script_hx(&cols, -67.0, 13.0, 19.5, 3.9, 2.6, 1200.0, 120.0, 180.0, "", true, "entrance", "idle", "fall")),
        ] {
            assert!(s.contains("self.makeInt(") || s.contains("self.makeFloat(") || s.contains("self.makeBool("),
                "{name}: no persistent state (self.make*) — counters reset every frame: {s}");
            // the bug shape: a top-level mutable counter as a plain numeric var.
            for bad in ["var m_phase = 3;", "var m_timer = 0;", "var m_frame = 0;", "var m_cooldown = 0;", "var m_cool = 0;"] {
                assert!(!s.contains(bad), "{name}: plain-var counter `{bad}` re-inits every frame: {s}");
            }
        }
    }

    #[test]
    fn hazard_spawns_with_null_owner() {
        // a null hitbox owner is neutral and passes the engine's team-hit validation (hits
        // everyone), and createCustomGameObject accepts a null owner.
        let spawn = "\t\t\tvar _hz0 = match.createCustomGameObject(self.getResource().getContent(\"demohazard0\"), owner);\n";
        let s = script_hx("demo", false, spawn);
        assert!(s.contains("var owner = null;"), "hazard owner should be null: {s}");
        assert!(s.contains("createCustomGameObject"), "no spawn call: {s}");
    }
}

/// The art fallback, reachable from the stage tests. Exposed rather than duplicated so the tests
/// pin the code that actually ships.
pub fn render_placeholder_for_test(model: &StageModel) -> StageArt { render_placeholder(model) }
