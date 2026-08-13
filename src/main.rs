// On Windows, link release builds as a GUI-subsystem app so launching the .exe
// (double-click or from Explorer) does NOT pop up / flash a console window. CLI
// subcommands re-attach to the launching terminal at runtime (see
// platform::attach_parent_console). Debug builds keep the console subsystem so
// `cargo run` output works without ceremony.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
// Clippy baseline (CI runs `cargo clippy -- -D warnings`): pedantic-style / pre-existing
// lint families we accept. new code should still try to avoid them.
#![allow(
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::empty_line_after_doc_comments,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::manual_checked_ops,
    clippy::unnecessary_unwrap,
    clippy::neg_cmp_op_on_partial_ord,
    clippy::field_reassign_with_default,
)]

use std::fs::File;
use std::io::{BufReader, BufWriter};

// ── Gated memory-limit allocator (diagnostic) ───────────────────────────────
// Relocated from the converter binary when it folded into peptide: only ONE
// binary in a build may define #[global_allocator], and peptide is now it.
// Default-disabled (limit 0 → no behavior change). Set CONV_MEM_LIMIT_MB to a
// number to cap live heap: once live bytes exceed the cap, alloc returns null so
// Rust's alloc-error handler aborts with "memory allocation of N bytes failed".
// Used to localize convert-time OOMs. The atomics add a negligible load on each
// (de)alloc and are inert when the limit is 0.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

struct LimitAlloc;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static LIMIT: AtomicUsize = AtomicUsize::new(0); // bytes; 0 = disabled

unsafe impl GlobalAlloc for LimitAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let lim = LIMIT.load(Relaxed);
        if lim != 0 {
            let prev = LIVE.fetch_add(layout.size(), Relaxed);
            if prev + layout.size() > lim {
                LIVE.fetch_sub(layout.size(), Relaxed);
                return std::ptr::null_mut(); // → handle_alloc_error aborts with the size
            }
        }
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if LIMIT.load(Relaxed) != 0 { LIVE.fetch_sub(layout.size(), Relaxed); }
        System.dealloc(ptr, layout);
    }
}

#[global_allocator]
static GLOBAL: LimitAlloc = LimitAlloc;

mod asm;
#[allow(unused_imports)]
use asm::Asm;

// Shared friendly-command vocabulary. The patcher uses MOVES to generate the
// move-dispatch jump table; the bridge uses it to translate move names. One table.
mod interpreter;
mod vocab; // shared commands.hsx vocabulary + FM↔SSF2 reconciliation (interpreter level)
mod manifest; // engine-symbol dependency table + doctor/preflight progress UI
mod convert; // `peptide convert` — in-process SSF2 → Fraymakers conversion (folded-in converter)
mod config; // persisted config (Fraymakers/FrayTools paths, current char) + env-override resolvers
mod platform; // cross-platform path discovery + FrayTools publish-folder editing (ported from the egui GUI)
mod bridge; // headless TCP runtime (serve / send_once)
mod fastboot; // single definition of the quick-boot autostart, shared by CLI + GUI
mod session; // shared control-file tail loop for the CLI session loops (FM + SSF2)
mod ui; // terminal console (ratatui) + cross-platform launcher
mod gui; // graphical chat window (egui/eframe) — the default
mod overlay; // standalone transparent debugger HUD that floats over the game (CLI + GUI)
mod fraytools; // drive FrayTools over CDP: export .fra / render / harness (ported from Node)
mod ssf2; // `peptide ssf2` — SSF2 engine model: physics ground-truth + scaling + quickboot
mod ssf2_bridge; // host side of the SSF2 runtime bridge (session/tell/log/send over file-IPC)
mod debug_target; // OOP seam: one Command vocabulary, per-engine DebugTarget backends
mod ssf2_target; // SSF2 DebugTarget backend (AVM2 reflection eval over file-IPC)

use hlbc::opcodes::Opcode;
use hlbc::types::{Reg, RefString};
use hlbc::Bytecode;

fn main() -> anyhow::Result<()> {
    // Diagnostic heap cap (see LimitAlloc). CONV_MEM_LIMIT_MB=2000 caps live heap
    // at 2 GB; unset/0 disables (normal operation). Honored for `peptide convert`.
    if let Ok(mb) = std::env::var("CONV_MEM_LIMIT_MB") {
        if let Ok(n) = mb.trim().parse::<usize>() {
            LIMIT.store(n.saturating_mul(1024 * 1024), Relaxed);
            if n > 0 { eprintln!("CONV_MEM_LIMIT_MB={n} → live-heap cap {n} MB"); }
        }
    }

    let args: Vec<String> = std::env::args().collect();

    // Windows: the .exe is linked as a GUI app (no console on launch), so CLI
    // subcommands have no stdout/stderr until we re-attach to the terminal that
    // launched us. The graphical UI modes (no arg / `ui` / `gui`) want no
    // console; everything else is a CLI command and does.
    #[cfg(windows)]
    {
        let gui_mode = matches!(
            args.get(1).map(|s| s.as_str()),
            None | Some("ui") | Some("gui")
        );
        if !gui_mode {
            platform::attach_parent_console();
        }
    }

    // ── Runtime modes ──────────────────────────────────────────────────────────
    // The console UI is the DEFAULT. Headless is opt-in. The bytecode PATCHER (the
    // rest of main) runs only when arg1 is a path (e.g. `peptide <in> <out> connect …`),
    // not a mode word — so `peptide` with no args opens the UI.
    match args.get(1).map(|s| s.as_str()) {
        None | Some("ui") | Some("gui") => {
            gui::launch()?; // graphical chat window (default)
            return Ok(());
        }
        Some("tui") => {
            ui::launch()?; // terminal console (ratatui)
            return Ok(());
        }
        Some("headless") | Some("serve") => {
            bridge::serve(bridge::parse_port(&args), bridge::parse_token(&args).as_deref());
            return Ok(());
        }
        Some("session") => {
            // Long-lived daemon: boot/attach an engine, hold the TCP link, and
            // process commands appended to a control file (peptide tell) while
            // mirroring engine output to a log (peptide log). The canonical
            // agent workflow for iterating on a character or a conversion fix —
            // send an eval, read the reply, decide the next eval, same match.
            bridge::session(&args[2..]);
            return Ok(());
        }
        Some("overlay") => {
            // Standalone transparent debugger HUD that floats over the game. Its own
            // process (own window + event loop) so it works from CLI sessions and the GUI
            // alike; tails the session's out.log and renders state + the SCRIPTERR stream.
            overlay::run(&args[2..])?;
            return Ok(());
        }
        Some("tell") => {
            // Queue one command for a running `peptide session`.
            bridge::tell(&args[2..]);
            return Ok(());
        }
        Some("log") => {
            // Print (or --follow) a running session's mirrored engine output.
            bridge::log(&args[2..]);
            return Ok(());
        }
        Some("send") => {
            let mut cmd: Option<String> = None;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--port" || args[i] == "--token" || args[i] == "--delay" {
                    i += 2;
                    continue;
                }
                cmd = Some(args[i].clone());
                break;
            }
            let cmd = cmd.unwrap_or_else(|| {
                eprintln!("usage: peptide send [--port N] [--token T] \"<command>\"");
                std::process::exit(2);
            });
            bridge::send_once(bridge::parse_port(&args), bridge::parse_token(&args).as_deref(), &cmd);
            return Ok(());
        }
        Some("convert") => {
            // Fold-in of the old ssf2_converter binary: run an SSF2 → Fraymakers
            // conversion in-process (the converter is now a library crate).
            convert::run_cli(&args[2..])?;
            return Ok(());
        }
        Some("export") => {
            // Drive FrayTools' Publish to build the game-ready .fra (was: node export-in-fraytools.js).
            fraytools::export(&args)?;
            return Ok(());
        }
        Some("ssf2") => {
            // SSF2 engine integration: physics ground-truth, SSF2→FM scaling, quickboot.
            ssf2::run_cli(&args[2..])?;
            return Ok(());
        }
        Some("render") => {
            // Open an entity on the stage + capture the canvas PNG (was: node render-entity.js).
            fraytools::render(&args)?;
            return Ok(());
        }
        Some("harness") => {
            // Navigate + extract box geometry + PNG + JSON report (was: node harness.js).
            fraytools::harness(&args)?;
            return Ok(());
        }
        Some("help") | Some("-h") | Some("--help") => {
            print!("{}", interpreter::help_text());
            return Ok(());
        }
        Some(m) if m.starts_with('-') => {
            // a bare flag with no mode word -> default to the UI
            ui::launch()?;
            return Ok(());
        }
        _ => {} // arg1 is a path -> fall through to the bytecode patcher
    }

    // ── Bytecode patcher ───────────────────────────────────────────────────────
    if args.len() < 3 {
        eprintln!("usage: peptide <input.dat> <output.dat> [mode]   (the patcher)");
        eprintln!("       peptide                                   (the console UI)");
        eprintln!("       peptide headless | send \"<cmd>\" | help");
        std::process::exit(2);
    }
    let input = &args[1];
    let output = &args[2];
    let mode = args.get(3).map(|s| s.as_str()).unwrap_or("roundtrip");

    let mut r = BufReader::new(File::open(input)?);
    let mut code = Bytecode::deserialize(&mut r).map_err(|e| anyhow::anyhow!("deserialize: {e:?}"))?;
    eprintln!(
        "loaded: version={} functions={} strings={} types={} globals={}",
        code.version,
        code.functions.len(),
        code.strings.len(),
        code.types.len(),
        code.globals.len()
    );

    match mode {
        "roundtrip" => {}
        "doctor" => {
            // Preflight only: resolve the engine-symbol manifest, print the
            // checklist, and exit. Writes nothing. Run this first against a new
            // Fraymakers build to see exactly what (if anything) the patcher must
            // be re-pointed at. Output arg is ignored (pass `_` or any path).
            run_preflight(&code)?;
            eprintln!("doctor: all critical engine symbols resolved — patcher is compatible with this build");
            return Ok(());
        }
        "probe" => {
            let onloaded = find_fn(&code, "onLoaded", Some("fraymakers.Main"))
                .ok_or_else(|| anyhow::anyhow!("fraymakers.Main.onLoaded not found"))?;
            probe_edit(&mut code, onloaded)?
        }
        "inspect" => {
            inspect(&code);
            return Ok(()); // nothing to write
        }
        "whoref" => {
            let needle = args.get(4).cloned().unwrap();
            // string indices whose text contains the needle
            let sidxs: Vec<usize> = (0..code.strings.len())
                .filter(|&i| code.strings[i].as_str().contains(&needle))
                .collect();
            eprintln!("strings matching {needle:?}: {sidxs:?}");
            // const-globals initialized to those strings
            let mut gidxs: Vec<usize> = vec![];
            if let Some(consts) = &code.constants {
                for c in consts {
                    if c.fields.first().map(|f| sidxs.contains(f)).unwrap_or(false) {
                        gidxs.push(c.global.0);
                    }
                }
            }
            eprintln!("const-globals for those strings: {gidxs:?}");
            // functions that load those strings (String opcode) or GetGlobal those globals
            for f in &code.functions {
                let hit = f.ops.iter().any(|op| match op {
                    Opcode::String { ptr, .. } => sidxs.contains(&ptr.0),
                    Opcode::GetGlobal { global, .. } => gidxs.contains(&global.0),
                    _ => false,
                });
                if hit {
                    let pn = f.parent.and_then(|rt| type_name_of(&code, rt)).unwrap_or("?");
                    eprintln!("  findex {:6} {}::{}", f.findex.0, pn, s(&code, f.name));
                }
            }
            return Ok(());
        }
        "fnsof" => {
            let tname = args.get(4).cloned().unwrap();
            eprintln!("functions with parent == {tname}:");
            for f in &code.functions {
                if f.parent.and_then(|rt| type_name_of(&code, rt)) == Some(tname.as_str()) {
                    let argc = code.types[f.t.0].get_type_fun().map(|tf| tf.args.len()).unwrap_or(0);
                    eprintln!("  findex {:6} name={:?} argc={argc}", f.findex.0, s(&code, f.name));
                }
            }
            return Ok(());
        }
        "fninfo" => {
            let fx: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap();
            let fi = function_index_by_findex(&code, fx).unwrap();
            let f = &code.functions[fi];
            eprintln!("findex {fx}: name={:?} (#{}) parent={:?}",
                s(&code, f.name), f.name.0,
                f.parent.map(|rt| (rt.0, type_name_of(&code, rt).map(|x| x.to_string()))));
            if let Some(tf) = code.types[f.t.0].get_type_fun() {
                let args: Vec<(usize, String)> = tf.args.iter()
                    .map(|a| (a.0, code.types[a.0].get_type_obj().map(|o| s(&code, o.name).to_string()).unwrap_or_else(|| format!("t{}", a.0)))).collect();
                let ret = tf.ret.0;
                let retn = code.types[ret].get_type_obj().map(|o| s(&code, o.name).to_string()).unwrap_or_else(|| format!("t{ret}"));
                eprintln!("  args: {args:?}  ret: ({ret}, {retn:?})");
            }
            return Ok(());
        }
        "dis" => {
            let fx: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap();
            disasm(&code, fx);
            return Ok(());
        }
        "strs" => {
            // print one or more strings by index: `peptide <in> _ strs 24593 24755 …`
            for a in &args[4..] {
                if let Ok(i) = a.parse::<usize>() {
                    if i < code.strings.len() { eprintln!("  [{i}] = {:?}", code.strings[i].as_str()); }
                    else { eprintln!("  [{i}] = <out of range>"); }
                }
            }
            return Ok(());
        }
        "typefields" => {
            let tname = args.get(4).cloned().unwrap();
            // allow "t123" or a bare index to address a type directly
            let direct: Option<usize> = tname.strip_prefix('t').and_then(|n| n.parse().ok())
                .or_else(|| tname.parse().ok());
            match direct.filter(|&i| i < code.types.len()).or_else(|| find_type(&code, &tname)) {
                Some(ti) => {
                    eprintln!("type {ti} = {tname}");
                    let vfields = match &code.types[ti] {
                        hlbc::types::Type::Virtual { fields } => Some(fields),
                        _ => None,
                    };
                    if let Some(o) = code.types[ti].get_type_obj() {
                        for (i, fl) in o.fields.iter().enumerate() {
                            let tn = type_name_of(&code, fl.t)
                                .map(|x| x.to_string())
                                .unwrap_or_else(|| format!("t{}", fl.t.0));
                            eprintln!("  field {i:3}: {:?} : {tn}", s(&code, fl.name));
                        }
                    } else if let Some(fields) = vfields {
                        eprintln!("  (virtual)");
                        for (i, fl) in fields.iter().enumerate() {
                            let tn = type_name_of(&code, fl.t)
                                .map(|x| x.to_string())
                                .unwrap_or_else(|| format!("t{}", fl.t.0));
                            eprintln!("  field {i:3}: {:?} : {tn}", s(&code, fl.name));
                        }
                    } else if let hlbc::types::Type::Enum { constructs, .. } = &code.types[ti] {
                        eprintln!("  (enum, {} constructs)", constructs.len());
                        for (i, c) in constructs.iter().enumerate() {
                            eprintln!("  construct {i:3}: {:?} ({} params)", s(&code, c.name), c.params.len());
                        }
                    } else {
                        eprintln!("  (kind: {:?})", std::mem::discriminant(&code.types[ti]));
                    }
                }
                None => eprintln!("type not found: {tname}"),
            }
            return Ok(());
        }
        "callers" => {
            let target: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap();
            eprintln!("functions that Call*/Closure findex {target}:");
            for f in &code.functions {
                let hit = f.ops.iter().any(|op| match op {
                    Opcode::Call0 { fun, .. } | Opcode::Call1 { fun, .. }
                    | Opcode::Call2 { fun, .. } | Opcode::Call3 { fun, .. }
                    | Opcode::Call4 { fun, .. } | Opcode::CallN { fun, .. }
                    | Opcode::StaticClosure { fun, .. } | Opcode::InstanceClosure { fun, .. } => fun.0 == target,
                    _ => false,
                });
                if hit {
                    let pn = f.parent.and_then(|rt| type_name_of(&code, rt)).unwrap_or("?");
                    eprintln!("  {:6} {}::{}", f.findex.0, pn, s(&code, f.name));
                }
            }
            return Ok(());
        }
        "strgrep" => {
            let needle = args.get(4).cloned().unwrap();
            for (i, st) in code.strings.iter().enumerate() {
                if st.as_str().contains(&needle) {
                    eprintln!("  str {i:6}: {:?}", st.as_str());
                }
            }
            return Ok(());
        }
        "newof" => {
            // find functions that `New` a register of the given type index, and print the
            // op right after (usually the constructor Call) — to locate a class constructor.
            let ti: usize = args.get(4).and_then(|s| s.strip_prefix('t').unwrap_or(s).parse().ok()).unwrap();
            eprintln!("functions with New of type t{ti}:");
            for f in &code.functions {
                for (i, op) in f.ops.iter().enumerate() {
                    if let Opcode::New { dst } = op {
                        if f.regs.get(dst.0 as usize).map(|t| t.0) == Some(ti) {
                            let pn = f.parent.and_then(|rt| type_name_of(&code, rt)).unwrap_or("?");
                            eprintln!("  fn {:6} {}::{}  @op{i}", f.findex.0, pn, s(&code, f.name));
                            for (k, nop) in f.ops.iter().enumerate().skip(i + 1).take(4) {
                                eprintln!("      op{k}: {nop:?}");
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
        "protos" => {
            // dump a type's vtable protos IN ORDER (proto index = position), with findexes.
            let tname = args.get(4).cloned().unwrap();
            if let Some(ti) = find_type(&code, &tname) {
                if let Some(o) = code.types[ti].get_type_obj() {
                    for (i, p) in o.protos.iter().enumerate() {
                        eprintln!("  proto {i:3} -> findex {:6} {}", p.findex.0, s(&code, p.name));
                    }
                } else { eprintln!("  not an obj type: {tname}"); }
            } else { eprintln!("  type not found: {tname}"); }
            return Ok(());
        }
        "freaders" => {
            // list functions that have a Field read of <type>.<fieldidx> (obj reg typed as <type>).
            let tname = args.get(4).cloned().unwrap();
            let fidx_target: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap();
            let ti = match find_type(&code, &tname) { Some(t) => t, None => { eprintln!("type not found: {tname}"); return Ok(()); } };
            for f in &code.functions {
                // obj-based Field/SetField where the obj reg is the target type
                let obj_match = |op: &Opcode| matches!(op,
                    Opcode::Field { obj, field, .. } | Opcode::SetField { obj, field, .. }
                        if field.0 == fidx_target && f.regs.get(obj.0 as usize).map(|t| t.0) == Some(ti));
                // GetThis/SetThis operate on `this` (reg0) — count them when the fn's parent IS the type
                let parent_is_ti = f.parent.map(|rt| rt.0) == Some(ti);
                let this_match = |op: &Opcode| parent_is_ti && matches!(op,
                    Opcode::GetThis { field, .. } | Opcode::SetThis { field, .. } if field.0 == fidx_target);
                let writes = f.ops.iter().any(|op| matches!(op, Opcode::SetField { obj, field, .. } if field.0==fidx_target && f.regs.get(obj.0 as usize).map(|t| t.0)==Some(ti))
                    || (parent_is_ti && matches!(op, Opcode::SetThis { field, .. } if field.0==fidx_target)));
                if f.ops.iter().any(|op| obj_match(op) || this_match(op)) {
                    let pn = f.parent.and_then(|rt| type_name_of(&code, rt)).unwrap_or("?");
                    let kind = if writes { "W" } else { "R" };
                    eprintln!("  [{kind}] {:6} {}::{}", f.findex.0, pn, s(&code, f.name));
                }
            }
            return Ok(());
        }
        "methods" => {
            // list every function whose parent type name contains <needle>
            let needle = args.get(4).cloned().unwrap();
            for f in &code.functions {
                let pn = f.parent.and_then(|rt| type_name_of(&code, rt)).unwrap_or("");
                if pn.contains(&needle) {
                    eprintln!("  {:6} {}::{}", f.findex.0, pn, s(&code, f.name));
                }
            }
            return Ok(());
        }
        "connect" => {
            // peptide <in> <out> connect <port> <token> [<char> [<stage> [<assist>]]]
            // The socket bridge + command dispatch (s/t/q/x/c/p) are ALWAYS installed.
            // Providing a CHARACTER triggers HEADLESS fast-boot: skip the Title/menus
            // (no-op launchScreen) + filter the boot required-load to skip public:: base
            // content, and bake that char (+ optional stage/assist) as the launch default.
            // With NO character the game boots normally (title + full content load) and is
            // just a TCP bridge — headless does not trigger unless a character is given.
            let port: u16 = args.get(4).and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("connect mode needs <port>"))?;
            let token = args.get(5).cloned().unwrap_or_default();
            let char_name = args.get(6).cloned();
            let stage_name = args.get(7).cloned();
            let assist_name = args.get(8).cloned();
            let headless = char_name.is_some();
            // install dir = parent of the boot file, used to build custom/<char>/<char>.fra
            let install_dir = std::path::Path::new(input).parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            connect_edit(&mut code, port, &token, headless, char_name, stage_name, assist_name, &install_dir)?;
        }
        other => anyhow::bail!("unknown mode: {other}"),
    }

    let mut w = BufWriter::new(File::create(output)?);
    code.serialize(&mut w)
        .map_err(|e| anyhow::anyhow!("serialize: {e:?}"))?;
    eprintln!("wrote {output} (mode={mode})");
    Ok(())
}

/// Find the index into `code.functions` of the function whose findex == target.
fn function_index_by_findex(code: &Bytecode, findex: usize) -> Option<usize> {
    code.functions.iter().position(|f| f.findex.0 == findex)
}

// ---- name-based symbol resolver (robust across engine versions) -------------

fn s(code: &Bytecode, r: hlbc::types::RefString) -> &str {
    code.strings[r.0].as_str()
}

/// Type index of an Obj/Struct class by its fully-qualified name.
fn find_type(code: &Bytecode, name: &str) -> Option<usize> {
    (0..code.types.len()).find(|&i| {
        code.types[i]
            .get_type_obj()
            .map(|o| s(code, o.name) == name)
            .unwrap_or(false)
    })
}

/// Absolute field index (hierarchy-inclusive) of `field` within Obj type `tidx`.
fn find_field(code: &Bytecode, tidx: usize, field: &str) -> Option<usize> {
    code.types[tidx]
        .get_type_obj()
        .and_then(|o| o.fields.iter().position(|f| s(code, f.name) == field))
}

/// findex of a native function by name (natives live in a separate pool).
fn find_native(code: &Bytecode, name: &str) -> Option<usize> {
    code.natives
        .iter()
        .find(|n| s(code, n.name) == name)
        .map(|n| n.findex.0)
}

fn type_name_of(code: &Bytecode, rt: hlbc::types::RefType) -> Option<&str> {
    code.types
        .get(rt.0)
        .and_then(|t| t.get_type_obj())
        .map(|o| s(code, o.name))
}

/// Global-table index of a class's statics object, by the class's (`$`-prefixed)
/// statics type name. Engine recompiles renumber the global table, so a statics
/// object that used to sit at a pinned index must be found by its type instead.
fn find_statics_global(code: &Bytecode, statics_type_name: &str) -> Option<usize> {
    let t = find_type(code, statics_type_name)?;
    code.globals.iter().position(|g| g.0 == t)
}

/// findex of a function by short name, optionally constrained to a parent type.
fn find_fn(code: &Bytecode, name: &str, parent: Option<&str>) -> Option<usize> {
    code.functions
        .iter()
        .find(|f| {
            s(code, f.name) == name
                && match parent {
                    None => true,
                    Some(p) => f.parent.and_then(|rt| type_name_of(code, rt)) == Some(p),
                }
        })
        .map(|f| f.findex.0)
}

/// List a class's methods (proto names + findexes) — handy for discovery.
fn list_methods(code: &Bytecode, type_name: &str) {
    if let Some(ti) = find_type(code, type_name) {
        if let Some(o) = code.types[ti].get_type_obj() {
            eprintln!("  methods of {type_name} (type {ti}):");
            for p in &o.protos {
                eprintln!("    {} -> findex {}", s(code, p.name), p.findex.0);
            }
        }
    } else {
        eprintln!("  TYPE NOT FOUND: {type_name}");
    }
}

/// Human label for a findex: "Parent::name@findex" (or native).
fn fn_label(code: &Bytecode, findex: usize) -> String {
    if let Some(fi) = function_index_by_findex(code, findex) {
        let f = &code.functions[fi];
        let pn = f.parent.and_then(|rt| type_name_of(code, rt)).unwrap_or("?");
        return format!("{pn}::{}@{findex}", s(code, f.name));
    }
    if let Some(n) = code.natives.iter().find(|n| n.findex.0 == findex) {
        return format!("native {}.{}@{findex}", s(code, n.lib), s(code, n.name));
    }
    format!("?@{findex}")
}

/// Disassemble a function by findex, annotating call targets, fields, strings,
/// globals and the function's own register types / arg signature.
fn disasm(code: &Bytecode, findex: usize) {
    let Some(fi) = function_index_by_findex(code, findex) else {
        eprintln!("no function with findex {findex}");
        return;
    };
    let f = &code.functions[fi];
    let pn = f.parent.and_then(|rt| type_name_of(code, rt)).unwrap_or("?");
    eprintln!("== {pn}::{}@{findex} (fn #{fi}) ==", s(code, f.name));
    // register types
    for (i, r) in f.regs.iter().enumerate() {
        let tn = type_name_of(code, *r)
            .map(|x| x.to_string())
            .unwrap_or_else(|| format!("t{}", r.0));
        eprintln!("   reg{i}: {tn}");
    }
    let field_name = |rt: hlbc::types::RefType, idx: usize| -> String {
        // walk obj fields (flattened) — best-effort name lookup
        code.types
            .get(rt.0)
            .and_then(|t| t.get_type_obj())
            .and_then(|o| o.fields.get(idx).map(|fl| s(code, fl.name).to_string()))
            .unwrap_or_else(|| format!("#{idx}"))
    };
    for (i, op) in f.ops.iter().enumerate() {
        let mut note = String::new();
        match op {
            Opcode::Call0 { fun, .. }
            | Opcode::Call1 { fun, .. }
            | Opcode::Call2 { fun, .. }
            | Opcode::Call3 { fun, .. }
            | Opcode::Call4 { fun, .. }
            | Opcode::CallN { fun, .. } => note = format!("  ; {}", fn_label(code, fun.0)),
            Opcode::CallMethod { field, .. } | Opcode::CallThis { field, .. } => {
                note = format!("  ; method proto #{}", field.0)
            }
            Opcode::Field { obj, field, .. } | Opcode::SetField { obj, field, .. } => {
                let rt = f.regs.get(obj.0 as usize).copied();
                if let Some(rt) = rt {
                    note = format!("  ; .{} (of {})", field_name(rt, field.0),
                        type_name_of(code, rt).unwrap_or("?"));
                }
            }
            Opcode::String { ptr, .. } => {
                note = format!("  ; {:?}", code.strings[ptr.0].as_str())
            }
            Opcode::Int { ptr, .. } => note = format!("  ; ={}", code.ints[ptr.0]),
            Opcode::GetGlobal { global, .. } | Opcode::SetGlobal { global, .. } => {
                // is it a const string global?
                if let Some(consts) = &code.constants {
                    if let Some(c) = consts.iter().find(|c| c.global.0 == global.0) {
                        if let Some(&sidx) = c.fields.first() {
                            note = format!("  ; const str {:?}", code.strings[sidx].as_str());
                        }
                    }
                }
            }
            Opcode::Type { ty, .. } => {
                note = format!("  ; type {}", type_name_of(code, *ty)
                    .map(|x| x.to_string()).unwrap_or_else(|| format!("t{}", ty.0)))
            }
            Opcode::DynGet { field, .. } | Opcode::DynSet { field, .. } => {
                note = format!("  ; dynfield str {:?}", code.strings[field.0].as_str())
            }
            _ => {}
        }
        let lineinfo = f.debug_info.as_ref()
            .and_then(|d| d.get(i))
            .map(|(file, line)| {
                let fname = code.debug_files.as_ref()
                    .and_then(|files| files.get(*file))
                    .map(|s| s.as_str())
                    .unwrap_or("?");
                format!("  @{fname}:{line}")
            })
            .unwrap_or_default();
        eprintln!("  {i:4}:{lineinfo} {op:?}{note}");
    }
}

/// Print resolutions for everything the injection needs (validates the resolver).
fn inspect(code: &Bytecode) {
    let t = |n: &str| match find_type(code, n) {
        Some(i) => format!("type {i}"),
        None => "MISSING".into(),
    };
    let f = |n: &str, p: Option<&str>| match find_fn(code, n, p) {
        Some(i) => format!("findex {i}"),
        None => "MISSING".into(),
    };
    let fld = |tn: &str, fnm: &str| match find_type(code, tn).and_then(|ti| find_field(code, ti, fnm)) {
        Some(i) => format!("field {i}"),
        None => "MISSING".into(),
    };

    eprintln!("== TYPES ==");
    for n in [
        "sys.net.Socket",
        "sys.net.Host",
        "sys.net._Socket.SocketOutput",
        "sys.net._Socket.SocketInput",
        "String",
        "haxe.io.Bytes",
        "fraymakers.core.FraymakersMatchSettings",
        "pxf.core.MatchSettings",
    ] {
        eprintln!("  {n:50} {}", t(n));
    }

    eprintln!("== FUNCTIONS ==");
    for (n, p) in [
        ("__constructor__", Some("sys.net.Socket")),
        ("connect", Some("sys.net.Socket")),
        ("setBlocking", Some("sys.net.Socket")),
        ("__constructor__", Some("sys.net.Host")),
        ("ofString", Some("haxe.io.Bytes")),
        ("startMatch", None),
        ("createMatchSettings", None),
        ("createMatchSettingsConfig", None),
        ("onLoaded", Some("fraymakers.Main")),
        ("update", Some("fraymakers.Main")),
    ] {
        eprintln!("  {n:22} parent={p:?} -> {}", f(n, p));
    }

    eprintln!("== FIELDS ==");
    for (tn, fnm) in [
        ("sys.net.Socket", "output"),
        ("sys.net.Socket", "input"),
        ("pxf.core.MatchSettings", "matchConfig"),
        ("pxf.core.MatchSettings", "playerConfigs"),
    ] {
        eprintln!("  {tn}.{fnm:16} {}", fld(tn, fnm));
    }

    eprintln!("== METHODS (string-write candidates) ==");
    list_methods(code, "sys.net._Socket.SocketOutput");
    list_methods(code, "haxe.io.Output");
}

// ---- edit helpers -----------------------------------------------------------

fn add_string(code: &mut Bytecode, val: &str) -> usize {
    let i = code.strings.len();
    code.strings.push(hlbc::Str::from(val));
    i
}

/// Create a real `String` *object* constant and return its GLOBAL index.
/// HL's `String` opcode only yields raw bytes — actual String objects come from
/// constant-initialized globals: ConstantDef{global, fields:[strPoolIdx, intPoolIdxOfLen]}.
/// GetGlobal on the returned index yields a usable String (valid .bytes/.length).
fn add_string_const(code: &mut Bytecode, val: &str) -> usize {
    let str_idx = add_string(code, val);
    let len_idx = add_int(code, val.chars().count() as i32);
    let g_idx = code.globals.len();
    code.globals.push(hlbc::types::RefType(13)); // String
    code.constants
        .get_or_insert_with(Vec::new)
        .push(hlbc::types::ConstantDef {
            global: hlbc::types::RefGlobal(g_idx),
            fields: vec![str_idx, len_idx],
        });
    g_idx
}

// Fraymakers multiplayer: extra players (parts[4..]) are spawnPlayer'd into the live
// match one-per-frame from the update epilogue (currentMatch is null synchronously after
// startMatch). The PlayerConfig is built by the engine factory ClassFactory.
// defaultCreatePlayerConfig(playerVirtual@1957) — which runs the real netcode constructor
// (a bare `New PlayerConfig` left __bytesCache/__history null and crashed).
const FM_MULTIPLAYER: bool = true;

fn add_int(code: &mut Bytecode, val: i32) -> usize {
    // Reuse if already present (ints pool is small-ish); else append.
    if let Some(i) = code.ints.iter().position(|&x| x == val) {
        return i;
    }
    let i = code.ints.len();
    code.ints.push(val);
    i
}


// ---- headless match settings (data-file driven) -----------------------------

/// The peptide crate's source dir, captured at compile time. Used ONLY to
/// *locate* runtime asset files (commands.hsx, match_settings.conf,
/// peptide_ui.html) in a dev/source checkout — never to embed their content.
const PEPTIDE_CRATE_DIR: &str = env!("CARGO_MANIFEST_DIR");

/// Candidate locations for a peptide runtime asset file (e.g. `commands.hsx`),
/// tried in order. The first that exists wins. Resolution:
///   0. `$PEPTIDE_MATCH_SETTINGS` (match_settings.conf only — explicit file path)
///   1. `$PEPTIDE_ASSET_DIR/<rel>` (explicit override dir)
///   2. cwd-relative (`./<rel>`)
///   3. the packaged layout: a `data/` subfolder next to the binary (`<exe-dir>/data/<rel>`)
///   4. legacy packaged layout: directly next to the binary (`<exe-dir>/<rel>`)
///   5. `<exe-dir>/../../<rel>` (a `target/<profile>/<bin>` dev build → repo root)
///   6. the peptide crate's source dir (`PEPTIDE_CRATE_DIR/<rel>` — source checkout)
fn asset_candidate_paths(rel: &str) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    // File-specific override (highest priority): `$PEPTIDE_MATCH_SETTINGS` points a full
    // match_settings.conf path, the way the conf header documents — so you can iterate match
    // rules without editing the tracked default or the cwd copy.
    if rel == "match_settings.conf" {
        if let Ok(file) = std::env::var("PEPTIDE_MATCH_SETTINGS") {
            paths.push(std::path::PathBuf::from(file));
        }
    }
    if let Ok(dir) = std::env::var("PEPTIDE_ASSET_DIR") {
        paths.push(std::path::PathBuf::from(dir).join(rel));
    }
    paths.push(std::path::PathBuf::from(rel));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("data").join(rel)); // packaged: <exe-dir>/data/<rel>
            paths.push(dir.join(rel));              // legacy: directly next to the binary
            if let Some(up) = dir.parent().and_then(|p| p.parent()) {
                paths.push(up.join(rel));
            }
        }
    }
    paths.push(std::path::PathBuf::from(PEPTIDE_CRATE_DIR).join(rel));
    // peptide_ui.html lives under src/ in the source tree (commands.hsx and
    // match_settings.conf are at the crate root); cover both in a dev checkout.
    paths.push(std::path::PathBuf::from(PEPTIDE_CRATE_DIR).join("src").join(rel));
    paths
}

/// Read a peptide runtime asset file from disk (no compiled-in fallback). Panics
/// with the list of locations tried if it's missing — the asset files ship next
/// to the binary, so a missing one is a hard, loud error rather than a silent
/// stale-content bug.
fn read_asset(rel: &str) -> String {
    let tried = asset_candidate_paths(rel);
    for path in &tried {
        if let Ok(text) = std::fs::read_to_string(path) {
            eprintln!("peptide: loaded asset {}", path.display());
            return text;
        }
    }
    panic!(
        "peptide asset {rel:?} not found. Tried:\n{}\n\
         Ship it next to the binary, run from the repo, or set PEPTIDE_ASSET_DIR.",
        tried.iter().map(|p| format!("  - {}", p.display())).collect::<Vec<_>>().join("\n")
    );
}

/// Resolve the Fraymakers-side match settings from the SHARED source of truth
/// (`crate::interpreter::load_match_settings`, parsing the same
/// `match_settings.conf` SSF2 reads), so both engines spawn the same match.
/// All config *logic/values* live in interpreter.rs; the `s` bytecode merely
/// bakes these constants into the engine's `matchSettings` virtual (t675).
/// Returns the full struct — the builder maps each field onto t675's slots
/// (lives/time/itemFrequency=Int, damageRatio/startDamage/sizeRatio=Float,
/// teamAttack=Bool). Stamina has no FM equivalent and is a no-op here (mirror
/// of assists being a no-op on SSF2).
fn load_match_settings() -> crate::interpreter::MatchSettings {
    crate::interpreter::load_match_settings()
}

fn require_type(code: &Bytecode, name: &str) -> anyhow::Result<usize> {
    find_type(code, name).ok_or_else(|| anyhow::anyhow!("type not found: {name}"))
}
fn require_fn(code: &Bytecode, name: &str, parent: Option<&str>) -> anyhow::Result<usize> {
    find_fn(code, name, parent).ok_or_else(|| anyhow::anyhow!("fn not found: {name} parent={parent:?}"))
}
fn require_field(code: &Bytecode, tname: &str, fname: &str) -> anyhow::Result<usize> {
    let ti = require_type(code, tname)?;
    find_field(code, ti, fname).ok_or_else(|| anyhow::anyhow!("field not found: {tname}.{fname}"))
}

/// Resolve every entry in `manifest::MANIFEST` against a loaded bytecode, using the
/// SAME read-only resolvers the real patch uses (so the doctor can never disagree
/// with `connect_edit` about whether a symbol exists). Renders a live progress bar
/// when stderr is a TTY. Returns one `SymStatus` per symbol.
fn doctor(code: &Bytecode) -> Vec<manifest::SymStatus> {
    use manifest::{Kind, MANIFEST};
    let total = MANIFEST.len();
    let tty = manifest::is_tty();
    let mut out = Vec::with_capacity(total);
    for (i, sym) in MANIFEST.iter().enumerate() {
        let label = sym.label();
        // TTY -> in-place live bar; non-TTY (e.g. spawned by the GUI/TUI boot) ->
        // machine-parseable lines the parent captures to drive its own progress bar.
        if tty {
            manifest::render_live(i, total, &label);
        } else {
            manifest::emit_machine_progress(i, total, &label);
        }
        let resolved = match sym.kind {
            Kind::Fn => find_fn(code, sym.name, sym.parent),
            Kind::Native => find_native(code, sym.name),
            Kind::Type => find_type(code, sym.name),
            Kind::Field => sym
                .parent
                .and_then(|p| find_type(code, p))
                .and_then(|ti| find_field(code, ti, sym.name)),
        };
        out.push(manifest::SymStatus {
            group: sym.group,
            label,
            why: sym.why,
            critical: sym.critical,
            resolved,
        });
    }
    if tty {
        manifest::render_live(total, total, "done");
        manifest::clear_live();
    } else {
        manifest::emit_machine_progress(total, total, "done");
    }
    out
}

/// Run the manifest preflight, print the checklist, and ABORT if any critical
/// engine symbol is missing — so a layout change in a new Fraymakers build fails
/// loudly here, BEFORE any opcode is mutated, instead of silently corrupting the
/// patched binary. Called at the top of `connect_edit` and by the `doctor` mode.
fn run_preflight(code: &Bytecode) -> anyhow::Result<()> {
    let statuses = doctor(code);
    manifest::render_report(
        &statuses,
        &format!("Peptide preflight — Fraymakers bytecode v{}", code.version),
    );
    let (ok, miss_crit, warn) = manifest::summarize(&statuses);
    if !manifest::is_tty() {
        manifest::emit_machine_result(ok, miss_crit, warn);
    }
    if miss_crit > 0 {
        anyhow::bail!(
            "preflight: {miss_crit} critical engine symbol(s) missing — this Fraymakers \
             build is not compatible with the current patcher (see the checklist above \
             and DEVELOPMENT.md 'Surviving Fraymakers updates')"
        );
    }
    Ok(())
}

/// Insert `ops` at the front of function `fidx`, keeping debug_info length in
/// sync and clearing the (now stale) assigns table. Front insertion is safe for
/// HL's relative jumps (source and target shift together).
fn insert_ops_front(f: &mut hlbc::types::Function, ops: Vec<Opcode>) {
    let n = ops.len();
    for (i, op) in ops.into_iter().enumerate() {
        f.ops.insert(i, op);
    }
    if let Some(dbg) = f.debug_info.as_mut() {
        let first = dbg.first().copied().unwrap_or((0, 0));
        for _ in 0..n {
            dbg.insert(0, first);
        }
    }
    // IMPORTANT: the `assigns` (debug var-name -> op-index) table must stay
    // `Some` — HL always reads its count when debug info is present, so setting
    // it to None desyncs the stream ("Negative index" on load). Just shift the
    // op-indices to account for the prepended ops.
    if let Some(assigns) = f.assigns.as_mut() {
        for (_name, pos) in assigns.iter_mut() {
            *pos += n;
        }
    }
}

/// Append registers of the given type indices; returns the index of the first.
fn add_regs(f: &mut hlbc::types::Function, types: &[usize]) -> u32 {
    let base = f.regs.len() as u32;
    for &t in types {
        f.regs.push(hlbc::types::RefType(t));
    }
    base
}

/// Phase 1a: at onLoaded, connect a client socket to 127.0.0.1:<port> and send
/// `AUTH <token>` + a hello line. Proves socket injection + the auth handshake.
fn connect_edit(
    code: &mut Bytecode, port: u16, token: &str, headless: bool,
    char_name: Option<String>, stage_name: Option<String>, assist_name: Option<String>,
    install_dir: &str,
) -> anyhow::Result<()> {
    // Character/stage/assist baked as launch defaults. The char drives the self-bootstrap
    // (a custom .fra at <install>/custom/<char>/<char>.fra) and the bare-`s`/auto-launch
    // Base-game defaults when an arg is omitted: character → commandervideo, stage →
    // thespire (a real loadable stage), assist → commandervideoassist. All generic —
    // derived from the injector args; these are just the fallbacks. (Was impostor, but
    // impostor crashes the game on match load — see the open TODO.)
    let cname = char_name.as_deref().unwrap_or("commandervideo");
    let sname = stage_name.as_deref().unwrap_or("thespire");
    let aname = assist_name.as_deref().unwrap_or("commandervideoassist");
    let char_path = format!("{install_dir}/custom/{cname}/{cname}.fra");
    let char_pkgid = format!("{cname}.{cname}");
    let char_ns_key = format!("private::{cname}.{cname}");
    let char_resid = format!("private::{cname}");
    let stage_fqid = format!("public::{sname}.{sname}");
    let assist_fqid = format!("public::{aname}.{aname}");
    eprintln!("peptide: char={cname} stage={sname} assist={aname} headless={headless} path={char_path}");
    // Preflight: resolve the engine-symbol manifest first (read-only), show the
    // progress bar + doctor checklist, and ABORT before mutating anything if a
    // critical symbol is missing. This is the version-compatibility gate — a new
    // Fraymakers build that moved/renamed a depended-on symbol fails loudly here
    // instead of producing a corrupt patch.
    run_preflight(code)?;
    // Resolve everything by name (version-robust). Constructors have empty
    // names, so pin their findexes but verify their signatures.
    let sock_t = require_type(code, "sys.net.Socket")?;
    let host_t = require_type(code, "sys.net.Host")?;
    let out_t = require_type(code, "sys.net._Socket.SocketOutput")?;
    let str_t = require_type(code, "String")?;
    let connect = require_fn(code, "connect", Some("sys.net.Socket"))?;
    let write_str = require_fn(code, "writeString", Some("haxe.io.Output"))?;
    let flush = require_fn(code, "flush", Some("haxe.io.Output"))?;
    let out_field = require_field(code, "sys.net.Socket", "output")?;
    // Socket.init() (not the bare ctor) creates __s + input + output.
    let sock_init_method = require_fn(code, "init", Some("sys.net.Socket"))?;
    let ip_field = require_field(code, "sys.net.Host", "ip")?;
    let socket_init = find_native(code, "socket_init")
        .ok_or_else(|| anyhow::anyhow!("native socket_init not found"))?;
    // writeString is (Output, String, Encoding) — Haxe's optional ?encoding is a
    // required 3rd HL param. Grab the Encoding enum type so we can pass null
    // (ofString treats null encoding as UTF-8).
    let enc_t = {
        let wi = function_index_by_findex(code, write_str)
            .ok_or_else(|| anyhow::anyhow!("writeString fn missing"))?;
        code.types[code.functions[wi].t.0]
            .get_type_fun()
            .and_then(|tf| tf.args.get(2))
            .map(|a| a.0)
            .ok_or_else(|| anyhow::anyhow!("writeString has no encoding arg"))?
    };

    eprintln!("resolved: Socket={sock_t} Host={host_t} Out={out_t} connect={connect} writeString={write_str} out.field={out_field} Socket.init={sock_init_method} host.ip={ip_field} socket_init={socket_init}");

    // Constants. Skip Host.host_resolve (it SIGSEGVs mid-boot); set Host.ip
    // directly to 127.0.0.1 in network byte order (bytes 127,0,0,1 = 0x0100007F
    // on this little-endian host) and hand that to connect().
    const LOOPBACK_IP: i32 = 0x0100_007F; // 16777343
    let hello = format!("AUTH {token}\nHELLO_FROM_FRAYMAKERS\n");
    let port_idx = add_int(code, port as i32);
    let ip_const = add_int(code, LOOPBACK_IP);
    // Send the handshake byte-by-byte (writeByte) instead of writeString, which
    // routes through Bytes.ofString/utf16_to_utf8. ASCII out needs no conversion.
    let write_byte = require_fn(code, "writeByte", Some("sys.net._Socket.SocketOutput"))?;
    let set_blocking = require_fn(code, "setBlocking", Some("sys.net.Socket"))?;
    let recv_char = find_native(code, "socket_recv_char")
        .ok_or_else(|| anyhow::anyhow!("native socket_recv_char not found"))?;
    let s_field = require_field(code, "sys.net.Socket", "__s")?;
    let handle_t = code.types[sock_t]
        .get_type_obj()
        .and_then(|o| o.fields.get(s_field))
        .map(|fld| fld.t.0)
        .ok_or_else(|| anyhow::anyhow!("Socket.__s field type missing"))?;
    let hello_g = add_string_const(code, &hello); // real String object (GetGlobal)
    let ready_g = add_string_const(code, "READY\n"); // sent right after connect (engine is match-ready by preLoad)
    let pong_g = add_string_const(code, "PONG\n");
    let help_g = add_string_const(code, "help"); // console command to run as a test
    let ran_g = add_string_const(code, "RAN\n");
    let zero_idx = add_int(code, 0);
    let one_idx = add_int(code, 1);
    let p_idx = add_int(code, 'p' as i32); // 'ping' command byte
    let x_idx = add_int(code, 'x' as i32); // 'exit' command byte — clean engine shutdown
    // hxd.System.exit (Heaps app exit): clean process shutdown so the harness can quit the
    // engine WITHOUT `kill -9` mid-render (which leaves wedged U-state ./hl orphans on macOS).
    let hxd_exit = require_fn(code, "exit", Some("hxd.$System"))?;
    let c_idx = add_int(code, 'c' as i32); // 'console' command byte
    let s_idx = add_int(code, 's' as i32); // 'start match' (auto mode) command byte
    let s_versus_idx = add_int(code, 'S' as i32); // 'start match, FORCE versus' command byte (--versus)
    let stage_str = add_string(code, "stage");
    let character_str = add_string(code, "character");
    let port_str = add_string(code, "port");
    // distinct team per player so the match's hit detection treats them as opponents
    // (both default to -1 = same/unset, which suppresses inter-player hits). Dropped
    // harmlessly by ToVirtual if the player virtual has no `team` field.
    let team_str = add_string(code, "team");
    let matchrules_str = add_string(code, "matchRules");
    // defaultMatchRules is a static on pxf.core.$MatchSettings — the same value the
    // engine's MatchSettings ctor injects into its default matchConfig. We replace
    // field 1 (matchConfig) entirely, so we must re-supply matchRules or the match
    // scene null-derefs it on a later frame (clean exit 244, no crash.log).
    let ms_statics_t = find_type(code, "pxf.core.$MatchSettings")
        .ok_or_else(|| anyhow::anyhow!("$MatchSettings type not found"))?;
    let ms_global = code.globals.iter().position(|g| g.0 == ms_statics_t)
        .ok_or_else(|| anyhow::anyhow!("$MatchSettings statics global not found"))?;
    let dmr_field = find_field(code, ms_statics_t, "defaultMatchRules")
        .ok_or_else(|| anyhow::anyhow!("defaultMatchRules field not found"))?;
    let dmr_field_t = code.types[ms_statics_t]
        .get_type_obj()
        .and_then(|o| o.fields.get(dmr_field))
        .map(|fld| fld.t.0)
        .ok_or_else(|| anyhow::anyhow!("defaultMatchRules field type missing"))?;
    eprintln!("matchRules: $MatchSettings global={ms_global} field={dmr_field} fieldtype={dmr_field_t}");
    let resource_str = add_string(code, "resource");
    // Mode-based launch: create a real TrainingMode (createMode@18338 builds a
    // FraymakersMode whose hscript bridge + event wiring are set up), then call its
    // startMatch@6227 with {characters, matchSettings, pauseMenu}. This runs the
    // engine's own offline-match flow (gates transition, menu suspend/restore) —
    // no menu hand-teardown, and the mode stays alive to restore menus on match end.
    // Launch mode resource, selected at RUNTIME by roster size. trainingmode (the verified-stable
    // 1-2 player path, keeps the parity-harness "one fighter + one dummy" use case) is used for
    // <=2 players; vsmode (free-for-all) for 3-4. Both ids are baked; the `s` handler picks one
    // from parts.length (see the createMode site). `PEPTIDE_FORCE_TRAINING=1` pins trainingmode
    // (debug/comparison escape hatch).
    let trainingmode_g = add_string_const(code, "global::vsmode.trainingmode");
    let vsmode_g = add_string_const(code, "global::vsmode.vsmode");
    let force_training = std::env::var("PEPTIDE_FORCE_TRAINING").as_deref() == Ok("1");
    eprintln!("mode-launch: trainingmode + vsmode baked; runtime-selected by roster size (force_training={force_training})");
    let characters_str = add_string(code, "characters");
    let matchsettings_str = add_string(code, "matchSettings");
    let pausemenu_str = add_string(code, "pauseMenu");
    // Headless match settings (lives + timer) come from match_settings.conf
    // (see load_match_settings). Both fields exist in the matchSettings virtual
    // schema (t675: field 8 "lives", field 24 "time") and importJSON@5460 copies
    // them into the real MatchSettingsConfig (lives f16, time f19) during
    // _offlineMatchStart.
    let lives_str = add_string(code, "lives");
    let time_str = add_string(code, "time");
    let teamattack_str = add_string(code, "teamAttack");
    let teams_str = add_string(code, "teams");
    let itemfreq_str = add_string(code, "itemFrequency");
    let hazards_str = add_string(code, "hazards");
    let create_mode = require_fn(code, "createMode", Some("fraymakers.util.$FraymakersClassFactory"))?;
    let mode_start_match = require_fn(code, "startMatch", Some("fraymakers.core.FraymakersMode"))?;
    // FM's native pre-match loading screen factory. The menus set
    // MatchController.loadingScreenFactory before startMatch; the headless fast boot skips
    // the menus, so startMatch's `if (loadingScreenFactory != null)` gate fails and no
    // loading screen shows. We set the factory to the engine's own createLoadingScreen so
    // startMatch builds + shows it — parity with SSF2's loadingMenu. See the `s` handler.
    let create_loading_screen = require_fn(code, "createLoadingScreen", Some("pxf.controllers.$MatchController"))?;
    let mode_t = find_type(code, "fraymakers.core.FraymakersMode")
        .ok_or_else(|| anyhow::anyhow!("FraymakersMode type not found"))?;
    eprintln!("mode-launch: createMode={create_mode} FraymakersMode.startMatch={mode_start_match} FraymakersMode t={mode_t}");
    // line-command primitives (build a real String from received arg bytes, split it)
    let bytes_alloc = require_fn(code, "alloc", Some("haxe.io.$Bytes"))?;
    let bytes_set = require_fn(code, "set", Some("haxe.io.Bytes"))?;
    let bytes_getstring = require_fn(code, "getString", Some("haxe.io.Bytes"))?;
    let str_split = require_fn(code, "split", Some("String"))?;
    let space_g = add_string_const(code, " ");
    let buf_cap_idx = add_int(code, 512);
    let cfg = load_match_settings();
    eprintln!(
        "match-settings: lives={} time={} teamDamage={} itemFrequency={} hazards={} (FM applies lives/time/items/teamAttack/hazards; damage/size floats are SSF2-only)",
        cfg.lives, cfg.time, cfg.team_damage, cfg.item_frequency, cfg.hazards
    );
    // Int constant slots applied to the TrainingMode matchSettings virtual.
    // team_damage is a Bool emitted inline at the build site.
    let lives_idx = add_int(code, cfg.lives);
    let time_idx = add_int(code, cfg.time);
    let itemfreq_idx = add_int(code, cfg.item_frequency);
    let cfg_team_damage = cfg.team_damage;
    let cfg_hazards = cfg.hazards;
    let nl_idx = add_int(code, '\n' as i32);
    let two_idx = add_int(code, 2);
    let three_idx = add_int(code, 3);
    let four_idx = add_int(code, 4);
    let five_idx = add_int(code, 5);
    let six_idx = add_int(code, 6);
    eprintln!("line-cmd: alloc={bytes_alloc} set={bytes_set} getString={bytes_getstring} split={str_split}");
    // short-name resolution: a bare "sandbag" (no "::") is tried against each
    // namespace prefix; the first whose resource actually exists wins.
    let indexof_fn = require_fn(code, "indexOf", Some("String"))?;
    let str_add = require_fn(code, "__add__", Some("$String"))?;
    let getresid_fn = require_fn(code, "getResourceIdentifierString", Some("pxf.io.$ResourceManager"))?;
    let getpxf_fn = require_fn(code, "getPXFResource", Some("pxf.io.$ResourceManager"))?;
    // Match.spawnCharacter(resourceId, contentId) -> Character: adds a character to the
    // LIVE match. Used post-startMatch to add players 2..N (TrainingMode's startMatch only
    // takes one player at launch). Same two strings spawnPlayer derives from a PlayerConfig:
    // resourceId = getResourceIdentifierString(charRef), contentId = charRef.field(0).
    let spawn_char_fn = require_fn(code, "spawnCharacter", Some("pxf.core.Match"))?;
    // Match.spawnPlayer(PlayerConfig) -> Character: full player setup (spawn point, controller,
    // HUD, state init) — what makes a post-start-added char actually appear and play. The
    // PlayerConfig is built from a dynobj {character, port} via PlayerConfig.importJSON (it
    // DynGets character/port/lives/etc; unset fields default).
    let spawn_player_fn = require_fn(code, "spawnPlayer", Some("pxf.core.Match"))?;
    let pc_import_json = require_fn(code, "importJSON", Some("pxf.core.PlayerConfig"))?;
    let player_config_t = require_type(code, "pxf.core.PlayerConfig")?;
    // FraymakersClassFactory.createPlayerConfig(playerVirtual@1957) -> FraymakersPlayerConfig
    // (type 2536). Match.spawnPlayer -> createCharacter casts the config to the Fraymakers
    // SUBCLASS, so the BASE pxf.core.$ClassFactory.defaultCreatePlayerConfig (returns
    // pxf.core.PlayerConfig) throws "Can't cast ... to FraymakersPlayerConfig" — must use this.
    let create_player_config = require_fn(code, "createPlayerConfig", Some("fraymakers.util.$FraymakersClassFactory"))?;
    let pxfres_t = require_type(code, "pxf.structs.PXFResource")?;
    let nulli32_t2 = {
        let ci = function_index_by_findex(code, indexof_fn).unwrap();
        code.types[code.functions[ci].t.0].get_type_fun()
            .and_then(|tf| tf.args.get(2)).map(|a| a.0).unwrap()
    };
    let colon2_g = add_string_const(code, "::");
    let dot_g = add_string_const(code, ".");
    // namespace prefixes tried in order for bare names. private:: comes first so a
    // bare `s sandbag` resolves to the headless-loaded custom resource (the self-
    // bootstrap load + the `l` command both register it under private::sandbag);
    // built-in chars aren't in private::, so they harmlessly fall through to the rest.
    let ns_prefixes: Vec<usize> = ["private::", "custom::", "public::", "global::"]
        .iter().map(|p| add_string_const(code, p)).collect();
    // registry search: scan ResourceManager.pool (ArrayObj, f12) by index.
    // Index-loop is safe (no iterator-object hang). Pool is the ordered array that
    // addResource pushes every loaded resource onto (verified: addResource op44
    // hl.types.ArrayObj::push@89 on pool, md5 1b65af22). poolHash (f13) still
    // exists for getPXFResource lookups; we use pool for our safe iteration.
    // Statics-object globals: resolve by type name (the pinned indices 3508/3511/
    // 3458/3456/3449 drift every engine recompile). rm_g/mc_g/core_g replace the
    // literals everywhere below.
    let rm_g = find_statics_global(code, "pxf.io.$ResourceManager")
        .ok_or_else(|| anyhow::anyhow!("$ResourceManager statics global not found"))?;
    let mc_g = find_statics_global(code, "pxf.controllers.$MatchController")
        .ok_or_else(|| anyhow::anyhow!("$MatchController statics global not found"))?;
    let core_g = find_statics_global(code, "pxf.core.$CoreEngine")
        .ok_or_else(|| anyhow::anyhow!("$CoreEngine statics global not found"))?;
    eprintln!("statics globals (by name): RM={rm_g} MatchController={mc_g} CoreEngine={core_g}");
    // Engine methods called from injected ops, resolved by name (their findices drift
    // every recompile). Replace the old pinned literals 18287/18325/19543.
    let getresbyid_fn = require_fn(code, "getResourceByID", Some("pxf.io.$ResourceManager"))?;
    let cleanupmatch_fn = require_fn(code, "cleanupMatch", Some("pxf.controllers.$MatchController"))?;
    let destroymenus_fn = require_fn(code, "destroyAllActiveMenus", Some("pxf.controllers.$MenuController"))?;
    // fqid String -> parsed resource identifier (t669), round-tripped through
    // getResourceIdentifierString to canonicalize. Old pinned literal 18224.
    let parseresid_fn = require_fn(code, "parseResourceIdentifier", Some("pxf.io.$ResourceManager"))?;
    let rm_statics_t = code.globals[rm_g].0; // pxf.io.$ResourceManager statics
    let poolhash_field = find_field(code, rm_statics_t, "poolHash")
        .ok_or_else(|| anyhow::anyhow!("poolHash field not found"))?;
    let pool_field = find_field(code, rm_statics_t, "pool")
        .ok_or_else(|| anyhow::anyhow!("pool field not found on RM statics"))?;
    // getFullyQualifiedResourceId@1788 (on AbstractResource): returns the
    // poolHash key for a given resource, so we can build the correct resolved ref.
    let getfqid_fn = require_fn(code, "getFullyQualifiedResourceId", Some("pxf.io.AbstractResource"))?;
    // get_Loaded@1839: only fully-loaded resources have gone through createFromBytes
    // and have f17 set; unloaded stubs have null f17 and would crash on Field read.
    let get_loaded_fn = require_fn(code, "get_Loaded", Some("pxf.io.AbstractResource"))?;
    // AbstractResource type (394): safe intermediate type for GetArray from pool.
    let absres_t = require_type(code, "pxf.io.AbstractResource")?;
    eprintln!("pool: pool_field={pool_field} getfqid={getfqid_fn} get_loaded={get_loaded_fn} absres_t={absres_t}");
    let sm_keys = require_fn(code, "keys", Some("haxe.ds.StringMap"))?;
    let sm_exists = require_fn(code, "exists", Some("haxe.ds.StringMap"))?;
    let keysiter_t = {
        let ki = function_index_by_findex(code, sm_keys).unwrap();
        code.types[code.functions[ki].t.0].get_type_fun().map(|tf| tf.ret.0).unwrap()
    };
    let stringmap_t = require_type(code, "haxe.ds.StringMap")?;
    // per-content-type plain StringMap fields on PXFResource. There is no
    // assistPxfContentMap — assists are character-type entities, so the assist
    // search reuses characterPxfContentMap.
    let char_cmap_field = find_field(code, pxfres_t, "characterPxfContentMap")
        .ok_or_else(|| anyhow::anyhow!("characterPxfContentMap field not found"))?;
    let stage_cmap_field = find_field(code, pxfres_t, "stagePxfContentMap")
        .ok_or_else(|| anyhow::anyhow!("stagePxfContentMap field not found"))?;
    let assist_cmap_field = char_cmap_field;
    eprintln!("resolve: indexOf={indexof_fn} getResId={getresid_fn} getPXF={getpxf_fn} poolHash={poolhash_field} keys={sm_keys} exists={sm_exists} iter_t={keysiter_t} cmaps(char={char_cmap_field},assist={assist_cmap_field},stage={stage_cmap_field})");
    // fullIds must be REAL String objects (parseResourceIdentifier regex-matches them).
    // Baked from the injector args (bare-`s`/auto-launch defaults): char -> the
    // self-bootstrapped custom char (private::<char>), stage/assist -> public:: ids.
    // BARE default stage/assist names for slots the `s` caller omits. They must be
    // resolved through emit_resolve's bare-name path so the on-demand loader runs:
    // stage/assist are public:: base content that fast-boot filters out of the
    // required-load, so a parse-only of the full `::` id would leave a null resource
    // that getContentIdentifierString then null-derefs `.namespace`. (The char slot
    // uses g_name — set + loaded by the name-driven self-bootstrap — so no bare char
    // global is needed here.)
    let stage_bare_g = add_string_const(code, sname);
    let assist_bare_g = add_string_const(code, aname);
    // CUSTOM STAGE self-bootstrap (geometry-MVP): a converted SSF2 stage lives at
    // custom/<stage>/<stage>.fra but, unlike a built-in, is never registered in the
    // resource pool (fast-boot skips the bulk UGC scan), so Match.setupStage gets null.
    // If the baked stage has a custom .fra on disk (patch-time check, same gate idea as
    // base_p1 for the char), bake its private:: resid + path and emit a trimmed load
    // (build Resource + fetchThreaded + finishLoading + addResource) before the stage
    // resolve. No sprite-entity caching — a geometry-only stage has no art to render, its
    // collision/bounds/spawns come straight from the .entity. Built-in stages skip this
    // (no custom .fra) and resolve via the normal load-on-demand path.
    let stage_custom_path = format!("{install_dir}/custom/{sname}/{sname}.fra");
    let custom_stage = std::path::Path::new(&stage_custom_path).exists();
    let stage_resid_g = add_string_const(code, &format!("private::{sname}"));
    let stage_path_g = add_string_const(code, &stage_custom_path);
    let _ = (&char_resid, &stage_fqid, &assist_fqid); // full ids no longer baked (resolve from bare)
    let assist_str = add_string(code, "assist");
    let launched_g = add_string_const(code, "LAUNCHED\n");
    let launched2_g = add_string_const(code, "LAUNCHED "); // verbose ack prefix
    let sp_ok_g = add_string_const(code, "SP:1\n");   // DIAG: spawnPlayer returned a Character
    let sp_null_g = add_string_const(code, "SP:0\n");  // DIAG: spawnPlayer returned null
    let mr_g = add_string_const(code, "MR\n");   // DIAG: past emit_resolve
    let mi_g = add_string_const(code, "MI\n");   // DIAG: past importJSON
    let queued_g = add_string_const(code, "QUEUED\n"); // DIAG: s handler stashed pending players
    let pend_g = add_string_const(code, "PEND\n");     // DIAG: per-frame sees pending (non-idle)
    // Enhanced Peptide diagnostic for the null-assist case. Fraymakers' HUD
    // (DamageCounter.generateAssistSprite -> getContentIdentifierString) null-derefs `.namespace`
    // when a player has no assist, so rather than let that SIGSEGV the engine, Peptide skips the
    // player and emits this on the RESDIAG channel (collected into the crash/diag modal).
    let null_assist_diag_g = add_string_const(code,
        "RESDIAG: Peptide skipped an extra player — its assist did not resolve (null). Fraymakers' HUD \
         (DamageCounter.generateAssistSprite) requires a non-null assist; pass a valid assist id as the \
         3rd token of `s <char> <stage> <assist> …` so extra players inherit it.\n");
    let nl_g = add_string_const(code, "\n");
    let getcontentid_fn = require_fn(code, "getContentIdentifierString", Some("pxf.io.$ResourceManager"))?;
    // 'q' query command: report whether a match is live (answers "did it start?").
    let q_idx = add_int(code, 'q' as i32);
    let q_nomatch_g = add_string_const(code, "Q:NO_MATCH\n");
    let q_live_g = add_string_const(code, "Q:MATCH_LIVE\n");
    // 'k' command: dump pool keys (getFullyQualifiedResourceId for each pool element).
    // Reveals the ACTUAL namespace used by the headless UGC load path.
    let k_idx = add_int(code, 'k' as i32);
    let k_prefix_g = add_string_const(code, "K:");
    // diagnostics: did the local-UGC discovery queue/install any dirs?
    let k_dirs_pos_g = add_string_const(code, "K:DIRS_QUEUED>0\n");
    let k_dirs_zero_g = add_string_const(code, "K:DIRS_QUEUED=0\n");
    let k_inst_pos_g = add_string_const(code, "K:INSTALLED>0\n");
    let k_inst_zero_g = add_string_const(code, "K:INSTALLED=0\n");
    let _ = (k_inst_pos_g, k_inst_zero_g); // reserved for a follow-up installedUgc probe
    // UgcUtil statics g3449: directoriesToLoad=field 11 (ArrayObj),
    // installedUgc=field 10 (an object whose field 3 is an ArrayObj of items).
    let ugc_statics_g = find_statics_global(code, "fraymakers.util.$UgcUtil")
        .ok_or_else(|| anyhow::anyhow!("$UgcUtil statics global not found"))?;
    // Diagnostic: currentMatch (statics f6) is null right after `s` even when the
    // match is alive, because it's only set in onMatchReady. _matches (statics
    // f13, an ArrayObj) is pushed in the same onMatchReady, so its length tells us
    // whether a Match object actually exists yet. This disambiguates "match live,
    // q reads the wrong ref" (matches>0) from "match never started" (matches==0).
    let q_matches_g = add_string_const(code, "Q:MATCHES_NONEMPTY\n");
    let mc_statics_t = code.globals[mc_g].0; // pxf.controllers.$MatchController statics
    let cm_field = find_field(code, mc_statics_t, "currentMatch")
        .ok_or_else(|| anyhow::anyhow!("currentMatch field not found"))?;
    let matches_field = find_field(code, mc_statics_t, "_matches")
        .ok_or_else(|| anyhow::anyhow!("_matches field not found"))?;
    let match_t = find_type(code, "pxf.core.Match")
        .ok_or_else(|| anyhow::anyhow!("pxf.core.Match type not found"))?;
    // loadingScreenFactory field (+ its function type, for the scratch closure register).
    let lsf_field = find_field(code, mc_statics_t, "loadingScreenFactory")
        .ok_or_else(|| anyhow::anyhow!("loadingScreenFactory field not found"))?;
    let lsf_field_t = code.types[mc_statics_t]
        .get_type_obj()
        .and_then(|o| o.fields.get(lsf_field))
        .map(|fld| fld.t.0)
        .ok_or_else(|| anyhow::anyhow!("loadingScreenFactory field type missing"))?;
    eprintln!("query: $MatchController statics t={mc_statics_t} currentMatch field={cm_field} Match t={match_t}");
    // ---- move-dispatch + telemetry API (commands 'm' / 't') — resolved by name ----
    // Drive a move on the live player-0 Character via its own state machine
    // (Character.toState), and read its current state name (Character.getStateName)
    // — internal-function dispatch, NOT key-press simulation.
    let char_entity_t = require_type(code, "pxf.entity.Character")?;
    let to_state = require_fn(code, "toState", Some("pxf.entity.Character"))?;
    let get_state_name = require_fn(code, "getStateName", Some("pxf.entity.Character"))?;
    let cstate_statics_t = find_type(code, "pxf.entity.$CState")
        .ok_or_else(|| anyhow::anyhow!("pxf.entity.$CState type not found"))?;
    let cstate_global = code.globals.iter().position(|g| g.0 == cstate_statics_t)
        .ok_or_else(|| anyhow::anyhow!("$CState statics global not found"))?;
    let jab_field = find_field(code, cstate_statics_t, "JAB")
        .ok_or_else(|| anyhow::anyhow!("CState.JAB field not found"))?;
    let characters_field = find_field(code, match_t, "characters")
        .ok_or_else(|| anyhow::anyhow!("Match.characters field not found"))?;
    // Match collections walked by `tree` (the FM analog of the SSF2 display list). gameObjects alone
    // isn't the union, so the walk sweeps characters + custom game objects + projectiles; the runtime
    // class name on each entity disambiguates the kind.
    let cgo_field = find_field(code, match_t, "customGameObjects")
        .ok_or_else(|| anyhow::anyhow!("Match.customGameObjects field not found"))?;
    let proj_field = find_field(code, match_t, "projectiles")
        .ok_or_else(|| anyhow::anyhow!("Match.projectiles field not found"))?;
    let structures_field = find_field(code, match_t, "structures")
        .ok_or_else(|| anyhow::anyhow!("Match.structures field not found"))?;
    // The union. Swept in ADDITION to the specific collections above (not instead of them):
    // it's the only place vfx show up, and converted animated backdrop elements are vfx, so
    // without it the tree is blind to exactly the objects stage porting gets wrong.
    let gameobjects_field = find_field(code, match_t, "gameObjects")
        .ok_or_else(|| anyhow::anyhow!("Match.gameObjects field not found"))?;
    // ---- the stage's display layers ----
    // Vfx are NOT GameObjects (Vfx extends Entity), so they can never appear in any Match
    // collection. Converted animated backdrop elements ARE vfx, reparented into one of the
    // stage's eleven named layer containers, so the only way to see them is to walk those
    // containers. That walk is also the like-for-like counterpart of the SSF2 display list.
    //
    // Reached by constructing a StageApi over the live stage entity and calling its getters
    // DIRECTLY. The same calls are unreachable from the interpreter Peptide drives (ApiObject
    // dispatch lands with a null receiver there) but are fine as typed bytecode calls.
    let stage_api_ctor = require_fn(code, "__constructor__", Some("pxf.api.$StageApi"))?;
    let stage_api_t = require_type(code, "pxf.api.StageApi")?;
    let container_api_t = require_type(code, "pxf.api.ContainerApi")?;
    let disp_container_t = require_type(code, "pxf.display.Container")?;
    let capi_container_field = require_field(code, "pxf.api.ContainerApi", "__container__")?;
    // Counted off the heaps object's own child array, not pxf.display.Container.numChildren:
    // that field reads 0 even for the CHARACTERS layer, so it isn't maintained on the paths
    // the engine actually adds children through.
    let h2d_object_t = require_type(code, "h2d.Object")?;
    let cont_display_field = require_field(code, "pxf.display.Container", "_displayObject")?;
    let h2d_children_field = require_field(code, "h2d.Object", "children")?;
    // ---- screenshot chain (the 'y' command) ----
    // Structure is not sight: the walk above can prove an object exists, is visible, sized and
    // positioned, and still say nothing about whether a single pixel of it reaches the screen.
    // Heaps can read its own framebuffer back, so the engine takes its own screenshot and the
    // PNG comes home over the existing socket as hex (the stock-icon path's trick) — no file
    // written engine-side, and no dependence on the host being able to capture the window, which
    // it cannot: a headless session's window has no capturable surface.
    //
    // This captures the SCENE, not the camera view, so the frame can contain art the camera
    // crops away and carries no camera zoom. Same caveat on the SSF2 side. See the `shot` doc on
    // DebugTarget: set the camera before comparing frames across engines.
    let h2d_getscene = require_fn(code, "getScene", Some("h2d.Object"))?;
    let scene_capturebitmap = require_fn(code, "captureBitmap", Some("h2d.Scene"))?;
    let tile_gettexture = require_fn(code, "getTexture", Some("h2d.Tile"))?;
    let tex_capturepixels = require_fn(code, "capturePixels", Some("h3d.mat.Texture"))?;
    let pixels_topng = require_fn(code, "toPNG", Some("hxd.Pixels"))?;
    let bytes_tohex = require_fn(code, "toHex", Some("haxe.io.Bytes"))?;
    let h2d_scene_t = require_type(code, "h2d.Scene")?;
    let h2d_bitmap_t = require_type(code, "h2d.Bitmap")?;
    let h2d_tile_t = require_type(code, "h2d.Tile")?;
    let bitmap_tile_field = require_field(code, "h2d.Bitmap", "tile")?;
    let texture_t = require_type(code, "h3d.mat.Texture")?;
    let pixels_t = require_type(code, "hxd.Pixels")?;
    let iobytes_t = require_type(code, "haxe.io.Bytes")?;
    // The optional args (capture layer / mip level / crop bounds, png level) are read off the
    // callee's own signature rather than hardcoded: they are nullable primitives with no class
    // name to resolve, and pinning their type indices would silently rot on an engine update.
    // `require_fn` yields a FINDEX, which is not a position in `code.functions` — look the
    // function up by findex before reading its signature.
    let arg_t = |fun: usize, i: usize| -> anyhow::Result<usize> {
        let f = code.functions.iter().find(|f| f.findex.0 == fun)
            .ok_or_else(|| anyhow::anyhow!("no function with findex {fun}"))?;
        code.types[f.t.0].get_type_fun()
            .and_then(|tf| tf.args.get(i).map(|a| a.0))
            .ok_or_else(|| anyhow::anyhow!("arg {i} of fn {fun} has no type"))
    };
    let cap_layer_t = arg_t(tex_capturepixels, 1)?;
    let cap_bounds_t = arg_t(tex_capturepixels, 3)?;
    let png_level_t = arg_t(pixels_topng, 1)?;
    // Per-child readout, so a layer reports its actual CONTENTS rather than just a count.
    // This is the Fraymakers counterpart of the SSF2 display-list walk: one row per object
    // with the same facts (class, name, position, visibility), so the two engines' stages can
    // be diffed object by object instead of compared by totals.
    let h2d_x_field = require_field(code, "h2d.Object", "x")?;
    let h2d_y_field = require_field(code, "h2d.Object", "y")?;
    let h2d_vis_field = require_field(code, "h2d.Object", "visible")?;
    // absX/absY: where the sprite ACTUALLY renders in absolute space. A display child's own
    // x/y is local to its container, so it reads (0,0) and says nothing about placement; the
    // absolute pair is what an SSF2 world position can be compared against, and what makes the
    // stage-local -> absolute transform measurable instead of guessed.
    let disp_absx_field = require_field(code, "pxf.display.HeapsContainer", "absX")?;
    let disp_absy_field = require_field(code, "pxf.display.HeapsContainer", "absY")?;
    // Pixel size, the figure SSF2 reports per display object. The entity's `dimensions`
    // component is null in this engine, so it comes from the DISPLAY side instead:
    // `h2d.Object.getSize(out)` returns a Bounds, and the size is the max/min difference.
    let h2d_getsize = require_fn(code, "getSize", Some("h2d.Object"))?;
    let bounds_t = require_type(code, "h2d.col.Bounds")?;
    let b_xmin = require_field(code, "h2d.col.Bounds", "xMin")?;
    let b_ymin = require_field(code, "h2d.col.Bounds", "yMin")?;
    let b_xmax = require_field(code, "h2d.col.Bounds", "xMax")?;
    let b_ymax = require_field(code, "h2d.col.Bounds", "yMax")?;
    let h2d_name_field = require_field(code, "h2d.Object", "name")?;
    // Child slots swept per layer. bowserscastle's busiest layer holds ~41, so 48 covers it;
    // the group header carries the TRUE length either way, so a layer that outruns this reads
    // as truncated rather than silently short.
    let layer_child_idx: Vec<usize> = (0..48).map(|i| add_int(code, i)).collect();
    // Display rows are emitted as FIELDS, not prose: the host renders them with the same
    // `fmt_stage_node` the SSF2 walk uses, so one engine can't drift into its own layout and
    // make the two stages undiffable. `N` marks a node row; fields are tab-separated.
    let tree_kid_g = add_string_const(code, "N\t");
    let tree_hidden_g = add_string_const(code, "0");
    let tree_visible_g = add_string_const(code, "1");
    let tree_unnamed_g = add_string_const(code, "");
    let tree_space_g = add_string_const(code, "\t");   // field separator for N-rows
    // ---- the tick stack ----
    // Vfx are Entities but appear in NO Match collection, so a converted backdrop element is
    // invisible to the collection sweep. Everything that updates per frame is on the tick
    // stack, so that's where they are. Walked by class name ONLY: the list is heterogeneous
    // (entities alongside systems and components), and calling an Entity accessor on a
    // non-entity would take the engine down.
    let tickstack_field = require_field(code, "pxf.core.Match", "tickStack")?;
    let tickables_field = require_field(code, "pxf.core.TickStack", "m_tickables")?;
    let tickstack_t = require_type(code, "pxf.core.TickStack")?;
    let dllist_t = require_type(code, "pxf.util.DLinkedList")?;
    let dlnode_t = require_type(code, "pxf.util.DLinkedListNode")?;
    let dl_first_field = require_field(code, "pxf.util.DLinkedList", "first")?;
    let dl_len_field = require_field(code, "pxf.util.DLinkedList", "length")?;
    let dl_value_field = require_field(code, "pxf.util.DLinkedListNode", "value")?;
    let dl_next_field = require_field(code, "pxf.util.DLinkedListNode", "next")?;
    let tree_tick_g = add_string_const(code, "  [tickables] n=");
    // Field names for DYNAMIC access. A tick-stack entry is a boxed `Dynamic`, and neither an
    // unchecked nor a checked cast yields a usable Entity pointer from one (both read garbage:
    // 3.1e-314 / NaN). DynGet reads a field off the dynamic BY NAME instead, and returns null
    // for a field the object doesn't have — so it needs no type guard and can't crash the
    // engine on a heterogeneous list. This is what makes the Fraymakers side able to report an
    // animation clock at all, which is the axis the SSF2 walk already had.
    let f_animation = add_string(code, "animation");
    let f_currentframe = add_string(code, "currentFrame");
    let f_totalframes = add_string(code, "totalFrames");
    let f_currentanim = add_string(code, "currentAnimation");
    let f_body = add_string(code, "body");
    // Pixel size, so a record can be compared against SSF2's width/height.
    //
    // `dimensions` is the obvious source and it does NOT work: the component is declared on
    // the entity but is null at runtime — `p0.dimensions` reads null through the interpreter
    // while `p0.body.x` reads 328.315, so it isn't a reflection problem. Left wired (the
    // fields simply come back empty) rather than removed, because the real source is
    // `h2d.Object::getBounds()` on the display side, which is a METHOD call and so needs more
    // than the DynGet field reads the rest of this row uses.
    let f_dimensions = add_string(code, "dimensions");
    let f_width = add_string(code, "width");
    let f_height = add_string(code, "height");
    let f_x = add_string(code, "x");
    let f_y = add_string(code, "y");
    // Hops to follow. bowserscastle's list is 295 entries and the first ~90 are all
    // ItemSpawnPoints, so a short walk sees nothing but items (which aren't ported anyway)
    // and truncates before the interesting entities. The header carries the true length, so
    // a list longer than this still reads as truncated rather than silently short.
    const TICK_HOPS: usize = 320;
    // In VfxLayer order (back to front) — the same eleven the layer constants name.
    const STAGE_LAYERS: [&str; 11] = [
        "BACKGROUND_BEHIND", "BACKGROUND_STRUCTURES", "BACKGROUND_SHADOWS", "BACKGROUND_EFFECTS",
        "CHARACTERS_BACK", "CHARACTERS", "CHARACTERS_FRONT",
        "FOREGROUND_STRUCTURES", "FOREGROUND_SHADOWS", "FOREGROUND_EFFECTS", "FOREGROUND_FRONT",
    ];
    let layer_getters: Vec<usize> = [
        "getBackgroundBehindContainer", "getBackgroundStructuresContainer",
        "getBackgroundShadowsContainer", "getBackgroundEffectsContainer",
        "getCharactersBackContainer", "getCharactersContainer", "getCharactersFrontContainer",
        "getForegroundStructuresContainer", "getForegroundShadowsContainer",
        "getForegroundEffectsContainer", "getForegroundFrontContainer",
    ].iter().map(|n| require_fn(code, n, Some("pxf.api.StageApi"))).collect::<anyhow::Result<_>>()?;
    // The live stage, bound into the interpreter scope as `stage` rather than walked (it's a
    // single object, not a collection). The Fraymakers script API gives every entity script an
    // ambient `self`/`match`/`camera`/`stage`, where `stage` is the API class — the one with
    // getCurrentFrame/getTotalFrames and the per-layer display containers. So `stage` here is
    // that same API object, reached entity -> script wrapper -> api. Binding the ENTITY instead
    // looks like it works (it reflects) but is the wrong object: a stage entity carries no
    // body/animation/stageStats components, so every interesting field reads null.
    let stage_entity_t = require_type(code, "pxf.entity.Stage")?;
    let stage_field = find_field(code, match_t, "stageEntity")
        .ok_or_else(|| anyhow::anyhow!("Match.stageEntity field not found"))?;
    // NOT bound, and the reason is a HARD BOUNDARY worth knowing before trying again: what the
    // interpreter can dispatch on is per-CLASS, not per-surface. Entity classes are fine
    // (p0.getStateName() works). Everything else tried BINDS correctly and then fails on first
    // use, several different ways:
    //   - pxf.api.StageApi                    calls land with a null receiver
    //   - pxf.api.MatchApi                    takes the ENGINE DOWN on the first method call
    //   - Match.matchApiGlobals               binds safely as a virtual, but carries no
    //                                         getCamera/getCharacters, and its getter has no
    //                                         callers, so it is not what scripts are handed either
    //   - Match.camera, Match.matchSettings   bind and identify correctly, then take the ENGINE
    //                                         DOWN on the first FIELD read (camera.body.x)
    // A subclassed interpreter is not the answer either: one hscript.Interp class serves
    // everything. Each of these went through a correctly TYPED register (an arbitrary spare
    // register reads back as whatever it was declared to hold), so the binding is not the fault.
    // An interpreted script is handed a working ambient `match` by SOME path that readies the
    // interpreter per run, and Peptide calls interpretScript directly, below it. Finding that path
    // is the work; the binding is one line once it is known. Until then anything only these
    // objects can answer -- where the camera is, which switches the match was started with --
    // costs a convert-export-boot cycle, which is what makes it worth doing.
    // Entity methods (p0.getStateName()) work, API
    // methods do not, so the sandbox has to be reached in bytecode the way `tree` is.
    let m_idx = add_int(code, 'm' as i32);
    let t_idx = add_int(code, 't' as i32);
    let m_ack_g = add_string_const(code, "M:JAB\n");
    let m_ok_g = add_string_const(code, "M:OK\n"); // generic move-dispatch ack (move-by-name)
    let m_nomatch_g = add_string_const(code, "M:NOMATCH\n");
    // Move-by-name dispatch table. The client (bridge) sends `m <letter>` where the
    // selector byte = 'A' + ordinal (ordinal = index into interpreter::MOVES). We resolve
    // each move's CState field by NAME here (robust to findex/field drift), so the
    // engine-side jump table is GENERATED from the same shared table the client uses —
    // not hand-written per move. A move whose CState field is absent in this build
    // falls back to JAB (logged) so the dispatch never reads a bogus field.
    let space_idx = add_int(code, ' ' as i32);
    let negone_idx = add_int(code, -1);
    let move_fields: Vec<(usize, usize)> = interpreter::MOVES.iter().enumerate().map(|(i, (mv, fld))| {
        let fidx = find_field(code, cstate_statics_t, fld).unwrap_or_else(|| {
            eprintln!("move-dispatch: CState.{fld} (move {mv:?}) not found — falling back to JAB");
            jab_field
        });
        (add_int(code, 'A' as i32 + i as i32), fidx) // (letter int-pool idx, CState field idx)
    }).collect();
    eprintln!("move-dispatch: {} moves resolved (selector 'A'..); see interpreter::MOVES", move_fields.len());
    // ---- 'v' command: physics/vitals readback (criterion #6 numeric telemetry) ----
    // Reads player-0 Character body.x/y, physics.currentVelocityX/Y, damage._damage —
    // all Float (t6) — boxes each via ToDyn and formats with Std.string, then writes
    // one "P: x=.. y=.. vx=.. vy=.. dmg=..\n" line. Resolved by name (drift-robust).
    let v_idx = add_int(code, 'v' as i32);
    let std_string = find_fn(code, "string", Some("$Std"))
        .or_else(|| find_fn(code, "string", Some("Std")))
        .unwrap_or(5791); // $Std::string(Dynamic):String
    let body_t = require_type(code, "pxf.components.Body")?;
    let physics_t = require_type(code, "pxf.components.Physics")?;
    let damage_t = require_type(code, "pxf.components.Damage")?;
    // ---- 'w' (tree walk): symbols to read each gameObject's class + position ----
    let w_idx = add_int(code, 'w' as i32);
    let y_idx = add_int(code, 'y' as i32);
    let output_t = require_type(code, "haxe.io.Output")?;
    let entity_t = require_type(code, "pxf.entity.Entity")?;
    let entity_getx = require_fn(code, "getX", Some("pxf.entity.Entity"))?;
    let entity_gety = require_fn(code, "getY", Some("pxf.entity.Entity"))?;
    let type_getclass = require_fn(code, "getClass", Some("$Type"))?;
    let type_getclassname = require_fn(code, "getClassName", Some("$Type"))?;
    let tree_hdr_g = add_string_const(code, "TREE:\n");
    let tree_indent_g = add_string_const(code, "  ");
    let tree_at_g = add_string_const(code, " @(");
    let tree_comma_g = add_string_const(code, ",");
    let tree_close_g = add_string_const(code, ")\n");
    let tree_nomatch_g = add_string_const(code, "TREE: (no live match)\n");
    let shot_hdr_g = add_string_const(code, "SHOT:");
    let shot_fail_g = add_string_const(code, "SHOT: (no frame)\n");
    // slot-index constants for the UNROLLED walk (a bytecode loop whose body resolves/allocates
    // corrupts its counter at runtime — same phenomenon as the multiplayer extra-builds; unrolling
    // with fixed indices sidesteps it entirely).
    // Slot indices for the unrolled sweep. gameObjects is the UNION collection and on a real
    // stage it runs long (characters + hazards + every structure + every vfx), so it gets a
    // deeper set than the specific collections.
    let tree_slot_idx: Vec<usize> = (0..24).map(|i| add_int(code, i)).collect();
    // Per-collection group headers. They carry the collection's TRUE length, so a collection
    // longer than its slot budget reads as truncated instead of silently short — and they make
    // the gameObjects duplicates interpretable (a character appears in both its own collection
    // and the union).
    let tree_grp_g: Vec<usize> = ["characters", "customGameObjects", "projectiles", "structures", "gameObjects"]
        .iter().map(|n| add_string_const(code, &format!("  [{n}] n="))).collect();
    let tree_nl_g = add_string_const(code, "\n");
    let tree_layer_g: Vec<usize> = STAGE_LAYERS.iter()
        .map(|n| add_string_const(code, &format!("  [layer {n}] n="))).collect();
    // bare layer names, stamped on each display row so an element carries its own depth slot
    let tree_layername_g: Vec<usize> = STAGE_LAYERS.iter()
        .map(|n| add_string_const(code, n)).collect();
    let char_body_f = require_field(code, "pxf.entity.Character", "body")?;
    let char_physics_f = require_field(code, "pxf.entity.Character", "physics")?;
    let char_damage_f = require_field(code, "pxf.entity.Character", "damage")?;
    let body_x_f = find_field(code, body_t, "x").ok_or_else(|| anyhow::anyhow!("Body.x"))?;
    let body_y_f = find_field(code, body_t, "y").ok_or_else(|| anyhow::anyhow!("Body.y"))?;
    let phys_vx_f = find_field(code, physics_t, "currentVelocityX").ok_or_else(|| anyhow::anyhow!("Physics.currentVelocityX"))?;
    let phys_vy_f = find_field(code, physics_t, "currentVelocityY").ok_or_else(|| anyhow::anyhow!("Physics.currentVelocityY"))?;
    let dmg_f = find_field(code, damage_t, "_damage").ok_or_else(|| anyhow::anyhow!("Damage._damage"))?;
    let p_pre_g = add_string_const(code, "P:");
    let p_x_g = add_string_const(code, " x=");
    let p_y_g = add_string_const(code, " y=");
    let p_vx_g = add_string_const(code, " vx=");
    let p_vy_g = add_string_const(code, " vy=");
    let p_dmg_g = add_string_const(code, " dmg=");
    let p_nomatch_g = add_string_const(code, "P:NOMATCH\n");
    eprintln!("physics: Std.string={std_string} body.f={char_body_f} physics.f={char_physics_f} damage.f={char_damage_f}");
    // ---- hscript eval pipeline ('e' command): the engine bundles full hscript — the
    // same Parser + Interp that runs every character/assist script. `e` parses a script
    // string and executes it, so handler logic is readable Haxe text instead of
    // hand-emitted bytecode. SPIKE: a hardcoded script proves the in-engine pipeline;
    // the socket-driven arbitrary-script form (and engine-class registration) follow. ----
    let e_idx = add_int(code, 'e' as i32);
    let hs_parser_ctor = find_fn(code, "__constructor__", Some("hscript.$Parser")).unwrap_or(2284);
    let hs_parse = require_fn(code, "parseString", Some("hscript.Parser"))?;
    let hs_interp_ctor = find_fn(code, "__constructor__", Some("hscript.$Interp")).unwrap_or(2235);
    let hs_execute = require_fn(code, "execute", Some("hscript.Interp"))?;
    let hs_setvar = require_fn(code, "setVar", Some("hscript.Interp"))?;
    // The engine NEVER runs a bare interp: Main::init registers FraymakersScriptGlobals.
    // applyInterpreterGlobals as the globals-callback, and runs every program through
    // ApiScript.interpretScript (resets depth/declared, runs exprReturn, TRAPS errors).
    // We mirror that exactly so our top-scope interp is "loaded + ready" like the engine's.
    let hs_apply_globals = find_fn(code, "applyInterpreterGlobals", Some("fraymakers.api.$FraymakersScriptGlobals")).unwrap_or(18218);
    let hs_interp_script = find_fn(code, "interpretScript", Some("pxf.api.$ApiScript")).unwrap_or(2202);
    let eval_p0_g = add_string_const(code, "p0");  // bound to player-0 Character before each eval
    let eval_p1_g = add_string_const(code, "p1");  // bound to player-1 Character (null until 2+ players)
    let eval_p2_g = add_string_const(code, "p2");  // bound to player-2 Character (null until 3+ players)
    let eval_p3_g = add_string_const(code, "p3");  // bound to player-3 Character (null until 4 players)
    let eval_match_g = add_string_const(code, "match"); // bound to MatchController.currentMatch each eval
    let eval_chars_g = add_string_const(code, "characters"); // bound to the live character ArrayObj each eval
    let eval_stage_ent_g = add_string_const(code, "stageEntity"); // the raw entity behind it (tooling)
    // The command implementations, in hscript (ported from bytecode). Loaded ONCE into
    // the interp after applyInterpreterGlobals; every friendly command calls into these.
    // commands.hsx holds the end-user-facing helpers; the host-owned matchStatus icon
    // bridge (driven only by the GUI) lives in interpreter::UI_BRIDGE_HSX and is appended
    // here so both parse into the same interp at boot.
    let commands_g = add_string_const(
        code,
        &format!("{}\n{}", read_asset("commands.hsx"), interpreter::UI_BRIDGE_HSX),
    );
    // Bound into scope so commands.hsx's __eval() can parse+run the user command inside an
    // hscript try/catch (crash-proofing): __interp = the interp itself, __parser = a Parser,
    // __cmd = the raw user command line (set per-eval).
    let eval_interp_g = add_string_const(code, "__interp");
    let eval_parser_g = add_string_const(code, "__parser");
    let eval_cmd_g = add_string_const(code, "__cmd");
    // Crash-proofing: the eval hook wraps parse+exprReturn in a Trap. On ANY error it
    // logs to the engine (Sys.println -> engine log) and returns "ERR: <msg>" to Peptide.
    let eval_td_g = add_string_const(code, "__td"); // Tildebugger statics, for the Engine.log facade
    let err_prefix_g = add_string_const(code, "ERR: ");
    let err_log_prefix_g = add_string_const(code, "[peptide] eval error: ");
    let hs_expr_return = find_fn(code, "exprReturn", Some("hscript.Interp")).unwrap_or(2216);
    // Tildebugger.log(msg, ?color, ?flag) — the engine console log that Engine.log uses.
    let tilde_log = find_fn(code, "log", Some("pxf.core.$Tildebugger")).unwrap_or(17883);
    let hs_arr_ctor: usize = 262; // ArrayObj ctor used to reset Interp.declared (see ApiScript.interpretScript)
    let eval_cs_g = add_string_const(code, "CS");  // bound to the CState statics (move-id source)
    let hs_parser_t = require_type(code, "hscript.Parser")?;
    let hs_interp_t = require_type(code, "hscript.Interp")?;
    // hscript.Expr is an unnamed enum (t396) — not name-resolvable; index confirmed by
    // both parseString's return type and execute's 2nd arg. (Same hardcoded-index style
    // as RT(1957)/alloc_array=256 elsewhere; re-derive if the engine build changes.)
    let hs_expr_t: u32 = 396;
    let eval_script_g = add_string_const(code, "1 + 2");
    let eval_prefix_g = add_string_const(code, "E:");
    let eval_nl_g = add_string_const(code, "\n");
    eprintln!("eval: parseString={hs_parse} execute={hs_execute} Parser_t={hs_parser_t} Interp_t={hs_interp_t} Expr_t={hs_expr_t} parserCtor={hs_parser_ctor} interpCtor={hs_interp_ctor}");
    // ---- 'a' command: animation introspection (name + frame index/total) ----
    // Reads Character.animation -> currentAnimation (String) / currentFrame /
    // totalFrames (Int), writes "A:<name> frame <cur>/<total>". Lets the agent (and
    // a modder) see exactly where in an animation the character is — the observation
    // half of the drive-observe-iterate loop. Resolved by name (drift-robust).
    let a_idx = add_int(code, 'a' as i32);
    let anim_t = require_type(code, "pxf.components.Animation")?;
    let char_anim_f = require_field(code, "pxf.entity.Character", "animation")?;
    let anim_name_f = find_field(code, anim_t, "currentAnimation").ok_or_else(|| anyhow::anyhow!("Animation.currentAnimation"))?;
    let anim_cur_f = find_field(code, anim_t, "currentFrame").ok_or_else(|| anyhow::anyhow!("Animation.currentFrame"))?;
    let anim_total_f = find_field(code, anim_t, "totalFrames").ok_or_else(|| anyhow::anyhow!("Animation.totalFrames"))?;
    let a_pre_g = add_string_const(code, "A:");
    let a_frame_g = add_string_const(code, " frame ");
    let a_slash_g = add_string_const(code, "/");
    let a_nomatch_g = add_string_const(code, "A:NOMATCH\n");
    eprintln!("anim: Animation t={anim_t} char.anim.f={char_anim_f} name={anim_name_f} cur={anim_cur_f} total={anim_total_f}");
    // ---- 'f' (anim frame-step) + 'g' (anim resume): animation scrubbing ----
    // `step` pauses playback (Character.pauseAnimationPlayback) and advances one
    // frame via Animation.playFrame(anim, currentFrame+1), then reports A:<name>
    // frame cur/total — frame-by-frame inspection of a move. `play` resumes.
    let f_idx = add_int(code, 'f' as i32);
    let g_idx = add_int(code, 'g' as i32);
    let n_idx = add_int(code, 'n' as i32); // 'addCharacter' command byte (#3)
    let pause_field = require_field(code, "pxf.entity.Character", "pauseAnimationPlayback")?;
    let play_frame = require_fn(code, "playFrame", Some("pxf.components.Animation"))?;
    let g_ack_g = add_string_const(code, "PLAY\n");
    eprintln!("scrub: pauseAnimationPlayback.f={pause_field} playFrame={play_frame}");
    let t_prefix_g = add_string_const(code, "T:");
    let anim_prefix_g = add_string_const(code, "ANIM:"); // per-frame state-change telemetry
    let frame_prefix_g = add_string_const(code, "FRAME:"); // per-frame telemetry (timing parity)
    let bar_g = add_string_const(code, "|");
    let t_nomatch_g = add_string_const(code, "T:NOMATCH\n");
    eprintln!("move/telemetry: Character t={char_entity_t} toState={to_state} getStateName={get_state_name} CState t={cstate_statics_t} g={cstate_global} JAB.field={jab_field} characters.field={characters_field}");
    // ---- 'l' command: synchronous custom-.fra load (headless, no worker thread) ----
    // The async UGC/worker path stalls headless; instead we build a PXF Resource and
    // call Resource.fetchThreaded directly on the main thread (it's synchronous inside:
    // File.getBytes -> PXFResource.createFromBytes -> set_DataAsPxf), then finishLoading
    // + addResource. getPXFResource then returns it with _data populated (spawnPlayer's
    // requirement). See docs/ENGINE_RE_MAP_v2.md.
    let resource_t = require_type(code, "pxf.io.Resource")?;
    // pxf.io.$Resource.__constructor__ (Resource, String id, String path, enc). The
    // findex drifts every recompile (the old pinned 17827 now points at
    // Resource::convertBytesToImageBitmapData → garbage-bytes SIGSEGV), so by name.
    let resource_ctor = require_fn(code, "__constructor__", Some("pxf.io.$Resource"))?;
    let fetch_threaded = require_fn(code, "fetchThreaded", Some("pxf.io.Resource"))?;
    let finish_loading = require_fn(code, "finishLoading", Some("pxf.io.AbstractResource"))?;
    let add_resource = require_fn(code, "addResource", Some("pxf.io.$ResourceManager"))?;
    let rt_statics_t = find_type(code, "pxf.io.$ResourceType")
        .ok_or_else(|| anyhow::anyhow!("pxf.io.$ResourceType type not found"))?;
    let rt_global = code.globals.iter().position(|g| g.0 == rt_statics_t)
        .ok_or_else(|| anyhow::anyhow!("$ResourceType statics global not found"))?;
    let pxf_field = find_field(code, rt_statics_t, "PXF")
        .ok_or_else(|| anyhow::anyhow!("ResourceType.PXF field not found"))?;
    let res_filepath_field = require_field(code, "pxf.io.AbstractResource", "_filePath")?;
    let res_type_field = require_field(code, "pxf.io.AbstractResource", "_type")?;
    let res_isabs_field = require_field(code, "pxf.io.AbstractResource", "_isAbsolute")?;
    let enc241_t = 241usize; // 4th ctor arg type (Ref<ResourceType>); passed null
    let l_idx = add_int(code, 'l' as i32);
    let sandbag_id_g = add_string_const(code, cname);
    let sandbag_path_g = add_string_const(code, &char_path);
    let l_prefix_g = add_string_const(code, "L:");
    let l_fail_g = add_string_const(code, "L:FAIL\n");
    let l_cmapnull_g = add_string_const(code, " CMAP:NULL\n");
    let sandbag_pkgid_g = add_string_const(code, &char_pkgid);
    let key_sb_g = add_string_const(code, &format!(" KEY={cname}\n"));
    let key_sbsb_g = add_string_const(code, &format!(" KEY={char_pkgid}\n"));
    let key_unknown_g = add_string_const(code, " KEY=?\n");
    let get_sprite_entity = require_fn(code, "getPXFSpriteEntity", Some("pxf.io.$ResourceManager"))?;
    let spr_ok_g = add_string_const(code, "SPR:1\n");
    let spr_null_g = add_string_const(code, "SPR:0\n");
    // NSPR probe: is the *namespaced* buried-VFX key (private::sandbag.sandbag) cached?
    // (SPR probes the bare "sandbag" key; the buried-VFX ctor reads the namespaced one.)
    let nspr_ok_g = add_string_const(code, "NSPR:1\n");
    let nspr_null_g = add_string_const(code, "NSPR:0\n");
    // sprite-entity fix (safe approach B): set RM.requiredMediaIds=["*"] before fetchThreaded so
    // the engine's OWN media-preload closure (run by finishLoading) preloads all entities into
    // _data.entityMap + the namespaced sprite cache; then re-cache the "sandbag" entity under the
    // BARE key (Character ctor looks up the bare spriteContent). No risky direct preload calls.
    let get_data_as_pxf = require_fn(code, "get_DataAsPxf", Some("pxf.io.AbstractResource"))?;
    // Deterministic sprite-entity builder: cacheSpriteEntityData(pxf, idx:Int) builds entities[idx]
    // into PXFResource.entityMap[entity.#2] (its id) — synchronous, no UnsafeCast, no flaky preload.
    let cache_sprite_entity_data = require_fn(code, "cacheSpriteEntityData", Some("pxf.structs.PXFResource"))?;
    let pxf_entities_field = find_field(code, pxfres_t, "entities")
        .ok_or_else(|| anyhow::anyhow!("PXFResource.entities field not found"))?;
    let cache_sprite_entity = require_fn(code, "cacheSpriteEntity", Some("pxf.io.$ResourceManager"))?;
    let sm_get = require_fn(code, "get", Some("haxe.ds.StringMap"))?;
    // pxfSpriteEntityCache field (g3508) — for the getPXFSpriteEntity self-heal patch.
    let spritecache_field = find_field(code, code.globals[rm_g].0, "pxfSpriteEntityCache")
        .ok_or_else(|| anyhow::anyhow!("pxfSpriteEntityCache field not found"))?;
    let pxf_entitymap_field = find_field(code, pxfres_t, "entityMap")
        .ok_or_else(|| anyhow::anyhow!("PXFResource.entityMap field not found"))?;
    let reqmedia_field = find_field(code, rm_statics_t, "requiredMediaIds")
        .ok_or_else(|| anyhow::anyhow!("requiredMediaIds field not found"))?;
    let sprite_entity_t = 746usize;      // pxf.structs.PXFSpriteEntity
    let cse_arg3_t = 108usize;           // cacheSpriteEntity 3rd arg type
    let star_g = add_string_const(code, "*");
    // The buried-character Vfx (Character.hx:762) reads spriteContent = statsProps.spriteContent,
    // whose runtime value is getResource().getContent("sandbag") = getFullyQualifiedResourceId(res)
    // + "." + "sandbag" = "private::sandbag.sandbag" (RE: getContent@2185 -> getFQContentId@1789).
    // We already re-cache under that key below — the bug was sourcing the entity from entityMap.get
    // (null), so the re-cache never ran. Fixed to source from getPXFSpriteEntity (SPR:1 non-null).
    let ns_sandbag_g = add_string_const(code, &char_ns_key);
    // resource-identifier (NOT content id) form, for the idempotence probe in the
    // self-bootstrapping 's' command: getPXFResource(this) non-null ⇒ already loaded.
    let _res_resid_g = add_string_const(code, &char_resid);
    // In-session generic char: the `s` handler builds the launch char's strings at RUNTIME
    // from the command's parts[1] (falling back to the baked default char), so successive
    // `s` commands can switch characters without re-injecting. These const pieces + mutable
    // globals support that. customdir is derived from the install dir (no hardcoded path).
    let customdir_g = add_string_const(code, &format!("{install_dir}/custom/"));
    let slash_g = add_string_const(code, "/");
    let frasuffix_g = add_string_const(code, ".fra");
    let g_name = code.globals.len();  code.globals.push(hlbc::types::RefType(13)); // String: char name
    let g_resid = code.globals.len(); code.globals.push(hlbc::types::RefType(13)); // String: private::<name>
    let g_pkgid = code.globals.len(); code.globals.push(hlbc::types::RefType(13)); // String: <name>.<name>
    let g_nskey = code.globals.len(); code.globals.push(hlbc::types::RefType(13)); // String: private::<name>.<name>
    let g_path = code.globals.len();  code.globals.push(hlbc::types::RefType(13)); // String: <install>/custom/<name>/<name>.fra
    // deferred multiplayer: extra players (parts[4..]) are spawnPlayer'd into the live match
    // one-per-frame from the per-frame update, because currentMatch is NULL synchronously after
    // startMatch. g_pending_parts = the parts ArrayObj (null when nothing pending); g_pending_idx
    // = next parts index (starts 4); g_pending_port = next player port (starts 1).
    let g_pending_parts = code.globals.len(); code.globals.push(hlbc::types::RefType(38)); // ArrayObj
    let g_pending_idx = code.globals.len();   code.globals.push(hlbc::types::RefType(3));  // Int
    let g_pending_port = code.globals.len();  code.globals.push(hlbc::types::RefType(3));  // Int
    // addCharacter (#3): the deferred loop NULLs g_pending_parts when drained, so we keep a copy of
    // the parts ArrayObj here (set alongside g_pending_parts at startMatch). The `n` (addCharacter)
    // command re-arms g_pending from this to spawn one more fighter into the LIVE match on demand.
    let g_saved_parts = code.globals.len(); code.globals.push(hlbc::types::RefType(38)); // ArrayObj
    // `--versus` force flag: set per-launch by the 'S' dispatch byte (true) vs 's' (false); the
    // mode-select reads it to force versus mode even for a 1-2 player roster.
    let g_force_versus = code.globals.len(); code.globals.push(hlbc::types::RefType(7)); // Bool
    // resolved assist ref (type 669) stashed from the `s` handler (where the resolve works) so
    // extra players can reuse it — re-resolving an assist mid-match (per-frame) SIGSEGVs.
    let g_pending_assist = code.globals.len(); code.globals.push(hlbc::types::RefType(669));
    // p1 stash: the live match's extra (2nd) player Character. Set when the deferred
    // spawnPlayer succeeds, reset to null on each start-match. The eval hook binds the `p1`
    // hscript var from this, instead of indexing the raw Match.characters array (which has no
    // hscript RTTI — its .length reads as garbage, so a bytecode length-guard crashes the frame).
    // Typed like r_ret (the spawnPlayer return register, type 0 = the Dynamic-compatible reg),
    // so the stash SetGlobal from r_ret type-matches and the eval-hook read flows back through
    // r_ret into setVar (which takes Dynamic) with no boxing.
    let g_live_p1 = code.globals.len(); code.globals.push(hlbc::types::RefType(0));
    eprintln!("sprite-fix: get_DataAsPxf={get_data_as_pxf} cacheEntity={cache_sprite_entity} smGet={sm_get} entityMap.f={pxf_entitymap_field} requiredMediaIds.f={reqmedia_field}");
    eprintln!("load-cmd: Resource t={resource_t} ctor={resource_ctor} fetchThreaded={fetch_threaded} finishLoading={finish_loading} addResource={add_resource} RT.g={rt_global} PXF.field={pxf_field} _filePath={res_filepath_field} _type={res_type_field} _isAbsolute={res_isabs_field}");
    // Reveal-the-match plumbing: the match renders in CoreEngine.gameContainer
    // (added by the always-subscribed gameStarted handler); menuContainer is a
    // sibling painted on top. Hiding menuContainer's h2d display object reveals
    // the match — non-destructively, so currentMatch / the live match stay intact.
    let core_statics_t = code.globals[core_g].0; // pxf.core.$CoreEngine statics
    let menuc_field = find_field(code, core_statics_t, "menuContainer")
        .ok_or_else(|| anyhow::anyhow!("menuContainer field not found"))?;
    let container_t = find_type(code, "pxf.display.Container")
        .ok_or_else(|| anyhow::anyhow!("pxf.display.Container type not found"))?;
    let dispobj_field = find_field(code, container_t, "displayObject")
        .ok_or_else(|| anyhow::anyhow!("displayObject field not found"))?;
    let h2dobj_t = find_type(code, "h2d.Object")
        .ok_or_else(|| anyhow::anyhow!("h2d.Object type not found"))?;
    let visible_field = find_field(code, h2dobj_t, "visible")
        .ok_or_else(|| anyhow::anyhow!("h2d.Object.visible field not found"))?;
    eprintln!("reveal: CoreEngine t={core_statics_t} menuContainer={menuc_field} Container t={container_t} displayObject={dispobj_field} h2d.Object t={h2dobj_t} visible={visible_field}");

    // Console (Tildebugger) passthrough: Tildebugger.console.runCommand(cmd).
    // ImprovedConsole extends h2d.Console, so the field works as the receiver.
    let run_command = require_fn(code, "runCommand", Some("h2d.Console"))?;
    let set_enabled = require_fn(code, "set_enabled", Some("pxf.core.ImprovedConsole"))?;
    let tilde_global = find_statics_global(code, "pxf.core.$Tildebugger")
        .ok_or_else(|| anyhow::anyhow!("$Tildebugger statics global not found"))?;
    let tilde_t = code.globals[tilde_global].0;
    let console_field = find_field(code, tilde_t, "console")
        .ok_or_else(|| anyhow::anyhow!("Tildebugger.console field not found"))?;
    let console_t = code.types[tilde_t]
        .get_type_obj()
        .and_then(|o| o.fields.get(console_field))
        .map(|f| f.t.0)
        .ok_or_else(|| anyhow::anyhow!("console field type missing"))?;
    eprintln!("runCommand={run_command} tilde_t={tilde_t} console_field={console_field} console_t={console_t}");
    // exact type of String.bytes (field 0) — for the GetI8 char-loop register
    let bytes_t = code.types[str_t]
        .get_type_obj()
        .and_then(|o| o.fields.first())
        .map(|fld| fld.t.0)
        .ok_or_else(|| anyhow::anyhow!("String.bytes field missing"))?;
    eprintln!("String.bytes type = {bytes_t}");
    // Read chars via the engine's own charCodeAt(String,i32)->null<i32> (a real
    // function call; raw GetI16/GetI8 opcodes crash when we emit them).
    let char_code_at = require_fn(code, "charCodeAt", Some("String"))?;
    let nulli32_t = {
        let ci = function_index_by_findex(code, char_code_at)
            .ok_or_else(|| anyhow::anyhow!("charCodeAt fn missing"))?;
        code.types[code.functions[ci].t.0]
            .get_type_fun()
            .map(|tf| tf.ret.0)
            .ok_or_else(|| anyhow::anyhow!("charCodeAt has no return type"))?
    };
    eprintln!("charCodeAt={char_code_at} ret(null<i32>)={nulli32_t}");

    // Persistent global for the socket (survives past the frame; the poll loop
    // reads it later) + a once-guard bool so the connect runs a single time.
    let g_sock = code.globals.len();
    code.globals.push(hlbc::types::RefType(sock_t));
    let g_done = code.globals.len();
    code.globals.push(hlbc::types::RefType(7)); // Bool
    // ready flag: set true by MainMenu.__constructor__ (content load complete).
    // Commands are not processed until this is set (they buffer in TCP).
    let g_ready = code.globals.len();
    code.globals.push(hlbc::types::RefType(7)); // Bool
    // one-shot guard: once a match goes live we tear down the leftover menu stack
    // (MainMenu/Local Play/etc.) so the match scene underneath becomes visible.
    let g_shown = code.globals.len();
    code.globals.push(hlbc::types::RefType(7)); // Bool
    // Loaded-character sprite-cache key (e.g. "private::<id>.<id>"), set by the load
    // for WHATEVER char `s` loads. The getPXFSpriteEntity self-heal falls back to this
    // on a cache miss — generic, not sandbag-specific. Null until a char is loaded.
    let g_loaded_spritekey = code.globals.len();
    code.globals.push(hlbc::types::RefType(13)); // String
    // line-command buffer: "startMatch" takes runtime args (char/stage/assist) sent
    // over the socket. We accumulate the arg bytes into a haxe.io.Bytes scratch
    // buffer, then getString() it into a real String to split + parse.
    let bytes_t2 = require_type(code, "haxe.io.Bytes")?;
    let g_buf = code.globals.len();
    code.globals.push(hlbc::types::RefType(bytes_t2)); // haxe.io.Bytes
    let g_blen = code.globals.len();
    code.globals.push(hlbc::types::RefType(3)); // Int
    // Legacy one-shot guard global (now unused — see MULTI-LAUNCH note at the `s`
    // handler). Retained so harness global indices stay stable across the rest of
    // connect_edit; the `s` command may be sent repeatedly to start successive matches.
    let g_launched = code.globals.len();
    code.globals.push(hlbc::types::RefType(7)); // Bool
    // Persistent top-scope hscript interpreter: created ONCE (with applyInterpreterGlobals
    // -> the engine's global API), reused for every `e`. Null until first eval. This is the
    // single engine-linked interp into which all commands eventually move as one hscript file.
    let g_interp = code.globals.len();
    code.globals.push(hlbc::types::RefType(506)); // hscript.Interp

    // Injected HELD-control bitmask for the input-override handler ('i'). The
    // epilogue appended to Character.updateGameInput ORs this into
    // m_heldControls.buttons every frame and pulses its rising edge into
    // m_pressedControls, so the engine's own InputResolver maps it to actions
    // (NOT a synthetic keypress). 0 = no injection; an Int global defaults to 0.
    let g_inject_held = code.globals.len();
    code.globals.push(hlbc::types::RefType(3)); // Int
    // 'i' (input) command byte + the ASCII/scale constants its decimal-mask parser needs.
    let i_idx = add_int(code, 'i' as i32);
    let i_zero_char = add_int(code, '0' as i32);
    let i_nine_char = add_int(code, '9' as i32);
    let i_ten = add_int(code, 10);
    let i_ok_g = add_string_const(code, "I:OK\n");

    // Inject into the per-frame update (post-boot), NOT onLoaded: networking
    // string/host calls SIGSEGV during early config-load, but are fine once the
    // engine is fully up. The connect runs once (guard); the recv runs every frame.
    let update_fx = require_fn(code, "update", Some("fraymakers.Main"))?;
    let fidx = function_index_by_findex(code, update_fx)
        .ok_or_else(|| anyhow::anyhow!("update@{update_fx} not found"))?;
    let f = &mut code.functions[fidx];

    // regs 0-13: done,true,sock,host,port,out,ret,ip,byte,blockf,sock2,handle,c,zero
    // 14 str, 15 enc(unused), 16 len, 17 bytes(hl.Bytes=14), 18 sidx, 19 one, 20 off, 21 ch
    // ...22 nulli32_t 23 tilde 24 console | launch regs:
    // 25 ref0(669) 26 ref1(669) 27 pdyn(4366) 28 dynv(9) 29 pvirt(1957) 30 player(2536)
    // 31 typev(8) 32 natarr(11) 33 arr(38) 34 cfgdyn(4366) 35 cfgvirt(675) 36 config(668)
    // 37 settings(6738) 38 nullstr(13) 39 int(3)
    // ...40 matchRules value (dmr_field_t) 41 $MatchSettings statics (ms_statics_t) 42 assist ref(669)
    // 43 $MatchController statics (mc_statics_t) 44 currentMatch (match_t)
    // 45 CoreEngine statics (core_statics_t) 46 menuContainer (container_t) 47 displayObject (h2dobj_t)
    // 48 mode (mode_t=3522) 49 modeConfig virtual (1194) 50 startMatch config virtual (4482)
    // 51 Bytes scratch (bytes_t2) 52 ArrayObj split parts (38) 53 String scratch (delim/arg) 54 dyn element
    // 55 name 56 pkgid 57 candidate 58 resId (all String) 59 null<i32> 60 PXFResource (existence check)
    // 61 keys-iterator(unused) 62 key(String) 63 contentMap(StringMap) 64 bool 65 RM statics
    // 66 pool (ArrayObj=38); 67 pool.array (NativeArray=11); 68 pool elem (absres_t=394)
    // 69 Character (char_entity_t); 70 $CState statics (cstate_statics_t)
    // 71 Resource (resource_t); 72 $ResourceType statics (rt_statics_t); 73 enc Ref<ResourceType> (t241)
    let base = add_regs(f, &[7, 7, sock_t, host_t, 3, out_t, 0, 3, 3, 7, sock_t, handle_t, 3, 3, str_t, enc_t, 3, bytes_t, 3, 3, 3, 3, nulli32_t, tilde_t, console_t, 669, 669, 4366, 9, 1957, 2536, 8, 11, 38, 4366, 675, 668, 6738, 13, 3, dmr_field_t, ms_statics_t, 669, mc_statics_t, match_t, core_statics_t, container_t, h2dobj_t, mode_t, 1194, 4482, bytes_t2, 38, str_t, 0, str_t, str_t, str_t, str_t, nulli32_t2, pxfres_t, keysiter_t, str_t, stringmap_t, 7, rm_statics_t, 38, 11, absres_t, char_entity_t, cstate_statics_t, resource_t, rt_statics_t, enc241_t, sprite_entity_t, cse_arg3_t]);
    let rr = |i: u32| Reg(base + i);
    // eval regs: hscript Parser, Interp, Expr (parseString result), Dynamic (execute result).
    // Appended after the main block so every existing rr(i) index is unchanged.
    let eval_regs_base = add_regs(f, &[hs_parser_t as usize, hs_interp_t as usize, hs_expr_t as usize, 9]);
    let (e_parser, e_interp, e_expr, e_result) =
        (Reg(eval_regs_base), Reg(eval_regs_base + 1), Reg(eval_regs_base + 2), Reg(eval_regs_base + 3));
    // Scratch for the `stage` binding (a typed Field read needs a register of the field's type).
    let stage_reg = Reg(add_regs(f, &[stage_entity_t]));
    // PlayerConfig scratch for post-start spawnPlayer of extra players.
    let cfg_pc_reg = Reg(add_regs(f, &[player_config_t]));
    // Scratch for the headless pre-match loading-screen factory set: the MatchController
    // statics object + the factory closure (typed as loadingScreenFactory's function type).
    let ls_regs_base = add_regs(f, &[mc_statics_t, lsf_field_t]);
    let (ls_mc_reg, ls_fac_reg) = (Reg(ls_regs_base), Reg(ls_regs_base + 1));
    let (r_done, r_true, r_sock, r_host, r_port, r_out, r_ret, r_ip, r_byte, r_blockf, r_sock2, r_handle, r_c, r_zero) =
        (rr(0), rr(1), rr(2), rr(3), rr(4), rr(5), rr(6), rr(7), rr(8), rr(9), rr(10), rr(11), rr(12), rr(13));

    use hlbc::types::{RefFun, RefField, RefInt, RefGlobal, ValBool};
    // Emit a content-id resolver into `ops`: reads the String in reg `name`, writes
    // a content-ref (669) into reg `out`. If the name already has "::" it's parsed
    // as a full id; otherwise it's expanded to package.id (bare "x" -> "x.x") and
    // each namespace prefix is tried, picking the first whose resource exists.
    // Scratch: rr16,39(int) rr53,56,57,58(String) rr59(null<i32>) rr60(PXFResource); rr38 must be null.
    let nsp = ns_prefixes.clone();
    // cmap_field selects which per-type StringMap to search for a bare id
    // (characterPxfContentMap / assistPxfContentMap / stagePxfContentMap).
    let emit_resolve = |ops: &mut Vec<Opcode>, name: u32, out: u32, cmap_field: usize| {
        let r = |i: u32| Reg(base + i);
        // "::" present? -> full id
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(colon2_g) });
        ops.push(Opcode::Null { dst: r(59) });
        ops.push(Opcode::Call3 { dst: r(16), fun: RefFun(indexof_fn), arg0: r(name), arg1: r(53), arg2: r(59) });
        ops.push(Opcode::Int { dst: r(39), ptr: RefInt(zero_idx) });
        let j_full = ops.len();
        ops.push(Opcode::JSGte { a: r(16), b: r(39), offset: 0 });
        // "." present? -> use name as pkgid (prefix path); else registry-search by bare id
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(dot_g) });
        ops.push(Opcode::Null { dst: r(59) });
        ops.push(Opcode::Call3 { dst: r(16), fun: RefFun(indexof_fn), arg0: r(name), arg1: r(53), arg2: r(59) });
        ops.push(Opcode::Int { dst: r(39), ptr: RefInt(zero_idx) });
        let j_pkgname = ops.len();
        ops.push(Opcode::JSGte { a: r(16), b: r(39), offset: 0 });
        // BARE NAME (no "."): prefix expansion + cmap check.
        // For each namespace prefix (custom::, public::, global::), build the full id,
        // call getPXFResource to get a properly-typed PXFResource (r(60)), read
        // cmap_field from it (safe because getPXFResource returns pxfres_t). Accept
        // only if both resource AND cmap are non-null — this rejects stubs that loaded
        // under a different namespace without a populated content map. Last prefix
        // (global::) is always accepted as-is (fallback).
        // Jump wiring: each null check for prefix k jumps to the START of prefix k+1.
        // Collected in jump_to_next_prefix and patched below after all starts are known.
        // RS_NOTFOUND: pkgid = name + "." + name
        let _l_notfound = ops.len();
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(dot_g) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(name), arg1: r(53) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(56), arg1: r(name) });
        let j_to_prefix = ops.len();
        ops.push(Opcode::JAlways { offset: 0 });                        // -> L_PREFIX
        // L_PKGNAME: pkgid = name (caller already gave package.id)
        let l_pkgname = ops.len();
        ops.push(Opcode::Mov { dst: r(56), src: r(name) });
        // L_PREFIX: try each namespace prefix with pkgid in r56.
        // getPXFResource returns pxfres_t → Field on cmap_field is safe (no cast needed).
        let l_prefix = ops.len();
        let mut found_pref = vec![];
        let mut prefix_starts = vec![];
        // (j_op, target_prefix_index): j_op is a JNull that should jump to prefix[target_prefix_index]
        let mut jump_to_next_prefix: Vec<(usize, usize)> = vec![];
        let n = nsp.len();
        for (k, &pref) in nsp.iter().enumerate() {
            let prefix_start = ops.len();
            prefix_starts.push(prefix_start);
            ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(pref) });
            ops.push(Opcode::Call2 { dst: r(57), fun: RefFun(str_add), arg0: r(53), arg1: r(56) });
            ops.push(Opcode::Call2 { dst: r(out), fun: RefFun(parseresid_fn), arg0: r(57), arg1: r(38) });
            if k + 1 < n {
                ops.push(Opcode::Call1 { dst: r(58), fun: RefFun(getresid_fn), arg0: r(out) });
                ops.push(Opcode::Call1 { dst: r(60), fun: RefFun(getpxf_fn), arg0: r(58) });
                // LOAD-ON-DEMAND: getpxf null means the resource is in the pool (constructed
                // by importManifest) but its media was skipped by the boot required-load
                // filter (all public::). Load it synchronously — getResourceByID(resid,null)
                // -> cast to Resource -> fetchThreaded (sync read+decode) -> finishLoading —
                // then re-getpxf. r58 is the canonical pool key, so this matches whatever the
                // resolver would look up. Already-loaded resources (e.g. the self-bootstrapped
                // char) skip via the JNotNull guard; ids not in the pool skip via JNull.
                let j_already = ops.len();
                ops.push(Opcode::JNotNull { reg: r(60), offset: 0 });   // already loaded -> skip load
                ops.push(Opcode::Call2 { dst: r(68), fun: RefFun(getresbyid_fn), arg0: r(58), arg1: r(38) }); // getResourceByID
                let j_notpool = ops.len();
                ops.push(Opcode::JNull { reg: r(68), offset: 0 });      // not in pool -> skip load
                ops.push(Opcode::UnsafeCast { dst: r(71), src: r(68) }); // AbstractResource -> Resource
                ops.push(Opcode::Call1 { dst: r(6), fun: RefFun(fetch_threaded), arg0: r(71) });
                ops.push(Opcode::Call1 { dst: r(6), fun: RefFun(finish_loading), arg0: r(71) });
                ops.push(Opcode::Call1 { dst: r(60), fun: RefFun(getpxf_fn), arg0: r(58) }); // re-fetch
                let l_loaded = ops.len();
                if let Opcode::JNotNull { offset, .. } = &mut ops[j_already] { *offset = l_loaded as i32 - j_already as i32 - 1; }
                if let Opcode::JNull    { offset, .. } = &mut ops[j_notpool] { *offset = l_loaded as i32 - j_notpool as i32 - 1; }
                let j_res_null = ops.len();
                ops.push(Opcode::JNull { reg: r(60), offset: 0 });      // null resource -> next prefix
                jump_to_next_prefix.push((j_res_null, k + 1));
                // r(60) is pxfres_t (returned by getPXFResource) — safe to read cmap_field.
                ops.push(Opcode::Field { dst: r(63), obj: r(60), field: RefField(cmap_field) });
                let j_cmap_null = ops.len();
                ops.push(Opcode::JNull { reg: r(63), offset: 0 });      // null cmap -> next prefix
                jump_to_next_prefix.push((j_cmap_null, k + 1));
                let jf = ops.len();
                ops.push(Opcode::JAlways { offset: 0 });                 // cmap non-null -> DONE
                found_pref.push(jf);
            }
        }
        let j_nsdone = ops.len();
        ops.push(Opcode::JAlways { offset: 0 });
        // L_FULL
        let l_full = ops.len();
        ops.push(Opcode::Call2 { dst: r(out), fun: RefFun(parseresid_fn), arg0: r(name), arg1: r(38) });
        let l_done = ops.len();
        let set = |ops: &mut Vec<Opcode>, at: usize, tgt: usize| {
            let off = tgt as i32 - at as i32 - 1;
            match &mut ops[at] {
                Opcode::JSGte { offset, .. } | Opcode::JFalse { offset, .. }
                | Opcode::JTrue { offset, .. } | Opcode::JNotNull { offset, .. }
                | Opcode::JNull { offset, .. }
                | Opcode::JAlways { offset, .. } => *offset = off,
                _ => unreachable!(),
            }
        };
        set(ops, j_full, l_full);
        set(ops, j_pkgname, l_pkgname);
        set(ops, j_to_prefix, l_prefix);
        set(ops, j_nsdone, l_done);
        // Patch prefix null-jumps to point to the start of the next prefix's code.
        // prefix_starts[k] is the op-index of the first op in prefix k's block.
        for (j_op, target_k) in jump_to_next_prefix {
            if let Some(&tgt) = prefix_starts.get(target_k) {
                set(ops, j_op, tgt);
            }
        }
        for jf in found_pref { set(ops, jf, l_done); }
    };
    // Factored self-bootstrap: load the custom char named in register `name` from
    // <install>/custom/<name>/<name>.fra (sets g_name/g_resid/g_pkgid/g_nskey/g_path, idempotent
    // — skips if already loaded, else builds a PXF Resource + fetchThreaded + finishLoading +
    // caches its sprite entities). Same logic as player 1's inline self-bootstrap below, so extra
    // players (parts[4..]) get their char loaded FRESH in this `s` dispatch — the per-frame spawn
    // then finds it cached (a char loaded in a prior, since-cleaned-up match has a dead sprite
    // cache and crashes when re-spawned).
    #[allow(unused)]
    let emit_load_char = |ops: &mut Vec<Opcode>, name: u32| {
        let r = |i: u32| Reg(base + i);
        ops.push(Opcode::SetGlobal { global: RefGlobal(g_name), src: r(name) });
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(ns_prefixes[0]) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(53), arg1: r(name) });
        ops.push(Opcode::SetGlobal { global: RefGlobal(g_resid), src: r(56) });
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(dot_g) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(name), arg1: r(53) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(56), arg1: r(name) });
        ops.push(Opcode::SetGlobal { global: RefGlobal(g_pkgid), src: r(56) });
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(ns_prefixes[0]) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(53), arg1: r(56) });
        ops.push(Opcode::SetGlobal { global: RefGlobal(g_nskey), src: r(56) });
        ops.push(Opcode::GetGlobal { dst: r(56), global: RefGlobal(customdir_g) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(56), arg1: r(name) });
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(slash_g) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(56), arg1: r(53) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(56), arg1: r(name) });
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(frasuffix_g) });
        ops.push(Opcode::Call2 { dst: r(56), fun: RefFun(str_add), arg0: r(56), arg1: r(53) });
        ops.push(Opcode::SetGlobal { global: RefGlobal(g_path), src: r(56) });
        ops.push(Opcode::GetGlobal { dst: r(58), global: RefGlobal(g_resid) });
        ops.push(Opcode::Call1 { dst: r(60), fun: RefFun(getpxf_fn), arg0: r(58) });
        let j_skip = ops.len();
        ops.push(Opcode::JNotNull { reg: r(60), offset: 0 });
        ops.push(Opcode::New { dst: r(71) });
        ops.push(Opcode::GetGlobal { dst: r(55), global: RefGlobal(g_name) });
        ops.push(Opcode::GetGlobal { dst: r(56), global: RefGlobal(g_path) });
        ops.push(Opcode::Null { dst: r(73) });
        ops.push(Opcode::Call4 { dst: r_ret, fun: RefFun(resource_ctor), arg0: r(71), arg1: r(55), arg2: r(56), arg3: r(73) });
        ops.push(Opcode::Bool { dst: r(64), value: ValBool(true) });
        ops.push(Opcode::SetField { obj: r(71), field: RefField(res_isabs_field), src: r(64) });
        ops.push(Opcode::GetGlobal { dst: r(72), global: RefGlobal(rt_global) });
        ops.push(Opcode::Field { dst: r(16), obj: r(72), field: RefField(pxf_field) });
        ops.push(Opcode::SetField { obj: r(71), field: RefField(res_type_field), src: r(16) });
        ops.push(Opcode::Type { dst: r(31), ty: RT(13) });
        ops.push(Opcode::Int { dst: r(39), ptr: RefInt(one_idx) });
        ops.push(Opcode::Call2 { dst: r(32), fun: RefFun(256), arg0: r(31), arg1: r(39) });
        ops.push(Opcode::Int { dst: r(39), ptr: RefInt(zero_idx) });
        ops.push(Opcode::GetGlobal { dst: r(53), global: RefGlobal(star_g) });
        ops.push(Opcode::SetArray { array: r(32), index: r(39), src: r(53) });
        ops.push(Opcode::Call1 { dst: r(33), fun: RefFun(257), arg0: r(32) });
        ops.push(Opcode::GetGlobal { dst: r(65), global: RefGlobal(rm_g) });
        ops.push(Opcode::SetField { obj: r(65), field: RefField(reqmedia_field), src: r(33) });
        ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(fetch_threaded), arg0: r(71) });
        ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(finish_loading), arg0: r(71) });
        ops.push(Opcode::Call1 { dst: r(60), fun: RefFun(get_data_as_pxf), arg0: r(71) });
        ops.push(Opcode::Field { dst: r(66), obj: r(60), field: RefField(pxf_entities_field) });
        ops.push(Opcode::Field { dst: r(39), obj: r(66), field: RefField(0) });
        ops.push(Opcode::Int { dst: r(16), ptr: RefInt(zero_idx) });
        let loop_ = ops.len();
        let j_ge = ops.len();
        ops.push(Opcode::JSGte { a: r(16), b: r(39), offset: 0 });
        ops.push(Opcode::Call2 { dst: r_ret, fun: RefFun(cache_sprite_entity_data), arg0: r(60), arg1: r(16) });
        ops.push(Opcode::Incr { dst: r(16) });
        let j_back = ops.len();
        ops.push(Opcode::JAlways { offset: 0 });
        let done = ops.len();
        ops.push(Opcode::Field { dst: r(63), obj: r(60), field: RefField(pxf_entitymap_field) });
        let j_emap = ops.len();
        ops.push(Opcode::JNull { reg: r(63), offset: 0 });
        ops.push(Opcode::GetGlobal { dst: r(55), global: RefGlobal(g_name) });
        ops.push(Opcode::Call2 { dst: r(28), fun: RefFun(sm_get), arg0: r(63), arg1: r(55) });
        let j_ent = ops.len();
        ops.push(Opcode::JNull { reg: r(28), offset: 0 });
        ops.push(Opcode::UnsafeCast { dst: r(74), src: r(28) });
        ops.push(Opcode::Null { dst: r(75) });
        ops.push(Opcode::GetGlobal { dst: r(55), global: RefGlobal(g_name) });
        ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: r(55), arg1: r(74), arg2: r(75) });
        ops.push(Opcode::GetGlobal { dst: r(55), global: RefGlobal(g_pkgid) });
        ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: r(55), arg1: r(74), arg2: r(75) });
        ops.push(Opcode::GetGlobal { dst: r(55), global: RefGlobal(g_nskey) });
        ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: r(55), arg1: r(74), arg2: r(75) });
        let addres = ops.len();
        ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(add_resource), arg0: r(71) });
        ops.push(Opcode::GetGlobal { dst: r(55), global: RefGlobal(g_nskey) });
        ops.push(Opcode::SetGlobal { global: RefGlobal(g_loaded_spritekey), src: r(55) });
        let skip = ops.len();
        if let Opcode::JNotNull { offset, .. } = &mut ops[j_skip] { *offset = skip as i32 - j_skip as i32 - 1; }
        if let Opcode::JSGte  { offset, .. } = &mut ops[j_ge]   { *offset = done as i32 - j_ge as i32 - 1; }
        if let Opcode::JAlways{ offset, .. } = &mut ops[j_back] { *offset = loop_ as i32 - j_back as i32 - 1; }
        if let Opcode::JNull  { offset, .. } = &mut ops[j_emap] { *offset = addres as i32 - j_emap as i32 - 1; }
        if let Opcode::JNull  { offset, .. } = &mut ops[j_ent]  { *offset = addres as i32 - j_ent as i32 - 1; }
    };
    // ---- once-guard + connect + auth handshake (runs a single time) ----
    let mut ops = vec![
        Opcode::GetGlobal { dst: r_done, global: RefGlobal(g_done) },   // 0
        Opcode::JTrue { cond: r_done, offset: 0 },                      // 1 -> L_RECV (patched)
        Opcode::Bool { dst: r_true, value: ValBool(true) },             // 2
        Opcode::SetGlobal { global: RefGlobal(g_done), src: r_true },   // 3
        Opcode::Call0 { dst: r_ret, fun: RefFun(socket_init) },
        Opcode::New { dst: r_sock },
        Opcode::Call1 { dst: r_ret, fun: RefFun(sock_init_method), arg0: r_sock },
        Opcode::New { dst: r_host },
        Opcode::Int { dst: r_ip, ptr: RefInt(ip_const) },
        Opcode::SetField { obj: r_host, field: RefField(ip_field), src: r_ip },
        Opcode::Int { dst: r_port, ptr: RefInt(port_idx) },
        Opcode::Call3 { dst: r_ret, fun: RefFun(connect), arg0: r_sock, arg1: r_host, arg2: r_port },
        Opcode::SetGlobal { global: RefGlobal(g_sock), src: r_sock },
        // setBlocking(false) so the per-frame recv never blocks the render loop
        Opcode::Bool { dst: r_blockf, value: ValBool(false) },
        Opcode::Call2 { dst: r_ret, fun: RefFun(set_blocking), arg0: r_sock, arg1: r_blockf },
        Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) },
    ];
    // Send AUTH/HELLO from a real String via a char-loop (writeByte of s.bytes[i*2])
    // — avoids the crashing Bytes.ofString/utf16_to_utf8 native.
    let _ = (r_byte, write_str); // (kept for reference; unused now)
    // Send AUTH/HELLO via writeString of a REAL String object (GetGlobal of a
    // string constant). Now that the String is valid, ofString should work.
    let _ = (one_idx, bytes_t, char_code_at, nulli32_t);
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(hello_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // (READY + g_ready are NOT sent here — too early: the TrainingMode mode resource
    // and other required match content aren't loaded until the second boot-load pass,
    // whose onComplete is Main.onLoaded. We signal READY from onLoaded instead, after
    // required content is loaded but before launchScreen builds the Title.)
    let _ = ready_g;
    // allocate the line-command buffer once (haxe.io.Bytes.alloc(512))
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(buf_cap_idx) });
    ops.push(Opcode::Call1 { dst: rr(51), fun: RefFun(bytes_alloc), arg0: rr(16) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_buf), src: rr(51) });

    // ---- per-frame receive (runs every frame, but only once load-complete) ----
    let idx_recv = ops.len();
    // gate: if !g_ready (MainMenu not constructed yet) -> skip (cmds buffer in TCP)
    ops.push(Opcode::GetGlobal { dst: rr(9), global: RefGlobal(g_ready) });
    let idx_jready = ops.len();
    ops.push(Opcode::JFalse { cond: rr(9), offset: 0 });               // not ready -> L_ORIG
    // reveal-once: the mode's MATCH_STARTED flow removes the menu *background*, but
    // the menu *screens* (MainMenu) remain because we launched the mode without the
    // menu navigation that normally pops them. Once a match is live, destroy the
    // leftover active menu screens. (The match is mode-owned now, so this no longer
    // orphans it the way it did with the bare MatchController path.)
    ops.push(Opcode::GetGlobal { dst: rr(9), global: RefGlobal(g_shown) });
    let idx_jshown = ops.len();
    ops.push(Opcode::JTrue { cond: rr(9), offset: 0 });                // already done -> recv
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) }); // currentMatch
    let idx_jnomatch = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                // no match yet -> recv
    ops.push(Opcode::Call0 { dst: r_ret, fun: RefFun(destroymenus_fn) });        // destroyAllActiveMenus
    ops.push(Opcode::Bool { dst: rr(1), value: ValBool(true) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_shown), src: rr(1) });
    let idx_after_reveal = ops.len();
    if let Opcode::JTrue { offset, .. } = &mut ops[idx_jshown] { *offset = idx_after_reveal as i32 - idx_jshown as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_jnomatch] { *offset = idx_after_reveal as i32 - idx_jnomatch as i32 - 1; }
    if FM_MULTIPLAYER {
    // ---- deferred extra-player spawn: spawnPlayer ONE pending char per frame, once the match
    // is live (currentMatch is null synchronously after startMatch). Serialized one-per-frame so
    // each char's media loads alone (no threaded-load race). g_pending_parts = parts ArrayObj
    // (null = idle); g_pending_idx walks parts[4..]; g_pending_port = next port.
    ops.push(Opcode::GetGlobal { dst: rr(33), global: RefGlobal(g_pending_parts) });
    let idx_pend_jidle = ops.len();
    ops.push(Opcode::JNull { reg: rr(33), offset: 0 });                          // nothing pending -> end
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) }); // currentMatch
    let idx_pend_jnomatch = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                          // match not live yet -> end
    ops.push(Opcode::GetGlobal { dst: rr(16), global: RefGlobal(g_pending_idx) });
    ops.push(Opcode::Field { dst: rr(39), obj: rr(33), field: RefField(0) });    // parts.length
    let idx_pend_jclear = ops.len();
    ops.push(Opcode::JSGte { a: rr(16), b: rr(39), offset: 0 });                 // idx >= length -> clear
    ops.push(Opcode::Field { dst: rr(67), obj: rr(33), field: RefField(1) });    // parts.array (native)
    ops.push(Opcode::GetArray { dst: rr(55), array: rr(67), index: rr(16) });    // name = parts[idx]
    // RM.requiredMediaIds = ["*"] so emit_resolve's load-on-demand pulls the char's FULL media
    // (sprite entities). A partial load crashes cacheSpriteEntityData/processSpriteSheets when
    // the char is then realized. Same thing the self-bootstrap does for player 1.
    ops.push(Opcode::Type { dst: rr(31), ty: RT(13) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(one_idx) });
    ops.push(Opcode::Call2 { dst: rr(32), fun: RefFun(256), arg0: rr(31), arg1: rr(39) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(star_g) });
    ops.push(Opcode::SetArray { array: rr(32), index: rr(39), src: rr(53) });
    ops.push(Opcode::Call1 { dst: rr(33), fun: RefFun(257), arg0: rr(32) });
    ops.push(Opcode::GetGlobal { dst: rr(65), global: RefGlobal(rm_g) });
    ops.push(Opcode::SetField { obj: rr(65), field: RefField(reqmedia_field), src: rr(33) });
    emit_resolve(&mut ops, 55, 26, char_cmap_field);                            // char ref (load-on-demand)
    // ONE-SHOT MR: past emit_resolve
    ops.push(Opcode::GetGlobal { dst: r_sock2, global: RefGlobal(g_sock) });
    let idx_mr_js = ops.len();
    ops.push(Opcode::JNull { reg: r_sock2, offset: 0 });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(mr_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_mr_d = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_mr_js] { *offset = idx_mr_d as i32 - idx_mr_js as i32 - 1; }
    // assist ref = the one the `s` handler already resolved for player 1 (stashed in
    // g_pending_assist) — the HUD DamageCounter.generateAssistSprite reads it, so a null assist
    // null-derefs `.namespace`; re-resolving it here (per-frame) SIGSEGVs, so reuse the stash.
    ops.push(Opcode::GetGlobal { dst: rr(42), global: RefGlobal(g_pending_assist) });
    // NULL-ASSIST GUARD: a null assist would crash Fraymakers' HUD (DamageCounter) in spawnPlayer.
    // Instead, emit the enhanced Peptide RESDIAG message and skip this player (idx still advances
    // at `idx_pend_advance` below, so we don't retry it forever).
    let idx_na_jok = ops.len();
    ops.push(Opcode::JNotNull { reg: rr(42), offset: 0 });               // assist present -> ok
    ops.push(Opcode::GetGlobal { dst: r_sock2, global: RefGlobal(g_sock) });
    let idx_na_jsock = ops.len();
    ops.push(Opcode::JNull { reg: r_sock2, offset: 0 });                 // no socket -> skip diag write
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(null_assist_diag_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_na_jsock_d = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_na_jsock] { *offset = idx_na_jsock_d as i32 - idx_na_jsock as i32 - 1; }
    let idx_na_jadv = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                             // null assist -> skip spawn, advance
    let idx_na_ok = ops.len();
    if let Opcode::JNotNull { offset, .. } = &mut ops[idx_na_jok] { *offset = idx_na_ok as i32 - idx_na_jok as i32 - 1; }
    // build the player virtual { character: rr26, assist: rr42, port: g_pending_port } (type 1957)
    // and hand it to the engine factory -> FraymakersPlayerConfig -> spawnPlayer.
    ops.push(Opcode::New { dst: rr(27) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(character_str), src: rr(26) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(assist_str), src: rr(42) });
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_pending_port) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(port_str), src: rr(28) });
    // extra player -> team = its port (>=1), distinct from player 1's team 0, so the match
    // treats it as an opponent and inter-player hits register.
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_pending_port) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(team_str), src: rr(28) });
    ops.push(Opcode::ToVirtual { dst: rr(29), src: rr(27) });                    // -> player virtual@1957
    ops.push(Opcode::Call1 { dst: rr(30), fun: RefFun(create_player_config), arg0: rr(29) }); // FraymakersPlayerConfig@2536
    // ONE-SHOT MI: past PlayerConfig factory
    ops.push(Opcode::GetGlobal { dst: r_sock2, global: RefGlobal(g_sock) });
    let idx_mi_js = ops.len();
    ops.push(Opcode::JNull { reg: r_sock2, offset: 0 });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(mi_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_mi_d = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_mi_js] { *offset = idx_mi_d as i32 - idx_mi_js as i32 - 1; }
    ops.push(Opcode::Call2 { dst: r_ret, fun: RefFun(spawn_player_fn), arg0: rr(44), arg1: rr(30) });
    // stash the spawned extra player (r_ret) so the eval hook can bind `p1` to it. Read here,
    // before the SP diag reuses r_ret. null (SP:0) is fine — p1 just stays null.
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_live_p1), src: r_ret });
    // ONE-SHOT diag (fires once per extra player, NOT every frame — idx advances right below
    // so it never repeats): SP:1 = spawnPlayer returned a Character, SP:0 = null.
    ops.push(Opcode::GetGlobal { dst: r_sock2, global: RefGlobal(g_sock) });
    let idx_sp1_jsock = ops.len();
    ops.push(Opcode::JNull { reg: r_sock2, offset: 0 });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    let idx_sp1_jnull = ops.len();
    ops.push(Opcode::JNull { reg: r_ret, offset: 0 });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(sp_ok_g) });
    let idx_sp1_jd = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_sp1_n = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(sp_null_g) });
    let idx_sp1_w = ops.len();
    if let Opcode::JNull   { offset, .. } = &mut ops[idx_sp1_jnull] { *offset = idx_sp1_n as i32 - idx_sp1_jnull as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_sp1_jd]    { *offset = idx_sp1_w as i32 - idx_sp1_jd as i32 - 1; }
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_sp1_done = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_sp1_jsock] { *offset = idx_sp1_done as i32 - idx_sp1_jsock as i32 - 1; }
    // advance idx + port (read fresh from globals — emit_resolve clobbered rr16/rr39)
    let idx_pend_advance = ops.len();
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_na_jadv] { *offset = idx_pend_advance as i32 - idx_na_jadv as i32 - 1; }
    ops.push(Opcode::GetGlobal { dst: rr(16), global: RefGlobal(g_pending_idx) });
    ops.push(Opcode::Incr { dst: rr(16) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_idx), src: rr(16) });
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_pending_port) });
    ops.push(Opcode::Incr { dst: rr(39) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_port), src: rr(39) });
    let idx_pend_joneframe = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                                     // one spawn per frame -> end
    let idx_pend_clear = ops.len();
    ops.push(Opcode::Null { dst: rr(33) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_parts), src: rr(33) }); // done -> idle
    let idx_pend_end = ops.len();
    if let Opcode::JNull   { offset, .. } = &mut ops[idx_pend_jidle]    { *offset = idx_pend_end as i32 - idx_pend_jidle as i32 - 1; }
    if let Opcode::JNull   { offset, .. } = &mut ops[idx_pend_jnomatch] { *offset = idx_pend_end as i32 - idx_pend_jnomatch as i32 - 1; }
    if let Opcode::JSGte   { offset, .. } = &mut ops[idx_pend_jclear]   { *offset = idx_pend_clear as i32 - idx_pend_jclear as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_pend_joneframe]{ *offset = idx_pend_end as i32 - idx_pend_joneframe as i32 - 1; }
    }
    let _ = (menuc_field, dispobj_field, visible_field);
    ops.push(Opcode::GetGlobal { dst: r_sock2, global: RefGlobal(g_sock) });
    let idx_jnull = ops.len();
    ops.push(Opcode::JNull { reg: r_sock2, offset: 0 });                // not connected -> L_ORIG
    ops.push(Opcode::Field { dst: r_handle, obj: r_sock2, field: RefField(s_field) });
    ops.push(Opcode::Call1 { dst: r_c, fun: RefFun(recv_char), arg0: r_handle });
    ops.push(Opcode::Int { dst: r_zero, ptr: RefInt(zero_idx) });
    let idx_jslt = ops.len();
    ops.push(Opcode::JSLt { a: r_c, b: r_zero, offset: 0 });            // no data (c<0) -> L_ORIG
    let _ = write_byte;
    // dispatch: 'x' -> clean engine exit (no kill -9 wedge); 'p' -> PONG ; 'c' -> ...
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(x_idx) });
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 1 });          // not 'x' -> skip exit (fall to 'p')
    ops.push(Opcode::Call0 { dst: r_ret, fun: RefFun(hxd_exit) });      // hxd.System.exit() — terminates
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(p_idx) });
    let idx_jne_p = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });          // not 'p' -> 'c' check
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(pong_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // 'c' check (c=='p' falls through here and skips, since c!='c')
    let idx_c_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(c_idx) });
    let idx_jne_c = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });          // not 'c' -> L_ORIG
    ops.push(Opcode::GetGlobal { dst: rr(23), global: RefGlobal(tilde_global) });
    ops.push(Opcode::Field { dst: rr(24), obj: rr(23), field: RefField(console_field) });
    // enable the console first so its log display exists (handleCommand echoes to it)
    ops.push(Opcode::Bool { dst: rr(9), value: ValBool(true) });
    ops.push(Opcode::Call2 { dst: rr(1), fun: RefFun(set_enabled), arg0: rr(24), arg1: rr(9) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(help_g) });
    ops.push(Opcode::Call2 { dst: r_ret, fun: RefFun(run_command), arg0: rr(24), arg1: rr(14) });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(ran_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });

    // 's' (auto mode) / 'S' (--versus, force versus) check -> build a MatchSettings + startMatch.
    // Both run the SAME handler; the only difference is g_force_versus, which the mode-select reads.
    let idx_s_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(s_versus_idx) });
    let idx_j_sv = ops.len();
    ops.push(Opcode::JEq { a: r_c, b: rr(16), offset: 0 });             // 'S' -> force-versus path
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(s_idx) });
    let idx_jne_s = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });          // neither 's' nor 'S' -> L_ORIG
    // byte == 's': auto mode -> g_force_versus = false
    ops.push(Opcode::Bool { dst: rr(9), value: ValBool(false) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_force_versus), src: rr(9) });
    let idx_jbody = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> shared handler body
    // byte == 'S': force versus -> g_force_versus = true
    let idx_sv_set = ops.len();
    if let Opcode::JEq { offset, .. } = &mut ops[idx_j_sv] { *offset = idx_sv_set as i32 - idx_j_sv as i32 - 1; }
    ops.push(Opcode::Bool { dst: rr(9), value: ValBool(true) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_force_versus), src: rr(9) });
    let idx_body = ops.len();
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_jbody] { *offset = idx_body as i32 - idx_jbody as i32 - 1; }
    use hlbc::types::{RefString as RS, RefType as RT};
    // MULTI-LAUNCH: no one-shot guard. `s` re-runs every time its byte arrives over
    // TCP, so a single session can start successive matches with different args. The
    // socket read consumes the line (the `s` drain loop below reads through the trailing
    // '\n'), so one `s` runs exactly once; only a NEW `s` line re-launches. The
    // self-bootstrap below is idempotent (getPXFResource skip) and createMode +
    // startMatch re-initialize the match each time (verified: two launches, T:STAND, no
    // crash). (Legacy g_launched guard global retained unused to keep global indices stable.)
    let _ = g_launched;
    // NOTE: the self-bootstrap (custom-char load) now runs AFTER the line is read + parts
    // are split (below), so it can load the char NAMED in the `s` args (parts[1]) rather
    // than a fixed baked char — enabling in-session character switching. See the
    // "name-driven self-bootstrap" block further down.
    // refs: parseResourceIdentifier(fullId, null) -> content-ref@669
    ops.push(Opcode::Null { dst: rr(38) });                            // null namespace
    // ---- read the rest of the line ("s <char> <stage> <assist>") into g_buf ----
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_blen), src: rr(39) });
    let idx_s_drain = ops.len();
    ops.push(Opcode::Call1 { dst: r_c, fun: RefFun(recv_char), arg0: r_handle });
    ops.push(Opcode::Int { dst: r_zero, ptr: RefInt(zero_idx) });
    let idx_s_jslt = ops.len();
    ops.push(Opcode::JSLt { a: r_c, b: r_zero, offset: 0 });            // no more data -> S_BUILD
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(nl_idx) });
    let idx_s_jeq = ops.len();
    ops.push(Opcode::JEq { a: r_c, b: rr(16), offset: 0 });             // '\n' -> S_BUILD
    ops.push(Opcode::GetGlobal { dst: rr(51), global: RefGlobal(g_buf) });
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_blen) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(bytes_set), arg0: rr(51), arg1: rr(39), arg2: r_c });
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_blen) });
    ops.push(Opcode::Incr { dst: rr(39) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_blen), src: rr(39) });
    let idx_s_jback = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> S_DRAIN
    let idx_s_build = ops.len();
    // line = g_buf.getString(0, g_blen, null)
    ops.push(Opcode::GetGlobal { dst: rr(51), global: RefGlobal(g_buf) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetGlobal { dst: rr(16), global: RefGlobal(g_blen) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call4 { dst: rr(14), fun: RefFun(bytes_getstring), arg0: rr(51), arg1: rr(39), arg2: rr(16), arg3: rr(15) });
    // parts = line.split(" ")  -> ArrayObj ; partsArr = parts.array (native, type 11)
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(space_g) });
    ops.push(Opcode::Call2 { dst: rr(52), fun: RefFun(str_split), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::Field { dst: rr(32), obj: rr(52), field: RefField(1) }); // parts.array
    // ---- name-driven self-bootstrap: load the custom char NAMED in parts[1] ------------
    // Build this launch's char strings at RUNTIME: name = parts[1] (the `s` arg), or the
    // baked default char for a bare `s`. Then resid=private::<name>, pkgid=<name>.<name>,
    // nskey=private::<name>.<name>, path=<install>/custom/<name>/<name>.fra. This lets
    // successive `s` commands switch characters in one session. The load is at `s`-dispatch
    // time (not boot), so it adds no boot-time cost.
    ops.push(Opcode::Field { dst: rr(39), obj: rr(52), field: RefField(0) });   // parts.length
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(two_idx) });
    let idx_name_jdef = ops.len();
    ops.push(Opcode::JSLt { a: rr(39), b: rr(16), offset: 0 });                 // <2 tokens -> default char
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(one_idx) });
    ops.push(Opcode::GetArray { dst: rr(55), array: rr(32), index: rr(39) });   // name = parts[1]
    let idx_name_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_name_def = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_id_g) }); // baked default name
    let idx_name_done = ops.len();
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_name_jdef] { *offset = idx_name_def as i32 - idx_name_jdef as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_name_jdone] { *offset = idx_name_done as i32 - idx_name_jdone as i32 - 1; }
    // Player 1's char name = rr55 (parts[1] or default). Extra players ride in trailing
    // space-separated tokens parts[4..] (the host fills stage+assist at parts[2..3]); the
    // characters-array build below reads them with the same parts.array GetArrays used for
    // stage/assist — no String.split (that crashed). g_name drives player 1's self-bootstrap.
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_name), src: rr(55) });
    // resid = "private::" + name
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(ns_prefixes[0]) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(53), arg1: rr(55) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_resid), src: rr(56) });
    // pkgid = name + "." + name
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(dot_g) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(55), arg1: rr(53) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(56), arg1: rr(55) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pkgid), src: rr(56) });
    // nskey = "private::" + pkgid
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(ns_prefixes[0]) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(53), arg1: rr(56) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_nskey), src: rr(56) });
    // path = customdir + name + "/" + name + ".fra"
    ops.push(Opcode::GetGlobal { dst: rr(56), global: RefGlobal(customdir_g) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(56), arg1: rr(55) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(slash_g) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(56), arg1: rr(53) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(56), arg1: rr(55) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(frasuffix_g) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(56), arg1: rr(53) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_path), src: rr(56) });
    // BASE/BUILT-IN CHAR (player 1): if there's no custom/<char>.fra on disk, player 1 is a
    // built-in (e.g. commandervideo) the engine already ships. Detect that HERE by stat'ing the
    // exact file the self-bootstrap would load (char_path) — the patcher owns this decision, no
    // env hand-off from the host (which only the GUI set, leaving the CLI/session path to crash).
    // When it's a base char, skip the custom-fra self-bootstrap UNCONDITIONALLY (running it on a
    // missing file crashes async in finishLoading with null .entities); emit_resolve below loads
    // the base char's media via its public:: load-on-demand (the same path player 2 uses).
    let base_p1 = !std::path::Path::new(&char_path).exists();
    let idx_basechar_jskip = if base_p1 {
        let i = ops.len();
        ops.push(Opcode::JAlways { offset: 0 });                        // base p1 -> skip custom load
        Some(i)
    } else { None };
    // self-bootstrap (idempotent): getPXFResource(resid) non-null -> skip the load.
    ops.push(Opcode::GetGlobal { dst: rr(58), global: RefGlobal(g_resid) });
    ops.push(Opcode::Call1 { dst: rr(60), fun: RefFun(getpxf_fn), arg0: rr(58) });
    let idx_s_load_jskip = ops.len();
    ops.push(Opcode::JNotNull { reg: rr(60), offset: 0 });             // already loaded -> L_SKIP_LOAD
    ops.push(Opcode::New { dst: rr(71) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_name) });
    ops.push(Opcode::GetGlobal { dst: rr(56), global: RefGlobal(g_path) });
    ops.push(Opcode::Null { dst: rr(73) });
    ops.push(Opcode::Call4 { dst: r_ret, fun: RefFun(resource_ctor), arg0: rr(71), arg1: rr(55), arg2: rr(56), arg3: rr(73) });
    ops.push(Opcode::Bool { dst: rr(64), value: ValBool(true) });
    ops.push(Opcode::SetField { obj: rr(71), field: RefField(res_isabs_field), src: rr(64) });
    ops.push(Opcode::GetGlobal { dst: rr(72), global: RefGlobal(rt_global) });
    ops.push(Opcode::Field { dst: rr(16), obj: rr(72), field: RefField(pxf_field) });
    ops.push(Opcode::SetField { obj: rr(71), field: RefField(res_type_field), src: rr(16) });
    // RM.requiredMediaIds = ["*"] so finishLoading's preload populates entityMap
    ops.push(Opcode::Type { dst: rr(31), ty: RT(13) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(one_idx) });
    ops.push(Opcode::Call2 { dst: rr(32), fun: RefFun(256), arg0: rr(31), arg1: rr(39) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(star_g) });
    ops.push(Opcode::SetArray { array: rr(32), index: rr(39), src: rr(53) });
    ops.push(Opcode::Call1 { dst: rr(33), fun: RefFun(257), arg0: rr(32) });
    ops.push(Opcode::GetGlobal { dst: rr(65), global: RefGlobal(rm_g) });
    ops.push(Opcode::SetField { obj: rr(65), field: RefField(reqmedia_field), src: rr(33) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(fetch_threaded), arg0: rr(71) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(finish_loading), arg0: rr(71) });
    ops.push(Opcode::Call1 { dst: rr(60), fun: RefFun(get_data_as_pxf), arg0: rr(71) });
    ops.push(Opcode::Field { dst: rr(66), obj: rr(60), field: RefField(pxf_entities_field) });
    ops.push(Opcode::Field { dst: rr(39), obj: rr(66), field: RefField(0) });   // .length
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(zero_idx) });
    let idx_sl_loop = ops.len();
    let idx_sl_jge = ops.len();
    ops.push(Opcode::JSGte { a: rr(16), b: rr(39), offset: 0 });        // idx >= len -> build_done
    ops.push(Opcode::Call2 { dst: r_ret, fun: RefFun(cache_sprite_entity_data), arg0: rr(60), arg1: rr(16) });
    ops.push(Opcode::Incr { dst: rr(16) });
    let idx_sl_jback = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                           // -> build_loop
    let idx_sl_done = ops.len();
    // re-cache the main sprite entity under all 3 spriteContent key forms (buried-VFX fix)
    ops.push(Opcode::Field { dst: rr(63), obj: rr(60), field: RefField(pxf_entitymap_field) });
    let idx_sl_emap_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(63), offset: 0 });                // null entityMap -> addres
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_name) });
    ops.push(Opcode::Call2 { dst: rr(28), fun: RefFun(sm_get), arg0: rr(63), arg1: rr(55) });
    let idx_sl_ent_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(28), offset: 0 });                // null entity -> addres
    ops.push(Opcode::UnsafeCast { dst: rr(74), src: rr(28) });
    ops.push(Opcode::Null { dst: rr(75) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_name) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: rr(55), arg1: rr(74), arg2: rr(75) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_pkgid) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: rr(55), arg1: rr(74), arg2: rr(75) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_nskey) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: rr(55), arg1: rr(74), arg2: rr(75) });
    let idx_sl_addres = ops.len();
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(add_resource), arg0: rr(71) });
    // record the loaded char's sprite-cache key for the generic self-heal fallback
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_nskey) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_loaded_spritekey), src: rr(55) });
    let idx_sl_skip = ops.len();
    if let Opcode::JNotNull { offset, .. } = &mut ops[idx_s_load_jskip] { *offset = idx_sl_skip as i32 - idx_s_load_jskip as i32 - 1; }
    if let Some(j) = idx_basechar_jskip {
        if let Opcode::JAlways { offset, .. } = &mut ops[j] { *offset = idx_sl_skip as i32 - j as i32 - 1; }
    }
    if let Opcode::JSGte  { offset, .. } = &mut ops[idx_sl_jge]   { *offset = idx_sl_done as i32 - idx_sl_jge as i32 - 1; }
    if let Opcode::JAlways{ offset, .. } = &mut ops[idx_sl_jback] { *offset = idx_sl_loop as i32 - idx_sl_jback as i32 - 1; }
    if let Opcode::JNull  { offset, .. } = &mut ops[idx_sl_emap_jnull] { *offset = idx_sl_addres as i32 - idx_sl_emap_jnull as i32 - 1; }
    if let Opcode::JNull  { offset, .. } = &mut ops[idx_sl_ent_jnull]  { *offset = idx_sl_addres as i32 - idx_sl_ent_jnull as i32 - 1; }
    // the self-bootstrap reused rr32 for the requiredMediaIds array; restore parts.array
    // (rr52 survived) so the downstream char/stage/assist resolver still sees the tokens.
    ops.push(Opcode::Field { dst: rr(32), obj: rr(52), field: RefField(1) }); // parts.array (restored)
    // ---- close any live match before starting a new one (multi-`s` re-launch) ----
    // Reset the menu-reveal one-shot so THIS launch's loading menu is also dismissed once
    // its match goes live (the reveal logic at the top of the dispatch re-fires next frame).
    ops.push(Opcode::Bool { dst: rr(9), value: ValBool(false) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_shown), src: rr(9) });
    // MatchController.cleanupMatch@18325(currentMatch): removes it from _matches, kills its
    // events, nulls currentMatch, tears down its entities. First launch -> currentMatch null
    // -> skipped. Without this, successive `s` commands stacked matches (old ones never closed).
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });            // MatchController statics
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) }); // currentMatch
    let idx_jnocm = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                              // none -> skip cleanup
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(cleanupmatch_fn), arg0: rr(44) });        // cleanupMatch(currentMatch)
    let idx_after_cleanup = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_jnocm] { *offset = idx_after_cleanup as i32 - idx_jnocm as i32 - 1; }
    // ---- create the match mode: createMode({ resource: <mode ref> }) ----
    // Pick trainingmode vs vsmode by roster size at runtime: parts = [s,char0,stage,assist,extras…]
    // so player count N = max(1, parts.length - 3); N >= 3 (parts.length >= 6) -> vsmode (free-for-all,
    // up to 4 players), else trainingmode (1-2, the parity-harness dummy path). PEPTIDE_FORCE_TRAINING
    // bakes trainingmode unconditionally.
    if force_training {
        ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(trainingmode_g) });
    } else {
        ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(trainingmode_g) }); // default: training
        // force-versus (`--versus`, the 'S' byte) -> versus regardless of roster size
        ops.push(Opcode::GetGlobal { dst: rr(9), global: RefGlobal(g_force_versus) });
        let idx_mode_force = ops.len();
        ops.push(Opcode::JTrue { cond: rr(9), offset: 0 });                              // forced -> versus
        ops.push(Opcode::Field { dst: rr(16), obj: rr(52), field: RefField(0) });        // parts.length
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(six_idx) });
        let idx_mode_train = ops.len();
        ops.push(Opcode::JSLt { a: rr(16), b: rr(39), offset: 0 });                       // length < 6 -> keep training
        let idx_mode_versus = ops.len();
        if let Opcode::JTrue { offset, .. } = &mut ops[idx_mode_force] { *offset = idx_mode_versus as i32 - idx_mode_force as i32 - 1; }
        ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(vsmode_g) });         // versus
        let idx_mode_done = ops.len();
        if let Opcode::JSLt { offset, .. } = &mut ops[idx_mode_train] { *offset = idx_mode_done as i32 - idx_mode_train as i32 - 1; }
    }
    ops.push(Opcode::Call2 { dst: rr(25), fun: RefFun(parseresid_fn), arg0: rr(14), arg1: rr(38) }); // mode resource ref
    ops.push(Opcode::New { dst: rr(34) });                             // modeConfig dynobj
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(resource_str), src: rr(25) });
    ops.push(Opcode::ToVirtual { dst: rr(49), src: rr(34) });          // -> modeConfig virtual@1194
    ops.push(Opcode::Call1 { dst: rr(48), fun: RefFun(create_mode), arg0: rr(49) }); // FraymakersMode
    // ---- char/stage/assist refs: honor each `s` arg INDEPENDENTLY, falling back
    // to the baked default for any slot the caller omitted. The old all-or-nothing
    // "need 4 tokens or use ALL baked defaults" branch ignored partial args — e.g.
    // `s mario` self-bootstrapped mario but launched the baked default char/stage/
    // assist. Every slot resolves through emit_resolve (bare-name load-on-demand).
    //
    // CHAR: from g_name, which the name-driven self-bootstrap above already set to
    // parts[1] (the `s` arg) or the baked default — and loaded. So this matches the
    // char that was actually loaded, for both `s <char> …` and a bare `s`.
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(g_name) });
    emit_resolve(&mut ops, 55, 26, char_cmap_field);
    // STAGE: parts.length >= 3 ? parts[2] : baked stage default.
    ops.push(Opcode::Field { dst: rr(16), obj: rr(52), field: RefField(0) }); // parts.length
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(three_idx) });
    let idx_stage_jdef = ops.len();
    ops.push(Opcode::JSLt { a: rr(16), b: rr(39), offset: 0 });             // <3 tokens -> default
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(two_idx) });
    ops.push(Opcode::GetArray { dst: rr(55), array: rr(32), index: rr(39) }); // parts[2]
    let idx_stage_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_stage_def = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(stage_bare_g) });
    let idx_stage_done = ops.len();
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_stage_jdef] { *offset = idx_stage_def as i32 - idx_stage_jdef as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_stage_jdone] { *offset = idx_stage_done as i32 - idx_stage_jdone as i32 - 1; }
    // CUSTOM STAGE self-bootstrap (only emitted when the baked stage has a custom .fra).
    // rr(55) holds the stage name (parts[2]/baked); we DON'T touch it — the load uses the
    // baked private:: resid + path + bare name, so emit_resolve below still sees rr(55).
    if custom_stage {
        // idempotent: skip if already in the pool. NB: requiredMediaIds=["*"] is left as
        // the char self-bootstrap (which runs first for a custom p1) set it, so this block
        // never touches rr(32) = parts.array (the downstream stage/assist resolves need it).
        ops.push(Opcode::GetGlobal { dst: rr(58), global: RefGlobal(stage_resid_g) });
        ops.push(Opcode::Call1 { dst: rr(60), fun: RefFun(getpxf_fn), arg0: rr(58) });
        let idx_st_jskip = ops.len();
        ops.push(Opcode::JNotNull { reg: rr(60), offset: 0 });                 // already loaded -> skip
        ops.push(Opcode::New { dst: rr(71) });
        ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(stage_bare_g) }); // Resource id = bare name
        ops.push(Opcode::GetGlobal { dst: rr(56), global: RefGlobal(stage_path_g) });
        ops.push(Opcode::Null { dst: rr(73) });
        ops.push(Opcode::Call4 { dst: r_ret, fun: RefFun(resource_ctor), arg0: rr(71), arg1: rr(53), arg2: rr(56), arg3: rr(73) });
        ops.push(Opcode::Bool { dst: rr(64), value: ValBool(true) });
        ops.push(Opcode::SetField { obj: rr(71), field: RefField(res_isabs_field), src: rr(64) });
        ops.push(Opcode::GetGlobal { dst: rr(72), global: RefGlobal(rt_global) });
        ops.push(Opcode::Field { dst: rr(16), obj: rr(72), field: RefField(pxf_field) });
        ops.push(Opcode::SetField { obj: rr(71), field: RefField(res_type_field), src: rr(16) });
        ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(fetch_threaded), arg0: rr(71) });
        ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(finish_loading), arg0: rr(71) });
        ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(add_resource), arg0: rr(71) });
        let idx_st_skip = ops.len();
        if let Opcode::JNotNull { offset, .. } = &mut ops[idx_st_jskip] { *offset = idx_st_skip as i32 - idx_st_jskip as i32 - 1; }
        eprintln!("custom-stage self-bootstrap: emitted (private::{sname} <- {stage_custom_path})");
    }
    emit_resolve(&mut ops, 55, 25, stage_cmap_field); // stage ref (loads on demand)
    // ASSIST: parts.length >= 4 ? parts[3] : baked assist default.
    ops.push(Opcode::Field { dst: rr(16), obj: rr(52), field: RefField(0) }); // parts.length
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(four_idx) });
    let idx_assist_jdef = ops.len();
    ops.push(Opcode::JSLt { a: rr(16), b: rr(39), offset: 0 });             // <4 tokens -> default
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(three_idx) });
    ops.push(Opcode::GetArray { dst: rr(55), array: rr(32), index: rr(39) }); // parts[3]
    let idx_assist_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_assist_def = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(assist_bare_g) });
    let idx_assist_done = ops.len();
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_assist_jdef] { *offset = idx_assist_def as i32 - idx_assist_jdef as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_assist_jdone] { *offset = idx_assist_done as i32 - idx_assist_jdone as i32 - 1; }
    emit_resolve(&mut ops, 55, 42, assist_cmap_field); // assist ref (loads on demand)
    // patch the line-reader jumps
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_s_jslt] { *offset = idx_s_build as i32 - idx_s_jslt as i32 - 1; }
    if let Opcode::JEq { offset, .. } = &mut ops[idx_s_jeq] { *offset = idx_s_build as i32 - idx_s_jeq as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_s_jback] { *offset = idx_s_drain as i32 - idx_s_jback as i32 - 1; }
    // ---- player config data { character: rr26, assist: rr42, port: 0 } ----
    ops.push(Opcode::New { dst: rr(27) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(character_str), src: rr(26) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(assist_str), src: rr(42) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(port_str), src: rr(28) });
    // team = port (0). MUST be >=0: sanitizePlayerConfig@6250 op112 only skips its getPlayerPortColor
    // branch (which hangs for index>=1) when team>=0. team=port also gives free-for-all distinct teams.
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(team_str), src: rr(28) }); // rr28 = boxed 0
    ops.push(Opcode::ToVirtual { dst: rr(29), src: rr(27) });          // -> player virtual@1957 (player 1)
    // characters = [player1, player2, …, playerN] — ALL roster members go into the INITIAL
    // startMatch array so the engine natively constructs + registers every fighter via
    // _offlineMatchStart@6251 (its per-char loop runs the full PlayerConfig path for each).
    // Player count N = 1 + (parts.length - 4): player0 is parts[1], each extra is parts[4..].
    // Player1 (rr29, self-bootstrapped above) is index 0; each extra name is resolved by id
    // (emit_resolve = load-on-demand) and built into a player virtual with port=team=index.
    //
    // CRITICAL: every array element MUST share the t1957 type rr29 carries. A differently-typed
    // element makes _offlineMatchStart's per-char ToVirtual choke (hang). So store player1 first,
    // then REUSE rr29 for each extra (the array already holds the prior ref, so reuse is safe).
    // rr19=N, rr20=loop index (both Int, untouched by emit_resolve which scratches rr16/rr39).
    // rr32=chars native array (survives resolve). Single-player (length<5) -> N=1 and the loop
    // body never runs, byte-identical to the old size-1 build; a 2-char roster reproduces the old
    // mirror exactly (N=2). 3-4 players is the new path (Fraymakers has 4 ports).
    //
    // N = (length >= 5) ? length - 3 : 1
    ops.push(Opcode::Int { dst: rr(19), ptr: RefInt(one_idx) });             // N = 1 (player0 only)
    ops.push(Opcode::Field { dst: rr(16), obj: rr(52), field: RefField(0) }); // parts.length
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(five_idx) });
    let idx_arr_jsolo = ops.len();
    ops.push(Opcode::JSLt { a: rr(16), b: rr(39), offset: 0 });               // length<5 -> N stays 1
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(three_idx) });
    ops.push(Opcode::Sub { dst: rr(19), a: rr(16), b: rr(39) });             // N = length - 3
    let idx_arr_nset = ops.len();
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_arr_jsolo] { *offset = idx_arr_nset as i32 - idx_arr_jsolo as i32 - 1; }
    // alloc chars[N] into rr67 — a register neither emit_load_char nor emit_resolve clobbers, so
    // the array survives each extra's synchronous self-bootstrap below; store player0 at [0].
    ops.push(Opcode::Type { dst: rr(31), ty: RT(1957) });
    ops.push(Opcode::Call2 { dst: rr(67), fun: RefFun(256), arg0: rr(31), arg1: rr(19) }); // alloc[N]
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::SetArray { array: rr(67), index: rr(39), src: rr(29) }); // [0]=player0
    // Build players 2..N as UNROLLED, guarded straight-line blocks (Fraymakers has 4 ports, so at
    // most 3 extras). A runtime loop does NOT work here: emitting emit_resolve inside a back-branch
    // corrupts the loop counter at runtime (the static bytecode looks correct, but only one pass
    // runs), so each extra is its own block. For extra k (2..4) the player lands at characters[k-1]
    // from parts[k+2]; the block runs only when N >= k. Each extra is SYNCHRONOUSLY self-bootstrapped
    // (emit_load_char) before it is resolved, so a DISTINCT custom char (not baked as player0) has
    // its media + content map fully loaded before startMatch — without it Match.spawnPlayer
    // null-derefs characterPxfContentMap. emit_load_char is idempotent: a same-as-p0 or base-game
    // char is already loaded and the load is skipped.
    // index pools keyed by extra-player k (k = 2,3,4): guard value k, parts index k+2, slot/port k-1.
    let pk_guard = [two_idx, three_idx, four_idx];   // k    = 2,3,4
    let pk_parts = [four_idx, five_idx, six_idx];     // k+2  = 4,5,6 (parts index)
    let pk_slot  = [one_idx, two_idx, three_idx];     // k-1  = 1,2,3 (port / slot)
    for j in 0..3usize {
        // guard: N (rr19) >= k ? build : skip
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(pk_guard[j]) });
        let idx_skip = ops.len();
        ops.push(Opcode::JSLt { a: rr(19), b: rr(39), offset: 0 });           // N < k -> skip this player
        // name = parts[k+2]  (k=2 -> parts[4])
        ops.push(Opcode::Field { dst: rr(31), obj: rr(52), field: RefField(1) }); // parts.array (native)
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(pk_parts[j]) });
        ops.push(Opcode::GetArray { dst: rr(55), array: rr(31), index: rr(39) }); // parts[k+2] -> rr55
        // synchronously load this char (idempotent). scratches rr31/32/33/39/55/...
        emit_load_char(&mut ops, 55);
        // re-fetch the name (emit_load_char scratched rr55/rr31/rr39), then resolve it -> rr26
        ops.push(Opcode::Field { dst: rr(31), obj: rr(52), field: RefField(1) });
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(pk_parts[j]) });
        ops.push(Opcode::GetArray { dst: rr(55), array: rr(31), index: rr(39) });
        emit_resolve(&mut ops, 55, 26, char_cmap_field);                     // char ref -> rr26
        // player virtual { character: rr26, assist: rr42, port: k-1, team: k-1 } -> REUSE rr29
        ops.push(Opcode::New { dst: rr(27) });
        ops.push(Opcode::DynSet { obj: rr(27), field: RS(character_str), src: rr(26) });
        ops.push(Opcode::DynSet { obj: rr(27), field: RS(assist_str), src: rr(42) });
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(pk_slot[j]) });
        ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });                // port = k-1
        ops.push(Opcode::DynSet { obj: rr(27), field: RS(port_str), src: rr(28) });
        // team=port=k-1 (>=1): skips sanitizePlayerConfig's player-border branch; distinct ffa team.
        ops.push(Opcode::DynSet { obj: rr(27), field: RS(team_str), src: rr(28) });
        ops.push(Opcode::ToVirtual { dst: rr(29), src: rr(27) });             // REUSE rr29 (t1957)
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(pk_slot[j]) });
        ops.push(Opcode::SetArray { array: rr(67), index: rr(39), src: rr(29) }); // characters[k-1]
        let idx_after = ops.len();
        if let Opcode::JSLt { offset, .. } = &mut ops[idx_skip] { *offset = idx_after as i32 - idx_skip as i32 - 1; }
    }
    ops.push(Opcode::Call1 { dst: rr(33), fun: RefFun(257), arg0: rr(67) }); // wrap -> ArrayObj
    // matchSettings = { stage: rr25, matchRules: defaultMatchRules } (virtual@675)
    ops.push(Opcode::New { dst: rr(34) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(stage_str), src: rr(25) });
    ops.push(Opcode::GetGlobal { dst: rr(41), global: RefGlobal(ms_global) });
    ops.push(Opcode::Field { dst: rr(40), obj: rr(41), field: RefField(dmr_field) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(matchrules_str), src: rr(40) });
    // lives + timer from match_settings.conf. Same Int->ToDyn->DynSet path the
    // `port` field uses above; rr39 = int scratch, rr28 = dyn scratch (free here).
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(lives_idx) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(lives_str), src: rr(28) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(time_idx) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(time_str), src: rr(28) });
    // remaining config — same const->ToDyn->DynSet path; values baked from
    // match_settings.conf (interpreter.rs). itemFrequency (Int) + teamAttack (Bool)
    // are the fields the engine's TrainingMode start-flow accepts on a programmatic
    // match. The float fields (damageRatio/startDamage/sizeRatio) are intentionally
    // NOT applied on Fraymakers: TrainingMode's startMatch silently rejects them
    // (they belong to the versus-rules path, which our minimal self-bootstrap launch
    // doesn't drive), so setting them aborts the whole launch. They remain active on
    // SSF2 (versus mode). This is the same "apply the subset each engine supports"
    // OOP outcome as assists being a no-op on SSF2 and stamina a no-op on Fraymakers.
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(itemfreq_idx) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(39) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(itemfreq_str), src: rr(28) });
    // teamAttack: rr(64) is the function's Bool scratch (type 7), free past the
    // self-bootstrap load above.
    ops.push(Opcode::Bool { dst: rr(64), value: ValBool(cfg_team_damage) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(64) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(teamattack_str), src: rr(28) });
    // hazards: the switch a stage reads to decide what of itself to run. Replacing matchConfig
    // wholesale means an unset field is FALSE, so leaving this out launched every match with
    // hazards off -- and a converted stage that honours the switch then looked broken rather than
    // obedient. Both engines have the switch, so it lives in the shared match settings.
    ops.push(Opcode::Bool { dst: rr(64), value: ValBool(cfg_hazards) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(64) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(hazards_str), src: rr(28) });
    // teams=false (free-for-all): MatchSettingsConfig.teams (field 23, Bool) drives prepTeams@6237.
    ops.push(Opcode::Bool { dst: rr(64), value: ValBool(false) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(64) });
    ops.push(Opcode::DynSet { obj: rr(34), field: RS(teams_str), src: rr(28) });
    ops.push(Opcode::ToVirtual { dst: rr(35), src: rr(34) });          // -> matchSettings virtual@675
    // config = { characters, matchSettings, pauseMenu: null } (virtual@4482)
    ops.push(Opcode::New { dst: rr(27) });                             // dynobj (4366)
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(characters_str), src: rr(33) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(matchsettings_str), src: rr(35) });
    ops.push(Opcode::DynSet { obj: rr(27), field: RS(pausemenu_str), src: rr(38) }); // pauseMenu = null
    ops.push(Opcode::ToVirtual { dst: rr(50), src: rr(27) });          // -> config virtual@4482
    // HEADLESS ONLY: show FM's native pre-match loading screen. The menus normally set
    // MatchController.loadingScreenFactory before startMatch; the fast boot skips them, so
    // startMatch's `if (loadingScreenFactory != null)` gate fails and no screen shows.
    // Set the factory to the engine's own createLoadingScreen so startMatch builds + shows
    // it (parity with SSF2's loadingMenu). Gated on `headless` so a manual `s` in a --full
    // bridge boot doesn't force it.
    if headless {
        ops.push(Opcode::GetGlobal { dst: ls_mc_reg, global: RefGlobal(mc_g) });
        ops.push(Opcode::StaticClosure { dst: ls_fac_reg, fun: RefFun(create_loading_screen) });
        ops.push(Opcode::SetField { obj: ls_mc_reg, field: RefField(lsf_field), src: ls_fac_reg });
    }
    // mode.startMatch(config)  — runs the engine's offline-match flow (gates, menu suspend/restore).
    ops.push(Opcode::Call2 { dst: r_ret, fun: RefFun(mode_start_match), arg0: rr(48), arg1: rr(50) });
    if FM_MULTIPLAYER {
    // reset the p1 stash for this new match — a prior 2-player p1 must not leak in. r_ret is
    // type 0 (= g_live_p1's type) and free here (mode_start_match's return is unused).
    ops.push(Opcode::Null { dst: r_ret });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_live_p1), src: r_ret });
    // For a 2-player initial-array launch (length>=5) set g_live_p1 to a non-null sentinel so the
    // eval hook binds p1 = currentMatch.characters[1] (the real, natively-constructed player 2) via
    // its reliable native-array path. Single-char stays null (characters[1] would read OOB).
    ops.push(Opcode::Field { dst: rr(16), obj: rr(52), field: RefField(0) }); // parts.length
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(five_idx) });
    let idx_p1sent_jskip = ops.len();
    ops.push(Opcode::JSLt { a: rr(16), b: rr(39), offset: 0 });               // length<5 -> leave null
    ops.push(Opcode::ToDyn { dst: r_ret, src: rr(48) });                      // sentinel = mode (non-null)
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_live_p1), src: r_ret });
    let idx_p1sent_skip = ops.len();
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_p1sent_jskip] { *offset = idx_p1sent_skip as i32 - idx_p1sent_jskip as i32 - 1; }
    // ---- extra players: ALL roster members are now in the INITIAL characters array (the
    // N-player loop above), so the initial spawn needs NO deferred work. Set g_pending_idx =
    // parts.length so the per-frame deferred-spawn loop sees `idx >= length` and stays idle.
    // The stash (g_saved_parts + g_pending_assist + g_pending_port) is still kept so the
    // on-demand `addCharacter` (#3) command can re-arm the deferred path to add one more
    // fighter to the LIVE match later. g_pending_parts stays null = idle until then.
    ops.push(Opcode::Null { dst: rr(33) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_parts), src: rr(33) }); // idle (all players in initial array)
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_saved_parts), src: rr(52) });  // keep a copy for addCharacter (#3)
    ops.push(Opcode::Field { dst: rr(39), obj: rr(52), field: RefField(0) });        // parts.length
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_idx), src: rr(39) });   // idx = length -> deferred loop drained
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_port), src: rr(19) });  // next addChar port = N (free-for-all slot past the roster)
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_assist), src: rr(42) }); // resolved assist ref (rr42) for addCharacter
    }
    let _ = (one_idx, launched_g, getresid_fn, spawn_char_fn, five_idx, pxf_entities_field,
             cache_sprite_entity_data, queued_g, pend_g, mr_g, mi_g, sp_ok_g, sp_null_g,
             g_pending_parts, g_pending_idx, g_pending_port, spawn_player_fn, pc_import_json,
             cfg_pc_reg, star_g, reqmedia_field);
    // ack: "LAUNCHED <charId> <stageId> <assistId>\n" — echoes resolved content ids
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(launched2_g) });
    ops.push(Opcode::Call1 { dst: rr(53), fun: RefFun(getcontentid_fn), arg0: rr(26) }); // char id
    ops.push(Opcode::Call2 { dst: rr(14), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(space_g) });
    ops.push(Opcode::Call2 { dst: rr(14), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::Call1 { dst: rr(53), fun: RefFun(getcontentid_fn), arg0: rr(25) }); // stage id
    ops.push(Opcode::Call2 { dst: rr(14), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(space_g) });
    ops.push(Opcode::Call2 { dst: rr(14), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::Call1 { dst: rr(53), fun: RefFun(getcontentid_fn), arg0: rr(42) }); // assist id
    ops.push(Opcode::Call2 { dst: rr(14), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(nl_g) });
    ops.push(Opcode::Call2 { dst: rr(14), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });

    // 'q' query -> report whether MatchController.currentMatch is live.
    let idx_q_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(q_idx) });
    let idx_jne_q = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });          // not 'q' -> L_ORIG
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    // DIAGNOSTIC: report the buried-VFX key's cache status every q, to watch the
    // post-startMatch eviction timeline frame-by-frame (q is callable repeatedly).
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let l_null = a.label();
        let l_done = a.label();
        a.op(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(ns_sandbag_g) });
        a.op(Opcode::Call1 { dst: rr(28), fun: RefFun(get_sprite_entity), arg0: rr(55) });
        a.jnull(rr(28), l_null);
        a.op(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(nspr_ok_g) });
        a.jalways(l_done);
        a.place(l_null);
        a.op(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(nspr_null_g) });
        a.place(l_done);
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
        a.op(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) }); // currentMatch
    let idx_q_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                 // null -> NO_MATCH write
    // live:
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(q_live_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_q_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> L_ORIG
    // currentMatch null: check _matches.length to tell "match exists" from "none".
    let idx_q_nomatch = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(45), obj: rr(43), field: RefField(matches_field) }); // _matches (ArrayObj)
    let idx_q_jm_null = ops.len();
    ops.push(Opcode::JNull { reg: rr(45), offset: 0 });                 // _matches null -> NO_MATCH
    ops.push(Opcode::Field { dst: rr(46), obj: rr(45), field: RefField(0) }); // ArrayObj.length
    ops.push(Opcode::Int { dst: rr(47), ptr: RefInt(zero_idx) });
    let idx_q_jm_empty = ops.len();
    ops.push(Opcode::JSLte { a: rr(46), b: rr(47), offset: 0 });        // length<=0 -> NO_MATCH
    // _matches non-empty: a Match object exists even though currentMatch is null.
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(q_matches_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_q_jm_done = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> L_ORIG
    // truly no match:
    let idx_q_truly_none = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(q_nomatch_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });

    // ---- 'k' command: dump pool keys to find actual namespace ----
    // Iterate RM.pool (ArrayObj f12), call getFullyQualifiedResourceId on each element,
    // write "K:<fqid>\n" for each. Uses GetArray into dynobj register (type 9 = rr(28))
    // + UnsafeCast to absres_t (394 = rr(68)) before calling getfqid — the same pattern
    // used by addDirToLoadQueue ops 47-50 (GetArray→dynobj→UnsafeCast→concrete call).
    let idx_k_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(k_idx) });
    let idx_jne_k = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'k' -> L_ORIG
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    // diagnostic: UgcUtil.directoriesToLoad (g3449 f11, ArrayObj) length > 0?
    ops.push(Opcode::GetGlobal { dst: rr(65), global: RefGlobal(ugc_statics_g) });
    ops.push(Opcode::Field { dst: rr(66), obj: rr(65), field: RefField(11) }); // directoriesToLoad
    let idx_k_dirs_null = ops.len();
    ops.push(Opcode::JNull { reg: rr(66), offset: 0 });                  // null -> skip dirs report
    ops.push(Opcode::Field { dst: rr(16), obj: rr(66), field: RefField(0) }); // .length
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    let idx_k_dirs_jle = ops.len();
    ops.push(Opcode::JSLte { a: rr(16), b: rr(39), offset: 0 });          // <=0 -> dirs_zero
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(k_dirs_pos_g) });
    let idx_k_dirs_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_k_dirs_zero = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(k_dirs_zero_g) });
    let idx_k_dirs_done = ops.len();
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    let idx_k_after_dirs = ops.len();
    // pool = RM.pool (field 12)
    ops.push(Opcode::GetGlobal { dst: rr(65), global: RefGlobal(rm_g) });
    ops.push(Opcode::Field { dst: rr(66), obj: rr(65), field: RefField(pool_field) });
    let idx_k_jnull_pool = ops.len();
    ops.push(Opcode::JNull { reg: rr(66), offset: 0 });                   // null pool -> k_done
    ops.push(Opcode::Field { dst: rr(39), obj: rr(66), field: RefField(0) }); // pool.length
    ops.push(Opcode::Field { dst: rr(67), obj: rr(66), field: RefField(1) }); // pool.array
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(zero_idx) });          // counter=0
    let idx_k_loop = ops.len();
    let idx_k_jge = ops.len();
    ops.push(Opcode::JSGte { a: rr(16), b: rr(39), offset: 0 });          // done -> k_done
    ops.push(Opcode::GetArray { dst: rr(28), array: rr(67), index: rr(16) }); // pool[i] → dynobj
    ops.push(Opcode::Incr { dst: rr(16) });                               // counter++
    let idx_k_jnull_elem = ops.len();
    ops.push(Opcode::JNull { reg: rr(28), offset: 0 });                   // null elem -> loop
    // UnsafeCast dynobj → AbstractResource (type 394 = absres_t), safe since pool has AbstractResource
    ops.push(Opcode::UnsafeCast { dst: rr(68), src: rr(28) });
    ops.push(Opcode::Call1 { dst: rr(57), fun: RefFun(getfqid_fn), arg0: rr(68) }); // getfqid
    let idx_k_jnull_fqid = ops.len();
    ops.push(Opcode::JNull { reg: rr(57), offset: 0 });                   // null fqid -> loop
    // write "K:" + fqid + "\n"
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(k_prefix_g) });
    ops.push(Opcode::Call2 { dst: rr(58), fun: RefFun(str_add), arg0: rr(53), arg1: rr(57) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(nl_g) });
    ops.push(Opcode::Call2 { dst: rr(58), fun: RefFun(str_add), arg0: rr(58), arg1: rr(53) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(58), arg2: rr(15) });
    let idx_k_jback = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                              // -> k_loop
    let idx_k_done = ops.len();
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // k handler done; fall through to the 'w' check.

    // ---- 'w' command: FM live tree walk (the SSF2 `tree` analog) ----
    // hl ArrayObj doesn't reflect length/index in hscript, so this is bytecode: read
    // currentMatch.gameObjects (length=field0, native array=field1) and emit one line per entity —
    // "  <class> @(x,y)\n" (class via Type.getClassName(Type.getClass(el)), pos via Entity.getX/Y).
    // ---- 'y' command: the engine screenshots ITSELF and returns the PNG as hex ----
    // Any display object reaches the scene, so this reuses the same StageApi -> container ->
    // h2d.Object hop the walk uses, then asks Heaps to read its own framebuffer back.
    let idx_y_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(y_idx) });
    let idx_jne_y = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'y' -> 'w' check
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let fail = a.label();
        let end = a.label();
        let r_out2 = a.reg(output_t);
        let r_mc = a.reg(mc_statics_t);
        let r_cm = a.reg(match_t);
        let r_st = a.reg(stage_entity_t);
        let r_api = a.reg(stage_api_t);
        let r_capi = a.reg(container_api_t);
        let r_cont = a.reg(disp_container_t);
        let r_h2d = a.reg(h2d_object_t);
        let r_scene = a.reg(h2d_scene_t);
        let r_bmp = a.reg(h2d_bitmap_t);
        let r_tile = a.reg(h2d_tile_t);
        let r_tex = a.reg(texture_t);
        let r_px = a.reg(pixels_t);
        let r_bytes = a.reg(iobytes_t);
        let r_hex = a.reg(str_t);
        let r_lay = a.reg(cap_layer_t);
        let r_bnd = a.reg(cap_bounds_t);
        let r_lvl = a.reg(png_level_t);
        let r_acc = a.reg(str_t);
        let r_lbl = a.reg(str_t);
        let r_ret2 = a.reg(9);
        let r_void = a.reg(0);
        a.op(Opcode::Field { dst: r_out2, obj: r_sock2, field: RefField(out_field) });
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, fail);
        a.op(Opcode::Field { dst: r_st, obj: r_cm, field: RefField(stage_field) });
        a.jnull(r_st, fail);
        // stage -> its characters layer -> the heaps object that owns the scene
        a.op(Opcode::New { dst: r_api });
        a.op(Opcode::Call2 { dst: r_void, fun: RefFun(stage_api_ctor), arg0: r_api, arg1: r_st });
        a.op(Opcode::Call1 { dst: r_capi, fun: RefFun(layer_getters[5]), arg0: r_api });
        a.jnull(r_capi, fail);
        a.op(Opcode::Field { dst: r_cont, obj: r_capi, field: RefField(capi_container_field) });
        a.jnull(r_cont, fail);
        a.op(Opcode::Field { dst: r_h2d, obj: r_cont, field: RefField(cont_display_field) });
        a.jnull(r_h2d, fail);
        a.op(Opcode::Call1 { dst: r_scene, fun: RefFun(h2d_getscene), arg0: r_h2d });
        a.jnull(r_scene, fail);
        // captureBitmap(null target) allocates its own render target sized to the scene
        a.op(Opcode::Null { dst: r_tile });
        a.op(Opcode::Call2 { dst: r_bmp, fun: RefFun(scene_capturebitmap), arg0: r_scene, arg1: r_tile });
        a.jnull(r_bmp, fail);
        a.op(Opcode::Field { dst: r_tile, obj: r_bmp, field: RefField(bitmap_tile_field) });
        a.jnull(r_tile, fail);
        a.op(Opcode::Call1 { dst: r_tex, fun: RefFun(tile_gettexture), arg0: r_tile });
        a.jnull(r_tex, fail);
        // read the framebuffer back: whole surface (null layer / mip / crop)
        a.op(Opcode::Null { dst: r_lay });
        a.op(Opcode::Null { dst: r_bnd });
        a.op(Opcode::Call4 { dst: r_px, fun: RefFun(tex_capturepixels), arg0: r_tex, arg1: r_lay, arg2: r_lay, arg3: r_bnd });
        a.jnull(r_px, fail);
        a.op(Opcode::Null { dst: r_lvl });
        a.op(Opcode::Call2 { dst: r_bytes, fun: RefFun(pixels_topng), arg0: r_px, arg1: r_lvl });
        a.jnull(r_bytes, fail);
        a.op(Opcode::Call1 { dst: r_hex, fun: RefFun(bytes_tohex), arg0: r_bytes });
        a.jnull(r_hex, fail);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(shot_hdr_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_hex });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_nl_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Null { dst: r_lbl });
        a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
        a.jalways(end);
        a.place(fail);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(shot_fail_g) });
        a.op(Opcode::Null { dst: r_lbl });
        a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
        a.place(end);
        a.op(Opcode::Call1 { dst: r_ret2, fun: RefFun(flush), arg0: r_out2 });
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    let y_skip = (ops.len() - idx_jne_y - 1) as i32;
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_y] { *offset = y_skip; }

    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(w_idx) });
    let idx_jne_w = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'w' -> 'l' check (patched)
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let nomatch = a.label();
        let end = a.label();
        let r_out2 = a.reg(output_t);
        let r_mc   = a.reg(mc_statics_t);
        let r_cm   = a.reg(match_t);
        let r_go   = a.reg(38);              // ArrayObj
        let r_ts   = a.reg(tickstack_t);     // Match.tickStack
        let r_len  = a.reg(3);
        let r_i    = a.reg(3);
        let r_arr  = a.reg(11);              // native array
        let r_el   = a.reg(9);               // dynobj element
        let r_ent  = a.reg(entity_t);
        let r_cls  = a.reg(9);               // Class<Dynamic>, passed by pointer
        let r_str  = a.reg(str_t);           // class name / Std.string scratch
        let r_f    = a.reg(6);               // Float
        let r_dyn  = a.reg(9);               // boxed Float
        let r_acc  = a.reg(str_t);
        let r_lbl  = a.reg(str_t);
        let r_ret2 = a.reg(9);               // call-result throwaway
        // out = socket.output
        a.op(Opcode::Field { dst: r_out2, obj: r_sock2, field: RefField(out_field) });
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, nomatch);
        // header "TREE:\n"
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_hdr_g) });
        a.op(Opcode::Null { dst: r_lbl });
        a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
        // Sweep the specific collections AND the gameObjects union. gameObjects alone isn't the
        // union of the specific ones, and the specific ones don't cover vfx, so both are needed;
        // entries that appear twice are labelled by their group header.
        // UNROLLED over fixed slot indices: a bytecode loop whose body makes resolve/alloc calls
        // corrupts its counter at runtime (the looped version dropped characters[1] and over-ran
        // customGameObjects[2], killing the handler on an out-of-bounds GetArray) — the same
        // phenomenon the multiplayer extra-builds hit; fixed indices + forward jumps sidestep it.
        for (grp, (fld, slots)) in [
            (characters_field, 8usize), (cgo_field, 8), (proj_field, 8),
            (structures_field, 16), (gameobjects_field, 24),
        ].into_iter().enumerate() {
            // group header: "  [<name>] n=<true length>\n"
            {
                let skip_hdr = a.label();
                a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
                a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
                a.jnull(r_cm, skip_hdr);
                a.op(Opcode::Field { dst: r_go, obj: r_cm, field: RefField(fld) });
                a.jnull(r_go, skip_hdr);
                a.op(Opcode::Field { dst: r_len, obj: r_go, field: RefField(0) });
                a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_grp_g[grp]) });
                a.op(Opcode::ToDyn { dst: r_dyn, src: r_len });
                a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_nl_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                a.op(Opcode::Null { dst: r_lbl });
                a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
                a.place(skip_hdr);
            }
            for &slot_idx in &tree_slot_idx[..slots] {
                let skip = a.label();
                // re-derive the collection chain fresh per slot (nothing persists across calls).
                a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
                a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
                a.jnull(r_cm, skip);
                a.op(Opcode::Field { dst: r_go, obj: r_cm, field: RefField(fld) });
                a.jnull(r_go, skip);
                a.op(Opcode::Field { dst: r_len, obj: r_go, field: RefField(0) }); // length
                a.op(Opcode::Field { dst: r_arr, obj: r_go, field: RefField(1) }); // native array
                let in_range = a.label();
                a.op(Opcode::Int { dst: r_i, ptr: RefInt(slot_idx) });
                a.jslt(r_i, r_len, in_range);                              // slot < len -> read it
                a.jalways(skip);
                a.place(in_range);
                a.op(Opcode::GetArray { dst: r_el, array: r_arr, index: r_i });
                a.jnull(r_el, skip);                                       // empty slot -> next
                // class name = Type.getClassName(Type.getClass(el))
                a.op(Opcode::Call1 { dst: r_cls, fun: RefFun(type_getclass), arg0: r_el });
                a.op(Opcode::Call1 { dst: r_str, fun: RefFun(type_getclassname), arg0: r_cls });
                // acc = "  " + class + " @("
                a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_indent_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_at_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                // x = el.getX()
                // SafeCast, not UnsafeCast: a tickable's `value` is Dynamic, and reinterpreting
                // that as an object pointer reads garbage (it produced 3.1e-314 / NaN). The
                // class-name guard above has already established this is a Vfx, so the checked
                // cast succeeds.
                a.op(Opcode::SafeCast { dst: r_ent, src: r_el });
                a.op(Opcode::Call1 { dst: r_f, fun: RefFun(entity_getx), arg0: r_ent });
                a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
                a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_comma_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                // y = el.getY() (el survives the calls; re-cast for the typed call)
                a.op(Opcode::SafeCast { dst: r_ent, src: r_el });
                a.op(Opcode::Call1 { dst: r_f, fun: RefFun(entity_gety), arg0: r_ent });
                a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
                a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_close_g) }); // ")\n"
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                a.op(Opcode::Null { dst: r_lbl });
                a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
                a.place(skip);
            }
        }
        // ---- the stage's display layers (the FM counterpart of the SSF2 display list) ----
        // One line per named layer with its child count, so a converted backdrop element that
        // never got created, or landed in the wrong layer, is visible as a count mismatch.
        {
            let r_st   = a.reg(stage_entity_t);
            let r_api  = a.reg(stage_api_t);
            let r_capi = a.reg(container_api_t);
            let r_cont = a.reg(disp_container_t);
            let r_h2d  = a.reg(h2d_object_t);
            let r_kids = a.reg(38);              // ArrayObj
            let r_bool = a.reg(7);               // visible flag
            let r_bounds = a.reg(bounds_t);
            let r_f2 = a.reg(6);
            let r_void = a.reg(0);
            for (li, getter) in layer_getters.iter().enumerate() {
                let skip = a.label();
                a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
                a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
                a.jnull(r_cm, skip);
                a.op(Opcode::Field { dst: r_st, obj: r_cm, field: RefField(stage_field) });
                a.jnull(r_st, skip);
                // a fresh api handle per layer — nothing persists across the calls in between
                a.op(Opcode::New { dst: r_api });
                a.op(Opcode::Call2 { dst: r_void, fun: RefFun(stage_api_ctor), arg0: r_api, arg1: r_st });
                a.op(Opcode::Call1 { dst: r_capi, fun: RefFun(*getter), arg0: r_api });
                a.jnull(r_capi, skip);
                a.op(Opcode::Field { dst: r_cont, obj: r_capi, field: RefField(capi_container_field) });
                a.jnull(r_cont, skip);
                a.op(Opcode::Field { dst: r_h2d, obj: r_cont, field: RefField(cont_display_field) });
                a.jnull(r_h2d, skip);
                a.op(Opcode::Field { dst: r_kids, obj: r_h2d, field: RefField(h2d_children_field) });
                a.jnull(r_kids, skip);
                a.op(Opcode::Field { dst: r_len, obj: r_kids, field: RefField(0) });   // children.length
                a.op(Opcode::Field { dst: r_arr, obj: r_kids, field: RefField(1) });   // children.array
                a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_layer_g[li]) });
                a.op(Opcode::ToDyn { dst: r_dyn, src: r_len });
                a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_nl_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                a.op(Opcode::Null { dst: r_lbl });
                a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });

                // one row per child: "      <class> \"<name>\" @(x,y)[ HIDDEN]"
                for &kid_idx in &layer_child_idx {
                    let next_kid = a.label();
                    a.op(Opcode::Int { dst: r_i, ptr: RefInt(kid_idx) });
                    a.jslt(r_i, r_len, next_kid);          // slot < len -> read it
                    a.jalways(skip);                       // past the end: whole layer done
                    a.place(next_kid);
                    a.op(Opcode::GetArray { dst: r_el, array: r_arr, index: r_i });
                    let after = a.label();
                    a.jnull(r_el, after);
                    a.op(Opcode::UnsafeCast { dst: r_h2d, src: r_el });
                    // class
                    a.op(Opcode::Call1 { dst: r_cls, fun: RefFun(type_getclass), arg0: r_el });
                    a.op(Opcode::Call1 { dst: r_str, fun: RefFun(type_getclassname), arg0: r_cls });
                    a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_kid_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    // name (may be null -> "-")
                    a.op(Opcode::Field { dst: r_str, obj: r_h2d, field: RefField(h2d_name_field) });
                    let have_name = a.label();
                    a.jnotnull(r_str, have_name);
                    a.op(Opcode::GetGlobal { dst: r_str, global: RefGlobal(tree_unnamed_g) });
                    a.place(have_name);
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    // x, y as fields
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    a.op(Opcode::Field { dst: r_f, obj: r_h2d, field: RefField(h2d_x_field) });
                    a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
                    a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    a.op(Opcode::Field { dst: r_f, obj: r_h2d, field: RefField(h2d_y_field) });
                    a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
                    a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    // an invisible object is a real porting outcome, so say so
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    a.op(Opcode::Field { dst: r_bool, obj: r_h2d, field: RefField(h2d_vis_field) });
                    let vis_done = a.label();
                    let vis_set = a.label();
                    a.jtrue(r_bool, vis_set);
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_hidden_g) });
                    a.jalways(vis_done);
                    a.place(vis_set);
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_visible_g) });
                    a.place(vis_done);
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    // No animation clock on a display child: it's a transform wrapper. The
                    // per-element clock comes from the tick-stack walk below, which reaches
                    // the Vfx entities themselves.
                    // frame/total/label are empty for a display child (a transform wrapper
                    // has no clock); the SIZE slots carry absX/absY instead, which is the one
                    // thing a display child knows that the entity doesn't: where it actually
                    // renders. The LAYER is stamped last. Field positions stay identical to a
                    // tickable row so ONE host parser reads both.
                    for _ in 0..3 {
                        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    }
                    // pixel size, from the display object's own bounds
                    a.op(Opcode::Null { dst: r_bounds });
                    a.op(Opcode::Call2 { dst: r_bounds, fun: RefFun(h2d_getsize), arg0: r_h2d, arg1: r_bounds });
                    for (lo, hi) in [(b_xmin, b_xmax), (b_ymin, b_ymax)] {
                        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                        let bskip = a.label();
                        a.jnull(r_bounds, bskip);
                        a.op(Opcode::Field { dst: r_f, obj: r_bounds, field: RefField(hi) });
                        a.op(Opcode::Field { dst: r_f2, obj: r_bounds, field: RefField(lo) });
                        a.op(Opcode::Sub { dst: r_f, a: r_f, b: r_f2 });
                        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
                        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                        a.place(bskip);
                    }
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_layername_g[li]) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    // absolute render position, its own pair of fields
                    for fld in [disp_absx_field, disp_absy_field] {
                        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                        a.op(Opcode::Field { dst: r_f, obj: r_h2d, field: RefField(fld) });
                        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
                        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
                        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    }
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_nl_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    a.op(Opcode::Null { dst: r_lbl });
                    a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
                    a.place(after);
                }
                a.place(skip);
            }
        }
        // ---- the tick stack: where the Vfx actually live ----
        {
            let r_list = a.reg(dllist_t);
            let r_node = a.reg(dlnode_t);
            let r_dynb = a.reg(9);   // body, as Dynamic
            let r_dyna = a.reg(9);   // animation component, as Dynamic
            let r_dynv = a.reg(9);   // a field value, as Dynamic
            let done = a.label();

            a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
            a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
            a.jnull(r_cm, done);
            a.op(Opcode::Field { dst: r_ts, obj: r_cm, field: RefField(tickstack_field) });
            a.jnull(r_ts, done);
            a.op(Opcode::Field { dst: r_list, obj: r_ts, field: RefField(tickables_field) });
            a.jnull(r_list, done);

            // header with the TRUE length
            a.op(Opcode::Field { dst: r_len, obj: r_list, field: RefField(dl_len_field) });
            a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_tick_g) });
            a.op(Opcode::ToDyn { dst: r_dyn, src: r_len });
            a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
            a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
            a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_nl_g) });
            a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
            a.op(Opcode::Null { dst: r_lbl });
            a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });

            // UNROLLED hops. A bytecode loop whose body makes resolve/alloc calls corrupts its
            // own counter here (same failure the collection sweep and the multiplayer builds
            // hit), so this walks fixed hops with forward jumps only.
            a.op(Opcode::Field { dst: r_node, obj: r_list, field: RefField(dl_first_field) });
            for _ in 0..TICK_HOPS {
                a.jnull(r_node, done);
                a.op(Opcode::Field { dst: r_el, obj: r_node, field: RefField(dl_value_field) });
                let step = a.label();
                a.jnull(r_el, step);
                a.op(Opcode::Call1 { dst: r_cls, fun: RefFun(type_getclass), arg0: r_el });
                a.op(Opcode::Call1 { dst: r_str, fun: RefFun(type_getclassname), arg0: r_cls });
                a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_kid_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                // name field: unused for tickables, kept so the row shape matches the display
                // walk and both parse with the same host-side reader.
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                // x, y via body (null-safe: a non-entity simply yields empty fields)
                a.op(Opcode::DynGet { dst: r_dynb, obj: r_el, field: RefString(f_body) });
                for fname in [f_x, f_y] {
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    let bskip = a.label();
                    a.jnull(r_dynb, bskip);
                    a.op(Opcode::DynGet { dst: r_dynv, obj: r_dynb, field: RefString(fname) });
                    a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dynv });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    a.place(bskip);
                }
                // visible: not tracked per tickable; emit the field so the shape holds
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_visible_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                // the animation clock — frame, total, label
                a.op(Opcode::DynGet { dst: r_dyna, obj: r_el, field: RefString(f_animation) });
                for fname in [f_currentframe, f_totalframes, f_currentanim] {
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    let askip = a.label();
                    a.jnull(r_dyna, askip);
                    a.op(Opcode::DynGet { dst: r_dynv, obj: r_dyna, field: RefString(fname) });
                    a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dynv });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    a.place(askip);
                }
                // pixel size, from the dimensions component (null-safe, like the rest)
                a.op(Opcode::DynGet { dst: r_dynb, obj: r_el, field: RefString(f_dimensions) });
                for fname in [f_width, f_height] {
                    a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_space_g) });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                    let dskip = a.label();
                    a.jnull(r_dynb, dskip);
                    a.op(Opcode::DynGet { dst: r_dynv, obj: r_dynb, field: RefString(fname) });
                    a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dynv });
                    a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
                    a.place(dskip);
                }
                // No layer field here: the engine leaves its layer containers UNNAMED, so
                // walking Vfx -> view -> rootContainer -> parent -> name comes back empty.
                // The layer is known at patch time by the display walk instead (it visits one
                // container at a time), so it's stamped on those rows and joined to this
                // clock by position.
                a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(tree_nl_g) });
                a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
                a.op(Opcode::Null { dst: r_lbl });
                a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
                a.place(step);
                a.op(Opcode::Field { dst: r_node, obj: r_node, field: RefField(dl_next_field) });
            }
            a.place(done);
        }

        // re-fetch the output (r_out2 may be stale after the writes) and flush once.
        a.op(Opcode::Field { dst: r_out2, obj: r_sock2, field: RefField(out_field) });
        a.op(Opcode::Call1 { dst: r_ret2, fun: RefFun(flush), arg0: r_out2 });
        a.jalways(end);
        a.place(nomatch);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(tree_nomatch_g) });
        a.op(Opcode::Null { dst: r_lbl });
        a.op(Opcode::Call3 { dst: r_ret2, fun: RefFun(write_str), arg0: r_out2, arg1: r_acc, arg2: r_lbl });
        a.op(Opcode::Call1 { dst: r_ret2, fun: RefFun(flush), arg0: r_out2 });
        a.place(end);
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    // w handler done; fall through to the 'l' check.

    // ---- 'l' command: synchronous custom-.fra load (sandbag), bypassing the worker thread ----
    // Build a PXF Resource for custom/sandbag/sandbag.fra and call fetchThreaded directly
    // (sync read+decode+set_DataAsPxf), then finishLoading + addResource. Ack "L:<fqid>" if
    // getPXFResource then finds it, else "L:FAIL".
    let idx_l_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(l_idx) });
    let idx_jne_l = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'l' -> m-check
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    // r71 = new Resource("sandbag", <abs .fra path>, null)
    ops.push(Opcode::New { dst: rr(71) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_id_g) });
    ops.push(Opcode::GetGlobal { dst: rr(56), global: RefGlobal(sandbag_path_g) });
    ops.push(Opcode::Null { dst: rr(73) });                              // null enc (t241)
    ops.push(Opcode::Call4 { dst: r_ret, fun: RefFun(resource_ctor), arg0: rr(71), arg1: rr(55), arg2: rr(56), arg3: rr(73) });
    // force _isAbsolute = true (use _filePath verbatim) and _type = ResourceType.PXF
    ops.push(Opcode::Bool { dst: rr(64), value: ValBool(true) });
    ops.push(Opcode::SetField { obj: rr(71), field: RefField(res_isabs_field), src: rr(64) });
    ops.push(Opcode::GetGlobal { dst: rr(72), global: RefGlobal(rt_global) });
    ops.push(Opcode::Field { dst: rr(16), obj: rr(72), field: RefField(pxf_field) }); // PXF (Int)
    ops.push(Opcode::SetField { obj: rr(71), field: RefField(res_type_field), src: rr(16) });
    let _ = res_filepath_field; // (ctor already set _filePath from arg2)
    // set RM.requiredMediaIds = ["*"] so loadMedia's preload closure (run by finishLoading)
    // actually preloads our entities into _data.entityMap (empty headless otherwise -> SPR:0).
    ops.push(Opcode::Type { dst: rr(31), ty: RT(13) });                  // String element type
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(one_idx) });
    ops.push(Opcode::Call2 { dst: rr(32), fun: RefFun(256), arg0: rr(31), arg1: rr(39) }); // alloc_array<String>(1)
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(star_g) });
    ops.push(Opcode::SetArray { array: rr(32), index: rr(39), src: rr(53) });
    ops.push(Opcode::Call1 { dst: rr(33), fun: RefFun(257), arg0: rr(32) }); // wrap -> ArrayObj
    ops.push(Opcode::GetGlobal { dst: rr(65), global: RefGlobal(rm_g) });    // RM statics
    ops.push(Opcode::SetField { obj: rr(65), field: RefField(reqmedia_field), src: rr(33) });
    // synchronous read+decode (main thread): fetchThreaded -> File.getBytes -> createFromBytes -> set_DataAsPxf
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(fetch_threaded), arg0: rr(71) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(finish_loading), arg0: rr(71) }); // runs preload closure
    // Deterministically build every sprite entity into PXFResource.entityMap (the flaky
    // requiredMediaIds=["*"] preload sometimes leaves it empty -> SPR:0 -> crash). Loop
    // cacheSpriteEntityData(pxf, idx) over entities[0..len]; each call sets entityMap[entity.#2].
    ops.push(Opcode::Call1 { dst: rr(60), fun: RefFun(get_data_as_pxf), arg0: rr(71) }); // PXFResource
    ops.push(Opcode::Field { dst: rr(66), obj: rr(60), field: RefField(pxf_entities_field) }); // entities (ArrayObj)
    ops.push(Opcode::Field { dst: rr(39), obj: rr(66), field: RefField(0) }); // .length (Int)
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(zero_idx) });            // idx = 0
    let idx_l_bld_loop = ops.len();
    let idx_l_bld_jge = ops.len();
    ops.push(Opcode::JSGte { a: rr(16), b: rr(39), offset: 0 });             // idx >= len -> build_done
    ops.push(Opcode::Call2 { dst: r_ret, fun: RefFun(cache_sprite_entity_data), arg0: rr(60), arg1: rr(16) });
    ops.push(Opcode::Incr { dst: rr(16) });
    let idx_l_bld_jback = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                                 // -> build_loop
    let idx_l_bld_done = ops.len();
    // entity = entityMap.get("sandbag")  (built above; "sandbag" = the main sprite's id)
    ops.push(Opcode::Field { dst: rr(63), obj: rr(60), field: RefField(pxf_entitymap_field) }); // entityMap
    let idx_l_emap_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(63), offset: 0 });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_id_g) });
    ops.push(Opcode::Call2 { dst: rr(28), fun: RefFun(sm_get), arg0: rr(63), arg1: rr(55) }); // entity (dyn)
    let idx_l_ent_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(28), offset: 0 });
    ops.push(Opcode::UnsafeCast { dst: rr(74), src: rr(28) });           // -> PXFSpriteEntity (t746)
    ops.push(Opcode::Null { dst: rr(75) });                              // 3rd arg (t108)
    // re-cache under all 3 candidate spriteContent key formats (the buried-vfx uses the namespaced
    // "private::sandbag.sandbag"; the other two are harmless); cacheSpriteEntity just sets a map entry.
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_id_g) });    // "sandbag"
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: rr(55), arg1: rr(74), arg2: rr(75) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_pkgid_g) }); // "sandbag.sandbag"
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: rr(55), arg1: rr(74), arg2: rr(75) });
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(ns_sandbag_g) });    // "private::sandbag.sandbag"
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(cache_sprite_entity), arg0: rr(55), arg1: rr(74), arg2: rr(75) });
    let idx_l_skip_recache = ops.len();
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(add_resource), arg0: rr(71) });
    // verify: getPXFResource(getFullyQualifiedResourceId(res)) non-null?
    ops.push(Opcode::Call1 { dst: rr(57), fun: RefFun(getfqid_fn), arg0: rr(71) }); // fqid string
    ops.push(Opcode::Call1 { dst: rr(60), fun: RefFun(getpxf_fn), arg0: rr(57) });  // PXFResource | null
    let idx_l_jfail = ops.len();
    ops.push(Opcode::JNull { reg: rr(60), offset: 0 });                  // null -> L:FAIL
    // L:OK -> "L:" + fqid + (" CMAP:OK" | " CMAP:NULL") + already-newlined status
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(l_prefix_g) });
    ops.push(Opcode::Call2 { dst: rr(58), fun: RefFun(str_add), arg0: rr(53), arg1: rr(57) });
    // diagnostic: is the loaded PXFResource's characterPxfContentMap populated?
    ops.push(Opcode::Field { dst: rr(63), obj: rr(60), field: RefField(char_cmap_field) });
    let idx_l_cmap_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(63), offset: 0 });                  // null cmap -> CMAP:NULL
    // probe the actual content key: exists(cmap,"sandbag")? then exists(cmap,"sandbag.sandbag")?
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_id_g) });
    ops.push(Opcode::Call2 { dst: rr(64), fun: RefFun(sm_exists), arg0: rr(63), arg1: rr(55) });
    let idx_l_probe1_jfalse = ops.len();
    ops.push(Opcode::JFalse { cond: rr(64), offset: 0 });                // not "sandbag" -> probe2
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(key_sb_g) });
    let idx_l_probe1_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_l_probe2 = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_pkgid_g) });
    ops.push(Opcode::Call2 { dst: rr(64), fun: RefFun(sm_exists), arg0: rr(63), arg1: rr(55) });
    let idx_l_probe2_jfalse = ops.len();
    ops.push(Opcode::JFalse { cond: rr(64), offset: 0 });                // not "sandbag.sandbag" -> unknown
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(key_sbsb_g) });
    let idx_l_probe2_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_l_probe_unknown = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(key_unknown_g) });
    let idx_l_probe_unknown_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_l_cmap_null = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(l_cmapnull_g) });
    let idx_l_cmap_done = ops.len();
    ops.push(Opcode::Call2 { dst: rr(58), fun: RefFun(str_add), arg0: rr(58), arg1: rr(53) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(58), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // probe: did the media-preload populate pxfSpriteEntityCache for "sandbag"? (the
    // buried-character Vfx in Character ctor null-crashes when getSprite(id) is null)
    ops.push(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(sandbag_id_g) });
    ops.push(Opcode::Call1 { dst: rr(28), fun: RefFun(get_sprite_entity), arg0: rr(55) });
    let idx_l_spr_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(28), offset: 0 });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(spr_ok_g) });
    let idx_l_spr_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });
    let idx_l_spr_null = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(spr_null_g) });
    let idx_l_spr_done = ops.len();
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // NSPR probe (built via the Asm label helper — first adoption): is the namespaced
    // buried-VFX key cached after load? Self-contained block, reuses scratch regs, no
    // new registers; all jumps internal so it splices into the op stream cleanly.
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let l_null = a.label();
        let l_done = a.label();
        a.op(Opcode::GetGlobal { dst: rr(55), global: RefGlobal(ns_sandbag_g) });
        a.op(Opcode::Call1 { dst: rr(28), fun: RefFun(get_sprite_entity), arg0: rr(55) });
        a.jnull(rr(28), l_null);
        a.op(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(nspr_ok_g) });
        a.jalways(l_done);
        a.place(l_null);
        a.op(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(nspr_null_g) });
        a.place(l_done);
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
        a.op(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs); // no reg() calls -> empty, no-op (keeps the pattern explicit)
        ops.extend(a_ops);
    }
    let idx_l_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                             // -> m-check (continue chain)
    let idx_l_fail = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(l_fail_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // (l_fail falls through into the 'm' check, which rejects 'l')

    // ---- 'm' command: drive a move BY NAME on the live player-0 Character ----
    // Wire form: `m <letter>` where letter = 'A' + ordinal (ordinal indexes
    // interpreter::MOVES; the bridge does the name→letter mapping). Bare `m` (no
    // arg) = JAB. We walk MatchController.currentMatch -> Match.characters[0],
    // drain the rest of the line to read the selector byte, pick the CState via a
    // GENERATED jump table (one JEq arm per move, built from move_fields above),
    // and call the Character's own state machine toState(char, CState.X, null) —
    // internal dispatch, NOT key-press. Reports M:OK / M:NOMATCH.
    let idx_m_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(m_idx) });
    let idx_jne_m = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'm' -> t-check
    {
        // Self-contained Asm block: all branches are internal labels (Asm resolves
        // every offset at build time, so a mis-jump is a compile error, not an
        // engine crash). On completion it falls off its end into the 't' check,
        // which rejects the still-'m' command byte and routes to L_ORIG.
        let mut a = Asm::new(f.regs.len() as u32);
        let nomatch = a.label();
        let dispatch = a.label();
        let drain_loop = a.label();
        let drain_done = a.label();
        let after = a.label();
        let r_mc    = a.reg(mc_statics_t);
        let r_cm    = a.reg(match_t);
        let r_chars = a.reg(38);             // ArrayObj
        let r_len   = a.reg(3);
        let r_one   = a.reg(3);
        let r_z     = a.reg(3);
        let r_arr   = a.reg(11);             // NativeArray
        let r_elem  = a.reg(9);              // dyn
        let r_char  = a.reg(char_entity_t);
        let r_sel   = a.reg(3);              // selector byte (-1 = none)
        let r_chr   = a.reg(3);              // recv scratch
        let r_tmp   = a.reg(3);
        let r_cs    = a.reg(cstate_statics_t);
        let r_state = a.reg(3);
        // output = sock2.output
        a.op(Opcode::Field { dst: rr(5), obj: rr(10), field: RefField(out_field) });
        // walk to player-0 Character
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, nomatch);
        a.op(Opcode::Field { dst: r_chars, obj: r_cm, field: RefField(characters_field) });
        a.jnull(r_chars, nomatch);
        a.op(Opcode::Field { dst: r_len, obj: r_chars, field: RefField(0) }); // .length
        a.op(Opcode::Int { dst: r_one, ptr: RefInt(one_idx) });
        a.jslt(r_len, r_one, nomatch);       // length < 1 -> no player
        a.op(Opcode::Field { dst: r_arr, obj: r_chars, field: RefField(1) }); // .array
        a.op(Opcode::Int { dst: r_z, ptr: RefInt(zero_idx) });
        a.op(Opcode::GetArray { dst: r_elem, array: r_arr, index: r_z });
        a.jnull(r_elem, nomatch);
        a.op(Opcode::UnsafeCast { dst: r_char, src: r_elem });
        // drain the rest of the line, keeping the last non-space byte as the selector
        a.op(Opcode::Int { dst: r_sel, ptr: RefInt(negone_idx) });
        a.place(drain_loop);
        a.op(Opcode::Call1 { dst: r_chr, fun: RefFun(recv_char), arg0: rr(11) });
        a.op(Opcode::Int { dst: r_tmp, ptr: RefInt(zero_idx) });
        a.jslt(r_chr, r_tmp, drain_done);    // <0 (no more data) -> done
        a.op(Opcode::Int { dst: r_tmp, ptr: RefInt(nl_idx) });
        a.jeq(r_chr, r_tmp, drain_done);     // '\n' -> done
        a.op(Opcode::Int { dst: r_tmp, ptr: RefInt(space_idx) });
        a.jeq(r_chr, r_tmp, drain_loop);     // ' ' -> skip
        a.op(Opcode::Mov { dst: r_sel, src: r_chr }); // record selector
        a.jalways(drain_loop);
        a.place(drain_done);
        // default state = CState.JAB, then the generated selector chain may override
        a.op(Opcode::GetGlobal { dst: r_cs, global: RefGlobal(cstate_global) });
        a.op(Opcode::Field { dst: r_state, obj: r_cs, field: RefField(jab_field) });
        let set_labels: Vec<_> = move_fields.iter().map(|_| a.label()).collect();
        for ((letter_idx, _fld), &lbl) in move_fields.iter().zip(set_labels.iter()) {
            a.op(Opcode::Int { dst: r_tmp, ptr: RefInt(*letter_idx) });
            a.jeq(r_sel, r_tmp, lbl);        // selector == 'A'+i -> set move i
        }
        a.jalways(dispatch);                 // no match -> keep JAB default
        for ((_letter, fld), &lbl) in move_fields.iter().zip(set_labels.iter()) {
            a.place(lbl);
            a.op(Opcode::Field { dst: r_state, obj: r_cs, field: RefField(*fld) });
            a.jalways(dispatch);
        }
        a.place(dispatch);
        a.op(Opcode::Null { dst: rr(38) });  // null animName (String)
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(to_state), arg0: r_char, arg1: r_state, arg2: rr(38) });
        a.op(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(m_ok_g) });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: rr(14), arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.jalways(after);
        a.place(nomatch);
        a.op(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(m_nomatch_g) });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: rr(14), arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.place(after);
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    // (m handler falls through into the 't' check, which rejects 'm' -> L_ORIG)
    let _ = (m_ack_g, jab_field); // m_ack_g retained for ABI stability; jab_field used above

    // ---- 't' command: telemetry — report player-0 Character state name ----
    // currentMatch -> characters[0] -> getStateName(char). Reports T:<state> /
    // T:NOMATCH. Sampled across frames, this detects progress vs. a frozen state.
    let idx_t_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(t_idx) });
    let idx_jne_t = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 't' -> L_ORIG
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) });
    let idx_t_jnomatch = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });
    ops.push(Opcode::Field { dst: rr(66), obj: rr(44), field: RefField(characters_field) });
    let idx_t_jnochars = ops.len();
    ops.push(Opcode::JNull { reg: rr(66), offset: 0 });
    ops.push(Opcode::Field { dst: rr(16), obj: rr(66), field: RefField(0) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    let idx_t_jempty = ops.len();
    ops.push(Opcode::JSLte { a: rr(16), b: rr(39), offset: 0 });
    ops.push(Opcode::Field { dst: rr(67), obj: rr(66), field: RefField(1) });
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetArray { dst: rr(28), array: rr(67), index: rr(16) });
    let idx_t_jnull_elem = ops.len();
    ops.push(Opcode::JNull { reg: rr(28), offset: 0 });
    ops.push(Opcode::UnsafeCast { dst: rr(69), src: rr(28) });
    ops.push(Opcode::Call1 { dst: rr(57), fun: RefFun(get_state_name), arg0: rr(69) }); // String
    let idx_t_jnull_name = ops.len();
    ops.push(Opcode::JNull { reg: rr(57), offset: 0 });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(t_prefix_g) });
    ops.push(Opcode::Call2 { dst: rr(58), fun: RefFun(str_add), arg0: rr(53), arg1: rr(57) });
    ops.push(Opcode::GetGlobal { dst: rr(53), global: RefGlobal(nl_g) });
    ops.push(Opcode::Call2 { dst: rr(58), fun: RefFun(str_add), arg0: rr(58), arg1: rr(53) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(58), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_t_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                             // -> L_ORIG
    let idx_t_nomatch = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(t_nomatch_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    // t handler done; fall through to the 'v' (physics) check.

    // ---- 'v' command: physics/vitals readback ----
    let idx_v_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(v_idx) });
    let idx_jne_v = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'v' -> L_ORIG
    {
        // Self-contained Asm block: walk to player 0, format five Floats via
        // Std.string, write one "P: x=.. y=.. vx=.. vy=.. dmg=..\n" line.
        let mut a = Asm::new(f.regs.len() as u32);
        let nomatch = a.label();
        let after = a.label();
        let r_mc   = a.reg(mc_statics_t);
        let r_cm   = a.reg(match_t);
        let r_chs  = a.reg(38);
        let r_len  = a.reg(3);
        let r_one  = a.reg(3);
        let r_z    = a.reg(3);
        let r_arr  = a.reg(11);
        let r_el   = a.reg(9);
        let r_char = a.reg(char_entity_t);
        // One correctly-typed reg per component: HL computes a Field's byte offset from
        // the reg's STATIC type, so a Body-typed reg can't be reused to read Physics/Damage
        // fields (it read uninitialized memory → garbage velocities).
        let r_body = a.reg(body_t);
        let r_phys = a.reg(physics_t);
        let r_dmg  = a.reg(damage_t);
        let r_f    = a.reg(6);               // Float scratch
        let r_dyn  = a.reg(9);               // boxed Float
        let r_str  = a.reg(str_t);           // Std.string result
        let r_acc  = a.reg(str_t);           // accumulator
        let r_lbl  = a.reg(str_t);
        a.op(Opcode::Field { dst: rr(5), obj: rr(10), field: RefField(out_field) });
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, nomatch);
        a.op(Opcode::Field { dst: r_chs, obj: r_cm, field: RefField(characters_field) });
        a.jnull(r_chs, nomatch);
        a.op(Opcode::Field { dst: r_len, obj: r_chs, field: RefField(0) });
        a.op(Opcode::Int { dst: r_one, ptr: RefInt(one_idx) });
        a.jslt(r_len, r_one, nomatch);
        a.op(Opcode::Field { dst: r_arr, obj: r_chs, field: RefField(1) });
        a.op(Opcode::Int { dst: r_z, ptr: RefInt(zero_idx) });
        a.op(Opcode::GetArray { dst: r_el, array: r_arr, index: r_z });
        a.jnull(r_el, nomatch);
        a.op(Opcode::UnsafeCast { dst: r_char, src: r_el });
        // acc = "P:"
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(p_pre_g) });
        // helper closure: append `label` then Std.string(<comp>.field)
        // (emitted inline below per value; Rust can't borrow `a` in a closure cleanly here)
        // x = body.x
        a.op(Opcode::Field { dst: r_body, obj: r_char, field: RefField(char_body_f) });
        a.op(Opcode::Field { dst: r_f, obj: r_body, field: RefField(body_x_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(p_x_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        // y = body.y
        a.op(Opcode::Field { dst: r_f, obj: r_body, field: RefField(body_y_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(p_y_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        // vx = physics.currentVelocityX
        a.op(Opcode::Field { dst: r_phys, obj: r_char, field: RefField(char_physics_f) });
        a.op(Opcode::Field { dst: r_f, obj: r_phys, field: RefField(phys_vx_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(p_vx_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        // vy = physics.currentVelocityY
        a.op(Opcode::Field { dst: r_f, obj: r_phys, field: RefField(phys_vy_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(p_vy_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        // dmg = damage._damage
        a.op(Opcode::Field { dst: r_dmg, obj: r_char, field: RefField(char_damage_f) });
        a.op(Opcode::Field { dst: r_f, obj: r_dmg, field: RefField(dmg_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_f });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(p_dmg_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        // + "\n" and write
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(nl_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.jalways(after);
        a.place(nomatch);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(p_nomatch_g) });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.place(after);
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    // (v handler falls through to the 'a' (animation) check)

    // ---- 'a' command: animation introspection ----
    let idx_a_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(a_idx) });
    let idx_jne_a = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'a' -> L_ORIG
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let nomatch = a.label();
        let after = a.label();
        let r_mc   = a.reg(mc_statics_t);
        let r_cm   = a.reg(match_t);
        let r_chs  = a.reg(38);
        let r_len  = a.reg(3);
        let r_one  = a.reg(3);
        let r_z    = a.reg(3);
        let r_arr  = a.reg(11);
        let r_el   = a.reg(9);
        let r_char = a.reg(char_entity_t);
        let r_anim = a.reg(anim_t);
        let r_name = a.reg(str_t);
        let r_int  = a.reg(3);
        let r_dyn  = a.reg(9);
        let r_str  = a.reg(str_t);
        let r_acc  = a.reg(str_t);
        let r_lbl  = a.reg(str_t);
        a.op(Opcode::Field { dst: rr(5), obj: rr(10), field: RefField(out_field) });
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, nomatch);
        a.op(Opcode::Field { dst: r_chs, obj: r_cm, field: RefField(characters_field) });
        a.jnull(r_chs, nomatch);
        a.op(Opcode::Field { dst: r_len, obj: r_chs, field: RefField(0) });
        a.op(Opcode::Int { dst: r_one, ptr: RefInt(one_idx) });
        a.jslt(r_len, r_one, nomatch);
        a.op(Opcode::Field { dst: r_arr, obj: r_chs, field: RefField(1) });
        a.op(Opcode::Int { dst: r_z, ptr: RefInt(zero_idx) });
        a.op(Opcode::GetArray { dst: r_el, array: r_arr, index: r_z });
        a.jnull(r_el, nomatch);
        a.op(Opcode::UnsafeCast { dst: r_char, src: r_el });
        // anim = char.animation; name = anim.currentAnimation
        a.op(Opcode::Field { dst: r_anim, obj: r_char, field: RefField(char_anim_f) });
        a.jnull(r_anim, nomatch);
        a.op(Opcode::Field { dst: r_name, obj: r_anim, field: RefField(anim_name_f) });
        a.jnull(r_name, nomatch);
        // acc = "A:" + name + " frame " + str(currentFrame) + "/" + str(totalFrames) + "\n"
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(a_pre_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_name });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(a_frame_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Field { dst: r_int, obj: r_anim, field: RefField(anim_cur_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_int });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(a_slash_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Field { dst: r_int, obj: r_anim, field: RefField(anim_total_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_int });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(nl_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.jalways(after);
        a.place(nomatch);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(a_nomatch_g) });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.place(after);
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    // (a handler falls through to the 'f' (frame-step) check)

    // ---- 'f' command: animation frame-step (pause + advance one frame + report) ----
    let idx_f_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(f_idx) });
    let idx_jne_f = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'f' -> g-check
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let nomatch = a.label();
        let after = a.label();
        let r_mc = a.reg(mc_statics_t); let r_cm = a.reg(match_t); let r_chs = a.reg(38);
        let r_len = a.reg(3); let r_one = a.reg(3); let r_z = a.reg(3);
        let r_arr = a.reg(11); let r_el = a.reg(9); let r_char = a.reg(char_entity_t);
        let r_anim = a.reg(anim_t); let r_name = a.reg(str_t); let r_int = a.reg(3);
        let r_dyn = a.reg(9); let r_str = a.reg(str_t); let r_acc = a.reg(str_t); let r_lbl = a.reg(str_t);
        let r_true = a.reg(7); let r_rv = a.reg(0);
        a.op(Opcode::Field { dst: rr(5), obj: rr(10), field: RefField(out_field) });
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, nomatch);
        a.op(Opcode::Field { dst: r_chs, obj: r_cm, field: RefField(characters_field) });
        a.jnull(r_chs, nomatch);
        a.op(Opcode::Field { dst: r_len, obj: r_chs, field: RefField(0) });
        a.op(Opcode::Int { dst: r_one, ptr: RefInt(one_idx) });
        a.jslt(r_len, r_one, nomatch);
        a.op(Opcode::Field { dst: r_arr, obj: r_chs, field: RefField(1) });
        a.op(Opcode::Int { dst: r_z, ptr: RefInt(zero_idx) });
        a.op(Opcode::GetArray { dst: r_el, array: r_arr, index: r_z });
        a.jnull(r_el, nomatch);
        a.op(Opcode::UnsafeCast { dst: r_char, src: r_el });
        // pause playback, then advance one frame via playFrame(anim, currentFrame+1)
        a.op(Opcode::Bool { dst: r_true, value: ValBool(true) });
        a.op(Opcode::SetField { obj: r_char, field: RefField(pause_field), src: r_true });
        a.op(Opcode::Field { dst: r_anim, obj: r_char, field: RefField(char_anim_f) });
        a.jnull(r_anim, nomatch);
        a.op(Opcode::Field { dst: r_int, obj: r_anim, field: RefField(anim_cur_f) });
        a.op(Opcode::Incr { dst: r_int });
        a.op(Opcode::Call2 { dst: r_rv, fun: RefFun(play_frame), arg0: r_anim, arg1: r_int });
        // report A:<name> frame <cur>/<total> (re-read after the step)
        a.op(Opcode::Field { dst: r_name, obj: r_anim, field: RefField(anim_name_f) });
        a.jnull(r_name, nomatch);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(a_pre_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_name });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(a_frame_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Field { dst: r_int, obj: r_anim, field: RefField(anim_cur_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_int });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(a_slash_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Field { dst: r_int, obj: r_anim, field: RefField(anim_total_f) });
        a.op(Opcode::ToDyn { dst: r_dyn, src: r_int });
        a.op(Opcode::Call1 { dst: r_str, fun: RefFun(std_string), arg0: r_dyn });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_str });
        a.op(Opcode::GetGlobal { dst: r_lbl, global: RefGlobal(nl_g) });
        a.op(Opcode::Call2 { dst: r_acc, fun: RefFun(str_add), arg0: r_acc, arg1: r_lbl });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.jalways(after);
        a.place(nomatch);
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(a_nomatch_g) });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.place(after);
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    // ---- 'g' command: resume animation playback ----
    let idx_g_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(g_idx) });
    let idx_jne_g = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });            // not 'g' -> L_ORIG
    {
        let mut a = Asm::new(f.regs.len() as u32);
        let nomatch = a.label();
        let after = a.label();
        let r_mc = a.reg(mc_statics_t); let r_cm = a.reg(match_t); let r_chs = a.reg(38);
        let r_len = a.reg(3); let r_one = a.reg(3); let r_z = a.reg(3);
        let r_arr = a.reg(11); let r_el = a.reg(9); let r_char = a.reg(char_entity_t);
        let r_false = a.reg(7); let r_acc = a.reg(str_t);
        a.op(Opcode::Field { dst: rr(5), obj: rr(10), field: RefField(out_field) });
        a.op(Opcode::GetGlobal { dst: r_mc, global: RefGlobal(mc_g) });
        a.op(Opcode::Field { dst: r_cm, obj: r_mc, field: RefField(cm_field) });
        a.jnull(r_cm, nomatch);
        a.op(Opcode::Field { dst: r_chs, obj: r_cm, field: RefField(characters_field) });
        a.jnull(r_chs, nomatch);
        a.op(Opcode::Field { dst: r_len, obj: r_chs, field: RefField(0) });
        a.op(Opcode::Int { dst: r_one, ptr: RefInt(one_idx) });
        a.jslt(r_len, r_one, nomatch);
        a.op(Opcode::Field { dst: r_arr, obj: r_chs, field: RefField(1) });
        a.op(Opcode::Int { dst: r_z, ptr: RefInt(zero_idx) });
        a.op(Opcode::GetArray { dst: r_el, array: r_arr, index: r_z });
        a.jnull(r_el, nomatch);
        a.op(Opcode::UnsafeCast { dst: r_char, src: r_el });
        a.op(Opcode::Bool { dst: r_false, value: ValBool(false) });
        a.op(Opcode::SetField { obj: r_char, field: RefField(pause_field), src: r_false });
        a.place(nomatch);   // either way, just ack PLAY (resume is idempotent / no-match harmless)
        a.op(Opcode::GetGlobal { dst: r_acc, global: RefGlobal(g_ack_g) });
        a.op(Opcode::Null { dst: rr(15) });
        a.op(Opcode::Call3 { dst: rr(6), fun: RefFun(write_str), arg0: rr(5), arg1: r_acc, arg2: rr(15) });
        a.op(Opcode::Call1 { dst: rr(6), fun: RefFun(flush), arg0: rr(5) });
        a.place(after);
        let _ = after;
        let (a_ops, a_regs) = a.finish();
        add_regs(f, &a_regs);
        ops.extend(a_ops);
    }
    // (g handler falls through to the 'n' check)

    // ---- 'n' command: addCharacter (#3) — re-arm the deferred spawn from g_saved_parts so the
    // per-frame loop drops one more fighter (the last roster char) into the LIVE match on demand,
    // no relaunch. Reuses the proven deferred-spawn path; only re-arms the pending globals. ----
    let idx_n_check = ops.len();
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(n_idx) });
    let idx_jne_n = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });        // not 'n' -> 'e' check (patched below)
    ops.push(Opcode::GetGlobal { dst: rr(33), global: RefGlobal(g_saved_parts) });
    let idx_n_jnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(33), offset: 0 });               // no match started -> n_done
    ops.push(Opcode::Field { dst: rr(16), obj: rr(33), field: RefField(0) });         // parts.length
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(five_idx) });
    let idx_n_jslt = ops.len();
    ops.push(Opcode::JSLt { a: rr(16), b: rr(39), offset: 0 });       // length<5 (no extra char) -> n_done
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_parts), src: rr(33) }); // re-arm parts
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(one_idx) });
    ops.push(Opcode::Sub { dst: rr(16), a: rr(16), b: rr(39) });      // length - 1 = last extra index
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_pending_idx), src: rr(16) });    // re-arm idx -> spawns next frame
    let idx_n_done = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_n_jnull] { *offset = idx_n_done as i32 - idx_n_jnull as i32 - 1; }
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_n_jslt] { *offset = idx_n_done as i32 - idx_n_jslt as i32 - 1; }
    // (n handler falls through to the 'e' check, which rejects 'n')

    // ---- 'e' (eval) APPENDED at the end of the dispatch chain (after 'g'), so the
    // proven x->p->c->s->...->g chain is byte-identical to baseline. 'e' no-match -> L_ORIG. ----
    let idx_e_check = ops.len();
    // ---- 'e' (eval): parse + execute an hscript string, write "E:<result>\n". SPIKE:
    // a hardcoded script ("1 + 2") proves the in-engine hscript pipeline end-to-end;
    // the socket-driven arbitrary-script form follows once this is green. This single
    // hook is the foundation that replaces the per-command bytecode handlers. ----
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(e_idx) });
    let idx_jne_e = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });          // not 'e' -> L_AFTER_E ('x' check)
    let _ = eval_script_g;
    // ---- read the rest of the line ("e <script…>") into g_buf, then getString -> rr14 ----
    // (mirrors the proven `s`-handler drain: accumulate bytes until '\n' or EOF.)
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_blen), src: rr(39) });
    let idx_e_drain = ops.len();
    ops.push(Opcode::Call1 { dst: r_c, fun: RefFun(recv_char), arg0: r_handle });
    ops.push(Opcode::Int { dst: r_zero, ptr: RefInt(zero_idx) });
    let idx_e_jslt = ops.len();
    ops.push(Opcode::JSLt { a: r_c, b: r_zero, offset: 0 });            // no more data -> getString
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(nl_idx) });
    let idx_e_jeq = ops.len();
    ops.push(Opcode::JEq { a: r_c, b: rr(16), offset: 0 });             // '\n' -> getString
    ops.push(Opcode::GetGlobal { dst: rr(51), global: RefGlobal(g_buf) });
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_blen) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(bytes_set), arg0: rr(51), arg1: rr(39), arg2: r_c });
    ops.push(Opcode::GetGlobal { dst: rr(39), global: RefGlobal(g_blen) });
    ops.push(Opcode::Incr { dst: rr(39) });
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_blen), src: rr(39) });
    let idx_e_jback = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> drain
    let idx_e_getstr = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(51), global: RefGlobal(g_buf) });
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetGlobal { dst: rr(16), global: RefGlobal(g_blen) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call4 { dst: rr(14), fun: RefFun(bytes_getstring), arg0: rr(51), arg1: rr(39), arg2: rr(16), arg3: rr(15) });
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_e_jslt] { *offset = idx_e_getstr as i32 - idx_e_jslt as i32 - 1; }
    if let Opcode::JEq  { offset, .. } = &mut ops[idx_e_jeq]  { *offset = idx_e_getstr as i32 - idx_e_jeq as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_e_jback] { *offset = idx_e_drain as i32 - idx_e_jback as i32 - 1; }
    // Save the command line: rr14 is reused by commands.hsx load + p0/characters bindings
    // below, so we parse the command AFTER the interp is ready, from this saved copy.
    ops.push(Opcode::Mov { dst: rr(55), src: rr(14) });
    // ---- get-or-create the PERSISTENT top-scope interp, loaded with the engine's global
    // API via applyInterpreterGlobals (CState/HitboxStats/events/… — exactly how Main::init
    // readies every script). Created once, reused for every eval; this is the single
    // engine-linked interp all commands eventually move into as one hscript file. ----
    let _ = (hs_execute, eval_cs_g);
    ops.push(Opcode::GetGlobal { dst: e_interp, global: RefGlobal(g_interp) });
    let idx_e_haveinterp = ops.len();
    ops.push(Opcode::JNotNull { reg: e_interp, offset: 0 });            // already built -> reuse
    ops.push(Opcode::New { dst: e_interp });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(hs_interp_ctor), arg0: e_interp });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(hs_apply_globals), arg0: e_interp }); // load engine API
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_interp), src: e_interp });
    // load the hscript command script (commands.hsx) (the ported command implementations) into the interp, once.
    ops.push(Opcode::New { dst: e_parser });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(hs_parser_ctor), arg0: e_parser });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(commands_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: e_expr, fun: RefFun(hs_parse), arg0: e_parser, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call2 { dst: e_result, fun: RefFun(hs_interp_script), arg0: e_expr, arg1: e_interp });
    // bind __interp = the interp itself, __parser = this parser (both reused by __eval()).
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_interp_g) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: e_interp });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_parser_g) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: e_parser });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    // bind __td = Tildebugger statics (so commands.hsx's Engine.log can call __td.log).
    ops.push(Opcode::GetGlobal { dst: rr(23), global: RefGlobal(tilde_global) });
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(23) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_td_g) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    let idx_e_interp_ready = ops.len();
    if let Opcode::JNotNull { offset, .. } = &mut ops[idx_e_haveinterp] { *offset = idx_e_interp_ready as i32 - idx_e_haveinterp as i32 - 1; }
    // ---- bind p0 = MatchController.currentMatch.characters[0] (as Dynamic; null if no match) ----
    // so scripts can reach the live character: `p0.toState(...)`, `p0.body.x`, etc.
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) });   // currentMatch
    // NOTE: `match` is an hscript facade defined in commands.hsx (pxf.core.Match has no
    // RTTI so its fields/methods don't reflect); we bind the reliable `characters` array
    // below and the facade reads it. eval_match_g kept for reference.
    let _ = eval_match_g;
    let idx_e_p0null = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                                // no match -> p0 = null
    ops.push(Opcode::Field { dst: rr(33), obj: rr(44), field: RefField(characters_field) }); // ArrayObj
    // bind `characters` = the live character ArrayObj (reliable: field-index nav, hscript
    // handles Array natively — characters[0], characters.length, characters[0].getStateName()).
    ops.push(Opcode::ToDyn { dst: rr(28), src: rr(33) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_chars_g) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    let idx_e_chnull = ops.len();
    ops.push(Opcode::JNull { reg: rr(33), offset: 0 });
    ops.push(Opcode::Field { dst: rr(32), obj: rr(33), field: RefField(1) });          // .array (native)
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::GetArray { dst: rr(28), array: rr(32), index: rr(39) });          // characters[0] -> Dynamic
    let idx_e_p0done = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                                           // -> setVar
    let idx_e_bindnull = ops.len();
    ops.push(Opcode::Null { dst: rr(28) });
    let idx_e_setp0 = ops.len();
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_p0_g) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    if let Opcode::JNull { offset, .. } = &mut ops[idx_e_p0null] { *offset = idx_e_bindnull as i32 - idx_e_p0null as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_e_chnull] { *offset = idx_e_bindnull as i32 - idx_e_chnull as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_e_p0done] { *offset = idx_e_setp0 as i32 - idx_e_p0done as i32 - 1; }
    // bind p1, p2, p3 = the live match's extra players (slots 1..3). We read the real
    // currentMatch.characters ArrayObj LENGTH in bytecode (field 0 — works fine; only hscript
    // lacks RTTI on this ArrayObj) and bind p<slot> = characters[slot] when slot < length, else
    // null. getCharacters() in commands.hsx drops the nulls, so each p<slot> appears only once the
    // match actually has that many players. Each bound p<slot> has the full Character API.
    // currentMatch.characters -> rr33 (ArrayObj), length -> rr16, native array -> rr32. Defensive:
    // no match / no array -> all extras null.
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(zero_idx) });                      // default length 0
    ops.push(Opcode::Null { dst: rr(32) });                                            // default no native array
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) });   // currentMatch
    let idx_px_jnomatch = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                                // no match -> extras null
    ops.push(Opcode::Field { dst: rr(33), obj: rr(44), field: RefField(characters_field) }); // ArrayObj
    let idx_px_jnoarr = ops.len();
    ops.push(Opcode::JNull { reg: rr(33), offset: 0 });                                // no array -> extras null
    ops.push(Opcode::Field { dst: rr(16), obj: rr(33), field: RefField(0) });          // characters.length
    ops.push(Opcode::Field { dst: rr(32), obj: rr(33), field: RefField(1) });          // characters.array (native)
    let idx_px_ready = ops.len();
    if let Opcode::JNull { offset, .. } = &mut ops[idx_px_jnomatch] { *offset = idx_px_ready as i32 - idx_px_jnomatch as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_px_jnoarr]   { *offset = idx_px_ready as i32 - idx_px_jnoarr as i32 - 1; }
    // for slot in 1..=3: p<slot> = (slot < length && array != null) ? array[slot] : null
    for (slot_idx, name_g) in [(one_idx, eval_p1_g), (two_idx, eval_p2_g), (three_idx, eval_p3_g)] {
        ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(slot_idx) });                  // slot
        let idx_s_jnull = ops.len();
        ops.push(Opcode::JSGte { a: rr(39), b: rr(16), offset: 0 });                   // slot >= length -> null
        let idx_s_jnoarr = ops.len();
        ops.push(Opcode::JNull { reg: rr(32), offset: 0 });                            // no array -> null
        ops.push(Opcode::GetArray { dst: rr(28), array: rr(32), index: rr(39) });      // characters[slot]
        let idx_s_jset = ops.len();
        ops.push(Opcode::JAlways { offset: 0 });                                       // -> setVar
        let idx_s_null = ops.len();
        ops.push(Opcode::Null { dst: rr(28) });
        let idx_s_set = ops.len();
        if let Opcode::JSGte   { offset, .. } = &mut ops[idx_s_jnull]  { *offset = idx_s_null as i32 - idx_s_jnull as i32 - 1; }
        if let Opcode::JNull   { offset, .. } = &mut ops[idx_s_jnoarr] { *offset = idx_s_null as i32 - idx_s_jnoarr as i32 - 1; }
        if let Opcode::JAlways { offset, .. } = &mut ops[idx_s_jset]   { *offset = idx_s_set as i32 - idx_s_jset as i32 - 1; }
        ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(name_g) });
        ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    }
    // ---- bind stageEntity = the live stage entity (null if no match) ----
    // Deliberately NOT named `stage`: the script API's ambient `stage` is the StageApi
    // sandbox, a different object with a smaller surface, and calling one `stage` would
    // imply a parity with converted scripts that doesn't hold (see the note at stage_field).
    // This is the raw entity, for tooling that needs what's actually in the match rather
    // than what content is allowed to touch — placement diffing against SSF2's display list.
    ops.push(Opcode::Null { dst: rr(28) });                                            // default null
    ops.push(Opcode::GetGlobal { dst: rr(43), global: RefGlobal(mc_g) });
    ops.push(Opcode::Field { dst: rr(44), obj: rr(43), field: RefField(cm_field) });   // currentMatch
    let idx_st_jnomatch = ops.len();
    ops.push(Opcode::JNull { reg: rr(44), offset: 0 });                                // no match -> null
    ops.push(Opcode::Field { dst: stage_reg, obj: rr(44), field: RefField(stage_field) });
    let idx_st_jnostage = ops.len();
    ops.push(Opcode::JNull { reg: stage_reg, offset: 0 });                             // no stage -> null
    ops.push(Opcode::ToDyn { dst: rr(28), src: stage_reg });
    let idx_st_set = ops.len();
    for j in [idx_st_jnomatch, idx_st_jnostage] {
        if let Opcode::JNull { offset, .. } = &mut ops[j] { *offset = idx_st_set as i32 - j as i32 - 1; }
    }
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_stage_ent_g) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(hs_setvar), arg0: e_interp, arg1: rr(14), arg2: rr(28) });
    // ---- crash-proof eval: parse + run inside a Trap. On ANY error (parse OR runtime)
    // log to the engine (Sys.println -> engine log) AND return "ERR: <msg>" to Peptide.
    // Uses exprReturn (throws on error, unlike interpretScript which swallows) with the
    // same depth/declared reset, so the interpreter can never crash the game frame. ----
    let _ = (hs_interp_script, eval_interp_g, eval_parser_g, eval_cmd_g);
    let idx_eval_trap = ops.len();
    ops.push(Opcode::Trap { exc: e_result, offset: 0 });                 // exception -> L_CATCH (patched)
    ops.push(Opcode::New { dst: e_parser });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(hs_parser_ctor), arg0: e_parser });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: e_expr, fun: RefFun(hs_parse), arg0: e_parser, arg1: rr(55), arg2: rr(15) });
    // guard: a null expr would make exprReturn segfault (uncatchable) — skip it.
    let idx_eval_jnotnull = ops.len();
    ops.push(Opcode::JNotNull { reg: e_expr, offset: 0 });               // expr != null -> run it
    ops.push(Opcode::Null { dst: e_result });                            // null/empty -> result null
    let idx_eval_jrun_done = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                             // -> after run
    let idx_eval_run = ops.len();
    // reset interp (depth=0, declared=new ArrayObj) — same as ApiScript.interpretScript
    ops.push(Opcode::Int { dst: rr(39), ptr: RefInt(zero_idx) });
    ops.push(Opcode::SetField { obj: e_interp, field: RefField(3), src: rr(39) });   // .depth = 0
    ops.push(Opcode::New { dst: rr(33) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(hs_arr_ctor), arg0: rr(33) });
    ops.push(Opcode::SetField { obj: e_interp, field: RefField(5), src: rr(33) });   // .declared = new
    ops.push(Opcode::Call2 { dst: e_result, fun: RefFun(hs_expr_return), arg0: e_interp, arg1: e_expr });
    let idx_eval_after_run = ops.len();
    ops.push(Opcode::Call1 { dst: rr(53), fun: RefFun(std_string), arg0: e_result }); // rr53 = result string
    ops.push(Opcode::EndTrap { exc: e_result });                         // normal path: pop trap
    let idx_eval_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                             // -> L_WRITE (patched)
    let idx_eval_catch = ops.len();
    ops.push(Opcode::EndTrap { exc: e_result });                         // catch: pop trap (e_result = exception)
    ops.push(Opcode::Call1 { dst: rr(53), fun: RefFun(std_string), arg0: e_result }); // exception -> string
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(err_prefix_g) });
    ops.push(Opcode::Call2 { dst: rr(53), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) }); // "ERR: " + msg
    // engine-log it to the in-game console: Tildebugger.log("[peptide] eval error: "+msg, null, null)
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(err_log_prefix_g) });
    ops.push(Opcode::Call2 { dst: rr(56), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(tilde_log), arg0: rr(56), arg1: rr(15), arg2: rr(15) });
    let idx_eval_write = ops.len();
    // wire the internal jumps
    if let Opcode::JNotNull { offset, .. } = &mut ops[idx_eval_jnotnull] { *offset = idx_eval_run as i32 - idx_eval_jnotnull as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_eval_jrun_done] { *offset = idx_eval_after_run as i32 - idx_eval_jrun_done as i32 - 1; }
    if let Opcode::Trap { offset, .. } = &mut ops[idx_eval_trap] { *offset = idx_eval_catch as i32 - idx_eval_trap as i32; } // catch = trap + offset
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_eval_jdone] { *offset = idx_eval_write as i32 - idx_eval_jdone as i32 - 1; }
    // out = "E:" + rr53 + "\n"  (rr53 = result or "ERR: ..." string)
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_prefix_g) });
    ops.push(Opcode::Call2 { dst: rr(53), fun: RefFun(str_add), arg0: rr(14), arg1: rr(53) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(eval_nl_g) });
    ops.push(Opcode::Call2 { dst: rr(53), fun: RefFun(str_add), arg0: rr(53), arg1: rr(14) });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(53), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_e_jdone = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> L_ORIG (patched in the n-block)

    // ---- 'i' (input): override player held controls — drives the engine's OWN
    // input→action mapping, not a synthetic keypress. APPENDED at the END of the
    // dispatch chain (after 'e') per the JIT-fragility rule: the proven
    // x->p->c->s->...->g->e prefix (and its register indices) stays byte-identical,
    // so s/e don't mis-JIT. Parses a decimal button bitmask from the rest of the line
    // into g_inject_held; the Character.updateGameInput epilogue applies it every
    // frame. 'i' no-match -> L_ORIG; a handled 'i' also falls through to L_ORIG (its
    // byte + the rest of the line are consumed by the digit loop). ----
    let idx_i_check = ops.len();
    let i_b = add_regs(f, &[3, 3, 3]); // acc, digit, tmp (all Int)
    let (r_acc, r_dig, r_tmp) = (Reg(i_b), Reg(i_b + 1), Reg(i_b + 2));
    ops.push(Opcode::Int { dst: rr(16), ptr: RefInt(i_idx) });
    let idx_jne_i = ops.len();
    ops.push(Opcode::JNotEq { a: r_c, b: rr(16), offset: 0 });          // not 'i' -> L_ORIG
    ops.push(Opcode::Int { dst: r_acc, ptr: RefInt(zero_idx) });        // acc = 0
    let idx_i_loop = ops.len();
    ops.push(Opcode::Call1 { dst: r_c, fun: RefFun(recv_char), arg0: r_handle });
    ops.push(Opcode::Int { dst: r_tmp, ptr: RefInt(zero_idx) });
    let idx_i_jneg = ops.len();
    ops.push(Opcode::JSLt { a: r_c, b: r_tmp, offset: 0 });             // c<0 (EOF) -> done
    ops.push(Opcode::Int { dst: r_tmp, ptr: RefInt(nl_idx) });
    let idx_i_jnl = ops.len();
    ops.push(Opcode::JEq { a: r_c, b: r_tmp, offset: 0 });              // '\n' -> done
    ops.push(Opcode::Int { dst: r_tmp, ptr: RefInt(i_zero_char) });
    let idx_i_jlt0 = ops.len();
    ops.push(Opcode::JSLt { a: r_c, b: r_tmp, offset: 0 });             // c<'0' -> loop (skip non-digit)
    ops.push(Opcode::Int { dst: r_tmp, ptr: RefInt(i_nine_char) });
    let idx_i_jgt9 = ops.len();
    ops.push(Opcode::JSGt { a: r_c, b: r_tmp, offset: 0 });             // c>'9' -> loop (skip non-digit)
    // acc = acc*10 + (c - '0')
    ops.push(Opcode::Int { dst: r_tmp, ptr: RefInt(i_zero_char) });
    ops.push(Opcode::Sub { dst: r_dig, a: r_c, b: r_tmp });
    ops.push(Opcode::Int { dst: r_tmp, ptr: RefInt(i_ten) });
    ops.push(Opcode::Mul { dst: r_acc, a: r_acc, b: r_tmp });
    ops.push(Opcode::Add { dst: r_acc, a: r_acc, b: r_dig });
    let idx_i_jback = ops.len();
    ops.push(Opcode::JAlways { offset: 0 });                            // -> loop
    let idx_i_done = ops.len();
    ops.push(Opcode::SetGlobal { global: RefGlobal(g_inject_held), src: r_acc });
    ops.push(Opcode::Field { dst: r_out, obj: r_sock2, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: rr(14), global: RefGlobal(i_ok_g) });
    ops.push(Opcode::Null { dst: rr(15) });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: rr(14), arg2: rr(15) });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let idx_i_after = ops.len(); // = L_ORIG (n): the next op after this last handler
    if let Opcode::JNotEq  { offset, .. } = &mut ops[idx_jne_i]  { *offset = idx_i_after as i32 - idx_jne_i as i32 - 1; }
    if let Opcode::JSLt    { offset, .. } = &mut ops[idx_i_jneg] { *offset = idx_i_done as i32 - idx_i_jneg as i32 - 1; }
    if let Opcode::JEq     { offset, .. } = &mut ops[idx_i_jnl]  { *offset = idx_i_done as i32 - idx_i_jnl as i32 - 1; }
    if let Opcode::JSLt    { offset, .. } = &mut ops[idx_i_jlt0] { *offset = idx_i_loop as i32 - idx_i_jlt0 as i32 - 1; }
    if let Opcode::JSGt    { offset, .. } = &mut ops[idx_i_jgt9] { *offset = idx_i_loop as i32 - idx_i_jgt9 as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_i_jback]{ *offset = idx_i_loop as i32 - idx_i_jback as i32 - 1; }

    // L_ORIG = first original op after the prepended block (index n).
    let n = ops.len() as i32;
    if let Opcode::JTrue { offset, .. } = &mut ops[1] { *offset = idx_recv as i32 - 2; }
    if let Opcode::JFalse { offset, .. } = &mut ops[idx_jready] { *offset = n - idx_jready as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_jnull] { *offset = n - idx_jnull as i32 - 1; }
    if let Opcode::JSLt { offset, .. } = &mut ops[idx_jslt] { *offset = n - idx_jslt as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_e_jdone] { *offset = n - idx_e_jdone as i32 - 1; } // 'e' eval done -> L_ORIG
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_p] { *offset = idx_c_check as i32 - idx_jne_p as i32 - 1; }
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_c] { *offset = idx_s_check as i32 - idx_jne_c as i32 - 1; }
    // 's' falls through to the 'q' check; route 'not s' there too; 'q' routes to 'k'; 'k' to L_ORIG.
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_s] { *offset = idx_q_check as i32 - idx_jne_s as i32 - 1; }
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_q] { *offset = idx_k_check as i32 - idx_jne_q as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_q_jnull] { *offset = idx_q_nomatch as i32 - idx_q_jnull as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_q_jdone] { *offset = n - idx_q_jdone as i32 - 1; }
    // _matches diagnostic branch wiring: null/empty -> truly-none; non-empty path
    // falls through then JAlways -> L_ORIG.
    if let Opcode::JNull { offset, .. } = &mut ops[idx_q_jm_null] { *offset = idx_q_truly_none as i32 - idx_q_jm_null as i32 - 1; }
    if let Opcode::JSLte { offset, .. } = &mut ops[idx_q_jm_empty] { *offset = idx_q_truly_none as i32 - idx_q_jm_empty as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_q_jm_done] { *offset = n - idx_q_jm_done as i32 - 1; }
    // k-command jumps ('k' no-match now routes to the 'l' check, not L_ORIG)
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_k] { *offset = idx_y_check as i32 - idx_jne_k as i32 - 1; }
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_w] { *offset = idx_l_check as i32 - idx_jne_w as i32 - 1; }
    // l-command jumps ('l' no-match -> m-check; getPXFResource null -> L:FAIL; done -> m-check)
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_l] { *offset = idx_m_check as i32 - idx_jne_l as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_l_jfail] { *offset = idx_l_fail as i32 - idx_l_jfail as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_l_cmap_jnull] { *offset = idx_l_cmap_null as i32 - idx_l_cmap_jnull as i32 - 1; }
    if let Opcode::JFalse { offset, .. } = &mut ops[idx_l_probe1_jfalse] { *offset = idx_l_probe2 as i32 - idx_l_probe1_jfalse as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_l_probe1_jdone] { *offset = idx_l_cmap_done as i32 - idx_l_probe1_jdone as i32 - 1; }
    if let Opcode::JFalse { offset, .. } = &mut ops[idx_l_probe2_jfalse] { *offset = idx_l_probe_unknown as i32 - idx_l_probe2_jfalse as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_l_probe2_jdone] { *offset = idx_l_cmap_done as i32 - idx_l_probe2_jdone as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_l_probe_unknown_jdone] { *offset = idx_l_cmap_done as i32 - idx_l_probe_unknown_jdone as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_l_spr_jnull] { *offset = idx_l_spr_null as i32 - idx_l_spr_jnull as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_l_spr_jdone] { *offset = idx_l_spr_done as i32 - idx_l_spr_jdone as i32 - 1; }
    // entity-build loop jumps (idx>=len -> done; tail -> loop)
    if let Opcode::JSGte { offset, .. } = &mut ops[idx_l_bld_jge] { *offset = idx_l_bld_done as i32 - idx_l_bld_jge as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_l_bld_jback] { *offset = idx_l_bld_loop as i32 - idx_l_bld_jback as i32 - 1; }
    // bare sprite re-cache guards (null entityMap / missing entity -> skip)
    if let Opcode::JNull { offset, .. } = &mut ops[idx_l_emap_jnull] { *offset = idx_l_skip_recache as i32 - idx_l_emap_jnull as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_l_ent_jnull] { *offset = idx_l_skip_recache as i32 - idx_l_ent_jnull as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_l_jdone] { *offset = idx_m_check as i32 - idx_l_jdone as i32 - 1; }
    // k diagnostic (directoriesToLoad) jumps
    if let Opcode::JNull { offset, .. } = &mut ops[idx_k_dirs_null] { *offset = idx_k_after_dirs as i32 - idx_k_dirs_null as i32 - 1; }
    if let Opcode::JSLte { offset, .. } = &mut ops[idx_k_dirs_jle] { *offset = idx_k_dirs_zero as i32 - idx_k_dirs_jle as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_k_dirs_jdone] { *offset = idx_k_dirs_done as i32 - idx_k_dirs_jdone as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_k_jnull_pool] { *offset = idx_k_done as i32 - idx_k_jnull_pool as i32 - 1; }
    if let Opcode::JSGte { offset, .. } = &mut ops[idx_k_jge] { *offset = idx_k_done as i32 - idx_k_jge as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_k_jnull_elem] { *offset = idx_k_loop as i32 - idx_k_jnull_elem as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_k_jnull_fqid] { *offset = idx_k_loop as i32 - idx_k_jnull_fqid as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_k_jback] { *offset = idx_k_loop as i32 - idx_k_jback as i32 - 1; }
    // m-command outer jump only ('m' no-match -> t-check). The handler body is now a
    // self-contained Asm block whose internal branches are resolved by Asm::finish(),
    // so there are no m_* external fixups to patch here anymore.
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_m] { *offset = idx_t_check as i32 - idx_jne_m as i32 - 1; }
    // t-command jumps ('t' no-match -> L_ORIG; failures -> T:NOMATCH; success -> L_ORIG)
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_t] { *offset = idx_v_check as i32 - idx_jne_t as i32 - 1; }
    // 'v' (physics) -> 'a' (animation) -> L_ORIG. Both bodies are self-contained Asm blocks.
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_v] { *offset = idx_a_check as i32 - idx_jne_v as i32 - 1; }
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_a] { *offset = idx_f_check as i32 - idx_jne_a as i32 - 1; }
    // 'f' (frame-step) -> 'g' (resume) -> L_ORIG. Self-contained Asm blocks.
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_f] { *offset = idx_g_check as i32 - idx_jne_f as i32 - 1; }
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_g] { *offset = idx_n_check as i32 - idx_jne_g as i32 - 1; } // g no-match -> 'n' check
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_n] { *offset = idx_e_check as i32 - idx_jne_n as i32 - 1; } // n no-match -> 'e' check
    if let Opcode::JNotEq { offset, .. } = &mut ops[idx_jne_e] { *offset = idx_i_check as i32 - idx_jne_e as i32 - 1; } // 'e' no-match -> 'i' check
    if let Opcode::JNull { offset, .. } = &mut ops[idx_t_jnomatch] { *offset = idx_t_nomatch as i32 - idx_t_jnomatch as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_t_jnochars] { *offset = idx_t_nomatch as i32 - idx_t_jnochars as i32 - 1; }
    if let Opcode::JSLte { offset, .. } = &mut ops[idx_t_jempty] { *offset = idx_t_nomatch as i32 - idx_t_jempty as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_t_jnull_elem] { *offset = idx_t_nomatch as i32 - idx_t_jnull_elem as i32 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[idx_t_jnull_name] { *offset = idx_t_nomatch as i32 - idx_t_jnull_name as i32 - 1; }
    if let Opcode::JAlways { offset, .. } = &mut ops[idx_t_jdone] { *offset = n - idx_t_jdone as i32 - 1; }
    insert_ops_front(f, ops);
    eprintln!("connect_edit: injected {n} ops into update@{update_fx} (console+startMatch dispatch); port={port}");

    // ---- input-override epilogue: apply g_inject_held to live controls each frame ----
    // The 'i' command sets g_inject_held; this makes Character.updateGameInput consume it
    // (held |= mask, pressed |= rising-edge) right where the engine feeds m_heldControls /
    // m_pressedControls — the SAME objects the InputResolver reads to trigger moves.
    inject_input_override(code, g_inject_held)?;

    // ---- getPXFSpriteEntity self-heal (RELIABLE SPAWN) ------------------------
    // RE proof: there is NO cache reset/removal — the buried-VFX null is a populate-
    // vs-consume RACE (the async per-resource PXF preload caches the sprite key only
    // when sandbag's resource finishes loading; onMatchReady's ctor sometimes wins).
    // Our headless load reliably caches "private::sandbag.sandbag" (NSPR:1, never
    // removed). So: rewrite getPXFSpriteEntity@18289 to, on ANY cache miss, fall back
    // to that always-present entry — converting the race into a guaranteed hit for
    // sandbag's sprite. Reuses the function's own regs (0=key,1=val,2=map,3=RM,4=ret);
    // the original body becomes unreachable (we always Ret).
    {
        let mut a = Asm::new(0); // reuses the fn's own regs (0-4); allocates none
        let l_ret = a.label();
        a.op(Opcode::GetGlobal { dst: Reg(3), global: RefGlobal(rm_g) });
        a.op(Opcode::Field { dst: Reg(2), obj: Reg(3), field: RefField(spritecache_field) });
        a.op(Opcode::Call2 { dst: Reg(1), fun: RefFun(sm_get), arg0: Reg(2), arg1: Reg(0) });
        a.jnotnull(Reg(1), l_ret);                       // hit -> return it
        // miss: fall back to the loaded char's key (generic — set by the load; null if none)
        a.op(Opcode::GetGlobal { dst: Reg(0), global: RefGlobal(g_loaded_spritekey) });
        a.jnull(Reg(0), l_ret);                          // no loaded char -> return null (orig behavior)
        a.op(Opcode::Call2 { dst: Reg(1), fun: RefFun(sm_get), arg0: Reg(2), arg1: Reg(0) });
        a.place(l_ret);
        a.op(Opcode::UnsafeCast { dst: Reg(4), src: Reg(1) });
        a.op(Opcode::Ret { ret: Reg(4) });
        let (a_ops, a_regs) = a.finish();
        let gfi = function_index_by_findex(code, get_sprite_entity)
            .ok_or_else(|| anyhow::anyhow!("getPXFSpriteEntity@{get_sprite_entity} not found"))?;
        add_regs(&mut code.functions[gfi], &a_regs);
        insert_ops_front(&mut code.functions[gfi], a_ops);
        eprintln!("connect_edit: getPXFSpriteEntity@{get_sprite_entity} self-heal (miss -> private::sandbag.sandbag)");
    }

    // FAST BOOT / SKIP TITLE+MENU: no-op launchScreen@17771 (the LAST boot load-step).
    // It is the ONLY creator of the Title scene (showMenuById("Title")) AND the caller
    // of UgcUtil.loadUgc (the slow "load ALL custom content" scan). Replacing its body
    // with an immediate void Ret suppresses BOTH — no Title/MainMenu is ever built, and
    // no custom content is bulk-loaded. The match-ready signal (READY + g_ready) is now
    // emitted from the connect block on the first update() after CoreEngine.preLoad
    // (which created the game/menu containers + MatchController + set updateLoopReady),
    // so `s` dispatches without ever reaching a menu. startMatch reuses the existing
    // gameContainer (created in configLoaded, before preLoad).
    // Resolve the boot fns by name (pinned findices drift every recompile). NOTE:
    // launchScreen is a STATIC method, so its parent is the `$`-companion type
    // `fraymakers.$Main`, not the instance type `fraymakers.Main`. onLoaded is an
    // instance method on `fraymakers.Main`.
    let launchscreen_fx = require_fn(code, "launchScreen", Some("fraymakers.$Main"))?;
    let onloaded_fx = require_fn(code, "onLoaded", Some("fraymakers.Main"))?;
    // Guard: confirm we resolved the RIGHT launchScreen by checking it actually
    // calls UgcUtil::loadUgc (its defining behaviour — "show Title + bulk-load UGC").
    // A rename/refactor that moved that logic elsewhere fails loudly here.
    let loadugc_fx = require_fn(code, "loadUgc", Some("fraymakers.util.$UgcUtil"))?;
    {
        let lf = function_index_by_findex(code, launchscreen_fx)
            .ok_or_else(|| anyhow::anyhow!("launchScreen fn not found"))?;
        let calls_loadugc = code.functions[lf].ops.iter().any(|op| matches!(op,
            Opcode::Call0 { fun, .. } | Opcode::Call1 { fun, .. } if fun.0 == loadugc_fx));
        if !calls_loadugc {
            anyhow::bail!("launchScreen@{launchscreen_fx} does not call UgcUtil::loadUgc — \
                           engine boot flow changed; re-verify the fast-boot no-op target");
        }
    }
    eprintln!("boot fns (by name): launchScreen={launchscreen_fx} onLoaded={onloaded_fx}");
    if headless {
        let lfi = function_index_by_findex(code, launchscreen_fx)
            .ok_or_else(|| anyhow::anyhow!("launchScreen fn not found"))?;
        let lf = &mut code.functions[lfi];
        let vreg = add_regs(lf, &[0]); // void-typed scratch reg for the early Ret
        insert_ops_front(lf, vec![Opcode::Ret { ret: Reg(vreg) }]);
        eprintln!("connect_edit: [headless] no-op'd launchScreen (skip Title + loadUgc)");
    }
    // Signal READY + set g_ready. The hook point differs by mode so the harness's
    // "wait for READY, then send commands" handshake fires at the right moment:
    //   - headless: Main.onLoaded@17746 (onComplete of the second boot-load; the Title is
    //     no-op'd, so this is the earliest point all match content exists). `s` then
    //     dispatches straight into a match with no menu ever shown — the fast-boot flow.
    //   - non-headless: the END of launchScreen@17771 — i.e. AFTER the Title has been built
    //     and shown. This guarantees a normal boot is never preempted: READY (and therefore
    //     any `s` the harness sends) only happens once the title is up. A match launches
    //     only if `s` is explicitly sent, after the title — never auto-skipped.
    let menu_ready_g = add_string_const(code, "READY\n");
    let ready_hook = if headless { onloaded_fx } else { launchscreen_fx };
    inject_ready_flag(code, ready_hook, g_ready, g_sock, out_field, write_str, flush, menu_ready_g, sock_t, out_t, str_t, enc_t)?;
    // FAST BOOT (deeper): filter the boot required-resources load to skip ALL public:: base
    // content (the ~10.6s of base-character renders we don't need for a 1-char training
    // match). Keeps global:: (hscript/vfx/vsmode) + private:: (common/fonts). The match's
    // stage/assist (also public::) are loaded on demand by the `s` handler from its args
    // (getResourceByID + fetchThreaded), so this is generic — no hardcoded resource names.
    // Headless-only: a normal boot needs the full content set for the menus/picker.
    if headless {
        inject_required_filter(code)?;
    }
    // Animation/state telemetry: hook Character.toState's exit so each completed state
    // transition emits "ANIM:<state>" over the harness socket (event-driven — fires exactly
    // on a change, no per-frame polling). Pinpoints which move was active at a crash.
    inject_anim_telemetry(code, to_state, g_sock, out_field, write_str, flush,
        get_state_name, str_add, anim_prefix_g, nl_g, sock_t, out_t, str_t, enc_t)?;
    // PER-FRAME telemetry, the mirror of the SSF2 recorder. `ANIM:` above fires on a state
    // CHANGE, which can't answer "how many frames did that move actually take" — the
    // question the SSF2/Fraymakers timing comparison is built on. This emits
    // "FRAME:<state>" every frame for the driven player, exactly as SSF2's injected
    // ENTER_FRAME handler pushes FRAME: over its bridge socket, so both engines report the
    // same quantity by the same mechanism and the host run-length-encodes either one.
    inject_frame_telemetry(code, g_sock, out_field, write_str, flush,
        get_state_name, str_add, frame_prefix_g, nl_g, sock_t, out_t, str_t, enc_t,
        char_anim_f, anim_name_f, anim_t, bar_g, entity_getx, entity_gety, std_string)?;
    // Engine-side script-error surfacing. The engine runs every character/assist/mode/stage
    // script through ApiScript.interpretScript, which builds a rich error (Std.string + line
    // via posInfos + origin) and calls Tildebugger.error — but ONLY when ApiUtilities.
    // loggingEnabled is true, which it isn't in normal play, so trapped script errors are
    // swallowed silently (char loads, hitboxes don't arm). We force that gate on so the
    // engine's own error path fires in the in-game console. ON by default; the GUI checkbox
    // (or PEPTIDE_ENGINE_LOGGING=0) opts out. Non-fatal: a symbol drift just skips the feature.
    if std::env::var("PEPTIDE_ENGINE_LOGGING").map(|v| v != "0").unwrap_or(true) {
        if let Err(e) = inject_logging_override(code) {
            eprintln!("inject_logging_override: skipped ({e})");
        }
        // Un-gate the in-game Tildebugger console so errors actually render on screen
        // (production builds drop non-"important" logs).
        if let Err(e) = inject_console_display_override(code) {
            eprintln!("inject_console_display_override: skipped ({e})");
        }
        // The socket leg: prepend to the Tildebugger.error chokepoint so every script error
        // (frame scripts, per-frame fns, lifecycle runners, one-shot eval) hits the TCP stream.
        if let Err(e) = inject_error_socket_mirror(
            code, g_sock, out_field, write_str, flush, str_add, nl_g, sock_t, out_t, str_t, enc_t,
        ) {
            eprintln!("inject_error_socket_mirror: skipped ({e})");
        }
    }
    // Crash diagnostics, split per the "keep logic OUT of bytecode" convention
    // (AGENT_CONTEXT.md): the ONLY thing that must be in bytecode is pulling the one fact
    // `error.log` lacks — the resource id that was null. inject_stage_diag emits that id
    // over the socket right before the (intentional) stage crash; all interpretation +
    // formatting of the enhanced log is host-side in `interpreter::interpret_crash`.
    inject_stage_diag(code, g_sock, out_field, write_str, flush, str_add, nl_g,
        sock_t, out_t, str_t, enc_t)?;
    // DIAG (multi-char launch pinpoint): mark each function in the offline launch chain so the LAST
    // marker that fires identifies where a 2-char startMatch blocks. Resolve by name (findices shift
    // per build). Remove once the hang is fixed.
    if std::env::var("PEPTIDE_LAUNCH_DIAG").is_ok() {
        let fm = "fraymakers.core.FraymakersMode";
        for (name, parent, marker) in [
            ("_offlineMatchStart", fm, "DIAG:OMS\n"),
            ("sanitizePlayerConfig", fm, "DIAG:SPC\n"),
            ("createPlayerConfig", "fraymakers.util.$FraymakersClassFactory", "DIAG:CPC\n"),
            ("sanitizePorts", fm, "DIAG:SPORTS\n"),
            ("prepTeams", fm, "DIAG:PREPTEAMS\n"),
            ("startMatch", "pxf.controllers.$MatchController", "DIAG:MCSTART\n"),
            ("spawnPlayer", "pxf.core.Match", "DIAG:SPAWN\n"),
        ] {
            if let Some(fx) = find_fn(code, name, Some(parent)) {
                inject_entry_marker(code, fx, marker, g_sock, out_field, write_str, flush,
                    sock_t, out_t, str_t, enc_t)?;
            } else {
                eprintln!("connect_edit: launch-diag could not resolve {parent}::{name}");
            }
        }
    }
    Ok(())
}

/// MINIMAL engine-side crash diagnostic (the one fact `error.log` can't give us). The
/// match-stage crash is `Match.setupStage` op74 `NullCheck Reg15`, where Reg15 =
/// `getPXFResource(stageId)` (op73) and Reg8 = the stage's resource-id string (op72). The
/// stack trace names `setupStage` but NOT the id, so we insert before op74: if Reg15 is
/// null, emit `RESDIAG: … <stageId>` to the socket, then fall through to the (intentional)
/// crash. Everything downstream — capturing + formatting the enhanced log — is host-side
/// (`interpreter::interpret_crash`, the page's RESDIAG buffer). Op indices are asserted so
/// a Fraymakers layout change fails loudly instead of corrupting control flow.
fn inject_stage_diag(
    code: &mut Bytecode,
    g_sock: usize, out_field: usize, write_str: usize, flush: usize, str_add: usize, nl_g: usize,
    sock_t: usize, out_t: usize, str_t: usize, enc_t: usize,
) -> anyhow::Result<()> {
    use hlbc::types::{RefField, RefFun, RefGlobal};
    let diag_g = add_string_const(code,
        "RESDIAG: stage failed to load — Match.setupStage got null from getPXFResource for resource id: ");
    let setupstage_fx = require_fn(code, "setupStage", Some("pxf.core.Match"))?;
    let fi = function_index_by_findex(code, setupstage_fx)
        .ok_or_else(|| anyhow::anyhow!("Match.setupStage not found"))?;
    // getPXFResource's findex drifts; resolve by name and assert op73 calls THAT.
    let getpxf_findex = require_fn(code, "getPXFResource", Some("pxf.io.$ResourceManager"))?;
    // Verify the op layout at the insertion point before touching it.
    match code.functions[fi].ops.get(73) {
        Some(Opcode::Call1 { dst, fun, .. }) if dst.0 == 15 && fun.0 == getpxf_findex => {}
        o => anyhow::bail!("setupStage op73 not getPXFResource->r15: {o:?}"),
    }
    match code.functions[fi].ops.get(74) {
        Some(Opcode::NullCheck { reg }) if reg.0 == 15 => {}
        o => anyhow::bail!("setupStage op74 not NullCheck r15: {o:?}"),
    }
    let base = add_regs(&mut code.functions[fi], &[sock_t, out_t, str_t, str_t, enc_t, 0]);
    let (r_sock, r_out, r_msg, r_nl, r_null, r_ret) =
        (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3), Reg(base + 4), Reg(base + 5));
    let at = 74usize;
    let mut ins = vec![
        Opcode::JNotNull { reg: Reg(15), offset: 0 },                          // 0 -> op74 (stage ok)
        Opcode::GetGlobal { dst: r_sock, global: RefGlobal(g_sock) },           // 1
        Opcode::JNull { reg: r_sock, offset: 0 },                             // 2 -> op74 (not connected)
        Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) },  // 3
        Opcode::GetGlobal { dst: r_msg, global: RefGlobal(diag_g) },           // 4
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: Reg(8) }, // 5  (+ stage id)
        Opcode::GetGlobal { dst: r_nl, global: RefGlobal(nl_g) },              // 6
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_nl }, // 7
        Opcode::Null { dst: r_null },                                          // 8
        Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: r_msg, arg2: r_null }, // 9
        Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out },          // 10
    ];
    let n = ins.len() as i32; // 11 — original op74 lands at index at+n
    if let Opcode::JNotNull { offset, .. } = &mut ins[0] { *offset = n - 1; }       // -> op74
    if let Opcode::JNull   { offset, .. } = &mut ins[2] { *offset = n - 3; }        // -> op74
    let f = &mut code.functions[fi];
    // op66 JAlways spans the insertion (targets op131) -> +n.
    match &mut f.ops[66] { Opcode::JAlways { offset, .. } => *offset += n,
        o => anyhow::bail!("setupStage op66 not JAlways: {o:?}") }
    for (i, op) in ins.into_iter().enumerate() { f.ops.insert(at + i, op); }
    if let Some(dbg) = f.debug_info.as_mut() {
        let fill = dbg.get(at).copied().unwrap_or((0, 0));
        for _ in 0..(n as usize) { dbg.insert(at, fill); }
    }
    if let Some(assigns) = f.assigns.as_mut() {
        for (_nm, pos) in assigns.iter_mut() { if *pos >= at { *pos += n as usize; } }
    }
    eprintln!("connect_edit: Match.setupStage stage-id crash diagnostic installed");
    Ok(())
}

/// DIAG: prepend a one-line socket marker to the ENTRY of `findex` (emits `<marker>` once per
/// call). Used to pinpoint where the multi-char launch chain blocks: whichever marker is the LAST
/// to fire (with no successor) is the hang site. Front insertion is jump-safe (insert_ops_front).
#[allow(clippy::too_many_arguments)]
fn inject_entry_marker(
    code: &mut Bytecode, findex: usize, marker: &str,
    g_sock: usize, out_field: usize, write_str: usize, flush: usize,
    sock_t: usize, out_t: usize, str_t: usize, enc_t: usize,
) -> anyhow::Result<()> {
    use hlbc::types::{RefField, RefFun, RefGlobal, Reg};
    let marker_g = add_string_const(code, marker);
    let fi = function_index_by_findex(code, findex)
        .ok_or_else(|| anyhow::anyhow!("findex {findex} not found for entry marker {marker:?}"))?;
    let base = add_regs(&mut code.functions[fi], &[sock_t, out_t, str_t, enc_t, 0]);
    let (r_sock, r_out, r_msg, r_null, r_ret) =
        (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3), Reg(base + 4));
    let m = vec![
        Opcode::GetGlobal { dst: r_sock, global: RefGlobal(g_sock) },          // 0
        Opcode::JNull { reg: r_sock, offset: 5 },                             // 1 -> op7 (skip marker)
        Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) }, // 2
        Opcode::GetGlobal { dst: r_msg, global: RefGlobal(marker_g) },         // 3
        Opcode::Null { dst: r_null },                                          // 4
        Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: r_msg, arg2: r_null }, // 5
        Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out },          // 6
    ];
    insert_ops_front(&mut code.functions[fi], m);
    eprintln!("connect_edit: entry marker {marker:?} installed on findex {findex}");
    Ok(())
}

/// Hook the exit of `pxf.entity.Character::toState` so every COMPLETED state transition
/// emits "ANIM:<stateName>\n" to the harness socket. Inserted before the function's final
/// Ret, so it runs on the success path (state set) — guard early-returns (rejected
/// transitions) don't reach it, which is correct (no change to report). reg0 = the Character.
fn inject_anim_telemetry(
    code: &mut Bytecode, to_state: usize, g_sock: usize, out_field: usize, write_str: usize,
    flush: usize, get_state_name: usize, str_add: usize, anim_prefix_g: usize, nl_g: usize,
    sock_t: usize, out_t: usize, str_t: usize, enc_t: usize,
) -> anyhow::Result<()> {
    use hlbc::types::{RefField, RefFun, RefGlobal};
    let fidx = function_index_by_findex(code, to_state)
        .ok_or_else(|| anyhow::anyhow!("Character.toState@{to_state} not found"))?;
    let f = &mut code.functions[fidx];
    let base = add_regs(f, &[sock_t, out_t, str_t, str_t, enc_t, 0]);
    let (r_sock, r_out, r_name, r_msg, r_null, r_ret) =
        (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3), Reg(base + 4), Reg(base + 5));
    let mut ops = vec![
        Opcode::GetGlobal { dst: r_sock, global: RefGlobal(g_sock) },          // 0
        Opcode::JNull { reg: r_sock, offset: 0 },                              // 1 -> skip (not connected)
        Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) }, // 2
        Opcode::Call1 { dst: r_name, fun: RefFun(get_state_name), arg0: Reg(0) }, // 3 (reg0 = char)
        Opcode::JNull { reg: r_name, offset: 0 },                              // 4 -> skip
        Opcode::GetGlobal { dst: r_msg, global: RefGlobal(anim_prefix_g) },     // 5
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name }, // 6
        Opcode::GetGlobal { dst: r_name, global: RefGlobal(nl_g) },             // 7 (reuse r_name for "\n")
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name }, // 8
        Opcode::Null { dst: r_null },                                          // 9
        Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: r_msg, arg2: r_null }, // 10
        Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out },         // 11
    ];
    let skip = ops.len() as i32; // 12 — falls through to the function's Ret
    if let Opcode::JNull { offset, .. } = &mut ops[1] { *offset = skip - 1 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[4] { *offset = skip - 4 - 1; }
    insert_ops_end(f, ops);
    eprintln!("connect_edit: anim telemetry hooked into Character.toState@{to_state}");
    Ok(())
}

/// Append PER-FRAME `FRAME:<stateName>` telemetry to `Character.updateGameInput`.
///
/// The Fraymakers half of the timing-parity pair. SSF2 gets the same treatment from
/// `abc_inject::inject_frame_recorder` (an ENTER_FRAME handler pushing FRAME: over its
/// bridge socket); this is the HashLink equivalent, so both engines report frames the same
/// way and the host can run-length-encode either stream into "<anim> xN".
///
/// Why per-frame rather than reusing the `ANIM:` hook on `toState`: that one is
/// event-driven and fires on a state CHANGE, which tells you the order of moves but not how
/// long each one lasted. Frame COUNT is the whole question — the converter's timing model is
/// a 30→60fps doubling, and confirming it needs the number of frames a move actually
/// occupied in each engine.
///
/// Why not poll from the host instead: polling races the move. A short attack lasts a few
/// hundred milliseconds, the same order as the round trip between sending the input and
/// starting to watch, so the watcher can arrive after it's over — measured on SSF2, where a
/// concurrent poll saw only `stand` in a window where the engine recorded a 7-frame attack.
///
/// `updateGameInput` runs on EVERY character each frame, so this is gated to one player by
/// team (the same gate `inject_input_override` uses, and the same default) — otherwise a
/// 4-player match would emit four interleaved streams.
#[allow(clippy::too_many_arguments)]
fn inject_frame_telemetry(
    code: &mut Bytecode, g_sock: usize, out_field: usize, write_str: usize,
    flush: usize, get_state_name: usize, str_add: usize, frame_prefix_g: usize, nl_g: usize,
    sock_t: usize, out_t: usize, str_t: usize, enc_t: usize,
    char_anim_f: usize, anim_name_f: usize, anim_t: usize, bar_g: usize,
    entity_getx: usize, entity_gety: usize, std_string: usize,
) -> anyhow::Result<()> {
    use hlbc::types::{RefField, RefFun, RefGlobal, RefInt};
    let upd = require_fn(code, "updateGameInput", Some("pxf.entity.Character"))?;
    let getteam = require_fn(code, "getTeam", Some("pxf.entity.Character"))?;
    let team = std::env::var("PEPTIDE_INPUT_TEAM").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let team_c = add_int(code, team);
    let fidx = function_index_by_findex(code, upd)
        .ok_or_else(|| anyhow::anyhow!("Character.updateGameInput@{upd} not found"))?;
    let f = &mut code.functions[fidx];
    // 6 = Float, 9 = Dynamic: POSITION rides the same line as the animation, so a frame's
    // telemetry carries where the entity was as well as what it was playing. SSF2's recorder
    // already reports `<label>|<frame>|<x>|<y>`; without the same here, per-frame displacement
    // can only be measured on one engine, and the speed/scale multipliers can't be derived from
    // observation at all -- which is what pushed measurement back onto wall-clock polling.
    let base = add_regs(f, &[3, 3, sock_t, out_t, str_t, str_t, enc_t, 0, anim_t, str_t, 6, 9, str_t]);
    let (r_team, r_zero, r_sock, r_out, r_name, r_msg, r_null, r_ret, r_anim, r_aname) =
        (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3), Reg(base + 4),
         Reg(base + 5), Reg(base + 6), Reg(base + 7), Reg(base + 8), Reg(base + 9));
    let (r_f, r_dyn, r_num) = (Reg(base + 10), Reg(base + 11), Reg(base + 12));
    let mut ops = vec![
        Opcode::Call1 { dst: r_team, fun: RefFun(getteam), arg0: Reg(0) },      // 0
        Opcode::Int { dst: r_zero, ptr: RefInt(team_c) },                       // 1
        Opcode::JEq { a: r_team, b: r_zero, offset: 1 },                        // 2 -> run
        Opcode::JAlways { offset: 0 },                                          // 3 -> end
        Opcode::GetGlobal { dst: r_sock, global: RefGlobal(g_sock) },           // 4
        Opcode::JNull { reg: r_sock, offset: 0 },                               // 5 -> end
        Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) },  // 6
        // ANIMATION name first — that's the unit SSF2's frame LABEL corresponds to.
        // A Fraymakers STATE spans several animations (one SSF2 "Jab" timeline splits
        // into jab1/jab2/jab3 by internal FrameLabel; see AGENT_CONTEXT "animations" and
        // src/anim_splitter.rs), so keying frames on the state would count a whole chain
        // as one run and could never line up with SSF2's per-label counts.
        Opcode::Field { dst: r_anim, obj: Reg(0), field: RefField(char_anim_f) },// 7
        Opcode::JNull { reg: r_anim, offset: 0 },                               // 8 -> end
        Opcode::Field { dst: r_aname, obj: r_anim, field: RefField(anim_name_f) },// 9
        Opcode::JNull { reg: r_aname, offset: 0 },                              // 10 -> end
        Opcode::Call1 { dst: r_name, fun: RefFun(get_state_name), arg0: Reg(0) },// 11
        Opcode::JNull { reg: r_name, offset: 0 },                               // 12 -> end
        Opcode::GetGlobal { dst: r_msg, global: RefGlobal(frame_prefix_g) },    // 13
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_aname }, // 14
        Opcode::GetGlobal { dst: r_aname, global: RefGlobal(bar_g) },           // 15
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_aname }, // 16
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name }, // 17
        // |<x>|<y> — the same fields SSF2's recorder emits, so both streams are diffable
        Opcode::GetGlobal { dst: r_name, global: RefGlobal(bar_g) },
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name },
        Opcode::Call1 { dst: r_f, fun: RefFun(entity_getx), arg0: Reg(0) },
        Opcode::ToDyn { dst: r_dyn, src: r_f },
        Opcode::Call1 { dst: r_num, fun: RefFun(std_string), arg0: r_dyn },
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_num },
        Opcode::GetGlobal { dst: r_name, global: RefGlobal(bar_g) },
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name },
        Opcode::Call1 { dst: r_f, fun: RefFun(entity_gety), arg0: Reg(0) },
        Opcode::ToDyn { dst: r_dyn, src: r_f },
        Opcode::Call1 { dst: r_num, fun: RefFun(std_string), arg0: r_dyn },
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_num },
        Opcode::GetGlobal { dst: r_name, global: RefGlobal(nl_g) },             // 18
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name }, // 19
        Opcode::Null { dst: r_null },                                           // 20
        Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: r_msg, arg2: r_null }, // 21
        Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out },          // 22
    ];
    let end = ops.len() as i32;
    if let Opcode::JAlways { offset } = &mut ops[3] { *offset = end - 3 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[5] { *offset = end - 5 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[8] { *offset = end - 8 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[10] { *offset = end - 10 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[12] { *offset = end - 12 - 1; }
    insert_ops_end(f, ops);
    eprintln!("connect_edit: per-frame FRAME: telemetry hooked into Character.updateGameInput@{upd} (team {team})");
    Ok(())
}

/// Filter `pxf.io.$ResourceManager::queueRequiredResources@18234` so the boot load skips
/// `public::` resources (base-character renders — the bulk of the slow second load). We
/// insert a 6-op test right after the loop's `get_Required` check (op22): if the resource's
/// fqid starts with "public::", jump to the loop-tail (skip queuing it). Mid-function
/// insertion is jump-safe here because only THREE jumps span op22 (op6 loop-exit, op21
/// not-required, op121 loop-back); we adjust exactly those by ±N. All build-specific op
/// indices are asserted before patching so a layout change fails loudly instead of
/// silently corrupting control flow.
fn inject_required_filter(code: &mut Bytecode) -> anyhow::Result<()> {
    use hlbc::types::{RefFun, RefGlobal, RefInt};
    let getfqid = require_fn(code, "getFullyQualifiedResourceId", Some("pxf.io.AbstractResource"))?;
    let indexof = require_fn(code, "indexOf", Some("String"))?;
    // 3rd arg (startIndex: Null<Int>) type of String.indexOf
    let startidx_t = {
        let ci = function_index_by_findex(code, indexof)
            .ok_or_else(|| anyhow::anyhow!("indexOf fn missing"))?;
        code.types[code.functions[ci].t.0].get_type_fun()
            .and_then(|tf| tf.args.get(2)).map(|a| a.0)
            .ok_or_else(|| anyhow::anyhow!("indexOf startIndex arg type missing"))?
    };
    let pub_g = add_string_const(code, "public::");
    let zero_c = add_int(code, 0);
    let qrr_findex = require_fn(code, "queueRequiredResources", Some("pxf.io.$ResourceManager"))?;
    let qfi = function_index_by_findex(code, qrr_findex)
        .ok_or_else(|| anyhow::anyhow!("queueRequiredResources not found"))?;
    // scratch regs: r_str(String), r_null(startidx_t), r_idx(Int), r_zero(Int)
    let base = add_regs(&mut code.functions[qfi], &[13, startidx_t, 3, 3]);
    let (r_str, r_null, r_idx, r_zero) = (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3));
    // SKIP if the fqid starts with "public::" (jump to loop-tail). The match's stage/assist
    // are ALSO public:: but the `s` handler loads them on demand (getResourceByID +
    // fetchThreaded), so we can skip the entire public:: namespace generically here — no
    // hardcoded resource names. The char is private:: (loaded by the self-bootstrap).
    let filt = vec![
        Opcode::Call1 { dst: Reg(13), fun: RefFun(getfqid), arg0: Reg(9) },                          // 22: fqid (reg9 = loop resource)
        Opcode::GetGlobal { dst: r_str, global: RefGlobal(pub_g) },                                   // 23
        Opcode::Null { dst: r_null },                                                                 // 24
        Opcode::Call3 { dst: r_idx, fun: RefFun(indexof), arg0: Reg(13), arg1: r_str, arg2: r_null }, // 25
        Opcode::Int { dst: r_zero, ptr: RefInt(zero_c) },                                             // 26
        Opcode::JEq { a: r_idx, b: r_zero, offset: 99 },                                              // 27: indexOf==0 (starts public::) -> loop-tail(127)
    ];
    let n = filt.len() as i32; // 6
    let f = &mut code.functions[qfi];
    // Assert + adjust the three jumps that span the insertion point (op22).
    match &mut f.ops[6]   { Opcode::JSGte  { offset, .. } => *offset += n, o => anyhow::bail!("qrr op6 not JSGte: {o:?}") }
    match &mut f.ops[21]  { Opcode::JFalse { offset, .. } => *offset += n, o => anyhow::bail!("qrr op21 not JFalse: {o:?}") }
    match &mut f.ops[121] { Opcode::JAlways{ offset, .. } => *offset -= n, o => anyhow::bail!("qrr op121 not JAlways: {o:?}") }
    let at = 22usize;
    for (i, op) in filt.into_iter().enumerate() { f.ops.insert(at + i, op); }
    if let Some(dbg) = f.debug_info.as_mut() {
        let fill = dbg.get(at).copied().unwrap_or((0, 0));
        for _ in 0..(n as usize) { dbg.insert(at, fill); }
    }
    if let Some(assigns) = f.assigns.as_mut() {
        for (_nm, pos) in assigns.iter_mut() { if *pos >= at { *pos += n as usize; } }
    }
    eprintln!("inject_required_filter: queueRequiredResources@18234 — skip public:: in boot load");
    Ok(())
}

/// Append ops just before the final Ret of a function (so they run after the
/// original body). Existing forward jumps are unaffected (they target indices
/// before the inserted block); debug_info gets matching entries.
fn insert_ops_end(f: &mut hlbc::types::Function, ops: Vec<Opcode>) {
    let at = f.ops.len().saturating_sub(1); // before the trailing Ret
    let n = ops.len();
    for (i, op) in ops.into_iter().enumerate() {
        f.ops.insert(at + i, op);
    }
    if let Some(dbg) = f.debug_info.as_mut() {
        let fill = dbg.get(at).copied().or_else(|| dbg.last().copied()).unwrap_or((0, 0));
        for _ in 0..n {
            dbg.insert(at, fill);
        }
    }
    if let Some(assigns) = f.assigns.as_mut() {
        for (_n, pos) in assigns.iter_mut() {
            if *pos >= at {
                *pos += n;
            }
        }
    }
}

/// Prepend a per-frame input override to the TOP of `Character.updateGameInput`.
///
/// The user-facing goal: drive a character's controls WITHOUT synthesizing keyboard /
/// gamepad input — by writing the control state the engine itself maps to actions.
///
/// `updateGameInput` copies `InputFeed`'s held/pressed `ControlsObject`s into the live
/// `m_heldControls(147)` / `m_pressedControls(146)` (its ip16-27), which the InputResolver
/// reads to fire moves AND `getHeldControls()` copies for scripts (e.g. a character's
/// `dashCheck`). The injection MUST land UPSTREAM of that copy: an earlier version OR-ed
/// the mask in at the function's END, but the ip27 copy overwrites 147 with the device's
/// (empty) controls earlier in the same frame, so anything reading 147 between ip27 and the
/// end-epilogue (runInputUpdateHook, the timer/script path) saw an empty mask — the resolver
/// (post-update) got it, but `getHeldControls()` in a same-frame timer did not.
///
/// So we prepend, modifying the InputFeed SOURCE before the copy:
///   - `inputFeed._heldControls.buttons   |= g_inject_held`         (held mask)
///   - `inputFeed._pressedControls.buttons |= g_inject_held & ~m_heldControls.buttons`
/// `m_heldControls(147)` at the top of updateGameInput still holds LAST frame's value (the
/// copy hasn't run yet), so the pressed term is a true one-frame RISING edge — a held mask
/// taps `pressed` once then stays held. The engine's own copyFrom then propagates both into
/// 146/147 for the WHOLE frame, so every consumer (resolver, scripts, timers) sees a
/// consistent mask. Guarded on a null `inputFeed`; a 0 mask is a no-op. Resolves by NAME.
fn inject_input_override(code: &mut Bytecode, g_inject_held: usize) -> anyhow::Result<()> {
    use hlbc::types::{RefField, RefGlobal, RefInt};
    let upd = require_fn(code, "updateGameInput", Some("pxf.entity.Character"))?;
    let co_t = require_type(code, "pxf.input.ControlsObject")?;
    let if_t = require_type(code, "pxf.components.InputFeed")?;
    let f_inputfeed = require_field(code, "pxf.entity.Character", "inputFeed")?;
    let f_held = require_field(code, "pxf.entity.Character", "m_heldControls")?; // last-frame held at fn top
    let f_if_held = require_field(code, "pxf.components.InputFeed", "_heldControls")?;
    let f_if_pressed = require_field(code, "pxf.components.InputFeed", "_pressedControls")?;
    let f_buttons = require_field(code, "pxf.input.ControlsObject", "buttons")?;
    let neg1 = add_int(code, -1); // ~x via Xor(x, -1)
    // Which player the injected input (`i`/hold/seq) drives, by team. updateGameInput
    // runs on EVERY character, so without this gate every player would receive the
    // injected held mask at once (both would jab and their hitboxes would clank).
    // Player 1 gets team 0, extras get team >= 1 (see the player-config team
    // assignment), so the team number uniquely selects one player. Default 0 drives
    // player 1; set $PEPTIDE_INPUT_TEAM=N at patch time to drive a different player
    // (e.g. =1 to drive the spawned dummy). getTeam is a public, hscript-callable accessor.
    let input_team = std::env::var("PEPTIDE_INPUT_TEAM").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let zero = add_int(code, input_team);
    use hlbc::types::RefFun;
    let getteam = require_fn(code, "getTeam", Some("pxf.entity.Character"))?;

    let fidx = function_index_by_findex(code, upd)
        .ok_or_else(|| anyhow::anyhow!("updateGameInput@{upd} not found"))?;
    let f = &mut code.functions[fidx];
    // Scratch regs: team, zero, mask, inputFeed, hc, hb, prev, pb, neg1, notprev, edge, pc, pcb.
    // ControlsObject/InputFeed-typed regs so HL computes field offsets correctly.
    let b = add_regs(f, &[3, 3, 3, if_t, co_t, 3, co_t, 3, 3, 3, 3, co_t, 3]);
    let (r_team, r_zero, r_mask, r_if, r_hc, r_hb, r_prev, r_pb, r_neg1, r_notprev, r_edge, r_pc, r_pcb) =
        (Reg(b), Reg(b + 1), Reg(b + 2), Reg(b + 3), Reg(b + 4), Reg(b + 5), Reg(b + 6), Reg(b + 7), Reg(b + 8), Reg(b + 9), Reg(b + 10), Reg(b + 11), Reg(b + 12));
    let this = Reg(0); // updateGameInput(this:Character)
    let mut ops = vec![
        // gate: if getTeam(this) != 0, skip the whole injection (only player 0 is driven).
        Opcode::Call1 { dst: r_team, fun: RefFun(getteam), arg0: this }, // [idx 0]
        Opcode::Int { dst: r_zero, ptr: RefInt(zero) },                  // [idx 1]
        Opcode::JEq { a: r_team, b: r_zero, offset: 1 },                 // [idx 2] team==0 -> run body
        Opcode::JAlways { offset: 0 },                                   // [idx 3] team!=0 -> skip to end
        Opcode::GetGlobal { dst: r_mask, global: RefGlobal(g_inject_held) },
        Opcode::Field { dst: r_if, obj: this, field: RefField(f_inputfeed) },
        Opcode::JNull { reg: r_if, offset: 0 }, // [idx 2] no inputFeed -> skip (patched to end)
        // inputFeed._heldControls.buttons |= mask
        Opcode::Field { dst: r_hc, obj: r_if, field: RefField(f_if_held) },
        Opcode::Field { dst: r_hb, obj: r_hc, field: RefField(f_buttons) },
        Opcode::Or { dst: r_hb, a: r_hb, b: r_mask },
        Opcode::SetField { obj: r_hc, field: RefField(f_buttons), src: r_hb },
        // edge = mask & ~(m_heldControls.buttons)  [147 = LAST frame's held at fn top]
        Opcode::Field { dst: r_prev, obj: this, field: RefField(f_held) },
        Opcode::Field { dst: r_pb, obj: r_prev, field: RefField(f_buttons) },
        Opcode::Int { dst: r_neg1, ptr: RefInt(neg1) },
        Opcode::Xor { dst: r_notprev, a: r_pb, b: r_neg1 },
        Opcode::And { dst: r_edge, a: r_mask, b: r_notprev },
        // inputFeed._pressedControls.buttons |= edge
        Opcode::Field { dst: r_pc, obj: r_if, field: RefField(f_if_pressed) },
        Opcode::Field { dst: r_pcb, obj: r_pc, field: RefField(f_buttons) },
        Opcode::Or { dst: r_pcb, a: r_pcb, b: r_edge },
        Opcode::SetField { obj: r_pc, field: RefField(f_buttons), src: r_pcb },
    ];
    // The team gate (JAlways idx 3) and the null-inputFeed guard (JNull idx 6) both skip
    // to the first original op, which insert_ops_front places right after this block.
    let n = ops.len() as i32;
    if let Opcode::JAlways { offset, .. } = &mut ops[3] { *offset = n - 3 - 1; }
    if let Opcode::JNull { offset, .. } = &mut ops[6] { *offset = n - 6 - 1; }
    insert_ops_front(f, ops);
    eprintln!("inject_input_override: updateGameInput@{upd} PREPEND (inputFeed held|=mask, pressed|=edge — upstream of the copy)");
    Ok(())
}

/// Force the trapped-script-error gate inside `ApiScript.interpretScript` to take the
/// logging branch, so the engine's own detailed error surfaces instead of being swallowed.
///
/// interpretScript wraps every character/assist/mode/stage script in a try/catch. The
/// catch already formats a detailed error (`Std.string(exc)` + line/col via `posInfos` +
/// the script origin) and calls `Tildebugger.error(...)`. The gate is the opposite polarity
/// of what its field name suggests: the bytecode is `if (loggingEnabled) return; else
/// log`, i.e. the read is FALSE on the path that actually logs (and `loggingEnabled` is the
/// value passed as the error()'s second arg, always false on the real logging path). In
/// normal play the field reads true, so the catch returns early and the error is silently
/// swallowed (the character loads, but e.g. its hitboxes never arm).
///
/// The fix is one in-place op swap: the gate's `Field` read becomes a constant `false`,
/// which forces the log branch AND reproduces the engine's genuine `error(msg, false)`
/// call exactly. No new ops, no offset shifts. Matched by field + obj-reg type (not op
/// index) so it survives engine-build drift. Local to interpretScript.
/// Flip the `loggingEnabled` gate in `ApiScript.interpretScript` (the one-shot script-eval
/// path) so its catch reaches the log branch. Only this one-shot path is gated; the
/// per-frame character paths (`interpretScriptFunction`, `HaxeScript.runFrameScripts`) call
/// `Tildebugger.error` unconditionally. See `inject_error_socket_mirror` for the socket leg.
fn inject_logging_override(code: &mut Bytecode) -> anyhow::Result<()> {
    let interp_script = require_fn(code, "interpretScript", Some("pxf.api.$ApiScript"))?;
    let apiutil_t = find_type(code, "pxf.api.$ApiUtilities")
        .ok_or_else(|| anyhow::anyhow!("pxf.api.$ApiUtilities type not found"))?;
    let logging_field = find_field(code, apiutil_t, "loggingEnabled")
        .ok_or_else(|| anyhow::anyhow!("ApiUtilities.loggingEnabled field not found"))?;
    let fidx = function_index_by_findex(code, interp_script)
        .ok_or_else(|| anyhow::anyhow!("interpretScript@{interp_script} not found"))?;
    let targets: Vec<(usize, Reg)> = {
        let f = &code.functions[fidx];
        f.ops.iter().enumerate().filter_map(|(i, op)| match op {
            Opcode::Field { dst, obj, field }
                if field.0 == logging_field
                    && f.regs.get(obj.0 as usize).map(|t| t.0) == Some(apiutil_t) =>
            {
                Some((i, *dst))
            }
            _ => None,
        }).collect()
    };
    if targets.is_empty() {
        anyhow::bail!("loggingEnabled gate read not found in interpretScript@{interp_script}");
    }
    let f = &mut code.functions[fidx];
    for (i, dst) in &targets {
        f.ops[*i] = Opcode::Bool { dst: *dst, value: hlbc::types::ValBool(false) };
    }
    eprintln!(
        "inject_logging_override: loggingEnabled gate -> false (one-shot eval log branch) in interpretScript@{interp_script} ({} site)",
        targets.len()
    );
    Ok(())
}

/// Force the in-game Tildebugger console to actually DISPLAY errors. `Tildebugger.log`
/// gates the `ImprovedConsole.log` call behind `if (production && !important) return;` —
/// Fraymakers ships as a production build and script errors arrive with the important flag
/// false, so the console silently drops them (the message was built and the socket mirror
/// already saw it; only the on-screen render is suppressed). Flip the `production` field
/// read to `false` so the display branch always runs. Local to `Tildebugger.log`.
fn inject_console_display_override(code: &mut Bytecode) -> anyhow::Result<()> {
    let log_fn = require_fn(code, "log", Some("pxf.core.$Tildebugger"))?;
    let tilde_t = find_type(code, "pxf.core.$Tildebugger")
        .ok_or_else(|| anyhow::anyhow!("pxf.core.$Tildebugger type not found"))?;
    let prod_field = find_field(code, tilde_t, "production")
        .ok_or_else(|| anyhow::anyhow!("Tildebugger.production field not found"))?;
    let fidx = function_index_by_findex(code, log_fn)
        .ok_or_else(|| anyhow::anyhow!("Tildebugger.log@{log_fn} not found"))?;
    let targets: Vec<(usize, Reg)> = {
        let f = &code.functions[fidx];
        f.ops.iter().enumerate().filter_map(|(i, op)| match op {
            Opcode::Field { dst, obj, field }
                if field.0 == prod_field
                    && f.regs.get(obj.0 as usize).map(|t| t.0) == Some(tilde_t) =>
            {
                Some((i, *dst))
            }
            _ => None,
        }).collect()
    };
    if targets.is_empty() {
        anyhow::bail!("production gate read not found in Tildebugger.log@{log_fn}");
    }
    let f = &mut code.functions[fidx];
    for (i, dst) in &targets {
        f.ops[*i] = Opcode::Bool { dst: *dst, value: hlbc::types::ValBool(false) };
    }
    eprintln!(
        "inject_console_display_override: production gate -> false in Tildebugger.log@{log_fn} ({} site); errors now render in the in-game console",
        targets.len()
    );
    Ok(())
}

/// Mirror EVERY engine error onto the harness socket as a `SCRIPTERR:` line by prepending
/// a socket write to the single chokepoint `Tildebugger.error(msg, ...)`. All script-error
/// catches (one-shot eval, per-frame `interpretScriptFunction`, keyframe `runFrameScripts`,
/// the lifecycle runners) funnel through here, so one front-insertion covers them all — and
/// the same errors keep going to the in-game Tildebugger console as before. `reg0` is the
/// message String. Guarded by a g_sock null-check (no-op before the harness connects).
#[allow(clippy::too_many_arguments)]
fn inject_error_socket_mirror(
    code: &mut Bytecode, g_sock: usize, out_field: usize, write_str: usize, flush: usize,
    str_add: usize, nl_g: usize, sock_t: usize, out_t: usize, str_t: usize, enc_t: usize,
) -> anyhow::Result<()> {
    use hlbc::types::{RefField, RefFun, RefGlobal};
    let tilde_error = require_fn(code, "error", Some("pxf.core.$Tildebugger"))?;
    let fidx = function_index_by_findex(code, tilde_error)
        .ok_or_else(|| anyhow::anyhow!("Tildebugger.error@{tilde_error} not found"))?;
    let scripterr_g = add_string_const(code, "SCRIPTERR: ");
    let f = &mut code.functions[fidx];
    // reg0 is the error message (String) — prepend runs before the original body touches it.
    let msg_reg = Reg(0);
    let base = add_regs(f, &[sock_t, out_t, str_t, str_t, enc_t, 0]);
    let (r_sock, r_out, r_name, r_msg, r_null, r_ret) =
        (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3), Reg(base + 4), Reg(base + 5));
    let mut ops = vec![
        Opcode::GetGlobal { dst: r_sock, global: RefGlobal(g_sock) },          // 0
        Opcode::JNull { reg: r_sock, offset: 0 },                              // 1 -> skip (not connected yet)
        Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) }, // 2
        Opcode::GetGlobal { dst: r_msg, global: RefGlobal(scripterr_g) },       // 3  "SCRIPTERR: "
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: msg_reg }, // 4  + message
        Opcode::GetGlobal { dst: r_name, global: RefGlobal(nl_g) },             // 5  "\n"
        Opcode::Call2 { dst: r_msg, fun: RefFun(str_add), arg0: r_msg, arg1: r_name }, // 6  + "\n"
        Opcode::Null { dst: r_null },                                          // 7
        Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: r_msg, arg2: r_null }, // 8
        Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out },         // 9
    ];
    let skip = ops.len() as i32;
    if let Opcode::JNull { offset, .. } = &mut ops[1] { *offset = skip - 1 - 1; }
    insert_ops_front(f, ops);
    eprintln!(
        "inject_error_socket_mirror: SCRIPTERR socket mirror prepended to Tildebugger.error@{tilde_error}; every engine/script error now also hits the Peptide TCP stream"
    );
    Ok(())
}

/// Set g_ready=true at the start of MainMenu's constructor AND send a marker over
/// the harness socket so we can see when the menu is built (= content loaded).
#[allow(clippy::too_many_arguments)]
fn inject_ready_flag(
    code: &mut Bytecode, ctor_findex: usize, g_ready: usize, g_sock: usize,
    out_field: usize, write_str: usize, flush: usize, marker_g: usize,
    sock_t: usize, out_t: usize, str_t: usize, enc_t: usize,
) -> anyhow::Result<()> {
    // ThreadTaskManager.init — spawns the worker thread that drains the UGC/.fra
    // load deque. Its findex drifts every recompile (the old pinned 25781 now points
    // at DelayBasedNetworkMatch::delayBasedInputCheck → SIGSEGV), so resolve by name.
    let ttm_init = find_fn(code, "init", Some("pxf.io.$ThreadTaskManager"))
        .ok_or_else(|| anyhow::anyhow!("ThreadTaskManager.init not found"))?;
    let fidx = function_index_by_findex(code, ctor_findex)
        .ok_or_else(|| anyhow::anyhow!("ctor @{ctor_findex} not found"))?;
    let f = &mut code.functions[fidx];
    let base = add_regs(f, &[7, sock_t, out_t, str_t, enc_t, 0]);
    let (r_b, r_sock, r_out, r_str, r_enc, r_ret) =
        (Reg(base), Reg(base + 1), Reg(base + 2), Reg(base + 3), Reg(base + 4), Reg(base + 5));
    use hlbc::types::{RefField, RefFun, RefGlobal};
    let mut ops = vec![
        // Kick off custom-content (UGC) loading. Our injected boot path
        // (Title.start → MainMenu) bypasses Main::launchScreen, which normally
        // calls UgcUtil.loadUgc. We call loadInLocalUgc@17842 (no-arg, no guards)
        // which scans getCwd()/custom async.
        //
        // The threaded local-UGC load (addDirToLoadQueue → load@1845 → fetch →
        // ThreadTaskManager.queueTask@25758 = deque_push) only ENQUEUES tasks; a
        // worker thread spawned by ThreadTaskManager.init@25781 (HaxeThread.create
        // running the deque_pop@26003 loop) must drain them. init is normally
        // called from CoreEngine.init@17729. We call init@25781 (no args) FIRST,
        // defensively, so the worker thread exists to drain our queued .fra loads
        // even if our headless boot path skipped the normal init. (Verified: the
        // ONLY function that pops the task deque is init's spawned worker; without
        // it the `k` pool-key dump showed only private::common, never custom.)
        Opcode::Call0 { dst: r_ret, fun: RefFun(ttm_init) },
        // FAST BOOT: do NOT call loadInLocalUgc here — that scanned custom/ and loaded
        // EVERY custom character (the slow "loading all custom content" screen). We only
        // need the ONE char `s` requests, which the `s`/`l` self-bootstrap loads
        // synchronously on demand. Base/global content (vfx, stages, assists) is loaded
        // by the normal config boot, not UGC, so it's unaffected. (Was: Call0 load_ugc.)
        Opcode::Bool { dst: r_b, value: hlbc::types::ValBool(true) },
        Opcode::SetGlobal { global: RefGlobal(g_ready), src: r_b },
        Opcode::GetGlobal { dst: r_sock, global: RefGlobal(g_sock) },
    ];
    let idx_jnull = ops.len();
    ops.push(Opcode::JNull { reg: r_sock, offset: 0 }); // no socket -> skip send
    ops.push(Opcode::Field { dst: r_out, obj: r_sock, field: RefField(out_field) });
    ops.push(Opcode::GetGlobal { dst: r_str, global: RefGlobal(marker_g) });
    ops.push(Opcode::Null { dst: r_enc });
    ops.push(Opcode::Call3 { dst: r_ret, fun: RefFun(write_str), arg0: r_out, arg1: r_str, arg2: r_enc });
    ops.push(Opcode::Call1 { dst: r_ret, fun: RefFun(flush), arg0: r_out });
    let n = ops.len() as i32;
    if let Opcode::JNull { offset, .. } = &mut ops[idx_jnull] { *offset = n - idx_jnull as i32 - 1; }
    let _ = (out_t, str_t, enc_t);
    insert_ops_front(f, ops);
    eprintln!("inject_ready_flag: patched ctor @{ctor_findex}");
    Ok(())
}

/// M1: inject `throw "HARNESS_PROBE_OK"` at the very start of a function, to
/// prove (a) opcode injection round-trips, (b) the function executes at boot.
fn probe_edit(code: &mut Bytecode, target_findex: usize) -> anyhow::Result<()> {
    // Add our marker string to the constant pool (appended -> no index shifts).
    let marker = "HARNESS_PROBE_OK";
    let str_idx = code.strings.len();
    code.strings.push(hlbc::Str::from(marker));
    eprintln!("added string #{str_idx} = {marker:?}");

    let fidx = function_index_by_findex(code, target_findex)
        .ok_or_else(|| anyhow::anyhow!("function findex {target_findex} not found"))?;
    let f = &mut code.functions[fidx];
    eprintln!(
        "patching fn idx {fidx} (findex {}), name#{}, {} regs, {} ops",
        f.findex.0,
        f.name.0,
        f.regs.len(),
        f.ops.len()
    );

    // Reuse an existing String-typed register if present (type 13 == String),
    // else add a new one.
    let str_reg = match f.regs.iter().position(|t| t.0 == 13) {
        Some(i) => i as u32,
        None => {
            let i = f.regs.len() as u32;
            f.regs.push(hlbc::types::RefType(13));
            i
        }
    };
    eprintln!("using String reg {str_reg}");

    // Prepend: String rN = "HARNESS_PROBE_OK"; Throw rN
    let inject = vec![
        Opcode::String {
            dst: Reg(str_reg),
            ptr: RefString(str_idx),
        },
        Opcode::Throw {
            exc: Reg(str_reg),
        },
    ];
    insert_ops_front(f, inject);
    eprintln!("injected ops at front; now {} ops", f.ops.len());
    Ok(())
}
