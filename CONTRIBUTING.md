# contributing

a working agreement for keeping the code, tests, and docs in sync as the project grows.
the intent is light-touch: small habits that catch the routine drift, no bureaucratic
gates.

## copyright boundary (read first)

SSF2 (© McLeodGaming) and Fraymakers / FrayTools (© Team Fray) are proprietary.
**never commit or publish their source, bytecode, decompiled output, disassembly, stack
traces, assets, or extracted strings** -- not in code, not in docs, not in commit
messages. reverse-engineering notes are fine **in our own words** for interoperability, so
paraphrase, never paste. full policy is in [`NOTICE.md`](NOTICE.md) "Reverse-engineering &
copyright boundary". the git-ignored `docs/` scratch folder is still subject to all of
this, it's just not where durable notes belong (see "the `docs/` folder" below).

**special case -- the Fraymakers engine.** to respect Team Fray's wishes, two
things stay out of the tracked repo: (1) how to **decompile or patch** the
Fraymakers engine or the FrayTools bundle, and (2) **specific non-hscript engine class /
function / field names** or the engine's internal symbol map. that's the one category of
RE notes that never gets promoted into a durable doc. it stays local-only in the gitignored
`docs/` scratch space. it's totally **fine** to document what Peptide does and how to
contribute to it (the patcher architecture, resolve-by-name, the `doctor` workflow, where
handlers go) and to use the public hscript scripting API. the SSF2 / SWF / ABC input-side
format notes aren't affected. this also reaches **code comments**: a `.rs` comment must not
name an engine class/function/field or cite an engine code line (`name@12345`) -- the symbol
map lives only in the patcher source (`manifest.rs`, `connect_edit`, the RE bins). details:
[`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) "engine-side knowledge is not in this repo".

## quick checklist (do before opening a PR / pushing a branch)

a few questions, in order:

1. **did you change `src/**` or `crates/**`?**
   run `cargo test --release`. the `golden_sandbag` snapshot is the main safety net, so if
   its hashes change you've changed character output and the golden needs an explicit update
   (`crates/ssf2-converter/tests/golden/sandbag_hashes.txt`).

2. **did you touch one of the "hot files" below?**
   check the doc section listed next to it. if the change shifts the description, update the
   doc in the same commit.

3. **did you add or rename anything user-facing?**
   - new CLI flag? update DEVELOPMENT §3.3 (and the README quick-start if basic usage changes).
   - new `conversion_log.json` field? update DEVELOPMENT §4 step [11].
   - new diagnostic binary? add a row in DEVELOPMENT §5.7.
   - new mapping JSONC section? update DEVELOPMENT §6.

4. **did you change the stage converter** (`stage_parser.rs` / `stage_emit.rs` / `stage_abc.rs`)?
   the fix must be universal (pattern detected structurally or from source data, with an
   exact-semantics fallback + conversion-log warning), never keyed to one stage or hazard by
   name. then prove stages still convert: `just sweep-stages` (fails on any error). that is a
   fixed TEN-stage sample, not the corpus: converting all 110 takes hours and writes tens of
   gigabytes (one stage emits 687MB on its own), so the exhaustive `just sweep-stages-all` is
   for when a change touches shared art or geometry rendering and you want the full answer. the
   sample covers plain geometry, hazards declared in both initialize and update, weather, moving
   platforms, backdrop elements, and boundary/parallax handling. spot-check one OTHER stage's
   output if the change touches shared rendering.

5. **did you add a host-facing debug command or feature?**
   it has to work **identically on both engines** (Fraymakers + SSF2). define it once
   through the shared seam: a `DebugTarget` trait method (default = `eval`) plus the engine
   helper in `commands.hsx` AND `src/ssf2_target.rs`, never an `if engine == …` branch in
   command/feature logic. full rule in [`AGENT_CONTEXT.md`](AGENT_CONTEXT.md) "ONE command
   vocabulary, TWO engines".

that's it! the rest is just tooling that backs the checklist (see "tooling" below).

## hot file → doc-section map

when you change one of these, skim the listed doc section in the same commit. if the change
is big enough to need a doc edit, the linked section is where it goes.

the converter source is the `crates/ssf2-converter/` library; Peptide (live-engine harness,
webview UI, engine driver) is the root `src/`.

| Hot file | Affected doc sections |
|---|---|
| `crates/ssf2-converter/src/convert.rs` (orchestration: `detect_char_names`, `process_character`, `write_conversion_log`, `run_conversion`) | DEVELOPMENT §3.3, §4, §5.1 |
| `src/convert.rs` (peptide `convert` CLI adapter) | DEVELOPMENT §3.3 |
| `crates/ssf2-converter/src/extractor.rs` (`CharacterData` struct) | DEVELOPMENT §5.4 |
| `crates/ssf2-converter/src/abc_parser.rs` (Stage A bundle extraction, constructor walker, normalStats_id) | DEVELOPMENT §5.3 |
| `crates/ssf2-converter/src/decompiler.rs` | DEVELOPMENT §5.3 |
| `crates/ssf2-converter/src/sprite_parser.rs` (box geometry, xframe transforms) | DEVELOPMENT §5.4; AGENT_CONTEXT §"Collision Boxes" |
| `crates/ssf2-converter/src/image_extractor.rs` (PNG / placement / skew bake / projectile-and-head discovery) | DEVELOPMENT §5.4; AGENT_CONTEXT §"Image Sprites" |
| `crates/ssf2-converter/src/sound_extractor.rs` | DEVELOPMENT §5.4 |
| `crates/ssf2-converter/src/api_mappings.rs` (translation pipeline) | DEVELOPMENT §5.5 |
| `crates/ssf2-converter/src/mappings.rs` | DEVELOPMENT §5.2 |
| `crates/ssf2-converter/src/entity_gen.rs` | DEVELOPMENT §5.6; AGENT_CONTEXT §"Fraymakers Entity Format" |
| `crates/ssf2-converter/src/haxe_gen.rs` (output orchestrator + transformation banner) | DEVELOPMENT §5.6 |
| `crates/ssf2-converter/src/palette_gen.rs` | DEVELOPMENT §5.6 |
| `crates/ssf2-converter/src/uuid_gen.rs` | DEVELOPMENT §5.6; AGENT_CONTEXT §"Top-Level Structure" if GUID seeding changes |
| Peptide root `src/` (gui.rs, peptide_ui.html, config.rs, platform.rs, manifest.rs, fraytools.rs) | docs/PEPTIDE_DESIGN.md; docs/PEPTIDE_GUIDE.md; TESTING.md |
| `crates/ssf2-converter/mappings/commands.jsonc` | DEVELOPMENT §6.1 |
| `crates/ssf2-converter/mappings/character/*.jsonc` | DEVELOPMENT §6.2–§6.4 |
| `src/bin/*` | DEVELOPMENT §5.7 (add a row to the table) |
| `tests/*` | Update coverage/status in `docs/STATUS.md` if the change is significant |

## when you delete code

deletion is the doc-drift case the tests have the hardest time catching. the convention:

- **removed a function?** grep the repo for references first:
  `git grep -n <fn_name> -- '*.md' '*.rs'`. update any doc hits.
- **removed a flag / mapping / output field?** same drill. watch out for the `*` substring
  matches especially, e.g. removing `extract_costume_*` meant grepping for any
  `extract_costume_*` reference.
- **removed a "known issue" because you fixed it?** strike its entry in `docs/STATUS.md`
  (both the "known issues & gaps" list and the matching "prioritized next steps" item).

## when you fix a "known issue"

known issues, the code-quality backlog, and prioritized next steps all live in
**`docs/STATUS.md`** (the single home for converter status and TODOs). when you fix
something, strike its entry in the "known issues & gaps" list there, plus the matching
"prioritized next steps" / "code-quality backlog" item.

## tooling

we don't gate on docs. we do have two light checks:

- **`tests/doc_freshness.rs`** -- two guards. `no_stale_terms_in_top_level_docs` flags
  known-stale terms (references to deleted functions, a detection paradigm we replaced,
  etc.); add a pattern whenever you remove something user-visible.
  `no_engine_internals_in_tracked_docs` is the **compliance trip-wire**, and it fails if any
  tracked doc names a Fraymakers/FrayTools engine-internal symbol (see the "special case"
  above). keep those names in the gitignored `docs/` scratch space only. both run in
  `cargo test`.
- **`tests/conventions.rs :: no_engine_internals_in_code_comments`** -- the same compliance
  trip-wire for `.rs` comments: it fails if a comment names a Fraymakers/FrayTools engine
  symbol or cites an engine code line (`name@12345`), outside the patcher source that is the
  symbol map's home (`manifest.rs`, `connect_edit` in `main.rs`, the `src/bin/` RE tools).
- **`tests/constructor_walk_detection.rs :: corpus_constructor_walk_matches_path2_enumeration`**
  -- guards the detection invariant (constructor walk and the fallback enumeration agree
  across the corpus). don't bypass this; fix the underlying divergence.

no pre-commit / pre-push hooks. they add friction for a one-person / small-team flow and
miss the real failure mode anyway (someone confidently breaking a doc reference). the test
suite + the checklist are the gates.

## commit hygiene

- one logical change per commit. big mixes (`fix X + refactor Y`) make doc drift harder to
  spot, so split them.
- the commit message body explains the *why* and lists files-changed by intent group. see
  `git log` for recent examples.
- when the change touches docs, mention the section in the message: "DEVELOPMENT §5.3:
  updated abc_parser description." future you will grep for it.

## architectural rationale

the *why* behind major design decisions (the path 1 → path 2 stat-extraction switch, the
constructor-walk detection, the multi-character project layout) lives in `git log` / commit
messages, not in a dedicated doc section. when you land a substantial design change, explain
the *why* in the commit message. keep the durable, current-state docs at the repo top level
(README / DEVELOPMENT / AGENT_CONTEXT / TESTING / CONTRIBUTING / NOTICE) plus the Peptide
docs in `docs/`, and don't let development history pile up in them.

## the `docs/` folder

`docs/` holds the **tracked** Peptide harness docs -- `PEPTIDE_GUIDE.md`,
`PEPTIDE_DESIGN.md`, `STATUS.md` (re-included via `.gitignore` negations) -- and is
otherwise a **gitignored scratch space** for working notes during a task (investigation
logs, RE findings mid-flight, throwaway plans).

the rule: **if a scratch note would help anyone else** (a fact, a gotcha, an RE finding, a
design decision, a "what's still open") **promote it into the relevant durable doc** (a
top-level doc, or one of the three `docs/` files), then delete the scratch note. don't let
durable knowledge live only in untracked scratch, because the next person (or agent) cloning
the repo never sees it.

**the one exception:** Fraymakers engine and FrayTools RE notes (see the "special
case" under copyright boundary above) are **never** promoted. they stay in the gitignored
scratch space permanently. `docs/ENGINE_INTERNALS.local.md` is the seed for that material,
so extend it locally and never move it into a tracked file.
