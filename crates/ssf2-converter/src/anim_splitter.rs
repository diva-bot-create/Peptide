/// Animation splitter — applies anim_split_rules.json to expand multi-label
/// SSF2 animations into separate Fraymakers animations.
///
/// Each `SplitAnim` describes one output Fraymakers animation.
/// It references the source animation by name plus a frame range [start, end).
/// The entity generator uses this to slice sprite_boxes and anim_images.

use std::collections::BTreeMap;
use crate::sprite_parser::AnimationBoxData;

/// One output Fraymakers animation, potentially a sub-range of an SSF2 animation.
#[derive(Debug, Clone)]
pub struct SplitAnim {
    /// Fraymakers animation name (e.g. "strong_forward_in")
    pub fm_name: String,
    /// Source SSF2/FM animation key in sprite_boxes / anim_images
    pub source_anim: String,
    /// Start frame within source animation (0-based, inclusive)
    pub start_frame: u16,
    /// End frame within source animation (exclusive); u16::MAX = use full length
    pub end_frame: u16,
    /// Frame labels within this split: (label_name, frame_within_split) — same order as AnimationBoxData::frame_labels
    pub labels: Vec<(String, u16)>,
    /// Whether this animation's tail should loop (AnimationStats endType=LOOP)
    pub loop_tail: bool,
    /// Frame within this split at which the loop starts (relative to split start)
    pub loop_frame: Option<u16>,
    /// After the [start_frame, end_frame) slice, append this many frames taken from the
    /// HEAD of the source (frames 0..append_head_frames). Used to make a loop seamless when
    /// the loop's entry frames were peeled off into a separate intro animation: e.g. a
    /// walk_loop sliced from [walk_in_len, total) appends [0, walk_in_len) so the full cycle
    /// plays starting just after the lean-in. 0 = no appended frames (the common case).
    pub append_head_frames: u16,
}

// Fraymakers template intro/outro slice lengths, in SOURCE (pre-doubling, 30fps) frames =
// round(template_60fps_len / 2): walk_in/walk_out (template 3) -> 2, fall_in (template 6) -> 3.
const WALK_IN_FRAMES: u16 = 2;
const WALK_OUT_FRAMES: u16 = 2;
const FALL_IN_FRAMES: u16 = 3;
/// stand_turn is the idle pose mirrored in place; it's held for the FM-template turn
/// length, NOT the full idle loop. Source (30fps) frames → ~2x engine frames.
const STAND_TURN_FRAMES: u16 = 3;

/// Produce the list of output animations for a character.
/// Returns one `SplitAnim` per Fraymakers animation to emit.
pub fn split_animations(
    source_anims: &BTreeMap<String, crate::extractor::AnimationInfo>,
    sprite_boxes: &BTreeMap<String, AnimationBoxData>,
    jump_startup: u16,
) -> Vec<SplitAnim> {
    let mut out: Vec<SplitAnim> = Vec::new();

    for anim_name in source_anims.keys() {
        let sb = sprite_boxes.get(anim_name);
        let total = sb.map(|s| s.total_frames).unwrap_or(1);
        let labels: Vec<(String, u16)> = sb
            .map(|s| s.frame_labels.clone())
            .unwrap_or_default();

        // Build label map: label_name -> frame_offset (within this animation)
        let label_map: BTreeMap<&str, u16> = labels.iter()
            .map(|(lbl, f)| (lbl.as_str(), *f))
            .collect();

        // Helper: find frame for a label, defaulting to end of animation
        let _lf = |lbl: &str| -> u16 { *label_map.get(lbl).unwrap_or(&total) };

        // debug (`PEPTIDE_DUMP_ANIM_LABELS`): each source animation's internal frame labels.
        if std::env::var("PEPTIDE_DUMP_ANIM_LABELS").is_ok() {
            eprintln!("[anim-labels] {anim_name} total={total} labels={labels:?} switches={:?}",
                sb.map(|s| s.content_switches.clone()).unwrap_or_default());
        }

        match anim_name.as_str() {

            // ── Aerials: split active frames + land animation ──────────────────
            "aerial_neutral" | "aerial_forward" | "aerial_back" | "aerial_up" | "aerial_down" => {
                let land_frame = label_map.get("continue")
                    .or_else(|| label_map.get("repeat"))
                    .copied();
                if let Some(lf_continue) = land_frame {
                    push_split(&mut out, anim_name, anim_name, 0, lf_continue, &labels, false, None);
                    let land_name = format!("{}_land", anim_name);
                    push_split(&mut out, &land_name, anim_name, lf_continue, total, &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Strong attacks: in / charge / attack ───────────────────────────
            "strong_forward" | "strong_up" | "strong_down" => {
                if let (Some(&charge_f), Some(&attack_f)) = (label_map.get("charging"), label_map.get("attack")) {
                    let in_name    = format!("{}_in", anim_name);
                    let charge_name = format!("{}_charge", anim_name);
                    let attack_name = format!("{}_attack", anim_name);
                    push_split(&mut out, &in_name,     anim_name, 0,        charge_f, &labels, false, None);
                    push_split(&mut out, &charge_name, anim_name, charge_f, attack_f, &labels, true, Some(0));
                    push_split(&mut out, &attack_name, anim_name, attack_f, total,    &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Grab / carry: standing grab, dash grab, hold, pummel ───────────
            "grab" | "carry" => {
                if anim_name == "carry" && out.iter().any(|s| s.fm_name == "grab") {
                    // carry shares the same inner sprite as grab — skip, already emitted
                    continue;
                }
                let dashgrab_f = label_map.get("dashgrab").copied();
                let grabbed_f  = label_map.get("grabbed").copied()
                    .or_else(|| label_map.get("grabbed2").copied());
                let attack_f   = label_map.get("attack").copied();

                if let (Some(dg_f), Some(gb_f), Some(atk_f)) = (dashgrab_f, grabbed_f, attack_f) {
                    let grab_end = dg_f;
                    push_split(&mut out, "grab",        anim_name, 0,     grab_end, &labels, false, None);
                    push_split(&mut out, "dash_grab",   anim_name, dg_f,  gb_f,    &labels, false, None);
                    push_split(&mut out, "grab_hold",   anim_name, gb_f,  atk_f,   &labels, true, Some(0));
                    push_split(&mut out, "grab_pummel", anim_name, atk_f, total,   &labels, false, None);
                } else if label_map.contains_key("grabbed") {
                    // samus-style: grabbed comes before dashgrab
                    let gb_f  = label_map["grabbed"];
                    let _loop_f = label_map.get("loop").copied().unwrap_or(gb_f);
                    let atk_f  = label_map.get("attack").copied().unwrap_or(total);
                    let dg_f   = label_map.get("dashgrab").copied().unwrap_or(atk_f);
                    push_split(&mut out, "grab",        anim_name, 0,     gb_f,  &labels, false, None);
                    push_split(&mut out, "grab_hold",   anim_name, gb_f,  atk_f, &labels, true, Some(0));
                    push_split(&mut out, "grab_pummel", anim_name, atk_f, dg_f,  &labels, false, None);
                    push_split(&mut out, "dash_grab",   anim_name, dg_f,  total, &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Run: dash / run / run_turn ─────────────────────────────────────
            "run" => {
                // Pattern A: run, dash, turn  (mario/captainfalcon/naruto)
                // Pattern B: run, run, turn   (jigglypuff/link/samus) — first 'run' is loop
                let _first_label = labels.first().map(|(l, _)| l.as_str()).unwrap_or("");
                let has_dash = label_map.contains_key("dash");
                let turn_f = label_map.get("turn").copied();

                if has_dash {
                    // Pattern A: run(loop)=first, dash=initial dash, turn=turnaround
                    let dash_f = label_map["dash"];
                    let run_f  = 0u16;
                    let turn_f = turn_f.unwrap_or(total);
                    push_split(&mut out, "dash",     anim_name, dash_f, turn_f, &labels, false, None);
                    push_split(&mut out, "run",      anim_name, run_f,  dash_f, &labels, true, Some(0));
                    push_split(&mut out, "run_turn", anim_name, turn_f, total,  &labels, false, None);
                } else if let Some(turn_f) = turn_f {
                    // Pattern B: labels are [run_frame_0, run_frame_N, turn]
                    // first segment = loop, second run segment doesn't exist (same frames)
                    // just split run / run_turn
                    push_split(&mut out, "run",      anim_name, 0,      turn_f, &labels, true, Some(0));
                    push_split(&mut out, "run_turn", anim_name, turn_f, total,  &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Jump: jump_squat + jump_in / jump_loop / jump_out (+ jump_back) ──
            // SSF2 has no jump-squat animation; the character holds the start of its
            // jump sprite for `jumpStartup` grounded frames. So we slice the first
            // `jumpStartup` frames off the front as `jump_squat`, then divide the
            // remaining airborne portion into thirds → jump_in / jump_loop / jump_out
            // (jump_loop is the looping middle). If the sprite has a `backflip` label,
            // only the forward portion [0, backflip) is split this way; [backflip, end)
            // stays as jump_back.
            "jump" => {
                let fwd_end = label_map.get("backflip").copied().unwrap_or(total);
                let js = jump_startup.min(fwd_end);
                // jump_squat: grounded startup frames. The slice is `jumpStartup` SOURCE
                // (30fps) frames; the global 30->60fps doubling later makes the final
                // jump_squat 2 x jumpStartup engine frames.
                eprintln!("anim: jump_squat = {} source frames (SSF2 jumpStartup={}) -> {} engine frames (2x)",
                    js, jump_startup, js * 2);
                if js > 0 { push_split(&mut out, "jump_squat", anim_name, 0, js, &labels, false, None); }
                // Remaining airborne frames, divided into thirds (rounded).
                let rem = fwd_end.saturating_sub(js);
                if rem == 0 {
                    // No airborne frames to split — emit the whole forward part as jump_in.
                    push_split(&mut out, "jump_in", anim_name, js, fwd_end, &labels, false, None);
                } else {
                    let third = ((rem as f32) / 3.0).round().max(1.0) as u16;
                    let in_end   = (js + third).min(fwd_end);
                    let loop_end = (js + 2 * third).min(fwd_end);
                    push_split(&mut out, "jump_in",   anim_name, js,       in_end,   &labels, false, None);
                    push_split(&mut out, "jump_loop", anim_name, in_end,   loop_end, &labels, true,  Some(0));
                    push_split(&mut out, "jump_out",  anim_name, loop_end, fwd_end,  &labels, false, None);
                }
                if fwd_end < total {
                    push_split(&mut out, "jump_back", anim_name, fwd_end, total, &labels, false, None);
                }
            }

            // ── Jump midair (double jump): forward / back ─────────────────────
            "jump_midair" => {
                if let Some(&bf) = label_map.get("backflip") {
                    push_split(&mut out, "jump_midair",      anim_name, 0,  bf,    &labels, false, None);
                    push_split(&mut out, "jump_midair_back", anim_name, bf, total, &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Stand: stand / idle_bored (uncrouch NOT extracted from stand) ──
            // FM's canonical standing-state animation is "stand" (CState.STAND),
            // NOT "idle". The bored flavor segment stays "idle_bored" (extra anim,
            // no state references it).
            "stand" => {
                let bored_f = label_map.get("bored")
                    .or_else(|| label_map.get("blink"))
                    .copied();
                let uncrouch_f = label_map.get("uncrouch").copied();
                if let Some(bf) = bored_f {
                    let idle_end = bf;
                    // stand loops at its own end
                    push_split(&mut out, "stand",      anim_name, 0,   idle_end, &labels, true, Some(0));
                    // bored: from bored label to uncrouch (or end)
                    let bored_end = uncrouch_f.unwrap_or(total);
                    push_split(&mut out, "idle_bored", anim_name, bf, bored_end, &labels, false, None);
                    // uncrouch NOT emitted from stand — crouch_out comes from crouch anim
                } else {
                    push_split(&mut out, "stand", anim_name, 0, uncrouch_f.unwrap_or(total), &labels, true, Some(0));
                }
            }

            // ── Crouch: crouch_in / crouch_loop (+ naruto crawl variants) ──────
            "crouch" => {
                let loop_f = label_map.get("loop")
                    .or_else(|| label_map.get("redo"))
                    .copied();
                if let Some(lf) = loop_f {
                    push_split(&mut out, "crouch_in",   anim_name, 0,   lf,    &labels, false, None);
                    // naruto also has crawlforward/crawlback after walking
                    let crawl_f = label_map.get("crawlforward").copied();
                    let crawlb_f = label_map.get("crawlback").copied();
                    let walk_f  = label_map.get("walking").copied();
                    if let Some(cf) = crawl_f {
                        let loop_end = walk_f.unwrap_or(cf);
                        push_split(&mut out, "crouch_loop",    anim_name, lf,     loop_end, &labels, true, Some(0));
                        push_split(&mut out, "crouch_forward", anim_name, cf, crawlb_f.unwrap_or(total), &labels, true, Some(0));
                        if let Some(cb_f) = crawlb_f {
                            push_split(&mut out, "crouch_back", anim_name, cb_f, total, &labels, true, Some(0));
                        }
                    } else {
                        push_split(&mut out, "crouch_loop", anim_name, lf, total, &labels, true, Some(0));
                    }
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Crouch_out: extract from 'uncrouch' label in crouch (if present)
            // Actually crouch_out comes from the crouch sprite's uncrouch label.
            // It's handled above in "crouch" — we don't emit it from "idle".

            // ── Special up: active frames / helpless fall ─────────────────────
            "special_up" | "special_up_air" => {
                let fall_f = label_map.get("fallLoop").copied();
                if let Some(ff) = fall_f {
                    push_split(&mut out, anim_name, anim_name, 0,   ff,    &labels, false, None);
                    // helpless: only emit once (special_up and special_up_air both reference it)
                    if !out.iter().any(|s| s.fm_name == "helpless") {
                        push_split(&mut out, "helpless", anim_name, ff, total, &labels, true, Some(0));
                    }
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Item smash: in / charge / attack ──────────────────────────────
            "item_smash" => {
                if let (Some(&charge_f), Some(&attack_f)) = (label_map.get("charging"), label_map.get("attack")) {
                    push_split(&mut out, "item_smash_in",     anim_name, 0,        charge_f, &labels, false, None);
                    push_split(&mut out, "item_smash_charge", anim_name, charge_f, attack_f, &labels, true, Some(0));
                    push_split(&mut out, "item_smash",        anim_name, attack_f, total,    &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Item throw: 4 directions ───────────────────────────────────────
            "item_throw" | "item_throw_air" => {
                let down_f    = label_map.get("toss_down").copied();
                let up_f      = label_map.get("toss_up").copied();
                let forward_f = label_map.get("toss_forward").copied();
                if let (Some(df), Some(uf), Some(ff)) = (down_f, up_f, forward_f) {
                    let back_end = df;
                    let down_end = uf;
                    let up_end   = ff;
                    push_split(&mut out, &format!("{}_back",    anim_name), anim_name, 0,   back_end, &labels, false, None);
                    push_split(&mut out, &format!("{}_down",    anim_name), anim_name, df,  down_end, &labels, false, None);
                    push_split(&mut out, &format!("{}_up",      anim_name), anim_name, uf,  up_end,   &labels, false, None);
                    push_split(&mut out, &format!("{}_forward", anim_name), anim_name, ff,  total,    &labels, false, None);
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Item screw: active / helpless ─────────────────────────────────
            "item_screw" => {
                if let Some(&ff) = label_map.get("fallLoop") {
                    push_split(&mut out, "item_screw", anim_name, 0,  ff,    &labels, false, None);
                    if !out.iter().any(|s| s.fm_name == "helpless") {
                        push_split(&mut out, "helpless", anim_name, ff, total, &labels, true, Some(0));
                    }
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Fly (tumble): regular / backflip ──────────────────────────────
            "fly" => {
                if let Some(&bf) = label_map.get("backflip") {
                    push_split(&mut out, "tumble",      anim_name, 0,  bf,    &labels, false, None);
                    push_split(&mut out, "tumble_back", anim_name, bf, total, &labels, false, None);
                } else {
                    // No backflip: just emit as tumble
                    push_split(&mut out, "tumble", anim_name, 0, total, &labels, false, None);
                }
            }

            // ── Helpless: reg / spec variants ────────────────────────────────
            "helpless" => {
                if let Some(&sf) = label_map.get("spec") {
                    let reg_f = label_map.get("reg").copied().unwrap_or(0);
                    push_split(&mut out, "helpless",         anim_name, reg_f, sf,    &labels, true, Some(0));
                    push_split(&mut out, "helpless_special", anim_name, sf,    total, &labels, true, Some(0));
                } else {
                    push_split(&mut out, "helpless", anim_name, 0, total, &labels, true, Some(0));
                }
            }

            // ── Ledge hang/lean: ledge_in / ledge_loop ────────────────────────
            "ledge_hang" | "ledge_lean" => {
                let loop_f = label_map.get("loop").copied();
                if let Some(lf) = loop_f {
                    let tether_f = label_map.get("tetherGrab").copied();
                    let in_end = lf;
                    push_split(&mut out, "ledge_in",   anim_name, 0,   in_end, &labels, false, None);
                    push_split(&mut out, "ledge_loop", anim_name, lf,  tether_f.unwrap_or(total), &labels, true, Some(0));
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Shield: shield_in / shield_loop (/ shield_out if shielddrop) ──
            "shield" => {
                let loop_f = label_map.get("loop")
                    .or_else(|| label_map.get("pause"))
                    .or_else(|| label_map.get("again"))
                    .or_else(|| label_map.get("stopped"))
                    .copied();
                if let Some(lf) = loop_f {
                    push_split(&mut out, "shield_in",   anim_name, 0,  lf,    &labels, false, None);
                    let drop_f = label_map.get("shielddrop").copied();
                    let loop_end = drop_f.unwrap_or(total);
                    push_split(&mut out, "shield_loop", anim_name, lf, loop_end, &labels, true, Some(0));
                    if let Some(df) = drop_f {
                        push_split(&mut out, "shield_out", anim_name, df, total, &labels, false, None);
                    }
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Fall: fall_in (entry) / fall_loop (seamless cycle) [+ fall_special] ──
            // Same scheme as walk: peel FALL_IN_FRAMES (template 6 -> 3 source) as fall_in, the
            // rest loops as fall_loop with the peeled head frames appended. A uspecFall label
            // splits the helpless tail off as fall_special.
            "fall" => {
                let fi = FALL_IN_FRAMES.min(total.saturating_sub(1));
                let uspec_f = label_map.get("uspecFall").copied();
                let loop_end = uspec_f.unwrap_or(total);
                if fi >= 1 { push_split(&mut out, "fall_in", anim_name, 0, fi, &labels, false, None); }
                if loop_end > fi {
                    push_split(&mut out, "fall_loop", anim_name, fi, loop_end, &labels, true, Some(0));
                    if let Some(s) = out.last_mut() { s.append_head_frames = fi; }
                } else {
                    push_split(&mut out, "fall_loop", anim_name, 0, total, &labels, true, Some(0));
                }
                if let Some(uf) = uspec_f {
                    push_split(&mut out, "fall_special", anim_name, uf, total, &labels, true, Some(0));
                }
            }

            // ── Swim / wall-stick: loop at 'loop'/'redo', else full ──────────────
            "swim" | "wall_stick" => {
                let loop_f = label_map.get("loop").or_else(|| label_map.get("redo")).copied();
                if let Some(lf) = loop_f {
                    push_split(&mut out, anim_name, anim_name, 0, total, &labels, true, Some(lf));
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Walk: walk_in (lean-in) / walk_loop (seamless cycle) / walk_out (from land) ──
            // Template lengths are 60fps; our source is 30fps (doubled later), so the slice
            // lengths are template_len/2 (walk_in 3 -> 2, walk_out 3 -> 2). walk_loop takes the
            // remaining frames and appends the peeled head frames so the full cycle plays
            // seamlessly starting just after the lean-in. walk_out reuses the start of `skid`.
            "walk" => {
                let wi = WALK_IN_FRAMES.min(total.saturating_sub(1));
                if wi >= 1 { push_split(&mut out, "walk_in", anim_name, 0, wi, &labels, false, None); }
                if total > wi {
                    push_split(&mut out, "walk_loop", anim_name, wi, total, &labels, true, Some(0));
                    if let Some(s) = out.last_mut() { s.append_head_frames = wi; }
                } else {
                    push_split(&mut out, "walk_loop", anim_name, 0, total, &labels, true, Some(0));
                }
                push_split(&mut out, "walk_out", "skid", 0, WALK_OUT_FRAMES, &[], false, None);
            }

            // ── Buried / dizzy: loop at tail ──────────────────────────────────
            "buried" | "dizzy" => {
                let loop_f = label_map.get("loop")
                    .or_else(|| label_map.get("again"))
                    .copied();
                if let Some(lf) = loop_f {
                    push_split(&mut out, anim_name, anim_name, 0, total, &labels, true, Some(lf));
                } else {
                    push_full(&mut out, anim_name, total, &labels);
                }
            }

            // ── Land: light / heavy (dodgeland) ──────────────────────────────
            "land" => {
                if let Some(&df) = label_map.get("dodgeland") {
                    push_split(&mut out, "land_light", anim_name, 0,  df,    &labels, false, None);
                    push_split(&mut out, "land_heavy", anim_name, df, total, &labels, false, None);
                } else {
                    push_split(&mut out, "land_light", anim_name, 0, total, &labels, false, None);
                }
            }

            // ── Crash (knocked-down → getup → collapse): split by frame label ──
            // SSF2's `crash` (GetUp) sprite holds the whole sequence on one timeline.
            // labels (e.g. mario): bounce, dead, getup, done, standloop, die, collapse.
            // Each named split runs until the NEXT named split-point:
            //   bounce    -> crash_bounce
            //   dead      -> crash_loop              (looping; lying knocked-down)
            //   getup     -> crash_get_up
            //   standloop -> crash_collapse_in_loop  (looping; UNUSED slot)
            //   collapse  -> crash_collapse          (UNUSED slot)
            // NB: animations.jsonc renames SSF2 `crash` -> `crash_bounce` BEFORE the
            // splitter runs, so the incoming anim_name here is `crash_bounce`.
            "crash_bounce" => {
                let dead_f      = label_map.get("dead").copied();
                let getup_f     = label_map.get("getup").copied();
                let standloop_f = label_map.get("standloop").copied();
                let collapse_f  = label_map.get("collapse").copied();
                let bounce_start = label_map.get("bounce").copied().unwrap_or(0);
                let bounce_end = dead_f.or(getup_f).or(standloop_f).or(collapse_f).unwrap_or(total);
                push_split(&mut out, "crash_bounce", anim_name, bounce_start, bounce_end, &labels, false, None);
                if let Some(f) = dead_f {
                    let end = getup_f.or(standloop_f).or(collapse_f).unwrap_or(total);
                    push_split(&mut out, "crash_loop", anim_name, f, end, &labels, true, Some(0));
                }
                if let Some(f) = getup_f {
                    let end = standloop_f.or(collapse_f).unwrap_or(total);
                    push_split(&mut out, "crash_get_up", anim_name, f, end, &labels, false, None);
                }
                if let Some(f) = standloop_f {
                    let end = collapse_f.unwrap_or(total);
                    push_split(&mut out, "crash_collapse_in_loop", anim_name, f, end, &labels, true, Some(0));
                }
                if let Some(f) = collapse_f {
                    push_split(&mut out, "crash_collapse", anim_name, f, total, &labels, false, None);
                }
            }

            // ── Jab4 rapid: loop at 'again' ───────────────────────────────────
            "jab4" => {
                let loop_f = label_map.get("again").copied();
                push_split(&mut out, anim_name, anim_name, 0, total, &labels, loop_f.is_some(), loop_f);
            }

            // ── Idle-sprite bleedthrough: truncate at uncrouch/bored/blink ────
            // item_float, ladder, respawn, special, select_screen share idle sprite
            "item_float" | "ladder" | "respawn" | "special" | "select_screen" => {
                let end_f = label_map.get("bored")
                    .or_else(|| label_map.get("blink"))
                    .or_else(|| label_map.get("uncrouch"))
                    .copied()
                    .unwrap_or(total);
                push_split(&mut out, anim_name, anim_name, 0, end_f, &labels, false, None);
            }

            // ── Hurt / defeat / stunned / screenko / starko / star_ko:
            //    ignore tail (engine routes hurt variants) ─────────────────────
            "hurt" | "defeat" | "stunned" | "screenko" | "starko" | "star_ko" => {
                // Keep only the first segment before done1/hurt2 etc.
                let first_tail = label_map.get("done1")
                    .or_else(|| label_map.get("hurt2"))
                    .copied()
                    .unwrap_or(total);
                push_split(&mut out, anim_name, anim_name, 0, first_tail, &labels, false, None);
            }

            // ── Taunt sprite bleedthrough: ignore tail ────────────────────────
            "taunt_up" | "taunt_down" => {
                let end_f = label_map.get("taunt_side").copied().unwrap_or(total);
                push_split(&mut out, anim_name, anim_name, 0, end_f, &labels, false, None);
            }

            // ── Tech: ignore 'done' tail (1f exit marker) ────────────────────
            "tech" | "tech_roll" => {
                let end_f = label_map.get("done").copied().unwrap_or(total);
                push_split(&mut out, anim_name, anim_name, 0, end_f, &labels, false, None);
            }

            // ── Tilt_forward: 'remove' is endlag, include it ─────────────────
            // special_side_air afterHit: include in animation
            // special_down_air continue+afterHit: include in animation
            // throw_back loop+loop2: keep full (loop is grab hold, loop2 is endlag)
            // victory win1+loop: keep full
            // item_homerun attack: keep full (windup+active)
            // jab1 goback/continue: keep full
            // ledge_hang tetherGrab: already handled above

            // ── Default: emit as-is (no split needed) ────────────────────────
            _ => {
                push_full(&mut out, anim_name, total, &labels);
            }
        }
    }

    // Deduplicate by fm_name — keep the first occurrence of each
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.fm_name.clone()));

    // ── Base-before-variant guarantee ────────────────────────────────────────
    // A variant animation (special_down_loop, item_throw_back, …) is only valid if
    // its BASE (special_down, item_throw) also exists — the engine reaches a variant
    // from its base via CState / landAnimation / script flow, so a variant whose base
    // is absent is broken. For every emitted variant whose base is MISSING, synthesize
    // the base from the variant's own frames so the base is intact.
    //
    // Canonical suffix-only groups (strong_*_in/_charge/_attack) intentionally have NO
    // base, so `_in`/`_charge`/`_attack` are NOT treated as base-requiring suffixes, and
    // any base that has those siblings is skipped. Iterates to a fixpoint (a synthesized
    // base may itself be a variant, e.g. item_throw_air → item_throw), capped for safety.
    const BASE_SUFFIXES: &[&str] = &["_loop", "_endlag", "_out", "_back", "_forward", "_up", "_down", "_land", "_hold", "_air"];
    for _ in 0..6 {
        let emitted: std::collections::HashSet<String> = out.iter().map(|s| s.fm_name.clone()).collect();
        let mut to_add: Vec<SplitAnim> = Vec::new();
        for s in &out {
            for suf in BASE_SUFFIXES {
                let Some(base) = s.fm_name.strip_suffix(suf) else { continue };
                if base.is_empty() || emitted.contains(base) { break; }
                // Only synthesize a base that is a REAL source animation (i.e. SSF2 had it,
                // e.g. item_throw ⟵ "toss"). Directional/charge GROUPS whose canonical form
                // is suffix-only — aerial_*, throw_*, tilt_* — have no base animation and
                // their prefix is not a source key, so they are skipped here.
                if !source_anims.contains_key(base) { break; }
                // Skip bases whose canonical form is the _in/_loop/_out (or _in/_charge/_attack)
                // set — the real entry already exists (crouch_in, shield_in, strong_forward_in),
                // so no bare base is needed. item_throw has no such sibling → it is synthesized.
                if emitted.contains(&format!("{base}_in"))
                    || emitted.contains(&format!("{base}_charge"))
                    || emitted.contains(&format!("{base}_attack")) { break; }
                if to_add.iter().any(|a| a.fm_name == base) { break; }
                let mut b = s.clone();
                b.fm_name = base.to_string();
                eprintln!("anim base-fill: synthesized missing base '{}' for variant '{}'", base, s.fm_name);
                to_add.push(b);
                break; // one base per variant name
            }
        }
        if to_add.is_empty() { break; }
        out.extend(to_add);
    }

    // ── Slot aliases: reuse one emitted animation for additional template slots ──
    // Several Fraymakers motion slots have no distinct SSF2 source and are meant to
    // reuse another animation. For each (target_slot <- source_fm_name) pair, if the
    // source animation was emitted and the target slot isn't already present, clone
    // the source SplitAnim (same frames/labels) under the target name. Graceful: a
    // character lacking the source simply doesn't get the alias.
    const SLOT_ALIASES: &[(&str, &str)] = &[
        // All 8 directional airdashes reuse the air-dodge animation.
        ("airdash_up",            "airdodge"),
        ("airdash_down",          "airdodge"),
        ("airdash_forward",       "airdodge"),
        ("airdash_back",          "airdodge"),
        ("airdash_forward_up",    "airdodge"),
        ("airdash_forward_down",  "airdodge"),
        ("airdash_back_up",       "airdodge"),
        ("airdash_back_down",     "airdodge"),
        // Airdash landing-lag slots reuse the heavy-land animation.
        ("airdash_land",             "land_heavy"),
        ("airdash_land_uncanceled",  "land_heavy"),
        ("airdash_land_whiff",       "land_heavy"),
        // Airdash freefall reuses the air-dodge pose.
        ("airdash_freefall",         "airdodge"),
        ("airdash_freefall_whiff",   "airdodge"),
        // SSF2 has ONE hurt animation; Fraymakers splits it by knockback magnitude —
        // reuse the single hurt for every hurt slot.
        ("hurt_light_low",     "hurt"),
        ("hurt_light_middle",  "hurt"),
        ("hurt_light_high",    "hurt"),
        ("hurt_medium",        "hurt"),
        // hurt_heavy comes from the SSF2 "flying" xframe (the launched somersault),
        // not the light-hurt reuse.
        ("hurt_thrown",        "hurt"),
        // Crouch exit reuses the crouch entry (SSF2 has no separate un-crouch sprite).
        ("crouch_out",   "crouch_in"),
        // Stand turn reuses the idle pose (SSF2 has no turn-in-place sprite).
        ("stand_turn",   "stand"),
        // Parry has no SSF2 equivalent — reuse the shield startup pose.
        ("parry_in",      "shield_in"),
        ("parry_success", "shield_in"),
        ("parry_fail",    "shield_in"),
        // Shield-hurt reuses the held-shield pose.
        ("shield_hurt",  "shield_loop"),
        // Emote (taunt) reuses an existing taunt.
        ("emote",        "taunt_up"),
        // Ledge wall/edge poses: reuse the ledge-lean (SSF2 'edgelean') for the
        // hang in/loop slots when no dedicated split exists.
        ("ledge_in",     "ledge_lean"),
        ("ledge_loop",   "ledge_lean"),
        // Ledge jump / wall jump reuse the jump startup + initial rise.
        ("ledge_jump_in", "jump_squat"),
        ("ledge_jump",    "jump_in"),
        ("wall_jump_in",  "jump_squat"),
        ("wall_jump",     "jump_in"),
        // Knockdown (SSF2 'crash') sequence: bounce is the source; the lie/getup/roll
        // slots reuse it (SSF2 has no distinct sprites for each phase).
        ("crash_loop",   "crash_bounce"),
        ("crash_get_up", "crash_bounce"),
        ("crash_roll",   "crash_bounce"),
        ("crash_attack", "crash_bounce"),
        // tech_ground also receives the getup motion (the get-up portion of the crash
        // sprite). Only fills if a char has no dedicated tech-ground animation already.
        ("tech_ground",  "crash_get_up"),
        // Special-fall (helpless) state animation.
        ("fall_special", "helpless"),
        // Assist call (Fraymakers-specific) — placeholder reuse of a neutral pose.
        ("assist_call",     "stand"),
        ("assist_call_air", "jump_in"),
    ];
    for (target, src) in SLOT_ALIASES {
        if out.iter().any(|s| s.fm_name == *target) { continue; }
        if let Some(mut a) = out.iter().find(|s| s.fm_name == *src).cloned() {
            a.fm_name = target.to_string();
            // stand_turn is the idle POSE mirrored horizontally in place (the flip is
            // applied in entity_gen), held for the FM-template turn length — not the
            // whole idle loop. Clip to STAND_TURN_FRAMES from the idle's first frame.
            if *target == "stand_turn" {
                a.start_frame = 0;
                a.end_frame = STAND_TURN_FRAMES;
                a.loop_tail = false;
                a.loop_frame = None;
                a.append_head_frames = 0;
                a.labels.retain(|(_, f)| *f < STAND_TURN_FRAMES);
            }
            log::debug!("anim alias: '{}' <- reuse of '{}'", target, src);
            out.push(a);
        }
    }

    // debug (`PEPTIDE_DUMP_ANIM_SPLITS`): the EXACT source->Fraymakers mapping, so a
    // timing check can pair them instead of guessing from name suffixes. `loop` marks a
    // split whose tail cycles — its emitted length is a cycle, not added duration, so it
    // must not be summed into a family total when comparing against the source.
    if std::env::var("PEPTIDE_DUMP_ANIM_SPLITS").is_ok() {
        for a in &out {
            let end = if a.end_frame == u16::MAX { "end".to_string() } else { a.end_frame.to_string() };
            eprintln!("[anim-split] {} <- {} [{}..{}){}{}",
                a.fm_name, a.source_anim, a.start_frame, end,
                if a.loop_tail { " loop" } else { "" },
                if a.append_head_frames > 0 { format!(" +head{}", a.append_head_frames) } else { String::new() });
        }
    }
    out
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn push_full(out: &mut Vec<SplitAnim>, name: &str, total: u16, labels: &[(String, u16)]) {
    out.push(SplitAnim {
        fm_name:     name.to_string(),
        source_anim: name.to_string(),
        start_frame: 0,
        end_frame:   total,
        labels:      labels.to_vec(),
        loop_tail:   false,
        loop_frame:  None,
        append_head_frames: 0,
    });
}

fn push_split(
    out: &mut Vec<SplitAnim>,
    fm_name: &str,
    source_anim: &str,
    start: u16,
    end: u16,
    all_labels: &[(String, u16)],
    loop_tail: bool,
    loop_frame: Option<u16>,
) {
    // Slice labels that fall within [start, end), rebased to 0
    let labels: Vec<(String, u16)> = all_labels.iter()
        .filter(|(_, f)| *f >= start && (end == u16::MAX || *f < end))
        .map(|(l, f)| (l.clone(), f.saturating_sub(start)))
        .collect();

    out.push(SplitAnim {
        fm_name:     fm_name.to_string(),
        source_anim: source_anim.to_string(),
        start_frame: start,
        end_frame:   end,
        labels,
        loop_tail,
        loop_frame,
        append_head_frames: 0,
    });
}
