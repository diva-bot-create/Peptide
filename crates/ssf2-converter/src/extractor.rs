use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use crate::swf_parser::SwfFile;
use crate::abc_parser::{self, XframeMap};

// ─── Output types for haxe_gen ───────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterData {
    pub name: String,
    pub attacks: Vec<Attack>,
    pub stats: CharacterStats,
    pub animations: BTreeMap<String, AnimationInfo>,
    pub scripts: Vec<ScriptInfo>,
    /// Instance variables declared on the SSF2 XxxExt class (slot/const traits).
    /// Carried over into Script.hx as top-level `var name;` declarations.
    #[serde(default)]
    pub ext_vars: Vec<String>,
    /// Initial values pulled out of the Ext-class constructor (iinit) for the
    /// `ext_vars`. `(name, rhs)`. Emitted at the top of initialize() unless
    /// the same name is already assigned in the merged-in SSF2 initialize body.
    #[serde(default)]
    pub ext_var_inits: Vec<(String, String)>,
    /// Per-projectile physics + hitboxes pulled from the Ext class's
    /// `getProjectileStats()` method. Keys match the SSF2 projectile name
    /// used in the projectile SymbolClass (e.g. `dee_nspec`). Consumed
    /// by haxe_gen's projectile-file generators to fill real values into
    /// ProjectileStats.hx / ProjectileHitboxStats.hx; falls back to
    /// scaffolding placeholders when a projectile has no entry here.
    #[serde(default)]
    pub projectile_data: BTreeMap<String, abc_parser::ProjectileData>,
    /// SSF2 animation name → Fraymakers animation name
    /// Built from xframe_map + SSF2→Fraymakers name table
    pub ssf2_to_fm_anim: BTreeMap<String, String>,
    /// SSF2 `attackSound{N}_id` table (1-based) backing `playAttackSound(N)`.
    /// Emitted into Script.hx as the `_attackSounds` array + helper. See
    /// `abc_parser::extract_indexed_string_fields`.
    #[serde(default)]
    pub attack_sounds: Vec<String>,
    /// SSF2 `attackVoice{N}_id` table (1-based) backing `playVoiceSound(N)`.
    #[serde(default)]
    pub voice_sounds: Vec<String>,
    /// Populated when this character is a transformation / alternate
    /// form (Giga Bowser, Wario Man). Forwarded from
    /// `ExtractedCharacter.derived_from`. See path 2 plan §1.6.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<abc_parser::DerivedFrom>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attack {
    pub name: String,           // Fraymakers name (e.g. "aerial_neutral")
    pub ssf2_name: String,      // Original SSF2 name (e.g. "a_air")
    pub hitboxes: Vec<Hitbox>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hitbox {
    pub damage:           f64,
    pub angle:            f64,
    pub base_knockback:   f64,
    pub knockback_growth: f64,
    pub hitstop:          i32,
    pub self_hitstop:     i32,
    pub hitstun:          i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharacterStats {
    pub weight:           f64,
    pub gravity:          f64,
    pub fall_speed:       f64,
    pub fast_fall_speed:  f64,
    pub walk_speed:       f64,
    pub dash_speed:       f64,
    pub air_mobility:     f64,
    pub max_jumps:        i32,
    pub jump_height:      f64,
    pub double_jump_height: f64,
    pub air_friction:     f64,
    /// SSF2 `shortHopSpeed` — initial velocity of a short hop (px/frame@30).
    /// 0 when SSF2 lacks it (then FM falls back to a jump-derived estimate).
    pub short_hop_speed:  f64,
    /// SSF2 `decel_rate` — grounded friction (px/frame²@30, stored negative).
    pub ground_friction:  f64,
    /// SSF2 `accel_rate` — grounded walk/run acceleration (px/frame²@30).
    pub ground_accel:     f64,
    /// SSF2 `accel_start` — initial grounded walk speed on press (px/frame@30).
    pub walk_initial:     f64,
    /// SSF2 `accel_start_dash` — initial dash speed on press (px/frame@30).
    pub dash_initial:     f64,
    /// SSF2 `jumpStartup` stat: # of grounded frames before the jump launches
    /// (the jump-squat duration). Used to slice jump_squat off the jump animation.
    pub jump_startup:     f64,
    /// Median scaleX from the root character MovieClip's xframe PlaceObject entries
    pub base_scale_x:     f64,
    /// Median scaleY from the root character MovieClip's xframe PlaceObject entries
    pub base_scale_y:     f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnimationInfo {
    pub name:   String,
    pub frames: u16,
    pub speed:  f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptInfo {
    pub name: String,
    pub code: String,
    pub is_ext_method: bool,
}

impl Default for CharacterStats {
    fn default() -> Self {
        Self {
            weight: 100.0, gravity: 0.0, fall_speed: 0.0, fast_fall_speed: 0.0,
            walk_speed: 0.0, dash_speed: 0.0, air_mobility: 0.0,
            max_jumps: 2, jump_height: 0.0, double_jump_height: 0.0, air_friction: 0.0,
            short_hop_speed: 0.0, ground_friction: 0.0, ground_accel: 0.0,
            walk_initial: 0.0, dash_initial: 0.0,
            jump_startup: 0.0,
            base_scale_x: 1.0, base_scale_y: 1.0,
        }
    }
}

// ─── Main extraction logic ───────────────────────────────────────────────────

pub fn extract(swf: &SwfFile, char_name: &str) -> Result<CharacterData> {
    log::info!("Extracting character data for '{}'", char_name);

    let mut all_attacks: BTreeMap<String, Vec<Hitbox>> = BTreeMap::new();
    let mut char_stats = CharacterStats::default();
    let mut animations: BTreeMap<String, AnimationInfo> = BTreeMap::new();
    let mut scripts: Vec<ScriptInfo> = Vec::new();
    let mut ext_vars: Vec<String> = Vec::new();
    let mut ext_var_inits: Vec<(String, String)> = Vec::new();
    let mut projectile_data: BTreeMap<String, abc_parser::ProjectileData> = BTreeMap::new();
    let mut xframe_map: XframeMap = BTreeMap::new();
    let mut derived_from: Option<abc_parser::DerivedFrom> = None;
    let mut attack_sounds: Vec<String> = Vec::new();
    let mut voice_sounds: Vec<String> = Vec::new();
    // Parity harness source-of-truth: when DUMP_PARITY is set, collect the RAW
    // SSF2 hitbox values (SSF2 key names: damage/direction/power/kbConstant/…)
    // per FM move name, before the SSF2→FM field mapping. Written to
    // /tmp/parity_<char>_ssf2.json for tools/parity_check.py to diff against the
    // generated HitboxStats.hx. (tools/parity_check.py documents the mapping.)
    let dump_parity = std::env::var("DUMP_PARITY").is_ok();
    let mut raw_attacks_dump: BTreeMap<String, Vec<BTreeMap<String, f64>>> = BTreeMap::new();

    // Parse each ABC block (usually just one)
    for (block_idx, abc_data) in swf.abc_blocks.iter().enumerate() {
        log::info!("Parsing ABC block {} ({} bytes)", block_idx, abc_data.len());

        match abc_parser::parse(abc_data) {
            Ok(abc) => {
                let extracted = abc_parser::extract_character(&abc, char_name)?;
                if derived_from.is_none() { derived_from = extracted.derived_from.clone(); }
                if attack_sounds.is_empty() { attack_sounds = extracted.attack_sounds.clone(); }
                if voice_sounds.is_empty()  { voice_sounds  = extracted.voice_sounds.clone(); }

                // Merge attacks
                for (name, attack_data) in &extracted.attacks {
                    let hitboxes = convert_hitboxes(&attack_data.hitboxes);
                    all_attacks.entry(name.clone()).or_default().extend(hitboxes);
                    if dump_parity {
                        raw_attacks_dump.entry(name.clone()).or_default()
                            .extend(attack_data.hitboxes.iter().cloned());
                    }
                }

                // Use stats if found
                if let Some(s) = &extracted.stats {
                    char_stats = convert_stats(&s.values);
                }

                // Decompiled Ext methods → Script.hx
                for (method_name, code) in &extracted.ext_methods {
                    scripts.push(ScriptInfo {
                        name: method_name.clone(),
                        code: code.clone(),
                        is_ext_method: true,
                    });
                }

                // Ext-class instance variable declarations → Script.hx top.
                for v in &extracted.ext_vars {
                    if !ext_vars.contains(v) { ext_vars.push(v.clone()); }
                }
                for (n, rhs) in &extracted.ext_var_inits {
                    if !ext_var_inits.iter().any(|(en, _)| en == n) {
                        ext_var_inits.push((n.clone(), rhs.clone()));
                    }
                }

                // Frame scripts → will go to .entity file (not Script.hx)
                for (method_name, actions) in &extracted.frame_scripts {
                    let code = render_frame_script(method_name, actions);
                    scripts.push(ScriptInfo { name: method_name.clone(), code, is_ext_method: false });
                }


                // Note: we no longer seed animations from raw symbol names here.
                // The xframe_map seeding below produces FM-named entries which avoids
                // duplicates like "bair" + "aerial_back".

                // Merge projectile stats from getProjectileStats()
                for (name, data) in &extracted.projectiles {
                    projectile_data.entry(name.clone()).or_insert_with(|| data.clone());
                }

                // Merge xframe_map (frame method → SSF2 anim name)
                xframe_map.extend(extracted.xframe_map.clone());

                log::info!("ABC block {}: {} attacks, stats={}, {} frame scripts, {} xframe mappings, {} symbols→animations",
                    block_idx,
                    extracted.attacks.len(),
                    extracted.stats.is_some(),
                    extracted.frame_scripts.len(),
                    extracted.xframe_map.len(),
                    animations.len(),
                );
            }
            Err(e) => {
                log::warn!("Failed to parse ABC block {}: {}", block_idx, e);
            }
        }
    }

    if dump_parity {
        // Dev-only diagnostic (DUMP_PARITY); use the OS temp dir so it works on Windows too.
        let path = std::env::temp_dir().join(format!("parity_{}_ssf2.json", char_name));
        if let Ok(j) = serde_json::to_string_pretty(&raw_attacks_dump) {
            let _ = std::fs::write(&path, j);
            log::info!("DUMP_PARITY: wrote raw SSF2 attack stats -> {}", path.display());
        }
    }

    // Convert attacks map to sorted vec
    let attacks: Vec<Attack> = all_attacks.into_iter().map(|(name, hitboxes)| Attack {
        ssf2_name: name.clone(),
        name: name.clone(),
        hitboxes,
    }).collect();

    // Build SSF2 anim name → Fraymakers anim name map
    let ssf2_to_fm_anim = build_ssf2_to_fm_anim(&xframe_map);

    // Also seed animations from xframe_map so every known SSF2 anim appears.
    // For animations that get split into sub-anims (jab→1/2/3, taunt→side/up/down),
    // expand them here so the final animation list is correct.
    for ssf2_name in xframe_map.values() {
        let fm_name = ssf2_to_fm_anim.get(ssf2_name).cloned().unwrap_or_else(|| ssf2_name.clone());
        // Expand split animations
        let sub_names = expand_split_anim(&fm_name);
        if sub_names.is_empty() {
            animations.entry(fm_name.clone()).or_insert(AnimationInfo {
                name: fm_name,
                frames: 0,
                speed: 1.0,
            });
        } else {
            for sub in sub_names {
                animations.entry(sub.clone()).or_insert(AnimationInfo {
                    name: sub,
                    frames: 0,
                    speed: 1.0,
                });
            }
        }
    }

    // Deduplicate: remove raw SSF2/symbol names that have a known FM equivalent.
    // e.g. if both "bair" and "aerial_back" exist, drop "bair".
    // Also drop internal/helper symbols that aren't real character animations.
    // Include sub-anim names (jab1/jab2/jab3 etc) produced by split expansion
    let mut fm_names: std::collections::BTreeSet<String> = ssf2_to_fm_anim.values().cloned().collect();
    for base_fm in ssf2_to_fm_anim.values() {
        for sub in expand_split_anim(base_fm) {
            fm_names.insert(sub);
        }
    }
    let internal_prefixes = ["groundref", "itemplaceholder", "collisonbox", "chargespark", "fireball", "mariocut", "finalsmash"];
    animations.retain(|key, _| {
        // Keep if it's a known FM name
        if fm_names.contains(key) { return true; }
        // Drop if it's a raw SSF2 name that maps to a different FM name
        if let Some(fm) = ssf2_to_fm_anim.get(key) {
            if fm != key { return false; }
        }
        // Drop internal/helper symbols
        let lower = key.to_lowercase();
        if internal_prefixes.iter().any(|p| lower.starts_with(p)) { return false; }
        // Keep anything else (unmapped names stay as-is)
        true
    });

    // Detect IMPLICIT instance vars: `self.X = ...` in the translated script bodies (Script.hx
    // functions + frame scripts) where X is neither a declared ext_var nor one of the char's
    // own functions. SSF2 base-class / dynamically-assigned vars (speed, bounceSpeed, charge,
    // …) aren't slot/const traits on the char class, so without this they'd be emitted as
    // undefined `self.X` (e.g. `self.speed`) and crash at runtime. Construct + wrap them as
    // persistent state like the declared vars. A clean literal RHS drives type inference.
    {
        let own_fns: std::collections::BTreeSet<&str> =
            scripts.iter().filter(|s| s.is_ext_method).map(|s| s.name.as_str()).collect();
        let re = regex::Regex::new(r"self\.([A-Za-z_]\w*)\s*=\s*([^=;][^;]*)").unwrap();
        let mut implicit: std::collections::BTreeMap<String, Option<String>> = Default::default();
        for s in &scripts {
            for cap in re.captures_iter(&s.code) {
                let name = cap[1].to_string();
                if ext_vars.contains(&name) || own_fns.contains(name.as_str()) { continue; }
                let rhs = cap[2].trim().to_string();
                let is_lit = matches!(rhs.as_str(), "true" | "false") || rhs.parse::<i64>().is_ok();
                let e = implicit.entry(name).or_insert(None);
                if is_lit && e.is_none() { *e = Some(rhs); }
            }
        }
        for (name, lit) in implicit {
            log::info!("implicit instance var '{}' (used but not declared) — constructing as persistent state", name);
            if let Some(rhs) = lit {
                if !ext_var_inits.iter().any(|(n, _)| n == &name) { ext_var_inits.push((name.clone(), rhs)); }
            }
            ext_vars.push(name);
        }
    }

    // SSF2 `removeTimer(callback)` identifies a timer by its CALLBACK; Fraymakers
    // `addTimer(interval, repeats, func, options?)` returns a uid and `removeTimer(uid)`
    // takes that uid. Passing the function straight through is a runtime type error
    // ("Can't cast (void ()) to i32"), the engine traps it, and the timer is never
    // removed — so every SSF2 poll that's supposed to stop itself runs forever.
    //
    // Bridge the two by remembering the uid: for a callback that is BOTH registered by a
    // named `addTimer` and cancelled by name, synthesize a persistent `<name>_timer`
    // slot, capture the uid into it at registration, and cancel through it. Emitted in
    // `self.<var>` form so the persistent-state pass wraps it into `.set()`/`.get()` with
    // every other instance var. A `removeTimer` whose argument isn't a bare identifier
    // (`removeTimer(effects.get(i))`) can't be resolved statically and is left alone.
    //
    // The slot defaults to -1, so if a later pass drops the registration (the callback is
    // cancelled from a script that survives while the one that registered it doesn't) the
    // cancel degrades to `removeTimer(-1)`, which the engine answers `false`. Same
    // not-removed behaviour as before, minus the trapped type error.
    //
    // Runs on the decompiled source, so it matches the SSF2 spellings (`createTimer` /
    // `destroyTimer`) as well as the Fraymakers ones — the commands.jsonc rename to
    // add/removeTimer happens downstream of here.
    {
        // decompiler output at this stage carries a doubled receiver
        // (`self.self.createTimer(…)`) and prefixes the callback (`self.dashCheck`);
        // both are normalized downstream, so match them loosely and re-emit verbatim.
        const RECV: &str = r"((?:self\.)+)";
        const ADD: &str = r"((?:create|add)Timer)";
        const RM:  &str = r"((?:destroy|remove)Timer)";
        let add_re = regex::Regex::new(
            &format!(r"{RECV}{ADD}\(\s*([^,()]+),\s*([^,()]+),\s*(?:self\.)?([A-Za-z_]\w*)\s*([,)])")
        ).unwrap();
        let rm_re = regex::Regex::new(
            &format!(r"{RECV}{RM}\(\s*(?:self\.)?([A-Za-z_]\w*)\s*\)")
        ).unwrap();
        let mut registered: std::collections::BTreeSet<String> = Default::default();
        let mut cancelled: std::collections::BTreeSet<String> = Default::default();
        for s in &scripts {
            for c in add_re.captures_iter(&s.code) { registered.insert(c[5].to_string()); }
            for c in rm_re.captures_iter(&s.code)  { cancelled.insert(c[3].to_string()); }
            if std::env::var("PEPTIDE_TIMER_DEBUG").is_ok() {
                for l in s.code.lines().filter(|l| l.contains("Timer")) {
                    eprintln!("[timer-raw] {}", l.trim());
                }
            }
        }
        let slots: Vec<(String, String)> = registered.intersection(&cancelled)
            .map(|n| (n.clone(), format!("{n}_timer")))
            .filter(|(_, slot)| !ext_vars.contains(slot))
            .collect();
        for (callback, slot) in &slots {
            log::info!("timer '{}' cancelled by callback -> capturing its uid in persistent '{}'",
                callback, slot);
            for s in scripts.iter_mut() {
                // capture at registration. only the exact (interval, repeats, <callback>)
                // shape is touched, so an inline `function () { … }` callback (which
                // nothing can cancel by name anyway) is left as-is.
                let cap = regex::Regex::new(&format!(
                    r"{RECV}{ADD}\(\s*([^,()]+),\s*([^,()]+),\s*((?:self\.)?{})\s*([,)])",
                    regex::escape(callback)
                )).unwrap();
                s.code = cap.replace_all(&s.code, format!(
                    "self.{slot} = ${{1}}${{2}}(${{3}}, ${{4}}, ${{5}}${{6}}"
                )).into_owned();
                // and cancel through the slot
                let can = regex::Regex::new(&format!(
                    r"{RECV}{RM}\(\s*(?:self\.)?{}\s*\)", regex::escape(callback)
                )).unwrap();
                s.code = can.replace_all(&s.code, format!("${{1}}${{2}}(self.{slot})")).into_owned();
            }
            if !ext_var_inits.iter().any(|(n, _)| n == slot) {
                ext_var_inits.push((slot.clone(), "-1".to_string()));
            }
            ext_vars.push(slot.clone());
        }
    }

    // SSF2 get/setGlobalVariable("name"[, v]) are string-keyed PERSISTENT per-character
    // variables. Collect each distinct key as a persistent-state ext var so a
    // `var name = self.makeInt(0)/makeBool(false)` declaration is emitted; the calls
    // themselves are rewritten to name.get()/name.set(v) by the commands.jsonc regex
    // rules. Init kind: Int when the var is used numerically (set to an int literal, or
    // its get() sits next to an arithmetic / numeric-comparison operator), else Bool
    // (most SSF2 globals are flags). hscript is dynamically typed, so the wrapper kind
    // only sets the value seen BEFORE the first set — picking the right default matters,
    // the get/set themselves work regardless.
    {
        let get_re = regex::Regex::new(r#"getGlobalVariable\(\s*"(\w+)"\s*\)"#).unwrap();
        let set_re = regex::Regex::new(r#"setGlobalVariable\(\s*"(\w+)"\s*,\s*(-?\d+\b)?"#).unwrap();
        let num_ctx = regex::Regex::new(
            r#"getGlobalVariable\(\s*"(\w+)"\s*\)\s*(?:[-+*/]|[<>]=?|[!=]=)\s*-?\d|-?\d\s*(?:[-+*/]|[<>]=?|[!=]=)\s*(?:self\.)?getGlobalVariable\(\s*"(\w+)"\s*\)|(?:self\.)?getGlobalVariable\(\s*"(\w+)"\s*\)\s*[-+*/]"#
        ).unwrap();
        let mut names: std::collections::BTreeSet<String> = Default::default();
        let mut numeric: std::collections::BTreeSet<String> = Default::default();
        for s in &scripts {
            for c in get_re.captures_iter(&s.code) { names.insert(c[1].to_string()); }
            for c in set_re.captures_iter(&s.code) {
                names.insert(c[1].to_string());
                if c.get(2).is_some() { numeric.insert(c[1].to_string()); }
            }
            for c in num_ctx.captures_iter(&s.code) {
                for i in 1..=3 { if let Some(m) = c.get(i) { numeric.insert(m.as_str().to_string()); } }
            }
        }
        for name in names {
            if ext_vars.contains(&name) { continue; }
            let init = if numeric.contains(&name) { "0" } else { "false" };
            log::info!("global var '{}' (SSF2 get/setGlobalVariable) -> persistent {} wrapper",
                name, if init == "0" { "Int" } else { "Bool" });
            if !ext_var_inits.iter().any(|(n, _)| n == &name) {
                ext_var_inits.push((name.clone(), init.to_string()));
            }
            ext_vars.push(name);
        }
    }

    // Resolve var/function NAME COLLISIONS: a name that is BOTH a persistent instance var
    // and one of the char's own functions cannot coexist at Script.hx top level (and makes
    // `self.<name>` ambiguous). Rename the FUNCTION (its definition + calls — `<name>(`) to
    // `<name>_2`, leaving the VAR with the bare name (wrapped to .get()/.set()). Function
    // CALLS are `<name>(`; var accesses are `<name>` / `<name>.get()`, so the `(` cleanly
    // distinguishes them.
    {
        let var_set: std::collections::BTreeSet<String> = ext_vars.iter().cloned().collect();
        let collisions: std::collections::BTreeSet<String> = scripts.iter()
            .filter(|s| s.is_ext_method && var_set.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        for name in &collisions {
            let new_name = format!("{}_2", name);
            log::info!("name collision '{}' (instance var + function) — renaming the function to '{}'", name, new_name);
            let re = regex::Regex::new(&format!(r"\b{}\(", regex::escape(name))).unwrap();
            for s in &mut scripts {
                s.code = re.replace_all(&s.code, format!("{}(", new_name)).into_owned();
                if s.is_ext_method && &s.name == name { s.name = new_name.clone(); }
            }
        }
    }

    // Drop any character-defined `flipX`. Fraymakers provides Entity.flipX(v:Float):Float as a
    // built-in (negates v when facing left); SSF2 characters that define their own `flipX` are
    // just reimplementing it. Removing the definition takes `flipX` out of the char's own-method
    // set, so rewrite_own_method_refs keeps every reference as `self.flipX(...)` — binding to the
    // engine implementation instead of a local copy. Any bare `flipX(` left over (e.g. a lex-call
    // the decompiler didn't qualify) is requalified to `self.flipX(` so nothing dangles.
    {
        let had_flipx = scripts.iter().any(|s| s.is_ext_method && s.name == "flipX");
        if had_flipx {
            scripts.retain(|s| !(s.is_ext_method && s.name == "flipX"));
            // Requalify any bare flipX( call to self.flipX( (skip ones already qualified with `.`).
            let re = regex::Regex::new(r"(^|[^.\w])flipX\(").unwrap();
            for s in &mut scripts {
                s.code = re.replace_all(&s.code, "${1}self.flipX(").into_owned();
            }
            log::info!("removed character-defined flipX — all references now use the engine's self.flipX");
        }
    }

    // Synthesize SSF2 state-transition shortcut handlers (toLand / toCrashLand / toHelpless).
    // Fraymakers has no such methods — each is just `self.toState(CState.X)`. They are used as
    // add/removeEventListener callbacks, so they MUST be stable NAMED functions (an inline
    // closure would make a later removeEventListener fail to match and leave a dangling
    // listener). We emit a named function per referenced shortcut; rewrite_own_method_refs then
    // strips `self.` from each call site (`self.toLand` -> bare `toLand`) so the listener and its
    // remover both reference this one definition. The body's `self.toState(...)` is an FM
    // built-in and is left untouched.
    {
        let shortcuts: [(&str, &str); 3] = [
            ("toLand",      "CState.LAND"),
            ("toCrashLand", "CState.CRASH_BOUNCE"),
            ("toHelpless",  "CState.FALL_SPECIAL"),
        ];
        for (name, state) in shortcuts {
            let re = regex::Regex::new(&format!(r"\b{}\b", name)).unwrap();
            let referenced = scripts.iter().any(|s| re.is_match(&s.code));
            let already_defined = scripts.iter().any(|s| s.is_ext_method && s.name == name);
            if referenced && !already_defined {
                scripts.push(ScriptInfo {
                    name: name.to_string(),
                    code: format!("function {}(arg0) {{\n\tself.toState({});\n}}", name, state),
                    is_ext_method: true,
                });
                log::info!("synthesized state-shortcut handler {}() -> self.toState({})", name, state);
            }
        }
    }

    log::info!("Total: {} attacks, {} animations, {} ssf2→fm mappings extracted",
        attacks.len(), animations.len(), ssf2_to_fm_anim.len());

    Ok(CharacterData {
        ext_vars,
        ext_var_inits,
        projectile_data,
        name: char_name.to_string(),
        attacks,
        stats: char_stats,
        animations,
        scripts,
        ssf2_to_fm_anim,
        attack_sounds,
        voice_sounds,
        derived_from,
    })
}

// ─── SSF2 → Fraymakers animation name table ───────────────────────────────────

/// The FM parent anims that split into sub-anims at the sprite level, paired with every
/// sub-anim name they can produce. One shared table drives both `expand_split_anim` (seeding
/// candidate names) and `is_split_sub_anim` (recognizing a produced sub-anim). The frame-range
/// splitting itself lives in `sprite_parser::sub_anim_splits`, which dispatches on this same
/// parent set. We can't know per-character how many sub-anims a sprite has, so we list enough
/// to cover the max we'll encounter (jab4).
pub const SPLIT_ANIMS: &[(&str, &[&str])] = &[
    // Jab: up to 4 hits (captainfalcon, pit, ryu)
    ("jab",   &["jab1", "jab2", "jab3", "jab4"]),
    // Taunt: always exactly 3 slots (the parent `taunt` keeps its name)
    ("taunt", &["taunt", "taunt_up", "taunt_down"]),
];

/// For an SSF2 animation that maps to a single FM name but splits into sub-animations at the
/// sprite level (e.g. jab → jab1/jab2/jab3), return ALL POSSIBLE sub-anim names. These seed the
/// animation list so even unseen sub-anims get placeholder entries; sprite_parser trims to only
/// those with actual frames.
pub fn expand_split_anim(fm_name: &str) -> Vec<String> {
    SPLIT_ANIMS.iter()
        .find(|(parent, _)| *parent == fm_name)
        .map(|(_, subs)| subs.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

/// The FM animation a split piece came FROM (`jab1` -> `jab`), or `None` if this name is not a
/// piece. The mapping files speak in parent names -- `a` maps to `jab`, never to `jab1` -- so
/// anything looking an SSF2 animation up from a piece has to ask for the parent.
pub fn split_parent(fm_name: &str) -> Option<&'static str> {
    SPLIT_ANIMS.iter()
        .find(|(parent, subs)| *parent != fm_name && subs.contains(&fm_name))
        .map(|(parent, _)| *parent)
}

/// Returns true if this FM animation name is a sub-animation produced by splitting (a produced
/// name distinct from its parent — so `jab1` is, but the un-renamed `taunt` parent is not).
pub fn is_split_sub_anim(fm_name: &str) -> bool {
    SPLIT_ANIMS.iter()
        .any(|(parent, subs)| *parent != fm_name && subs.contains(&fm_name))
}

fn build_ssf2_to_fm_anim(xframe_map: &XframeMap) -> BTreeMap<String, String> {
    // SSF2 animation name → Fraymakers animation name table is loaded from
    // mappings/character/animations.json (see crate::mappings).
    let lookup = &crate::mappings::character_animations().ssf2_to_fm;
    let mut result = BTreeMap::new();

    // Map every unique SSF2 anim name from xframe_map through the table
    for ssf2_name in xframe_map.values() {
        let fm_name = lookup.get(ssf2_name)
            .cloned()
            .unwrap_or_else(|| ssf2_name.clone()); // fallback: keep as-is
        result.insert(ssf2_name.clone(), fm_name);
    }

    result
}

// ─── Conversion helpers ─────────────────────────────────────────────────────────

fn convert_hitboxes(raw: &[BTreeMap<String, f64>]) -> Vec<Hitbox> {
    // SSF2 key → Fraymakers hitbox field mapping is loaded from
    // mappings/character/hitbox_stats.json (see crate::mappings).
    let cfg = crate::mappings::character_hitbox_stats();
    raw.iter().map(|obj| {
        // Value of a Fraymakers field = max over its SSF2 source keys that are
        // PRESENT. Only-present is load-bearing: SSF2 uses NEGATIVE values as
        // special-angle sentinels (e.g. direction=-2), and if an absent sibling
        // key (e.g. "angle") defaulted to 0.0 it would win the max() and clobber
        // the real negative value to 0 (wrong launch direction). Absent keys must
        // not participate. If no source key is present, the field is 0.0.
        let v = |fm: &str| cfg.keys_for(fm).iter()
            .filter_map(|k| obj.get(k).copied())
            .reduce(f64::max)
            .unwrap_or(0.0);
        Hitbox {
            damage:           v("damage"),
            angle:            v("angle"),
            base_knockback:   v("baseKnockback"),
            knockback_growth: v("knockbackGrowth"),
            hitstop:          v("hitstop") as i32,
            self_hitstop:     v("selfHitstop") as i32,
            hitstun:          v("hitstun") as i32,
        }
    }).collect()
}

fn convert_stats(vals: &BTreeMap<String, f64>) -> CharacterStats {
    // SSF2 key name → Fraymakers field mapping is loaded from
    // mappings/character/stats.json (see crate::mappings).
    let stat_map = crate::mappings::character_stats();
    let get = |field: &str| {
        stat_map.keys_for(field).iter()
            .find_map(|k| vals.get(k)).copied().unwrap_or(0.0)
    };
    CharacterStats {
        weight:             get("weight"),
        gravity:            get("gravity"),
        fall_speed:         get("fall_speed"),
        fast_fall_speed:    get("fast_fall_speed"),
        walk_speed:         get("walk_speed"),
        dash_speed:         get("dash_speed"),
        air_mobility:       get("air_mobility"),
        // SSF2 max_jump counts midair jumps; Fraymakers counts total jumps.
        // The +1 offset is loaded from mappings/character/stats.json.
        max_jumps:          get("max_jumps") as i32 + stat_map.offset("max_jumps") as i32,
        jump_height:        get("jump_height"),
        double_jump_height: get("double_jump_height"),
        air_friction:       get("air_friction"),
        // Real SSF2 movement constants that were previously discarded in favour
        // of template constants — wired through so walk/run/jump feel matches
        // the source. Raw keys (px/frame or px/frame²@30); 0 ⇒ absent in SSF2.
        short_hop_speed:    vals.get("shortHopSpeed").copied().unwrap_or(0.0),
        ground_friction:    vals.get("decel_rate").copied().unwrap_or(0.0),
        ground_accel:       vals.get("accel_rate").copied().unwrap_or(0.0),
        walk_initial:       vals.get("accel_start").copied().unwrap_or(0.0),
        dash_initial:       vals.get("accel_start_dash").copied().unwrap_or(0.0),
        // Internal-only (NOT a Fraymakers stat): raw SSF2 jumpStartup frame count,
        // used solely to slice jump_squat off the front of the jump animation.
        jump_startup:       vals.get("jumpStartup").copied().unwrap_or(0.0),
        // base_scale_x/y are set later from sprite_parser::extract_xframe_scale
        base_scale_x:       1.0,
        base_scale_y:       1.0,
    }
}

fn render_frame_script(_method_name: &str, actions: &[crate::abc_parser::FrameAction]) -> String {
    // Actions now contain decompiled Haxe code in action.action (frame=0 sentinel)
    // Just concatenate the decompiled bodies
    actions.iter().map(|a| a.action.as_str()).collect::<Vec<_>>().join("\n")
}

