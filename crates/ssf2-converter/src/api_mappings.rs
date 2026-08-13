/// SSF2 → Fraymakers API mapping table.
///
/// SSF2 (ActionScript 3) uses a MovieClip timeline model with SSF2API/SSF2Character
/// wrappers. Fraymakers uses Haxe with Entity/Character/GameObject class hierarchy.
///
/// This module provides:
///   1. Method-level mappings (SSF2 call → FM equivalent)
///   2. Property mappings (SSF2 property access → FM getter/setter)
///   3. Constant mappings (SSF2 state constants → FM CState/etc)
///   4. Pattern-level transformations (multi-statement → single FM call)

use std::collections::BTreeMap;

// ─── SSF2 "self.self" Pattern ──────────────────────────────────────────────────
// In decompiled SSF2 sub-MC code, "self.self" refers to the character instance.
// In Fraymakers, the Script.hx `self` already IS the character/entity.
// So "self.self.endAttack()" → "self.endAnimation()"
//
// This is handled at the text level in the post-processor.

/// Remove SSF2 readiness guard if-blocks that are always-true in Fraymakers.
///
/// SSF2 wraps initialization code with guards like:
///   `if (SSF2API.isReady()) { ... }`
///   `if (self && SSF2API.isReady()) { ... }`
///   `if (SSF2API.isReady() && self) { ... }`
///
/// In Fraymakers Script.hx, `self` is always valid and the API is always ready,
/// so these guards are unnecessary. This function removes the if-wrapper and
/// dedents the body by one level, effectively inlining the body.
pub fn remove_readiness_guards(code: &str) -> String {
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();

        if is_isready_guard(trimmed) {
            // Count braces to find the matching close-brace of this if block
            let mut depth: i32 = 0;
            let mut found_open = false;
            let mut body_start = i;
            let mut j = i;

            'scan: while j < lines.len() {
                for ch in lines[j].chars() {
                    match ch {
                        '{' => {
                            depth += 1;
                            if !found_open {
                                found_open = true;
                                body_start = j;
                            }
                        }
                        '}' => {
                            depth -= 1;
                            if found_open && depth == 0 {
                                break 'scan;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }

            if found_open && depth == 0 {
                // Emit body lines (body_start+1 .. j), each dedented by one tab
                // Add a comment so the reader knows a guard was stripped
                out.push("// [FM] isReady guard removed — always true in Fraymakers".to_string());
                for body_line in &lines[body_start + 1..j] {
                    out.push(strip_one_tab(body_line).to_string());
                }
                i = j + 1; // skip the closing `}` line
                continue;
            } else {
                // Couldn't parse — emit original
                out.push(line.to_string());
                i += 1;
                continue;
            }
        }

        out.push(line.to_string());
        i += 1;
    }

    // Re-join, preserving trailing newline if original had one
    let mut joined = out.join("\n");
    if code.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

/// Returns true if a trimmed line is a SSF2 readiness guard if-statement.
fn is_isready_guard(trimmed: &str) -> bool {
    // Must start with `if (` or `if(`
    if !trimmed.starts_with("if (") && !trimmed.starts_with("if(") {
        return false;
    }
    // Must contain SSF2API.isReady() — the canonical readiness check
    trimmed.contains("SSF2API.isReady()")
}

/// Remove `if (… isForcedCrash …) { … }` blocks entirely. SSF2's `isForcedCrash`
/// (a training-mode forced-crash flag) has no Fraymakers concept, so the whole guarded
/// block is obsolete in FM. Leaves a one-line marker in place of the deleted block.
/// (Runs before comment_out_unknown_calls so the block goes away cleanly instead of
/// being dead-commented.)
pub fn remove_forced_crash_guards(code: &str) -> String {
    if !code.contains("isForcedCrash") {
        return code.to_string();
    }
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let is_guard = (trimmed.starts_with("if (") || trimmed.starts_with("if(")
            || trimmed.starts_with("} else if"))
            && trimmed.contains("isForcedCrash")
            && trimmed.ends_with('{');
        if is_guard {
            // brace-match to the block's close, then skip the whole block.
            let mut depth = 0i32;
            let mut found_open = false;
            let mut j = i;
            'scan: while j < lines.len() {
                for ch in lines[j].chars() {
                    match ch {
                        '{' => { depth += 1; found_open = true; }
                        '}' => { depth -= 1; if found_open && depth == 0 { break 'scan; } }
                        _ => {}
                    }
                }
                j += 1;
            }
            if found_open && depth == 0 {
                let indent = &lines[i][..lines[i].len() - lines[i].trim_start().len()];
                out.push(format!("{}// [FM] isForcedCrash guard removed (no forced-crash in Fraymakers)", indent));
                i = j + 1;
                continue;
            }
        }
        out.push(lines[i].to_string());
        i += 1;
    }
    let mut joined = out.join("\n");
    if code.ends_with('\n') { joined.push('\n'); }
    joined
}

/// Strip one tab (or 4 spaces) from the start of a line.
fn strip_one_tab(line: &str) -> &str {
    line.strip_prefix('\t')
        .or_else(|| line.strip_prefix("    "))
        .unwrap_or(line)
}

/// Apply text-level SSF2→FM API translations to decompiled Haxe code.
/// This is a post-processing step run on the output of the decompiler.
///
/// Pipeline order:
///   1. `remove_readiness_guards`      — strip `SSF2API.isReady()` guards
///   2. `double_frame_counts`           — 30→60 fps scaling on frame-typed
///                                        fields BEFORE any rename or split,
///                                        so SSF2 field names still match
///   3. `apply_call_splits`             — fan out SSF2 umbrella calls
///                                        (e.g. updateAttackStats) to their
///                                        FM target methods using
///                                        `call_splits` from commands.jsonc
///   4. literal `replacements`          — text find→replace pairs (ordered;
///                                        SSF2 field names like `hitStun:`
///                                        get renamed to FM `hitstop:` here)
///   5. `regex_replacements`            — regex-based renames
///   6. `strip_last_frame_end_animation` — drop redundant terminal calls
///   7. `comment_out_unknown_calls`     — `ssf2_only` markers + log unknowns
pub fn translate_ssf2_to_fm(code: &str) -> String {
    let mut result = remove_readiness_guards(code);

    // 30→60fps frame doubling on SSF2-named frame fields (hitStun, hitLag,
    // refreshRate, etc.). Runs BEFORE call_splits and the rename pass so
    // SSF2 field names still match `frame_params` entries. Previously this
    // ran only in entity_gen; moved here so Script.hx + ext-methods get it
    // too (otherwise frame fields inside updateHitboxStats calls in
    // Script.hx kept SSF2 30fps values).
    result = double_frame_counts(&result);

    // Fan out SSF2 umbrella calls into one or more FM calls per the
    // `call_splits` table. Runs BEFORE the rename pass so the rename
    // pass can finish field-name conversions on the emitted FM calls.
    result = apply_call_splits(&result);

    // Literal SSF2 → Fraymakers find/replace pairs. Order matters.
    for r in &crate::mappings::api_commands().replacements {
        result = result.replace(&r.from, &r.to);
    }

    // Regex replacements — arg-dropping, arg-aware dispatch, etc.
    for re in regex_replacement_cache() {
        let next = re.regex.replace_all(&result, re.replacement.as_str());
        if matches!(&next, std::borrow::Cow::Owned(_)) {
            result = next.into_owned();
        }
    }

    // Translate the SSF2 "infinite timer" sentinel. Runs AFTER the literal
    // replacements above have renamed createTimer → addTimer, so a single
    // pass covers both spellings.
    result = fix_timer_infinite_repeat(result);

    // Context-aware self.attachEffect(...) → match.createVfx(...) rewrite.
    // Needs the per-character effect→primary-animation map (set up by
    // haxe_gen::generate via `with_effect_animations`); a static regex
    // can't pick the right animation name for effects that split into
    // multiple FrameLabel-derived animations.
    result = rewrite_attach_effect_calls(&result);

    // SSF2 playSound("id") → Fraymakers AudioClip.play(...). Must run before
    // comment_out_unknown_calls so the rewritten calls aren't flagged.
    result = rewrite_play_sound_calls(&result);

    // SSF2 self.fireProjectile("name", x, y) → match.createProjectile(...) + position.
    // Must run before comment_out_unknown_calls so the spawn isn't commented out.
    result = rewrite_fire_projectile_calls(&result);

    // SSF2 RECV.addEffectToList(effect, owner) → keep just the effect-spawning first
    // arg (createVfx already spawns it). Balanced-scan so the closing paren and owner
    // arg are removed cleanly instead of orphaned.
    result = rewrite_add_effect_to_list(&result);

    result = strip_last_frame_end_animation(&result);
    // `if (… isForcedCrash …) { … }` blocks are obsolete in FM — delete them
    // (before comment_out_unknown_calls would dead-comment them).
    result = remove_forced_crash_guards(&result);
    // SSF2 hitTestGround(x, y) point test → FM line-segment hitTestStructuresWithLineSegment.
    result = rewrite_hit_test_ground(&result);
    // AS3 self.Number(x) cast → Std.parseFloat(Std.string(x)).
    result = rewrite_number_cast(&result);
    // SSF2Utils.decel(v, amount) → inline Math min/max (FM has no Math.decel).
    result = rewrite_decel_calls(&result);
    // Any SSF2Event.* / SSF2Utils.* still present has no Fraymakers equivalent
    // in the tables above; neutralize it so the line can't reference an
    // undefined symbol and crash the script at runtime.
    result = neutralize_unmapped_ssf2(&result);
    // `.currentLabel` / `.currentFrame` are Flash MovieClip TIMELINE properties with
    // no Fraymakers equivalent. They survive because getMC()→getViewRootContainer()
    // maps to a real FM call, so the line stays live and reads a property the FM
    // container doesn't have (null access at runtime). Neutralize block-aware.
    result = neutralize_flash_timeline_props(&result);
    // Comment out statements the decompiler couldn't fully reconstruct, where the
    // leftover `/* ? */` / `/* class */` marker would make INVALID Haxe (a `.x`
    // receiver after a comment, or `= ;` empty RHS) that the engine rejects —
    // killing the whole script. Commenting keeps the gap visible and the rest of
    // the script alive. See docs/STATUS.md "frame-script translation".
    result = neutralize_decompiler_gaps(&result);
    result = comment_out_unknown_calls(&result);
    result
}

/// Markers a decompiler-unreconstructable expression renders as.
const GAP_MARKERS: &[&str] = &["/* ? */", "/* class */"];

/// True when `line` uses a gap marker in a position that is INVALID Haxe:
///   - as a receiver:        `/* ? */.foo`  →  `.foo` (parse error)
///   - as a whole RHS value: `x = /* ? */;` →  `x = ;` (parse error)
/// `f(/* ? */)` (empty arg) and `return /* ? */;` (→ `return ;`) stay valid, so
/// they are NOT matched.
/// Remove `/* … */` spans and a trailing `//` comment, returning the trimmed
/// remaining code. Used to test what the parser actually sees on a line.
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

fn line_has_invalid_gap(line: &str) -> bool {
    let stripped = strip_comments(line);
    // A line whose real code starts with `.` is a dangling method-chain
    // continuation — the decompiler renders chains inline, so this only happens
    // when the chain's base was an unrecoverable expr on the previous line
    // (`/* ? */` / `/* class */`). `.foo()` at statement start is a parse error.
    if stripped.starts_with('.') {
        return true;
    }
    // `.{` (a block-expression attached to a `.` access) is never valid Haxe —
    // the projectile/inline-block rewrite can emit `getOwner().{ var _p = … }`.
    // The whole statement is unparseable, so comment it out.
    if stripped.contains(".{") {
        return true;
    }
    for m in GAP_MARKERS {
        let mut from = 0;
        while let Some(rel) = line[from..].find(m) {
            let start = from + rel;
            let after = start + m.len();
            // receiver: marker directly followed by `.`
            let next = line[after..].trim_start();
            if next.starts_with('.') {
                return true;
            }
            // empty RHS: marker preceded by `=` (not `==`/`!=`/`<=`…) and the
            // value ends right after (`;`/`)`/`,`/end).
            let before = line[..start].trim_end();
            let assign = before.ends_with('=')
                && !before.ends_with("==") && !before.ends_with("!=")
                && !before.ends_with("<=") && !before.ends_with(">=");
            if assign && (next.is_empty() || next.starts_with([';', ')', ','])) {
                return true;
            }
            from = after;
        }
    }
    false
}

/// Comment out lines whose decompiler gap marker is in an invalid position
/// (`line_has_invalid_gap`), block-aware so an opener's body/brace go with it.
pub fn neutralize_decompiler_gaps(code: &str) -> String {
    // Cheap skip when there's nothing to do: no gap markers, no `.{`
    // block-as-property, and no line that could begin with a dangling `.`.
    if !GAP_MARKERS.iter().any(|m| code.contains(m))
        && !code.contains(".{")
        && !code.lines().any(|l| l.trim_start().starts_with('.'))
    {
        return code.to_string();
    }
    fn net_braces(line: &str) -> i32 {
        let live = line.split("//").next().unwrap_or(line);
        live.matches('{').count() as i32 - live.matches('}').count() as i32
    }
    let lines: Vec<&str> = code.lines().collect();
    let mut dead = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if !trimmed.starts_with("//") && line_has_invalid_gap(lines[i]) {
            dead[i] = true;
            let mut depth = net_braces(lines[i]);
            let mut j = i + 1;
            while depth > 0 && j < lines.len() {
                depth += net_braces(lines[j]);
                dead[j] = true;
                j += 1;
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    let trailing_nl = code.ends_with('\n');
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if dead[idx] && !trimmed.starts_with("//") {
            let indent = &line[..line.len() - trimmed.len()];
            out.push(format!("{}// [gap: decompiler couldn't recover] {}", indent, trimmed));
        } else {
            out.push(line.to_string());
        }
    }
    let mut joined = out.join("\n");
    if trailing_nl { joined.push('\n'); }
    joined
}

/// SSF2 namespaces with no Fraymakers symbol that, if they survive translation,
/// crash the script (`Unknown variable: SSF2Event` / `SSF2Utils`). Anything
/// mapped by the tables above is already renamed by the time this runs.
const UNMAPPED_SSF2_PREFIXES: &[&str] = &["SSF2Event.", "SSF2Utils."];

fn line_has_unmapped_ssf2(line: &str) -> bool {
    UNMAPPED_SSF2_PREFIXES.iter().any(|p| line.contains(p))
}

/// Comment out lines that still reference a bare unmapped `SSF2*.` symbol after
/// the rename passes. Those have no Fraymakers equivalent, so the engine would
/// hit `Unknown variable` and drop the whole script; commenting the line (or the
/// whole block, for inline-closure handlers) keeps the rest of the handler alive.
pub fn neutralize_unmapped_ssf2(code: &str) -> String {
    if !UNMAPPED_SSF2_PREFIXES.iter().any(|p| code.contains(p)) {
        return code.to_string();
    }
    // Net `{` minus `}` on a line, ignoring an inline `//` comment tail (string
    // contents are not scanned — event-listener lines don't carry `{`-in-strings).
    fn net_braces(line: &str) -> i32 {
        let live = line.split("//").next().unwrap_or(line);
        live.matches('{').count() as i32 - live.matches('}').count() as i32
    }

    let lines: Vec<&str> = code.lines().collect();
    let mut dead = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if line_has_unmapped_ssf2(lines[i]) && !trimmed.starts_with("//") {
            dead[i] = true;
            // If the listener line opens a block (e.g. an inline `function() {`
            // handler), comment through the matching close so the orphaned body
            // and brace don't leak as live code.
            let mut depth = net_braces(lines[i]);
            let mut j = i + 1;
            while depth > 0 && j < lines.len() {
                depth += net_braces(lines[j]);
                dead[j] = true;
                j += 1;
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }

    let trailing_nl = code.ends_with('\n');
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if dead[idx] && !trimmed.starts_with("//") {
            let indent = &line[..line.len() - trimmed.len()];
            // Tag the opener line with the symbol name; continuation lines just dead.
            let prefix = UNMAPPED_SSF2_PREFIXES.iter().find(|p| line.contains(**p));
            if let Some(p) = prefix {
                let sym = line.split(*p).nth(1)
                    .map(|r| r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
                    .unwrap_or_default();
                out.push(format!("{}// [SSF2-only: {}] {}", indent, sym, trimmed));
            } else {
                out.push(format!("{}// [SSF2-dead] {}", indent, trimmed));
            }
        } else {
            out.push(line.to_string());
        }
    }
    let mut joined = out.join("\n");
    if trailing_nl { joined.push('\n'); }
    joined
}

/// Flash MovieClip timeline tokens: `currentLabel` / `currentFrame`, whether read
/// as a property (`getViewRootContainer().currentLabel`) or as the bare frame-script
/// playhead identifier (`curFrame.get() == currentFrame`). Neither has a Fraymakers
/// equivalent, so a live use is a runtime null-access / Unknown-variable. Comment the
/// statement (and, if it opens a block like `if (...currentLabel...) {`, the whole
/// block) so the script stays alive. balance_commented_blocks fixes any orphaned close.
const FLASH_TIMELINE_PROPS: &[&str] = &["currentLabel", "currentFrame"];

/// True if `needle` appears in `hay` as a whole identifier (not a substring of a
/// longer name like `getCurrentFrame` or `currentFrameRate`).
fn contains_word(hay: &str, needle: &str) -> bool {
    let b = hay.as_bytes();
    let n = needle.as_bytes();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let s = from + rel;
        let before_ok = s == 0 || !is_id(b[s - 1]);
        let e = s + n.len();
        let after_ok = e >= b.len() || !is_id(b[e]);
        if before_ok && after_ok { return true; }
        from = s + 1;
    }
    false
}

pub fn neutralize_flash_timeline_props(code: &str) -> String {
    if !FLASH_TIMELINE_PROPS.iter().any(|p| contains_word(code, p)) {
        return code.to_string();
    }
    fn net_braces(line: &str) -> i32 {
        let live = line.split("//").next().unwrap_or(line);
        live.matches('{').count() as i32 - live.matches('}').count() as i32
    }
    let lines: Vec<&str> = code.lines().collect();
    let mut dead = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        let hit = FLASH_TIMELINE_PROPS.iter().any(|p| contains_word(lines[i], p));
        if hit && !trimmed.starts_with("//") {
            dead[i] = true;
            let mut depth = net_braces(lines[i]);
            let mut j = i + 1;
            while depth > 0 && j < lines.len() {
                depth += net_braces(lines[j]);
                dead[j] = true;
                j += 1;
            }
            i = j.max(i + 1);
            continue;
        }
        i += 1;
    }
    let trailing_nl = code.ends_with('\n');
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if dead[idx] && !trimmed.starts_with("//") {
            let indent = &line[..line.len() - trimmed.len()];
            let sym = FLASH_TIMELINE_PROPS.iter().find(|p| contains_word(line, p))
                .copied().unwrap_or("currentLabel");
            out.push(format!("{}// [SSF2-only: {}] {}", indent, sym, trimmed));
        } else {
            out.push(line.to_string());
        }
    }
    let mut joined = out.join("\n");
    if trailing_nl { joined.push('\n'); }
    joined
}

/// Placeholder audio content id (a silent .wav emitted per character by
/// haxe_gen) used for SSF2 sounds with no extracted asset and no GlobalSfx
/// match — paired with a visible TODO so the modder can swap in a real sound.
pub const PLACEHOLDER_SOUND_ID: &str = "_ssf2_placeholder";

thread_local! {
    /// Per-character set of extracted audio content ids (the `id` of each
    /// emitted `<name>.wav.meta`). Installed by `AvailableSoundsGuard` at the
    /// top of `haxe_gen::generate`; read by `rewrite_play_sound_calls` to
    /// decide getContent("id") vs. placeholder. Empty outside a guard scope.
    static AVAILABLE_SOUNDS: std::cell::RefCell<std::collections::BTreeSet<String>>
        = const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// RAII guard installing the per-character extracted-sound id set for the
/// duration of a generation pass (mirrors `EffectAnimGuard`).
pub struct AvailableSoundsGuard;

impl AvailableSoundsGuard {
    pub fn install(ids: std::collections::BTreeSet<String>) -> Self {
        AVAILABLE_SOUNDS.with(|c| { *c.borrow_mut() = ids; });
        AvailableSoundsGuard
    }
}

impl Drop for AvailableSoundsGuard {
    fn drop(&mut self) {
        AVAILABLE_SOUNDS.with(|c| { c.borrow_mut().clear(); });
    }
}

/// Build the Fraymakers replacement for a single `playSound` call, given the
/// extracted first string-literal id (the sound name). Resolution order:
///   1. global_sound_map  → `AudioClip.play(GlobalSfx.<CONST>)`
///   2. extracted asset    → `AudioClip.play(self.getResource().getContent("id"))`
///   3. neither            → placeholder asset + visible `/* TODO … */`
fn build_sound_call(id: Option<&str>) -> String {
    let cfg = crate::mappings::api_commands();
    let t = &crate::mappings::script_templates().global.playsound;
    let req = crate::mappings::require_template;
    let id = match id {
        Some(i) if !i.is_empty() => i,
        _ => return req("global.playsound.placeholder_no_static_id", &t.placeholder_no_static_id)
            .replace("{{placeholder_id}}", PLACEHOLDER_SOUND_ID),
    };
    if let Some(c) = cfg.global_sound_map.get(id) {
        return req("global.playsound.global", &t.global).replace("{{global_sfx}}", c);
    }
    let have = AVAILABLE_SOUNDS.with(|s| s.borrow().contains(id));
    if have {
        return req("global.playsound.asset", &t.asset).replace("{{ssf2_id}}", id);
    }
    req("global.playsound.placeholder_unmapped", &t.placeholder_unmapped)
        .replace("{{placeholder_id}}", PLACEHOLDER_SOUND_ID)
        .replace("{{ssf2_id}}", id)
}

/// Decide the array-literal form for one SSF2 sound id (option (i)): keep the
/// raw id when it's a global OR an extracted per-character asset; otherwise
/// substitute the silent placeholder and append a visible TODO naming the
/// original. Returns `(literal_id, optional_todo_comment)`.
fn sanitise_sound_id(id: &str) -> (String, Option<String>) {
    let is_global = crate::mappings::api_commands().global_sound_map.contains_key(id);
    let have_asset = AVAILABLE_SOUNDS.with(|s| s.borrow().contains(id));
    if is_global || have_asset {
        (id.to_string(), None)
    } else {
        let todo = crate::mappings::require_template(
            "character.audio.placeholder_array_entry_todo",
            &crate::mappings::script_templates().character.audio.placeholder_array_entry_todo,
        ).replace("{{ssf2_id}}", id);
        (PLACEHOLDER_SOUND_ID.to_string(), Some(todo))
    }
}

/// `onTeardown` setup that stops + nulls the active voice clip (mirrors the
/// no-overlap guard in `playVoiceSound`). Returned only when the character has
/// a voice helper (`_activeVoiceClip` is declared); spliced into the single
/// template `onTeardown` by `haxe_gen::generate_script`. Empty otherwise, so
/// voice-less characters get no cleanup statements (and no dead hook body).
pub fn voice_teardown_cleanup(has_voice_helper: bool) -> String {
    if has_voice_helper {
        crate::mappings::require_template(
            "character.audio.voice_teardown_cleanup",
            &crate::mappings::script_templates().character.audio.voice_teardown_cleanup,
        ).to_string()
    } else {
        String::new()
    }
}

/// Build an `Array<String>` literal for a sound table, substituting the silent
/// placeholder + a visible TODO (option (i)) for ids that are neither a global
/// nor an extracted asset, so intent stays inspectable at the data layer.
fn sound_array_literal(ids: &[String]) -> String {
    if ids.is_empty() { return "[]".to_string(); }
    let mut parts = Vec::with_capacity(ids.len());
    for id in ids {
        let (lit, todo) = sanitise_sound_id(id);
        match todo {
            Some(c) => parts.push(format!("\"{lit}\" {c}")),
            None    => parts.push(format!("\"{lit}\"")),
        }
    }
    format!("[{}]", parts.join(", "))
}

/// Emit the Script.hx block backing `playAttackSound(N)` / `playVoiceSound(N)`:
/// the `_attackSounds` / `_voiceSounds` arrays (1-based SSF2 tables of raw ids,
/// kept inspectable as documentation of intent), the `_activeVoiceClip`
/// no-overlap tracker, the two helpers, and a shared `_playResolvedSound(id)`
/// resolver that `switch`es on the *string id read from the array*.
///
/// The resolver's `case`s are generated by iterating the FULL `global_sound_map`
/// from commands.jsonc — single source of truth, so a new global mapping
/// propagates here on the next conversion; every other id falls through to a
/// per-character `getContent` fetch. The returned `AudioClip` handle lets
/// `playVoiceSound` stop the previous voice before starting a new one.
///
/// Attack and voice sections are emitted independently: a section appears when
/// the character has that table OR calls that function. The "OR calls" clause
/// keeps a character that calls a trigger without declaring a table (e.g.
/// sandbag) bound to a defined, bounds-guarded no-op rather than an undefined
/// reference; absent both, nothing is emitted (no dead-code helpers).
///
/// Must be called inside an `AvailableSoundsGuard` scope so `sanitise_sound_id`
/// can tell an extracted asset id from one needing the placeholder.
pub fn generate_sound_helpers(
    attack_ids: &[String], voice_ids: &[String],
    calls_attack: bool, calls_voice: bool,
) -> String {
    let emit_attack = !attack_ids.is_empty() || calls_attack;
    let emit_voice  = !voice_ids.is_empty()  || calls_voice;
    if !emit_attack && !emit_voice { return String::new(); }

    let cfg = crate::mappings::api_commands();
    let t = &crate::mappings::script_templates().character.audio;
    let req = crate::mappings::require_template;

    // Resolver cases: one per global_sound_map entry (BTreeMap → deterministic
    // alphabetical order for stable goldens). switch on the id; default fetches
    // the per-character asset by content id.
    let case_tpl = req("character.audio.global_sound_case", &t.global_sound_case);
    let mut cases = String::new();
    for (id, c) in &cfg.global_sound_map {
        cases.push_str(&case_tpl.replace("{{ssf2_id}}", id).replace("{{global_sfx}}", c));
    }

    let mut out = String::new();
    out.push_str(req("character.audio.header_comment", &t.header_comment));

    if emit_attack {
        out.push_str(&req("character.audio.attack_sounds_decl", &t.attack_sounds_decl)
            .replace("{{entries}}", &sound_array_literal(attack_ids)));
    }
    if emit_voice {
        out.push_str(&req("character.audio.voice_sounds_decl", &t.voice_sounds_decl)
            .replace("{{entries}}", &sound_array_literal(voice_ids)));
        out.push_str(req("character.audio.active_voice_clip_decl", &t.active_voice_clip_decl));
    }
    out.push('\n');

    if emit_attack {
        out.push_str(req("character.audio.play_attack_sound_fn", &t.play_attack_sound_fn));
    }
    if emit_voice {
        out.push_str(req("character.audio.play_voice_sound_fn", &t.play_voice_sound_fn));
    }

    // Shared resolver — returns the AudioClip handle so voice playback can be
    // tracked/stopped. playAttackSound discards the return.
    out.push_str(&req("character.audio.play_resolved_sound_resolver", &t.play_resolved_sound_resolver)
        .replace("{{global_cases}}", &cases));

    out
}

/// Rewrite every `[receiver.]playSound( "id" [, …] )` call to its Fraymakers
/// `AudioClip.play(...)` equivalent. Uses balanced-paren scanning (SSF2
/// playSound args can contain nested calls, e.g. position objects), extracts
/// the first string-literal argument, and drops the rest (FM AudioClip.play
/// takes the content + an optional options object we don't synthesise).
/// SSF2 `self.fireProjectile("name", xOff, yOff)` → FM `match.createProjectile(...)`
/// plus a spawn-position offset, mirroring the FrayMakers character-template idiom
/// (`fireNSpecialProjectile`: createProjectile, then setX/setY with flipX for facing).
/// The two args are the spawn OFFSET (x via flipX so it tracks facing, y added raw); some
/// moves derive that offset from velocity, e.g. zelda's `fireProjectile("airneedle",
/// self.getXSpeed(), self.getYSpeed())` spawns the needle ahead in the direction of motion —
/// so each arg can be an expression (a method call), not just a literal. Args are passed
/// through verbatim. Without this the call is left as an `// [SSF2-only: fireProjectile]`
/// comment and the move spawns NO projectile. Name → manifest content id via
/// projectile_content_id.
pub fn rewrite_fire_projectile_calls(code: &str) -> String {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        // Consume the WHOLE dotted/method receiver chain before fireProjectile
        // (`self.`, `self.getOwner().`, …) so the block replaces the entire
        // `<receiver>.fireProjectile(...)` call. Leaving a dangling `receiver.`
        // before the emitted `{ … }` produces invalid `receiver.{ … }`. The
        // block always creates the projectile on `match` with `self` as owner.
        // Each arg may contain ONE level of parens (a method call like self.getXSpeed()).
        regex::Regex::new(
            r#"(?:[A-Za-z_]\w*(?:\([^()]*\))?\.)*fireProjectile\(\s*"([^"]+)"\s*(?:,\s*((?:[^,()]|\([^()]*\))+?)\s*)?(?:,\s*((?:[^,()]|\([^()]*\))+?)\s*)?\)"#,
        )
        .expect("fireProjectile regex")
    });
    re.replace_all(code, |caps: &regex::Captures| {
        let content_id = crate::projectile_gen::projectile_content_id(&caps[1]);
        let off = |i: usize| {
            caps.get(i)
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty())
                .unwrap_or("0")
                .to_string()
        };
        format!(
            "{{ var _proj = match.createProjectile(self.getResource().getContent(\"{}\"), self); \
_proj.setX(self.getX() + self.flipX({})); _proj.setY(self.getY() + ({})); _proj; }}",
            content_id,
            off(2),
            off(3),
        )
    })
    .into_owned()
}

pub fn rewrite_play_sound_calls(code: &str) -> String {
    const NEEDLE: &str = "playSound(";
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0usize;
    while let Some(rel) = code[cursor..].find(NEEDLE) {
        let pos = cursor + rel; // index of 'p' in playSound(
        // Walk back over the dotted receiver (self. / self.self. / parent. …).
        let mut recv_start = pos;
        while recv_start > 0 {
            let c = bytes[recv_start - 1] as char;
            if c.is_alphanumeric() || c == '_' || c == '.' { recv_start -= 1; } else { break; }
        }
        // Guard: make sure this is really `playSound` and not a longer ident
        // ending in playSound (e.g. `myplaySound`). The char before the
        // receiver-run (or the run itself) must not glue another identifier.
        // recv_start..pos is the receiver; the token right before NEEDLE is
        // "playSound" only if recv_start==pos or recv_start ends with '.'.
        let receiver_ok = recv_start == pos || bytes[pos - 1] as char == '.';
        if !receiver_ok {
            out.push_str(&code[cursor..pos + NEEDLE.len()]);
            cursor = pos + NEEDLE.len();
            continue;
        }
        // Find the matching close paren.
        let open = pos + NEEDLE.len() - 1; // index of '('
        let mut depth = 0i32;
        let mut i = open;
        let mut close = None;
        while i < bytes.len() {
            match bytes[i] as char {
                '(' => depth += 1,
                ')' => { depth -= 1; if depth == 0 { close = Some(i); break; } }
                _ => {}
            }
            i += 1;
        }
        let close = match close {
            Some(e) => e,
            None => { out.push_str(&code[cursor..]); cursor = code.len(); break; }
        };
        // Extract first string-literal id from the args.
        let args = &code[open + 1..close];
        let id = args.find('"').and_then(|s| args[s + 1..].find('"').map(|e| &args[s + 1..s + 1 + e]));
        // Emit text up to the receiver, then the replacement.
        out.push_str(&code[cursor..recv_start]);
        out.push_str(&build_sound_call(id));
        cursor = close + 1;
    }
    out.push_str(&code[cursor..]);
    out
}

thread_local! {
    /// Per-character map of `<effect_name>` → first/primary animation name
    /// in its emitted `.entity` file. Set at the top of `haxe_gen::generate`
    /// via `with_effect_animations` and cleared on return. Used only by
    /// `rewrite_attach_effect_calls`. Empty map = no effects discovered
    /// (or we're outside a `with_effect_animations` scope) → all calls
    /// fall through to the `ssf2_only` marker.
    static EFFECT_PRIMARY_ANIMS: std::cell::RefCell<BTreeMap<String, String>>
        = const { std::cell::RefCell::new(BTreeMap::new()) };
}

/// RAII guard that installs an effect→primary-animation map on
/// construction and clears it on drop. Use at the top of a
/// per-character generation pass so any `translate_ssf2_to_fm` call
/// made on this thread (Character.entity frame scripts, Script.hx,
/// per-attack scripts) gets context-aware attachEffect rewriting.
pub struct EffectAnimGuard;

impl EffectAnimGuard {
    pub fn install(map: BTreeMap<String, String>) -> Self {
        EFFECT_PRIMARY_ANIMS.with(|cell| { *cell.borrow_mut() = map; });
        EffectAnimGuard
    }
}

impl Drop for EffectAnimGuard {
    fn drop(&mut self) {
        EFFECT_PRIMARY_ANIMS.with(|cell| { cell.borrow_mut().clear(); });
    }
}

/// Rewrite `self.attachEffect("name")` and `self.attachEffect("name", { props })`
/// to `match.createVfx(new VfxStats({ spriteContent: …, animation: "<primary>", <translated props> }), self)`.
///
/// Animation-name resolution:
///   - If `name` is in the thread_local map (a local effect we extracted),
///     use that effect's primary animation name — picks the right name
///     even when the entity has multiple FrameLabel-derived animations.
///   - If `name` is not in the map, fall back to `"active"`. Most
///     unknown references are FM-global effects (`global_dust_blast`,
///     `global_spark`, `itempickup_effect`, …) whose entities live in
///     the engine's standard library; `"active"` is the project-wide
///     default animation name for VFX entities we emit, so any local
///     fallback also lands on a valid animation. Worst case the
///     animation is wrong → runtime warning; never worse than the
///     original SSF2 call which we couldn't translate at all.
///
/// Per-prop translation:
///   - The 2-arg form parses the `{…}` block with paren-aware comma
///     splitting (so `Random.getFloat(0, 1)` is one value, not two).
///     Each `key: value` is routed through the `attach_effect_props`
///     mapping table in commands.jsonc, which supports direct renames,
///     1→N expansions (e.g. `parentLock` → `relativeWith` +
///     `resizeWith` + `flipWith`), and explicit `todo` notes for keys
///     with no clean FM equivalent (`syncHitStun`, `loop`, `behind`, …).
///   - Unmapped props emit a `// TODO: <key>: <value> — note` line
///     above the call, preserving the original value alongside the
///     reason. The call line itself never carries free-form comments.
pub fn rewrite_attach_effect_calls(code: &str) -> String {
    static RE_LINE_2ARG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_ANY_2ARG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RE_1ARG: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    // Two 2-arg patterns. The first is line-anchored (`(?m)^([ \t]*)…`)
    // and captures the line's indent so the emitted `// TODO:` lines
    // can mirror it. The second is the mid-line fallback (assignments
    // like `self.effect = self.attachEffect(…)` or calls that follow
    // an inline comment); it can't safely inject TODO lines without
    // breaking the surrounding expression, so unmapped props are
    // dropped silently in that case. The two passes run in order, so
    // the line-anchored form is preferred whenever it can match.
    let re_line_2arg = RE_LINE_2ARG.get_or_init(|| {
        regex::Regex::new(r#"(?m)^([ \t]*)\bself\.attachEffect\(\s*"([^"]*)"\s*,\s*\{([^{}]*)\}\s*\)"#).unwrap()
    });
    let re_any_2arg = RE_ANY_2ARG.get_or_init(|| {
        regex::Regex::new(r#"\bself\.attachEffect\(\s*"([^"]*)"\s*,\s*\{([^{}]*)\}\s*\)"#).unwrap()
    });
    let re_1arg = RE_1ARG.get_or_init(|| {
        regex::Regex::new(r#"\bself\.attachEffect\(\s*"([^"]*)"\s*\)"#).unwrap()
    });

    // Build the translated `new VfxStats({…})` literal from the parsed
    // props. Returns the bag body (no surrounding `{}`) plus a list of
    // TODO notes for unmapped props.
    let build_bag = |name: &str, props_block: &str| -> (String, Vec<String>) {
        let mut fm_fields: Vec<(String, String)> = Vec::new();
        let mut todo_lines: Vec<String> = Vec::new();
        for (key, value) in parse_prop_bag(props_block) {
            translate_attach_effect_prop(&key, &value, &mut fm_fields, &mut todo_lines);
        }
        let mut bag = build_vfx_head(name);
        for (k, v) in &fm_fields {
            bag.push_str(&format!(", {}: {}", k, v));
        }
        (bag, todo_lines)
    };

    // Pass 1: line-anchored 2-arg form — emit TODOs above the call.
    let after_line_2arg = re_line_2arg.replace_all(code, |caps: &regex::Captures| {
        let indent = &caps[1];
        let name = &caps[2];
        let props_block = &caps[3];
        let (bag, todo_lines) = build_bag(name, props_block);

        let mut out = String::new();
        for note in &todo_lines {
            out.push_str(indent);
            out.push_str("// TODO: ");
            out.push_str(note);
            out.push('\n');
        }
        out.push_str(indent);
        out.push_str(&format!(
            "match.createVfx(new VfxStats({{ {bag} }}), self)"
        ));
        out
    });

    // Pass 2: mid-line 2-arg fallback — translate inline. Embedded
    // calls can't host above-line TODOs (would break the surrounding
    // expression), so any unmapped props ride along as a trailing
    // `/* TODO: … */` block-comment instead. That keeps the warning
    // visible without altering line topology.
    let after_any_2arg = re_any_2arg.replace_all(&after_line_2arg, |caps: &regex::Captures| {
        let name = &caps[1];
        let props_block = &caps[2];
        let (bag, todos) = build_bag(name, props_block);
        let call = format!("match.createVfx(new VfxStats({{ {bag} }}), self)");
        if todos.is_empty() {
            call
        } else {
            format!("{} /* TODO: {} */", call, todos.join(" | TODO: "))
        }
    });

    // 1-arg form — no props to translate; the head alone is the bag.
    let after_1arg = re_1arg.replace_all(&after_any_2arg, |caps: &regex::Captures| {
        let name = &caps[1];
        let head = build_vfx_head(name);
        format!("match.createVfx(new VfxStats({{ {head} }}), self)")
    });
    after_1arg.into_owned()
}

/// Build the `spriteContent: …, animation: …` head of the VfxStats bag
/// for an effect named `name`. Shape selection:
///   - If `name` matches `global_vfx_map`, emit the Fraymakers global
///     shape:
///       `spriteContent: "global::vfx.vfx", animation: GlobalVfx.<C>`
///     The constant is unquoted because it's an accessor on the
///     `GlobalVfx` class, not a string literal — the engine resolves
///     the underlying animation name at runtime.
///   - Otherwise emit the per-character shape that resolves `name`
///     against the local effect entity we generated:
///       `spriteContent: self.getResource().getContent("name"),
///        animation: "<primary or active fallback>"`
///     where the animation name comes from `EFFECT_PRIMARY_ANIMS` (set
///     up per-character by `haxe_gen::generate`), or `"active"` when
///     the name isn't a known local effect.
pub fn build_vfx_head(name: &str) -> String {
    let cfg = crate::mappings::api_commands();
    if let Some(constant) = cfg.global_vfx_map.get(name) {
        format!(
            "spriteContent: \"global::vfx.vfx\", animation: GlobalVfx.{c}",
            c = constant,
        )
    } else {
        let anim = EFFECT_PRIMARY_ANIMS.with(|cell| {
            cell.borrow().get(name).cloned().unwrap_or_else(|| "active".to_string())
        });
        format!(
            "spriteContent: self.getResource().getContent(\"{n}\"), animation: \"{a}\"",
            n = name, a = anim,
        )
    }
}

/// Split a `{ key1: val1, key2: val2, … }` body (without the braces) into
/// `(key, value)` pairs, respecting parentheses so values like
/// `Random.getFloat(0, 1)` aren't split on the inner comma. Whitespace
/// around keys and values is trimmed.
fn parse_prop_bag(body: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut depth: i32 = 0;
    let mut cur = String::new();
    for ch in body.chars() {
        match ch {
            '(' | '[' => { depth += 1; cur.push(ch); }
            ')' | ']' => { depth -= 1; cur.push(ch); }
            ',' if depth == 0 => {
                if let Some((k, v)) = cur.split_once(':') {
                    let k = k.trim().to_string();
                    let v = v.trim().to_string();
                    if !k.is_empty() && !v.is_empty() {
                        out.push((k, v));
                    }
                }
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    if !cur.trim().is_empty() {
        if let Some((k, v)) = cur.split_once(':') {
            let k = k.trim().to_string();
            let v = v.trim().to_string();
            if !k.is_empty() && !v.is_empty() {
                out.push((k, v));
            }
        }
    }
    out
}

/// Route one SSF2 prop through `attach_effect_props` in commands.jsonc.
/// On a successful mapping, push `(fm_field, value)` pairs into
/// `fm_fields`. On an explicit `todo` or an unknown key, push a
/// `// TODO:` note onto `todo_lines`. Both arrays are caller-owned so
/// the rewrite can emit them in source order.
fn translate_attach_effect_prop(
    key: &str,
    value: &str,
    fm_fields: &mut Vec<(String, String)>,
    todo_lines: &mut Vec<String>,
) {
    use crate::mappings::AttachEffectPropMapping as M;
    let cfg = crate::mappings::api_commands();
    match cfg.attach_effect_props.get(key) {
        Some(M::Simple(fm)) => {
            fm_fields.push((fm.clone(), value.to_string()));
        }
        Some(M::Detailed { target, expand_to, todo }) => {
            if !expand_to.is_empty() {
                for fm in expand_to {
                    fm_fields.push((fm.clone(), value.to_string()));
                }
            } else if let Some(fm) = target {
                fm_fields.push((fm.clone(), value.to_string()));
            }
            if let Some(note) = todo {
                // Even routed entries can carry a porter-facing caveat
                // (matches call_splits's todo semantics).
                todo_lines.push(format!("{}: {} — {}", key, value, note));
            } else if expand_to.is_empty() && target.is_none() {
                // Entry exists but routes nowhere → treat as unmapped.
                todo_lines.push(format!("{}: {} — entry present but no target/expand_to", key, value));
            }
        }
        None => {
            todo_lines.push(format!("{}: {} — no mapping in attach_effect_props", key, value));
        }
    }
}

/// Strip `self.endAnimation()` calls that appear alone on a line inside the
/// last-numbered frame function of each animation group.
///
/// Pattern: a function named `<anim>__frame<N>` where no higher-numbered frame
/// function exists for that animation in the same script. In that case endAnimation
/// is redundant — FM already ends the animation when the final frame completes.
pub fn strip_last_frame_end_animation(code: &str) -> String {
    // Collect all (anim_prefix, max_frame_num) seen across all functions
    let mut max_frames: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    for (prefix, frame_num) in iter_frame_fns(code) {
        let entry = max_frames.entry(prefix.to_string()).or_insert(0);
        if frame_num > *entry {
            *entry = frame_num;
        }
    }

    // Now process line by line: when inside a last-frame function and we see
    // a line that is ONLY `self.endAnimation();`, replace with a comment.
    let lines: Vec<&str> = code.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_last_frame = false;

    for line in &lines {
        let trimmed = line.trim();
        // Check if this line opens a last-frame function
        if let Some((prefix, frame_num)) = parse_frame_fn_header(trimmed) {
            in_last_frame = max_frames.get(&prefix).copied() == Some(frame_num);
        } else if trimmed == "}" {
            // Leaving any function
            in_last_frame = false;
        }

        if in_last_frame && trimmed == "self.endAnimation();" {
            // Strip it — FM animation ends naturally on the last frame
            out.push(format!(
                "{}// [FM] endAnimation removed — redundant on last frame",
                &line[..line.len() - trimmed.len()]
            ));
        } else {
            out.push(line.to_string());
        }
    }

    let mut joined = out.join("\n");
    if code.ends_with('\n') { joined.push('\n'); }
    joined
}

// Simple &str-based frame function pattern matching (no regex dep).

/// Iterate `function <prefix>__frame<N>(…)` headers in a block of code,
/// yielding `(prefix, N)` for each.
fn iter_frame_fns(code: &str) -> impl Iterator<Item = (String, u32)> + '_ {
    code.lines().filter_map(|line| parse_frame_fn_header(line.trim()))
}

/// Parse a single trimmed line. Returns `Some((prefix, N))` if the line
/// is a `function <prefix>__frame<N>(…)` header, `None` otherwise.
fn parse_frame_fn_header(trimmed: &str) -> Option<(String, u32)> {
    let rest = trimmed.strip_prefix("function ")?;
    let paren = rest.find('(')?;
    let name = &rest[..paren];
    let frame_pos = name.rfind("__frame")?;
    let prefix = name[..frame_pos].to_string();
    let frame_num: u32 = name[frame_pos + "__frame".len()..].parse().ok()?;
    Some((prefix, frame_num))
}

/// Fix `setIntangibility` pairs across the full assembled Script.hx.
///
/// SSF2 pattern:
///   `anim__frame1`:  self.setIntangibility(true);
///   `anim__frame20`: self.setIntangibility(false);
///
/// FM equivalent: a single `applyGlobalBodyStatus(BodyStatus.INTANGIBLE, N)` call
/// where N = (false_frame - true_frame). The false call is removed entirely.
///
/// This is a full-script pass (not per-frame-body) because we need to look
/// across multiple frame functions to compute the duration.
pub fn fix_intangibility_pairs(full_script: &str) -> String {
    // 1. Collect all (anim_prefix, frame_num, line_index) for setIntangibility calls
    let lines: Vec<&str> = full_script.lines().collect();

    // Track current function context while scanning
    let mut current_prefix: Option<String> = None;
    let mut current_frame: u32 = 0;

    // (prefix, frame, line_idx, is_true)
    let mut calls: Vec<(String, u32, usize, bool)> = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if let Some((pfx, fnum)) = parse_frame_fn_header(trimmed) {
            current_prefix = Some(pfx);
            current_frame = fnum;
        }
        // (We don't clear `current_prefix` on `}` — frame functions can
        // contain inner braces, so a naive `if trimmed == "}"` would close
        // the wrong scope.)
        if let Some(ref pfx) = current_prefix {
            if trimmed == "self.setIntangibility(true);" {
                calls.push((pfx.clone(), current_frame, idx, true));
            } else if trimmed == "self.setIntangibility(false);" {
                calls.push((pfx.clone(), current_frame, idx, false));
            }
        }
    }

    // 2. Pair each true with its nearest following false in the same prefix.
    // Sort calls by (prefix, frame_num) so numeric order is used, not textual.
    calls.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut replacements: Vec<(usize, String)> = Vec::new(); // (line_idx, new_line)
    let mut removed: std::collections::HashSet<usize> = std::collections::HashSet::new();

    for (i, (pfx, true_frame, true_line, is_true)) in calls.iter().enumerate() {
        if !is_true { continue; }
        // Find the next false call in the same animation (by frame number, already sorted)
        if let Some((_, false_frame, false_line, _)) = calls[i+1..].iter()
            .find(|(p, _, _, is_t)| p == pfx && !is_t)
        {
            let duration = false_frame.saturating_sub(*true_frame);
            let indent = &lines[*true_line][..lines[*true_line].len() - lines[*true_line].trim_start().len()];
            replacements.push((
                *true_line,
                format!("{}self.applyGlobalBodyStatus(BodyStatus.INTANGIBLE, {});", indent, duration),
            ));
            removed.insert(*false_line);
        } else {
            // No matching false — use 0 as duration and leave a TODO
            let indent = &lines[*true_line][..lines[*true_line].len() - lines[*true_line].trim_start().len()];
            replacements.push((
                *true_line,
                format!("{}self.applyGlobalBodyStatus(BodyStatus.INTANGIBLE, 0 /*TODO: calculate duration*/); //[FM] no matching setIntangibility(false) found", indent),
            ));
        }
    }

    // 3. Collect all unmatched false calls (no preceding true in this script)
    let mut unmatched_false: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (_, _, false_line, is_true) in &calls {
        if !is_true && !removed.contains(false_line) {
            // This false was never consumed by a true
            unmatched_false.insert(*false_line);
        }
    }

    // 4. Rebuild, applying replacements and removing/commenting paired false calls
    let replace_map: std::collections::HashMap<usize, String> =
        replacements.into_iter().collect();

    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for (idx, line) in lines.iter().enumerate() {
        if removed.contains(&idx) {
            // Paired false — duration already encoded in the true
            out.push(format!(
                "{}// [FM] setIntangibility(false) removed — duration encoded above",
                &line[..line.len() - line.trim_start().len()]
            ));
        } else if unmatched_false.contains(&idx) {
            // Unpaired false — intangibility set outside this script (e.g. by state entry)
            let indent = &line[..line.len() - line.trim_start().len()];
            out.push(format!(
                "{}// [SSF2-only: setIntangibility] {} //TODO: intangibility set outside this script",
                indent, line.trim()
            ));
        } else if let Some(replacement) = replace_map.get(&idx) {
            out.push(replacement.clone());
        } else {
            out.push(line.to_string());
        }
    }

    let mut joined = out.join("\n");
    if full_script.ends_with('\n') { joined.push('\n'); }
    joined
}

/// Comment out entire statements containing SSF2-only method calls that have
/// no Fraymakers equivalent. The list of names comes from
/// `commands.jsonc :: ssf2_only` (see crate::mappings).
///
/// Each matched line is replaced with `// [SSF2-only: NAME] <original>` so it
/// won't compile but stays readable. Lines that already start with `//` are
/// left alone (idempotent).
///
/// Side-effect: when log level is at least `debug`, any `.NAME(` call whose
/// name doesn't appear in ANY section of commands.jsonc (replacements,
/// passthrough_fm_apis, ssf2_only, or frame_params) is logged once globally.
/// This is what makes `passthrough_fm_apis` functional — listing a name
/// there suppresses it from this "unknown call" stream. Run with
/// `RUST_LOG=debug` to surface new SSF2 calls that need attention.
/// Split SSF2 umbrella calls (currently `updateAttackStats`) into one or
/// more grouped FM calls per the `call_splits` table in commands.jsonc.
///
/// Source pattern: `<receiver>.<method>({ k1: v1, k2: v2, … })`. The
/// receiver may be any chain (`self`, `match.getCamera()`,
/// `self.lightningBolts[…]`) — it's preserved verbatim on every emitted
/// call. Calls whose argument isn't an object literal are left alone.
///
/// Per-field semantics (drawn from CallSplit.fields):
///   - mapping `"updateAnimationStats.leaveGroundCancel"`  →
///     groups under target `updateAnimationStats` with FM field
///     `leaveGroundCancel: <value>`
///   - source fields absent from the mapping become a `// TODO: …` line
///   - all fields sharing a target method merge into ONE call with
///     `, `-joined pairs, no trailing comma
///   - source line indentation propagates to every emitted line
pub fn apply_call_splits(code: &str) -> String {
    let cfg = crate::mappings::api_commands();
    if cfg.call_splits.is_empty() { return code.to_string(); }
    let mut current = code.to_string();
    for (source_method, split) in &cfg.call_splits {
        current = apply_one_call_split(&current, source_method, split);
    }
    current
}

fn apply_one_call_split(code: &str, method: &str, split: &crate::mappings::CallSplit) -> String {
    let needle = format!(".{}(", method);
    let bytes = code.as_bytes();
    let mut result = String::with_capacity(code.len());
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        let Some(rel) = code[cursor..].find(&needle) else {
            result.push_str(&code[cursor..]);
            break;
        };
        let dot_pos = cursor + rel;
        // Don't replace inside a single-line comment — if the line up to
        // dot_pos contains a `//` (outside of a string), skip this match.
        if line_is_commented_at(code, dot_pos) {
            result.push_str(&code[cursor..=dot_pos]);
            cursor = dot_pos + 1;
            continue;
        }
        let paren_open = dot_pos + needle.len() - 1;
        let receiver_start = find_receiver_start(code, dot_pos);
        let receiver = &code[receiver_start..dot_pos];

        let Some(paren_close) = find_matching_close(code, paren_open) else {
            // Unclosed call — give up on this match, advance past `.`.
            result.push_str(&code[cursor..=dot_pos]);
            cursor = dot_pos + 1;
            continue;
        };

        let args = code[paren_open + 1..paren_close].trim();
        if !args.starts_with('{') || !args.ends_with('}') {
            // Not an object-literal argument — leave the call alone.
            result.push_str(&code[cursor..=paren_close]);
            cursor = paren_close + 1;
            continue;
        }
        let body = &args[1..args.len() - 1];
        let fields = parse_object_fields(body);

        let line_start = code[..receiver_start].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let indent: String = code[line_start..receiver_start].chars()
            .take_while(|c| c.is_whitespace() && *c != '\n').collect();

        // Consume trailing `;` and any spaces between `)` and `;`.
        let mut full_end = paren_close + 1;
        while full_end < bytes.len() && bytes[full_end] == b' ' { full_end += 1; }
        if full_end < bytes.len() && bytes[full_end] == b';' { full_end += 1; }

        let rendered = render_call_split(receiver, &fields, split, &indent);
        // When the render is empty (every field was skipped) AND the only
        // text between cursor and the call is leading whitespace on this
        // line, also consume the trailing newline so the line vanishes
        // cleanly instead of leaving a blank-with-indent.
        if rendered.is_empty()
            && code[..receiver_start].rfind('\n').map(|i| i + 1).unwrap_or(0) >= cursor
            && code[cursor..receiver_start].chars().all(|c| c.is_whitespace() && c != '\n')
        {
            // Skip up to and including the next '\n' after the call.
            let mut skip_end = full_end;
            while skip_end < bytes.len() && bytes[skip_end] != b'\n' {
                if !(bytes[skip_end] as char).is_whitespace() { break; }
                skip_end += 1;
            }
            if skip_end < bytes.len() && bytes[skip_end] == b'\n' {
                skip_end += 1;
                cursor = skip_end;
                continue;
            }
        }
        result.push_str(&code[cursor..receiver_start]);
        result.push_str(&rendered);
        cursor = full_end;
    }
    result
}

/// Per-field decision when resolving a CallSplit field against a source.
enum FieldOutcome {
    /// Route into the named target method's grouped call. `todo_note`,
    /// if present, also emits an inline `// TODO:` comment for the user
    /// to verify the value.
    Route { target_method: String, fm_field: String, value: String, todo_note: Option<String> },
    /// Emit only as a TODO comment line — no routed call.
    Todo { src_name: String, src_value: String, note: Option<String> },
    /// Drop the field entirely (no emission, no TODO) — used by
    /// `skip_if_value` matches.
    Skip,
}

fn render_call_split(
    receiver: &str,
    source_fields: &[(String, String)],
    split: &crate::mappings::CallSplit,
    indent: &str,
) -> String {
    use crate::mappings::CallSplitFieldMapping as M;
    use std::collections::BTreeMap;

    let parse_target = |s: &str| -> Option<(String, String)> {
        s.rfind('.').map(|d| (s[..d].to_string(), s[d + 1..].to_string()))
    };

    let mut outcomes: Vec<FieldOutcome> = Vec::with_capacity(source_fields.len());
    for (field_name, value) in source_fields {
        let trimmed = value.trim();
        let outcome = match split.fields.get(field_name) {
            None => FieldOutcome::Todo {
                src_name: field_name.clone(),
                src_value: value.clone(),
                note: None,
            },
            Some(M::Simple(s)) => match parse_target(s) {
                Some((tm, ff)) => FieldOutcome::Route {
                    target_method: tm, fm_field: ff, value: value.clone(), todo_note: None,
                },
                None => FieldOutcome::Todo {
                    src_name: field_name.clone(),
                    src_value: value.clone(),
                    note: Some("malformed mapping in commands.jsonc".to_string()),
                },
            },
            Some(M::Detailed { target, value_map, skip_if_value, todo }) => {
                if let Some(sv) = skip_if_value {
                    if trimmed == sv.trim() {
                        FieldOutcome::Skip
                    } else {
                        resolve_detailed(field_name, value, trimmed, target, value_map, todo, &parse_target)
                    }
                } else {
                    resolve_detailed(field_name, value, trimmed, target, value_map, todo, &parse_target)
                }
            }
        };
        outcomes.push(outcome);
    }

    // Group Route outcomes by target method (preserve first-seen order),
    // collect TODO lines (from both Todo outcomes and Route todo_notes).
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut todo_lines: Vec<String> = Vec::new();

    for o in outcomes {
        match o {
            FieldOutcome::Route { target_method, fm_field, value, todo_note } => {
                if !groups.contains_key(&target_method) {
                    order.push(target_method.clone());
                }
                if let Some(n) = &todo_note {
                    todo_lines.push(format!("// TODO: {}: {}", fm_field, n));
                }
                groups.entry(target_method).or_default().push((fm_field, value));
            }
            FieldOutcome::Todo { src_name, src_value, note } => {
                let n = note.as_deref().unwrap_or("no FM mapping in call_splits");
                todo_lines.push(format!("// TODO: {}: {} — {}", src_name, src_value, n));
            }
            FieldOutcome::Skip => {}
        }
    }

    let mut lines: Vec<String> = Vec::new();
    lines.extend(todo_lines);
    for target_method in &order {
        let pairs = groups.get(target_method).unwrap();
        let joined: Vec<String> = pairs.iter().map(|(f, v)| format!("{}: {}", f, v)).collect();
        lines.push(format!("{}.{}({{ {} }});", receiver, target_method, joined.join(", ")));
    }

    if lines.is_empty() {
        // Everything skipped (or source object was empty). Drop the
        // source line entirely so the output has no stray no-op.
        return String::new();
    }
    lines.join(&format!("\n{}", indent))
}

/// Resolve a Detailed field mapping when `skip_if_value` didn't apply.
/// Handles the value_map lookup + target routing.
fn resolve_detailed(
    field_name: &str,
    raw_value: &str,
    trimmed_value: &str,
    target: &Option<String>,
    value_map: &std::collections::BTreeMap<String, String>,
    todo: &Option<String>,
    parse_target: &dyn Fn(&str) -> Option<(String, String)>,
) -> FieldOutcome {
    if !value_map.is_empty() {
        match value_map.get(trimmed_value) {
            Some(mapped_value) => match target.as_deref().and_then(parse_target) {
                Some((tm, ff)) => FieldOutcome::Route {
                    target_method: tm, fm_field: ff, value: mapped_value.clone(),
                    todo_note: todo.clone(),
                },
                None => FieldOutcome::Todo {
                    src_name: field_name.to_string(),
                    src_value: raw_value.to_string(),
                    note: todo.clone().or_else(|| Some("value_map matched but no target defined".to_string())),
                },
            },
            None => FieldOutcome::Todo {
                src_name: field_name.to_string(),
                src_value: raw_value.to_string(),
                note: todo.clone().or_else(|| Some("value not in value_map".to_string())),
            },
        }
    } else if let Some(t) = target {
        match parse_target(t) {
            Some((tm, ff)) => FieldOutcome::Route {
                target_method: tm, fm_field: ff, value: raw_value.to_string(),
                todo_note: todo.clone(),
            },
            None => FieldOutcome::Todo {
                src_name: field_name.to_string(),
                src_value: raw_value.to_string(),
                note: todo.clone().or_else(|| Some("malformed target".to_string())),
            },
        }
    } else {
        FieldOutcome::Todo {
            src_name: field_name.to_string(),
            src_value: raw_value.to_string(),
            note: todo.clone(),
        }
    }
}

/// Parse the inside of an object literal (everything between `{` and `}`,
/// exclusive) into `(field_name, value_expr)` pairs. Tracks brace/paren/
/// bracket depth + string state so nested objects, function calls in
/// values, and quoted commas don't break field splitting.
fn parse_object_fields(body: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = body.chars().collect();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ',') { i += 1; }
        if i >= chars.len() { break; }
        // Quoted key form: `"foo bar": value`. Accept the same set of
        // characters as inside any string literal — read until the closing
        // quote (honouring backslash escapes), then proceed to the `:`
        // separator as with bare identifier keys.
        let field_name: String = if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            i += 1;
            let mut name = String::new();
            while i < chars.len() && chars[i] != quote {
                if chars[i] == '\\' {
                    i += 1;
                    if i >= chars.len() { break; }
                    name.push(chars[i]);
                    i += 1;
                    continue;
                }
                name.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i] == quote { i += 1; }
            name
        } else {
            let name_start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') { i += 1; }
            chars[name_start..i].iter().collect()
        };
        if field_name.is_empty() { break; }
        while i < chars.len() && chars[i].is_whitespace() { i += 1; }
        if i >= chars.len() || chars[i] != ':' { break; }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() { i += 1; }
        let val_start = i;
        let mut depth = 0i32;
        let mut in_str: Option<char> = None;
        while i < chars.len() {
            let c = chars[i];
            if let Some(q) = in_str {
                if c == '\\' { i += 1; if i < chars.len() { i += 1; } continue; }
                if c == q { in_str = None; }
            } else {
                match c {
                    '"' | '\'' => in_str = Some(c),
                    '{' | '[' | '(' => depth += 1,
                    '}' | ']' | ')' => depth -= 1,
                    ',' if depth == 0 => break,
                    _ => {}
                }
            }
            i += 1;
        }
        let value: String = chars[val_start..i].iter().collect::<String>().trim().to_string();
        if !value.is_empty() {
            out.push((field_name, value));
        }
    }
    out
}

/// Walk backward from a `.<method>(` position to find the start of the
/// receiver expression. Accepts identifier chars, `.`, balanced `[…]`
/// and `(…)` (for method-chain / index expressions).
fn find_receiver_start(code: &str, dot_pos: usize) -> usize {
    let bytes = code.as_bytes();
    let mut i = dot_pos;
    loop {
        if i == 0 { return 0; }
        let prev = bytes[i - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'.' {
            i -= 1;
        } else if prev == b']' {
            match find_matching_open(code, i - 1, b'[', b']') {
                Some(open) => i = open,
                None => return i,
            }
        } else if prev == b')' {
            match find_matching_open(code, i - 1, b'(', b')') {
                Some(open) => i = open,
                None => return i,
            }
        } else {
            return i;
        }
    }
}

/// Walk forward from an opening bracket/paren/brace to find the matching
/// close. Tracks string state so quoted delimiters don't disturb the count.
/// SSF2 `RECV.addEffectToList(effect, owner)` registers an already-built effect on an
/// entity's effect list. In Fraymakers the usual `effect` arg is a `match.createVfx(...)`
/// that already spawns the effect, so the wrapper is redundant. Keep just the first
/// arg; drop the receiver, the owner arg, and the wrapper's closing paren. A plain
/// string strip of `.addEffectToList(` would leave that `)` (and `, owner`) dangling,
/// which is a parse error the engine swallows silently. #13.
fn rewrite_add_effect_to_list(code: &str) -> String {
    // Match the call name with OR without a receiver: `recv.addEffectToList(` and a
    // bare `addEffectToList(` (the latter appears when SSF2's receiver was stripped
    // upstream) both leave a dangling `)` + owner arg if not unwrapped.
    const NEEDLE: &str = "addEffectToList(";
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0;
    while let Some(rel) = code[cursor..].find(NEEDLE) {
        let name_start = cursor + rel;
        let prev = code[..name_start].chars().last();
        // Skip if it's part of a longer identifier (e.g. removeAddEffectToList).
        if prev.map(|c| c.is_alphanumeric() || c == '_').unwrap_or(false) {
            out.push_str(&code[cursor..name_start + NEEDLE.len()]);
            cursor = name_start + NEEDLE.len();
            continue;
        }
        // Skip a function DEFINITION: `function addEffectToList(arg0) {` is a decl,
        // not a call. Rewriting it strips the parens and produces invalid hscript
        // (`function /* addEffectToList */ arg0 {`), which fails to parse and takes
        // the whole Script.hx scope down with it.
        let before = code[..name_start].trim_end();
        let is_fn_def = before.strip_suffix("function")
            .map(|b| b.is_empty() || !b.chars().last().is_some_and(|c| c.is_alphanumeric() || c == '_'))
            .unwrap_or(false);
        if is_fn_def {
            out.push_str(&code[cursor..name_start + NEEDLE.len()]);
            cursor = name_start + NEEDLE.len();
            continue;
        }
        let paren_open = name_start + NEEDLE.len() - 1; // index of '('
        let close = match find_matching_close(code, paren_open) {
            Some(c) => c,
            None => {
                out.push_str(&code[cursor..=name_start]);
                cursor = name_start + 1;
                continue;
            }
        };
        // With a `.`, drop the whole receiver chain; a bare call keeps text up to the name.
        let cut_start = if prev == Some('.') {
            find_receiver_start(code, name_start - 1)
        } else {
            name_start
        };
        let arg1 = first_top_level_arg(&code[paren_open + 1..close]).trim();
        out.push_str(&code[cursor..cut_start]);
        out.push_str("/* addEffectToList */ ");
        out.push_str(arg1);
        cursor = close + 1;
    }
    out.push_str(&code[cursor..]);
    out
}

/// SSF2 `[recv.]hitTestGround(x, y[, opts])` → FM
/// `hitTestStructuresWithLineSegment(new Point(x, y), new Point(x, y), null, null)`.
/// SSF2 tests a single point and returns the platform-or-null; FM tests a line
/// segment and returns an Array, so:
///   - the point becomes a degenerate (x,y)→(x,y) segment (TODO: a short vertical
///     span may be more faithful for ground detection),
///   - the optional SSF2 options object is dropped (FM
///     StructureColliderSearchOptions differs), and
///   - an immediately-following `!= null` / `== null` is rewritten to `.length > 0`
///     / `.length == 0` (an Array is never null, so a raw null-check would be
///     silently always-true). A bare boolean use gets `.length > 0`.
/// Balanced-scan so nested-paren / object args don't break the split.
fn rewrite_hit_test_ground(code: &str) -> String {
    const NEEDLE: &str = "hitTestGround(";
    if !code.contains(NEEDLE) {
        return code.to_string();
    }
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0;
    while let Some(rel) = code[cursor..].find(NEEDLE) {
        let start = cursor + rel;
        let paren_open = start + NEEDLE.len() - 1;
        let close = match find_matching_close(code, paren_open) {
            Some(c) => c,
            None => { out.push_str(&code[cursor..=start]); cursor = start + 1; continue; }
        };
        let args = &code[paren_open + 1..close];
        let a1 = first_top_level_arg(args).trim();
        let rest = args[first_top_level_arg(args).len()..].trim_start_matches([',', ' ']);
        let a2 = first_top_level_arg(rest).trim();
        if a1.is_empty() || a2.is_empty() {
            out.push_str(&code[cursor..=start]); cursor = start + 1; continue;
        }
        // Look at what immediately follows the call for a null comparison.
        let after = code[close + 1..].trim_start();
        let (suffix, consume) = if let Some(r) = after.strip_prefix("!= null") {
            (".length > 0".to_string(), code[close + 1..].len() - r.len())
        } else if let Some(r) = after.strip_prefix("== null") {
            (".length == 0".to_string(), code[close + 1..].len() - r.len())
        } else {
            (".length > 0".to_string(), 0)
        };
        out.push_str(&code[cursor..start]);
        out.push_str(&format!(
            "hitTestStructuresWithLineSegment(new Point({a1}, {a2}), new Point({a1}, {a2}), null, null){suffix} /*TODO: SSF2 point ground-test -> FM line segment; verify segment & options*/"
        ));
        cursor = close + 1 + consume;
    }
    out.push_str(&code[cursor..]);
    out
}

/// AS3 `self.Number(x)` (a global cast the decompiler mis-attached to `self`, so it
/// resolves to null at runtime) → Haxe `Std.parseFloat(Std.string(x))`, which converts
/// strings AND already-numeric values to Float. `Std` is in character-script scope
/// (live-probed). Balanced-scan so nested-paren args don't break the wrap.
fn rewrite_number_cast(code: &str) -> String {
    const NEEDLE: &str = "self.Number(";
    if !code.contains(NEEDLE) {
        return code.to_string();
    }
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0;
    while let Some(rel) = code[cursor..].find(NEEDLE) {
        let start = cursor + rel;
        let paren_open = start + NEEDLE.len() - 1;
        let close = match find_matching_close(code, paren_open) {
            Some(c) => c,
            None => { out.push_str(&code[cursor..=start]); cursor = start + 1; continue; }
        };
        let inner = code[paren_open + 1..close].trim();
        out.push_str(&code[cursor..start]);
        out.push_str(&format!("Std.parseFloat(Std.string({inner}))"));
        cursor = close + 1;
    }
    out.push_str(&code[cursor..]);
    out
}

/// SSF2Utils.decel(value, amount) → inline decay-toward-zero. SSF2's helper
/// reduces |value| by amount, clamped at 0 (never crosses sign). Fraymakers'
/// Math has no `decel`, so emit the equivalent ternary using Math.min/max
/// (both present in FM). Balanced-scan so nested-paren args (e.g.
/// Math.abs(Math.calculateXVelocity(...))) split correctly.
fn rewrite_decel_calls(code: &str) -> String {
    const NEEDLE: &str = "SSF2Utils.decel(";
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0;
    while let Some(rel) = code[cursor..].find(NEEDLE) {
        let start = cursor + rel;
        let paren_open = start + NEEDLE.len() - 1;
        let close = match find_matching_close(code, paren_open) {
            Some(c) => c,
            None => { out.push_str(&code[cursor..=start]); cursor = start + 1; continue; }
        };
        let args = &code[paren_open + 1..close];
        let a = first_top_level_arg(args).trim();
        let b = args[first_top_level_arg(args).len()..].trim_start_matches([',', ' ']).trim();
        if b.is_empty() {
            // not the 2-arg form — leave untouched
            out.push_str(&code[cursor..=start]); cursor = start + 1; continue;
        }
        out.push_str(&code[cursor..start]);
        out.push_str(&format!("(({a}) < 0 ? Math.min(0, ({a}) + ({b})) : Math.max(0, ({a}) - ({b})))"));
        cursor = close + 1;
    }
    out.push_str(&code[cursor..]);
    out
}

/// Substring of a comma-separated arg list up to the first top-level (depth-0,
/// outside string literals) comma, or the whole string if there is none.
fn first_top_level_arg(args: &str) -> &str {
    let b = args.as_bytes();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match in_str {
            Some(q) => {
                if c == b'\\' { i += 2; continue; }
                if c == q { in_str = None; }
            }
            None => match c {
                b'"' | b'\'' => in_str = Some(c),
                b'(' | b'{' | b'[' => depth += 1,
                b')' | b'}' | b']' => depth -= 1,
                b',' if depth == 0 => return &args[..i],
                _ => {}
            },
        }
        i += 1;
    }
    args
}

fn find_matching_close(code: &str, open_pos: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 1i32;
    let mut i = open_pos + 1;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = in_str {
            if c == b'\\' { i += 2; continue; }
            if c == q { in_str = None; }
        } else {
            // Skip comments — a `'`/`(`/`)` inside them is text, not code (e.g. an
            // inserted `/* TODO: …don't… */` would otherwise open a phantom string).
            if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') { i += 1; }
                i += 2;
                continue;
            }
            if c == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                while i < bytes.len() && bytes[i] != b'\n' { i += 1; }
                continue;
            }
            match c {
                b'"' | b'\'' => in_str = Some(c),
                b'(' | b'{' | b'[' => depth += 1,
                b')' | b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 { return Some(i); }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn find_matching_open(code: &str, close_pos: usize, open: u8, close: u8) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 1i32;
    let mut i = close_pos;
    while i > 0 {
        i -= 1;
        if bytes[i] == close { depth += 1; }
        else if bytes[i] == open {
            depth -= 1;
            if depth == 0 { return Some(i); }
        }
    }
    None
}

/// True if `pos` sits after a `//` on the same line (i.e. inside a
/// single-line comment). Used to skip call_splits matches that landed
/// inside comments.
fn line_is_commented_at(code: &str, pos: usize) -> bool {
    let line_start = code[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let line_prefix = &code[line_start..pos];
    let mut in_str: Option<char> = None;
    let chars: Vec<char> = line_prefix.chars().collect();
    let mut j = 0;
    while j < chars.len() {
        let c = chars[j];
        if let Some(q) = in_str {
            if c == '\\' { j += 1; if j < chars.len() { j += 1; } continue; }
            if c == q { in_str = None; }
        } else {
            match c {
                '"' | '\'' => in_str = Some(c),
                '/' if j + 1 < chars.len() && chars[j + 1] == '/' => return true,
                _ => {}
            }
        }
        j += 1;
    }
    false
}

/// Inferred type for an SSF2 instance variable, picked from `ext_var_inits`.
/// Drives which `self.make<Kind>(default)` wrapper is used when rewriting
/// the var declaration, and whether `.inc()`/`.dec()` are emitted for
/// increment/decrement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtVarType { Bool, Int, Object }

/// SSF2 ext-var names that collide with identifiers already in scope in a Fraymakers
/// script and therefore must NOT be wrapped as persistent state. Wrapping one emits e.g.
/// `var self = self.makeObject(null)`, which shadows the real reference for the rest of the
/// file — silently breaking every `self.foo()` / `match.foo()` call (this is what killed
/// sandbag's down-special: its `dashCheck` ran against a null `self`). These names are
/// dropped from the wrapper declarations and left untouched by the persistent-state pass,
/// so references resolve to the genuine engine-provided `self` / `match`.
pub const RESERVED_EXT_VARS: &[&str] = &["self", "match"];

/// Classify each ext_var as Bool / Int / Object based on its initial-value
/// expression in `ext_var_inits`. Vars with no init expression — or whose
/// init isn't a clean literal — fall back to Object (which can hold any
/// value; only the optional `.inc()` / `.dec()` shorthands need Int).
pub fn infer_ext_var_types(
    ext_vars: &[String],
    ext_var_inits: &[(String, String)],
) -> std::collections::BTreeMap<String, ExtVarType> {
    let init_lookup: std::collections::BTreeMap<&String, &String> =
        ext_var_inits.iter().map(|(n, e)| (n, e)).collect();
    let mut out = std::collections::BTreeMap::new();
    for name in ext_vars {
        let kind = match init_lookup.get(name).map(|s| s.trim()) {
            Some("true") | Some("false") => ExtVarType::Bool,
            // Integer literals → Int (`5`, `-3`, `0`). The wrapper exposes
            // `.inc()` / `.dec()` and `.get()` / `.set()` semantics.
            Some(s) if s.parse::<i64>().is_ok() => ExtVarType::Int,
            // Non-integer numeric literals (e.g. `0.5`, `1.5e2`) must NOT
            // classify as Int — `self.makeInt(0)` would round on `.set(0.5)`
            // and `.inc()`/`.dec()` would step by 1 rather than the source-
            // logical step. Fall through to Object, which is the safe FM
            // wrapper for any non-bool / non-int value.
            _ => ExtVarType::Object,
        };
        out.insert(name.clone(), kind);
    }
    out
}

/// Wrap SSF2 instance-variable accesses into Fraymakers' persistent-state
/// wrappers. Pattern (from Fraymakers/character-template Script.hx):
///
///   ```text
///   var counter = self.makeInt(0);   // declaration
///   counter.set(5);                  // assignment
///   counter.get();                   // read
///   counter.inc();                   // ++
///   counter.dec();                   // --
///   ```
///
/// Affects only references to names in `var_types`; other `self.X` calls
/// (FM API methods, fields) are left alone. Safe to run multiple times —
/// the rewrites are idempotent because they no longer contain `self.<name>`.
///
/// Called on Script.hx and on each translated entity frame-script body
/// with the same type map, so cross-file references stay consistent.
pub fn wrap_persistent_state(
    code: &str,
    var_types: &std::collections::BTreeMap<String, ExtVarType>,
) -> String {
    if var_types.is_empty() { return code.to_string(); }
    let mut result = code.to_string();
    for (name, kind) in var_types {
        // Increment / decrement (Int only — Object/Bool don't have .inc/.dec).
        // Regexes for the three patterns we apply are compiled once per
        // (name, kind) pair via persistent_state_regexes() and reused across
        // the per-frame-script and Script.hx call sites in entity_gen.rs.
        let res = persistent_state_regexes(name);
        if *kind == ExtVarType::Int {
            result = res.inc.replace_all(&result, format!("{}.inc()", name)).into_owned();
            result = res.dec.replace_all(&result, format!("{}.dec()", name)).into_owned();
        }
        // Assignment (`self.foo = X` → `foo.set(X)`). Hand-rolled scan
        // (instead of a regex) so multi-line RHS — e.g. an inline closure
        // literal `function () { … };` that the decompiler produces — is
        // captured correctly. We track brace + string state to find the
        // `;` at depth-zero rather than the first `;` we see.
        result = rewrite_persistent_assignment(&result, name);
        // Read (`self.foo` not followed by `=`, `++`, `--`). Field/method
        // chains (`self.foo.bar`) get `foo.get().bar` which is the
        // desired form. The assignment pass above has already rewritten
        // `self.foo = …`, so this pass is read-only — no `self.foo =`
        // remains for it to mis-match.
        result = res.read.replace_all(&result, format!("{}.get()", name)).into_owned();
    }
    // After wrapping, an SSF2 instance var used as a timer/event CALLBACK now reads as
    // `X.get()` — a value (e.g. the `effects` Array), not a function. Invoking it crashes
    // IntervalTimer.process / event dispatch in Fraymakers (SSF2 tolerated it as a no-op).
    // Replace such callbacks with a no-op closure.
    neutralize_getter_callbacks(result)
}

/// Replace `X.get()` (an instance-var read) sitting in a timer/event CALLBACK position
/// — `addTimer(dur, rep, X.get())` / `addEventListener(EVENT, X.get() [,opts])` — with a
/// no-op closure. Such a callback is data, not a function, and crashes when invoked.
fn neutralize_getter_callbacks(code: String) -> String {
    use regex::Regex;
    let no_op = "function(){} /*TODO: SSF2 used a non-function (instance var) as the callback here*/";
    // addTimer: callback is the 3rd (last) arg
    let re_timer = Regex::new(r"(addTimer\(\s*[^,()]+,\s*[^,()]+,\s*)[A-Za-z_][\w.]*\.get\(\)(\s*\))").unwrap();
    let code = re_timer.replace_all(&code, format!("${{1}}{}${{2}}", no_op)).into_owned();
    // addEventListener: callback is the 2nd arg (options may follow)
    let re_evt = Regex::new(r"(addEventListener\(\s*[^,()]+,\s*)[A-Za-z_][\w.]*\.get\(\)(\s*[,)])").unwrap();
    re_evt.replace_all(&code, format!("${{1}}{}${{2}}", no_op)).into_owned()
}

/// Translate SSF2's "repeat forever" timer sentinel to Fraymakers' spelling.
///
/// SSF2's `createTimer`/`addTimer(delay, repeatCount, callback)` follows the
/// AS3 `flash.utils.Timer` convention where a non-positive `repeatCount` (`0`
/// is how SSF2 character scripts spell it) means "repeat indefinitely".
/// Fraymakers reserves `-1` for an infinite timer; a literal `0` there fires
/// zero/one time and then silently stops. A polling timer copied verbatim —
/// e.g. sandbag's down-special `addTimer(2, 0, dashCheck)`, which keeps
/// reading held controls until a direction is pressed and only then removes
/// itself — therefore never polls, so the move applies no momentum and ignores
/// button presses. Rewrite the `0` repeat count to `-1`.
///
/// A `0`-repeat timer is meaningless in Fraymakers (you would not register a
/// timer to fire zero times), so every `0` we see here is an SSF2 infinite
/// sentinel — the rewrite is unconditional and safe. Positive counts and an
/// already-correct `-1` are left untouched.
fn fix_timer_infinite_repeat(code: String) -> String {
    use regex::Regex;
    // addTimer( <delay> , 0 , …  →  addTimer( <delay> , -1 , …
    // `[^,()]+` matches a simple delay arg (number or identifier), mirroring
    // `neutralize_getter_callbacks`; it deliberately skips delays that contain
    // a nested call (rare for a timer delay).
    let re = Regex::new(r"(addTimer\(\s*[^,()]+,\s*)0(\s*,)").unwrap();
    re.replace_all(&code, "${1}-1${2}").into_owned()
}

/// Rewrite references to the character's OWN script functions from `self.<fn>` to bare
/// `<fn>`. In Fraymakers, frame scripts share Script.hx's scope, so a character's own
/// functions are called/referenced WITHOUT `self.` (the template does
/// `addTimer(.., specialDown_checkLoop)`, no `self.`). `self.<fn>` instead resolves to a
/// nonexistent Character member (null) — e.g. as a timer/event callback it becomes a null
/// that crashes IntervalTimer.process / event dispatch. FM API methods (toState,
/// setYVelocity, …) are NOT in `methods`, so they keep their `self.` prefix.
pub fn rewrite_own_method_refs(code: &str, methods: &[String]) -> String {
    use regex::Regex;
    if methods.is_empty() { return code.to_string(); }
    // longest name first so `self.moveUp` isn't partially matched as `self.move`
    let mut names: Vec<&String> = methods.iter().filter(|m| !m.is_empty()).collect();
    names.sort_by_key(|m| std::cmp::Reverse(m.len()));
    let alt = names.iter().map(|m| regex::escape(m)).collect::<Vec<_>>().join("|");
    let re = Regex::new(&format!(r"\bself\.({})\b", alt)).unwrap();
    re.replace_all(code, "$1").into_owned()
}

/// Per-name compiled regex bundle used by `wrap_persistent_state`.
struct PersistentStateRegexes {
    inc:  regex::Regex,
    dec:  regex::Regex,
    read: regex::Regex,
}

/// Compile (or return) the three regexes for one ext_var name. Cached in a
/// process-global `OnceLock<Mutex<BTreeMap<String, ...>>>` so the second
/// (and tenth, and thousandth) wrap_persistent_state call for the same
/// character doesn't recompile them. `wrap_persistent_state` runs once on
/// Script.hx and once per embedded frame-script body during entity_gen
/// (hundreds of times for a typical character) — this caching is a
/// straight perf win, output unchanged.
fn persistent_state_regexes(name: &str) -> std::sync::Arc<PersistentStateRegexes> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<String, std::sync::Arc<PersistentStateRegexes>>>
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::BTreeMap::new()));
    let mut guard = cache.lock().unwrap();
    if let Some(v) = guard.get(name) { return v.clone(); }
    let escaped = regex::escape(name);
    let v = std::sync::Arc::new(PersistentStateRegexes {
        inc:  regex::Regex::new(&format!(r"\bself\.{}\+\+", escaped)).unwrap(),
        dec:  regex::Regex::new(&format!(r"\bself\.{}--",   escaped)).unwrap(),
        read: regex::Regex::new(&format!(r"\bself\.{}\b",   escaped)).unwrap(),
    });
    guard.insert(name.to_string(), v.clone());
    v
}

/// Rewrite every top-level `self.<name> = <rhs>;` to `<name>.set(<rhs>);`.
/// `<rhs>` may span newlines and contain nested `{ … }` / `( … )` /
/// `[ … ]` / strings — the scan tracks bracket depth and string state and
/// only matches the depth-zero `;` as the terminator.
///
/// Matches:
/// - `self.foo = expr;`                    (single-line)
/// - `self.foo = function () { … };`       (multi-line closure)
/// - `self.foo = { a: 1, b: 2 };`          (object literal)
/// - `self.foo  =  Random.getFloat(0, 1);` (whitespace + nested call)
///
/// Skips:
/// - `self.foo == bar`   (the `==` after the `=` is consumed by the next-
///   char check; we require a single `=` followed by something other than `=`)
/// - `self.foo += 1`     (`+=` doesn't match the bare `=` boundary)
/// - `self.foobar = …`   (token boundary check: char before `self.foo` must
///   not be alphanumeric/underscore; char after `foo` must not be either)
fn rewrite_persistent_assignment(code: &str, name: &str) -> String {
    let needle = format!("self.{}", name);
    let bytes = code.as_bytes();
    let nl = needle.len();
    let mut out = String::with_capacity(code.len());
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        // Find the next candidate occurrence of `self.<name>`.
        let rel = match code[cursor..].find(&needle) {
            Some(r) => r,
            None => { out.push_str(&code[cursor..]); break; }
        };
        let start = cursor + rel;

        // Token boundary on the LEFT — char before `self` must not be a word char.
        let lhs_ok = start == 0 || {
            let prev = bytes[start - 1];
            !(prev.is_ascii_alphanumeric() || prev == b'_')
        };
        // Token boundary on the RIGHT — char after the matched name must not
        // be a word char (otherwise `self.foo` is a prefix of `self.foobar`).
        let after_name = start + nl;
        let rhs_ok = after_name >= bytes.len() || {
            let next = bytes[after_name];
            !(next.is_ascii_alphanumeric() || next == b'_')
        };
        if !lhs_ok || !rhs_ok {
            out.push_str(&code[cursor..=start]);
            cursor = start + 1;
            continue;
        }

        // Skip optional whitespace, then require a single `=` not followed
        // by `=`. (Excludes `==`, `+=`, `-=`, etc. — `+=` doesn't reach here
        // because the byte before `=` would be `+`, not whitespace.)
        let mut p = after_name;
        while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') { p += 1; }
        if p >= bytes.len() || bytes[p] != b'=' {
            out.push_str(&code[cursor..=start]);
            cursor = start + 1;
            continue;
        }
        if p + 1 < bytes.len() && bytes[p + 1] == b'=' {
            // `==` — comparison, not assignment. Skip this match.
            out.push_str(&code[cursor..=start]);
            cursor = start + 1;
            continue;
        }
        let eq_pos = p;

        // Find the depth-zero, outside-of-string `;`.
        let mut q = eq_pos + 1;
        let mut depth: i32 = 0;
        let mut in_str: Option<u8> = None;
        let mut end_semi: Option<usize> = None;
        while q < bytes.len() {
            let c = bytes[q];
            if let Some(quote) = in_str {
                if c == b'\\' { q = q.saturating_add(2); continue; }
                if c == quote { in_str = None; }
            } else {
                match c {
                    b'"' | b'\'' => in_str = Some(c),
                    b'{' | b'(' | b'[' => depth += 1,
                    b'}' | b')' | b']' => {
                        if depth == 0 { break; }
                        depth -= 1;
                    }
                    b';' if depth == 0 => { end_semi = Some(q); break; }
                    _ => {}
                }
            }
            q += 1;
        }
        let semi = match end_semi {
            Some(s) => s,
            None => {
                // Unterminated — give up on this site, advance past `self.`.
                out.push_str(&code[cursor..=start]);
                cursor = start + 1;
                continue;
            }
        };

        // Compose: keep up-to start, emit `<name>.set(<rhs>);`.
        out.push_str(&code[cursor..start]);
        // Strip surrounding whitespace from the RHS so `set(  x  )` doesn't appear.
        let rhs = code[eq_pos + 1..semi].trim();
        out.push_str(&format!("{}.set({});", name, rhs));
        cursor = semi + 1;
    }
    out
}

/// Net `{` minus `}` on a single line, ignoring braces inside string literals and
/// after a `//` line comment. Drives block-aware comment-out so a multi-line dead
/// block (condition + body + close) is fully neutralized, not just its first line.
fn net_brace_delta(line: &str) -> i32 {
    let b = line.as_bytes();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match in_str {
            Some(q) => {
                if c == b'\\' { i += 2; continue; }
                if c == q { in_str = None; }
            }
            None => match c {
                b'/' if i + 1 < b.len() && b[i + 1] == b'/' => break,
                b'"' | b'\'' => in_str = Some(c),
                b'{' => depth += 1,
                b'}' => depth -= 1,
                _ => {}
            },
        }
        i += 1;
    }
    depth
}

pub fn comment_out_unknown_calls(code: &str) -> String {
    let cfg = crate::mappings::api_commands();
    // Build the `.NAME(` match strings from the JSON ssf2_only list.
    let markers: Vec<(String, &str)> = cfg.ssf2_only.iter()
        .map(|e| (format!(".{}(", e.name), e.name.as_str()))
        .collect();

    // Names defined as local functions in THIS script are NOT SSF2-only — a call
    // to a real local function is valid and must not be commented out (e.g. mario
    // defines `function jumpToContinue(...)` and also uses it as a COLLIDE_FLOOR
    // listener; commenting its direct call broke the landing/continue transition).
    let local_fns: std::collections::HashSet<String> = {
        let re = regex::Regex::new(r"(?m)^\s*function\s+(\w+)\s*\(").unwrap();
        re.captures_iter(code).map(|c| c[1].to_string()).collect()
    };

    let lines: Vec<&str> = code.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut ssf2_hits: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        if trimmed.starts_with("//") { out.push(line.to_string()); i += 1; continue; }
        let hit = markers.iter().find(|(m, name)| line.contains(m) && !local_fns.contains(*name));
        if let Some((_, name)) = hit {
            let indent = &line[..line.len() - line.trim_start().len()];
            *ssf2_hits.entry((*name).to_string()).or_insert(0) += 1;
            // If this statement OPENS a `{` block (e.g. `if (self.isCPU()) {`),
            // the block body was guarded by a now-unsupported call, so it is dead in
            // FM. Comment the whole block through its matching `}` — leaving the body
            // or the close brace live would produce unconditional code or an orphaned
            // `}` (a parse error the engine swallows silently). #13.
            let mut depth = net_brace_delta(line);
            out.push(format!("{}// [SSF2-only: {}] {}", indent, name, trimmed));
            i += 1;
            while depth > 0 && i < lines.len() {
                let cont = lines[i];
                let cont_indent = &cont[..cont.len() - cont.trim_start().len()];
                let cont_trimmed = cont.trim();
                if cont_trimmed.starts_with("//") {
                    out.push(cont.to_string());
                } else {
                    out.push(format!("{}// [SSF2-dead] {}", cont_indent, cont_trimmed));
                }
                depth += net_brace_delta(cont);
                i += 1;
            }
        } else {
            out.push(line.to_string());
            i += 1;
        }
    }
    let mut joined = out.join("\n");
    if code.ends_with('\n') { joined.push('\n'); }

    // Always accrue counts into the conversion log (the JSON file is written
    // unconditionally at the end of the run). The debug-level log line is
    // separately gated inside log_unknown_calls.
    if !ssf2_hits.is_empty() {
        let mut log = conversion_log().lock().unwrap();
        for (name, n) in ssf2_hits {
            *log.ssf2_only.entry(name).or_insert(0) += n;
        }
    }
    log_unknown_calls(&joined, cfg);
    joined
}

/// Un-comment `// [SSF2-only: NAME] <stmt>` lines where NAME is actually defined
/// as a local `function NAME(...)` in the ASSEMBLED script. comment_out_unknown_calls
/// runs per-method, so it can't see a sibling function definition and wrongly flags a
/// valid call to a real local function (e.g. mario's `jumpToContinue`, also used as a
/// COLLIDE_FLOOR listener). This whole-file post-pass restores those calls.
pub fn uncomment_local_fn_calls(code: &str) -> String {
    let def_re = regex::Regex::new(r"(?m)^\s*function\s+(\w+)\s*\(").unwrap();
    let locals: std::collections::HashSet<String> =
        def_re.captures_iter(code).map(|c| c[1].to_string()).collect();
    if locals.is_empty() { return code.to_string(); }
    let line_re = regex::Regex::new(r"^(\s*)// \[SSF2-only: (\w+)\] (.*)$").unwrap();
    let mut restored = 0usize;
    let mut joined = code.lines().map(|line| {
        if let Some(c) = line_re.captures(line) {
            if locals.contains(&c[2]) {
                restored += 1;
                return format!("{}{}", &c[1], &c[3]);
            }
        }
        line.to_string()
    }).collect::<Vec<_>>().join("\n");
    if code.ends_with('\n') { joined.push('\n'); }
    if restored > 0 {
        log::info!("uncomment_local_fn_calls: restored {} call(s) to locally-defined functions", restored);
    }
    joined
}

/// Drop the `unknown` tally for every name the ASSEMBLED script defines as a local
/// `function NAME(...)`. [`log_unknown_calls`] runs per-method, so an author-defined
/// SSF2 helper (`isDarkPit`, `updateAuraPaws`, `hurtSelf`) counts as an unmapped gap
/// even though its translated body is emitted right there and the call resolves fine.
/// Same reconciliation as [`uncomment_local_fn_calls`], on the bookkeeping instead of
/// the code — the unknown stream is what we prioritize converter work from, so it has
/// to mean "no FM equivalent", not "the converter didn't recognize the name". Records
/// the names; [`snapshot_conversion_log`] applies them, so a frame-script pass that
/// accrues counts AFTER the defining file still gets discounted.
pub fn discount_local_fn_calls(code: &str) {
    let def_re = regex::Regex::new(r"(?m)^\s*function\s+(\w+)\s*\(").unwrap();
    let mut log = conversion_log().lock().unwrap();
    log.local_fns.extend(def_re.captures_iter(code).map(|c| c[1].to_string()));
}

/// Comment out any `}` whose matching `{` lives on a commented-out line. When a
/// translator neutralizes a block-opening statement (`if (self.getMC()...) {` becomes
/// `// [SSF2-only: getMC] if (...) {`), the opening brace is now inside a comment but
/// the matching `}` is still live code, so the script no longer parses and the engine
/// drops the whole handler silently. This whole-file post-pass walks braces in source
/// order on a stack tagged by whether each opener was dead, and comments any live close
/// that matches a dead open. Iterates to a fixpoint to absorb nested cascades. #13.
pub fn balance_commented_blocks(code: &str) -> String {
    fn is_dead(line: &str) -> bool {
        line.trim_start().starts_with("//")
    }
    // Indices of `{` / `}` on a line that are structural. For a dead (commented) line,
    // every brace in the raw text counts (the original block markers live in the
    // comment). For a live line, skip braces inside strings or after an inline `//`.
    fn braces(line: &str, dead: bool) -> Vec<(usize, bool)> {
        let b = line.as_bytes();
        let mut v = Vec::new();
        if dead {
            for (i, &c) in b.iter().enumerate() {
                if c == b'{' { v.push((i, true)); } else if c == b'}' { v.push((i, false)); }
            }
            return v;
        }
        let mut in_str: Option<u8> = None;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            match in_str {
                Some(q) => {
                    if c == b'\\' { i += 2; continue; }
                    if c == q { in_str = None; }
                }
                None => match c {
                    b'/' if i + 1 < b.len() && b[i + 1] == b'/' => break,
                    b'"' | b'\'' => in_str = Some(c),
                    b'{' => v.push((i, true)),
                    b'}' => v.push((i, false)),
                    _ => {}
                },
            }
            i += 1;
        }
        v
    }

    let net = |line: &str| -> i32 {
        braces(line, is_dead(line)).iter().map(|(_, o)| if *o { 1 } else { -1 }).sum()
    };

    let trailing_nl = code.ends_with('\n');
    let mut lines: Vec<String> = code.lines().map(|s| s.to_string()).collect();
    // Local forward scan per commented opener — robust to imbalance elsewhere in the
    // file (a global stack mispairs once any earlier block is uneven). For each dead
    // line that nets a positive `{`, walk forward tracking depth until it returns to
    // the opener's base level; if the line that closes it is live, that `}` is the
    // orphan, so comment it.
    for _ in 0..16 {
        let mut to_comment: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        for idx in 0..lines.len() {
            if !is_dead(&lines[idx]) {
                continue;
            }
            let open_delta = net(&lines[idx]);
            if open_delta <= 0 {
                continue; // not a net block-opener
            }
            let mut depth = open_delta;
            for (j, line) in lines.iter().enumerate().skip(idx + 1) {
                depth += net(line);
                if depth <= 0 {
                    if !is_dead(line) {
                        to_comment.insert(j);
                    }
                    break;
                }
            }
        }
        if to_comment.is_empty() {
            break;
        }
        for j in to_comment {
            let line = &lines[j];
            let indent = &line[..line.len() - line.trim_start().len()];
            lines[j] = format!("{}// [SSF2-dead] {}", indent, line.trim());
        }
    }
    let mut joined = lines.join("\n");
    if trailing_nl {
        joined.push('\n');
    }
    joined
}

/// One regex_replacements entry, pre-compiled from the JSONC config.
struct CompiledRegexReplacement {
    regex: regex::Regex,
    replacement: String,
}

/// Pre-compile every `regex_replacements` entry once per process. Bad patterns
/// log a warning and are silently skipped, so a typo in commands.jsonc can't
/// break the conversion.
fn regex_replacement_cache() -> &'static [CompiledRegexReplacement] {
    static CACHE: std::sync::OnceLock<Vec<CompiledRegexReplacement>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let cfg = crate::mappings::api_commands();
        let mut out = Vec::with_capacity(cfg.regex_replacements.len());
        for r in &cfg.regex_replacements {
            match regex::Regex::new(&r.pattern) {
                Ok(re) => out.push(CompiledRegexReplacement {
                    regex: re,
                    replacement: r.replacement.clone(),
                }),
                Err(e) => log::warn!(
                    "[api_mappings] regex_replacements pattern failed to compile — \
                     pattern={:?} note={:?} error={}",
                    r.pattern, r.note, e
                ),
            }
        }
        out
    })
}

/// Per-character bookkeeping written to `conversion_log.json` at the end of a
/// run. `unknown` are calls that didn't appear in ANY commands.jsonc section
/// (genuine gaps); `ssf2_only` are calls we know lack a Fraymakers equivalent
/// and have intentionally commented out. The "already logged" set keeps each
/// unknown name's debug-log line down to one occurrence per process.
#[derive(Debug, Default, Clone)]
pub struct ConversionLog {
    pub unknown: std::collections::BTreeMap<String, usize>,
    pub ssf2_only: std::collections::BTreeMap<String, usize>,
    pub already_logged: std::collections::BTreeSet<String>,
    /// Helper names this character's own script defines (see [`discount_local_fn_calls`]).
    /// Filtered out of `unknown` at snapshot time, so it doesn't matter whether the
    /// defining file or a later frame-script pass accrued the count.
    pub local_fns: std::collections::BTreeSet<String>,
}

fn conversion_log() -> &'static std::sync::Mutex<ConversionLog> {
    static LOG: std::sync::OnceLock<std::sync::Mutex<ConversionLog>> = std::sync::OnceLock::new();
    LOG.get_or_init(|| std::sync::Mutex::new(ConversionLog::default()))
}

/// Reset the conversion log — call at the start of each character so per-
/// character counts and the once-per-name dedup start from a clean slate.
pub fn reset_conversion_log() {
    *conversion_log().lock().unwrap() = ConversionLog::default();
}

/// Snapshot the current log (does not reset). Used at the end of a run to
/// write `conversion_log.json` next to the exported character. Calls to the
/// character's own helpers are dropped from `unknown` here (see
/// [`discount_local_fn_calls`]) so the stream means "no FM equivalent".
pub fn snapshot_conversion_log() -> ConversionLog {
    let mut snap = conversion_log().lock().unwrap().clone();
    let locals = std::mem::take(&mut snap.local_fns);
    snap.unknown.retain(|name, _| !locals.contains(name));
    snap.local_fns = locals;
    snap
}

/// Walk `code` for `.NAME(` call sites and log any name that isn't covered
/// by any of the five commands.jsonc sections. Locally-defined helper names
/// still land here (this pass runs per-method, so it can't see a sibling
/// definition); [`discount_local_fn_calls`] subtracts them once the whole
/// script is assembled.
fn log_unknown_calls(code: &str, cfg: &crate::mappings::ApiCommands) {
    // Build the union of all known names across every section.
    let mut known: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    // Call-shaped identifiers (`NAME(`) in a mapping's source and target text. A regex
    // pattern escapes its call paren, so `NAME\(` counts as a call site too — without
    // that, every name a regex mapping EMITS (`Common.setAutocancel($1)`) reads back as
    // an unmapped gap.
    let extract = |s: &str, out: &mut std::collections::BTreeSet<String>| {
        let bytes = s.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            // start of identifier?
            if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                { i += 1; }
                let call = bytes.get(i) == Some(&b'(')
                    || (bytes.get(i) == Some(&b'\\') && bytes.get(i + 1) == Some(&b'('));
                if call {
                    out.insert(String::from_utf8_lossy(&bytes[start..i]).into_owned());
                }
            } else {
                i += 1;
            }
        }
    };
    for r in &cfg.replacements {
        extract(&r.from, &mut known);
        extract(&r.to,   &mut known);
    }
    for r in &cfg.regex_replacements {
        extract(&r.pattern,     &mut known);
        extract(&r.replacement, &mut known);
    }
    for p in &cfg.passthrough_fm_apis { known.insert(p.name.clone()); }
    for b in &cfg.haxe_builtins       { known.insert(b.name.clone()); }
    for o in &cfg.ssf2_only           { known.insert(o.name.clone()); }
    for f in &cfg.frame_params        { known.insert(f.name.clone()); }

    let bytes = code.as_bytes();
    let mut i = 0usize;
    let debug = log::log_enabled!(log::Level::Debug);
    let mut log_state = conversion_log().lock().unwrap();
    while i < bytes.len() {
        if bytes[i] == b'.'
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_')
        {
            let start = i + 1;
            let mut j = start;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
            { j += 1; }
            if j < bytes.len() && bytes[j] == b'(' {
                let name = String::from_utf8_lossy(&bytes[start..j]).into_owned();
                if !known.contains(&name) {
                    *log_state.unknown.entry(name.clone()).or_insert(0) += 1;
                    if debug && log_state.already_logged.insert(name.clone()) {
                        log::debug!(
                            "[api_mappings] unknown call '{}' — add to replacements / \
                             passthrough_fm_apis / ssf2_only in commands.jsonc as appropriate",
                            name
                        );
                    }
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
}

// ─── 30fps → 60fps frame-count scaling ───────────────────────────────────────
//
// SSF2 plays at 30fps, Fraymakers at 60fps. Numeric arguments in decompiled
// frame-script Haxe that represent frame counts must be doubled so the script
// runs at the same real time. Only specific, known frame-count fields are
// touched — speeds, positions, IDs, angles, repeat counts and multipliers are
// deliberately left alone.

/// Double the first non-negative integer literal that follows each occurrence
/// of `marker` (after optional spaces/tabs). Values `>= skip_at` are treated as
/// sentinels and left unchanged; non-literal arguments (expressions, negatives)
/// are skipped because no digits immediately follow the marker.
fn double_int_after_marker(code: &str, marker: &str, skip_at: i64, keep_one: bool) -> String {
    let mut out = String::with_capacity(code.len() + 16);
    let bytes = code.as_bytes();
    let mut i = 0usize;
    while i < code.len() {
        if code[i..].starts_with(marker) {
            out.push_str(marker);
            let mut j = i + marker.len();
            while j < code.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
                out.push(bytes[j] as char);
                j += 1;
            }
            let start = j;
            while j < code.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > start {
                match code[start..j].parse::<i64>() {
                    Ok(1) if keep_one => out.push_str(&code[start..j]),
                    Ok(n) if n < skip_at => out.push_str(&(n * 2).to_string()),
                    _ => out.push_str(&code[start..j]),
                }
            }
            i = j;
        } else {
            let ch = code[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Double the integer literal at positional argument `arg_idx` of every
/// `fn_name(...)` call. Arguments before the target are walked over with
/// bracket-depth tracking, so commas inside nested calls/arrays/objects don't
/// miscount. Non-literal and negative arguments are left unchanged; a literal
/// `>= skip_at` is treated as a sentinel and left unchanged.
fn double_call_arg(code: &str, fn_name: &str, arg_idx: usize, skip_at: i64, keep_one: bool) -> String {
    let marker = format!("{}(", fn_name);
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len() + 16);
    let mut cursor = 0usize;
    while let Some(rel) = code[cursor..].find(&marker) {
        let arg0 = cursor + rel + marker.len(); // start of argument 0
        // Walk to the start of argument `arg_idx`, depth-tracking brackets.
        let mut j = arg0;
        let mut depth = 0i32;
        let mut cur = 0usize;
        let mut bailed = false;
        while cur < arg_idx && j < code.len() {
            match bytes[j] {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    if depth == 0 { bailed = true; break; } // call closed early
                    depth -= 1;
                }
                b',' if depth == 0 => cur += 1,
                _ => {}
            }
            j += 1;
        }
        // Copy verbatim up to the target argument's start.
        out.push_str(&code[cursor..j]);
        cursor = j;
        if bailed || cur < arg_idx { continue; } // argument not present
        // Skip leading whitespace, then double the integer literal if present.
        let mut k = j;
        while k < code.len() && (bytes[k] == b' ' || bytes[k] == b'\t') { k += 1; }
        out.push_str(&code[j..k]);
        let start = k;
        while k < code.len() && bytes[k].is_ascii_digit() { k += 1; }
        if k > start {
            match code[start..k].parse::<i64>() {
                Ok(1) if keep_one => out.push_str(&code[start..k]),
                Ok(n) if n < skip_at => out.push_str(&(n * 2).to_string()),
                _ => out.push_str(&code[start..k]),
            }
        }
        cursor = k;
    }
    out.push_str(&code[cursor..]);
    out
}

/// Double every command parameter flagged `isframe` in mappings/commands.json
/// — `createTimer` delays, `stancePlayFrame` indices, and object-literal
/// fields like `hitStun` / `refreshRate` / `chargetime_max`. This is the
/// single, data-driven path for frame-count scaling in decompiled frame-script
/// Haxe; which parameters are doubled is controlled entirely by the JSON.
pub fn double_frame_counts(code: &str) -> String {
    let mut out = code.to_string();
    for p in &crate::mappings::api_commands().frame_params {
        if !p.isframe { continue; }
        let skip_at = p.sentinel.unwrap_or(i64::MAX);
        out = match p.kind.as_str() {
            "field" => double_int_after_marker(&out, &format!("{}:", p.name), skip_at, p.keep_one),
            "call"  => double_call_arg(&out, &p.name, p.arg, skip_at, p.keep_one),
            _ => out, // unknown kind — leave untouched
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fire_projectile_becomes_create_projectile_spawn() {
        // SSF2's fireProjectile must spawn a real FM projectile, not get
        // commented out (which left the neutral special spawning nothing).
        let out = translate_ssf2_to_fm("self.fireProjectile(\"mario_fireball\", 22, -20);\n");
        assert!(out.contains("match.createProjectile"), "no createProjectile: {out}");
        assert!(out.contains("getContent(\"mariofireballProjectile\")"),
            "content id wrong (want mariofireballProjectile): {out}");
        assert!(out.contains("self.flipX(22)"), "x offset/facing not applied: {out}");
        assert!(out.contains("self.getY() + (-20)"), "y offset not applied: {out}");
        assert!(!out.contains("[SSF2-only: fireProjectile]"),
            "spawn was still commented out: {out}");
        // No-offset form defaults to 0,0.
        let bare = translate_ssf2_to_fm("self.fireProjectile(\"mario_fireball\");\n");
        assert!(bare.contains("self.flipX(0)") && bare.contains("self.getY() + (0)"),
            "bare fireProjectile didn't default offsets: {bare}");
        // Expression args (a method call, e.g. zelda's airneedle spawning ahead of motion)
        // must match too — they contain parens, which a naive [^,()] arg pattern would miss.
        let expr = translate_ssf2_to_fm(
            "self.fireProjectile(\"airneedle\", self.getXSpeed(), self.getYSpeed());\n");
        assert!(expr.contains("match.createProjectile"), "expr-arg form not rewritten: {expr}");
        assert!(expr.contains("self.flipX(self.getXSpeed())"),
            "expr x-offset not carried: {expr}");
        assert!(expr.contains("self.getY() + (self.getYSpeed())"),
            "expr y-offset not carried: {expr}");
        assert!(!expr.contains("[SSF2-only: fireProjectile]"),
            "expr-arg fireProjectile left commented: {expr}");
        // A non-`self` receiver chain (zelda: self.getOwner().fireProjectile) must be
        // consumed WHOLE — otherwise the dangling `receiver.` glues onto the emitted
        // block as `receiver.{ … }`, which is invalid Haxe.
        let owner = translate_ssf2_to_fm(
            "proj = self.getOwner().fireProjectile(\"cameraPanProjectile\", 0, 0);\n");
        assert!(owner.contains("match.createProjectile"), "owner-receiver not rewritten: {owner}");
        assert!(!owner.contains(".{"), "dangling receiver produced .{{: {owner}");
        assert!(owner.trim_end().ends_with("_proj; };") || owner.contains("_proj; }"),
            "block doesn't return _proj (wrong value in assignment position): {owner}");
    }

    #[test]
    fn test_global_variable_rewrite() {
        // SSF2 get/setGlobalVariable -> Fraymakers makeX wrapper .get()/.set().
        let out = translate_ssf2_to_fm(concat!(
            "self.setGlobalVariable(\"canStartRise\", true);\n",
            "if (self.getGlobalVariable(\"canStartRise\")) { return; }\n",
            "self.setGlobalVariable(\"standtime\", self.getGlobalVariable(\"standtime\") - 1);\n",
        ));
        assert!(out.contains("canStartRise.set(true)"), "set not rewritten: {out}");
        assert!(out.contains("canStartRise.get()"), "get not rewritten: {out}");
        assert!(out.contains("standtime.set(standtime.get() - 1)"), "nested get/set not rewritten: {out}");
        assert!(!out.contains("GlobalVariable"), "raw GlobalVariable call left in output: {out}");
        assert!(!out.contains("AnimationStatsMetadata"), "old wrong mapping still present: {out}");
    }

    #[test]
    fn test_uncomment_local_fn_calls() {
        // A call commented [SSF2-only: NAME] is restored iff NAME is a local function.
        let code = concat!(
            "function jumpToContinue(arg0) {\n",
            "\tself.removeEventListener(EntityEvent.COLLIDE_FLOOR, jumpToContinue);\n",
            "}\n",
            "function setLandingLag(arg0) {\n",
            "\t\t\t// [SSF2-only: jumpToContinue] jumpToContinue();\n",
            "\t\t\t// [SSF2-only: tossItem] self.tossItem(270);\n",
            "}\n",
        );
        let out = uncomment_local_fn_calls(code);
        // jumpToContinue is defined locally -> restored (indent preserved, marker gone)
        assert!(out.contains("\t\t\tjumpToContinue();"), "local-fn call not restored: {out}");
        assert!(!out.contains("[SSF2-only: jumpToContinue]"), "marker not removed");
        // tossItem is NOT defined locally -> stays commented
        assert!(out.contains("[SSF2-only: tossItem] self.tossItem(270)"), "non-local wrongly restored");
    }

    #[test]
    fn rewrite_add_effect_to_list_unwraps_balanced() {
        let code = "self.addEffectToList(match.createVfx(new VfxStats({ x: 1 }), self), self);";
        let out = rewrite_add_effect_to_list(code);
        assert_eq!(
            out,
            "/* addEffectToList */ match.createVfx(new VfxStats({ x: 1 }), self);"
        );
        // balanced parens after the rewrite
        assert_eq!(out.matches('(').count(), out.matches(')').count());
    }

    #[test]
    fn number_cast_rewritten_balanced() {
        let out = rewrite_number_cast("cardNumber.set(self.Number(child.get().name));");
        assert!(out.contains("Std.parseFloat(Std.string(child.get().name))"), "wrong: {out}");
        assert!(!out.contains("self.Number("), "old cast left: {out}");
        assert_eq!(out.matches('(').count(), out.matches(')').count(), "unbalanced: {out}");
    }

    #[test]
    fn rewrite_add_effect_to_list_bare_call_unwrapped() {
        // No receiver (the SSF2 receiver was stripped upstream) — must still unwrap so
        // the wrapper's `)` + owner arg don't dangle (marth/pichu finalsmash).
        let code = "\taddEffectToList(match.createVfx(new VfxStats({ x: 1 }), self), self);";
        let out = rewrite_add_effect_to_list(code);
        assert!(out.contains("match.createVfx(new VfxStats({ x: 1 }), self)"), "effect lost: {out}");
        assert!(!out.contains("addEffectToList("), "wrapper not removed: {out}");
        assert_eq!(out.matches('(').count(), out.matches(')').count(), "unbalanced: {out}");
    }

    #[test]
    fn rewrite_add_effect_to_list_leaves_fn_definition_intact() {
        // `function addEffectToList(arg0) {` is the SSF2 ext-method DEFINITION, not a
        // call. It must pass through untouched — mangling it to
        // `function /* addEffectToList */ arg0 {` is invalid hscript and crashes the
        // whole Script.hx scope (zelda:stand rand/repeats Unknown-variable cascade).
        let code = "function addEffectToList(arg0) {\n\teffects.get().push(arg0);\n}";
        let out = rewrite_add_effect_to_list(code);
        assert!(out.contains("function addEffectToList(arg0) {"), "definition mangled: {out}");
        assert_eq!(out.matches('(').count(), out.matches(')').count(), "unbalanced: {out}");
    }

    #[test]
    fn neutralize_flash_timeline_props_comments_block() {
        // getMC()→getViewRootContainer() leaves a live `.currentLabel` read on a
        // container that has no such property. The whole `if (...) {...}` must be
        // commented block-aware so it can't null-access at runtime.
        let code = "if (self.getViewRootContainer().currentLabel != \"b\") {\n\tEngine.log(\"x\");\n}\nself.toState(CState.STAND);";
        let out = neutralize_flash_timeline_props(code);
        assert!(!out.lines().any(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && t.contains(".currentLabel")
        }), "live currentLabel survived: {out}");
        // The trailing live statement is untouched.
        assert!(out.contains("self.toState(CState.STAND);"), "ate live code: {out}");
        // Block body got swept too (no orphaned Engine.log left live).
        assert!(out.lines().filter(|l| !l.trim_start().starts_with("//")).all(|l| !l.contains("Engine.log")),
            "block body leaked: {out}");
    }

    #[test]
    fn neutralize_flash_timeline_props_bare_identifier_and_word_boundary() {
        // Bare `currentFrame` playhead must be neutralized…
        let bare = "if (curFrame.get() == currentFrame) {\n\tx();\n}";
        let out = neutralize_flash_timeline_props(bare);
        assert!(out.lines().all(|l| l.trim_start().starts_with("//") || !contains_word(l, "currentFrame")),
            "bare currentFrame survived: {out}");
        // …but a longer identifier that merely contains it must NOT be touched.
        let safe = "var n = self.getCurrentFrameRate();";
        assert_eq!(neutralize_flash_timeline_props(safe), safe, "word-boundary false positive");
    }

    #[test]
    fn balance_commented_blocks_fixes_orphaned_close() {
        let code = concat!(
            "function destroy(arg0) {\n",
            "\t// [SSF2-only: getMC] if (self.getMC().currentLabel != \"x\") {\n",
            "\t\t// [SSF2-only: getMC] self.getMC().currentLabel == \"x\";\n",
            "\t}\n",
            "\t// [SSF2-only: getStanceMC] if (self.getStanceMC() != \"a\") {\n",
            "\t\tself.playLabel(\"attack\");\n",
            "\t}\n",
            "}\n",
        );
        let out = balance_commented_blocks(code);
        // the two `}` whose openers are commented must now be commented too; the
        // function's own braces stay live and the whole thing balances.
        let live: i32 = out.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .map(net_brace_delta)
            .sum();
        assert_eq!(live, 0, "live braces unbalanced after fix:\n{out}");
        // only the function body's own close should remain live.
        let live_closes = out.lines()
            .filter(|l| !l.trim_start().starts_with("//") && l.trim() == "}")
            .count();
        assert_eq!(live_closes, 1, "expected 1 live `}}` (function end), got {live_closes}:\n{out}");
    }

    #[test]
    fn test_basic_translations() {
        let input = concat!(
            "self.self.endAttack();\n",
            "SSF2API.print(\"hello\");\n",
            "self.self.updateAttackBoxStats(1, { damage: 5, direction: 45, power: 10 });\n",
            "self.self.refreshAttackID();",
        );

        let output = translate_ssf2_to_fm(input);
        assert!(output.contains("self.endAnimation()"), "endAttack not translated");
        assert!(output.contains("Engine.log(\"hello\")"), "print not translated");
        assert!(output.contains("self.updateHitboxStats"), "updateAttackBoxStats not translated");
        assert!(output.contains("angle: 45"), "direction: not renamed to angle:");
        assert!(output.contains("baseKnockback: 10"), "power: not renamed");
        assert!(output.contains("self.reactivateHitboxes()"), "refreshAttackID not translated");
    }

    #[test]
    fn test_timer_infinite_repeat_translated() {
        // SSF2's 0 = "repeat forever" must become Fraymakers' -1.
        let out = translate_ssf2_to_fm("self.addTimer(2, 0, dashCheck);");
        assert!(out.contains("addTimer(2, -1, dashCheck)"), "0 repeat not rewritten to -1: {out}");
        // SSF2 createTimer is renamed to addTimer and its delay is 30→60fps
        // doubled (52 → 104), then the repeat sentinel is fixed.
        let out2 = translate_ssf2_to_fm("self.createTimer(52, 0, loopSound2);");
        assert!(out2.contains("addTimer(104, -1, loopSound2)"), "createTimer 0 not rewritten: {out2}");
        // Positive counts and an existing -1 are untouched; a 0 delay stays.
        let out3 = translate_ssf2_to_fm("self.addTimer(0, 1, enable);\nself.addTimer(2, -1, poll);");
        assert!(out3.contains("addTimer(0, 1, enable)"), "positive count or 0 delay altered: {out3}");
        assert!(out3.contains("addTimer(2, -1, poll)"), "existing -1 altered: {out3}");
    }

    #[test]
    fn test_timer_delay_one_kept_per_frame() {
        // A per-frame SSF2 timer (createTimer delay 1) must stay at 1 — NOT
        // double to 2 — so it keeps polling every engine frame.
        let out = translate_ssf2_to_fm("self.createTimer(1, 0, dashCheck);");
        assert!(out.contains("addTimer(1, -1, dashCheck)"), "delay 1 should stay 1: {out}");
        // Larger delays still scale 30→60fps.
        let out2 = translate_ssf2_to_fm("self.createTimer(3, 0, poll);");
        assert!(out2.contains("addTimer(6, -1, poll)"), "delay 3 should double to 6: {out2}");
    }

    #[test]
    fn test_isready_guard_removed() {
        let input = concat!(
            "function a__frame0() {\n",
            "\tif (SSF2API.isReady()) {\n",
            "\t\tself.looped = false;\n",
            "\t\tself.playsound = SSF2API.random();\n",
            "\t}\n",
            "\treturn;\n",
            "}",
        );
        let output = translate_ssf2_to_fm(input);
        // The if-block wrapper should be gone
        assert!(!output.contains("SSF2API.isReady()"), "guard not removed");
        assert!(!output.contains("if ("), "if block should be stripped");
        // Body should be inlined (one tab dedented)
        assert!(output.contains("self.looped = false;"), "body not inlined");
        assert!(output.contains("Random.getFloat"), "random not translated");
    }

    #[test]
    fn test_self_and_isready_guard_removed() {
        let input = concat!(
            "\tif (self && SSF2API.isReady()) {\n",
            "\t\tself.x = 10;\n",
            "\t}",
        );
        let output = translate_ssf2_to_fm(input);
        assert!(!output.contains("SSF2API.isReady()"), "guard not removed");
        assert!(output.contains("self.x = 10;"), "body not inlined");
    }

    #[test]
    fn test_nested_isready_guard() {
        // Guard with nested braces inside
        let input = concat!(
            "\tif (SSF2API.isReady()) {\n",
            "\t\tif (x > 0) {\n",
            "\t\t\tself.y = 1;\n",
            "\t\t}\n",
            "\t}",
        );
        let output = translate_ssf2_to_fm(input);
        assert!(!output.contains("SSF2API.isReady()"), "outer guard not removed");
        // inner if should survive
        assert!(output.contains("if (x > 0)"), "inner if should survive");
        assert!(output.contains("self.y = 1;"), "body not inlined");
    }

    #[test]
    fn mapped_events_are_renamed() {
        // High-frequency events with a confirmed FM equivalent.
        let out = translate_ssf2_to_fm("self.addEventListener(SSF2Event.GROUND_LEAVE, h);");
        assert!(out.contains("GameObjectEvent.LEFT_GROUND"), "GROUND_LEAVE not mapped: {out}");
        assert!(!out.contains("SSF2Event."), "raw SSF2Event leaked: {out}");
        // ATTACK_HIT must not corrupt the longer ATTACK_HIT_SHIELD prefix.
        let shield = translate_ssf2_to_fm("self.addEventListener(SSF2Event.ATTACK_HIT_SHIELD, h);");
        assert!(shield.contains("GameObjectEvent.SHIELD_HIT_DEALT"), "ATTACK_HIT_SHIELD mismapped: {shield}");
        assert!(!shield.contains("HIT_DEALT_SHIELD"), "prefix corruption: {shield}");
    }

    #[test]
    fn unmapped_event_is_neutralized_not_leaked() {
        // An event with no FM equivalent must be commented, never left raw.
        let out = translate_ssf2_to_fm("self.addEventListener(SSF2Event.CHAR_ATTACK_COMPLETE, h);");
        assert!(!out.lines().any(|l| !l.trim_start().starts_with("//") && l.contains("SSF2Event.")),
            "raw SSF2Event leaked live: {out}");
        assert!(out.contains("[SSF2-only: CHAR_ATTACK_COMPLETE]"), "no marker: {out}");
    }

    #[test]
    fn unmapped_event_inline_closure_block_is_fully_commented() {
        // Inline-closure handler: the whole block must be commented so the body
        // and closing brace don't leak as orphaned live code.
        let input = "self.addEventListener(SSF2Event.CHAR_ATTACK_COMPLETE, function() {\n\tself.x = 1;\n});";
        let out = neutralize_unmapped_ssf2(input);
        for l in out.lines() {
            assert!(l.trim_start().starts_with("//"), "line left live: {l:?}");
        }
    }

    #[test]
    fn ssf2utils_decel_inlined_with_nested_args() {
        let input = "self.setXSpeed(SSF2Utils.decel(self.getXSpeed(), Math.abs(Math.calculateXVelocity(a.get(), b.get()))));";
        let out = translate_ssf2_to_fm(input);
        assert!(!out.contains("SSF2Utils.decel"), "decel not rewritten: {out}");
        assert!(out.contains("Math.min(0,") && out.contains("Math.max(0,"), "no min/max form: {out}");
        assert!(out.contains("Math.calculateXVelocity(a.get(), b.get())"), "nested arg corrupted: {out}");
    }

    #[test]
    fn forced_crash_guard_block_removed() {
        let input = "before();\nif (!self.isForcedCrash()) {\n\tself.setIntangibility(true);\n}\nafter();";
        let out = remove_forced_crash_guards(input);
        assert!(!out.contains("isForcedCrash") || out.contains("guard removed"),
            "isForcedCrash not removed: {out}");
        assert!(!out.contains("setIntangibility(true)"), "guarded body not removed: {out}");
        assert!(out.contains("before();") && out.contains("after();"), "surrounding code lost: {out}");
    }

    #[test]
    fn hit_test_ground_to_line_segment_with_null_check() {
        let neq = rewrite_hit_test_ground("if (self.hitTestGround(a, b) != null) {");
        assert!(neq.contains("hitTestStructuresWithLineSegment(new Point(a, b), new Point(a, b), null, null).length > 0"),
            "!= null not rewritten to .length > 0: {neq}");
        assert!(!neq.contains("hitTestGround"), "old call left: {neq}");
        let eq = rewrite_hit_test_ground("x = self.hitTestGround(a, b) == null;");
        assert!(eq.contains(".length == 0"), "== null not rewritten: {eq}");
        // The 3rd options arg is dropped; nested-object arg must not break the split.
        let opts = rewrite_hit_test_ground("self.hitTestGround(p.x, p.y - 5, { platforms: false })");
        assert!(opts.contains("new Point(p.x, p.y - 5)") && opts.contains(".length > 0"),
            "options form mishandled: {opts}");
    }

    #[test]
    fn ssf2utils_cast_is_neutralized() {
        let input = "if (!SSF2Utils.cast(arg0.data.grabber, SSF2Character)) {\n\tself.x = 1;\n}";
        let out = translate_ssf2_to_fm(input);
        assert!(!out.lines().any(|l| !l.trim_start().starts_with("//") && l.contains("SSF2Utils.")),
            "raw SSF2Utils leaked: {out}");
    }

    #[test]
    fn invalid_gap_receiver_is_commented() {
        // `/* ? */.standtime.set(...)` is invalid Haxe; the line must be commented.
        let out = neutralize_decompiler_gaps("\t/* ? */.standtime.set(standtime.get() - 1);");
        assert!(out.trim_start().starts_with("//"), "invalid receiver not commented: {out}");
    }

    #[test]
    fn empty_rhs_gap_is_commented() {
        let out = neutralize_decompiler_gaps("\tObject.SSF2Utils = /* class */;");
        assert!(out.trim_start().starts_with("//"), "empty RHS not commented: {out}");
    }

    #[test]
    fn valid_gap_positions_are_preserved() {
        // `f(/* ? */)` (empty arg) and `return /* ? */;` (→ `return ;`) are valid.
        let keep = "\tself.doThing(/* ? */);\n\treturn /* ? */;";
        let out = neutralize_decompiler_gaps(keep);
        assert!(!out.lines().any(|l| l.trim_start().starts_with("//")),
            "valid gap position wrongly commented: {out}");
    }

    #[test]
    fn dot_brace_block_property_is_commented() {
        // The projectile/inline-block rewrite can emit `getOwner().{ var _p = … }`
        // — `.{` is invalid Haxe (and its inner `return` would also choke), so the
        // whole line must be commented even with no other gap marker present.
        let input = "\tproj.set(self.getOwner().{ var _p = make(); _p.setX(0); });";
        let out = neutralize_decompiler_gaps(input);
        assert!(out.trim_start().starts_with("//"), "dot-brace block left live: {out}");
    }

    #[test]
    fn multiline_leading_dot_continuation_is_commented() {
        // The marker ends one line (in a // comment), the chain continues with a
        // leading `.` on the next — that `.foo()` is a parse error and must be commented.
        let input = "\t/* ? */// TODO note\n\t.self.updateAnimationStats({ landType: LandType.NORMAL });";
        let out = neutralize_decompiler_gaps(input);
        assert!(!out.lines().any(|l| !l.trim_start().starts_with("//") && l.trim_start().starts_with('.')),
            "leading-dot continuation left live: {out}");
    }
}
