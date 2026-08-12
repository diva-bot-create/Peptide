//! ssf2_bridge — the HOST side of the SSF2 runtime bridge, mirroring the Fraymakers
//! `bridge.rs` (session / tell / log / send) but for the AVM2 engine over an ASYNC
//! TCP SOCKET. The patched SSF2 dials into our loopback server (the port is baked
//! into the bridge at patch time) and answers on `socketData` events.
//!
//! This replaced the old per-frame FileStream IPC: its synchronous reads ran every
//! frame and starved SSF2's async resource loader (breaking spawns). Event-driven
//! socket IO touches the engine ONLY when we send a command, so the loader runs
//! undisturbed — the same transport model as Fraymakers' loopback socket.
//!
//! Wire protocol (request/response, one command at a time over the persistent
//! connection): host writes "<seq>\t<verb>\t<a1>\t<a2>"; the engine replies
//! "<seq> <result>\n". The seq disambiguates; a socket EOF means SSF2 went away.

use anyhow::{bail, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Path for the per-frame jump-probe trajectory CSV. Computed at call time so it
/// resolves to the platform temp dir on both macOS (`/tmp/`) and Windows (`%TEMP%`).
/// Uses forward slashes so Flash's `FileStream` accepts the path on Windows too.
/// Where the injected per-frame animation recorder writes. The host truncates this to
/// open a recording window and reads it to close one, so no engine-side gate (and no
/// reflection, which can't reach the injected slots) is involved.
pub fn frame_trace_path() -> String {
    std::env::temp_dir()
        .join("peptide_ssf2_frames.csv")
        .to_string_lossy()
        .replace('\\', "/")
}

pub fn traj_path() -> String {
    std::env::temp_dir()
        .join("peptide_ssf2_traj.csv")
        .to_string_lossy()
        .replace('\\', "/")
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The live loopback connection to the patched SSF2 (None until it dials in / after
/// it goes away). All `request()`s share it; the Mutex also serializes them, so the
/// matchStatus poll and a console command can't interleave on the one socket.
struct SockConn {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}
static CONN: Mutex<Option<SockConn>> = Mutex::new(None);

/// Bind the loopback listener BEFORE launching SSF2 (the engine connects from its
/// document ctor, so the port must already be open). Hand the returned listener to
/// `accept_engine` after the launch.
pub fn bind(port: u16) -> Result<TcpListener> {
    Ok(TcpListener::bind(("127.0.0.1", port))?)
}

/// Accept the engine's dial-in (call AFTER launch). Stores the live connection,
/// replacing any prior one. Bounded by `secs` so a no-show can't hang the boot.
pub fn accept_engine(listener: &TcpListener, secs: u64) -> Result<()> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match listener.accept() {
            Ok((stream, _peer)) => {
                stream.set_nonblocking(false)?;
                let _ = stream.set_nodelay(true);
                let writer = stream.try_clone()?;
                *CONN.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SockConn { reader: BufReader::new(stream), writer });
                return Ok(());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline { bail!("SSF2 did not connect within {secs}s"); }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => bail!("accept failed: {e}"),
        }
    }
}

/// Drop the current connection (on disconnect / before a fresh boot).
pub fn disconnect() {
    *CONN.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Pick a loopback port for the SSF2 bridge (19000–20999), away from the Fraymakers
/// range (18000–19999). Wall-clock seeded so successive boots vary.
pub fn pick_port() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(7);
    19000 + (n % 2000) as u16
}

/// A process-unique starting seq (seeded from the wall clock; the atomic increments
/// within a process) so reply matching is robust across reconnects.
fn seq_base() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
    (nanos % 1_000_000_000) * 1000
}

fn session_dir(args: &[String]) -> PathBuf {
    arg_val(args, "--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".peptide/ssf2-session"))
}

fn arg_val(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

/// Send one command and read its reply over the socket. `command` is TAB-joined
/// "verb\ta1\ta2"; we prepend "<seq>\t" so the engine's `split("\t")` yields
/// [seq, verb, a1, a2]. The reply line is "<seq> <result>". Holds the connection
/// lock for the whole exchange (serializing all requests). A socket EOF surfaces as
/// an error AND drops the connection — that's how we learn SSF2 was closed.
pub fn request(command: &str, timeout: Duration) -> Result<String> {
    if SEQ.load(Ordering::Relaxed) == 0 { SEQ.store(seq_base().max(1), Ordering::Relaxed); }
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut guard = CONN.lock().unwrap_or_else(|e| e.into_inner());
    let conn = guard.as_mut().ok_or_else(|| anyhow::anyhow!("no SSF2 connection"))?;

    // write the command — no trailing newline: one command per send, and the engine
    // reads `bytesAvailable` per socketData event (strict request/response).
    if let Err(e) = conn.writer.write_all(format!("{seq}\t{command}").as_bytes())
        .and_then(|_| conn.writer.flush())
    {
        *guard = None;
        bail!("SSF2 write failed (connection gone): {e}");
    }

    // read reply lines until one matches our seq (or timeout / EOF).
    let _ = conn.reader.get_ref().set_read_timeout(Some(timeout));
    let prefix = format!("{seq} ");
    let deadline = Instant::now() + timeout;
    loop {
        let mut line = String::new();
        match conn.reader.read_line(&mut line) {
            Ok(0) => { *guard = None; bail!("SSF2 connection closed"); } // EOF — engine gone
            Ok(_) => {
                if let Some(rest) = line.trim_end_matches(['\r', '\n']).strip_prefix(&prefix) {
                    return Ok(rest.to_string());
                }
                // a non-matching line (stale) — keep reading until the deadline
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {
                bail!("no response for {command:?} within {timeout:?}");
            }
            Err(e) => { *guard = None; bail!("SSF2 read failed (connection gone): {e}"); }
        }
        if Instant::now() >= deadline {
            bail!("no response for {command:?} within {timeout:?}");
        }
    }
}


/// Wait until the engine is STABLY responsive — the SSF2 analogue of Fraymakers'
/// `READY` line. The per-frame reflection hook answers `PING` very early (from the
/// document ctor), but while SSF2 is still loading its boot content the engine
/// starves frames, so PINGs come back only intermittently and any command fired in
/// that window runs at a bad time and crashes the game. We therefore gate on a RUN
/// of `needed` consecutive PINGs (the streak resets on every miss): it can only
/// accumulate once the boot load is finished and frames run smoothly. This makes
/// the host "queue" the boot spawn / user commands until loading completes, instead
/// of firing them into the loading hook. Returns true on a clean streak, false on
/// overall timeout.
pub fn wait_ready(needed: u32, total: Duration) -> bool {
    let start = Instant::now();
    let deadline = start + total;
    // Minimum settle floor: SSF2's boot has lulls BETWEEN load phases where a single
    // PING (or even a quick probe) succeeds, so a streak alone can pass too early.
    // Require at least this much wall time before accepting readiness.
    let floor = Duration::from_secs(6);
    let mut streak = 0u32;
    while Instant::now() < deadline {
        // A MULTI-OP probe (3 round-trips that must ALL land) is a much stronger
        // "frames aren't being starved by a load" signal than a single PING: during
        // a load the per-frame handler can't service three requests back-to-back.
        if probe_responsive() {
            streak += 1;
            if streak >= needed && start.elapsed() >= floor { return true; }
            std::thread::sleep(Duration::from_millis(250));
        } else {
            streak = 0; // a dropped probe means the engine is still loading — restart
        }
    }
    false
}

/// One multi-op responsiveness probe: read the live match's stage data (GC → GET →
/// READ). All three must land within the short window; a load starving frames will
/// drop at least one, which is exactly the "still loading" condition we gate on.
fn probe_responsive() -> bool {
    let t = Duration::from_millis(500);
    request("GC", t).is_ok()
        && request("GET\tstageData", t).is_ok()
        && request("READ", t).is_ok()
}

/// Block until the patched engine emits its one-shot `READY` line — the SSF2 analogue of
/// Fraymakers firing READY at boot complete. The bridge injects it at
/// the boot's initial-menu entry point (boot complete; see `abc_inject::inject_ready_signal`),
/// so this is a REAL boot-complete event, not the old PING-streak/flat-floor heuristic.
/// Reads the persistent connection until a bare `READY` line arrives (accumulating across
/// read timeouts so a split line isn't dropped) or `total` elapses. Returns true on READY.
pub fn wait_for_ready(total: Duration) -> bool {
    let mut guard = CONN.lock().unwrap_or_else(|e| e.into_inner());
    let conn = match guard.as_mut() {
        Some(c) => c,
        None => return false,
    };
    let _ = conn.reader.get_ref().set_read_timeout(Some(Duration::from_millis(500)));
    let deadline = Instant::now() + total;
    let mut line = String::new();
    loop {
        if Instant::now() >= deadline {
            return false;
        }
        match conn.reader.read_line(&mut line) {
            Ok(0) => return false, // EOF — engine gone
            Ok(_) => {
                if line.trim_end_matches(['\r', '\n']) == "READY" {
                    return true;
                }
                line.clear(); // some other unsolicited line — discard, await READY
            }
            // timeout: any partial bytes stay in `line` to be completed next read
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return false, // connection gone
        }
    }
}

/// Wait for the engine to be boot-complete. Primary path: the event-driven `READY` line.
/// Fallback (only if READY never arrives — e.g. an unexpected SSF2 build where the hook
/// no-op'd): the legacy responsiveness settle, so a boot still succeeds rather than hangs.
pub fn wait_ready_signal(total: Duration) -> bool {
    if wait_for_ready(total) {
        return true;
    }
    wait_ready(10, Duration::from_secs(6))
}

/// `peptide ssf2 send "<cmd>"` — one-shot command, printing the reply. Because the
/// engine dials into ONE process's socket, the live connection lives in the
/// `session` process; a standalone `send` therefore routes through the running
/// session's control file and reads the reply back from its log (a synchronous
/// `tell`). Start a session first (`peptide ssf2 session`).
pub fn send(args: &[String]) -> Result<()> {
    let cmd = args.iter().rev().find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow::anyhow!("usage: peptide ssf2 send \"<command>\""))?;
    let dir = session_dir(args);
    let control = dir.join("control");
    let logp = dir.join("out.log");
    if !control.exists() {
        bail!("no SSF2 session running — start one with `peptide ssf2 session` (the socket bridge lives in that process)");
    }
    let before = std::fs::metadata(&logp).map(|m| m.len()).unwrap_or(0) as usize;
    {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&control)?;
        writeln!(f, "{cmd}")?;
    }
    // the session appends ">> <cmd>" then "<< <reply>"; wait for that reply line.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(80));
        if let Ok(s) = std::fs::read_to_string(&logp) {
            if s.len() > before {
                if let Some(r) = s[before..].lines().find_map(|l| l.strip_prefix("<< ")) {
                    println!("{r}");
                    return Ok(());
                }
            }
        }
    }
    bail!("no reply from the SSF2 session within 10s");
}

/// `peptide ssf2 session` — boot the patched app (echo handler for now) and run
/// the control-file command loop, mirroring each result to out.log.
pub fn session(args: &[String]) -> Result<()> {
    let dir = session_dir(args);
    std::fs::create_dir_all(&dir)?;
    let control = dir.join("control");
    let logp = dir.join("out.log");
    let metap = dir.join("meta");
    std::fs::write(&control, b"")?;
    std::fs::write(&logp, b"")?;

    // Same host-side overlay the Fraymakers session uses — one shared spawn, engine-agnostic.
    let _overlay = crate::overlay::spawn_for_session(&logp, args);

    // bind the loopback server FIRST (the engine dials in from its ctor), patch the
    // app to connect to that port, launch, then accept the connection.
    let port = pick_port();
    let listener = bind(port)?;
    disconnect(); // drop any stale connection
    // Quick boot: bake the match char + stage so SSF2 skips the disclaimer/menus and loads
    // straight toward the match (see inject_quickboot). `--full` = a normal boot (the
    // disclaimer plays and fires the event-driven READY).
    let opts = crate::fastboot::BootOptions::from_cli(args);
    let cfg = crate::config::Config::load();
    let fastboot: Option<(String, String)> = if opts.full {
        None
    } else {
        let ch = opts.char_name.clone().unwrap_or_else(|| cfg.char_name());
        if ch.trim().is_empty() { None } else { Some((ch, cfg.ssf2_stage())) }
    };
    let app = crate::ssf2::install_patched(
        port, fastboot.as_ref().map(|(c, s)| (c.as_str(), s.as_str())))?;
    let exe = crate::ssf2::ssf2_exe_path(&app);
    let mut child = std::process::Command::new(&exe)
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .spawn()?;
    std::fs::write(&metap, format!("pid={}\napp={}\nport={}\n", child.id(), app.display(), port))?;

    let slog = |msg: &str| {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&logp) {
            let _ = writeln!(f, "{msg}");
        }
        println!("{msg}");
    };
    slog(&format!("[ssf2-session] booted {} (pid {}) on :{port}; waiting for connection…", app.display(), child.id()));
    if let Err(e) = accept_engine(&listener, 30) {
        slog(&format!("[ssf2-session] {e}"));
    }

    // Readiness. Quick boot SKIPS the disclaimer, so there's no event-driven READY — use the
    // responsiveness heuristic (the boot loading settling un-starves frames). A normal boot
    // (--full) waits for the disclaimer's READY. Generous timeout for a cold boot either way.
    let ready = if fastboot.is_some() {
        wait_ready(10, Duration::from_secs(120))
    } else {
        wait_ready_signal(Duration::from_secs(120))
    };
    slog(if ready { "[ssf2-session] engine READY — peptide ssf2 tell \"<cmd>\"" }
         else { "[ssf2-session] engine never settled — accepting commands anyway" });

    // Quick boot (headless): just like the Fraymakers `session --char`, land straight in a
    // match instead of parking the bridge at the boot screen. The match-launch (SPAWN +
    // config + GO) is host-driven over the bridge. The decision + command come from the
    // shared `fastboot` module (one home for CLI + GUI); `--full` opts out via BootOptions.
    if ready {
        let opts = crate::fastboot::BootOptions::from_cli(args);
        if let Some(cmd) = crate::fastboot::command(crate::fastboot::Engine::Ssf2, &opts) {
            slog(&format!("[ssf2-session] quick boot — auto-launching ({cmd})"));
            let mut target = crate::ssf2_target::Ssf2Target::new();
            match crate::debug_target::run_command(&mut target, &cmd) {
                Ok(Some(r)) => slog(&format!("<< {r}")),
                Ok(None) => {}
                Err(e) => slog(&format!("<< quick-boot spawn failed: {e}")),
            }
        }
    }

    // ANIM state tracking, parity with the Fraymakers session: FM's engine PUSHES
    // per-frame ANIM telemetry over its socket; SSF2's bridge is synchronous RPC
    // (nothing engine-initiated), so poll player 0's animation from the same
    // single-writer loop the commands run on and log a deduped `ANIM:` line on
    // every change. A failed read (no match yet, mid-load) backs the poll off so
    // boot/load never stalls command dispatch.
    let mut last_anim = String::new();
    let mut next_poll = std::time::Instant::now();
    let anim_tick = move || {
        let now = std::time::Instant::now();
        if now < next_poll { return false; }
        let target = crate::ssf2_target::Ssf2Target::new();
        match target.current_anim(0) {
            Some(anim) => {
                next_poll = now + Duration::from_millis(300);
                if anim != last_anim {
                    last_anim = anim.clone();
                    slog(&format!("ANIM:{}", anim.to_uppercase()));
                }
            }
            None => { next_poll = now + Duration::from_secs(3); }
        }
        false
    };

    // Shared control-file tail loop (see `session::tail_control`); `exit`/`quit` kills the
    // app and stops the loop. SSF2 is synchronous RPC, so each line runs inline here.
    crate::session::tail_control(
        &control,
        Duration::from_millis(50),
        anim_tick,
        |raw| {
            if raw == "exit" || raw == "quit" {
                slog("[ssf2-session] exit");
                let _ = child.kill(); // cross-platform (was pkill -f SSF2-patched)
                return false;
            }
            slog(&format!(">> {raw}"));
            let mut target = crate::ssf2_target::Ssf2Target::new();
            match crate::debug_target::run_command(&mut target, raw) {
                Ok(Some(r)) => slog(&format!("<< {r}")),
                Ok(None) => {}
                Err(e) => slog(&format!("<< ERR: {e}")),
            }
            true
        },
    );
    Ok(())
}

/// `peptide ssf2 jumpcapture <char>` — drive a live SSF2 jump and capture the
/// per-frame trajectory (written by the injected probe). Requires a running
/// `peptide ssf2 session`. Steps: SPAWN the char, wait for the match, navigate
/// to Characters[0], read its CharacterStats.JumpSpeed, set YSpeed=-JumpSpeed to
/// launch a jump, then read the trajectory CSV the probe accumulated.
pub fn jumpcapture(args: &[String]) -> Result<()> {
    let ch = args.iter().rev().find(|a| !a.starts_with("--")).cloned().unwrap_or_else(|| "mario".into());
    let t = Duration::from_secs(5);
    let nav = |path: &[&str]| -> Result<String> {
        // path like ["GC","GET stageData","GET Characters","IDX 0"]
        let mut last = String::new();
        for step in path { last = request(&step.replace(' ', "\t"), t)?; }
        Ok(last)
    };
    let stage = args.iter().position(|a| a == "--stage").and_then(|i| args.get(i+1)).cloned().unwrap_or_else(|| "battlefield".into());
    // 1. SPAWN: build the Game (sets currentGame) + queue stage/char + kick the async resource load.
    println!("SPAWN {ch} on {stage} → {}", request(&format!("SPAWN\t{ch}\t{stage}"), Duration::from_secs(8))?);
    // 2. wait for the async resource load to finish.
    let mut loaded = false;
    for _ in 0..80 {
        if request("LOADED", t).map(|r| r == "true").unwrap_or(false) { loaded = true; break; }
        std::thread::sleep(Duration::from_millis(400));
    }
    println!("resources fully loaded: {loaded}");
    // 3. GO: start the match — spawns the stage next frame.
    println!("GO → {}", request("GO", Duration::from_secs(8))?);
    // wait for the match to come up (stageData non-null)
    let mut up = false;
    for _ in 0..40 {
        let _ = request("GC", t);
        let _ = request("GET\tstageData", t);
        if request("READ", t).map(|r| r != "null").unwrap_or(false) { up = true; break; }
        std::thread::sleep(Duration::from_millis(300));
    }
    if !up { bail!("match did not start (stageData stayed null) — SPAWN may need more setup"); }
    println!("match is live");
    // navigate to Characters[0]
    nav(&["GC", "GET stageData", "GET Characters", "IDX 0"])?;
    // read JumpSpeed: cur=char → CharacterStats → JumpSpeed
    nav(&["GET CharacterStats", "GET JumpSpeed"])?;
    let js = request("READ", t)?;
    println!("character JumpSpeed = {js}");
    let jsv: f64 = js.trim().parse().unwrap_or(15.0);
    let tp = traj_path();
    std::fs::write(&tp, b"")?;
    nav(&["GC", "GET stageData", "GET Characters", "IDX 0"])?;
    request(&format!("SETP\tYSpeed\t{}", -jsv), t)?;
    println!("launched jump (YSpeed = {}), capturing…", -jsv);
    std::thread::sleep(Duration::from_millis(1500));
    let traj = std::fs::read_to_string(&tp).unwrap_or_default();
    println!("=== SSF2 jump trajectory (t,X,Y,YSpeed) ===\n{traj}");
    // find apex (min Y, y is down-positive)
    let mut min_y = f64::INFINITY; let mut ground = f64::NEG_INFINITY;
    for line in traj.lines() {
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() >= 3 { if let Ok(y) = cols[2].parse::<f64>() { min_y = min_y.min(y); ground = ground.max(y); } }
    }
    if min_y.is_finite() {
        println!("SSF2 apex displacement = {:.1} px (ground {:.1} → apex {:.1})", ground - min_y, ground, min_y);
    }
    Ok(())
}

/// `peptide ssf2 tell "<cmd>"` — append a command to a running session's control file.
pub fn tell(args: &[String]) -> Result<()> {
    let dir = session_dir(args);
    let cmd = args.iter().rev().find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow::anyhow!("usage: peptide ssf2 tell \"<command>\""))?;
    let control = dir.join("control");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&control)?;
    writeln!(f, "{cmd}")?;
    Ok(())
}

/// `peptide ssf2 log` — print the session output log.
pub fn log(args: &[String]) -> Result<()> {
    let dir = session_dir(args);
    let logp = dir.join("out.log");
    let n: usize = arg_val(args, "-n").and_then(|s| s.parse().ok()).unwrap_or(40);
    let s = std::fs::read_to_string(&logp).unwrap_or_default();
    let lines: Vec<&str> = s.lines().collect();
    for l in lines.iter().rev().take(n).rev() { println!("{l}"); }
    let _ = Path::new(&logp);
    Ok(())
}
