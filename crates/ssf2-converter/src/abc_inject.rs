//! abc_inject — an AVM2 bytecode assembler + injection helpers on top of
//! `abc_codec`. This is the SSF2-side analogue of the Fraymakers HashLink
//! patcher (`src/asm.rs` + `insert_ops_front`): it adds the strings /
//! namespaces / multinames it needs, emits opcodes, and splices a payload into
//! an existing method so injected ActionScript runs live.
//!
//! Offset safety: we PREPEND the payload to the target method body. Relative
//! branches (the only kind AVM2 uses for jumps) are unaffected by a uniform
//! shift of the whole body; only the absolute exception-table offsets
//! (from/to/target) need `+= prepend_len`, which we fix up.

use crate::abc_codec::*;
use std::path::Path;

// ── AVM2 opcodes we emit ──
const OP_GETLOCAL0: u8 = 0xD0;
const OP_PUSHSCOPE: u8 = 0x30;
const OP_POPSCOPE: u8 = 0x1D;
const OP_FINDPROPSTRICT: u8 = 0x5D;
const OP_GETLEX: u8 = 0x60;
const OP_PUSHSTRING: u8 = 0x2C;
const OP_CONSTRUCTPROP: u8 = 0x4A;
const OP_CALLPROPVOID: u8 = 0x4F;
const OP_GETPROPERTY: u8 = 0x66;
const OP_SETLOCAL: u8 = 0x63;
const OP_GETLOCAL: u8 = 0x62;
const OP_CALLPROPERTY: u8 = 0x46;
const OP_CONVERT_S: u8 = 0x70;
const OP_CONVERT_I: u8 = 0x73;
const OP_RETURNVOID: u8 = 0x47;
// branches + stack/value ops for the reflection dispatcher
const OP_JUMP: u8 = 0x10;
#[allow(dead_code)] const OP_IFTRUE: u8 = 0x11;
const OP_IFFALSE: u8 = 0x12;
const OP_PUSHFALSE: u8 = 0x27;
const OP_IFSTRICTEQ: u8 = 0x19;
#[allow(dead_code)] const OP_IFSTRICTNE: u8 = 0x1A;
const OP_DUP: u8 = 0x2A;
const OP_SWAP: u8 = 0x2B;
const OP_POP: u8 = 0x29;
const OP_SETPROPERTY: u8 = 0x61;
#[allow(dead_code)] // kept for completeness of the opcode set
const OP_RETURNVALUE: u8 = 0x48;
const OP_PUSHBYTE: u8 = 0x24;
const OP_PUSHNULL: u8 = 0x20;
#[allow(dead_code)] const OP_GETGLOBALSCOPE: u8 = 0x64;
const OP_ADD: u8 = 0xA0;
const OP_CONVERT_D: u8 = 0x75;
const OP_PUSHTRUE: u8 = 0x26;
const OP_NEWARRAY: u8 = 0x56;
const OP_NEWOBJECT: u8 = 0x55;
const OP_PUSHSHORT: u8 = 0x25;
/// `istypelate`: pops (value, type) and pushes whether value is an instance of type. Used
/// to guard MovieClip-only / container-only properties in the NODE verb — reading
/// `currentFrame` off a Bitmap throws (display classes are sealed), which would abort the
/// whole command through the dispatcher's try and answer "ERR:" for an otherwise fine node.
const OP_ISTYPELATE: u8 = 0xB3;
const OP_IFGE: u8 = 0x18;

// AVM2 namespace kinds
const NS_PACKAGE: u8 = 0x16; // CONSTANT_PackageNamespace

// ── SSF2 engine namespaces (single source of truth) ──
// Every injected call into the live engine resolves its symbols BY NAME against one
// of these package namespaces, so a recompile of SSF2 survives as long as the symbol
// still exists. If SSF2 is ever repackaged, this is the one place the package paths
// change; `ssf2_manifest()` + `verify_symbols()` then flag any class/member that moved
// (run via `peptide ssf2 doctor` and the patch preflight) instead of silently breaking.
pub const SSF2_PKG: &str = "com.mcleodgaming.ssf2";              // Main (document class)
pub const SSF2_NS_CONTROLLERS: &str = "com.mcleodgaming.ssf2.controllers"; // GameController, Game, MenuController, PlayerSetting
pub const SSF2_NS_ENGINE: &str = "com.mcleodgaming.ssf2.engine"; // Character, StageData, AttackData/Object, Stats
pub const SSF2_NS_UTIL: &str = "com.mcleodgaming.ssf2.util";     // Controller, ControlsObject, ResourceManager
pub const SSF2_NS_ENUMS: &str = "com.mcleodgaming.ssf2.enums";   // Mode

/// A tiny opcode emitter with the AVM2 u30 var-encoding + label/branch fixups.
#[derive(Default)]
struct Code {
    b: Vec<u8>,
    /// label id -> byte position (None until placed).
    labels: Vec<Option<usize>>,
    /// (position of the 3 s24 offset bytes, target label id).
    fixups: Vec<(usize, usize)>,
}
impl Code {
    fn op(&mut self, o: u8) { self.b.push(o); }
    fn u30(&mut self, mut v: u32) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 { byte |= 0x80; }
            self.b.push(byte);
            if v == 0 { break; }
        }
    }
    fn op_u30(&mut self, o: u8, a: u32) { self.op(o); self.u30(a); }
    fn op_u30_u30(&mut self, o: u8, a: u32, c: u32) { self.op(o); self.u30(a); self.u30(c); }

    /// Allocate a fresh label id.
    fn new_label(&mut self) -> usize { self.labels.push(None); self.labels.len() - 1 }
    /// Mark the current position as label `l`'s target.
    fn place(&mut self, l: usize) { self.labels[l] = Some(self.b.len()); }
    /// Byte position of a placed label (for exception-table from/to/target offsets).
    fn pos(&self, l: usize) -> u32 { self.labels[l].expect("unplaced label") as u32 }
    /// Emit a branch opcode `op` targeting label `l`. AVM2 s24 offset is relative
    /// to the byte AFTER the 3 offset bytes. Records a fixup resolved by `finish`.
    fn branch(&mut self, op: u8, l: usize) {
        self.b.push(op);
        let off_pos = self.b.len();
        self.fixups.push((off_pos, l));
        self.b.extend_from_slice(&[0, 0, 0]); // placeholder s24
    }
    /// Resolve all branch fixups; returns the finished bytecode.
    fn finish(mut self) -> Vec<u8> {
        for (off_pos, l) in &self.fixups {
            let target = self.labels[*l].expect("unplaced label");
            let next = off_pos + 3; // s24 is relative to the instruction AFTER the offset
            let delta = target as i64 - next as i64;
            let bytes = (delta as i32).to_le_bytes();
            self.b[*off_pos] = bytes[0];
            self.b[*off_pos + 1] = bytes[1];
            self.b[*off_pos + 2] = bytes[2];
        }
        self.b
    }
}

/// Prepend `payload` to a method body: splice it before the existing code and shift the
/// absolute exception-table offsets by the payload length. AVM2 branches are relative
/// (shift-invariant), so only the exception from/to/target need fixing. This is the one
/// home for the "inject at method entry" mechanics every `inject_*` helper relies on;
/// callers still set their own frame requirements (max_stack / max_scope_depth /
/// local_count) since those are payload-specific.
fn prepend_code(body: &mut MethodBody, payload: &[u8]) {
    let n = payload.len() as u32;
    let mut spliced = payload.to_vec();
    spliced.extend_from_slice(&body.code);
    body.code = spliced;
    // shift the absolute exception-table offsets by the prepend length (relative
    // branches are shift-invariant, so only from/to/target need fixing).
    for e in &mut body.exceptions { e.from += n; e.to += n; e.target += n; }
}

// ───────────────────────────── symbol manifest ─────────────────────────────
// The SSF2 analogue of src/manifest.rs (the Fraymakers side): the single table of
// every engine symbol the injected bridge + verbs depend on. Each is resolved BY NAME
// against the loaded SSF2 ABC, so a recompile of SSF2 survives as long as the symbol
// still exists. `verify_symbols` runs at the top of every patch (and `peptide ssf2
// doctor`): if a class or member has moved, the patch ABORTS with the missing symbol
// named, instead of producing a SWF whose injected calls fail silently at runtime.
//
// Any NEW SSF2 symbol an injection comes to depend on MUST be listed here.

/// One SSF2 engine dependency: a class by local name, optionally a member on it.
pub struct Ssf2Symbol {
    /// Instance class local name (matched by `find_class_by_name`).
    pub class: &'static str,
    /// A method/getter/setter/slot the class must declare, or `None` for "class exists".
    pub member: Option<&'static str>,
    /// Subsystem, for grouping the doctor checklist.
    pub group: &'static str,
    /// Why we depend on it (shown when missing).
    pub why: &'static str,
    /// `true` → the patch can't function without it (a miss aborts); `false` → a loud
    /// warning but the patch proceeds (graceful-degradation site).
    pub critical: bool,
}

macro_rules! ssf2_sym {
    ($class:literal $(. $member:literal)?, $group:literal, $why:literal, $crit:literal) => {
        Ssf2Symbol { class: $class, member: ssf2_sym!(@m $($member)?), group: $group, why: $why, critical: $crit }
    };
    (@m $member:literal) => { Some($member) };
    (@m) => { None };
}

/// Every SSF2 class/member the injected bridge + command verbs resolve at runtime.
/// Grouped by the feature that needs them; keep in step with the `inject_*` helpers
/// and the host-side `Ssf2Target` verbs.
pub const SSF2_MANIFEST: &[Ssf2Symbol] = &[
    // ── document + boot hooks (resolved by name at patch time too) ──
    ssf2_sym!("Main",                              "boot", "document class: socket bridge + listeners attach to its ctor", true),
    ssf2_sym!("DisclaimerMenu"."checkDisclaimer",  "boot", "event-driven READY hook (boot-complete signal)", true),
    ssf2_sym!("MenuController"."showInitialMenu",  "boot", "quick-boot: rewritten to queue the match + skip menus", true),
    // ── socket bridge / match launch ──
    ssf2_sym!("GameController"."stageData",         "bridge", "the live match graph root (characters live under it)", true),
    ssf2_sym!("GameController"."currentGame",       "bridge", "the Game the spawn verbs build + startMatch reads", true),
    ssf2_sym!("GameController"."startMatch",        "bridge", "start the programmatically-built match", true),
    ssf2_sym!("MenuController"."disposeAllMenus",   "bridge", "tear menus down after a programmatic startMatch", true),
    ssf2_sym!("Game",                               "bridge", "constructed (N player slots) by the SPAWN verb", true),
    ssf2_sym!("Mode",                               "bridge", "Mode.VERSUS for the built Game", true),
    ssf2_sym!("ResourceManager"."queueResources",   "bridge", "queue stage + character resources to load", true),
    ssf2_sym!("ResourceManager"."load",             "bridge", "drive the in-context async resource load", true),
    ssf2_sym!("PlayerSetting"."character",          "bridge", "per-slot character id the match builds from", true),
    ssf2_sym!("Stats"."getStats",                   "bridge", "CharacterData lookup (makePlayer/ctor read it)", true),
    // ── live match graph (reflection bridge navigation) ──
    ssf2_sym!("StageData"."Characters",             "reflect", "per-player character nodes (p0..p3 resolve through it)", true),
    ssf2_sym!("Character"."getDamage",              "reflect", "matchStatus + dmg/info read the damage percent", false),
    // ── input injection (hold/seq/scenario) ──
    ssf2_sym!("Controller"."getControlStatus",      "input", "per-frame input read we prepend the applicator to", true),
    ssf2_sym!("Character"."ControlSettings",        "input", "public getter to the player's Controller (targeting)", true),
    ssf2_sym!("ControlsObject"."controls",          "input", "the control bitmask the applicator writes per frame", true),
    ssf2_sym!("Character"."setState",               "input", "scenario's toState(STAND) lowering", false),
    // ── addCharacter (live add into a reserved slot) ──
    ssf2_sym!("StageData"."activateCharacters",     "addchar", "re-activate after constructing the added Character", true),
    ssf2_sym!("StageData"."deactivateCharacters",   "addchar", "bracket the construct like makePlayer does", true),
    ssf2_sym!("Character",                           "addchar", "constructed directly (its ctor self-registers)", true),
    ssf2_sym!("PlayerSetting"."exist",              "addchar", "reserved-slot flag flipped true when filled", true),
    // ── tune (mutate a move's stored hitbox stats) ──
    ssf2_sym!("Character"."AttackDataObj",          "tune", "protected getter to the AttackData (move table)", true),
    ssf2_sym!("AttackData"."getAttack",             "tune", "look up a move's AttackObject by name", true),
    ssf2_sym!("AttackObject"."AttackBoxes",         "tune", "the move's stored attack boxes (string-keyed)", true),
    ssf2_sym!("AttackDamage"."Damage",              "tune", "a stored box's mutable stat setter (Damage/KBConstant/…)", true),
];

/// Does class index `ci` (or any of its superclasses) declare a trait named `member`
/// (method/getter/setter/slot)? Walks the super chain so inherited members resolve
/// (e.g. `AttackDataObj`/`getDamage` live on `InteractiveSprite`, the parent of
/// `Character`). Stops at the first super that isn't a user class (a builtin like
/// Object/Sprite), and guards against cycles.
fn class_has_member(abc: &Abc, ci: usize, member: &str) -> bool {
    let named = |abc: &Abc, c: usize| {
        let m = |ts: &[Trait]| ts.iter().any(|t| abc.multiname_local(t.name).as_deref() == Some(member));
        m(&abc.instances[c].traits) || m(&abc.classes[c].traits)
    };
    let mut cur = Some(ci);
    for _ in 0..32 { // depth guard
        let Some(c) = cur else { return false };
        if named(abc, c) { return true; }
        cur = abc.multiname_local(abc.instances[c].super_name)
            .and_then(|s| abc.find_class_by_name(&s));
    }
    false
}

/// Resolve each manifest symbol against `abc`, returning `(symbol, resolved?)` for the
/// doctor checklist and the patch preflight.
pub fn verify_symbols(abc: &Abc) -> Vec<(&'static Ssf2Symbol, bool)> {
    SSF2_MANIFEST.iter().map(|s| {
        let ok = match abc.find_class_by_name(s.class) {
            None => false,
            Some(ci) => s.member.map(|m| class_has_member(abc, ci, m)).unwrap_or(true),
        };
        (s, ok)
    }).collect()
}

/// Patch preflight: abort if any CRITICAL manifest symbol is missing, naming what moved
/// (and why) so a SSF2 layout change fails loudly instead of corrupting the patch.
pub fn preflight(abc: &Abc) -> anyhow::Result<()> {
    let missing: Vec<String> = verify_symbols(abc).into_iter()
        .filter(|(s, ok)| s.critical && !ok)
        .map(|(s, _)| match s.member {
            Some(m) => format!("  {}.{} — {}", s.class, m, s.why),
            None => format!("  {} — {}", s.class, s.why),
        })
        .collect();
    if missing.is_empty() { return Ok(()); }
    anyhow::bail!(
        "SSF2 symbol preflight failed — {} critical engine symbol(s) moved or were renamed \
         (SSF2 likely updated). Peptide resolves these by name; update SSF2_MANIFEST + the \
         inject_* helpers to the new names:\n{}",
        missing.len(), missing.join("\n")
    )
}

/// Resolved multiname indices for the AIR filesystem API we call.
struct FsApi {
    file: u32,        // QName(flash.filesystem, "File")
    file_stream: u32, // QName(flash.filesystem, "FileStream")
    file_mode: u32,   // QName(flash.filesystem, "FileMode")
    write_prop: u32,  // QName(public, "WRITE")
    open: u32,        // QName(public, "open")
    write_utf: u32,   // QName(public, "writeUTFBytes")
    close: u32,       // QName(public, "close")
    write_bytes: u32, // QName(public, "writeBytes") — binary payloads (a captured frame)
    bitmap_data: u32, // QName(flash.display, "BitmapData")
    png_opts: u32,    // QName(flash.display, "PNGEncoderOptions")
    rectangle: u32,   // QName(flash.geom, "Rectangle")
    draw: u32,        // QName(public, "draw")
    encode: u32,      // QName(public, "encode")
    transform: u32,   // QName(public, "transform")
    matrix: u32,      // QName(public, "matrix")
}

fn intern_fs_api(abc: &mut Abc) -> FsApi {
    let fs_ns_name = abc.intern_string("flash.filesystem");
    let fs_ns = abc.intern_namespace(NS_PACKAGE, fs_ns_name);
    let pub_ns_name = abc.intern_string("");
    let pub_ns = abc.intern_namespace(NS_PACKAGE, pub_ns_name);
    let n_file = abc.intern_string("File");
    let disp_ns_name = abc.intern_string("flash.display");
    let disp_ns = abc.intern_namespace(NS_PACKAGE, disp_ns_name);
    let geom_ns_name = abc.intern_string("flash.geom");
    let geom_ns = abc.intern_namespace(NS_PACKAGE, geom_ns_name);
    let n_fs = abc.intern_string("FileStream");
    let n_fm = abc.intern_string("FileMode");
    let n_write = abc.intern_string("WRITE");
    let n_open = abc.intern_string("open");
    let n_wub = abc.intern_string("writeUTFBytes");
    let n_close = abc.intern_string("close");
    let n_wb = abc.intern_string("writeBytes");
    let n_bd = abc.intern_string("BitmapData");
    let n_po = abc.intern_string("PNGEncoderOptions");
    let n_rect = abc.intern_string("Rectangle");
    let n_draw = abc.intern_string("draw");
    let n_enc = abc.intern_string("encode");
    let n_tr = abc.intern_string("transform");
    let n_mx = abc.intern_string("matrix");
    FsApi {
        file: abc.intern_qname(fs_ns, n_file),
        file_stream: abc.intern_qname(fs_ns, n_fs),
        file_mode: abc.intern_qname(fs_ns, n_fm),
        write_prop: abc.intern_qname(pub_ns, n_write),
        open: abc.intern_qname(pub_ns, n_open),
        write_utf: abc.intern_qname(pub_ns, n_wub),
        close: abc.intern_qname(pub_ns, n_close),
        write_bytes: abc.intern_qname(pub_ns, n_wb),
        bitmap_data: abc.intern_qname(disp_ns, n_bd),
        png_opts: abc.intern_qname(disp_ns, n_po),
        rectangle: abc.intern_qname(geom_ns, n_rect),
        draw: abc.intern_qname(pub_ns, n_draw),
        encode: abc.intern_qname(pub_ns, n_enc),
        transform: abc.intern_qname(pub_ns, n_tr),
        matrix: abc.intern_qname(pub_ns, n_mx),
    }
}

/// Emit the payload:
///   var f = new File(<native_path>);
///   var fs = new FileStream();
///   fs.open(f, FileMode.WRITE);
///   fs.writeUTFBytes(<content>);
///   fs.close();
/// using two scratch locals (l_file, l_fs). Wrapped in getlocal0/pushscope ..
/// popscope so global names resolve when prepended to the method entry (before
/// the method's own pushscope).
fn emit_marker_payload(abc: &mut Abc, native_path: &str, content: &str, l_file: u32, l_fs: u32) -> Vec<u8> {
    let api = intern_fs_api(abc);
    let s_path = abc.intern_string(native_path);
    let s_content = abc.intern_string(content);

    let mut c = Code::default();
    // establish a scope so findpropstrict/getlex resolve top-level definitions
    c.op(OP_GETLOCAL0);
    c.op(OP_PUSHSCOPE);
    // f = new File(path)
    c.op_u30(OP_FINDPROPSTRICT, api.file);
    c.op_u30(OP_PUSHSTRING, s_path);
    c.op_u30_u30(OP_CONSTRUCTPROP, api.file, 1);
    c.op_u30(OP_SETLOCAL, l_file);
    // fs = new FileStream()
    c.op_u30(OP_FINDPROPSTRICT, api.file_stream);
    c.op_u30_u30(OP_CONSTRUCTPROP, api.file_stream, 0);
    c.op_u30(OP_SETLOCAL, l_fs);
    // fs.open(f, FileMode.WRITE)
    c.op_u30(OP_GETLOCAL, l_fs);
    c.op_u30(OP_GETLOCAL, l_file);
    c.op_u30(OP_GETLEX, api.file_mode);
    c.op_u30(OP_GETPROPERTY, api.write_prop);
    c.op_u30_u30(OP_CALLPROPVOID, api.open, 2);
    // fs.writeUTFBytes(content)
    c.op_u30(OP_GETLOCAL, l_fs);
    c.op_u30(OP_PUSHSTRING, s_content);
    c.op_u30_u30(OP_CALLPROPVOID, api.write_utf, 1);
    // fs.close()
    c.op_u30(OP_GETLOCAL, l_fs);
    c.op_u30_u30(OP_CALLPROPVOID, api.close, 0);
    // restore scope
    c.op(OP_POPSCOPE);
    c.b
}

/// Inject a "write marker file on startup" payload into the document class
/// constructor (`Main`). Returns Ok(()) on success. This is the smoke test that
/// proves injected AVM2 code runs in the live engine.
pub fn inject_startup_marker(abc: &mut Abc, doc_class_local: &str, native_path: &str, content: &str) -> anyhow::Result<()> {
    // Locate the document class and its constructor (iinit) method body.
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;
    let iinit = abc.instances[ci].iinit;

    // scratch locals at the end of the current frame
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit)
        .ok_or_else(|| anyhow::anyhow!("no method body for {doc_class_local} iinit (method {iinit})"))?;
    let l_file = abc.bodies[body_idx].local_count;
    let l_fs = l_file + 1;

    let payload = emit_marker_payload(abc, native_path, content, l_file, l_fs);

    let body = &mut abc.bodies[body_idx];
    // prepend payload
    prepend_code(body, &payload);
    // bump frame requirements
    body.local_count = l_fs + 1;
    body.max_stack = body.max_stack.max(4);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    // fix absolute exception offsets (relative branches are shift-invariant)
    Ok(())
}

/// Inject a per-frame heartbeat: add a handler method that writes the current
/// `getTimer()` value to `native_path` every frame, and install it as an
/// ENTER_FRAME listener on the document object from its constructor. Proves
/// repeated (per-frame) injected execution — the transport linchpin for the
/// full command loop. Returns Ok(()) on success.
pub fn inject_enterframe_heartbeat(abc: &mut Abc, doc_class_local: &str, native_path: &str) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;

    // ── multinames we need ──
    let fs_ns_name = abc.intern_string("flash.filesystem");
    let fs_ns = abc.intern_namespace(NS_PACKAGE, fs_ns_name);
    let utils_ns_name = abc.intern_string("flash.utils");
    let utils_ns = abc.intern_namespace(NS_PACKAGE, utils_ns_name);
    let events_ns_name = abc.intern_string("flash.events");
    let events_ns = abc.intern_namespace(NS_PACKAGE, events_ns_name);
    let pub_ns_name = abc.intern_string("");
    let pub_ns = abc.intern_namespace(NS_PACKAGE, pub_ns_name);

    let n_file = abc.intern_string("File"); let mn_file = abc.intern_qname(fs_ns, n_file);
    let n_fstream = abc.intern_string("FileStream"); let mn_fstream = abc.intern_qname(fs_ns, n_fstream);
    let n_fmode = abc.intern_string("FileMode"); let mn_fmode = abc.intern_qname(fs_ns, n_fmode);
    let n_write = abc.intern_string("WRITE"); let mn_write = abc.intern_qname(pub_ns, n_write);
    let n_open = abc.intern_string("open"); let mn_open = abc.intern_qname(pub_ns, n_open);
    let n_wub = abc.intern_string("writeUTFBytes"); let mn_wub = abc.intern_qname(pub_ns, n_wub);
    let n_close = abc.intern_string("close"); let mn_close = abc.intern_qname(pub_ns, n_close);
    let n_gettimer = abc.intern_string("getTimer"); let mn_gettimer = abc.intern_qname(utils_ns, n_gettimer);
    let n_event = abc.intern_string("Event"); let mn_event = abc.intern_qname(events_ns, n_event);
    let n_enterframe = abc.intern_string("ENTER_FRAME"); let mn_enterframe = abc.intern_qname(pub_ns, n_enterframe);
    let n_addel = abc.intern_string("addEventListener"); let mn_addel = abc.intern_qname(pub_ns, n_addel);
    let n_tick = abc.intern_string("peptideBridgeTick"); let mn_tick = abc.intern_qname(pub_ns, n_tick);
    let s_path = abc.intern_string(native_path);

    // ── the handler method body (this=local0, event=local1, file=local2, fs=local3) ──
    let (l_file, l_fs) = (2u32, 3u32);
    let mut h = Code::default();
    h.op(OP_GETLOCAL0); h.op(OP_PUSHSCOPE);
    h.op_u30(OP_FINDPROPSTRICT, mn_file); h.op_u30(OP_PUSHSTRING, s_path); h.op_u30_u30(OP_CONSTRUCTPROP, mn_file, 1); h.op_u30(OP_SETLOCAL, l_file);
    h.op_u30(OP_FINDPROPSTRICT, mn_fstream); h.op_u30_u30(OP_CONSTRUCTPROP, mn_fstream, 0); h.op_u30(OP_SETLOCAL, l_fs);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30(OP_GETLOCAL, l_file); h.op_u30(OP_GETLEX, mn_fmode); h.op_u30(OP_GETPROPERTY, mn_write); h.op_u30_u30(OP_CALLPROPVOID, mn_open, 2);
    h.op_u30(OP_GETLOCAL, l_fs);
    // String(getTimer())
    h.op_u30(OP_FINDPROPSTRICT, mn_gettimer); h.op_u30_u30(OP_CALLPROPERTY, mn_gettimer, 0); h.op(OP_CONVERT_S);
    h.op_u30_u30(OP_CALLPROPVOID, mn_wub, 1);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30_u30(OP_CALLPROPVOID, mn_close, 0);
    h.op(OP_RETURNVOID);

    let handler_method = abc.add_method(MethodInfo {
        param_types: vec![0],   // (event:*)
        return_type: 0,         // :void/*
        name: n_tick,
        flags: 0,
        options: Vec::new(),
        param_names: Vec::new(),
    });
    abc.add_body(MethodBody {
        method: handler_method,
        max_stack: 4,
        local_count: 4,
        init_scope_depth: 0,
        max_scope_depth: 2,
        code: h.b,
        exceptions: Vec::new(),
        traits: Vec::new(),
    });
    abc.add_instance_method_trait(ci, mn_tick, handler_method);

    // ── install listener: prepend to the constructor ──
    //   this.addEventListener(Event.ENTER_FRAME, this.peptideBridgeTick)
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    c.op(OP_GETLOCAL0);                       // receiver = this
    c.op_u30(OP_GETLEX, mn_event); c.op_u30(OP_GETPROPERTY, mn_enterframe); // Event.ENTER_FRAME
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_tick);                  // this.peptideBridgeTick (bound)
    c.op_u30_u30(OP_CALLPROPVOID, mn_addel, 2);
    c.op(OP_POPSCOPE);
    let payload = c.b;

    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit)
        .ok_or_else(|| anyhow::anyhow!("no ctor body for {doc_class_local}"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(3);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Inject a per-frame command channel: each frame the handler reads `cmd_path`
/// (host writes commands there; the host guarantees the file exists so no branch
/// is needed) and writes the bytes back to `resp_path`. This proves the
/// bidirectional host→engine→host transport — the foundation of the command
/// loop. (Expanding past echo to reflection dispatch is added vocabulary on the
/// same mechanism.)
pub fn inject_command_channel(abc: &mut Abc, doc_class_local: &str, cmd_path: &str, resp_path: &str) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;

    let fs_ns = { let s = abc.intern_string("flash.filesystem"); abc.intern_namespace(NS_PACKAGE, s) };
    let events_ns = { let s = abc.intern_string("flash.events"); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    let mn_file = q(abc, fs_ns, "File");
    let mn_fstream = q(abc, fs_ns, "FileStream");
    let mn_fmode = q(abc, fs_ns, "FileMode");
    let mn_read = q(abc, pub_ns, "READ");
    let mn_write = q(abc, pub_ns, "WRITE");
    let mn_open = q(abc, pub_ns, "open");
    let mn_close = q(abc, pub_ns, "close");
    let mn_bytesavail = q(abc, pub_ns, "bytesAvailable");
    let mn_readutf = q(abc, pub_ns, "readUTFBytes");
    let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_event = q(abc, events_ns, "Event");
    let mn_enterframe = q(abc, pub_ns, "ENTER_FRAME");
    let mn_addel = q(abc, pub_ns, "addEventListener");
    let mn_tick = q(abc, pub_ns, "peptideBridgeTick");
    let n_tick = abc.intern_string("peptideBridgeTick");
    let s_cmd = abc.intern_string(cmd_path);
    let s_resp = abc.intern_string(resp_path);

    // handler frame: this=0, event=1, file=2, fs=3, str=4
    let (l_file, l_fs, l_str) = (2u32, 3u32, 4u32);
    let mut h = Code::default();
    h.op(OP_GETLOCAL0); h.op(OP_PUSHSCOPE);
    // read: f=new File(cmd); fs=new FileStream(); fs.open(f, READ);
    h.op_u30(OP_FINDPROPSTRICT, mn_file); h.op_u30(OP_PUSHSTRING, s_cmd); h.op_u30_u30(OP_CONSTRUCTPROP, mn_file, 1); h.op_u30(OP_SETLOCAL, l_file);
    h.op_u30(OP_FINDPROPSTRICT, mn_fstream); h.op_u30_u30(OP_CONSTRUCTPROP, mn_fstream, 0); h.op_u30(OP_SETLOCAL, l_fs);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30(OP_GETLOCAL, l_file); h.op_u30(OP_GETLEX, mn_fmode); h.op_u30(OP_GETPROPERTY, mn_read); h.op_u30_u30(OP_CALLPROPVOID, mn_open, 2);
    // str = fs.readUTFBytes(fs.bytesAvailable)
    h.op_u30(OP_GETLOCAL, l_fs);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30(OP_GETPROPERTY, mn_bytesavail);
    h.op_u30_u30(OP_CALLPROPERTY, mn_readutf, 1); h.op_u30(OP_SETLOCAL, l_str);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30_u30(OP_CALLPROPVOID, mn_close, 0);
    // write: f=new File(resp); fs=new FileStream(); fs.open(f, WRITE); fs.writeUTFBytes(str); fs.close()
    h.op_u30(OP_FINDPROPSTRICT, mn_file); h.op_u30(OP_PUSHSTRING, s_resp); h.op_u30_u30(OP_CONSTRUCTPROP, mn_file, 1); h.op_u30(OP_SETLOCAL, l_file);
    h.op_u30(OP_FINDPROPSTRICT, mn_fstream); h.op_u30_u30(OP_CONSTRUCTPROP, mn_fstream, 0); h.op_u30(OP_SETLOCAL, l_fs);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30(OP_GETLOCAL, l_file); h.op_u30(OP_GETLEX, mn_fmode); h.op_u30(OP_GETPROPERTY, mn_write); h.op_u30_u30(OP_CALLPROPVOID, mn_open, 2);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30(OP_GETLOCAL, l_str); h.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    h.op_u30(OP_GETLOCAL, l_fs); h.op_u30_u30(OP_CALLPROPVOID, mn_close, 0);
    h.op(OP_RETURNVOID);

    let handler = abc.add_method(MethodInfo { param_types: vec![0], return_type: 0, name: n_tick, flags: 0, options: vec![], param_names: vec![] });
    abc.add_body(MethodBody { method: handler, max_stack: 4, local_count: 5, init_scope_depth: 0, max_scope_depth: 2, code: h.b, exceptions: vec![], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_tick, handler);

    // install listener in ctor
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    c.op(OP_GETLOCAL0);
    c.op_u30(OP_GETLEX, mn_event); c.op_u30(OP_GETPROPERTY, mn_enterframe);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_tick);
    c.op_u30_u30(OP_CALLPROPVOID, mn_addel, 2);
    c.op(OP_POPSCOPE);
    let payload = c.b;
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit).ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(3);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Inject the full reflection dispatcher over an ASYNC TCP SOCKET (flash.net.Socket)
/// — the AVM2 transport that mirrors the Fraymakers loopback socket. The added
/// `peptideOnData` handler fires on `socketData` events (NOT every frame), reads one
/// "<seq>\t<verb>\t<a1>\t<a2>" command, acts via reflection on a persistent
/// `peptideCur` register, and writes "<seq> <result>\n" back over the socket:
///   PING            -> "pong"
///   ROOT            -> cur = this (the document); "ok"
///   GET   <name>    -> cur = cur[name]            (property by runtime name); "ok"
///   IDX   <n>       -> cur = cur[Number(n)]       (array/index access); "ok"
///   CALL  <name>    -> cur = cur.name()           (0-arg method, e.g. getX); "ok"
///   CALL1 <name> <n>-> cur = cur.name(Number(n))  (1-arg, e.g. setYSpeed); "ok"
///   READ            -> String(cur)
/// Event-driven IO is the whole point: the handler runs ONLY when the host sends a
/// command, so it never does per-frame blocking IO and therefore never starves
/// SSF2's async resource loader (the flaw of the old per-frame FileStream bridge).
/// That also removes the need for the execute-once guard. The engine dials into the
/// host's loopback server at `host:port` from the document ctor.
/// Find an existing `QName` multiname in the pool by local name, preferring one whose
/// qualified name mentions `prefer_owner` — so a protected-namespace member is reused by
/// exact (ns, name) match (runtime callproperty resolves by exact match) rather than
/// forged public.
fn find_qname_by_local_owned(abc: &Abc, local: &str, prefer_owner: &str) -> Option<u32> {
    let mut fallback = None;
    for (i, mn) in abc.multinames.iter().enumerate() {
        let idx = i as u32 + 1;
        if matches!(mn, Multiname::QName { .. }) && abc.multiname_local(idx).as_deref() == Some(local) {
            if abc.multiname_qualified(idx).map(|q| q.contains(prefer_owner)).unwrap_or(false) {
                return Some(idx);
            }
            fallback.get_or_insert(idx);
        }
    }
    fallback
}

pub fn inject_socket_bridge(abc: &mut Abc, doc_class_local: &str, host: &str, port: u16) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;

    // namespaces
    let net_ns = { let s = abc.intern_string("flash.net"); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    // flash.net.Socket — async TCP. read/writeUTFBytes/flush/bytesAvailable/connect/addEventListener.
    let mn_socket = q(abc, net_ns, "Socket");
    let mn_bytesavail = q(abc, pub_ns, "bytesAvailable");
    let mn_readutf = q(abc, pub_ns, "readUTFBytes");
    let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_flush = q(abc, pub_ns, "flush");
    let mn_connect = q(abc, pub_ns, "connect");
    let mn_addel = q(abc, pub_ns, "addEventListener");
    let mn_split = q(abc, pub_ns, "split");
    let mn_cur = q(abc, pub_ns, "peptideCur");
    let mn_sock = q(abc, pub_ns, "peptideSock");
    let mn_ondata = q(abc, pub_ns, "peptideOnData");
    let n_ondata = abc.intern_string("peptideOnData");
    // GameController static singleton (reaches the live match / characters)
    let ctrl_ns = { let s = abc.intern_string(SSF2_NS_CONTROLLERS); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_gc = q(abc, ctrl_ns, "GameController");
    let s_v_gc = abc.intern_string("GC");
    let s_v_setp = abc.intern_string("SETP");
    // SPAWN: build Game(1, Mode.TRAINING) + set stage/char + GameController.startMatch
    let enums_ns = { let s = abc.intern_string(SSF2_NS_ENUMS); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_game = q(abc, ctrl_ns, "Game");
    let mn_mode = q(abc, enums_ns, "Mode");
    let mn_versus = q(abc, pub_ns, "VERSUS");
    let mn_startmatch = q(abc, pub_ns, "startMatch");
    let mn_leveldata = q(abc, pub_ns, "LevelData");
    let mn_stage = q(abc, pub_ns, "stage");
    let mn_playersettings = q(abc, pub_ns, "PlayerSettings");
    let mn_character = q(abc, pub_ns, "character");
    let mn_human = q(abc, pub_ns, "human");
    let mn_costume = q(abc, pub_ns, "costume");
    let s_v_spawn = abc.intern_string("SPAWN");
    let s_spawned = abc.intern_string("spawned");
    // resource preload verbs (QUEUE ids, LOADNEXT kicks the async loader, LOADED reports done)
    let util_ns = { let s = abc.intern_string(SSF2_NS_UTIL); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_rm = q(abc, util_ns, "ResourceManager");
    let mn_queueresources = q(abc, pub_ns, "queueResources");
    let mn_loadnext = q(abc, pub_ns, "loadNext");
    let mn_isfullyloaded = q(abc, pub_ns, "isFullyLoaded");
    let s_v_queue = abc.intern_string("QUEUE");
    // multiplayer host-orchestration primitives (no in-bytecode loop): set a STRING
    // property (SETP coerces to Number), queue ONE resource id, and (re)load the queue.
    let s_v_sets = abc.intern_string("SETS");
    let s_v_queue1 = abc.intern_string("QUEUE1");
    let s_v_mload = abc.intern_string("MLOAD");
    let s_v_loadnext = abc.intern_string("LOADNEXT");
    let s_v_loaded = abc.intern_string("LOADED");
    let mn_load = q(abc, pub_ns, "load");
    let mn_flushqueue = q(abc, pub_ns, "flushLoadQueue");
    let mn_currentgame = q(abc, pub_ns, "currentGame");
    let s_v_go = abc.intern_string("GO");
    let s_queued = abc.intern_string("queued");
    let s_v_calls = abc.intern_string("CALLS");
    let s_multimode = abc.intern_string("multimode");
    // RM/STATS root verbs (cur = the static class) for probing the resource pool / stats
    let engine_ns = { let s = abc.intern_string(SSF2_NS_ENGINE); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_stats_real = q(abc, engine_ns, "Stats");
    let s_v_rm = abc.intern_string("RM");
    let s_v_stats = abc.intern_string("STATS");
    // MenuController (static) — owns the menu/intro screens; disposeAllMenus() tears
    // them down so a programmatic startMatch isn't left behind the menus. `MC` root
    // verb = getlex MenuController (parity with GC/RM/STATS).
    let mn_menuctrl = q(abc, ctrl_ns, "MenuController");
    let mn_disposemenus = q(abc, pub_ns, "disposeAllMenus");
    let s_v_mc = abc.intern_string("MC");
    // runtime-named multiname (MultinameL) bound to the public ns-set
    let pub_nsset = abc.intern_ns_set(vec![pub_ns]);
    let mnl = abc.intern_multinamel(pub_nsset);
    // string constants
    let s_socketdata = abc.intern_string("socketData"); // ProgressEvent.SOCKET_DATA type
    let s_host = abc.intern_string(host);
    let s_port = abc.intern_string(&port.to_string()); // connect() coerces the String to int
    let s_nl = abc.intern_string("\n");                 // reply framing (host reads to '\n')
    let s_tab = abc.intern_string("\t");
    let s_sp = abc.intern_string(" ");
    let s_q = abc.intern_string("?");
    let s_pong = abc.intern_string("pong");
    let s_ok = abc.intern_string("ok");
    let s_v_ping = abc.intern_string("PING");
    let s_v_root = abc.intern_string("ROOT");
    let s_v_get = abc.intern_string("GET");
    let s_v_idx = abc.intern_string("IDX");
    let s_v_call = abc.intern_string("CALL");
    let s_v_call1 = abc.intern_string("CALL1");
    let s_v_read = abc.intern_string("READ");
    // NODE: every property of the CURRENT node in ONE reply, pipe-joined:
    //   <class>|<name>|<x>|<y>|<w>|<h>|<visible>|<numChildren>|<frame>|<total>|<label>
    // The host's tree walk read ten properties per node, and because a GET moves the
    // cursor it had to re-navigate from ROOT for each one — about sixty round trips per
    // node, which made a stage tree effectively un-runnable. This makes it one.
    let s_v_node = abc.intern_string("NODE");
    let s_bar = abc.intern_string("|");
    let s_dash = abc.intern_string("-1");
    let s_empty2 = abc.intern_string("");
    let disp_ns = { let ss = abc.intern_string("flash.display"); abc.intern_namespace(NS_PACKAGE, ss) };
    let mn_movieclip = q(abc, disp_ns, "MovieClip");
    let mn_container = q(abc, disp_ns, "DisplayObjectContainer");
    // TREE: the WHOLE subtree in one reply, walked recursively inside the engine.
    //
    // Even with CHILDREN batching the reads, the host still had to move the cursor to each
    // container, so a walk cost a few hundred round trips. Recursion does it in ONE. It also
    // makes the walk ATOMIC: the previous walks took long enough that the live display list
    // shifted underneath them, so the same object could be reported twice (podoboos appeared
    // at 50/82, 6/82 and 64/82 in a single walk) and others missed entirely.
    //
    // Recursion is a METHOD CALL, which is exactly why it works where a loop did not: a
    // backward branch fails AVM2 verification and takes the whole dispatcher down with it
    // (bisected — see the CHILDREN comment). Each record is prefixed with its depth so the
    // host can rebuild the shape without tracking position.
    // getBounds(root) -> the node's WORLD bounding box. A node's own x/y is its registration
    // point, which is not where its art starts, and the two engines pick different origins:
    // Fraymakers rasterises each element into its own image and records that image's top-left.
    // Comparing a registration point against an image corner produces a mapping that fits no
    // element (measured: SSF2 x ascending 381/408/446 against Fraymakers 703/118/241 — not
    // even monotonic). The bounding box is the shared reference both sides can be read in.
    let mn_getbounds = q(abc, pub_ns, "getBounds");
    let s_v_tree = abc.intern_string("TREE");
    let s_v_shot = abc.intern_string("SHOT");
    let s_shot_ok = abc.intern_string("SHOT:ok ");
    let fsapi = intern_fs_api(abc);
    let mn_pwalk = {
        let empty = abc.intern_string("");
        let ns = abc.intern_namespace(NS_PACKAGE, empty);
        q(abc, ns, "peptideWalk")
    };
    // Records are separated by ";;", NOT by a newline: the wire is line-based, so a
    // newline-joined reply is truncated to its first record and the walk silently sees one
    // child per container.
    let s_nl2 = abc.intern_string(";;");
    let mn_getchildat = {
        let empty = abc.intern_string("");
        let ns = abc.intern_namespace(NS_PACKAGE, empty);
        q(abc, ns, "getChildAt")
    };

    let mn_n_name = q(abc, pub_ns, "name");
    let mn_n_x = q(abc, pub_ns, "x");
    let mn_n_y = q(abc, pub_ns, "y");
    let mn_n_w = q(abc, pub_ns, "width");
    let mn_n_h = q(abc, pub_ns, "height");
    let mn_n_vis = q(abc, pub_ns, "visible");
    let mn_n_nch = q(abc, pub_ns, "numChildren");
    let mn_n_cf = q(abc, pub_ns, "currentFrame");
    let mn_n_tf = q(abc, pub_ns, "totalFrames");
    let mn_n_cl = q(abc, pub_ns, "currentLabel");
    // LOG <msg>: command-level parity with commands.hsx `log()` — replies
    // "logged: <msg>" (SSF2 has no on-screen console sink to mirror __td.log, so
    // the reply IS the observable, same as the E:logged:… line Fraymakers returns).
    let s_v_log = abc.intern_string("LOG");
    let s_logged = abc.intern_string("logged: ");
    // error reporting: a thrown command (e.g. a bad reflection path) is caught and the
    // reply becomes "ERR:<exception>" instead of timing out the host silently.
    let s_errpfx = abc.intern_string("ERR:");
    // HOLD <idx> <mask> / SEQ <idx> <csv-masks>: host input injection. These write
    // per-frame state directly onto the TARGET player's Controller (reached via the
    // public Characters[idx].ControlSettings chain); the per-frame applicator
    // (inject_input_applicator) reads it off that same controller. SSF2's controls are
    // frame-paced engine-side (a queued mask list drained one per frame), mirroring how
    // Fraymakers drains one `i` line per frame. State lives on the controller (not the
    // document) because the applicator runs inside Controller and the document singleton
    // Main.ROOT is a non-public static the reflection seam can't reach.
    let s_v_hold = abc.intern_string("HOLD");
    let s_v_seq = abc.intern_string("SEQ");
    let s_comma = abc.intern_string(",");
    // controller-state slots (added to the Controller class by inject_input_applicator;
    // here we just need the public multinames to set them on the target controller)
    let mn_holdmask = q(abc, pub_ns, "peptideHoldMask");   // persistent held mask (release = 0)
    let mn_seqlist = q(abc, pub_ns, "peptideSeq");         // Array of per-frame masks, or null
    let mn_seqidx = q(abc, pub_ns, "peptideSeqIdx");       // next index into peptideSeq
    let mn_active = q(abc, pub_ns, "peptideActive");       // injection on for this controller
    // nav to the target controller: GameController.stageData.Characters[idx].ControlSettings
    let mn_stagedata2 = q(abc, pub_ns, "stageData");
    let mn_characters2 = q(abc, pub_ns, "Characters");
    let mn_controlsettings = q(abc, pub_ns, "ControlSettings");
    // ADDCHAR <char>: live add-player into the running match (build PlayerSetting, push,
    // StageData.makePlayer). makePlayer lives in StageData's protected namespace, so reuse
    // the engine's own multiname from the pool; Vector.push is the AS3 builtin ns.
    let s_v_addchar = abc.intern_string("ADDCHAR");
    let mn_expansion = q(abc, pub_ns, "expansion");
    let mn_team = q(abc, pub_ns, "team");
    let mn_exist = q(abc, pub_ns, "exist");
    // direct Character construction (skips makePlayer's HUD attach, which assumes the
    // start-game per-slot setup). The Character ctor self-registers into the live match
    // via StageData.addPlayer/addCharacter. importData/de/activateCharacters are public
    // (only makePlayer itself is protected).
    let mn_character_class = q(abc, engine_ns, "Character");
    let mn_getstats = q(abc, pub_ns, "getStats");
    let mn_importdata = q(abc, pub_ns, "importData");
    let mn_deactivate = q(abc, pub_ns, "deactivateCharacters");
    let mn_activate = q(abc, pub_ns, "activateCharacters");
    let mn_x_start = q(abc, pub_ns, "x_start");
    let mn_y_start = q(abc, pub_ns, "y_start");
    let s_player_id = abc.intern_string("player_id");
    let s_shieldtype = abc.intern_string("shieldType");
    let s_shield = abc.intern_string("shield");
    let s_stamina = abc.intern_string("stamina");
    // TUNE <move> <accessor> <value>: mutate a stored attack box's stat on the current
    // peptideCur (a Character). cur.AttackDataObj().getAttack(move).AttackBoxes[0] is an
    // AttackDamage with public setters (Damage/KBConstant/Direction/Power); we set the
    // accessor by runtime name. AttackDataObj is protected → reuse the engine's QName.
    let s_v_tune = abc.intern_string("TUNE");
    let mn_attackdataobj = find_qname_by_local_owned(abc, "AttackDataObj", "InteractiveSprite")
        .ok_or_else(|| anyhow::anyhow!("AttackDataObj multiname not found in pool"))?;
    let mn_getattack = q(abc, pub_ns, "getAttack");
    let mn_attackboxes = q(abc, pub_ns, "AttackBoxes");
    let s_attackbox = abc.intern_string("attackBox"); // the default box key (AttackBoxes is string-keyed)

    // add the persistent register + the socket slot to the class
    abc.add_instance_slot(ci, mn_cur);
    abc.add_instance_slot(ci, mn_sock);

    // locals: this=0 event=1 (2,3 unused) cmd=4 arr=5 seq=6 verb=7 a1=8 a2=9 result=10
    //         g=11 ps=12 a3=13 (optional 3rd command arg, e.g. SPAWN's player count)
    let (l_cmd, l_arr, l_seq, l_verb, l_a1, l_a2, l_res, l_a3) = (4u32,5,6,7,8,9,10,13);
    // scratch for SHOT (bitmap, encoded bytes, file, stream)
    let (l_shot_bd, l_shot_ba, l_shot_f, l_shot_fs) = (14u32, 15, 16, 17);
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);

    // ── read one command from the socket: cmd = this.peptideSock.readUTFBytes(bytesAvailable) ──
    // The host writes exactly one command per send and waits for the reply (strict
    // request/response), so one socketData event = one command; no buffering needed.
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock);                                   // socket (receiver)
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock); c.op_u30(OP_GETPROPERTY, mn_bytesavail); // arg = bytesAvailable
    c.op_u30_u30(OP_CALLPROPERTY, mn_readutf, 1); c.op_u30(OP_SETLOCAL, l_cmd);

    // arr = cmd.split("\t")
    c.op_u30(OP_GETLOCAL, l_cmd); c.op_u30(OP_PUSHSTRING, s_tab); c.op_u30_u30(OP_CALLPROPERTY, mn_split, 1); c.op_u30(OP_SETLOCAL, l_arr);
    // seq/verb/a1/a2 = arr[0..3]
    let field = |c: &mut Code, idx: u8, dst: u32| { c.op_u30(OP_GETLOCAL, l_arr); c.op(OP_PUSHBYTE); c.op(idx); c.op_u30(OP_GETPROPERTY, mnl); c.op_u30(OP_SETLOCAL, dst); };
    field(&mut c, 0, l_seq); field(&mut c, 1, l_verb); field(&mut c, 2, l_a1); field(&mut c, 3, l_a2);
    field(&mut c, 4, l_a3); // optional 3rd arg (e.g. SPAWN player count); undefined when absent
    // result = "?"
    c.op_u30(OP_PUSHSTRING, s_q); c.op_u30(OP_SETLOCAL, l_res);

    // try-region start: the whole dispatch is wrapped so a thrown command (bad path,
    // unresolved member, engine-side null-deref) reports "ERR:<exception>" instead of
    // killing the handler silently (which would time the host out).
    let l_try_start = c.new_label(); c.place(l_try_start);

    let l_done = c.new_label();
    // No execute-once guard: this is event-driven (one socketData = one command), so
    // a side-effecting command runs exactly once, not 30×/sec like the file bridge.
    // dispatch chain
    let mut next = c.new_label();
    // PING
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_ping); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_PUSHSTRING, s_pong); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // ROOT
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_root); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op(OP_GETLOCAL0); c.op_u30(OP_SETPROPERTY, mn_cur); // this.peptideCur = this  (push this, this; setproperty)
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // GET <name>: cur = cur[a1]
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_get); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0);                                   // receiver for setproperty
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_GETPROPERTY, mnl); // cur[a1]
    c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // IDX <n>: cur = cur[Number(a1)]
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_idx); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_D); c.op_u30(OP_GETPROPERTY, mnl);
    c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // CALL <name>: cur = cur.name()
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_call); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30_u30(OP_CALLPROPERTY, mnl, 0);
    c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // CALL1 <name> <n>: cur = cur.name(Number(n))
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_call1); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_D); c.op_u30_u30(OP_CALLPROPERTY, mnl, 1);
    c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // CALLS <name> <str>: cur = cur.name(<str>)  (string arg, e.g. getLibraryMC)
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_calls); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_GETLOCAL, l_a2); c.op_u30_u30(OP_CALLPROPERTY, mnl, 1);
    c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // GC: cur = getlex GameController (the static match singleton)
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_gc); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // RM: cur = getlex ResourceManager
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_rm); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // STATS: cur = getlex Stats
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_stats); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETLEX, mn_stats_real); c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // MC: cur = getlex MenuController (so the host can call menu methods directly)
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_mc); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETLEX, mn_menuctrl); c.op_u30(OP_SETPROPERTY, mn_cur);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // SETP <name> <n>: cur[name] = Number(n)  (e.g. set YSpeed to launch a jump)
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_setp); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur);   // cur (object)
    c.op_u30(OP_GETLOCAL, l_a1);                            // name (runtime key)
    c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_D);        // value
    c.op_u30(OP_SETPROPERTY, mnl);                          // cur[name] = value
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // SETS <name> <str>: cur[name] = <str>  (STRING set; SETP coerces to Number, so
    // string fields like PlayerSetting.character need this).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_sets); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur);
    c.op_u30(OP_GETLOCAL, l_a1);
    c.op_u30(OP_GETLOCAL, l_a2);
    c.op_u30(OP_SETPROPERTY, mnl);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // QUEUE1 <id>: ResourceManager.queueResources([id]) — queue ONE resource.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_queue1); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_NEWARRAY, 1); c.op_u30_u30(OP_CALLPROPVOID, mn_queueresources, 1);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // MLOAD: ResourceManager.load({multimode:true}) — (re)drive the load of the queue.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_mload); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_GETLEX, mn_rm);
    c.op_u30(OP_PUSHSTRING, s_multimode); c.op(OP_PUSHTRUE); c.op_u30(OP_NEWOBJECT, 1);
    c.op_u30_u30(OP_CALLPROPVOID, mn_load, 1);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // SPAWN <char> <stage>: g=new Game(1,TRAINING); g.LevelData.stage=a2;
    //   ps=g.PlayerSettings[0]; ps.character=a1; ps.human=true; ps.costume=0;
    //   GameController.startMatch(g)   (the character spawns next frame)
    let (l_g, l_ps) = (11u32, 12u32);
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_spawn); c.branch(OP_IFSTRICTNE, next);
    // g = new Game(Number(a3), Mode.VERSUS). The first Game arg is the PLAYER-SLOT
    // COUNT (creates that many PlayerSettings); a3 is the host-supplied player count.
    // VERSUS so the match honors the rules the host applies before startMatch.
    c.op_u30(OP_FINDPROPSTRICT, mn_game); c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_D);
    c.op_u30(OP_GETLEX, mn_mode); c.op_u30(OP_GETPROPERTY, mn_versus);
    c.op_u30_u30(OP_CONSTRUCTPROP, mn_game, 2); c.op_u30(OP_SETLOCAL, l_g);
    // g.LevelData.stage = a2
    c.op_u30(OP_GETLOCAL, l_g); c.op_u30(OP_GETPROPERTY, mn_leveldata); c.op_u30(OP_GETLOCAL, l_a2); c.op_u30(OP_SETPROPERTY, mn_stage);
    // ps = g.PlayerSettings[0]
    c.op_u30(OP_GETLOCAL, l_g); c.op_u30(OP_GETPROPERTY, mn_playersettings); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_GETPROPERTY, mnl); c.op_u30(OP_SETLOCAL, l_ps);
    // ps.character = a1
    c.op_u30(OP_GETLOCAL, l_ps); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_SETPROPERTY, mn_character);
    // ps.human = true
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHTRUE); c.op_u30(OP_SETPROPERTY, mn_human);
    // ps.costume = 0
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETPROPERTY, mn_costume);
    // GameController.currentGame = g  (so GO can startMatch(currentGame))
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETLOCAL, l_g); c.op_u30(OP_SETPROPERTY, mn_currentgame);
    // flush the title-screen's polluted load queue first (it holds stuck items
    // like menu_news that never load offline → isFullyLoaded would never go true).
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30_u30(OP_CALLPROPVOID, mn_flushqueue, 0);
    // queue the STAGE + CHARACTER resources (both are in the resource pool at a fresh
    // boot, keyed by id; load() then loads both and unloadOldResources keeps them since
    // they're queued). Character must be loaded too or makePlayer null-derefs on
    // Stats.getStats. queueResources([stage]); queueResources([char]).
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_GETLOCAL, l_a2); c.op_u30(OP_NEWARRAY, 1); c.op_u30_u30(OP_CALLPROPVOID, mn_queueresources, 1);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_NEWARRAY, 1); c.op_u30_u30(OP_CALLPROPVOID, mn_queueresources, 1);
    // ResourceManager.load({multimode:true}) — multimode=false SKIPS loading the
    // queue (jumps past the queue[0].load(handleLoaded) block); multimode=true
    // actually drives the in-context async load chain that decrypts content.
    c.op_u30(OP_GETLEX, mn_rm);
    c.op_u30(OP_PUSHSTRING, s_multimode); c.op(OP_PUSHTRUE); c.op_u30(OP_NEWOBJECT, 1);
    c.op_u30_u30(OP_CALLPROPVOID, mn_load, 1);
    c.op_u30(OP_PUSHSTRING, s_queued); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // GO: GameController.startMatch(currentGame); MenuController.disposeAllMenus()
    //     — start the match AND tear down the menu/intro screens so the live match
    //     isn't left sitting behind them (the menu flow normally does this teardown;
    //     a programmatic startMatch skips it).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_go); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_currentgame); c.op_u30_u30(OP_CALLPROPVOID, mn_startmatch, 1);
    c.op_u30(OP_GETLEX, mn_menuctrl); c.op_u30_u30(OP_CALLPROPVOID, mn_disposemenus, 0);
    c.op_u30(OP_PUSHSTRING, s_spawned); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // QUEUE <a1> <a2>: ResourceManager.queueResources([a1, a2]) — queue ids to load
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_queue); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_GETLEX, mn_rm);
    c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_GETLOCAL, l_a2); c.op_u30(OP_NEWARRAY, 2);
    c.op_u30_u30(OP_CALLPROPVOID, mn_queueresources, 1);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // LOADNEXT: ResourceManager.loadNext() — kick the async loader
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_loadnext); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30_u30(OP_CALLPROPVOID, mn_loadnext, 0);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // LOADED: result = String(ResourceManager.isFullyLoaded())
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_loaded); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30_u30(OP_CALLPROPERTY, mn_isfullyloaded, 0); c.op(OP_CONVERT_S); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // HOLD <idx> <mask>: set the persistent held mask on Characters[idx]'s controller.
    // release = mask 0. (peptideSeq cleared so a stale timeline can't override the hold.)
    // No-op when no match is live (stageData null). ctrl is stashed in l_a3 (free here).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_hold); c.branch(OP_IFSTRICTNE, next);
    let l_hold_skip = c.new_label();
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata2); c.op_u30(OP_SETLOCAL, l_a3);
    c.op_u30(OP_GETLOCAL, l_a3); c.branch(OP_IFFALSE, l_hold_skip);
    c.op_u30(OP_GETLOCAL, l_a3); c.op_u30(OP_GETPROPERTY, mn_characters2);
    c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_D); c.op_u30(OP_GETPROPERTY, mnl);
    c.op_u30(OP_GETPROPERTY, mn_controlsettings); c.op_u30(OP_SETLOCAL, l_a3); // l_a3 = ctrl
    c.op_u30(OP_GETLOCAL, l_a3); c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_D); c.op_u30(OP_SETPROPERTY, mn_holdmask);
    c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_PUSHNULL); c.op_u30(OP_SETPROPERTY, mn_seqlist);
    c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_PUSHTRUE); c.op_u30(OP_SETPROPERTY, mn_active);
    c.place(l_hold_skip);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // SEQ <idx> <m,m,…>: queue a per-frame mask timeline on Characters[idx]'s controller;
    // the applicator drains one per frame and auto-releases (mask 0) once exhausted.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_seq); c.branch(OP_IFSTRICTNE, next);
    let l_seq_skip = c.new_label();
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata2); c.op_u30(OP_SETLOCAL, l_a3);
    c.op_u30(OP_GETLOCAL, l_a3); c.branch(OP_IFFALSE, l_seq_skip);
    c.op_u30(OP_GETLOCAL, l_a3); c.op_u30(OP_GETPROPERTY, mn_characters2);
    c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_D); c.op_u30(OP_GETPROPERTY, mnl);
    c.op_u30(OP_GETPROPERTY, mn_controlsettings); c.op_u30(OP_SETLOCAL, l_a3); // l_a3 = ctrl
    // ctrl.peptideSeq = a2.split(",")
    c.op_u30(OP_GETLOCAL, l_a3); c.op_u30(OP_GETLOCAL, l_a2); c.op_u30(OP_PUSHSTRING, s_comma); c.op_u30_u30(OP_CALLPROPERTY, mn_split, 1); c.op_u30(OP_SETPROPERTY, mn_seqlist);
    c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETPROPERTY, mn_seqidx);
    c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETPROPERTY, mn_holdmask);
    c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_PUSHTRUE); c.op_u30(OP_SETPROPERTY, mn_active);
    c.place(l_seq_skip);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // ADDCHAR <char> <slot>: live add-player into a RESERVED (pre-allocated, empty) slot.
    // The match's player containers are fixed-size at startGame, so spawn reserves spare
    // slots (exist=false, null character) and addCharacter fills one here: set the slot's
    // PlayerSetting, importData the stats, then construct the Character directly (the ctor
    // self-registers via StageData.addPlayer/addCharacter), bracketed by de/activate like
    // makePlayer. We skip makePlayer's attachHealthBox (its HUD attach assumes start-game
    // per-slot setup). No-op if no match is live. l_g=currentGame, l_ps=the slot's
    // PlayerSetting, l_a3=stats CharacterData.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_addchar); c.branch(OP_IFSTRICTNE, next);
    let l_ac_skip = c.new_label();
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_currentgame); c.op_u30(OP_SETLOCAL, l_g);
    c.op_u30(OP_GETLOCAL, l_g); c.branch(OP_IFFALSE, l_ac_skip);
    // ps = g.PlayerSettings[Number(slot)]  (the reserved slot)
    c.op_u30(OP_GETLOCAL, l_g); c.op_u30(OP_GETPROPERTY, mn_playersettings); c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_D); c.op_u30(OP_GETPROPERTY, mnl); c.op_u30(OP_SETLOCAL, l_ps);
    c.op_u30(OP_GETLOCAL, l_ps); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30(OP_SETPROPERTY, mn_character);
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHTRUE); c.op_u30(OP_SETPROPERTY, mn_human);
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHTRUE); c.op_u30(OP_SETPROPERTY, mn_exist);
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETPROPERTY, mn_costume);
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETPROPERTY, mn_expansion);
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHBYTE); c.op(0xFF); c.op_u30(OP_SETPROPERTY, mn_team); // team = -1 (FFA)
    c.op_u30(OP_GETLOCAL, l_ps); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETPROPERTY, mn_x_start);
    c.op_u30(OP_GETLOCAL, l_ps); c.op_u30(OP_PUSHSHORT, 100); c.op_u30(OP_SETPROPERTY, mn_y_start);
    // stats = Stats.getStats(char)
    c.op_u30(OP_GETLEX, mn_stats_real); c.op_u30(OP_GETLOCAL, l_a1); c.op_u30_u30(OP_CALLPROPERTY, mn_getstats, 1); c.op_u30(OP_SETLOCAL, l_a3);
    // stats.importData({player_id: Number(slot)+1 (1-based, matching makePlayer's blue), shieldType:"shield", stamina:0})
    c.op_u30(OP_GETLOCAL, l_a3);
    c.op_u30(OP_PUSHSTRING, s_player_id); c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_D); c.op(OP_PUSHBYTE); c.op(1); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_shieldtype); c.op_u30(OP_PUSHSTRING, s_shield);
    c.op_u30(OP_PUSHSTRING, s_stamina); c.op(OP_PUSHBYTE); c.op(0);
    c.op_u30(OP_NEWOBJECT, 3);
    c.op_u30_u30(OP_CALLPROPVOID, mn_importdata, 1);
    // stageData.deactivateCharacters()  (makePlayer brackets the construct with de/activate)
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata2); c.op_u30_u30(OP_CALLPROPVOID, mn_deactivate, 0);
    // new Character(stats, ps, stageData) — ctor self-registers via addPlayer/addCharacter
    c.op_u30(OP_FINDPROPSTRICT, mn_character_class);
    c.op_u30(OP_GETLOCAL, l_a3); c.op_u30(OP_GETLOCAL, l_ps);
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata2);
    c.op_u30_u30(OP_CONSTRUCTPROP, mn_character_class, 3); c.op(OP_POP);
    // stageData.activateCharacters()
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata2); c.op_u30_u30(OP_CALLPROPVOID, mn_activate, 0);
    c.place(l_ac_skip);
    c.op_u30(OP_PUSHSTRING, s_ok); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // TUNE <move> <accessor> <value>: mutate stored attack box 0's stat on peptideCur.
    // cur.AttackDataObj().getAttack(a1).AttackBoxes[0][a2] = Number(a3). AttackDataObj is
    // protected (reused QName); getAttack/AttackBoxes/the AttackDamage setter are public.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_tune); c.branch(OP_IFSTRICTNE, next);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur);
    c.op_u30(OP_GETPROPERTY, mn_attackdataobj);                    // AttackDataObj is a GETTER
    c.op_u30(OP_GETLOCAL, l_a1); c.op_u30_u30(OP_CALLPROPERTY, mn_getattack, 1); // getAttack(move) is a method
    c.op_u30(OP_GETPROPERTY, mn_attackboxes);                      // AttackBoxes is a GETTER (string-keyed)
    c.op_u30(OP_PUSHSTRING, s_attackbox); c.op_u30(OP_GETPROPERTY, mnl); // box = AttackBoxes["attackBox"]
    c.op(OP_DUP);                                                  // [box, box]
    c.op_u30(OP_GETLOCAL, l_a2);                                   // accessor (runtime name)
    c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_D);               // Number(value)
    c.op_u30(OP_SETPROPERTY, mnl);                                 // box[accessor] = value ; leaves [box]
    c.op_u30(OP_GETLOCAL, l_a2); c.op_u30(OP_GETPROPERTY, mnl);    // read box[accessor] back
    c.op(OP_CONVERT_S); c.op_u30(OP_SETLOCAL, l_res);             // result = the new value (confirmation)
    c.branch(OP_JUMP, l_done);
    // LOG <msg>: result = "logged: " + a1  (commands.hsx log() parity)
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_log); c.branch(OP_IFSTRICTNE, next);
    c.op_u30(OP_PUSHSTRING, s_logged); c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_ADD); c.op_u30(OP_SETLOCAL, l_res); c.branch(OP_JUMP, l_done);
    // SHOT <path>: the engine photographs ITSELF and writes the PNG straight to disk.
    //
    // The counterpart of Fraymakers' `shot`, and the reason the vocabulary can ask both
    // engines the same question: an object walk proves an object EXISTS, is visible, sized
    // and positioned, and none of that is evidence a pixel of it was drawn. Comparing the two
    // engines' art needs their actual frames.
    //
    // The transports differ because the engines do. Fraymakers hex-encodes over its socket;
    // here the frame is written directly, because hex-encoding a ByteArray needs a LOOP and a
    // backward branch fails AVM2 verification, taking the whole dispatcher with it (the same
    // constraint that made the tree walk recursive). AIR can write the file itself, so it does.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_shot); c.branch(OP_IFSTRICTNE, next);
    {
        // bd = new BitmapData(<w>, <h>, false, 0) — the size comes over the WIRE rather than
        // from `this.stage`: reading it off the dispatcher's receiver raises #1069 (the property
        // is not on that object), and the host already knows the view dimensions.
        c.op_u30(OP_FINDPROPSTRICT, fsapi.bitmap_data);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op(OP_PUSHFALSE);
        c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.bitmap_data, 4);
        c.op_u30(OP_SETLOCAL, l_shot_bd);
        // bd.draw(this, this.transform.matrix) — the root WITH its own transform, so the capture
        // carries the camera's pan/zoom. Drawing without the matrix ignores it and photographs the
        // stage at its authored scale, which silently makes a cross-engine frame comparison
        // measure FRAMING rather than art.
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op(OP_GETLOCAL0);
        c.op(OP_GETLOCAL0);
        c.op_u30(OP_GETPROPERTY, fsapi.transform);
        c.op_u30(OP_GETPROPERTY, fsapi.matrix);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.draw, 2);
        // ba = bd.encode(new Rectangle(0, 0, w, h), new PNGEncoderOptions())
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.rectangle);
        c.op_u30(OP_PUSHBYTE, 0); c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.rectangle, 4);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.png_opts);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.png_opts, 0);
        c.op_u30_u30(OP_CALLPROPERTY, fsapi.encode, 2);
        c.op_u30(OP_SETLOCAL, l_shot_ba);
        // write it: new File(<path arg>), FileStream.open(WRITE), writeBytes, close
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file);
        c.op_u30(OP_GETLOCAL, l_a1);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file, 1);
        c.op_u30(OP_SETLOCAL, l_shot_f);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file_stream);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file_stream, 0);
        c.op_u30(OP_SETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_f);
        c.op_u30(OP_GETLEX, fsapi.file_mode);
        c.op_u30(OP_GETPROPERTY, fsapi.write_prop);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.open, 2);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_ba);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.write_bytes, 1);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.close, 0);
        c.op_u30(OP_PUSHSTRING, s_shot_ok);
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_ADD);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // NODE: the whole node in one reply (see s_v_node). Properties every DisplayObject
    // has are read unguarded; MovieClip-only and container-only ones are behind an
    // `istypelate` check, because these classes are SEALED and reading a missing property
    // throws — which the dispatcher's try would turn into "ERR:" for the entire node.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_node); c.branch(OP_IFSTRICTNE, next);
    {
        let cur = |c: &mut Code| { c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); };
        let bar = |c: &mut Code| { c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD); };
        // class + the DisplayObject-safe fields
        cur(&mut c); c.op(OP_CONVERT_S);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_name); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_x); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_y); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_w); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_h); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_vis); c.op(OP_CONVERT_S); c.op(OP_ADD);
        // numChildren, only if it is a container
        bar(&mut c);
        let l_nc_no = c.new_label(); let l_nc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_container); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_nc_no);
        cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_nch); c.op(OP_CONVERT_S); c.branch(OP_JUMP, l_nc_end);
        c.place(l_nc_no); c.op_u30(OP_PUSHSTRING, s_dash);
        c.place(l_nc_end); c.op(OP_ADD);
        // frame / total / label, only if it is a MovieClip
        let l_mc_no = c.new_label(); let l_mc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_movieclip); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_mc_no);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_tf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cl); c.op(OP_CONVERT_S); c.op(OP_ADD);
        c.branch(OP_JUMP, l_mc_end);
        c.place(l_mc_no);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_empty2); c.op(OP_ADD);
        c.place(l_mc_end);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // TREE <maxDepth>: the whole subtree from the root, in one reply (see s_v_tree).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_tree); c.branch(OP_IFSTRICTNE, next);
    {
        c.op(OP_GETLOCAL0);                                   // receiver
        c.op(OP_GETLOCAL0);                                   // node = this (the display root)
        c.op_u30(OP_PUSHBYTE, 0);                             // depth
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_I);      // maxDepth (wire arg)
        c.op_u30_u30(OP_CALLPROPERTY, mn_pwalk, 3);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // SHOT <path>: the engine photographs ITSELF and writes the PNG straight to disk.
    //
    // The counterpart of Fraymakers' `shot`, and the reason the vocabulary can ask both
    // engines the same question: an object walk proves an object EXISTS, is visible, sized
    // and positioned, and none of that is evidence a pixel of it was drawn. Comparing the two
    // engines' art needs their actual frames.
    //
    // The transports differ because the engines do. Fraymakers hex-encodes over its socket;
    // here the frame is written directly, because hex-encoding a ByteArray needs a LOOP and a
    // backward branch fails AVM2 verification, taking the whole dispatcher with it (the same
    // constraint that made the tree walk recursive). AIR can write the file itself, so it does.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_shot); c.branch(OP_IFSTRICTNE, next);
    {
        // bd = new BitmapData(<w>, <h>, false, 0) — the size comes over the WIRE rather than
        // from `this.stage`: reading it off the dispatcher's receiver raises #1069 (the property
        // is not on that object), and the host already knows the view dimensions.
        c.op_u30(OP_FINDPROPSTRICT, fsapi.bitmap_data);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op(OP_PUSHFALSE);
        c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.bitmap_data, 4);
        c.op_u30(OP_SETLOCAL, l_shot_bd);
        // bd.draw(this, this.transform.matrix) — the root WITH its own transform, so the capture
        // carries the camera's pan/zoom. Drawing without the matrix ignores it and photographs the
        // stage at its authored scale, which silently makes a cross-engine frame comparison
        // measure FRAMING rather than art.
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op(OP_GETLOCAL0);
        c.op(OP_GETLOCAL0);
        c.op_u30(OP_GETPROPERTY, fsapi.transform);
        c.op_u30(OP_GETPROPERTY, fsapi.matrix);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.draw, 2);
        // ba = bd.encode(new Rectangle(0, 0, w, h), new PNGEncoderOptions())
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.rectangle);
        c.op_u30(OP_PUSHBYTE, 0); c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.rectangle, 4);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.png_opts);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.png_opts, 0);
        c.op_u30_u30(OP_CALLPROPERTY, fsapi.encode, 2);
        c.op_u30(OP_SETLOCAL, l_shot_ba);
        // write it: new File(<path arg>), FileStream.open(WRITE), writeBytes, close
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file);
        c.op_u30(OP_GETLOCAL, l_a1);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file, 1);
        c.op_u30(OP_SETLOCAL, l_shot_f);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file_stream);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file_stream, 0);
        c.op_u30(OP_SETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_f);
        c.op_u30(OP_GETLEX, fsapi.file_mode);
        c.op_u30(OP_GETPROPERTY, fsapi.write_prop);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.open, 2);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_ba);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.write_bytes, 1);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.close, 0);
        c.op_u30(OP_PUSHSTRING, s_shot_ok);
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_ADD);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // NODE: the whole node in one reply (see s_v_node). Properties every DisplayObject
    // has are read unguarded; MovieClip-only and container-only ones are behind an
    // `istypelate` check, because these classes are SEALED and reading a missing property
    // throws — which the dispatcher's try would turn into "ERR:" for the entire node.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_node); c.branch(OP_IFSTRICTNE, next);
    {
        let cur = |c: &mut Code| { c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); };
        let bar = |c: &mut Code| { c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD); };
        // class + the DisplayObject-safe fields
        cur(&mut c); c.op(OP_CONVERT_S);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_name); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_x); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_y); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_w); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_h); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_vis); c.op(OP_CONVERT_S); c.op(OP_ADD);
        // numChildren, only if it is a container
        bar(&mut c);
        let l_nc_no = c.new_label(); let l_nc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_container); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_nc_no);
        cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_nch); c.op(OP_CONVERT_S); c.branch(OP_JUMP, l_nc_end);
        c.place(l_nc_no); c.op_u30(OP_PUSHSTRING, s_dash);
        c.place(l_nc_end); c.op(OP_ADD);
        // frame / total / label, only if it is a MovieClip
        let l_mc_no = c.new_label(); let l_mc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_movieclip); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_mc_no);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_tf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cl); c.op(OP_CONVERT_S); c.op(OP_ADD);
        c.branch(OP_JUMP, l_mc_end);
        c.place(l_mc_no);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_empty2); c.op(OP_ADD);
        c.place(l_mc_end);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // TREE <maxDepth>: the whole subtree from the root, in one reply (see s_v_tree).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_tree); c.branch(OP_IFSTRICTNE, next);
    {
        c.op(OP_GETLOCAL0);                                   // receiver
        c.op(OP_GETLOCAL0);                                   // node = this (the display root)
        c.op_u30(OP_PUSHBYTE, 0);                             // depth
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_I);      // maxDepth (wire arg)
        c.op_u30_u30(OP_CALLPROPERTY, mn_pwalk, 3);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // SHOT <path>: the engine photographs ITSELF and writes the PNG straight to disk.
    //
    // The counterpart of Fraymakers' `shot`, and the reason the vocabulary can ask both
    // engines the same question: an object walk proves an object EXISTS, is visible, sized
    // and positioned, and none of that is evidence a pixel of it was drawn. Comparing the two
    // engines' art needs their actual frames.
    //
    // The transports differ because the engines do. Fraymakers hex-encodes over its socket;
    // here the frame is written directly, because hex-encoding a ByteArray needs a LOOP and a
    // backward branch fails AVM2 verification, taking the whole dispatcher with it (the same
    // constraint that made the tree walk recursive). AIR can write the file itself, so it does.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_shot); c.branch(OP_IFSTRICTNE, next);
    {
        // bd = new BitmapData(<w>, <h>, false, 0) — the size comes over the WIRE rather than
        // from `this.stage`: reading it off the dispatcher's receiver raises #1069 (the property
        // is not on that object), and the host already knows the view dimensions.
        c.op_u30(OP_FINDPROPSTRICT, fsapi.bitmap_data);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op(OP_PUSHFALSE);
        c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.bitmap_data, 4);
        c.op_u30(OP_SETLOCAL, l_shot_bd);
        // bd.draw(this, this.transform.matrix) — the root WITH its own transform, so the capture
        // carries the camera's pan/zoom. Drawing without the matrix ignores it and photographs the
        // stage at its authored scale, which silently makes a cross-engine frame comparison
        // measure FRAMING rather than art.
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op(OP_GETLOCAL0);
        c.op(OP_GETLOCAL0);
        c.op_u30(OP_GETPROPERTY, fsapi.transform);
        c.op_u30(OP_GETPROPERTY, fsapi.matrix);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.draw, 2);
        // ba = bd.encode(new Rectangle(0, 0, w, h), new PNGEncoderOptions())
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.rectangle);
        c.op_u30(OP_PUSHBYTE, 0); c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.rectangle, 4);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.png_opts);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.png_opts, 0);
        c.op_u30_u30(OP_CALLPROPERTY, fsapi.encode, 2);
        c.op_u30(OP_SETLOCAL, l_shot_ba);
        // write it: new File(<path arg>), FileStream.open(WRITE), writeBytes, close
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file);
        c.op_u30(OP_GETLOCAL, l_a1);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file, 1);
        c.op_u30(OP_SETLOCAL, l_shot_f);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file_stream);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file_stream, 0);
        c.op_u30(OP_SETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_f);
        c.op_u30(OP_GETLEX, fsapi.file_mode);
        c.op_u30(OP_GETPROPERTY, fsapi.write_prop);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.open, 2);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_ba);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.write_bytes, 1);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.close, 0);
        c.op_u30(OP_PUSHSTRING, s_shot_ok);
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_ADD);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // NODE: the whole node in one reply (see s_v_node). Properties every DisplayObject
    // has are read unguarded; MovieClip-only and container-only ones are behind an
    // `istypelate` check, because these classes are SEALED and reading a missing property
    // throws — which the dispatcher's try would turn into "ERR:" for the entire node.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_node); c.branch(OP_IFSTRICTNE, next);
    {
        let cur = |c: &mut Code| { c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); };
        let bar = |c: &mut Code| { c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD); };
        // class + the DisplayObject-safe fields
        cur(&mut c); c.op(OP_CONVERT_S);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_name); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_x); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_y); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_w); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_h); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_vis); c.op(OP_CONVERT_S); c.op(OP_ADD);
        // numChildren, only if it is a container
        bar(&mut c);
        let l_nc_no = c.new_label(); let l_nc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_container); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_nc_no);
        cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_nch); c.op(OP_CONVERT_S); c.branch(OP_JUMP, l_nc_end);
        c.place(l_nc_no); c.op_u30(OP_PUSHSTRING, s_dash);
        c.place(l_nc_end); c.op(OP_ADD);
        // frame / total / label, only if it is a MovieClip
        let l_mc_no = c.new_label(); let l_mc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_movieclip); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_mc_no);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_tf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cl); c.op(OP_CONVERT_S); c.op(OP_ADD);
        c.branch(OP_JUMP, l_mc_end);
        c.place(l_mc_no);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_empty2); c.op(OP_ADD);
        c.place(l_mc_end);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // TREE <maxDepth>: the whole subtree from the root, in one reply (see s_v_tree).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_tree); c.branch(OP_IFSTRICTNE, next);
    {
        c.op(OP_GETLOCAL0);                                   // receiver
        c.op(OP_GETLOCAL0);                                   // node = this (the display root)
        c.op_u30(OP_PUSHBYTE, 0);                             // depth
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_I);      // maxDepth (wire arg)
        c.op_u30_u30(OP_CALLPROPERTY, mn_pwalk, 3);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // SHOT <path>: the engine photographs ITSELF and writes the PNG straight to disk.
    //
    // The counterpart of Fraymakers' `shot`, and the reason the vocabulary can ask both
    // engines the same question: an object walk proves an object EXISTS, is visible, sized
    // and positioned, and none of that is evidence a pixel of it was drawn. Comparing the two
    // engines' art needs their actual frames.
    //
    // The transports differ because the engines do. Fraymakers hex-encodes over its socket;
    // here the frame is written directly, because hex-encoding a ByteArray needs a LOOP and a
    // backward branch fails AVM2 verification, taking the whole dispatcher with it (the same
    // constraint that made the tree walk recursive). AIR can write the file itself, so it does.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_shot); c.branch(OP_IFSTRICTNE, next);
    {
        // bd = new BitmapData(<w>, <h>, false, 0) — the size comes over the WIRE rather than
        // from `this.stage`: reading it off the dispatcher's receiver raises #1069 (the property
        // is not on that object), and the host already knows the view dimensions.
        c.op_u30(OP_FINDPROPSTRICT, fsapi.bitmap_data);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op(OP_PUSHFALSE);
        c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.bitmap_data, 4);
        c.op_u30(OP_SETLOCAL, l_shot_bd);
        // bd.draw(this, this.transform.matrix) — the root WITH its own transform, so the capture
        // carries the camera's pan/zoom. Drawing without the matrix ignores it and photographs the
        // stage at its authored scale, which silently makes a cross-engine frame comparison
        // measure FRAMING rather than art.
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op(OP_GETLOCAL0);
        c.op(OP_GETLOCAL0);
        c.op_u30(OP_GETPROPERTY, fsapi.transform);
        c.op_u30(OP_GETPROPERTY, fsapi.matrix);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.draw, 2);
        // ba = bd.encode(new Rectangle(0, 0, w, h), new PNGEncoderOptions())
        c.op_u30(OP_GETLOCAL, l_shot_bd);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.rectangle);
        c.op_u30(OP_PUSHBYTE, 0); c.op_u30(OP_PUSHBYTE, 0);
        c.op_u30(OP_GETLOCAL, l_a2); c.op(OP_CONVERT_I);
        c.op_u30(OP_GETLOCAL, l_a3); c.op(OP_CONVERT_I);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.rectangle, 4);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.png_opts);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.png_opts, 0);
        c.op_u30_u30(OP_CALLPROPERTY, fsapi.encode, 2);
        c.op_u30(OP_SETLOCAL, l_shot_ba);
        // write it: new File(<path arg>), FileStream.open(WRITE), writeBytes, close
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file);
        c.op_u30(OP_GETLOCAL, l_a1);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file, 1);
        c.op_u30(OP_SETLOCAL, l_shot_f);
        c.op_u30(OP_FINDPROPSTRICT, fsapi.file_stream);
        c.op_u30_u30(OP_CONSTRUCTPROP, fsapi.file_stream, 0);
        c.op_u30(OP_SETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_f);
        c.op_u30(OP_GETLEX, fsapi.file_mode);
        c.op_u30(OP_GETPROPERTY, fsapi.write_prop);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.open, 2);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30(OP_GETLOCAL, l_shot_ba);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.write_bytes, 1);
        c.op_u30(OP_GETLOCAL, l_shot_fs);
        c.op_u30_u30(OP_CALLPROPVOID, fsapi.close, 0);
        c.op_u30(OP_PUSHSTRING, s_shot_ok);
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_ADD);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // NODE: the whole node in one reply (see s_v_node). Properties every DisplayObject
    // has are read unguarded; MovieClip-only and container-only ones are behind an
    // `istypelate` check, because these classes are SEALED and reading a missing property
    // throws — which the dispatcher's try would turn into "ERR:" for the entire node.
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_node); c.branch(OP_IFSTRICTNE, next);
    {
        let cur = |c: &mut Code| { c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); };
        let bar = |c: &mut Code| { c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD); };
        // class + the DisplayObject-safe fields
        cur(&mut c); c.op(OP_CONVERT_S);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_name); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_x); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_y); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_w); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_h); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_vis); c.op(OP_CONVERT_S); c.op(OP_ADD);
        // numChildren, only if it is a container
        bar(&mut c);
        let l_nc_no = c.new_label(); let l_nc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_container); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_nc_no);
        cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_nch); c.op(OP_CONVERT_S); c.branch(OP_JUMP, l_nc_end);
        c.place(l_nc_no); c.op_u30(OP_PUSHSTRING, s_dash);
        c.place(l_nc_end); c.op(OP_ADD);
        // frame / total / label, only if it is a MovieClip
        let l_mc_no = c.new_label(); let l_mc_end = c.new_label();
        cur(&mut c); c.op_u30(OP_GETLEX, mn_movieclip); c.op(OP_ISTYPELATE); c.branch(OP_IFFALSE, l_mc_no);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_tf); c.op(OP_CONVERT_S); c.op(OP_ADD);
        bar(&mut c); cur(&mut c); c.op_u30(OP_GETPROPERTY, mn_n_cl); c.op(OP_CONVERT_S); c.op(OP_ADD);
        c.branch(OP_JUMP, l_mc_end);
        c.place(l_mc_no);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_dash); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
        c.op_u30(OP_PUSHSTRING, s_empty2); c.op(OP_ADD);
        c.place(l_mc_end);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // TREE <maxDepth>: the whole subtree from the root, in one reply (see s_v_tree).
    c.place(next); next = c.new_label();
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_tree); c.branch(OP_IFSTRICTNE, next);
    {
        c.op(OP_GETLOCAL0);                                   // receiver
        c.op(OP_GETLOCAL0);                                   // node = this (the display root)
        c.op_u30(OP_PUSHBYTE, 0);                             // depth
        c.op_u30(OP_GETLOCAL, l_a1); c.op(OP_CONVERT_I);      // maxDepth (wire arg)
        c.op_u30_u30(OP_CALLPROPERTY, mn_pwalk, 3);
        c.op_u30(OP_SETLOCAL, l_res);
    }

    // CHILDREN: every child of the current node, one NODE-shaped record each, newline-joined.
    //
    // The host walk re-navigates from ROOT for every node (ROOT + one getChildAt per level +
    // NODE), so a stage tree costs on the order of (depth + 2) round trips PER NODE — about
    // 1,800 for bowserscastle. That is not only slow: the walk is not atomic, so the live
    // display list shifts underneath it and objects get reported twice or missed entirely.
    // Reading a whole sibling set at once removes the per-child navigation and makes each
    // container's children one consistent snapshot.
    //
    // UNROLLED over fixed child indices with FORWARD jumps only. A real loop here — counter
    // local, `inclocal_i`, backward `jump` — fails AVM2 verification and takes the whole verb
    // dispatcher down with it (the engine comes up with SCRIPTERR and every verb, NODE
    // included, answers "?"). Bisected to the backward branch specifically: identical code
    // with the jump removed verifies fine. The Fraymakers walk hit the same wall and uses the
    // same shape; see `tree` in src/main.rs.

    // NOTE: a CHILDREN verb (a whole sibling set in one reply) was tried here to collapse the
    // walk's per-node round trips. The AVM2 body failed verification and took the entire verb
    // dispatcher down with it -- the engine came up with SCRIPTERR and every verb, including
    // NODE, returned "?". Reverted rather than left half-working; the walk is pruned host-side
    // instead (see Ssf2Target::walk_node), which is the smaller win but keeps the engine sound.
    c.branch(OP_JUMP, l_done);
    // READ: result = String(cur)
    c.place(next);
    c.op_u30(OP_GETLOCAL, l_verb); c.op_u30(OP_PUSHSTRING, s_v_read); c.branch(OP_IFSTRICTNE, l_done);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_cur); c.op(OP_CONVERT_S); c.op_u30(OP_SETLOCAL, l_res);

    c.place(l_done);
    // try-region end. Normal path skips the catch handler; a thrown command lands here
    // with the exception on the operand stack.
    let l_try_end = c.new_label(); c.place(l_try_end);
    let l_reply = c.new_label();
    c.branch(OP_JUMP, l_reply);
    // catch handler: result = "ERR:" + String(exception). Re-push the scope the unwind
    // dropped (so the scope depth matches the normal path at l_reply).
    let l_catch = c.new_label(); c.place(l_catch);
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);          // operand: [exc] ; scope restored to depth 1
    c.op(OP_CONVERT_S);                              // [String(exc)]
    c.op_u30(OP_PUSHSTRING, s_errpfx); c.op(OP_SWAP); c.op(OP_ADD); // ["ERR:" + String(exc)]
    c.op_u30(OP_SETLOCAL, l_res);
    c.place(l_reply);
    // ── write reply over the socket: peptideSock.writeUTFBytes(seq + " " + result + "\n"); flush() ──
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock);                                   // socket (receiver)
    c.op_u30(OP_GETLOCAL, l_seq); c.op_u30(OP_PUSHSTRING, s_sp); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_res); c.op(OP_ADD); c.op_u30(OP_PUSHSTRING, s_nl); c.op(OP_ADD); // seq + " " + result + "\n"
    c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock); c.op_u30_u30(OP_CALLPROPVOID, mn_flush, 0);
    c.op(OP_RETURNVOID);

    let handler = abc.add_method(MethodInfo { param_types: vec![0], return_type: 0, name: n_ondata, flags: 0, options: vec![], param_names: vec![] });
    // exception-table entry covering the whole dispatch (catch-all: exc_type 0). Capture
    // the byte offsets before finish() consumes the Code.
    let exc = Exception { from: c.pos(l_try_start), to: c.pos(l_try_end), target: c.pos(l_catch), exc_type: 0, var_name: 0 };
    // locals 14-17 and the deeper stack belong to SHOT (bitmap/bytes/file/stream, and the
    // BitmapData + Rectangle constructions): a body that uses a local or stack slot it did
    // not declare fails AVM2 verification at LOAD, which reads as "the engine never settled"
    // rather than as a bytecode error.
    abc.add_body(MethodBody { method: handler, max_stack: 12, local_count: 18, init_scope_depth: 0, max_scope_depth: 2, code: c.finish(), exceptions: vec![exc], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_ondata, handler);

    // ── peptideWalk(node, depth, maxDepth) : String ────────────────────────────────────
    // The recursive half of the TREE verb. Emits `<depth>|<class>|<name>|<x>|<y>|<w>|<h>|
    // <visible>|<numChildren>|<frame>|<total>|<label>` for `node`, then appends the same for
    // each child, records separated by ";;" (NOT newline -- the wire is line-based, and a
    // newline-joined reply is truncated to its first record).
    //
    // Children are UNROLLED with forward jumps only, for the same reason the CHILDREN verb is:
    // a backward branch fails verification. Recursion supplies the repetition a loop would
    // have, and costs one call per node instead of one round trip.
    {
        let mut w = Code::default();
        let (l_node, l_depth, l_max, l_res, l_kid) = (1u32, 2u32, 3u32, 4u32, 5u32);
        let node = |w: &mut Code| { w.op_u30(OP_GETLOCAL, l_node); };
        let kid = |w: &mut Code| { w.op_u30(OP_GETLOCAL, l_kid); };
        let bar = |w: &mut Code| { w.op_u30(OP_PUSHSTRING, s_bar); w.op(OP_ADD); };

        w.op(OP_GETLOCAL0); w.op(OP_PUSHSCOPE);
        // res = depth + "|" + <this node's record> + ";;"
        w.op_u30(OP_GETLOCAL, l_depth); w.op(OP_CONVERT_S);
        bar(&mut w); node(&mut w); w.op(OP_CONVERT_S); w.op(OP_ADD);
        for mn in [mn_n_name, mn_n_x, mn_n_y, mn_n_w, mn_n_h, mn_n_vis] {
            bar(&mut w); node(&mut w); w.op_u30(OP_GETPROPERTY, mn); w.op(OP_CONVERT_S); w.op(OP_ADD);
        }
        bar(&mut w);
        let (n_no, n_done) = (w.new_label(), w.new_label());
        node(&mut w); w.op_u30(OP_GETLEX, mn_container); w.op(OP_ISTYPELATE); w.branch(OP_IFFALSE, n_no);
        node(&mut w); w.op_u30(OP_GETPROPERTY, mn_n_nch); w.op(OP_CONVERT_S); w.branch(OP_JUMP, n_done);
        w.place(n_no); w.op_u30(OP_PUSHSTRING, s_dash);
        w.place(n_done); w.op(OP_ADD);
        let (c_no, c_done) = (w.new_label(), w.new_label());
        node(&mut w); w.op_u30(OP_GETLEX, mn_movieclip); w.op(OP_ISTYPELATE); w.branch(OP_IFFALSE, c_no);
        for mn in [mn_n_cf, mn_n_tf, mn_n_cl] {
            bar(&mut w); node(&mut w); w.op_u30(OP_GETPROPERTY, mn); w.op(OP_CONVERT_S); w.op(OP_ADD);
        }
        w.branch(OP_JUMP, c_done);
        w.place(c_no);
        for _ in 0..2 { bar(&mut w); w.op_u30(OP_PUSHSTRING, s_dash); w.op(OP_ADD); }
        bar(&mut w); w.op_u30(OP_PUSHSTRING, s_empty2); w.op(OP_ADD);
        w.place(c_done);
        // world bounding-box top-left: node.getBounds(root).x / .y
        for mn in [mn_n_x, mn_n_y] {
            bar(&mut w);
            node(&mut w);
            w.op(OP_GETLOCAL0);
            w.op_u30_u30(OP_CALLPROPERTY, mn_getbounds, 1);
            w.op_u30(OP_GETPROPERTY, mn);
            w.op(OP_CONVERT_S); w.op(OP_ADD);
        }
        w.op_u30(OP_PUSHSTRING, s_nl2); w.op(OP_ADD);
        w.op_u30(OP_SETLOCAL, l_res);

        let l_ret = w.new_label();
        // stop at maxDepth, and never descend into a non-container
        w.op_u30(OP_GETLOCAL, l_depth); w.op_u30(OP_GETLOCAL, l_max); w.branch(OP_IFGE, l_ret);
        node(&mut w); w.op_u30(OP_GETLEX, mn_container); w.op(OP_ISTYPELATE); w.branch(OP_IFFALSE, l_ret);

        for i in 0..48u32 {
            w.op_u30(OP_PUSHBYTE, i);
            node(&mut w); w.op_u30(OP_GETPROPERTY, mn_n_nch);
            w.branch(OP_IFGE, l_ret);
            node(&mut w); w.op_u30(OP_PUSHBYTE, i);
            w.op_u30_u30(OP_CALLPROPERTY, mn_getchildat, 1); w.op_u30(OP_SETLOCAL, l_kid);
            // res += this.peptideWalk(kid, depth + 1, max)
            w.op_u30(OP_GETLOCAL, l_res);
            w.op(OP_GETLOCAL0);
            kid(&mut w);
            w.op_u30(OP_GETLOCAL, l_depth); w.op_u30(OP_PUSHBYTE, 1); w.op(OP_ADD);
            w.op_u30(OP_GETLOCAL, l_max);
            w.op_u30_u30(OP_CALLPROPERTY, mn_pwalk, 3);
            w.op(OP_ADD);
            w.op_u30(OP_SETLOCAL, l_res);
        }
        w.place(l_ret);
        w.op_u30(OP_GETLOCAL, l_res); w.op(OP_RETURNVALUE);

        let n_pw = abc.intern_string("peptideWalk");
        let pw = abc.add_method(MethodInfo { param_types: vec![0, 0, 0], return_type: 0, name: n_pw, flags: 0, options: vec![], param_names: vec![] });
        abc.add_body(MethodBody { method: pw, max_stack: 10, local_count: 6, init_scope_depth: 0, max_scope_depth: 2, code: w.finish(), exceptions: vec![], traits: vec![] });
        abc.add_instance_method_trait(ci, mn_pwalk, pw);
    }

    // ctor: this.peptideSock = new Socket(); addEventListener("socketData", this.peptideOnData);
    //       connect(host, port)  — dial into the host's loopback server. connect() coerces
    //       the port String to int; the handler fires only when the host sends data.
    let mut ic = Code::default();
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHSCOPE);
    // this.peptideSock = new Socket()
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_FINDPROPSTRICT, mn_socket); ic.op_u30_u30(OP_CONSTRUCTPROP, mn_socket, 0); ic.op_u30(OP_SETPROPERTY, mn_sock);
    // this.peptideSock.addEventListener("socketData", this.peptideOnData)
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_sock);
    ic.op_u30(OP_PUSHSTRING, s_socketdata);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_ondata);
    ic.op_u30_u30(OP_CALLPROPVOID, mn_addel, 2);
    // this.peptideSock.connect(host, port)
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_sock);
    ic.op_u30(OP_PUSHSTRING, s_host); ic.op_u30(OP_PUSHSTRING, s_port);
    ic.op_u30_u30(OP_CALLPROPVOID, mn_connect, 2);
    ic.op(OP_POPSCOPE);
    let payload = ic.finish();
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit).ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(3);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Inject the per-frame INPUT APPLICATOR — the SSF2 half of host input injection
/// (`hold`/`release`/`seq`/`scenario`). This is the SSF2 analogue of Fraymakers'
/// per-frame control-mask epilogue: each frame the engine reads a player's controls
/// once, and we make that read return a host-supplied mask.
///
/// SSF2's per-frame input read flows through `Controller.getControlStatus()` (a
/// human player's `Character.m_getKey` calls it once per frame, then derives
/// held/pressed from it via the controls buffer). We PREPEND a guarded early-return:
/// when `this.peptideActive` is set, it returns a `ControlsObject` whose `controls`
/// is the mask for this frame, so held/pressed semantics are computed natively
/// downstream (a first-frame mask reads as "pressed", subsequent identical frames as
/// "held" — exactly like a real button). Otherwise it falls through to the stock
/// keyboard read untouched.
///
/// State lives on the CONTROLLER itself (added here as instance slots; set by the
/// socket bridge's HOLD/SEQ verbs via the public `Characters[idx].ControlSettings`
/// chain), not the document — the applicator runs inside `Controller`, so reading off
/// `this` needs no global anchor (the document singleton `Main.ROOT` is a non-public
/// static the reflection seam can't reach), and only the targeted controller carries
/// `peptideActive`, so no per-frame identity comparison is needed:
///   * `peptideActive`   — injection on for this controller
///   * `peptideHoldMask` — the persistent held mask (`release` = 0)
///   * `peptideSeq`      — a per-frame mask Array (or null); drained one/frame
///   * `peptideSeqIdx`   — next index into `peptideSeq`; auto-releases at the end
///
/// Reads off `this` can't throw; an idle/un-targeted controller has `peptideActive`
/// undefined → falls through to the unmodified body.
pub fn inject_input_applicator(abc: &mut Abc, _doc_class_local: &str) -> anyhow::Result<()> {
    // hook Controller.getControlStatus (resolved by NAME — version-resilient)
    let ctrl_ci = abc.find_class_by_name("Controller")
        .ok_or_else(|| anyhow::anyhow!("Controller not found"))?;
    let method = abc.instances[ctrl_ci].traits.iter().find_map(|t| match t.data {
        TraitKindData::Method { method, .. }
            if abc.multiname_local(t.name).as_deref() == Some("getControlStatus") => Some(method),
        _ => None,
    }).ok_or_else(|| anyhow::anyhow!("Controller.getControlStatus not found"))?;
    let body_idx = abc.bodies.iter().position(|b| b.method == method)
        .ok_or_else(|| anyhow::anyhow!("no body for getControlStatus"))?;

    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    // per-controller injection-state slots (must match the public QNames the socket
    // bridge sets on the target controller)
    let mn_active = q(abc, pub_ns, "peptideActive");
    let mn_holdmask = q(abc, pub_ns, "peptideHoldMask");
    let mn_seqlist = q(abc, pub_ns, "peptideSeq");
    let mn_seqidx = q(abc, pub_ns, "peptideSeqIdx");
    let mn_controls = q(abc, pub_ns, "controls");
    let mn_length = q(abc, pub_ns, "length");
    let util_ns = { let s = abc.intern_string(SSF2_NS_UTIL); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_controlsobject = q(abc, util_ns, "ControlsObject");
    let pub_nsset = abc.intern_ns_set(vec![pub_ns]);
    let mnl = abc.intern_multinamel(pub_nsset); // runtime [idx] access

    // add the state slots to the Controller class (public, untyped)
    abc.add_instance_slot(ctrl_ci, mn_active);
    abc.add_instance_slot(ctrl_ci, mn_holdmask);
    abc.add_instance_slot(ctrl_ci, mn_seqlist);
    abc.add_instance_slot(ctrl_ci, mn_seqidx);

    // scratch local (getControlStatus uses only local_0/local_1)
    let l_mask = 2u32;
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    let l_fall = c.new_label();
    let l_use_hold = c.new_label();
    let l_seq_live = c.new_label();
    let l_apply = c.new_label();
    // if (!this.peptideActive) fall
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_active); c.branch(OP_IFFALSE, l_fall);
    // if (this.peptideSeq == null) use hold
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_seqlist); c.branch(OP_IFFALSE, l_use_hold);
    // if (this.peptideSeqIdx < this.peptideSeq.length) seq-live, else exhausted
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_seqidx);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_seqlist); c.op_u30(OP_GETPROPERTY, mn_length);
    c.branch(OP_IFLT, l_seq_live);
    // exhausted: mask = 0 ; this.peptideSeq = null (auto-release)
    c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_SETLOCAL, l_mask);
    c.op(OP_GETLOCAL0); c.op(OP_PUSHNULL); c.op_u30(OP_SETPROPERTY, mn_seqlist);
    c.branch(OP_JUMP, l_apply);
    // seq-live: mask = int(this.peptideSeq[this.peptideSeqIdx]) ; this.peptideSeqIdx += 1
    c.place(l_seq_live);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_seqlist);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_seqidx); c.op_u30(OP_GETPROPERTY, mnl);
    c.op(OP_CONVERT_I); c.op_u30(OP_SETLOCAL, l_mask);
    c.op(OP_GETLOCAL0);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_seqidx); c.op(OP_PUSHBYTE); c.op(1); c.op(OP_ADD);
    c.op_u30(OP_SETPROPERTY, mn_seqidx);
    c.branch(OP_JUMP, l_apply);
    // use-hold: mask = int(this.peptideHoldMask)
    c.place(l_use_hold);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_holdmask); c.op(OP_CONVERT_I); c.op_u30(OP_SETLOCAL, l_mask);
    // apply: return a ControlsObject whose controls = mask
    c.place(l_apply);
    c.op_u30(OP_FINDPROPSTRICT, mn_controlsobject); c.op_u30_u30(OP_CONSTRUCTPROP, mn_controlsobject, 0);
    c.op(OP_DUP); c.op_u30(OP_GETLOCAL, l_mask); c.op_u30(OP_SETPROPERTY, mn_controls);
    c.op(OP_POPSCOPE); c.op(OP_RETURNVALUE);
    // fall-through: restore scope, then the original keyboard-read body runs
    c.place(l_fall);
    c.op(OP_POPSCOPE);
    let payload = c.finish();

    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.local_count = body.local_count.max(l_mask + 1);
    body.max_stack = body.max_stack.max(4);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Inject an EVENT-DRIVEN READY signal — the SSF2 analogue of Fraymakers firing READY
/// at boot complete. SSF2's boot plays a disclaimer video first, once all content is
/// loaded and the game is interactive; that IS the "ready" signal. We prepend a one-shot
/// `READY\n` emit to `DisclaimerMenu.checkDisclaimer` — the game's own recurring check
/// that watches the disclaimer movie play frame-by-frame (`m_subMenu.currentFrame` vs
/// `totalFrames`) until it ends. So the host waits for a real boot-complete event instead
/// of the old `wait_ready` PING-streak heuristic (a flat 6s floor + responsiveness
/// polling that fired a ways after loading visibly finished).
///
/// Because `checkDisclaimer` runs repeatedly while the disclaimer plays, the one-shot
/// `peptideReadySent` flag + the `peptideSock.connected` guard make this robust against
/// the loopback socket's async connect: READY fires on the FIRST frame the socket is
/// connected (essentially the start of the disclaimer) and never again. `checkDisclaimer`
/// is a `DisclaimerMenu` instance method, so it reaches the bridge socket (an instance
/// slot on the document, set up by `inject_socket_bridge`) through the document singleton
/// `Main.ROOT`. The connected guard also prevents an unconnected write from throwing and
/// aborting the boot.
pub fn inject_ready_signal(abc: &mut Abc, doc_class_local: &str) -> anyhow::Result<()> {
    let doc_ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;

    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    // getlex needs the class as QName(package, localName) — split the FQN (e.g.
    // "com.mcleodgaming.ssf2.Main" → ns "com.mcleodgaming.ssf2", name "Main"). A dotted
    // name in the empty namespace would throw a ReferenceError and crash the boot.
    let (pkg, local) = doc_class_local.rsplit_once('.').unwrap_or(("", doc_class_local));
    let doc_ns = { let s = abc.intern_string(pkg); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_main = { let s = abc.intern_string(local); abc.intern_qname(doc_ns, s) };
    let mn_root = q(abc, pub_ns, "ROOT");            // Main.ROOT = the document instance
    let mn_sock = q(abc, pub_ns, "peptideSock");     // the bridge socket (instance slot)
    let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_flush = q(abc, pub_ns, "flush");
    let mn_readysent = q(abc, pub_ns, "peptideReadySent"); // one-shot guard flag
    let mn_connected = q(abc, pub_ns, "connected");        // Socket.connected (Boolean)
    let s_ready = abc.intern_string("READY\n");

    // one-shot flag slot on the document instance (defaults to undefined → falsy)
    abc.add_instance_slot(doc_ci, mn_readysent);

    // hook DisclaimerMenu.checkDisclaimer — the recurring check that runs while the boot
    // disclaimer video plays (resolved by NAME, version-resilient; it's an instance method).
    let disc_ci = abc.find_class_by_name("DisclaimerMenu")
        .ok_or_else(|| anyhow::anyhow!("DisclaimerMenu not found"))?;
    let method = abc.instances[disc_ci].traits.iter().find_map(|t| match t.data {
        TraitKindData::Method { method, .. }
            if abc.multiname_local(t.name).as_deref() == Some("checkDisclaimer") => Some(method),
        _ => None,
    }).ok_or_else(|| anyhow::anyhow!("DisclaimerMenu.checkDisclaimer not found"))?;
    let body_idx = abc.bodies.iter().position(|b| b.method == method)
        .ok_or_else(|| anyhow::anyhow!("no body for checkDisclaimer"))?;

    // payload (prepended — uniform shift keeps relative branches valid). CRITICAL: every
    // dereference is null-guarded so this code can NEVER throw. A prepend that throws would
    // skip checkDisclaimer's own body (its `nextMenu()` advance), leaving the disclaimer
    // looping forever. With the guards it always falls through to the original body, so the
    // disclaimer behaves exactly as stock until READY fires and the autostart spawn replaces
    // it. We touch only the document singleton (Main.ROOT.*), never checkDisclaimer's `this`:
    //   if (Main.ROOT != null && !Main.ROOT.peptideReadySent
    //       && Main.ROOT.peptideSock != null && Main.ROOT.peptideSock.connected) {
    //       Main.ROOT.peptideReadySent = true;
    //       Main.ROOT.peptideSock.writeUTFBytes("READY\n"); Main.ROOT.peptideSock.flush();
    //   }
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    let l_skip = c.new_label();
    // if (Main.ROOT == null) skip — ROOT is assigned in Main's ctor, but guard anyway.
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root);
    c.branch(OP_IFFALSE, l_skip);
    // if (Main.ROOT.peptideReadySent) skip — already sent once.
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root); c.op_u30(OP_GETPROPERTY, mn_readysent);
    c.branch(OP_IFTRUE, l_skip);
    // if (Main.ROOT.peptideSock == null) skip — socket slot not set yet.
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root); c.op_u30(OP_GETPROPERTY, mn_sock);
    c.branch(OP_IFFALSE, l_skip);
    // if (!Main.ROOT.peptideSock.connected) skip — writing to an unconnected socket throws.
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root); c.op_u30(OP_GETPROPERTY, mn_sock);
    c.op_u30(OP_GETPROPERTY, mn_connected);
    c.branch(OP_IFFALSE, l_skip);
    // peptideReadySent = true (set BEFORE the write, so a write-throw can't cause a re-emit)
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root); c.op(OP_PUSHTRUE); c.op_u30(OP_SETPROPERTY, mn_readysent);
    // peptideSock.writeUTFBytes("READY\n"); peptideSock.flush();
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root); c.op_u30(OP_GETPROPERTY, mn_sock);
    c.op_u30(OP_PUSHSTRING, s_ready); c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op_u30(OP_GETLEX, mn_main); c.op_u30(OP_GETPROPERTY, mn_root); c.op_u30(OP_GETPROPERTY, mn_sock);
    c.op_u30_u30(OP_CALLPROPVOID, mn_flush, 0);
    c.place(l_skip);
    c.op(OP_POPSCOPE);
    let payload = c.finish();

    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(3);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// QUICK BOOT: replace `MenuController.showInitialMenu`'s body so that, instead of showing
/// the disclaimer (and the whole menu chain after it), it QUEUES the match's character +
/// stage for loading and returns. This is the engine half of SSF2's headless fast-boot — the
/// SSF2 analogue of skipping Fraymakers' Title. We branch at the CALL SITE (showInitialMenu =
/// `MenuController.disclaimerMenu.show()`), NOT inside DisclaimerMenu: a fast-boot patch
/// rewrites this one method so the disclaimer is never reached. The boot's loading screen
/// then decrypts the queued char/stage alongside the required resources, and the host spawns
/// straight into the match once loading settles.
///
/// `char_id`/`stage_id` are the resource ids (the same ids the SPAWN verb queues). Queuing an
/// unknown id is a safe no-op in `queueResources`. This replaces the body wholesale (the
/// original is ~6 ops: `disclaimerMenu.show()`), so the disclaimer call simply isn't emitted.
pub fn inject_quickboot(abc: &mut Abc, char_id: &str, stage_id: &str) -> anyhow::Result<()> {
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let util_ns = { let s = abc.intern_string(SSF2_NS_UTIL); abc.intern_namespace(NS_PACKAGE, s) };
    let ctrl_ns = { let s = abc.intern_string(SSF2_NS_CONTROLLERS); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    let mn_rm = q(abc, util_ns, "ResourceManager");
    let mn_queueres = q(abc, pub_ns, "queueResources");
    let mn_menuctrl = q(abc, ctrl_ns, "MenuController");
    let mn_loadingmenu = q(abc, pub_ns, "loadingMenu");
    let mn_show = q(abc, pub_ns, "show");
    let s_char = abc.intern_string(char_id);
    let s_stage = abc.intern_string(stage_id);

    // resolve MenuController.showInitialMenu (static method) by NAME (version-resilient).
    let menu_ci = abc.find_class_by_name("MenuController")
        .ok_or_else(|| anyhow::anyhow!("MenuController not found"))?;
    let method = abc.classes[menu_ci].traits.iter().find_map(|t| match t.data {
        TraitKindData::Method { method, .. }
            if abc.multiname_local(t.name).as_deref() == Some("showInitialMenu") => Some(method),
        _ => None,
    }).ok_or_else(|| anyhow::anyhow!("MenuController.showInitialMenu not found"))?;
    let body_idx = abc.bodies.iter().position(|b| b.method == method)
        .ok_or_else(|| anyhow::anyhow!("no body for showInitialMenu"))?;

    // NEW body:
    //   ResourceManager.queueResources([char]); ResourceManager.queueResources([stage]);
    //   MenuController.loadingMenu.show();   // keep a loading screen up (no black screen)
    //   return;
    // The host kills the loading screen (disposeAllMenus) once the match is live.
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_PUSHSTRING, s_char); c.op_u30(OP_NEWARRAY, 1); c.op_u30_u30(OP_CALLPROPVOID, mn_queueres, 1);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_PUSHSTRING, s_stage); c.op_u30(OP_NEWARRAY, 1); c.op_u30_u30(OP_CALLPROPVOID, mn_queueres, 1);
    c.op_u30(OP_GETLEX, mn_menuctrl); c.op_u30(OP_GETPROPERTY, mn_loadingmenu); c.op_u30_u30(OP_CALLPROPVOID, mn_show, 0);
    c.op(OP_RETURNVOID);
    let code = c.finish();

    let body = &mut abc.bodies[body_idx];
    body.code = code;
    body.max_stack = body.max_stack.max(2);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(1);
    body.exceptions.clear(); // wholesale replace — drop the old body's handlers/offsets
    Ok(())
}

/// Inject a per-frame JUMP CAPTURE PROBE: once a match is live
/// (GameController.stageData != null), append "<t>,<X>,<Y>,<YSpeed>\n" for the
/// character at `char_index` to `traj_path` every frame. Null-guarded so it's
/// silent at the menu. The host clears the traj file, triggers a jump (via the
/// reflection bridge's SETP YSpeed), waits, and reads the trajectory.
pub fn inject_jump_probe(abc: &mut Abc, doc_class_local: &str, traj_path: &str, char_index: u8) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;
    let fs_ns = { let s = abc.intern_string("flash.filesystem"); abc.intern_namespace(NS_PACKAGE, s) };
    let utils_ns = { let s = abc.intern_string("flash.utils"); abc.intern_namespace(NS_PACKAGE, s) };
    let events_ns = { let s = abc.intern_string("flash.events"); abc.intern_namespace(NS_PACKAGE, s) };
    let ctrl_ns = { let s = abc.intern_string(SSF2_NS_CONTROLLERS); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    let mn_file = q(abc, fs_ns, "File");
    let mn_fstream = q(abc, fs_ns, "FileStream");
    let mn_fmode = q(abc, fs_ns, "FileMode");
    let mn_append = q(abc, pub_ns, "APPEND");
    let mn_open = q(abc, pub_ns, "open");
    let mn_close = q(abc, pub_ns, "close");
    let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_gc = q(abc, ctrl_ns, "GameController");
    let mn_stagedata = q(abc, pub_ns, "stageData");
    let mn_characters = q(abc, pub_ns, "Characters");
    let mn_x = q(abc, pub_ns, "X");
    let mn_y = q(abc, pub_ns, "Y");
    let mn_yspeed = q(abc, pub_ns, "YSpeed");
    let mn_gettimer = q(abc, utils_ns, "getTimer");
    let mn_event = q(abc, events_ns, "Event");
    let mn_enterframe = q(abc, pub_ns, "ENTER_FRAME");
    let mn_addel = q(abc, pub_ns, "addEventListener");
    let mn_probe = q(abc, pub_ns, "peptideJumpProbe");
    let n_probe = abc.intern_string("peptideJumpProbe");
    let pub_nsset = abc.intern_ns_set(vec![pub_ns]);
    let mnl = abc.intern_multinamel(pub_nsset);
    let s_traj = abc.intern_string(traj_path);
    let s_comma = abc.intern_string(",");
    let s_nl = abc.intern_string("\n");

    // locals: this=0 event=1 sd=2 c0=3 fileObj=4 fs=5
    let (l_sd, l_c0, l_fo, l_fs) = (2u32, 3, 4, 5);
    let mut c = Code::default();
    let l_skip = c.new_label();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    // sd = GameController.stageData ; if null -> skip
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata); c.op_u30(OP_SETLOCAL, l_sd);
    c.op_u30(OP_GETLOCAL, l_sd); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_skip);
    // c0 = sd.Characters[char_index]
    c.op_u30(OP_GETLOCAL, l_sd); c.op_u30(OP_GETPROPERTY, mn_characters);
    c.op(OP_PUSHBYTE); c.op(char_index); c.op_u30(OP_GETPROPERTY, mnl); c.op_u30(OP_SETLOCAL, l_c0);
    // open traj (APPEND)
    c.op_u30(OP_FINDPROPSTRICT, mn_file); c.op_u30(OP_PUSHSTRING, s_traj); c.op_u30_u30(OP_CONSTRUCTPROP, mn_file, 1); c.op_u30(OP_SETLOCAL, l_fo);
    c.op_u30(OP_FINDPROPSTRICT, mn_fstream); c.op_u30_u30(OP_CONSTRUCTPROP, mn_fstream, 0); c.op_u30(OP_SETLOCAL, l_fs);
    c.op_u30(OP_GETLOCAL, l_fs); c.op_u30(OP_GETLOCAL, l_fo); c.op_u30(OP_GETLEX, mn_fmode); c.op_u30(OP_GETPROPERTY, mn_append); c.op_u30_u30(OP_CALLPROPVOID, mn_open, 2);
    // fs.writeUTFBytes( getTimer + "," + c0.X + "," + c0.Y + "," + c0.YSpeed + "\n" )
    c.op_u30(OP_GETLOCAL, l_fs);
    c.op_u30(OP_FINDPROPSTRICT, mn_gettimer); c.op_u30_u30(OP_CALLPROPERTY, mn_gettimer, 0); c.op(OP_CONVERT_S);
    c.op_u30(OP_PUSHSTRING, s_comma); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_x); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_comma); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_y); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_comma); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_yspeed); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_nl); c.op(OP_ADD);
    c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op_u30(OP_GETLOCAL, l_fs); c.op_u30_u30(OP_CALLPROPVOID, mn_close, 0);
    c.place(l_skip);
    c.op(OP_RETURNVOID);

    let probe = abc.add_method(MethodInfo { param_types: vec![0], return_type: 0, name: n_probe, flags: 0, options: vec![], param_names: vec![] });
    abc.add_body(MethodBody { method: probe, max_stack: 6, local_count: 6, init_scope_depth: 0, max_scope_depth: 2, code: c.finish(), exceptions: vec![], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_probe, probe);

    // install listener in ctor
    let mut ic = Code::default();
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHSCOPE);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETLEX, mn_event); ic.op_u30(OP_GETPROPERTY, mn_enterframe);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_probe);
    ic.op_u30_u30(OP_CALLPROPVOID, mn_addel, 2);
    ic.op(OP_POPSCOPE);
    let payload = ic.finish();
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit).ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(3);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Per-frame ANIMATION RECORDER — the engine PUSHES its own frame-by-frame state over the
/// bridge socket, exactly the way Fraymakers does.
///
/// This mirrors `main.rs::inject_anim_telemetry` on the Fraymakers side: there, a hook on
/// `Character.toState` writes `ANIM:<state>` to the harness socket, the host's reader routes
/// it by prefix, and nothing polls. SSF2's bridge was pure request/response with nothing
/// engine-initiated, so the host had to poll for the same information — and polling loses,
/// for two reasons that are easy to mistake for each other:
///   * it races the move. A 7-frame attack is ~233ms and the round trip between "send input"
///     and "start watching" is the same order, so the watcher arrives after it's over.
///     Measured: the recorder captured `a x7` in a window where a concurrent poll saw only
///     `stand`.
///   * it can't see a frame it didn't sample, and no sample rate fixes the race above.
/// The engine is present at every frame by definition, so it does the observing.
///
/// Emits `FRAME:<label>|<frame>|<x>|<y>` for character `char_index` each ENTER_FRAME on the
/// SAME socket the command protocol uses (`peptideSock`). That's safe because the host's
/// response reader is seq-matched and already skips non-matching lines; it now routes them
/// by prefix instead of dropping them, which is precisely what the Fraymakers stream pump
/// does. Writing to the socket rather than a file also drops the per-frame FileStream IO
/// this first used, and with it the temp file the host had to truncate to open a window.
pub fn inject_frame_recorder(abc: &mut Abc, doc_class_local: &str, char_index: u8) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;
    let events_ns = { let s = abc.intern_string("flash.events"); abc.intern_namespace(NS_PACKAGE, s) };
    let ctrl_ns = { let s = abc.intern_string(SSF2_NS_CONTROLLERS); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };

    let mn_gc = q(abc, ctrl_ns, "GameController");
    let mn_stagedata = q(abc, pub_ns, "stageData");
    let mn_characters = q(abc, pub_ns, "Characters");
    let mn_mc = q(abc, pub_ns, "MC");
    let mn_curlabel = q(abc, pub_ns, "currentLabel");
    let mn_curframe = q(abc, pub_ns, "currentFrame");
    let mn_x = q(abc, pub_ns, "X");
    let mn_y = q(abc, pub_ns, "Y");
    let mn_event = q(abc, events_ns, "Event");
    let mn_enterframe = q(abc, pub_ns, "ENTER_FRAME");
    let mn_addel = q(abc, pub_ns, "addEventListener");
    let mn_sock = q(abc, pub_ns, "peptideSock");
    let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_flush = q(abc, pub_ns, "flush");
    let mn_rec = q(abc, pub_ns, "peptideFrameRec");
    let n_rec = abc.intern_string("peptideFrameRec");
    let pub_nsset = abc.intern_ns_set(vec![pub_ns]);
    let mnl = abc.intern_multinamel(pub_nsset);
    let s_tag = abc.intern_string("FRAME:");
    let s_bar = abc.intern_string("|");
    let s_nl = abc.intern_string("\n");

    // locals: this=0 event=1 sd=2 c0=3 mc=4
    let (l_sd, l_c0, l_mc) = (2u32, 3, 4);
    let mut c = Code::default();
    let l_skip = c.new_label();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    // if (this.peptideSock == null) return;  — not connected yet, nobody to push to
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_skip);
    // sd = GameController.stageData; if (sd == null) return;
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata); c.op_u30(OP_SETLOCAL, l_sd);
    c.op_u30(OP_GETLOCAL, l_sd); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_skip);
    // c0 = sd.Characters[char_index]; if (c0 == null) return;
    c.op_u30(OP_GETLOCAL, l_sd); c.op_u30(OP_GETPROPERTY, mn_characters);
    c.op(OP_PUSHBYTE); c.op(char_index); c.op_u30(OP_GETPROPERTY, mnl); c.op_u30(OP_SETLOCAL, l_c0);
    c.op_u30(OP_GETLOCAL, l_c0); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_skip);
    // mc = c0.MC; if (mc == null) return;
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_mc); c.op_u30(OP_SETLOCAL, l_mc);
    c.op_u30(OP_GETLOCAL, l_mc); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_skip);
    // peptideSock.writeUTFBytes("FRAME:" + label +"|"+ frame +"|"+ X +"|"+ Y + "\n"); flush()
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock);
    c.op_u30(OP_PUSHSTRING, s_tag);
    c.op_u30(OP_GETLOCAL, l_mc); c.op_u30(OP_GETPROPERTY, mn_curlabel); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_mc); c.op_u30(OP_GETPROPERTY, mn_curframe); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_x); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_bar); c.op(OP_ADD);
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_y); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_nl); c.op(OP_ADD);
    c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock); c.op_u30_u30(OP_CALLPROPVOID, mn_flush, 0);
    c.place(l_skip);
    c.op(OP_RETURNVOID);

    let rec = abc.add_method(MethodInfo {
        param_types: vec![0], return_type: 0, name: n_rec, flags: 0, options: vec![], param_names: vec![],
    });
    abc.add_body(MethodBody {
        method: rec, max_stack: 8, local_count: 6, init_scope_depth: 0, max_scope_depth: 2,
        code: c.finish(), exceptions: vec![], traits: vec![],
    });
    abc.add_instance_method_trait(ci, mn_rec, rec);

    let mut ic = Code::default();
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHSCOPE);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETLEX, mn_event); ic.op_u30(OP_GETPROPERTY, mn_enterframe);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_rec);
    ic.op_u30_u30(OP_CALLPROPVOID, mn_addel, 2);
    ic.op(OP_POPSCOPE);
    let payload = ic.finish();
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit)
        .ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(3);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Global UNCAUGHT-ERROR reporting: mirror every engine-side error onto the bridge socket
/// as a `SCRIPTERR:` line, the same channel Fraymakers has.
///
/// The asymmetry this closes is not cosmetic. Fraymakers mirrors every engine error to the
/// harness socket, which is what `tools/tests/script_error_scan.sh` reads; SSF2 had no
/// engine-error channel at all, so the same scan had nothing to see and a converted SSF2
/// character could throw all day in silence. `ERR:` is NOT the equivalent — that is a
/// command REPLY ("your request failed"), not unsolicited telemetry about the engine's own
/// code.
///
/// AIR exposes `loaderInfo.uncaughtErrorEvents`, and the runtime provides it whether or not
/// the SWF ever references it (SSF2's constant pool has none of these names — we intern
/// them). Registering there catches errors thrown anywhere the engine didn't handle them,
/// including inside loaded character content.
pub fn inject_error_reporter(abc: &mut Abc, doc_class_local: &str) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local)
        .ok_or_else(|| anyhow::anyhow!("class {doc_class_local} not found"))?;
    let events_ns = { let s = abc.intern_string("flash.events"); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };

    let mn_uee = q(abc, events_ns, "UncaughtErrorEvent");
    let mn_uncaught = q(abc, pub_ns, "UNCAUGHT_ERROR");
    let mn_loaderinfo = q(abc, pub_ns, "loaderInfo");
    let mn_ueevents = q(abc, pub_ns, "uncaughtErrorEvents");
    let mn_addel = q(abc, pub_ns, "addEventListener");
    let mn_text = q(abc, pub_ns, "text");
    let mn_sock = q(abc, pub_ns, "peptideSock");
    let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_flush = q(abc, pub_ns, "flush");
    let mn_h = q(abc, pub_ns, "peptideOnError");
    let n_h = abc.intern_string("peptideOnError");
    let s_tag = abc.intern_string("SCRIPTERR: ");
    let s_nl = abc.intern_string("\n");

    // handler(e): if (peptideSock != null) peptideSock.writeUTFBytes("SCRIPTERR: " + e.text + "\n")
    let mut c = Code::default();
    let l_skip = c.new_label();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_skip);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock);
    c.op_u30(OP_PUSHSTRING, s_tag);
    c.op_u30(OP_GETLOCAL, 1); c.op_u30(OP_GETPROPERTY, mn_text); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30(OP_PUSHSTRING, s_nl); c.op(OP_ADD);
    c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_sock); c.op_u30_u30(OP_CALLPROPVOID, mn_flush, 0);
    c.place(l_skip);
    c.op(OP_RETURNVOID);

    let h = abc.add_method(MethodInfo {
        param_types: vec![0], return_type: 0, name: n_h, flags: 0, options: vec![], param_names: vec![],
    });
    abc.add_body(MethodBody {
        method: h, max_stack: 6, local_count: 3, init_scope_depth: 0, max_scope_depth: 2,
        code: c.finish(), exceptions: vec![], traits: vec![],
    });
    abc.add_instance_method_trait(ci, mn_h, h);

    // ctor: this.loaderInfo.uncaughtErrorEvents.addEventListener(UNCAUGHT_ERROR, this.peptideOnError)
    let mut ic = Code::default();
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHSCOPE);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_loaderinfo); ic.op_u30(OP_GETPROPERTY, mn_ueevents);
    ic.op_u30(OP_GETLEX, mn_uee); ic.op_u30(OP_GETPROPERTY, mn_uncaught);
    ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_h);
    ic.op_u30_u30(OP_CALLPROPVOID, mn_addel, 2);
    ic.op(OP_POPSCOPE);
    let payload = ic.finish();
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit)
        .ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(4);
    body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// Timer-driven load TEST (no per-frame handler): the constructor schedules
/// `peptideLoadKick` (build Game + queue sandbag+stage + multimode load) once via
/// setTimeout, and `peptideLoadCheck` every 1.5s via setInterval, which writes
/// "LIB:<getLibraryMC> STATS:<m_statObjects[char]>" to `marker`. Isolates whether
/// the resource parse completes WITHOUT a per-frame FileStream-IO command loop
/// competing for the event loop (the game itself loads via timers, not per-frame).
pub fn inject_load_test(abc: &mut Abc, doc_class_local: &str, marker: &str, char_id: &str, stage_id: &str) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local).ok_or_else(|| anyhow::anyhow!("class not found"))?;
    let fs_ns = { let s = abc.intern_string("flash.filesystem"); abc.intern_namespace(NS_PACKAGE, s) };
    let utils_ns = { let s = abc.intern_string("flash.utils"); abc.intern_namespace(NS_PACKAGE, s) };
    let ctrl_ns = { let s = abc.intern_string(SSF2_NS_CONTROLLERS); abc.intern_namespace(NS_PACKAGE, s) };
    let enums_ns = { let s = abc.intern_string(SSF2_NS_ENUMS); abc.intern_namespace(NS_PACKAGE, s) };
    let engine_ns = { let s = abc.intern_string(SSF2_NS_ENGINE); abc.intern_namespace(NS_PACKAGE, s) };
    let util_ns = { let s = abc.intern_string(SSF2_NS_UTIL); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    let mn_file = q(abc, fs_ns, "File"); let mn_fstream = q(abc, fs_ns, "FileStream"); let mn_fmode = q(abc, fs_ns, "FileMode");
    let mn_write = q(abc, pub_ns, "WRITE"); let mn_open = q(abc, pub_ns, "open"); let mn_close = q(abc, pub_ns, "close"); let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let mn_game = q(abc, ctrl_ns, "Game"); let mn_mode = q(abc, enums_ns, "Mode"); let mn_training = q(abc, pub_ns, "TRAINING");
    let mn_gc = q(abc, ctrl_ns, "GameController"); let mn_rm = q(abc, util_ns, "ResourceManager"); let mn_stats = q(abc, engine_ns, "Stats");
    let mn_currentgame = q(abc, pub_ns, "currentGame"); let mn_leveldata = q(abc, pub_ns, "LevelData"); let mn_stage = q(abc, pub_ns, "stage");
    let mn_playersettings = q(abc, pub_ns, "PlayerSettings"); let mn_character = q(abc, pub_ns, "character"); let mn_human = q(abc, pub_ns, "human"); let mn_costume = q(abc, pub_ns, "costume");
    let mn_flushqueue = q(abc, pub_ns, "flushLoadQueue"); let mn_queueres = q(abc, pub_ns, "queueResources"); let mn_load = q(abc, pub_ns, "load");
    let mn_getlibmc = q(abc, pub_ns, "getLibraryMC"); let mn_statobjs = q(abc, pub_ns, "m_statObjects");
    let mn_settimeout = q(abc, utils_ns, "setTimeout"); let mn_setinterval = q(abc, utils_ns, "setInterval");
    let mn_kick = q(abc, pub_ns, "peptideLoadKick"); let n_kick = abc.intern_string("peptideLoadKick");
    let mn_check = q(abc, pub_ns, "peptideLoadCheck"); let n_check = abc.intern_string("peptideLoadCheck");
    let pub_nsset = abc.intern_ns_set(vec![pub_ns]); let mnl = abc.intern_multinamel(pub_nsset);
    let s_char = abc.intern_string(char_id); let s_stage = abc.intern_string(stage_id);
    let s_multimode = abc.intern_string("multimode"); let s_marker = abc.intern_string(marker);
    let s_libpfx = abc.intern_string("LIB:"); let s_statpfx = abc.intern_string(" STATS:"); let s_stagelib = abc.intern_string(&format!("stage_{stage_id}"));

    // ── peptideLoadKick(): build Game(1,TRAINING) + set stage/char + flush + queue + multimode load ──
    let (l_g, l_ps) = (1u32, 2u32);
    let mut k = Code::default();
    k.op(OP_GETLOCAL0); k.op(OP_PUSHSCOPE);
    k.op_u30(OP_FINDPROPSTRICT, mn_game); k.op(OP_PUSHBYTE); k.op(1); k.op_u30(OP_GETLEX, mn_mode); k.op_u30(OP_GETPROPERTY, mn_training); k.op_u30_u30(OP_CONSTRUCTPROP, mn_game, 2); k.op_u30(OP_SETLOCAL, l_g);
    k.op_u30(OP_GETLOCAL, l_g); k.op_u30(OP_GETPROPERTY, mn_leveldata); k.op_u30(OP_PUSHSTRING, s_stage); k.op_u30(OP_SETPROPERTY, mn_stage);
    k.op_u30(OP_GETLOCAL, l_g); k.op_u30(OP_GETPROPERTY, mn_playersettings); k.op(OP_PUSHBYTE); k.op(0); k.op_u30(OP_GETPROPERTY, mnl); k.op_u30(OP_SETLOCAL, l_ps);
    k.op_u30(OP_GETLOCAL, l_ps); k.op_u30(OP_PUSHSTRING, s_char); k.op_u30(OP_SETPROPERTY, mn_character);
    k.op_u30(OP_GETLOCAL, l_ps); k.op(OP_PUSHTRUE); k.op_u30(OP_SETPROPERTY, mn_human);
    k.op_u30(OP_GETLOCAL, l_ps); k.op(OP_PUSHBYTE); k.op(0); k.op_u30(OP_SETPROPERTY, mn_costume);
    k.op_u30(OP_GETLEX, mn_gc); k.op_u30(OP_GETLOCAL, l_g); k.op_u30(OP_SETPROPERTY, mn_currentgame);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30_u30(OP_CALLPROPVOID, mn_flushqueue, 0);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30(OP_PUSHSTRING, s_stage); k.op_u30(OP_NEWARRAY, 1); k.op_u30_u30(OP_CALLPROPVOID, mn_queueres, 1);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30(OP_PUSHSTRING, s_char); k.op_u30(OP_NEWARRAY, 1); k.op_u30_u30(OP_CALLPROPVOID, mn_queueres, 1);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30(OP_PUSHSTRING, s_multimode); k.op(OP_PUSHTRUE); k.op_u30(OP_NEWOBJECT, 1); k.op_u30_u30(OP_CALLPROPVOID, mn_load, 1);
    k.op(OP_RETURNVOID);
    let m_kick = abc.add_method(MethodInfo { param_types: vec![], return_type: 0, name: n_kick, flags: 0, options: vec![], param_names: vec![] });
    abc.add_body(MethodBody { method: m_kick, max_stack: 5, local_count: 3, init_scope_depth: 0, max_scope_depth: 2, code: k.finish(), exceptions: vec![], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_kick, m_kick);

    // ── peptideLoadCheck(): write "LIB:<getLibraryMC> STATS:<m_statObjects[char]>" ──
    let (l_fo, l_fs, l_lib) = (1u32, 2u32, 3u32);
    let mut c = Code::default();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    // lib = String(ResourceManager.getLibraryMC("stage_<id>"))
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_PUSHSTRING, s_stagelib); c.op_u30_u30(OP_CALLPROPERTY, mn_getlibmc, 1); c.op(OP_CONVERT_S); c.op_u30(OP_SETLOCAL, l_lib);
    // open marker (WRITE)
    c.op_u30(OP_FINDPROPSTRICT, mn_file); c.op_u30(OP_PUSHSTRING, s_marker); c.op_u30_u30(OP_CONSTRUCTPROP, mn_file, 1); c.op_u30(OP_SETLOCAL, l_fo);
    c.op_u30(OP_FINDPROPSTRICT, mn_fstream); c.op_u30_u30(OP_CONSTRUCTPROP, mn_fstream, 0); c.op_u30(OP_SETLOCAL, l_fs);
    c.op_u30(OP_GETLOCAL, l_fs); c.op_u30(OP_GETLOCAL, l_fo); c.op_u30(OP_GETLEX, mn_fmode); c.op_u30(OP_GETPROPERTY, mn_write); c.op_u30_u30(OP_CALLPROPVOID, mn_open, 2);
    // writeUTFBytes("LIB:" + lib)  — stats omitted (m_statObjects may be null → throw)
    let _ = (mn_stats, mn_statobjs, s_statpfx);
    c.op_u30(OP_GETLOCAL, l_fs);
    c.op_u30(OP_PUSHSTRING, s_libpfx); c.op_u30(OP_GETLOCAL, l_lib); c.op(OP_ADD);
    c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op_u30(OP_GETLOCAL, l_fs); c.op_u30_u30(OP_CALLPROPVOID, mn_close, 0);
    c.op(OP_RETURNVOID);
    let m_check = abc.add_method(MethodInfo { param_types: vec![], return_type: 0, name: n_check, flags: 0, options: vec![], param_names: vec![] });
    abc.add_body(MethodBody { method: m_check, max_stack: 6, local_count: 4, init_scope_depth: 0, max_scope_depth: 2, code: c.finish(), exceptions: vec![], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_check, m_check);

    // ── constructor: setTimeout(this.peptideLoadKick, 3000); setInterval(this.peptideLoadCheck, 1500) ──
    let mut ic = Code::default();
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHSCOPE);
    ic.op_u30(OP_FINDPROPSTRICT, mn_settimeout); ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_kick); ic.op_u30(OP_PUSHSHORT, 3000); ic.op_u30_u30(OP_CALLPROPVOID, mn_settimeout, 2);
    ic.op_u30(OP_FINDPROPSTRICT, mn_setinterval); ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_check); ic.op_u30(OP_PUSHSHORT, 1500); ic.op_u30_u30(OP_CALLPROPVOID, mn_setinterval, 2);
    ic.op(OP_POPSCOPE);
    let payload = ic.finish();
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit).ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(4); body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

const OP_NEGATE: u8 = 0x90;
const OP_CONVERT_B: u8 = 0x76;
const OP_IFLT: u8 = 0x15;

/// Full TIMER-DRIVEN autospawn (no per-frame command handler — that starves the
/// async resource Loader). Constructor schedules:
///   setTimeout(peptideKick, delay): build Game(1,TRAINING)+sandbag/stage, flush,
///     queue stage+char, ResourceManager.load({multimode:true}).
///   setInterval(peptideTick, 250ms): a state machine —
///     state 0: wait until getLibraryMC(stage) && Stats.m_statObjects[char] ready,
///              then GameController.startMatch(currentGame); state=1.
///     state 1: wait until GameController.stageData != null, then jump the
///              character: Characters[0].YSpeed = -CharacterStats.JumpSpeed; state=2.
///     state 2: done. (Pair with inject_jump_probe to log the trajectory.)
pub fn inject_autospawn(abc: &mut Abc, doc_class_local: &str, char_id: &str, stage_id: &str, delay_ms: u32) -> anyhow::Result<()> {
    let ci = abc.find_class_by_name(doc_class_local).ok_or_else(|| anyhow::anyhow!("class not found"))?;
    let utils_ns = { let s = abc.intern_string("flash.utils"); abc.intern_namespace(NS_PACKAGE, s) };
    let ctrl_ns = { let s = abc.intern_string(SSF2_NS_CONTROLLERS); abc.intern_namespace(NS_PACKAGE, s) };
    let enums_ns = { let s = abc.intern_string(SSF2_NS_ENUMS); abc.intern_namespace(NS_PACKAGE, s) };
    let engine_ns = { let s = abc.intern_string(SSF2_NS_ENGINE); abc.intern_namespace(NS_PACKAGE, s) };
    let util_ns = { let s = abc.intern_string(SSF2_NS_UTIL); abc.intern_namespace(NS_PACKAGE, s) };
    let pub_ns = { let s = abc.intern_string(""); abc.intern_namespace(NS_PACKAGE, s) };
    let q = |abc: &mut Abc, ns: u32, nm: &str| { let s = abc.intern_string(nm); abc.intern_qname(ns, s) };
    let mn_game = q(abc, ctrl_ns, "Game"); let mn_mode = q(abc, enums_ns, "Mode"); let mn_training = q(abc, pub_ns, "TRAINING");
    let mn_gc = q(abc, ctrl_ns, "GameController"); let mn_rm = q(abc, util_ns, "ResourceManager"); let mn_stats = q(abc, engine_ns, "Stats");
    let mn_currentgame = q(abc, pub_ns, "currentGame"); let mn_leveldata = q(abc, pub_ns, "LevelData"); let mn_stage = q(abc, pub_ns, "stage");
    let mn_playersettings = q(abc, pub_ns, "PlayerSettings"); let mn_character = q(abc, pub_ns, "character"); let mn_human = q(abc, pub_ns, "human"); let mn_costume = q(abc, pub_ns, "costume");
    let mn_flushqueue = q(abc, pub_ns, "flushLoadQueue"); let mn_queueres = q(abc, pub_ns, "queueResources"); let mn_load = q(abc, pub_ns, "load");
    let mn_getlibmc = q(abc, pub_ns, "getLibraryMC"); let mn_statobjs = q(abc, pub_ns, "m_statObjects");
    let mn_startmatch = q(abc, pub_ns, "startMatch"); let mn_stagedata = q(abc, pub_ns, "stageData"); let mn_characters = q(abc, pub_ns, "Characters");
    let mn_charstats = q(abc, pub_ns, "CharacterStats"); let mn_jumpspeed = q(abc, pub_ns, "JumpSpeed"); let mn_yspeed = q(abc, pub_ns, "YSpeed");
    let mn_settimeout = q(abc, utils_ns, "setTimeout"); let mn_setinterval = q(abc, utils_ns, "setInterval");
    let mn_state = q(abc, pub_ns, "peptideState");
    let mn_wait = q(abc, pub_ns, "peptideWait");
    let mn_kick = q(abc, pub_ns, "peptideKick"); let n_kick = abc.intern_string("peptideKick");
    let mn_tick = q(abc, pub_ns, "peptideTick"); let n_tick = abc.intern_string("peptideTick");
    let pub_nsset = abc.intern_ns_set(vec![pub_ns]); let mnl = abc.intern_multinamel(pub_nsset);
    let s_char = abc.intern_string(char_id); let s_stage = abc.intern_string(stage_id);
    let s_multimode = abc.intern_string("multimode"); let s_stagelib = abc.intern_string(&format!("stage_{stage_id}"));
    // debug marker
    let fs_ns = { let s = abc.intern_string("flash.filesystem"); abc.intern_namespace(NS_PACKAGE, s) };
    let mn_file = q(abc, fs_ns, "File"); let mn_fstream = q(abc, fs_ns, "FileStream"); let mn_fmode = q(abc, fs_ns, "FileMode");
    let mn_write = q(abc, pub_ns, "WRITE"); let mn_open = q(abc, pub_ns, "open"); let mn_close = q(abc, pub_ns, "close"); let mn_writeutf = q(abc, pub_ns, "writeUTFBytes");
    let s_dbg = abc.intern_string("/tmp/peptide_ssf2_autodbg.txt");
    let s_sp = abc.intern_string(" lib=");
    let s_mpfx = abc.intern_string(" m=");

    abc.add_instance_slot(ci, mn_state);
    abc.add_instance_slot(ci, mn_wait);

    // peptideKick(): build + queue + load
    let (l_g, l_ps) = (1u32, 2u32);
    let mut k = Code::default();
    k.op(OP_GETLOCAL0); k.op(OP_PUSHSCOPE);
    k.op_u30(OP_FINDPROPSTRICT, mn_game); k.op(OP_PUSHBYTE); k.op(1); k.op_u30(OP_GETLEX, mn_mode); k.op_u30(OP_GETPROPERTY, mn_training); k.op_u30_u30(OP_CONSTRUCTPROP, mn_game, 2); k.op_u30(OP_SETLOCAL, l_g);
    k.op_u30(OP_GETLOCAL, l_g); k.op_u30(OP_GETPROPERTY, mn_leveldata); k.op_u30(OP_PUSHSTRING, s_stage); k.op_u30(OP_SETPROPERTY, mn_stage);
    k.op_u30(OP_GETLOCAL, l_g); k.op_u30(OP_GETPROPERTY, mn_playersettings); k.op(OP_PUSHBYTE); k.op(0); k.op_u30(OP_GETPROPERTY, mnl); k.op_u30(OP_SETLOCAL, l_ps);
    k.op_u30(OP_GETLOCAL, l_ps); k.op_u30(OP_PUSHSTRING, s_char); k.op_u30(OP_SETPROPERTY, mn_character);
    k.op_u30(OP_GETLOCAL, l_ps); k.op(OP_PUSHTRUE); k.op_u30(OP_SETPROPERTY, mn_human);
    k.op_u30(OP_GETLOCAL, l_ps); k.op(OP_PUSHBYTE); k.op(0); k.op_u30(OP_SETPROPERTY, mn_costume);
    k.op_u30(OP_GETLEX, mn_gc); k.op_u30(OP_GETLOCAL, l_g); k.op_u30(OP_SETPROPERTY, mn_currentgame);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30_u30(OP_CALLPROPVOID, mn_flushqueue, 0);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30(OP_PUSHSTRING, s_stage); k.op_u30(OP_NEWARRAY, 1); k.op_u30_u30(OP_CALLPROPVOID, mn_queueres, 1);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30(OP_PUSHSTRING, s_char); k.op_u30(OP_NEWARRAY, 1); k.op_u30_u30(OP_CALLPROPVOID, mn_queueres, 1);
    k.op_u30(OP_GETLEX, mn_rm); k.op_u30(OP_PUSHSTRING, s_multimode); k.op(OP_PUSHTRUE); k.op_u30(OP_NEWOBJECT, 1); k.op_u30_u30(OP_CALLPROPVOID, mn_load, 1);
    k.op(OP_RETURNVOID);
    let m_kick = abc.add_method(MethodInfo { param_types: vec![], return_type: 0, name: n_kick, flags: 0, options: vec![], param_names: vec![] });
    abc.add_body(MethodBody { method: m_kick, max_stack: 5, local_count: 3, init_scope_depth: 0, max_scope_depth: 2, code: k.finish(), exceptions: vec![], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_kick, m_kick);

    // peptideTick(): state machine. locals: this=0, tmp=1, c0=2
    let (l_tmp, l_c0) = (1u32, 2u32);
    let mut c = Code::default();
    let l_ret = c.new_label();
    let l_pop_ret = c.new_label();
    c.op(OP_GETLOCAL0); c.op(OP_PUSHSCOPE);
    // ---- DEBUG: write "S<state> lib=<getLibraryMC>" to autodbg each tick ----
    c.op_u30(OP_FINDPROPSTRICT, mn_file); c.op_u30(OP_PUSHSTRING, s_dbg); c.op_u30_u30(OP_CONSTRUCTPROP, mn_file, 1); c.op_u30(OP_SETLOCAL, l_c0);
    c.op_u30(OP_FINDPROPSTRICT, mn_fstream); c.op_u30_u30(OP_CONSTRUCTPROP, mn_fstream, 0); c.op_u30(OP_SETLOCAL, l_tmp);
    c.op_u30(OP_GETLOCAL, l_tmp); c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETLEX, mn_fmode); c.op_u30(OP_GETPROPERTY, mn_write); c.op_u30_u30(OP_CALLPROPVOID, mn_open, 2);
    c.op_u30(OP_GETLOCAL, l_tmp);
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_state); c.op(OP_CONVERT_S);
    c.op_u30(OP_PUSHSTRING, s_sp); c.op(OP_ADD);
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_PUSHSTRING, s_stagelib); c.op_u30_u30(OP_CALLPROPERTY, mn_getlibmc, 1); c.op(OP_CONVERT_S); c.op(OP_ADD);
    // + " m=" + String(Stats.m_statObjects)
    c.op_u30(OP_PUSHSTRING, s_mpfx); c.op(OP_ADD);
    c.op_u30(OP_GETLEX, mn_stats); c.op_u30(OP_GETPROPERTY, mn_statobjs); c.op(OP_CONVERT_S); c.op(OP_ADD);
    c.op_u30_u30(OP_CALLPROPVOID, mn_writeutf, 1);
    c.op_u30(OP_GETLOCAL, l_tmp); c.op_u30_u30(OP_CALLPROPVOID, mn_close, 0);
    // ---- state 1 check first (so a freshly-set state 1 also runs jump path) ----
    let l_s1 = c.new_label(); let l_after0 = c.new_label();
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_state); c.op(OP_PUSHBYTE); c.op(1); c.branch(OP_IFSTRICTEQ, l_s1);
    // ---- state 0: wait for load, then startMatch ----
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_state); c.op(OP_PUSHBYTE); c.op(0); c.branch(OP_IFSTRICTNE, l_after0);
    // if !getLibraryMC(stage) → ret  (stage library is the load-ready signal; the
    // char loads in the same queue. m_statObjects is private-ns, can't gate on it.)
    c.op_u30(OP_GETLEX, mn_rm); c.op_u30(OP_PUSHSTRING, s_stagelib); c.op_u30_u30(OP_CALLPROPERTY, mn_getlibmc, 1); c.op(OP_CONVERT_B); c.branch(OP_IFFALSE, l_ret);
    // stage is ready; the char loads after it in the queue. Wait ~20 ticks (5s)
    // so the char finishes before startMatch→makePlayer→getStats(char).
    let _ = (mn_statobjs, mnl);
    c.op(OP_GETLOCAL0); c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_wait); c.op(OP_PUSHBYTE); c.op(1); c.op(OP_ADD); c.op_u30(OP_SETPROPERTY, mn_wait); // wait++
    c.op(OP_GETLOCAL0); c.op_u30(OP_GETPROPERTY, mn_wait); c.op(OP_PUSHBYTE); c.op(20); c.branch(OP_IFLT, l_ret); // if wait < 20 → ret
    // stage+char ready → startMatch(currentGame); state=1
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_currentgame); c.op_u30_u30(OP_CALLPROPVOID, mn_startmatch, 1);
    c.op(OP_GETLOCAL0); c.op(OP_PUSHBYTE); c.op(1); c.op_u30(OP_SETPROPERTY, mn_state);
    c.branch(OP_JUMP, l_ret);
    c.place(l_after0); c.branch(OP_JUMP, l_ret); // states >=2: nothing
    // ---- state 1: wait for stageData, then jump ----
    c.place(l_s1);
    c.op_u30(OP_GETLEX, mn_gc); c.op_u30(OP_GETPROPERTY, mn_stagedata); c.op(OP_DUP); c.op(OP_PUSHNULL); c.branch(OP_IFSTRICTEQ, l_pop_ret);
    // c0 = stageData.Characters[0]
    c.op_u30(OP_GETPROPERTY, mn_characters); c.op(OP_PUSHBYTE); c.op(0); c.op_u30(OP_GETPROPERTY, mnl); c.op_u30(OP_SETLOCAL, l_c0);
    let _ = l_tmp;
    // c0.YSpeed = -(c0.CharacterStats.JumpSpeed)
    c.op_u30(OP_GETLOCAL, l_c0);
    c.op_u30(OP_GETLOCAL, l_c0); c.op_u30(OP_GETPROPERTY, mn_charstats); c.op_u30(OP_GETPROPERTY, mn_jumpspeed); c.op(OP_NEGATE);
    c.op_u30(OP_SETPROPERTY, mn_yspeed);
    c.op(OP_GETLOCAL0); c.op(OP_PUSHBYTE); c.op(2); c.op_u30(OP_SETPROPERTY, mn_state);
    c.branch(OP_JUMP, l_ret);
    // pop+ret label (when stageData dup is on stack and null)
    c.place(l_pop_ret); c.op(OP_POP);
    c.place(l_ret);
    c.op(OP_RETURNVOID);
    let m_tick = abc.add_method(MethodInfo { param_types: vec![], return_type: 0, name: n_tick, flags: 0, options: vec![], param_names: vec![] });
    abc.add_body(MethodBody { method: m_tick, max_stack: 6, local_count: 3, init_scope_depth: 0, max_scope_depth: 2, code: c.finish(), exceptions: vec![], traits: vec![] });
    abc.add_instance_method_trait(ci, mn_tick, m_tick);

    // constructor: state=0; setTimeout(peptideKick, delay); setInterval(peptideTick, 250)
    let mut ic = Code::default();
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHSCOPE);
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHBYTE); ic.op(0); ic.op_u30(OP_SETPROPERTY, mn_state);
    ic.op(OP_GETLOCAL0); ic.op(OP_PUSHBYTE); ic.op(0); ic.op_u30(OP_SETPROPERTY, mn_wait);
    ic.op_u30(OP_FINDPROPSTRICT, mn_settimeout); ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_kick); ic.op_u30(OP_PUSHSHORT, delay_ms); ic.op_u30_u30(OP_CALLPROPVOID, mn_settimeout, 2);
    ic.op_u30(OP_FINDPROPSTRICT, mn_setinterval); ic.op(OP_GETLOCAL0); ic.op_u30(OP_GETPROPERTY, mn_tick); ic.op_u30(OP_PUSHSHORT, 250); ic.op_u30_u30(OP_CALLPROPVOID, mn_setinterval, 2);
    ic.op(OP_POPSCOPE);
    let payload = ic.finish();
    let iinit = abc.instances[ci].iinit;
    let body_idx = abc.bodies.iter().position(|b| b.method == iinit).ok_or_else(|| anyhow::anyhow!("no ctor body"))?;
    let body = &mut abc.bodies[body_idx];
    prepend_code(body, &payload);
    body.max_stack = body.max_stack.max(4); body.max_scope_depth = body.max_scope_depth.max(body.init_scope_depth + 1).max(2);
    Ok(())
}

/// SWF tag code for DoABC2.
const TAG_DO_ABC2: u16 = 82;

/// Locate the single DoAbc2 tag inside a raw SWF tag stream and return
/// (tag_start, tag_total_len, abc_offset_within_stream, abc_len). The tag body
/// is: flags(u32 LE) + name(NUL-terminated) + abc bytes.
fn find_doabc2(tags: &[u8]) -> anyhow::Result<(usize, usize, usize, usize)> {
    let mut i = 0usize;
    while i + 2 <= tags.len() {
        let code_len = u16::from_le_bytes([tags[i], tags[i + 1]]);
        let code = code_len >> 6;
        let mut len = (code_len & 0x3f) as usize;
        let mut hdr = 2usize;
        if len == 0x3f {
            if i + 6 > tags.len() { anyhow::bail!("truncated long tag header"); }
            len = u32::from_le_bytes([tags[i + 2], tags[i + 3], tags[i + 4], tags[i + 5]]) as usize;
            hdr = 6;
        }
        let body_start = i + hdr;
        if body_start + len > tags.len() { anyhow::bail!("tag body exceeds stream"); }
        if code == TAG_DO_ABC2 {
            // body: flags(4) + name(NUL-terminated) + abc
            let body = &tags[body_start..body_start + len];
            let name_end = body[4..].iter().position(|&b| b == 0)
                .ok_or_else(|| anyhow::anyhow!("DoAbc2 name not NUL-terminated"))? + 4;
            let abc_off = body_start + name_end + 1;
            let abc_len = (body_start + len) - abc_off;
            return Ok((i, hdr + len, abc_off, abc_len));
        }
        i = body_start + len;
    }
    anyhow::bail!("no DoAbc2 tag found")
}

/// Read an SWF, inject the startup marker into `doc_class` ("…ssf2.Main"), and
/// write a patched SWF to `out`. Byte-splices the DoAbc2 tag so all other tags
/// are preserved verbatim; recompresses via the swf crate's raw-tag writer.
pub fn patch_file(
    in_swf: &Path, out_swf: &Path,
    doc_class: &str, marker_path: &str, content: &str,
) -> anyhow::Result<()> {
    let (doc_class, marker_path, content) = (doc_class.to_string(), marker_path.to_string(), content.to_string());
    patch_file_with(in_swf, out_swf, move |abc| inject_startup_marker(abc, &doc_class, &marker_path, &content))
}

/// Patch SSF2's ABC with an arbitrary injection step `f`, byte-splicing the
/// DoAbc2 tag and recompressing. The general form of `patch_file`.
/// Decompress an SSF2 SWF and parse its DoAbc2 block into an [`Abc`]. The read-only
/// front half of `patch_file_with`, shared so `peptide ssf2 doctor` checks the exact
/// same bytecode the patch would.
pub fn read_abc(in_swf: &Path) -> anyhow::Result<Abc> {
    let data = std::fs::read(in_swf)?;
    let buf = swf::decompress_swf(&data[..]).map_err(|e| anyhow::anyhow!("decompress: {e}"))?;
    let (_, _, abc_off, abc_len) = find_doabc2(&buf.data)?;
    parse(&buf.data[abc_off..abc_off + abc_len])
}

pub fn patch_file_with(
    in_swf: &Path, out_swf: &Path,
    f: impl FnOnce(&mut Abc) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let data = std::fs::read(in_swf)?;
    let buf = swf::decompress_swf(&data[..]).map_err(|e| anyhow::anyhow!("decompress: {e}"))?;
    let tags = &buf.data;

    let (tag_start, tag_total, abc_off, abc_len) = find_doabc2(tags)?;

    // Parse → inject → re-serialize the ABC.
    let abc_bytes = &tags[abc_off..abc_off + abc_len];
    let mut abc = parse(abc_bytes)?;
    f(&mut abc)?;
    let new_abc = write(&abc);

    // Rebuild the DoAbc2 tag with the new abc bytes (keep flags+name prefix).
    // Re-emit a long-form header so we never hit the 0x3f short/long boundary.
    let orig_code_len = u16::from_le_bytes([tags[tag_start], tags[tag_start + 1]]);
    let orig_hdr = if (orig_code_len & 0x3f) == 0x3f { 6 } else { 2 };
    let body_start = tag_start + orig_hdr;
    let body_prefix = &tags[body_start..abc_off]; // flags(4) + name + NUL
    let new_body_len = body_prefix.len() + new_abc.len();

    let mut out_tags = Vec::with_capacity(tags.len() + new_abc.len());
    out_tags.extend_from_slice(&tags[..tag_start]);
    // long-form tag header: ((code<<6)|0x3f) as u16, then u32 length
    let code_len = (TAG_DO_ABC2 << 6) | 0x3f;
    out_tags.extend_from_slice(&code_len.to_le_bytes());
    out_tags.extend_from_slice(&(new_body_len as u32).to_le_bytes());
    out_tags.extend_from_slice(body_prefix);
    out_tags.extend_from_slice(&new_abc);
    out_tags.extend_from_slice(&tags[tag_start + tag_total..]);

    // Reconstruct a Header from the parsed HeaderExt and write a patched SWF.
    let h = &buf.header;
    let header = swf::Header {
        compression: h.compression(),
        version: h.version(),
        stage_size: h.stage_size().clone(),
        frame_rate: h.frame_rate(),
        num_frames: h.num_frames(),
    };
    let f = std::fs::File::create(out_swf)?;
    swf::write::write_swf_raw_tags(&header, &out_tags, std::io::BufWriter::new(f))
        .map_err(|e| anyhow::anyhow!("write_swf_raw_tags: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_ssf2_marker_reparse() {
        let inp = std::path::Path::new("/Users/jimmy/Downloads/SSF2BetaMac_v1.4.0.1-standalone 2/SSF2.app/Contents/Resources/SSF2.swf");
        if !inp.exists() { eprintln!("SSF2.swf missing; skip"); return; }
        let outp = std::path::Path::new("/tmp/SSF2-patched.swf");
        patch_file(inp, outp, "com.mcleodgaming.ssf2.Main",
            "/tmp/peptide_ssf2_marker.txt", "alive").expect("patch");
        // re-parse the patched SWF and the injected ABC to confirm validity
        let data = std::fs::read(outp).unwrap();
        let buf = swf::decompress_swf(&data[..]).expect("re-decompress patched");
        let parsed = swf::parse_swf(&buf).expect("re-parse patched");
        let abc_bytes: &[u8] = parsed.tags.iter().find_map(|t| if let swf::Tag::DoAbc2(a)=t {Some(a.data)} else {None}).expect("DoAbc2");
        let abc = crate::abc_codec::parse(abc_bytes).expect("re-parse injected abc (full consume)");
        eprintln!("patched ABC re-parses: strings={} multinames={} bodies={}", abc.strings.len(), abc.multinames.len(), abc.bodies.len());
    }

    /// The whole SSF2_MANIFEST must resolve against the installed SSF2 — a regression
    /// guard so a wrong/renamed manifest entry is caught here, and `preflight` passes on
    /// the supported build. Skips cleanly when the SSF2 corpus isn't present.
    #[test]
    fn manifest_resolves_against_real_ssf2() {
        let inp = std::path::Path::new("/Users/jimmy/Downloads/SSF2BetaMac_v1.4.0.1-standalone 2/SSF2.app/Contents/Resources/SSF2.swf");
        if !inp.exists() { eprintln!("SSF2.swf missing; skip"); return; }
        let abc = read_abc(inp).expect("read abc");
        let missing: Vec<_> = verify_symbols(&abc).into_iter().filter(|(_, ok)| !ok)
            .map(|(s, _)| s.member.map(|m| format!("{}.{m}", s.class)).unwrap_or_else(|| s.class.into()))
            .collect();
        assert!(missing.is_empty(), "manifest symbols not resolving: {missing:?}");
        preflight(&abc).expect("preflight should pass on the supported SSF2 build");
    }

    /// The detection logic itself: a real class resolves (walking the super chain for an
    /// inherited member), a bogus class/member does not.
    #[test]
    fn verify_detects_missing_symbols() {
        let inp = std::path::Path::new("/Users/jimmy/Downloads/SSF2BetaMac_v1.4.0.1-standalone 2/SSF2.app/Contents/Resources/SSF2.swf");
        if !inp.exists() { eprintln!("SSF2.swf missing; skip"); return; }
        let abc = read_abc(inp).expect("read abc");
        assert!(abc.find_class_by_name("NoSuchClass_xyz").is_none());
        let ci = abc.find_class_by_name("Character").expect("Character exists");
        assert!(class_has_member(&abc, ci, "AttackDataObj"), "inherited member resolves via super chain");
        assert!(!class_has_member(&abc, ci, "noSuchMember_xyz"), "bogus member is reported missing");
    }
}

#[cfg(test)]
mod asm_tests {
    use super::*;

    /// prepend_code MUST shift the exception-table offsets by the payload length, not
    /// just splice the code — a body with a try/catch (e.g. Main.iinit) is silently
    /// corrupted otherwise (the catch points mid-instruction → AVM2 verify failure).
    /// This guards the exact regression where a refactor dropped the shift.
    #[test]
    fn prepend_code_shifts_exceptions() {
        let mut body = MethodBody {
            method: 0, max_stack: 0, local_count: 0, init_scope_depth: 0, max_scope_depth: 0,
            code: vec![0xAA, 0xBB],
            exceptions: vec![Exception { from: 1, to: 2, target: 1, exc_type: 0, var_name: 0 }],
            traits: vec![],
        };
        prepend_code(&mut body, &[0x10, 0x20, 0x30]); // 3-byte payload
        assert_eq!(body.code, vec![0x10, 0x20, 0x30, 0xAA, 0xBB], "payload prepended before old code");
        let e = &body.exceptions[0];
        assert_eq!((e.from, e.to, e.target), (4, 5, 4), "exception offsets shifted by payload len (3)");
    }

    #[test]
    fn branch_fixup_offsets() {
        // jump FWD; <pop>; place FWD: forward jump should skip the pop.
        let mut c = Code::default();
        let l = c.new_label();
        c.branch(OP_JUMP, l);  // bytes: [0x10][o0 o1 o2]  off_pos=1, next=4
        c.op(OP_POP);          // at pos 4
        c.place(l);            // target = 5
        let code = c.finish();
        // s24 at [1..4] should encode 5 - 4 = 1
        assert_eq!(code[0], OP_JUMP);
        let off = i32::from_le_bytes([code[1], code[2], code[3], 0]);
        assert_eq!(off, 1, "forward jump offset");
        assert_eq!(code[4], OP_POP);
        // backward jump: target before the branch -> negative offset
        let mut c = Code::default();
        let top = c.new_label();
        c.place(top);          // target = 0
        c.op(OP_POP);          // pos 0
        c.branch(OP_JUMP, top);// op at 1, off_pos=2, next=5 -> off = 0-5 = -5
        let code = c.finish();
        let off = i32::from_le_bytes([code[2], code[3], code[4], 0]);
        // sign-extend 24-bit
        let off = (off << 8) >> 8;
        assert_eq!(off, -5, "backward jump offset");
    }
}
