//! bridge — headless runtime: the loopback TCP server that the injected engine
//! dials into. `serve` is the interactive stdin<->socket bridge; `send_once` is a
//! one-shot for scripts. Connection setup (await_engine) lives in `ui` and is shared.

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::interpreter::{translate, gloss, Translated};

const DEFAULT_PORT: u16 = 17999;

/// Write one wire line + flush; false if the socket is gone. Shared by the
/// interactive `serve` loop and the persistent `session` daemon.
fn send_wire(w: &mut TcpStream, wire: &str) -> bool {
    let mut line = wire.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).is_ok() && w.flush().is_ok()
}
pub fn parse_port(args: &[String]) -> u16 {
    args.iter()
        .position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT)
}
pub fn parse_token(args: &[String]) -> Option<String> {
    args.iter()
        .position(|a| a == "--token")
        .and_then(|i| args.get(i + 1))
        .cloned()
}
/// Read the engine's byte stream, assemble newline-delimited lines, drop channel
/// feeds (matchStatus/icons), dedup repeated per-frame ANIM lines, gloss the rest,
/// and hand each surviving line to `emit(raw, pretty)`. Signals `ready_tx` on every
/// READY line. Returns when the stream ends (EOF or read error) — the caller must
/// NOT close the socket on return (that Eof-crashes the engine's per-frame write).
///
/// ROBUSTNESS: reads RAW BYTES + lossy-decodes, instead of `reader.lines()` (which
/// aborts the moment the engine emits a non-UTF8 byte). Shared by `serve` (emits to
/// stdout) and `session` (emits to the log file).
/// CLI sink for the shared Fraymakers stream router (`session::pump_fray_stream`): mirrors
/// meaningful lines to `emit` (stdout for `serve`, the log for `session`) and signals READY.
/// Channel feeds (matchStatus/icons) are dropped — they're not for the CLI/log. Per-frame
/// ANIM telemetry is suppressed unless the state changed.
struct CliStreamSink<'a, F: FnMut(&str, &str)> {
    ready_tx: &'a mpsc::Sender<()>,
    emit: F,
    last_anim: String,
}
impl<F: FnMut(&str, &str)> CliStreamSink<'_, F> {
    fn pretty(line: &str) -> String {
        match gloss(line) {
            Some(g) => format!("<< {line:<28} ({g})"),
            None => format!("<< {line}"),
        }
    }
}
impl<F: FnMut(&str, &str)> crate::session::FrayStreamSink for CliStreamSink<'_, F> {
    fn on_ready(&mut self) { let _ = self.ready_tx.send(()); }
    // RESDIAG flows to the sink like a normal line (the caller's `emit` captures the
    // breadcrumb for the crash report and logs it), matching the prior behavior.
    fn on_resdiag(&mut self, line: &str) { (self.emit)(line, &Self::pretty(line)); }
    // channel feeds: dropped (no-op) — they belong to widgets, not the CLI/log.
    fn on_anim(&mut self, state: &str) {
        if state != self.last_anim {
            self.last_anim = state.to_string();
            let line = format!("ANIM:{state}");
            (self.emit)(&line, &Self::pretty(&line));
        }
    }
    fn on_line(&mut self, raw: &str) { (self.emit)(raw, &Self::pretty(raw)); }
}

fn pump_engine_lines(
    reader: BufReader<TcpStream>,
    ready_tx: &mpsc::Sender<()>,
    emit: impl FnMut(&str, &str),
) {
    let mut sink = CliStreamSink { ready_tx, emit, last_anim: String::new() };
    crate::session::pump_fray_stream(reader, &mut sink);
}

pub fn serve(port: u16, token: Option<&str>) {
    let (reader, mut write_half) = crate::ui::await_engine(port, token);

    // socket -> stdout; signal once the engine reports READY. Keep a small ring of
    // the last meaningful events so a crash dump can show what was happening.
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    // Eval replies are also forwarded here so a host-side command can READ engine state
    // instead of sleeping. `await` polls the animation clock over this channel; without
    // it the only way to "wait for an animation" is a wall-clock guess, which observes a
    // half-played animation under load and idles the rest of the time.
    let (eval_tx, eval_rx) = mpsc::channel::<String>();
    thread::spawn(move || {
        let mut history: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        pump_engine_lines(reader, &ready_tx, |raw, pretty| {
            if let Some(v) = raw.strip_prefix("E:") { let _ = eval_tx.send(v.to_string()); }
            println!("{pretty}");
            if raw.starts_with(|c: char| c.is_ascii_uppercase()) && raw.contains(':') {
                history.push_back(raw.to_string());
                if history.len() > 16 { history.pop_front(); }
            }
        });
        eprintln!("peptide-bridge: engine stream ended (holding socket open)");
        if !history.is_empty() {
            eprintln!("peptide-bridge: ── last {} engine events before stream ended (crash context) ──", history.len());
            for ev in &history { eprintln!("peptide-bridge:    {ev}"); }
        }
    });

    // Always wait for READY before accepting any input.
    eprintln!("peptide-bridge: waiting for engine READY…");
    match ready_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(()) => eprintln!("peptide-bridge: engine is READY — enter commands:"),
        Err(_) => {
            eprintln!("peptide-bridge: timed out waiting for READY");
            return;
        }
    }

    // stdin -> socket. The FIRST command fires the instant READY arrives
    // (event-driven, no artificial pre-delay). FRAY_CMD_GAP (seconds, may be
    // fractional) paces SUBSEQUENT commands so a piped multi-command sequence
    // doesn't all flush at once — callers dump every command immediately and
    // let us space them, instead of sleeping before READY.
    let cmd_gap = std::env::var("FRAY_CMD_GAP").ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|g| *g > 0.0);
    let stdin = std::io::stdin();
    let mut first = true;
    for line in stdin.lock().lines() {
        let Ok(raw) = line else { break };
        // `await` / `awaitmatch` are executed HERE, host-side, by polling the engine's
        // own animation clock — neither has a wire verb. This is the pacing primitive:
        // a caller sends a state, then `await`, and the next command doesn't go out until
        // the animation has actually played (or looped / stalled), whatever that took in
        // wall-clock terms.
        let parsed = crate::interpreter::parse(&raw);
        // `record`/`trace` are pure host-side buffer operations over the FRAME: telemetry
        // the engine pushes — no wire verb, nothing to ask the engine.
        if let crate::interpreter::Command::Record { on } = parsed {
            if on { crate::debug_target::fm_frames_clear(); }
            println!("<< record: {}", if on { "window opened (engine pushes FRAME: telemetry)" }
                                      else { "the Fraymakers recorder always pushes — `trace` reads the window" });
            first = false;
            continue;
        }
        if matches!(parsed, crate::interpreter::Command::Trace) {
            let frames = crate::debug_target::fm_frames_take();
            let runs = crate::debug_target::rle_frames(&frames);
            println!("<< TRACE:{} frames, {} animations", frames.len(), runs.len());
            for (label, n) in &runs { println!("<<   {label} x{n}"); }
            first = false;
            continue;
        }
        if matches!(parsed, crate::interpreter::Command::Await { .. }
                          | crate::interpreter::Command::AwaitMatch { .. })
        {
            let idx = match parsed {
                crate::interpreter::Command::Await { idx, .. }
                | crate::interpreter::Command::AwaitMatch { idx, .. } => idx,
                _ => 0,
            };
            let w = &mut write_half;
            // One sampler for both: ask for the clock, then take the next reply that is
            // actually a sample. Replies from EARLIER commands are still queued (`E:true`
            // from the toState that got us here), so returning the first thing off the
            // channel reads a stale value and the watch gives up before it starts.
            let mut sample = || {
                if !send_wire(w, &format!("e animFeed({idx})")) {
                    return Ok(None);
                }
                let deadline = Instant::now() + Duration::from_millis(1500);
                while Instant::now() < deadline {
                    match eval_rx.recv_timeout(Duration::from_millis(200)) {
                        Ok(v) => {
                            if let Some(s) = crate::debug_target::AnimState::parse(&v) {
                                return Ok(Some(s));
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(_) => return Ok(None),
                    }
                }
                Ok(None)
            };
            let msg = match parsed {
                crate::interpreter::Command::AwaitMatch { tries, .. } => {
                    match crate::debug_target::await_live(sample, tries) {
                        Ok(Some(s)) => format!("MATCHREADY:p{idx} anim={} pos=({:.1},{:.1})", s.anim, s.x, s.y),
                        Ok(None) => format!("MATCHREADY:none (p{idx} never became readable in {tries} tries)"),
                        Err(e) => format!("MATCHREADY:error {e}"),
                    }
                }
                crate::interpreter::Command::Await { budget, .. } => {
                    match crate::debug_target::watch_animation(&mut sample, budget, 3) {
                        Ok(w) => match &w.last {
                            Some(s) => format!(
                                "AWAIT:{:?} anim={} frame={}/{} frames_seen={} pos=({:.1},{:.1})",
                                w.outcome, s.anim, s.frame, s.total, w.frames_seen, s.x, s.y),
                            None => format!("AWAIT:{:?} (no live character p{idx})", w.outcome),
                        },
                        Err(e) => format!("AWAIT:error {e}"),
                    }
                }
                _ => unreachable!(),
            };
            println!("<< {msg}");
            first = false;
            continue;
        }
        // Translate the friendly command into the engine wire line; `help` is
        // handled here and never reaches the engine.
        match translate(&raw) {
            Translated::Client(text) => { print!("{text}"); continue; }
            Translated::Wire(wire) => {
                if !first {
                    if let Some(g) = cmd_gap { thread::sleep(Duration::from_secs_f64(g)); }
                }
                first = false;
                if wire == raw.trim() {
                    eprintln!("peptide-bridge: SENT {wire:?}");
                } else {
                    eprintln!("peptide-bridge: SENT {wire:?}  (from {:?})", raw.trim());
                }
                if !send_wire(&mut write_half, &wire) {
                    eprintln!("peptide-bridge: write failed (engine gone?)");
                    break;
                }
            }
        }
    }

    // CRITICAL: stdin EOF (e.g. a piped command list finished) must NOT close the
    // socket — that would Eof-crash the engine's per-frame write. Hold the socket
    // open until the engine itself goes away. FRAY_HOLD_SECS bounds the wait so a
    // scripted run still terminates; default is long enough for a match.
    let hold = std::env::var("FRAY_HOLD_SECS").ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(600);
    eprintln!("peptide-bridge: stdin closed; holding socket open up to {hold}s (engine keeps running)");
    let mut probe = [0u8; 1];
    let deadline = Duration::from_secs(hold);
    let step = Duration::from_millis(250);
    let mut waited = Duration::ZERO;
    // Keep the write half alive; periodically check the engine is still there by
    // attempting a zero-impact write of nothing (flush). If the engine vanished,
    // the flush/write eventually errors and we exit.
    while waited < deadline {
        thread::sleep(step);
        waited += step;
        if write_half.write_all(&[]).is_err() || write_half.flush().is_err() {
            eprintln!("peptide-bridge: engine gone; exiting");
            break;
        }
        let _ = &mut probe; // reserved for future liveness ping
    }
}
pub fn send_once(port: u16, token: Option<&str>, cmd: &str) {
    // Client-only commands (help) never open a socket — answer them inline through
    // the shared parser so the vocabulary stays identical to SSF2's.
    match crate::interpreter::parse(cmd) {
        crate::interpreter::Command::Help => { print!("{}", crate::interpreter::help_text()); return; }
        crate::interpreter::Command::Client(s) => { if !s.trim().is_empty() { print!("{s}"); } return; }
        _ => {}
    }
    let (mut reader, write_half) = crate::ui::await_engine(port, token);

    // ALWAYS wait for the engine's "READY" line (title screen / welcome announcer
    // = all .fra content loaded) before sending any command. Sending earlier runs
    // engine code mid-load, which crashes. Bounded read-timeout loop so a stalled
    // boot eventually gives up instead of blocking forever.
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(1)));
    eprintln!("peptide-bridge: waiting for engine READY…");
    let ready_deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => { eprintln!("peptide-bridge: connection closed before READY"); return; }
            Ok(_) => {
                let l = line.trim();
                if !l.is_empty() { println!("<< {l}"); }
                if l.contains("READY") { eprintln!("peptide-bridge: engine is READY"); break; }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {
                if std::time::Instant::now() >= ready_deadline {
                    eprintln!("peptide-bridge: timed out waiting for READY — aborting send");
                    return;
                }
            }
            Err(_) => { eprintln!("peptide-bridge: read error waiting for READY — aborting"); return; }
        }
    }
    // Optional delay AFTER READY before sending the command. READY fires at boot
    // complete, but custom/workshop content (UGC) loads ASYNC during the title
    // sequence and may not be in the engine's resource pool yet. Sending
    // `s <custom char>` too early hits an unresolved content ref and crashes the spawn.
    // FRAY_POST_READY_DELAY (seconds) lets UGC finish before we send.
    if let Ok(d) = std::env::var("FRAY_POST_READY_DELAY") {
        if let Ok(secs) = d.trim().parse::<f64>() {
            if secs > 0.0 {
                eprintln!("peptide-bridge: post-READY delay {secs}s (let UGC load)…");
                thread::sleep(Duration::from_millis((secs * 1000.0) as u64));
            }
        }
    }

    // Execute through the SAME OOP dispatcher SSF2 uses: parse → Command → backend.
    let mut target = crate::debug_target::FraymakersTarget::new(reader, write_half);
    eprintln!("peptide-bridge: sent {cmd:?}");
    match crate::debug_target::run_command(&mut target, cmd) {
        Ok(Some(reply)) => for l in reply.lines() { println!("<< {l}"); },
        Ok(None) => {}
        Err(e) => eprintln!("peptide-bridge: {e}"),
    }
}

// ── persistent session (agent-driven iterative testing) ─────────────────────
// `peptide session` is the long-lived daemon for iterating on a character or a
// conversion fix: it boots the engine (or attaches to one), HOLDS the TCP link
// open, and processes commands appended to a control file while mirroring every
// engine line to a log. Unlike `serve` (one stdin stream, consumed once) it lets
// a caller inject NEW commands over time — `peptide tell "<cmd>"` queues one,
// `peptide log` reads the replies — so you can send an eval, read its result,
// then decide the next eval, all against the SAME live match. This is how the
// game keeps streaming TCP messages back while you run evals. Stop it with
// `peptide tell exit` (clean engine shutdown) or by killing the process.

/// Where a session keeps its control file + output log. `PEPTIDE_SESSION_DIR`
/// overrides; `--dir` on the command line overrides that. One well-known dir by
/// default so `tell`/`log` find the running session with no arguments.
/// Holds the spawned overlay child and kills it when the session drops.
pub(crate) fn default_session_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PEPTIDE_SESSION_DIR") {
        if !d.trim().is_empty() { return PathBuf::from(d); }
    }
    let base = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    base.join(".peptide").join("session")
}

/// Value of `--flag <value>` if present.
fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn session_dir(args: &[String]) -> PathBuf {
    arg_val(args, "--dir").map(PathBuf::from).unwrap_or_else(default_session_dir)
}

/// Mirror one line to BOTH the session log file (canonical — what `peptide log`
/// reads) and stdout (useful when the daemon is run in the foreground).
fn slog(log: &Arc<Mutex<std::fs::File>>, s: &str) {
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
    }
    println!("{s}");
    let _ = std::io::stdout().flush();
}

/// Translate one queued command and send it to the engine, logging the action.
fn process_cmd(writer: &mut TcpStream, raw: &str, log: &Arc<Mutex<std::fs::File>>) {
    match translate(raw) {
        Translated::Client(text) => slog(log, &format!(">> (client) {}", text.trim_end())),
        Translated::Wire(wire) => {
            slog(log, &format!(">> SENT {wire:?}  (from {:?})", raw.trim()));
            if !send_wire(writer, &wire) { slog(log, ">> write failed (engine gone?)"); }
        }
    }
}

/// Decide whether the session should also take typed commands from this terminal.
/// Default: yes when stdin is a TTY (a human/agent sitting at the prompt). `-i` /
/// `--interactive` forces it on even when stdin is piped; `--no-input` forces it off
/// (pure daemon: only `peptide tell` feeds it).
fn want_interactive(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--no-input") { return false; }
    if args.iter().any(|a| a == "-i" || a == "--interactive") { return true; }
    std::io::stdin().is_terminal()
}

/// Foreground REPL feeder: read lines typed at this terminal and append each to the
/// session control file, so they flow through the SAME single-writer dispatch path as
/// `peptide tell` (no second socket writer → no races). Engine output is already
/// mirrored to stdout by `slog`, so the terminal behaves like the GUI chat: type a
/// command, watch the reply stream back. EOF (Ctrl-D) just stops reading stdin; it does
/// NOT close the socket or kill the engine — type `exit` (or Ctrl-C) to shut down.
fn spawn_stdin_feeder(control: PathBuf, log: Arc<Mutex<std::fs::File>>) {
    thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(raw) = line else { break };
            if raw.trim().is_empty() { continue; }
            match std::fs::OpenOptions::new().append(true).open(&control) {
                Ok(mut f) => { let _ = writeln!(f, "{}", raw.trim()); }
                Err(e) => { slog(&log, &format!("[session] stdin: cannot write control file: {e}")); break; }
            }
        }
    });
}

/// `peptide session [--dir D] [--char C | --full] [--no-boot --port N --token T] [-i|--no-input]`
/// Boot (or attach to) an engine and run the persistent command loop.
pub fn session(args: &[String]) {
    let dir = session_dir(args);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("peptide session: cannot create session dir {}: {e}", dir.display());
        std::process::exit(1);
    }
    let control = dir.join("control");
    let logp = dir.join("out.log");
    let metap = dir.join("meta");

    // Acquire the engine connection. Default: BOOT a throwaway engine we own (a
    // baked --char triggers headless fast-boot; --full does a Title/UGC bridge
    // boot you then drive with `spawn`). `--no-boot`/`--attach`: connect to an
    // engine someone else launched on --port/--token.
    let no_boot = args.iter().any(|a| a == "--no-boot" || a == "--attach");
    // When we BOOT a headless engine with a baked char (not --full, not --attach), the
    // engine reaches READY with all match content loaded but parked — the `s` start-match
    // is socket-driven and nobody has sent it yet. A true "quick boot" lands straight in a
    // match, so we auto-fire a single bare `s` once READY arrives (uses the baked default
    // char/stage/assist). Set below in the boot branch; None for --full / --attach.
    let mut autostart = false;
    let (reader, write_half, port, token, _guard) = if no_boot {
        let port = parse_port(args);
        let token = parse_token(args);
        eprintln!("peptide session: attaching on :{port} (waiting for engine to dial in)…");
        let (r, w) = crate::ui::await_engine(port, token.as_deref());
        (r, w, port, token.unwrap_or_default(), None)
    } else {
        let full = args.iter().any(|a| a == "--full");
        // `--char` may carry a comma-separated roster (e.g. `mario,mario`) so the auto-boot builds
        // a multi-player match directly; the patcher bakes only player 1 (first token), the rest
        // resolve at launch from the full roster the fastboot command carries.
        let bake: Option<String> = if full {
            None
        } else {
            let roster = arg_val(args, "--char").unwrap_or_else(|| crate::config::Config::load().char_name());
            Some(roster.split(',').next().unwrap_or(&roster).trim().to_string())
        };
        autostart = bake.is_some();
        match crate::ui::patch_and_launch_with_progress(None, bake.as_deref()) {
            Ok((port, token, guard)) => {
                eprintln!("peptide session: engine launched on :{port}; waiting for it to dial in…");
                match crate::ui::reawait(port, &token, 45) {
                    Some((r, w)) => (r, w, port, token, Some(guard)),
                    None => { eprintln!("peptide session: engine did not connect within 45s"); return; }
                }
            }
            Err(e) => { eprintln!("peptide session: boot failed: {e}"); return; }
        }
    };

    // Fresh log + control each run. Truncate control so stale queued commands
    // from a previous session are never replayed; we read only what's appended
    // after startup (tracked by byte offset).
    let log = match std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&logp) {
        Ok(f) => Arc::new(Mutex::new(f)),
        Err(e) => { eprintln!("peptide session: cannot open log {}: {e}", logp.display()); return; }
    };
    let _ = std::fs::write(&control, b"");
    let _ = std::fs::write(&metap, format!(
        "port={port}\ntoken={token}\ncontrol={}\nlog={}\npid={}\n",
        control.display(), logp.display(), std::process::id()));

    // Float the transparent debugger HUD over the game, tailing this session's log. Engine-
    // agnostic: the SSF2 session spawns it the same way. On by default; opt out with
    // --no-overlay or PEPTIDE_OVERLAY=0. Killed when the session ends (drop + pid watchdog).
    let _overlay = crate::overlay::spawn_for_session(&logp, args);

    // socket -> log (+ stdout); signal READY once the engine reports it. Shares the
    // reader with `serve` via pump_engine_lines; the emit closure mirrors each line
    // to the log and collects the engine's RESDIAG breadcrumbs (the failing resource
    // id) so a crash report can name what didn't load — error.log alone can't.
    let done = Arc::new(AtomicBool::new(false));
    let resdiag = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    {
        let log = Arc::clone(&log);
        let done = Arc::clone(&done);
        let resdiag = Arc::clone(&resdiag);
        thread::spawn(move || {
            pump_engine_lines(reader, &ready_tx, |raw, pretty| {
                if raw.starts_with("RESDIAG") || raw.contains("resource id:") {
                    if let Ok(mut v) = resdiag.lock() { v.push(raw.to_string()); }
                }
                slog(&log, pretty);
            });
            slog(&log, "<< [engine stream ended]");
            done.store(true, Relaxed);
        });
    }

    slog(&log, &format!("[session] dir={} port={} — waiting for engine READY…", dir.display(), port));
    // Soft READY wait: a freshly-booted engine emits READY; an attached one may
    // have sent it before we connected. Either way proceed after the wait so
    // `--attach` to a live match still works.
    let interactive = want_interactive(args);
    let how = if interactive { "type commands here, or peptide tell \"<cmd>\"" } else { "peptide tell \"<cmd>\"" };
    match ready_rx.recv_timeout(Duration::from_secs(60)) {
        Ok(()) => slog(&log, &format!("[session] engine READY — accepting commands ({how})")),
        Err(_) => slog(&log, &format!("[session] no READY within 60s — accepting commands anyway ({how}, attach mode?)")),
    }

    // Foreground REPL: also take commands typed at this terminal. They're appended to the
    // control file so they share the one dispatch path below (and `tell` keeps working).
    // This is the terminal equivalent of the GUI chat — hold focus, send, watch replies.
    if interactive {
        slog(&log, "[session] interactive — type a command and press Enter (`exit` or Ctrl-C to quit, Ctrl-D stops input)");
        spawn_stdin_feeder(control.clone(), Arc::clone(&log));
    }

    // Poll the control file for newly-appended command lines and dispatch them.
    let mut writer = write_half;

    // Quick boot: a baked-char headless boot lands straight in a match. The engine is
    // READY with all content loaded but the `s` start-match is socket-driven, so fire the
    // shared fastboot command now. The decision + command live in `fastboot` (one home for
    // CLI + GUI). --full/--attach already cleared `autostart` → bridge-only boot.
    if autostart {
        let opts = crate::fastboot::BootOptions::from_cli(args);
        if let Some(cmd) = crate::fastboot::command(crate::fastboot::Engine::Fraymakers, &opts) {
            slog(&log, &format!("[session] quick boot — auto-launching ({cmd})"));
            process_cmd(&mut writer, &cmd, &log);
        }
    }
    // Shared control-file tail loop (see `session::tail_control`). Stop when the engine
    // stream ends (the reader thread set `done`); each line is sent to the engine.
    crate::session::tail_control(
        &control,
        Duration::from_millis(50),
        || {
            if done.load(Relaxed) {
                slog(&log, "[session] engine gone; exiting");
                true
            } else {
                false
            }
        },
        |raw| {
            process_cmd(&mut writer, raw, &log);
            true
        },
    );

    // Crash triage: the engine vanished. Gather the diagnostics needed to find
    // the failing resource — the engine's error.log + the RESDIAG breadcrumbs
    // (the resource id error.log lacks) — interpret them, and write a dedicated
    // crash.log in the session dir (read it with `peptide log --crash`).
    write_crash_log(&dir, port, &resdiag, &log);
}

/// Build the session's crash.log from the engine's error.log + collected RESDIAG
/// breadcrumbs, and echo the plain-English interpretation into the session log.
fn write_crash_log(dir: &std::path::Path, port: u16, resdiag: &Arc<Mutex<Vec<String>>>, log: &Arc<Mutex<std::fs::File>>) {
    // Let the engine's crash handler finish flushing error.log after the socket EOF.
    thread::sleep(Duration::from_millis(700));
    let elog = crate::config::Config::load().fraymakers_root()
        .map(|r| r.join("error.log"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let breadcrumbs: Vec<String> = resdiag.lock().map(|v| v.clone()).unwrap_or_default();
    if elog.trim().is_empty() && breadcrumbs.is_empty() {
        slog(log, "[session] engine exited with no error.log (clean shutdown or no crash dump)");
        return;
    }
    let interp = crate::interpreter::interpret_crash(&elog, &breadcrumbs);
    let mut report = String::new();
    report.push_str(&format!("peptide session crash report  (port={port}, dir={})\n\n", dir.display()));
    if let Some(ref i) = interp {
        report.push_str("== what happened ==\n");
        report.push_str(i);
        report.push_str("\n\n");
    }
    if !breadcrumbs.is_empty() {
        report.push_str("== RESDIAG breadcrumbs (engine resource-load trail) ==\n");
        for b in &breadcrumbs { report.push_str(b); report.push('\n'); }
        report.push('\n');
    }
    report.push_str("== engine error.log ==\n");
    report.push_str(if elog.trim().is_empty() { "(empty / not found)\n" } else { &elog });
    let crashp = dir.join("crash.log");
    let _ = std::fs::write(&crashp, &report);
    slog(log, &format!("[session] CRASH — full diagnostics in {} (peptide log --crash)", crashp.display()));
    if let Some(i) = interp {
        for l in i.lines() { slog(log, &format!("[crash] {l}")); }
    }
}

/// `peptide tell [--dir D] "<command>"` — queue one command for a running session.
pub fn tell(args: &[String]) {
    let dir = session_dir(args);
    let control = dir.join("control");
    if !control.exists() {
        eprintln!("peptide tell: no session at {} — start one with `peptide session`", dir.display());
        std::process::exit(1);
    }

    // Batch mode: `tell --file <path>` queues every command in a file in one go (one per
    // line; blank lines and `#` comments are skipped). The lines share the session's single
    // dispatch path, so a file can mix engine commands, `e <hscript>`, and `seq`/`hold`
    // inputs — a whole test scenario from disk. Covers peptide todo #9 (CLI half).
    if let Some(path) = arg_val(args, "--file").or_else(|| arg_val(args, "-F")) {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => { eprintln!("peptide tell: cannot read batch file {path}: {e}"); std::process::exit(1); }
        };
        let lines: Vec<&str> = text.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        if lines.is_empty() {
            eprintln!("peptide tell: batch file {path} has no commands (blank/comment-only)");
            std::process::exit(1);
        }
        match std::fs::OpenOptions::new().append(true).open(&control) {
            Ok(mut f) => {
                for l in &lines {
                    if writeln!(f, "{l}").is_err() {
                        eprintln!("peptide tell: write failed mid-batch"); std::process::exit(1);
                    }
                }
                eprintln!("peptide tell: queued {} command(s) from {path}", lines.len());
            }
            Err(e) => { eprintln!("peptide tell: cannot write {}: {e}", control.display()); std::process::exit(1); }
        }
        return;
    }

    // The command is everything after the flags (so both quoted "spawn sandbag"
    // and bare `tell spawn sandbag` work). `--dir D` must precede the command.
    let mut i = 0;
    let mut cmd: Option<String> = None;
    while i < args.len() {
        if args[i] == "--dir" { i += 2; continue; }
        cmd = Some(args[i..].join(" "));
        break;
    }
    let cmd = match cmd {
        Some(c) if !c.trim().is_empty() => c,
        _ => { eprintln!("usage: peptide tell [--dir D] \"<command>\"  |  peptide tell --file <path>"); std::process::exit(2); }
    };
    match std::fs::OpenOptions::new().append(true).open(&control) {
        Ok(mut f) => {
            if writeln!(f, "{}", cmd.trim()).is_ok() {
                eprintln!("peptide tell: queued {:?}", cmd.trim());
            } else {
                eprintln!("peptide tell: write failed");
                std::process::exit(1);
            }
        }
        Err(e) => { eprintln!("peptide tell: cannot write {}: {e}", control.display()); std::process::exit(1); }
    }
}

/// `peptide log [--dir D] [-n N] [--follow] [--crash]` — print the session's
/// engine output, or the crash report (`--crash`).
pub fn log(args: &[String]) {
    let dir = session_dir(args);
    if args.iter().any(|a| a == "--crash") {
        let crashp = dir.join("crash.log");
        match std::fs::read_to_string(&crashp) {
            Ok(c) => print!("{c}"),
            Err(_) => eprintln!("peptide log: no crash report at {} (the engine didn't crash, or no session ran)", crashp.display()),
        }
        return;
    }
    let logp = dir.join("out.log");
    let follow = args.iter().any(|a| a == "--follow" || a == "-f");
    let n: usize = arg_val(args, "-n").or_else(|| arg_val(args, "--tail"))
        .and_then(|s| s.parse().ok()).unwrap_or(40);
    if !logp.exists() {
        eprintln!("peptide log: no session log at {} — start one with `peptide session`", logp.display());
        std::process::exit(1);
    }
    let content = std::fs::read_to_string(&logp).unwrap_or_default();
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    for l in &lines[start..] { println!("{l}"); }
    if follow {
        let mut offset = content.len() as u64;
        loop {
            thread::sleep(Duration::from_millis(200));
            if let Ok(mut f) = std::fs::File::open(&logp) {
                use std::io::Seek;
                if f.seek(std::io::SeekFrom::Start(offset)).is_ok() {
                    let mut chunk = String::new();
                    if let Ok(k) = f.read_to_string(&mut chunk) {
                        if k > 0 { offset += k as u64; print!("{chunk}"); let _ = std::io::stdout().flush(); }
                    }
                }
            }
        }
    }
}
