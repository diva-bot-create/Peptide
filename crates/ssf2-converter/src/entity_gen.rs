/// Fraymakers .entity file generator
///
/// Generates the JSON entity file compatible with FrayTools.
/// Format based on the official Fraymakers character template:
/// https://github.com/Fraymakers/character-template
///
/// Key differences from our previous format:
/// - IMAGE symbols use `imageAsset` (GUID ref to .meta file) not `path`
/// - COLLISION_BOX keyframes reference symbol $ids (geometry in symbols)
/// - COLLISION_BODY keyframes reference symbol $ids
/// - All keyframes have `tweened`, `tweenType`, `pluginMetadata`
/// - Layers have `hidden`, `locked`, `pluginMetadata` (with box type metadata)
/// - Entity top-level has `pluginMetadata`, `plugins`, `version`, etc.
/// - Each PNG gets a .meta sidecar file with a GUID

use crate::extractor::CharacterData;
use crate::sprite_parser::{AnimationBoxData, BoxType};
use crate::image_extractor::ImageExtractionResult;
use serde_json::{json, Value};
use std::collections::BTreeMap;

// ─── UUID helpers ─────────────────────────────────────────────────────────────

fn det_uuid(seed: &str) -> String {
    crate::uuid_gen::det_uuid(seed)
}

fn uuid(char_id: &str, context: &str) -> String {
    det_uuid(&format!("{}::{}", char_id, context))
}

// ─── Box type → Fraymakers metadata type string ──────────────────────────────

fn box_type_to_fm(bt: BoxType) -> &'static str {
    match bt {
        BoxType::Hitbox     => "HIT_BOX",
        BoxType::Hurtbox    => "HURT_BOX",
        BoxType::GrabBox    => "GRAB_BOX",
        BoxType::ItemBox    => "NONE",  // no item-box concept in FM; a stray HURT_BOX here (esp. at
                                        // index 0, colliding with the real hitbox index) breaks the
                                        // entity's collision processing. NONE = a non-colliding box.
        BoxType::ShieldBox  => "REFLECT_BOX",
        BoxType::ReflectBox => "REFLECT_BOX",
        BoxType::AbsorbBox  => "COUNTER_BOX",
        BoxType::LedgeBox    => "LEDGE_GRAB_BOX",
        // GrabHoldBox uses a POINT layer, not COLLISION_BOX — handled separately.
        BoxType::GrabHoldBox  => "GRAB_HOLD_POINT",
    }
}

/// Convert an SSF2 instance name to a Fraymakers layer name.
/// SSF2 naming → FM naming:
///   attackBox  → hitbox0   (SSF2 "attack" = FM "hit")
///   attackBox2 → hitbox1
///   attackBox3 → hitbox2
///   hitBox     → hurtbox0  (SSF2 "hit" = FM "hurt")
///   hitBox2    → hurtbox1
///   hurtBox    → hurtbox0
///   grabBox    → grabbox0
///   ledgeBox   → ledgegrabbox0
///   etc.
fn ssf2_box_name_to_fm(inst_name: &str) -> String {
    let lower = inst_name.to_lowercase();

    // Extract numeric suffix: "attackBox2" → suffix=2, "hitBox" → suffix=0 (none = 0)
    // SSF2 uses 1-based suffixes (attackBox=first, attackBox2=second)
    // FM uses 0-based (hitbox0, hitbox1)
    let (prefix_lower, raw_num) = if let Some(pos) = lower.find(|c: char| c.is_ascii_digit()) {
        let num: usize = lower[pos..].parse().unwrap_or(1);
        (&lower[..pos], num)
    } else {
        (lower.as_str(), 1usize)
    };
    // Convert to 0-based: raw_num 1 → index 0, raw_num 2 → index 1
    let index = raw_num.saturating_sub(1);

    match prefix_lower {
        p if p.starts_with("attackbox") || p.starts_with("attack_box") =>
            format!("hitbox{}", index),
        p if p.starts_with("hitbox") || p.starts_with("hurtbox") =>
            format!("hurtbox{}", index),
        p if p.starts_with("grabbox") || p.starts_with("grab") =>
            format!("grabbox{}", index),
        p if p.starts_with("itembox") | p.starts_with("item_box") =>
            format!("itembox{}", index),
        p if p.starts_with("shieldbox") =>
            format!("shieldbox{}", index),
        p if p.starts_with("reflectbox") =>
            format!("reflectbox{}", index),
        p if p.starts_with("absorbbox") =>
            format!("absorbbox{}", index),
        p if p.starts_with("ledgebox") || p.starts_with("ledgegrab") =>
            format!("ledgegrabbox{}", index),
        p if p.starts_with("touchbox") =>
            format!("grabholdpoint{}", index),
        _ => format!("{}{}", prefix_lower.trim_end_matches('_'), index),
    }
}

/// Extract the numeric index from a Fraymakers box layer name.
/// "hitbox0" → 0, "hurtbox1" → 1, "grabbox0" → 0
fn fm_box_index(fm_name: &str) -> usize {
    fm_name.chars().rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars().rev()
        .collect::<String>()
        .parse()
        .unwrap_or(0)
}

fn box_color(bt: BoxType) -> &'static str {
    match bt {
        BoxType::Hitbox     => "0xff0000",
        BoxType::Hurtbox    => "0xfcba03",
        BoxType::GrabBox    => "0xff00ff",
        BoxType::ItemBox    => "0xffff00",
        BoxType::ShieldBox  => "0x48f748",
        BoxType::ReflectBox => "0x48f748",
        BoxType::AbsorbBox  => "0x42ecff",
        BoxType::LedgeBox    => "0xbababa",
        BoxType::GrabHoldBox => "0x9999ff",  // light blue
    }
}

// ─── Meta file generation ─────────────────────────────────────────────────────

/// Generate .meta JSON sidecar for a PNG image asset
pub fn generate_meta(guid: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "export": false,
        "guid": guid,
        "id": "",
        "pluginMetadata": {},
        "plugins": [],
        "tags": [],
        "version": 2
    })).unwrap()
}

// ─── Main generator ───────────────────────────────────────────────────────────

/// Stretch every animation timeline 2x for the 30fps → 60fps move.
///
/// SSF2 plays at 30fps; Fraymakers runs at 60fps. To preserve each
/// animation's real-time playback speed, every keyframe must be held for two
/// Fraymakers frames instead of one. FrayTools timelines are laid out purely
/// by sequential keyframe `length`, so doubling every keyframe's `length`
/// doubles every layer's span and every keyframe's start position in
/// lockstep — image, collision-box, collision-body/ECB, frame-script and
/// label layers all scale together and cannot fall out of sync.
/// Fraymakers character-template animation order (docs 0.3.0). Used to (a) sort the
/// character's template animations into template order and (b) decide which animations are
/// "unused" — anything that is neither a template animation nor a direct (one-suffix) variant
/// of one, e.g. grab_pummel_out (parent grab_pummel isn't a template anim) or star_ko.
const TEMPLATE_ANIM_ORDER: &[&str] = &[
    "revival", "intro", "stand", "stand_turn", "walk_in", "walk_loop",
    "walk_out", "dash", "run", "run_turn", "skid", "jump_squat",
    "jump_in", "jump_loop", "jump_midair", "jump_out", "fall_in", "fall_loop",
    "land_light", "wall_jump_in", "wall_jump", "fall_special", "airdash_land", "airdash_freefall",
    "airdash_freefall_whiff", "land_heavy", "airdash_land_uncanceled", "airdash_land_whiff", "crouch_in", "crouch_loop",
    "crouch_out", "ledge_in", "ledge_loop", "ledge_climb_in", "ledge_climb", "ledge_roll_in",
    "ledge_roll", "ledge_jump_in", "ledge_jump", "ledge_attack_in", "ledge_attack", "shield_in",
    "shield_loop", "shield_hurt", "shield_out", "roll", "spot_dodge", "tech",
    "tech_roll", "hurt_light_low", "hurt_light_middle", "hurt_light_high", "hurt_medium", "hurt_heavy",
    "hurt_thrown", "tumble", "crash_bounce", "crash_loop", "crash_get_up", "crash_roll",
    "crash_attack", "airdash_forward", "airdash_forward_up", "airdash_forward_down", "airdash_up", "airdash_down",
    "airdash_back", "airdash_back_up", "airdash_back_down", "aerial_neutral", "aerial_neutral_land", "aerial_forward",
    "aerial_forward_land", "aerial_back", "aerial_back_land", "aerial_down", "aerial_down_land", "aerial_up",
    "aerial_up_land", "dash_attack", "jab1", "jab2", "jab3", "tilt_forward",
    "tilt_up", "tilt_down", "parry_in", "parry_fail", "parry_success", "strong_forward_in",
    "strong_forward_charge", "strong_forward_attack", "strong_up_in", "strong_up_charge", "strong_up_attack", "strong_down_in",
    "strong_down_charge", "strong_down_attack", "assist_call", "assist_call_air", "special_neutral", "special_neutral_air",
    "special_up", "special_up_air", "special_down", "special_down_loop", "special_down_endlag", "special_down_air",
    "special_down_air_loop", "special_down_air_endlag", "special_side", "special_side_air", "grab", "grab_hold",
    "throw_forward", "throw_back", "throw_up", "throw_down", "emote",
];

/// Reorder the entity's animations to match the character template, with all non-template,
/// non-variant animations grouped under a "---UNUSED ANIMATIONS BELOW---" separator.
fn reorder_animations(animations: &mut Vec<Value>, layers: &mut Vec<Value>, keyframes: &mut Vec<Value>, char_id: &str) {
    let idx = |name: &str| TEMPLATE_ANIM_ORDER.iter().position(|t| *t == name);
    let mut used: Vec<(usize, u8, Value)> = Vec::new();
    let mut unused: Vec<Value> = Vec::new();
    for a in animations.drain(..) {
        let name = a.get("name").and_then(Value::as_str).unwrap_or("").to_string();
        // A direct variant shares the template anim as its immediate prefix (one suffix removed):
        // special_neutral_air→special_neutral, grab_pummel→grab. grab_pummel_out→grab_pummel (NOT a
        // template anim) so it is NOT a variant and lands in UNUSED.
        let parent_idx = name.rsplit_once('_').and_then(|(p, _)| idx(p));
        if let Some(i) = idx(&name) {
            used.push((i, 0, a));
        } else if let Some(i) = parent_idx {
            used.push((i, 1, a));
        } else {
            unused.push(a);
        }
    }
    used.sort_by_key(|(i, t, _)| (*i, *t));
    let mut out: Vec<Value> = used.into_iter().map(|(_, _, a)| a).collect();
    if !unused.is_empty() {
        let kf_id = uuid(char_id, "kf_unused_separator");
        let layer_id = uuid(char_id, "layer_unused_separator");
        let anim_id = uuid(char_id, "anim_unused_separator");
        keyframes.push(json!({ "$id": kf_id, "type": "FRAME_SCRIPT", "length": 1, "code": "", "pluginMetadata": {} }));
        layers.push(json!({ "$id": layer_id, "name": "Scripts", "type": "FRAME_SCRIPT",
            "keyframes": [kf_id], "hidden": false, "locked": false, "language": "", "pluginMetadata": {} }));
        out.push(json!({ "$id": anim_id, "name": "---UNUSED ANIMATIONS BELOW---",
            "layers": [layer_id], "pluginMetadata": {} }));
        let (ordered_count, unused_count) = (out.len() - 1, unused.len());
        out.extend(unused);
        log::info!("reorder: {} template-ordered animations, {} unused (below separator)",
            ordered_count, unused_count);
    }
    *animations = out;
}

/// Append every character-template animation that did NOT make it into the entity
/// (never converted, or dropped as empty) as an empty stub, grouped at the very
/// bottom under a blank "====MISSING ANIMATIONS====" separator. This surfaces the
/// gaps for the modder (e.g. tumble, the extra jabs, parry, the special-down loop)
/// with the same empty template shape they have in the official character template,
/// instead of silently omitting them. Call AFTER drop + reorder so the stubs are
/// neither dropped nor sorted back into the main list.
fn append_missing_template_animations(
    animations: &mut Vec<Value>, layers: &mut Vec<Value>, keyframes: &mut Vec<Value>, char_id: &str,
    image_slots: usize,
) {
    let present: std::collections::BTreeSet<String> = animations.iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str).map(String::from))
        .collect();
    let missing: Vec<String> = crate::mappings::character_animation_template().iter()
        .map(|e| e.name.clone())
        .filter(|n| !present.contains(n))
        .collect();
    if missing.is_empty() { return; }

    // Blank separator animation (a single empty frame-script layer, like the UNUSED one).
    let sep_kf = uuid(char_id, "kf_missing_separator");
    let sep_layer = uuid(char_id, "layer_missing_separator");
    keyframes.push(json!({ "$id": sep_kf, "type": "FRAME_SCRIPT", "length": 1, "code": "", "pluginMetadata": {} }));
    layers.push(json!({ "$id": sep_layer, "name": "Scripts", "type": "FRAME_SCRIPT",
        "keyframes": [sep_kf], "hidden": false, "locked": false, "language": "", "pluginMetadata": {} }));
    animations.push(json!({ "$id": uuid(char_id, "anim_missing_separator"),
        "name": "====MISSING ANIMATIONS====", "layers": [sep_layer], "pluginMetadata": {} }));

    for name in &missing {
        // A stub still has to address every image slot. These stand in for moves the character
        // does not have, but the states are real and can be entered, and an animation that leaves
        // a layer unaddressed shows whatever the last animation put there -- so a missing move
        // would wear the leftovers of whatever preceded it.
        let mut layer_ids: Vec<String> = Vec::new();
        for slot in 0..image_slots.max(1) {
            let kf_id = uuid(char_id, &format!("kf_missing_{}_{}", name, slot));
            let layer_id = uuid(char_id, &format!("layer_missing_{}_{}", name, slot));
            keyframes.push(json!({ "$id": kf_id, "type": "IMAGE", "length": 1, "symbol": Value::Null,
                "tweened": false, "tweenType": "LINEAR", "pluginMetadata": {} }));
            layers.push(json!({ "$id": layer_id, "name": format!("Image {}", slot), "type": "IMAGE",
                "keyframes": [kf_id], "hidden": false, "locked": false, "pluginMetadata": {} }));
            layer_ids.push(layer_id);
        }
        animations.push(json!({ "$id": uuid(char_id, &format!("anim_missing_{}", name)),
            "name": name, "layers": layer_ids, "pluginMetadata": {} }));
    }
    log::info!("Added {} missing template animation stub(s) under ====MISSING ANIMATIONS====: {}",
        missing.len(), missing.join(", "));
}

/// Double every keyframe `length` for the SSF2 30fps -> Fraymakers 60fps move (one source frame =
/// two FM frames). FrayTools timelines are laid out purely by sequential keyframe length, so
/// doubling each keyframe doubles every layer span + keyframe start in lockstep. Shared by the
/// stage emitter so stage/hazard animations get the SAME timing treatment as characters.
pub(crate) fn double_keyframe_lengths(keyframes: &mut [Value]) {
    for kf in keyframes {
        if let Some(len) = kf.get("length").and_then(Value::as_u64) {
            kf["length"] = json!(len * 2);
        }
    }
}

/// Remove a redundant trailing `return;` (the SSF2 returnvoid) from a frame-script body.
/// Only strips when `return` is a standalone final token, never an early `return` mid-script.
fn strip_trailing_return(body: &str) -> String {
    let t = body.trim_end();
    if let Some(pre) = t.strip_suffix("return;") {
        if pre.is_empty() || !pre.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_') {
            return pre.trim_end().to_string();
        }
    }
    body.to_string()
}

/// Remove a top-level (brace-depth 0) bare `return;` that is followed by more
/// statements in a frame script. hscript compiles a frame script as a statement
/// sequence, where code AFTER a `return` is unreachable and rejected at parse
/// time ("Unexpected token return"). The decompiler occasionally emits a spurious
/// unconditional `return;` mid-body (control-flow it couldn't structure), leaving
/// the real logic dead after it. Dropping the bare return keeps that logic — a
/// conditional early-exit (`if (c) { return; }`) is inside a block (depth > 0) and
/// is left untouched. Comment-only / blank tails don't count as "more statements".
/// Remove the source's loop-back call from the last frame of an animation that Fraymakers loops
/// on its own. SSF2 loops a stance by playing a label again from the final frame; the converted
/// animation expresses the same thing as `endType: LOOP`, so the ported call is redundant -- and
/// worse than redundant, because the label it names belongs to the un-sliced clip.
fn strip_loop_back_calls(body: &str) -> String {
    body.lines()
        .filter(|line| {
            let t = line.trim();
            !(t.starts_with("self.playLabel(") && t.ends_with(");"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn strip_unreachable_returns(body: &str) -> String {
    if !body.contains("return;") {
        return body.to_string();
    }
    fn strip_comments(line: &str) -> String {
        let b = line.as_bytes();
        let mut s = String::new();
        let mut i = 0;
        while i < b.len() {
            if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') { i += 1; }
                i += 2;
                continue;
            }
            if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' { break; }
            s.push(b[i] as char);
            i += 1;
        }
        s.trim().to_string()
    }
    let lines: Vec<&str> = body.lines().collect();
    let last_code = lines.iter().rposition(|l| !strip_comments(l).is_empty());
    let mut out: Vec<&str> = Vec::with_capacity(lines.len());
    let mut depth = 0i32;
    for (idx, line) in lines.iter().enumerate() {
        let code = strip_comments(line);
        let is_bare_return_depth0 = depth == 0 && code == "return;";
        depth += code.matches('{').count() as i32 - code.matches('}').count() as i32;
        if is_bare_return_depth0 && last_code.is_some_and(|lc| idx < lc) {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

/// Frame-doubling holds every keyframe for two frames, which also stretches each 1-frame
/// SCRIPT keyframe to two — making the script's frame span ambiguous. A frame script must
/// fire on exactly ONE frame, so after doubling we split every non-blank FRAME_SCRIPT
/// keyframe into a 1-frame script followed by a blank keyframe holding the remainder (so
/// total timing is preserved and the script is always followed by a blank or the anim end).
/// Calls that hand control away from the running animation. SSF2 executes a frame's code before
/// drawing it, so a frame carrying one of these is never displayed.

/// The SSF2 animation a Fraymakers animation was converted from.
///
/// A SPLIT piece has to ask for its parent: the mapping files say `a` becomes `jab`, never
/// `jab1`, so looking one up by the piece name finds nothing -- and "nothing" is indistinguishable
/// from "no scripts", which is how sandbag's jab ended up with no frame scripts at all and no idea
/// where SSF2 leaves it.
fn ssf2_source_name(data: &CharacterData, source_anim: &str, anim_name: &str) -> Option<String> {
    let mut names: Vec<&str> = vec![source_anim, anim_name];
    for n in [source_anim, anim_name] {
        if let Some(parent) = crate::extractor::split_parent(n) { names.push(parent); }
    }
    for want in names {
        if let Some((ssf2, _)) = data.ssf2_to_fm_anim.iter().find(|(_, fm)| fm.as_str() == want) {
            return Some(ssf2.clone());
        }
    }
    None
}

/// Calls that END the move. SSF2 executes a frame's code before drawing it, so a frame carrying
/// one of these is never displayed.
///
/// Only an explicit end counts. A `gotoAndStop` is just as likely to be a LOOP inside the move --
/// a smash's charge holds itself with one -- and treating that as an ending trims a frame the
/// original does draw, which showed up as a smash coming out SHORTER than its source.
const ANIM_TERMINATORS: &[&str] = &["endAttack"];

fn enforce_one_frame_scripts(layers: &mut [Value], keyframes: &mut Vec<Value>, char_id: &str) {
    let mut new_blanks: Vec<Value> = Vec::new();
    let mut counter = 0usize;
    for layer in layers.iter_mut() {
        if layer.get("type").and_then(Value::as_str) != Some("FRAME_SCRIPT") { continue; }
        let layer_id = layer.get("$id").and_then(Value::as_str).unwrap_or("").to_string();
        let ids: Vec<String> = match layer.get("keyframes").and_then(Value::as_array) {
            Some(a) => a.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            None => continue,
        };
        let mut new_ids: Vec<String> = Vec::with_capacity(ids.len());
        for id in ids {
            new_ids.push(id.clone());
            if let Some(kf) = keyframes.iter_mut().find(|k| k.get("$id").and_then(Value::as_str) == Some(&id)) {
                let has_code = !kf.get("code").and_then(Value::as_str).unwrap_or("").is_empty();
                let len = kf.get("length").and_then(Value::as_u64).unwrap_or(1);
                if has_code && len > 1 {
                    kf["length"] = json!(1);
                    let blank_id = uuid(char_id, &format!("kf_script_pad_{}_{}", layer_id, counter));
                    counter += 1;
                    new_blanks.push(json!({
                        "$id": blank_id,
                        "type": "FRAME_SCRIPT",
                        "length": len - 1,
                        "code": "",
                        "pluginMetadata": {}
                    }));
                    new_ids.push(blank_id);
                }
            }
        }
        layer["keyframes"] = json!(new_ids);
    }
    keyframes.extend(new_blanks);
}

pub fn generate_entity(
    data: &CharacterData,
    char_id: &str,
    sprite_boxes: &BTreeMap<String, AnimationBoxData>,
    img_result: &ImageExtractionResult,
    // How many of jab1..jab4 carry real image content. Decides which empty
    // animations are kept as placeholders for Script.hx jab-chain references.
    populated_jabs: usize,
) -> String {
    let mut keyframes: Vec<Value> = Vec::new();
    let mut layers: Vec<Value> = Vec::new();
    let mut symbols: Vec<Value> = Vec::new();
    let mut animations: Vec<Value> = Vec::new();

    // ── Build image asset GUID map (symbol_name → meta GUID) ──────────────────
    // Each image gets a deterministic GUID for its .meta file
    let mut image_guids: BTreeMap<String, String> = BTreeMap::new();
    for img in img_result.images.values() {
        let meta_guid = crate::uuid_gen::image_meta_guid(&img.symbol_name);
        image_guids.insert(img.symbol_name.clone(), meta_guid);
    }

    // IMAGE symbols are now created per-placement (per anim/frame) to carry correct
    // world-space x/y/scaleX/scaleY from the root MC transform + local sub-sprite matrix.
    // We collect them during the animation loop below.
    // (The old per-bitmap shared symbol approach is replaced.)

    // ── Build frame script lookup ─────────────────────────────────────────────
    let _frame_script_map: BTreeMap<String, String> = data.scripts.iter()
        .filter(|s| !s.is_ext_method)
        .map(|s| (s.name.clone(), s.code.clone()))
        .collect();

    // ── Apply animation splits ────────────────────────────────────────────────
    // The splitter expands multi-label SSF2 animations into separate FM animations.
    // Each SplitAnim carries (fm_name, source_anim, start_frame, end_frame, loop info).
    let split_anims = crate::anim_splitter::split_animations(&data.animations, sprite_boxes, data.stats.jump_startup.round().max(0.0) as u16, &data.scripts);

    for split in &split_anims {
        let anim_name = &split.fm_name;
        let source_anim = &split.source_anim;
        let anim_info = data.animations.get(source_anim)
            .or_else(|| data.animations.get(anim_name));
        // frame_count is the length of this split slice
        let split_len = if split.end_frame == u16::MAX {
            sprite_boxes.get(source_anim)
                .map(|sb| sb.total_frames as u32)
                .or_else(|| anim_info.map(|ai| ai.frames as u32))
                .unwrap_or(1)
                .saturating_sub(split.start_frame as u32)
        } else {
            (split.end_frame as u32).saturating_sub(split.start_frame as u32)
        };
        let base_len = split_len.max(1);
        let head = split.append_head_frames as u32;     // extra frames taken from source [0, head)
        let mut frame_count = base_len + head;           // total emitted frames (see the terminator
                                                         // trim in the frame-script pass below)
        let split_start_u = split.start_frame as u32;
        // Map a logical (emitted) frame index to its SOURCE frame: the slice maps to
        // [start, end); any appended frames wrap back to the source head [0, head).
        let src_frame = move |f: u32| -> u16 {
            if f < base_len { (split_start_u + f) as u16 } else { (f - base_len) as u16 }
        };
        // A frame whose code ENDS the move is never SEEN in SSF2, so it must not be emitted.
        //
        // The two engines run a frame's code on opposite sides of drawing it. SSF2 runs the code
        // FIRST, so a frame carrying `endAttack` (or a goto) hands control away before anything is
        // drawn: the move is already over and that frame never appears. Fraymakers runs a frame's
        // code AFTER drawing it, so emitting the same frame puts a pose on screen the original
        // never showed and the converted move lasts one source frame longer than its source.
        //
        // This has to happen before ANY layer is built. Every layer is laid out against
        // `frame_count`, and the engine takes an animation's length from its longest layer, so
        // trimming only some of them leaves the animation exactly as long as it was and merely
        // inconsistent.
        if frame_count >= 2 {
            let ssf2_name = ssf2_source_name(data, source_anim, anim_name);
            let frame_offset = sprite_boxes.get(source_anim.as_str())
                .map(|sb| sb.sprite_frame_offset as u32).unwrap_or(0) + split.start_frame as u32;
            if let Some(ssf2) = ssf2_name {
                let want = frame_offset + frame_count - 1;
                let prefix = format!("{}__frame", ssf2);
                let ends = data.scripts.iter().any(|sc| {
                    !sc.is_ext_method
                        && sc.name.strip_prefix(&prefix).and_then(|r| r.parse::<u32>().ok()) == Some(want)
                        && ANIM_TERMINATORS.iter().any(|t| sc.code.contains(t))
                });
                if ends { frame_count -= 1; }
            }
        }

        let anim_id = uuid(char_id, &format!("anim_{}", anim_name));
        let mut anim_layer_ids: Vec<String> = Vec::new();
        // IMAGE layers are collected separately so they can be emitted FIRST in
        // the final layer list. In FrayTools, lower index = drawn first = behind;
        // putting image layers before the collision boxes (hurt/hit/body/item/etc.)
        // keeps the sprite behind the boxes so they stay visible when editing.
        let mut anim_image_layer_ids: Vec<String> = Vec::new();

        // ── 1. LABEL layer ────────────────────────────────────────────────────
        // Frame 0 always gets the FM animation name as a label.
        // Additional SSF2 inner sprite labels are placed at their original frame offsets.
        {
            let layer_id = uuid(char_id, &format!("layer_label_{}", anim_name));
            let mut label_kf_ids: Vec<Value> = Vec::new();

            // Use pre-sliced labels from the splitter (already rebased to this split's frame 0)
            // Frame 0 always gets the FM animation name; splitter labels augment from there.
            let mut all_labels: Vec<(u16, String)> = vec![(0, anim_name.to_string())];
            for (lbl, frame) in &split.labels {
                if *frame == 0 { continue; } // don't overwrite the fm_name at frame 0
                all_labels.push((*frame, lbl.clone()));
            }
            all_labels.sort_by_key(|(f, _)| *f);
            all_labels.dedup_by_key(|(f, _)| *f);

            // Emit LABEL keyframes with correct lengths to fill the timeline
            let mut cursor: u32 = 0;
            for (i, (frame, label)) in all_labels.iter().enumerate() {
                let f = *frame as u32;
                // Gap before this label (unlabeled frames)
                if f > cursor {
                    let gap_id = uuid(char_id, &format!("kf_label_gap_{}_{}", anim_name, cursor));
                    keyframes.push(json!({
                        "$id": gap_id,
                        "type": "LABEL",
                        "length": f - cursor,
                        "name": "",
                        "pluginMetadata": {}
                    }));
                    label_kf_ids.push(gap_id.into());
                }
                // This label's keyframe — length extends to next label or end of animation
                let next_frame = all_labels.get(i + 1).map(|(nf, _)| *nf as u32).unwrap_or(frame_count);
                let length = (next_frame - f).max(1);
                let kf_id = uuid(char_id, &format!("kf_label_{}_f{}", anim_name, f));
                keyframes.push(json!({
                    "$id": kf_id,
                    "type": "LABEL",
                    "length": length,
                    "name": label,
                    "pluginMetadata": {}
                }));
                label_kf_ids.push(kf_id.into());
                cursor = f + length;
            }

            layers.push(json!({
                "$id": layer_id,
                "name": "Labels",
                "type": "LABEL",
                "keyframes": label_kf_ids,
                "hidden": false,
                "locked": false,
                "pluginMetadata": {}
            }));
            anim_layer_ids.push(layer_id);
        }

        // ── 2. FRAME_SCRIPT layer ─────────────────────────────────────────────
        // Each script gets a 1-frame keyframe. Frames without code get blank
        // keyframes (symbol: null). This matches FrayTools' expectation that
        // script keyframes fire once on their start frame only.
        {
            let layer_id = uuid(char_id, &format!("layer_script_{}", anim_name));

            // Find the SSF2 name (a split piece resolves through its parent).
            let ssf2_name = ssf2_source_name(data, source_anim, anim_name);

            // Frame offset = sprite base offset + split start frame
            let sprite_frame_offset = sprite_boxes.get(source_anim.as_str())
                .map(|sb| sb.sprite_frame_offset as u32)
                .unwrap_or(0);
            let split_start = split.start_frame as u32;
            let frame_offset = sprite_frame_offset + split_start;

            // Collect per-frame scripts within the split's frame range
            let mut frame_code: BTreeMap<u32, String> = BTreeMap::new();
            if let Some(ref ssf2) = ssf2_name {
                let prefix = format!("{}__frame", ssf2);
                for script in &data.scripts {
                    if script.is_ext_method { continue; }
                    if let Some(rest) = script.name.strip_prefix(&prefix) {
                        if let Ok(global_frame) = rest.parse::<u32>() {
                            if global_frame >= frame_offset {
                                let local_frame = global_frame - frame_offset;
                                if local_frame < frame_count {
                                    // Frame-script bodies run through the same
                                    // SSF2 → Fraymakers conversion as Script.hx:
                                    // (1) double frame-count args flagged in
                                    //     commands.jsonc `frame_params` —
                                    //     SSF2 names like `hitStun:` are still
                                    //     matchable here, so this must run
                                    //     BEFORE the translation;
                                    // (2) `translate_ssf2_to_fm` applies the
                                    //     commands.jsonc literal replacements
                                    //     (API renames, self.self fixups,
                                    //     hitbox field renames, …) so frame
                                    //     scripts speak the same FM dialect as
                                    //     Script.hx.
                                    let body = extract_function_body(&script.code);
                                    // `double_frame_counts` is now the first
                                    // step inside translate_ssf2_to_fm so
                                    // Script.hx + ext-methods + entity frame
                                    // scripts all get the same 30→60fps
                                    // treatment from a single source of truth.
                                    let body = crate::api_mappings::translate_ssf2_to_fm(&body);
                                    let mut var_types = crate::api_mappings::infer_ext_var_types(
                                        &data.ext_vars, &data.ext_var_inits);
                                    // Reserved names (self/match) are NOT wrapped — they
                                    // resolve to the engine globals (mirrors haxe_gen).
                                    for r in crate::api_mappings::RESERVED_EXT_VARS { var_types.remove(*r); }
                                    // (1) own script-function refs `self.<fn>` -> bare `<fn>`
                                    // (frame scripts share Script.hx's scope), then
                                    // (2) instance-var refs -> persistent .get()/.set().
                                    let ext_methods: Vec<String> = data.scripts.iter()
                                        .filter(|s| s.is_ext_method).map(|s| s.name.clone())
                                        .filter(|n| !data.ext_vars.contains(n)).collect();
                                    let body = crate::api_mappings::rewrite_own_method_refs(&body, &ext_methods);
                                    let body = crate::api_mappings::wrap_persistent_state(
                                        &body, &var_types);
                                    // Drop a redundant trailing `return;` — a frame script
                                    // is a statement block, not a function, so the final
                                    // `return;` (from the SSF2 returnvoid) is noise.
                                    let body = strip_trailing_return(&body);
                                    // Also drop a spurious mid-body `return;` that
                                    // leaves real code unreachable (a frame script is
                                    // a statement sequence; code after `return` is a
                                    // parse error). See strip_unreachable_returns.
                                    let body = strip_unreachable_returns(&body);
                                    // An animation that LOOPS carries the source's own loop-back
                                    // (`stancePlayFrame("again")` -> `playLabel("again")`) on its
                                    // last frame. Fraymakers loops it natively via endType LOOP,
                                    // and the label the call names was sliced off with the entry,
                                    // so leaving it in is a call to a label that no longer exists:
                                    // it threw once per cycle and never looped anything.
                                    let body = if split.loop_tail && local_frame + 1 == frame_count {
                                        strip_loop_back_calls(&body)
                                    } else {
                                        body
                                    };
                                    if body.trim().is_empty() { continue; }
                                    frame_code.insert(local_frame, body);
                                }
                            }
                        }
                    }
                }
            }

            // ── Frame-script post-processing (per-animation) ─────────────────────
            // (4) Freefall / special-fall states carry no frame scripts.
            if matches!(split.fm_name.as_str(),
                "airdash_freefall" | "airdash_freefall_whiff" | "fall_special") {
                frame_code.clear();
            }
            if frame_count >= 1 {
                let last = frame_count - 1;
                // (5) Strip self.endAnimation() from the last two frames — the animation
                //     already ends there; an explicit endAnimation just cuts it short.
                for f in [last, last.saturating_sub(1)] {
                    if let Some(code) = frame_code.get_mut(&f) {
                        *code = code.replace("self.endAnimation();", "").replace("self.endAnimation()", "");
                    }
                }
                // (6) Move any code on the second-to-last frame onto the final frame.
                if frame_count >= 2 {
                    let second_last = frame_count - 2;
                    if let Some(moved) = frame_code.remove(&second_last) {
                        let moved = moved.trim().to_string();
                        if !moved.is_empty() {
                            let e = frame_code.entry(last).or_default();
                            *e = if e.trim().is_empty() { moved }
                                 else { format!("{}\n{}", moved, e.trim_start()) };
                            // The merge can append code after a `return;` that was
                            // trailing in `moved` — re-strip so the joined script parses.
                            *e = strip_unreachable_returns(e);
                        }
                    }
                }
                // Drop entries emptied by the endAnimation strip so they become blank frames.
                frame_code.retain(|_, c| !c.trim().is_empty());
            }

            // Build keyframe sequence: for each frame with code, emit a 1-frame
            // script keyframe. Fill gaps with blank keyframes. After the last
            // script frame, fill remaining frames with a blank keyframe.
            let mut script_kf_ids: Vec<String> = Vec::new();
            let mut cursor: u32 = 0;

            let script_frames: Vec<u32> = frame_code.keys().copied().collect();
            for &sf in &script_frames {
                // Gap before this script frame
                if sf > cursor {
                    let gap_kf_id = uuid(char_id, &format!("kf_script_gap_{}_{}", anim_name, cursor));
                    keyframes.push(json!({
                        "$id": gap_kf_id,
                        "type": "FRAME_SCRIPT",
                        "length": sf - cursor,
                        "code": "",
                        "pluginMetadata": {}
                    }));
                    script_kf_ids.push(gap_kf_id);
                }
                // Script keyframe (1 frame)
                let kf_id = uuid(char_id, &format!("kf_script_{}_f{}", anim_name, sf));
                keyframes.push(json!({
                    "$id": kf_id,
                    "type": "FRAME_SCRIPT",
                    "length": 1,
                    "code": frame_code[&sf],
                    "pluginMetadata": {}
                }));
                script_kf_ids.push(kf_id);
                cursor = sf + 1;
            }
            // Trailing blank to fill remaining frames
            if cursor < frame_count {
                let tail_kf_id = uuid(char_id, &format!("kf_script_tail_{}", anim_name));
                keyframes.push(json!({
                    "$id": tail_kf_id,
                    "type": "FRAME_SCRIPT",
                    "length": frame_count - cursor,
                    "code": "",
                    "pluginMetadata": {}
                }));
                script_kf_ids.push(tail_kf_id);
            }

            // If no scripts at all, still need one blank keyframe spanning the animation
            if script_kf_ids.is_empty() {
                let kf_id = uuid(char_id, &format!("kf_script_empty_{}", anim_name));
                keyframes.push(json!({
                    "$id": kf_id,
                    "type": "FRAME_SCRIPT",
                    "length": frame_count,
                    "code": "",
                    "pluginMetadata": {}
                }));
                script_kf_ids.push(kf_id);
            }

            layers.push(json!({
                "$id": layer_id,
                "name": "Scripts",
                "type": "FRAME_SCRIPT",
                "keyframes": script_kf_ids,
                "hidden": false,
                "locked": false,
                "language": "",
                "pluginMetadata": {}
            }));
            anim_layer_ids.push(layer_id);
        }

        // The launched-state slots (tumble, hurt_heavy) anchor the BODY on the
        // entity position -- the official character template's convention -- but
        // their SSF2 source xframes carry root-timeline stance placements that
        // are preview positions, not runtime anchors (mario's "flying" sits a
        // full body-height BELOW the origin; every grounded xframe sits feet-at-
        // origin). Correction: move the hurtbox union's centre onto the origin
        // and shift the whole box set (hurtboxes, itemboxes, ECB) by that one
        // delta. The IMAGE placement applies the same correction (tumble
        // re-centres per-frame because it also strips rotation; hurt_heavy
        // translates by this exact shift, keeping art-box registration intact).
        // Every other animation keeps its placement and boxes verbatim.
        let box_shift: (f64, f64) = if anim_name == "tumble" || anim_name == "hurt_heavy" {
            let mut aabb: Option<(f64, f64, f64, f64)> = None; // left,right,top,bottom
            if let Some(bd) = sprite_boxes.get(source_anim.as_str()) {
                for lf in 0..frame_count {
                    if let Some(boxes) = bd.frames.get(&src_frame(lf)) {
                        for b in boxes {
                            if b.box_type != crate::sprite_parser::BoxType::Hurtbox { continue; }
                            let (l, r, t, btm) = (b.x, b.x + b.width, b.y, b.y + b.height);
                            aabb = Some(match aabb {
                                None => (l, r, t, btm),
                                Some((al, ar, at, ab)) => (al.min(l), ar.max(r), at.min(t), ab.max(btm)),
                            });
                        }
                    }
                }
            }
            aabb.map(|(l, r, t, b)| (-(l + r) / 2.0, -(t + b) / 2.0)).unwrap_or((0.0, 0.0))
        } else {
            (0.0, 0.0)
        };

        // ── 3. COLLISION_BODY layer (per-frame ECB from hurtbox bounds) ───────
        {
            let layer_id = uuid(char_id, &format!("layer_body_{}", anim_name));

            // FrayTools draws the COLLISION_BODY (ECB) as a 4-vertex diamond:
            //   foot vertex (0, -foot),  head vertex (0, -head),
            //   hip vertices (±hipWidth/2 + hipXOffset, -(head+foot)/2 + hipYOffset)
            // foot/head are heights measured UP from the character origin; the
            // hip's vertical position is an offset from the foot/head midpoint.
            // The foot and head vertices are pinned to x = 0.
            //
            // Per frame we fit the diamond to the union AABB of that frame's
            // HURTBOX-typed boxes so its four vertices touch the box's four
            // edges: foot at the bottom, head at the top, hips at the sides at
            // mid-height. hipXOffset re-centres the hip on the box; hipYOffset
            // stays 0 (hip at the vertical midpoint).
            let default_body = (0.0_f64, 86.0_f64, 40.0_f64, 0.0_f64); // foot,head,hipWidth,hipXOffset

            let mut per_frame: Vec<(f64, f64, f64, f64)> = Vec::with_capacity(frame_count as usize);
            {
                let box_data = sprite_boxes.get(source_anim.as_str());
                // Per-frame body from the frame's HURTBOX union, or None when the
                // frame has NO hurtboxes at all.
                let raw: Vec<Option<(f64, f64, f64, f64)>> = (0..frame_count).map(|f| {
                    let src_f = src_frame(f);
                    let mut aabb: Option<(f64, f64, f64, f64)> = None; // left,right,top,bottom
                    if let Some(boxes) = box_data.and_then(|bd| bd.frames.get(&src_f)) {
                        for b in boxes {
                            if b.box_type != crate::sprite_parser::BoxType::Hurtbox { continue; }
                            let (l, r) = (b.x + box_shift.0, b.x + b.width + box_shift.0);
                            let (t, btm) = (b.y + box_shift.1, b.y + b.height + box_shift.1);
                            aabb = Some(match aabb {
                                None => (l, r, t, btm),
                                Some((al, ar, at, ab)) => (al.min(l), ar.max(r), at.min(t), ab.max(btm)),
                            });
                        }
                    }
                    aabb.map(|(l, r, t, btm)| (
                        round2(-btm),          // foot  = up-height of the bottom edge
                        round2(-t),            // head  = up-height of the top edge
                        round2(r - l),         // hipWidth = box width
                        round2((l + r) / 2.0), // hipXOffset = box centre x
                    ))
                }).collect();
                // A frame with NO hurtboxes keeps the ECB exactly as it was — hold the
                // previous body so the RLE below emits NO new keyframe for it (the ECB
                // persists until the next hurtbox frame). Leading no-hurtbox frames
                // backfill from the first real body, so the ECB never shows a default
                // diamond mid-animation. All-empty animation → default.
                let first_real = raw.iter().flatten().next().copied();
                let mut last = first_real.unwrap_or(default_body);
                for b in &raw {
                    if let Some(bb) = b { last = *bb; }
                    per_frame.push(last);
                }
            }

            // Run-length encode consecutive frames with an identical body.
            let mut body_kf_ids: Vec<String> = Vec::new();
            let mut f: u32 = 0;
            while f < frame_count {
                let body = per_frame[f as usize];
                let mut run = 1u32;
                while (f + run) < frame_count {
                    let nb = per_frame[(f + run) as usize];
                    let same = (nb.0 - body.0).abs() < 0.01 && (nb.1 - body.1).abs() < 0.01
                        && (nb.2 - body.2).abs() < 0.01 && (nb.3 - body.3).abs() < 0.01;
                    if same { run += 1; } else { break; }
                }
                let sym_id = uuid(char_id, &format!("sym_body_{}_{}", anim_name, f));
                let kf_id = uuid(char_id, &format!("kf_body_{}_{}", anim_name, f));
                // The ECB is a 4-vertex diamond: foot (0,-foot), head (0,-head), hips
                // (hipXOffset ± hipWidth/2, midY). The foot/head verts are PINNED to
                // x=0, so for a convex diamond the hips must straddle x=0, i.e.
                // |hipXOffset| < hipWidth/2. When the hurtbox union is far off-centre
                // (e.g. zelda fall, centre ~-38 vs width ~38) the raw offset pushes both
                // hips to one side and the shape collapses into an ARROW — which renders
                // wrong and crashes FrayTools on edit. Clamp the offset to keep the hips
                // straddling the centre (a comfortable margin) so it's always a valid
                // diamond.
                let half_w = (body.2 / 2.0).max(0.0);
                let max_off = (half_w - half_w * 0.25).max(0.0); // hips keep ≥25% past centre
                let hip_x_offset = round2(body.3.clamp(-max_off, max_off));
                symbols.push(json!({
                    "$id": sym_id,
                    "alpha": Value::Null,
                    "color": Value::Null,
                    "foot": body.0,
                    "head": body.1,
                    "hipWidth": body.2,
                    "hipXOffset": hip_x_offset,
                    "hipYOffset": 0,
                    "pluginMetadata": {},
                    "type": "COLLISION_BODY"
                }));
                keyframes.push(json!({
                    "$id": kf_id,
                    "type": "COLLISION_BODY",
                    "length": run,
                    "symbol": sym_id,
                    "tweened": false,
                    "tweenType": "LINEAR",
                    "pluginMetadata": {}
                }));
                body_kf_ids.push(kf_id);
                f += run;
            }

            layers.push(json!({
                "$id": layer_id,
                "name": "Body",
                "type": "COLLISION_BODY",
                "keyframes": body_kf_ids,
                "hidden": false,
                "locked": false,
                "defaultAlpha": 0.5,
                "defaultColor": "0xffa500",
                "defaultFoot": 0,
                "defaultHead": 86,
                "defaultHipWidth": 40,
                "defaultHipXOffset": 0,
                "defaultHipYOffset": 0,
                "pluginMetadata": {}
            }));
            anim_layer_ids.push(layer_id);
        }

        // ── 4. COLLISION_BOX layers ───────────────────────────────────────────
        if let Some(anim_box_data) = sprite_boxes.get(source_anim.as_str()) {
            // Walk logical frames 0..frame_count and resolve each to its source frame (handles
            // both the [start,end) slice and any appended head frames for a wrapped loop).
            let mut instances_in_anim: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for lf in 0..frame_count {
                if let Some(boxes) = anim_box_data.frames.get(&src_frame(lf)) {
                    for b in boxes {
                        instances_in_anim.insert(b.instance_name.clone());
                    }
                }
            }

            for inst_name in instances_in_anim.iter() {
                let box_type = BoxType::from_instance_name(inst_name).unwrap_or(BoxType::Hurtbox);
                let fm_box_type = box_type_to_fm(box_type);
                let color = box_color(box_type);
                // Convert SSF2 instance name to FM layer name (hitBox→hurtbox0, attackBox→hitbox0)
                let fm_layer_name = ssf2_box_name_to_fm(inst_name);
                let box_idx = fm_box_index(&fm_layer_name);
                let layer_id = uuid(char_id, &format!("layer_box_{}_{}", anim_name, inst_name));

                let mut box_kf_ids: Vec<String> = Vec::new();
                let total = frame_count; // use split length, not source total
                let mut frame_idx: u32 = 0;

                // Collect this instance's boxes per logical frame (rebased to 0..frame_count).
                let mut active_frames: Vec<(u16, &crate::sprite_parser::FrameBox)> = Vec::new();
                for lf in 0..frame_count {
                    if let Some(boxes) = anim_box_data.frames.get(&src_frame(lf)) {
                        if let Some(b) = boxes.iter().find(|b| b.instance_name == *inst_name) {
                            active_frames.push((lf as u16, b));
                        }
                    }
                }
                active_frames.sort_by_key(|(f, _)| *f);

                if active_frames.is_empty() { continue; }

                // Build runs: merge consecutive frames with identical geometry
                // into one keyframe; a gap in frame numbers means a blank keyframe.
                // This correctly handles RemoveObject (box absent on frame N even
                // if it was present on N-1 and reappears on N+1).
                let is_point = box_type == crate::sprite_parser::BoxType::GrabHoldBox;
                let kf_type = if is_point { "POINT" } else { "COLLISION_BOX" };

                let mut i = 0;
                while i < active_frames.len() {
                    let (start_frame, fb) = active_frames[i];

                    // Merge consecutive frames with the same box geometry
                    let mut run_end = i; // inclusive index of last frame in run
                    while run_end + 1 < active_frames.len() {
                        let (next_f, next_fb) = active_frames[run_end + 1];
                        let (cur_f, _) = active_frames[run_end];
                        // Must be exactly the next frame AND same geometry
                        let consecutive = next_f == cur_f + 1;
                        let same_geom = (next_fb.x - fb.x).abs() < 0.01
                            && (next_fb.y - fb.y).abs() < 0.01
                            && (next_fb.width - fb.width).abs() < 0.01
                            && (next_fb.height - fb.height).abs() < 0.01
                            && (next_fb.rotation - fb.rotation).abs() < 0.01;
                        if consecutive && same_geom {
                            run_end += 1;
                        } else {
                            break;
                        }
                    }
                    let run_len = (active_frames[run_end].0 - start_frame + 1) as u32;

                    // Gap keyframe (blank) before this run
                    if start_frame as u32 > frame_idx {
                        let gap_kf_id = uuid(char_id, &format!("kf_box_gap_{}_{}_{}", anim_name, inst_name, frame_idx));
                        keyframes.push(json!({
                            "$id": gap_kf_id,
                            "type": kf_type,
                            "length": (start_frame as u32) - frame_idx,
                            "symbol": Value::Null,
                            "tweened": false,
                            "tweenType": "LINEAR",
                            "pluginMetadata": {}
                        }));
                        box_kf_ids.push(gap_kf_id);
                        // frame_idx updated at end of loop body
                    }

                    let sym_id = uuid(char_id, &format!("sym_box_{}_{}_{}", anim_name, inst_name, start_frame));

                    if is_point {
                        // POINT symbol: bottom-center of the touchBox.
                        // In SSF2, touchBox marks the hold region; the grab hold
                        // position is at the bottom-center (where the opponent's feet anchor).
                        let cx = round2(fb.x + box_shift.0 + fb.width / 2.0);
                        let cy = round2(fb.y + box_shift.1 + fb.height);
                        symbols.push(json!({
                            "$id": sym_id,
                            "alpha": 1,
                            "color": color,
                            "pluginMetadata": {},
                            "rotation": 0,
                            "type": "POINT",
                            "x": cx,
                            "y": cy
                        }));
                    } else {
                        // COLLISION_BOX symbol. FrayTools rotates a box around
                        // (x + pivotX, y + pivotY) and HONORS off-center pivots
                        // (verified: a 41x21 menu box round-trips through FrayTools
                        // keeping pivot (18,18), not the center (20.5,10.5)). So we
                        // place the pivot at the true SSF2 anchor:
                        //   - itemBox → bottom-center (the hand attachment point),
                        //     so the box sweeps around the hand as it rotates.
                        //   - every other box → geometric center.
                        // (x, y) is the un-rotated top-left (Theory A, confirmed by
                        // matching framy hurtboxes against the body image).
                        let (pivot_x, pivot_y) = if box_type == crate::sprite_parser::BoxType::ItemBox {
                            (fb.width / 2.0, fb.height)        // bottom-center = hand
                        } else {
                            (fb.width / 2.0, fb.height / 2.0)  // center
                        };
                        // SWF `atan2(b, a)` in y-down is CW-positive; FrayTools uses
                        // the same convention (see f472a2dd for the IMAGE path), so
                        // emit the angle as-is, normalized to 0-360.
                        let theta = ((fb.rotation % 360.0) + 360.0) % 360.0;

                        // ── itemBox anchor bake (measured, not guessed) ──
                        // The itemBox is the only box that carries rotation (all
                        // others are AABB-collapsed to 0 in sprite_parser).
                        // FrayTools renders a COLLISION_BOX's registration anchor
                        // at  s + R(-θ)·pivot  (docs/fraytools_internals.md §2) —
                        // it MOVES with θ. Emitting the raw top-left lets the hand
                        // drift up to ~49px on rotated frames (measured by
                        // `probe_itembox`). anchor(x,y) is affine in (x,y), so to
                        // pin the hand we solve  topleft = hand − anchor(0,0,pivot,θ).
                        // At θ=0 this collapses to (fb.x, fb.y), so non-rotated
                        // boxes stay byte-identical.
                        let (box_x, box_y) = if box_type == crate::sprite_parser::BoxType::ItemBox {
                            let hand = crate::fraytools_transform::intended_pivot_point(
                                fb.x, fb.y, pivot_x, pivot_y);
                            let off = crate::fraytools_transform::collision_box_anchor(
                                0.0, 0.0, pivot_x, pivot_y, theta);
                            (hand.0 - off.0, hand.1 - off.1)
                        } else {
                            (fb.x, fb.y)
                        };
                        symbols.push(json!({
                            "$id": sym_id,
                            "alpha": 0.5,
                            "color": color,
                            "pivotX": round2(pivot_x),
                            "pivotY": round2(pivot_y),
                            "pluginMetadata": {},
                            "rotation": round2(theta),
                            "scaleX": round2(fb.width),
                            "scaleY": round2(fb.height),
                            "type": "COLLISION_BOX",
                            "x": round2(box_x + box_shift.0),
                            "y": round2(box_y + box_shift.1)
                        }));
                    }

                    let kf_id = uuid(char_id, &format!("kf_box_{}_{}_{}", anim_name, inst_name, start_frame));
                    keyframes.push(json!({
                        "$id": kf_id,
                        "type": kf_type,
                        "length": run_len,
                        "symbol": sym_id,
                        "tweened": false,
                        "tweenType": "LINEAR",
                        "pluginMetadata": {}
                    }));
                    box_kf_ids.push(kf_id);
                    frame_idx = start_frame as u32 + run_len;
                    i = run_end + 1;
                }

                // Tail gap
                if frame_idx < total {
                    let tail_kf_id = uuid(char_id, &format!("kf_box_tail_{}_{}_{}", anim_name, inst_name, frame_idx));
                    keyframes.push(json!({
                        "$id": tail_kf_id,
                        "type": kf_type,
                        "length": total - frame_idx,
                        "symbol": Value::Null,
                        "tweened": false,
                        "tweenType": "LINEAR",
                        "pluginMetadata": {}
                    }));
                    box_kf_ids.push(tail_kf_id);
                }

                if box_kf_ids.is_empty() { continue; }

                if box_type == crate::sprite_parser::BoxType::GrabHoldBox {
                    // grabHoldPoint uses a POINT layer, not COLLISION_BOX
                    layers.push(json!({
                        "$id": layer_id,
                        "name": fm_layer_name,
                        "type": "POINT",
                        "keyframes": box_kf_ids,
                        "hidden": false,
                        "locked": false,
                        "pluginMetadata": {
                            "com.fraymakers.FraymakersMetadata": {
                                "pointType": "GRAB_HOLD_POINT"
                            }
                        }
                    }));
                } else {
                    layers.push(json!({
                        "$id": layer_id,
                        "name": fm_layer_name,
                        "type": "COLLISION_BOX",
                        "keyframes": box_kf_ids,
                        "hidden": false,
                        "locked": false,
                        "defaultAlpha": 0.5,
                        "defaultColor": color,
                        "pluginMetadata": {
                            "com.fraymakers.FraymakersMetadata": {
                                "collisionBoxType": fm_box_type,
                                "index": box_idx
                            }
                        }
                    }));
                }
                anim_layer_ids.push(layer_id);
            }
        }

        // ── 5. IMAGE layers (one per depth slot, back-to-front) ────────────────────
        {
            // Every animation addresses the SAME number of slots: the deepest any of them needs.
            // The ones it uses carry its images; the rest are blanked below.
            let entity_image_slots = img_result.anim_images.values()
                .map(|a| a.max_depth_slots.max(1)).max().unwrap_or(1);
            let num_slots = img_result.anim_images.get(source_anim.as_str())
                .map(|a| a.max_depth_slots.max(1))
                .unwrap_or(1);

            for slot in 0..num_slots {
                let img_layer_id = uuid(char_id, &format!("layer_image_{}_{}", anim_name, slot));
                let mut img_kf_ids: Vec<String> = Vec::new();

                if let Some(anim_imgs) = img_result.anim_images.get(source_anim.as_str()) {
                    let total = frame_count;

                    // LOOP the visual timeline like Flash does for a nested MovieClip
                    // whose frame count is shorter than the parent (gameplay) timeline.
                    // SSF2's `fall` is an 8-frame looping sprite running under a 20-frame
                    // hurtbox timeline, so source frames past the sprite's last frame wrap
                    // modulo its length (frame 8 → sprite frame 0) instead of coming back
                    // empty and making the character vanish mid-animation. When the sprite
                    // and gameplay timelines are equal length this is the identity.
                    let img_len = anim_imgs.frames.keys().max()
                        .map(|m| *m as u32 + 1).unwrap_or(1).max(1);
                    let mut held: Vec<Option<crate::image_extractor::FrameImageEntry>> = (0..total)
                        .map(|f| {
                            let looped = (src_frame(f) as u32 % img_len) as u16;
                            anim_imgs.frames.get(&looped).and_then(|v| v.get(slot)).cloned()
                        })
                        .collect();
                    // An exit slot filled from its entry animation plays the entry BACKWARDS.
                    if split.reversed { held.reverse(); }

                    let mut f: u32 = 0;
                    while f < total {
                        let entry = held[f as usize].as_ref();

                        // Key for run-length: same symbol AND same world transform
                        let sym_name = entry.map(|e| e.symbol_name.as_str());
                        let shape_id = entry.map(|e| e.shape_id);
                        let world_tx  = entry.map(|e| round2(e.world_tx)).unwrap_or(0.0);
                        let world_ty  = entry.map(|e| round2(e.world_ty)).unwrap_or(0.0);
                        let world_sx  = entry.map(|e| round2(e.world_sx)).unwrap_or(1.0);
                        let world_sy  = entry.map(|e| round2(e.world_sy)).unwrap_or(1.0);
                        let world_rot = entry.map(|e| round2(e.world_rotation)).unwrap_or(0.0);

                        // Run-length encode consecutive frames with identical symbol + world transform.
                        let mut run = 1u32;
                        while f + run < total {
                            let next = held[(f + run) as usize].as_ref();
                            let matches = next.map(|e| e.symbol_name.as_str()) == sym_name
                                && next.map(|e| round2(e.world_tx))       == sym_name.is_some().then_some(world_tx)
                                && next.map(|e| round2(e.world_ty))       == sym_name.is_some().then_some(world_ty)
                                && next.map(|e| round2(e.world_sx))       == sym_name.is_some().then_some(world_sx)
                                && next.map(|e| round2(e.world_sy))       == sym_name.is_some().then_some(world_sy)
                                && next.map(|e| round2(e.world_rotation)) == sym_name.is_some().then_some(world_rot);
                            if matches { run += 1; } else { break; }
                        }

                        let kf_id = uuid(char_id, &format!("kf_image_{}_s{}_f{}", anim_name, slot, f));

                        // Resolve bitmap and create a per-placement IMAGE symbol with world coords
                        let bitmap_img = shape_id.and_then(|sid| {
                            let bmp_id = img_result.shape_to_bitmap.get(&sid).copied().unwrap_or(sid);
                            img_result.images.get(&bmp_id)
                        }).or_else(|| {
                            sym_name.and_then(|sn| {
                                img_result.images.values().find(|img| img.symbol_name == sn)
                            })
                        });

                        let symbol_ref = if let Some(img) = bitmap_img {
                            // Create a per-placement symbol (unique per anim/slot/frame)
                            let per_placement_sym_id = uuid(char_id,
                                &format!("sym_img_{}_s{}_f{}", anim_name, slot, f));
                            let meta_guid = image_guids.get(&img.symbol_name)
                                .cloned().unwrap_or_default();

                            // FrayTools IMAGE-symbol model:
                            //   world(L) = (x, y) + R(rotation)·diag(sx,sy)·L
                            // The bitmap's local (0,0) is placed at (x, y) and the
                            // image rotates/scales around it.
                            //
                            // The SWF places the bitmap with world matrix
                            //   world_pt = M·L + (tx, ty),  M = R(rot)·diag(sx,sy).
                            // Faithfully reproducing it — x=tx, y=ty, rotation/scale
                            // from the decomposition — is exact for every frame, so a
                            // spin (orbiting translation + rotation) stays anchored
                            // automatically, with no special-casing.
                            //
                            // For a DefineShape with a bitmap fill the bitmap is
                            // offset inside the shape; shape_pivot is that offset, so
                            // the bitmap origin lands at M·shape_pivot.
                            //
                            // Preserve sign: negative scaleX/scaleY = flip, which FrayTools supports.
                            // stand_turn has no SSF2 sprite — it's the idle pose mirrored
                            // horizontally in place, so negate scaleX for it.
                            // Fold the shape's bitmap-FILL scale into the placement.
                            // SSF2 references a high-res bitmap through a DefineShape whose
                            // fill matrix scales it (often far down). The PNG is at native
                            // res, so without this the sprite renders 1/fill too big (e.g.
                            // zelda's shared magic particle at fill 0.078 → ~13× oversized,
                            // the screen-filling nair effect). M_place·M_fill composes to
                            // scale (world_sx·fsx, world_sy·fsy) with the same rotation, and
                            // the stored shape_pivot (= fill_tx / fsx) × fsx recovers the
                            // true fill translation in pixels. Absent fill (vector shapes,
                            // 1:1 bitmaps) → (1,1), so body sprites are untouched.
                            let (fsx, fsy) = shape_id
                                .and_then(|sid| img_result.shape_fill_scale.get(&sid))
                                .copied()
                                .unwrap_or((1.0, 1.0));
                            // A turn animation is drawn facing the direction being turned FROM,
                            // and fraymakers flips the character's facing when the turn STARTS, so
                            // the art needs mirroring for the whole animation -- not just its first
                            // frame, which left the rest of a run turn facing backwards.
                            let turn_flip = anim_name == "stand_turn" || anim_name == "run_turn";
                            let fm_sx = round2(world_sx * fsx) * if turn_flip { -1.0 } else { 1.0 };
                            let fm_sy = round2(world_sy * fsy);

                            // World-space matrix components
                            let wa = entry.map(|e| e.world_a).unwrap_or(1.0);
                            let wb = entry.map(|e| e.world_b).unwrap_or(0.0);
                            let wc = entry.map(|e| e.world_c).unwrap_or(0.0);
                            let wd = entry.map(|e| e.world_d).unwrap_or(1.0);

                            let (off_x, off_y) = shape_id
                                .and_then(|sid| img_result.shape_pivot.get(&sid))
                                .copied()
                                .unwrap_or((0.0, 0.0));
                            let (off_x, off_y) = (off_x * fsx, off_y * fsy);
                            let mut fm_x = round2(world_tx + wa * off_x + wc * off_y);
                            let fm_y = round2(world_ty + wb * off_x + wd * off_y);
                            // stand_turn mirrors the idle pose IN PLACE about the character's
                            // vertical axis (entity x=0). Negating scaleX alone flips about the
                            // sprite's own left edge, sliding it sideways; also negate the x
                            // position so the whole placement reflects about the origin.
                            if turn_flip {
                                fm_x = round2(-fm_x);
                            }
                            // FM's engine applies its OWN rotation during the TUMBLE state, about
                            // the entity position, so a baked SSF2 per-frame rotation/offset fights
                            // it (the sprite orbits without spinning). For tumble ONLY: strip the
                            // authored rotation AND centre the pose on the origin so the engine's
                            // spin rotates it in place (the official template centres its tumble
                            // pose the same way). Every other animation keeps the authored
                            // corner + rotation form. FrayTools places an IMAGE so its centre
                            // lands at (x + scaleX*w/2, y + scaleY*h/2) with SIGNED scales
                            // (scaleY is negative under the y-flip), so centring on the origin
                            // means x = -scaleX*w/2, y = -scaleY*h/2.
                            let mut fm_y = fm_y;
                            let pivot_x = 0.0_f64;
                            let pivot_y = 0.0_f64;
                            let mut emit_rot = ((world_rot % 360.0) + 360.0) % 360.0;
                            // A mirror reflects the pose's ROTATION too: reflecting about the
                            // vertical axis maps an angle to its negative. Negating scaleX and x
                            // without it leaves a rotated frame turned the wrong way -- invisible
                            // on stand_turn, whose idle pose is flat, and plain on a run turn,
                            // whose frames are authored at 180 and 150 degrees.
                            if turn_flip {
                                emit_rot = ((-emit_rot % 360.0) + 360.0) % 360.0;
                            }
                            if anim_name == "tumble" {
                                if let Some(img) = bitmap_img {
                                    let (hw, hh) = (img.width as f64 / 2.0, img.height as f64 / 2.0);
                                    fm_x = round2(-fm_sx * hw);
                                    fm_y = round2(-fm_sy * hh);
                                    emit_rot = 0.0;
                                }
                            }
                            // hurt_heavy is also a launched pose (see box_shift above): translate
                            // the art by the same hurtbox-union-to-origin delta as the boxes, so
                            // the body centres on the entity position with art-box registration
                            // preserved exactly. No rotation strip -- the engine doesn't spin
                            // hurt_heavy.
                            if anim_name == "hurt_heavy" {
                                fm_x = round2(fm_x + box_shift.0);
                                fm_y = round2(fm_y + box_shift.1);
                            }

                            symbols.push(json!({
                                "$id": per_placement_sym_id,
                                "alpha": 1,
                                "imageAsset": meta_guid,
                                "pivotX": pivot_x,
                                "pivotY": pivot_y,
                                "pluginMetadata": {},
                                // SWF world_rot = atan2(b,a) in y-down coords.
                                // Normalize to FrayTools 0-360 range (no negation).
                                "rotation": round2(emit_rot),
                                "scaleX": fm_sx,
                                "scaleY": fm_sy,
                                "type": "IMAGE",
                                "x": fm_x,
                                "y": fm_y
                            }));
                            Some(per_placement_sym_id)
                        } else {
                            None
                        };

                        keyframes.push(json!({
                            "$id": kf_id,
                            "type": "IMAGE",
                            "length": run,
                            "symbol": symbol_ref.map(Value::String).unwrap_or(Value::Null),
                            "tweened": false,
                            "tweenType": "LINEAR",
                            "pluginMetadata": {}
                        }));
                        img_kf_ids.push(kf_id);
                        f += run;
                    }
                } else if slot == 0 {
                    // No image data — single null keyframe for slot 0 only
                    let kf_id = uuid(char_id, &format!("kf_image_{}_s0_f0", anim_name));
                    keyframes.push(json!({
                        "$id": kf_id,
                        "type": "IMAGE",
                        "length": frame_count,
                        "symbol": Value::Null,
                        "tweened": false,
                        "tweenType": "LINEAR",
                        "pluginMetadata": {}
                    }));
                    img_kf_ids.push(kf_id);
                }

                layers.push(json!({
                    "$id": img_layer_id,
                    "name": format!("Image {}", slot),
                    "type": "IMAGE",
                    "keyframes": img_kf_ids,
                    "hidden": false,
                    "locked": false,
                    "pluginMetadata": {}
                }));
                anim_image_layer_ids.push(img_layer_id);
            }

            // Then BLANK the slots this animation does not use.
            //
            // A layer the engine is not told about keeps whatever the previous animation put
            // there. Sandbag's revival is five layers deep (the neutral pose plus its platform
            // and glow); walk is one. Walk after revival therefore inherits four layers it never
            // asked for, and the platform rides along under the character for as long as the next
            // animation stays shallow. It reads as a stray frame of the wrong move, which is a
            // hard thing to trace back to a layer count.
            //
            // SSF2 does not have the problem because a Flash timeline clears its own layers. Here
            // each animation has to say so, which costs one empty keyframe per unused slot: the
            // layer exists, is addressed, and is explicitly holding nothing.
            for slot in num_slots..entity_image_slots {
                let img_layer_id = uuid(char_id, &format!("layer_image_{}_{}", anim_name, slot));
                let kf_id = uuid(char_id, &format!("kf_image_blank_{}_{}", anim_name, slot));
                keyframes.push(json!({
                    "$id": kf_id, "type": "IMAGE", "length": frame_count.max(1),
                    "symbol": Value::Null, "tweened": false, "tweenType": "LINEAR",
                    "pluginMetadata": {}
                }));
                layers.push(json!({
                    "$id": img_layer_id,
                    "name": format!("Image {}", slot),
                    "type": "IMAGE",
                    "keyframes": [kf_id],
                    "hidden": false,
                    "locked": false,
                    "pluginMetadata": {}
                }));
                anim_image_layer_ids.push(img_layer_id);
            }
        }

        // Emit IMAGE layers first (drawn behind), then the collision/label/script
        // layers, preserving relative order within each group.
        let mut ordered_layer_ids = anim_image_layer_ids;
        ordered_layer_ids.extend(anim_layer_ids);

        animations.push(json!({
            "$id": anim_id,
            "name": anim_name,
            "layers": ordered_layer_ids,
            "pluginMetadata": {}
        }));
    }

    // 30fps → 60fps: hold every keyframe for two frames (see fn docs).
    double_keyframe_lengths(&mut keyframes);
    // Re-collapse each script keyframe to a single frame (+ trailing blank) so frame
    // scripts fire exactly once, not across the doubled 2-frame span.
    enforce_one_frame_scripts(&mut layers, &mut keyframes, char_id);

    // Drop animations whose image timeline is entirely blank (no IMAGE
    // keyframe carries a symbol). Empty stubs clutter the FrayTools editor
    // and confuse modders. KEEP_EMPTY allowlists empty animations that the
    // jab-chain boilerplate in Script.hx still references by name — those
    // would crash at runtime if dropped.
    //
    // For populated_jabs == 1 (single-jab characters) Script.hx emits no
    // chain at all, so jab2/jab3 are not referenced and can be dropped like
    // the other empties. For populated_jabs >= 2 the chain is emitted; the
    // chain reaches up to jab3, so jab3 must exist if it's the last link
    // and would otherwise be empty (populated_jabs == 2). populated_jabs
    // >= 3 means jab2/jab3 are themselves populated and don't need an
    // allowlist entry.
    let keep_empty: &[&str] = match populated_jabs {
        2 => &["jab3"],
        _ => &[],
    };
    {
        let layer_type: std::collections::BTreeMap<String, String> = layers.iter()
            .filter_map(|l| Some((l["$id"].as_str()?.to_string(), l["type"].as_str()?.to_string())))
            .collect();
        let layer_kfs: std::collections::BTreeMap<String, Vec<String>> = layers.iter()
            .filter_map(|l| {
                let id = l["$id"].as_str()?.to_string();
                let kfs = l["keyframes"].as_array()?.iter()
                    .filter_map(|v| Some(v.as_str()?.to_string())).collect();
                Some((id, kfs))
            }).collect();
        let kf_has_symbol: std::collections::BTreeMap<String, bool> = keyframes.iter()
            .filter_map(|k| Some((
                k["$id"].as_str()?.to_string(),
                k.get("symbol").map(|v| !v.is_null()).unwrap_or(false),
            ))).collect();

        let mut drop_layers: std::collections::BTreeSet<String> = Default::default();
        let mut dropped_names: Vec<String> = Vec::new();
        animations.retain(|a| {
            let name = match a["name"].as_str() { Some(n) => n, None => return true };
            if keep_empty.contains(&name) { return true; }
            let lids = match a["layers"].as_array() { Some(v) => v, None => return true };
            let has_image = lids.iter().filter_map(|v| v.as_str()).any(|lid| {
                if layer_type.get(lid).map(String::as_str) != Some("IMAGE") { return false; }
                layer_kfs.get(lid).map(|ks|
                    ks.iter().any(|k| *kf_has_symbol.get(k).unwrap_or(&false))
                ).unwrap_or(false)
            });
            if !has_image {
                dropped_names.push(name.to_string());
                for v in lids { if let Some(s) = v.as_str() { drop_layers.insert(s.to_string()); } }
            }
            has_image
        });
        let mut drop_kfs: std::collections::BTreeSet<String> = Default::default();
        for lid in &drop_layers {
            if let Some(ks) = layer_kfs.get(lid) {
                for k in ks { drop_kfs.insert(k.clone()); }
            }
        }
        layers.retain(|l| l["$id"].as_str().map(|id| !drop_layers.contains(id)).unwrap_or(true));
        keyframes.retain(|k| k["$id"].as_str().map(|id| !drop_kfs.contains(id)).unwrap_or(true));
        if !dropped_names.is_empty() {
            log::info!("Dropped {} empty animation(s): {}", dropped_names.len(), dropped_names.join(", "));
        }
    }

    // Order template animations per the character template; relegate everything that is
    // neither a template animation nor a direct variant of one to an UNUSED section.
    reorder_animations(&mut animations, &mut layers, &mut keyframes, char_id);
    let entity_image_slots = img_result.anim_images.values()
        .map(|a| a.max_depth_slots.max(1)).max().unwrap_or(1);
    append_missing_template_animations(&mut animations, &mut layers, &mut keyframes, char_id,
        entity_image_slots);

    let entity = json!({
        "animations": animations,
        "export": true,
        "guid": uuid(char_id, "entity_guid"),
        "id": char_id,
        "keyframes": keyframes,
        "layers": layers,
        "paletteMap": Value::Null,
        "pluginMetadata": {
            "com.fraymakers.FraymakersMetadata": {
                "objectType": "CHARACTER",
                "version": "0.4.0"
            }
        },
        "plugins": ["com.fraymakers.FraymakersMetadata"],
        "symbols": symbols,
        "tags": [],
        "terrains": [],
        "tilesets": [],
        "version": 14
    });

    serde_json::to_string_pretty(&entity).unwrap_or_else(|_| "{}".to_string())
}

/// Generate entity with paletteMap filled in
pub fn generate_entity_with_palette(
    data: &CharacterData,
    char_id: &str,
    sprite_boxes: &BTreeMap<String, AnimationBoxData>,
    img_result: &ImageExtractionResult,
    populated_jabs: usize,
    palette_collection_guid: &str,
    palette_map_id: &str,
) -> String {
    let json_str = generate_entity(data, char_id, sprite_boxes, img_result, populated_jabs);
    let mut entity: serde_json::Value = serde_json::from_str(&json_str).unwrap_or(serde_json::json!({}));
    entity["paletteMap"] = serde_json::json!({
        "paletteCollection": palette_collection_guid,
        "paletteMap": palette_map_id
    });
    serde_json::to_string_pretty(&entity).unwrap_or(json_str)
}

/// Get image GUIDs for .meta file generation
pub fn get_image_meta_guids(
    img_result: &ImageExtractionResult,
) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for img in img_result.images.values() {
        let meta_guid = crate::uuid_gen::image_meta_guid(&img.symbol_name);
        result.insert(img.png_path.clone(), meta_guid);
    }
    result
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Strip the `function name() {` wrapper from a script and return just the body,
/// with one level of leading tab removed from each line.
/// De-indent (strip one leading tab) and trim leading/trailing blank lines.
fn dedent_body(inner: &str) -> Vec<String> {
    let mut lines: Vec<String> = inner.lines()
        .map(|l| l.strip_prefix('\t').unwrap_or(l).to_string())
        .collect();
    while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) { lines.remove(0); }
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) { lines.pop(); }
    lines
}

/// Extract the statement body from `script.code`.
///
/// A single `script.code` usually wraps one `function name() { … }`, but it can hold
/// MORE than one when two SSF2 frame scripts land on the same frame (e.g. a slot-aliased
/// animation like `tech_ground` reusing `crash_*` plus its own script). Naively dropping
/// only the first `function …{` line and the last `}` then leaks the inner function and a
/// stray brace, producing unparseable keyframe code. Instead, brace-match every top-level
/// `function …{ }` (skipping braces inside `//` comments and strings) and concatenate their
/// bodies — all the scripts run on that frame. Falls back to the old line-based strip if no
/// function wrapper is present.
fn extract_function_body(code: &str) -> String {
    let b = code.as_bytes();
    let mut bodies: Vec<String> = Vec::new();
    let mut i = 0;
    while let Some(rel) = code[i..].find("function ") {
        let fpos = i + rel;
        // Opening brace of this function's block.
        let Some(boff) = code[fpos..].find('{') else { break };
        let open = fpos + boff;
        // Brace-match forward, ignoring `//` comments and string contents.
        let mut depth = 0i32;
        let mut j = open;
        let mut in_str: Option<u8> = None;
        let mut close: Option<usize> = None;
        while j < b.len() {
            let c = b[j];
            match in_str {
                Some(q) => {
                    if c == b'\\' { j += 2; continue; }
                    if c == q { in_str = None; }
                }
                None => match c {
                    b'/' if j + 1 < b.len() && b[j + 1] == b'/' => {
                        while j < b.len() && b[j] != b'\n' { j += 1; }
                        continue;
                    }
                    b'"' | b'\'' => in_str = Some(c),
                    b'{' => depth += 1,
                    b'}' => {
                        depth -= 1;
                        if depth == 0 { close = Some(j); break; }
                    }
                    _ => {}
                },
            }
            j += 1;
        }
        let Some(close) = close else { break }; // unbalanced; bail to fallback
        bodies.push(dedent_body(&code[open + 1..close]).join("\n"));
        i = close + 1;
    }

    if !bodies.is_empty() {
        return bodies.join("\n");
    }

    // Fallback: no `function` wrapper found (or unbalanced) — old line-based strip.
    let mut lines: Vec<&str> = code.lines().collect();
    while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) { lines.remove(0); }
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) { lines.pop(); }
    if lines.is_empty() { return String::new(); }
    if lines.first().map(|l| l.trim_start().starts_with("function ")).unwrap_or(false) {
        lines.remove(0);
    }
    if lines.last().map(|l| l.trim() == "}").unwrap_or(false) { lines.pop(); }
    lines.iter().map(|l| l.strip_prefix('\t').unwrap_or(l)).collect::<Vec<_>>().join("\n")
}

// ─── Menu entity generation ───────────────────────────────────────────────────

/// Information about a menu image extracted from the SWF.
#[derive(Debug, Clone)]
pub struct MenuImageInfo {
    /// The symbol name of the head/portrait image (e.g. "mario_dm0")
    pub head_symbol: String,
    /// Width of the head image in pixels
    pub head_width: u32,
    /// Height of the head image in pixels
    pub head_height: u32,
    /// Meta GUID for the head image
    pub head_meta_guid: String,
}

/// Generate a menu.entity file for the character's UI images.
///
/// SSF2 provides one portrait image (from the `_head` sprite, e.g. `mario_dm0`).
/// We reuse it across all required Fraymakers menu animations:
///   - `full` — full character portrait (character select screen)
///   - `css` — character select screen thumbnail (2 layers: foreground + background)
///   - `icon` — small character icon
///   - `icon_no_palette` — icon without palette swap
///   - `stock` — stock icon (lives remaining)
///   - `hud` / `hud_front` — HUD portrait (default expression)
///   - `hud_angry` / `hud_angry_front` — HUD portrait (angry)
///   - `hud_happy` / `hud_happy_front` — HUD portrait (happy)
///   - `hud_hurt` / `hud_hurt_front` — HUD portrait (hurt)
///   - `hud_sad` / `hud_sad_front` — HUD portrait (sad)
///
/// Since SSF2 only has one portrait, all HUD variants use the same image.
pub fn generate_menu_entity(
    char_id: &str,
    menu_info: &MenuImageInfo,
) -> String {
    let mut keyframes: Vec<Value> = Vec::new();
    let mut layers: Vec<Value> = Vec::new();
    let mut symbols: Vec<Value> = Vec::new();
    let mut animations: Vec<Value> = Vec::new();

    let head_guid = &menu_info.head_meta_guid;
    let img_w = menu_info.head_width as f64;
    let img_h = menu_info.head_height as f64;
    let pivot_x = round2(img_w / 2.0);
    let pivot_y = round2(img_h / 2.0);

    // Helper functions return (sym, kf, layer) tuples; caller pushes them.
    fn make_image_items(char_id: &str, head_guid: &str, pivot_x: f64, pivot_y: f64,
                        anim_name: &str, suffix: &str, x: f64, y: f64)
        -> (Value, Value, Value, String) {
        let sym_id = uuid(char_id, &format!("menu_sym_{}{}", anim_name, suffix));
        let kf_id = uuid(char_id, &format!("menu_kf_{}{}", anim_name, suffix));
        let layer_id = uuid(char_id, &format!("menu_layer_{}{}", anim_name, suffix));
        let sym = json!({ "$id": sym_id, "alpha": 1, "imageAsset": head_guid,
            "pivotX": pivot_x, "pivotY": pivot_y, "pluginMetadata": {},
            "rotation": 0, "scaleX": 1, "scaleY": 1, "type": "IMAGE",
            "x": round2(x), "y": round2(y) });
        let kf = json!({ "$id": kf_id, "length": 1, "pluginMetadata": {},
            "symbol": sym_id, "tweenType": "LINEAR", "tweened": false, "type": "IMAGE" });
        let layer = json!({ "$id": layer_id, "name": "Image Layer", "type": "IMAGE",
            "keyframes": [kf_id], "hidden": false, "locked": false, "pluginMetadata": {} });
        (sym, kf, layer, layer_id)
    }

    fn make_blank_image(char_id: &str, anim_name: &str, suffix: &str) -> (Value, Value, String) {
        let kf_id = uuid(char_id, &format!("menu_kf_{}{}_blank", anim_name, suffix));
        let layer_id = uuid(char_id, &format!("menu_layer_{}{}_blank", anim_name, suffix));
        let kf = json!({ "$id": kf_id, "length": 1, "pluginMetadata": {},
            "symbol": Value::Null, "tweenType": "LINEAR", "tweened": false, "type": "IMAGE" });
        let layer = json!({ "$id": layer_id, "name": "Image Layer", "type": "IMAGE",
            "keyframes": [kf_id], "hidden": false, "locked": false, "pluginMetadata": {} });
        (kf, layer, layer_id)
    }

    fn make_hud_aid(char_id: &str, anim_name: &str, size: f64) -> (Value, Value, Value, String) {
        let sym_id = uuid(char_id, &format!("menu_hud_aid_sym_{}", anim_name));
        let kf_id = uuid(char_id, &format!("menu_hud_aid_kf_{}", anim_name));
        let layer_id = uuid(char_id, &format!("menu_hud_aid_layer_{}", anim_name));
        let sym = json!({ "$id": sym_id, "alpha": Value::Null, "color": Value::Null,
            "pivotX": round2(size/2.0), "pivotY": round2(size/2.0), "pluginMetadata": {},
            "rotation": 0, "scaleX": size, "scaleY": size, "type": "COLLISION_BOX",
            "x": 0, "y": 0 });
        let kf = json!({ "$id": kf_id, "length": 1, "pluginMetadata": {},
            "symbol": sym_id, "tweenType": "LINEAR", "tweened": false, "type": "COLLISION_BOX" });
        let layer = json!({ "$id": layer_id, "name": "HUD Visual Aid", "type": "COLLISION_BOX",
            "keyframes": [kf_id], "hidden": false, "locked": false, "pluginMetadata": {} });
        (sym, kf, layer, layer_id)
    }

    fn make_blank_script(char_id: &str, anim_name: &str) -> (Value, Value, String) {
        let kf_id = uuid(char_id, &format!("menu_script_kf_{}", anim_name));
        let layer_id = uuid(char_id, &format!("menu_script_layer_{}", anim_name));
        let kf = json!({ "$id": kf_id, "length": 1, "pluginMetadata": {},
            "symbol": Value::Null, "tweenType": "LINEAR", "tweened": false, "type": "FRAME_SCRIPT" });
        let layer = json!({ "$id": layer_id, "name": "Frame Script Layer", "type": "FRAME_SCRIPT",
            "keyframes": [kf_id], "hidden": false, "locked": false, "pluginMetadata": {} });
        (kf, layer, layer_id)
    }

    fn make_icon_aid(char_id: &str, anim_name: &str) -> (Value, Value, Value, String) {
        let sym_id = uuid(char_id, &format!("menu_icon_aid_sym_{}", anim_name));
        let kf_id = uuid(char_id, &format!("menu_icon_aid_kf_{}", anim_name));
        let layer_id = uuid(char_id, &format!("menu_icon_aid_layer_{}", anim_name));
        let sym = json!({ "$id": sym_id, "alpha": Value::Null, "color": Value::Null,
            "pivotX": 18, "pivotY": 18, "pluginMetadata": {},
            "rotation": 0, "scaleX": 41, "scaleY": 21, "type": "COLLISION_BOX",
            "x": 0, "y": 0 });
        let kf = json!({ "$id": kf_id, "length": 1, "pluginMetadata": {},
            "symbol": sym_id, "tweenType": "LINEAR", "tweened": false, "type": "COLLISION_BOX" });
        let layer = json!({ "$id": layer_id, "name": "Size Visual Aid", "type": "COLLISION_BOX",
            "keyframes": [kf_id], "hidden": false, "locked": false, "pluginMetadata": {} });
        (sym, kf, layer, layer_id)
    }

    // Macro to push items returned by helpers
    macro_rules! push_img {
        ($anim:expr, $suf:expr, $x:expr, $y:expr) => {{
            let (s, k, l, lid) = make_image_items(char_id, head_guid, pivot_x, pivot_y, $anim, $suf, $x, $y);
            symbols.push(s); keyframes.push(k); layers.push(l); lid
        }}
    }
    macro_rules! push_blank_img {
        ($anim:expr, $suf:expr) => {{
            let (k, l, lid) = make_blank_image(char_id, $anim, $suf);
            keyframes.push(k); layers.push(l); lid
        }}
    }
    macro_rules! push_hud_aid {
        ($anim:expr, $size:expr) => {{
            let (s, k, l, lid) = make_hud_aid(char_id, $anim, $size);
            symbols.push(s); keyframes.push(k); layers.push(l); lid
        }}
    }
    macro_rules! push_script {
        ($anim:expr) => {{
            let (k, l, lid) = make_blank_script(char_id, $anim);
            keyframes.push(k); layers.push(l); lid
        }}
    }
    macro_rules! push_icon_aid {
        ($anim:expr) => {{
            let (s, k, l, lid) = make_icon_aid(char_id, $anim);
            symbols.push(s); keyframes.push(k); layers.push(l); lid
        }}
    }

    // ── full: big portrait ───────────────────────────────────────────────
    {
        let img_layer = push_img!("full", "", -pivot_x, -pivot_y);
        let blank_layer = push_blank_img!("full", "");
        animations.push(json!({ "$id": uuid(char_id, "menu_anim_full"),
            "name": "full", "layers": [img_layer, blank_layer], "pluginMetadata": {} }));
    }

    // ── css: character select screen (2 image layers) ────────────────────
    {
        let fg = push_img!("css", "_fg", -pivot_x, -pivot_y);
        let bg = push_img!("css", "_bg", -pivot_x, -pivot_y);
        animations.push(json!({ "$id": uuid(char_id, "menu_anim_css"),
            "name": "css", "layers": [fg, bg], "pluginMetadata": {} }));
    }

    // ── icon ─────────────────────────────────────────────────────────────
    {
        let img = push_img!("icon", "", 0.0, 0.0);
        let aid = push_icon_aid!("icon");
        let script = push_script!("icon");
        animations.push(json!({ "$id": uuid(char_id, "menu_anim_icon"),
            "name": "icon", "layers": [img, aid, script], "pluginMetadata": {} }));
    }

    // ── icon_no_palette ──────────────────────────────────────────────────
    {
        let img = push_img!("icon_no_palette", "", 0.0, 0.0);
        let script = push_script!("icon_no_palette");
        let aid = push_icon_aid!("icon_no_palette");
        animations.push(json!({ "$id": uuid(char_id, "menu_anim_icon_no_palette"),
            "name": "icon_no_palette", "layers": [img, script, aid], "pluginMetadata": {} }));
    }

    // ── stock ────────────────────────────────────────────────────────────
    {
        let aid = push_hud_aid!("stock", 24.0);
        let img = push_img!("stock", "", -pivot_x, -pivot_y);
        animations.push(json!({ "$id": uuid(char_id, "menu_anim_stock"),
            "name": "stock", "layers": [aid, img], "pluginMetadata": {} }));
    }

    // ── hud variants ────────────────────────────────────────────────────
    for hud_name in &["hud", "hud_front", "hud_angry", "hud_angry_front",
                      "hud_happy", "hud_happy_front", "hud_hurt", "hud_hurt_front",
                      "hud_sad", "hud_sad_front"] {
        let img = push_img!(hud_name, "", -pivot_x, -pivot_y);
        let aid = push_hud_aid!(hud_name, 36.0);
        animations.push(json!({ "$id": uuid(char_id, &format!("menu_anim_{}", hud_name)),
            "name": *hud_name, "layers": [img, aid], "pluginMetadata": {} }));
    }

    // 30fps → 60fps: hold every keyframe for two frames (see fn docs).
    double_keyframe_lengths(&mut keyframes);

    let entity = json!({
        "animations": animations,
        "export": true,
        "guid": uuid(char_id, "menu_entity_guid"),
        "id": "menu",
        "keyframes": keyframes,
        "layers": layers,
        "paletteMap": Value::Null,
        "pluginMetadata": {
            "com.fraymakers.FraymakersMetadata": {
                "spritesheetGroup": "menu",
                "version": "0.3.1"
            }
        },
        "plugins": ["com.fraymakers.FraymakersMetadata"],
        "symbols": symbols,
        "tags": ["menu"],
        "terrains": [],
        "tilesets": [],
        "version": 14
    });

    serde_json::to_string_pretty(&entity).unwrap_or_else(|_| "{}" .to_string())
}

// ─── Projectile entity generation ─────────────────────────────────────────────

/// Information about a projectile extracted from the SWF.
#[derive(Debug, Clone)]
pub struct ProjectileInfo {
    /// The projectile's symbol name (e.g. "mario_fireball")
    pub name: String,
    /// The inner animation sprite name (e.g. "mario_fla.mario_fireball_mc_210")
    pub inner_sprite_name: Option<String>,
    /// Frame count of the inner animation sprite
    pub inner_frame_count: u16,
    /// Collision boxes from the inner sprite, if any
    pub boxes: Option<AnimationBoxData>,
    /// Image frames from the inner sprite (symbol name per frame)
    pub image_frames: Vec<Option<String>>,
    /// Per-frame SSF2 local transform (parallel to image_frames). Applied to the
    /// IMAGE symbol so the sprite renders at its SSF2 display size, not full native.
    pub image_matrices: Vec<Option<crate::image_extractor::ImageLocalMatrix>>,
    /// Meta GUIDs for each unique image used
    pub image_guids: BTreeMap<String, String>,
    /// Extra per-state data for multi-state projectiles (e.g. link_bomb).
    /// Each entry is (ssf2_label, image_frames, image_guids, boxes, frame_count).
    pub extra_states: Vec<ProjectileStateData>,
    /// SSF2 frame labels found INSIDE the inner animation sprite, in
    /// timeline order. `(1-based frame, label)`. Empty for projectiles
    /// without timeline labels — those fall back to the FM template
    /// trio (`projectileSpawn` / `projectileIdle` / `projectileDestroy`).
    pub inner_labels: Vec<(u16, String)>,
}

/// Image + box data for one state of a multi-state projectile.
#[derive(Debug, Clone)]
pub struct ProjectileStateData {
    pub label: String,
    pub image_frames: Vec<Option<String>>,
    pub image_matrices: Vec<Option<crate::image_extractor::ImageLocalMatrix>>,
    pub image_guids: BTreeMap<String, String>,
    pub boxes: Option<AnimationBoxData>,
    pub frame_count: u16,
}

/// Generate a projectile.entity file for a single SSF2 projectile.
///
/// SSF2 projectile structure:
///   Root sprite (e.g. `mario_fireball`, 1 frame):
///     - LABEL 'attack_idle'
///     - PlaceObject with name='stance' → inner animation sprite
///   Inner sprite (e.g. `mario_fla.mario_fireball_mc_210`, N frames):
///     - Collision boxes (hitBox, attackBox)
///     - Image placements per frame
///     - Labels (e.g. 'loop')
///
/// Fraymakers projectile animations:
///   - `projectileSpawn` — 1 frame, first frame image
///   - `projectileIdle` — all frames with hitbox/hurtbox
///   - `projectileDestroy` — last frame image + blank frame script
/// Map an SSF2 projectile outer-sprite frame label to a Fraymakers animation name.
/// attack_idle → projectileIdle (the default), attack_hold → projectileHeld, etc.
pub fn ssf2_proj_label_to_fm_anim(label: &str) -> String {
    match label {
        "attack_idle" => "projectileIdle".to_string(),
        "attack_hold" => "projectileHeld".to_string(),
        "attack_toss" => "projectileActive".to_string(),
        other => format!("projectile_{}", other.replace(|c: char| !c.is_alphanumeric(), "_")),
    }
}

/// FrayTools IMAGE-symbol transform (scaleX, scaleY, rotation, x, y) from an
/// optional SSF2 local matrix. Projectiles/effects are standalone (no root MC to
/// compose), so the local matrix applies directly. None → identity (the old default,
/// which rendered large bitmaps at full native size — i.e. way too big).
fn proj_image_xform(mat: Option<&crate::image_extractor::ImageLocalMatrix>) -> (f64, f64, f64, f64, f64) {
    match mat {
        Some(m) => (
            round2(m.sx),
            round2(m.sy),
            round2(((m.rotation % 360.0) + 360.0) % 360.0),
            round2(m.tx),
            round2(m.ty),
        ),
        None => (1.0, 1.0, 0.0, 0.0, 0.0),
    }
}

pub fn generate_projectile_entity(
    char_id: &str,
    proj: &ProjectileInfo,
) -> String {
    let mut keyframes: Vec<Value> = Vec::new();
    let mut layers: Vec<Value> = Vec::new();
    let mut symbols: Vec<Value> = Vec::new();
    let mut animations: Vec<Value> = Vec::new();

    let proj_id = &proj.name;
    let total_frames = proj.inner_frame_count.max(1) as u32;

    // ── Build image keyframes for idle animation ─────────────────────────
    let mut idle_image_kf_ids: Vec<String> = Vec::new();
    for frame in 0..total_frames {
        let sym_name = proj.image_frames.get(frame as usize).and_then(|s| s.as_ref());
        let kf_id = uuid(char_id, &format!("proj_{}_idle_img_kf_{}", proj_id, frame));
        if let Some(sym_name) = sym_name {
            if let Some(guid) = proj.image_guids.get(sym_name.as_str()) {
                let sym_id = uuid(char_id, &format!("proj_{}_idle_img_sym_{}", proj_id, frame));
                let (sx, sy, rot, x, y) = proj_image_xform(
                    proj.image_matrices.get(frame as usize).and_then(|m| m.as_ref()));
                symbols.push(json!({
                    "$id": sym_id,
                    "alpha": 1,
                    "imageAsset": guid,
                    "pivotX": 0,
                    "pivotY": 0,
                    "pluginMetadata": {},
                    "rotation": rot,
                    "scaleX": sx,
                    "scaleY": sy,
                    "type": "IMAGE",
                    "x": x,
                    "y": y
                }));
                keyframes.push(json!({
                    "$id": kf_id,
                    "length": 1,
                    "pluginMetadata": {},
                    "symbol": sym_id,
                    "tweenType": "LINEAR",
                    "tweened": false,
                    "type": "IMAGE"
                }));
            } else {
                keyframes.push(json!({
                    "$id": kf_id,
                    "length": 1,
                    "pluginMetadata": {},
                    "symbol": Value::Null,
                    "tweenType": "LINEAR",
                    "tweened": false,
                    "type": "IMAGE"
                }));
            }
        } else {
            keyframes.push(json!({
                "$id": kf_id,
                "length": 1,
                "pluginMetadata": {},
                "symbol": Value::Null,
                "tweenType": "LINEAR",
                "tweened": false,
                "type": "IMAGE"
            }));
        }
        idle_image_kf_ids.push(kf_id);
    }

    let idle_image_layer_id = uuid(char_id, &format!("proj_{}_idle_img_layer", proj_id));
    layers.push(json!({
        "$id": idle_image_layer_id,
        "name": "Image Layer",
        "type": "IMAGE",
        "keyframes": idle_image_kf_ids,
        "hidden": false,
        "locked": false,
        "pluginMetadata": {}
    }));

    // ── Build collision box layers for idle (from inner sprite boxes) ────
    let mut idle_box_layer_ids: Vec<String> = Vec::new();
    if let Some(boxes) = &proj.boxes {
        // Collect all unique instance names across frames
        let mut instances_in_proj: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for frame_boxes in boxes.frames.values() {
            for b in frame_boxes {
                instances_in_proj.insert(b.instance_name.clone());
            }
        }

        for inst_name in instances_in_proj.iter() {
            let box_type = BoxType::from_instance_name(inst_name).unwrap_or(BoxType::Hurtbox);
            let fm_layer_name = ssf2_box_name_to_fm(inst_name);
            let fm_box_type = box_type_to_fm(box_type);
            let color = box_color(box_type);
            let box_idx = fm_box_index(&fm_layer_name);

            let mut box_kf_ids: Vec<String> = Vec::new();
            let mut frame_idx: u32 = 0;

            // Collect active frames for this instance
            let mut active_frames: Vec<(usize, &crate::sprite_parser::FrameBox)> = Vec::new();
            for f in 0..total_frames {
                if let Some(frame_boxes) = boxes.frames.get(&(f as u16)) {
                    if let Some(fb) = frame_boxes.iter().find(|b| &b.instance_name == inst_name) {
                        active_frames.push((f as usize, fb));
                    }
                }
            }

            let is_point = box_type == crate::sprite_parser::BoxType::GrabHoldBox;
            let kf_type = if is_point { "POINT" } else { "COLLISION_BOX" };

            // Build keyframes using run-length encoding
            let mut i = 0;
            while i < active_frames.len() {
                let (start_frame, fb) = active_frames[i];
                let mut run_end = i;
                while run_end + 1 < active_frames.len() {
                    let (next_f, next_fb) = active_frames[run_end + 1];
                    let (cur_f, _) = active_frames[run_end];
                    let consecutive = next_f == cur_f + 1;
                    let same_geom = (next_fb.x - fb.x).abs() < 0.01
                        && (next_fb.y - fb.y).abs() < 0.01
                        && (next_fb.width - fb.width).abs() < 0.01
                        && (next_fb.height - fb.height).abs() < 0.01
                        && (next_fb.rotation - fb.rotation).abs() < 0.01;
                    if consecutive && same_geom { run_end += 1; } else { break; }
                }
                let run_len = (active_frames[run_end].0 - start_frame + 1) as u32;

                // Gap keyframe
                if start_frame as u32 > frame_idx {
                    let gap_kf_id = uuid(char_id, &format!("proj_{}_box_gap_{}_{}", proj_id, inst_name, frame_idx));
                    keyframes.push(json!({
                        "$id": gap_kf_id,
                        "type": kf_type,
                        "length": (start_frame as u32) - frame_idx,
                        "symbol": Value::Null,
                        "tweened": false,
                        "tweenType": "LINEAR",
                        "pluginMetadata": {}
                    }));
                    box_kf_ids.push(gap_kf_id);
                }

                // Box symbol
                let sym_id = uuid(char_id, &format!("proj_{}_box_sym_{}_{}", proj_id, inst_name, start_frame));
                if is_point {
                    let cx = round2(fb.x + fb.width / 2.0);
                    let cy = round2(fb.y + fb.height);
                    symbols.push(json!({
                        "$id": sym_id,
                        "alpha": 1,
                        "color": color,
                        "pluginMetadata": {},
                        "rotation": 0,
                        "type": "POINT",
                        "x": cx,
                        "y": cy
                    }));
                } else {
                    // Mirror the Character.entity collision-box emission exactly
                    // (see the path above): ItemBox pivots on its bottom-centre
                    // (the hand) and bakes the rotation into the top-left so the
                    // hand stays pinned under rotation; every other box type is
                    // already AABB-collapsed to rotation 0 in sprite_parser, so
                    // it uses a centre pivot and raw top-left. Without this,
                    // rotated projectile itemboxes drift exactly like the
                    // character path did before the bake.
                    let (pivot_x, pivot_y) = if box_type == crate::sprite_parser::BoxType::ItemBox {
                        (fb.width / 2.0, fb.height)        // bottom-centre = hand
                    } else {
                        (fb.width / 2.0, fb.height / 2.0)  // centre
                    };
                    let theta = ((fb.rotation % 360.0) + 360.0) % 360.0;
                    let (box_x, box_y) = if box_type == crate::sprite_parser::BoxType::ItemBox {
                        let hand = crate::fraytools_transform::intended_pivot_point(
                            fb.x, fb.y, pivot_x, pivot_y);
                        let off = crate::fraytools_transform::collision_box_anchor(
                            0.0, 0.0, pivot_x, pivot_y, theta);
                        (hand.0 - off.0, hand.1 - off.1)
                    } else {
                        (fb.x, fb.y)
                    };
                    symbols.push(json!({
                        "$id": sym_id,
                        "alpha": 0.5,
                        "color": color,
                        "pivotX": round2(pivot_x),
                        "pivotY": round2(pivot_y),
                        "pluginMetadata": {},
                        "rotation": round2(theta),
                        "scaleX": round2(fb.width),
                        "scaleY": round2(fb.height),
                        "type": "COLLISION_BOX",
                        "x": round2(box_x),
                        "y": round2(box_y)
                    }));
                }

                let kf_id = uuid(char_id, &format!("proj_{}_box_kf_{}_{}", proj_id, inst_name, start_frame));
                keyframes.push(json!({
                    "$id": kf_id,
                    "type": kf_type,
                    "length": run_len,
                    "symbol": sym_id,
                    "tweened": false,
                    "tweenType": "LINEAR",
                    "pluginMetadata": {}
                }));
                box_kf_ids.push(kf_id);
                frame_idx = start_frame as u32 + run_len;
                i = run_end + 1;
            }

            // Tail gap
            if frame_idx < total_frames {
                let tail_kf_id = uuid(char_id, &format!("proj_{}_box_tail_{}_{}", proj_id, inst_name, frame_idx));
                keyframes.push(json!({
                    "$id": tail_kf_id,
                    "type": kf_type,
                    "length": total_frames - frame_idx,
                    "symbol": Value::Null,
                    "tweened": false,
                    "tweenType": "LINEAR",
                    "pluginMetadata": {}
                }));
                box_kf_ids.push(tail_kf_id);
            }

            if box_kf_ids.is_empty() { continue; }

            let layer_id = uuid(char_id, &format!("proj_{}_box_layer_{}", proj_id, inst_name));
            if is_point {
                layers.push(json!({
                    "$id": layer_id,
                    "name": fm_layer_name,
                    "type": "POINT",
                    "keyframes": box_kf_ids,
                    "hidden": false,
                    "locked": false,
                    "pluginMetadata": {
                        "com.fraymakers.FraymakersMetadata": {
                            "pointType": "GRAB_HOLD_POINT"
                        }
                    }
                }));
            } else {
                layers.push(json!({
                    "$id": layer_id,
                    "name": fm_layer_name,
                    "type": "COLLISION_BOX",
                    "keyframes": box_kf_ids,
                    "hidden": false,
                    "locked": false,
                    "defaultAlpha": 0.5,
                    "defaultColor": color,
                    "pluginMetadata": {
                        "com.fraymakers.FraymakersMetadata": {
                            "collisionBoxType": fm_box_type,
                            "index": box_idx
                        }
                    }
                }));
            }
            idle_box_layer_ids.push(layer_id);
        }
    }

    // ── projectileSpawn animation (1 frame, first image) ─────────────────
    {
        let (_, _, spawn_img) = if !proj.image_frames.is_empty() && proj.image_frames[0].is_some() {
            let sym_name = proj.image_frames[0].as_ref().unwrap();
            let guid = proj.image_guids.get(sym_name.as_str()).cloned().unwrap_or_default();
            let sym_id = uuid(char_id, &format!("proj_{}_spawn_sym", proj_id));
            let (sx, sy, rot, x, y) = proj_image_xform(proj.image_matrices.first().and_then(|m| m.as_ref()));
            symbols.push(json!({
                "$id": sym_id,
                "alpha": 1,
                "imageAsset": guid,
                "pivotX": 0,
                "pivotY": 0,
                "pluginMetadata": {},
                "rotation": rot,
                "scaleX": sx,
                "scaleY": sy,
                "type": "IMAGE",
                "x": x,
                "y": y
            }));
            let kf_id = uuid(char_id, &format!("proj_{}_spawn_kf", proj_id));
            keyframes.push(json!({
                "$id": kf_id,
                "length": 1,
                "pluginMetadata": {},
                "symbol": sym_id,
                "tweenType": "LINEAR",
                "tweened": false,
                "type": "IMAGE"
            }));
            let layer_id = uuid(char_id, &format!("proj_{}_spawn_layer", proj_id));
            layers.push(json!({
                "$id": layer_id,
                "name": "Image Layer",
                "type": "IMAGE",
                "keyframes": [kf_id],
                "hidden": false,
                "locked": false,
                "pluginMetadata": {}
            }));
            (sym_id, kf_id, layer_id)
        } else {
            // Blank spawn frame
            let kf_id = uuid(char_id, &format!("proj_{}_spawn_kf", proj_id));
            keyframes.push(json!({
                "$id": kf_id,
                "length": 1,
                "pluginMetadata": {},
                "symbol": Value::Null,
                "tweenType": "LINEAR",
                "tweened": false,
                "type": "IMAGE"
            }));
            let layer_id = uuid(char_id, &format!("proj_{}_spawn_layer", proj_id));
            layers.push(json!({
                "$id": layer_id,
                "name": "Image Layer",
                "type": "IMAGE",
                "keyframes": [kf_id],
                "hidden": false,
                "locked": false,
                "pluginMetadata": {}
            }));
            (String::new(), kf_id, layer_id)
        };
        animations.push(json!({
            "$id": uuid(char_id, &format!("proj_{}_anim_spawn", proj_id)),
            "name": "projectileSpawn",
            "layers": [spawn_img],
            "pluginMetadata": {}
        }));
    }

    // ── projectileIdle animation (all frames + boxes) ────────────────────
    {
        let mut idle_layers = vec![idle_image_layer_id.clone()];
        idle_layers.extend(idle_box_layer_ids);
        animations.push(json!({
            "$id": uuid(char_id, &format!("proj_{}_anim_idle", proj_id)),
            "name": "projectileIdle",
            "layers": idle_layers,
            "pluginMetadata": {}
        }));
    }

    // ── projectileDestroy animation (last frame + blank script) ──────────
    {
        let last_frame = total_frames.saturating_sub(1) as usize;
        let sym_name = proj.image_frames.get(last_frame).and_then(|s| s.as_ref());
        let destroy_kf_id = uuid(char_id, &format!("proj_{}_destroy_kf", proj_id));
        if let Some(sym_name) = sym_name {
            if let Some(guid) = proj.image_guids.get(sym_name.as_str()) {
                let sym_id = uuid(char_id, &format!("proj_{}_destroy_sym", proj_id));
                let (sx, sy, rot, x, y) = proj_image_xform(
                    proj.image_matrices.get(last_frame).and_then(|m| m.as_ref()));
                symbols.push(json!({
                    "$id": sym_id,
                    "alpha": 1,
                    "imageAsset": guid,
                    "pivotX": 0,
                    "pivotY": 0,
                    "pluginMetadata": {},
                    "rotation": rot,
                    "scaleX": sx,
                    "scaleY": sy,
                    "type": "IMAGE",
                    "x": x,
                    "y": y
                }));
                keyframes.push(json!({
                    "$id": destroy_kf_id,
                    "length": 1,
                    "pluginMetadata": {},
                    "symbol": sym_id,
                    "tweenType": "LINEAR",
                    "tweened": false,
                    "type": "IMAGE"
                }));
            } else {
                keyframes.push(json!({
                    "$id": destroy_kf_id,
                    "length": 1,
                    "pluginMetadata": {},
                    "symbol": Value::Null,
                    "tweenType": "LINEAR",
                    "tweened": false,
                    "type": "IMAGE"
                }));
            }
        } else {
            keyframes.push(json!({
                "$id": destroy_kf_id,
                "length": 1,
                "pluginMetadata": {},
                "symbol": Value::Null,
                "tweenType": "LINEAR",
                "tweened": false,
                "type": "IMAGE"
            }));
        }
        let destroy_img_layer = uuid(char_id, &format!("proj_{}_destroy_img_layer", proj_id));
        layers.push(json!({
            "$id": destroy_img_layer,
            "name": "Image Layer",
            "type": "IMAGE",
            "keyframes": [destroy_kf_id],
            "hidden": false,
            "locked": false,
            "pluginMetadata": {}
        }));

        let destroy_script_kf = uuid(char_id, &format!("proj_{}_destroy_script_kf", proj_id));
        keyframes.push(json!({
            "$id": destroy_script_kf,
            "length": 1,
            "pluginMetadata": {},
            "symbol": Value::Null,
            "tweenType": "LINEAR",
            "tweened": false,
            "type": "FRAME_SCRIPT"
        }));
        let destroy_script_layer = uuid(char_id, &format!("proj_{}_destroy_script_layer", proj_id));
        layers.push(json!({
            "$id": destroy_script_layer,
            "name": "Frame Script Layer",
            "type": "FRAME_SCRIPT",
            "keyframes": [destroy_script_kf],
            "hidden": false,
            "locked": false,
            "pluginMetadata": {}
        }));

        animations.push(json!({
            "$id": uuid(char_id, &format!("proj_{}_anim_destroy", proj_id)),
            "name": "projectileDestroy",
            "layers": [destroy_img_layer, destroy_script_layer],
            "pluginMetadata": {}
        }));
    }

    // ── Extra animations for multi-state projectiles (e.g. link_bomb) ────────
    for state_data in &proj.extra_states {
        let state_slug = state_data.label.replace(|c: char| !c.is_alphanumeric(), "_");
        let anim_name = ssf2_proj_label_to_fm_anim(&state_data.label);

        // Image layer
        let img_layer_id = uuid(char_id, &format!("proj_{}_extra_{}_img_layer", proj_id, state_slug));
        let mut img_kf_ids: Vec<Value> = Vec::new();
        let total = state_data.frame_count as usize;
        for (fi, sym_opt) in state_data.image_frames.iter().enumerate().take(total) {
            let kf_id = uuid(char_id, &format!("proj_{}_extra_{}_img_kf_{}", proj_id, state_slug, fi));
            if let Some(sym_name) = sym_opt {
                if let Some(guid) = state_data.image_guids.get(sym_name.as_str()) {
                    let sym_id = uuid(char_id, &format!("proj_{}_extra_{}_sym_{}", proj_id, state_slug, fi));
                    symbols.push(json!({
                        "$id": sym_id, "alpha": 1, "imageAsset": guid,
                        "pivotX": 0, "pivotY": 0, "pluginMetadata": {},
                        "rotation": 0, "scaleX": 1, "scaleY": 1, "type": "IMAGE", "x": 0, "y": 0
                    }));
                    keyframes.push(json!({
                        "$id": kf_id, "length": 1, "pluginMetadata": {},
                        "symbol": sym_id, "tweenType": "LINEAR", "tweened": false, "type": "IMAGE"
                    }));
                    img_kf_ids.push(kf_id.into());
                    continue;
                }
            }
            keyframes.push(json!({
                "$id": kf_id, "length": 1, "pluginMetadata": {},
                "symbol": Value::Null, "tweenType": "LINEAR", "tweened": false, "type": "IMAGE"
            }));
            img_kf_ids.push(kf_id.into());
        }
        if img_kf_ids.is_empty() {
            let kf_id = uuid(char_id, &format!("proj_{}_extra_{}_img_empty", proj_id, state_slug));
            keyframes.push(json!({
                "$id": kf_id, "length": total.max(1) as u32,
                "pluginMetadata": {}, "symbol": Value::Null,
                "tweenType": "LINEAR", "tweened": false, "type": "IMAGE"
            }));
            img_kf_ids.push(kf_id.into());
        }
        layers.push(json!({
            "$id": img_layer_id, "name": "Image Layer", "type": "IMAGE",
            "keyframes": img_kf_ids, "hidden": false, "locked": false, "pluginMetadata": {}
        }));

        // Script layer (blank)
        let script_kf_id = uuid(char_id, &format!("proj_{}_extra_{}_script_kf", proj_id, state_slug));
        let script_layer_id = uuid(char_id, &format!("proj_{}_extra_{}_script_layer", proj_id, state_slug));
        keyframes.push(json!({
            "$id": script_kf_id, "length": total.max(1) as u32,
            "code": "", "pluginMetadata": {}, "symbol": Value::Null,
            "tweenType": "LINEAR", "tweened": false, "type": "FRAME_SCRIPT"
        }));
        layers.push(json!({
            "$id": script_layer_id, "name": "Frame Script Layer", "type": "FRAME_SCRIPT",
            "keyframes": [script_kf_id], "hidden": false, "locked": false, "pluginMetadata": {}
        }));

        animations.push(json!({
            "$id": uuid(char_id, &format!("proj_{}_extra_{}_anim", proj_id, state_slug)),
            "name": anim_name,
            "layers": [img_layer_id, script_layer_id],
            "pluginMetadata": {}
        }));
    }

    // 30fps → 60fps: hold every keyframe for two frames (see fn docs).
    double_keyframe_lengths(&mut keyframes);

    let entity_id = crate::projectile_gen::projectile_content_id(&proj.name);
    let entity = json!({
        "animations": animations,
        "export": true,
        "guid": uuid(char_id, &format!("proj_{}_entity_guid", proj_id)),
        "id": entity_id,
        "keyframes": keyframes,
        "layers": layers,
        "paletteMap": Value::Null,
        "pluginMetadata": {
            "com.fraymakers.FraymakersMetadata": {
                "objectType": "PROJECTILE",
                "version": "0.1.1"
            }
        },
        "plugins": ["com.fraymakers.FraymakersMetadata"],
        "symbols": symbols,
        "tags": [],
        "terrains": [],
        "tilesets": [],
        "version": 14
    });

    serde_json::to_string_pretty(&entity).unwrap_or_else(|_| "{}".to_string())
}

/// Compute the list of animation names a `generate_effect_entity` call would
/// emit, given an effect's discovered FrameLabels. Used to build the
/// effect→primary-animation map that drives the context-aware
/// `self.attachEffect("name")` → `match.createVfx(...)` rewrite. Keep
/// this in lockstep with the `segments` block of `generate_effect_entity`.
pub fn effect_animation_names(effect: &crate::image_extractor::DiscoveredEffect) -> Vec<String> {
    let total_frames = effect.frame_count.max(1) as usize;
    if effect.inner_labels.is_empty() {
        return vec!["active".to_string()];
    }
    let mut names: Vec<String> = Vec::new();
    for (i, (frame_1based, label)) in effect.inner_labels.iter().enumerate() {
        let start = (frame_1based.saturating_sub(1)) as usize;
        let end = if i + 1 < effect.inner_labels.len() {
            effect.inner_labels[i + 1].0.saturating_sub(1) as usize
        } else {
            total_frames
        };
        if end > start {
            let anim_name: String = label
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .collect();
            names.push(anim_name);
        }
    }
    if let Some(first_start) = effect.inner_labels.first().map(|(f, _)| f.saturating_sub(1) as usize) {
        if first_start > 0 {
            names.insert(0, "active".to_string());
        }
    }
    if names.is_empty() {
        names.push("active".to_string());
    }
    names
}

/// Generate a `<effect_name>.entity` file for one SSF2 effect sprite.
///
/// Effects are pure visual playback — no scripts, no stats, no manifest
/// entry. The character's `Script.hx` triggers them with
/// `match.createVfx(new VfxStats({ spriteContent: self.getResource()
/// .getContent("<name>"), animation: "<label-or-vfx>" }), self)`.
///
/// SSF2 effect structure:
///   Effect sprite (e.g. `effect_land`, N frames):
///     - FrameLabels at various 1-based frames split it into named
///       sub-animations.
///     - Image placements per frame.
///
/// Fraymakers effect animations:
///   - One animation per inner FrameLabel (animation name = label).
///   - If there are no labels, a single `vfx` animation covering all frames.
///
/// Each animation has a single Image Layer; no collision/script layers.
pub fn generate_effect_entity(
    char_id: &str,
    effect: &crate::image_extractor::DiscoveredEffect,
    img_result: &crate::image_extractor::ImageExtractionResult,
    parsed_swf: &swf::Swf<'_>,
) -> String {
    let mut keyframes: Vec<Value> = Vec::new();
    let mut layers: Vec<Value> = Vec::new();
    let mut symbols: Vec<Value> = Vec::new();
    let mut animations: Vec<Value> = Vec::new();

    let effect_id = &effect.name;
    let total_frames = effect.frame_count.max(1) as u32;

    // Pull per-frame images by walking the effect sprite's timeline.
    let frame_images = crate::image_extractor::extract_projectile_frame_images_from_swf(
        parsed_swf,
        char_id,
        effect.sprite_id,
        img_result,
    ).unwrap_or(crate::image_extractor::ProjectileFrameImages {
        frames: vec![],
        matrices: vec![],
        image_guids: std::collections::BTreeMap::new(),
    });

    // Determine animation segments: (name, start_0based, end_exclusive).
    // Mirror `effect_animation_names`: same ordering and same fallbacks.
    // No labels → single "active" animation covering the whole sprite.
    let segments: Vec<(String, usize, usize)> = if effect.inner_labels.is_empty() {
        vec![("active".to_string(), 0usize, total_frames as usize)]
    } else {
        let mut segs: Vec<(String, usize, usize)> = Vec::new();
        for (i, (frame_1based, label)) in effect.inner_labels.iter().enumerate() {
            let start = (frame_1based.saturating_sub(1)) as usize;
            let end = if i + 1 < effect.inner_labels.len() {
                effect.inner_labels[i + 1].0.saturating_sub(1) as usize
            } else {
                total_frames as usize
            };
            if end > start {
                let anim_name: String = label
                    .chars()
                    .map(|c| if c.is_alphanumeric() { c } else { '_' })
                    .collect();
                segs.push((anim_name, start, end));
            }
        }
        // Cover any frames before the first label with a leading `active` segment.
        if let Some(first_start) = effect.inner_labels.first().map(|(f, _)| f.saturating_sub(1) as usize) {
            if first_start > 0 {
                segs.insert(0, ("active".to_string(), 0, first_start));
            }
        }
        if segs.is_empty() {
            segs.push(("active".to_string(), 0, total_frames as usize));
        }
        segs
    };

    for (seg_idx, (anim_name, start, end)) in segments.iter().enumerate() {
        let mut img_kf_ids: Vec<Value> = Vec::new();
        let seg_len = end.saturating_sub(*start).max(1);
        for offset in 0..seg_len {
            let frame_idx = start + offset;
            let kf_id = uuid(char_id, &format!("eff_{}_seg{}_img_kf_{}", effect_id, seg_idx, frame_idx));
            let sym_name = frame_images.frames.get(frame_idx).and_then(|s| s.as_ref());
            if let Some(sym_name) = sym_name {
                if let Some(guid) = frame_images.image_guids.get(sym_name.as_str()) {
                    let sym_id = uuid(char_id, &format!("eff_{}_seg{}_img_sym_{}", effect_id, seg_idx, frame_idx));
                    let (sx, sy, rot, x, y) = proj_image_xform(
                        frame_images.matrices.get(frame_idx).and_then(|m| m.as_ref()));
                    symbols.push(json!({
                        "$id": sym_id,
                        "alpha": 1,
                        "imageAsset": guid,
                        "pivotX": 0,
                        "pivotY": 0,
                        "pluginMetadata": {},
                        "rotation": rot,
                        "scaleX": sx,
                        "scaleY": sy,
                        "type": "IMAGE",
                        "x": x,
                        "y": y
                    }));
                    keyframes.push(json!({
                        "$id": kf_id,
                        "length": 1,
                        "pluginMetadata": {},
                        "symbol": sym_id,
                        "tweenType": "LINEAR",
                        "tweened": false,
                        "type": "IMAGE"
                    }));
                    img_kf_ids.push(kf_id.into());
                    continue;
                }
            }
            keyframes.push(json!({
                "$id": kf_id,
                "length": 1,
                "pluginMetadata": {},
                "symbol": Value::Null,
                "tweenType": "LINEAR",
                "tweened": false,
                "type": "IMAGE"
            }));
            img_kf_ids.push(kf_id.into());
        }

        let img_layer_id = uuid(char_id, &format!("eff_{}_seg{}_img_layer", effect_id, seg_idx));
        layers.push(json!({
            "$id": img_layer_id,
            "name": "Image Layer",
            "type": "IMAGE",
            "keyframes": img_kf_ids,
            "hidden": false,
            "locked": false,
            "pluginMetadata": {}
        }));

        animations.push(json!({
            "$id": uuid(char_id, &format!("eff_{}_seg{}_anim", effect_id, seg_idx)),
            "name": anim_name,
            "layers": [img_layer_id],
            "pluginMetadata": {}
        }));
    }

    // 30fps → 60fps: hold every keyframe for two frames.
    double_keyframe_lengths(&mut keyframes);

    // FrayTools requires every entity to carry the FraymakersMetadata plugin
    // with an objectType, and a non-null paletteMap. Without these, the entity
    // editor throws "Invalid MetadataFieldType for value: null" and
    // "Cannot read property 'paletteCollection' of null" → the whole effect
    // renders as "Error rendering component". VFX entities use objectType
    // "VFX" (they're spawned via match.createVfx); paletteMap may be filled in
    // by the caller (haxe_gen) and defaults to {} otherwise.
    let entity = json!({
        "animations": animations,
        "export": true,
        "guid": uuid(char_id, &format!("eff_{}_entity_guid", effect_id)),
        "id": effect_id,
        "keyframes": keyframes,
        "layers": layers,
        "paletteMap": {},
        "pluginMetadata": {
            "com.fraymakers.FraymakersMetadata": {
                "objectType": "VFX",
                "version": "0.1.0"
            }
        },
        "plugins": ["com.fraymakers.FraymakersMetadata"],
        "symbols": symbols,
        "tags": [],
        "terrains": [],
        "tilesets": [],
        "version": 14
    });

    serde_json::to_string_pretty(&entity).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod extract_body_tests {
    use super::{extract_function_body, strip_unreachable_returns};

    #[test]
    fn spurious_mid_return_dropped_keeps_following_code() {
        // special_up frame 79 shape: a depth-0 `return;` before the real logic.
        let body = "AudioClip.play(\"a\");\nreturn;\nif (x) {\n\tplayVoiceSound(1);\n}\nplayAttackSound(1);";
        let out = strip_unreachable_returns(body);
        assert!(!out.lines().any(|l| l.trim() == "return;"), "mid return not dropped: {out}");
        assert!(out.contains("playAttackSound(1);"), "following code lost: {out}");
    }

    #[test]
    fn conditional_return_in_block_is_kept() {
        // A real early-exit inside an if (depth > 0) must NOT be stripped.
        let body = "if (x) {\n\treturn;\n}\ndoThing();";
        let out = strip_unreachable_returns(body);
        assert!(out.contains("return;"), "conditional return wrongly stripped: {out}");
    }

    #[test]
    fn trailing_only_return_left_for_trailing_pass() {
        // Last-statement return has no code after — not our job (strip_trailing_return handles it).
        let body = "doThing();\nreturn;";
        assert_eq!(strip_unreachable_returns(body), body);
    }

    // helper: depth never goes negative and ends at zero (comment/string-naive is fine here)
    fn structurally_valid(s: &str) -> bool {
        let mut depth = 0i32;
        let mut min = 0i32;
        for c in s.chars() {
            if c == '{' { depth += 1; }
            if c == '}' { depth -= 1; min = min.min(depth); }
        }
        depth == 0 && min >= 0
    }

    #[test]
    fn single_function_body_is_unwrapped() {
        let code = "function foo__frame0() {\n\tA;\n\tB;\n}";
        assert_eq!(extract_function_body(code), "A;\nB;");
    }

    #[test]
    fn nested_braces_preserved_and_dedented() {
        let code = "function foo() {\n\tif (x) {\n\t\tB;\n\t}\n}";
        assert_eq!(extract_function_body(code), "if (x) {\n\tB;\n}");
    }

    #[test]
    fn two_functions_on_one_frame_merge_bodies() {
        // Two SSF2 frame scripts landing on the same frame (slot-aliased anim).
        // Both bodies must run, and the result must be structurally valid (no
        // leaked inner `function` wrapper, no stray brace).
        let code = "function tech__frame0() {\n\tA;\n}\n\nfunction tech__frame0() {\n\tB;\n}";
        let body = extract_function_body(code);
        assert!(body.contains("A;") && body.contains("B;"), "both bodies kept: {body}");
        assert!(!body.contains("function "), "inner wrapper leaked: {body}");
        assert!(structurally_valid(&body), "unbalanced output: {body}");
    }

    #[test]
    fn commented_braces_do_not_confuse_matcher() {
        // A fully commented-out SSF2-only if-block must not throw off brace matching.
        let code = "function f() {\n\tA;\n\t// [SSF2-only: x] if (g()) {\n\t// [SSF2-dead] }\n}";
        let body = extract_function_body(code);
        assert!(structurally_valid(&body), "unbalanced: {body}");
        assert!(!body.contains("function "), "wrapper leaked: {body}");
    }
}
