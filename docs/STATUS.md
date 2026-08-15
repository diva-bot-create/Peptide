# Peptide status, known issues, and next steps

where the SSF2 to Fraymakers conversion stands across the roster, plus the live list of
open issues and what's worth doing next. this is the one place we track status and TODOs
for the whole project, so known issues and next steps live here instead of scattered
around. the converter status is up top; the Peptide (engine-harness) feature TODOs are at
the very bottom. if you want to verify any of this yourself,
[`PEPTIDE_GUIDE.md`](PEPTIDE_GUIDE.md) shows you how.

## coverage

**the whole SSF2 roster converts and drives in-engine. nothing's currently broken!**
every character:

- **converts.** the converter exits 0 and produces output, with no hard-conversion failures.
- **drives in-engine.** it spawns in Fraymakers via Peptide, reaches `STAND`, and runs its
  moves without crashing (P0: `LAUNCHED` + `ANIM:STAND` + `M:OK`).
- **has populated entities.** real movesets and hitboxes, checked by
  `tools/tests/parity_check.py` (entity animation counts plus HIT_BOX active-frame
  coverage) so we're not just trusting a shallow spawn signal. hitbox frame-coverage runs
  52-90%+ per character; what's left are projectile specials and throws that have no melee
  box anyway.

a few are hand-verified deep (drive moves + physics + anim): `sandbag` (the reference),
`mario` (the deepest, a full 18-move sweep), `kirby`, `bowser`, `fox`. everybody else is
batch-verified (spawn + jab + special_neutral, no crash).

## stage porting

`peptide ssf2 stage <file.ssf>` converts an SSF2 stage to a playable Fraymakers stage. it
parses the SSF2 placement tree for the floor, soft platforms, death/camera boxes, and
entrance/respawn beacons, rasterizes the stage's art (vector shapes + bitmap backgrounds) into the stage sprite, and emits
the FM `.entity` + manifest + StageStats/Script + `.fraytools`; FrayTools builds + publishes
the `.fra` to the engine's `custom/` dir. all 110 corpus stages convert. battlefield (DAT328)
is live-verified: `spawn sandbag battlefieldssf2` (drive with `FRAY_STAGE=<stage>`) loads the
stage, the fighter stands on the platforms, walks, and falls off the edges into the blast
zone (KO + respawn), no crash. NB: a stage needs visible content to place players (the engine
sizes it from the sprite bounds); the sprite is the SSF2 art (vector shapes + bitmap backgrounds rasterized + composited);
stages with no rasterizable art fall back to a geometry placeholder.

the SSF2 `foreground` plane normally folds behind fighters (it's usually a structure's re-drawn
front face, which would read as a duplicate platform); a `keep_foreground` metadata flag opts a
stage out so a genuine overlay stays in front (bowserscastle's glowing lava sheet over the floor).

the backdrop is emitted the way SSF2 authors it: each animated element (lava bubbles, torches,
embers, podoboos, a spectator) is its own FM layer/animation on its own loop, in back-to-front
draw order, rather than one baked composite. elements are grouped by their SSF2 symbol id; a
static element is one frame, an animated one keeps its loop (run-length-encoded at the 30->60fps
doubling, so a held source frame reads as a pause). a single-element backdrop is one layer, same
as before.

the converted stage carries a clean display name (a per-stage override in
`mappings/stage/metadata.jsonc`, with the SSF2 id title-cased as the fallback) plus a source
series, and records its original SSF2 soundtrack (the `bgm_*` ids
pulled from the stage DAT) in the description. it plays a real FM bgm (the override map's pick
or `default_music`) since the SSF2 audio isn't shipped with FM. the content id is suffixed
`<id>ssf2` so it can't shadow a built-in stage.

auto-detected moving platforms (the `moving`-named SSF2 container, propagated to its collision
child) are kept as static collision at their start position, labelled `(SSF2 moving, static)`
in the entity and surfaced in a warning. the motion itself isn't ported there: SSF2 moving
platforms are bespoke per stage (custom AS3 classes / timeline animation), so reproducing them
is per-stage work like porting a character special.

declared `platforms` in metadata become real FM moving Structures (the engine idiom from the
stage-template): a per-platform grey block animation (an IMAGE sized to the platform + a FLOOR
line segment) and one structure CONTENT per platform that the stage spawns and that moves itself
in its own Script. bowserscastle has three grey standing platforms RE'd from the SSF2 terrain
(two wide ledge-bounded side platforms + the small central `terrainGround` block, at their real
positions and widths, in the SSF2 platform grey), sitting over the lava. the thwomp lands on
them and pushes them down: the shared sink/rise Script polls the stage's custom game objects,
sinks the column the thwomp landed on, holds, then rises back. the thwomp is a custom game object
that falls onto each platform column in turn (each at its own height), damages on landing,
re-arms its hitbox each cycle, then rises and moves to the next column.

stage hazards become Fraymakers custom game objects the stage spawns with a null owner, so each
is neutral and damages everyone. damage and knockback come from the entity's native HIT_BOX
(HitboxStats), and three pieces are load-bearing for that hitbox to connect: a null owner (passes
team-hit validation), `stateTransitionMapOverrides` mapping `PState.ACTIVE` to the hitbox
animation (so an animation actually plays and the collision detector resolves it against the
HitboxStats map), and a content-ref `spriteContent` (a bare id string doesn't load the sprite). a
local state machine plays the active/inactive animations for the on/off pulse, and the hitbox is
re-armed on a cadence (`rehit`) so a lingering fighter keeps taking hits. movement patterns
(oscillate / circle / thwomp-fall) move the whole object. each hazard renders its real rasterized
SSF2 art (the thwomp's stone face, the bumper, the lava) recovered from the placement tree; a
hand-declared hazard borrows the detected sprite of its kind, and only a hazard with no recoverable
art falls back to a translucent placeholder box.

damaging hazards auto-detect from the SSF2 placement tree (lava, acid, thwomps, spikes, bumpers,
tornados, damage zones, podoboos, piranhas) by linkage name. the kind is propagated down from the
named container, leaf shapes of one hazard cluster into a single box, and the result is filtered to
the reachable area inside the blast zone (so a hazard parked off-screen at frame 0 doesn't ship as a
phantom hitbox). a `hazards` list in metadata overrides detection for a stage (full manual control,
e.g. a thwomp whose frame-0 position is parked, or a dynamically-spawned hazard); `no_hazards: true`
suppresses a false positive. dynamically-spawned hazards (cannonballs, arwing lasers, birdo eggs,
holy lances) that don't sit in the static tree are a per-stage follow-up, declared by hand as the
need arises. follow-ups: the moving-platform motion, dynamically-spawned hazards, per-stage FM-music
mapping, porting the actual SSF2 audio (licensing-permitting).

stage porting is NOT finished. bowserscastle is the deepest port and the working testbed, and
even it has gaps: chunks of the reconstructed stage behavior are still commented out in the
emitted scripts pending verification (SFX calls among them), so the in-game stage is missing
sound effects and whatever else those blocks drive. other complex stages haven't had the same
1:1 treatment yet (each stage's gimmicks are bespoke AS3 that needs its own RE pass; the
machinery here generalizes, the per-stage work doesn't come for free).

## hitbox-stat parity

**every character's hitbox stats match the SSF2 source. no exceptions right now.**
`tools/tests/parity_check.py` checks every hitbox's `damage` / `angle` / `baseKnockback` /
`knockbackGrowth` / `hitstop` / `selfHitstop` / `hitstun` against the SSF2 source (dumped
via `DUMP_PARITY=1`). that covers the hitbox-*stats* slice of functional equivalence.

the real bar for a "done" character is functional equivalence with SSF2, which is a higher
bar than "spawns, animates, doesn't crash." hitbox-stat parity is one piece of it. the open
dimensions below are the rest.

## open parity dimensions

- **frame data (active-frame range).** startup/active/recovery lives in the `.entity`
  collision-box keyframes and animation lengths, not in `HitboxStats.hx`.
  **animation LENGTHS are now checked**: `tools/tests/timing_parity.py` verifies the
  30→60fps doubling for every emitted animation against the source range the splitter
  sliced it from (mario: 164 animations, 0 off 2.00x). what's still open is the
  COLLISION-BOX active range within each animation, which is the part that decides
  startup/active/recovery. the live half is the `record`/`trace` frame recorder (both
  engines push per-frame telemetry); it confirms the emitted lengths actually play that
  way, on a sample rather than the whole roster.
- **a frame whose code ends the move is no longer emitted** (fixed). the two engines run a
  frame's code on opposite sides of drawing it: SSF2 runs it FIRST, so a frame carrying
  `endAttack` hands control away before anything is drawn and never appears; Fraymakers runs it
  AFTER, so emitting that frame showed a pose the original never displayed and the move ran one
  source frame long. the trim happens before any layer is built, because an animation's length is
  its LONGEST layer -- trimming only the images left the labels at the old length and the
  animation exactly as long as before, just inconsistent.

  ONLY an explicit `endAttack` counts. a `gotoAndStop` is as likely to be a LOOP inside the move
  (a smash holds its charge with one), and treating that as an ending trims a frame the original
  does draw.

  measured on the fixture, sandbag, both engines: jab, tilt_forward, tilt_up, tilt_down,
  aerial_neutral, aerial_forward, aerial_up, aerial_down and special_down are all now exactly
  2.00, against 2.10-2.17 before.

- **split animations were losing every frame script** (fixed). the mapping files speak in parent
  names -- `a` becomes `jab`, never `jab1` -- so looking an SSF2 animation up by a split PIECE
  name found nothing, and nothing is indistinguishable from "this animation has no scripts".
  sandbag's jab was emitted with zero frame scripts: no dust, no swing sound, and nothing marking
  where the move ends. pieces resolve through their parent now, and jab went from 3 missing
  scripts and 2.15 to 3 scripts and exactly 2.00.

  worth a corpus pass: every character with a multi-hit jab was missing those scripts.
- **stage weather is ported** (fixed). a stage can build particle weather in CODE rather than on a
  timeline, so nothing else in the pipeline sees it: the art walk finds no placement and the hazard
  path finds nothing that damages anyone. bowserscastle converted with every layer correct and the
  air empty.

  `abc_parser::extract_stage_weather` steps the stage's own `initialize` for the shape "construct a
  weather object from a particle CLASS, then hand it an area and a count" -- keyed on that shape,
  not on any stage or class name. bowserscastle reads back as 30 particles of `ember` over
  255.5x252.5. the particle is rasterised through the bitmap-fill path (SSF2 art nearly always is a
  bitmap fill, which the vector rasteriser declines by design), and the stage spawns that many
  looping VFX and drifts each one itself, rolling rise, drift and alpha per particle from the same
  ranges the source rolls them from. per-frame speeds convert through `velocity_scale` like every
  other one: SSF2's 1..3 rise becomes 0.65..1.95.

  verified live: 30 VFX in the match, positions scattered across the field and changing between
  samples.

- **weather is tethered to the camera, and sized by it** (fixed). SSF2 attaches the field to the
  VIEW: it fills the screen wherever the camera goes and a particle leaving one edge returns at the
  other. each particle now stores an offset from `match.getCamera()` (which reports the CENTRE of
  the view, confirmed live) resolved every frame.

  the field is sized from the camera too, not from SSF2's rectangle scaled up. SSF2's authored area
  is that engine's screen; fraymakers' screen is a different shape (640x360 at zoom 1 vs the 498x492
  the scaled numbers gave), so copying them across misses the sides and overshoots the top. sizing
  off `getViewportWidth()/getZoomScaleX()` re-derived per frame is what makes it the same field, and
  since both are one screenful the source's particle COUNT carries over unchanged.

  the viewport is reported in WORLD units and already accounts for zoom, so dividing by the zoom
  scale is wrong: it pins the field to a constant 480 half-width while the visible half-view is
  ~690, leaving the sides bare. sizing straight off the viewport tracks it. verified live through
  the borrowed content interpreter, which can read the stage script's own state: with the fighters
  1200 apart, `halfW` 1035.6 against a viewport of 1380.8, particles spanning -704..935, and the
  worst per-particle tether error across all 30 exactly 0.

- **`e` reaches the script-api sandbox AND keeps engine-level access** (fixed). eval now runs in a
  LIVE content interpreter, borrowed rather than rebuilt: `currentMatch -> stageEntity -> its
  ApiScript -> runner -> interpreter`. an interpreted script's ambient variables are written into
  that interpreter's variable map by its runner's constructor (`self`/`exports` from the base
  hscript runner, `camera`/`match`/`stage` from the entity one), so `e` inherits the whole surface
  content sees instead of reconstructing it. peptide's own names are additive on top, so both hold
  at once. verified live:

  | expression | answers |
  | --- | --- |
  | `match.getCamera().getX()` | 1047.4 |
  | `match.getCamera().getViewportWidth()` | 640.0 |
  | `match.getMatchSettingsConfig().hazards` | true |
  | `p0.getStateName()` | STAND |
  | `stageEntity != null` | true |

  rebuilding it instead does NOT work, and the reason is why four earlier attempts failed:
  content's `match` is a `FraymakersMatchApi`, a SUBCLASS, not the `pxf.api.MatchApi` that
  `Match.matchApi` is typed as. hand-binding that field yields an object that identifies correctly
  and takes the engine down on first touch. borrowing never names the type.

  the borrow tries the STAGE's script first and a CHARACTER's second. only hscript content carries
  a borrowable interpreter -- a built-in stage is compiled haxescript and has none -- so the
  character source is what makes the sandbox available on shipped stages, which is most of them.
  both casts are trapped: an unguarded one throws on a compiled runner and takes the engine down,
  which is exactly what `e` did on thespire before the guard went in.

  ONE vocabulary change came with it: `match` is now the content api, so
  `match.getCharacters()` hands back CharacterApi objects rather than raw entities, and
  `getStateName()`/`body`/`damage` are not on those. entity-level access is `p0`..`p3` or the bare
  `getCharacters()` (both verified live), and TESTING.md plus the two test scripts use those.

  two things this forced. `commands.hsx` must not redefine content's names: it used to assign
  `match` unconditionally, which in a borrowed interpreter replaced the live stage's api mid-match
  and broke it (its own getCamera went null on the very next frame). the facade is now installed
  only when nothing else has set `match`. and a fallback interp is never treated as final, so an
  `e` sent before the match starts cannot pin it -- the borrow is retried until it succeeds.

- **weather is detected by the engine call it makes, not by a class name** (fixed). a stage hands
  its weather's container to the engine's weather layer, so "the object whose `getContainer()` is
  added to `getWeatherMC()`" identifies it exactly, on every stage that has any, without knowing a
  single class name. keying on a name containing "weather" found 2 of the 8 stages in the corpus
  that call it, and only if the construction sat in `initialize` (clocktown builds its rain behind
  a state machine, smashville in `syncStage`).

  the COUNT and area come from the class too, not from the call: the constructor says which
  parameter lands in which field, and the signature carries the defaults for whatever the stage
  did not pass. bowserscastle passes only the particle class and takes the rest from those
  defaults, which is why reading the call site alone found nothing. its real numbers are 50
  particles over 640x360 -- and that area being exactly one SSF2 screen is independent
  confirmation that a weather field is meant to be one screenful.

  | stage | particle | count |
  | --- | --- | --- |
  | bowserscastle | ember | 50 |
  | clocktown | ClockTownRainDrop | 100 |
  | crateria | CrateriaGreenRainDrop | 50 |
  | fairyglade | fairyglade_particle | 8 |
  | smashville | (rain) | 100 |
  | finalvalley | (rain) | not readable, skipped with a warning |

  a stage whose count cannot be read emits NO weather rather than a loop over nothing, which would
  render as a stage that carries weather and shows none.

- **the "revival platform frame" was the harness, not the converter** (fixed). a frame of the
  revival pose showed up at the head of tilt_up, down_smash and walk. the emitted animations are
  clean -- tilt_up carries a body layer and a motion smear and nothing else, and the unused image
  layers really are blank -- because nothing was wrong with them.

  the sweep parked aerials BELOW the floor, which is right on the fixture (open space, blast
  boundary far below) and wrong everywhere else: on an ordinary stage, below the floor IS the blast
  zone. the engine KOs and respawns the character, and the two frames of REVIVAL that follow land
  at the start of the next move. measured on thespire, whose floor is y=73: parking at y=600 gives
  `revival x2` then stand, parking at y=73 gives stand alone, and a bare `reset` never produces it.

  two fixes. the floor is MEASURED per engine at the start of a sweep (drop the character, see
  where it stops) instead of being the fixture's constant applied to every stage. and aerials park
  high ABOVE the floor with enough clearance for ~100 frames of fall, so a move still gets to
  finish: an aerial now reads `aerial_neutral x40` then fall then land, where before it was
  whatever the respawn left behind.

- **an unconvertible effect drew the character's revival frame** (fixed). a converted move that
  asked for an effect the character does not carry spawned it anyway, and what the engine drew was
  not nothing: `getContent` does NOT validate, it just builds `private::<resource>.<id>` from
  whatever it is handed, so a missing id is a DANGLING reference and a VfxStats built on one falls
  back to the resource's first animation. for a character that is `revival` -- so the move drew the
  bag on its blue revival platform for a frame, behind itself.

  which effects, and where: `effect_land` is asked for by tilt_up, walk_loop, strong_down_attack
  and tech_roll -- up tilt, walk and down smash, exactly the moves it was reported in. these are
  SSF2 SHARED effects, registered by whichever character's file defines them (sandbag's
  `effect_land` lives in bandanadee's), so a character that only references one has nothing to
  convert from its own file.

  two fixes. `effect_land` and `ground_bounce` map to `GlobalVfx.LAND_DUST`, since fraymakers has
  its own equivalent. and anything still unmapped is no longer spawned at all: with neither a
  GlobalVfx mapping nor a local effect entity there is nothing to draw, so the emitter leaves a
  TODO naming the effect instead of a call that draws the wrong thing
  (`api_mappings::effect_is_spawnable`). sandbag went from 13 dangling vfx references to none.

- **four animation/effect faults, all fixed.**

  | fault | cause | fix |
  | --- | --- | --- |
  | effect placements ignored facing | the source flips the offset by hand (`self.flipX(78)`) AND fraymakers mirrors it via `flipWith`, so the two cancelled | drop the source's flip exactly when `flipWith` is set. measured live: `flipWith` + raw `x: 78` lands at +78 facing right, -78 facing left |
  | `run_turn` faced backwards | only its FIRST frame was mirrored | a turn is drawn facing the direction turned FROM and the engine flips facing when the turn starts, so the whole animation mirrors -- scaleX, x AND rotation, since reflecting about the vertical axis maps an angle to its negative |
  | `crouch_out` played like a second crouch | the exit slot cloned the entry FORWARD | splits carry a `reversed` flag; the exit runs the entry backwards |
  | `fall_loop` looped wrong, and the fall looked like the jump never ended | the loop was sliced at a template constant, and the peeled entry frames were appended to the END -- so every cycle flashed the entry pose | the loop comes from the clip's OWN loop-back call |

  the loop detection is structural, not a name list: a looping SSF2 stance ends by playing a label
  again from its final frame, so `source_loop_frame` reads that call's target and looks up where
  that label sits. the name varies across the corpus (`again` on sandbag and mario, `redo` on fox
  and samus) and none of it is hardcoded. sandbag's fall: entry frames 0-5, loop 5-14.

  the ported loop-back call is then STRIPPED from a looping animation: fraymakers loops natively
  via `endType: LOOP`, and the label the call named was sliced off with the entry, so it threw once
  per cycle and looped nothing.

- **backdrop elements no longer emit the same picture hundreds of times** (fixed). SSF2 authors a
  small looping element and the stage timeline it sits on runs far longer, so the art walk sees the
  cycle over and over and every repeat was rasterised. clocktown arrived as 3379 sprites / 263MB --
  1363 frames of bottom fire drawn from 8 distinct images, 621 frames of fire light drawn from 2 --
  and FrayTools could not publish it at all, at any timeout.

  two lossless reductions, both in the element writer: a sequence that is one cycle repeated emits
  ONE cycle (the element loops anyway, and the tail does not have to be a whole cycle), and an
  image used on more than one frame is written once and referenced again. clocktown: 3379 -> 726
  sprites, 263MB -> 164MB. bowserscastle is unchanged bar 8 files.

  a third reduction is in place but rarely applies: an element that is a large still picture with
  one small moving part is emitted as a plate plus a crop of the part that moves. it samples before
  committing, and declines when the element changes across most of its canvas -- which is what
  clocktown's remaining 623-frame town backdrop does, so that stage is still too heavy to publish.

- **crateria converts and runs clean.** 2.4MB, 27 sprites, no script errors: 50 weather particles
  tethered to the camera (worst per-particle error 0) and spanning the view, the hazards switch
  wired, and both caution-sign hazards spawned.

- **a clip the mapping file does not name kept none of its scripts** (fixed). two passes named the
  same sub-MC differently: the sprite side reads the MAIN TIMELINE (which xframe label placed this
  clip), the script side guessed from the class name against `label_to_ssf2`. a variant the table
  does not list -- `DownSpecialAir` next to `DownSpecial` -- resolved to nothing on the script
  side, and a sub-MC that cannot be named is SKIPPED, so every frame script it had was dropped.

  sandbag's aerial down special had 0 of its 18 scripts while the grounded one had all 18: no
  input poll, no momentum, no floor listeners. it now runs them all, and driving it while airborne
  travels 2255 units on a held direction where it used to do nothing, with no script errors.

  the fix is to ask the timeline first and fall back to the label heuristic, so the two passes
  agree by construction rather than by both being taught the same names. what remains unnamed is
  what should be: ChargeSpark, CollisonBox, groundRef_mc, itemPlaceholder_mc.

- **the hazards switch is ported, not decided** (fixed). both engines let a match turn hazards off.
  in SSF2 the engine does not apply that to a stage: the stage ASKS (`SSF2API.isHazardsOn()`) and
  chooses what to skip, and 47 stages in the corpus ask without agreeing on what it means. so the
  converter reads each stage's own answer out of its bytecode (`abc_parser::extract_hazard_gate`):
  the gated region is the branch after the call, and what counts as gated is what the stage SPAWNS
  in there. a stage that never asks runs everything regardless, and so does its port.

  on bowserscastle that comes out as `gated_classes: ["Thwomp"]` -- the faller is spawned inside the
  branch, the lava is placed regardless, and the embers update BEFORE the check. verified live both
  ways: hazards on = lava + thwomp + spectator, hazards off = lava + spectator, thwomp gone, all 36
  structures still there and the fighter still standing on them.

  the harness had been hiding this. peptide replaces the whole match config on a headless launch, so
  a field it never set read as false and every test match ran with hazards OFF. `hazards` is now a
  match setting like lives and time (`match_settings.conf`, default on), which also makes the off
  case testable on purpose.

- **special-angle sentinels.** SSF2 sentinel angles (`-1`/`-2`/`-3`…) are preserved
  faithfully, we just haven't mapped them to FM's special-angle codes yet. needs the
  SSF2-sentinel → FM-angle table.
- **per-segment hitbox fidelity.** split sub-anims (`jab2`/`jab3`) inherit the base
  attack's hitbox stats for now. that's sound (FM gates a hitbox by the animation's
  collision-box layers, so an inactive inherited box just sits there inert), but the
  damage/KB *values* are an approximation of the real per-hit finisher. getting the true
  per-hit values means pulling hitbox activation frames out of the SSF2 sprite timeline.
- **physics-stat tuning.** movement stats (gravity/fall/walk/jump/weight) are mapped and
  scaled, but the `stats.jsonc :: multipliers` got hand-tuned by eyeballing template
  characters against SSF2 data. `friction`, `shortHopSpeed`, and the ECB head/hip/foot
  positions are hand-tuned constants too, so ground deceleration, short-hop, and hurtbox
  sizing are all approximate. generated `CharacterStats.hx` flags the shaky numbers with
  `/*TODO*/`. dialing the scale factors in for real needs an in-engine SSF2-vs-converted
  comparison (the dummy / measurement path), not a static fix.
- **SSF2-only / no-FM-equivalent inventory.** SSF2 API surface with no Fraymakers
  mapping. Calls get commented out as `[SSF2-only]` (no impact for human play of
  non-item characters); stat fields and events are handled as noted. The
  authoritative per-entry list (with notes + `manual_port`/`no_equivalent`
  category) lives in `mappings/commands.jsonc :: ssf2_only`; runtime tallies land
  in each character's `conversion_log.json :: ssf2_only`. Confirm any entry as
  droppable (like `priority`) or map it as the API surfaces.

  - **mapped this pass (live-verified on zelda/sandbag/ganondorf):** `getMC`→
    `getViewRootContainer()`, `getStanceMC`→`getSprite()` (members read off it may
    differ; flagged inline), `toCrashLand`→`toState(CState.CRASH_BOUNCE)`, `toIdle`→
    `toState(CState.STAND)`, `toFlying`→`toState(CState.HURT_HEAVY)`; `hitTestGround(x,y)`
    →`hitTestStructuresWithLineSegment(new Point(x,y), new Point(x,y), null, null)` with
    the `!= null`/`== null` checks rewritten to `.length > 0`/`== 0` (Array return), the
    options arg dropped, and a TODO on the point→segment approximation; `if (… isForcedCrash …)`
    blocks DELETED (no forced-crash in FM). AS3 global casts the decompiler wrongly
    attached to `self` (so they were null → "Invalid function null"): `self.int`/
    `self.uint`→`Std.int`, `self.Number(x)`→`Std.parseFloat(Std.string(x))` (`Std` is
    in character-script scope, confirmed by a live probe). Stat fields: `hasEffect`→
    `flinch`, `sdiDistance`→`hitstopNudgeMultiplier` (value/6, since SSF2 default is 6),
    `shieldDamage`→`shieldDamageMultiplier: 1` with the old value kept in a TODO.
  - **method calls, `no_equivalent` (57), commented out.** item system (`getItem`,
    `getItems`, `getItemStat`, `pickupItem`, `tossItem`, `toToss`, `removeItem`,
    `generateItem`, `updateItemStats`, `isZDropped`); CPU/AI (`isCPU`, `getCPULevel`,
    `getCPUAction`, `getCPUForcedAction`, `getCPUTarget`, `setCPUAttackQueue`,
    `importCPUControls`, `resetCPUControls`, `isStandby`, `getStandby`); Flash/MovieClip
    (`getMCByLinkageName`, `getHitBox`, `getKirbyHatMC`, `getHUDBackgroundMC`,
    `getHUDForegroundMC`, `killDarkener`); sound-needs-a-clip-handle (`stopSound`,
    `stopSFX`, `stopHoldSound`, `stopListening`); final-smash/kirby/misc
    (`isUsingFinalSmash`, `setFinalSmashMeterCharge`, `getCurrentKirbyPower`,
    `getCurrentMusicInfo`, `getQualitySettings`); other (`addIgnoreObject`,
    `calculateKnockback`, `checkAtkilled`, `clearEffectsOnStateChange`, `endControl`,
    `exportStats`, `getLedges`, `getNearestPath`, `isReversed`, `jumpToContinue`,
    `killAttackboxes`, `removeSelfPlatform`, `replacePalette`, `setAttackEnabled`,
    `setLastUsed`, `shootOutOpponent`, `spawnEnemy`, `stop`, `updateEnemyStats`).
  - **method calls, `manual_port` (26), close FM equivalent needs hand-work.**
    `fireProjectile`→`match.createProjectile`, `unnattachFromGround`→`unattachFromFloor`,
    `swapDepthsWithGrabbedOpponent`→`swapDepths`, `getMetalStatus`→StatusEffect/BodyStatus,
    `setColorFilters`→`setCostumeShader`, `getCharacter`/`getProjectile`→
    `match.getCharacters`/`getProjectiles`, plus `angleControl`, `createSelfPlatform`,
    `forceAttack`, `getAttackBoxStat`, `getAttackStat`, `getCurrentAttackFrame`,
    `getCurrentProjectile`, `getExecTime`, `getHealthBox`, `getLinkageID`, `getMidground`,
    `getNearest`, `getNearestLedge`, `getPlatformBetweenPoints`,
    `hitTestGroundBetweenPoints`, `homeTowardsTarget`, `inUpperLeftWarningBounds`,
    `isEqual`, `setHurtInterrupt`.
  - **hitbox/attack stat fields still with no FM mapping**, surfaced as compile/runtime
    "invalid stat" for per-site fixing: `camShake`, `chargedPriority`, `ignoreChargeDamage`,
    `meteorBounce`, `onlyAffectsGround`. `stackKnockback` is actually a real FM
    `HitboxStatsProps` field (same name) so it passes through fine, not a gap. NB: the
    "Invalid hitbox stat" check fires only when a hitbox is ACTIVE in-engine, so an eval
    `updateHitboxStats(0, {x: 1})` on an inactive box won't surface it; confirm field
    validity against the FM API types.
    - `priority` DROPPED (confirmed unnecessary). `burn`/`shock` `: true` map to FM
      `element` (`ElementType.FIRE`/`ELECTRIC`, live-verified); their `: false` /
      `pitfall: 0` no-element forms are dropped. Still open (carry a value or an
      unconfirmed FM enum, so per-site, not a flag→enum rename): `aura` (no confirmed
      `ElementType.AURA`), `paralysis` (numeric stun duration), `pitfall` > 0 (FM
      `ElementType.BURY` + `bury*` timing fields, not a bare flag).
  - **Flash display-list / timeline methods called on the mapped view objects**
    (`getMC`→`getViewRootContainer()`, `getStanceMC`→`getSprite()`): `swapChildren`
    (35), `removeChild` (48), `addChild` (12), `getChildByName` (14), `getChildAt`/
    `numChildren`, and timeline reads `currentLabel` (37), `currentFrame` (18),
    `gotoAndPlay`. NONE exist in the FraymakersApiTypes (FM rendering/animation is not
    a Flash display list), so they're null at runtime and need per-site porting (e.g.
    z-order via FM layering, timeline reads via the FM animation API). This is the main
    remaining `Invalid function null` source after the AS3-cast fix.
  - **`SSF2Event` types with no confirmed FM equivalent, neutralized** (the line is
    commented so it can't pass a null event): `KO_POINT`, `CHAR_ATTACK_COMPLETE`,
    `CHAR_ATTACK_CHANGED`, `REVERSE`, `REVERSE_HIT`, `CHAR_COUNTER`, `CHAR_METAL_CHANGE`,
    `PROJ_COLLIDE`, `PROJ_DESTROYED`, `PROJ_HURT`, `HOMING_TARGET`, `ENEMY_DESTROYED`,
    `ITEM_TOSSED`, `ITEM_DESTROYED`, `CHAR_SELF_DESTRUCT`, `CHAR_ABSORB`,
    `CHAR_POWER_SHIELD_HIT`, `CHAR_SIZE_CHANGE`, `CHAR_TRANSFORM`, `ATTACK_ENABLED`,
    `GAME_ITEM_CREATED`. (Mapped this pass: `ATTACK_HIT`, `ATTACK_HIT_SHIELD`,
    `GROUND_LEAVE`, `CHAR_SHIELD_HIT`, `CHAR_KO_DEATH`→`CharacterEvent.KNOCK_OUT`.)

## measuring progress

`tools/tests/translation_completeness.sh` counts untranslated markers per character:
`/* ? */` (decompiler couldn't recover an expr/condition/receiver), `[SSF2-only:` (no FM
equivalent), and `TODO` (value punted to a default). lower is better. it's the safe
before/after gate for decompiler/mapping changes, since a real fix should *drop* the marker
count without adding new ones, and the in-engine spawn sweep still has to pass. read it
alongside the amount of code emitted, though: a decompiler fix that recovers a previously
dropped body ADDS markers, because the untranslatable calls inside it were invisible while
the whole body was missing. seven characters are fully `/* ? */`-clean. the
decompiler-quality outliers worth a focused pass are `bowser`, `kirby`, `dedede`, `rayman`,
`krystal`, `sora`, `lucario`, `wario`.

the other half of the measurement is `conversion_log.json :: unknown`, the per-character
tally of call sites the converter doesn't recognize. it counts only genuine gaps: names a
regex mapping emits, the character's own helper functions, and Haxe stdlib methods are all
discounted, so an entry there means "no Fraymakers equivalent" rather than "the converter
didn't recognize the name". corpus total is 1619 calls across 55 names (see "character
system gaps" below).

## how parity should be verified going forward

the endgame is a `verify <move>` Peptide command (see
[`PEPTIDE_DESIGN.md`](PEPTIDE_DESIGN.md) roadmap) that drives a move against a dummy and
diffs what actually happens against the SSF2 reference values. we've already got the
hitbox-stat reference in hand. the missing piece is in-engine measurement of emergent
behavior (a dummy opponent plus post-hit readback), and that's the next focused project.

---

## stage hazard inventory

full audit of all 110 stages: which have scripted actor/hazard classes, what the converter
handles, and what's a declared gap. run via `ssf2_objgraph <stage.ssf> scripts` + the
debug-info pass. corpus conversion: PASS=110 FAIL=0.

**ported** (converter emits a hazard CGO with disasm-sourced stats; none live-verified yet --
boot `FRAY_STAGE=<id> peptide session --char mario,mario` and watch the out.log hit stream for
>= 2 cycles to confirm):

| stage | hazard / actor | converter output |
|---|---|---|
| bowserscastle | Thwomp (6-column fall), BowsersCastleLava (hazard floor), BowsersCastlePlatform (sinking) | multi-anim CGO + hazard_floor + sinking-platform Structure (the one live-verified stage) |
| casinonightzone | CNZBumper, Bumper1_B/T, Bumper2/3 L/R | 4 Bumper hazard CGOs (auto-detected by class name) |
| hyrulecastle64 | HyruleTornado | Tornado hazard CGO (oscillateX) |
| kingdom1 | PiranhaPlant x2 | two static Piranha CGOs at game(74,74)+(670,244) via `game_coords` metadata; disasm stats (w35 h49, damage 5, angle 90, knockback 90, rehit 30). replaced the auto-detect that stacked both at one wrong spot |

**the `game_coords` mechanism.** a metadata `hazards` entry with `"game_coords": true` takes
the raw SSF2 `setX/setY` + `getOwnStats` width/height literals (terrain-local) and runs them
through the stage's static `terrain_off` + `scale`, the same transform as `sink_columns` and
auto-detected actor hazards. use it for any hazard whose position is a static AS3 literal but
isn't in the placement tree. kingdom1 is the worked example.

**unported, with full disasm spec** (the values below are the measured port spec -- the SSF2
key map is `direction`->angle, `power`->baseKnockback, `kbConstant`->knockbackGrowth,
`refreshRate`->rehit x2; frames x2, px x1.3 for FM). each is unported because it's a moving /
flying / projectile-spawning actor whose in-play position needs a live read (placing a static
box at the offscreen spawn would be a wrong-but-plausible value, worse than a gap), OR its
damaging attackBox extent isn't a disasm literal:

| stage | class | spawn (game coords) | stats (from getAttackStats / onHurt) | why not static |
|---|---|---|---|---|
| dreamland | WhispyWind | (168,137) facing right / (88,137) left; cadence FrameTimer(300); lifetime 300f | damage 0, power 11, kbConstant 1, direction 0 (a pure horizontal wind PUSH, not a hitbox); ghost, width/height 0 | the wind attackBox extent (how far the gust reaches) isn't a disasm literal; needs a live read of the forceAttack("wind") box |
| kingdom1 | PowBlockMK | runtime `powCoordinates` (NOT a disasm literal) | onHurt: damage 20, direction 90, power 90, kbConstant 60 (shockwave when the block is hit) | event-triggered (ENEMY_HURT) + runtime position |
| kingdom2 | Birdo | walks in from (-500,98) / (975,98); throw cadence m_spitTimer 20f; stage spawn timer 450f | contact: damage 5, power 46, kbConstant 62, direction -4. egg projectile: damage 1, power 55, kbConstant 5, direction 20, xspeed 8, time_max 300 | walks in from offscreen + throws egg projectiles; both need live position |
| kingdom2 | Pidgit | walks in from (-500,0) / (975,0); oscillation timer 60f; XSPEED 5, YSPEED 30, scale 2x | carries a carpet platform (not a damage hazard) | moving platform, offscreen spawn |
| sectorz | Arwing | flies from (1292,-120); decision timers 80-150f; X_SPEED 3, Y_SPEED 4, Y_ACCEL 1; despawn at y<-400 | fires arwing_laser projectile: damage 16, power 80, kbConstant 50, direction 35, xspeed 26 (refreshRate 10) | flies across; the laser is a projectile |
| sectorz | SectorZLaser | gun at (-83.25,290.85); charge FrameTimer(90), bullet delay 3f | beam: damage 15, power 175, kbConstant 15, direction 175 (+ second box: damage 5, power 125, direction 125) | the beam sweeps; its swept attackBox extent isn't a disasm literal |
| saffroncity | PokeHazards | window at (450,143); spawn cadence FrameTimer(30 x rand(10,20)) | rotates attacks per pokemon: charmander damage 5/power 45/kb 15/dir -4; venusaur damage 14/power 65/kb 30/dir -3; porygon damage 18/power 65/kb 25/dir -4; electrode damage 30/power 50/kb 100/dir 76/burn | the damage depends on which pokemon spawns (rotating); picking one would be invention |
| saffroncity | CharmanderFire | NOT IN DISASM (initialize empty) | w23 h23, damage 3, power 45, kbConstant 7, direction 45, burn | a child projectile of PokeHazards |
| saffroncity | VenusaurLeaf | NOT IN DISASM | w35 h20, damage 3, power 40, kbConstant 70, direction 60, xspeed 25, time_max 20 | a child projectile of PokeHazards |
| peachscastle | BonzaiBill | digs to (181,-54)/(236,-134)/(288,-53), then flies (SPEED 2, dig 0.1); timers 150/30/90f | explode: damage 10, power 70, kbConstant 125, direction -1, burn. cry/idle: damage 5, power 46, kbConstant 62, direction -9 | digs around then flies across (target (236,308)); fully dynamic path |
| racetothefinish | RTTFDamageZoneHandler | 4 segments; positions NOT IN DISASM (runtime regions, default (-3000,-3000)) | zone damage 10 | segment positions are runtime-populated |
| mushroomkingdom3 | koopa0..koopa17 | per-stage timeline | rolling shells | timeline-driven, no getAttackStats class |
| skullfortress | SniperJoe, Batton, Met | per-stage | patrolling enemies + shots | full enemy AI + projectiles |
| yoshisisland | GooniePlatform, RotatingPlatform | per-stage | moving platforms | rotation/path, not a hazard |
| fourside | FoursideSpaceship | per-stage | UFO moving platform + beam | moving platform + beam |
| junglehijinx | JungleHijinxBarrel | per-stage | barrel cannon (a launcher, not damage) | out of hazard scope |
| pacmaze | EdgeWrap | n/a | screen-wrap behavior | no FM edge-wrap primitive |

to finish any of these: boot SSF2 on the stage (`PEPTIDE_SSF2_STAGE=<id> peptide ssf2 session
--char mario`), `tree 3` to read the live in-play position of the actor / its attackBox, then
add a `game_coords` (or FM-coord) `hazards` entry with the spec above + the measured position,
and live-verify in FM. the stats are already extracted; only the live position is missing.

**weather / ambient effects** (visual only; no hitbox output needed, but missing from
converted stages makes them look flat compared to SSF2):

| class | stages | what it adds |
|---|---|---|
| EmberWeather | bowserscastle | rising fire embers (camera-space, 50 particles, 6x6px, 2px/frame rise at 30fps) -- measured but unported |
| RainWeather | clocktown, crateria, finalvalley, smashville | rain particle field |
| SnowWeather | snowpointtemple, waitingroom | snow particle field |
| FairyGladeWeather | fairyglade | firefly sparkle field |
| WaterSplash | fairyglade, gangplankgalleon, lakeofrage, planetnamek | splash vfx on landing near water |

**stage-level special behaviors** (not hazards; structural/gameplay gaps):

- `pacmaze` EdgeWrap: FM has no native edge-wrap API; requires engine-side custom behavior or
  scripted position teleport in the stage Script. gap until FM exposes that primitive.
- moving platforms across every affected stage: the converter marks them `(SSF2 moving, static)`.
  each is a per-stage AS3 class; see PORTING_STAGES.md for the disasm recipe.

---

## character system gaps

calls in the `conversion_log.json :: unknown` stream: SSF2 API the converter doesn't classify at
all, as opposed to the `ssf2_only` universal gaps (item system, getCharacter, getMetalStatus)
covered above in "SSF2-only / no-FM-equivalent inventory".

a character's OWN helper functions don't appear here. `updateAuraPaws`, `isDarkPit`, `hurtSelf`
and friends are emitted and translated into that character's `Script.hx`, so their calls resolve;
the tally discounts every name the assembled script defines. the work they represent is decompiler
BODY quality, tracked by the `/* ? */` counts in "measuring progress", not call mapping.

all 49 characters (47 SSF files, incl. multi-char bowser/wario/zelda) convert exit 0.
corpus sweep: PASS=49 FAIL=0. the only empty function bodies left in the corpus are the
intentional template stubs (`inputUpdateHook` on all 49, `onTeardown` on 2).

**the whole remaining stream**, 1619 calls across 55 names. the item system alone is 73% of it:

| call | count | chars | what it is | port path |
|---|---|---|---|---|
| activateItem / deactivateItem | 678 / 506 | 42 | item grab/drop | part of the item-system gap; no FM item API |
| forceGrabbedHurtFrame | 130 | 32 | holds the grabbed character on a specific hurt frame during a throw | check whether FM's `GRABBED` state already drives the right frame; if so these are no-ops to comment out |
| getScale | 62 | 9 | reads the character's uniform scale | FM splits the axes: `getScaleX()` / `getScaleY()`, no uniform accessor |
| attachEffectOverlay | 22 | 9 | overlay VFX pinned to the character | no FM equivalent found; closest is createVfx with `relativeWith` |
| applyPalette | 22 | 4 | runtime palette swap | FM: `setCostumeShader()` |
| getOwnStats | 15 | 2 | the actor's own stat block | no FM equivalent; SSF2 hazard/actor idiom |
| setupAutolinkAngle / stopAutolinkAngle / updateAutolinkTarget | 10 / 12 / 4 | 4 | SSF2 autolink (an attack steering itself toward the target it hit) | no FM equivalent; needs a Script.hx hook on HIT_DEALT |
| addToDamageMeter / removeFromDamageMeter / updateBar / throbDamageCounter | 9 / 9 / 9 / 8 | 9 | SSF2 HUD damage-meter drawing | no FM equivalent; FM owns its own HUD |
| takeDamage | 8 | 7 | deal damage to another object | no FM equivalent by that name; `updateHitboxStats` / a real hitbox is the FM route |

the remaining ~40 names are 1-6 calls each on 1-4 characters (`toDodgeRoll`, `grab`,
`generateCharacterItem`, `getHomingTarget`, `spitFoe`, `setInvincibility`, …). they're
long-tail per-move SSF2 API, worth mapping only when a specific character is being audited.
`reset`, `tick`, `dispose`, `start` and `distance` all exist in the Fraymakers API under the
same names but on types the converter can't confirm from the call site, so confirm the receiver
before treating one as a gap.

---

## known issues & gaps

the live list of open converter issues. strike an entry when you fix it.

- **fraymakers exposes no blend modes; ssf2 blends ship as a SOLVED alpha overlay.** an ssf2
  placement drawn with a flash blend mode (bowserscastle's `bowsers_lightmask` uses HardLight)
  can't be reproduced at runtime: the renderer underneath has `blendMode` on every display object,
  but a mod only ever gets a `ContainerApi`, whose whole surface is addChild/addChildAt/removeChild
  plus state, and the scripting api's entire visual vocabulary is scale, alpha and visibility. no
  setBlendMode, setTint, setFilter or setShader exists anywhere in `pxf.api.*`. reaching the field
  needs engine injection, which is fine for peptide's own testing and useless for a converted
  stage, since that stage runs on the stock engine.
  so the converter SOLVES the blend against the art beneath it and emits the result as an ordinary
  alpha overlay in the foreground, above the character containers (`fit_blend_overlay` in
  `stage_parser.rs`): per pixel it picks `C = HardLight(base, mask)` at the mask's own alpha, which
  reproduces the static art exactly AND still tints whatever moves underneath. baking into the art
  is the fallback when there's nothing to solve against; it's equally exact but tints only what
  was baked, which is wrong for the layer fighters run around inside.
  two gotchas worth keeping: the overlay must ship at FULL layer alpha (the authored foreground
  sheet wants ~0.5, and halving a solved overlay halves the blend itself), and a blended placement
  must not be split into its own per-element layer.
  remaining gap, structural: a chromatic mask can't be matched exactly on MOVING art. normal
  blending gives `out_c = b_c(1-a) + C_c*a`, so the slope is shared across channels and only the
  offset is per-channel, while HardLight needs a per-channel slope. a greyscale mask IS exactly
  expressible (black at `1-2t` + white at `2t-1`); `mask_chroma` reports which case a stage is in.
  quantifying the residue against ssf2 needs camera-matched shots, since `shot` captures the
  display root and the two engines' frames are currently different views.

- **shape-only menu portraits.** a few characters (`donkeykong`, `fox`, `marth`) have
  `*_head` portraits built entirely from shapes instead of a bitmap. the head finder grabs
  a Bitmap placement when there is one; when there isn't, the head image comes up empty and
  `Menu.entity` ships a placeholder. wants a small SWF shape rasterizer.

- **mario sprite placement not re-verified.** after the recent rotation / itemBox /
  shear-baking work most characters look right in FrayTools, but mario specifically hasn't
  been re-checked frame by frame. he was the canary that drove the rotation work, so some
  of his animations might still need a focused pass.

- **vector-only effect sprites get skipped silently.** effects whose visuals are pure
  vector shapes with solid-colour fills (some charge sparkles, the F-air twinkle) can't be
  rasterized without a full SWF vector renderer, so only bitmap-backed shapes get exported.

- **frame-script / API translation is incomplete.** `commands.jsonc` handles the bulk of
  SSF2 API calls, and the `ssf2_only` list plus the `conversion_log.json :: unknown` stream
  surface whatever's left. the generated `.hx` always wants a human read.

- **decompiler body quality is the main translation gap.** call mapping is in decent shape
  (1619 unmapped calls corpus-wide, mostly the item system); what's left is bodies the
  decompiler recovers imperfectly, counted per character by the `/* ? */` markers. an
  unrecognized opcode is the expensive failure mode: the instruction stream desyncs and the
  rest of the method decodes as nothing, silently, so a body can go missing without any
  marker at all. when auditing a character, check for suspiciously short or empty function
  bodies against `dump_method <ssf> find <name>`, not just the marker count.

- **projectile behaviour is stubbed.** projectile *entities* (visuals, boxes, animations,
  palettes) generate fine. projectile *behaviour* (`<Pascal>Script.hx`) is template
  scaffolding with `// TODO: tune X_SPEED / Y_SPEED` placeholders and, for multi-state
  projectiles, empty `LState` transitions.

- **stat scaling is approximate.** see "physics-stat tuning" above. the
  `stats.jsonc :: multipliers` are hand-tuned and the generated `CharacterStats.hx` marks
  the shaky ones with `/*TODO*/`.

- **robustness.** `process_character` swallows per-stage errors and carries on with
  defaults. great for batch runs, but it means a partly-broken character can sneak out
  without an obvious failure. the Tier 1 validation pass catches the most common silent
  regressions (empty stats, empty attacks, declared-vs-extracted mismatch) and writes them
  to `conversion_log.json :: validation_warnings`. always skim `conversion_stats.json` and
  `conversion_log.json` after a run.

- **transformation characters need manual FM-side wiring.** giga bowser and wario man come
  out as standalone packages (`characters/gigabowser/`, `characters/wario_man/`) because
  Fraymakers has no native transformation API. the TODO banner in `CharacterStats.hx` and
  the `ssf2_source` block in `conversion_log.json` both flag it. the content author still
  has to script the swap by hand in the parent character's `Script.hx`.

- **path 2 enumeration fallback is still around.** the fallback in `detect_char_names`
  (instance-method enumeration on Main) is kept as a safety net for a hypothetical future SSF
  that builds its roster array dynamically. a full 47-character corpus sweep never triggers
  the "constructor walk returned empty" warn, so the constructor walk handles the whole known
  roster; the net stays until a shipped release confirms the same in the wild, then it and
  `derive_id_from_getter` can go.

---

## prioritized next steps

roughly the order a fresh agent should pick these up. the stage and character system gaps
are catalogued in the two new sections above; the items here are the main converter work.

1. **keep the full-corpus sweeps green.** after any parser/emitter change: characters
   (49/49 exit 0), stages (110/110 emitted), and `cargo test --workspace &&
   cargo clippy -- -D warnings`. (currently all green.)

2. **stage hazard keywords: sectorz, saffroncity, peachscastle, dreamland.**
   add `arwing|laser`, `pokehazard|charmander|venusaur`, `banzai|bonzai`, and
   `whispywind|whispy` to `hazard_kind()` in `stage_parser.rs`. all four have the actor
   parsed already (confirmed by debug output: actors listed with class name + x=None means
   they're dynamically spawned so position must come from disasm). for each: disasm
   initialize/update for spawn position, timing, damage, and size; add a metadata entry
   with the measured values; live-verify via phase-7 (boot + watch hit stream).

3. **shape-only head rasterizer.** `donkeykong`, `fox`, `marth` portraits are placeholders.
   add a minimal SWF shape rasterizer (or pull from `ruffle`) for the `*_head` symbol.

4. **verify mario in FrayTools.** re-run mario, scrub frame by frame in FrayTools.

5. **projectile behaviour.** the `<Pascal>Script.hx` stubs need real translated logic from
   the decompiler + JSONC pipeline.

6. **character system gaps (in priority order):** see the table in "character system gaps"
   for the full stream.
   a. `forceGrabbedHurtFrame` (130 calls, 32 chars) -- investigate whether the FM grabbed
      state already drives the correct frame; if yes, comment these out as no-ops. the
      biggest entry that isn't the item system.
   b. the SSF2 autolink group (`setupAutolinkAngle` / `stopAutolinkAngle` /
      `updateAutolinkTarget`, 4 chars) -- one Script.hx hook on HIT_DEALT covers all three.
   c. `getScale` (62 calls, 9 chars) -- FM splits the axes; decide whether to emit
      `getScaleX()` or carry a helper that asserts the two match.
   d. the HUD damage-meter group (`addToDamageMeter` / `removeFromDamageMeter` /
      `updateBar` / `throbDamageCounter`, 9 chars) -- FM owns its own HUD, so these are
      probably `ssf2_only` no_equivalent rather than a port.

7. **validate stat scaling** against hand-tuned reference characters.

8. **delete the path 2 enumeration fallback** once a shipped release confirms it's unused.

---

## peptide / harness todos

the live-engine tooling features we want to build. this is the one list (PEPTIDE_DESIGN.md
points here).

1. **multiplayer in quick boot.** 1-4 player matches work on Fraymakers. the `spawn`/fastboot
   vocabulary takes a roster of up to 4 comma-separated characters (`spawn a,b,c,d [stage] [assist]`,
   `FRAY_ROSTER`/config `roster`, `--char a,b,c,d`); the launch builds the full N-player characters
   array and the engine constructs every fighter natively. 1-2 players use training mode (the
   parity-harness "fighter + dummy" path), 3-4 auto-engage versus mode (selected at runtime by roster
   size). each extra is synchronously self-bootstrapped, so DISTINCT custom chars in one roster work
   (`spawn sandbag,mario,sandbag,mario` -> 4 live, verified). every fighter binds to `p0`/`p1`/`p2`/`p3`
   and `match.getCharacters().length` reports the real count (read host-side from the live match, since
   hscript lacks RTTI on the raw character list). verified live: rosters of 1/2/2-distinct/3/4 ->
   `getCharacters()` 2/2/2/3/4, zero crashes, `p3.getStateName()` drivable. `spawn --versus a,b` forces
   versus mode for a 1-2 player roster (wire byte `S`). SSF2 input injection is live too:
   `hold`/`release`/`seq` drive the controlled player via a per-frame applicator injected into
   `Controller.getControlStatus` (host control masks translated to SSF2's control-bit layout),
   verified live (`hold right` walks: XSpeed 0 -> 4 -> 0 on release; `seq jump:…` leaves the
   ground). still open: they don't yet take hits in a verified way. prerequisite for #6. (impl note:
   extras are built as UNROLLED straight-line blocks -- emitting the
   char-resolve helper inside a runtime back-branch corrupts the loop counter, so each of the <=3
   extras is its own guarded block.)
3. **`addCharacter`** -- drop one more fighter into the LIVE match on the fly, **working on both
   engines**. Fraymakers (`addCharacter`, aliases `addchar`/`add`, wire `n`) re-arms the per-frame
   deferred-spawn. SSF2 does a real live add: its per-match player containers are fixed-size at
   `StageData.startGame`, so `spawn` reserves one spare slot (exist=false, null character, capped at
   4) and `addCharacter` fills it -- set the slot's PlayerSetting, importData the stats, then
   construct the Character directly (its ctor self-registers via `addPlayer`/`addCharacter`),
   bracketed by de/activateCharacters like makePlayer, skipping makePlayer's start-game-only
   attachHealthBox. verified live: `addCharacter` on a 1-char match adds a real drivable `sandbag`
   (count 1->2, walks under input, match stable). note: the added fighter can bind to a different
   `pN` slot than expected. open on FM: the deferred add hasn't been re-verified against a
   versus-mode match for slots past the initial roster.
4. **scenario replay test env.** the `scenario` command sets up a deterministic, re-runnable
   scene: `scenario <p0 x,y[,vx,vy]> <p1 x,y[,vx,vy]> [<ctrl:frames>…]` places both players at
   fixed positions (optionally with world-space momentum), resets them to neutral STAND, then
   plays an input timeline on p0. re-run the exact line to replay it. host-side macro composing
   `eval` + `seq` through the DebugTarget seam, working on **both engines** (verified live on SSF2:
   the setup lowers `setX`/`setY`/`setXVelocity` to field writes and `toState(CState.STAND)` to
   `setState(0)`, then the input timeline plays via the SSF2 input applicator). still open: setting
   a precise animation FRAME (not just the state) and the hit-measurement readback (#6) that makes
   a scenario's outcome quantifiable.
5. **live move-tuning** -- the `tune <player> <hitbox> <stat>=<value> …` command hot-reloads a
   move's hitbox stats into the running match with no relaunch, **working on both engines**.
   Fraymakers addresses a hitbox by INDEX (`tune p0 0 damage=15 baseKnockback=50 angle=45`) via the
   engine's `updateHitboxStats`. SSF2 addresses by MOVE NAME (its attacks are move-name keyed):
   `tune p0 a damage=15 angle=45` mutates the `a` move's stored `AttackBoxes["attackBox"]` (an
   AttackDamage with public setters) via the TUNE verb -- `AttackDataObj()` (protected getter,
   reused QName) -> `getAttack(move)` -> the box -> set Damage/KBConstant/Power/Direction, read back
   as confirmation. mutating the stored box persists (it's re-imported each hit). move keys are the
   raw SSF2 attack names (`a`, `a_forward_tilt`, `special`, …). verified live: `tune p0 a damage=77`
   -> box0 Damage=77; `tune p0 a_forward_tilt damage=33 angle=200` -> Damage=33, Direction=200.
   still open: the UI surface, tweaking move *code* (not just stats) on the fly, persisting tweaks
   back to source, and tuning boxes past box 0 / by frame.
6. **in-engine hit measurement** (needs #1): hit-result readback (damage dealt, knockback
   distance + angle, hitstun frames), KO-threshold search (binary-search the dummy's % for
   the lowest KO), and an active-box dump (every active hit/hurt box this frame). damage
   readback already works live: `scenario 0,0 20,0` (dummy overlapping) + `seq attack:3`
   (drive the attack via INPUT, not `toState`) + reading `p1.damage._damage` shows the dummy
   take the hit (0 → 9 on a sandbag jab). so the `toState(JAB)` "doesn't arm a hitbox" confound
   is bypassed by input-driving the attack. still open: knockback/hitstun readback, the
   KO-search, the active-box dump, and two position confounds: `p0.flipX()` returns 0 not a
   facing sign, and `getX` carries an offset from `setX` (`setX(0)` reads back `getX -19.5`).
7. **frame-advantage display** in the Peptide UI, on shield hit and on hit.
8. **overlay mode.** `$PEPTIDE_OVERLAY=1` floats the console ON TOP of the running game:
   always-on-top, compact (440x560), parked top-right of the primary monitor, with the full UI
   (matching our theming). F8 toggles always-on-top on/off live so you can pop it over the match
   and drop it back without relaunching. still open: true window transparency (the system webview
   makes this fiddly per-OS) and auto re-fit when the game window moves/resizes (needs OS window
   tracking).
9. **batch commands / inputs from a file** -- the UI half. the CLI half is `peptide tell
   --file <path>` (one command per line, `#` comments skipped; mixes engine cmds, `e`
   hscript, and `seq`/`hold` inputs through the one dispatch path).
10. **way more hscript commands.** convenience commands wrap common eval patterns through the
    one dispatch path: `scenario` (#4), `tune` (#5), `dmg <player> <value>` (set damage percent),
    `info` (one-shot readout of both players' x / state / damage / team), `reset` (neutral state +
    zero momentum), and `kill <player>` (force a KO into the bottom blast zone). all validated +
    unit-tested. more can be added the same way (a `Cmd` registry entry + a `parse_*` that returns
    `Command::Eval`).
11. **SSF2 command parity.** the SSF2 backend lowers the same command vocabulary to reflection
    verbs, so the host-side eval macros work there too. `dmg`/`info`/`reset`/`kill` are verified
    live on SSF2 (the evaluator handles a `;`-joined multi-statement line, an `[…]` array literal,
    the `damage._damage` getter/setter idiom, position/velocity setters as property writes, and
    `toState(neutral)` as the SSF2 state setter); `console` declares its gap (SSF2 has no debug
    console). `dmg`/`info`/`kill` are verified on Fraymakers too. input injection
    (`hold`/`release`/`seq`, `scenario`'s timeline), `addCharacter`, and `tune` all work on SSF2
    now (see #1/#3/#4/#5). parity is NOT closed out; still open:
    - **the `status` feed on SSF2** isn't working 100% yet (the matchStatus readout the GUI
      widgets consume), so the command set is close but not at parity.
    - **`reset` on Fraymakers** trips a script error (one of its setters / `toState` resolves to
      null on the FM character); the SSF2 path is unaffected. pre-existing, unrelated to the SSF2
      parity work, worth a separate look.
