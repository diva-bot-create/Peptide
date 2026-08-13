# Peptide -- SSF2 / Fraymakers format reference (agent context)

this doc is the authoritative reference for AI agents on this codebase. both SSF2's SWF
format and Fraymakers' entity format are largely undocumented, so everything here was
reverse-engineered from first principles during development.

> **Copyright boundary -- do not paste, do not publish.** SSF2 (© McLeodGaming)
> and Fraymakers / FrayTools (© Team Fray) are proprietary. everything here is
> described **in our own words** for interoperability; the JSON shapes below are
> **illustrative examples we authored**, not copied from any copyrighted file. never
> add verbatim third-party source, bytecode, decompiled output, or assets to the
> repo -- see [`NOTICE.md`](NOTICE.md) "Reverse-engineering & copyright boundary".

> ## engine-side knowledge is not in this repo (read this)
>
> to **respect Team Fray's wishes** (and stay on the right side of the law), the
> tracked docs don't explain *how to decompile or patch the Fraymakers
> engine or the FrayTools editor bundle*, and don't document **specific non-hscript
> engine class / function / field names** or the engine's internal symbol map. to keep
> it that way, please don't add the following to any tracked file, commit message, or
> PR:
> - the decompilation / RE *technique* -- how engine symbols are located in the
>   binary, the load-path internals, and the FrayTools bundle render internals.
> - the engine's internal **symbol map** -- engine type names, field slots, function
>   indices, the `CState` integer values, the live-match/character access path, and
>   the named load / dispatch / telemetry functions.
>
> **what's fine to document (and stays):** what Peptide *does* (it boots a patched
> throwaway copy of the local engine and drives it over a loopback protocol), and how
> to **contribute to Peptide, including the patcher** -- the layering, the "minimum
> bytecode, maximum hscript" rule, the resolve-by-name-never-by-index discipline, the
> symbol-manifest + `doctor` preflight workflow, and where new handlers belong. those
> describe *our* code and process, not the engine's internals. the **hscript** scripting
> API (`CState.JAB`, `match.getCharacters()`, `HitboxStats`, …) is the public modding
> surface and is fine to use and show.
>
> **code comments, specifically.** the engine-symbol rule reaches into `.rs` comments,
> not just docs: a comment must not name a Fraymakers/FrayTools engine class, function,
> or field, nor cite an engine code line (`name@12345`). this is the ONLY doc rule that
> carries over to comments -- the lowercase / current-state-only / house-voice rules are
> for docs, comment however you like. the one exception is the source that *is* the symbol
> map's home (`src/manifest.rs`, `connect_edit` in `src/main.rs`, and the
> `crates/ssf2-converter/src/bin/` RE tools), where naming the symbol is the code's job;
> everywhere else describe the behavior, not the engine symbol. enforced by
> `no_engine_internals_in_code_comments` in `tests/conventions.rs`.
>
> scope: this is specifically the **Fraymakers engine and FrayTools**. the
> SSF2 / SWF / AVM2-ABC format notes in this document (the converter's *input* side)
> are unaffected and stay -- they describe McLeodGaming's Flash content the converter
> legitimately reads, not the Fraymakers engine.
>
> **where the engine context actually lives -- read the code, it's the reference.**
> the prose stays high-level on purpose; the precise engine surface is in the source,
> which is canonical. to orient:
> - **`src/manifest.rs` -- the `MANIFEST` table.** the in-repo map of every engine
>   symbol the patcher depends on, grouped by subsystem (`socket-bridge`, `boot`,
>   `content`, `hscript-eval`, `line-cmd`, `move-dispatch`, `telemetry`, `console`).
>   every entry has a `why` string explaining its role. reading it top to bottom is the
>   fastest way to learn the engine functions/fields/types the harness touches and how
>   they fit together -- start here!
> - **`connect_edit` in `src/main.rs`** -- the patch + dispatch shape (what gets spliced
>   in, the wire handlers, the load/spawn flow). the resolver helpers it calls
>   (`find_fn`/`require_fn`, `find_type`, `find_field`, `find_native`) each name the
>   symbol they look up; follow them for the exact surface.
> - **`commands.hsx`** -- the in-engine hscript vocabulary (the live match/character
>   access the harness binds), and **`src/interpreter.rs`** -- the host-side command
>   routing.
> - **the `peptide` read-only inspection subcommands** (`doctor`, `inspect`, `fnsof`,
>   `typefields`, `fninfo`, `dis`, `callers`, `strgrep`, `whoref`) re-resolve anything
>   that moved in a new build, against your *local* engine. `doctor` resolves the whole
>   `MANIFEST` and prints a pass/fail checklist. resolve by name, re-verify by
>   disassembly; treat every index/slot as build-specific.
> - for the few runtime values that aren't compile-time dependencies (e.g. state ids)
>   and for longer narrative, keep notes in the gitignored `docs/` scratch space
>   (`docs/ENGINE_INTERNALS.local.md` is the seed). **never** promote engine symbol
>   maps or the decompile/patch how-to into a tracked doc, and never paste verbatim
>   engine bytecode / Haxe / disassembly / strings / assets anywhere.

> **structure note.** **Peptide is the parent product** and the single shipping binary
> at the repo root (`src/`, package `peptide`). the SSF2 → Fraymakers converter is a
> **library crate** at `crates/ssf2-converter/` (package-named `ssf2_converter`);
> conversion runs in-process via `run_conversion` / `peptide convert <file.ssf>`, no
> standalone converter binary. Peptide specifics:
> [`docs/PEPTIDE_GUIDE.md`](docs/PEPTIDE_GUIDE.md) (usage) and
> [`docs/PEPTIDE_DESIGN.md`](docs/PEPTIDE_DESIGN.md) (internals).

> **test corpus (SSF2 inputs) -- you bring your own.** the `.ssf` files the converter
> reads aren't in the repo (they're SSF2 game content, © McLeodGaming). by default they're
> expected at the sibling `../ssf2-ssfs/`; set **`$SSF2_SSFS_DIR`** to point somewhere else.
> you don't need them to build or to `cargo test` -- corpus-dependent tests check for the
> files and skip cleanly when they're absent (the skip line tells you how to point at the
> corpus). full convention in [`DEVELOPMENT.md`](DEVELOPMENT.md) §2 "where the test inputs
> live"; the shared resolver is `crates/ssf2-converter/tests/common/mod.rs`.

> **architecture convention -- minimum bytecode, maximum hscript (read before every commit).**
> hand-emitted engine bytecode in `src/main.rs` (`connect_edit` and its `inject_*`
> helpers) is the most expensive and fragile code in the repo: a wrong register type or
> jump offset silently corrupts the engine and can only be caught by launching the game.
> so: **add only the minimum bytecode needed to make a thing possible, and implement as
> much of the behaviour as you can in `commands.hsx`.** bytecode is allowed -- just keep
> it to the irreducible engine-side primitive that hscript/host can't otherwise reach.
> where each kind of logic belongs:
> - **`commands.hsx` (hscript in the engine's own interpreter via the `e` hook) -- the
>   DEFAULT for engine-side behaviour.** anything you can express by calling the script
>   API or the bound values (p0/match/CState/…) goes here, not in bytecode.
> - **`src/interpreter.rs` (host-side Rust, runs in the Peptide process)** for anything
>   Peptide can do itself: translating commands, parsing `error.log`, building diagnostics,
>   routing TCP channels, reading character files. no engine round-trip needed → host-side.
> - **bytecode (`connect_edit`/`inject_*`) -- only the minimum primitive** hscript/host
>   can't reach: the socket bootstrap, per-frame dispatch, the command hooks, or binding a
>   value into the eval scope. push everything ELSE up into hscript.
>
> before adding to `connect_edit`/`inject_*`, ask: is this the *smallest* engine primitive
> that unblocks the feature, with the rest done in `commands.hsx` / `interpreter.rs`? (do
> NOT document the specific engine class/function/field names you patch against -- see
> "engine-side knowledge is not in this repo" below.)

> **architecture convention -- ONE command vocabulary, TWO engines (read before adding any
> host-facing feature).** Peptide debugs both Fraymakers and SSF2. a command or feature must
> behave **identically** on both -- the user types the same thing and it does the equivalent
> thing regardless of which engine is attached. this is enforced by a layered OOP seam;
> respect the layer boundaries:
> - **`src/interpreter.rs` -- the engine-agnostic vocabulary.** `parse()` turns a typed line
>   into a `Command`; `src/vocab.rs` declares the `commands.hsx` vocabulary + the FM↔SSF2
>   name reconciliation. new command syntax is defined **here, once**, never per-engine.
> - **`src/debug_target.rs` -- the `DebugTarget` trait (the feature surface).** every
>   host-facing feature is a trait method. give it a DEFAULT that just evaluates the engine
>   helper of the same name (`match_status()` → `eval("matchStatus()")`, etc.). both backends
>   implement `eval`, so a new feature reaches **both** engines the moment each engine's eval
>   knows the expression -- Fraymakers via `commands.hsx`, SSF2 via `src/ssf2_target.rs`. an
>   engine that genuinely can't do a feature *overrides the method to say so* (e.g. SSF2
>   `char_icon` → `None`); it doesn't get silently special-cased in the caller.
> - **backends (`FraymakersTarget`, `Ssf2Target`) + the transport** are the ONLY place
>   engine differences live. everything above them is shared.
> - **passive host features (a stream the engine emits, or a display) are shared the same
>   way.** the live debugger overlay (`src/overlay.rs`) is engine-agnostic: it tails the
>   session `out.log` and renders it, and BOTH the Fraymakers (`bridge`) and SSF2
>   (`ssf2_bridge`) sessions spawn it through the one `overlay::spawn_for_session`. anything
>   the engine *surfaces into that log* is the per-engine half: surfacing trapped script
>   errors as a `SCRIPTERR:` line is the FM engine's job (done by the bytecode patch); SSF2
>   surfaces its own errors through its bridge. the overlay just shows whichever arrives. so
>   the feature ("see errors + state over the game") is one host feature on both engines; only
>   *how each engine produces the data* differs, and an engine that can't produce some of it is
>   a capability gap (an empty field), not a special case in the host.
>
> THE RULE: **never write `if engine == fraymakers { … } else { … }` in feature/command logic.**
> if you reach for that, the difference belongs lower down -- in `vocab.rs`, a trait-method
> override, or the transport. the GUI's per-engine branching is limited to the transport
> shell (which socket to talk to); the moment it creeps into *what a command does*, you've
> broken the contract. a new feature should be: add a `DebugTarget` method (default = eval) +
> implement the expression in `commands.hsx` AND `ssf2_target` -- then it "just works" on both!
> see [`docs/PEPTIDE_DESIGN.md`](docs/PEPTIDE_DESIGN.md) for the full seam.

cross-reference: [`DEVELOPMENT.md`](DEVELOPMENT.md) covers build / pipeline / modules and
the current set of mapping JSONC files; this file covers the **input** and **output**
formats themselves.

---

## reference resources

always check these before guessing about either format:
- **Fraymakers API docs** (community-run, high utility -- start here for any
  question about engine functions/scripts/classes): https://shifterbit.github.io/fraymakers-api-docs/
- **Fraymakers character template** (official): https://github.com/Fraymakers/character-template
- **SSF2 modding docs**: https://ssf2-modding.readthedocs.io/en/latest/reference/index.html

the character template's `library/entities/character.entity` is the ground truth for entity format.
it's a 2.67 MB file -- parse it programmatically with `curl | python3`, not web_fetch (truncates).

```bash
curl -s https://raw.githubusercontent.com/Fraymakers/character-template/main/library/entities/character.entity | python3 -c "
import sys, json; obj = json.load(sys.stdin)
# inspect layers, keyframes, symbols as needed
"
```

---

## what this tool does

converts Super Smash Flash 2 (SSF2) character `.ssf` files into Fraymakers character packages
compatible with FrayTools. it extracts:
- bitmap images (PNG sprites per frame)
- collision box data (hitboxes, hurtboxes, grab/ledge/reflect/absorb boxes) per animation frame
- frame scripts (decompiled from ABC bytecode and rewritten through the JSONC command table)
- sound references (WAV via `ffmpeg`)
- palette / costume data (15 costumes per character from `misc.ssf`)
- projectile and effect sprites as standalone `.entity` files
- a menu / portrait entity for the character-select screen

output is a FrayTools character package directory (full layout in [`README.md`](README.md) and
[`DEVELOPMENT.md`](DEVELOPMENT.md) §7).

---

## SSF2 (.ssf) file format

`.ssf` files are SSF-wrapped SWF files. the unwrapper lives in `src/ssf.rs`:

- a `.ssf` is either a raw SWF (`FWS` / `CWS` / `ZWS` magic -- passed through), or
- an SSF-wrapped file: `u32 swf_len` + `u32 garbage_header_size` + zlib payload.

after unwrapping, `swf_parser::parse` uses the Ruffle `swf` crate (`decompress_swf` +
`parse_swf`) to turn the bytes into a tag tree.

### what a package must contain to LOAD (not just to parse)

peptide's reader is deliberately forgiving, so a stage package can convert cleanly and still be a
file the game will not open: the container form, the tag ORDER, the class graph, the clip tree and
the api layer are all load-bearing, and none of them announce themselves when wrong.

that is its own subject and it has its own home:
[`docs/SSF2_PACKAGE_FORMAT.md`](docs/SSF2_PACKAGE_FORMAT.md) covers every requirement, why each one
fails silently, and how to diagnose a package that will not load.

### SWF structure for SSF2 characters

```
SymbolClass        → maps char_id (u16) → class name string
DefineBitsLossless → raw bitmap data (palette / RGBA / ARGB)
DefineBitsJpeg3    → JPEG image data (with optional zlib alpha mask)
DefineShape        → shape with a bitmap fill (wraps a DefineBits*)
DefineSprite       → animation timeline (PlaceObject/ShowFrame/RemoveObject tags)
DoABC / DoABC2     → AS3 bytecode blocks (the character's logic + stats + costume tables)
DefineSound        → audio (Nellymoser, MP3, ADPCM)
```

### animation sprites

each character animation lives in a named `DefineSprite`:
- name format: `{char}_fla.{AnimLabel}_{index}` e.g. `mario_fla.FAir_42`
- the root MC (main timeline) places these sprites at specific frame labels (`stance` placements)
- each animation sprite contains a sequence of PlaceObject/ShowFrame/RemoveObject tags

the character's main timeline (root) is a DefineSprite whose `SymbolClass` name matches
the character id exactly (e.g. `mario`, `fox`). frame labels on the root MC drive the
`xframe` map: SSF2 frame methods assign an animation label to an `xframe` field, recovered
by the extractor from the ABC bytecode.

### root MC transform

every animation sprite is placed by the root MC with a transform:
```
tx, ty  = world offset of the character origin (typically negative, e.g. -24.70, -55.30)
sx, sy  = character scale (typically 1.1 for Mario)
```
all child positions must be composed through this transform via the full affine matrix
(a, b, c, d, tx, ty) -- translation × scale alone won't do it, since some root placements rotate.
see `sprite_parser::XframeTransform` and `image_extractor::ImageLocalMatrix`.

### SWF matrix decomposition

SWF PlaceObject matrices use `Fixed16` (fixed-point) values:
```
a, b, c, d = matrix components (b and c carry shear/rotation)
tx, ty     = translation in TWIPS (divide by 20 to get pixels)
```

decompose into scale/rotation:
```rust
scale_x = sqrt(a² + b²)
scale_y = sqrt(c² + d²)
// Flip detection: encode the sign in scale_y when the determinant is negative.
let det = a*d - b*c;
let sy  = if det < 0.0 { -scale_y } else { scale_y };
rotation_deg = atan2(b, a).to_degrees()
```

### rotation convention -- both SWF and FrayTools are CW-positive

SWF's `atan2(b, a)` in y-down screen space and FrayTools' rotation field use the
**same CW-positive convention**. **don't negate.** the converter emits rotation
values normalized to the `[0, 360)` range:

```rust
rotation: round2(((swf_rotation % 360.0) + 360.0) % 360.0)
```

this applies to **both** IMAGE symbols and COLLISION_BOX symbols. the
`((swf_rotation % 360.0) + 360.0) % 360.0` formula is the source of truth.

### skew handling

FrayTools' IMAGE keyframe can express translation + rotation + scaleX + scaleY, but
**not shear**. when a SWF placement matrix has shear (non-perpendicular x/y column
vectors, detected via `ImageLocalMatrix::has_skew()`), `image_extractor::prerender_skewed_frames`
bakes the world-space linear part into a fresh PNG and rewrites the placement as a plain
translation. non-sheared placements (pure rotation + scale + flip) take the faithful
scale+rotation path.

### collision boxes

SSF2 encodes ALL collision-box data in the SWF timeline, not in AS3 code.

the collision-box character (typically `CollisonBox_6` -- note SSF2's internal typo
"Collison") is a small square shape (~100×100 unit, scaled per instance by the
PlaceObject matrix). `sprite_parser::find_collision_box_base_size` measures the
shape's true bounds at runtime (not hardcoded):
```
width   = |scale_x| * BASE_SIZE
height  = |scale_y| * BASE_SIZE
center_x = tx_pixels   (tx/ty ARE the centre, not top-left)
top_left_x = center_x - width/2
top_left_y = center_y - height/2
```

#### box instance names → BoxType

the SSF2 instance name on the PlaceObject determines the FM box type. see
`sprite_parser::BoxType::from_instance_name` and `entity_gen::box_type_to_fm`:

| SSF2 instance name | Fraymakers type | Notes |
|---|---|---|
| `attackBox`, `attackBox2`… | `HIT_BOX` | Active hitbox |
| `hitBox`, `hitBox2`… | `HURT_BOX` | Hurtbox (SSF2 "hit" = FM "hurt") |
| `hurtBox` | `HURT_BOX` | Hurtbox (alternate name) |
| `grabBox` | `GRAB_BOX` | Grab range |
| `itemBox` | `HURT_BOX` | Item pickup -- no native FM equivalent; emitted as hurtbox |
| `touchBox` | `GRAB_HOLD_POINT` | **POINT layer** (not COLLISION_BOX) |
| `shieldBox` | `REFLECT_BOX` | |
| `reflectBox` | `REFLECT_BOX` | |
| `absorbBox` | `COUNTER_BOX` | |
| `ledgeBox` / `ledgegrabbox` | `LEDGE_GRAB_BOX` | |
| anything else `*box` | `HURT_BOX` | Fallback |

**`touchBox` → grabholdpoint (verified):**
- layer type is `POINT`, not `COLLISION_BOX`.
- layer name is `grabholdpoint<N>`.
- `pluginMetadata["com.fraymakers.FraymakersMetadata"]` uses `pointType: "GRAB_HOLD_POINT"`.
- symbol is type `POINT` with just `x` / `y` / `alpha` / `color` / `rotation` (bottom-centre
  of the touchBox area -- where the opponent's feet anchor).
- verified against the official Fraymakers character template `grab_hold` animation.

#### itemBox special case

`itemBox` (typically id=991) is placed relative to the **hand attachment point**
(bottom-centre of the box). the PlaceObject tx/ty is the hand position; the inner
shape geometry hangs upward from it (inner_w ≈ 3.7, inner_h ≈ 21.9 pixels). the
emitted COLLISION_BOX symbol uses `pivotY = height` (instead of `height / 2`) so
rotation pivots around the hand. `itemBox` is the only routinely rotated collision
box, so the pivot-at-bottom convention matters here.

### graphic-symbol placements (applies to characters, stages, projectiles)

a sprite PlaceObject that carries a `ratio` is a Graphic-symbol placement (Flash's
"Single Frame" insert): it shows the ONE frame the ratio selects, never self-animates,
and never runs its frame scripts. a placement with `ratio=None` is a real MovieClip.
Replace (and move-style Place) tags without a matrix/name/ratio KEEP the depth slot's
existing state and only swap the character, so a slot's transform persists across
Replace chains (bowserscastle's bubble pool cycles 8 slots this way).

### custom game object scripting contracts (live-verified)

hscript-level behavior of the public CGO scripting surface, learned the hard way.
each of these cost real debugging time, so trust them before re-deriving:

- `make*` slots cannot be dereferenced at module scope on the first eval (the api
  is not live yet, `.get()` null-accesses). declare at module scope, deref only
  inside functions. module scope re-runs every frame, so plain `var`s reset each
  frame and `make*` slots are the only cross-frame state.
- local-state calls go through the `Common.` binding
  (`Common.initLocalStateMachine()` etc.), registered at module scope each eval,
  the template idiom.
- a CGO outside the death bounds is culled and respawned with its script state
  WIPED, every frame it stays outside. spawn wrappers lean on this deliberately
  (fresh instance = fresh timers), but a probe that teleports its reporter
  off-stage measures the wipe, not what it meant to measure.
- a moving game object needs an ECB in its GameObjectStats
  (`floor/aerialFootPosition`, `HeadPosition`, `HipWidth`) or it falls straight
  through stage terrain and `isOnFloor()` never fires. size it from the hazard's
  own SSF2 collision box. with an ECB it will also ground on its OWN follower
  structures, so a self-platform structure must `addToBlacklist` its hazard.
- stage hazards set `immovable: true` (shove fighters, ignore windboxes, never
  pushed back), the SSF2 enemy contract.
- `self.makeFrameTimer(frames)` with `tick()`/`completed`/`reset()` is the real
  FrameTimer and lowers 1:1 from SSF2's (both count UP to a length).

### Fraymakers canonical character animation names

from the public `Fraymakers/character-template` repo (`character.entity`): the
airborne set is `fall_in` / `fall_loop` / `fall_special` / `tumble`, there is NO
plain "fall". walk and crouch follow the same `_in` / `_loop` split. the hurt set
is `hurt_light_low/middle/high`, `hurt_medium`, `hurt_heavy`, `hurt_thrown`.
SSF2's launch sequence is two xframes: "flying" (the hitstun somersault) maps to
`hurt_heavy`, then "falling" (the drifting tumble) maps to `tumble`. refer to
SSF2 animations by xframe name or linkage id only, never by fla symbol names.

### SSF2 stage structure

some of what SSF2 shows at runtime is NOT in the stage file at all: the engine
mounts quality-tier effect layers from its own library (bowserscastle's animated
lava sheet + `lavafade` gradient occur once in the stage SWF bytes, the ABC pool
reference only, no placement). the converter repairs the visible gap data-driven:
an art layer whose bottom edge lands inside a static stage-wide hazard region is
extended by bottom-row repeat with a fade below the death bounds.

a stage `.ssf` is a SWF whose document root places two things: the `<id>_bg` backdrop
container (linkage `<id>_bg`, no instance name) and the `stageMC` (linkage `stage_<id>`).
the AS3 `stage_<id>` class exposes the stage to the engine via named slots, and the placement
uses those same INSTANCE names. identify layers by instance name + linkage id + the AS3
slots, NOT by the fla-prefixed timeline symbol names (`<id>_fla.<thing>_<n>`), which are
auto-generated and don't generalize.

the planes (stage-root child instance names = AS3 slots), back to front:

| instance name | role | maps to |
|---|---|---|
| `<id>_bg` (linkage, unnamed) | painted backdrop, fixed (moves 1:1 with the world) | FM background IMAGE |
| `*_cambg` (inside `<id>_bg`) | camera-relative parallax layers (`getCameraBackgrounds`) | FM camera background |
| `background` | fixed backdrop plane | FM background IMAGE |
| `terrain` | collision masks (invisible in SSF2) | FM collision boxes / line segments |
| `foreground` / `*_fg` | draws in front of fighters | FM foreground IMAGE |
| `shadowMask` / `reflectionMask` | masks (not art) | dropped |
| `stance` | spawn-pose beacons | dropped (spawns come from the markers below) |

markers live inside `terrain`, identified by LINKAGE suffix (no instance name): `*TerrainMC*`
(solid floor), `terrainGround_platform*` (drop-through), `CollisonBox*` [sic], `ledge_mc_*`,
`pN_Start` / `pN_Spawn` (spawns), `boundary_clip` / `deathBoundary` / `camBoundary`,
`warningbounds_*`, `itemGen_mc`.

the `SSF2Stage` AS3 class (visible in any stage's ABC via `ssf2_objgraph <stage.ssf> slots
SSF2Stage`) confirms the model: `getBackground` / `getMidground` / `getForeground` are the
fixed planes; `getCameraBackgrounds` is the separate parallax system. parallax is rare (1 of
110 corpus stages, junglehijinx); the rest have a single fixed backdrop.

**parallax math (from the SSF2 game engine `Vcam`/`VcamBG`/`VcamBGSettings`).** each
camera-background layer (`<id>_bg` content + the `_cambg` layers) is its OWN parallax plane,
and SSF2 auto-derives its pan rate from the layer's pixel size vs the 640x360 game view:
```
xPanMultiplier = (bgWidth  - 640) / (2 * bgWidth)     (clamp < 0 to 0)
yPanMultiplier = (bgHeight - 360) / (2 * bgHeight)
```
so a wide layer scrolls slower-than-stage by a larger factor and a layer <= the view size is
screen-fixed (multiplier 0). this is per-LAYER (a 3095px sunray plane -> 0.40, a 1255px tree
plane -> 0.245, a 396px sun -> 0), which is what makes the depth read. `VcamBGSettings` is
field-for-field the same as the Fraymakers camera-background config (`xPanMultiplier`/`mode`/
`autoPanMultiplier`/`foreground`/... -- shared McLeodGaming lineage), so the converter computes
each layer's multiplier with the formula above (native px) and emits it explicitly per layer.

**PAN vs BOUNDS mode (`VcamBGSettings.mode`).** SSF2 has two camera-background modes:
- `PAN`: `Vcam.syncPositions` sets `bg.x = centX + cameraOffset * xPanMultiplier` -- a straight
  parallax pan at the per-layer multiplier. this is what the `_cambg` layers use (the
  `autoPanMultiplier` formula above feeds `xPanMultiplier`, which only `PAN` consumes), so it's
  the correct/default mode for converted parallax.
- `BOUNDS` (with `horizontalScroll`): `Vcam.changeBG` makes left/right copies of the sprite at
  `±width_override` so it tiles seamlessly, anchored to the camera bounds -- for a repeating
  backdrop (clouds/gradient that wraps), not discrete elements.
both map to `ParallaxMode.PAN` / `ParallaxMode.BOUNDS` on the Fraymakers side and the converter
emits either; it defaults to PAN (no corpus stage tiles) and uses BOUNDS for a layer flagged to
loop.

**scale.** Fraymakers space is SSF2 space scaled up by `size_multiplier` (the same knob the
character converter scales sprites by, default 1.3; in world-units-per-second the spatial
scale works out to exactly `size_multiplier`). so the stage parser scales every coordinate
(floor / platforms / bounds / spawns / ledges + art placement) by it, and the emitter renders
the native-resolution art IMAGE layers at that scale. a 1:1 stage would be too small for the
scaled-up fighters, and the backdrop wouldn't fill the FM camera.

### image sprites

each animation's visual content sits inside its DefineSprite as a stack of `PlaceObject`
tags placing one or more named `DefineSprite`s (the sub-sprites) or unnamed effect
sprites. `image_extractor::build_anim_frame_images` walks each animation's display list
and records, per frame, every placed image with its full world-space affine matrix
(`FrameImageEntry`).

#### effect sprites

some animations contain nested effect movieclips (e.g. `mario_fla.ChargeSpark_25`):
`_fla.`-named sub-sprites that aren't top-level animation containers. the image
extractor flattens them -- composes each effect frame's content into the parent
timeline via matrix multiplication (`ImageLocalMatrix::compose`).

pure-vector effect shapes (solid-colour fills, no bitmap) **cannot be rendered**
without a full SWF vector rasterizer, so they're silently skipped -- only bitmap-backed
shapes are exported. affected examples include some sparkle / charge effects.

#### shape_to_bitmap map

`DefineShape` tags may have multiple fill entries:
1. `id = 65535` -- the SWF null / clipping bitmap (**always skip**).
2. the real bitmap id.

the extractor takes the first non-65535 bitmap fill. `image_extractor::shape_pivot`
captures where the shape's local (0,0) lands inside its bitmap (from the fill matrix
`tx / (a/20)`, `ty / (d/20)`); this drives image-placement pivot offsets in the
emitted entity.

#### unnamed sprites

some sub-sprites and shapes have no `SymbolClass` entry; those are resolved via the
display list (their inner content placed in by depth/timeline). they get synthetic
`id_NNNN` names internally and are stripped from the final entity output if they carry
no real image content.

---

## Fraymakers entity format (.entity)

the `.entity` file is a JSON document consumed by FrayTools, the core of a character
package. all GUIDs are deterministic -- seeded by `{char_id}::{context}` and run through
UUID v5 (SHA-1 namespace) in `src/uuid_gen.rs`.

### top-level structure

```json
{
  "animations": [...],        // per-animation objects
  "export": true,
  "guid": "...",              // deterministic GUID for this entity
  "id": "mario",              // character id (lowercase)
  "keyframes": [...],         // ALL keyframes from all animations, flat array
  "layers": [...],            // ALL layers from all animations, flat array
  "paletteMap": null,         // or { "paletteCollection": "...", "paletteMap": "..." }
  "pluginMetadata": {
    "com.fraymakers.FraymakersMetadata": {
      "objectType": "CHARACTER",
      "version": "0.4.0"
    }
  },
  "plugins": ["com.fraymakers.FraymakersMetadata"],
  "symbols": [...],           // ALL symbols (IMAGE, COLLISION_BOX, COLLISION_BODY, POINT)
  "tags": [],
  "terrains": [],
  "tilesets": [],
  "version": 14
}
```

### animation object

```json
{
  "$id": "...",         // deterministic GUID
  "name": "idle",       // Fraymakers animation name
  "layers": ["..."],    // ordered array of layer $ids
  "pluginMetadata": {}
}
```

empty animations (those whose IMAGE timeline carries no real symbols across all
frames) are dropped from the final entity. the one exception is a small allowlist
controlled by `populated_jabs` in `haxe_gen.rs`: when a character has exactly two
populated jabs the converter keeps `jab3` as an empty placeholder so the Script.hx
jab-chain references don't break at runtime.

### layer types

every animation typically has this layer stack (in order):
1. `LABEL` -- animation name label on frame 0, plus any inner FrameLabel tags.
2. `FRAME_SCRIPT` -- per-frame Haxe code (`code` is the function body, no wrapper).
3. `COLLISION_BODY` -- character ECB/body diamond, per frame.
4. `COLLISION_BOX` -- one layer per box instance (hitbox0, hurtbox0, etc.).
5. `POINT` -- one layer per `touchBox` (grab-hold point).
6. `IMAGE` -- one layer per depth slot (Image 0, Image 1, …).

#### LABEL layer

```json
{ "$id": "...", "name": "Labels", "type": "LABEL",
  "keyframes": ["kf_id"], "hidden": false, "locked": false, "pluginMetadata": {} }
```

LABEL keyframe:
```json
{ "$id": "...", "type": "LABEL", "length": 1, "name": "idle", "pluginMetadata": {} }
```

#### FRAME_SCRIPT layer

```json
{ "$id": "...", "name": "Scripts", "type": "FRAME_SCRIPT",
  "keyframes": [...], "hidden": false, "locked": false,
  "language": "", "pluginMetadata": {} }
```

FRAME_SCRIPT keyframe -- `code` is the function body **only** (no `function name() {`
wrapper). blank frames use `"code": ""`:

```json
{ "$id": "...", "type": "FRAME_SCRIPT", "length": 1,
  "code": "self.playAnimation(\"jab2\");", "pluginMetadata": {} }
```

#### COLLISION_BODY layer (ECB / body diamond)

the ECB is a 4-vertex diamond: foot (bottom), head (top), and two hip vertices at the
sides at the foot/head midpoint. `entity_gen.rs` computes the per-frame diamond as the
axis-aligned bounding box of that frame's HURTBOX-typed collision boxes, so the body
auto-fits the pose. consecutive frames with identical bodies are run-length encoded
into a single keyframe.

```json
{ "$id": "...", "name": "Body", "type": "COLLISION_BODY", "keyframes": [...],
  "defaultAlpha": 0.5, "defaultColor": "0xffa500",
  "defaultFoot": 0, "defaultHead": 86, "defaultHipWidth": 40,
  "defaultHipXOffset": 0, "defaultHipYOffset": 0, "pluginMetadata": {} }
```

COLLISION_BODY symbol:
```json
{ "$id": "...", "alpha": null, "color": null,
  "foot": 0, "head": 86, "hipWidth": 40, "hipXOffset": 0, "hipYOffset": 0,
  "pluginMetadata": {}, "type": "COLLISION_BODY" }
```

#### COLLISION_BOX layer

```json
{ "$id": "...", "name": "hitbox0", "type": "COLLISION_BOX", "keyframes": [...],
  "defaultAlpha": 0.5, "defaultColor": "0xff0000",
  "pluginMetadata": {
    "com.fraymakers.FraymakersMetadata": {
      "collisionBoxType": "HIT_BOX",
      "index": 0
    }
  } }
```

COLLISION_BOX symbol:
```json
{ "$id": "...", "alpha": 0.5, "color": "0xff0000",
  "pivotX": 24.0, "pivotY": 13.0,         // centre of the box (bottom-centre for itemBox)
  "pluginMetadata": {},
  "rotation": 12.5,                        // CW-positive degrees, normalized to [0, 360)
  "scaleX": 48.0,                          // width in pixels
  "scaleY": 26.0,                          // height in pixels
  "type": "COLLISION_BOX",
  "x": -54.0, "y": -35.0 }                 // top-left in world space, y-down
```

COLLISION_BOX keyframe -- blank frame uses `"symbol": null`:
```json
{ "$id": "...", "type": "COLLISION_BOX", "length": 2,
  "symbol": "sym_$id_or_null", "tweened": false, "tweenType": "LINEAR",
  "pluginMetadata": {} }
```

**Fraymakers collision-box type strings** (used in `collisionBoxType`):

| String | Description |
|---|---|
| `HIT_BOX` | Active hitbox |
| `HURT_BOX` | Hurtbox |
| `GRAB_BOX` | Grab range |
| `LEDGE_GRAB_BOX` | Ledge grab |
| `REFLECT_BOX` | Reflect / shield (from SSF2 `shieldBox` and `reflectBox`) |
| `COUNTER_BOX` | Counter / absorb (from SSF2 `absorbBox`) |

#### POINT layer

used for named points in space (grab hold position, pivot points, etc.).

```json
{ "$id": "...", "name": "grabholdpoint0", "type": "POINT", "keyframes": [...],
  "hidden": false, "locked": false,
  "pluginMetadata": {
    "com.fraymakers.FraymakersMetadata": {
      "pointType": "GRAB_HOLD_POINT"
    }
  } }
```

POINT symbol -- coordinate only, no size:
```json
{ "$id": "...", "alpha": 1, "color": "0xff0000",
  "pluginMetadata": {}, "rotation": 0, "type": "POINT",
  "x": 9.62, "y": -9.7 }
```

POINT keyframe:
```json
{ "$id": "...", "type": "POINT", "length": 1,
  "symbol": "sym_$id_or_null", "tweened": false, "tweenType": "LINEAR",
  "pluginMetadata": {} }
```

**known point types** (in `pluginMetadata.com.fraymakers.FraymakersMetadata.pointType`):
- `GRAB_HOLD_POINT` -- where a grabbed opponent is positioned (from SSF2 `touchBox`).
- `PIVOT_POINT` -- rotation pivot override.

#### IMAGE layer

```json
{ "$id": "...", "name": "Image 0", "type": "IMAGE", "keyframes": [...],
  "hidden": false, "locked": false, "pluginMetadata": {} }
```

IMAGE symbol -- **one is created per placement** (each anim/slot/frame gets its own
symbol; symbols are NOT shared across frames because the world matrix varies):

```json
{ "$id": "...", "alpha": 1,
  "imageAsset": "meta_guid",         // GUID from the PNG's .meta sidecar (NOT a file path)
  "pivotX": 0, "pivotY": 0,          // bitmap (0,0); shape_pivot offsets are folded into x/y
  "pluginMetadata": {},
  "rotation": 350.5,                  // CW-positive degrees, normalized to [0, 360)
  "scaleX": 1.1, "scaleY": 1.1,       // signed: negative = flip
  "type": "IMAGE",
  "x": -54.0, "y": -35.0 }            // world-space, y-down
```

the model FrayTools uses is: `world_pt = (x, y) + R(rot) · diag(sx, sy) · local_pt`.
the converter mirrors this exactly -- `x` / `y` come from the SWF world matrix's
translation plus a shape-pivot correction; rotation and scale from the matrix's linear
part. sheared placements are pre-rendered to a baked bitmap (see "skew handling" above)
and emitted as identity-rotation, identity-scale plain translations.

IMAGE keyframe:
```json
{ "$id": "...", "type": "IMAGE", "length": 3,
  "symbol": "sym_$id_or_null", "tweened": false, "tweenType": "LINEAR",
  "pluginMetadata": {} }
```

consecutive frames with the same symbol AND identical world matrix are run-length
encoded into one keyframe.

### .meta sidecar files

every PNG needs a `.meta` JSON file at the same path + `.meta` extension:
```json
{ "export": false, "guid": "deterministic-guid", "id": "",
  "pluginMetadata": {}, "plugins": [], "tags": [], "version": 2 }
```

the GUID in the `.meta` file is what `imageAsset` in IMAGE symbols references, not the
PNG file path.

### per-projectile and per-effect entities

each discovered projectile (SSF2 sprite carrying an `attack_idle` FrameLabel + a `stance`
PlaceObject) gets its own `library/entities/<name>.entity` plus script files at
`library/scripts/Projectile/<Pascal>{Script,Stats,HitboxStats,AnimationStats}.hx`.
animation names come from the inner sprite's FrameLabel tags when present, else fall back
to the FM template trio `projectileSpawn` / `projectileIdle` / `projectileDestroy`.

each discovered effect (root-level SymbolClass'd sprite that's neither a projectile, the
character itself, the head sprite, an `_fla.*` timeline, nor a HUD/icon) is emitted as a
plain `library/entities/<name>.entity` with one IMAGE layer per inner FrameLabel-derived
segment -- no scripts, no stats. the character's `Script.hx` references these via
`match.createVfx(new VfxStats({…}), self)` calls produced by the `attachEffect` rewriter
in `api_mappings.rs`.

---

## how FrayTools renders what we emit

> **not in this repo (compliance).** this repo doesn't carry a spec of how FrayTools
> interprets the `.entity` files we emit (the Y-negation, transform order, which box
> types honor rotation, pivot/scale rules, keyframe-length timing, the manifest/`.meta`
> binding, the palette shader). to respect Team Fray's wishes, we keep that
> reverse-engineering write-up out of the tracked repo (see "engine-side knowledge
> is not in this repo" below); please leave it that way.
>
> what you still need for `entity_gen.rs` work is captured structurally elsewhere in
> this file: the `.entity` schema (every layer/symbol/keyframe shape), the CW-positive
> `[0,360)` rotation convention the converter emits, the Y-down coordinate system, and
> the `.meta`-sidecar / manifest id binding -- all properties of the files **we** author.
> to re-derive FrayTools' render behaviour, do it locally against your own install and
> keep the notes in the gitignored `docs/` scratch space -- never in a tracked file.

---

## coordinate system

both SSF2 and Fraymakers use **y-down** screen coordinates (positive y = down).
no y-flip is needed between the two systems.

```
Origin: character foot (ground contact point)
Positive x: right
Positive y: down (into ground)
Negative y: up (into air)
```

world space = root MC transform applied to local SWF coordinates.

---

## animation name mapping (SSF2 → Fraymakers)

the SSF2 → FM animation-name table is **data-driven** via
`mappings/character/animations.jsonc`. keys:

- `ssf2_to_fm` -- SSF2 xframe / animation name (e.g. `stand`, `a_air_forward`) → FM
  animation name (e.g. `idle`, `aerial_forward`).
- `label_to_ssf2` -- sprite-symbol AnimLabel (lowercased, suffix stripped, e.g.
  `nair`) → SSF2 animation name.

loaded by `crate::mappings::character_animations()` in `src/mappings.rs`. used by
`extractor::build_ssf2_to_fm_anim`, `sprite_parser::static_ssf2_to_fm`, and the
sprite-symbol resolver `sprite_parser::extract_ssf2_anim_name`. edit this file (not
the Rust source) to fix animation-name mapping.

some SSF2 sprites pack multiple FM animations into one timeline separated by internal
FrameLabel tags (e.g. a "Jab" sprite contains jab1 / jab2 / jab3 / jab4; a "Strong"
sprite contains in/charge/attack). the splitter lives in `src/anim_splitter.rs` (and
`sprite_parser::sub_anim_splits` for the sprite-level equivalent). the label patterns
driving which splits are **hardcoded match arms** in `anim_splitter.rs` -- no external
rules file.

---

## UUID generation

all GUIDs in the entity are **deterministic**, seeded by `{char_id}::{context}` using
UUID v5 (SHA-1 namespace). this ensures regeneration produces the same entity
byte-for-byte (in the GUID dimension). see `src/uuid_gen.rs`.

---

## diagnostic binaries

~30 `--bin` targets exist for debugging (gated behind `--features dev-tools`).
the most useful selection:
```
dump_image_placement   — per-frame PlaceObject data for any sprite
dump_collision_box     — collision-box geometry for an animation
dump_images            — list all extracted bitmaps
dump_sprites           — list all DefineSprite symbols
dump_shape_bounds      — measure CollisonBox shape bounds
dump_shape_origins     — shape origin offsets
dump_pivots            — pivot points
dump_frame_labels      — frame labels inside a sprite timeline
dump_raw_frame         — raw tag dump for one frame
dump_inner_sprite      — inspect a nested (effect) sprite
dump_proj_states       — projectile state discovery
dump_stage             — root timeline inspection
dump_costumes          — costume tables from misc.ssf
dump_aerial_down_frames— targeted debug for the aerial-down animation
dump_trail_matrices    — trail-effect matrix inspection
check_shape_bitmap     — inspect a shape's bitmap fill + fill matrix
what_is_id             — identify what a numeric SWF character id refers to
```

these tools are the workbench for inspecting a SWF without re-deriving the format.
costume extraction is in-process inside `ssf2_converter` (no separate `extract_costumes`
binary).

usage example (the bins are dev-tools-gated, so run via cargo):
```bash
cargo run -p ssf2_converter --features dev-tools --bin dump_image_placement -- ../ssf2-ssfs/mario.ssf "FAir_42"
cargo run -p ssf2_converter --features dev-tools --bin dump_collision_box -- ../ssf2-ssfs/mario.ssf "a_air_forward"
```

---

## known issues / open questions

open converter issues and the prioritized TODO list live in
[`docs/STATUS.md`](docs/STATUS.md) -- the single home for converter status and
next steps. this document is the format reference, not the issue tracker; check
STATUS.md for what's currently broken, approximate, or unverified (shape-only menu
portraits, vector-only effect sprites, projectile-behaviour stubs, stat-scaling
approximations, and the rest).

## source file summary

| File | Purpose |
|---|---|
| `convert.rs` | In-process conversion entry point (`run_conversion`); orchestrates the pipeline |
| `extractor.rs` | Bridge between ABC parser and generator (`CharacterData`) |
| `abc_parser.rs` | AVM2/ABC bytecode parser + semantic extractors (~2650 LOC) |
| `decompiler.rs` | Bytecode → Haxe-ish source, full CFG reconstruction (~2050 LOC) |
| `sprite_parser.rs` | Per-frame collision-box geometry from SWF timelines |
| `image_extractor.rs` | PNG extraction, per-frame image placement, skew baking, projectile/effect/head discovery |
| `vector_raster.rs` | Rasterizes vector-shape sprites (shape-only heads/effects) |
| `entity_gen.rs` | `.entity` JSON generator (~2300 LOC) |
| `haxe_gen.rs` | Output orchestrator (writes the whole character package) |
| `anim_splitter.rs` | Multi-label SSF2 sprite → multiple FM animations |
| `api_mappings.rs` | SSF2 → FM API translation pipeline + JSONC-driven rewriters (~2530 LOC) |
| `mappings.rs` | JSONC loader / cache for the editable mapping files under `mappings/` |
| `sound_extractor.rs` | DefineSound → WAV via synthetic FLV + `ffmpeg` |
| `palette_gen.rs` | SSF2 costume / k-means palette generation |
| `swf_parser.rs` | Thin wrapper over the Ruffle `swf` crate |
| `ssf.rs` | SSF wrapper → raw SWF |
| `uuid_gen.rs` | Deterministic UUID v5 generation |
| `fraytools_project.rs` | `<name>.fraytools` project descriptor |
| `fraytools_transform.rs` | Shared FrayTools transform/coordinate helpers |
| `project.rs` | Shared project / manifest structs |
| `lib.rs` | `pub mod` declarations + `run_conversion` re-export (so `src/bin/*` can `use ssf2_converter::*`) |
