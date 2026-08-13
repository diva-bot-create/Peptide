//! The single source of truth for every Fraymakers engine symbol Peptide
//! depends on — the bytecode patcher (critical entries) plus the few read-only
//! features that read engine classes by name (e.g. the stage-parallax preview;
//! non-critical, a miss only degrades the feature).
//!
//! WHY THIS FILE EXISTS
//! --------------------
//! Fraymakers ships as HashLink bytecode. Every recompile (i.e. every Fraymakers
//! update) renumbers function indices (`findex`), field slots, and type indices.
//! Peptide therefore resolves engine symbols BY NAME at patch time — a name
//! lookup survives a recompile as long as the symbol still exists, where a pinned
//! integer index silently points at the wrong function.
//!
//! This table lists what the patcher depends on so that a brand-new Fraymakers
//! build can be triaged in ONE command:
//!
//!     peptide <new-fraymakers.dat> _ doctor
//!
//! instead of an archaeology dig. The same table drives the live preflight that
//! runs at the top of every real patch (`run_preflight`) — it renders a progress
//! bar + checklist and ABORTS before mutating a single opcode if a critical
//! symbol has gone missing, so a layout change fails loudly instead of corrupting
//! the engine.
//!
//! THE CANONICAL ENGINE-SURFACE REFERENCE
//! --------------------------------------
//! The prose docs stay deliberately high-level about the engine, so THIS TABLE is
//! the canonical, in-repo map of the engine surface the harness actually touches.
//! If you're trying to understand which engine functions / fields / types Peptide
//! reaches into and why, read `MANIFEST` below top-to-bottom: it is grouped by
//! subsystem (`socket-bridge`, `boot`, `content`, `hscript-eval`, `line-cmd`,
//! `move-dispatch`, `telemetry`, `console`), and every entry carries a `why` string
//! describing its role. The `connect_edit` patcher in `main.rs` consumes exactly
//! these symbols (via the `find_fn` / `find_native` / `find_type` / `find_field`
//! resolvers), so the table doubles as the index into the patch logic.
//!
//! THE RULE FOR FUTURE WORK
//! ------------------------
//! Any NEW engine symbol a patch comes to depend on MUST be added here. Keep this
//! list in step with what `connect_edit` actually resolves. See
//! docs/PEPTIDE_DESIGN.md, "Version resilience — surviving Fraymakers updates", for
//! the full philosophy, and the `peptide` read-only inspection subcommands
//! (`doctor` / `inspect` / `fnsof` / `typefields` / `fninfo` / `dis` / `callers` /
//! `strgrep` / `whoref`) for re-resolving anything that moves in a new build.

use std::io::{IsTerminal, Write};

/// What kind of symbol an entry resolves to. Mirrors the four read-only resolver
/// helpers in main.rs (`find_fn` / `find_native` / `find_type` / `find_field`),
/// so the doctor uses the exact same lookup logic the real patch does — no drift.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Method/function resolved by (name, parent type).
    Fn,
    /// Native function resolved by name (natives live in their own pool).
    Native,
    /// A type by its fully-qualified name.
    Type,
    /// An instance field: `parent` is the owning type, `name` the field.
    Field,
}

/// One engine dependency.
pub struct Symbol {
    pub kind: Kind,
    pub name: &'static str,
    /// Owning type for `Fn`/`Field`; ignored for `Type`/`Native`.
    pub parent: Option<&'static str>,
    /// Subsystem this belongs to — used to group the checklist.
    pub group: &'static str,
    /// Human reason we depend on it (shown for missing symbols).
    pub why: &'static str,
    /// `true` -> the patcher cannot function without it; a miss ABORTS the patch.
    /// `false` -> a graceful-degradation or pinned-fallback site; a miss is a
    /// loud WARNING but the patch proceeds.
    pub critical: bool,
}

impl Symbol {
    /// Human label, e.g. `hscript.Interp::execute`, `pxf.entity.Character.body`,
    /// `native socket_init`, `sys.net.Socket`.
    pub fn label(&self) -> String {
        match self.kind {
            Kind::Fn => match self.parent {
                Some(p) => format!("{p}::{}", self.name),
                None => self.name.to_string(),
            },
            Kind::Field => match self.parent {
                Some(p) => format!("{p}.{}", self.name),
                None => self.name.to_string(),
            },
            Kind::Native => format!("native {}", self.name),
            Kind::Type => self.name.to_string(),
        }
    }
}

/// Outcome of resolving one `Symbol` against a loaded bytecode.
pub struct SymStatus {
    pub group: &'static str,
    pub label: String,
    pub why: &'static str,
    pub critical: bool,
    /// `Some(findex/type-index/field-index)` if resolved, `None` if missing.
    pub resolved: Option<usize>,
}

macro_rules! sym {
    (fn $name:literal in $parent:literal, $group:literal, $why:literal, $crit:literal) => {
        Symbol { kind: Kind::Fn, name: $name, parent: Some($parent), group: $group, why: $why, critical: $crit }
    };
    (native $name:literal, $group:literal, $why:literal, $crit:literal) => {
        Symbol { kind: Kind::Native, name: $name, parent: None, group: $group, why: $why, critical: $crit }
    };
    (type $name:literal, $group:literal, $why:literal, $crit:literal) => {
        Symbol { kind: Kind::Type, name: $name, parent: None, group: $group, why: $why, critical: $crit }
    };
    (field $name:literal of $parent:literal, $group:literal, $why:literal, $crit:literal) => {
        Symbol { kind: Kind::Field, name: $name, parent: Some($parent), group: $group, why: $why, critical: $crit }
    };
}

/// Every engine symbol the patcher leans on, grouped by subsystem. Extracted from
/// `connect_edit` — keep them in step. Names are case-sensitive and must match the
/// engine exactly (e.g. static classes are `$`-prefixed: `pxf.io.$ResourceManager`).
pub const MANIFEST: &[Symbol] = &[
    // ── Screenshot (`shot`) ─────────────────────────────────────────────────
    // Heaps reads its own framebuffer back, so the engine photographs itself and the
    // PNG comes home as hex on the command socket. This is what makes a claim about
    // what is ON SCREEN checkable: the object walk proves an object exists, is visible,
    // sized and positioned, and none of that is evidence a pixel of it was drawn.
    // Critical: without any of them the `shot` verb has no capture path at all.
    sym!(fn "getScene" in "h2d.Object",          "screenshot", "reach the scene from any display object", true),
    sym!(fn "captureBitmap" in "h2d.Scene",      "screenshot", "render the live scene to a texture", true),
    sym!(fn "getTexture" in "h2d.Tile",          "screenshot", "the captured tile's texture", true),
    sym!(fn "capturePixels" in "h3d.mat.Texture", "screenshot", "read the framebuffer back to pixels", true),
    sym!(fn "toPNG" in "hxd.Pixels",             "screenshot", "encode the frame engine-side", true),
    sym!(fn "toHex" in "haxe.io.Bytes",          "screenshot", "ship the PNG over the text socket", true),
    sym!(type "h2d.Scene",                       "screenshot", "capture target scene", true),
    sym!(type "h2d.Bitmap",                      "screenshot", "what captureBitmap returns", true),
    sym!(type "h2d.Tile",                        "screenshot", "the captured bitmap's tile", true),
    sym!(type "h3d.mat.Texture",                 "screenshot", "texture the pixels are read from", true),
    sym!(type "hxd.Pixels",                      "screenshot", "decoded frame before PNG encode", true),
    sym!(type "haxe.io.Bytes",                   "screenshot", "encoded PNG bytes", true),
    sym!(field "tile" of "h2d.Bitmap",           "screenshot", "bitmap -> tile hop", true),

    // ── Socket bridge ───────────────────────────────────────────────────────
    // The TCP handshake + command loop. Without ANY of these Peptide cannot talk
    // to the engine at all, so every one is critical.
    sym!(type "sys.net.Socket",                  "socket-bridge", "client socket to the harness", true),
    sym!(type "sys.net.Host",                    "socket-bridge", "loopback host for connect()", true),
    sym!(type "sys.net._Socket.SocketOutput",    "socket-bridge", "byte-level handshake output", true),
    sym!(type "String",                          "socket-bridge", "string objects for every reply", true),
    sym!(fn "connect" in "sys.net.Socket",       "socket-bridge", "open the bridge connection", true),
    sym!(fn "init" in "sys.net.Socket",          "socket-bridge", "create __s + input + output", true),
    sym!(fn "setBlocking" in "sys.net.Socket",   "socket-bridge", "non-blocking command poll", true),
    sym!(fn "writeString" in "haxe.io.Output",   "socket-bridge", "encoding arg type source", true),
    sym!(fn "writeByte" in "sys.net._Socket.SocketOutput", "socket-bridge", "byte-by-byte handshake", true),
    sym!(fn "flush" in "haxe.io.Output",         "socket-bridge", "push replies to the wire", true),
    sym!(field "output" of "sys.net.Socket",     "socket-bridge", "the write side of the socket", true),
    sym!(field "ip" of "sys.net.Host",           "socket-bridge", "set 127.0.0.1 directly (skip resolve)", true),
    sym!(field "__s" of "sys.net.Socket",        "socket-bridge", "raw handle for recv polling", true),
    sym!(native "socket_init",                   "socket-bridge", "winsock/posix socket bring-up", true),
    sym!(native "socket_recv_char",              "socket-bridge", "read a command byte", true),
    sym!(fn "exit" in "hxd.$System",             "socket-bridge", "clean 'x' shutdown (else kill -9)", false),

    // ── Boot / headless launch ──────────────────────────────────────────────
    sym!(fn "update" in "fraymakers.Main",       "boot", "per-frame hook = our command pump", true),
    sym!(fn "createMode" in "fraymakers.util.$FraymakersClassFactory", "boot", "build a real FraymakersMode", true),
    sym!(fn "startMatch" in "fraymakers.core.FraymakersMode",          "boot", "engine's own offline-match flow", true),
    sym!(type "fraymakers.core.FraymakersMode",  "boot", "mode object that owns the match", true),
    sym!(type "pxf.core.$MatchSettings",         "boot", "defaultMatchRules statics", true),
    sym!(type "pxf.core.Match",                  "boot", "live-match query target", true),
    // fast-boot fns (resolved by name so a recompile can't silently mis-point them):
    sym!(fn "launchScreen" in "fraymakers.$Main", "boot", "fast-boot no-op target (skips Title + bulk UGC); STATIC method, $-parent", true),
    sym!(fn "onLoaded" in "fraymakers.Main",      "boot", "headless READY hook (load-step queue head)", true),
    sym!(fn "loadUgc" in "fraymakers.util.$UgcUtil", "boot", "launchScreen guard: the no-op target must call this", true),
    sym!(fn "init" in "pxf.io.$ThreadTaskManager", "boot", "spawns the worker that drains the .fra load deque", true),
    sym!(fn "cleanupMatch" in "pxf.controllers.$MatchController", "boot", "close the prior match on re-launch", true),
    sym!(fn "destroyAllActiveMenus" in "pxf.controllers.$MenuController", "boot", "clear menus before a headless match", true),
    // statics-object types — find_statics_global locates the global holding each:
    sym!(type "pxf.io.$ResourceManager",         "boot", "RM statics (pool/poolHash/content maps)", true),
    sym!(type "pxf.controllers.$MatchController", "boot", "MatchController statics (currentMatch)", true),
    sym!(type "pxf.core.$CoreEngine",            "boot", "CoreEngine statics", true),
    sym!(type "pxf.core.$Tildebugger",           "boot", "Tildebugger statics (console + error mirror)", true),
    sym!(type "fraymakers.util.$UgcUtil",        "boot", "UgcUtil statics (load queue diagnostics)", true),

    // ── Content resolution / self-bootstrap ─────────────────────────────────
    sym!(type "pxf.io.AbstractResource",         "content", "base resource type", true),
    sym!(type "pxf.io.Resource",                 "content", "threaded fetch target", true),
    sym!(type "pxf.structs.PXFResource",         "content", "per-type content maps live here", true),
    sym!(type "haxe.ds.StringMap",               "content", "pool/content-map iteration", true),
    sym!(fn "getFullyQualifiedResourceId" in "pxf.io.AbstractResource", "content", "public:: boot filter + pool keys", true),
    sym!(fn "get_Loaded" in "pxf.io.AbstractResource", "content", "skip unloaded stubs (null f17)", true),
    sym!(fn "fetchThreaded" in "pxf.io.Resource",      "content", "load the custom char on demand", true),
    sym!(fn "finishLoading" in "pxf.io.AbstractResource", "content", "block until the char is ready", true),
    sym!(fn "getPXFResource" in "pxf.io.$ResourceManager",            "content", "resolve a fqid to its resource", true),
    sym!(fn "getResourceIdentifierString" in "pxf.io.$ResourceManager", "content", "short-name resolution", true),
    sym!(fn "getContentIdentifierString" in "pxf.io.$ResourceManager", "content", "content id for the launch payload", true),
    sym!(fn "addResource" in "pxf.io.$ResourceManager",              "content", "register the bootstrapped char", true),
    sym!(fn "__constructor__" in "pxf.io.$Resource", "content", "build the self-bootstrap Resource (id/path/enc)", true),
    sym!(fn "queueRequiredResources" in "pxf.io.$ResourceManager", "content", "boot required-load filter target", true),
    sym!(fn "getResourceByID" in "pxf.io.$ResourceManager",       "content", "load-on-demand pool fetch", true),
    sym!(fn "parseResourceIdentifier" in "pxf.io.$ResourceManager", "content", "fqid string -> resource identifier (canonicalize)", true),
    sym!(fn "setupStage" in "pxf.core.Match",    "content", "stage-null crash-diagnostic insertion point", true),
    sym!(field "characterPxfContentMap" of "pxf.structs.PXFResource", "content", "char + assist lookup", true),
    sym!(field "stagePxfContentMap" of "pxf.structs.PXFResource",     "content", "stage lookup", true),
    sym!(fn "keys" in "haxe.ds.StringMap",       "content", "iterate content maps", true),
    sym!(fn "exists" in "haxe.ds.StringMap",     "content", "membership test in resolution", true),
    sym!(fn "get" in "haxe.ds.StringMap",        "content", "fetch a resolved entry", true),

    // ── hscript eval pipeline ('e' command) — the strategic core ────────────
    // The engine bundles the SAME hscript Parser + Interp that runs every
    // character script. Moving handler logic from hand-emitted bytecode into
    // hscript text (executed here) is how we shed brittle bytecode — keep these
    // resolving by name.
    sym!(type "hscript.Parser",                  "hscript-eval", "parse command/handler script text", true),
    sym!(type "hscript.Interp",                  "hscript-eval", "execute parsed scripts", true),
    sym!(fn "parseString" in "hscript.Parser",   "hscript-eval", "text -> Expr", true),
    sym!(fn "execute" in "hscript.Interp",       "hscript-eval", "run an Expr", true),
    sym!(fn "setVar" in "hscript.Interp",        "hscript-eval", "bind p0/p1/match into scope", true),
    // The next three have pinned-index fallbacks in connect_edit (.unwrap_or):
    // a miss here means we are silently relying on a STALE index — warn, don't abort.
    sym!(fn "exprReturn" in "hscript.Interp",    "hscript-eval", "engine-style program run (pinned fallback if missing)", false),
    sym!(fn "applyInterpreterGlobals" in "fraymakers.api.$FraymakersScriptGlobals", "hscript-eval", "engine globals callback (pinned fallback if missing)", false),
    sym!(fn "interpretScript" in "pxf.api.$ApiScript", "hscript-eval", "engine's wrapped run (pinned fallback if missing)", false),
    sym!(fn "log" in "pxf.core.$Tildebugger",    "hscript-eval", "engine console log facade (pinned fallback if missing)", false),

    // ── Line-command parsing primitives ─────────────────────────────────────
    sym!(fn "alloc" in "haxe.io.$Bytes",         "line-cmd", "buffer received arg bytes", true),
    sym!(fn "set" in "haxe.io.Bytes",            "line-cmd", "fill the arg buffer", true),
    sym!(fn "getString" in "haxe.io.Bytes",      "line-cmd", "bytes -> command String", true),
    sym!(fn "split" in "String",                 "line-cmd", "tokenize a command line", true),
    sym!(fn "indexOf" in "String",               "line-cmd", "namespace-prefix probing", true),
    sym!(fn "charCodeAt" in "String",            "line-cmd", "selector-byte math", true),

    // ── Move dispatch + telemetry (criteria #4-#6) ──────────────────────────
    sym!(type "pxf.entity.Character",            "move-dispatch", "the live player-0 entity", true),
    sym!(type "pxf.entity.$CState",              "move-dispatch", "move-id source (JAB, etc.)", true),
    sym!(fn "toState" in "pxf.entity.Character", "move-dispatch", "drive a move via the state machine", true),
    sym!(fn "getStateName" in "pxf.entity.Character", "move-dispatch", "read back current state", true),
    sym!(field "JAB" of "pxf.entity.$CState",    "move-dispatch", "representative move id (dispatch fallback)", true),
    // Telemetry degrades gracefully — non-critical.
    sym!(type "pxf.components.Body",             "telemetry", "x/y readback ('v' command)", false),
    sym!(type "pxf.components.Physics",          "telemetry", "velocity readback", false),
    sym!(type "pxf.components.Damage",           "telemetry", "damage readback", false),
    sym!(type "pxf.components.Animation",        "telemetry", "animation introspection ('a')", false),
    sym!(field "body" of "pxf.entity.Character", "telemetry", "Character -> Body", false),
    sym!(field "physics" of "pxf.entity.Character", "telemetry", "Character -> Physics", false),
    sym!(field "damage" of "pxf.entity.Character",  "telemetry", "Character -> Damage", false),
    sym!(field "animation" of "pxf.entity.Character", "telemetry", "Character -> Animation", false),
    sym!(fn "playFrame" in "pxf.components.Animation", "telemetry", "step an animation frame", false),
    sym!(fn "string" in "$Std",                  "telemetry", "Std.string for numeric formatting (pinned fallback if missing)", false),

    // ── Engine console ('c' command) ────────────────────────────────────────
    sym!(fn "runCommand" in "h2d.Console",       "console", "run an engine console command", false),
    sym!(fn "set_enabled" in "pxf.core.ImprovedConsole", "console", "open the console overlay", false),

    // ── Stage parallax preview (read-only, not the patcher) ─────────────────
    // Backs the GUI stage parallax preview's "pull params from the engine" path
    // (gui.rs fm_engine), so the preview carries no ported engine constants. The
    // preview is read-only — a miss just degrades it to a fallback rate, never
    // corrupts anything — so every entry is non-critical.
    sym!(type "pxf.core.camera.ParallaxBG",       "stage-parallax", "camera-background class (the engine's parallax model)", false),
    sym!(type "pxf.core.camera.ParallaxBGConfig", "stage-parallax", "per-layer parallax config schema", false),
    sym!(field "xPanMultiplier" of "pxf.core.camera.ParallaxBGConfig", "stage-parallax", "the explicit per-layer pan rate the converter writes", false),
    sym!(type "pxf.core.camera.$ParallaxMode",    "stage-parallax", "BOUNDS/PAN/DEPTH mode constants", false),
];

/// `true` when stderr is an interactive terminal (so live progress bars are
/// useful). When piped (e.g. spawned by the harness) we emit plain lines instead.
pub fn is_tty() -> bool {
    std::io::stderr().is_terminal()
}

const BAR_WIDTH: usize = 28;

/// Render/refresh the single-line live progress bar (call once per symbol, before
/// resolving it). TTY only — no-op semantics are the caller's job (guard with
/// `is_tty()`).
pub fn render_live(done: usize, total: usize, current: &str) {
    let filled = if total == 0 { BAR_WIDTH } else { done * BAR_WIDTH / total };
    let bar: String = (0..BAR_WIDTH)
        .map(|i| if i < filled { '#' } else { '-' })
        .collect();
    // \r + clear-to-end-of-line, truncate the label so the line never wraps.
    let label: String = current.chars().take(42).collect();
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K  preflight [{bar}] {done:>2}/{total}  {label}");
    let _ = err.flush();
}

/// Clear the live progress line once resolution is done.
pub fn clear_live() {
    let mut err = std::io::stderr();
    let _ = write!(err, "\r\x1b[2K");
    let _ = err.flush();
}

/// Machine-parseable progress line, emitted to stderr when the patcher runs as a
/// subprocess (non-TTY) — e.g. spawned by the GUI/TUI boot path, which captures
/// stderr and renders the bar in the connection modal. Format (stable, parse it
/// by prefix): `@@PFP <done> <total> <label>`.
pub const PROGRESS_PREFIX: &str = "@@PFP";
/// Final machine summary line: `@@PFR <ok> <missing_critical> <missing_warn>`.
pub const RESULT_PREFIX: &str = "@@PFR";

pub fn emit_machine_progress(done: usize, total: usize, label: &str) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{PROGRESS_PREFIX} {done} {total} {label}");
    let _ = err.flush();
}

pub fn emit_machine_result(ok: usize, miss_crit: usize, miss_warn: usize) {
    let mut err = std::io::stderr();
    let _ = writeln!(err, "{RESULT_PREFIX} {ok} {miss_crit} {miss_warn}");
    let _ = err.flush();
}

/// Print the grouped doctor checklist to stderr. This is the "what the doctor
/// shows" output — emitted after the live bar during a patch, and on its own for
/// the standalone `doctor` mode.
pub fn render_report(statuses: &[SymStatus], title: &str) {
    let mut err = std::io::stderr();
    let (ok_mark, miss_mark) = ("[ ok ]", "[MISS]");
    let _ = writeln!(err, "\n{title}");
    let _ = writeln!(err, "{}", "-".repeat(title.len().min(72)));

    let mut last_group = "";
    for st in statuses {
        if st.group != last_group {
            let _ = writeln!(err, "  {}:", st.group);
            last_group = st.group;
        }
        match st.resolved {
            Some(idx) => {
                let _ = writeln!(err, "    {ok_mark} {:<52} #{idx}", st.label);
            }
            None => {
                let tag = if st.critical { "CRITICAL" } else { "warn" };
                let _ = writeln!(err, "    {miss_mark} {:<52} MISSING ({tag}) — {}", st.label, st.why);
            }
        }
    }

    let (ok, miss_crit, miss_warn) = summarize(statuses);
    let _ = writeln!(
        err,
        "\n  {ok}/{} resolved · {miss_crit} critical missing · {miss_warn} warnings",
        statuses.len()
    );
    if miss_crit > 0 {
        let _ = writeln!(
            err,
            "  -> {miss_crit} critical engine symbol(s) gone. This Fraymakers build is NOT\n     compatible with the current patcher. Re-find the names above and update\n     src/manifest.rs + connect_edit. See docs/PEPTIDE_DESIGN.md 'Version resilience'."
        );
    } else if miss_warn > 0 {
        let _ = writeln!(
            err,
            "  -> patch can proceed, but {miss_warn} non-critical symbol(s) changed (pinned\n     fallbacks / degraded telemetry). Re-verify before trusting those paths."
        );
    }
    let _ = err.flush();
}

/// (resolved, missing-critical, missing-non-critical)
pub fn summarize(statuses: &[SymStatus]) -> (usize, usize, usize) {
    let mut ok = 0;
    let mut miss_crit = 0;
    let mut miss_warn = 0;
    for st in statuses {
        match st.resolved {
            Some(_) => ok += 1,
            None if st.critical => miss_crit += 1,
            None => miss_warn += 1,
        }
    }
    (ok, miss_crit, miss_warn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_is_grouped_contiguously() {
        // render_report only prints a group header on change, so all symbols of a
        // group must be contiguous or the checklist would split a group in two.
        let mut seen: Vec<&str> = Vec::new();
        let mut last = "";
        for sym in MANIFEST {
            if sym.group != last {
                assert!(
                    !seen.contains(&sym.group),
                    "group {:?} is not contiguous in MANIFEST",
                    sym.group
                );
                seen.push(sym.group);
                last = sym.group;
            }
        }
    }

    #[test]
    fn fn_and_field_entries_have_a_parent() {
        for sym in MANIFEST {
            match sym.kind {
                Kind::Fn | Kind::Field => assert!(
                    sym.parent.is_some(),
                    "{} needs a parent type",
                    sym.label()
                ),
                Kind::Type | Kind::Native => assert!(sym.parent.is_none()),
            }
        }
    }

    #[test]
    fn labels_render_per_kind() {
        let f = Symbol { kind: Kind::Fn, name: "execute", parent: Some("hscript.Interp"), group: "g", why: "", critical: true };
        assert_eq!(f.label(), "hscript.Interp::execute");
        let fld = Symbol { kind: Kind::Field, name: "body", parent: Some("pxf.entity.Character"), group: "g", why: "", critical: true };
        assert_eq!(fld.label(), "pxf.entity.Character.body");
        let n = Symbol { kind: Kind::Native, name: "socket_init", parent: None, group: "g", why: "", critical: true };
        assert_eq!(n.label(), "native socket_init");
        let t = Symbol { kind: Kind::Type, name: "sys.net.Socket", parent: None, group: "g", why: "", critical: true };
        assert_eq!(t.label(), "sys.net.Socket");
    }

    #[test]
    fn summarize_counts_each_bucket() {
        let mk = |resolved, critical| SymStatus {
            group: "g", label: "x".into(), why: "", critical, resolved,
        };
        let statuses = vec![
            mk(Some(1), true),  // ok
            mk(Some(2), false), // ok
            mk(None, true),     // critical miss
            mk(None, false),    // warn
            mk(None, false),    // warn
        ];
        assert_eq!(summarize(&statuses), (2, 1, 2));
    }
}
