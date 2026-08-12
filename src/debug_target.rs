//! debug_target — the OOP seam that lets ONE command vocabulary drive BOTH
//! engines. `interpreter::parse` turns a human line into an engine-agnostic
//! `Command`; a `DebugTarget` executes it. `FraymakersTarget` speaks the HashLink
//! socket/wire protocol; `Ssf2Target` (src/ssf2_target.rs) speaks AVM2 reflection
//! over file-IPC. `run_command` is the single dispatcher used by the session
//! layer, so `spawn sandbag`, `e match.getCharacters()[0]...`, `hold down+special`,
//! `seq …` etc. behave identically regardless of which engine is attached.

use anyhow::Result;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::interpreter::{self, Command, SpawnArgs};

/// A debugger backend: executes the engine-agnostic [`Command`] set. Each method
/// returns the engine's textual reply (or `()` for a fire-and-forget shutdown).
///
/// FEATURE SURFACE: a host-facing feature (matchStatus, char icons, and anything
/// added later) is modelled here as a trait method whose DEFAULT implementation
/// just evaluates the engine helper of the same name (`matchStatus()`,
/// `iconFeed(slot)`, …). Both backends implement `eval`, so a new feature
/// automatically reaches BOTH engines the moment each one's `eval` knows the
/// expression — Fraymakers via `commands.hsx`, SSF2 via `ssf2_target`'s evaluator.
/// An engine that genuinely can't do a feature overrides the method to say so
/// (e.g. SSF2 has no stock-icon pipeline → `char_icon` returns `None`).
pub trait DebugTarget {
    #[allow(dead_code)] // part of the trait surface; not all callers wired yet
    fn engine(&self) -> &'static str;
    fn eval(&mut self, expr: &str) -> Result<String>;
    fn spawn(&mut self, args: &SpawnArgs) -> Result<String>;
    fn hold(&mut self, mask: u32) -> Result<String>;
    fn seq(&mut self, masks: &[u32]) -> Result<String>;
    fn console(&mut self) -> Result<String>;
    fn add_character(&mut self) -> Result<String>;
    fn exit(&mut self) -> Result<()>;
    fn load(&mut self) -> Result<String>;

    /// The per-character status feed (`MATCHSTATUS:<id>|<dmg>|<anim>;…`) the host
    /// polls into the matchStatus widget. Default: eval the engine's `matchStatus()`
    /// helper. `None` when there's no live match (empty feed).
    fn match_status(&mut self) -> Result<Option<String>> {
        Ok(non_empty(strip_eval(self.eval("matchStatus()")?)))
    }

    /// A character's stock icon for match `slot` (`ICON:<slot>:<hex>;<palette>`),
    /// host-requested on demand. Default: eval `iconFeed(slot)`. Engines without a
    /// stock-icon pipeline override this to return `None` (the widget keeps a glyph).
    fn char_icon(&mut self, slot: u32) -> Result<Option<String>> {
        Ok(non_empty(strip_eval(self.eval(&format!("iconFeed({slot})"))?)))
    }

    /// Dump the live object tree to `depth` — the stage-triage ground truth (every
    /// live object's name/class/position/size/frame). SSF2 walks its display list
    /// via reflection; an engine without a tree walk declares the gap.
    fn tree(&mut self, _depth: u32) -> Result<String> {
        Ok("tree: no live object-tree walk on this engine yet (SSF2-only)".into())
    }

    /// Character `idx`'s live animation clock. Default: eval the engine's `animFeed(i)`
    /// helper. `None` when that character doesn't exist (or there's no live match).
    ///
    /// This is the primitive every frame-accurate operation is built on. Waiting on
    /// wall-clock time is not a valid substitute: an engine's frame rate varies with
    /// load, so a sleep long enough to be safe is mostly idle and a sleep short enough
    /// to be quick silently observes a half-played animation. Read the clock instead.
    fn char_anim(&mut self, idx: usize) -> Result<Option<AnimState>> {
        Ok(non_empty(strip_eval(self.eval(&format!("animFeed({idx})"))?))
            .and_then(|s| AnimState::parse(&s)))
    }

    /// Arm/disarm the engine's own per-frame recorder, clearing its buffer.
    ///
    /// Default: declare the gap. Fraymakers doesn't need one — it PUSHES per-frame `ANIM:`
    /// telemetry over its socket and its hscript eval is fast enough that `await` can poll
    /// the clock directly. SSF2 has neither, so it implements this with injected bytecode.
    fn record(&mut self, _on: bool) -> Result<String> {
        Ok("record: this engine streams per-frame telemetry already — use `await` \
            (no host-side recorder needed)".into())
    }

    /// Read back the recorded frame history and stop recording. See [`Self::record`].
    fn frame_trace(&mut self) -> Result<String> {
        Ok("trace: this engine has no host-side frame buffer — its per-frame ANIM \
            telemetry is already in the session log, and `await` reports frame counts"
            .into())
    }
}

/// One sample of a character's animation clock: which animation, how far into it, how
/// long it is, and where the character is. `frame`/`total` are `-1` when the engine
/// couldn't report them.
#[derive(Clone, Debug, PartialEq)]
pub struct AnimState {
    pub anim: String,
    pub frame: i32,
    pub total: i32,
    pub x: f64,
    pub y: f64,
    pub state: String,
}

impl AnimState {
    /// Parse the shared `<anim>|<frame>|<total>|<x>|<y>|<state>` wire shape both
    /// backends emit. Returns `None` if it isn't that shape, so a garbled or
    /// no-live-match reply reads as "no sample" rather than a bogus one.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        // an engine reply can carry a leading tag or trailing noise; take the last
        // line that actually has the field count we need.
        let line = s.lines().rev().find(|l| l.matches('|').count() >= 5)?;
        let f: Vec<&str> = line.rsplitn(6, '|').collect();
        // rsplitn yields reversed; re-order to (anim, frame, total, x, y, state)
        let (state, y, x, total, frame, anim) = (f[0], f[1], f[2], f[3], f[4], f[5]);
        Some(AnimState {
            anim: anim.trim().trim_start_matches("E:").trim().to_string(),
            frame: frame.trim().parse().unwrap_or(-1),
            total: total.trim().parse().unwrap_or(-1),
            x: x.trim().parse().unwrap_or(0.0),
            y: y.trim().parse().unwrap_or(0.0),
            state: state.trim().to_string(),
        })
    }
    /// `true` once the clock has reached the end of the animation.
    pub fn at_end(&self) -> bool { self.total > 0 && self.frame >= self.total - 1 }
}

/// Why [`await_animation`] stopped watching.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnimOutcome {
    /// Played to its last frame.
    Completed,
    /// The clock wrapped back — a looping animation, which never "finishes".
    Looped,
    /// The engine moved on to a different animation.
    Changed,
    /// The clock stopped advancing without reaching the end (a paused or dead handler).
    Stalled,
    /// Ran out of budget while still advancing.
    BudgetExhausted,
    /// No character / no readable clock.
    NoSample,
}

/// The result of watching one animation play: how it ended, the last sample taken, and
/// how many distinct frames were actually observed advancing.
#[derive(Clone, Debug)]
pub struct AnimWatch {
    pub outcome: AnimOutcome,
    pub last: Option<AnimState>,
    pub frames_seen: u32,
}

/// Watch character `idx`'s animation until it finishes, loops, changes, or stalls —
/// polling the engine's own frame counter, never a wall-clock delay.
///
/// `budget_frames` bounds the watch so a looping or wedged animation can't hang the
/// caller; it is a CEILING, not a pace. `stall_polls` is how many consecutive samples
/// may show the same frame before we call it stalled — the engine is genuinely
/// between frames sometimes, so one repeat is not evidence of anything.
///
/// Goes through `char_anim`, so it works on Fraymakers and SSF2 without a branch.
pub fn await_animation(
    target: &mut dyn DebugTarget,
    idx: usize,
    budget_frames: u32,
    stall_polls: u32,
) -> Result<AnimWatch> {
    watch_animation(|| target.char_anim(idx), budget_frames, stall_polls)
}

/// Poll until a character is actually readable, or `tries` samples have come back empty.
///
/// A match doesn't exist the instant `spawn` returns: the engine still has to construct
/// the characters, and how long that takes depends on what content it had to load. Every
/// driver here used to paper over that with a fixed delay or by grepping the log for a
/// hopeful line, which costs the first observation of every run — the Fraymakers scan
/// opened with `NoSample` and silently threw away its first driven state.
pub fn await_live(
    mut sample: impl FnMut() -> Result<Option<AnimState>>,
    tries: u32,
) -> Result<Option<AnimState>> {
    for _ in 0..tries.max(1) {
        if let Some(s) = sample()? { return Ok(Some(s)); }
    }
    Ok(None)
}

/// The frame-watching state machine itself, over any source of samples.
///
/// Split out from [`await_animation`] because the two live drivers reach the engine
/// differently: the `DebugTarget` path calls `char_anim` and gets the reply back
/// inline, while `bridge::serve` hands its socket reader to a printing thread and has
/// to pick replies off a channel. Only the sampling differs; the decision about when
/// an animation is done must not.
/// How long the clock may sit on the same frame before the watch calls it stalled.
/// Generous enough to cover a slow frame on either engine (SSF2 33ms, Fraymakers 16ms).
const STALL_AFTER: Duration = Duration::from_millis(400);

pub fn watch_animation(
    mut sample: impl FnMut() -> Result<Option<AnimState>>,
    budget_frames: u32,
    _stall_polls: u32,
) -> Result<AnimWatch> {
    let Some(first) = sample()? else {
        return Ok(AnimWatch { outcome: AnimOutcome::NoSample, last: None, frames_seen: 0 });
    };
    let start_anim = first.anim.clone();
    let mut prev = first;
    let mut frames_seen = 0u32;
    // "Stalled" is measured as TIME SINCE THE CLOCK LAST MOVED, not as a count of
    // identical samples. A poll count is the wrong unit: sampling faster than the engine
    // renders naturally returns the same frame several times in a row, so a fixed count
    // reports a perfectly healthy animation as stalled — and how often that happens
    // depends on the sampler's speed, so the same code silently behaved differently on
    // the two engines (Fraymakers polls at roughly one sample per frame and squeaked by;
    // SSF2's evals are sub-millisecond against a 33ms frame, so it tripped instantly and
    // every SSF2 watch came back Stalled with frames_seen=0).
    let mut last_advance = Instant::now();
    let mut polls = 0u32;
    // Backstop only. A fast sampler takes many polls per rendered frame, so this can't be
    // budget*2 any more — it has to be loose enough not to cut a healthy animation short.
    let max_polls = budget_frames.max(1) * 200;
    loop {
        polls += 1;
        if polls > max_polls {
            return Ok(AnimWatch { outcome: AnimOutcome::BudgetExhausted, last: Some(prev), frames_seen });
        }
        let Some(cur) = sample()? else {
            return Ok(AnimWatch { outcome: AnimOutcome::NoSample, last: Some(prev), frames_seen });
        };
        if cur.anim != start_anim {
            return Ok(AnimWatch { outcome: AnimOutcome::Changed, last: Some(cur), frames_seen });
        }
        if cur.frame < prev.frame {
            return Ok(AnimWatch { outcome: AnimOutcome::Looped, last: Some(cur), frames_seen });
        }
        if cur.frame > prev.frame {
            frames_seen += cur.frame.saturating_sub(prev.frame).unsigned_abs();
            last_advance = Instant::now();
        } else if last_advance.elapsed() >= STALL_AFTER {
            let outcome = if cur.at_end() { AnimOutcome::Completed } else { AnimOutcome::Stalled };
            return Ok(AnimWatch { outcome, last: Some(cur), frames_seen });
        }
        if cur.at_end() {
            return Ok(AnimWatch { outcome: AnimOutcome::Completed, last: Some(cur), frames_seen });
        }
        prev = cur;
    }
}

/// Frames the Fraymakers engine has PUSHED since the last `record`.
///
/// The exact counterpart of `ssf2_bridge::FRAMES`. Both engines now emit `FRAME:` per frame
/// and the host buffers it, so `trace` reads the same shape from either one and a timing
/// comparison isn't comparing two different measurements.
static FM_FRAMES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Route one engine-pushed frame line into the buffer (bounded, so a session left
/// recording can't grow without limit: 20k frames is ~5.5 minutes at 60fps).
pub fn push_frame(state: &str) {
    let mut f = FM_FRAMES.lock().unwrap();
    if f.len() < 20_000 { f.push(state.to_string()); }
}
pub fn fm_frames_clear() { FM_FRAMES.lock().unwrap().clear(); }
pub fn fm_frames_take() -> Vec<String> { FM_FRAMES.lock().unwrap().clone() }

/// The ONE rendering of each host-computed result, shared by every driver and both
/// engines. These strings previously existed in three files (bridge.rs's serve loop, the
/// Fraymakers target, the SSF2 target) and could drift apart — which is exactly how `tree`
/// ended up answering "SSF2-only" through one path and working through another.
pub fn fmt_record(on: bool) -> String {
    if on { "record: window opened (engine pushes FRAME: telemetry)".into() }
    else { "record: the recorder always pushes — `trace` reads the window".into() }
}

/// `TRACE:` header plus one `<animation> xN` row per run. `ends[i]`, when present, is the
/// position the run finished at (SSF2 carries it in the stream; Fraymakers doesn't).
pub fn fmt_trace(total: usize, runs: &[(String, usize)], ends: &[Option<String>]) -> String {
    let mut out = format!("TRACE:{total} frames, {} animations\n", runs.len());
    for (i, (label, n)) in runs.iter().enumerate() {
        let end = ends.get(i).and_then(|e| e.as_deref()).unwrap_or("");
        out.push_str(&format!("  {label} x{n}{end}\n"));
    }
    out
}

pub fn fmt_await(w: &AnimWatch, idx: usize) -> String {
    match &w.last {
        Some(s) => format!(
            "AWAIT:{:?} anim={} frame={}/{} frames_seen={} pos=({:.1},{:.1})",
            w.outcome, s.anim, s.frame, s.total, w.frames_seen, s.x, s.y),
        None => format!("AWAIT:{:?} (no live character p{idx})", w.outcome),
    }
}

pub fn fmt_ready(live: Option<&AnimState>, idx: usize, tries: u32) -> String {
    match live {
        Some(s) => format!("MATCHREADY:p{idx} anim={} pos=({:.1},{:.1})", s.anim, s.x, s.y),
        None => format!("MATCHREADY:none (p{idx} never became readable in {tries} tries)"),
    }
}

/// Run-length-encode a per-frame stream into `"<anim> xN"` records — the comparable unit
/// across engines. Shared so Fraymakers and SSF2 can't report it differently.
pub fn rle_frames(frames: &[String]) -> Vec<(String, usize)> {
    let mut runs: Vec<(String, usize)> = Vec::new();
    for f in frames {
        let label = f.split('|').next().unwrap_or(f).trim().to_string();
        if label.is_empty() { continue; }
        match runs.last_mut() {
            Some((l, n)) if *l == label => *n += 1,
            _ => runs.push((label, 1)),
        }
    }
    runs
}

/// Drop a leading `E:` (the Fraymakers eval-reply wrapper) so feed payloads are
/// uniform regardless of which backend produced them.
fn strip_eval(s: String) -> String {
    s.strip_prefix("E:").map(str::to_string).unwrap_or(s)
}
fn non_empty(s: String) -> Option<String> {
    if s.trim().is_empty() { None } else { Some(s) }
}

/// Parse `line` and execute it on `target`. Returns `Some(reply)` to show the
/// user, or `None` for a no-op (empty line). The single front door shared by the
/// Fraymakers and SSF2 session loops — identical syntax, identical routing.
pub fn run_command(target: &mut dyn DebugTarget, line: &str) -> Result<Option<String>> {
    Ok(match interpreter::parse(line) {
        Command::Help => Some(interpreter::help_text()),
        Command::Client(s) => if s.trim().is_empty() { None } else { Some(s) },
        Command::Spawn(a) => Some(target.spawn(&a)?),
        Command::Eval(e) => Some(target.eval(&e)?),
        Command::Hold(m) => Some(target.hold(m)?),
        Command::Seq(s) => Some(target.seq(&s)?),
        // Scenario = set up the scene (eval), then play p0's input timeline (seq).
        // Same DebugTarget seam, so it works identically on Fraymakers and SSF2.
        Command::Scenario { setup, masks } => {
            let mut out = target.eval(&setup)?;
            if !masks.is_empty() {
                let s = target.seq(&masks)?;
                if !s.trim().is_empty() {
                    if !out.trim().is_empty() { out.push('\n'); }
                    out.push_str(&s);
                }
            }
            Some(out)
        }
        Command::Await { idx, budget } => {
            let w = await_animation(target, idx, budget, 3)?;
            Some(fmt_await(&w, idx))
        }
        Command::AwaitMatch { idx, tries } => {
            let live = await_live(|| target.char_anim(idx), tries)?;
            Some(fmt_ready(live.as_ref(), idx, tries))
        }
        Command::Info => {
            // Enumerate the players that exist rather than assuming two. `char_anim`
            // already reports animation/position/state and answers None for an absent
            // slot, so this is correct for any roster size on either engine.
            let mut out = String::new();
            for idx in 0..4 {
                let Some(s) = target.char_anim(idx)? else { continue };
                let dmg = strip_eval(target.eval(&format!("p{idx}.damage._damage"))
                    .unwrap_or_default());
                let dmg = dmg.trim();
                // The per-frame sampler drops `state` to keep a poll cheap; `info` is a
                // one-shot readout, so it can afford the extra read rather than print "?".
                let state = if s.state == "?" || s.state.is_empty() {
                    let v = strip_eval(target.eval(&format!("p{idx}.getStateName()"))
                        .unwrap_or_default());
                    let v = v.trim().to_string();
                    if v.is_empty() { "?".into() } else { v }
                } else { s.state.clone() };
                out.push_str(&format!(
                    "p{idx}: anim={} frame={}/{} pos=({:.1},{:.1}) state={} damage={}\n",
                    s.anim, s.frame, s.total, s.x, s.y, state,
                    if dmg.is_empty() { "?" } else { dmg }));
            }
            Some(if out.is_empty() { "info: no live characters".into() } else { out })
        }
        Command::Record { on } => Some(target.record(on)?),
        Command::Trace => Some(target.frame_trace()?),
        Command::Console => Some(target.console()?),
        Command::Tree(depth) => Some(target.tree(depth)?),
        Command::AddCharacter => Some(target.add_character()?),
        Command::Exit => { target.exit()?; Some("exit".into()) }
        Command::Load => Some(target.load()?),
    })
}

// ─────────────────────────── Fraymakers backend ───────────────────────────

/// Drives the live Fraymakers engine over its loopback TCP socket. Each
/// `Command` is encoded to the wire via `interpreter::command_to_wire` (so the
/// protocol stays in one place) and the reply is drained synchronously.
pub struct FraymakersTarget {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl FraymakersTarget {
    pub fn new(reader: BufReader<TcpStream>, writer: TcpStream) -> Self {
        // Short read timeout so draining replies returns once the engine goes quiet.
        let _ = reader.get_ref().set_read_timeout(Some(Duration::from_millis(120)));
        FraymakersTarget { reader, writer }
    }

    /// Connect by awaiting the engine's dial-in (it must already be launched).
    #[allow(dead_code)] // alternate constructor for the attach-to-running-engine path
    pub fn connect(port: u16, token: Option<&str>) -> Self {
        let (r, w) = crate::ui::await_engine(port, token);
        Self::new(r, w)
    }

    /// Send a wire line (which may itself be multi-line for `seq`) and drain the
    /// engine's reply until it goes quiet (or a hard cap elapses).
    fn send_wire(&mut self, wire: &str) -> Result<String> {
        if !wire.is_empty() {
            let line = if wire.ends_with('\n') { wire.to_string() } else { format!("{wire}\n") };
            self.writer.write_all(line.as_bytes())?;
            self.writer.flush()?;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut out = String::new();
        loop {
            let mut buf = String::new();
            match self.reader.read_line(&mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    let t = buf.trim();
                    if !t.is_empty() && t != "READY" {
                        if let Some(g) = interpreter::gloss(t) { out.push_str(&g); }
                        else { out.push_str(t); }
                        out.push('\n');
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {
                    if !out.is_empty() { break; } // quiet after some output
                    if Instant::now() >= deadline { break; }
                }
                Err(_) => break,
            }
            if Instant::now() >= deadline { break; }
        }
        Ok(out.trim_end().to_string())
    }

    fn run(&mut self, cmd: &Command) -> Result<String> {
        match interpreter::command_to_wire(cmd) {
            interpreter::Translated::Wire(w) => self.send_wire(&w),
            interpreter::Translated::Client(c) => Ok(c.trim_end().to_string()),
        }
    }
}

impl DebugTarget for FraymakersTarget {
    /// Open a recording window: drop whatever the engine has pushed so far.
    /// Symmetric with `Ssf2Target::record` — both engines push per-frame telemetry and the
    /// host decides what counts as a window.
    fn record(&mut self, on: bool) -> Result<String> {
        if on { fm_frames_clear(); }
        Ok(fmt_record(on))
    }

    /// Close the window: run-length-encode every frame the engine pushed since `record`.
    /// Identical output shape to the SSF2 side, via the shared `rle_frames`, so a timing
    /// comparison is reading the same measurement from both engines.
    fn frame_trace(&mut self) -> Result<String> {
        let frames = fm_frames_take();
        let runs = rle_frames(&frames);
        Ok(fmt_trace(frames.len(), &runs, &[]))
    }

    fn engine(&self) -> &'static str { "fraymakers" }
    fn eval(&mut self, expr: &str) -> Result<String> { self.run(&Command::Eval(expr.to_string())) }
    fn spawn(&mut self, a: &SpawnArgs) -> Result<String> { self.run(&Command::Spawn(a.clone())) }
    fn hold(&mut self, mask: u32) -> Result<String> { self.run(&Command::Hold(mask)) }
    fn seq(&mut self, masks: &[u32]) -> Result<String> { self.run(&Command::Seq(masks.to_vec())) }
    fn console(&mut self) -> Result<String> { self.run(&Command::Console) }
    fn add_character(&mut self) -> Result<String> { self.run(&Command::AddCharacter) }
    fn exit(&mut self) -> Result<()> { let _ = self.run(&Command::Exit)?; Ok(()) }
    fn load(&mut self) -> Result<String> { self.run(&Command::Load) }
    // Fraymakers DOES walk its live object tree (the `w` verb). Without this it inherited
    // the trait default, which declares the feature SSF2-only — so `tree` answered
    // "not supported" through the session/GUI path (run_command -> target.tree) while the
    // same command worked through the serve path (command_to_wire -> "w"). Two drivers,
    // opposite answers, same command.
    fn tree(&mut self, depth: u32) -> Result<String> { self.run(&Command::Tree(depth)) }
}
