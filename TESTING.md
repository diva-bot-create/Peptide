# validating a converted character

how to take a converted SSF2 character end-to-end and prove it's correct, both in the
**FrayTools editor** (does it lay out / render right?) and in the **Fraymakers engine** (does
it load, spawn, animate, and behave at runtime?).

there are **two** harnesses doing two different jobs. each lives under `tools/` and contains
**no Fraymakers/FrayTools code, assets, or strings** -- they just drive the user's *local*
installs over standard protocols. everything they touch (engine artifacts, patched copies,
`.fra` packages, `node_modules/`) stays on the user's machine and is git-ignored.

> **copyright / compliance boundary -- never publish.** to respect Team Fray's wishes, the
> tracked docs **don't explain how to decompile or patch the Fraymakers engine or
> the FrayTools bundle**, and **don't name specific non-hscript engine classes / functions /
> fields** or the engine's internal symbol map (move-dispatch / telemetry / load symbols, the
> FrayTools render internals). we haven't included that material; please keep it that way. it's
> fine to document what the harness *does* and how to contribute to it (the commands, the
> patcher workflow, resolve-by-name, the `doctor` preflight). never commit or publish the
> Fraymakers engine bytecode/Haxe/disassembly, FrayTools bundle code, SSF2 AS3, decompiled
> output, or any of their assets. see [`NOTICE.md`](NOTICE.md) "Reverse-engineering &
> copyright boundary" and [`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) "engine-side knowledge is not
> in this repo".

| Harness | Dir | Drives | Transport | Answers |
|---|---|---|---|---|
| **FrayTools** (`peptide`) | `src/fraytools.rs` | the user's FrayTools editor | Chrome DevTools Protocol | "does it render / lay out right? publish me a `.fra`" |
| **Fraymakers** (`peptide`) | `src/` + `tools/` | the user's Fraymakers engine | patched engine copy + loopback TCP | "does it behave at runtime? load / spawn / move / read state" |

both surfaces are subcommands of the one `peptide` binary; the detailed command surface lives in:
- [`docs/PEPTIDE_GUIDE.md`](docs/PEPTIDE_GUIDE.md) -- `peptide export` (publish), `peptide harness` (box geometry + capture), `peptide render`, the live-engine harness, the loopback bridge, and `tools/run.sh`. `compare_boxes` is a `dev-tools`-gated diagnostic bin.

this doc is the **cross-tool workflow + the current validation status**. the engine-side
integration map isn't in the tracked repo (removed for compliance, see the boundary note).

---

## 1. the end-to-end iteration loop

```
# 1. Make a converter change
$EDITOR src/<...>.rs

# 2. Rebuild + regenerate the target character
cargo build --release
./build/release/peptide convert ../ssf2-ssfs/<id>.ssf
#    → ./characters/<id>/   (⚠ verify it is FRESH — see §6 stale-output trap)

# 3. Publish to .fra → lands in Fraymakers' custom/<id>/
./build/release/peptide export \
  --project "$PWD/characters/<id>/<id>.fraytools"

# 4. Boot ONE persistent engine session, then drive + observe it iteratively
./build/release/peptide session --full &        # boots + holds a live engine
#   wait for "engine READY" in:  ./build/release/peptide log
./build/release/peptide tell "spawn <id>"                       # start a real match
./build/release/peptide tell "match.getCharacters()[0].getStateName()"   # read state
./build/release/peptide tell "match.getCharacters()[0].toState(CState.JAB)"  # drive a move
./build/release/peptide log -n 20                # read everything the engine streamed back
./build/release/peptide tell "exit"             # clean shutdown when done

# 5. Compare behaviour vs expected → fix in the converter (never hand-edit output) → repeat
```

**the session IS the loop.** `peptide session` is the canonical way to test and iterate on a
converted character: it boots the engine ONCE and HOLDS the TCP link open, so you send a
command (`peptide tell …`), read the reply (`peptide log`), decide the next one, and repeat,
all against the *same* live match. full command surface in §3. (the old one-shot
`tools/run.sh "<cmd>" <secs>` / `tools/runseq.sh` paths still work for scripted runs, but they
reboot per invocation, so use a session for interactive iteration.)

**validating a boot (notes for agents):**
- a quick boot is **fast** -- you'll see `READY` → `LAUNCHED` → `ANIM:STAND` in **under ~20s**.
  don't budget 40s; snapshot `peptide log -n 40` once the boot's had ~15-20s and you'll have
  the whole sequence.
- **`timeout` doesn't exist on macOS** (it's `gtimeout` from coreutils, if installed). to
  observe the stream without blocking, just take a couple of `peptide log -n N` snapshots
  rather than `timeout … peptide log --follow`.

**which harness when:**
- **FrayTools-side** (`peptide harness` / `peptide export`) -- visual/layout ground truth (box
  geometry, pivots, rendering) and producing the publishable `.fra`. pair with `compare_boxes`
  for a numeric verdict.
- **Fraymakers-side** (`peptide session`) -- runtime behaviour (loads, spawns, animates,
  transitions state, responds to a move dispatch). where freeze / crash / physics bugs surface.

**branch note:** `main` holds the converter **and** the merged harness work. (older
`fraymakers-match-harness` / `steam-shim-experiments` branches are historical.)

---

## 2. FrayTools harness -- publish + box geometry

code: `src/fraytools.rs` (pure Rust; speaks CDP over HTTP + WebSocket directly, no Node). it
drives the user's local FrayTools (Electron + CDP) through FrayTools' own runtime objects, no
pixel coordinates.

- **`peptide export`** runs FrayTools' own **Publish** (the Fraymakers Content Exporter) on a
  converted project and prints the published `.fra` path. "Publish All" writes both
  `<projectDir>/build/` and the Fraymakers `custom/<id>/` dir the converter wired up. this is
  what the GUI's "Export in FrayTools" button shells out to.
- **`peptide harness`** opens an entity, navigates to an animation + frame via Redux
  `store.dispatch`, reads box geometry from the entity JSON, captures the stage as a PNG, and
  emits a JSON report whose `rendered_anchor` field is *FrayTools' own* placement of each box's
  pivot.

**cold-launch handling.** on a cold launch FrayTools' `/json/version` answers 200 before the
renderer registers an inspectable page target, so the scripts `waitForTarget()` (poll
`/json/list` for a real `page`/`webview` target) before connecting.

tested against **FrayTools 0.4.0**; role detection is by method/prop presence (not class name),
so it's somewhat version-tolerant. the script leaves FrayTools running on success (attach
model); if CDP attach starts failing mid-session, kill FrayTools and let the script cold-launch
a fresh instance.

### `compare_boxes` -- numeric verdict

```
cargo run -p ssf2_converter --features dev-tools --bin compare_boxes -- \
  --ssf2 ../ssf2-ssfs/<id>.ssf \
  --char <id> --json /tmp/box.json [--tolerance 2.0]
```

reads the harness JSON, parses the SSF2 source, matches boxes by type + size, and reports
per-box drift (FrayTools rendered anchor vs SSF2 intended position). it's **split-aware** --
split sub-animations (`strong_forward_attack`, `*_land`, `grab_hold`, …) map back to their
source SSF2 anim + start-frame offset so they verify instead of being skipped. exits 0 on full
pass, 1 on any failure.

---

## 3. Fraymakers engine harness (`peptide`)

code: `src/` (Rust), with shell orchestration in `tools/`. two bins:
- **`peptide`** parses the Fraymakers engine's bytecode (`hlbc` crate), injects a per-frame
  dispatch block, and writes a patched throwaway copy. also has read-only inspection subcommands.
- **`peptide`** is the loopback TCP server (the injected engine code is the client).

the command loop runs against the live match. the **engine's internal symbol map** (the named
move-dispatch / telemetry / content-load functions, type/field layout, `CState` integer values)
is intentionally **not documented in this repo**, see the compliance boundary note at the top.
any such notes belong only in the gitignored `docs/` scratch space.

### boot model -- `peptide session` (canonical) + `tell` / `log`

`peptide session` is the primary surface for interactive, agent-driven testing: one command
boots a throwaway-patched engine, holds the loopback TCP link open, and runs a command loop you
feed over time.

```
peptide session [--full | --char <id>] [--dir D]   # boot + hold a live engine (long-lived)
peptide tell "<command>"                           # queue a command for the running session
peptide log  [-n N] [--follow]                     # print/tail the engine replies it mirrored
```

run in a terminal, `session` holds focus and is also an interactive REPL, the terminal
equivalent of the GUI chat. type a command, press Enter, and the engine's reply streams
straight back to the same window (the TCP link stays live the whole time). it's enabled
automatically when stdin is a TTY; `peptide tell` from another terminal still works alongside
it. force it on for a piped stdin with `-i` / `--interactive`, or off (pure `tell`-driven
daemon) with `--no-input`. EOF (Ctrl-D) just stops reading input; type `exit` (or Ctrl-C) to
shut the engine down.

- **`session`** boots a throwaway-patched copy of the engine (the pristine install is never
  touched), binds the loopback server, and waits for the engine to dial in + reach READY. on
  clean exit (or `tell exit`) it kills the engine and removes the transient files. needs
  `dangerouslyDisableSandbox` (the engine's window).
  - `--full` = Title/UGC bridge boot, then drive it with `tell "spawn <id>"` once READY (the
    reliable path; avoids the headless filtered-load crash).
  - `--char <id>` = headless fast-boot baking that character.
  - `--no-boot --port N --token T` = attach to an engine launched elsewhere.
- **`tell`** appends one command to the session's control file; the daemon picks it up within
  ~50ms, translates it, and sends it. commands are the same friendly vocabulary as everywhere
  else (`spawn`, `exit`, or any hscript expression).
- **`log`** prints the mirrored engine output (the canonical record of replies); `--follow`
  tails it live. session state lives under `~/.peptide/session/` (override with `--dir` or
  `PEPTIDE_SESSION_DIR` for parallel sessions).

a session beats one-shots because it keeps the engine alive, so you send → observe → decide →
send again against the SAME match while the game streams TCP back. the one-shot `tools/run.sh`
and `tools/runseq.sh` reboot per run: fine for scripted batches, wrong for iteration.

**wedge discipline:** a crashed engine can leave an uninterruptible (UE) `hl _conn.dat` orphan
that `kill -9` can't reap (GPU/Metal wedge). batch your checks into ONE session; if orphans
pile up, the box needs a reboot before more boots behave. see
`memory/project_peptide-env-wedge-lesson.md`.

### commands

the full command surface -- the tiny friendly vocabulary (`spawn`, `eval`, `hold`, `release`,
`seq`, `load`, `console`, `exit`, `help`) and the **hscript-first model** (anything
unrecognized runs as an hscript expression against the live match) -- is documented once in
[`docs/PEPTIDE_GUIDE.md`](docs/PEPTIDE_GUIDE.md) "Commands -- and the hscript model". type
`peptide help` for the live list. below are only the testing-workflow specifics.

per-state-change animation telemetry streams as `ANIM:<state>` (the bridge dedups to changes
only), the animation-capture readback (criterion #5).

short-name resolution (`spawn` / `load`): a bare `sandbag` (no `::`) is tried against
`private::`, `custom::`, `public::`, `global::` in order; first existing resource wins.
`private::` is first so a bare name resolves to headless-loaded custom content. or pass a full
`namespace::package.id`.

**multi-command sessions:** hscript probes need the *same* live match (a reboot loses it). use
`tools/runseq.sh <gap_s> "cmd1" "cmd2" …` to feed a gapped sequence into one session: it boots
once (boot→READY budget via `FRAY_READY_BUDGET`), fires the first command at READY, and paces
the rest by `gap_s` (fractional OK). example:
`./tools/runseq.sh 6 "spawn sandbag" "match.getCharacters()[0].getStateName()" "match.getCharacters()[0].toState(CState.JAB)"`.

### script-error scan (does the translated code actually RUN?)

`tools/tests/script_error_scan.sh <id> …` converts, exports, then drives the character through
every `CState` and collects each trapped hscript error into a per-character signature:

```
SCAN_OUT=/tmp/before tools/tests/script_error_scan.sh sandbag falco   # baseline
…converter change…
SCAN_OUT=/tmp/after  tools/tests/script_error_scan.sh sandbag falco
diff -r /tmp/before /tmp/after
```

this is a different question from the spawn sweep. `batch_spawn_test.sh` asks "does it launch
and move"; Fraymakers TRAPS a script error, logs it, and carries on with that frame script dead
from the failing line onward, so a broken handler passes a spawn test and looks fine from the
outside. the signature is error text only (no counts, no positions), so two runs of the same
build match exactly and a diff is all real change.

two things the driving has to get right, both of which produce convincing garbage if you skip
them: **park between states** (driving FALL or an aerial leaves the fighter airborne, it drifts
out of the blast zone and dies, and every later action then errors on frame 1) and **spawn two
characters** (the `scenario` park addresses p0 AND p1, so a solo roster half-fails on a null p1
and floods the signature). `SCAN_EXPORT=0` reuses the published `.fra` when you only changed the
driving.

the `peptide` binary also exposes **read-only engine-inspection subcommands** used when
re-deriving the engine integration on a new build. their existence is noted in
[`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) "engine-side knowledge is not in this repo"; the
specifics and any findings stay in local-only notes, not here.

### the debugger overlay (a HUD that floats over the game)

`peptide overlay` is a standalone, semi-transparent, click-through HUD that floats on top of
the running game and shows the live state (current animation) plus the `SCRIPTERR:` error
stream. it's a separate process/window, not the Peptide GUI, so it works the same whether the
session was started from the CLI (`peptide session`) or the GUI. both session paths spawn it
for you and it self-exits when the engine/session dies. opt out with `--no-overlay` or
`PEPTIDE_OVERLAY=0`. on macOS it pins to the Fraymakers window and follows it; on other
platforms it shows as a fixed-position HUD (the per-platform window-find is the open piece).

it reads the session's `out.log`, so anything the engine surfaces into that log shows up: the
error half is per-engine (Fraymakers surfaces trapped script errors as `SCRIPTERR:` via the
bytecode patch; SSF2 surfaces its own through its bridge), the overlay just renders whatever
arrives. same ONE-vocabulary-TWO-engines seam as everything else (see AGENT_CONTEXT).

### driving + observing the GUI from code

the GUI can be driven headlessly for reproducible boot testing:

- `PEPTIDE_GUI_AUTOBOOT=<verb>` fires one boot IPC after the page loads -- `<verb>` is the part
  after `@@boot:`, e.g. `quick:mario`, `regular`, `ssf2:mario`. same as clicking that button.
- `PEPTIDE_GUI_TRACE=1` echoes every incoming IPC message and page-bound event to stderr, so a
  run can be captured + read back (pair it with `screencapture` to see the result).
- `PEPTIDE_NO_POLL=1` disables the per-200ms `matchStatus` poll (isolation knob when you suspect
  the host poll is perturbing a fragile match-start).

example: `PEPTIDE_GUI_TRACE=1 PEPTIDE_GUI_AUTOBOOT=quick:mario peptide gui` boots the GUI, quick-
boots mario, and traces the whole boot flow to stderr. note the freeze class some converted
chars hit on match frame 1 is NOT visible to `peptide tell`/eval (the frame loop is hung), so
confirm those visually (a screenshot), not over the socket.

### 3.0 surviving Fraymakers updates (the version-compatibility gate)

every Fraymakers update renumbers the engine's internal indices, so a patcher that pins
integers breaks silently. Peptide is built so a new build is a fast, self-diagnosing
turnaround. the rules (full write-up in [`docs/PEPTIDE_DESIGN.md`](docs/PEPTIDE_DESIGN.md)
"Version resilience"):

1. **resolve by name, not index** -- `find_fn`/`require_fn`, `find_type`, `find_field`. names survive a recompile; pinned integers silently point at the wrong thing.
2. **fail loudly, never fall back to a pinned index** -- a missing name aborts the patch instead of corrupting it.
3. **prefer hscript over hand-emitted bytecode** -- logic in [`commands.hsx`](commands.hsx) runs through the engine's own interpreter and is immune to index drift.
4. **avoid mid-function opcode injection** -- use the layout-robust insert helpers or hscript.

every engine symbol the patcher needs is declared once in [`src/manifest.rs`](src/manifest.rs);
**`doctor`** is the read-only preflight that resolves the whole manifest against a given engine
file and prints a pass/fail checklist. the same check runs at the top of every real patch and
**aborts before mutating anything** if a critical symbol is missing, so an incompatible build
fails loudly instead of producing broken output. in the GUI it shows in the boot modal
("Verifying engine N/N").

**version-bump loop:** run `doctor` against the new build → for each miss, find the symbol's
new name with the read-only inspection subcommands and update both `manifest.rs` and the
matching `require_*` call → repeat until clean → then the in-engine spawn-test below. (the
concrete engine symbol names and the technique for locating them are compliance-sensitive, so
keep them in local notes only; see the boundary note above.)

### 3.1 headless content load · 4. engine symbol map

> **not in this repo (compliance).** this repo doesn't document how custom content loads into a
> headless engine, or the engine's internal **symbol map** (the named move-dispatch /
> telemetry / load functions, type/field layout, `CState` integer values, the live-character
> access path). to respect Team Fray's wishes, that material -- specific non-hscript engine
> class/function/field names and the decompilation technique -- stays out of the tracked repo.
>
> to orient without it: the engine surface the harness uses is enumerated in code. read the
> **`MANIFEST` table in `src/manifest.rs`** (grouped by subsystem, each entry annotated with
> what it's for) and the **`connect_edit`** patcher in `src/main.rs`, which consumes those
> symbols. re-resolve anything that moved on a new build with the read-only inspection
> subcommands (`doctor`, `inspect`, `fnsof`, `dis`, …), and keep deeper notes in the gitignored
> `docs/` scratch space. full pointer: [`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) "engine-side
> knowledge is not in this repo". the drive-and-read validation walk itself (`LAUNCHED → STAND
> → drive a move → read state → match-live`) runs through the `spawn` + hscript commands above
> and shows up in the validation status below.

---

## 5. in-engine validation status

stop-condition for "a converted character is validated" (the original 6-criterion mandate,
sandbag as the reference character):

| # | Criterion | sandbag | mario |
|---|---|---|---|
| 1 | **Conversion clean** -- exit 0; `conversion_log.json` triaged | **MET** | **MET** |
| 2 | **FrayTools layout match** -- `compare_boxes` in tolerance | **MET** (§6) | not re-run |
| 3 | **Engine boots + spawns**, no crash | **MET** -- spawn → `T:STAND` | **MET** -- spawn → INTRO → STAND, stable |
| 4 | **Every move runs** via the internal control path | **MET** (spot-checked across categories) | **MET** -- full moveset driven |
| 5 | **Animations play** (per-state `ANIM:` stream) | **MET** | **MET** |
| 6 | **Physics within tolerance** of `CharacterStats.hx` | readback live | readback live (dash_attack moves x −130→49) |

**both reference characters (sandbag, mario) drive their full movesets in-engine, verified
live.** every state produces the expected `ANIM:<STATE>` transition + `M:OK` and recovers to
`STAND`, no crash. the generic load+drive pipeline is validated broadly across the roster too;
per-character coverage lives in [`docs/STATUS.md`](docs/STATUS.md).

**open / deferred converter bugs surfaced during validation:**
- **add-timer callback resolution (charge states).** charge frame scripts emit an add-timer
  call whose third argument should be the timer *callback* but is instead the effects array.
  `abc_parser` mis-resolves the SSF2 callback to the effects variable; the fix is in the
  AS3→hscript callback resolution. the emitted call is neutralized to an empty closure with a
  TODO, so it no longer null-derefs on a charged smash, but the timer does nothing.

---

## 6. validation template -- sandbag box geometry

the reference sweep, the template for the rest of the roster. convert clean, then:

```
peptide harness \
  --project "$PWD/characters/sandbag/sandbag.fraytools" \
  --entity entities/Sandbag.entity --animation <anim> --frame <n> \
  --out-json /tmp/box.json --port 9222
cargo run -p ssf2_converter --features dev-tools --bin compare_boxes -- \
  --ssf2 ../ssf2-ssfs/sandbag.ssf \
  --char sandbag --json /tmp/box.json --tolerance 2.0
```

(`compare_boxes` is a `dev-tools`-gated diagnostic bin.)

FrayTools renders only **static** collision layers (hurt / item / body); hitboxes are runtime
script data and don't appear here, so they're validated at runtime instead (§5).

**result (current output):** every hurt / hit / body box converts sub-pixel (0.000–0.001 px
drift) across the move set (idle, jabs, tilts, strongs, aerials, grab, specials), 0 FAIL.

**itembox -- the one to watch.** the `itemBox` is the only routinely rotated collision box
(placed relative to the hand attachment point, with `pivotY = height` so it rotates about the
hand). its rotated-affine anchor bake (`entity_gen.rs`) lands at <0.01 px drift in current
output. it's gameplay-noncritical (item pickup range, not hit/hurt detection), but since it's
the trickiest geometry it's the **first box type to re-check** if anything looks off. `peptide
harness`'s `rendered_anchor` is FrayTools' ground truth to bake against.

---

## 7. pitfalls

- **stale-output trap -- verify you're debugging FRESH output.** the converter **merges into**
  whatever already exists in `characters/<id>/`; it doesn't wipe the dir first, so files from
  old converter versions (renamed-away paths like `scripts/Character/Script.hx` with a broken
  loop, old `library/sounds/*.ogg`) silently persist. before trusting a runtime result, confirm
  the `.fra` you published came from a fresh regen (check
  `characters/<id>/library/scripts/<Pascal>/` mtimes; the dir should hold *only*
  current-converter paths). a full corpus purge once removed ~7,300 such files. *prevention
  idea:* have the converter `library/`-scope clean each char's output dir at the start of a run.
- **engine builds shift the integration target.** a new Fraymakers build can move the engine
  symbols the harness depends on, so the harness re-resolves them by name at patch time and
  fails loudly if one is missing. the how-to for re-resolving is compliance-sensitive and lives
  only in local notes, see [`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) "engine-side knowledge is not
  in this repo".
- **wedged `hl` processes.** orphaned `hl _conn.dat` procs from earlier boots occasionally
  wedge into uninterruptible sleep (`ps` STAT contains `U`, e.g. `UNE`); `kill -9` can't reap
  them and they only clear on a full reboot. a wedged orphan does **not** block a new engine
  from launching, so the "kill prior instance" step gets a hard ~2-second budget, then move on.
  don't loop / retry / wait it out.
- **stale `.fra` haunting -- verify the LOADED file, not just the source.** converter fixes
  silently never reach the game until every affected `.fra` is re-exported. before judging any
  in-game behavior: check `ls -la <install>/custom/*/` dates against the fix date, and grep the
  install `.fra` for a marker only the new build emits (`grep -ac <marker> <install .fra>`).
  a week-old character `.fra` once presented as a fresh animation-mapping bug ("spinning
  falls"), and a stage `.fra` raced an export twice and faked two thwomp regressions.
- **serialize session tests.** overlapping launches make the log reader match the PREVIOUS
  session's `out.log`. the discipline: kill `peptide session` AND the engine process, sleep a
  few seconds, regen + export, verify the install `.fra` marker, truncate `out.log` in the
  FOREGROUND, then launch once and wait for `LAUNCHED`.
- **silent engine-launch failure after heavy cycling.** occasionally a launch produces no
  engine process and an empty `out.log` (distinct from the harmless wedged-`U` orphans above).
  kill the session daemon, wait ~12 s, retry once; it has always come up on the second try.
- **live probing without a debugger.** the `.fra` embeds hscript SOURCE, so same-length text
  swaps byte-patched into the install `.fra` make cheap probes: rename a call (`inc()` to
  `inq()`) and the SCRIPTERR line number proves the statement runs; replace a condition with a
  padded `true` to test a branch body. for value probes, encode state in position
  (`self.setX(800 + bitmask)`) and read it through the `tree` command.
- **output is not the deliverable.** `characters/` is git-ignored, so converter output never
  shows in git. commit `src/` + docs changes, and re-derive output by re-running the converter
  (deterministic GUIDs make regen idempotent).
