//! In-process conversion entry point.
//!
//! This is the library face of what used to be the `ssf2_converter` binary's
//! `main()`. The binary is gone — `peptide convert …` and the Peptide GUI both
//! call [`run_conversion`] directly, in-process. The orchestration and the
//! per-character pipeline that used to live in `src/main.rs` moved here verbatim.
//!
//! ## Concurrency
//! `run_conversion` is **not** safe to call concurrently. The conversion log is a
//! process-global `Mutex<ConversionLog>` (reset per character) and the extractors
//! use `thread_local!` caches. Callers driving conversions from a long-running GUI
//! must run each conversion on its own worker thread and never two at once.
//! Logging is via the `log` facade; the caller owns logger initialisation (the
//! library never calls `env_logger::init`).

use std::path::{Path, PathBuf};
use std::fs;

use anyhow::{Context, Result};

use crate::project::{ManifestCharEntry, MultiCharSlot, ProcessedCharacter};
use crate::{
    abc_parser, extractor, haxe_gen, image_extractor, mappings, sound_extractor, sprite_parser, ssf,
    swf_parser,
};

/// Inputs to a single conversion run. Mirrors the old CLI flags.
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// The `.ssf` (or raw SWF) to convert.
    pub input: PathBuf,
    /// Output directory for generated Fraymakers files (CLI default: `./characters`).
    pub output: PathBuf,
    /// Character-name override. For a multi-character `.ssf`, selects only that
    /// character. Auto-detected from the SWF when `None`.
    pub name: Option<String>,
    /// Explicit `misc.ssf` for costume palettes. Auto-detected next to the input
    /// when `None`.
    pub misc_ssf: Option<PathBuf>,
    /// Rollback flag: emit each character of a multi-character `.ssf` as its own
    /// project (the pre-Stage-B layout) instead of one unified project.
    pub per_character_projects: bool,
}

impl ConvertOptions {
    /// Construct with the CLI defaults (`output = ./characters`, everything else off).
    pub fn new(input: impl Into<PathBuf>) -> Self {
        ConvertOptions {
            input: input.into(),
            output: PathBuf::from("./characters"),
            name: None,
            misc_ssf: None,
            per_character_projects: false,
        }
    }
}

/// Result of a conversion run — what the UI / CLI report afterwards.
#[derive(Debug, Clone)]
pub struct ConversionSummary {
    /// The directory the project was written to (`output`, or `output/<project_id>`
    /// for a unified multi-character project).
    pub project_dir: PathBuf,
    /// Character ids processed.
    pub characters: Vec<String>,
    /// True when this run produced one unified multi-character project.
    pub multi_char: bool,
    /// The unified project's id, when `multi_char`.
    pub project_id: Option<String>,
    /// Every `.fraytools` project file produced by this run.
    pub fraytools_files: Vec<PathBuf>,
    /// Per-character conversion-log blocks (multi-char only; single-char writes
    /// its own `conversion_log.json` inline).
    pub log_blocks: Vec<serde_json::Value>,
    /// Tier-1 validation warnings collected across all characters (also logged).
    pub warnings: Vec<String>,
}

/// Convert one `.ssf` into a Fraymakers character package (or a unified
/// multi-character project). In-process; see the module docs for the concurrency
/// contract. The caller initialises logging.
pub fn run_conversion(opts: ConvertOptions) -> Result<ConversionSummary> {
    log::info!("SSF2 → Fraymakers Converter");
    log::info!("Input: {}", opts.input.display());

    // Read + decompress SSF
    let ssf_data = fs::read(&opts.input).with_context(|| format!("read {}", opts.input.display()))?;
    log::info!("Loaded {} bytes", ssf_data.len());
    let swf_data = ssf::decompress(&ssf_data)?;
    log::info!("Decompressed SWF: {} bytes", swf_data.len());

    let swf = swf_parser::parse(&swf_data)?;
    log::info!("Parsed SWF: v{}, {} ABC blocks", swf.version, swf.abc_blocks.len());

    // Parse the SWF tag tree once for the whole file. The tags (sprites, images,
    // sounds) are identical across every character in a multi-char .ssf, so the
    // per-character extractors all borrow this one parse instead of re-parsing.
    let parsed_swf_buf = swf::decompress_swf(&swf_data[..])
        .map_err(|e| anyhow::anyhow!("decompress SWF: {}", e))?;
    let parsed_swf = swf::parse_swf(&parsed_swf_buf)
        .map_err(|e| anyhow::anyhow!("parse SWF: {}", e))?;

    // ── Palette extraction from misc.ssf ──────────────────────────────────────
    // Explicit --misc-ssf, else misc.ssf next to the input. Extract all costume
    // data in-process to a temp JSON cache, deleted when done.
    let costumes_path: Option<PathBuf> = opts.misc_ssf.clone().and_then(|p| {
        match extract_costumes_to_temp(&p) {
            Ok(path) => Some(path),
            Err(e) => {
                log::warn!("Costume extraction from {:?} failed: {}", p, e);
                None
            }
        }
    }).or_else(|| {
        let misc_ssf = opts.input.parent()?.join("misc.ssf");
        if !misc_ssf.exists() {
            log::info!("misc.ssf not found next to input — skipping palette extraction");
            return None;
        }
        log::info!("Found misc.ssf — extracting costume palettes...");
        match extract_costumes_to_temp(&misc_ssf) {
            Ok(path) => {
                log::info!("Costume palettes cached to {}", path.display());
                Some(path)
            }
            Err(e) => {
                log::warn!("Costume extraction failed: {} — palettes will use k-means fallback", e);
                None
            }
        }
    });
    let costumes_is_temp = costumes_path.is_some();

    // Determine which character names to process
    let char_names: Vec<String> = if let Some(name) = opts.name.clone() {
        vec![name]
    } else {
        let detected = detect_char_names(&swf, &opts.input);
        if detected.is_empty() {
            let stem = opts.input.file_stem().and_then(|s| s.to_str()).unwrap_or("character");
            // SSF2 `DAT<n>.ssf` archives are index-named (stages, items, UI, …), so a
            // stem-as-character fallback would mint a junk "DAT10" pseudo-character for
            // every non-character archive. Bail clearly instead — the stem fallback is
            // only meaningful for a hand-named single-character .swf.
            if is_dat_archive_stem(stem) {
                anyhow::bail!(
                    "{} is a valid SSF2 DAT archive but contains no convertible character \
                     (likely a stage, item, or other asset). Pass --name <id> to force a \
                     specific character if you believe one is present.",
                    opts.input.display()
                );
            }
            vec![stem.to_string()]
        } else {
            detected
        }
    };

    log::info!("Characters to process: {:?}", char_names);

    // Per-SSF emission shape (docs/multi_character_projects_plan.md §2):
    //   * single-character SSF (or --per-character-projects): each char in its own
    //     characters/<id>/ project.
    //   * multi-character SSF in default mode: one characters/<project_id>/ project
    //     with all characters as peer entities.
    let multi_char_mode = char_names.len() > 1 && !opts.per_character_projects;
    let project_id: Option<String> = if multi_char_mode {
        swf.abc_blocks.iter()
            .filter_map(|b| abc_parser::parse(b).ok())
            .find_map(|abc| abc_parser::extract_main_package_metadata(&abc).and_then(|md| md.id))
            .or_else(|| opts.input.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
    } else { None };

    // Pre-compute each character's PascalCase form once.
    let all_pascals: Vec<String> = {
        let md = swf.abc_blocks.iter()
            .filter_map(|b| abc_parser::parse(b).ok())
            .find_map(|abc| abc_parser::extract_main_package_metadata(&abc));
        char_names.iter().map(|id| {
            md.as_ref()
                .and_then(|md| md.characters.iter().find(|(d, _)| d == id))
                .map(|(_, method)| abc_parser::pascal_form(method))
                .unwrap_or_else(|| abc_parser::pascal_form(id))
        }).collect()
    };

    let mut accumulated_logs: Vec<serde_json::Value> = Vec::new();
    let mut accumulated_manifest_chars: Vec<ManifestCharEntry> = Vec::new();
    let mut all_warnings: Vec<String> = Vec::new();
    let project_dir: PathBuf = match (&multi_char_mode, &project_id) {
        (true, Some(pid)) => opts.output.join(pid),
        _ => opts.output.clone(),
    };

    for (slot_idx, char_name) in char_names.iter().enumerate() {
        log::info!("─── Processing: {} ───", char_name);
        let slot = if multi_char_mode {
            Some(MultiCharSlot {
                project_dir: project_dir.clone(),
                slot_idx,
                pascals: all_pascals.clone(),
                char_ids: char_names.clone(),
            })
        } else { None };
        match process_character(
            &parsed_swf, &swf, char_name, &opts.output, costumes_path.as_deref(),
            &opts.input, slot.as_ref(), &mut all_warnings,
        ) {
            Ok(Some(artifacts)) => {
                accumulated_manifest_chars.push(artifacts.manifest_entry);
                accumulated_logs.push(artifacts.log_block);
            }
            Ok(None) => { /* single-character; finalized inside process_character */ }
            Err(e) => {
                log::error!("Failed to process {}: {}", char_name, e);
                all_warnings.push(format!("failed to process {char_name}: {e}"));
            }
        }
    }

    // Multi-char: write the project-level manifest + .fraytools + log after all
    // characters are processed. (Single-char finalizes inside process_character.)
    if multi_char_mode {
        if let Err(e) = finalize_multi_char_project(
            &project_dir, project_id.as_deref().unwrap_or("project"),
            &accumulated_manifest_chars, &accumulated_logs, &opts.input,
        ) {
            log::error!("Failed to finalize multi-char project: {}", e);
            all_warnings.push(format!("failed to finalize multi-char project: {e}"));
        }
    }

    // Clean up temp costumes file if we created it
    if costumes_is_temp {
        if let Some(ref p) = costumes_path {
            if let Err(e) = fs::remove_file(p) {
                log::warn!("Failed to remove temp costumes file: {}", e);
            } else {
                log::info!("Removed temp costumes cache");
            }
        }
    }

    // #13: lint every emitted .hx for structural breakage (unbalanced delimiters,
    // dangling string, empty interpolation) so a generation bug surfaces in the
    // convert output as a SCRIPTERR line instead of silently dying in-engine. Scope
    // to only THIS run's output dirs (single-char output is the shared root, so
    // linting project_dir would re-flag stale unrelated characters).
    let lint_dirs: Vec<PathBuf> = if multi_char_mode {
        vec![project_dir.clone()]
    } else {
        char_names.iter().map(|c| opts.output.join(c)).collect()
    };
    for dir in &lint_dirs {
        for problem in lint_emitted_hscript(dir) {
            log::error!("{problem}");
            all_warnings.push(problem);
        }
    }

    // Collect the .fraytools files produced (for the UI's "open / publish" step).
    let fraytools_files = collect_fraytools_files(&project_dir, multi_char_mode, &char_names, &opts.output);

    Ok(ConversionSummary {
        project_dir,
        characters: char_names,
        multi_char: multi_char_mode,
        project_id,
        fraytools_files,
        log_blocks: accumulated_logs,
        warnings: all_warnings,
    })
}

/// Enumerate the `.fraytools` project files this run produced. Multi-char writes
/// one under `project_dir`; single-char writes one under each `output/<char>/`.
fn collect_fraytools_files(
    project_dir: &Path, multi_char: bool, char_names: &[String], output: &Path,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push_first_in = |dir: &Path| {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "fraytools").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    };
    if multi_char {
        push_first_in(project_dir);
    } else {
        for c in char_names {
            push_first_in(&output.join(c));
        }
    }
    out
}

/// #13: walk every emitted `.hx` under `dir` and structurally lint it, returning
/// one `SCRIPTERR:` line per problem. The engine executes generated hscript with a
/// try/catch that, by default, swallows parse/runtime failures silently — a
/// generation bug then reads in-engine as "the move just doesn't do anything." This
/// catches the realistic generation failures (unbalanced delimiters, an unterminated
/// string, an empty `${}`) at convert time so they surface in the session instead.
fn lint_emitted_hscript(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().map(|x| x == "hx").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    files.sort();
    let mut problems = Vec::new();
    for f in &files {
        let Ok(src) = fs::read_to_string(f) else { continue };
        let rel = f.strip_prefix(dir).unwrap_or(f).display();
        if let Some(msg) = hscript_structural_problem(&src) {
            problems.push(format!("SCRIPTERR: {rel}: {msg}"));
        }
    }
    problems
}

/// Lightweight hscript structural check: scan outside string literals and comments,
/// require balanced `(){}[]` and a terminated string, and reject empty `${}`
/// interpolation. Returns the first problem found, or None if the file looks sound.
/// NOT a full parser — it only catches the breakage our own emitter can produce.
fn hscript_structural_problem(src: &str) -> Option<String> {
    let b = src.as_bytes();
    let mut stack: Vec<(u8, usize)> = Vec::new(); // (closing char, byte offset of opener)
    let mut line = 1usize;
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == b'\n' {
            line += 1;
            i += 1;
            continue;
        }
        // comments
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                if b[i] == b'\n' {
                    line += 1;
                }
                i += 1;
            }
            i += 2;
            continue;
        }
        // string literals (" or '); ' supports ${...} interpolation
        if c == b'"' || c == b'\'' {
            let quote = c;
            let start_line = line;
            i += 1;
            let mut terminated = false;
            while i < b.len() {
                let d = b[i];
                if d == b'\\' {
                    i += 2;
                    continue;
                }
                if d == b'\n' {
                    line += 1;
                    i += 1;
                    continue;
                }
                if quote == b'\'' && d == b'$' && i + 1 < b.len() && b[i + 1] == b'{' {
                    if i + 2 < b.len() && b[i + 2] == b'}' {
                        return Some(format!("empty string interpolation `${{}}` near line {start_line}"));
                    }
                    // skip the interpolation braces so an inner } doesn't end scan early
                    let mut depth = 0i32;
                    i += 1; // at '{'
                    loop {
                        if i >= b.len() {
                            return Some(format!("unterminated `${{...}}` interpolation opened near line {start_line}"));
                        }
                        match b[i] {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    i += 1;
                                    break;
                                }
                            }
                            b'\n' => line += 1,
                            _ => {}
                        }
                        i += 1;
                    }
                    continue;
                }
                if d == quote {
                    terminated = true;
                    i += 1;
                    break;
                }
                i += 1;
            }
            if !terminated {
                return Some(format!("unterminated string literal opened near line {start_line}"));
            }
            continue;
        }
        match c {
            b'(' => stack.push((b')', line)),
            b'{' => stack.push((b'}', line)),
            b'[' => stack.push((b']', line)),
            b')' | b'}' | b']' => match stack.pop() {
                Some((expected, _)) if expected == c => {}
                Some((expected, opened)) => {
                    return Some(format!(
                        "mismatched delimiter: expected `{}` but found `{}` at line {line} (opener at line {opened})",
                        expected as char, c as char
                    ));
                }
                None => {
                    return Some(format!("stray closing `{}` at line {line}", c as char));
                }
            },
            _ => {}
        }
        i += 1;
    }
    if let Some((expected, opened)) = stack.last() {
        return Some(format!(
            "unbalanced delimiter: `{}` opened at line {opened} never closed",
            match expected {
                b')' => '(',
                b'}' => '{',
                _ => '[',
            }
        ));
    }
    None
}

/// Extract all costume palettes from misc.ssf in-process and write to a temp JSON
/// file. Returns the path to the temp file on success.
fn extract_costumes_to_temp(misc_ssf: &Path) -> Result<PathBuf> {
    let raw = fs::read(misc_ssf).context("read misc.ssf")?;
    let swf_data = ssf::decompress(&raw).context("decompress misc.ssf")?;
    let swf = swf_parser::parse(&swf_data).context("parse misc.ssf")?;

    let mut all_costumes: std::collections::BTreeMap<String, Vec<abc_parser::CostumeData>> =
        std::collections::BTreeMap::new();

    for abc_bytes in &swf.abc_blocks {
        let abc = abc_parser::parse(abc_bytes).context("parse ABC")?;
        let found = abc_parser::scan_all_costume_methods(&abc);
        for (char_name, costumes) in found {
            all_costumes.entry(char_name).or_default().extend(costumes);
        }
    }

    // Drop noise: unknown key, or fewer than 10 costumes
    all_costumes.retain(|k, v| k != "unknown" && v.len() >= 10);

    let json_val: serde_json::Value = all_costumes.iter().map(|(char_name, costumes)| {
        let arr: serde_json::Value = costumes.iter().map(|c| serde_json::json!({
            "name":         c.name,
            "colors":       c.colors,
            "replacements": c.replacements,
        })).collect::<Vec<_>>().into();
        (char_name.clone(), arr)
    }).collect::<serde_json::Map<_, _>>().into();

    let temp_path = misc_ssf.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(".ssf2_costumes_cache.json");
    fs::write(&temp_path, serde_json::to_string(&json_val)?).context("write costumes cache")?;

    log::info!("Extracted {} characters' costume data from misc.ssf", all_costumes.len());
    Ok(temp_path)
}

/// Derive the canonical character id from a `Main::get<X>()` method name: strip
/// `get`, lowercase the remainder, preserve explicit `_`.
fn derive_id_from_getter(method_name: &str) -> Option<String> {
    let stripped = method_name.strip_prefix("get")?;
    if stripped.is_empty() { return None; }
    Some(stripped.to_lowercase())
}

/// Detect all character names in a SWF (constructor walk, with a `Main::get*`
/// enumeration fallback). Empty for SWFs without a `Main` class.
/// True when a file stem looks like an SSF2 `DAT<n>` archive name (case-insensitive
/// `dat` followed by one or more digits), e.g. `DAT0`, `dat123`. Used to suppress
/// filename-based heuristics that assume the input was named after its character.
fn is_dat_archive_stem(stem: &str) -> bool {
    let lower = stem.to_lowercase();
    lower
        .strip_prefix("dat")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

fn detect_char_names(swf: &swf_parser::SwfFile, _input_path: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for abc_bytes in &swf.abc_blocks {
        let Ok(abc) = abc_parser::parse(abc_bytes) else { continue };

        // Primary: constructor walk.
        if let Some(md) = abc_parser::extract_main_package_metadata(&abc) {
            if !md.characters.is_empty() {
                for (id, _method) in &md.characters {
                    if seen.insert(id.clone()) { names.push(id.clone()); }
                }
                continue;
            }
            log::warn!("constructor walk returned empty characters[] — falling back to instance-method enumeration");
        }

        // Fallback: enumerate Main's instance get* methods.
        let Some(main) = abc.classes.iter().find(|c| c.name == "Main") else { continue };
        for t in &main.instance_methods {
            if let Some(id) = derive_id_from_getter(&t.name) {
                if seen.insert(id.clone()) { names.push(id); }
            }
        }
    }

    names
}

#[allow(clippy::too_many_arguments)]
fn process_character(
    parsed_swf: &swf::Swf,
    swf: &swf_parser::SwfFile,
    char_name: &str,
    output: &Path,
    costumes: Option<&Path>,
    input_path: &Path,
    multi_char_slot: Option<&MultiCharSlot>,
    warnings_out: &mut Vec<String>,
) -> Result<Option<ProcessedCharacter>> {
    // Fresh conversion log for this character.
    crate::api_mappings::reset_conversion_log();

    // Extract character data (ABC: attacks, stats, frame scripts, xframe map)
    // What the main timeline says each sub-MC is, so a clip whose label is not in the mapping
    // file still gets named -- and keeps its frame scripts.
    let class_anim = sprite_parser::class_to_ssf2_anim(parsed_swf, &swf.symbols, &char_name.to_lowercase());
    let mut char_data = extractor::extract(swf, char_name, &class_anim)?;
    log::info!("Extracted: {} attacks, {} animations, {} ssf2→fm mappings",
        char_data.attacks.len(), char_data.animations.len(), char_data.ssf2_to_fm_anim.len());

    // Lift Main's package metadata once per character.
    let package_metadata = swf.abc_blocks.iter()
        .filter_map(|b| abc_parser::parse(b).ok())
        .find_map(|abc| abc_parser::extract_main_package_metadata(&abc));
    let validation = run_tier1_validation(char_name, &char_data, package_metadata.as_ref(), input_path);
    warnings_out.extend(validation.iter().cloned());

    // PascalCase form for entity filenames + scripts subdir.
    let char_pascal: String = package_metadata.as_ref()
        .and_then(|md| md.characters.iter().find(|(id, _)| id == char_name))
        .map(|(_, method)| abc_parser::pascal_form(method))
        .unwrap_or_else(|| abc_parser::pascal_form(char_name));

    // Median xframe scale from the root character MovieClip.
    let (base_scale_x, base_scale_y) = sprite_parser::extract_xframe_scale_from_swf(parsed_swf, char_name)
        .unwrap_or_else(|e| {
            log::warn!("extract_xframe_scale failed: {}, defaulting to 1.0", e);
            (1.0, 1.0)
        });
    let size_mult = mappings::character_stats().scaling.size_multiplier;
    char_data.stats.base_scale_x = base_scale_x * size_mult;
    char_data.stats.base_scale_y = base_scale_y * size_mult;
    log::info!("Character base scale: scaleX={:.4}, scaleY={:.4} (raw {:.4}/{:.4} × {} size multiplier)",
        char_data.stats.base_scale_x, char_data.stats.base_scale_y, base_scale_x, base_scale_y, size_mult);

    // Root MC transforms — computed once and shared.
    let xform_map = sprite_parser::extract_xframe_transforms_from_swf(
        parsed_swf, char_name, &char_data.ssf2_to_fm_anim,
    ).unwrap_or_default();

    // Per-frame collision box geometry.
    let sprite_boxes = sprite_parser::parse_sprite_boxes_from_swf(
        parsed_swf, char_name, &char_data.ssf2_to_fm_anim, &xform_map,
    ).unwrap_or_else(|e| {
        log::warn!("sprite_parser failed: {}", e);
        Default::default()
    });
    log::info!("Sprite boxes: {} animations with geometry", sprite_boxes.len());

    let char_output_dir = match multi_char_slot {
        Some(s) => s.project_dir.clone(),
        None => output.join(char_name),
    };
    // Clean the generated `library/` subtree before (re)writing, so a file that a
    // prior conversion produced but this one no longer does (e.g. an old
    // palette_preview.png after a rename) doesn't orphan — orphans can even collide
    // on deterministic GUIDs. Multi-char characters SHARE one project library/, so
    // only the FIRST slot cleans it; later slots add into it.
    let is_first_slot = multi_char_slot.map(|s| s.slot_idx == 0).unwrap_or(true);
    if is_first_slot {
        let lib = char_output_dir.join("library");
        if lib.exists() {
            if let Err(e) = std::fs::remove_dir_all(&lib) {
                log::warn!("could not clean stale library dir {}: {}", lib.display(), e);
            }
        }
    }
    let img_result = image_extractor::extract_images_from_swf(
        parsed_swf, &char_output_dir, char_name, &char_data.ssf2_to_fm_anim, &xform_map,
    ).unwrap_or_else(|e| {
        log::warn!("image_extractor failed: {}", e);
        image_extractor::ImageExtractionResult {
            images: Default::default(),
            shape_to_bitmap: Default::default(),
            shape_pivot: Default::default(),
            shape_fill_scale: Default::default(),
            anim_images: Default::default(),
        }
    });
    log::info!("Extracted {} sprite images, {} anim image maps",
        img_result.images.len(), img_result.anim_images.len());

    // Sounds: flat library/audio/*.wav (single) or library/audio/<char>/*.wav (multi).
    let sounds_dir = match multi_char_slot {
        Some(_) => char_output_dir.join(format!("library/audio/{}", char_name)),
        None    => char_output_dir.join("library/audio"),
    };
    let sounds = match sound_extractor::extract_all_sounds(parsed_swf, &sounds_dir, char_name) {
        Ok(s) => s,
        Err(e) => { log::warn!("sound_extractor failed: {}", e); vec![] }
    };

    // Discover projectiles, effects, head sprite.
    let (mut projectiles, effects, head_sprite) = image_extractor::discover_projectiles_and_head_from_swf(
        parsed_swf, char_name,
    ).unwrap_or_else(|e| {
        log::warn!("discover_projectiles_and_head failed: {}", e);
        (vec![], vec![], None)
    });
    // In a multi-char .ssf the SWF prefixes a sub-character's projectiles with its id
    // (e.g. sheik's "sheik_uspecDamage"), but the fireProjectile spawn call uses the
    // LOGICAL name ("uspecDamage"). Strip a leading "<subchar>_" so the projectile's
    // content id matches the spawn (else the createProjectile dangles). Single-char is
    // a no-op (its own name isn't prefixed).
    let strip_prefixes: Vec<String> = multi_char_slot
        .map(|s| s.char_ids.iter().map(|c| format!("{}_", c)).collect())
        .unwrap_or_default();
    if !strip_prefixes.is_empty() {
        for p in &mut projectiles {
            for pfx in &strip_prefixes {
                if let Some(rest) = p.name.strip_prefix(pfx.as_str()) {
                    p.name = rest.to_string();
                    break;
                }
            }
        }
    }
    log::info!("Discovered {} projectiles, {} effects, head={}",
        projectiles.len(), effects.len(),
        head_sprite.as_ref().map(|h| h.name.as_str()).unwrap_or("none"));

    // Generate Fraymakers files.
    haxe_gen::generate(output, char_name, &char_pascal, &char_data, &sprite_boxes, &img_result,
        costumes, &sounds, &projectiles, &effects, head_sprite.as_ref(), parsed_swf,
        multi_char_slot)?;
    log::info!("Generated Fraymakers files for {}", char_name);

    let projectile_names: Vec<String> = projectiles.iter().map(|p| p.name.clone()).collect();
    let menu_entity_id = match multi_char_slot {
        Some(_) => format!("{}_menu", char_name),
        None    => "menu".to_string(),
    };

    if multi_char_slot.is_none() {
        // Single-char path: inline conversion-log write.
        write_conversion_log(&char_output_dir, char_name, &char_data,
            package_metadata.as_ref(), &validation)?;
        return Ok(None);
    }

    // Multi-char path: return artifacts for the finalizer.
    let log_block = build_conversion_log_block(char_name, &char_data,
        package_metadata.as_ref(), &validation);
    Ok(Some(ProcessedCharacter {
        manifest_entry: ManifestCharEntry {
            char_id:          char_name.to_string(),
            display_name:     char_pascal.clone(),
            projectile_names,
            menu_entity_id,
        },
        log_block,
    }))
}

/// Assemble the per-character `ssf2_source` + `validation_warnings` payload used
/// inside the multi-char project log's `characters: [...]` array.
fn build_conversion_log_block(
    char_name: &str,
    char_data: &extractor::CharacterData,
    md: Option<&abc_parser::MainPackageMetadata>,
    validation_warnings: &[String],
) -> serde_json::Value {
    let snap = crate::api_mappings::snapshot_conversion_log();
    let to_entries = |m: std::collections::BTreeMap<String, usize>| -> Vec<serde_json::Value> {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
            .collect()
    };
    let mut payload = serde_json::json!({
        "id": char_name,
        "unknown":   to_entries(snap.unknown),
        "ssf2_only": to_entries(snap.ssf2_only),
    });
    if let Some(md) = md {
        let source_method = md.characters.iter()
            .find(|(id, _)| id == char_name)
            .map(|(_, m)| format!("Main::{}", m));
        let mut ssf2_source = serde_json::Map::new();
        if let Some(id) = &md.id   { ssf2_source.insert("package_id".into(),   serde_json::json!(id)); }
        if let Some(g)  = &md.guid { ssf2_source.insert("package_guid".into(), serde_json::json!(g));  }
        if let Some(sm) = source_method {
            ssf2_source.insert("source_method".into(), serde_json::json!(sm));
        }
        if let Some(df) = &char_data.derived_from {
            ssf2_source.insert("parent_normal_stats_id".into(),
                serde_json::json!(df.parent_normal_stats_id));
            ssf2_source.insert("source_method".into(), serde_json::json!(df.source_method));
            ssf2_source.insert("note".into(), serde_json::json!(
                "Fraymakers has no native transformation API. Both \
                 characters ship as peer entities in the same project so \
                 that, when the API lands, the parent's Script.hx can \
                 swap between them at runtime."
            ));
        }
        payload.as_object_mut().unwrap()
            .insert("ssf2_source".to_string(), serde_json::Value::Object(ssf2_source));
    }
    if !validation_warnings.is_empty() {
        payload.as_object_mut().unwrap().insert(
            "validation_warnings".to_string(),
            serde_json::json!(validation_warnings),
        );
    }
    payload
}

/// Write the multi-char project's `<project>.fraytools` + merged
/// `library/manifest.json` + project-level `conversion_log.json`.
fn finalize_multi_char_project(
    project_dir: &Path,
    project_id: &str,
    chars: &[ManifestCharEntry],
    char_logs: &[serde_json::Value],
    input_path: &Path,
) -> Result<()> {
    log::info!("Finalising multi-char project: {} ({} characters)", project_id, chars.len());

    fs::write(project_dir.join(format!("{}.fraytools", project_id)),
        crate::fraytools_project::generate_fraytools_project(project_id))?;

    let manifest = haxe_gen::generate_multi_char_manifest(project_id, chars);
    fs::write(project_dir.join("library/manifest.json"), &manifest)?;
    fs::write(project_dir.join("library/manifest.json.meta"),
        haxe_gen::generate_manifest_meta_pub(&crate::uuid_gen::det_uuid(
            &format!("{}::manifest::meta", project_id))))?;

    let payload = serde_json::json!({
        "project":      project_id,
        "input":        input_path.file_name().and_then(|s| s.to_str()),
        "characters":   char_logs,
    });
    fs::write(project_dir.join("conversion_log.json"),
        serde_json::to_string_pretty(&payload)? + "\n")?;
    Ok(())
}

/// Tier-1 validation warnings — soft logs only, never hard-fail.
fn run_tier1_validation(
    char_name: &str,
    char_data: &extractor::CharacterData,
    md: Option<&abc_parser::MainPackageMetadata>,
    input_path: &Path,
) -> Vec<String> {
    let mut warnings = Vec::new();

    if char_data.attacks.is_empty() {
        warnings.push("attacks map is empty — extractor produced no attack data".to_string());
    }
    let s = &char_data.stats;
    let stats_look_empty = s.weight == 0.0 && s.gravity == 0.0 && s.walk_speed == 0.0
        && s.dash_speed == 0.0 && s.jump_height == 0.0;
    if stats_look_empty {
        warnings.push("stats look like defaults (weight=0, gravity=0, …) — Stage A may have failed".to_string());
    }

    if let Some(md) = md {
        let declared: Vec<&str> = md.characters.iter().map(|(id, _)| id.as_str()).collect();
        if !declared.is_empty() && !declared.contains(&char_name) {
            warnings.push(format!(
                "character {:?} not in Main's declared roster {:?} — likely a `--name` override against a phantom",
                char_name, declared,
            ));
        }
        if let (Some(id), Some(stem)) = (md.id.as_deref(), input_path.file_stem().and_then(|s| s.to_str())) {
            let stem_lc = stem.to_lowercase();
            // SSF2 `DAT<n>.ssf` archives are named by index, never by character, so
            // the stem can't match the id — skip the "may have been renamed" check
            // for them (it's meant to catch a hand-renamed single-character .swf).
            if !is_dat_archive_stem(stem) && id.to_lowercase() != stem_lc {
                warnings.push(format!(
                    "Main.id {:?} disagrees with filename stem {:?} — file may have been renamed",
                    id, stem_lc,
                ));
            }
        }
    }

    if !warnings.is_empty() {
        log::warn!("tier-1 validation: {} warning(s) for {}", warnings.len(), char_name);
        for w in &warnings { log::warn!("  - {}", w); }
    }
    warnings
}

/// Write `<char_dir>/conversion_log.json` for a single-character project.
fn write_conversion_log(
    char_dir: &Path,
    char_name: &str,
    char_data: &extractor::CharacterData,
    md: Option<&abc_parser::MainPackageMetadata>,
    validation_warnings: &[String],
) -> Result<()> {
    let snap = crate::api_mappings::snapshot_conversion_log();
    let to_entries = |m: std::collections::BTreeMap<String, usize>| -> Vec<serde_json::Value> {
        let mut v: Vec<(String, usize)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .map(|(name, count)| serde_json::json!({ "name": name, "count": count }))
            .collect()
    };
    let mut payload = serde_json::json!({
        "character": char_name,
        "unknown": to_entries(snap.unknown),
        "ssf2_only": to_entries(snap.ssf2_only),
    });

    if let Some(md) = md {
        let source_method = md.characters.iter()
            .find(|(id, _)| id == char_name)
            .map(|(_, m)| format!("Main::{}", m));

        let mut ssf2_source = serde_json::Map::new();
        if let Some(id) = &md.id   { ssf2_source.insert("package_id".into(),   serde_json::json!(id)); }
        if let Some(g)  = &md.guid { ssf2_source.insert("package_guid".into(), serde_json::json!(g));  }
        if let Some(sm) = source_method {
            ssf2_source.insert("source_method".into(), serde_json::json!(sm));
        }
        if let Some(df) = &char_data.derived_from {
            ssf2_source.insert("parent_normal_stats_id".into(),
                serde_json::json!(df.parent_normal_stats_id));
            ssf2_source.insert("source_method".into(), serde_json::json!(df.source_method));
            ssf2_source.insert("note".into(), serde_json::json!(
                "Fraymakers has no native transformation API; \
                 this character is emitted as a standalone package and \
                 must be wired manually in the parent's Script.hx."
            ));
        }
        payload.as_object_mut().unwrap()
            .insert("ssf2_source".to_string(), serde_json::Value::Object(ssf2_source));
    } else if let Some(df) = &char_data.derived_from {
        payload.as_object_mut().unwrap().insert(
            "ssf2_source".to_string(),
            serde_json::json!({
                "parent_normal_stats_id": df.parent_normal_stats_id,
                "source_method": df.source_method,
                "note": "Fraymakers has no native transformation API; \
                    this character is emitted as a standalone package and \
                    must be wired manually in the parent's Script.hx.",
            }),
        );
    }

    if !validation_warnings.is_empty() {
        payload.as_object_mut().unwrap().insert(
            "validation_warnings".to_string(),
            serde_json::json!(validation_warnings),
        );
    }
    fs::create_dir_all(char_dir)?;
    fs::write(
        char_dir.join("conversion_log.json"),
        serde_json::to_string_pretty(&payload)? + "\n",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_id_from_getter_covers_every_corpus_shape() {
        let cases: &[(&str, &str)] = &[
            ("getMario",         "mario"),
            ("getBandanaDee",    "bandanadee"),
            ("getCaptainFalcon", "captainfalcon"),
            ("getChibiRobo",     "chibirobo"),
            ("getDonkeyKong",    "donkeykong"),
            ("getgameandwatch",  "gameandwatch"),
            ("getMegaMan",       "megaman"),
            ("getMetaKnight",    "metaknight"),
            ("getPacMan",        "pacman"),
            ("getBlackMage",     "blackmage"),
            ("getGigaBowser",    "gigabowser"),
            ("getWario_Man",     "wario_man"),
            ("getSheik",         "sheik"),
        ];
        for (m, expected) in cases {
            let got = derive_id_from_getter(m);
            assert_eq!(got.as_deref(), Some(*expected),
                "derive_id_from_getter({:?}) = {:?}, expected Some({:?})", m, got, expected);
        }
    }

    #[test]
    fn derive_id_rejects_non_get_prefix() {
        assert_eq!(derive_id_from_getter("init"), None);
        assert_eq!(derive_id_from_getter(""), None);
        assert_eq!(derive_id_from_getter("get"), None);
    }

    #[test]
    fn hscript_lint_accepts_sound_script() {
        let ok = r#"
            function init() {
                // a comment with unbalanced { ( [ chars
                var s = "string with } ] ) braces";
                var t = 'interp ${entity.getName()} and an escaped \' quote';
                /* block } comment */
                self.doThing({ a: 1, b: [2, 3] });
            }
        "#;
        assert_eq!(hscript_structural_problem(ok), None);
    }

    #[test]
    fn hscript_lint_catches_unbalanced_brace() {
        let bad = "function f() {\n  if (x) {\n    y();\n}\n";
        assert!(hscript_structural_problem(bad).is_some());
    }

    #[test]
    fn hscript_lint_catches_mismatch_and_stray_close() {
        assert!(hscript_structural_problem("foo([1, 2)").is_some());
        assert!(hscript_structural_problem("a) + b").is_some());
    }

    #[test]
    fn hscript_lint_catches_unterminated_string_and_empty_interp() {
        assert!(hscript_structural_problem("var s = \"oops;").is_some());
        assert!(hscript_structural_problem("var s = 'val ${}';").is_some());
    }

    #[test]
    fn hscript_lint_ignores_delims_in_strings_and_comments() {
        assert_eq!(hscript_structural_problem("var s = \"){]}[(\";"), None);
        assert_eq!(hscript_structural_problem("// )]}\nvar x = 1;"), None);
    }
}
