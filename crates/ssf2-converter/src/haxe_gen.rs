/// Fraymakers character file generator.
/// Produces output matching the official character-template structure:
/// https://github.com/Fraymakers/character-template
/// Reference character: https://github.com/ZacharyClayton721/kung-fu-man-fraymakers

use anyhow::Result;
use std::fs;
use std::path::Path;
use crate::extractor::{CharacterData, Hitbox};
use crate::entity_gen;
use crate::fraytools_project;
use crate::palette_gen;
use crate::uuid_gen::det_uuid;

pub fn generate(output_dir: &Path, char_name: &str, char_pascal: &str, data: &CharacterData, sprite_boxes: &std::collections::BTreeMap<String, crate::sprite_parser::AnimationBoxData>, img_result: &crate::image_extractor::ImageExtractionResult, costumes_json: Option<&Path>, sounds: &[crate::sound_extractor::SoundEntry], projectiles: &[crate::image_extractor::DiscoveredProjectile], effects: &[crate::image_extractor::DiscoveredEffect], head_sprite: Option<&crate::image_extractor::DiscoveredHead>, parsed_swf: &swf::Swf<'_>, multi_char_slot: Option<&crate::project::MultiCharSlot>) -> Result<()> {
    let char_id = char_name.to_lowercase().replace(" ", "");
    // In multi-char mode every character writes into the shared project
    // dir; in single-char mode each character has its own subdir under
    // `output_dir`. The project-finalizer (main::finalize_multi_char_project)
    // handles the project-level manifest + .fraytools when in multi-char.
    let char_dir = match multi_char_slot {
        Some(s) => s.project_dir.clone(),
        None    => output_dir.join(&char_id),
    };
    // Per docs/multi_character_projects_plan.md §1: character scripts
    // live at library/scripts/<Pascal>/ (was library/scripts/Character/).
    let scripts_dir = char_dir.join(format!("library/scripts/{}", char_pascal));
    fs::create_dir_all(&scripts_dir)?;

    // Effect → primary-animation map for the context-aware
    // self.attachEffect("name") → match.createVfx(...) rewrite. The guard
    // installs the map for the duration of this function and clears it on
    // drop, so every translate_ssf2_to_fm call made while generating this
    // character (Character.entity frame scripts, Script.hx, per-attack
    // scripts) sees the right effect→animation pairing.
    let effect_anim_map: std::collections::BTreeMap<String, String> = effects.iter()
        .filter_map(|e| {
            entity_gen::effect_animation_names(e)
                .into_iter()
                .next()
                .map(|first| (e.name.clone(), first))
        })
        .collect();
    let _effect_anim_guard = crate::api_mappings::EffectAnimGuard::install(effect_anim_map);

    // Per-character extracted-sound id set → lets rewrite_play_sound_calls
    // emit getContent("id") for sounds we have and a placeholder for the rest.
    // Mirrors how sound_extractor sanitizes names for the .wav filenames.
    let sound_ids: std::collections::BTreeSet<String> = sounds.iter()
        .map(|s| s.name.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect())
        .collect();
    let _sounds_guard = crate::api_mappings::AvailableSoundsGuard::install(sound_ids);

    log::info!("Generating Fraymakers character package in {}", char_dir.display());

    // How many of jab1/jab2/jab3/jab4 actually have image content. Drives
    // the jab-chain emission in Script.hx and the keep-empty allowlist in
    // entity_gen — a single-jab character gets no chain boilerplate, and
    // jab2/jab3 are dropped as empty along with the others.
    let populated_jabs = count_populated_jabs(img_result);

    fs::write(scripts_dir.join("HitboxStats.hx"),   generate_hitbox_stats(data, &char_id))?;
    fs::write(scripts_dir.join("CharacterStats.hx"), generate_character_stats(data, &char_id))?;
    let splits = crate::anim_splitter::split_animations(&data.animations, sprite_boxes, data.stats.jump_startup.round().max(0.0) as u16, &data.scripts);
    fs::write(scripts_dir.join("AnimationStats.hx"), generate_animation_stats(data, &splits))?;
    fs::write(scripts_dir.join("Script.hx"),         generate_script(data, &char_id, populated_jabs))?;

    // .meta sidecar files for character scripts
    fs::write(scripts_dir.join("HitboxStats.hx.meta"),    script_meta(&format!("{}HitboxStats", char_id),    &det_uuid(&format!("{}::HitboxStats::meta", char_id)),    ScriptMetaKind::CharacterHitboxStats))?;
    fs::write(scripts_dir.join("CharacterStats.hx.meta"), script_meta(&format!("{}CharacterStats", char_id), &det_uuid(&format!("{}::CharacterStats::meta", char_id)), ScriptMetaKind::CharacterStats))?;
    fs::write(scripts_dir.join("AnimationStats.hx.meta"), script_meta(&format!("{}AnimationStats", char_id), &det_uuid(&format!("{}::AnimationStats::meta", char_id)), ScriptMetaKind::CharacterAnimationStats))?;
    fs::write(scripts_dir.join("Script.hx.meta"),         script_meta(&format!("{}Script", char_id),         &det_uuid(&format!("{}::Script::meta", char_id)),         ScriptMetaKind::CharacterScript))?;

    // .fraytools + manifest are project-level. In single-char mode we
    // write them here; in multi-char mode they're written once by the
    // project finalizer in main.rs after every character is processed.
    if multi_char_slot.is_none() {
        fs::write(char_dir.join(format!("{}.fraytools", char_name)), fraytools_project::generate_fraytools_project(char_name))?;
        let proj_names: Vec<String> = projectiles.iter().map(|p| p.name.clone()).collect();
        fs::write(char_dir.join("library/manifest.json"), generate_manifest(&char_id, char_name, &proj_names))?;
        fs::write(char_dir.join("library/manifest.json.meta"), generate_manifest_meta(&det_uuid(&format!("{}::manifest::meta", char_id))))?;
    }

    // <Pascal>.entity (was Character.entity — see plan §1)
    let entities_dir = char_dir.join("library/entities");
    fs::create_dir_all(&entities_dir)?;
    let char_entity_filename = format!("{}.entity", char_pascal);
    fs::write(entities_dir.join(&char_entity_filename), entity_gen::generate_entity(data, &char_id, sprite_boxes, img_result, populated_jabs))?;

    // Generate .meta sidecar files for each sprite PNG. Image GUIDs are
    // symbol-only (crate::uuid_gen::image_meta_guid) so they're consistent across
    // the character, projectile/effect and menu paths — critical for multi-char
    // projects where one library/sprites/ is shared (else refs dangle → placeholders).
    let meta_guids = entity_gen::get_image_meta_guids(img_result);
    let sprites_dir = char_dir.join("library/sprites");
    let mut meta_count = 0;
    for (png_rel_path, guid) in &meta_guids {
        let meta_path = sprites_dir.join(format!("{}.meta", png_rel_path.trim_start_matches("library/sprites/")));
        if let Some(parent) = meta_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&meta_path, entity_gen::generate_meta(guid))?;
        meta_count += 1;
    }
    log::info!("Generated {} .meta sidecar files", meta_count);

    let mut palette_collection_guid: Option<String> = None;
    let mut palette_base_map_id: Option<String> = None;
    // ── Palette / costumes ──────────────────────────────────────────────────
    // For multi-char projects, characters share the project-level
    // costumes.palettes / palette_preview.png paths. The first character
    // (constructor-walk slot 0) keeps the unsuffixed filename; subsequent
    // characters append `2`/`3`/... per the suffix rule in
    // docs/multi_character_projects_plan.md §2.
    let suffix = multi_char_slot.and_then(|s| s.collision_suffix())
        .map(|n| n.to_string()).unwrap_or_default();
    match palette_gen::generate_palettes_and_remap(&char_id, char_name, &sprites_dir, costumes_json) {
        Ok(pal) => {
            // Suffix goes on the BASE name, not the extension: char 2 is
            // `costumes2.palettes`, NOT `costumes.palettes2`. A `.palettes2`
            // extension is malformed — FrayTools only recognizes `.palettes`
            // collections, so the 2nd+ char's costumes never loaded (the engine's
            // "Could not find script: <char>Costumes" lookup then failed). Same
            // class of bug as the palette_preview.png suffix fixed below.
            fs::write(char_dir.join(format!("library/costumes{}.palettes", suffix)),       &pal.palettes_json)?;
            fs::write(char_dir.join(format!("library/costumes{}.palettes.meta", suffix)),  &pal.palettes_meta_json)?;
            // CHAR-UNIQUE preview filename. The old `palette_preview.png{suffix}` scheme
            // gave the 2nd+ char of a multi-char project a name like `palette_preview.png2`
            // — a malformed extension that ALSO shares the base name `palette_preview` with
            // char 1, so FrayTools (which derives a sprite's GUID from its path) collides
            // the two previews. A char-prefixed `.png` name is unique + valid. The palette
            // links its preview by GUID (imageAsset), so the filename isn't referenced elsewhere.
            let preview_name = format!("{}_palette_preview.png", char_id);
            fs::write(sprites_dir.join(&preview_name),                    &pal.preview_png)?;
            fs::write(sprites_dir.join(format!("{}.meta", preview_name)), &pal.preview_meta_json)?;
            // Write the entity with the paletteMap filled in
            let entity_json = entity_gen::generate_entity_with_palette(
                data, &char_id, sprite_boxes, img_result, populated_jabs,
                &pal.collection_guid, &pal.base_map_id,
            );
            fs::write(entities_dir.join(&char_entity_filename), entity_json)?;
            palette_collection_guid = Some(pal.collection_guid.clone());
            palette_base_map_id = Some(pal.base_map_id.clone());
        }
        Err(e) => {
            log::warn!("palette_gen failed (sprites will have no palette): {}", e);
        }
    }

    // ── menu.entity ────────────────────────────────────────────────────────────────
    if let Some(head) = head_sprite {
        if let Some(ref img_sym) = head.image_symbol {
            // Find the head image in our extracted images
            let head_image = img_result.images.values().find(|img| &img.symbol_name == img_sym);
            if let Some(head_img) = head_image {
                let head_meta_guid = crate::uuid_gen::image_meta_guid(img_sym);
                let menu_info = entity_gen::MenuImageInfo {
                    head_symbol: img_sym.clone(),
                    head_width: head_img.width,
                    head_height: head_img.height,
                    head_meta_guid,
                };
                let mut menu_json = entity_gen::generate_menu_entity(&char_id, &menu_info);
                // Fill in paletteMap if available
                if let (Some(ref cg), Some(ref pm)) = (&palette_collection_guid, &palette_base_map_id) {
                    let mut menu_val: serde_json::Value = serde_json::from_str(&menu_json).unwrap_or(serde_json::json!({}));
                    menu_val["paletteMap"] = serde_json::json!({
                        "paletteCollection": cg,
                        "paletteMap": pm
                    });
                    menu_json = serde_json::to_string_pretty(&menu_val).unwrap_or(menu_json);
                }
                let menu_filename = match multi_char_slot {
                    Some(_) => format!("{}_Menu.entity", char_pascal),
                    None    => "Menu.entity".to_string(),
                };
                fs::write(entities_dir.join(&menu_filename), menu_json)?;
                log::info!("Generated {} using {} ({}x{})", menu_filename, img_sym, head_img.width, head_img.height);
            } else {
                log::warn!("Head image '{}' not found in extracted images, skipping Menu.entity", img_sym);
            }
        } else {
            log::warn!("Head sprite '{}' has no image placement, skipping Menu.entity", head.name);
        }
    } else {
        log::warn!("No head sprite found, skipping Menu.entity");
    }

    // ── projectile files ──────────────────────────────────────────────────
    // Delegated to the summoner-agnostic projectile subsystem so stages/items
    // can emit projectiles the same way (see projectile_gen). The character is
    // just one kind of owner here.
    {
        let scripts_root = char_dir.join("library/scripts");
        let ctx = crate::projectile_gen::ProjectileGenCtx {
            owner_id: &char_id,
            entities_dir: &entities_dir,
            scripts_root: &scripts_root,
            parsed_swf,
            img_result,
            projectile_data: &data.projectile_data,
            palette_collection_guid: palette_collection_guid.as_deref(),
            palette_base_map_id: palette_base_map_id.as_deref(),
        };
        crate::projectile_gen::generate_projectiles(&ctx, projectiles)?;
    }

    // ── effect .entity files ─────────────────────────────────────────────
    // Per-effect entities (no scripts, no stats, no manifest entries).
    // The character's Script.hx spawns them via match.createVfx(...).
    let mut blank_effects: Vec<String> = Vec::new();
    for effect in effects {
        let filename = format!("{}.entity", effect.name);
        let entity_json = entity_gen::generate_effect_entity(
            &char_id, effect, img_result, parsed_swf,
        );
        // Surface effects that still rasterized to nothing (no image symbols) —
        // typically Kirby copy-ability hats / placeholders whose visuals are
        // built from deeply-nested named sprites we don't yet flatten.
        // Count IMAGE *symbols* (which carry "imageAsset"), not the IMAGE
        // *layer* type — a blank effect still has an Image Layer.
        let image_symbol_count = entity_json.matches("\"imageAsset\"").count();
        if image_symbol_count == 0 && effect.frame_count > 0 {
            blank_effects.push(effect.name.clone());
            log::warn!("Effect '{}' has no extractable images ({} frames) — renders blank in FrayTools (no vector/bitmap content found to rasterize)", effect.name, effect.frame_count);
        }
        fs::write(entities_dir.join(&filename), entity_json)?;
        let anim_names = entity_gen::effect_animation_names(effect);
        log::info!(
            "Generated effect entity: {} ({} frames, animations: [{}], {} image symbols)",
            filename,
            effect.frame_count,
            anim_names.join(", "),
            image_symbol_count,
        );
    }
    if !blank_effects.is_empty() {
        log::warn!("{} effect(s) render blank (no extractable art): {}", blank_effects.len(), blank_effects.join(", "));
    }

    // Stats summary for debugging
    let stats_json = serde_json::json!({
        "char_id": char_id,
        "display_name": char_name,
        "attacks_extracted": data.attacks.len(),
        "stats_extracted": data.stats.weight != 0.0,
        "animations": data.animations.len(),
        "frame_scripts": data.scripts.len(),
        "ssf2_to_fm_anim": data.ssf2_to_fm_anim,
    });
    fs::write(char_dir.join("conversion_stats.json"), serde_json::to_string_pretty(&stats_json)?)?;

    // ── Sound content entries ─────────────────────────────────────────────────────────────────
    if !sounds.is_empty() {
        // Mirrors main::process_character's per-char-subdir rule for
        // multi-char projects (plan §3); flat path for single-char.
        let audio_dir = match multi_char_slot {
            Some(_) => char_dir.join(format!("library/audio/{}", char_id)),
            None    => char_dir.join("library/audio"),
        };
        generate_sound_entries(&audio_dir, char_name, sounds)?;
        log::info!("Generated sound entries for {} sounds", sounds.len());
    }

    log::info!("Generated: {} attacks, {} animations, {} frame scripts",
        data.attacks.len(), data.animations.len(), data.scripts.len());
    Ok(())
}

// ─── SSF2 → Fraymakers stat scaling ─────────────────────────────────────────
// SSF2 uses pixel-per-frame units at 60fps.
// Fraymakers uses its own unit system. Approximate conversions based on
// studying template characters vs SSF2 data.

// Scaling factors are loaded from mappings/character/stats.json (see crate::mappings).
fn ssf2_gravity_to_fm(v: f64) -> f64 { crate::mappings::character_stats().scale("gravity", v) }
fn ssf2_speed_to_fm(v: f64) -> f64   { crate::mappings::character_stats().scale("speed", v) }
fn ssf2_jump_to_fm(v: f64) -> f64    { crate::mappings::character_stats().scale("jump", v) }
fn ssf2_walk_to_fm(v: f64) -> f64    { crate::mappings::character_stats().scale("walk", v) }
fn ssf2_dash_to_fm(v: f64) -> f64    { crate::mappings::character_stats().scale("dash", v) }
fn ssf2_air_to_fm(v: f64) -> f64     { crate::mappings::character_stats().scale("air_friction", v.abs()) }

fn fmt(v: f64) -> String {
    if v == v.round() && v.abs() < 1000.0 {
        format!("{}", v as i64)
    } else {
        format!("{:.2}", v).trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

// ─── HitboxStats.hx ─────────────────────────────────────────────────────────

fn generate_hitbox_stats(data: &CharacterData, _char_id: &str) -> String {
    let attack_lookup: std::collections::BTreeMap<_, _> = data.attacks.iter()
        .map(|a| (a.name.as_str(), a))
        .collect();

    let t = &crate::mappings::script_templates().character.stats;
    let req = crate::mappings::require_template;
    let mut out = req("character.stats.hitbox_header", &t.hitbox_header)
        .replace("{{char_name}}", &data.name);

    let tables = crate::mappings::character_stats_tables();
    let standard: std::collections::HashSet<&str> = tables.hitbox_sections.iter()
        .flat_map(|s| s.moves.iter().map(|m| m.as_str())).collect();

    for sec in &tables.hitbox_sections {
        out.push_str(&req("character.stats.hitbox_section_comment", &t.hitbox_section_comment)
            .replace("{{section}}", &sec.section));
        for move_name in &sec.moves {
            let move_name = move_name.as_str();
            // SSF2 stores a split combo (jab1/2/3, strong _in/_charge/_attack) as ONE
            // attack keyed by the base move; the FM timeline splits it into several named
            // sub-animations. Direct lookup only finds the base name, so fall back to it
            // for the sub-names — otherwise jab2/jab3 etc. emit 0-damage placeholders and
            // are non-functional in-engine. (Per-segment fidelity is a deeper follow-up;
            // inheriting the base attack's hitboxes is far closer to SSF2 than zero.)
            let direct = attack_lookup.get(move_name).copied();
            let base = base_attack_name(move_name);
            let inherited = direct.is_none() && base.is_some();
            let found = direct.or_else(|| base.as_deref().and_then(|b| attack_lookup.get(b).copied()));
            if let Some(attack) = found {
                out.push_str(&format_attack(move_name, &attack.hitboxes, false,
                    if inherited { base.as_deref() } else { None }));
            } else if move_name == "emote" {
                out.push_str(req("character.stats.hitbox_emote", &t.hitbox_emote));
            } else {
                out.push_str(&format_attack_todo(move_name));
            }
        }
    }

    // Extra attacks from SSF2 that don't map to standard moves
    let extras: Vec<_> = data.attacks.iter()
        .filter(|a| !standard.contains(a.name.as_str())).collect();
    if !extras.is_empty() {
        out.push_str(req("character.stats.hitbox_extras_header", &t.hitbox_extras_header));
        for attack in extras {
            out.push_str(&format_attack(&attack.name, &attack.hitboxes, true, None));
        }
    }

    out.push_str(req("character.stats.hitbox_footer", &t.hitbox_footer));
    let out = strip_trailing_commas(&out);
    // ERROR HANDLER for broken hitbox stats: the engine parses HitboxStats as one hscript object
    // literal; a single malformed entry (e.g. a half-commented attack block) breaks the WHOLE parse
    // → null stats → the character silently deals no damage (hurtboxes still work, so nothing
    // crashes — a nasty silent failure). Validate the generated object here so a converter bug
    // surfaces loudly at convert time instead of as dead hitboxes in-engine.
    assert_balanced_stats(&out, &format!("HitboxStats for {}", data.name));
    out
}

/// Fail the conversion (loudly, in peptide) if a generated stats object is not a syntactically
/// balanced hscript object literal — the failure mode that produces dead hitboxes in-engine.
/// Counts `{}`/`[]` ignoring `//` line comments (string literals in these files never contain
/// braces). A mismatch means an attack block is malformed and the engine's parse would fail.
fn assert_balanced_stats(s: &str, context: &str) {
    let mut curly: i64 = 0;
    let mut square: i64 = 0;
    for line in s.lines() {
        let code = match line.find("//") { Some(i) => &line[..i], None => line };
        for c in code.chars() {
            match c {
                '{' => curly += 1,
                '}' => curly -= 1,
                '[' => square += 1,
                ']' => square -= 1,
                _ => {}
            }
        }
        if curly < 0 || square < 0 {
            panic!("broken stats object ({context}): unbalanced braces — an attack block is \
                    malformed (a stray/closing brace). The engine would fail to parse this and the \
                    character would deal no damage. Offending line: {:?}", line.trim());
        }
    }
    if curly != 0 || square != 0 {
        panic!("broken stats object ({context}): unbalanced braces at end \
                (curly={curly}, square={square}) — a malformed attack block. The engine would fail \
                to parse this and the character would deal no damage.");
    }
}

/// Remove trailing commas inside object literals — any `,` that is followed only by
/// whitespace and then a `}`. The hitbox-stats templates append `},` after every hitbox
/// and every move, which leaves a trailing comma before the closing brace of the last
/// element. hscript's object-literal parser REJECTS trailing commas, so the whole
/// HitboxStats expression fails to parse → null stats → hitboxes get no stats and never
/// deal damage (hurtboxes need no stats, so they still work — which is exactly the
/// "converted chars can be hit but can't hit" bug). This pass makes the output valid.
/// Only touches structural trailing commas; commas BETWEEN fields are untouched.
fn strip_trailing_commas(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b',' {
            let mut j = i + 1;
            while j < b.len() && (b[j] as char).is_whitespace() { j += 1; }
            if j < b.len() && b[j] == b'}' {
                i += 1; // drop the comma, keep the whitespace that follows
                continue;
            }
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

fn guess_limb(move_name: &str) -> String {
    // Ordered substring rules from mappings/character/stats_tables.jsonc;
    // first match wins, else the default.
    let lr = &crate::mappings::character_stats_tables().limb_rules;
    for rule in &lr.rules {
        if rule.contains.iter().any(|s| move_name.contains(s.as_str())) {
            return rule.limb.clone();
        }
    }
    lr.default.clone()
}

/// Base move that carries the SSF2 hitbox stats for a split sub-animation name,
/// or None if `move_name` isn't a split sub-name. SSF2 keeps the whole combo as a
/// single attack under the base name; the FM split produces extra names that must
/// inherit those stats. Kept in lockstep with the anim_splitter's split rules.
fn base_attack_name(move_name: &str) -> Option<String> {
    // jabN (N >= 2) inherits jab1 (the SSF2 `a` attack carries the combo's hitboxes).
    if let Some(rest) = move_name.strip_prefix("jab") {
        if rest.parse::<u32>().map(|n| n >= 2).unwrap_or(false) {
            return Some("jab1".to_string());
        }
    }
    // strong_*_in / strong_*_charge inherit strong_*_attack (the hit-bearing phase,
    // which is the name normalize_attack_name emits for the SSF2 strong attack).
    for suf in ["_in", "_charge"] {
        if let Some(stem) = move_name.strip_suffix(suf) {
            if stem.starts_with("strong_") {
                return Some(format!("{stem}_attack"));
            }
        }
    }
    None
}

fn format_attack(name: &str, hitboxes: &[Hitbox], is_extra: bool, inherited_from: Option<&str>) -> String {
    let t = &crate::mappings::script_templates().character.stats;
    let req = crate::mappings::require_template;
    let limb = guess_limb(name);
    // Always build the block with a real tab prefix; an `is_extra` block is commented out WHOLE
    // below. (The old code only commented the opening `// SSF2: name: {` line and left the hitbox
    // body active+keyless — a syntax error that broke the ENTIRE HitboxStats object parse, so NO
    // hitboxes got stats and converted characters couldn't deal damage. Hurtboxes need no stats,
    // so they kept working — which is exactly the observed bug.)
    let prefix = "\t";
    let mut out = String::new();
    if let Some(base) = inherited_from {
        // Make the inheritance explicit so a modder knows these values were copied
        // from the base move (SSF2 stored the combo as one attack) and may want
        // per-hit tuning.
        out.push_str(&format!("\t// stats inherited from {base} (SSF2 stores this combo as one attack — tune per-hit if needed)\n"));
    }
    out.push_str(&req("character.stats.hitbox_attack_open", &t.hitbox_attack_open)
        .replace("{{prefix}}", prefix).replace("{{name}}", name));
    // Frame-count fields are doubled for the 30fps -> 60fps timing change.
    // Which fields are frame counts is driven by the `isframe` flags in
    // mappings/character/hitbox_stats.json (see crate::mappings), the same
    // mechanism commands.json uses — so all frame-doubling is one path.
    let hb_cfg = crate::mappings::character_hitbox_stats();
    let scale = |fm_field: &str, v: i32| if hb_cfg.is_frame(fm_field) { v * 2 } else { v };
    for (i, hb) in hitboxes.iter().enumerate() {
        // The -1 "no override" sentinel (and SSF2's 255, which maps to it) is
        // emit formatting and stays in code; only the doubling is data-driven.
        let hitstun = if hb.hitstun == 255 || hb.hitstun == -1 { -1 } else { scale("hitstun", hb.hitstun) };
        let hitstop = if hb.hitstop <= 0 { -1 } else { scale("hitstop", hb.hitstop) };
        let self_hitstop = if hb.self_hitstop <= 0 { -1 } else { scale("selfHitstop", hb.self_hitstop) };

        out.push_str(&req("character.stats.hitbox_field_main", &t.hitbox_field_main)
            .replace("{{i}}", &i.to_string())
            .replace("{{damage}}", &(hb.damage as i32).to_string())
            .replace("{{angle}}", &(hb.angle as i32).to_string())
            .replace("{{base_knockback}}", &(hb.base_knockback as i32).to_string())
            .replace("{{knockback_growth}}", &(hb.knockback_growth as i32).to_string())
            .replace("{{hitstop}}", &hitstop.to_string())
            .replace("{{self_hitstop}}", &self_hitstop.to_string()));
        if hitstun != -1 {
            out.push_str(&req("character.stats.hitbox_field_hitstun", &t.hitbox_field_hitstun)
                .replace("{{hitstun}}", &hitstun.to_string()));
        }
        out.push_str(&req("character.stats.hitbox_field_close", &t.hitbox_field_close)
            .replace("{{limb}}", &limb));
    }
    out.push_str(req("character.stats.hitbox_attack_close", &t.hitbox_attack_close));
    if is_extra {
        // Comment the ENTIRE block — an SSF2 extra has no FM move name, so it's documentation
        // only. Commenting every line keeps it inert (and syntactically valid).
        out = out.lines()
            .map(|l| format!("\t// SSF2: {}", l.trim_start()))
            .collect::<Vec<_>>()
            .join("\n");
        out.push('\n');
    }
    out
}

fn format_attack_todo(name: &str) -> String {
    let limb = guess_limb(name);
    crate::mappings::require_template(
        "character.stats.hitbox_todo_attack",
        &crate::mappings::script_templates().character.stats.hitbox_todo_attack,
    ).replace("{{name}}", name).replace("{{limb}}", &limb)
}

// ─── CharacterStats.hx ───────────────────────────────────────────────────────

fn generate_character_stats(data: &CharacterData, char_id: &str) -> String {
    let s = &data.stats;
    // flat constants + template physics defaults come from stats.jsonc.
    let cfg = crate::mappings::character_stats();
    let c = |k: &str| cfg.constant(k);
    let c_num = |k: &str| c(k).parse::<f64>().unwrap_or(0.0);

    // Convert SSF2 values to Fraymakers equivalents (scaling driven by the multipliers in
    // stats.jsonc). When SSF2 LACKS a value, fall back to the CHARACTER-TEMPLATE DEFAULT
    // (not 0) so a converted character is never left with 0 weight/gravity/etc. — those
    // zeros caused null/degenerate physics in-engine. The annotation flags template-sourced
    // values so an author can see what wasn't derived from SSF2.
    let dflt = " /*template default*/";
    let (gravity, gravity_todo)            = if s.gravity > 0.0          { (ssf2_gravity_to_fm(s.gravity), "") }       else { (c_num("gravity"), dflt) };
    let (terminal_vel, terminal_vel_todo)  = if s.fall_speed > 0.0       { (ssf2_speed_to_fm(s.fall_speed), "") }      else { (c_num("terminalVelocity"), dflt) };
    let (fast_fall, fast_fall_todo)        = if s.fast_fall_speed > 0.0  { (ssf2_speed_to_fm(s.fast_fall_speed), "") } else { (c_num("fastFallSpeed"), dflt) };
    let (jump_speed, jump_speed_todo)      = if s.jump_height > 0.0      { (ssf2_jump_to_fm(s.jump_height), "") }      else { (c_num("jumpSpeed"), dflt) };
    let dj_speed      = if s.double_jump_height > 0.0 { ssf2_jump_to_fm(s.double_jump_height) } else { 0.0 };
    let (walk_cap, walk_cap_todo)          = if s.walk_speed > 0.0       { (ssf2_walk_to_fm(s.walk_speed), "") }       else { (c_num("walkSpeedCap"), dflt) };
    let (dash_speed, dash_speed_todo)      = if s.dash_speed > 0.0       { (ssf2_dash_to_fm(s.dash_speed), "") }       else { (c_num("dashSpeed"), dflt) };
    let (aerial_fric, aerial_fric_todo)    = if s.air_friction != 0.0    { (ssf2_air_to_fm(s.air_friction), "") }      else { (c_num("aerialFriction"), dflt) };
    let (weight, weight_todo)              = if s.weight > 0.0           { (s.weight, "") }                            else { (c_num("weight"), dflt) };

    // Derived stats — expression strings in stats.jsonc, evaluated with the already-converted
    // stats as variables. short_hop derives from jump_speed (template-defaulted above) so it
    // is always sane; aerial_cap derives from SSF2 air mobility/friction, so if SSF2 had
    // NEITHER, fall back to the template default rather than a near-zero derived value.
    let vars: std::collections::BTreeMap<String, f64> = [
        ("jump_speed".to_string(), jump_speed),
        ("air_mobility_raw".to_string(), s.air_mobility),
        ("aerial_friction".to_string(), aerial_fric),
    ].into_iter().collect();
    let short_hop_d  = crate::mappings::evaluate_stat_derivation("shortHopSpeed", &vars).unwrap_or(0.0);
    let aerial_cap_d = crate::mappings::evaluate_stat_derivation("aerialSpeedCap", &vars).unwrap_or(0.0);
    // shortHopSpeed: prefer the REAL SSF2 value (velocity-scaled). Only fall
    // back to the jump-derived estimate / template constant when SSF2 lacked it.
    let (short_hop, short_hop_todo) = if s.short_hop_speed > 0.0 {
        (ssf2_speed_to_fm(s.short_hop_speed), "")
    } else if short_hop_d > 0.0 {
        (short_hop_d, " /*TODO: estimated from jumpSpeed; SSF2 had no shortHopSpeed*/")
    } else {
        (c_num("shortHopSpeed"), dflt)
    };
    let (aerial_cap, aerial_fric_cap_todo) = if s.air_mobility != 0.0 || s.air_friction != 0.0 { (aerial_cap_d, "") } else { (c_num("aerialSpeedCap"), dflt) };

    // ── Real SSF2 movement constants, previously hardcoded template values ──
    // Now derived from the source (verified against docs/ssf2-physics-model.md)
    // so walk/run/jump feel matches SSF2. Accelerations use accel_scale; speeds
    // use velocity_scale (see mappings::scale). The single size_multiplier knob
    // governs both.
    let sm = crate::mappings::character_stats();
    // friction := |decel_rate| (grounded). SSF2 stores it negative.
    let (friction, friction_todo) = if s.ground_friction != 0.0 {
        (sm.scale("friction", s.ground_friction.abs()), "")
    } else {
        (c_num("friction"), " /*TODO*/")
    };
    // SSF2 has ONE grounded accel (accel_rate) feeding both walk and run/ground.
    let (walk_accel, walk_accel_todo) = if s.ground_accel > 0.0 {
        (sm.scale("walk_accel", s.ground_accel), "")
    } else {
        (c_num("walkSpeedAcceleration"), "")
    };
    let ground_accel_v = if s.ground_accel > 0.0 { walk_accel } else { c_num("groundSpeedAcceleration") };
    let run_accel_v    = if s.ground_accel > 0.0 { walk_accel } else { c_num("runSpeedAcceleration") };
    // SSF2 accel_rate_air (carried in air_mobility) → aerial horizontal accel.
    let (aerial_accel, aerial_accel_todo) = if s.air_mobility != 0.0 {
        (sm.scale("air_accel", s.air_mobility), "")
    } else {
        (c_num("aerialSpeedAcceleration"), "")
    };
    // Initial walk/dash speeds: accel_start / accel_start_dash are MULTIPLIERS on
    // the respective speed cap (m_charRun: xSpeed = AccelStart*norm_xSpeed, dash
    // = AccelStartDash*max_xSpeed). Convert the resulting absolute kick, then
    // clamp into a sane [0, cap] band (SSF2 re-clamps to the cap next frame; some
    // dummies carry a degenerate >cap kick value).
    let walk_initial = if s.walk_initial > 0.0 && s.walk_speed > 0.0 {
        ssf2_speed_to_fm(s.walk_initial * s.walk_speed).min(walk_cap).max(0.0)
    } else { c_num("walkSpeedInitial") };
    let run_initial = if s.dash_initial > 0.0 && s.dash_speed > 0.0 {
        ssf2_speed_to_fm(s.dash_initial * s.dash_speed).min(dash_speed).max(0.0)
    } else { c_num("runSpeedInitial") };
    // runSpeedCap mirrors the dash cap (SSF2 max_xSpeed) when present.
    let run_cap = if s.dash_speed > 0.0 { dash_speed } else { c_num("runSpeedCap") };

    // doubleJumpSpeeds: the real converted value, or the character-template default.
    let dj_array = if dj_speed > 0.0 {
        format!("[{}]", fmt(dj_speed))
    } else {
        format!("[{}]{}", c("doubleJumpSpeedFallback"), dflt)
    };

    // Transformation banner: when this character was extracted from a
    // Main::get<X>() bundle whose cData.normalStats_id ≠ the derived id
    // (Giga Bowser, Wario Man), prepend a TODO note. Fraymakers has no
    // native transformation hook; the form is emitted as a standalone
    // character and the content author must wire the trigger by hand.
    let t = &crate::mappings::script_templates().character.stats;
    let req = crate::mappings::require_template;

    let transformation_banner = if let Some(df) = &data.derived_from {
        req("character.stats.char_stats_banner", &t.char_stats_banner)
            .replace("{{parent}}", &df.parent_normal_stats_id)
            .replace("{{source_method}}", &df.source_method)
    } else {
        String::new()
    };

    // aerial_cap is derived from air_mobility_raw + aerial_friction; the TODO
    // marker must reflect whether the DERIVATION result is zero, not whether
    // `s.air_mobility` happens to be (fixes §3.10).
    let mut out = transformation_banner;
    out.push_str(&req("character.stats.char_stats_main", &t.char_stats_main)
        .replace("{{char_name}}", &data.name)
        .replace("{{char_id}}", char_id)
        .replace("{{base_scale_x}}", &fmt(s.base_scale_x))
        .replace("{{base_scale_y}}", &fmt(s.base_scale_y))
        .replace("{{weight}}", &fmt(weight)).replace("{{weight_todo}}", weight_todo)
        .replace("{{gravity}}", &fmt(gravity)).replace("{{gravity_todo}}", gravity_todo)
        .replace("{{short_hop}}", &fmt(short_hop)).replace("{{short_hop_todo}}", short_hop_todo)
        .replace("{{jump_speed}}", &fmt(jump_speed)).replace("{{jump_speed_todo}}", jump_speed_todo)
        .replace("{{dj_array}}", &dj_array)
        .replace("{{terminal_vel}}", &fmt(terminal_vel)).replace("{{terminal_vel_todo}}", terminal_vel_todo)
        .replace("{{fast_fall}}", &fmt(fast_fall)).replace("{{fast_fall_todo}}", fast_fall_todo)
        .replace("{{friction}}", &fmt(friction)).replace("{{friction_todo}}", friction_todo)
        .replace("{{walk_speed_initial}}", &fmt(walk_initial))
        .replace("{{walk_speed_accel}}", &fmt(walk_accel)).replace("{{walk_speed_accel_todo}}", walk_accel_todo)
        .replace("{{walk_cap}}", &fmt(walk_cap)).replace("{{walk_cap_todo}}", walk_cap_todo)
        .replace("{{dash_speed}}", &fmt(dash_speed)).replace("{{dash_speed_todo}}", dash_speed_todo)
        .replace("{{run_speed_initial}}", &fmt(run_initial))
        .replace("{{run_speed_accel}}", &fmt(run_accel_v))
        .replace("{{run_speed_cap}}", &fmt(run_cap))
        .replace("{{ground_speed_accel}}", &fmt(ground_accel_v))
        .replace("{{ground_speed_cap}}", &c("groundSpeedCap"))
        .replace("{{aerial_speed_accel}}", &fmt(aerial_accel)).replace("{{aerial_speed_accel_todo}}", aerial_accel_todo)
        .replace("{{aerial_cap}}", &fmt(aerial_cap)).replace("{{aerial_cap_todo}}", aerial_fric_cap_todo)
        .replace("{{aerial_fric}}", &fmt(aerial_fric)).replace("{{aerial_fric_todo}}", aerial_fric_todo));

    // Flat-constant sections — every value comes from stats.json `constants`.
    out.push_str(&req("character.stats.char_stats_section_comment", &t.char_stats_section_comment).replace("{{title}}", "ENVIRONMENTAL COLLISION BODY (ECB) STATS"));
    let tables = crate::mappings::character_stats_tables();
    for fa in &tables.stats_fields.ecb {
        out.push_str(&req("character.stats.char_stats_field_anno", &t.char_stats_field_anno).replace("{{field}}", &fa.field).replace("{{value}}", &c(&fa.field)).replace("{{anno}}", &fa.anno));
    }
    out.push('\n');

    out.push_str(&req("character.stats.char_stats_section_comment", &t.char_stats_section_comment).replace("{{title}}", "CAMERA BOX STATS"));
    for f in tables.stats_fields.camera.iter().map(|s| s.as_str()) {
        out.push_str(&req("character.stats.char_stats_field_plain", &t.char_stats_field_plain).replace("{{field}}", f).replace("{{value}}", &c(f)));
    }
    out.push('\n');

    out.push_str(&req("character.stats.char_stats_section_comment", &t.char_stats_section_comment).replace("{{title}}", "ROLL AND LEDGE JUMP STATS"));
    for f in tables.stats_fields.roll.iter().map(|s| s.as_str()) {
        out.push_str(&req("character.stats.char_stats_field_plain", &t.char_stats_field_plain).replace("{{field}}", f).replace("{{value}}", &c(f)));
    }
    out.push('\n');

    out.push_str(&req("character.stats.char_stats_section_comment", &t.char_stats_section_comment).replace("{{title}}", "AIRDASH STATS"));
    for f in tables.stats_fields.airdash.iter().map(|s| s.as_str()) {
        out.push_str(&req("character.stats.char_stats_field_plain", &t.char_stats_field_plain).replace("{{field}}", f).replace("{{value}}", &c(f)));
    }
    out.push('\n');

    out.push_str(&req("character.stats.char_stats_section_comment", &t.char_stats_section_comment).replace("{{title}}", "SHIELD STATS"));
    for f in tables.stats_fields.shield.iter().map(|s| s.as_str()) {
        out.push_str(&req("character.stats.char_stats_field_plain", &t.char_stats_field_plain).replace("{{field}}", f).replace("{{value}}", &c(f)));
    }
    out.push('\n');

    out.push_str(&req("character.stats.char_stats_section_comment", &t.char_stats_section_comment).replace("{{title}}", "VOICE STATS"));
    for f in tables.stats_fields.voice.iter().map(|s| s.as_str()) {
        out.push_str(&req("character.stats.char_stats_field_plain", &t.char_stats_field_plain).replace("{{field}}", f).replace("{{value}}", &c(f)));
    }

    out.push_str(req("character.stats.char_stats_footer", &t.char_stats_footer));
    out
}

// ─── AnimationStats.hx ───────────────────────────────────────────────────────

fn generate_animation_stats(data: &CharacterData, splits: &[crate::anim_splitter::SplitAnim]) -> String {
    use std::collections::BTreeSet;

    // ── Base FM template: animations with hand-tuned properties ──────────────
    // These are the standard Fraymakers character-template entries.
    // Order and grouping match the official template.
    let template = crate::mappings::character_animation_template();

    // Collect template names for dedup
    let template_names: BTreeSet<&str> = template.iter().map(|e| e.name.as_str()).collect();

    let t = &crate::mappings::script_templates().character.stats;
    let req = crate::mappings::require_template;
    let mut out = req("character.stats.anim_stats_header", &t.anim_stats_header)
        .replace("{{char_name}}", &data.name);
    let entry_tpl = req("character.stats.anim_stats_entry", &t.anim_stats_entry);

    // Emit template entries (the template Vec is data — stays in Rust). `body`
    // is the `{}` / `{props}` object literal assembled here.
    for e in template {
        let body = if e.props.is_empty() { "{}".to_string() } else { format!("{{{}}}", e.props) };
        out.push_str(&entry_tpl.replace("{{name}}", &e.name).replace("{{body}}", &body));
    }

    // Emit split animations not already in template
    let mut extra_names: Vec<&str> = Vec::new();
    for split in splits {
        if !template_names.contains(split.fm_name.as_str()) && !extra_names.contains(&split.fm_name.as_str()) {
            extra_names.push(&split.fm_name);
        }
    }
    if !extra_names.is_empty() {
        out.push_str(req("character.stats.anim_stats_split_header", &t.anim_stats_split_header));
        for name in &extra_names {
            // Check if this split has loop_tail
            let is_loop = splits.iter().any(|s| s.fm_name == *name && s.loop_tail);
            let body = if is_loop { "{endType:AnimationEndType.LOOP}".to_string() } else { "{}".to_string() };
            out.push_str(&entry_tpl.replace("{{name}}", name).replace("{{body}}", &body));
        }
    }

    out.push_str(req("character.stats.anim_stats_footer", &t.anim_stats_footer));
    out
}

// ─── Script.hx ───────────────────────────────────────────────────────────────

/// How many of jab1..jab4 actually have image content. Drives whether the
/// jab-chain boilerplate is emitted in Script.hx, and whether the empty-
/// animation drop in entity_gen keeps jab2/jab3 as referenced placeholders.
fn count_populated_jabs(img: &crate::image_extractor::ImageExtractionResult) -> usize {
    ["jab1", "jab2", "jab3", "jab4"].iter()
        .filter(|name| img.anim_images.get(**name)
            .map(|af| af.frames.values().any(|f| !f.is_empty()))
            .unwrap_or(false))
        .count()
}

fn generate_script(data: &CharacterData, _char_id: &str, populated_jabs: usize) -> String {
    // Filter out trivial slot-initializer stubs.
    let ext_methods: Vec<&crate::extractor::ScriptInfo> = data.scripts.iter()
        .filter(|s| s.is_ext_method)
        .filter(|s| !s.code.contains("Object.SSF2API"))
        .collect();

    // SSF2 ext methods whose names collide with the template functions are
    // MERGED into those functions (their bodies inlined after the template's
    // mandatory setup) instead of renamed to ssf2_*. That keeps one canonical
    // initialize() / update() / ... and avoids a redundant ssf2_initialize().
    const TEMPLATE_FNS: [&str; 5] =
        ["initialize", "update", "inputUpdateHook", "handleLinkFrames", "onTeardown"];
    let mut template_bodies: std::collections::BTreeMap<&str, String> =
        std::collections::BTreeMap::new();
    let mut regular_ext: Vec<&crate::extractor::ScriptInfo> = Vec::new();
    for s in &ext_methods {
        if let Some(tf) = TEMPLATE_FNS.iter().find(|t| s.name == **t).copied() {
            let translated = crate::api_mappings::translate_ssf2_to_fm(&s.code);
            if let Some(body) = extract_fn_body(&translated) {
                template_bodies.insert(tf, body);
            }
        } else {
            regular_ext.push(s);
        }
    }

    // Script.hx output templates (moved to commands.jsonc :: script_templates).
    let fw = &crate::mappings::script_templates().character.framework;
    let req = crate::mappings::require_template;

    // Emit one merged template function per name. `setup` is the mandatory FM
    // line(s) the template must always include (e.g. the LINK_FRAMES listener
    // registration on initialize); the SSF2 body, if any, is appended.
    let emit_tpl = |out: &mut String, header_comment: &str, signature: &str, setup: &str, name: &str| {
        if !header_comment.is_empty() { out.push_str(header_comment); }
        out.push_str(signature);
        out.push_str(setup);
        if let Some(body) = template_bodies.get(name) {
            out.push_str(body);
            out.push('\n');
        }
        out.push_str(req("character.framework.fn_close", &fw.fn_close));
    };

    let mut out = req("character.framework.header", &fw.header).replace("{{char_name}}", &data.name);

    // Instance variables carried over from the SSF2 XxxExt class (its
    // Slot/Const traits — `public var foo:T;`). Emitted as Fraymakers
    // persistent-state wrappers: `var foo = self.makeInt(0)` /
    // `self.makeBool(false)` / `self.makeObject(null)`, with the kind
    // inferred from each var's init expression in `ext_var_inits`.
    // Wrapped wrappers expose `.get() / .set(v) / .inc() / .dec()`.
    let mut var_types = crate::api_mappings::infer_ext_var_types(&data.ext_vars, &data.ext_var_inits);
    // An SSF2 ext var whose name collides with a RESERVED Fraymakers identifier must NOT be
    // wrapped — emitting `var self = self.makeObject(null)` SHADOWS the real `self` API for
    // the whole script, so every `self.foo()` in a function silently operates on a null
    // wrapper (this is exactly what killed sandbag's down-special dashCheck). `self` (and the
    // `match` global) already exist in scope; drop the wrapper so refs resolve to them, and
    // strip them from var_types so the persistent-state pass leaves `self.x`/`match.x` alone.
    for r in crate::api_mappings::RESERVED_EXT_VARS { var_types.remove(*r); }
    let is_reserved = |n: &str| crate::api_mappings::RESERVED_EXT_VARS.contains(&n);
    if !data.ext_vars.is_empty() {
        out.push_str(&req("character.framework.instance_vars_comment", &fw.instance_vars_comment)
            .replace("{{char_name}}", &data.name));
        for v in &data.ext_vars {
            if is_reserved(v) { continue; }
            let (factory, default) = match var_types.get(v).copied().unwrap_or(crate::api_mappings::ExtVarType::Object) {
                crate::api_mappings::ExtVarType::Bool   => ("makeBool", "false"),
                crate::api_mappings::ExtVarType::Int    => ("makeInt", "0"),
                crate::api_mappings::ExtVarType::Object => ("makeObject", "null"),
            };
            out.push_str(&req("character.framework.ext_var_decl", &fw.ext_var_decl)
                .replace("{{name}}", v).replace("{{factory}}", factory).replace("{{default}}", default));
        }
        out.push('\n');
    }

    out.push_str(req("character.framework.general_functions_begin", &fw.general_functions_begin));

    // initialize — extend the template's setup with iinit-derived
    // `self.<var> = <expr>;` assignments for each ext_var, but SKIP any name
    // the merged-in SSF2 initialize body already assigns (per user: "if
    // something is already set in initialize then skip that").
    let init_body_text = template_bodies.get("initialize").map(|s| s.as_str()).unwrap_or("");
    let mut init_setup = String::from(
        req("character.framework.link_frames_listener", &fw.link_frames_listener)
    );
    for (name, expr) in &data.ext_var_inits {
        if is_reserved(name) { continue; } // reserved (self/match) — not a wrapped var
        // Skip names the SSF2 initialize body already covers — match both
        // the legacy `self.X = ` form (in case the merged body hasn't been
        // wrapped yet) and the new `X.set(` form.
        let legacy_needle = format!("self.{} = ", name);
        let wrapped_needle = format!("{}.set(", name);
        if !init_body_text.contains(&legacy_needle) && !init_body_text.contains(&wrapped_needle) {
            // Skip emitting an initial assignment when the wrapper's own
            // default already matches (e.g. `var foo = self.makeBool(false)`
            // doesn't need `foo.set(false)` right after).
            let default_already_matches = matches!(
                (var_types.get(name).copied(), expr.trim()),
                (Some(crate::api_mappings::ExtVarType::Bool), "false")
                | (Some(crate::api_mappings::ExtVarType::Int), "0")
                | (Some(crate::api_mappings::ExtVarType::Object), "null"),
            );
            if !default_already_matches {
                init_setup.push_str(&req("character.framework.ext_var_init_assign", &fw.ext_var_init_assign)
                    .replace("{{name}}", name).replace("{{expr}}", expr));
            }
        }
    }
    // Sound-helper emission flags (see the sound block below). Computed here so
    // onTeardown can splice in voice-clip cleanup when this character has a
    // voice helper.
    let calls_attack = data.scripts.iter().any(|s| s.code.contains("playAttackSound("));
    let calls_voice  = data.scripts.iter().any(|s| s.code.contains("playVoiceSound("));
    let emit_voice = !data.voice_sounds.is_empty() || calls_voice;
    // onTeardown cleanup: stop + null the active voice clip when a voice helper
    // exists. Spliced into the single template onTeardown so any SSF2
    // onTeardown body is preserved.
    let teardown_setup = crate::api_mappings::voice_teardown_cleanup(emit_voice);

    emit_tpl(&mut out, req("character.framework.initialize_header", &fw.initialize_header),
        req("character.framework.initialize_sig", &fw.initialize_sig), &init_setup, "initialize");
    emit_tpl(&mut out, "", req("character.framework.update_sig", &fw.update_sig), "", "update");
    emit_tpl(&mut out,
        req("character.framework.input_update_hook_header", &fw.input_update_hook_header),
        req("character.framework.input_update_hook_sig", &fw.input_update_hook_sig),
        "", "inputUpdateHook");
    emit_tpl(&mut out, req("character.framework.handle_link_frames_header", &fw.handle_link_frames_header),
        req("character.framework.handle_link_frames_sig", &fw.handle_link_frames_sig), "", "handleLinkFrames");
    emit_tpl(&mut out, "", req("character.framework.onteardown_sig", &fw.onteardown_sig), &teardown_setup, "onTeardown");

    out.push_str(req("character.framework.general_functions_end", &fw.general_functions_end));

    if !regular_ext.is_empty() {
        out.push_str(req("character.framework.decompiled_ext_header", &fw.decompiled_ext_header));
        for script in &regular_ext {
            let translated = crate::api_mappings::translate_ssf2_to_fm(&script.code);
            out.push_str(&translated);
            out.push('\n');
        }
    }

    // Frame scripts are embedded directly in the entity file via FRAME_SCRIPT layers.
    // They are no longer duplicated here.

    // SSF2 sound triggers — playAttackSound(N) / playVoiceSound(N) helpers
    // backed by the character's attackSound{N}_id / attackVoice{N}_id tables.
    // Frame-script calls (self.playAttackSound(N) → bare playAttackSound(N) via
    // commands.jsonc) resolve here. Attack/voice sections emit independently:
    // each when its table is non-empty OR the character calls that function —
    // the latter keeps a call-without-table (e.g. sandbag) bound to a defined,
    // bounds-guarded no-op. Runs inside the generate() AvailableSounds guard so
    // unmapped ids fall back to the silent placeholder. (calls_attack/calls_voice
    // computed above, near the onTeardown emission.)
    out.push_str(&crate::api_mappings::generate_sound_helpers(
        &data.attack_sounds, &data.voice_sounds, calls_attack, calls_voice));

    // Jab chain transition logic — only when the character actually has a
    // multi-hit combo. Single-jab characters get no chain boilerplate, so
    // nothing references the missing jab2/jab3 animations.
    if populated_jabs >= 2 {
        out.push_str(&generate_jab_scripts(populated_jabs));
    }

    // Full-script post-pass: fix paired setIntangibility calls
    out = crate::api_mappings::fix_intangibility_pairs(&out);

    // Full-script post-pass. ORDER matters:
    //  (1) own-function refs `self.<fn>` -> bare `<fn>` (frame scripts share this scope),
    //  (2) instance-var refs `self.<var>` -> persistent wrappers (.get()/.set()/.inc()/.dec()).
    // Frame scripts embedded in the entity get the same two rewrites in entity_gen so
    // cross-file references stay consistent. FM API methods (self.toState, …) are untouched.
    // own FUNCTION names only — exclude any that's also a persistent var (e.g. a name that
    // is both a slot trait and a method), so `self.<var>` stays for the var-wrap pass.
    let ext_methods: Vec<String> = data.scripts.iter().filter(|s| s.is_ext_method)
        .map(|s| s.name.clone()).filter(|n| !data.ext_vars.contains(n)).collect();
    out = crate::api_mappings::rewrite_own_method_refs(&out, &ext_methods);
    out = crate::api_mappings::wrap_persistent_state(&out, &var_types);

    // Whole-file pass: a call commented as [SSF2-only: NAME] is valid after all if
    // NAME is defined as a local function here (comment_out runs per-method and
    // can't see sibling defs). Restore those calls.
    out = crate::api_mappings::uncomment_local_fn_calls(&out);
    // Same reconciliation on the bookkeeping: a call to a helper this file defines is
    // not an unmapped gap, so drop it from the conversion log's `unknown` tally.
    crate::api_mappings::discount_local_fn_calls(&out);

    // Final pass: any commenter that neutralized a block-opening `{` (e.g. an inline
    // SSF2-only translator on `if (self.getMC()...) {`) leaves the matching `}` live
    // and orphaned, which is a parse error the engine swallows silently. Comment any
    // such orphaned close so the emitted hscript stays balanced. #13.
    out = crate::api_mappings::balance_commented_blocks(&out);

    out
}

// ─── Jab chain scripts ─────────────────────────────────────────────────────

/// Generate jab1/jab2/jab3 chain transition frame scripts.
///
/// In SSF2, the single 'Jab_21' sprite has three sub-animations separated by
/// internal frame labels (begin → hit2 → hit3). SSF2 code calls `gotoAndPlay("hit2")`
/// to chain into the next hit on button press, and `gotoAndPlay("begin")` to loop jab1.
///
/// In Fraymakers, each is a separate animation. The chain logic lives in framescripts:
///   - jab1: on last frame, if attack pressed → enter jab2; else idle
///   - jab2: on last frame, if attack pressed → enter jab3; else idle
///   - jab3: on last frame → idle
/// Extract the body between the outermost braces of a `function NAME(...) { ... }`
/// string. Used to inline a translated SSF2 ext method into the matching
/// template function. Returns None if the body is empty or unbalanced.
fn extract_fn_body(code: &str) -> Option<String> {
    let open = code.find('{')?;
    let close = code.rfind('}')?;
    if close <= open { return None; }
    let body = code[open + 1..close].trim_matches('\n').trim_end();
    if body.is_empty() { None } else { Some(body.to_string()) }
}

fn generate_jab_scripts(populated_jabs: usize) -> String {
    let base = crate::mappings::require_template(
        "character.jab.chain_helpers",
        &crate::mappings::script_templates().character.jab.chain_helpers,
    ).to_string();
    if populated_jabs < 4 {
        return base; // 1-3 hit jabs: template chain (jab1→jab2→jab3→idle) is exact.
    }
    // 4-hit jab: extend the chain — jab3 chains to jab4 on a re-press, and jab4
    // returns to idle. Same shape as the jab1/jab2 links, with CState.JAB4.
    let jab3_idle = "function jab3_end() {\n\tentity.playCState(CState.IDLE);\n}";
    let jab3_chain = "function jab3_end() {\n\
\tif (entity.checkInput(ControlsObject.ATTACK)) {\n\
\t\tentity.setAnimation(\"jab4\");\n\
\t\tentity.playCState(CState.JAB4);\n\
\t} else {\n\
\t\tentity.playCState(CState.IDLE);\n\
\t}\n}\n\n\
function jab4_end() {\n\
\tentity.playCState(CState.IDLE);\n}";
    if base.contains(jab3_idle) {
        base.replace(jab3_idle, jab3_chain)
    } else {
        // Template shape changed — append the jab4 link defensively.
        format!("{}\n\nfunction jab4_end() {{\n\tentity.playCState(CState.IDLE);\n}}\n", base)
    }
}

// ─── manifest.json ───────────────────────────────────────────────────────────

fn generate_manifest(char_id: &str, display_name: &str, projectile_names: &[String]) -> String {
    let ai_id   = format!("{}Ai", char_id);
    let ai_script_id = format!("{}AiScript", char_id);

    let mut content = vec![serde_json::json!({
            "id": char_id,
            "name": display_name,
            "description": format!("{} — converted from Super Smash Flash 2", display_name),
            "type": "character",
            "objectStatsId":    format!("{}CharacterStats", char_id),
            "animationStatsId": format!("{}AnimationStats", char_id),
            "hitboxStatsId":    format!("{}HitboxStats", char_id),
            "scriptId":         format!("{}Script", char_id),
            "costumesId":       format!("{}Costumes", char_id),
            "aiId":             ai_id,
            "metadata": {
                "ui": {
                    "entityId": "menu",
                    "render": {
                        "animation":               "full",
                        "animation_icon":          "icon",
                        "animation_icon_no_palette": "icon_no_palette",
                        "x_offset":       0,
                        "y_offset":       38,
                        "x_offset_door":  0,
                        "y_offset_door":  0,
                        "x_offset_door_ffa": 0,
                        "y_offset_door_ffa": 0
                    },
                    "hud": {
                        "animation":              "hud",
                        "animation_front":        "hud_front",
                        "animation_happy":        "hud_happy",
                        "animation_happy_front":  "hud_happy_front",
                        "animation_sad":          "hud_sad",
                        "animation_sad_front":    "hud_sad_front",
                        "animation_angry":        "hud_angry",
                        "animation_angry_front":  "hud_angry_front",
                        "animation_hurt":         "hud_hurt",
                        "animation_hurt_front":   "hud_hurt_front",
                        "animation_stock_icon":   "stock"
                    },
                    "css": {
                        "animation": "css",
                        "info": {
                            "game": "Super Smash Flash 2",
                            "description": format!("{} — ported from SSF2", display_name)
                        }
                    }
                }
            }
    })];  // close vec![json!({...})]

    content.push(serde_json::json!({
        "id":       ai_id,
        "type":     "characterAi",
        "scriptId": ai_script_id
    }));

    // Add projectile entries
    for proj_name in projectile_names {
        let entity_id = proj_name.replace('_', "");
        content.push(serde_json::json!({
            "id":               crate::projectile_gen::projectile_content_id(proj_name),
            "type":             "projectile",
            "objectStatsId":    format!("{}ProjectileStats", entity_id),
            "animationStatsId": format!("{}ProjectileAnimationStats", entity_id),
            "hitboxStatsId":    format!("{}ProjectileHitboxStats", entity_id),
            "scriptId":         format!("{}ProjectileScript", entity_id),
            "costumesId":       format!("{}Costumes", char_id)
        }));
    }

    serde_json::json!({
        "resourceId": char_id,
        "content": content
    }).to_string()
}

/// Multi-character project manifest. Mirrors `generate_manifest`'s
/// per-character content shape but emits one entry per character in
/// the project, plus per-character AI entries, plus per-character
/// projectile entries. Used by `main::finalize_multi_char_project`.
pub fn generate_multi_char_manifest(
    project_id: &str,
    chars: &[crate::project::ManifestCharEntry],
) -> String {
    let mut content: Vec<serde_json::Value> = Vec::new();
    // Projectiles are discovered from the one shared SWF, so multiple sub-characters
    // can list the same one. Each projectile content id must appear ONCE in the manifest.
    let mut seen_proj: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for entry in chars {
        let char_id = &entry.char_id;
        let display_name = &entry.display_name;
        let ai_id        = format!("{}Ai", char_id);
        let ai_script_id = format!("{}AiScript", char_id);

        content.push(serde_json::json!({
            "id": char_id,
            "name": display_name,
            "description": format!("{} — converted from Super Smash Flash 2", display_name),
            "type": "character",
            "objectStatsId":    format!("{}CharacterStats", char_id),
            "animationStatsId": format!("{}AnimationStats", char_id),
            "hitboxStatsId":    format!("{}HitboxStats", char_id),
            "scriptId":         format!("{}Script", char_id),
            "costumesId":       format!("{}Costumes", char_id),
            "aiId":             ai_id.clone(),
            "metadata": {
                "ui": {
                    "entityId": entry.menu_entity_id,
                    "render": {
                        "animation":               "full",
                        "animation_icon":          "icon",
                        "animation_icon_no_palette": "icon_no_palette",
                        "x_offset":       0,
                        "y_offset":       38,
                        "x_offset_door":  0,
                        "y_offset_door":  0,
                        "x_offset_door_ffa": 0,
                        "y_offset_door_ffa": 0
                    },
                    "hud": {
                        "animation":              "hud",
                        "animation_front":        "hud_front",
                        "animation_happy":        "hud_happy",
                        "animation_happy_front":  "hud_happy_front",
                        "animation_sad":          "hud_sad",
                        "animation_sad_front":    "hud_sad_front",
                        "animation_angry":        "hud_angry",
                        "animation_angry_front":  "hud_angry_front",
                        "animation_hurt":         "hud_hurt",
                        "animation_hurt_front":   "hud_hurt_front",
                        "animation_stock_icon":   "stock"
                    },
                    "css": {
                        "animation": "css",
                        "info": {
                            "game": "Super Smash Flash 2",
                            "description": format!("{} — ported from SSF2", display_name)
                        }
                    }
                }
            }
        }));
        content.push(serde_json::json!({
            "id":       ai_id,
            "type":     "characterAi",
            "scriptId": ai_script_id
        }));
        for proj_name in &entry.projectile_names {
            let content_id = crate::projectile_gen::projectile_content_id(proj_name);
            if !seen_proj.insert(content_id.clone()) { continue; } // already listed by an earlier char
            let entity_id = proj_name.replace('_', "");
            content.push(serde_json::json!({
                "id":               content_id,
                "type":             "projectile",
                "objectStatsId":    format!("{}ProjectileStats", entity_id),
                "animationStatsId": format!("{}ProjectileAnimationStats", entity_id),
                "hitboxStatsId":    format!("{}ProjectileHitboxStats", entity_id),
                "scriptId":         format!("{}ProjectileScript", entity_id),
                "costumesId":       format!("{}Costumes", char_id)
            }));
        }
    }

    serde_json::json!({
        "resourceId": project_id,
        "content": content
    }).to_string()
}

/// Public alias for the manifest .meta sidecar generator, so
/// `main::finalize_multi_char_project` can call it without leaking the
/// private name.
pub fn generate_manifest_meta_pub(guid: &str) -> String {
    generate_manifest_meta(guid)
}

// ─── Sound content entries ────────────────────────────────────────────────────

/// Write a `.wav.meta` sidecar next to each extracted audio file, matching
/// the schema observed in `Fraymakers/character-template` (id = filename
/// sans `.wav`, plus pluginMetadata + plugins references). No central audio
/// manifest is needed: reference characters register sounds purely through
/// these per-file sidecars.
fn generate_sound_entries(
    audio_dir: &Path,
    char_name: &str,
    sounds: &[crate::sound_extractor::SoundEntry],
) -> Result<()> {
    fs::create_dir_all(audio_dir)?;

    for s in sounds {
        let safe_name: String = s.name.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
            .collect();
        let wav_path = audio_dir.join(format!("{}.wav", safe_name));
        if !wav_path.exists() { continue; }

        // content id = filename sans .wav — referenced from CharacterStats
        // voice arrays and from AudioClip.play(self.getResource().getContent(<id>))
        let guid = det_uuid(&format!("{}::sound_meta_{}", char_name, safe_name));
        let meta = serde_json::json!({
            "export": true,
            "guid":   guid,
            "id":     safe_name,
            "pluginMetadata": {},
            "plugins": ["com.fraymakers.FraymakersMetadata"],
            "tags":    [],
            "version": 1
        });
        fs::write(
            audio_dir.join(format!("{}.wav.meta", safe_name)),
            serde_json::to_string_pretty(&meta)?,
        )?;
    }

    // Silent placeholder asset. `rewrite_play_sound_calls` points SSF2 sounds
    // that have no extracted asset and no GlobalSfx match at this id (with a
    // visible TODO), so the AudioClip.play() call is valid (plays silence)
    // and the modder can swap in a real sound.
    let ph = crate::api_mappings::PLACEHOLDER_SOUND_ID;
    fs::write(audio_dir.join(format!("{ph}.wav")), silent_wav())?;
    let ph_guid = det_uuid(&format!("{}::sound_meta_{}", char_name, ph));
    let ph_meta = serde_json::json!({
        "export": true,
        "guid":   ph_guid,
        "id":     ph,
        "pluginMetadata": {},
        "plugins": ["com.fraymakers.FraymakersMetadata"],
        "tags":    [],
        "version": 1
    });
    fs::write(
        audio_dir.join(format!("{ph}.wav.meta")),
        serde_json::to_string_pretty(&ph_meta)?,
    )?;

    Ok(())
}

/// A minimal valid silent WAV (8 kHz, mono, 16-bit PCM, ~50 ms of silence)
/// used as the placeholder audio asset for unmapped SSF2 sounds.
fn silent_wav() -> Vec<u8> {
    let sample_rate: u32 = 8000;
    let num_samples: u32 = sample_rate / 20; // ~50 ms
    let data_size: u32 = num_samples * 2;    // 16-bit mono
    let mut w = Vec::with_capacity(44 + data_size as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_size).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());      // fmt chunk size
    w.extend_from_slice(&1u16.to_le_bytes());        // PCM
    w.extend_from_slice(&1u16.to_le_bytes());        // mono
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes());        // block align
    w.extend_from_slice(&16u16.to_le_bytes());       // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_size.to_le_bytes());
    w.resize(44 + data_size as usize, 0);            // silence
    w
}

/// Convert a projectile name to a valid entity filename.
/// "mario_fireball" → "mario_fireball"
pub(crate) fn sanitize_entity_name(name: &str) -> String {
    name.replace(|c: char| !c.is_alphanumeric() && c != '_' && c != '-', "_")
}

/// Convert an SSF2 projectile name like `dee_nspec` to the PascalCase
/// prefix used for the script-file names (`DeeNspec` → `DeeNspecScript.hx`).
/// Matches the convention seen in real FM character mods
/// (aJewelofRarity/AnnieCharacter: `Cut` → `CutScript.hx`).
pub(crate) fn snake_to_pascal(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut capitalize_next = true;
    for c in name.chars() {
        if c == '_' || c == '-' {
            capitalize_next = true;
        } else if capitalize_next {
            out.extend(c.to_uppercase());
            capitalize_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

// ─── Projectile script generators ─────────────────────────────────────────────

/// Generate `library/manifest.json.meta` — the JSON sidecar that pairs
/// `manifest.json` with its content id (`"manifest"`) and language hint.
/// Schema cross-referenced against `Fraymakers/character-template`.
fn generate_manifest_meta(guid: &str) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "export": true,
        "guid": guid,
        "id": "manifest",
        "language": "json",
        "pluginMetadata": {
            "com.fraymakers.FraymakersMetadata": { "version": "0.1.0" }
        },
        "plugins": [],
        "tags": [],
        "version": 1
    })).unwrap_or_default()
}

/// Which kind of `.hx.meta` sidecar to emit. The choice determines the
/// `language`, `pluginMetadata`, and `plugins` fields — values are taken
/// verbatim from the Fraymakers/character-template reference repo so that
/// FrayTools recognises our exports the same way it does first-party ones.
#[derive(Copy, Clone)]
pub enum ScriptMetaKind {
    /// Character `Script.hx.meta` — objectType=CHARACTER + plugin listed.
    CharacterScript,
    /// `CharacterStats.hx.meta` — plugin listed, plugin version 0.4.0.
    CharacterStats,
    /// `AnimationStats.hx.meta` — pluginMetadata version only, no plugin.
    CharacterAnimationStats,
    /// `HitboxStats.hx.meta` — pluginMetadata version only, no plugin.
    CharacterHitboxStats,
    /// Projectile `ProjectileScript.hx.meta` — objectType=PROJECTILE + plugin.
    ProjectileScript,
    /// Projectile companion stats files — empty pluginMetadata, no plugin.
    ProjectileStats,
    /// Projectile companion stats files — empty pluginMetadata, no plugin.
    ProjectileAnimationStats,
    /// Projectile companion stats files — empty pluginMetadata, no plugin.
    ProjectileHitboxStats,
}

/// Generate a `.hx.meta` sidecar matching the Fraymakers reference layout.
/// All script-kind .meta files use `language: "hscript"`. The
/// `pluginMetadata` shape and `plugins` array vary by kind — see the enum
/// docs. Cross-referenced against Fraymakers/character-template.
pub(crate) fn script_meta(id: &str, guid: &str, kind: ScriptMetaKind) -> String {
    use ScriptMetaKind::*;
    // `plugin_meta` always contains a `com.fraymakers.FraymakersMetadata`
    // entry except for the projectile companion stats files, which use an
    // empty object in the reference.
    let plugin_meta = match kind {
        CharacterScript => serde_json::json!({
            "com.fraymakers.FraymakersMetadata": {
                "objectType": "CHARACTER",
                "version": "0.3.0"
            }
        }),
        ProjectileScript => serde_json::json!({
            "com.fraymakers.FraymakersMetadata": {
                "objectType": "PROJECTILE",
                "version": "0.1.1"
            }
        }),
        CharacterStats => serde_json::json!({
            "com.fraymakers.FraymakersMetadata": { "version": "0.4.0" }
        }),
        CharacterAnimationStats | CharacterHitboxStats => serde_json::json!({
            "com.fraymakers.FraymakersMetadata": { "version": "0.1.0" }
        }),
        ProjectileStats | ProjectileAnimationStats | ProjectileHitboxStats => {
            serde_json::json!({})
        }
    };
    // `plugins` is listed only on the files the reference flags it on: the
    // character + projectile Script.hx.meta and CharacterStats.hx.meta.
    let plugins: Vec<&str> = match kind {
        CharacterScript | ProjectileScript | CharacterStats => {
            vec!["com.fraymakers.FraymakersMetadata"]
        }
        _ => vec![],
    };
    serde_json::to_string_pretty(&serde_json::json!({
        "export": true,
        "guid": guid,
        "id": id,
        "language": "hscript",
        "pluginMetadata": plugin_meta,
        "plugins": plugins,
        "tags": [],
        "version": 1
    })).unwrap_or_default()
}

/// ProjectileScript.hx — handles lifecycle events.
/// Multi-state projectiles get extra PState constants and state-switching logic.
/// Synthesize a projectile's launch (X_SPEED, Y_SPEED) from the SSF2
/// `getProjectileStats()` physics. SSF2 projectiles carry no behavior script —
/// the engine drives them from these stats — so we derive the launch from the
/// best available speed source and apply the SAME velocity_scale the character
/// physics uses (the one `size_multiplier` knob in stats.jsonc + the 30->60fps
/// ratio). Returns (x, y, note).
fn synth_projectile_launch(ssf2_match: Option<&crate::abc_parser::ProjectileData>) -> (f64, f64, String) {
    let vs = crate::mappings::character_stats().scaling.velocity_scale();
    let stats = match ssf2_match {
        Some(d) if !d.stats.is_empty() => &d.stats,
        // No SSF2 data at all: fall back to the historical template default.
        _ => return (8.0, 0.0, "no SSF2 speed data — default launch (verify live).".to_string()),
    };
    let pick = |keys: &[&str]| keys.iter().find_map(|k| stats.get(*k).copied());
    // Explicit launch speed if SSF2 provides one; otherwise the speed cap is the
    // best proxy (the fireball-class projectiles launch at / accelerate to cap).
    let (x_raw, x_src) = pick(&["xSpeed", "x_speed", "norm_xSpeed"]).map(|v| (v, "xSpeed"))
        .or_else(|| pick(&["groundSpeedCap", "ground_speed_cap", "aerialSpeedCap", "aerial_speed_cap"]).map(|v| (v, "speedCap")))
        .unwrap_or((8.0, "default"));
    let y_raw = pick(&["ySpeed", "y_speed"]).unwrap_or(0.0);
    let note = format!(
        "X from SSF2 {}={} * velocity_scale {:.3}; bounce/lifetime not in SSF2 char data — verify live.",
        x_src, fmt_num(x_raw), vs
    );
    (x_raw * vs, y_raw * vs, note)
}

pub(crate) fn generate_projectile_script(
    _char_id: &str,
    entity_id: &str,
    extra_states: &[entity_gen::ProjectileStateData],
    ssf2_match: Option<&crate::abc_parser::ProjectileData>,
) -> String {
    let t = &crate::mappings::script_templates().projectile.script;
    let req = crate::mappings::require_template;
    let (x_speed, y_speed, speed_note) = synth_projectile_launch(ssf2_match);
    if extra_states.is_empty() {
        // Single-state: standard template
        req("projectile.script.single_state", &t.single_state)
            .replace("{{entity_id}}", entity_id)
            .replace("{{x_speed}}", &fmt_num(x_speed))
            .replace("{{y_speed}}", &fmt_num(y_speed))
            .replace("{{speed_note}}", &speed_note)
    } else {
        // Multi-state: use Fraymakers local state machine instead of fake PStates
        // Each SSF2 frame label becomes an LState that drives animation switching.
        let mut lstate_prep = String::from(req("projectile.script.lstate_idle_prep", &t.lstate_idle_prep));
        let mut update_branches = String::new();
        let line_tpl = req("projectile.script.lstate_prep_line", &t.lstate_prep_line);
        let branch_tpl = req("projectile.script.update_branch", &t.update_branch);
        for state in extra_states {
            let fm = entity_gen::ssf2_proj_label_to_fm_anim(&state.label);
            let lname = match state.label.as_str() {
                "attack_hold" => "HELD",
                "attack_toss" => "ACTIVE",
                _ => "CUSTOM",
            };
            lstate_prep.push_str(&line_tpl.replace("{{lstate_name}}", lname).replace("{{fm_anim}}", &fm));
            update_branches.push_str(&branch_tpl
                .replace("{{lstate_name}}", lname)
                .replace("{{frame_count}}", &state.frame_count.to_string()));
        }
        req("projectile.script.multi_state", &t.multi_state)
            .replace("{{entity_id}}", entity_id)
            .replace("{{lstate_prep}}", &lstate_prep)
            .replace("{{update_branches}}", &update_branches)
            .replace("{{x_speed}}", &fmt_num(x_speed))
            .replace("{{y_speed}}", &fmt_num(y_speed))
            .replace("{{speed_note}}", &speed_note)
    }
}

/// ProjectileAnimationStats.hx — endType for each projectile animation.
/// Three anchor animation names used across a projectile's stats files.
/// When SSF2's inner sprite carries its own FrameLabel tags, those literal
/// labels become the names; otherwise the converter falls back to FM's
/// template trio (`projectileSpawn` / `projectileIdle` / `projectileDestroy`).
/// This is the projectile-side parallel to character xframe-label
/// extraction the user explicitly asked for.
struct ProjectileAnims {
    active:  String,
    destroy: String,
    /// All distinct animation names referenced by this projectile, in
    /// timeline order. Includes spawn/active/destroy plus any extra
    /// labels from `inner_labels` or multi-state `extra_states`.
    all: Vec<String>,
}

fn projectile_anim_names(proj: &entity_gen::ProjectileInfo) -> ProjectileAnims {
    // Sanitize a raw SSF2 label into a valid Haxe identifier.
    fn ident(s: &str) -> String {
        let cleaned: String = s.chars()
            .map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' })
            .collect();
        // Disallow leading digit.
        if cleaned.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
            format!("_{}", cleaned)
        } else if cleaned.is_empty() {
            "projectile".to_string()
        } else {
            cleaned
        }
    }

    // Prefer real SSF2 labels when present, ordered by frame.
    let mut sorted: Vec<&(u16, String)> = proj.inner_labels.iter().collect();
    sorted.sort_by_key(|(f, _)| *f);
    let raw: Vec<String> = sorted.into_iter().map(|(_, l)| ident(l)).collect();

    let (spawn, active, destroy) = match raw.len() {
        0 => (
            "projectileSpawn".to_string(),
            "projectileIdle".to_string(),
            "projectileDestroy".to_string(),
        ),
        1 => {
            // Single label — re-use as the active animation; synthesize
            // spawn/destroy variants from it. Matches the Annie
            // convention of using one logical animation across states.
            let only = raw[0].clone();
            (only.clone(), only.clone(), only)
        }
        2 => (raw[0].clone(), raw[0].clone(), raw[1].clone()),
        _ => (raw[0].clone(), raw[1].clone(), raw[raw.len() - 1].clone()),
    };

    // Build the union of all referenced names + any multi-state labels.
    let mut all: Vec<String> = vec![spawn.clone(), active.clone(), destroy.clone()];
    for s in &proj.extra_states {
        let n = entity_gen::ssf2_proj_label_to_fm_anim(&s.label);
        if !all.contains(&n) { all.push(n); }
    }
    for r in &raw {
        if !all.contains(r) { all.push(r.clone()); }
    }
    // De-dup while preserving first-seen order.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    all.retain(|n| seen.insert(n.clone()));

    ProjectileAnims { active, destroy, all }
}

pub(crate) fn generate_projectile_animation_stats(proj: &entity_gen::ProjectileInfo) -> String {
    let t = &crate::mappings::script_templates().projectile.stats;
    let req = crate::mappings::require_template;
    let anims = projectile_anim_names(proj);
    let mut lines: Vec<String> = Vec::new();
    for (i, n) in anims.all.iter().enumerate() {
        // Mark the destroy animation with resetId:false to prevent
        // hit-id churn on state change (per template convention).
        if n == &anims.destroy && i > 0 {
            lines.push(req("projectile.stats.proj_anim_entry_destroy", &t.proj_anim_entry_destroy)
                .replace("{{name}}", n));
        } else {
            lines.push(req("projectile.stats.proj_anim_entry_normal", &t.proj_anim_entry_normal)
                .replace("{{name}}", n));
        }
    }
    req("projectile.stats.proj_anim_wrapper", &t.proj_anim_wrapper)
        .replace("{{body}}", &lines.join(",\n"))
}

/// ProjectileStats.hx — physics, geometry, and state → animation mapping.
/// Heuristic match between a discovered projectile sprite (e.g.
/// `"dee_nspec"`) and SSF2's per-attack `getProjectileStats()` entries
/// (keyed by attack name like `"b"`, `"b_up"`, `"final_smash"`, etc.).
///
/// Strategy:
/// 1. Exact name match (`dee_nspec` == `dee_nspec`) wins.
/// 2. Substring match on common projectile name fragments:
///       *nspec*       → `b`
///       *fspec/sspec* → `b_forward`
///       *uspec*       → `b_up`
///       *dspec*       → `b_down`
///       *fs*/*finalsmash* → `final_smash` / any key containing "final"
/// 3. Otherwise, pick the entry with the most populated data
///    (stats.len() + hitboxes.len()) — best-effort.
/// 4. None if the projectile_data map is empty.
///
/// Result is best-effort; callers should mark output with a TODO so the
/// modder verifies the mapping per call site.
pub(crate) fn best_match_projectile_data<'a>(
    proj_name: &str,
    projectile_data: &'a std::collections::BTreeMap<String, crate::abc_parser::ProjectileData>,
) -> Option<&'a crate::abc_parser::ProjectileData> {
    if projectile_data.is_empty() { return None; }
    if let Some(exact) = projectile_data.get(proj_name) { return Some(exact); }
    let lower = proj_name.to_lowercase();
    let candidate_keys: &[&str] = if lower.contains("nspec") || lower.contains("nspecial") {
        &["b", "nspec", "neutral_special"]
    } else if lower.contains("fspec") || lower.contains("sspec") || lower.contains("sspecial") {
        &["b_forward", "fspec", "sspec", "side_special"]
    } else if lower.contains("uspec") || lower.contains("uspecial") {
        &["b_up", "uspec", "up_special"]
    } else if lower.contains("dspec") || lower.contains("dspecial") {
        &["b_down", "dspec", "down_special"]
    } else if lower.contains("finalsmash") || lower.contains("_fs") || lower.ends_with("fs") {
        &["final_smash", "finalsmash", "fs"]
    } else {
        &[]
    };
    for k in candidate_keys {
        if let Some(d) = projectile_data.get(*k) { return Some(d); }
        // Fuzzy: any key containing the candidate
        if let Some((_, d)) = projectile_data.iter().find(|(name, _)| name.contains(*k)) {
            return Some(d);
        }
    }
    // Fallback: pick the entry with the most populated data.
    projectile_data.values()
        .max_by_key(|d| d.stats.len() + d.hitboxes.len())
}

pub(crate) fn generate_projectile_stats(
    _char_id: &str,
    entity_id: &str,
    proj: &entity_gen::ProjectileInfo,
    ssf2_match: Option<&crate::abc_parser::ProjectileData>,
) -> String {
    let t = &crate::mappings::script_templates().projectile.stats;
    let req = crate::mappings::require_template;
    let content_id = crate::projectile_gen::projectile_content_id(entity_id);
    let anims = projectile_anim_names(proj);

    // SSF2 physics field name → FM ProjectileStats field name. The set
    // is small and most names align; this table handles the few that
    // need renaming. Unknown SSF2 keys are emitted as `// TODO: …`.
    // SSF2 physics key → FM field map + scaffolding defaults (mappings/projectile_tables.jsonc).
    let ptables = crate::mappings::projectile_tables();

    // Build the physics lines from SSF2 data where available.
    let (mut physics_lines, mut todo_lines, source_note): (Vec<String>, Vec<String>, String) =
        match ssf2_match {
            Some(d) if !d.stats.is_empty() => {
                let mut taken: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
                let mut out: Vec<String> = Vec::new();
                for pm in &ptables.physics_map {
                    if taken.contains(pm.fm.as_str()) { continue; }
                    if let Some(v) = d.stats.get(pm.ssf2.as_str()) {
                        out.push(req("projectile.stats.proj_stats_physics_line", &t.proj_stats_physics_line)
                            .replace("{{field}}", &pm.fm).replace("{{value}}", &fmt_num(*v)));
                        taken.insert(pm.fm.as_str());
                    }
                }
                // Surface any SSF2 stat keys we didn't map as TODOs
                let mut todos: Vec<String> = Vec::new();
                let mapped_ssf2: std::collections::BTreeSet<&str> = ptables.physics_map.iter()
                    .filter(|pm| taken.contains(pm.fm.as_str()))
                    .map(|pm| pm.ssf2.as_str())
                    .collect();
                for (k, v) in &d.stats {
                    if !mapped_ssf2.contains(k.as_str()) {
                        todos.push(req("projectile.stats.proj_stats_todo_line", &t.proj_stats_todo_line)
                            .replace("{{key}}", k).replace("{{value}}", &fmt_num(*v)));
                    }
                }
                (out, todos, req("projectile.stats.proj_stats_source_matched", &t.proj_stats_source_matched).to_string())
            }
            _ => (vec![], vec![], req("projectile.stats.proj_stats_source_default", &t.proj_stats_source_default).to_string()),
        };

    // Always emit the scaffolding defaults for fields SSF2 didn't supply.
    let provided: std::collections::BTreeSet<String> = physics_lines.iter()
        .filter_map(|l| l.split(':').next().map(|s| s.trim().to_string()))
        .collect();
    for fv in &ptables.scaffolding {
        if !provided.contains(&fv.field) {
            physics_lines.push(req("projectile.stats.proj_stats_physics_line", &t.proj_stats_physics_line)
                .replace("{{field}}", &fv.field).replace("{{value}}", &fv.value));
        }
    }
    physics_lines.sort();
    todo_lines.sort();

    req("projectile.stats.proj_stats_body", &t.proj_stats_body)
        .replace("{{entity_id}}", entity_id)
        .replace("{{source_note}}", &source_note)
        .replace("{{content_id}}", &content_id)
        .replace("{{active}}", &anims.active)
        .replace("{{destroy}}", &anims.destroy)
        .replace("{{physics}}", &physics_lines.join("\n"))
        .replace("{{todos}}", &if todo_lines.is_empty() { String::new() } else { todo_lines.join("\n") + "\n" })
}

/// ProjectileHitboxStats.hx — pulls real hitbox values from SSF2's
/// `getProjectileStats()` data when a match is found. Uses
/// `mappings/character/hitbox_stats.jsonc` as the canonical SSF2→FM
/// field-name table (same source the character HitboxStats.hx generator
/// uses — single source of truth, not duplicated).
pub(crate) fn generate_projectile_hitbox_stats(
    _char_id: &str,
    entity_id: &str,
    proj: &entity_gen::ProjectileInfo,
    ssf2_match: Option<&crate::abc_parser::ProjectileData>,
) -> String {
    let t = &crate::mappings::script_templates().projectile.stats;
    let req = crate::mappings::require_template;
    let mapping = crate::mappings::character_hitbox_stats();
    let anims = projectile_anim_names(proj);

    let (idle_body, source_note) = match ssf2_match.and_then(|d| d.hitboxes.first()) {
        Some(hb) => {
            // Build hitbox0 from SSF2 keys via the hitbox_stats.jsonc canon.
            let mut parts: Vec<String> = Vec::new();
            for field in &mapping.fields {
                // Take the MAX of all listed SSF2 keys (mirrors how
                // hitbox_stats.jsonc is used elsewhere). Absent keys count as 0.
                let mut best: Option<f64> = None;
                for key in &field.ssf2_keys {
                    if let Some(v) = hb.get(key) {
                        best = Some(best.map_or(*v, |b: f64| b.max(*v)));
                    }
                }
                if let Some(mut v) = best {
                    if field.isframe { v *= 2.0; } // 30→60fps
                    parts.push(format!("{}: {}", field.fm_field, fmt_num(v)));
                }
            }
            // Always emit the projectile-typical flags
            parts.push("reversibleAngle: true".to_string());
            parts.push("directionalInfluence: false".to_string());
            parts.push("reflectable: true".to_string());
            (
                req("projectile.stats.proj_hitbox_idle_matched", &t.proj_hitbox_idle_matched)
                    .replace("{{parts}}", &parts.join(", ")),
                req("projectile.stats.proj_hitbox_source_matched", &t.proj_hitbox_source_matched).to_string(),
            )
        }
        None => (
            req("projectile.stats.proj_hitbox_idle_default", &t.proj_hitbox_idle_default).to_string(),
            req("projectile.stats.proj_hitbox_source_default", &t.proj_hitbox_source_default).to_string(),
        ),
    };

    // Build the projection-by-animation block dynamically: all known
    // projectile animations get an entry; the hitbox payload lives on
    // the "active" animation (matches FM convention — hitboxes are
    // attached to the animation that's playing while the projectile is
    // alive). Spawn/destroy animations stay empty unless the SSF2 source
    // explicitly carried hitbox data for them, which we don't currently
    // surface (per-animation hitboxes is a follow-up).
    let mut anim_blocks: Vec<String> = Vec::new();
    for name in &anims.all {
        if name == &anims.active {
            anim_blocks.push(req("projectile.stats.proj_hitbox_active_block", &t.proj_hitbox_active_block)
                .replace("{{name}}", name).replace("{{idle_body}}", &idle_body));
        } else {
            anim_blocks.push(req("projectile.stats.proj_hitbox_empty_block", &t.proj_hitbox_empty_block)
                .replace("{{name}}", name));
        }
    }
    let body = req("projectile.stats.proj_hitbox_body", &t.proj_hitbox_body)
        .replace("{{entity_id}}", entity_id)
        .replace("{{source_note}}", &source_note)
        .replace("{{blocks}}", &anim_blocks.join(",\n"));
    let body = strip_trailing_commas(&body);
    assert_balanced_stats(&body, &format!("ProjectileHitboxStats for {entity_id}"));
    body
}

/// Format a float like SSF2 would have written it: integers stay integers,
/// floats round to 3 decimal places without trailing zeros.
fn fmt_num(v: f64) -> String {
    if v.fract() == 0.0 { format!("{}", v as i64) }
    else {
        let s = format!("{:.3}", v);
        let s = s.trim_end_matches('0').trim_end_matches('.');
        s.to_string()
    }
}

#[cfg(test)]
mod hitbox_split_tests {
    use super::{base_attack_name, strip_trailing_commas, assert_balanced_stats};

    #[test]
    fn split_sub_anims_map_to_their_base_attack() {
        // jabN (N>=2) inherits jab1; strong _in/_charge inherit _attack.
        assert_eq!(base_attack_name("jab2").as_deref(), Some("jab1"));
        assert_eq!(base_attack_name("jab3").as_deref(), Some("jab1"));
        assert_eq!(base_attack_name("strong_forward_in").as_deref(), Some("strong_forward_attack"));
        assert_eq!(base_attack_name("strong_up_charge").as_deref(), Some("strong_up_attack"));
        assert_eq!(base_attack_name("strong_down_in").as_deref(), Some("strong_down_attack"));
        // Non-split / base names have no fallback.
        assert_eq!(base_attack_name("jab1"), None);
        assert_eq!(base_attack_name("tilt_forward"), None);
        assert_eq!(base_attack_name("dash_attack"), None);
        assert_eq!(base_attack_name("strong_forward_attack"), None);
    }

    #[test]
    fn strip_trailing_commas_removes_only_structural_commas() {
        let got = strip_trailing_commas("{ jab1: { hitbox0: { d: 1, a: 2 }, }, }");
        // the trailing commas before } are gone; the comma between fields stays
        assert_eq!(got, "{ jab1: { hitbox0: { d: 1, a: 2 } } }");
    }

    #[test]
    fn assert_balanced_stats_accepts_valid_and_ignores_comments() {
        // a half-commented block (the real bug) is still balanced once the open is in a comment
        // AND the body is commented too — the fixed form — so it passes.
        assert_balanced_stats("{\n\tjab1: { hitbox0: {} }\n\t// SSF2: x: {\n\t// SSF2: }\n}", "t");
    }

    #[test]
    #[should_panic(expected = "broken stats object")]
    fn assert_balanced_stats_rejects_the_half_commented_block_bug() {
        // the original bug: the open `x: {` is commented out but the body + close are ACTIVE,
        // leaving an extra `}` → unbalanced → must be caught.
        assert_balanced_stats("{\n\t// SSF2: x: {\n\t\thitbox0: {}\n\t}\n}", "t");
    }
}
