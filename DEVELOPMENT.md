# Peptide -- converter developer guide

> **Audience:** a developer or AI agent picking up this project cold.
> this doc covers what the project is, how to build and run it, how the
> conversion pipeline is wired together, what every module does, what works,
> what's unfinished, and what to do next.
>
> **companion docs (all top-level):**
> - [`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) -- the authoritative *format* reference
>   (SSF2 `.ssf`/SWF internals and the Fraymakers `.entity` JSON schema). read it
>   alongside this file.
> - [`README.md`](README.md) -- short user-facing summary.
> - [`TESTING.md`](TESTING.md) -- the two validation harnesses (FrayTools-side +
>   Fraymakers-engine-side), the end-to-end iteration loop, and in-engine
>   validation status.
> - [`CONTRIBUTING.md`](CONTRIBUTING.md) -- hot-file → doc-section map + the
>   per-change checklist.
> - known issues, the code-quality backlog, and prioritized next steps live in
>   [`docs/STATUS.md`](docs/STATUS.md) -- the single home for converter status and
>   TODOs ("what should I fix next" work).

---

## table of contents

1. [what this project is](#1-what-this-project-is)
2. [repository layout](#2-repository-layout)
3. [setup, build & run](#3-setup-build--run)
4. [the conversion pipeline](#4-the-conversion-pipeline)
5. [module-by-module reference](#5-module-by-module-reference)
6. [mapping files (`mappings/`)](#6-mapping-files-mappings)
7. [30 → 60 fps doubling](#7-30--60-fps-doubling)
8. [input format: SSF2 `.ssf`](#8-input-format-ssf2-ssf)
9. [output format: Fraymakers character package](#9-output-format-fraymakers-character-package)
10. [current status -- what works](#10-current-status--what-works)
11. [known issues, next steps & backlog → STATUS.md](#11-known-issues-next-steps-and-the-code-quality-backlog)
12. [tips for the next agent](#12-tips-for-the-next-agent)

---

## 1. what this project is

**Peptide** is a Fraymakers modding toolkit: one app and one binary (Rust) with three jobs.
- **convert** -- turn a Super Smash Flash 2 (SSF2) character `.ssf` into a complete,
  FrayTools-compatible Fraymakers character package (sprites, animations, collision boxes,
  costumes, sounds, projectiles, effects, menu art, decompiled logic).
- **test live** -- boot Fraymakers and drive a real match (spawn, run moves, read state, eval
  hscript) to validate a conversion in the engine. see [`TESTING.md`](TESTING.md) and
  [`docs/PEPTIDE_GUIDE.md`](docs/PEPTIDE_GUIDE.md).
- **drive FrayTools** -- publish a project to `.fra`, render an entity, or pull box geometry
  over the DevTools protocol.

this doc is the developer guide for the **converter** side. both SSF2 and Fraymakers are indie
platform-fighting games: SSF2 ships its characters as Flash content; Fraymakers characters are
authored in **FrayTools** (the official modding editor) as a directory of JSON + Haxe + PNG
assets. feed the converter an SSF2 `.ssf` file and it writes a complete FrayTools-compatible
character folder.

it converts, per character:

| SSF2 source | → | Fraymakers output |
|---|---|---|
| Bitmap sprites (one per animation frame) | → | `library/sprites/*.png` + `.meta` sidecars |
| Per-frame collision boxes (hitboxes, hurtboxes, grab/ledge/reflect/absorb/touch boxes…) | → | `COLLISION_BOX` / `COLLISION_BODY` / `POINT` layers in the character entity (`<Pascal>.entity`) |
| Animation timelines | → | `animations` / `layers` / `keyframes` in the character entity |
| AS3 frame scripts (ABC bytecode) | → | `FRAME_SCRIPT` keyframe code in the entity |
| AS3 character logic (`XxxExt` class methods) | → | decompiled into `Script.hx` |
| Character stats (weight, gravity, speeds…) | → | `CharacterStats.hx` (data-driven via `mappings/character/stats.jsonc`) |
| Attack / hitbox data | → | `HitboxStats.hx` (data-driven via `mappings/character/hitbox_stats.jsonc`) |
| Animation flags | → | `AnimationStats.hx` |
| Costume palettes (`misc.ssf`) | → | `costumes.palettes` + entity `paletteMap` |
| Sounds | → | `library/audio/*.wav` + per-sound `.wav.meta` content entries |
| Projectiles | → | `library/entities/<name>.entity` + `library/scripts/Projectile/<Pascal>*.hx` |
| Effects (VFX) | → | `library/entities/<effect>.entity` (one per effect; no scripts/stats); referenced from `Script.hx` via `match.createVfx(…)` |
| Menu / portrait head sprite | → | `Menu.entity` (full / css / icon / hud variants; `<Pascal>_Menu.entity` in multi-char projects) |

the conversion is **fully automatic and deterministic** -- every GUID in the
output is derived (UUID v5) from the character id + a context string, so
re-running on the same input reproduces byte-identical GUIDs.

**scope note:** the converter is a *data* converter, it doesn't run SSF2 or Fraymakers (that's
Peptide's live-engine harness side, see [`TESTING.md`](TESTING.md)). translated logic lands in
`.hx` files a human must still review: many lines are emitted with `/*TODO*/` markers or as
`// [SSF2-only: NAME] …` comments where there's no equivalent. the per-run SSF2-only calls are
logged to `conversion_log.json`.

---

## 2. repository layout

```
peptide/  (repo root — Cargo workspace; the `peptide` binary is the product)
├── Cargo.toml                Workspace + the `peptide` package
├── Cargo.lock
├── README.md                 Short user-facing readme
├── AGENT_CONTEXT.md          Authoritative SSF2 / Fraymakers FORMAT reference
├── DEVELOPMENT.md            ← this file (the converter library internals)
├── CONTRIBUTING.md           Hot-file → doc-section map + per-change checklist
├── TESTING.md                Validation harnesses + validation status
├── LICENSE                   Apache License 2.0
├── NOTICE                    Peptide's attribution notice (carry on redistribution; Apache 4d)
├── NOTICE.md                 Dependency attribution (Ruffle swf crate, etc.)
├── .gitignore                Ignores *.ssf, *.swf, /build, characters/ media, engine artifacts
│
├── src/                       Peptide — engine harness, webview UI, bytecode patcher
│   ├── main.rs                CLI dispatch (gui/tui/convert/export/render/harness/patcher) + allocator
│   ├── convert.rs             `peptide convert` adapter → ssf2_converter::run_conversion
│   ├── config.rs              persisted config + env-override resolvers
│   ├── platform.rs            FrayTools/Fraymakers path discovery + publish-folder editing
│   ├── gui.rs                 webview glue (wry/tao): screen router IPC, boot flow
│   ├── peptide_ui.html        the UI (home / setup / converter / frayhook / console)
│   ├── ui.rs / bridge.rs      ratatui TUI + headless TCP runtime + the launcher
│   ├── manifest.rs            engine-symbol dependency table (doctor preflight)
│   ├── fraytools.rs           FrayTools CDP driver (export / render / harness)
│   └── asm.rs / interpreter.rs  opcode helpers + the friendly-command vocabulary
│
├── crates/ssf2-converter/     The SSF2 → Fraymakers converter, as a library crate
│   ├── Cargo.toml             package `ssf2_converter` (lib; dev bins gated by `dev-tools`)
│   ├── src/
│   │   ├── lib.rs             pub mod declarations + run_conversion re-export
│   │   ├── convert.rs         run_conversion(ConvertOptions) — the in-process entry point
│   │   ├── ssf.rs             .ssf → raw SWF decompression
│   │   ├── swf_parser.rs      SWF tag parsing (thin wrapper over the Ruffle `swf` crate)
│   │   ├── swf_timeline.rs    display list frame by frame — the ONE reader, shared by the
│   │   │                      character and stage paths (matrix, ratio, blend, alpha, visibility)
│   │   ├── abc_parser.rs      AVM2/ABC bytecode parser + semantic extractors (~2650 LOC)
│   │   ├── decompiler.rs      ABC bytecode → Haxe-ish source (~2050 LOC)
│   │   ├── extractor.rs       Bridges abc_parser output into CharacterData
│   │   ├── anim_splitter.rs   Splits multi-move SSF2 sprites into FM animations
│   │   ├── sprite_parser.rs   Per-frame collision-box geometry from SWF timelines
│   │   ├── image_extractor.rs PNG extraction, placement, skew baking, projectile/effect/head
│   │   ├── vector_raster.rs   Rasterizes vector-shape sprites (shape-only heads/effects)
│   │   ├── sound_extractor.rs Audio extraction (Nellymoser / MP3 / ADPCM → WAV via ffmpeg)
│   │   ├── palette_gen.rs      Costume palette generation
│   │   ├── api_mappings.rs     Decompiled-Haxe rewriter pipeline (JSONC-driven, ~2530 LOC)
│   │   ├── mappings.rs         JSONC loader + OnceLock-cached accessors for mappings/*.jsonc
│   │   ├── entity_gen.rs       Fraymakers .entity JSON builder (~2300 LOC)
│   │   ├── entity_compact.rs   post-pass on every finished entity: share identical symbols,
│   │   │                       empty out invisible ones, fold runs of identical keyframes
│   │   ├── haxe_gen.rs         Top-level output orchestrator — writes the whole package
│   │   ├── fraytools_project.rs  Emits the `<name>.fraytools` project file
│   │   ├── fraytools_transform.rs  Shared FrayTools transform/coordinate helpers
│   │   ├── project.rs         Shared project / manifest structs
│   │   ├── uuid_gen.rs         Deterministic UUID v5 generation
│   │   └── bin/               ~30 diagnostic binaries (dump_*/check_*/probe_*/find_*/audit_*; gated: --features dev-tools)
│   ├── mappings/              ← editable runtime config (JSONC), baked in via include_str!
│   │   ├── commands.jsonc      universal SSF2 → FM API command conversions
│   │   └── character/          animations.jsonc / stats.jsonc / hitbox_stats.jsonc
│   └── tests/                 integration + golden tests
│
├── tools/                     Build + engine-harness orchestration (shell + Node)
│   ├── make-app.sh             macOS: build peptide + wrap it in build/Peptide.app
│   ├── make-win.sh             Cross-compile the Windows peptide.exe into build/windows/
│   ├── run.sh / runseq.sh      Boot Fraymakers + send one / a sequence of console commands
│   ├── recipe.sh               Run a shareable .recipe (commands + #!char/#!gap directives)
│   ├── rebuild-sandbag.sh      Quick: rebuild peptide + convert sandbag.ssf
│   ├── tests/                  Test + parity harnesses and golden fixtures
│   │   ├── ab_compare.sh        Capture/diff a golden behavior signature (regression gate)
│   │   ├── batch_spawn_test.sh  Batch export + in-engine spawn-test a set of characters
│   │   ├── parity_check.py      Static SSF2-source-vs-output hitbox-parity diff
│   │   ├── translation_completeness.sh  Per-character untranslated-marker dashboard
│   │   └── recipes/             .recipe scripts + .golden behavior signatures
│   └── (FrayTools drive)       ported into the peptide binary: `peptide export|render|harness`
├── characters/                Converter OUTPUT (`*.hx`, `*.json`, `*.entity` tracked; media ignored)
└── build/                     Cargo build output (git-ignored; target-dir → build/release/peptide)
```

**where the test inputs live.** the `.ssf` files are SSF2 game content
(© McLeodGaming), so they're never committed (`.gitignore` excludes `*.ssf`) -- you
bring your own. by default they're expected in a **sibling directory** of the repo:

```
<workspace>/
├── Peptide/                     ← this repo
└── ssf2-ssfs/                   ← your SSF2 .ssf files + misc.ssf
```

so from the repo root the corpus is `../ssf2-ssfs` -- the relative path the CLI
examples and `tools/rebuild-sandbag.sh` use. keeping the corpus somewhere else? point
`$SSF2_SSFS_DIR` at it (the tests honor it; the `src/bin/` diagnostics take the corpus
path as their first CLI arg).

`ssf2-ssfs/` holds the SSF2 roster (`mario.ssf`, `fox.ssf`, `link.ssf`, …) plus
**`misc.ssf`** (costume palette data). **you don't need any of it just to build or
test:** a fresh checkout's `cargo build` and `cargo test` stay green, because every test
and dev bin that needs the corpus checks for it and skips cleanly when it's absent. you
only need the files to actually run conversions or exercise the corpus-dependent tests.

---

## 3. setup, build & run

### 3.1 prerequisites

- **Rust** (stable toolchain) -- required to build the core converter.
- **`ffmpeg`** on `PATH` -- runtime sound conversion (Nellymoser / MP3 /
  ADPCM → WAV). if absent the conversion still completes; sound extraction is
  skipped with a warning.
- the desktop app is the `peptide` binary itself (a system webview) -- no extra
  toolchain or OS-specific SDK. the converter is platform-agnostic.

there's **no external Rust runtime dependency** for SWF decompression, bitmap
decoding, or ABC parsing -- those all happen in-process (`src/ssf.rs`,
`src/swf_parser.rs`, `src/abc_parser.rs`, `src/decompiler.rs`).

### 3.2 build

```bash
cargo build --release
```

this produces `build/release/peptide` -- the single binary (engine harness +
in-process converter + FrayTools driver). the converter is a **library**
(`crates/ssf2-converter`) with no binary of its own. the ~30 **diagnostic binaries**
live in the converter crate behind the `dev-tools` feature (see
[§5.7](#57-diagnostic-binaries-srcbin)); they're excluded from the default build.

### 3.3 run the converter (CLI)

conversion is a Peptide subcommand:

```
peptide convert <FILE.ssf> [OPTIONS]

Arguments:
  <FILE>                 Path to the .ssf file to convert

Options:
  -o, --output <DIR>     Output directory            [default: ./characters]
  -n, --name <NAME>      Character-name override. For a multi-character .ssf,
                         selects only that character. Auto-detected if omitted.
      --misc-ssf <FILE>  Path to misc.ssf for costume palettes.
                         Auto-detected next to the input file if omitted.
  -v, --verbose          Debug-level logging
```

the same conversion runs behind the app's **SSF2 → Fraymakers Converter** screen
and programmatically via `ssf2_converter::run_conversion(ConvertOptions)`.

costume extraction is **in-process** -- `convert.rs::extract_costumes_to_temp`
unwraps `misc.ssf` and runs `abc_parser::scan_all_costume_methods` directly,
writing a temporary JSON cache deleted after the run. there's no separate
`extract_costumes` binary anymore.

examples:

```bash
# Convert mario; costumes auto-loaded from ssf2-ssfs/misc.ssf next to it
./build/release/peptide convert ../ssf2-ssfs/mario.ssf

# Explicit output dir + explicit misc.ssf
./build/release/peptide convert ../ssf2-ssfs/fox.ssf \
    --output ./characters --misc-ssf ../ssf2-ssfs/misc.ssf
```

output for character `mario` lands in `./characters/mario/` -- a complete
FrayTools character package (see [§9](#9-output-format-fraymakers-character-package)).

### 3.4 quick rebuild loop

`tools/rebuild-sandbag.sh` is the inner dev loop: rebuild the release binary,
re-convert `sandbag.ssf`, then print a sprite count.

```bash
./tools/rebuild-sandbag.sh
```

> **input path.** the script reads `../ssf2-ssfs/sandbag.ssf` (relative to the
> repo root), so run it from the repo root. `sandbag` is the standard smoke-test
> character (small, simple) -- keep using it for fast iteration.

### 3.5 the desktop app

the graphical app **is** `peptide` -- running the binary with no arguments opens
the system-webview window (wry/tao; WKWebView on macOS, WebView2 on Windows,
WebKitGTK on Linux). the whole UI is `src/peptide_ui.html`. on first run a
**Setup** screen captures where Fraymakers and FrayTools live and the current
character; after that a **Home** screen offers three buttons:

- **Launch Peptide** -- boot the engine and drive a live match (the console).
- **SSF2 → Fraymakers Converter** -- run a conversion in-process (a worker thread
  calling `run_conversion`), with a progress bar and a result panel.
- **FrayTools Hook** -- publish to `.fra` / render an entity / extract box
  geometry, driving FrayTools over CDP.

the webview glue lives in `src/gui.rs` (see [`docs/PEPTIDE_DESIGN.md`](docs/PEPTIDE_DESIGN.md)
for the harness internals); there's no separate GUI crate anymore.

**double-clickable macOS app.** `./tools/make-app.sh` builds the `peptide`
binary and wraps it in `build/Peptide.app` -- a normal Finder app (name, dock
icon, `.ssf` association) with `peptide` as the bundle executable. it
ad-hoc-codesigns the bundle so Gatekeeper allows a locally-built app to launch,
then opens it on success (`--no-open` to just build). `build/` is git-ignored.

```bash
./tools/make-app.sh            # build + assemble build/Peptide.app + launch
./tools/make-app.sh --no-open  # build + assemble only (packaging / CI)
```

**Windows build.** `./tools/make-win.sh` cross-compiles `peptide.exe` into
`build/windows/` (prefers `cargo-xwin` for the MSVC ABI, falls back to
`mingw-w64`; prints the exact install command if neither toolchain is present),
or build natively on Windows with `cargo build --release`. the webview uses the
WebView2 runtime that ships with Windows 10/11.

---

## 4. the conversion pipeline

end to end, one `run_conversion` call does this (`crates/ssf2-converter/src/convert.rs`):

```
 .ssf file
    │
    ▼
[1] ssf::decompress              .ssf → raw SWF bytes
    │                            (.ssf = 8-byte header + zlib payload, OR a raw SWF)
    ▼
[2] swf_parser::parse            SWF → { version, symbols (id→class name),
    │                                    abc_blocks (raw ABC bytecode) }
    ▼
[3] detect_char_names            Walk `Main`'s constructor:
    │                            `self.register("characters", [self.getX(), …])`
    │                            → list of character ids to process. The id
    │                            comes from each method name (strip `get`,
    │                            lowercase). `Main`'s constructor is the
    │                            SSF's canonical "table of contents".
    │                            Fallback path (defensive, for one release):
    │                            enumerate `Main`'s instance `get*` methods.
    │                            (`--name` overrides; filename is the
    │                            last-resort fallback for misc.ssf-style SWFs.)
    │                            See §5.1 (and `git log` for the detection history).
    │
    ├─ (once) extract_costumes_to_temp:
    │     misc.ssf → ssf::decompress → swf_parser::parse →
    │     abc_parser::scan_all_costume_methods → temp costumes JSON
    │     (in-process; temp file deleted at end of run)
    │
    ▼  for each character id:
process_character():
    │
    ├─ api_mappings::reset_conversion_log()   start a fresh per-character log
    │
[4] extractor::extract           ABC → CharacterData {
    │                              attacks + hitboxes, stats, decompiled Ext methods,
    │                              frame scripts, ssf2→fm anim-map, ext_vars +
    │                              ext_var_inits, projectile_data (per-projectile
    │                              SSF2 physics + hitboxes from getProjectileStats()) }
    │                            (delegates to abc_parser + decompiler)
    ▼
[5] sprite_parser::extract_xframe_scale     root MovieClip → base scaleX/scaleY
    │
[6] sprite_parser::parse_sprite_boxes       SWF timelines → per-animation,
    │                                       per-frame collision-box geometry
    ▼
[7] image_extractor::extract_images         SWF bitmaps → PNG files + per-frame
    │                                       image PLACEMENT (full affine matrix) data
    │                                       + skew-bake on sheared placements
    ▼
[8] sound_extractor::extract_all_sounds     SWF audio → library/audio/*.wav (via ffmpeg)
    │
[9] image_extractor::discover_projectiles_and_head
    │                                       find projectile sprites, effect sprites,
    │                                       and menu head sprite
    ▼
[10] haxe_gen::generate          Writes the ENTIRE output package:
       │
       ├─ EffectAnimGuard::install(effect→primary-animation map)
       │   so all translate_ssf2_to_fm calls in this scope can resolve
       │   attachEffect("name") → match.createVfx(…) with the right animation.
       │
       ├─ HitboxStats.hx / CharacterStats.hx / AnimationStats.hx / Script.hx
       │   (+ .hx.meta sidecars)
       ├─ anim_splitter::split_animations  (jab→jab1/2/3/4, taunt→3 slots,
       │                                    aerial → active + land, strong → in/charge/attack,
       │                                    grab → grab/dash_grab/grab_hold/grab_pummel, …)
       ├─ entity_gen::generate_entity      → library/entities/<Pascal>.entity
       │   (drops empty animations; jab-count-driven jab keep-list)
       ├─ <project>.fraytools  (fraytools_project)   + library/manifest.json (+ .meta)
       ├─ entity_gen::get_image_meta_guids → a .meta sidecar per sprite PNG
       ├─ palette_gen::generate_palettes_and_remap
       │     → costumes.palettes (+ .meta), palette_preview.png,
       │       then REWRITES <Pascal>.entity with paletteMap filled in
       ├─ entity_gen::generate_menu_entity      from the discovered head sprite
       ├─ for each projectile:
       │     ├─ <proj>.entity   (visuals + boxes + multi-state animations)
       │     └─ library/scripts/Projectile/<Pascal>{Script,Stats,HitboxStats,AnimationStats}.hx
       │        (Annie / aJewelofRarity convention: single Projectile/ dir,
       │        Pascal-cased filename per projectile)
       ├─ for each effect:
       │     └─ <effect>.entity   (image-only; one animation per inner FrameLabel,
       │                          or single `active` if no labels)
       ├─ generate_sound_entries  (per-WAV .meta sidecar)
       └─ conversion_stats.json
    │
[11] write_conversion_log        Snapshot api_mappings::snapshot_conversion_log()
                                 → <char>/conversion_log.json with:
                                   - character (the derived id),
                                   - unknown + ssf2_only counts,
                                   - ssf2_source: { package_id, package_guid,
                                     source_method } (always present when
                                     `Main` exists); adds parent_normal_stats_id
                                     + note for transformations (Giga Bowser,
                                     Wario Man — bundles whose cData.normalStats_id
                                     mismatches the derived id),
                                   - validation_warnings (Tier 1 soft logs:
                                     empty stats/attacks, char_name not in
                                     declared roster, id ≠ filename stem).
                                 Used by the GUI "Unhandled Calls" popup.
```

three sub-systems are worth calling out:

- **ABC path** (`abc_parser` + `decompiler`): SSF2 keeps character *logic*,
  *stats*, *attack tables*, *projectile stats* and *costume data* inside AS3
  bytecode. `abc_parser` is a from-scratch ABC (AVM2) parser; `decompiler`
  reconstructs control flow (a CFG with proper if/else/while) and renders
  Haxe-like source.

- **SWF-timeline path** (`sprite_parser` + `image_extractor`): SSF2 keeps all
  *visual* and *collision* data in the SWF display list (`DefineSprite`
  timelines of `PlaceObject` / `ShowFrame` / `RemoveObject` tags). these two
  modules walk those timelines frame-by-frame and produce per-animation,
  per-frame, per-layer data structures.

- **rewriter pipeline** (`api_mappings::translate_ssf2_to_fm`): every block of
  decompiled Haxe (Script.hx ext methods, ext-class iinit, frame scripts) passes
  through a fixed pipeline of structural and text transforms -- see
  [§5.5](#55-the-translation-pipeline-api_mappingsrs) for the exact order.

`haxe_gen::generate` is the real orchestrator of *output* -- `main.rs` just
prepares the inputs and hands everything to it.

---

## 5. module-by-module reference

sizes are approximate (Rust LOC). modules are grouped by pipeline role. this is a map of what
each module does plus its real entry points; the code is the detail.

### 5.1 entry point & wiring

#### `convert.rs` (~715 LOC)
the in-process conversion entry point. `run_conversion` + `process_character` orchestrate the
run (`peptide convert` and the Peptide GUI both call `run_conversion(ConvertOptions)`).
`detect_char_names` walks `Main`'s constructor for `register("characters", [self.getX(), …])`
and derives an id per array entry (strip `get`, lowercase, preserve `_`: `getGigaBowser` →
`gigabowser`); sub-characters (Sheik, Giga Bowser, Wario Man) fall out as extra array elements,
with a `get*`-enumeration fallback if the walk comes back empty. `extract_costumes_to_temp`
pulls per-character costume data from `misc.ssf` into a temp cache. `process_character` runs
pipeline steps [4]–[10] for one character, wrapping each stage so a failure warns and continues
with a default. `run_tier1_validation` soft-logs warnings (empty attacks, all-default stats,
char_name not in the roster, Main.id ≠ filename stem) into `conversion_log.json ::
validation_warnings`. `write_conversion_log` writes `<char_dir>/conversion_log.json` (fields in
§4 step [11]).

#### `lib.rs`
`pub mod` declarations + the `run_conversion` re-export. every module is public so `src/bin/`
can `use ssf2_converter::*`. the crate is a library, no standalone binary.

### 5.2 SSF / SWF / mapping layer

#### `ssf.rs`
`decompress(data) -> Vec<u8>`: `.ssf` → raw SWF bytes. a `.ssf` is either a raw SWF
(`FWS`/`CWS`/`ZWS` magic, passed through) or an SSF-wrapped file: `u32 swf_len` + `u32
garbage_header_size` + zlib payload.

#### `swf_parser.rs`
`parse(data) -> SwfFile`: thin wrapper over the **`swf` crate** (Ruffle), returning `SwfFile {
version, frame_count, frame_rate, symbols, abc_blocks }`. **Note**: several downstream modules
re-run `swf::decompress_swf` + `swf::parse_swf` on the raw buffer instead of reusing this
(code-quality backlog in [`docs/STATUS.md`](docs/STATUS.md)).

#### `mappings.rs`
JSONC loader for `mappings/*.jsonc`: `strip_jsonc` removes comments + trailing commas, then
`serde_json` parses. each accessor is `OnceLock`-cached: `character_animations()` →
`AnimationMappings`, `character_stats()` → `StatMappings`, `character_hitbox_stats()` →
`HitboxStatsMapping`, `api_commands()` → `ApiCommands`. an on-disk copy (working-dir,
next-to-binary, or repo root) overrides the `include_str!`'d default; malformed overrides warn
and fall back. `evaluate_stat_derivation` compiles `stats.jsonc :: derivations` expressions via
`fasteval` against the converted stats (so e.g. `aerialSpeedCap = max(air_mobility_raw,
aerial_friction) * 5.0`).

### 5.3 ABC (ActionScript bytecode) layer

#### `abc_parser.rs` (~2650 LOC -- largest module)
a complete AVM2/ABC parser written from scratch (constant pool, `Method`s / `Class`es /
`Trait`s / `Script`s / `MethodBody`s → `AbcFile`), plus semantic extraction tuned to SSF2's
code shape:
- **Stage A (stats / attacks / projectiles)** runs off `Main::get<X>()`. `extract_character`
  finds the bundle via `find_bundle_method`, runs `extract_attack_objects` /
  `extract_projectile_objects` / `extract_ssf2_stats`, and sets `derived_from` when
  `cData.normalStats_id` mismatches the name (drives the transformation banner). then Stage B
  walks the `<X>Ext` class (frame scripts, ext_methods, ext_vars + iinit inits) and `_fla.*`
  sub-classes. `extract_normal_stats_id` drives transformation detection.
- **detection / package metadata:** `extract_main_package_metadata` pulls `id`, `guid`, and
  the `characters` array (`Vec<(derived_id, method_name)>`) out of Main's iinit in one pass and
  feeds `conversion_log.json :: ssf2_source`. `derive_id_from_bundle_method_name` is the shared
  strip-`get`/lowercase rule.
- **shared plumbing:** `scan_method<V: AbcVisitor>` is the single AVM2 stack simulator behind
  `AttackVisitor` / `ProjectileVisitor` / `StatsVisitor` / `CostumeVisitor`.
  `scan_all_costume_methods` / `decode_costume_objects` recover costume tables from `misc.ssf`;
  `extract_xframe_name` maps internal frame methods to animation labels.

#### `decompiler.rs` (~2050 LOC)
turns ABC method bytecode into readable Haxe-ish source, rebuilding real control flow (well
past a disassembler). `build_blocks` splits into basic blocks; `BlockDecoder` / `StructuredDecoder`
reconstruct a CFG and recover structured control flow (`if`/`else`/`while`, short-circuit
`&&`/`||`, branch inversion); `Expr` / `Stmt` ASTs render via `render_stmts` / `render_closure`;
`infer_activation_slots` / `rename_loop_counters` / `rename_locals_in_*` name locals.
`guard_loop_termination` appends `i = i + 1` to any `while (i < ….length)` counter loop whose
body doesn't advance the counter (SSF2 mutates arrays mid-iteration via `splice`); it only
touches loops missing the advance. `decompile_method` / `decompile_closure` are the entry
points; `lookup_api` is a small inline SSF2 → FM table used during decompilation, distinct from
the JSONC rewriter in `api_mappings`.

### 5.4 extraction layer

#### `extractor.rs`
bridges abc_parser → the generator and owns the `CharacterData` struct (`attacks`, `stats`,
`animations`, `scripts`, `ext_vars`, `ext_var_inits`, `projectile_data`, `ssf2_to_fm_anim`,
`derived_from`; `derived_from` drives the transformation banner). key fns: `extract()` (drives
abc_parser, dedupes raw SSF2 names against FM names, seeds split sub-anims), `build_ssf2_to_fm_anim`
(via `animations.jsonc`), `convert_hitboxes` / `convert_stats` (field mapping via
`hitbox_stats.jsonc` / `stats.jsonc`), `expand_split_anim` (`jab`→`jab1..4`, `taunt`→up/down).

#### `anim_splitter.rs`
`split_animations(...)`: some SSF2 sprites pack several FM moves into one timeline split by
internal frame labels (a "Jab" sprite = jab1 → jab2 → jab3). the cut rules are **hardcoded
match arms** keyed on `anim_name` (aerial / strong / grab / jump / special / taunt / …),
derived by scanning the corpus' inner FrameLabels.

#### `sprite_parser.rs`
extracts **collision-box geometry** from SWF `DefineSprite` timelines. `parse_sprite_boxes`
produces per-animation, per-frame boxes (`BoxType` enum → `FrameBox` / `AnimationBoxData`);
`extract_xframe_transforms` recovers the root-MovieClip affine matrix; `find_collision_box_base_size`
measures the `CollisonBox_6` shape at runtime (note SSF2's internal typo "Collison"), finding it
by which box-typed instances actually place since the symbol isn't reliably named `CollisonBox`;
`matrix_to_box` / `matrix_to_itembox` turn a `PlaceObject` matrix
into an FM box; `split_jab` / `split_taunt` mirror `anim_splitter` so box and image splits stay
aligned; `apply_fallbacks` clones box data from a related anim (`stunned ← hurt`, `swim ← fall`,
…).

#### `image_extractor.rs` (~1970 LOC)
extracts the **visual sprites** and their **per-frame placement**. `extract_images` decodes
every `DefineBitsLossless` / `DefineBitsJpeg3` to PNG and builds `shape_to_bitmap` (skipping the
SWF null bitmap id `65535`) + `shape_pivot`. `build_anim_frame_images` walks each timeline
recording every placed image's world-space affine matrix (`FrameImageEntry`), with two-pass
`_fla.` effect-sprite flattening. `ImageLocalMatrix` does the SWF matrix decomposition (scale,
rotation via `atan2(b, a)`, flip as negative `sy`, `has_skew()`). `prerender_skewed_frames`
bakes sheared placements (which FrayTools' IMAGE symbol can't express) into fresh PNGs and
rewrites them as plain translations. `discover_projectiles_and_head` finds projectile sprites
(`attack_idle` FrameLabel + `stance` PlaceObject), effect sprites, and the menu head (prefers
`*_head`, accepts `*_icon`, excludes `*_hud`).

#### `sound_extractor.rs`
`extract_all_sounds(...)`: pulls `DefineSound` out of the SWF and writes `library/audio/*.wav`
(PCM 16-bit). Nellymoser + MP3 get wrapped in a synthetic **FLV** and remuxed through `ffmpeg`;
ADPCM uses a generic FLV wrapper.

> the hand-rolled SWF tag walker (`parse_sounds`) could be replaced with the `swf` crate's
> `DefineSound` support (code-quality backlog in [`docs/STATUS.md`](docs/STATUS.md)).

### 5.5 the translation pipeline (`api_mappings.rs`)

`translate_ssf2_to_fm(code)` is the single entry every block of decompiled Haxe passes through
(Script.hx ext methods, ext-class iinit, embedded frame scripts). the fixed order:

1. **`remove_readiness_guards`** -- strip `if (SSF2API.isReady())` wrappers, inline the body.
2. **`double_frame_counts`** -- 30 → 60 fps scaling via `commands.jsonc :: frame_params`; runs
   before the rename pass so SSF2 field names still match (see [§7](#7-30--60-fps-doubling)).
3. **`apply_call_splits`** -- fan out SSF2 umbrella calls (`updateAttackStats`) into multiple FM
   calls per `commands.jsonc :: call_splits`; bracket- and comment-aware; unmapped fields become
   `// TODO:`, `skip_if_value` matches drop silently.
4. **literal `replacements`** -- ordered find/replace from `commands.jsonc :: replacements`
   (order matters).
5. **`regex_replacements`** -- precompiled `commands.jsonc :: regex_replacements` (arg-dropping,
   arg-aware dispatch).
6. **`rewrite_attach_effect_calls`** -- `attachEffect(...)` → `match.createVfx(new
   VfxStats({…}), self)`. `build_vfx_head` emits a `global_vfx_map` constant route or a
   per-character `getResource().getContent(...)` route; per-prop translation via `commands.jsonc
   :: attach_effect_props` (`Simple` renames, `Detailed` 1→N `expand_to`, `todo`).
7. **`strip_last_frame_end_animation`** -- drop the redundant `self.endAnimation();` on the last
   frame.
8. **`comment_out_unknown_calls`** -- `ssf2_only` names → `// [SSF2-only: NAME]`, counted into
   `ConversionLog::ssf2_only`. also records any `.NAME(` absent from every `commands.jsonc`
   section into `ConversionLog::unknown` (drives the GUI "Unhandled Calls" popup).

two whole-script passes also run (in `haxe_gen`): `fix_intangibility_pairs` collapses a
`setIntangibility(true)` / `(false)` pair into one `applyGlobalBodyStatus(BodyStatus.INTANGIBLE,
N)`; `wrap_persistent_state` rewrites SSF2 ext-class instance vars into FM persistent-state
wrappers (`self.foo++` → `foo.inc()`, etc.), with factory choice from `infer_ext_var_types`.

### 5.6 generation layer

#### `haxe_gen.rs` (~1750 LOC)
the **output orchestrator**: `generate(...)` writes the whole character package (pipeline step
[10]). it installs the `EffectAnimGuard` (effect → primary-animation map), decides the jab chain
via `count_populated_jabs` (`populated_jabs == 2` keeps `jab3` as an allowlisted empty
placeholder), and runs the stat-scaling wrappers (`ssf2_gravity_to_fm`, …) that route every
value through `stats.jsonc :: multipliers`. it emits the four character `.hx` files via
`generate_hitbox_stats` / `generate_character_stats` / `generate_animation_stats` /
`generate_script` (`generate_script` merges SSF2 ext methods matching template fns like
`initialize` / `update` into the template, keeping the names). `generate_manifest` writes
`library/manifest.json`; `generate_projectile_script` uses a local-state-machine
(`Common.initLocalStateMachine()` + one `LState` per SSF2 frame label); the
`generate_projectile_*` fns pull real values from `getProjectileStats()` data via
`best_match_projectile_data`. `generate_jab_scripts` runs only when `populated_jabs >= 2`,
`generate_sound_entries` writes per-WAV `.meta` sidecars, and `script_meta` emits the `.hx.meta`
sidecar variants.

#### `entity_gen.rs` (~2300 LOC)
builds the Fraymakers `.entity` JSON. `generate_entity` / `generate_entity_with_palette` write
`library/entities/<Pascal>.entity`; `double_keyframe_lengths` doubles every keyframe `length`
for 30 → 60 fps ([§7](#7-30--60-fps-doubling)); an empty-animation drop pass removes IMAGE-empty
anims except a `populated_jabs`-driven `keep_empty` allowlist. `generate_menu_entity` builds the
head entity (`full` / `css` / `icon` / `stock` / ten `hud*` variants);
`generate_projectile_entity` handles multi-state projectiles (anim names from inner FrameLabels,
else the FM template trio); `generate_effect_entity` emits one image-only `.entity` per effect.
`box_type_to_fm` & friends do the SSF2-box → FM-layer mapping (table in `AGENT_CONTEXT.md`).
COLLISION_BOX and IMAGE rotation both use the CW-positive, 0-360-normalized convention (no
negation, matching SWF's `atan2(b, a)`).

#### `palette_gen.rs`
`generate_palettes_and_remap(...)`: `build_from_ssf2` is the real path (uses `misc.ssf` costume
tables, ~15 costumes); `build_from_sprites` / `kmeans` / `rotate_hue` is the fallback (k-means
from idle-sprite pixels + hue-rotated team variants). `build_output` writes `costumes.palettes`,
its `.meta`, and the preview PNG.

#### `fraytools_project.rs`
`generate_fraytools_project(char_name)`: the `<name>.fraytools` descriptor, including the
default collision-box layer preset (hitbox / hurtbox / grabbox / counterbox / reflectbox /
ledgegrabbox / holdbox / absorbbox / custom × 3) and the `frame_rate: 60` / `paletteShaderMode:
"RG_MAP"` settings.

#### `uuid_gen.rs`
`det_uuid(seed)` -- RFC-4122 **UUID v5** (SHA-1 namespace). every GUID is
`det_uuid("{char_id}::{context}")`, which is what makes conversions reproducible.

### 5.6b debug environment variables

env-gated diagnostics on the normal `peptide convert` / `peptide ssf2 stage` runs, no
rebuild or special binary needed:

| Variable | Effect |
|---|---|
| `PEPTIDE_STAGE_DEBUG` | stage parse trace: per-instance art walk, plane map, element emission (`[bg-emit]` name/frames/canvas/holds), placement modifiers (clip_depth/blend/cxform), platform + faller extraction |
| `PEPTIDE_DUMP_CLASS=<name>` | decompile every instance method of an AS3 class in the input |
| `PEPTIDE_CLIP_TAGS=<ids>` | raw timeline dump of a sprite (frames, labels, PlaceObject depth + ratio) |
| `PEPTIDE_DUMP_SHAPE=<ids>` | a shape's bounds + fill styles, gradients with stops + matrices |
| `PEPTIDE_DUMP_BMPS=<ids>` | write bitmaps to `/tmp/bmp_<id>.png` |
| `PEPTIDE_WALK_TRACE=<substr>` | trace clip placements whose name/symbol matches during the stage walk |
| `PEPTIDE_DUMP_ANIM_LABELS` | each character source animation's internal frame labels + content-switch frames |
| `PEPTIDE_BG_INLINE` | force backdrop elements baked into the stage layer (debug/diff aid) |
| `PEPTIDE_DECOMP_HAZARD` | emit stage hazards via the decompiler reconstruction instead of the template |

### 5.7 diagnostic binaries (`src/bin/`)

**reverse-engineering / debugging tools**, separate from the conversion, each its own executable
in `build/release/`. useful when a conversion looks wrong. the everyday SWF-inspection set:

| Binary | Purpose |
|---|---|
| `dump_sprites` | List every `DefineSprite` symbol in a SWF |
| `dump_images` | List all extracted bitmaps |
| `dump_image_placement` | Per-frame `PlaceObject` data for a named sprite |
| `dump_collision_box` | Collision-box geometry for an animation |
| `dump_shape_bounds` | Measure the true bounds of the `CollisonBox` shape |
| `dump_shape_origins` | Shape origin offsets |
| `dump_pivots` | Pivot points |
| `dump_frame_labels` | Frame labels inside a sprite timeline |
| `dump_raw_frame` | Raw tag dump for one frame |
| `dump_inner_sprite` | Inspect a nested (effect) sprite |
| `dump_proj_states` | Projectile state discovery |
| `dump_stage` | Stage / root timeline inspection |
| `dump_costumes` | Costume tables from `misc.ssf` |
| `dump_aerial_down_frames` | Targeted debug for the aerial-down animation |
| `dump_trail_matrices` | Trail-effect matrix inspection |
| `check_shape_bitmap` | Inspect a shape's bitmap fill + fill matrix |
| `what_is_id` | Identify what a numeric SWF character id refers to |

plus a set of corpus-audit / detection bins for constructor-walk and sub-character work:
`audit_main_iinit`, `audit_main_gets`, `find_transformations`, `find_get_callers`,
`decompile_main_iinit`, `dump_main_class`, `find_char_sources`, `diff_inline_vs_bundle`,
`dump_misc`, `find_char_mcs`, `grep_classes` / `scan_all_classes` / `scan_ext`.

typical use: `./build/release/dump_image_placement ../ssf2-ssfs/mario.ssf "FAir_42"`.

---

## 6. mapping files (`mappings/`)

most of the converter's *behaviour* is data -- JSONC files loaded at runtime
with `include_str!`'d defaults as a guaranteed-valid fallback. edit these
files (the JSONC, not the Rust source) to tune the conversion. they live in
`mappings/` at the repo root.

### 6.1 `mappings/commands.jsonc` (universal SSF2 → FM API conversions)

sections, in the order they're applied by `translate_ssf2_to_fm`:

- **`replacements`** -- ordered literal find/replace pairs. ORDER MATTERS.
  covers the bulk of SSF2 → FM API renames (`self.self.` → `self.`,
  `.endAttack()` → `.endAnimation()`, `.setXSpeed(` → `.setXVelocity(`,
  `SSF2API.print(` → `Engine.log(`, hitbox field renames like `direction:` →
  `angle:`, etc.).
- **`regex_replacements`** -- `{ pattern, replacement, note? }`. compiled
  once into a `OnceLock` cache; bad patterns log a warning and are skipped.
  used for cases the literal table can't express (arg-dropping, arg-aware
  dispatch).
- **`call_splits`** -- map of `<source_method>` → `{ fields: { <ssf2_field>
  → <"target_method.fm_field"> | { target?, value_map?, skip_if_value?,
  todo? } } }`. drives the fan-out of SSF2 umbrella calls into multiple FM
  target methods (currently `updateAttackStats` → `updateAnimationStats` +
  `updateHitboxStats`). fields sharing a target method are GROUPED into a
  single combined call. fields with no mapping become `// TODO:` comments
  above the emitted calls.
- **`attach_effect_props`** -- map of `<ssf2_prop>` → `"fmPropName"` |
  `{ target?, expand_to?, todo? }`. drives the inline translation that
  injects translated fields into `new VfxStats({…})`. `expand_to` lets one
  SSF2 prop become several FM props (e.g. `parentLock` →
  `relativeWith` + `resizeWith` + `flipWith`).
- **`global_vfx_map`** -- `<effect_name>` → `<GlobalVfx constant name>`.
  names listed here are rewritten to `spriteContent: "global::vfx.vfx",
  animation: GlobalVfx.<C>` instead of the per-character resource lookup.
- **`frame_params`** -- `[{ kind: "call" | "field", name, arg?, isframe,
  sentinel? }]`. drives 30 → 60 fps doubling. `kind: "call"` doubles the
  literal at positional arg index `arg` of every `name(...)` call. `kind:
  "field"` doubles the literal that follows `name:`. only entries with
  `isframe: true` are doubled; values ≥ `sentinel` are left alone (e.g.
  255 / -1 "no override" sentinels for hitStun).
- **`passthrough_fm_apis`** -- `[{ name, note? }]`. names listed here are
  "known FM API calls" -- left untouched and suppressed from the
  `conversion_log.json :: unknown` stream.
- **`ssf2_only`** -- `[{ name, note? }]`. names listed here have no FM
  equivalent -- every call site gets replaced with
  `// [SSF2-only: <name>] <original>` and counted in `conversion_log.json
  :: ssf2_only`.

### 6.2 `mappings/character/animations.jsonc`

```jsonc
{
  "ssf2_to_fm":     { "stand": "idle", "a_air_forward": "aerial_forward", ... },
  "label_to_ssf2":  { "nair": "a_air", "fair": "a_air_forward", ... }
}
```

- `ssf2_to_fm` is the primary table; used everywhere SSF2 anim names need
  to become FM anim names (extractor, sprite_parser, image_extractor,
  haxe_gen).
- `label_to_ssf2` is consulted by `sprite_parser::extract_ssf2_anim_name`
  when a sub-sprite symbol's local label (lowercased, suffix-stripped, e.g.
  `NAir` → `nair`) doesn't appear directly in `ssf2_to_fm` -- it bridges the
  sprite-label-shorthand to the SSF2 xframe name.

### 6.3 `mappings/character/stats.jsonc`

drives **every value** in `CharacterStats.hx`. sections:

- **`field_keys`** -- `<fm_field>` → `[<ssf2_key>, …]`. ordered list of
  SSF2 keys to try when extracting an FM stat (the first matching key wins).
- **`multipliers`** -- `<name>` → `{ divisor, target, floor }`. applied as
  `(raw / divisor * target).max(floor)`. referenced by the `scale("name",
  v)` wrappers in `haxe_gen.rs` (gravity / speed / jump / walk / dash /
  air_friction).
- **`offsets`** -- `<stat>` → integer offset added after extraction (used
  for `max_jumps + 1` to bridge SSF2's midair-jumps count and FM's total-
  jumps count).
- **`derivations`** -- `<stat>` → expression string. compiled once with
  `fasteval` (built-ins `max` / `min` plus our `clamp(x, lo, hi)`);
  evaluated against the already-converted stats exposed as variables. used
  for `shortHopSpeed`, `aerialSpeedCap`, etc.
- **`constants`** -- `<field>` → raw JSON value. emitted Haxe-literally for
  fields the converter derives from no SSF2 source (ECB, camera box,
  shield 9-slice, voice IDs).

### 6.4 `mappings/character/hitbox_stats.jsonc`

```jsonc
{
  "fields": [
    { "fm_field": "damage",          "ssf2_keys": ["damage"] },
    { "fm_field": "angle",           "ssf2_keys": ["direction", "angle"] },
    { "fm_field": "baseKnockback",   "ssf2_keys": ["power", "kbConstant", "weightKB"] },
    { "fm_field": "knockbackGrowth", "ssf2_keys": ["kbGrowth"] },
    { "fm_field": "hitstop",         "ssf2_keys": ["hitLag", "hitstop"], "isframe": true },
    { "fm_field": "selfHitstop",     "ssf2_keys": ["selfHitLag"],       "isframe": true },
    { "fm_field": "hitstun",         "ssf2_keys": ["hitStun", "hitstun"], "isframe": true }
  ]
}
```

per FM hitbox field, the converter takes the **max** over all listed SSF2
source keys (an absent key counts as 0). `isframe: true` flags the field as
a frame count → it gets doubled for the 30 → 60 fps move. this same canon
is used by `entity_gen` (character hitboxes), `haxe_gen`
(`generate_projectile_hitbox_stats`), and `extractor::convert_hitboxes`
(single source of truth across the codebase).

---

## 7. 30 → 60 fps doubling

SSF2 plays at 30 fps; Fraymakers at 60 fps. to preserve playback speed,
every frame-count value must be doubled. the converter does this in **two
unified places**:

1. **Timeline lengths** -- `entity_gen::double_keyframe_lengths` doubles the
   `length` field of every keyframe (LABEL, FRAME_SCRIPT, COLLISION_BODY,
   COLLISION_BOX, POINT, IMAGE) right before the entity JSON is finalized.
   FrayTools timelines are laid out purely by sequential keyframe length, so
   doubling every length doubles every layer's span and every keyframe's start
   position in lockstep -- image, collision-box, frame-script and label layers
   can never fall out of sync.

2. **Frame-count arguments / fields in code** --
   `api_mappings::double_frame_counts` doubles specific literal arguments
   and object-literal fields based on `commands.jsonc :: frame_params`. two
   helpers do the work:
   - `double_int_after_marker(code, "name:", skip_at)` for `kind: "field"`.
   - `double_call_arg(code, "fn_name", arg_idx, skip_at)` for `kind:
     "call"` -- bracket-depth-aware so commas inside nested calls / arrays /
     objects don't miscount.

   runs **first** in the translation pipeline so SSF2 field names still
   match the `frame_params` entries (the literal `replacements` pass below
   it renames `hitStun:` → `hitstop:`, etc.). same pass covers Script.hx
   ext methods, decompiled ext-class iinit, and embedded frame scripts in
   the entity -- they all go through `translate_ssf2_to_fm`.

3. **HitboxStats.hx and projectile-hitbox values** -- `entity_gen` and
   `haxe_gen::generate_projectile_hitbox_stats` both consult
   `hitbox_stats.jsonc :: isframe` to decide which extracted hitbox values
   to double (hitstop / hitstun / selfHitstop, with sentinel handling for
   the 255 / -1 "no override" values).

---

## 8. input format: SSF2 `.ssf`

summary of what the code assumes -- full detail in
[`AGENT_CONTEXT.md`](AGENT_CONTEXT.md).

- a `.ssf` file is a **renamed/SSF-wrapped SWF (Flash)**. `ssf.rs` unwraps it.
- **character logic, stats, attack tables and costume data live in AS3 bytecode**
  (`DoABC` tags) -- parsed by `abc_parser`, decompiled by `decompiler`.
- **sprites, animation timelines and collision boxes live in the SWF display
  list** -- `DefineBitsLossless` / `DefineBitsJpeg3` / `DefineShape` /
  `DefineSprite` tags with `PlaceObject` / `ShowFrame` / `RemoveObject` --
  parsed by `sprite_parser` / `image_extractor`.
- the roster is detected by **walking `Main`'s constructor** for its
  `register("characters", [self.getX(), …])` table (see §4 / §5.1); each
  character's stats/attacks/projectiles come from the matching `Main::get<X>()`
  bundle method (Stage A) and its behaviour code from the per-character `XxxExt`
  AS3 class where one exists (`MarioExt`, `FoxExt`, … -- Stage B). sub-characters
  sharing an Ext class (Sheik shares `ZeldaExt`) still appear as their own
  `Main::getSheik` bundle, so the constructor walk picks them up.
- animation sprites are named `{char}_fla.{AnimLabel}_{index}` (e.g.
  `mario_fla.FAir_42`).
- collision boxes are a square shape (`CollisonBox_6` -- note SSF2's internal
  typo) scaled/positioned by a `PlaceObject` matrix; box *type* comes from
  the instance name (`attackBox` → hitbox, `hitBox` → hurtbox, `grabBox`,
  `touchBox`, `ledgeBox`, `reflectBox`, …).
- SWF matrices use fixed-point components; translations are in **twips**
  (÷20 = pixels). both SSF2 and Fraymakers use **y-down** screen coordinates,
  so no vertical flip is needed.
- both SWF (`atan2(b, a)`) and FrayTools use **CW-positive rotation** -- the
  converter does NOT negate; it normalizes to `[0, 360)`.
- costume data is in `misc.ssf` → `Misc.as` → `getCostumeData()`: ~15
  costumes per character (Red / Green / Blue / Default + ~11 alts), each
  with parallel `colors` and `replacements` arrays.

---

## 9. output format: Fraymakers character package

entity files and the per-character scripts subdir are named after the
character in **PascalCase**, derived from the `Main::get<X>` method name
(`getMario` → `Mario`, `getBandanaDee` → `BandanaDee`, `getWario_Man` →
`WarioMan`, `getgameandwatch` → `Gameandwatch`) by the `pascal_form` rule.

for the single-character SSF `mario`:

```
characters/mario/
├── mario.fraytools                       FrayTools project file (project settings only)
├── conversion_stats.json                 debug summary of the run
├── conversion_log.json                   unhandled / SSF2-only counts + ssf2_source + validation_warnings
└── library/
    ├── manifest.json (+ .meta)            ← one type:"character" entry + AI + per-projectile entries
    ├── costumes.palettes  (+ .meta)        15 SSF2 costumes as FM palettes
    ├── entities/
    │   ├── Mario.entity                    main entity, PascalCase = character id
    │   ├── Menu.entity                     full / css / icon / stock / hud variants
    │   ├── <projectile>.entity             one per discovered projectile
    │   └── <effect>.entity                 one per discovered VFX sprite
    ├── sprites/
    │   ├── *.png                           extracted frame bitmaps
    │   ├── *.png.meta                      GUID sidecar per PNG (entity refs the GUID)
    │   └── palette_preview.png (+ .meta)
    ├── scripts/
    │   ├── Mario/                          <Pascal>/
    │   │   ├── CharacterStats.hx           movement physics
    │   │   ├── HitboxStats.hx              per-attack hitbox data
    │   │   ├── AnimationStats.hx           animation flags
    │   │   ├── Script.hx                   decompiled character logic
    │   │   └── *.hx.meta
    │   └── Projectile/
    │       └── <Pascal>{Script,Stats,HitboxStats,AnimationStats}.hx (+ .meta)
    └── audio/
        ├── *.wav                           extracted sounds (PCM 16-bit via ffmpeg) — flat for single-char
        └── *.wav.meta                      per-sound content sidecar
```

### multi-character SSFs

`zelda.ssf` (Zelda + Sheik), `bowser.ssf` (Bowser + Giga Bowser), and
`wario.ssf` (Wario + Wario Man) each emit **one shared project**
containing both characters as peer entities -- a forward requirement for
Fraymakers' future transformation API, which needs both forms in one
project to swap between them at runtime. for `zelda.ssf`:

```
characters/zelda/
├── zelda.fraytools                       one project for both characters
├── conversion_log.json                   project-scoped, characters:[…] array (per-char ssf2_source)
└── library/
    ├── manifest.json (+ .meta)            TWO type:"character" entries (zelda + sheik), namespaced ids
    ├── costumes.palettes  (+ .meta)        Zelda's (constructor-walk slot 0)
    ├── costumes.palettes2 (+ .meta)        Sheik's (slot 1 → numeric collision suffix)
    ├── palette_preview.png / .png2         same collision-suffix rule
    ├── entities/
    │   ├── Zelda.entity   Sheik.entity     one character entity each
    │   ├── Zelda_Menu.entity  Sheik_Menu.entity   per-character portraits
    │   └── <projectile>.entity             shared projectile entities
    ├── scripts/
    │   ├── Zelda/   Sheik/                  per-character script subdirs
    │   └── Projectile/                      shared
    ├── sprites/                             shared (PNG names are SSF2 <char>_fla.-prefixed)
    └── audio/
        ├── zelda/*.wav                      per-character audio subdirs (multi-char only)
        └── sheik/*.wav
```

collision-suffix rule: files that would collide at a shared library
path (`costumes.palettes`, `palette_preview.png`) get a numeric suffix
in constructor-walk order -- slot 0 unsuffixed, slot 1 → `2`, etc.
per-character entities/scripts don't collide (the PascalCase name is in
the path). `--per-character-projects` reverts a multi-char SSF to the
single-character standalone layout. transformation forms (Giga Bowser,
Wario Man) carry a TODO banner in `CharacterStats.hx` about the manual
wiring still owed for the not-yet-shipped FM transformation API.

`.entity` files are JSON. the top-level shape is `{ animations[], layers[],
keyframes[], symbols[], paletteMap, pluginMetadata, plugins, version: 14, … }`.
each animation owns an ordered layer stack -- `LABEL`, `FRAME_SCRIPT`,
`COLLISION_BODY`, one `COLLISION_BOX` / `POINT` per box instance, one `IMAGE`
per depth slot -- and layers reference keyframes which reference symbols. the
full schema (every layer/symbol/keyframe shape, the box-type enum, `.meta`
sidecar format) is the ground truth in [`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) --
keep it open when working on `entity_gen.rs`.

projectile scripts use the Annie/aJewelofRarity convention: a **single**
`library/scripts/Projectile/` directory holds files for every projectile,
each prefixed by the projectile name in PascalCase
(`DeeNspecScript.hx`, `DeeNspecStats.hx`, `DeeNspecHitboxStats.hx`,
`DeeNspecAnimationStats.hx`, …).

effects (VFX) are emitted as plain entities under
`library/entities/<effect>.entity` -- one IMAGE layer per inner FrameLabel
segment (or a single `active` animation if there are no labels). no scripts,
no stats. the character's `Script.hx` triggers them via the
`attachEffect` → `match.createVfx(...)` rewrite.

---

## 10. current status -- what works

**the pipeline runs end to end and produces complete, FrayTools-shaped
character packages.** evidence in the repo:

- `characters/sandbag/` -- the smoke-test character (small, simple).
- `characters/mario/`, `characters/fox/`, `characters/link/`,
  `characters/naruto/`, `characters/jigglypuff/`, `characters/samus/`,
  `characters/bandanadee/`, `characters/captainfalcon/`, … -- full
  conversions across the roster.
- multi-character SSFs (`zelda.ssf` → Zelda + Sheik, `bowser.ssf` →
  Bowser + Giga Bowser, `wario.ssf` → Wario + Wario Man) emit a package per
  registered character, so the converter produces more packages than input
  files. all are registered peers in their SSFs' `Main::<init>` rosters.

> **coverage, known issues, and next steps live in [`docs/STATUS.md`](docs/STATUS.md).**
> that doc is the single home for converter status and the TODO list. the
> feature inventory below ("what is solid") stays here.

what is solid:

- `.ssf` → SWF decompression, SWF parsing, ABC parsing.
- **constructor-walk character detection** from `Main`'s iinit
  `register("characters", […])` array; sub-characters (Sheik, Giga
  Bowser, Wario Man) fall out naturally; transformations carry a
  `derived_from` marker driving a TODO banner in `CharacterStats.hx`
  and metadata in `conversion_log.json`. Tier 1 validation hooks
  (empty stats / id ≠ filename / declared-vs-extracted) emit soft logs
  to `conversion_log.json :: validation_warnings`.
- character / stats / attack / frame-script / projectile-stat / ext-var
  extraction from ABC. stats / attacks / projectiles come from the
  `Main::get<X>()` bundle method body (Stage A); behavior code (frame
  scripts, ext methods, ext vars + iinit-derived inits) comes from the
  matching `<X>Ext` class (Stage B).
- the AS3 decompiler (CFG reconstruction, structured control flow,
  short-circuit `&&` / `||` collapsing, loop counter renaming).
- bitmap extraction → PNG, sprite `.meta` generation.
- collision-box geometry extraction; full affine matrix composition with
  root MC transform; CW-positive 0-360 rotation on both IMAGE and
  COLLISION_BOX symbols; itemBox pivot-at-bottom-centre.
- shear-bake fallback for sheared image placements
  (`prerender_skewed_frames`).
- costume/palette extraction from `misc.ssf` (with k-means fallback).
- sound extraction to WAV via `ffmpeg` (Nellymoser, MP3, ADPCM).
- deterministic GUIDs.
- entity JSON assembly: LABEL / FRAME_SCRIPT / COLLISION_BODY /
  COLLISION_BOX / POINT / IMAGE layers; per-frame ECB diamond auto-fit
  to hurtboxes; run-length-encoded keyframes; 30 → 60 fps length doubling.
- multi-state projectile entities (LState local state machine in the
  emitted script).
- per-effect entities (`<name>.entity`) split by inner FrameLabels;
  `match.createVfx(new VfxStats({…}), self)` rewrite for both 1-arg and
  2-arg `attachEffect` calls; `global_vfx_map` constant routing for FM
  built-in VFX.
- persistent-state wrappers for SSF2 ext-class vars (`var foo =
  self.makeInt(0)`; `foo.get()` / `foo.set(v)` / `foo.inc()` /
  `foo.dec()`); ext-class iinit assignments emitted in
  `initialize()` (skip-dup against the merged SSF2 init body).
- jab-chain emission gated by populated-jab count.
- empty-animation dropping (with a jab-aware keep-list).
- `Menu.entity` with broadened head-sprite detection (`_head`, `_icon`;
  excludes `_hud`).
- conversion log + the converter screen's warnings panel.
- Peptide as the single-binary parent product (webview UI; converter folded in as a library).

---

## 11. known issues, next steps, and the code-quality backlog

these now live in **[`docs/STATUS.md`](docs/STATUS.md)** -- the single home for
converter status and TODOs. see its "Known issues & gaps", "Code-quality backlog",
and "Prioritized next steps" sections. when you fix something, strike it there.

---

## 12. tips for the next agent

- **smoke test = `sandbag`.** `./tools/rebuild-sandbag.sh` is the fast loop.
  mario is the heavy, full-featured test (projectiles, costumes, big
  roster of moves).
- **`AGENT_CONTEXT.md` is the format bible** -- formats are reverse-
  engineered and documented nowhere else.
- **edit the JSONC, leave the Rust alone.** most behaviour lives in
  `mappings/`. a new API rename, marking a method SSF2-only, tweaking a
  stat scaling -- all JSONC edits, no rebuild needed beyond a fresh run.
- **reach for the `dump_*` binaries** before guessing. they exist
  precisely so you can inspect a SWF without re-deriving the format.
  example: `./build/release/dump_collision_box ../ssf2-ssfs/mario.ssf
  "a_air_forward"`.
- **inputs are the sibling `../ssf2-ssfs/` folder**, outside this repo
  (`.gitignore` excludes `*.ssf`). `misc.ssf` lives there too and is
  needed for real costume colours.
- **output is git-tracked partially** -- `.hx`, `.json`, `.entity`,
  `.meta` are committed in `characters/`, but binary media (PNGs,
  WAVs) are git-ignored. deleting `characters/` and re-running is safe;
  the entity / script files regenerate identically (deterministic
  GUIDs).
- **GUIDs are deterministic** -- re-running is idempotent w.r.t. GUIDs,
  so diffs between runs reflect real logic changes, no GUID churn.
- **always check `conversion_log.json`** after a conversion. the
  `unknown` list tells you which SSF2 API calls slipped past every
  mapping table; the `ssf2_only` list tells you which calls we
  deliberately gave up on. both are good signals when something feels
  off in the converted output.
- **reference resources** (from `AGENT_CONTEXT.md`): the official
  [Fraymakers character-template](https://github.com/Fraymakers/character-template)
  repo's `library/entities/character.entity` is ground truth for the
  entity format; the [Fraymakers API docs](https://shifterbit.github.io/fraymakers-api-docs/)
  (community-run, high utility -- the reference for engine
  functions/scripts/classes) and
  [SSF2 modding docs](https://ssf2-modding.readthedocs.io/) cover the
  rest.
- **the pipeline is fail-soft.** a stage that errors logs a warning and
  continues -- always scan the run log, `conversion_stats.json`, and
  `conversion_log.json`. a non-crashing run isn't necessarily a clean run.
</content>
</invoke>
