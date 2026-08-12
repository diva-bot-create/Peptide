//! Peptide command interpreter — the host-side front end that takes our
//! human-readable commands and translates them into the wire protocol read by
//! the **bytecode command interpreter we patched into the game** (the dispatch
//! chain injected by `connect_edit` in `main.rs`, which runs inside the engine's
//! `update()` loop).
//!
//! Design split (see TESTING.md "Engine RE map"):
//!   - The ENGINE-SIDE interpreter speaks a terse single-byte wire protocol: one
//!     byte selects a handler in the patched-in bytecode (`s` = spawn/launch,
//!     `x` = exit, `l` = load, `c` = console, `e` = run an hscript string). Only
//!     `s` and `e` read a trailing argument line. That dispatch is hand-written
//!     HashLink bytecode and is deliberately kept minimal.
//!   - HUMANS (and the GUI) type full lines. THIS module is where the two meet:
//!     `translate()` turns what you type into the wire line the patched engine
//!     understands. A recognized command maps to its wire byte ("spawn sandbag"
//!     -> "s sandbag", "exit" -> "x"); ANYTHING ELSE is treated as an hscript
//!     expression and forwarded to the engine's `e` handler ("match.getCharacters()
//!     [0].toState(CState.JAB)" -> "e match.getCharacters()[0].toState(CState.JAB)").
//!     So this file is the vocabulary/routing layer ONLY — it never runs game
//!     logic itself; the engine's bytecode interpreter (and the hscript it then
//!     runs, see commands.hsx) does that on the other side of the socket.
//!
//! Both `peptide` (the patcher, which generates the move-dispatch jump table from
//! `MOVES` for the still-present `m` bytecode handler) and `peptide-bridge` (the
//! client that calls `translate()`) include this file, so the host-side command
//! surface and the patched-in protocol can never drift apart.

/// One human-facing command: the friendly name, its aliases, the single wire
/// byte the engine dispatches on, a one-line argument summary, and a help blurb.
pub struct Cmd {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// The wire byte the engine matches on. `'\0'` means handled entirely
    /// client-side (e.g. `help`) and nothing is sent.
    #[allow(dead_code)] // documents the wire protocol; read on the engine side
    pub wire: char,
    pub args: &'static str,
    pub help: &'static str,
}

/// The friendly command set. Deliberately TINY: the real interface is `eval`
/// (`e`) — every non-command line is run as hscript through the engine's own
/// interpreter, which already exposes the full Fraymakers script API (CState,
/// HitboxStats, Assist, MatchModifier, Announcer, …) plus live character access
/// via `match.getCharacters()`. So instead of a per-feature command, you write
/// the hscript directly:
///     match.getCharacters()[0].getStateName()
///     match.getCharacters()[0].toState(CState.SPECIAL_NEUTRAL)
///     match.getCharacters()[0].damage._damage
/// The earlier round of auto-player-0 sugar (state/move/physics/anim/…) was
/// dropped in favour of this explicit, no-hidden-target model.
pub const COMMANDS: &[Cmd] = &[
    Cmd { name: "help",    aliases: &["h", "?"],            wire: '\0',
          args: "",                              help: "list these commands + the hscript model (client-side; sends nothing)" },
    Cmd { name: "startMatch", aliases: &["spawn", "start", "launch", "s"], wire: 's',
          args: "<char[,…]> [stage] [assist] [--versus]", help: "start a 1-4 player match (loads custom content if needed); comma-separate up to 4 characters (3-4 auto-engage versus mode; --versus forces versus for 1-2); stage/assist default to thespire/commandervideoassist" },
    Cmd { name: "eval",    aliases: &["e"],                  wire: 'e',
          args: "<hscript>",                     help: "run an hscript expression in the engine and print E:<result>. This is also the default for any unrecognized line." },
    Cmd { name: "load",    aliases: &["l"],                 wire: 'l',
          args: "",                              help: "synchronous custom-.fra load probe (diagnostic; spawn does this itself)" },
    Cmd { name: "console", aliases: &["c"],                 wire: 'c',
          args: "",                              help: "run the engine's debug console `help` command (RAN) — a side-effecting call hscript can't make, so it stays a wire byte" },
    Cmd { name: "tree",    aliases: &["dump"],              wire: '\0',
          args: "[depth]",                       help: "dump the live object tree (name/class/position/size/frame per node) — the stage-triage ground truth. SSF2 walks the display list; depth defaults to 6" },
    Cmd { name: "await",   aliases: &["settle", "playout"],  wire: '\0',
          args: "[pN] [budgetFrames]",           help: "let the current animation PLAY OUT, watching the engine's own frame counter (never a wall-clock delay): returns when it completes / loops / changes / stalls. reports the outcome, frames actually seen, and end position. budget is a ceiling (a looping animation never finishes), default 240" },
    Cmd { name: "record",  aliases: &["rec"],                   wire: '\0',
          args: "[off]",                         help: "arm the engine's OWN per-frame recorder and clear its buffer (`record off` disarms). on SSF2 this is an injected ENTER_FRAME handler — the only way to observe it accurately, since polling samples slower than the animations run" },
    Cmd { name: "trace",   aliases: &["frames"],                wire: '\0',
          args: "",                              help: "read back the recorded frame history (`<label>|<frame>|<x>|<y>;` per frame) in ONE round trip and stop recording" },
    Cmd { name: "awaitmatch", aliases: &["ready", "awaitspawn"], wire: '\0',
          args: "[pN] [tries]",                  help: "block until a character is actually READABLE (the match is up, not just spawn-acknowledged) — the deterministic replacement for sleeping after spawn. default 60 tries" },
    Cmd { name: "addCharacter", aliases: &["addchar", "add"], wire: 'n',
          args: "",                              help: "drop one more fighter into the LIVE match on demand (re-spawns the last roster char via the deferred-spawn path) — no relaunch (peptide todo #3)" },
    Cmd { name: "exit",    aliases: &["quit", "stop", "x"], wire: 'x',
          args: "",                              help: "cleanly shut the engine down (hxd.System.exit — no kill-9 orphan)" },
    Cmd { name: "hold",    aliases: &["press", "keys", "input"], wire: 'i',
          args: "<control[+control…]>",          help: "hold control inputs (e.g. hold down+special) — feeds the engine's input→action mapping, not a synthetic keypress" },
    Cmd { name: "release", aliases: &["unpress"], wire: 'i',
          args: "",                              help: "release all injected controls (sends mask 0)" },
    Cmd { name: "seq",     aliases: &["play", "inputs"], wire: 'i',
          args: "<controls:frames> …",           help: "play a frame-accurate input timeline (e.g. seq down+special:2 right:12) — one input per engine frame, auto-releases at the end" },
    Cmd { name: "scenario", aliases: &["scene", "replay"], wire: 'e',
          args: "<p0 x,y[,vx,vy]> <p1 x,y[,vx,vy]> [<ctrl:frames>…]",
          help: "set up a repeatable test scenario (peptide todo #4): place both players at fixed positions (optionally with momentum vx,vy), reset them to neutral STAND, then play an input timeline on p0. re-run the exact same line to replay it." },
    Cmd { name: "tune",    aliases: &["movestat"], wire: 'e',
          args: "<player> <hitboxIndex> <stat>=<value> …",
          help: "hot-reload a move's hitbox stats into the running match (peptide todo #5): e.g. tune p0 0 damage=15 baseKnockback=50 angle=45 — calls updateHitboxStats live, no relaunch." },
    Cmd { name: "dmg",     aliases: &["damage", "percent"], wire: 'e',
          args: "<player> <value>",
          help: "set a player's damage percent (e.g. dmg p1 80) — handy for knockback / KO-threshold testing" },
    Cmd { name: "info",    aliases: &["status", "players"], wire: 'e',
          args: "",
          help: "one-shot readout of both players: [p0 x, state, dmg, team, p1 x, state, dmg, team]" },
    Cmd { name: "reset",   aliases: &["neutral"], wire: 'e',
          args: "",
          help: "reset both players to neutral STAND with zero momentum (clean slate between tests)" },
    Cmd { name: "kill",    aliases: &["ko", "blast"], wire: 'e',
          args: "<player>",
          help: "fling a player into the bottom blast zone to force a KO (e.g. kill p1) — for respawn / stock testing" },
];

/// Control name → button bit (matches pxf.input.ControlsObject's bitmask, field
/// `buttons`). The `i` command sends the OR of the named bits as a decimal mask;
/// the engine ORs it into m_heldControls every frame and derives the pressed edge.
/// Directional + the core action buttons are bits 0–7; the rest follow the engine's
/// own ControlsObject layout. `shield` aliases SHIELD1.
pub const CONTROLS: &[(&str, u32)] = &[
    ("up", 0x1), ("down", 0x2), ("left", 0x4), ("right", 0x8),
    ("attack", 0x10), ("special", 0x20), ("action", 0x40), ("jump", 0x80),
    ("shield", 0x100), ("shield1", 0x100), ("shield2", 0x200),
    ("grab", 0x400), ("emote", 0x800), ("taunt", 0x800), ("pause", 0x1000),
    ("dash", 0x20000),
];

/// Expand a `seq` token list into the per-frame control masks (engine-agnostic).
///
/// Each token is a step `<controls>:<frames>` (or `<controls>*<frames>`; bare `<controls>`
/// = 1 frame). `controls` is a `+`-joined control list (`down+special`), a raw mask
/// (`34`/`0x22`), or one of `release`/`none`/`neutral`/empty for the 0 mask (held neutral).
/// Steps are expanded to `frames` copies each and concatenated; a final `0` mask is appended
/// so the character returns to neutral when the sequence ends. Total frames are capped.
///
/// The engine's command dispatcher processes exactly ONE command line per frame, so the
/// Fraymakers encoder emits one `i <mask>` wire line per element (a burst of N plays out
/// as N consecutive input frames — frame-accurate, paced by the engine, not host timing);
/// the SSF2 backend plays the same masks one engine frame at a time.
pub fn expand_sequence_masks(tokens: &[&str]) -> Result<Vec<u32>, String> {
    const MAX_FRAMES: usize = 4000;
    let mut masks: Vec<u32> = Vec::new();
    for step in tokens {
        let (ctrl, frames) = match step.split_once(':').or_else(|| step.split_once('*')) {
            Some((c, n)) => {
                let f: usize = n.trim().parse()
                    .map_err(|_| format!("bad frame count in {step:?} (want <controls>:<frames>)"))?;
                (c.trim(), f)
            }
            None => (step.trim(), 1),
        };
        if frames == 0 { return Err(format!("frame count must be >= 1 in {step:?}")); }
        let mask = if ctrl.is_empty()
            || matches!(ctrl.to_ascii_lowercase().as_str(), "release" | "none" | "neutral") {
            0
        } else {
            controls_mask(&[ctrl])?
        };
        if masks.len() + frames > MAX_FRAMES {
            return Err(format!("sequence too long (> {MAX_FRAMES} frames)"));
        }
        for _ in 0..frames { masks.push(mask); }
    }
    if masks.is_empty() { return Err("empty sequence".into()); }
    masks.push(0); // auto-release: return to neutral at the end
    Ok(masks)
}

/// Parse a `hold`/`press` argument list into a button bitmask. Tokens are the
/// control names in [`CONTROLS`], joined by spaces and/or `+` (so `down+special`,
/// `down special`, and `0x22`/`34` all work). A bare integer (decimal or `0x`-hex)
/// passes through as a raw mask. Returns an error string naming the bad token.
pub fn controls_mask(tokens: &[&str]) -> Result<u32, String> {
    let mut mask = 0u32;
    for raw in tokens {
        for tok in raw.split('+').filter(|t| !t.is_empty()) {
            let t = tok.to_ascii_lowercase();
            if let Some((_, bit)) = CONTROLS.iter().find(|(n, _)| *n == t) {
                mask |= bit;
            } else if let Some(hex) = t.strip_prefix("0x") {
                mask |= u32::from_str_radix(hex, 16).map_err(|_| format!("bad control or mask: {tok:?}"))?;
            } else if let Ok(n) = t.parse::<u32>() {
                mask |= n;
            } else {
                let names: Vec<&str> = CONTROLS.iter().map(|(n, _)| *n).collect();
                return Err(format!("unknown control {tok:?} (known: {})", names.join(", ")));
            }
        }
    }
    Ok(mask)
}

/// Move name → CState field NAME, in the order the engine's generated jump table
/// expects. The client sends the table INDEX (the ordinal) as the `m` argument;
/// the engine resolves each CState field by NAME at patch time (robust to findex
/// drift) and emits one comparison arm per entry, in this exact order. Keep the
/// two in lockstep by sharing this one table.
///
/// Friendly names mirror the Fraymakers animation vocabulary so a modder can
/// guess them (`tilt_down`, `aerial_forward`, `special_neutral`, …).
pub const MOVES: &[(&str, &str)] = &[
    ("jab",             "JAB"),
    ("dash_attack",     "DASH_ATTACK"),
    ("tilt_forward",    "TILT_FORWARD"),
    ("tilt_up",         "TILT_UP"),
    ("tilt_down",       "TILT_DOWN"),
    // Strongs (smashes) are 3-phase in CState (_IN -> _CHARGE -> _ATTACK); _IN is the
    // entry point that drives the whole move, so the friendly name maps there.
    ("strong_forward",  "STRONG_FORWARD_IN"),
    ("strong_up",       "STRONG_UP_IN"),
    ("strong_down",     "STRONG_DOWN_IN"),
    ("aerial_neutral",  "AERIAL_NEUTRAL"),
    ("aerial_forward",  "AERIAL_FORWARD"),
    ("aerial_back",     "AERIAL_BACK"),
    ("aerial_up",       "AERIAL_UP"),
    ("aerial_down",     "AERIAL_DOWN"),
    ("special_neutral", "SPECIAL_NEUTRAL"),
    ("special_side",    "SPECIAL_SIDE"),
    ("special_up",      "SPECIAL_UP"),
    ("special_down",    "SPECIAL_DOWN"),
    ("grab",            "GRAB"),
    ("stand",           "STAND"),
    ("fall",            "FALL"),
];

/// Find the command whose name or alias matches `tok` (case-insensitive).
pub fn lookup(tok: &str) -> Option<&'static Cmd> {
    // Case-insensitive on both sides so a camelCase canonical name (e.g. `startMatch`)
    // still matches what the user types, and the displayed name keeps its casing.
    COMMANDS.iter().find(|c| {
        c.name.eq_ignore_ascii_case(tok) || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(tok))
    })
}

/// Outcome of translating one friendly line.
pub enum Translated {
    /// Send this exact line to the engine.
    Wire(String),
    /// Handled client-side; print this text, send nothing.
    Client(String),
}

/// A spawn request: one or more players (player 0, 1, …), a stage, and an optional
/// assist for player 0. Multiple characters are given comma-separated
/// (`spawn mario,sonic battlefield`); each becomes a player. Assists are a
/// Fraymakers concept — on SSF2 the assist field is a no-op.
#[derive(Debug, Clone)]
pub struct SpawnArgs {
    pub characters: Vec<String>,
    pub stage: Option<String>,
    pub assist: Option<String>,
    /// `--versus`: force versus mode even for a 1-2 player roster (3-4 players engage versus
    /// automatically regardless). No-op on SSF2.
    pub force_versus: bool,
}

impl SpawnArgs {
    /// Player 0's character (the one being debugged), or "" if none.
    pub fn character(&self) -> &str {
        self.characters.first().map(String::as_str).unwrap_or("")
    }
}

// ── shared match settings ────────────────────────────────────────────────────
// The headless debug-match RULES, the SINGLE source of truth read by BOTH engines
// so a `spawn` produces the same match on Fraymakers and SSF2. The *data* lives in
// `match_settings.conf` (the source of truth); this is just the parser/loader,
// living at the interpreter (highest-abstraction) level so every backend reads the
// one file through the one path. Fraymakers bakes lives+time into its start-match
// bytecode; SSF2 applies all three to the versus `Game`.

/// Parsed `match_settings.conf` — the full match-rule set, with universal defaults
/// (a never-ending 1-on-stage debug match). `damage_ratio` is the multiplier on
/// INCOMING damage when a hit lands (not a starting/current damage value). Each
/// backend maps these onto its engine: SSF2 → GameSettings/PlayerSetting fields,
/// Fraymakers → the start-match config.
#[derive(Debug, Clone, Copy)]
pub struct MatchSettings {
    pub lives: i32,          // stock count (999 ≈ infinite)
    pub time: i32,           // timer seconds (0 = no timer)
    pub damage_ratio: f64,   // incoming-damage multiplier (1.0 = normal)
    pub team_damage: bool,   // friendly fire
    pub start_damage: i32,   // starting damage %
    pub using_stamina: bool, // stamina (HP) mode instead of stocks
    pub start_stamina: i32,  // starting stamina/HP (when using_stamina)
    pub size_ratio: f64,     // character size multiplier
    pub item_frequency: i32, // item spawn frequency (0 = items off)
}

impl Default for MatchSettings {
    fn default() -> Self {
        MatchSettings {
            lives: 999,
            time: 0,
            damage_ratio: 1.0,
            team_damage: false,
            start_damage: 0,
            using_stamina: false,
            start_stamina: 150,
            size_ratio: 1.0,
            item_frequency: 0,
        }
    }
}

/// Parse the `key = value` config (`#` comment; unknown keys ignored; a missing key
/// keeps its [`Default`] — the last-resort fallback if the file is absent). Bools
/// accept true/false/1/0/on/off.
pub fn parse_match_settings(text: &str) -> MatchSettings {
    fn b(v: &str) -> Option<bool> {
        match v.to_ascii_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => Some(true),
            "false" | "0" | "off" | "no" => Some(false),
            _ => None,
        }
    }
    let mut s = MatchSettings::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() { continue; }
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            match k.trim() {
                "lives" => if let Ok(n) = v.parse() { s.lives = n; },
                "time" => if let Ok(n) = v.parse() { s.time = n; },
                "damage_ratio" => if let Ok(n) = v.parse() { s.damage_ratio = n; },
                "team_damage" => if let Some(x) = b(v) { s.team_damage = x; },
                "start_damage" => if let Ok(n) = v.parse() { s.start_damage = n; },
                "using_stamina" => if let Some(x) = b(v) { s.using_stamina = x; },
                "start_stamina" => if let Ok(n) = v.parse() { s.start_stamina = n; },
                "size_ratio" => if let Ok(n) = v.parse() { s.size_ratio = n; },
                "item_frequency" => if let Ok(n) = v.parse() { s.item_frequency = n; },
                _ => {}
            }
        }
    }
    s
}

/// Load the shared match rules from `match_settings.conf` (the source of truth).
pub fn load_match_settings() -> MatchSettings {
    parse_match_settings(&crate::read_asset("match_settings.conf"))
}

/// The ENGINE-AGNOSTIC command, produced by [`parse`]. Both the Fraymakers and
/// SSF2 debug backends ([`crate::debug_target::DebugTarget`]) execute these, so
/// one command vocabulary drives both engines with the same syntax. The
/// Fraymakers wire encoding lives in [`translate`] (derived from this).
#[derive(Debug, Clone)]
pub enum Command {
    /// Client-side help text.
    Help,
    /// Boot a match with the given character (+ optional stage/assist).
    Spawn(SpawnArgs),
    /// Evaluate an expression (hscript on Fraymakers; reflection on SSF2).
    Eval(String),
    /// Hold the given control bitmask (release = `Hold(0)`).
    Hold(u32),
    /// Play a frame-accurate input timeline — one mask per engine frame
    /// (already auto-released with a trailing `0`).
    Seq(Vec<u32>),
    /// A repeatable test scenario: a setup hscript line (positions/momentum/neutral
    /// state for both players) plus an optional input timeline on p0. Host-side macro
    /// that composes eval + seq — see peptide todo #4.
    Scenario { setup: String, masks: Vec<u32> },
    /// Run the engine's debug console.
    Console,
    /// Dump the live object tree to the given depth — the stage-triage ground truth
    /// (every live game object with its name/class/position/size/frame).
    Tree(u32),
    /// Watch a character's animation until it finishes, loops, changes, or stalls,
    /// polling the ENGINE'S OWN frame counter. The frame-accurate replacement for
    /// "sleep and hope": a wall-clock wait either observes a half-played animation or
    /// spends most of its time idle, and which one it does changes with engine load.
    /// `budget` bounds the watch (a looping animation never finishes) — a ceiling,
    /// not a pace.
    Await { idx: usize, budget: u32 },
    /// Block until a character is readable — the match is genuinely up, not just
    /// `spawn`-acknowledged. Replaces "sleep a few seconds after spawn", which loses
    /// the first observation when the engine is slow and wastes the wait when it isn't.
    AwaitMatch { idx: usize, tries: u32 },
    /// Arm (or disarm) the engine's own per-frame recorder, clearing its buffer.
    Record { on: bool },
    /// Read back the recorded frame history and stop recording.
    ///
    /// The two engines answer this from opposite directions, which is the whole reason
    /// it's a trait method rather than an eval: SSF2 has no script interpreter, so an
    /// injected ENTER_FRAME handler accumulates `<label>|<frame>|<x>|<y>;` in memory and
    /// this reads the lot in ONE round trip. Fraymakers already PUSHES per-frame `ANIM:`
    /// telemetry over its socket and its eval is fast enough to poll, so it says so
    /// rather than pretending to have a buffer.
    Trace,
    /// Drop one more fighter into the live match (peptide todo #3).
    AddCharacter,
    /// Cleanly shut the engine down.
    Exit,
    /// Diagnostic content (re)load.
    Load,
    /// Nothing to send; print this client-side message (usage / error / empty).
    Client(String),
}

/// Parse a human command line into an engine-agnostic [`Command`]. This is the
/// single front-end shared by every backend — unknown heads and bare expressions
/// become `Eval`, exactly like the Fraymakers default.
pub fn parse(line: &str) -> Command {
    let line = line.trim();
    if line.is_empty() {
        return Command::Client(String::new());
    }
    let mut parts = line.split_whitespace();
    let head = parts.next().unwrap_or("");
    let rest: Vec<&str> = parts.collect();

    if let Some(cmd) = lookup(head) {
        match cmd.name {
            "help" => return Command::Help,
            "startMatch" => {
                // `--versus` may appear anywhere; pull it out, then parse positionally. The first
                // remaining token is the player list: comma-separated characters (up to 4 — Fraymakers
                // has 4 ports). stage/assist are the next two positional tokens.
                let force_versus = rest.contains(&"--versus");
                let pos: Vec<&str> = rest.iter().copied().filter(|t| *t != "--versus").collect();
                let characters: Vec<String> = pos.first().copied().unwrap_or("")
                    .split(',').map(str::trim).filter(|c| !c.is_empty()).map(str::to_string).collect();
                if characters.len() > FM_MAX_PLAYERS {
                    return Command::Client(format!(
                        "spawn: {} players requested but Fraymakers supports at most {FM_MAX_PLAYERS}. \
                         Comma-separate up to {FM_MAX_PLAYERS} characters (e.g. spawn a,b,c,d stage assist).\n",
                        characters.len()));
                }
                return Command::Spawn(SpawnArgs {
                    characters,
                    stage: pos.get(1).map(|s| s.to_string()),
                    assist: pos.get(2).map(|s| s.to_string()),
                    force_versus,
                });
            }
            "eval" => return Command::Eval(rest.join(" ")),
            "hold" => {
                if rest.is_empty() {
                    return Command::Client(
                        "usage: hold <control[+control…]>  (e.g. hold down+special). release = clear.\n".into());
                }
                return match controls_mask(&rest) {
                    Ok(mask) => Command::Hold(mask),
                    Err(e) => Command::Client(format!("{e}\n")),
                };
            }
            "release" => return Command::Hold(0),
            "seq" => {
                if rest.is_empty() {
                    return Command::Client(
                        "usage: seq <controls:frames> …  (e.g. seq down+special:2 right:12). One input per engine frame; auto-releases at the end.\n".into());
                }
                return match expand_sequence_masks(&rest) {
                    Ok(masks) => Command::Seq(masks),
                    Err(e) => Command::Client(format!("{e}\n")),
                };
            }
            "scenario" => return parse_scenario(&rest),
            "tune" => return parse_tune(&rest),
            "dmg" => {
                if rest.len() != 2 {
                    return Command::Client("usage: dmg <player> <value>  (e.g. dmg p1 80)\n".into());
                }
                let (player, val) = (rest[0], rest[1]);
                if !player.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                    return Command::Client(format!("dmg: {player:?} is not a valid player reference\n"));
                }
                if val.parse::<f64>().is_err() {
                    return Command::Client(format!("dmg: value must be numeric (got {val:?})\n"));
                }
                return Command::Eval(format!("{player}.damage._damage = {val}"));
            }
            "info" => {
                return Command::Eval(
                    "[p0.getX(), p0.getStateName(), p0.damage._damage, p0.getTeam(), \
                     p1.getX(), p1.getStateName(), p1.damage._damage, p1.getTeam()]".into());
            }
            "reset" => {
                return Command::Eval(
                    "p0.setXSpeed(0); p0.setYSpeed(0); p1.setXSpeed(0); p1.setYSpeed(0); \
                     p0.toState(CState.STAND); p1.toState(CState.STAND)".into());
            }
            "kill" => {
                let player = match rest.first() {
                    Some(p) if p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => *p,
                    Some(p) => return Command::Client(format!("kill: {p:?} is not a valid player reference\n")),
                    None => return Command::Client("usage: kill <player>  (e.g. kill p1)\n".into()),
                };
                return Command::Eval(format!("{player}.setY({player}.getY() + 3000)"));
            }
            "console" => return Command::Console,
            "tree" => {
                let depth = rest.first().and_then(|d| d.parse().ok()).unwrap_or(6);
                return Command::Tree(depth);
            }
            "await" | "settle" | "playout" => {
                // await [pN] [budgetFrames] — both optional, either order-independent
                // enough that a bare `await` is the common case.
                let mut idx = 0usize;
                let mut budget = 240u32;
                for a in &rest {
                    if let Some(n) = a.strip_prefix('p').and_then(|n| n.parse::<usize>().ok()) {
                        idx = n;
                    } else if let Ok(n) = a.parse::<u32>() {
                        budget = n;
                    } else {
                        return Command::Client(format!(
                            "await: don't understand {a:?}\nusage: await [pN] [budgetFrames]\n"));
                    }
                }
                return Command::Await { idx, budget };
            }
            // Frame trace: `record` opens a window (clears the engine's buffer and arms
            // it), `trace` closes it and reads the whole thing back in one round trip.
            // This is how SSF2 is observed accurately — see Command::Trace.
            "record" | "rec" => {
                let on = !matches!(rest.first().map(|s| s.to_ascii_lowercase()).as_deref(), Some("off"));
                return Command::Record { on };
            }
            "trace" | "frames" => return Command::Trace,
            "awaitmatch" | "ready" | "awaitspawn" => {
                let mut idx = 0usize;
                let mut tries = 60u32;
                for a in &rest {
                    if let Some(n) = a.strip_prefix('p').and_then(|n| n.parse::<usize>().ok()) {
                        idx = n;
                    } else if let Ok(n) = a.parse::<u32>() {
                        tries = n;
                    }
                }
                return Command::AwaitMatch { idx, tries };
            }
            "addCharacter" => return Command::AddCharacter,
            "exit" => return Command::Exit,
            "load" => return Command::Load,
            _ => {}
        }
    }
    // Unknown head OR a bare expression → eval the whole (collapsed) line.
    Command::Eval(collapse_multiline(line))
}

/// Parse a `scenario <p0 x,y[,vx,vy]> <p1 x,y[,vx,vy]> [<ctrl:frames>…]` line into a
/// pre-expanded [`Command::Scenario`]. Sets both players to fixed positions (and momentum,
/// if 4 values), resets them to neutral STAND, then plays the input timeline on p0. The
/// whole thing is deterministic + re-runnable — that's the point (peptide todo #4).
fn parse_scenario(rest: &[&str]) -> Command {
    const USAGE: &str = "usage: scenario <p0 x,y[,vx,vy]> <p1 x,y[,vx,vy]> [<ctrl:frames>…]\n  e.g. scenario 300,400 360,400 attack:2   (place both, reset to STAND, jab on p0; re-run to replay)\n";
    if rest.len() < 2 {
        return Command::Client(USAGE.into());
    }
    // "x,y" or "x,y,vx,vy" -> the per-player setup hscript (setX/setY [+ setXSpeed/setYSpeed]).
    // setXSpeed/setYSpeed, NOT setXVelocity/setYVelocity: the latter do not exist on a
    // Fraymakers character and fail with "Invalid function null". SSF2 lowers both
    // spellings to its XSpeed/YSpeed property (vocab::SETTERS), so this spelling is the one
    // that works on BOTH engines.
    fn body(slot: &str, who: &str) -> Result<String, String> {
        let n: Vec<&str> = slot.split(',').map(str::trim).collect();
        if n.len() != 2 && n.len() != 4 {
            return Err(format!("scenario: {who} must be x,y or x,y,vx,vy (got {slot:?})"));
        }
        for v in &n {
            v.parse::<f64>().map_err(|_| format!("scenario: {who} has a non-numeric value {v:?}"))?;
        }
        let mut s = format!("{who}.setX({}); {who}.setY({})", n[0], n[1]);
        if n.len() == 4 {
            s.push_str(&format!("; {who}.setXSpeed({}); {who}.setYSpeed({})", n[2], n[3]));
        }
        Ok(s)
    }
    let p0 = match body(rest[0], "p0") { Ok(s) => s, Err(e) => return Command::Client(format!("{e}\n")) };
    let p1 = match body(rest[1], "p1") { Ok(s) => s, Err(e) => return Command::Client(format!("{e}\n")) };
    // setup: position/momentum both players, then reset to neutral STAND.
    let setup = format!("{p0}; {p1}; p0.toState(CState.STAND); p1.toState(CState.STAND)");
    // optional input timeline on p0 (same expansion as `seq`).
    let masks = if rest.len() > 2 {
        match expand_sequence_masks(&rest[2..]) {
            Ok(m) => m,
            Err(e) => return Command::Client(format!("{e}\n")),
        }
    } else {
        Vec::new()
    };
    Command::Scenario { setup, masks }
}

/// Parse `tune <player> <hitboxIndex> <stat>=<value> …` into an `updateHitboxStats`
/// eval — live move-stat hot-reload (peptide todo #5). Numeric values pass through bare;
/// non-numeric values (e.g. an AttackLimb enum) pass through verbatim, so
/// `tune p0 0 limb=AttackLimb.FOOT` works too.
fn parse_tune(rest: &[&str]) -> Command {
    const USAGE: &str = "usage: tune <player> <hitbox> <stat>=<value> …\n  Fraymakers: <hitbox> = a hitbox index (e.g. tune p0 0 damage=15 baseKnockback=50 angle=45).\n  SSF2: <hitbox> = a move name (e.g. tune p0 AttackS3 damage=15 angle=45); mutates that move's stored box 0.\n  live updateHitboxStats; no relaunch.\n";
    if rest.len() < 3 {
        return Command::Client(USAGE.into());
    }
    let player = rest[0];
    // very light validation: player looks like an identifier (p0/p1/self/a char ref).
    if !player.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Command::Client(format!("tune: {player:?} is not a valid player reference\n"));
    }
    // hitbox selector: a non-negative integer (Fraymakers hitbox index) OR an identifier
    // (SSF2 move name). Passed verbatim into updateHitboxStats; each engine interprets it.
    let idx = rest[1];
    let is_index = idx.parse::<i64>().map(|n| n >= 0).unwrap_or(false);
    if !is_index && !idx.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Command::Client(format!("tune: hitbox selector must be a non-negative integer or a move name (got {idx:?})\n"));
    }
    let mut fields = Vec::new();
    for kv in &rest[2..] {
        let (k, v) = match kv.split_once('=') {
            Some((k, v)) if !k.is_empty() && !v.is_empty() => (k, v),
            _ => return Command::Client(format!("tune: each stat must be key=value (got {kv:?})\n")),
        };
        if !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Command::Client(format!("tune: {k:?} is not a valid stat name\n"));
        }
        fields.push(format!("{k}: {v}"));
    }
    Command::Eval(format!("{player}.updateHitboxStats({idx}, {{ {} }})", fields.join(", ")))
}

/// Encode an engine-agnostic [`Command`] into the Fraymakers wire line. This is
/// the FraymakersTarget's view; SSF2's backend executes the `Command` differently.
/// Fraymakers' baked default stage/assist ids (match the patcher's `connect_edit`
/// `unwrap_or` defaults in main.rs). Sent to fill the stage+assist wire slots when a
/// multi-player `s` omits them, so the extra players stay at parts[4+].
const FM_DEFAULT_STAGE: &str = "thespire";
const FM_DEFAULT_ASSIST: &str = "commandervideoassist";
/// Max players a Fraymakers match supports (4 ports). `spawn` rejects a larger roster.
pub const FM_MAX_PLAYERS: usize = 4;

pub fn command_to_wire(cmd: &Command) -> Translated {
    match cmd {
        Command::Help => Translated::Client(help_text()),
        Command::Client(s) => Translated::Client(s.clone()),
        Command::Spawn(a) => {
            // wire: `s <char0> <stage> <assist> [char1 char2 …]`. This wire path drives
            // Fraymakers. The engine `s` handler splits on spaces (NOT commas) and reads
            // parts[1]=char0, parts[2]=stage, parts[3]=assist, parts[4..]=extra players.
            // So for >1 player we MUST emit the stage+assist slots (defaulting to the same
            // ids the patcher bakes) to keep the extra players at parts[4+]. SSF2 multiplayer
            // is handled separately by `Ssf2Target::spawn` (idle dummies) and never uses this.
            // `--versus` swaps the lead byte to `S` (force versus mode); the engine dispatch
            // treats `S` and `s` identically except for the mode it selects.
            let mut wire = String::from(if a.force_versus { "S" } else { "s" });
            if !a.characters.is_empty() { wire.push(' '); wire.push_str(a.character()); }
            if a.characters.len() > 1 {
                wire.push(' '); wire.push_str(a.stage.as_deref().unwrap_or(FM_DEFAULT_STAGE));
                wire.push(' '); wire.push_str(a.assist.as_deref().unwrap_or(FM_DEFAULT_ASSIST));
                for c in &a.characters[1..] { wire.push(' '); wire.push_str(c); }
            } else {
                if let Some(st) = &a.stage { wire.push(' '); wire.push_str(st); }
                if let Some(asst) = &a.assist { wire.push(' '); wire.push_str(asst); }
            }
            Translated::Wire(wire)
        }
        Command::Eval(e) => Translated::Wire(if e.is_empty() { "e".into() } else { format!("e {e}") }),
        // `await` is a HOST-side polling loop over the engine's frame counter, not a wire
        // verb — there is nothing to send. It's executed by `run_command`, which owns the
        // loop; reaching here means a caller translated it to wire by mistake.
        Command::Await { .. } | Command::AwaitMatch { .. }
        | Command::Record { .. } | Command::Trace => Translated::Client(
            "await/awaitmatch/record/trace are executed host-side (run_command), not sent to the engine\n".into()),
        Command::Hold(m) => Translated::Wire(format!("i {m}")),
        Command::Seq(masks) => Translated::Wire(masks.iter().map(|m| format!("i {m}")).collect::<Vec<_>>().join("\n")),
        Command::Scenario { setup, masks } => {
            let mut w = format!("e {setup}");
            for m in masks { w.push('\n'); w.push_str(&format!("i {m}")); }
            Translated::Wire(w)
        }
        Command::Console => Translated::Wire("c".into()),
        // FM tree walk: the 'w' wire handler walks currentMatch.gameObjects in bytecode (hl ArrayObj
        // doesn't reflect length/index in hscript, so the walk can't be eval-side).
        Command::Tree(_) => Translated::Wire("w".into()),
        Command::AddCharacter => Translated::Wire("n".into()),
        Command::Exit => Translated::Wire("x".into()),
        Command::Load => Translated::Wire("l".into()),
    }
}

/// Translate a friendly command line into the engine wire line.
///
/// - `help` / `?` → prints the help text, sends nothing.
/// - a known friendly name (or its alias, or the bare wire letter) → rewritten
///   to `<wire-byte> <args…>`; `move <name>` resolves the name to its ordinal.
/// - anything else → passed through verbatim (forward-compatible: a raw wire
///   line a human typed still reaches the engine untouched).
pub fn translate(line: &str) -> Translated {
    // Engine-agnostic parse, then encode to the Fraymakers wire. (Empty line → an
    // empty wire send, preserving prior behaviour rather than a client no-op.)
    if line.trim().is_empty() { return Translated::Wire(String::new()); }
    command_to_wire(&parse(line))
}

/// Split one chat message into independent commands at BLANK lines.
///
/// A message is one or more commands. Consecutive lines form a single command; a blank
/// line (whitespace only) ends it and starts the next. Each returned block is handed to
/// `translate` on its own, so `spawn sandbag`, a blank line, then a `match.…` expression
/// run as two separate wire commands with two separate replies.
///
/// The blank-line split is suppressed while inside a string literal, a `/* … */` block
/// comment, or unbalanced `()`/`[]`/`{}` nesting — so a genuine multi-line statement (or a
/// string/block that itself contains a blank line) is never torn apart. A bare single-line
/// (or single multi-line) message returns exactly one block, preserving prior behaviour.
pub fn split_commands(text: &str) -> Vec<String> {
    enum St { Code, Str(char), Block }
    let mut blocks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut st = St::Code;
    let mut escaped = false; // inside Str
    let mut depth: i32 = 0;  // open ()/[]/{} nesting

    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let at_top = matches!(st, St::Code) && depth <= 0;
        if line.trim().is_empty() && at_top {
            // separator: a blank line at the top level ends the current command.
            if !cur.trim().is_empty() { blocks.push(cur.trim().to_string()); }
            cur.clear();
            continue;
        }
        if !cur.is_empty() { cur.push('\n'); }
        cur.push_str(line);
        // advance the string/comment/nesting scanner across this line.
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match st {
                St::Code => match c {
                    '/' if chars.peek() == Some(&'/') => break, // line comment -> rest is inert
                    '/' if chars.peek() == Some(&'*') => { chars.next(); st = St::Block; }
                    '"' | '\'' => { escaped = false; st = St::Str(c); }
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    _ => {}
                },
                St::Str(q) => {
                    if escaped { escaped = false; }
                    else if c == '\\' { escaped = true; }
                    else if c == q { st = St::Code; }
                }
                St::Block => {
                    if c == '*' && chars.peek() == Some(&'/') { chars.next(); st = St::Code; }
                }
            }
        }
    }
    if !cur.trim().is_empty() { blocks.push(cur.trim().to_string()); }
    blocks
}

/// Fold a (possibly multi-line) hscript snippet onto a single physical line so it can
/// cross the newline-delimited engine wire — the engine's `e` drain reads bytes until a
/// '\n', so any real newline in the payload would fragment one command into several.
///
/// hscript treats newlines as ordinary whitespace (statements are separated by `;`, never
/// by line breaks), so this is semantics-preserving for the executable code:
///   - newlines OUTSIDE a string literal collapse to a single space;
///   - `//` line comments are dropped to end-of-line (they never execute anyway);
///   - `/* … */` block comments are kept (their internal newlines become spaces);
///   - a real newline INSIDE a string literal becomes the `\n` escape, preserving the
///     string's meaning while keeping the wire frame intact;
///   - string contents pass through verbatim with `\`-escapes respected, so a `\"` does
///     not end the string and a `//` inside a string is not mistaken for a comment.
fn collapse_multiline(src: &str) -> String {
    if !src.contains('\n') && !src.contains('\r') {
        return src.to_string(); // common case: already one line
    }
    enum St { Code, Str(char), Line, Block }
    let mut out = String::with_capacity(src.len());
    let mut chars = src.chars().peekable();
    let mut st = St::Code;
    let mut escaped = false; // only meaningful inside Str
    while let Some(c) = chars.next() {
        match st {
            St::Code => match c {
                '\r' => {}                                   // swallow; the '\n' is the separator
                '\n' => out.push(' '),
                '/' if chars.peek() == Some(&'/') => { chars.next(); st = St::Line; }
                '/' if chars.peek() == Some(&'*') => { chars.next(); out.push_str("/*"); st = St::Block; }
                '"' | '\'' => { out.push(c); escaped = false; st = St::Str(c); }
                _ => out.push(c),
            },
            St::Str(q) => {
                if escaped { out.push(c); escaped = false; }
                else if c == '\\' { out.push(c); escaped = true; }
                else if c == '\r' {}                          // swallow (\n handles it)
                else if c == '\n' { out.push_str("\\n"); }    // real newline -> escape, keep meaning
                else if c == q { out.push(c); st = St::Code; }
                else { out.push(c); }
            }
            St::Line => { if c == '\n' { out.push(' '); st = St::Code; } } // else: drop comment text
            St::Block => match c {
                '*' if chars.peek() == Some(&'/') => { chars.next(); out.push_str("*/"); st = St::Code; }
                '\r' => {}
                '\n' => out.push(' '),
                _ => out.push(c),
            },
        }
    }
    out
}

// ── TCP channels ─────────────────────────────────────────────────────────────────
// Some TCP lines belong to a side CHANNEL (a structured feed), not the chat. They are
// routed by the GUI to the relevant widget and SUPPRESSED everywhere a human reads the
// raw stream (the chat, the CLI). The first channel is `matchStatus` (host-polled).
pub const MATCH_STATUS_TAG: &str = "MATCHSTATUS:";
/// `ICON:<slot>:<png-hex>` — a character's stock icon ripped from the engine (the host
/// decodes the hex to a base64 data: URL for the matchStatus widget). Host-requested, not
/// polled, so it lands once per character rather than every tick.
pub const ICON_TAG: &str = "ICON:";

/// If `line` is a channel line, return `(channel_name, payload)`. The engine wraps eval
/// replies as `E:<result>`, so a polled `e matchStatus()` arrives as `E:MATCHSTATUS:<…>`;
/// we look through the `E:` prefix. Used by the GUI (to route) and the CLI (to suppress).
pub fn channel_payload(line: &str) -> Option<(&'static str, &str)> {
    let l = line.strip_prefix("E:").unwrap_or(line);
    if let Some(p) = l.strip_prefix(MATCH_STATUS_TAG) { return Some(("matchStatus", p)); }
    if let Some(p) = l.strip_prefix(ICON_TAG) { return Some(("charIcon", p)); }
    None
}

/// A friendly gloss for an engine reply line (additive — callers keep the raw
/// line and append this in parens). Returns None when there's nothing to add.
pub fn gloss(reply: &str) -> Option<String> {
    let r = reply.trim();
    // Eval-hook replies are wrapped as "E:<result>" — gloss the inner payload so
    // hscript-ported commands read as friendly as the old wire-byte ones did
    // (e.g. "E:Q:MATCH_LIVE" -> "a match is live", "E:PONG" -> "engine alive").
    let r = r.strip_prefix("E:").unwrap_or(r);
    let body = |s: &str| s.to_string();
    // ANIM:/LAUNCHED come from the engine bootstrap + telemetry; the others gloss the
    // replies of the still-present bytecode handlers (q/p/t/m) if a raw byte is sent.
    if let Some(s) = r.strip_prefix("ANIM:") { return Some(format!("animation: {}", body(s))); }
    if let Some(s) = r.strip_prefix("LAUNCHED ") { return Some(format!("match launched: {}", body(s))); }
    if let Some(s) = r.strip_prefix("T:") { return Some(format!("state: {}", body(s))); }
    if let Some(s) = r.strip_prefix("M:") { return Some(format!("move dispatched: {}", body(s))); }
    if let Some(s) = r.strip_prefix("I:") { return Some(format!("controls set: {}", body(s))); }
    match r {
        "Q:MATCH_LIVE" => Some("a match is live".into()),
        "Q:NO_MATCH"   => Some("no match running".into()),
        "PONG"         => Some("engine alive".into()),
        _ => None,
    }
}

/// Build the crash modal's "Enhanced log" from the engine's `error.log` and the `RESDIAG:`
/// breadcrumbs the patched engine emitted (the one fact `error.log` lacks — the failing
/// resource id). HOST-SIDE — no engine bytecode (see AGENT_CONTEXT.md "keep logic OUT of
/// bytecode"). Top = a plain-English translation; bottom = the abridged engine exception.
/// Returns None when there's nothing to interpret.
pub fn interpret_crash(error_log: &str, resdiag: &[String]) -> Option<String> {
    let log = error_log.trim();
    if log.is_empty() && resdiag.is_empty() { return None; }

    // The failing resource id the engine reported: "RESDIAG: … resource id: <id>".
    let id = resdiag.iter().rev()
        .find_map(|l| l.split("resource id:").nth(1))
        .map(|s| s.trim().to_string());
    // Abridged exception + the deepest meaningful app frame (skip std/haxe/hxd plumbing).
    let exc = log.lines().find(|l| l.contains("Exception:"))
        .and_then(|l| l.split_once("Exception:").map(|x| x.1)).map(str::trim);
    let frame = log.lines()
        .filter(|l| l.contains("Called from") && (l.contains("pxf.") || l.contains("fraymakers.")))
        .map(|l| l.trim_start_matches("Called from").trim())
        .next();

    let is_stage = log.contains("setupStage") || log.contains("stagePxfContentMap")
        || resdiag.iter().any(|l| l.contains("stage failed"));
    // Null assist: the HUD's DamageCounter renders each player's assist icon, so a player with a
    // null/unresolved assist null-derefs `.namespace` in getContentIdentifierString. Common when
    // an extra player (multi-player `s a,b`) didn't inherit a valid assist.
    let is_assist = log.contains("generateAssistSprite") || log.contains("DamageCounter")
        || resdiag.iter().any(|l| l.contains("assist did not resolve"));

    let mut out = String::new();
    if is_assist {
        out.push_str("Hmm… a player has no assist.\n");
        out.push_str("Fraymakers' HUD (DamageCounter.generateAssistSprite) needs every player to \
                      have a resolved assist — a null one crashes it. If you spawned multiple \
                      players (`s <char>,<char2>`), pass a valid assist as the 3rd token \
                      (`s <char> <stage> <assist> …`) so extra players inherit it.");
    } else if is_stage {
        out.push_str("Hmm… the stage didn't load.\n");
        match &id {
            Some(i) => out.push_str(&format!(
                "{i} isn't a valid/loadable stage resource. Match.setupStage asked the \
                 ResourceManager for it and it returned null.")),
            None => out.push_str(
                "The configured stage isn't a valid/loadable stage resource — Match.setupStage \
                 asked the ResourceManager for it and it returned null."),
        }
    } else if log.contains(".namespace") || log.contains("getContentIdentifierString") {
        out.push_str("Hmm… a resource didn't load.\n");
        out.push_str("Something the match needs — a character, stage, or assist — didn't \
                      resolve to a real resource.");
        if let Some(i) = &id { out.push_str(&format!("\nLast resource reported: {i}.")); }
    } else {
        out.push_str("Hmm… Fraymakers crashed unexpectedly.");
    }

    if exc.is_some() || frame.is_some() {
        out.push_str("\n\n");
        if let Some(e) = exc { out.push_str(&format!("Engine exception: {e}\n")); }
        if let Some(f) = frame { out.push_str(&format!("Crash site: {f}")); }
    }
    Some(out.trim_end().to_string())
}

/// The `help` listing.
pub fn help_text() -> String {
    let mut out = String::from("Peptide commands (friendly name [aliases] <args> — description):\n");
    for c in COMMANDS {
        let al = if c.aliases.is_empty() { String::new() } else { format!(" [{}]", c.aliases.join(", ")) };
        out.push_str(&format!("  {:<8}{:<22} {:<26} {}\n", c.name, al, c.args, c.help));
    }
    out.push_str("\nEverything else is hscript, run in the engine's own interpreter via `e`.\n");
    out.push_str("Live character access (explicit target — no hidden player 0):\n");
    out.push_str("  match.getCharacters()                       all characters in the match\n");
    out.push_str("  match.getCharacters().length                how many\n");
    out.push_str("  match.getCharacters()[0].getStateName()     a character's current state\n");
    out.push_str("  match.getCharacters()[0].toState(CState.SPECIAL_NEUTRAL)   drive a move\n");
    out.push_str("  match.getCharacters()[0].body.x             read a field\n");
    out.push_str("  match.getCharacters()[0].damage._damage     damage %\n");
    out.push_str("\nThe full Fraymakers script API is in scope (same as character/mode scripts):\n");
    out.push_str("  CState  CStateGroup  HitboxStats  StatusEffectType  EntityHitCondition\n");
    out.push_str("  Assist  AssistEvent  MatchModifier  Announcer  GameMenus  Css  GlobalVfx\n");
    out.push_str("  GlobalSfx  CameraShakeType  GraphicsSettings  DisplaySettings  Ai*Option …\n");
    out.push_str("\nExamples:\n");
    out.push_str("  startMatch sandbag            start a sandbag match (default stage/assist; alias: spawn)\n");
    out.push_str("  spawn sandbag,sandbag         2-player mirror match (comma-separated roster)\n");
    out.push_str("  spawn sandbag,mario,sandbag,mario   4-player versus match (1-4 players; p0..p3 drive each)\n");
    out.push_str("  match.getCharacters()[0].getStateName()    read a character's state\n");
    out.push_str("  CState.JAB                    inspect an API enum value\n");
    out.push_str("  exit                          shut the engine down\n");
    out.push_str("\nCLI modes (run as `peptide <mode> …`, not engine commands):\n");
    out.push_str("  (no args) | gui               open the Peptide window (default)\n");
    out.push_str("  tui                           terminal console\n");
    out.push_str("  convert <file.ssf> [opts]     SSF2 → Fraymakers conversion (peptide convert --help)\n");
    out.push_str("  export  --project <.fraytools>   drive FrayTools Publish → build the .fra\n");
    out.push_str("  render  --entity <rel> [...]     render an entity to PNG via FrayTools\n");
    out.push_str("  harness --entity <rel> [...]     extract box geometry + PNG + JSON report\n");
    out.push_str("  headless | send \"<cmd>\"        TCP bridge runtime / one-shot command\n");
    out.push_str("\nPersistent session (iterate on a char/fix; the engine keeps streaming back):\n");
    out.push_str("  session [--char C | --full]   boot + hold a live engine, read commands from a control file\n");
    out.push_str("  tell \"<command>\"              queue a command for the running session (e.g. tell \"spawn sandbag\")\n");
    out.push_str("  log [-n N] [--follow]         print the session's mirrored engine replies\n");
    out.push_str("  log --crash                   print the crash report (error.log + RESDIAG, interpreted)\n");
    out
}

/// Host-owned **UI bridge** hscript — the matchStatus stock-icon feed.
///
/// These helpers must *execute* in-engine (they reflect over live engine objects:
/// the DamageCounter, the SpriteApi, the h2d.TileGroup texture, the PaletteSwapShader),
/// so they can't be Rust. But their only consumer is the host GUI's icon pipeline
/// (`gui.rs::icon_data_url` decodes the hex + replays the palette), NOT the end user.
/// So the *source* lives here, next to the host code that drives it, instead of in
/// `commands.hsx` (which is reserved for end-user-facing script helpers). The patcher
/// concatenates this onto `commands.hsx` so both are parsed into the same interp at boot
/// (see `main.rs` where `commands.hsx` is read); behaviour is identical to before.
///
/// Requested by the GUI via `@@icon:<slot>` → `e iconFeed(<slot>)` → the `charIcon`
/// channel. Emits PNG **hex** (an instance method — reflection-safe; a static Base64
/// closure call crashes the engine) plus the palette-swap map; the host recolors.
pub const UI_BRIDGE_HSX: &str = r#"
// ─── peptide UI bridge (host-owned; see interpreter.rs::UI_BRIDGE_HSX) ───
// matchStatus stock-icon feed. Defined in-engine but driven only by the host GUI.

// iconRaw(c) — pull the character's STOCK icon straight out of the ENGINE as PNG hex. Works for
// ANY character in the match, including ones with no FrayTools project on disk. The chain is pure
// reflection over engine objects — ZERO bytecode: the character's DamageCounter
// (get_fraymakersDamageCounter) -> getStockIconSprites().icons[0] (a pxf SpriteApi — note
// getStockIconSprites lives on the API, not the entity, so we go through the DamageCounter exactly
// as the engine does) -> __sprite__ -> _tileLayers[0].tile (the live texture; _bitmaps tiles are
// sized-but-textureless) -> h3d.mat.Texture -> capturePixels -> crop -> toPNG -> toHex. (Hex, not
// base64: toHex is an instance method, reflection-safe; a static Base64 closure call crashes the
// engine. The host decodes the hex.) Returns "ERR:<where>" on a missing link; null-safe throughout.
iconRaw = function(c) {
  if (c == null) return "ERR:nochar";
  var dc = c.get_fraymakersDamageCounter();
  if (dc == null) return "ERR:nodc";
  var sprs = dc.getStockIconSprites();
  if (sprs == null) return "ERR:nosprs";
  // Prefer the per-stock icon (icons[0]); fall back to the single compact icon.
  var spr = null;
  if (sprs.icons != null && sprs.icons.length > 0) spr = sprs.icons[0];
  if (spr == null) spr = sprs.compactIcon;
  if (spr == null) return "ERR:nospr";
  var s = spr.__sprite__;
  if (s == null) return "ERR:nosprite";
  // The live texture is on a TileGroup in _tileLayers, NOT on _bitmaps (whose tiles are
  // sized-but-textureless). _tileLayers[0].tile is a sub-rect of a power-of-two atlas tex.
  var tl = s._tileLayers;
  if (tl == null || tl.length == 0 || tl[0] == null) return "ERR:notl";
  var tile = tl[0].tile;
  if (tile == null) return "ERR:notile";
  var tex = tile.innerTex;
  if (tex == null) return "ERR:notex";
  var full = tex.capturePixels(0, 0, null);
  if (full == null) return "ERR:nopx";
  // Pass the tile's own coords straight to sub() — hscript's `Std` is NOT bound in this interp,
  // so Std.int() would throw; capturePixels/sub accept the tile's numeric fields directly.
  var crop = full.sub(tile.x, tile.y, tile.width, tile.height);
  if (crop == null) return "ERR:nocrop";
  var png = crop.toPNG();
  if (png == null) return "ERR:nopng";
  // Emit the PNG as HEX (an instance method — reflection-safe, unlike a static Base64 closure
  // call, which crashes the engine). The host decodes hex -> bytes -> base64 data: URL. The
  // image still comes wholly from the engine; hex is just safe transport over the text socket.
  return png.toHex();
};

// iconOf(c) — iconRaw, normalized: PNG hex on success, "" on a null/missing icon, or the raw
// "ERR:<where>" marker during bring-up. The host turns the hex into a base64 data: URL.
iconOf = function(c) {
  var b = iconRaw(c);
  if (b == null) return "";
  return b;
};

// paletteOf(c) — the character's stock-icon palette as " <src>><dst>"-pairs (ARGB ints, signed).
// The stock icon's recolor lives in a PaletteSwapShader added to the sprite (DamageCounter
// ._charShader.__paletteSwapShader__.paletteMap), NOT baked into the base texture, so iconRaw
// captures un-recolored art; the host replays this exact-color map over those pixels. "" if no
// palette (then the host shows the base art unchanged). Null-safe throughout.
paletteOf = function(c) {
  var dc = (c != null) ? c.get_fraymakersDamageCounter() : null;
  if (dc == null) return "";
  var sh = dc._charShader;
  if (sh == null) return "";
  var ps = sh.__paletteSwapShader__;
  if (ps == null) return "";
  var pm = ps.paletteMap;
  if (pm == null) return "";
  var it = pm.keys();
  var s = "";
  while (it.hasNext()) { var k = it.next(); s = s + k + ">" + pm.get(k) + " "; }
  return s;
};

// iconFeed(i) — the "charIcon" TCP channel: "ICON:<slot>:<png-hex>;<palette>" for the character in
// match slot i (host-requested via @@icon:<i>, NOT polled, since a capture is comparatively
// expensive). The host decodes the hex, applies <palette> (the swap map), and turns it into a
// base64 data: URL. On any failure the hex is empty ("ICON:<i>:"), so the widget keeps its glyph.
iconFeed = function(i) {
  var cs = getCharacters();
  if (i < 0 || i >= cs.length || cs[i] == null) return "ICON:" + i + ":";
  var c = cs[i];
  var h = iconRaw(c);
  if (h == null || h.substr(0, 4) == "ERR:") return "ICON:" + i + ":";
  return "ICON:" + i + ":" + h + ";" + paletteOf(c);
};

// iconDiag(c) — fully-guarded, never-throws introspection of the stock-icon chain. It walks the
// per-stock icon (icons[0], falling back to compactIcon), then dumps every entry of BOTH the
// sprite's _bitmaps and its _tileLayers (h2d.TileGroup), reporting each tile's size and whether a
// texture is bound (innerTex). h2d.Bitmap tiles can be sized-but-textureless; the live texture
// usually lives on a TileGroup's tile, so this run reveals exactly where to read pixels from.
iconDiag = function(c) {
  if (c == null) return "nochar";
  var dc = c.get_fraymakersDamageCounter();
  if (dc == null) return "dc=null";
  var sprs = dc.getStockIconSprites();
  if (sprs == null) return "sprs=null";
  var out = "icons=" + (sprs.icons == null ? "?" : ("" + sprs.icons.length));
  var spr = null;
  if (sprs.icons != null && sprs.icons.length > 0) { spr = sprs.icons[0]; }
  if (spr == null) { spr = sprs.compactIcon; out = out + " (compact)"; }
  else { out = out + " (icons[0])"; }
  if (spr == null) { return out + " noSpr"; }
  var s = spr.__sprite__;
  if (s == null) { return out + " sprite=null"; }
  // ---- _bitmaps ----
  var bm = s._bitmaps;
  if (bm == null) { out = out + " bm=null"; }
  else {
    out = out + " bm=" + bm.length;
    var i = 0;
    while (i < bm.length) {
      var bt = (bm[i] == null) ? null : bm[i].tile;
      var btx = (bt == null) ? null : bt.innerTex;
      out = out + " bm" + i + "=" + (bt == null ? "noTile" : (bt.width + "x" + bt.height)) + "/" + (btx == null ? "noTex" : ("TEX" + btx.width + "x" + btx.height));
      i = i + 1;
    }
  }
  // ---- _tileLayers (TileGroups; their .tile usually holds the real texture) ----
  var tl = s._tileLayers;
  if (tl == null) { out = out + " tl=null"; }
  else {
    out = out + " tl=" + tl.length;
    var j = 0;
    while (j < tl.length) {
      var lt = (tl[j] == null) ? null : tl[j].tile;
      var ltx = (lt == null) ? null : lt.innerTex;
      out = out + " tl" + j + "=" + (lt == null ? "noTile" : (lt.width + "x" + lt.height)) + "/" + (ltx == null ? "noTex" : ("TEX" + ltx.width + "x" + ltx.height));
      j = j + 1;
    }
  }
  return out;
};
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_payload_routes_matchstatus_and_icon() {
        // through the E: prefix, and bare
        assert_eq!(channel_payload("E:MATCHSTATUS:a|0|idle"), Some(("matchStatus", "a|0|idle")));
        assert_eq!(channel_payload("MATCHSTATUS:"), Some(("matchStatus", "")));
        assert_eq!(channel_payload("E:ICON:0:89504e47"), Some(("charIcon", "0:89504e47")));
        assert_eq!(channel_payload("ICON:2:"), Some(("charIcon", "2:"))); // empty hex (graceful fail)
        // not a channel -> goes to the chat
        assert_eq!(channel_payload("E:hello"), None);
        assert_eq!(channel_payload("LAUNCHED foo"), None);
    }

    fn wire(line: &str) -> String {
        match translate(line) {
            Translated::Wire(w) => w,
            Translated::Client(_) => panic!("expected Wire, got Client"),
        }
    }

    #[test]
    fn controls_mask_parses_names_and_combos() {
        assert_eq!(controls_mask(&["down"]).unwrap(), 0x2);
        assert_eq!(controls_mask(&["down+special"]).unwrap(), 0x22);
        assert_eq!(controls_mask(&["down", "special"]).unwrap(), 0x22); // space-joined
        assert_eq!(controls_mask(&["right"]).unwrap(), 0x8);
        assert_eq!(controls_mask(&["shield"]).unwrap(), 0x100);         // alias of shield1
        assert_eq!(controls_mask(&["0x22"]).unwrap(), 0x22);            // raw hex
        assert_eq!(controls_mask(&["34"]).unwrap(), 34);               // raw decimal
        assert!(controls_mask(&["banana"]).is_err());
    }

    #[test]
    fn hold_press_release_translate_to_input_byte() {
        assert_eq!(wire("hold down+special"), "i 34");   // 0x2|0x20 = 34
        assert_eq!(wire("press right"), "i 8");
        assert_eq!(wire("hold up"), "i 1");
        assert_eq!(wire("release"), "i 0");
        // bad control is handled client-side (no wire), not sent to the engine
        assert!(matches!(translate("hold banana"), Translated::Client(_)));
        assert!(matches!(translate("hold"), Translated::Client(_))); // usage hint
    }

    #[test]
    fn seq_expands_to_one_input_line_per_frame() {
        // down+special for 2 frames, right for 3 frames, then auto-release.
        let w = wire("seq down+special:2 right:3");
        let lines: Vec<&str> = w.split('\n').collect();
        assert_eq!(lines, vec!["i 34", "i 34", "i 8", "i 8", "i 8", "i 0"]);
        // bare step = 1 frame; `release`/`neutral`/empty = mask 0.
        assert_eq!(wire("seq attack release:2"), "i 16\ni 0\ni 0\ni 0");
        // star separator + raw mask both work.
        assert_eq!(wire("seq 0x8*2"), "i 8\ni 8\ni 0");
        // errors are client-side (never sent to the engine).
        assert!(matches!(translate("seq down:0"), Translated::Client(_)));   // 0 frames
        assert!(matches!(translate("seq bogus:2"), Translated::Client(_)));  // bad control
        assert!(matches!(translate("seq"), Translated::Client(_)));          // usage
    }

    #[test]
    fn scenario_sets_up_scene_then_plays_inputs() {
        // positions only -> setup eval (with neutral reset), no input lines.
        assert_eq!(
            wire("scenario 300,400 360,400"),
            "e p0.setX(300); p0.setY(400); p1.setX(360); p1.setY(400); p0.toState(CState.STAND); p1.toState(CState.STAND)"
        );
        // positions + an input timeline on p0 -> setup eval then one `i` line per frame.
        assert_eq!(
            wire("scenario 300,400 360,400 attack:2"),
            "e p0.setX(300); p0.setY(400); p1.setX(360); p1.setY(400); p0.toState(CState.STAND); p1.toState(CState.STAND)\ni 16\ni 16\ni 0"
        );
        // 4-tuple adds momentum (world-space velocity).
        assert!(wire("scenario 300,400,5,0 360,400").contains("p0.setXSpeed(5); p0.setYSpeed(0)"));
        // bad shapes are client-side errors (nothing sent to the engine).
        assert!(matches!(translate("scenario 300,400"), Translated::Client(_)));         // only one player
        assert!(matches!(translate("scenario 300 360,400"), Translated::Client(_)));     // p0 not x,y
        assert!(matches!(translate("scenario a,b 360,400"), Translated::Client(_)));     // non-numeric
        assert!(matches!(translate("scenario 300,400 360,400 bogus:2"), Translated::Client(_))); // bad control
    }

    #[test]
    fn tune_builds_update_hitbox_stats_eval() {
        // numeric stats pass through bare; enum-ish values pass verbatim.
        assert_eq!(wire("tune p0 0 damage=15 baseKnockback=50"),
                   "e p0.updateHitboxStats(0, { damage: 15, baseKnockback: 50 })");
        assert_eq!(wire("tune p1 2 limb=AttackLimb.FOOT"),
                   "e p1.updateHitboxStats(2, { limb: AttackLimb.FOOT })");
        // a move-name selector (SSF2 addressing) is accepted and passed verbatim.
        assert_eq!(wire("tune p0 AttackS3 damage=15 angle=45"),
                   "e p0.updateHitboxStats(AttackS3, { damage: 15, angle: 45 })");
        // shape errors are client-side (never sent).
        assert!(matches!(translate("tune p0 0"), Translated::Client(_)));        // no stats
        assert!(matches!(translate("tune p0 0 damage"), Translated::Client(_)));  // not key=value
        assert!(matches!(translate("tune p0 -1 damage=1"), Translated::Client(_))); // negative index, not a valid name
        assert!(matches!(translate("tune p0 1.5 damage=1"), Translated::Client(_))); // not int, not a bare identifier
    }

    #[test]
    fn dmg_and_info_are_eval_wrappers() {
        assert_eq!(wire("dmg p1 80"), "e p1.damage._damage = 80");
        assert!(wire("info").starts_with("e [p0.getX(), p0.getStateName()"));
        assert!(matches!(translate("dmg p1"), Translated::Client(_)));       // missing value
        assert!(matches!(translate("dmg p1 lots"), Translated::Client(_)));  // non-numeric
    }

    #[test]
    fn reset_and_kill_are_eval_wrappers() {
        assert!(wire("reset").contains("p0.toState(CState.STAND)") && wire("reset").contains("setXSpeed(0)"));
        assert_eq!(wire("kill p1"), "e p1.setY(p1.getY() + 3000)");
        assert!(matches!(translate("kill"), Translated::Client(_)));        // missing player
        assert!(matches!(translate("kill p1;evil"), Translated::Client(_))); // injection guard
    }

    #[test]
    fn bootstrap_commands_stay_wire_bytes() {
        // match-launch + clean exit + resource load are the irreducible bootstrap hooks;
        // console is a side-effecting method call hscript can't invoke. All stay wire bytes.
        assert_eq!(wire("spawn sandbag thespire x"), "s sandbag thespire x");
        assert_eq!(wire("startMatch sandbag thespire x"), "s sandbag thespire x");
        assert_eq!(wire("exit"), "x");
        assert_eq!(wire("console"), "c");
        assert_eq!(wire("load"), "l");
        assert_eq!(wire("start sandbag"), "s sandbag");
        assert_eq!(wire("quit"), "x");
    }

    #[test]
    fn spawn_encodes_multi_player_rosters() {
        // a 2-char mirror: wire keeps player0 at parts[1], stage/assist at parts[2..3],
        // and the extra player at parts[4].
        assert_eq!(wire("spawn sandbag,sandbag"),
                   format!("s sandbag {FM_DEFAULT_STAGE} {FM_DEFAULT_ASSIST} sandbag"));
        // a full 4-char roster with an explicit stage: players 3 and 4 land at parts[5..6].
        assert_eq!(wire("spawn sandbag,mario,sandbag,mario thespire"),
                   format!("s sandbag thespire {FM_DEFAULT_ASSIST} mario sandbag mario"));
        // explicit stage + assist are honored for the multi-player slots too.
        assert_eq!(wire("spawn a,b,c,d stg ast"), "s a stg ast b c d");
    }

    #[test]
    fn spawn_rejects_more_than_four_players() {
        // Fraymakers has 4 ports; a 5-char roster is a client-side error, not a wire send.
        assert!(matches!(translate("spawn a,b,c,d,e"), Translated::Client(_)));
        // exactly 4 is fine.
        assert!(matches!(translate("spawn a,b,c,d"), Translated::Wire(_)));
    }

    #[test]
    fn raw_hscript_passes_through_eval() {
        // the dropped sugar commands + anything unrecognized are treated as hscript and
        // run through the eval hook — this is the explicit, no-hidden-target model.
        assert_eq!(wire("match.getCharacters()[0].getStateName()"),
                   "e match.getCharacters()[0].getStateName()");
        assert_eq!(wire("match.getCharacters()[0].toState(CState.JAB)"),
                   "e match.getCharacters()[0].toState(CState.JAB)");
        assert_eq!(wire("CState.JAB"), "e CState.JAB");
        assert_eq!(wire("1 + 2"), "e 1 + 2");
        assert_eq!(wire("eval CState.JAB"), "e CState.JAB");   // explicit eval == default
    }

    #[test]
    fn multiline_hscript_collapses_to_one_wire_line() {
        // A pasted/typed multi-line snippet must reach the engine as ONE '\n'-free line
        // (the wire is newline-delimited); hscript separates statements with `;`, so
        // folding line breaks to spaces is semantics-preserving.
        assert_eq!(
            wire("var c = match.getCharacters()[0];\nc.toState(CState.JAB);"),
            "e var c = match.getCharacters()[0]; c.toState(CState.JAB);"
        );
        // CRLF (Windows clipboard) folds the same way — no stray '\r'.
        assert_eq!(wire("a;\r\nb;"), "e a; b;");
        // a single line is untouched (and never gets a sentinel).
        assert_eq!(wire("1 + 2"), "e 1 + 2");
    }

    #[test]
    fn multiline_drops_line_comments_and_keeps_strings_intact() {
        // `//` comments must not swallow the following line once newlines collapse.
        assert_eq!(wire("a;  // set a\nb;"), "e a;   b;");
        // `//` and line breaks INSIDE a string literal are preserved (newline -> \n escape).
        assert_eq!(wire("log(\"a//b\");"), "e log(\"a//b\");");
        assert_eq!(wire("log(\"a\nb\");"), "e log(\"a\\nb\");");
        // block comments survive; their internal newline becomes a space.
        assert_eq!(wire("a;/* x\ny */b;"), "e a;/* x y */b;");
    }

    #[test]
    fn split_commands_separates_on_blank_lines() {
        // a blank line breaks one message into independent commands…
        assert_eq!(
            split_commands("spawn sandbag\n\nmatch.getCharacters()[0].getStateName()"),
            vec!["spawn sandbag", "match.getCharacters()[0].getStateName()"]
        );
        // …several blanks / leading / trailing blanks collapse and are ignored.
        assert_eq!(split_commands("\n\na\n\n\nb\n\n"), vec!["a", "b"]);
        // no blank line -> exactly one block (single command, possibly multi-line).
        assert_eq!(
            split_commands("var c = match.getCharacters()[0];\nc.toState(CState.JAB);"),
            vec!["var c = match.getCharacters()[0];\nc.toState(CState.JAB);"]
        );
        // empty / whitespace-only message -> no commands.
        assert!(split_commands("   \n\n").is_empty());
    }

    #[test]
    fn split_commands_keeps_multiline_statements_intact() {
        // a blank line INSIDE open braces is not a separator (one statement).
        assert_eq!(
            split_commands("if (x) {\n\n  y;\n}"),
            vec!["if (x) {\n\n  y;\n}"]
        );
        // a blank line inside a string literal is not a separator.
        assert_eq!(split_commands("log(\"a\n\nb\")"), vec!["log(\"a\n\nb\")"]);
        // and the multi-command case still feeds each block through translate cleanly.
        let wires: Vec<String> = split_commands("1 + 2\n\nspawn sandbag")
            .iter().map(|c| wire(c)).collect();
        assert_eq!(wires, vec!["e 1 + 2", "s sandbag"]);
    }

    #[test]
    fn help_is_client_side() {
        assert!(matches!(translate("help"), Translated::Client(_)));
        assert!(matches!(translate("?"), Translated::Client(_)));
    }

    #[test]
    fn unknown_routes_to_eval() {
        // unrecognized input is treated as hscript and run through the eval hook
        assert_eq!(wire("somethingnew arg"), "e somethingnew arg");
    }

    #[test]
    fn interpret_crash_stage_with_resdiag() {
        let log = "Exception: Null access .stagePxfContentMap\n\
                   Called from pxf.core.Match.setupStage (pxf/core/Match.hx line 1095)\n\
                   Called from hxd.App.mainLoop (hxd/App.hx line 193)";
        let resdiag = vec![
            "RESDIAG: stage failed to load — Match.setupStage got null from getPXFResource for resource id: global::teststage".to_string()
        ];
        let out = interpret_crash(log, &resdiag).unwrap();
        // top: plain-English translation with the failing id woven in
        assert!(out.starts_with("Hmm… the stage didn't load."), "got: {out}");
        assert!(out.contains("global::teststage isn't a valid/loadable stage resource"), "got: {out}");
        // bottom: abridged exception + crash site (NOT the full stack)
        assert!(out.contains("Engine exception: Null access .stagePxfContentMap"), "got: {out}");
        assert!(out.contains("Crash site: pxf.core.Match.setupStage (pxf/core/Match.hx line 1095)"), "got: {out}");
        assert!(!out.contains("mainLoop"), "abridged log should drop hxd plumbing: {out}");
    }

    #[test]
    fn interpret_crash_empty_is_none() {
        assert!(interpret_crash("", &[]).is_none());
    }
}
