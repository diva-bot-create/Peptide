//! ui — the friendly full-screen console for driving Fraymakers (the default mode of
//! the `peptide` binary; `ui::launch` boots the engine and runs it). Color-coded
//! scrollback, command palette (Tab), history (up/down), scroll (PgUp/PgDn), help (F1).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::interpreter::{translate, Translated};

fn env_char() -> Option<String> {
    std::env::var("FRAY_CHAR").ok().filter(|s| !s.is_empty())
}

// ─────────────────────────── connection (shared) ───────────────────────────

/// We're the TCP server (the injected engine code is the client). Bind localhost,
/// wait for the engine to dial in, optionally check the AUTH token.
pub fn await_engine(port: u16, token: Option<&str>) -> (BufReader<TcpStream>, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap_or_else(|e| {
        eprintln!("peptide-ui: cannot bind 127.0.0.1:{port}: {e}");
        std::process::exit(1);
    });
    eprintln!("peptide-ui: waiting for Fraymakers to connect on :{port}…");
    loop {
        let (stream, _peer) = listener.accept().expect("accept failed");
        let writer = stream.try_clone().expect("clone");
        let mut reader = BufReader::new(stream);
        match token {
            None => return (reader, writer),
            Some(expected) => {
                let mut first = String::new();
                if reader.read_line(&mut first).is_ok()
                    && first.trim().strip_prefix("AUTH ").map(str::trim) == Some(expected)
                {
                    return (reader, writer);
                }
                eprintln!("peptide-ui: rejected unauthenticated peer; still listening");
            }
        }
    }
}

/// Like `await_engine`, but bounded: bind `port` and wait at most `secs` for an engine to
/// dial in (authenticating with `token`). Returns the live connection, or None on timeout
/// — without killing anything. Used by the GUI to "TCP back into" a session and to bound
/// the wait after a boot, so the page can drive its reconnect modal instead of blocking.
pub fn reawait(port: u16, token: &str, secs: u64) -> Option<(BufReader<TcpStream>, TcpStream)> {
    let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
    listener.set_nonblocking(true).ok()?;
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _peer)) => {
                stream.set_nonblocking(false).ok()?;
                let writer = stream.try_clone().ok()?;
                let mut reader = BufReader::new(stream);
                // bound the auth read so a silent peer can't hang us past the deadline
                let _ = reader.get_ref().set_read_timeout(Some(Duration::from_secs(2)));
                let mut first = String::new();
                let ok = reader.read_line(&mut first).is_ok()
                    && first.trim().strip_prefix("AUTH ").map(str::trim) == Some(token);
                let _ = reader.get_ref().set_read_timeout(None);
                if ok {
                    return Some((reader, writer));
                }
                // wrong/no token — drop this peer, keep listening until the deadline
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    None
}

// ─────────────────────────── cross-platform launch ─────────────────────────

/// The env var that points the dynamic loader at the install dir (so the engine
/// finds its bundled libs / DLLs). The engine binary NAME is resolved separately
/// through `Config::engine_name` (env → config → per-OS default) since it differs
/// per install — Steam ships it as `Fraymakers.exe` on Windows, `hl` elsewhere.
fn engine_lib_var() -> Option<&'static str> {
    #[cfg(target_os = "windows")]
    { None } // current_dir is enough for DLL resolution on Windows
    #[cfg(target_os = "macos")]
    { Some("DYLD_LIBRARY_PATH") }
    #[cfg(all(unix, not(target_os = "macos")))]
    { Some("LD_LIBRARY_PATH") }
}

// The per-OS default Fraymakers install path now lives in
// `platform::default_fraymakers_root`, read through `config::Config`.

fn pseudo_seed() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(12345);
    n ^ (std::process::id().wrapping_mul(2654435761))
}

/// Removes the throwaway files + kills the engine when the UI exits (including panic).
pub struct Cleanup {
    conn: PathBuf,
    appid: PathBuf,
    engine: Option<Child>,
}
impl Cleanup {
    /// A Cleanup that owns just a spawned engine process — no throwaway conn/appid
    /// files. Used by the SSF2 GUI boot, whose teardown is simply killing the app,
    /// so it reuses the same window-close disposal path as the Fraymakers engine.
    pub fn for_engine(engine: Child) -> Cleanup {
        Cleanup { conn: PathBuf::new(), appid: PathBuf::new(), engine: Some(engine) }
    }

    /// Kill the engine + remove the throwaway files. Idempotent. The GUI calls this on
    /// window-close because its event loop exits the process (skipping Drop).
    pub fn dispose(&mut self) {
        if let Some(mut c) = self.engine.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_file(&self.conn);
        let _ = std::fs::remove_file(&self.appid);
    }
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// Self-contained, cross-platform entry point: locate Fraymakers, patch a throwaway
/// engine copy (via the sibling `peptide` binary), boot it, and run the console.
/// hlboot-sdl.dat is never modified. Tested on macOS; Windows/Linux use cfg! defaults
/// and honor FRAY_DIR / FRAY_ENGINE / FRAY_BOOT / FRAY_CHAR overrides.
pub fn boot() -> std::io::Result<(BufReader<TcpStream>, TcpStream, u16, String, Cleanup)> {
    let (port, token, guard) = patch_and_launch()?;
    // bind + wait for the engine to dial in, then hand the live connection back.
    match reawait(port, &token, 45) {
        Some((reader, writer)) => Ok((reader, writer, port, token, guard)),
        None => {
            // guard drops here -> the engine that never connected is killed + cleaned up
            Err(io_err("Fraymakers did not connect (timed out waiting on the engine)"))
        }
    }
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::other(msg.to_string())
}

/// Patch a throwaway engine copy and launch it, returning the chosen port + token and a
/// Cleanup guard owning the process. Does NOT wait for the engine to connect — the caller
/// drives that via `reawait`, so a GUI can show progress while the engine loads. Returns
/// Err (instead of exiting) so callers can recover with a boot prompt. hlboot-sdl.dat is
/// never modified.
pub fn patch_and_launch() -> std::io::Result<(u16, String, Cleanup)> {
    // Terminal/CLI keeps the headless fast-boot (bake the config char).
    let c = crate::config::Config::load().char_name();
    patch_and_launch_with_progress(None, Some(&c), None)
}

/// Like `patch_and_launch`, but streams the patcher's manifest-preflight progress to
/// `on_progress` (done, total, label) so a UI can render a live patching bar. The
/// patcher emits `@@PFP <done> <total> <label>` lines (see manifest::PROGRESS_PREFIX)
/// on stderr when run non-interactively; we parse them here. A `None` callback keeps
/// the old silent behavior.
pub fn patch_and_launch_with_progress(
    // Called synchronously on THIS thread as each preflight line is read, so it
    // needs no Send/Sync bound (the GUI's wry proxy isn't Send).
    on_progress: Option<&dyn Fn(usize, usize, &str)>,
    // A character to bake for HEADLESS fast-boot (skip the Title; a bare `spawn`
    // launches it) — used by the CLI/terminal. `None` = a FULL boot (Title + UGC
    // load) that is just a live TCP bridge: the GUI uses this for BOTH Regular and
    // Quick boot, and Quick then sends an explicit `spawn <char>` from the page once
    // READY. (Full boot also avoids the headless filtered-load null-namespace crash.)
    bake_char: Option<&str>,
    // A stage to bake alongside the char. The patcher's custom-stage self-bootstrap keys off
    // the BAKED name (it stats custom/<stage>/<stage>.fra at patch time), so a converted stage
    // only ever loads when it's baked here — naming it at runtime in `spawn <char> <stage>`
    // leaves it unregistered and `setupStage` gets null. `None` = the configured stage.
    bake_stage: Option<&str>,
) -> std::io::Result<(u16, String, Cleanup)> {
    // Read launch settings through the persisted config (env vars still win —
    // see Config's resolver methods — then the saved config, then defaults).
    let cfg = crate::config::Config::load();
    let fray_dir: PathBuf = cfg.fraymakers_root()
        .ok_or_else(|| io_err("could not determine the Fraymakers install path (set it in Setup or FRAY_DIR)"))?;
    let boot_name = cfg.boot_name();
    let boot = fray_dir.join(&boot_name);
    let conn = fray_dir.join("_conn.dat");
    let appid = fray_dir.join("steam_appid.txt");
    if !boot.exists() {
        return Err(io_err(&format!("{} not found (set the Fraymakers install in Setup or FRAY_DIR)", boot.display())));
    }

    // A baked (headless) char should be published to a `.fra` IF it's a custom char. A base/built-in
    // char (e.g. commandervideo) has no custom/<c>.fra — the engine already has it, and the `s`
    // handler's base-char skip resolves it via public::. So only error if it looks custom but is
    // missing; for an absent file we warn and continue (the base-char path handles it).
    if let Some(c) = bake_char {
        // Informational only: the patcher (connect_edit) self-detects base-vs-custom by stat'ing
        // custom/<c>/<c>.fra itself, so there's no flag to pass — we just warn if it looks absent.
        let fra = fray_dir.join("custom").join(c).join(format!("{c}.fra"));
        if !fra.exists() {
            eprintln!("peptide: no custom build for {c} ({}); treating it as a built-in/base char.",
                fra.display());
        }
    }
    let stage = bake_stage.map(str::to_string).unwrap_or_else(|| cfg.stage());
    let assist = cfg.assist();

    let seed = pseudo_seed();
    let port: u16 = 18000 + (seed % 2000) as u16;
    let token = format!("fray-{seed:x}");

    // the patcher is THIS same binary: `peptide <boot> <out> connect …` (arg1 is a path,
    // so it routes to the bytecode patcher, not back to a runtime mode).
    let patcher = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("peptide"));

    // patch a throwaway _conn.dat. Passing a char triggers headless fast-boot; with
    // none, the engine does a full Title/UGC boot and is just a TCP bridge.
    std::fs::write(&appid, b"1420350")?;
    // Capture stderr so we can both (a) parse the preflight progress lines for the UI
    // bar and (b) surface the doctor checklist on a patch failure (a new Fraymakers
    // build that moved a critical symbol aborts here with a precise reason).
    // connect <port> <token> [char stage assist] — the char/stage/assist trio is
    // included ONLY for a headless bake; omitted for a full-boot TCP bridge.
    let mut args: Vec<std::ffi::OsString> = vec![
        boot.as_os_str().to_os_string(), conn.as_os_str().to_os_string(),
        "connect".into(), port.to_string().into(), token.clone().into(),
    ];
    if let Some(c) = bake_char {
        args.push(c.into());
        args.push(stage.clone().into());
        args.push(assist.clone().into());
    }
    let mut child = Command::new(&patcher)
        .args(&args)
        .stdout(Stdio::null()).stderr(Stdio::piped())
        .spawn()
        .map_err(|e| io_err(&format!("failed to spawn the patcher {}: {e}", patcher.display())))?;
    let mut tail: Vec<String> = Vec::new(); // last lines, for an error message
    if let Some(err) = child.stderr.take() {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix(crate::manifest::PROGRESS_PREFIX) {
                // "@@PFP <done> <total> <label>"
                let mut it = rest.trim().splitn(3, ' ');
                if let (Some(d), Some(t)) = (it.next(), it.next()) {
                    if let (Ok(done), Ok(total)) = (d.parse::<usize>(), t.parse::<usize>()) {
                        if let Some(cb) = on_progress {
                            cb(done, total, it.next().unwrap_or("").trim());
                        }
                    }
                }
                continue;
            }
            if line.starts_with(crate::manifest::RESULT_PREFIX) { continue; }
            tail.push(line);
            if tail.len() > 40 { tail.remove(0); }
        }
    }
    let patch_ok = child.wait().map(|s| s.success()).unwrap_or(false);
    if !patch_ok {
        let _ = std::fs::remove_file(&appid);
        let why = tail.iter().rev().find(|l| l.contains("preflight") || l.contains("MISSING") || l.contains("Error"))
            .cloned()
            .unwrap_or_else(|| format!("is {} present?", patcher.display()));
        return Err(io_err(&format!("failed to patch the engine — {why}")));
    }
    let _ = std::fs::remove_file(fray_dir.join("error.log"));

    // boot the patched engine.
    let engine_bin = cfg.engine_name();
    let lib_var = engine_lib_var();
    let engine_path = fray_dir.join(&engine_bin);
    let mut cmd = Command::new(&engine_path);
    cmd.arg("_conn.dat").current_dir(&fray_dir).stdout(Stdio::null()).stderr(Stdio::null());
    if let Some(var) = lib_var {
        cmd.env(var, ".");
    }
    let engine = match cmd.spawn() {
        Ok(child) => Some(child),
        Err(_) => {
            let _ = std::fs::remove_file(&conn);
            let _ = std::fs::remove_file(&appid);
            return Err(io_err(&format!("failed to launch the engine ({})", engine_path.display())));
        }
    };
    Ok((port, token, Cleanup { conn, appid, engine }))
}

/// Boot the engine + console UI (terminal). The GUI uses `boot()` directly.
pub fn launch() -> std::io::Result<()> {
    let (reader, writer, port, _token, _guard) = boot()?;
    run_with(reader, writer, port) // owns the terminal until the user quits; _guard cleans up
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Sent,
    Result,
    Error,
    Anim,
    Move,
    Physics,
    State,
    Launched,
    System,
}

struct Out {
    text: String,
    kind: Kind,
}

struct App {
    lines: Vec<Out>,
    input: String,
    history: Vec<String>,
    hist_pos: Option<usize>,
    scroll_back: u16, // how many lines scrolled up from the bottom
    show_help: bool,
    show_palette: bool,
    palette_sel: usize,
    port: u16,
    char_name: Option<String>,
    ready: bool,
}

impl App {
    fn new(port: u16) -> Self {
        let mut app = App {
            lines: Vec::new(),
            input: String::new(),
            history: Vec::new(),
            hist_pos: None,
            scroll_back: 0,
            show_help: false,
            show_palette: false,
            palette_sel: 0,
            port,
            char_name: env_char(),
            ready: false,
        };
        app.sys("Welcome to Peptide. The engine is loading…");
        app.sys("Type a command and press Enter. Tab = command palette · F1 = help.");
        app
    }

    fn push(&mut self, text: String, kind: Kind) {
        self.lines.push(Out { text, kind });
        self.scroll_back = 0; // jump back to live tail on new output
    }
    fn sys(&mut self, s: &str) {
        self.push(s.to_string(), Kind::System);
    }

    /// Classify + record a line that came FROM the engine.
    fn engine_line(&mut self, line: &str) {
        if line.contains("READY") {
            self.ready = true;
            self.sys("Engine ready. Try: spawn sandbag");
        }
        if let Some(rest) = line.strip_prefix("LAUNCHED") {
            // pull the char id (private::<id>.<id>) for the status bar
            if let Some(tok) = rest.split_whitespace().next() {
                let id = tok.rsplit("::").next().unwrap_or(tok);
                let id = id.split('.').next().unwrap_or(id);
                if !id.is_empty() {
                    self.char_name = Some(id.to_string());
                }
            }
        }
        let kind = if line.contains("ERR") {
            Kind::Error
        } else if line.starts_with("LAUNCHED") {
            Kind::Launched
        } else if line.starts_with("E:") {
            Kind::Result
        } else if line.starts_with("ANIM:") {
            Kind::Anim
        } else if line.starts_with("M:") {
            Kind::Move
        } else if line.starts_with("P:") {
            Kind::Physics
        } else if line.starts_with("T:") {
            Kind::State
        } else {
            Kind::System
        };
        // Trim the noisy "E:" prefix for display (keep ERR visible).
        let shown = line.strip_prefix("E:").unwrap_or(line).to_string();
        self.push(shown, kind);
    }
}

const PALETTE: &[(&str, &str)] = &[
    ("spawn sandbag", "start a match with sandbag"),
    ("state", "current state of player 0"),
    ("physics", "position / velocity / damage"),
    ("anim", "current animation + frame"),
    ("move jab", "drive a move (jab, tilt_up, special_neutral, …)"),
    ("play", "resume animation playback"),
    ("step", "advance one animation frame"),
    ("match.getCharacters()", "list the characters in the match (hscript)"),
    ("p0.getStateName()", "call any method on player 0 (hscript)"),
    ("Engine.log(\"hi\")", "log to the in-game console"),
    ("exit", "shut the engine down"),
];

fn run_with(reader: BufReader<TcpStream>, writer: TcpStream, port: u16) -> std::io::Result<()> {

    // socket -> channel (raw lines); dedup repeated ANIM frames.
    let (tx, rx): (mpsc::Sender<String>, Receiver<String>) = mpsc::channel();
    let mut byte_reader = reader;
    thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut one = [0u8; 1];
        let mut last_anim = String::new();
        loop {
            match byte_reader.read(&mut one) {
                Ok(0) => {
                    let _ = tx.send("\u{1}engine closed the connection".to_string());
                    break;
                }
                Ok(_) => {
                    if one[0] == b'\n' {
                        let line = String::from_utf8_lossy(&buf).trim_end_matches('\r').to_string();
                        buf.clear();
                        if let Some(a) = line.strip_prefix("ANIM:") {
                            if a == last_anim {
                                continue;
                            }
                            last_anim = a.to_string();
                        }
                        if tx.send(line).is_err() {
                            break;
                        }
                    } else {
                        buf.push(one[0]);
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });

    let mut term = ratatui::init();
    let mut app = App::new(port);
    let mut writer = writer;
    let res = event_loop(&mut term, &mut app, &rx, &mut writer);
    ratatui::restore();
    // be polite: ask the engine to exit cleanly on the way out
    let _ = writer.write_all(b"x\n");
    res
}

fn event_loop(
    term: &mut ratatui::DefaultTerminal,
    app: &mut App,
    rx: &Receiver<String>,
    writer: &mut TcpStream,
) -> std::io::Result<()> {
    loop {
        // drain engine output
        while let Ok(line) = rx.try_recv() {
            if let Some(sys) = line.strip_prefix('\u{1}') {
                app.sys(sys);
            } else {
                app.engine_line(&line);
            }
        }

        term.draw(|f| draw(f, app))?;

        if event::poll(Duration::from_millis(40))? {
            if let Event::Key(k) = event::read()? {
                if k.kind != KeyEventKind::Press {
                    continue;
                }
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Char('c') if ctrl => return Ok(()),
                    KeyCode::Char('d') if ctrl => return Ok(()),
                    KeyCode::Esc => {
                        if app.show_help || app.show_palette {
                            app.show_help = false;
                            app.show_palette = false;
                        } else {
                            return Ok(());
                        }
                    }
                    KeyCode::F(1) => app.show_help = !app.show_help,
                    KeyCode::Tab => {
                        app.show_palette = !app.show_palette;
                        app.palette_sel = 0;
                    }
                    KeyCode::Up if app.show_palette => {
                        app.palette_sel = app.palette_sel.saturating_sub(1);
                    }
                    KeyCode::Down if app.show_palette => {
                        app.palette_sel = (app.palette_sel + 1).min(PALETTE.len() - 1);
                    }
                    KeyCode::Enter if app.show_palette => {
                        app.input = PALETTE[app.palette_sel].0.to_string();
                        app.show_palette = false;
                    }
                    KeyCode::Up => history_prev(app),
                    KeyCode::Down => history_next(app),
                    KeyCode::PageUp => app.scroll_back = app.scroll_back.saturating_add(8),
                    KeyCode::PageDown => app.scroll_back = app.scroll_back.saturating_sub(8),
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    KeyCode::Enter => submit(app, writer)?,
                    _ => {}
                }
            }
        }
    }
}

fn history_prev(app: &mut App) {
    if app.history.is_empty() {
        return;
    }
    let pos = match app.hist_pos {
        None => app.history.len() - 1,
        Some(0) => 0,
        Some(p) => p - 1,
    };
    app.hist_pos = Some(pos);
    app.input = app.history[pos].clone();
}

fn history_next(app: &mut App) {
    match app.hist_pos {
        Some(p) if p + 1 < app.history.len() => {
            app.hist_pos = Some(p + 1);
            app.input = app.history[p + 1].clone();
        }
        _ => {
            app.hist_pos = None;
            app.input.clear();
        }
    }
}

fn submit(app: &mut App, writer: &mut TcpStream) -> std::io::Result<()> {
    let line = app.input.trim().to_string();
    app.input.clear();
    app.hist_pos = None;
    if line.is_empty() {
        return Ok(());
    }
    app.history.push(line.clone());
    app.push(format!("› {line}"), Kind::Sent);

    match translate(&line) {
        Translated::Wire(w) => send(writer, &w)?,
        Translated::Client(text) => {
            for l in text.lines() {
                app.sys(l);
            }
        }
    }
    Ok(())
}

fn send(writer: &mut TcpStream, wire: &str) -> std::io::Result<()> {
    if wire.is_empty() {
        return Ok(());
    }
    writer.write_all(format!("{wire}\n").as_bytes())?;
    writer.flush()
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::vertical([
        Constraint::Length(1), // title
        Constraint::Min(3),    // scrollback
        Constraint::Length(3), // input
        Constraint::Length(1), // hints
    ])
    .split(f.area());

    draw_title(f, chunks[0], app);
    draw_scrollback(f, chunks[1], app);
    draw_input(f, chunks[2], app);
    draw_hints(f, chunks[3], app);

    if app.show_palette {
        draw_palette(f, app);
    }
    if app.show_help {
        draw_help(f);
    }
}

fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let dot = if app.ready { "●".green() } else { "◌".yellow() };
    let status = if app.ready { "ready" } else { "loading…" };
    let chr = app.char_name.clone().unwrap_or_else(|| "—".into());
    let line = Line::from(vec![
        " Peptide ".bold().on_dark_gray(),
        "  ".into(),
        dot,
        format!(" {status}").into(),
        "   char: ".dark_gray(),
        chr.cyan(),
        "   port: ".dark_gray(),
        format!(":{}", app.port).into(),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_scrollback(f: &mut Frame, area: Rect, app: &App) {
    let inner_h = area.height.saturating_sub(2) as usize; // minus borders
    let mut lines: Vec<Line> = Vec::with_capacity(app.lines.len());
    for o in &app.lines {
        let style = match o.kind {
            Kind::Sent => Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            Kind::Result => Style::default().fg(Color::Green),
            Kind::Error => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            Kind::Anim => Style::default().fg(Color::Yellow),
            Kind::Move => Style::default().fg(Color::LightMagenta),
            Kind::Physics => Style::default().fg(Color::LightBlue),
            Kind::State => Style::default().fg(Color::LightCyan),
            Kind::Launched => Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            Kind::System => Style::default().fg(Color::DarkGray),
        };
        let prefix = match o.kind {
            Kind::Sent => "",
            Kind::System => "  ",
            _ => "« ",
        };
        lines.push(Line::styled(format!("{prefix}{}", o.text), style));
    }
    let total = lines.len();
    // bottom-anchored scroll: show the tail unless the user scrolled up
    let max_top = total.saturating_sub(inner_h);
    let top = max_top.saturating_sub(app.scroll_back as usize);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(" output ".dark_gray());
    let para = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((top as u16, 0));
    f.render_widget(para, area);
}

fn draw_input(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(" command ".blue().bold());
    let text = Line::from(vec!["❯ ".blue().bold(), app.input.clone().white()]);
    f.render_widget(Paragraph::new(text).block(block), area);
    // cursor after the prompt + input
    let cx = area.x + 4 + app.input.chars().count() as u16;
    let cy = area.y + 1;
    f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(2)), cy));
}

fn draw_hints(f: &mut Frame, area: Rect, _app: &App) {
    let hint = Line::from(vec![
        " Tab ".on_dark_gray(),
        " palette  ".dark_gray(),
        " ↑↓ ".on_dark_gray(),
        " history  ".dark_gray(),
        " PgUp/PgDn ".on_dark_gray(),
        " scroll  ".dark_gray(),
        " F1 ".on_dark_gray(),
        " help  ".dark_gray(),
        " Esc/Ctrl+C ".on_dark_gray(),
        " quit".dark_gray(),
    ]);
    f.render_widget(Paragraph::new(hint), area);
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_palette(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 62, (PALETTE.len() + 2) as u16);
    f.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (i, (cmd, desc)) in PALETTE.iter().enumerate() {
        let sel = i == app.palette_sel;
        let marker = if sel { "❯ " } else { "  " };
        let style = if sel {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{marker}{cmd:<26}"), style),
            Span::styled(format!(" {desc}"), Style::default().fg(Color::DarkGray)),
        ]));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" command palette · Enter to pick · Esc to close ".cyan());
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 70, 18);
    f.render_widget(Clear, area);
    let text = vec![
        Line::from("Peptide drives a running Fraymakers from this console.".bold()),
        Line::from(""),
        Line::from(vec!["spawn <char> ".cyan(), "start a match (e.g. spawn sandbag)".dark_gray()]),
        Line::from(vec!["state / physics / anim ".cyan(), "read player 0".dark_gray()]),
        Line::from(vec!["move <name> ".cyan(), "jab, tilt_up, strong_forward, special_neutral, aerial_up, grab".dark_gray()]),
        Line::from(vec!["play / step ".cyan(), "resume / advance one animation frame".dark_gray()]),
        Line::from(vec!["exit ".cyan(), "shut the engine down".dark_gray()]),
        Line::from(""),
        Line::from("Anything else is run as hscript in the engine:".bold()),
        Line::from(vec!["  match.getCharacters() ".green(), "list the characters".dark_gray()]),
        Line::from(vec!["  p0.getStateName() ".green(), "call any method on player 0".dark_gray()]),
        Line::from(vec!["  p0.body.x ".green(), "read any field".dark_gray()]),
        Line::from(vec!["  Engine.log(\"hi\") ".green(), "log to the in-game console".dark_gray()]),
        Line::from(""),
        Line::from("Errors never crash the engine — they show here and in Engine.log.".italic()),
        Line::from("Press F1 or Esc to close.".dark_gray()),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Green))
        .title(" help ".green().bold());
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), area);
}

#[cfg(test)]
mod demo {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn dump(buf: &ratatui::buffer::Buffer) -> String {
        let a = buf.area;
        let mut s = String::new();
        for y in 0..a.height {
            for x in 0..a.width {
                let sym = buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" ");
                s.push_str(if sym.is_empty() { " " } else { sym });
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn render_demo() {
        let mut app = App::new(19794);
        app.ready = true;
        app.char_name = Some("sandbag".into());
        app.push("› spawn sandbag".into(), Kind::Sent);
        app.engine_line("LAUNCHED private::sandbag.sandbag public::thespire.thespire");
        app.engine_line("ANIM:INTRO");
        app.engine_line("ANIM:STAND");
        app.push("› physics".into(), Kind::Sent);
        app.engine_line("E:P: x=-130 y=73 vx=0 vy=0 dmg=0");
        app.push("› move jab".into(), Kind::Sent);
        app.engine_line("ANIM:JAB");
        app.engine_line("E:M:OK");
        app.push("› match.getCharacters()".into(), Kind::Sent);
        app.engine_line("E:[fraymakers.entity.FraymakersCharacter]");
        app.push("› )(oops".into(), Kind::Sent);
        app.engine_line("E:ERR: hscript:1: Unexpected token: \")\"");
        app.input = "p0.getStateName()".into();

        let mut term = Terminal::new(TestBackend::new(92, 24)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        println!("\n===== MAIN VIEW =====\n{}", dump(term.backend().buffer()));

        let mut p = App::new(19794);
        p.ready = true; p.char_name = Some("sandbag".into());
        p.show_palette = true; p.palette_sel = 4;
        term.draw(|f| draw(f, &p)).unwrap();
        println!("\n===== Tab: COMMAND PALETTE =====\n{}", dump(term.backend().buffer()));

        let mut h = App::new(19794);
        h.ready = true; h.show_help = true;
        term.draw(|f| draw(f, &h)).unwrap();
        println!("\n===== F1: HELP =====\n{}", dump(term.backend().buffer()));
    }
}
