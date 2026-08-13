/// ABC (ActionScript Bytecode) parser for SSF2 character files.
///
/// SSF2 characters store all gameplay data in AS3 classes compiled to ABC bytecode:
///   - Attack data: object literals with damage, angle, knockback, etc.
///   - Character stats: getOwnStats() method returning physics values
///   - Frame scripts: per-frame logic in timeline classes
///
/// ABC format reference: https://web.archive.org/web/2024/https://adobe.com/content/dam/amd/en/devnet/actionscript/articles/avm2overview.pdf

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use crate::decompiler;

// ─── AVM2 opcodes we care about ──────────────────────────────────────────────
const OP_PUSHBYTE:      u8 = 0x24;
const OP_PUSHSHORT:     u8 = 0x25;
const OP_PUSHINT:       u8 = 0x2D;
const OP_PUSHUINT:      u8 = 0x2E;
const OP_PUSHDOUBLE:    u8 = 0x2F;
const OP_PUSHSTRING:    u8 = 0x2C;
const OP_PUSHTRUE:      u8 = 0x26;
const OP_PUSHFALSE:     u8 = 0x27;
const OP_PUSHNULL:      u8 = 0x20;
const OP_PUSHNAN:       u8 = 0x28;
const OP_NEWOBJECT:     u8 = 0x55;
const OP_NEWARRAY:      u8 = 0x56;
const OP_CALLPROPERTY:  u8 = 0x46;
const OP_CALLPROPVOID:  u8 = 0x4F;
const OP_SETPROPERTY:   u8 = 0x61;
const OP_GETPROPERTY:   u8 = 0x66;
const OP_FINDPROP:      u8 = 0x5C;
const OP_FINDPROPSTRICT:u8 = 0x5D;
const OP_GETLEX:        u8 = 0x60;
const OP_COERCE:        u8 = 0x80;
const OP_COERCE_A:      u8 = 0x82;
const OP_RETURNVALUE:   u8 = 0x48;
const OP_RETURNVOID:    u8 = 0x47;
const OP_GETLOCAL0:     u8 = 0xD0;
const OP_GETLOCAL1:     u8 = 0xD1;
const OP_GETLOCAL2:     u8 = 0xD2;
const OP_GETLOCAL3:     u8 = 0xD3;
const OP_GETLOCAL:      u8 = 0x62;
const OP_SETLOCAL:      u8 = 0x63;
const OP_SETLOCAL0:     u8 = 0xD4;
const OP_SETLOCAL1:     u8 = 0xD5;
const OP_SETLOCAL2:     u8 = 0xD6;
const OP_SETLOCAL3:     u8 = 0xD7;
const OP_INITPROPERTY:  u8 = 0x68;
const OP_CONSTRUCTPROP: u8 = 0x4A;
const OP_CONSTRUCT:     u8 = 0x42;
const OP_NOP:           u8 = 0x02;
const OP_POP:           u8 = 0x29;
const OP_DUP:           u8 = 0x2A;
const OP_SWAP:          u8 = 0x2B;
const OP_ADD:           u8 = 0xA0;
const OP_SUBTRACT:      u8 = 0xA1;
const OP_MULTIPLY:      u8 = 0xA2;
const OP_DIVIDE:        u8 = 0xA3;
const OP_NEGATE:        u8 = 0x90;
const OP_CONVERT_D:     u8 = 0x84;
const OP_CONVERT_I:     u8 = 0x83;
const OP_LABEL:         u8 = 0x09;
const OP_JUMP:          u8 = 0x10;
const OP_IFTRUE:        u8 = 0x11;
const OP_IFFALSE:       u8 = 0x12;
const OP_IFEQ:          u8 = 0x13;
const OP_IFNE:          u8 = 0x14;
const OP_IFLT:          u8 = 0x15;
const OP_IFLE:          u8 = 0x16;
const OP_IFGT:          u8 = 0x17;
const OP_IFGE:          u8 = 0x18;
const OP_IFSTRICTEQ:    u8 = 0x19;
const OP_IFSTRICTNE:    u8 = 0x1A;
// negated comparison branches (see decompiler.rs) — same 3-byte offset as their
// positive twins, so the scan desyncs on any method that uses one.
const OP_IFNLT:         u8 = 0x0C;
const OP_IFNLE:         u8 = 0x0D;
const OP_IFNGT:         u8 = 0x0E;
const OP_IFNGE:         u8 = 0x0F;

// ─── Parsed ABC structures ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AbcFile {
    /// ABC string constant pool. Interned as `Arc<str>` so resolving a name
    /// onto a `Multiname` / `Method` is a refcount bump, not a fresh allocation.
    pub strings: Vec<Arc<str>>,
    pub ints: Vec<i32>,
    pub uints: Vec<u32>,
    pub doubles: Vec<f64>,
    pub multinames: Vec<Multiname>,
    pub methods: Vec<Method>,
    pub classes: Vec<Class>,
    pub scripts: Vec<Script>,
    pub method_bodies: Vec<MethodBody>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Multiname {
    pub kind: u8,
    pub name_idx: u32,
    pub ns_idx: u32,
    pub name: Arc<str>, // resolved from string pool (shared, not re-allocated)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Method {
    pub name_idx: u32,
    pub name: Arc<str>,
    pub param_count: u32,
    pub return_type_idx: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Class {
    pub name: String,
    pub super_name: String,
    pub instance_methods: Vec<Trait>,
    pub class_methods: Vec<Trait>,
    pub constructor_idx: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trait {
    pub name: String,
    pub kind: u8,
    pub method_idx: u32,
    pub slot_idx: u32,
    /// Slot/Const default value, when numeric (the ABC trait's vindex/vkind resolved against the
    /// int/uint/double pools) — e.g. a class constant like `SINK_SPEED = 30`.
    #[serde(default)]
    pub default: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Script {
    pub init_method_idx: u32,
    pub traits: Vec<Trait>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodBody {
    pub method_idx: u32,
    pub max_stack: u32,
    pub local_count: u32,
    pub bytecode: Vec<u8>,
    pub activation_traits: Vec<Trait>,  // named slots from newactivation
}

// ─── Extracted character data ─────────────────────────────────────────────────

/// Maps SSF2 frame method name → SSF2 animation name (from self.xframe = "...").
/// e.g. "frame14" → "a", "frame29" → "a_air"
pub type XframeMap = BTreeMap<String, String>;

pub struct ExtractedCharacter {
    pub name: String,
    pub attacks: BTreeMap<String, AttackData>,
    /// Per-projectile physics + hitboxes pulled from the Ext class's
    /// `getProjectileStats()` method. Keys are SSF2 projectile names
    /// (matching the projectile sprite SymbolClass names).
    pub projectiles: BTreeMap<String, ProjectileData>,
    pub stats: Option<CharStats>,
    pub frame_scripts: BTreeMap<String, Vec<FrameAction>>,
    /// Decompiled Ext class methods translated to Fraymakers Haxe
    pub ext_methods: BTreeMap<String, String>,
    /// Names of instance Slot/Const traits on the Ext class (the SSF2
    /// `public var foo:T;` declarations).
    pub ext_vars: Vec<String>,
    /// Initial values for the ext_vars, pulled out of the Ext class
    /// constructor (iinit). `(name, rhs_expression)`; one per var that the
    /// constructor actually assigns.
    pub ext_var_inits: Vec<(String, String)>,
    /// frame method name → SSF2 animation name (from self.xframe = "...")
    pub xframe_map: XframeMap,
    /// SSF2 `attackSound{N}_id` values (1-based, contiguous) — the per-attack
    /// SFX table indexed by `playAttackSound(N)`. Entries are SSF2 sound ids
    /// (global ids like `brawl_swing_l` or per-character bank ids).
    pub attack_sounds: Vec<String>,
    /// SSF2 `attackVoice{N}_id` values (1-based, contiguous) — the voice/grunt
    /// table indexed by `playVoiceSound(N)`. Voice playback does not overlap.
    pub voice_sounds: Vec<String>,
    /// Populated when this character's bundle method's `cData.normalStats_id`
    /// disagrees with `char_name` — i.e. this is a transformation /
    /// alternate form of a parent character (Giga Bowser, Wario Man).
    /// Fraymakers has no native transformation API; the converter emits
    /// the alternate form as a standalone character package and surfaces
    /// this metadata in `CharacterStats.hx` (TODO banner) and
    /// `conversion_stats.json` (`ssf2_source`).
    pub derived_from: Option<DerivedFrom>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedFrom {
    /// `cData.normalStats_id` — the parent character's id.
    pub parent_normal_stats_id: String,
    /// The Main method that produced this bundle (e.g. `getGigaBowser`).
    pub source_method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CostumeData {
    pub name: String,
    /// Source ARGB color values — the base sprite palette (same across all costumes)
    pub colors: Vec<u32>,
    /// Replacement ARGB color values — what each source color becomes for this costume
    pub replacements: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttackData {
    pub hitboxes: Vec<BTreeMap<String, f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharStats {
    pub values: BTreeMap<String, f64>,
}

/// One projectile-stat entry extracted from SSF2's `getProjectileStats()`.
/// The Ext class returns a top-level object keyed by projectile name
/// (matching the projectile sprite's class name, e.g. `dee_nspec`).
/// Each value combines:
///   - flat-scalar physics fields (gravity, xSpeed, ySpeed, friction, …)
///   - a nested `attackBoxes` object with the projectile's hitboxes
/// Hitbox shape mirrors `AttackData.hitboxes` so it can share the
/// `hitbox_stats.jsonc` SSF2→FM field-name canon.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectileData {
    pub stats:    BTreeMap<String, f64>,
    pub hitboxes: Vec<BTreeMap<String, f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameAction {
    pub frame: u32,
    pub action: String,
    pub args: Vec<String>,
}

// ─── Reader helpers ───────────────────────────────────────────────────────────

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    #[allow(dead_code)]
    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(anyhow!("read_u8: out of bounds at {}", self.pos));
        }
        let b = self.data[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn read_u16(&mut self) -> Result<u16> {
        let lo = self.read_u8()? as u16;
        let hi = self.read_u8()? as u16;
        Ok(lo | (hi << 8))
    }

    #[allow(dead_code)]
    fn read_u32(&mut self) -> Result<u32> {
        let lo = self.read_u16()? as u32;
        let hi = self.read_u16()? as u32;
        Ok(lo | (hi << 16))
    }

    fn read_f64(&mut self) -> Result<f64> {
        if self.pos + 8 > self.data.len() {
            return Err(anyhow!("read_f64: out of bounds"));
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(f64::from_le_bytes(bytes))
    }

    /// Variable-length encoded u30/u32
    fn read_u30(&mut self) -> Result<u32> {
        let mut result = 0u32;
        let mut shift = 0;
        loop {
            let b = self.read_u8()? as u32;
            result |= (b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 || shift >= 35 {
                break;
            }
        }
        Ok(result)
    }

    /// Variable-length encoded s32
    fn read_s32(&mut self) -> Result<i32> {
        let v = self.read_u30()?;
        // sign-extend from 29 bits
        if v & 0x10000000 != 0 {
            Ok((v | 0xE0000000) as i32)
        } else {
            Ok(v as i32)
        }
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_u30()? as usize;
        if self.pos + len > self.data.len() {
            return Err(anyhow!("read_string: length {} out of bounds at pos {}", len, self.pos));
        }
        // ABC strings are spec'd as UTF-8 but SSF2 SWFs sometimes carry a
        // Windows-1252 byte (an `é` etc.) — `from_utf8_lossy` silently
        // inserts U+FFFD, which downstream filters (`looks_like_char_name`,
        // is-alphanumeric checks on xframe labels) then reject, losing the
        // trait. Log a debug-level warning when we actually replaced any
        // bytes so the loss is visible without spamming the normal log.
        let raw = &self.data[self.pos..self.pos + len];
        let s = match std::str::from_utf8(raw) {
            Ok(valid) => valid.to_string(),
            Err(_) => {
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "read_string: non-UTF8 bytes at pos {} (len={}); \
                         using lossy UTF-8 decode — character data may be \
                         silently filtered later",
                        self.pos, len
                    );
                }
                String::from_utf8_lossy(raw).to_string()
            }
        };
        self.pos += len;
        Ok(s)
    }

    #[allow(dead_code)]
    fn skip(&mut self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() {
            return Err(anyhow!("skip: {} out of bounds at {}", n, self.pos));
        }
        self.pos += n;
        Ok(())
    }
}

// ─── ABC parsing ─────────────────────────────────────────────────────────────

pub fn parse(data: &[u8]) -> Result<AbcFile> {
    let mut r = Reader::new(data);

    // Header: minor version, major version
    let _minor = r.read_u16()?;
    let _major = r.read_u16()?;

    // ── Constant pool ──────────────────────────────────────────────────────────

    // Integers
    let int_count = r.read_u30()? as usize;
    let mut ints = vec![0i32];
    for _ in 1..int_count {
        ints.push(r.read_s32()?);
    }

    // Unsigned integers
    let uint_count = r.read_u30()? as usize;
    let mut uints = vec![0u32];
    for _ in 1..uint_count {
        uints.push(r.read_u30()?);
    }

    // Doubles
    let double_count = r.read_u30()? as usize;
    let mut doubles = vec![f64::NAN];
    for _ in 1..double_count {
        doubles.push(r.read_f64()?);
    }

    // Strings
    let string_count = r.read_u30()? as usize;
    let mut strings: Vec<Arc<str>> = vec![Arc::from("")];
    for _ in 1..string_count {
        strings.push(Arc::from(r.read_string()?));
    }

    log::debug!("ABC constants: {} ints, {} uints, {} doubles, {} strings",
        ints.len(), uints.len(), doubles.len(), strings.len());

    // Namespaces
    let ns_count = r.read_u30()? as usize;
    let mut namespaces: Vec<String> = vec![String::new()];
    for _ in 1..ns_count {
        let _kind = r.read_u8()?;
        let name_idx = r.read_u30()?;
        namespaces.push(strings.get(name_idx as usize).map(|s| s.to_string()).unwrap_or_default());
    }

    // Namespace sets
    let nsset_count = r.read_u30()? as usize;
    for _ in 1..nsset_count {
        let ns_count = r.read_u30()? as usize;
        for _ in 0..ns_count {
            r.read_u30()?;
        }
    }

    // Multinames
    let mn_count = r.read_u30()? as usize;
    let mut multinames = vec![Multiname { kind: 0, name_idx: 0, ns_idx: 0, name: Arc::from("") }];
    for _ in 1..mn_count {
        let kind = r.read_u8()?;
        let mn = match kind {
            0x07 | 0x0D => { // QName, QNameA
                let ns = r.read_u30()?;
                let name_idx = r.read_u30()?;
                let name = strings.get(name_idx as usize).cloned().unwrap_or_default();
                Multiname { kind, name_idx, ns_idx: ns, name }
            }
            0x0F | 0x10 => { // RTQName, RTQNameA
                let name_idx = r.read_u30()?;
                Multiname { kind, name_idx, ns_idx: 0, name: strings.get(name_idx as usize).cloned().unwrap_or_default() }
            }
            0x11 | 0x12 => { // RTQNameL, RTQNameLA
                Multiname { kind, name_idx: 0, ns_idx: 0, name: Arc::from("") }
            }
            0x09 | 0x0E => { // Multiname, MultinameA
                let name_idx = r.read_u30()?;
                let _ns_set = r.read_u30()?;
                let name = strings.get(name_idx as usize).cloned().unwrap_or_default();
                Multiname { kind, name_idx, ns_idx: 0, name }
            }
            0x1B | 0x1C => { // MultinameL, MultinameLA
                let _ns_set = r.read_u30()?;
                Multiname { kind, name_idx: 0, ns_idx: 0, name: Arc::from("") }
            }
            0x1D => { // TypeName (generic)
                let _qname = r.read_u30()?;
                let param_count = r.read_u30()? as usize;
                for _ in 0..param_count { r.read_u30()?; }
                Multiname { kind, name_idx: 0, ns_idx: 0, name: Arc::from("") }
            }
            _ => {
                log::warn!("Unknown multiname kind: 0x{:02X}", kind);
                Multiname { kind, name_idx: 0, ns_idx: 0, name: Arc::from("") }
            }
        };
        multinames.push(mn);
    }

    // ── Methods ───────────────────────────────────────────────────────────────

    let method_count = r.read_u30()? as usize;
    let mut methods = Vec::with_capacity(method_count);
    for _ in 0..method_count {
        let param_count = r.read_u30()?;
        let return_type_idx = r.read_u30()?;
        for _ in 0..param_count { r.read_u30()?; } // param types
        let name_idx = r.read_u30()?;
        let flags = r.read_u8()?;
        if flags & 0x08 != 0 { // HAS_OPTIONAL
            let opt_count = r.read_u30()? as usize;
            for _ in 0..opt_count {
                r.read_u30()?; // value index
                r.read_u8()?;  // value kind
            }
        }
        if flags & 0x80 != 0 { // HAS_PARAM_NAMES
            for _ in 0..param_count { r.read_u30()?; }
        }
        let name = strings.get(name_idx as usize).cloned().unwrap_or_default();
        methods.push(Method { name_idx, name, param_count, return_type_idx });
    }

    // ── Metadata ──────────────────────────────────────────────────────────────
    let metadata_count = r.read_u30()? as usize;
    for _ in 0..metadata_count {
        r.read_u30()?; // name
        let item_count = r.read_u30()? as usize;
        for _ in 0..item_count {
            r.read_u30()?; // key
            r.read_u30()?; // value
        }
    }

    // ── Classes ───────────────────────────────────────────────────────────────
    let class_count = r.read_u30()? as usize;
    let mut classes = Vec::with_capacity(class_count);

    // Instance infos
    for _ in 0..class_count {
        let name_idx = r.read_u30()?;
        let super_name_idx = r.read_u30()?;
        let flags = r.read_u8()?;
        if flags & 0x08 != 0 { r.read_u30()?; } // protected ns
        let iface_count = r.read_u30()? as usize;
        for _ in 0..iface_count { r.read_u30()?; }
        let constructor_idx = r.read_u30()?;
        let trait_count = r.read_u30()? as usize;
        let mut instance_methods = Vec::new();
        for _ in 0..trait_count {
            if let Ok(t) = parse_trait(&mut r, &strings, &multinames, &ints, &uints, &doubles) {
                instance_methods.push(t);
            }
        }
        let name = multinames.get(name_idx as usize).map(|m| m.name.to_string()).unwrap_or_default();
        let super_name = multinames.get(super_name_idx as usize).map(|m| m.name.to_string()).unwrap_or_default();
        // Also resolve namespace-qualified name for _fla.* classes
        let ns_name = if let Some(mn) = multinames.get(name_idx as usize) {
            if mn.kind == 0x07 || mn.kind == 0x0D {
                // QName: namespace_idx is stored, get namespace string
                let ns_idx = mn.ns_idx as usize;
                if ns_idx < namespaces.len() && !namespaces[ns_idx].is_empty() {
                    format!("{}.{}", namespaces[ns_idx], mn.name)
                } else {
                    mn.name.to_string()
                }
            } else { mn.name.to_string() }
        } else { name.clone() };
        let full_name = if ns_name != name && ns_name.contains("_fla.") { ns_name } else { name.clone() };
        classes.push(Class { name: full_name, super_name, instance_methods, class_methods: vec![], constructor_idx });
    }

    // Class infos (static traits). One class_info per class, in order — the first
    // loop pushed exactly `class_count` entries, so we can walk them directly.
    for class in classes.iter_mut() {
        let _static_init = r.read_u30()?;
        let trait_count = r.read_u30()? as usize;
        for _ in 0..trait_count {
            if let Ok(t) = parse_trait(&mut r, &strings, &multinames, &ints, &uints, &doubles) {
                class.class_methods.push(t);
            }
        }
    }

    // ── Scripts ───────────────────────────────────────────────────────────────
    let script_count = r.read_u30()? as usize;
    let mut scripts = Vec::with_capacity(script_count);
    for _ in 0..script_count {
        let init_method_idx = r.read_u30()?;
        let trait_count = r.read_u30()? as usize;
        let mut traits = Vec::new();
        for _ in 0..trait_count {
            if let Ok(t) = parse_trait(&mut r, &strings, &multinames, &ints, &uints, &doubles) {
                traits.push(t);
            }
        }
        scripts.push(Script { init_method_idx, traits });
    }

    // ── Method bodies ─────────────────────────────────────────────────────────
    let body_count = r.read_u30()? as usize;
    let mut method_bodies = Vec::with_capacity(body_count);
    for _ in 0..body_count {
        let method_idx = r.read_u30()?;
        let max_stack = r.read_u30()?;
        let local_count = r.read_u30()?;
        let _init_scope_depth = r.read_u30()?;
        let _max_scope_depth = r.read_u30()?;
        let code_len = r.read_u30()? as usize;
        let start = r.pos;
        let bytecode = if r.pos + code_len <= r.data.len() {
            let bc = r.data[start..start + code_len].to_vec();
            r.pos += code_len;
            bc
        } else {
            r.pos = r.data.len();
            vec![]
        };

        // Skip exception handlers
        let ex_count = r.read_u30().unwrap_or(0) as usize;
        for _ in 0..ex_count {
            r.read_u30().ok(); // from
            r.read_u30().ok(); // to
            r.read_u30().ok(); // target
            r.read_u30().ok(); // exc_type
            r.read_u30().ok(); // var_name
        }

        // Parse method body traits (activation slot names)
        let trait_count = r.read_u30().unwrap_or(0) as usize;
        let mut activation_traits = Vec::new();
        for _ in 0..trait_count {
            match parse_trait(&mut r, &strings, &multinames, &ints, &uints, &doubles) {
                Ok(t) => activation_traits.push(t),
                Err(_) => break,
            }
        }

        method_bodies.push(MethodBody { method_idx, max_stack, local_count, bytecode, activation_traits });
    }

    log::info!("ABC: {} methods, {} classes, {} method bodies", methods.len(), classes.len(), method_bodies.len());

    Ok(AbcFile { strings, ints, uints, doubles, multinames, methods, classes, scripts, method_bodies })
}

fn parse_trait(r: &mut Reader, _strings: &[Arc<str>], multinames: &[Multiname], ints: &[i32], uints: &[u32], doubles: &[f64]) -> Result<Trait> {
    let name_idx = r.read_u30()?;
    let kind_byte = r.read_u8()?;
    let kind = kind_byte & 0x0F;
    let has_metadata = kind_byte & 0x40 != 0;
    let name = multinames.get(name_idx as usize).map(|m| m.name.to_string()).unwrap_or_default();

    let mut default: Option<f64> = None;
    let (method_idx, slot_idx) = match kind {
        0 | 6 => { // Slot, Const
            let slot_id = r.read_u30()?;
            let _type_name = r.read_u30()?;
            let vindex = r.read_u30()?;
            if vindex != 0 {
                let vkind = r.read_u8()?;
                default = match vkind {
                    0x03 => ints.get(vindex as usize).map(|v| *v as f64),
                    0x04 => uints.get(vindex as usize).map(|v| *v as f64),
                    0x06 => doubles.get(vindex as usize).copied(),
                    _ => None,
                };
            }
            (0, slot_id)
        }
        1..=3 => { // Method, Getter, Setter
            let _disp_id = r.read_u30()?;
            let method_idx = r.read_u30()?;
            (method_idx, 0)
        }
        4 | 5 => { // Class, Function
            let slot_id = r.read_u30()?;
            let idx = r.read_u30()?;
            (idx, slot_id)
        }
        _ => {
            return Err(anyhow!("Unknown trait kind: {}", kind));
        }
    };

    if has_metadata {
        let mc = r.read_u30()? as usize;
        for _ in 0..mc { r.read_u30()?; }
    }

    Ok(Trait { name, kind, method_idx, slot_idx, default })
}

// ─── Character data extraction ────────────────────────────────────────────────

/// Returns true if this frame method name is a root-MovieClip xframe setter
/// (e.g. "frame1", "frame23") as opposed to a real animation frame script
/// (e.g. "a__frame2", "stand__frame0").
/// Root xframe setters just do `self.xframe = "..."; self.stop();` and have
/// no gameplay value in Fraymakers — they only drive animation name extraction.
fn is_root_xframe_method(name: &str) -> bool {
    name.starts_with("frame")
        && name["frame".len()..].chars().all(|c| c.is_ascii_digit())
        && !name["frame".len()..].is_empty()
}

/// Extract the SSF2 animation name set by self.xframe from a frame* method's bytecode.
/// Scans for the first PUSHSTRING instruction whose value looks like an animation name.
fn extract_xframe_name(bytecode: &[u8], abc: &AbcFile) -> Option<String> {
    let mut i = 0;
    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        match op {
            OP_PUSHSTRING => {
                if let Some(idx) = read_u30_at(bytecode, &mut i) {
                    if let Some(s) = abc.strings.get(idx as usize) {
                        // xframe values are short snake_case strings, not bytecode artifacts
                        if !s.is_empty() && s.len() < 40 && s.chars().all(|c| c.is_alphanumeric() || c == '_') {
                            return Some(s.to_string());
                        }
                    }
                }
            }
            OP_PUSHBYTE => { i += 1; }
            OP_PUSHSHORT | OP_PUSHINT | OP_PUSHUINT | OP_PUSHDOUBLE => { read_u30_at(bytecode, &mut i); }
            OP_GETLEX | OP_FINDPROPSTRICT | OP_FINDPROP | OP_GETPROPERTY |
            OP_SETPROPERTY | OP_INITPROPERTY | OP_COERCE | OP_CALLPROPVOID |
            OP_CALLPROPERTY | OP_GETLOCAL | OP_SETLOCAL | OP_NEWARRAY | OP_NEWOBJECT => {
                read_u30_at(bytecode, &mut i);
            }
            OP_CONSTRUCTPROP => { read_u30_at(bytecode, &mut i); read_u30_at(bytecode, &mut i); }
            OP_JUMP | OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFLT |
            OP_IFLE | OP_IFGT | OP_IFGE | OP_IFSTRICTEQ | OP_IFSTRICTNE
                if i + 3 <= bytecode.len() => { i += 3; }
            _ => {}
        }
    }
    None
}

/// Extract character data by analyzing ABC bytecode. Path-2 implementation:
/// stats / attacks / projectiles come from `Main::get<X>()` (the "bundle"
/// — see `docs/path2_unification_plan.md`); behavior code (frame scripts,
/// ext methods, ext vars + inits) comes from the per-character `<X>Ext`
/// class plus the main character MovieClip and per-character sub-MCs.
pub fn extract_character(abc: &AbcFile, char_name: &str) -> Result<ExtractedCharacter> {
    let mut attacks: BTreeMap<String, AttackData> = BTreeMap::new();
    let mut projectiles: BTreeMap<String, ProjectileData> = BTreeMap::new();
    let mut stats: Option<CharStats> = None;
    let mut frame_scripts: BTreeMap<String, Vec<FrameAction>> = BTreeMap::new();
    let mut ext_methods: BTreeMap<String, String> = BTreeMap::new();
    let mut xframe_map: XframeMap = BTreeMap::new();
    let mut attack_sounds: Vec<String> = Vec::new();
    let mut voice_sounds: Vec<String> = Vec::new();

    // ── Stage A: stats / attacks / projectiles from Main::get<X>() ─────
    let mut derived_from: Option<DerivedFrom> = None;
    if let Some((body, method_name)) = find_bundle_method(abc, char_name) {
        log::info!("path-2: extracting Stage A from Main::{}", method_name);
        attacks     = extract_attack_objects(&body.bytecode, abc);
        projectiles = extract_projectile_objects(&body.bytecode, abc);
        stats       = extract_ssf2_stats(&body.bytecode, abc);

        // SSF2 sound tables — the 1-based attackSound{N}_id / attackVoice{N}_id
        // fields that playAttackSound(N) / playVoiceSound(N) index into.
        attack_sounds = extract_indexed_string_fields(body, abc, "attackSound", "_id");
        voice_sounds  = extract_indexed_string_fields(body, abc, "attackVoice", "_id");
        log::info!("path-2: {} attack sound(s), {} voice sound(s)",
            attack_sounds.len(), voice_sounds.len());

        // Detect transformation: cData.normalStats_id ≠ derived char_name.
        // Recorded as metadata; Fraymakers has no native transformation
        // surface, so emission downstream is unchanged but the
        // CharacterStats.hx header gets a TODO banner.
        if let Some(nsi) = extract_normal_stats_id(body, abc) {
            if nsi != char_name {
                log::info!("path-2: {:?} is a transformation of {:?} (via Main::{})",
                    char_name, nsi, method_name);
                derived_from = Some(DerivedFrom {
                    parent_normal_stats_id: nsi,
                    source_method: format!("Main::{}", method_name),
                });
            }
        }
    } else {
        log::warn!("path-2: no Main::get* matching {:?}; Stage B behavior code will still be extracted", char_name);
    }

    // Build method name lookup: method_idx → name
    let mut method_names: BTreeMap<u32, String> = BTreeMap::new();
    for body in abc.method_bodies.iter() {
        if let Some(method) = abc.methods.get(body.method_idx as usize) {
            if !method.name.is_empty() {
                method_names.insert(body.method_idx, method.name.to_string());
            }
        }
    }

    // Find the XxxExt class (e.g. MarioExt) — holds getOwnStats + getAttackStats.
    // SSF2 class names don't always match the filename: CaptainFalcon → CaptainExt,
    // ChibiRobo → ChibiExt, GameAndWatch → gameandwatchExt, etc.
    // Strategy: try exact match, then prefix match on Ext classes, then any single Ext class.
    let char_lower = char_name.to_lowercase();
    let ext_class_name = format!("{}Ext",
        char_name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default()
        + &char_name[1..]);
    let ext_classes: Vec<&Class> = abc.classes.iter()
        .filter(|c| c.name.ends_with("Ext") && c.name != "SSF2CharacterExt")
        .collect();
    let char_class = abc.classes.iter()
        // 1. Exact match: MarioExt
        .find(|c| c.name == ext_class_name)
        // 2. Case-insensitive contains: captainext contains "captain" (prefix of captainfalcon)
        .or_else(|| ext_classes.iter().copied().find(|c| {
            let cn = c.name.to_lowercase().replace("ext", "");
            char_lower.starts_with(&cn) || cn.starts_with(&char_lower)
        }))
        // 3. Only one non-base Ext class in the file
        .or_else(|| if ext_classes.len() == 1 { Some(ext_classes[0]) } else { None });

    log::info!("Character Ext class: {:?}", char_class.map(|c| &c.name));

    // Build a map from method_idx → trait name (e.g. getOwnStats, getAttackStats, frame1)
    let mut trait_name_for_method: BTreeMap<u32, String> = BTreeMap::new();
    for class in &abc.classes {
        for t in class.instance_methods.iter().chain(class.class_methods.iter()) {
            if !t.name.is_empty() {
                trait_name_for_method.insert(t.method_idx, t.name.clone());
            }
        }
    }

    // Build body lookup: method_idx → body
    let body_by_method: BTreeMap<u32, &MethodBody> = abc.method_bodies.iter()
        .map(|b| (b.method_idx, b))
        .collect();

    // --- Process MarioExt methods specifically ---
    let mut ext_vars: Vec<String> = Vec::new();
    let mut ext_var_inits: Vec<(String, String)> = Vec::new();
    if let Some(ext) = char_class {
        // Collect Slot (kind=0) / Const (kind=6) instance traits — the SSF2
        // class's `public var foo:T;` declarations. We carry these over to
        // Script.hx as top-level `var foo;` so the references later in the
        // translated methods aren't undeclared.
        for t in &ext.instance_methods {
            if (t.kind == 0 || t.kind == 6) && !t.name.is_empty() {
                ext_vars.push(t.name.clone());
            }
        }
        // Decompile the constructor (iinit) and pull out `self.<var> = expr;`
        // assignments for each ext_var. This recovers the initial values the
        // AS3 source wrote as `public var foo:T = expr;` (the compiler puts
        // them at the top of iinit) so the generator can emit them inside
        // initialize().
        if !ext_vars.is_empty() {
            if let Some(iinit_body) = body_by_method.get(&ext.constructor_idx) {
                let params: Vec<String> = if let Some(m) = abc.methods.get(iinit_body.method_idx as usize) {
                    (0..m.param_count).map(|i| format!("arg{}", i)).collect()
                } else { vec![] };
                let raw = decompiler::decompile_method(iinit_body, abc, "iinit", &params);
                let translated = crate::api_mappings::translate_ssf2_to_fm(&raw);
                let mut seen: std::collections::BTreeSet<String> = Default::default();
                for line in translated.lines() {
                    let trimmed = line.trim();
                    for v in &ext_vars {
                        let prefix = format!("self.{} = ", v);
                        if let Some(rest) = trimmed.strip_prefix(&prefix) {
                            if seen.insert(v.clone()) {
                                let rhs = rest.trim_end_matches(';').trim().to_string();
                                ext_var_inits.push((v.clone(), rhs));
                            }
                            break;
                        }
                    }
                }
            }
        }
        for t in &ext.instance_methods {
            let Some(body) = body_by_method.get(&t.method_idx) else { continue };
            match t.name.as_str() {
                // Stat getters are no-ops here — stats/attacks/projectiles
                // come from Main::get<X>() (Stage A, applied at the top of
                // this function). The names are still listed in the
                // catch-all exclusion below so they don't end up in
                // Script.hx as helper methods.
                "getOwnStats" | "getAttackStats" | "getProjectileStats" | "getItemStats" => {}
                name if name.starts_with("frame") => {
                    // Extract xframe animation name first (always, for animation name mapping)
                    if let Some(anim_name) = extract_xframe_name(&body.bytecode, abc) {
                        xframe_map.insert(name.to_string(), anim_name);
                        // Root xframe setters (frame1, frame23, ...) only set self.xframe and stop.
                        // They have no gameplay value in Fraymakers — skip adding to frame_scripts.
                        if is_root_xframe_method(name) { continue; }
                    }
                    // Use decompiler for full Haxe output (real animation frame scripts only)
                    let params: Vec<String> = if let Some(method) = abc.methods.get(body.method_idx as usize) {
                        (0..method.param_count).map(|i| format!("arg{}", i)).collect()
                    } else { vec![] };
                    let code = decompiler::decompile_method(body, abc, name, &params);
                    frame_scripts.insert(name.to_string(), vec![FrameAction {
                        frame: 0,
                        action: code,
                        args: vec![],
                    }]);
                }
                // Decompile all other Ext methods for Script.hx
                // Skip slot/const traits (kind 0/6) — those are variable declarations, not methods
                name if !matches!(name, "getOwnStats" | "getAttackStats" | "getItemStats" | "getProjectileStats")
                    // Only decompile actual method traits (kind 1/2/3), not slots (kind 0/6)
                    // The trait.kind is stored in the Trait struct; method_idx > 0 means it's a real method
                    && (t.kind & 0x0F != 0 || t.slot_idx == 0) => {
                        // Get param count from method signature
                        let params: Vec<String> = if let Some(method) = abc.methods.get(body.method_idx as usize) {
                            (0..method.param_count).map(|i| format!("arg{}", i)).collect()
                        } else {
                            vec![]
                        };
                        let code = decompiler::decompile_method(body, abc, name, &params);
                        ext_methods.insert(name.to_string(), code);
                    }
                _ => {}
            }
        }
    }

    // --- Also scan the main character class for frame scripts ---
    // The class name matches the filename for most chars but not all:
    // captainfalcon.ssf -> class 'captainfalcon'
    // gameandwatch.ssf -> class 'gameAndWatch'
    // Pick the class whose name most closely matches char_name AND has the most frame* methods.
    let main_class = abc.classes.iter()
        // 1. Exact case-insensitive match
        .find(|c| c.name.to_lowercase() == char_lower)
        // 2. Last resort: whichever non-Ext, non-fla, non-proj class has the MOST frame* methods.
        //    Handles cases like captainfalcon.ssf -> class 'falcon', chibirobo.ssf -> 'chibirobo', etc.
        .or_else(|| {
            abc.classes.iter()
                .filter(|c| {
                    let cn = c.name.to_lowercase();
                    !cn.ends_with("ext") && !cn.contains("_fla") && !cn.contains("api")
                        && !cn.contains("proj") && !cn.contains("helper") && !cn.contains("_hud")
                })
                .max_by_key(|c| c.instance_methods.iter().filter(|t| t.name.starts_with("frame")).count())
                .filter(|c| c.instance_methods.iter().any(|t| t.name.starts_with("frame")))
        });
    if let Some(mc) = main_class {
        log::info!("Main class '{}': {} frame methods", mc.name, mc.instance_methods.len());
        // Helper methods on the main class — anything that isn't a frame*
        // entry or a stat-getter — are decompiled and emitted in Script.hx
        // alongside the Ext-class methods. Calls to per-character helpers
        // like `aimTrampoline`, `chef`, `eat`, `gokuKaiokenHUD` would
        // otherwise resolve to nothing (the helpers exist only on the main
        // class, not the Ext class).
        const STAT_GETTERS: &[&str] = &["getOwnStats", "getAttackStats", "getItemStats", "getProjectileStats"];
        for t in &mc.instance_methods {
            if t.name.starts_with("frame") {
                let Some(body) = body_by_method.get(&t.method_idx) else { continue };
                // Extract xframe name
                if let Some(anim_name) = extract_xframe_name(&body.bytecode, abc) {
                    xframe_map.insert(t.name.clone(), anim_name);
                }
                let params: Vec<String> = if let Some(method) = abc.methods.get(body.method_idx as usize) {
                    (0..method.param_count).map(|i| format!("arg{}", i)).collect()
                } else { vec![] };
                let code = decompiler::decompile_method(body, abc, &t.name, &params);
                frame_scripts.insert(t.name.clone(), vec![FrameAction {
                    frame: 0,
                    action: code,
                    args: vec![],
                }]);
            } else if matches!(t.kind, 1..=3) && !t.name.is_empty()
                && !STAT_GETTERS.contains(&t.name.as_str())
                && !ext_methods.contains_key(&t.name)
            {
                // kind 1/2/3 = Method/Getter/Setter; skip Slot/Const (0/6) and
                // anything the Ext class already provides under the same name.
                let Some(body) = body_by_method.get(&t.method_idx) else { continue };
                let params: Vec<String> = if let Some(method) = abc.methods.get(body.method_idx as usize) {
                    (0..method.param_count).map(|i| format!("arg{}", i)).collect()
                } else { vec![] };
                let code = decompiler::decompile_method(body, abc, &t.name, &params);
                ext_methods.insert(t.name.clone(), code);
            }
        }
    }

    // --- Sub-MovieClip helper methods ---
    // Some per-character helpers (e.g. pacman's `aimTrampoline` on
    // `pacman_fla.UpSpecial_37`) live on sub-MC `_fla.*` classes rather than
    // the main class or the Ext class. Their frame-script callers reference
    // them by bare name, so they need to land in Script.hx like the other
    // helpers — otherwise the calls resolve to nothing. Scope the scan to
    // classes that belong to THIS character (name starts with the char
    // lowercase prefix) so we don't suck in SSF2's framework classes
    // (Controls, Matrix3D, KeyboardOptions, etc.) that are unrelated and
    // would flood Script.hx with generic utility methods.
    {
        const STAT_GETTERS: &[&str] = &["getOwnStats", "getAttackStats", "getItemStats", "getProjectileStats"];
        for class in &abc.classes {
            // Skip the Ext class and the main class (already processed).
            if Some(class.name.as_str()) == char_class.map(|c| c.name.as_str()) { continue; }
            if Some(class.name.as_str()) == main_class.map(|c| c.name.as_str()) { continue; }
            // Limit to this character's classes: same prefix as the main
            // class name (or the char name), so `pacman_fla.UpSpecial_37` is
            // in but `Controls` / `Matrix3D` are out. Also drop projectile-
            // specific sub-MCs — those have their own converter path.
            let cn = class.name.to_lowercase();
            let main_prefix = main_class.map(|c| c.name.to_lowercase()).unwrap_or_default();
            let main_root = main_prefix.split(['.', '_']).next().unwrap_or(&char_lower);
            let belongs = !main_root.is_empty()
                && (cn.starts_with(main_root) || cn.starts_with(&char_lower));
            if !belongs { continue; }
            if cn.contains("proj") || cn.contains("helper") { continue; }
            for t in &class.instance_methods {
                if !matches!(t.kind, 1..=3) { continue; }
                if t.name.is_empty() || t.name.starts_with("frame") { continue; }
                if STAT_GETTERS.contains(&t.name.as_str()) { continue; }
                if ext_methods.contains_key(&t.name) { continue; }
                let Some(body) = body_by_method.get(&t.method_idx) else { continue };
                let params: Vec<String> = if let Some(m) = abc.methods.get(body.method_idx as usize) {
                    (0..m.param_count).map(|i| format!("arg{}", i)).collect()
                } else { vec![] };
                let code = decompiler::decompile_method(body, abc, &t.name, &params);
                ext_methods.insert(t.name.clone(), code);
            }
        }
    }

    // --- Also scan script-level traits for frame* methods ---
    // In SSF2, the character's main MovieClip frame scripts are compiled as
    // script-level (top-level) functions, NOT as class instance methods.
    // These appear in abc.scripts[*].traits, not in any class.
    if xframe_map.is_empty() || frame_scripts.is_empty() {
        for script in &abc.scripts {
            for t in &script.traits {
                if !t.name.starts_with("frame") { continue; }
                let Some(body) = body_by_method.get(&t.method_idx) else { continue };
                if let Some(anim_name) = extract_xframe_name(&body.bytecode, abc) {
                    xframe_map.entry(t.name.clone()).or_insert(anim_name);
                }
                if !frame_scripts.contains_key(&t.name) {
                    let params: Vec<String> = if let Some(method) = abc.methods.get(body.method_idx as usize) {
                        (0..method.param_count).map(|i| format!("arg{}", i)).collect()
                    } else { vec![] };
                    let code = decompiler::decompile_method(body, abc, &t.name, &params);
                    frame_scripts.insert(t.name.clone(), vec![FrameAction {
                        frame: 0,
                        action: code,
                        args: vec![],
                    }]);
                }
            }
        }
        if !xframe_map.is_empty() {
            log::info!("Found {} xframe mappings in script-level traits", xframe_map.len());
        }
    }

    // --- Extract sub-MC frame scripts ---
    // SSF2 animation MovieClips (e.g. captainfalcon_fla.JabCombo_47) register per-frame
    // logic via addFrameScript(N, closure) inside their class iinit.
    // Each class name encodes the main-timeline frame number as a suffix (e.g. _47).
    // We map that back to an animation name via xframe_map (e.g. frame47 → "a").
    //
    // Find the addFrameScript multiname index
    let afs_mn_idx = abc.multinames.iter().position(|mn| &*mn.name == "addFrameScript");
    if let Some(afs_idx) = afs_mn_idx {
        // Encode afs_idx as u30 for pattern matching
        let mut afs_enc = Vec::new();
        let mut v = afs_idx as u32;
        loop { if v < 0x80 { afs_enc.push(v as u8); break; } afs_enc.push((v as u8 & 0x7F) | 0x80); v >>= 7; }
        let callpropvoid_afs: Vec<u8> = std::iter::once(0x4Fu8).chain(afs_enc.iter().copied()).collect();
        let callproperty_afs: Vec<u8> = std::iter::once(0x46u8).chain(afs_enc.iter().copied()).collect();

        // Build ssf2→fm map for extract_ssf2_anim_name lookups
        let xframe_fm_map: BTreeMap<String, String> = xframe_map.values()
            .map(|ssf2| (ssf2.clone(), ssf2.clone())) // identity for now, haxe_gen does FM translation
            .collect();

        let fla_classes: Vec<&Class> = abc.classes.iter()
            .filter(|c| c.name.contains("_fla."))
            .collect();
        log::info!("Found {} _fla. sub-MC classes", fla_classes.len());
        if fla_classes.is_empty() {
            // Dump first 10 class names for debugging
            let names: Vec<&str> = abc.classes.iter().take(20).map(|c| c.name.as_str()).collect();
            log::info!("First 20 class names: {:?}", names);
        }
        for class in &abc.classes {
            // Only process captainfalcon_fla.* style sub-MC classes
            if !class.name.contains("_fla.") { continue; }

            // Map sub-MC class name → SSF2 animation name using the same mapping
            // as sprite_parser (e.g. "JabCombo" → "a", "NAir" → "a_air")
            let ssf2_anim = crate::sprite_parser::extract_ssf2_anim_name(
                &class.name, &char_lower, &xframe_fm_map
            );
            let Some(anim_name) = ssf2_anim else {
                log::debug!("Sub-MC '{}': no animation name mapping, skipping", class.name);
                continue;
            };

            // Get iinit body
            let Some(iinit_body) = body_by_method.get(&class.constructor_idx) else {
                log::debug!("Sub-MC '{}': no body for constructor_idx {}", class.name, class.constructor_idx);
                continue;
            };
            let bc = &iinit_body.bytecode;
            log::debug!("Sub-MC '{}': iinit {} bytes, anim='{}'", class.name, bc.len(), anim_name);

            // Quick scan: does this iinit contain addFrameScript at all?
            let has_afs = bc.windows(callpropvoid_afs.len()).any(|w| w == callpropvoid_afs.as_slice())
                || bc.windows(callproperty_afs.len()).any(|w| w == callproperty_afs.as_slice());
            if !has_afs {
                log::debug!("Sub-MC '{}': no addFrameScript call in iinit", class.name);
            }

            // Scan iinit for addFrameScript(frameNum, closure) pairs
            // Pattern: pushbyte/pushshort <N>, newfunction <method_idx>, callpropvoid(addFrameScript, 2)
            let mut i = 0usize;
            while i < bc.len() {
                // Match callpropvoid(addFrameScript, argc=2)
                if bc[i..].starts_with(&callpropvoid_afs) || bc[i..].starts_with(&callproperty_afs) {
                    // Check arg count = 2 after the multiname
                    let after_mn = i + callpropvoid_afs.len();
                    let mut tmp = after_mn;
                    if let Some(argc) = read_u30_at(bc, &mut tmp) {
                        if argc >= 2 && argc % 2 == 0 {
                            // addFrameScript is variadic: addFrameScript(f0, fn0, f1, fn1, ...)
                            // SSF2 pattern: pushbyte N, getlocal0, getproperty MN (bound method ref)
                            // OR: pushbyte N, newfunction M (inline closure)
                            let scan_start = 0usize; // scan entire iinit
                            let mut pairs: Vec<(u32, String, Option<u32>)> = Vec::new(); // (frame, method_name, method_idx)
                            let mut j = scan_start;
                            let mut pending_frame: Option<u32> = None;
                            while j < i {
                                if bc[j] == OP_PUSHBYTE && j + 1 < bc.len() {
                                    pending_frame = Some(bc[j + 1] as u32);
                                    j += 2;
                                } else if bc[j] == OP_PUSHSHORT {
                                    let mut k = j + 1;
                                    if let Some(v) = read_u30_at(bc, &mut k) {
                                        pending_frame = Some(v);
                                        j = k;
                                    } else { j += 1; }
                                } else if bc[j] == OP_PUSHINT {
                                    let mut k = j + 1;
                                    if let Some(v) = read_u30_at(bc, &mut k) {
                                        let int_val = abc.ints.get(v as usize).copied().unwrap_or(0);
                                        pending_frame = Some(int_val as u32);
                                        j = k;
                                    } else { j += 1; }
                                } else if bc[j] == OP_GETPROPERTY {
                                    let mut k = j + 1;
                                    if let Some(mn_idx) = read_u30_at(bc, &mut k) {
                                        if let Some(frame) = pending_frame.take() {
                                            let method_name = abc.multinames.get(mn_idx as usize)
                                                .map(|m| m.name.to_string()).unwrap_or_default();
                                            // Find the method_idx for this method name in this class
                                            let midx = class.instance_methods.iter()
                                                .find(|t| t.name == method_name)
                                                .map(|t| t.method_idx);
                                            pairs.push((frame, method_name, midx));
                                        }
                                        j = k;
                                    } else { j += 1; }
                                } else if bc[j] == 0x40 { // newfunction
                                    let mut k = j + 1;
                                    if let Some(midx) = read_u30_at(bc, &mut k) {
                                        if let Some(frame) = pending_frame.take() {
                                            pairs.push((frame, format!("closure_{}", midx), Some(midx)));
                                        }
                                        j = k;
                                    } else { j += 1; }
                                } else {
                                    // Skip other opcodes (getlocal0, etc.) without clearing pending_frame
                                    j += 1;
                                }
                            }

                            log::debug!("Sub-MC '{}' anim='{}': addFrameScript argc={}, found {} pairs",
                                class.name, anim_name, argc, pairs.len());

                            for (sub_f, method_name, midx) in &pairs {
                                let body = midx.and_then(|mi| abc.method_bodies.iter().find(|b| b.method_idx == mi));
                                if let Some(closure_body) = body {
                                    let param_count = abc.methods.get(*midx.as_ref().unwrap() as usize)
                                        .map(|m| m.param_count as usize).unwrap_or(0);
                                    let params: Vec<String> = (0..param_count)
                                        .map(|pi| format!("arg{}", pi)).collect();
                                    let code = decompiler::decompile_method(
                                        closure_body, abc,
                                        &format!("{}__frame{}", anim_name, sub_f),
                                        &params
                                    );
                                    let script_key = format!("{}__frame{}", anim_name, sub_f);
                                    frame_scripts.entry(script_key).or_default()
                                        .push(FrameAction {
                                            frame: *sub_f,
                                            action: code,
                                            args: vec![],
                                        });
                                } else {
                                    log::debug!("  frame {} method '{}': no body found", sub_f, method_name);
                                }
                            }

                            i = tmp; // advance past the call
                            continue;
                        }
                    }
                }
                i += 1;
            }
        }

        // Count only sub-MC entries (keys containing "__frame")
        let sub_mc_count = frame_scripts.keys().filter(|k| k.contains("__frame")).count();
        let total_fs = frame_scripts.len();
        log::info!("Sub-MC frame scripts extracted: {} new entries (total frame_scripts now: {})", sub_mc_count, total_fs);
    }

    // --- Fallback: scan ALL method bodies for xframe mappings ---
    // In SSF2, setproperty(self.xframe = "a") appears inside anonymous timeline
    // frame methods linked via SymbolClass, not named class traits. These are
    // compile-time closures with no discoverable name via trait scanning.
    // Strategy: scan every method body, collect pushstring values that immediately
    // precede setproperty(xframe), and build the full ssf2 animation name set.
    if xframe_map.is_empty() {
        // Find the xframe multiname index
        let xframe_mn_idx = abc.multinames.iter().enumerate()
            .find(|(_, mn)| &*mn.name == "xframe")
            .map(|(i, _)| i);

        // Also check ALL multinames named 'xframe' (might be QName and Multiname variants)
        let xframe_mn_indices: Vec<usize> = abc.multinames.iter().enumerate()
            .filter(|(_, mn)| &*mn.name == "xframe")
            .map(|(i, _)| i)
            .collect();
        log::debug!("xframe multiname indices: {:?}", xframe_mn_indices);

        if let Some(xf_idx) = xframe_mn_idx {
            // Build patterns for ALL xframe multiname indices
            let mut all_patterns: Vec<Vec<u8>> = Vec::new();
            for idx in &xframe_mn_indices {
                let mut enc = Vec::new();
                let mut v = *idx as u32;
                loop {
                    if v < 0x80 { enc.push(v as u8); break; }
                    enc.push((v as u8 & 0x7F) | 0x80);
                    v >>= 7;
                }
                // setproperty = 0x61, initproperty = 0x68, setlex = nope
                all_patterns.push(std::iter::once(0x61u8).chain(enc.iter().copied()).collect());
                all_patterns.push(std::iter::once(0x68u8).chain(enc.iter().copied()).collect());
            }
            let _ = xf_idx; // suppress warning
            let _setprop_pattern = all_patterns[0].clone(); // for compat, keep first

            let mut seen_anims: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
            for body in &abc.method_bodies {
                let bc = &body.bytecode;
                // Scan for setproperty/initproperty(xframe) using all known multiname indices
                let mut i = 0usize;
                while i < bc.len() {
                    let matched = all_patterns.iter().any(|p| bc[i..].starts_with(p));
                    if matched {
                        // Scan backwards from i for the most recent pushstring
                        let mut j = i.saturating_sub(1);
                        while j > 0 {
                            if bc[j] == 0x2C {
                                // pushstring: decode u30 idx
                                let mut k = j + 1;
                                if let Some(si) = read_u30_at(bc, &mut k) {
                                    if let Some(s) = abc.strings.get(si as usize) {
                                        if !s.is_empty() && s.len() < 40
                                            && s.chars().all(|c| c.is_alphanumeric() || c == '_')
                                        {
                                            seen_anims.insert(s.to_string());
                                        }
                                    }
                                }
                                break;
                            }
                            j -= 1;
                        }
                        let pat_len = all_patterns.iter().find(|p| bc[i..].starts_with(p.as_slice())).map(|p| p.len()).unwrap_or(1);
                        i += pat_len;
                    } else {
                        i += 1;
                    }
                }
            }

            if !seen_anims.is_empty() {
                log::info!("xframe fallback: found {} SSF2 animation names across all method bodies", seen_anims.len());
                // We can't map frame*→anim name one-to-one here (no frame method names),
                // but we CAN populate the extractor's ssf2_to_fm table used by sprite_parser.
                // Emit a synthetic mapping: ssf2_name → ssf2_name (identity) so the sprite
                // parser can at least match sprites. Real FM name mapping uses the static table.
                for anim in &seen_anims {
                    xframe_map.entry(format!("__all_{}", anim)).or_insert(anim.clone());
                }
            }
        }
    }

    // --- Fallback: scan all bodies if we got nothing ---
    if attacks.is_empty() {
        log::warn!("getAttackStats extraction yielded nothing, falling back to full scan");
        for body in &abc.method_bodies {
            let extracted = extract_attack_objects(&body.bytecode, abc);
            for (name, data) in extracted {
                attacks.entry(name).or_insert(data);
            }
        }
    }
    if stats.is_none() {
        for body in &abc.method_bodies {
            if let Some(s) = extract_stats_from_body(&body.bytecode, abc) {
                stats = Some(s);
                break;
            }
        }
    }

    // Per-character costume data isn't extracted here — both
    // getCostumeData() and applyPalette() are thin wrappers around the
    // engine SWF's runtime API. The real costumes for misc.ssf-style
    // characters come from `scan_all_costume_methods` against misc.ssf
    // (driven from main.rs); palette_gen.rs falls back to k-means on
    // sprite imagery when that isn't available.
    log::info!("Extracted {} attacks, {} frame scripts, {} ext methods, {} xframe mappings, stats={}",
        attacks.len(), frame_scripts.len(), ext_methods.len(), xframe_map.len(), stats.is_some());

    Ok(ExtractedCharacter {
        name: char_name.to_string(),
        attacks,
        projectiles,
        stats,
        frame_scripts,
        ext_methods,
        ext_vars,
        ext_var_inits,
        xframe_map,
        attack_sounds,
        voice_sounds,
        derived_from,
    })
}

/// Find the body of `Main::get<X>()` where the lowercased method-name
/// suffix matches `char_name`. The match is the derived-id rule from
/// `docs/path2_unification_plan.md` §1: strip `get`, lowercase, compare
/// — no camelCase splitting, no underscore handling beyond preservation.
/// Returns the bundle method's body and the original method name.
pub fn find_bundle_method<'a>(abc: &'a AbcFile, char_name: &str)
    -> Option<(&'a MethodBody, String)>
{
    let main = abc.classes.iter().find(|c| c.name == "Main")?;
    let getter = main.instance_methods.iter().find(|t| {
        if !t.name.starts_with("get") { return false; }
        t.name[3..].to_lowercase() == char_name
    })?;
    let body = abc.method_bodies.iter().find(|b| b.method_idx == getter.method_idx)?;
    Some((body, getter.name.clone()))
}

/// Strip `get`, lowercase the remainder, preserve `_`. Returns None
/// for an empty/missing suffix. Mirrors `main.rs::derive_id_from_getter`
/// — kept in sync; both will collapse into one identity once the path 2
/// enumeration fallback is removed in a follow-up commit.
pub fn derive_id_from_bundle_method_name(method_name: &str) -> Option<String> {
    let suffix = method_name.strip_prefix("get")?;
    if suffix.is_empty() { return None; }
    Some(suffix.to_lowercase())
}

/// Derive the PascalCase form used for entity filenames and the
/// `library/scripts/<Pascal>/` subdir per
/// `docs/multi_character_projects_plan.md`. Rule: strip `get` (if
/// present), strip `_` characters, uppercase the first character.
///
/// Examples:
///   `getMario`        → `Mario`
///   `getSandbag`      → `Sandbag`
///   `getBandanaDee`   → `BandanaDee`
///   `getCaptainFalcon`→ `CaptainFalcon`
///   `getGigaBowser`   → `GigaBowser`
///   `getWario_Man`    → `WarioMan`
///   `getMegaMan`      → `MegaMan`
///   `getgameandwatch` → `Gameandwatch`  (no SSF2-side case info)
///   `sandbag`         → `Sandbag`       (no `get` prefix, fallback)
///   `wario_man`       → `Warioman`      (fallback path loses word boundary)
///
/// The fallback path (input without a `get` prefix) is meant for cases
/// where we don't have the SSF2 method name handy — `--name` override,
/// filename fallback for misc.ssf-like SWFs. Multi-char projects
/// always come through detection with the method name available, so
/// the fallback never fires for Wario Man et al.
pub fn pascal_form(method_or_id: &str) -> String {
    let suffix = method_or_id.strip_prefix("get").unwrap_or(method_or_id);
    let no_underscores: String = suffix.chars().filter(|c| *c != '_').collect();
    let mut chars = no_underscores.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Metadata declared by `Main`'s constructor — the SSF "table of contents".
/// Populated by `extract_main_package_metadata` from a single iinit body
/// walk. Used by detection (the `characters` list IS the roster) and by
/// the conversion log (id/guid go into `ssf2_source`).
#[derive(Debug, Clone, Default)]
pub struct MainPackageMetadata {
    /// Value of `register("id", ...)` — the SSF package id; matches the
    /// filename stem in every observed corpus SSF. None if the id call
    /// is absent (none observed) or the value didn't match the
    /// expected string-literal shape.
    pub id: Option<String>,
    /// Value of `register("guid", ...)` — per-SSF UUID. Useful for
    /// downstream traceability.
    pub guid: Option<String>,
    /// `(derived_id, method_name)` pairs collected from
    /// `register("characters", [self.getX(), self.getY(), ...])`.
    /// Order matches the array order in the iinit (which mirrors the
    /// engine's roster order). Empty when the walk failed — callers
    /// fall back to the path 2 instance-method enumeration.
    pub characters: Vec<(String, String)>,
    /// `register("music", {id:...}, …)` track ids (the `bgm_*` resource names) in
    /// order. For a stage, its intended SSF2 soundtrack; empty for characters / when
    /// no music block is present.
    pub music: Vec<String>,
}

/// Read Main's constructor and pull out `id`, `guid`, and the
/// `characters` array. Returns None if Main is absent.
pub fn extract_main_package_metadata(abc: &AbcFile) -> Option<MainPackageMetadata> {
    let main = abc.classes.iter().find(|c| c.name == "Main")?;
    let body = abc.method_bodies.iter().find(|b| b.method_idx == main.constructor_idx)?;
    Some(MainPackageMetadata {
        id:         scan_register_string_arg(body, abc, "id"),
        guid:       scan_register_string_arg(body, abc, "guid"),
        characters: scan_register_characters_array(body, abc),
        music:      scan_register_music_ids(body, abc),
    })
}

/// Walk Main's constructor to collect the `register("music", {id:...}, …)` track
/// ids — every `bgm_*` string between the `"music"` key and the `register` call that
/// consumes it (the SSF2 stage's intended soundtrack, in order).
fn scan_register_music_ids(body: &MethodBody, abc: &AbcFile) -> Vec<String> {
    let Some(music_key) = abc.strings.iter().position(|s| &**s == "music") else { return Vec::new() };
    let bc = &body.bytecode;
    let (mut i, mut out, mut collecting) = (0usize, Vec::new(), false);
    while i < bc.len() {
        let op = bc[i];
        if op == OP_PUSHSTRING {
            let mut j = i + 1;
            if let Some(s_idx) = read_u30_at(bc, &mut j) {
                if s_idx as usize == music_key {
                    collecting = true;
                } else if collecting {
                    if let Some(s) = abc.strings.get(s_idx as usize) {
                        if s.starts_with("bgm_") { out.push(s.to_string()); }
                    }
                }
            }
            i = j;
            continue;
        }
        // the first call after the "music" key is `register("music", …)` — end of block.
        if collecting && (op == OP_CALLPROPVOID || op == OP_CALLPROPERTY) { break; }
        i += 1;
        skip_opcode_operands(op, bc, &mut i);
    }
    out
}

/// Walk a method body to find `pushstring KEY ; pushstring VALUE ;
/// callpropvoid register, 2` and return `VALUE`. Used for the
/// string-valued `register` keys (`id`, `guid`).
fn scan_register_string_arg(body: &MethodBody, abc: &AbcFile, key: &str) -> Option<String> {
    let key_idx = abc.strings.iter().position(|s| &**s == key)? as u32;
    let bc = &body.bytecode;
    let mut i = 0;
    while i < bc.len() {
        let op = bc[i];
        if op == OP_PUSHSTRING {
            let mut j = i + 1;
            let s_idx = read_u30_at(bc, &mut j)?;
            // The id/guid pattern is `pushstring KEY ; pushstring VALUE`
            // back-to-back — value immediately follows.
            if s_idx == key_idx && j < bc.len() && bc[j] == OP_PUSHSTRING {
                let mut k = j + 1;
                let v_idx = read_u30_at(bc, &mut k)?;
                return abc.strings.get(v_idx as usize).map(|s| s.to_string());
            }
            i = j;
            continue;
        }
        i += 1;
        skip_opcode_operands(op, bc, &mut i);
    }
    None
}

/// Walk a method body to find `pushstring "characters" ; ... ; newarray N`
/// and collect every `callproperty` / `callpropvoid` whose multiname
/// starts with `get` between those two points. Each collected name is
/// folded through `derive_id_from_bundle_method_name` and paired with
/// its raw method name.
fn scan_register_characters_array(body: &MethodBody, abc: &AbcFile) -> Vec<(String, String)> {
    let Some(key_idx) = abc.strings.iter()
        .position(|s| &**s == "characters").map(|i| i as u32)
    else { return Vec::new() };

    let bc = &body.bytecode;
    let mut i = 0;
    while i < bc.len() {
        let op = bc[i];
        if op == OP_PUSHSTRING {
            let mut j = i + 1;
            let Some(s_idx) = read_u30_at(bc, &mut j) else { return Vec::new() };
            if s_idx == key_idx {
                // Walk forward, collecting getters, until newarray.
                let mut k = j;
                let mut out: Vec<(String, String)> = Vec::new();
                while k < bc.len() {
                    let op2 = bc[k];
                    if op2 == OP_NEWARRAY {
                        return out;
                    }
                    if op2 == OP_CALLPROPERTY || op2 == OP_CALLPROPVOID {
                        let mut m = k + 1;
                        let Some(mn_idx) = read_u30_at(bc, &mut m) else { break };
                        // arg count — read + discard
                        read_u30_at(bc, &mut m);
                        let name = abc.multinames.get(mn_idx as usize)
                            .map(|mn| mn.name.to_string()).unwrap_or_default();
                        if let Some(id) = derive_id_from_bundle_method_name(&name) {
                            out.push((id, name));
                        }
                        k = m;
                        continue;
                    }
                    k += 1;
                    skip_opcode_operands(op2, bc, &mut k);
                }
                return out;
            }
            i = j;
            continue;
        }
        i += 1;
        skip_opcode_operands(op, bc, &mut i);
    }
    Vec::new()
}

/// Advance `i` past the operand bytes of the opcode whose byte was
/// already consumed (so on entry, `i` points one past the opcode
/// byte). Mirrors the dispatch in `scan_method`, kept thin enough to
/// be the truth-source for the small dedicated walkers (constructor
/// + normalStats_id) without dragging in the full AVM2 stack
/// interpreter.
fn skip_opcode_operands(op: u8, bc: &[u8], i: &mut usize) {
    match op {
        OP_PUSHDOUBLE | OP_PUSHSTRING | OP_PUSHINT | OP_PUSHUINT |
        OP_COERCE     | OP_GETLEX     | OP_FINDPROPSTRICT | OP_FINDPROP |
        OP_GETPROPERTY| OP_INITPROPERTY| OP_SETPROPERTY |
        OP_GETLOCAL   | OP_SETLOCAL   | OP_NEWARRAY   | OP_NEWOBJECT => {
            read_u30_at(bc, i);
        }
        OP_PUSHBYTE   => { if *i < bc.len() { *i += 1; } }
        OP_PUSHSHORT  => { read_u30_at(bc, i); }
        OP_CALLPROPERTY | OP_CALLPROPVOID | OP_CONSTRUCTPROP => {
            read_u30_at(bc, i); read_u30_at(bc, i);
        }
        OP_CONSTRUCT => { read_u30_at(bc, i); }
        OP_JUMP | OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFLT |
        OP_IFLE | OP_IFGT  | OP_IFGE    | OP_IFSTRICTEQ | OP_IFSTRICTNE
            if *i + 3 <= bc.len() => { *i += 3; }
        _ => {}
    }
}

/// Scan a method body for the literal string value of `normalStats_id`.
/// The AVM2 pattern emitted by SSF2's source is:
///     pushstring "normalStats_id"
///     pushstring "<value>"
///     setproperty/initproperty
/// We find the first occurrence of `pushstring "normalStats_id"` and
/// return the very next pushed string. Used by `extract_character` to
/// detect transformation/alternate-form bundles (their `normalStats_id`
/// is the parent character's id, not their own derived id).
pub fn extract_normal_stats_id(body: &MethodBody, abc: &AbcFile) -> Option<String> {
    let needle_idx = abc.strings.iter().position(|s| &**s == "normalStats_id")? as u32;
    let bytes = &body.bytecode;
    let mut i = 0;
    while i < bytes.len() {
        let op = bytes[i];
        if op == OP_PUSHSTRING {
            let mut j = i + 1;
            if let Some(idx) = read_u30_at(bytes, &mut j) {
                if idx == needle_idx {
                    // Walk forward to the next OP_PUSHSTRING and take its value.
                    let mut k = j;
                    while k < bytes.len() {
                        if bytes[k] == OP_PUSHSTRING {
                            let mut m = k + 1;
                            let v = read_u30_at(bytes, &mut m)?;
                            return abc.strings.get(v as usize).map(|s| s.to_string());
                        }
                        k += 1;
                    }
                    return None;
                }
                i = j; continue;
            }
        }
        i += 1;
    }
    None
}

/// Extract the string value of a single object-literal field from a bundle
/// method body, using the same `pushstring <field-name>; … ; pushstring
/// <value>` pattern as `extract_normal_stats_id`. Returns None if the field
/// name isn't in the string pool, or isn't pushed in this body.
fn extract_field_string(body: &MethodBody, abc: &AbcFile, field: &str) -> Option<String> {
    let needle_idx = abc.strings.iter().position(|s| &**s == field)? as u32;
    let bytes = &body.bytecode;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == OP_PUSHSTRING {
            let mut j = i + 1;
            if let Some(idx) = read_u30_at(bytes, &mut j) {
                if idx == needle_idx {
                    // Value is the next string pushed after the field name.
                    let mut k = j;
                    while k < bytes.len() {
                        if bytes[k] == OP_PUSHSTRING {
                            let mut m = k + 1;
                            let v = read_u30_at(bytes, &mut m)?;
                            return abc.strings.get(v as usize).map(|s| s.to_string());
                        }
                        k += 1;
                    }
                    return None;
                }
                i = j; continue;
            }
        }
        i += 1;
    }
    None
}

/// Collect a contiguous 1-based run of indexed string fields from a bundle
/// body: `{prefix}1{suffix}`, `{prefix}2{suffix}`, … stopping at the first
/// index whose field isn't present in this body. Backs SSF2's
/// `attackSound{N}_id` / `attackVoice{N}_id` sound tables (the targets of
/// `playAttackSound(N)` / `playVoiceSound(N)`, both 1-based).
pub fn extract_indexed_string_fields(
    body: &MethodBody, abc: &AbcFile, prefix: &str, suffix: &str,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut n = 1usize;
    loop {
        let field = format!("{prefix}{n}{suffix}");
        match extract_field_string(body, abc, &field) {
            Some(v) => { out.push(v); n += 1; }
            None => break,
        }
    }
    out
}

/// Symbolic value pushed onto the simulated AVM2 stack by `scan_method`
/// and inspected by `AbcVisitor` implementations to recognise object
/// literals (`newobject`) that carry SSF2 attack / projectile / stat /
/// costume data.
#[derive(Debug, Clone)]
pub(crate) enum StackVal {
    Str(String),
    Num(f64),
    Bool(()),  // unused field — kept for stack value compatibility
    Null,
    /// A parsed object literal from newobject
    Obj(BTreeMap<String, StackVal>),
    /// A parsed array from newarray
    Arr(Vec<StackVal>),
    /// A visitor-defined marker carried on the simulated stack (e.g. a plane
    /// accessor's result, or a `getlex` class name). The scanner never creates
    /// these itself — only the property/lex hooks do — so default visitors
    /// (which don't override the hooks) never see them.
    Tag(String),
    Unknown,
}

// ─── Shared AVM2 stack-sim scanner ───────────────────────────────────────────
//
// SSF2 keeps several flavours of structured data inside AVM2 bytecode:
//   1. `getAttackStats()` — one top-level newobject keyed by move name,
//       each value is a hitbox object literal.
//   2. `getProjectileStats()` — one top-level newobject keyed by
//      projectile name, each value is a physics + attackBoxes object.
//   3. `getOwnStats()` / fallback — one newobject with character physics keys.
//   4. `misc.ssf :: getCostumeData()` (and per-character costume methods)
//      — one or more newobjects matching one of two palette-table shapes.
//
// Each used to live in its own bespoke stack interpreter (~150 lines × 4 = ~600
// lines of nearly-identical opcode handlers). They've now been unified
// behind `scan_method` + the `AbcVisitor` trait. Each former extractor
// is a thin wrapper that builds a visitor, calls `scan_method`, and
// reads the visitor's accumulator. See the visitors immediately below.
//
// We did NOT consolidate `extract_ssf2_stats` — it's a fundamentally
// different algorithm (a linear pushstring-followed-by-numeric-push scan
// that doesn't model the stack at all). Folding it into the visitor
// model would require either complicating the scanner or weakening the
// extractor's recognition.

/// What `AbcVisitor::on_newobject` wants the scanner to do with the
/// freshly-built object literal.
pub(crate) enum NewObjectAction {
    /// Push StackVal::Obj(obj) onto the stack — the default for visitors
    /// that may need to look at this object again as a sub-value in a
    /// later newobject.
    PushObj(BTreeMap<String, StackVal>),
    /// Push some other StackVal (typically StackVal::Unknown) instead
    /// of the obj. Used by visitors that recorded the obj as a
    /// top-level extraction target and don't need its identity on the
    /// stack any more.
    Push(StackVal),
    /// Halt scanning entirely. Used by single-shot extractors that have
    /// already found what they came for (e.g. CharacterStats).
    Stop,
}

/// Hook surface for `scan_method`. Default impls match the
/// attack/projectile/stats simulators' common semantics; the costume
/// visitor overrides a couple of behaviours where its parsing differs.
pub(crate) trait AbcVisitor {
    /// Called when a `newobject(N)` opcode completes. The scanner has
    /// already drained 2N items from the stack and built the obj map;
    /// the visitor decides what to record, then returns an action
    /// telling the scanner what to push (or whether to stop).
    fn on_newobject(
        &mut self,
        obj: BTreeMap<String, StackVal>,
        current_char: &Option<String>,
    ) -> NewObjectAction {
        let _ = current_char;
        NewObjectAction::PushObj(obj)
    }

    /// Called when a `newarray(N)` opcode completes with `items` drained
    /// in declared order. Default: drop the items and push
    /// StackVal::Unknown — matches the attack/projectile/stats sims,
    /// which never inspect array contents. The costume visitor returns
    /// `StackVal::Arr(items)` so it can read color literals back.
    fn on_newarray(&mut self, items: Vec<StackVal>) -> StackVal {
        let _ = items;
        StackVal::Unknown
    }

    /// If true, the scanner uses the costume-style getproperty / set-
    /// property handling: track a runtime-key or static-name match
    /// against `current_char`, and on STATIC getproperty preserve the
    /// receiver on the stack (so `_loc1_.mario.push(...)` still binds
    /// the right receiver). False (default) = pop receiver and push
    /// Unknown, matching the attack/projectile sims' post-§3.5-fix
    /// behaviour.
    fn costume_getproperty_semantics(&self) -> bool { false }

    // ── Property/lex hooks (for the stage plane-map + actor visitors) ──
    // All default to `None`, meaning "scanner keeps its existing behavior"
    // (push Unknown). So the attack/projectile/stats/costume visitors, which
    // don't override these, are byte-for-byte unaffected. A stage visitor
    // returns `Some(StackVal::Tag(...))` to carry a marker (a plane, a class
    // name, a spawned-actor handle) forward on the simulated stack.

    /// `getlex <Name>` / `findpropstrict <Name>`: the resolved name. Return a
    /// marker to put on the stack (e.g. the class name for a following call).
    fn on_getlex(&mut self, _name: &str) -> Option<StackVal> { None }
    /// `callproperty/callpropvoid <method>(args)` on `receiver` (already drained
    /// from the stack: receiver + args). For callproperty the return is pushed
    /// (Unknown if None); for callpropvoid it's only a side-effect hook.
    fn on_callproperty(&mut self, _method: &str, _args: &[StackVal], _receiver: &StackVal) -> Option<StackVal> { None }
    /// `constructprop <Class>(args)`. Return a marker for the constructed value.
    fn on_constructprop(&mut self, _class: &str, _args: &[StackVal]) -> Option<StackVal> { None }
    /// `getproperty <prop>` on `receiver`. Return a marker to propagate the
    /// receiver's tag through a chained access (e.g. `getBackground().clip`).
    fn on_getproperty(&mut self, _prop: &str, _receiver: &StackVal) -> Option<StackVal> { None }
    /// Opt in to locals tracking: `setlocal N` stores the stack top into a
    /// locals array and `getlocal N` reloads it, so a visitor can follow a
    /// value stashed in a temp (e.g. a spawned actor reloaded for `setX`).
    /// Default false = `getlocal` pushes Unknown, exactly as before.
    fn track_locals(&self) -> bool { false }
}

/// One pass over a method body. Built-in handlers cover every opcode
/// every former simulator handled. The visitor's hooks decide what to
/// record on newobject / newarray and (optionally) how to treat
/// getproperty — see the trait above.
pub(crate) fn scan_method<V: AbcVisitor>(bytecode: &[u8], abc: &AbcFile, visitor: &mut V) {
    let mut stack: Vec<StackVal> = Vec::new();
    let mut current_char: Option<String> = None;
    let mut i = 0;
    let costume_mode = visitor.costume_getproperty_semantics();
    let track_locals = visitor.track_locals();
    let mut locals: Vec<StackVal> = Vec::new();
    fn set_local(locals: &mut Vec<StackVal>, n: usize, v: StackVal) {
        if n >= locals.len() { locals.resize(n + 1, StackVal::Unknown); }
        locals[n] = v;
    }

    while i < bytecode.len() {
        let op = bytecode[i];
        i += 1;
        match op {
            // ── Constant pushes ─────────────────────────────────────────
            OP_PUSHSTRING => {
                if let Some(idx) = read_u30_at(bytecode, &mut i) {
                    let s = abc.strings.get(idx as usize).map(|s| s.to_string()).unwrap_or_default();
                    stack.push(StackVal::Str(s));
                }
            }
            OP_PUSHDOUBLE => {
                if let Some(idx) = read_u30_at(bytecode, &mut i) {
                    stack.push(StackVal::Num(abc.doubles.get(idx as usize).copied().unwrap_or(0.0)));
                }
            }
            OP_PUSHBYTE => {
                if i < bytecode.len() {
                    stack.push(StackVal::Num(bytecode[i] as i8 as f64));
                    i += 1;
                }
            }
            OP_PUSHSHORT => {
                if let Some(v) = read_u30_at(bytecode, &mut i) {
                    stack.push(StackVal::Num(v as i16 as f64));
                }
            }
            OP_PUSHINT => {
                if let Some(idx) = read_u30_at(bytecode, &mut i) {
                    stack.push(StackVal::Num(abc.ints.get(idx as usize).copied().unwrap_or(0) as f64));
                }
            }
            OP_PUSHUINT => {
                if let Some(idx) = read_u30_at(bytecode, &mut i) {
                    stack.push(StackVal::Num(abc.uints.get(idx as usize).copied().unwrap_or(0) as f64));
                }
            }
            OP_PUSHTRUE | OP_PUSHFALSE => stack.push(StackVal::Bool(())),
            OP_PUSHNULL | OP_PUSHNAN => stack.push(StackVal::Null),

            // ── newobject (the only visitor-driven push) ────────────────
            OP_NEWOBJECT => {
                if let Some(count) = read_u30_at(bytecode, &mut i) {
                    let count = count as usize;
                    let needed = count * 2;
                    let mut obj: BTreeMap<String, StackVal> = BTreeMap::new();
                    if stack.len() >= needed {
                        let pairs: Vec<_> = stack.drain(stack.len() - needed..).collect();
                        for chunk in pairs.chunks(2) {
                            if let StackVal::Str(k) = &chunk[0] {
                                obj.insert(k.clone(), chunk[1].clone());
                            }
                        }
                    }
                    match visitor.on_newobject(obj, &current_char) {
                        NewObjectAction::PushObj(o)  => stack.push(StackVal::Obj(o)),
                        NewObjectAction::Push(v)     => stack.push(v),
                        NewObjectAction::Stop        => return,
                    }
                }
            }

            // ── newarray (visitor-overridable) ──────────────────────────
            OP_NEWARRAY => {
                if let Some(count) = read_u30_at(bytecode, &mut i) {
                    let count = count as usize;
                    let drain_start = stack.len().saturating_sub(count);
                    let items: Vec<StackVal> = stack.drain(drain_start..).collect();
                    stack.push(visitor.on_newarray(items));
                }
            }

            // ── Calls & construction (drain args+receiver, hook, push) ──
            OP_CALLPROPERTY | OP_CALLPROPVOID => {
                let mn_idx = read_u30_at(bytecode, &mut i).unwrap_or(0);
                let argc = read_u30_at(bytecode, &mut i).unwrap_or(0) as usize;
                let method = abc.multinames.get(mn_idx as usize).map(|m| m.name.to_string()).unwrap_or_default();
                let drain = stack.len().min(argc + 1);
                let drained: Vec<StackVal> = stack.drain(stack.len() - drain..).collect();
                let (receiver, args) = drained.split_first()
                    .map(|(r, a)| (r.clone(), a.to_vec()))
                    .unwrap_or((StackVal::Null, Vec::new()));
                let hooked = visitor.on_callproperty(&method, &args, &receiver);
                if op == OP_CALLPROPERTY { stack.push(hooked.unwrap_or(StackVal::Unknown)); }
            }
            OP_CONSTRUCTPROP => {
                let mn_idx = read_u30_at(bytecode, &mut i).unwrap_or(0);
                let argc = read_u30_at(bytecode, &mut i).unwrap_or(0) as usize;
                let class = abc.multinames.get(mn_idx as usize).map(|m| m.name.to_string()).unwrap_or_default();
                let drain = stack.len().min(argc + 1);
                let drained: Vec<StackVal> = stack.drain(stack.len() - drain..).collect();
                let args: Vec<StackVal> = drained.into_iter().skip(1).collect();
                let hooked = visitor.on_constructprop(&class, &args);
                stack.push(hooked.unwrap_or(StackVal::Unknown));
            }
            OP_CONSTRUCT => {
                let argc = read_u30_at(bytecode, &mut i).unwrap_or(0) as usize;
                let drain = stack.len().min(argc + 1);
                stack.drain(stack.len() - drain..);
                stack.push(StackVal::Unknown);
            }

            // ── Property reads/writes ───────────────────────────────────
            // setproperty/initproperty are the same in every former sim:
            // drain 2 items. The costume sim ALSO inspects the multiname
            // for a static char name. Track that opportunistically.
            OP_SETPROPERTY | OP_INITPROPERTY => {
                let mn_idx = read_u30_at(bytecode, &mut i).unwrap_or(0);
                if costume_mode {
                    let mn = abc.multinames.get(mn_idx as usize);
                    let is_runtime = mn.map(|m| m.kind == 0x1B || m.kind == 0x1C).unwrap_or(false);
                    let static_name = mn.and_then(|m| if m.name.is_empty() { None } else { Some(m.name.to_string()) });
                    if is_runtime {
                        let _val = stack.pop();
                        let key = stack.pop();
                        stack.pop(); // object
                        if let Some(StackVal::Str(s)) = &key {
                            if looks_like_char_name(s) { current_char = Some(s.clone()); }
                        }
                    } else {
                        stack.pop(); stack.pop();
                        if let Some(name) = static_name {
                            if looks_like_char_name(&name) { current_char = Some(name); }
                        }
                    }
                } else if stack.len() >= 2 {
                    stack.truncate(stack.len() - 2);
                }
            }

            // getproperty splits cleanly into two modes:
            //  - default: pop receiver, push Unknown (post-§3.5 fix)
            //  - costume: track current_char from multiname, and on
            //    STATIC getproperty preserve the receiver so chained
            //    `_loc1_.mario.push(...)` still resolves.
            OP_GETPROPERTY => {
                let mn_idx = read_u30_at(bytecode, &mut i).unwrap_or(0);
                if costume_mode {
                    let mn = abc.multinames.get(mn_idx as usize);
                    let is_runtime = mn.map(|m| m.kind == 0x1B || m.kind == 0x1C).unwrap_or(false);
                    let static_name = mn.and_then(|m| if m.name.is_empty() { None } else { Some(m.name.to_string()) });
                    if is_runtime {
                        let key = stack.pop();
                        stack.pop(); // receiver
                        if let Some(StackVal::Str(s)) = &key {
                            if looks_like_char_name(s) { current_char = Some(s.clone()); }
                        }
                        stack.push(StackVal::Null);
                    } else {
                        let top = stack.pop().unwrap_or(StackVal::Null);
                        if let Some(name) = static_name {
                            if looks_like_char_name(&name) { current_char = Some(name); }
                            stack.push(top); // preserve receiver for chained access
                        } else {
                            stack.push(StackVal::Null);
                        }
                    }
                } else {
                    let mn = abc.multinames.get(mn_idx as usize);
                    let name = mn.map(|m| m.name.to_string()).unwrap_or_default();
                    // a stage visitor (track_locals) wants an accurate stack: a RUNTIME multiname
                    // (RTQNameL 0x1B / MultinameL 0x1C, e.g. `arr[0]`) takes its index off the stack
                    // BELOW the receiver, so pop it first or a chain desyncs (`getCameraBackgrounds()[0].mc`).
                    if track_locals && mn.map(|m| m.kind == 0x1B || m.kind == 0x1C).unwrap_or(false) {
                        stack.pop();
                    }
                    let receiver = stack.pop().unwrap_or(StackVal::Null);
                    stack.push(visitor.on_getproperty(&name, &receiver).unwrap_or(StackVal::Unknown));
                }
            }

            OP_FINDPROPSTRICT | OP_FINDPROP | OP_GETLEX => {
                let mn_idx = read_u30_at(bytecode, &mut i).unwrap_or(0);
                let name = abc.multinames.get(mn_idx as usize).map(|m| m.name.to_string()).unwrap_or_default();
                stack.push(visitor.on_getlex(&name).unwrap_or(StackVal::Unknown));
            }

            // ── Coerce / convert — operand-bearing but mostly no-ops ────
            OP_COERCE | OP_COERCE_A | OP_CONVERT_D | OP_CONVERT_I => {
                if op == OP_COERCE { read_u30_at(bytecode, &mut i); }
            }

            // ── Plumbing ───────────────────────────────────────────────
            OP_NOP | OP_LABEL => {}
            OP_POP => { stack.pop(); }
            OP_DUP => { if let Some(top) = stack.last().cloned() { stack.push(top); } }
            OP_SWAP => { let len = stack.len(); if len >= 2 { stack.swap(len-1, len-2); } }
            OP_NEGATE => {
                match stack.pop() {
                    Some(StackVal::Num(v)) => stack.push(StackVal::Num(-v)),
                    _ => stack.push(StackVal::Unknown),
                }
            }
            OP_ADD | OP_SUBTRACT | OP_MULTIPLY | OP_DIVIDE => {
                let b = stack.pop(); let a = stack.pop();
                match (a, b) {
                    (Some(StackVal::Num(a)), Some(StackVal::Num(b))) => {
                        let r = match op {
                            OP_ADD => a+b, OP_SUBTRACT => a-b,
                            OP_MULTIPLY => a*b, OP_DIVIDE => a/b, _ => 0.0
                        };
                        stack.push(StackVal::Num(r));
                    }
                    _ => stack.push(StackVal::Unknown),
                }
            }
            OP_GETLOCAL0 | OP_GETLOCAL1 | OP_GETLOCAL2 | OP_GETLOCAL3 => {
                let n = (op - OP_GETLOCAL0) as usize;
                stack.push(if track_locals { locals.get(n).cloned().unwrap_or(StackVal::Unknown) } else { StackVal::Unknown });
            }
            OP_GETLOCAL => {
                let n = read_u30_at(bytecode, &mut i).unwrap_or(0) as usize;
                stack.push(if track_locals { locals.get(n).cloned().unwrap_or(StackVal::Unknown) } else { StackVal::Unknown });
            }
            OP_SETLOCAL0 | OP_SETLOCAL1 | OP_SETLOCAL2 | OP_SETLOCAL3 => {
                let v = stack.pop().unwrap_or(StackVal::Null);
                if track_locals { set_local(&mut locals, (op - OP_SETLOCAL0) as usize, v); }
            }
            OP_SETLOCAL => {
                let n = read_u30_at(bytecode, &mut i).unwrap_or(0) as usize;
                let v = stack.pop().unwrap_or(StackVal::Null);
                if track_locals { set_local(&mut locals, n, v); }
            }
            OP_RETURNVALUE => { stack.pop(); }
            OP_RETURNVOID => {}
            OP_JUMP | OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFLT |
            OP_IFLE | OP_IFGT | OP_IFGE | OP_IFNLT | OP_IFNLE | OP_IFNGT | OP_IFNGE |
            OP_IFSTRICTEQ | OP_IFSTRICTNE => {
                if i + 3 <= bytecode.len() { i += 3; }
                if op != OP_JUMP { stack.pop(); }
            }
            _ => {}
        }
        // Bound the stack so a malformed body can't blow memory.
        if stack.len() > 512 { stack.drain(0..256); }
    }
}

/// Helper used by the costume visitor + the costume-mode getproperty
/// branch of `scan_method`. Lowercase alphabetic-only strings between 3
/// and 24 chars long are accepted as character ids.
fn looks_like_char_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_alphanumeric())
        && s.chars().next().map(|c| c.is_ascii_lowercase()).unwrap_or(false)
        && s.len() >= 3 && s.len() <= 24
}

/// Visitor for `getAttackStats()`-style bodies. A top-level newobject
/// whose keys are SSF2 attack-move names (`a`, `b_air`, `a_air_forward`,
/// …) is the attack table; each value carries the hitbox data.
struct AttackVisitor {
    result: BTreeMap<String, AttackData>,
}

impl AbcVisitor for AttackVisitor {
    fn on_newobject(
        &mut self,
        obj: BTreeMap<String, StackVal>,
        _current_char: &Option<String>,
    ) -> NewObjectAction {
        let attack_keys_found: Vec<_> = obj.keys()
            .filter(|k| is_attack_name(k))
            .cloned().collect();
        for move_name in &attack_keys_found {
            if let Some(val) = obj.get(move_name) {
                let hitboxes = extract_hitboxes_from_val(val);
                if !hitboxes.is_empty() {
                    self.result.insert(normalize_attack_name(move_name), AttackData { hitboxes });
                }
            }
        }
        NewObjectAction::PushObj(obj)
    }
}

pub(crate) fn extract_attack_objects(bytecode: &[u8], abc: &AbcFile) -> BTreeMap<String, AttackData> {
    let mut v = AttackVisitor { result: BTreeMap::new() };
    scan_method(bytecode, abc, &mut v);
    v.result
}

/// Captures the single getAttackStats newobject that yields the MOST hitboxes — for a HAZARD,
/// whose getAttackStats returns the attack object DIRECTLY (`{ attackBoxes: {...} }`), not the
/// character pattern of an object keyed by move name. (The nested `attackBoxes` map fires its own
/// newobject too; "most hitboxes" picks the complete one and dedups the partial.)
struct HazardAttackVisitor {
    best: Vec<BTreeMap<String, f64>>,
}
impl AbcVisitor for HazardAttackVisitor {
    fn on_newobject(&mut self, obj: BTreeMap<String, StackVal>, _c: &Option<String>) -> NewObjectAction {
        let hbs = extract_hitboxes_from_val(&StackVal::Obj(obj.clone()));
        if hbs.len() > self.best.len() {
            self.best = hbs;
        }
        NewObjectAction::PushObj(obj)
    }
}

/// The behavior values a hazard class's `update()`/`initialize()` actually use, recovered by
/// stepping the class instead of hardcoding them: the camera-shake amplitude (`shake`/`shakeCamera`
/// argument), the rise speed (`setYSpeed`), the fall gravity (`updateEnemyStats({gravity})`), the
/// self-platform box (`createSelfPlatform`), the dust effect (`attachEffect`), and the sounds
/// (`playSound`). The FM CGO script is driven by these so the hazard's behavior comes from its own
/// code, not a per-hazard template of constants.
#[derive(Default, Clone, Debug)]
pub struct EnemyBehavior {
    pub shake: Option<f64>,
    pub rise_yspeed: Option<f64>,
    pub fall_gravity: Option<f64>,
    pub self_platform: Option<(f64, f64, f64, f64)>,
    pub dust: Option<(String, f64, f64)>,
    pub sounds: Vec<String>,
    /// The class adds itself as a camera target (`addToCamera`) — the FM port mirrors it with
    /// `match.getCamera().addTarget(self)` while the hazard is engaged.
    pub camera_target: bool,
}
struct BehaviorVisitor {
    b: EnemyBehavior,
}
impl AbcVisitor for BehaviorVisitor {
    fn on_callproperty(&mut self, method: &str, args: &[StackVal], _r: &StackVal) -> Option<StackVal> {
        let num = |i: usize| if let Some(StackVal::Num(n)) = args.get(i) { Some(*n) } else { None };
        match method {
            "shake" | "shakeCamera" => { if let Some(n) = num(0) { self.b.shake = Some(n); } }
            "setYSpeed" => { if let Some(n) = num(0) { if n < 0.0 { self.b.rise_yspeed = Some(n); } } }
            "updateEnemyStats" => {
                if let Some(StackVal::Obj(o)) = args.first() {
                    if let Some(StackVal::Num(g)) = o.get("gravity") { if *g > 0.0 { self.b.fall_gravity = Some(*g); } }
                }
            }
            "createSelfPlatform" => {
                if let (Some(a), Some(b), Some(c), Some(d)) = (num(0), num(1), num(2), num(3)) {
                    self.b.self_platform = Some((a, b, c, d));
                }
            }
            "attachEffect" => {
                if let Some(StackVal::Str(name)) = args.first() {
                    let (mut sx, mut sy) = (1.0, 1.0);
                    if let Some(StackVal::Obj(o)) = args.get(1) {
                        if let Some(StackVal::Num(v)) = o.get("scaleX") { sx = *v; }
                        if let Some(StackVal::Num(v)) = o.get("scaleY") { sy = *v; }
                    }
                    self.b.dust = Some((name.clone(), sx, sy));
                }
            }
            "playSound" => { if let Some(StackVal::Str(s)) = args.first() { if !self.b.sounds.contains(s) { self.b.sounds.push(s.clone()); } } }
            "addToCamera" => { self.b.camera_target = true; }
            _ => {}
        }
        None
    }
}
pub(crate) fn extract_enemy_behavior(abc: &AbcFile, class_name: &str) -> EnemyBehavior {
    let Some(class) = abc.classes.iter().find(|c| c.name == class_name) else { return EnemyBehavior::default() };
    let mut v = BehaviorVisitor { b: EnemyBehavior::default() };
    for m in ["initialize", "update", "runAI", "move", "releaseEnemy"] {
        if let Some(t) = class.instance_methods.iter().find(|t| &*t.name == m) {
            if let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) {
                scan_method(&body.bytecode, abc, &mut v);
            }
        }
    }
    v.b
}

/// Collects the animation labels a hazard class plays via `forceAttack("<label>")` — stepping the
/// class's lifecycle methods to learn its REAL animation set (the Thwomp's entrance/idle/fall, the
/// HyruleTornado's "stand"), the code-referenced handle to its art clip, instead of matching a
/// library symbol by keyword.
#[derive(Default)]
struct ForceAttackVisitor {
    labels: Vec<String>,
}
impl AbcVisitor for ForceAttackVisitor {
    fn on_callproperty(&mut self, method: &str, args: &[StackVal], _r: &StackVal) -> Option<StackVal> {
        if method == "forceAttack" {
            if let Some(StackVal::Str(s)) = args.first() {
                if !s.is_empty() && !self.labels.contains(s) { self.labels.push(s.clone()); }
            }
        }
        None
    }
}
/// How an SWF sub-clip's own timeline ENDS its playback, recovered from its Flash-generated
/// `<doc>_fla.<Clip>_NN` class (the 1-based `frameN` instance methods carry the frame scripts,
/// decompiled through the standard pipeline). A clip with no bound class or no hold genuinely loops.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TimelineHold {
    /// `stop()` at 1-based frame N: plays to N and freezes there.
    StopAt(u32),
    /// `gotoAndStop(target)` at 1-based frame N: plays to N, then freezes at `target`
    /// (a frame-label name, or a 1-based frame number rendered as digits).
    GotoStop(String, u32),
}

/// A stage's engine-spawned FALLER cycle (the thwomp pattern), stepped from the code: the enemy
/// class's entrance delay + landed wait (its initialize's FrameTimers, in declaration order), the
/// stage class's spawn cadence (the sum of its two largest timers: active + rest phases of the
/// spawn machine — an approximation until the stage update() is fully reconstructed), and the
/// spawn column x choices (the int-array literal in the stage update near spawnEnemy, terrain-x).
#[derive(Clone, Debug, Default)]
pub struct FallerCycle {
    pub entrance_delay: Option<f64>,
    pub land_wait: Option<f64>,
    pub spawn_period: Option<f64>,
    pub columns: Vec<f64>,
}

pub(crate) fn extract_faller_cycle(abc: &AbcFile, enemy_class: &str) -> Option<FallerCycle> {
    fn eval_expr(e: &str) -> Option<f64> {
        let e = e.trim();
        if let Some((a, b)) = e.split_once('*') {
            return Some(a.trim().parse::<f64>().ok()? * b.trim().parse::<f64>().ok()?);
        }
        e.parse::<f64>().ok()
    }
    let timer_re = regex::Regex::new(r"new FrameTimer\(([^)]+)\)").unwrap();
    let decompiled = |class: &Class, m: &str| -> String {
        class.instance_methods.iter().find(|t| t.name == m)
            .and_then(|t| abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx))
            .map(|b| decompiler::decompile_method(b, abc, m, &[]))
            .unwrap_or_default()
    };
    let enemy = abc.classes.iter().find(|c| c.name == enemy_class)?;
    let enemy_timers: Vec<f64> = timer_re.captures_iter(&decompiled(enemy, "initialize"))
        .filter_map(|c| eval_expr(&c[1])).collect();
    let mut out = FallerCycle {
        entrance_delay: enemy_timers.first().copied(),
        land_wait: enemy_timers.get(1).copied(),
        ..Default::default()
    };
    if let Some(stage) = abc.classes.iter().find(|c| c.super_name == "SSF2Stage") {
        let mut stage_timers: Vec<f64> = timer_re.captures_iter(&decompiled(stage, "initialize"))
            .filter_map(|c| eval_expr(&c[1])).collect();
        stage_timers.sort_by(|a, b| b.partial_cmp(a).unwrap());
        if stage_timers.len() >= 2 { out.spawn_period = Some(stage_timers[0] + stage_timers[1]); }
        else if let Some(&t) = stage_timers.first() { out.spawn_period = Some(t); }
        // spawn columns: the int-array literal in the stage update near the spawnEnemy call.
        let upd = decompiled(stage, "update");
        if upd.contains("spawnEnemy") {
            let arr_re = regex::Regex::new(r"\[(-?\d+(?:, *-?\d+){2,})\]").unwrap();
            if let Some(c) = arr_re.captures(&upd) {
                out.columns = c[1].split(',').filter_map(|v| v.trim().parse::<f64>().ok()).collect();
            }
        }
    }
    Some(out)
}

/// A sinking-platform class's authored motion, stepped from its OWN code (decompile + read):
/// the per-frame sink/rise speeds (class const slots referenced by `setY(getY() ± self.X)`),
/// the post-sink hold (`new FrameTimer(expr)` in initialize), and the rest/sunk Y caps from the
/// `getY() <op> N` comparisons in update(). All values in SSF2 30fps source units.
#[derive(Clone, Debug, Default)]
pub struct PlatformBehavior {
    pub sink_speed: Option<f64>,
    pub rise_speed: Option<f64>,
    pub wait_frames: Option<f64>,
    pub sink_depth: Option<f64>,
}

/// Extract [`PlatformBehavior`] from the first class extending `SSF2Platform` (the sinking
/// platform kind). None when the stage has no such class.
pub(crate) fn extract_platform_behavior(abc: &AbcFile) -> Option<PlatformBehavior> {
    let class = abc.classes.iter().find(|c| c.super_name == "SSF2Platform")?;
    let body_of = |name: &str| class.instance_methods.iter().find(|t| t.name == name)
        .and_then(|t| abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx));
    let code_of = |name: &str| body_of(name)
        .map(|b| decompiler::decompile_method(b, abc, name, &[]))
        .unwrap_or_default();
    let init = code_of("initialize");
    let update = code_of("update");
    let slot_default = |name: &str| class.instance_methods.iter()
        .find(|t| t.name == name).and_then(|t| t.default);
    // tiny eval for the FrameTimer arg ("30 * 13" / "390")
    fn eval_expr(e: &str) -> Option<f64> {
        let e = e.trim();
        if let Some((a, b)) = e.split_once('*') {
            return Some(a.trim().parse::<f64>().ok()? * b.trim().parse::<f64>().ok()?);
        }
        e.parse::<f64>().ok()
    }
    let mut out = PlatformBehavior::default();
    if let Some(c) = regex::Regex::new(r"new FrameTimer\(([^)]+)\)").unwrap().captures(&init) {
        out.wait_frames = eval_expr(&c[1]);
    }
    // speeds: the slot referenced when moving down (+) is the sink, up (-) the rise.
    if let Some(c) = regex::Regex::new(r"getY\(\) \+ self\.(\w+)").unwrap().captures(&update) {
        out.sink_speed = slot_default(&c[1]);
    }
    if let Some(c) = regex::Regex::new(r"getY\(\) - self\.(\w+)").unwrap().captures(&update) {
        out.rise_speed = slot_default(&c[1]);
    }
    // the rest/sunk caps: the two getY() comparisons; depth = max - min.
    let caps: Vec<f64> = regex::Regex::new(r"getY\(\) *(?:<=|>=|<|>) *(-?\d+(?:\.\d+)?)").unwrap()
        .captures_iter(&update)
        .filter_map(|c| c[1].parse::<f64>().ok())
        .collect();
    if caps.len() >= 2 {
        let lo = caps.iter().cloned().fold(f64::MAX, f64::min);
        let hi = caps.iter().cloned().fold(f64::MIN, f64::max);
        if hi > lo { out.sink_depth = Some(hi - lo); }
    }
    Some(out)
}

/// Debug aid (`PEPTIDE_DUMP_CLASS=<name>`): decompile every instance method of a class through
/// the standard pipeline and print it — the quickest way to read an SSF2 class's authored logic.
pub(crate) fn dump_class(abc: &AbcFile, class_name: &str) {
    let Some(class) = abc.classes.iter()
        .find(|c| c.name == class_name || c.name.ends_with(&format!(".{class_name}"))) else { return };
    eprintln!("[dump-class] {} extends {}", class.name, class.super_name);
    for t in &class.instance_methods {
        if let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) {
            let code = decompiler::decompile_method(body, abc, &t.name, &[]);
            eprintln!("--- {} ---\n{}", t.name, code);
        }
    }
}

/// The `setYSpeed(N)` timeline a sub-clip's frame scripts drive (the thwomp entrance's
/// descend-hover-rise bob): `(1-based frame, speed)` per frameN method that sets one. Source
/// 30fps SSF2 units.
pub(crate) fn extract_frame_velocities(abc: &AbcFile, class_name: &str) -> Vec<(u32, f64)> {
    let Some(class) = abc.classes.iter().find(|c| c.name == class_name) else { return Vec::new() };
    let re = regex::Regex::new(r"setYSpeed\((-?\d+(?:\.\d+)?)\)").unwrap();
    let mut out: Vec<(u32, f64)> = Vec::new();
    for t in &class.instance_methods {
        let Some(n) = t.name.strip_prefix("frame").and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) else { continue };
        let code = decompiler::decompile_method(body, abc, &t.name, &[]);
        if let Some(c) = re.captures(&code) {
            if let Ok(v) = c[1].parse::<f64>() { out.push((n, v)); }
        }
    }
    out.sort_by_key(|(f, _)| *f);
    out
}

pub(crate) fn extract_timeline_hold(abc: &AbcFile, class_name: &str) -> Option<TimelineHold> {
    let class = abc.classes.iter().find(|c| c.name == class_name)?;
    let stop_re = regex::Regex::new(r"\bstop\(\)").unwrap();
    let goto_re = regex::Regex::new(r#"gotoAndStop\((?:"([^"]+)"|(\d+))"#).unwrap();
    // playback freezes at the FIRST frame whose script holds (it never plays past it).
    let mut best: Option<(u32, TimelineHold)> = None;
    for t in &class.instance_methods {
        let Some(n) = t.name.strip_prefix("frame").and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) else { continue };
        let code = decompiler::decompile_method(body, abc, &t.name, &[]);
        let hold = if let Some(c) = goto_re.captures(&code) {
            let target = c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().to_string()).unwrap_or_default();
            Some(TimelineHold::GotoStop(target, n))
        } else if stop_re.is_match(&code) {
            Some(TimelineHold::StopAt(n))
        } else { None };
        if let Some(h) = hold {
            if best.as_ref().map(|(bn, _)| n < *bn).unwrap_or(true) { best = Some((n, h)); }
        }
    }
    best.map(|(_, h)| h)
}

/// What a clip's own timeline does at the end of a labelled segment, from the Flash frame script
/// on that frame. These are what make a multi-label clip a STATE MACHINE rather than a loop: the
/// segments are chained (or branched between) by them, and a segment no jump ever reaches is dead
/// during normal play — playing the clip end to end shows animations the source never shows.
#[derive(Clone, Debug, PartialEq)]
pub enum LabelJump {
    /// `gotoAndPlay("target")`, unconditional.
    Always(String),
    /// `if (safeRandom() > t) gotoAndPlay("target")`: taken `pct` percent of the time, otherwise
    /// playback runs on into whatever frame follows.
    Chance { pct: u32, target: String },
}

/// Every `gotoAndPlay` a Flash timeline class performs, by 1-based frame. Universal: any SSF2 clip
/// whose segments are chained this way, not just backdrop art.
pub(crate) fn extract_label_jumps(abc: &AbcFile, class_name: &str) -> Vec<(u32, LabelJump)> {
    let Some(class) = abc.classes.iter().find(|c| c.name == class_name) else { return Vec::new() };
    let goto_re = regex::Regex::new(r#"gotoAndPlay\("([^"]+)"\)"#).unwrap();
    // the SSF2 RNG is uniform [0,1), so `> t` fires with probability 1-t and `< t` with t.
    let rand_re = regex::Regex::new(r"safeRandom\(\)\s*([<>])\s*([0-9]*\.?[0-9]+)").unwrap();
    let mut out = Vec::new();
    for t in &class.instance_methods {
        let Some(n) = t.name.strip_prefix("frame").and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) else { continue };
        let code = decompiler::decompile_method(body, abc, &t.name, &[]);
        let Some(g) = goto_re.captures(&code) else { continue };
        let target = g[1].to_string();
        // a random test anywhere in the same frame script is taken to guard the jump: these
        // scripts are a couple of statements long, so there is nothing else for it to guard.
        let jump = match rand_re.captures(&code).and_then(|c| {
            let thr = c[2].parse::<f64>().ok()?;
            Some(if &c[1] == ">" { 1.0 - thr } else { thr })
        }) {
            Some(p) => LabelJump::Chance { pct: (p * 100.0).round().clamp(0.0, 100.0) as u32, target },
            None => LabelJump::Always(target),
        };
        out.push((n, jump));
    }
    out.sort_by_key(|(f, _)| *f);
    out
}

pub(crate) fn extract_force_attack_labels(abc: &AbcFile, class_name: &str) -> Vec<String> {
    let Some(class) = abc.classes.iter().find(|c| c.name == class_name) else { return vec![] };
    let mut v = ForceAttackVisitor::default();
    for m in ["initialize", "update", "runAI", "move", "releaseEnemy", "setState"] {
        if let Some(t) = class.instance_methods.iter().find(|t| &*t.name == m) {
            if let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) {
                scan_method(&body.bytecode, abc, &mut v);
            }
        }
    }
    v.labels
}

/// Reconstruct a hazard class's lifecycle methods (initialize + update + helpers) as FM hscript by
/// running them through the SAME `decompile_method` → `translate_ssf2_to_fm` pipeline the character
/// and projectile ports use — the real state machine, not a hand-written template. The output still
/// needs a field-state pass (`self.m_x` → `self.makeInt(...)`) + a FrameTimer helper before it RUNS;
/// returned for the gated reconstruction path to iterate on. Empty if the class has no such methods.
pub(crate) fn reconstruct_enemy_script(abc: &AbcFile, class_name: &str) -> Option<String> {
    let class = abc.classes.iter().find(|c| c.name == class_name)?;
    let mut out = String::new();
    for m in ["initialize", "update", "runAI", "move", "releaseEnemy"] {
        if let Some(t) = class.instance_methods.iter().find(|t| &*t.name == m) {
            if let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) {
                let raw = crate::decompiler::decompile_method(body, abc, m, &[]);
                out.push_str(&crate::api_mappings::translate_ssf2_to_fm(&raw));
                out.push('\n');
            }
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

/// The getAttackStats hitbox maps a HAZARD class declares (damage/direction/power/kbConstant + any
/// geometry), authoritative per-hazard instead of a generic per-kind default. Empty if the class
/// has no getAttackStats or no recoverable hitbox. Pairs with [`extract_own_stats_for`].
pub(crate) fn extract_attack_stats_for(abc: &AbcFile, class_name: &str) -> Vec<BTreeMap<String, f64>> {
    let Some(class) = abc.classes.iter().find(|c| c.name == class_name) else { return vec![] };
    let Some(t) = class.instance_methods.iter().find(|t| &*t.name == "getAttackStats") else { return vec![] };
    let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) else { return vec![] };
    let mut v = HazardAttackVisitor { best: vec![] };
    scan_method(&body.bytecode, abc, &mut v);
    v.best
}

/// The flat scalar fields a hazard class's `getOwnStats` declares (size, speeds, timers — e.g. the
/// HyruleTornado's `xSpeed`/`maxTime`). Authoritative motion/own params instead of guessed defaults.
pub(crate) fn extract_own_stats_for(abc: &AbcFile, class_name: &str) -> BTreeMap<String, f64> {
    let Some(class) = abc.classes.iter().find(|c| c.name == class_name) else { return BTreeMap::new() };
    let Some(t) = class.instance_methods.iter().find(|t| &*t.name == "getOwnStats") else { return BTreeMap::new() };
    let Some(body) = abc.method_bodies.iter().find(|b| b.method_idx == t.method_idx) else { return BTreeMap::new() };
    let mut v = OwnStatsVisitor { best: BTreeMap::new() };
    scan_method(&body.bytecode, abc, &mut v);
    v.best
}
struct OwnStatsVisitor {
    best: BTreeMap<String, f64>,
}
impl AbcVisitor for OwnStatsVisitor {
    fn on_newobject(&mut self, obj: BTreeMap<String, StackVal>, _c: &Option<String>) -> NewObjectAction {
        let nums: BTreeMap<String, f64> = obj.iter()
            .filter_map(|(k, v)| if let StackVal::Num(n) = v { Some((k.clone(), *n)) } else { None })
            .collect();
        if nums.len() > self.best.len() {
            self.best = nums;
        }
        NewObjectAction::PushObj(obj)
    }
}

/// Extract per-projectile stat objects from a `getProjectileStats()`
/// bytecode body. Mirrors `extract_attack_objects` but recognises
/// projectile-shaped objects: an object is a projectile if it carries
/// any flat-scalar physics field (gravity / xSpeed / ySpeed / friction /
/// etc.) OR a nested `attackBoxes` hitbox map.
///
/// Returns a map keyed by the SSF2 projectile name (the key under which
/// the per-projectile object appears in the top-level returned object).
/// SSF2 physics-stat keys commonly found on a projectile-stats object.
/// Any inner value-object holding at least one of these (or an
/// `attackBoxes` key) is recognised as a projectile entry.
const PROJECTILE_PHYSICS_KEYS: &[&str] = &[
    "gravity", "friction", "weight", "fall_speed", "terminalVelocity",
    "xSpeed", "ySpeed", "x_speed", "y_speed",
    "groundSpeedCap", "aerialSpeedCap", "aerialFriction",
    "ground_speed_cap", "aerial_speed_cap", "aerial_friction",
];

/// Visitor for `getProjectileStats()`-style bodies. The top-level
/// newobject's KEYS are projectile names; the VALUES are physics +
/// attackBoxes objects we promote to `ProjectileData`.
struct ProjectileVisitor {
    result: BTreeMap<String, ProjectileData>,
}

impl AbcVisitor for ProjectileVisitor {
    fn on_newobject(
        &mut self,
        obj: BTreeMap<String, StackVal>,
        _current_char: &Option<String>,
    ) -> NewObjectAction {
        let mut found = false;
        for (proj_name, val) in &obj {
            if let StackVal::Obj(inner) = val {
                let is_proj = inner.contains_key("attackBoxes")
                    || PROJECTILE_PHYSICS_KEYS.iter().any(|k| inner.contains_key(*k));
                if is_proj {
                    let mut stats: BTreeMap<String, f64> = BTreeMap::new();
                    for (k, v) in inner {
                        if let StackVal::Num(n) = v {
                            stats.insert(k.clone(), *n);
                        }
                    }
                    let hitboxes = extract_hitboxes_from_val(val);
                    self.result.insert(proj_name.clone(), ProjectileData { stats, hitboxes });
                    found = true;
                }
            }
        }
        // Preserve the original behaviour: when the visitor recorded
        // projectile entries from this obj, leave the obj OFF the stack
        // (Push(Unknown) is the same observable shape as the legacy
        // "don't push obj back" since the only thing that consumed the
        // top-of-stack after this newobject was OP_RETURNVALUE, which
        // pops one item either way).
        if found {
            NewObjectAction::Push(StackVal::Unknown)
        } else {
            NewObjectAction::PushObj(obj)
        }
    }
}

fn extract_projectile_objects(bytecode: &[u8], abc: &AbcFile) -> BTreeMap<String, ProjectileData> {
    let mut v = ProjectileVisitor { result: BTreeMap::new() };
    scan_method(bytecode, abc, &mut v);
    v.result
}

/// Recursively extract hitboxes from a StackVal.
/// SSF2 attack objects have key 'attackBoxes' → object with 'attackBox', 'attackBox2', etc.
fn extract_hitboxes_from_val(val: &StackVal) -> Vec<BTreeMap<String, f64>> {
    match val {
        StackVal::Obj(obj) => {
            // Look for 'attackBoxes' key
            if let Some(boxes_val) = obj.get("attackBoxes") {
                return extract_hitboxes_from_val(boxes_val);
            }
            // Is this object itself a hitbox? (has damage/direction/power)
            let hitbox_keys = ["damage", "direction", "power", "kbConstant", "hitStun", "hitLag", "weightKB"];
            if obj.keys().any(|k| hitbox_keys.contains(&k.as_str())) {
                let mut hb = BTreeMap::new();
                for k in &hitbox_keys {
                    if let Some(StackVal::Num(v)) = obj.get(*k) {
                        hb.insert(k.to_string(), *v);
                    }
                }
                return vec![hb];
            }
            // Might be a container of hitboxes: {attackBox: {...}, attackBox2: {...}, ...}
            let mut hitboxes = Vec::new();
            for (k, v) in obj {
                if k.starts_with("attackBox") {
                    hitboxes.extend(extract_hitboxes_from_val(v));
                }
            }
            hitboxes
        }
        _ => vec![]
    }
}

/// Visitor for `getOwnStats()`-style bodies (the heuristic fallback,
/// distinct from the targeted `extract_ssf2_stats` linear scan). Looks
/// for the first newobject whose keys overlap the canonical SSF2
/// character-stat names by at least 3; stops scanning the moment it
/// finds one.
struct StatsVisitor {
    found: Option<CharStats>,
}

const STATS_FALLBACK_KEYS: &[&str] = &[
    "gravity", "weight1", "norm_xSpeed", "max_xSpeed",
    "fastFallSpeed", "jumpSpeed", "jumpSpeedMidair",
    "accel_rate_air", "decel_rate_air", "max_ySpeed",
    "accel_rate", "walkSpeed", "dashSpeed", "airMobility",
];

impl AbcVisitor for StatsVisitor {
    fn on_newobject(
        &mut self,
        obj: BTreeMap<String, StackVal>,
        _current_char: &Option<String>,
    ) -> NewObjectAction {
        let numeric_stats: BTreeMap<String, f64> = obj.iter()
            .filter_map(|(k, v)| {
                if let StackVal::Num(n) = v { Some((k.clone(), *n)) } else { None }
            }).collect();
        let match_count = numeric_stats.keys()
            .filter(|k| STATS_FALLBACK_KEYS.contains(&k.as_str()))
            .count();
        if match_count >= 3 {
            self.found = Some(CharStats { values: numeric_stats });
            return NewObjectAction::Stop;
        }
        NewObjectAction::PushObj(obj)
    }
}

fn extract_stats_from_body(bytecode: &[u8], abc: &AbcFile) -> Option<CharStats> {
    let mut v = StatsVisitor { found: None };
    scan_method(bytecode, abc, &mut v);
    v.found
}

/// Extract the largest object in the body that has the most numeric key-value pairs.
/// Used as fallback for getOwnStats when the stat key heuristic doesn't match.
/// Targeted extractor: scan bytecode for SSF2 stat key-value pairs.
/// Looks for consecutive (pushstring key, push_num value) pairs where key is a known stat name.
fn extract_ssf2_stats(bytecode: &[u8], abc: &AbcFile) -> Option<CharStats> {
    const STAT_KEYS: &[&str] = &[
        "gravity", "weight1", "norm_xSpeed", "max_xSpeed", "max_ySpeed",
        "fastFallSpeed", "jumpSpeed", "jumpSpeedMidair", "shortHopSpeed",
        "accel_rate", "accel_rate_air", "decel_rate", "decel_rate_air",
        "accel_start", "accel_start_dash", "max_jump", "dodgeSpeed", "dodgeDecel",
        "roll_speed", "roll_decay", "max_projectile", "width", "height",
        "jumpStartup", "max_jumpSpeed", "groundToAirMultiplier",
    ];

    let mut values: BTreeMap<String, f64> = BTreeMap::new();
    let mut i = 0;

    while i < bytecode.len() {
        let op = bytecode[i]; i += 1;
        // Look for pushstring followed immediately by a numeric push
        if op == OP_PUSHSTRING {
            if let Some(str_idx) = read_u30_at(bytecode, &mut i) {
                let key = abc.strings.get(str_idx as usize).map(|s| s.to_string()).unwrap_or_default();
                if STAT_KEYS.contains(&key.as_str()) && i < bytecode.len() {
                    // Next op should be a numeric push
                    let next_op = bytecode[i]; i += 1;
                    let val = match next_op {
                        OP_PUSHBYTE => {
                            if i < bytecode.len() { let v = bytecode[i] as i8 as f64; i += 1; Some(v) } else { None }
                        }
                        OP_PUSHSHORT => {
                            read_u30_at(bytecode, &mut i).map(|v| v as i16 as f64)
                        }
                        OP_PUSHINT => {
                            read_u30_at(bytecode, &mut i)
                                .and_then(|idx| abc.ints.get(idx as usize).copied())
                                .map(|v| v as f64)
                        }
                        OP_PUSHUINT => {
                            read_u30_at(bytecode, &mut i)
                                .and_then(|idx| abc.uints.get(idx as usize).copied())
                                .map(|v| v as f64)
                        }
                        OP_PUSHDOUBLE => {
                            read_u30_at(bytecode, &mut i)
                                .and_then(|idx| abc.doubles.get(idx as usize).copied())
                        }
                        _ => {
                            // Back up one byte since it wasn't a numeric push
                            i -= 1;
                            None
                        }
                    };
                    if let Some(v) = val {
                        // FIRST-WINS: the genuine top-level character stats object
                        // appears first in the bundle method; later per-projectile
                        // data objects re-use keys like "gravity"/"width"/"height"
                        // (usually pushbyte 0), which last-wins would let clobber
                        // the real value. Keep the first occurrence.
                        values.entry(key).or_insert(v);
                    }
                }
                // Continue; don't double-consume
                continue;
            }
        }
        // Skip operand bytes for other instructions to keep position correct
        match op {
            OP_PUSHDOUBLE | OP_PUSHSTRING | OP_PUSHINT | OP_PUSHUINT |
            OP_COERCE | OP_GETLEX | OP_FINDPROPSTRICT | OP_FINDPROP |
            OP_GETPROPERTY | OP_INITPROPERTY | OP_SETPROPERTY | OP_GETLOCAL | OP_SETLOCAL => {
                read_u30_at(bytecode, &mut i);
            }
            OP_PUSHBYTE => { if i < bytecode.len() { i += 1; } }
            OP_PUSHSHORT => { read_u30_at(bytecode, &mut i); }
            OP_CALLPROPERTY | OP_CALLPROPVOID | OP_CONSTRUCTPROP => {
                read_u30_at(bytecode, &mut i); read_u30_at(bytecode, &mut i);
            }
            OP_CONSTRUCT | OP_NEWARRAY | OP_NEWOBJECT => { read_u30_at(bytecode, &mut i); }
            OP_JUMP | OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFLT |
            OP_IFLE | OP_IFGT | OP_IFGE | OP_IFSTRICTEQ | OP_IFSTRICTNE
                if i + 3 <= bytecode.len() => { i += 3; }
            _ => {}
        }
    }

    if values.len() >= 3 {
        Some(CharStats { values })
    } else {
        None
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn read_u30_at(data: &[u8], i: &mut usize) -> Option<u32> {
    let mut result = 0u32;
    let mut shift = 0;
    loop {
        if *i >= data.len() { return None; }
        let b = data[*i] as u32;
        *i += 1;
        result |= (b & 0x7F) << shift;
        shift += 7;
        if b & 0x80 == 0 || shift >= 35 { break; }
    }
    Some(result)
}

/// Is this string an SSF2 attack/move name?
fn is_attack_name(s: &str) -> bool {
    matches!(s, 
        "a" | "a_tilt" | "a_forward" | "a_forward_tilt" | "a_up_tilt" | "a_down_tilt" |
        "crouch_attack" | "a_forwardsmash" | "a_up" | "a_down" |
        "a_air" | "a_air_forward" | "a_air_backward" | "a_air_up" | "a_air_down" |
        "b" | "b_air" | "b_forward" | "b_forward_air" | "b_up" | "b_up_air" |
        "b_down" | "b_down_air" |
        "throw_up" | "throw_forward" | "throw_back" | "throw_down" |
        "ledge_attack" | "getup_attack" | "special" |
        "jab" | "jab1" | "jab2" | "jab3" | "dash" |
        "ftilt" | "utilt" | "dtilt" | "fsmash" | "usmash" | "dsmash" |
        "nair" | "fair" | "bair" | "uair" | "dair" |
        "nspecial" | "sspecial" | "uspecial" | "dspecial"
    )
}

fn normalize_attack_name(s: &str) -> String {
    let map: &[(&str, &str)] = &[
        ("a",              "jab1"),
        ("a_tilt",         "jab1"),
        ("a_forward",      "dash_attack"),
        ("a_forward_tilt", "tilt_forward"),
        ("a_up_tilt",      "tilt_up"),
        ("a_down_tilt",    "tilt_down"),
        ("crouch_attack",  "tilt_down"),
        ("a_forwardsmash", "strong_forward_attack"),
        ("a_up",           "strong_up_attack"),
        ("a_down",         "strong_down_attack"),
        ("a_air",          "aerial_neutral"),
        ("a_air_forward",  "aerial_forward"),
        ("a_air_backward", "aerial_back"),
        ("a_air_up",       "aerial_up"),
        ("a_air_down",     "aerial_down"),
        ("b",              "special_neutral"),
        ("b_air",          "special_neutral_air"),
        ("b_forward",      "special_side"),
        ("b_forward_air",  "special_side_air"),
        ("b_up",           "special_up"),
        ("b_up_air",       "special_up_air"),
        ("b_down",         "special_down"),
        ("b_down_air",     "special_down_air"),
        ("throw_up",       "throw_up"),
        ("throw_forward",  "throw_forward"),
        ("throw_back",     "throw_back"),
        ("throw_down",     "throw_down"),
        ("ledge_attack",   "ledge_attack"),
        ("getup_attack",   "crash_attack"),
    ];
    for (from, to) in map {
        if s == *from { return to.to_string(); }
    }
    s.to_string()
}


/// Scan ALL method bodies for SSF2 costume data patterns.
///
/// SSF2 misc.ssf stores costume data per-character as methods that return arrays of
/// {name: String, colors: [uint, uint, ...]} objects. This function finds them all.
///
/// Returns a map of class_name → Vec<CostumeData>.
/// Scan all method bodies for costume data. Returns per-character map.
/// Handles two layouts:
///   A) misc.ssf getCostumeData: one big method, character key from getproperty on a string constant
///   B) character-file layout: small per-costume method, char name inferred from class name
pub fn scan_all_costume_methods(abc: &AbcFile) -> BTreeMap<String, Vec<CostumeData>> {
    let mut results: BTreeMap<String, Vec<CostumeData>> = BTreeMap::new();
    for body in &abc.method_bodies {
        let per_char = decode_costume_objects(&body.bytecode, abc);
        for (char_name, costumes) in per_char {
            if !costumes.is_empty() {
                results.entry(char_name).or_default().extend(costumes);
            }
        }
    }
    results
}

/// Visitor for `misc.ssf :: getCostumeData()` and per-character costume
/// methods. Two palette-table shapes are recognised:
///
///   - Pattern A (misc.ssf): `{ team | base, paletteSwap: { colors,
///     replacements } }`. Costume name comes from `base` (Default) or
///     `team` (capitalised) or a synthesised `Alt N` counter.
///
///   - Pattern B (character file): `{ name: "...", colors: [uint…] }`.
///     Costume name is the literal `name`; no `replacements`.
///
/// The visitor relies on `scan_method`'s costume-mode getproperty
/// semantics to keep `current_char` in sync across multi-character
/// misc.ssf bodies (where the runtime key on `_loc1_[char]` IS the
/// character id, and the static name on `obj.mario` IS the character id).
struct CostumeVisitor {
    per_char: BTreeMap<String, Vec<CostumeData>>,
    alt_counters: BTreeMap<String, usize>,
}

impl AbcVisitor for CostumeVisitor {
    fn costume_getproperty_semantics(&self) -> bool { true }

    fn on_newarray(&mut self, items: Vec<StackVal>) -> StackVal {
        // Costume needs to read array contents back as colour literals,
        // so build a real Arr instead of the default Unknown placeholder.
        StackVal::Arr(items)
    }

    fn on_newobject(
        &mut self,
        obj: BTreeMap<String, StackVal>,
        current_char: &Option<String>,
    ) -> NewObjectAction {
        // Pattern A — misc.ssf style.
        if let Some(StackVal::Obj(ps)) = obj.get("paletteSwap") {
            let colors = stackval_arr_to_u32(ps.get("colors"));
            let replacements = stackval_arr_to_u32(ps.get("replacements"));
            if colors.len() >= 4 && colors.len() == replacements.len() {
                let char_key = current_char.clone().unwrap_or_else(|| "unknown".to_string());
                let alt_n = self.alt_counters.entry(char_key.clone()).or_insert(0);
                let costume_name = if obj.contains_key("base") {
                    "Default".to_string()
                } else if let Some(StackVal::Str(team)) = obj.get("team") {
                    // Capitalise the team name ("red" → "Red").
                    let mut c = team.chars();
                    match c.next() {
                        None    => team.clone(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                } else {
                    *alt_n += 1;
                    format!("Alt {}", alt_n)
                };
                self.per_char.entry(char_key).or_default()
                    .push(CostumeData { name: costume_name, colors, replacements });
            }
        }
        // Pattern B — character-file style.
        else if let (Some(StackVal::Str(name)), Some(StackVal::Arr(color_arr))) =
            (obj.get("name"), obj.get("colors"))
        {
            let colors: Vec<u32> = color_arr.iter().filter_map(|v| {
                if let StackVal::Num(n) = v { Some(*n as u32) } else { None }
            }).collect();
            if colors.len() >= 4 {
                let char_key = current_char.clone().unwrap_or_else(|| "unknown".to_string());
                self.per_char.entry(char_key).or_default()
                    .push(CostumeData { name: name.clone(), colors, replacements: vec![] });
            }
        }
        NewObjectAction::PushObj(obj)
    }
}

/// Pull every numeric item out of a possibly-Arr StackVal as u32.
fn stackval_arr_to_u32(v: Option<&StackVal>) -> Vec<u32> {
    match v {
        Some(StackVal::Arr(arr)) => arr.iter().filter_map(|x| {
            if let StackVal::Num(n) = x { Some(*n as u32) } else { None }
        }).collect(),
        _ => vec![],
    }
}

fn decode_costume_objects(code: &[u8], abc: &AbcFile) -> BTreeMap<String, Vec<CostumeData>> {
    let mut v = CostumeVisitor {
        per_char: BTreeMap::new(),
        alt_counters: BTreeMap::new(),
    };
    scan_method(code, abc, &mut v);
    v.per_char
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Bytecode-construction helpers ────────────────────────────────────
    //
    // Each `push_*` helper writes one AVM2 opcode + operand to a Vec<u8>.
    // The companion `mk_abc()` builds the constant pool slots the
    // opcodes index into. With these we can hand-craft tiny method bodies
    // that exercise the shared scanner against each visitor without
    // having to round-trip through a real SWF.

    fn write_u30(bc: &mut Vec<u8>, mut v: u32) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 { bc.push(b); break; }
            bc.push(b | 0x80);
        }
    }

    fn push_string(bc: &mut Vec<u8>, idx: u32) { bc.push(0x2C); write_u30(bc, idx); }
    fn push_double(bc: &mut Vec<u8>, idx: u32) { bc.push(0x2F); write_u30(bc, idx); }
    fn push_byte  (bc: &mut Vec<u8>, v: i8)    { bc.push(0x24); bc.push(v as u8); }
    fn push_uint  (bc: &mut Vec<u8>, idx: u32) { bc.push(0x2E); write_u30(bc, idx); }
    fn newobject  (bc: &mut Vec<u8>, n: u32)   { bc.push(0x55); write_u30(bc, n); }
    fn newarray   (bc: &mut Vec<u8>, n: u32)   { bc.push(0x56); write_u30(bc, n); }

    /// Build an `AbcFile` whose constant pools are seeded with the
    /// given strings / doubles / uints (indices start at 1, as AVM2's
    /// constant pool has a sentinel at index 0).
    fn mk_abc(strings: &[&str], doubles: &[f64], uints: &[u32]) -> AbcFile {
        let mut all_strings: Vec<Arc<str>> = vec![Arc::from("")];
        all_strings.extend(strings.iter().map(|s| Arc::from(*s)));
        let mut all_doubles = vec![f64::NAN];
        all_doubles.extend(doubles.iter().copied());
        let mut all_uints = vec![0u32];
        all_uints.extend(uints.iter().copied());
        AbcFile {
            strings: all_strings,
            ints: vec![0],
            uints: all_uints,
            doubles: all_doubles,
            multinames: vec![Multiname { kind: 0, name_idx: 0, ns_idx: 0, name: Arc::from("") }],
            methods: vec![],
            classes: vec![],
            scripts: vec![],
            method_bodies: vec![],
        }
    }

    // ── AttackVisitor ────────────────────────────────────────────────────

    #[test]
    fn attack_visitor_recognises_a_air_object() {
        // Build bytecode that constructs:
        //   { a_air: { damage: 5, direction: 45, power: 10 } }
        let abc = mk_abc(&["a_air", "damage", "direction", "power"], &[], &[]);
        let mut bc = Vec::new();
        push_string(&mut bc, 1); // "a_air" (outer key)
        push_string(&mut bc, 2); // "damage"
        push_byte(&mut bc, 5);
        push_string(&mut bc, 3); // "direction"
        push_byte(&mut bc, 45);
        push_string(&mut bc, 4); // "power"
        push_byte(&mut bc, 10);
        newobject(&mut bc, 3);   // inner hitbox = { damage, direction, power }
        newobject(&mut bc, 1);   // outer = { a_air: inner }
        bc.push(0x48);            // returnvalue
        let result = extract_attack_objects(&bc, &abc);
        assert!(result.contains_key("aerial_neutral"),
            "a_air must normalise to aerial_neutral; got: {:?}", result.keys().collect::<Vec<_>>());
        let attack = &result["aerial_neutral"];
        assert_eq!(attack.hitboxes.len(), 1, "one hitbox expected; got: {:?}", attack.hitboxes);
        assert_eq!(attack.hitboxes[0].get("damage").copied(), Some(5.0));
        assert_eq!(attack.hitboxes[0].get("direction").copied(), Some(45.0));
        assert_eq!(attack.hitboxes[0].get("power").copied(), Some(10.0));
    }

    #[test]
    fn attack_visitor_ignores_non_attack_objects() {
        let abc = mk_abc(&["foo", "bar"], &[], &[]);
        let mut bc = Vec::new();
        push_string(&mut bc, 1); push_byte(&mut bc, 1);
        push_string(&mut bc, 2); push_byte(&mut bc, 2);
        newobject(&mut bc, 2);
        let result = extract_attack_objects(&bc, &abc);
        assert!(result.is_empty(), "unmatched keys should produce no attacks; got: {:?}", result);
    }

    // ── ProjectileVisitor ────────────────────────────────────────────────

    #[test]
    fn projectile_visitor_detects_inner_obj_with_physics_key() {
        // { mario_fireball: { gravity: 0.5, xSpeed: 10 } }
        let abc = mk_abc(&["mario_fireball", "gravity", "xSpeed"], &[0.5], &[]);
        let mut bc = Vec::new();
        push_string(&mut bc, 1);           // outer key
        push_string(&mut bc, 2);           // "gravity"
        push_double(&mut bc, 1);            // 0.5
        push_string(&mut bc, 3);           // "xSpeed"
        push_byte  (&mut bc, 10);
        newobject  (&mut bc, 2);            // inner physics obj
        newobject  (&mut bc, 1);            // outer
        let result = extract_projectile_objects(&bc, &abc);
        assert!(result.contains_key("mario_fireball"),
            "projectile key must be picked up; got: {:?}", result.keys().collect::<Vec<_>>());
        let proj = &result["mario_fireball"];
        assert_eq!(proj.stats.get("gravity").copied(), Some(0.5));
        assert_eq!(proj.stats.get("xSpeed").copied(), Some(10.0));
    }

    #[test]
    fn projectile_visitor_skips_non_physics_inner() {
        // Inner obj has no projectile-shape keys → not recognised.
        let abc = mk_abc(&["foo", "label"], &[], &[]);
        let mut bc = Vec::new();
        push_string(&mut bc, 1);
        push_string(&mut bc, 2);
        push_byte  (&mut bc, 1);
        newobject  (&mut bc, 1);
        newobject  (&mut bc, 1);
        let result = extract_projectile_objects(&bc, &abc);
        assert!(result.is_empty(),
            "no-physics inner objects must not be recognised; got: {:?}", result);
    }

    // ── StatsVisitor ─────────────────────────────────────────────────────

    #[test]
    fn stats_visitor_accepts_threshold_of_3_keys() {
        // { gravity: 1, weight1: 100, jumpSpeed: 9 }  → 3 stat keys → accept
        let abc = mk_abc(&["gravity", "weight1", "jumpSpeed"], &[], &[]);
        let mut bc = Vec::new();
        push_string(&mut bc, 1); push_byte(&mut bc, 1);
        push_string(&mut bc, 2); push_byte(&mut bc, 100);
        push_string(&mut bc, 3); push_byte(&mut bc, 9);
        newobject(&mut bc, 3);
        let result = extract_stats_from_body(&bc, &abc);
        let stats = result.expect("3+ stat keys must yield CharStats");
        assert_eq!(stats.values.get("gravity").copied(), Some(1.0));
        assert_eq!(stats.values.get("weight1").copied(), Some(100.0));
        assert_eq!(stats.values.get("jumpSpeed").copied(), Some(9.0));
    }

    #[test]
    fn stats_visitor_rejects_two_keys() {
        // Only 2 stat keys → below threshold → None.
        let abc = mk_abc(&["gravity", "weight1"], &[], &[]);
        let mut bc = Vec::new();
        push_string(&mut bc, 1); push_byte(&mut bc, 1);
        push_string(&mut bc, 2); push_byte(&mut bc, 100);
        newobject(&mut bc, 2);
        assert!(extract_stats_from_body(&bc, &abc).is_none());
    }

    #[test]
    fn stats_visitor_stops_after_first_match() {
        // First newobject matches and triggers Stop; a second newobject
        // (with different values) must NOT overwrite the first.
        let abc = mk_abc(&["gravity", "weight1", "jumpSpeed"], &[], &[]);
        let mut bc = Vec::new();
        // First (matching) object
        push_string(&mut bc, 1); push_byte(&mut bc, 1);
        push_string(&mut bc, 2); push_byte(&mut bc, 100);
        push_string(&mut bc, 3); push_byte(&mut bc, 9);
        newobject(&mut bc, 3);
        // Second (also matching, but different values) — should never run.
        push_string(&mut bc, 1); push_byte(&mut bc, 99);
        push_string(&mut bc, 2); push_byte(&mut bc, 99);
        push_string(&mut bc, 3); push_byte(&mut bc, 99);
        newobject(&mut bc, 3);
        let stats = extract_stats_from_body(&bc, &abc).expect("first match should be returned");
        // Values must be from the FIRST newobject (1 / 100 / 9), not the second (99s).
        assert_eq!(stats.values.get("gravity").copied(), Some(1.0));
        assert_eq!(stats.values.get("weight1").copied(), Some(100.0));
    }

    // ── CostumeVisitor ───────────────────────────────────────────────────

    #[test]
    fn costume_visitor_pattern_b_character_file() {
        // { name: "Default", colors: [0xff112233, 0xff445566, 0xff778899, 0xffaabbcc] }
        let abc = mk_abc(&["Default", "name", "colors"], &[], &[0xff112233, 0xff445566, 0xff778899, 0xffaabbcc]);
        let mut bc = Vec::new();
        // Build the colors array on the stack first.
        push_uint(&mut bc, 1);  push_uint(&mut bc, 2);
        push_uint(&mut bc, 3);  push_uint(&mut bc, 4);
        newarray(&mut bc, 4);
        // Now the outer obj.
        push_string(&mut bc, 2);                     // "name"
        push_string(&mut bc, 1);                     // "Default"
        push_string(&mut bc, 3);                     // "colors"
        // ← The newarray result is already on the stack just below "name"/"Default"/"colors".
        //   But the obj wants pairs in (key, value) order from the bottom.
        //   Simpler: rebuild fresh — pop the existing array.
        // Reset: drop the array we just built and start over with the
        // canonical key/value order.
        let mut bc = Vec::new();
        push_string(&mut bc, 2);                     // key "name"
        push_string(&mut bc, 1);                     // value "Default"
        push_string(&mut bc, 3);                     // key "colors"
        push_uint(&mut bc, 1); push_uint(&mut bc, 2); push_uint(&mut bc, 3); push_uint(&mut bc, 4);
        newarray(&mut bc, 4);                         // value = arr
        newobject(&mut bc, 2);                        // { name, colors }
        let result = decode_costume_objects(&bc, &abc);
        let costumes = result.get("unknown").expect("no current_char → 'unknown' bucket");
        assert_eq!(costumes.len(), 1, "exactly one costume expected");
        assert_eq!(costumes[0].name, "Default");
        assert_eq!(costumes[0].colors, vec![0xff112233, 0xff445566, 0xff778899, 0xffaabbcc]);
    }

    #[test]
    fn costume_visitor_rejects_under_four_colors() {
        // colors.len() < 4 → silent skip (matches the legacy behaviour).
        let abc = mk_abc(&["Default", "name", "colors"], &[], &[0x11, 0x22]);
        let mut bc = Vec::new();
        push_string(&mut bc, 2);
        push_string(&mut bc, 1);
        push_string(&mut bc, 3);
        push_uint(&mut bc, 1); push_uint(&mut bc, 2);
        newarray(&mut bc, 2);
        newobject(&mut bc, 2);
        let result = decode_costume_objects(&bc, &abc);
        assert!(result.is_empty() || result.values().all(|v| v.is_empty()),
            "under-4-colour objects must not produce a CostumeData; got: {:?}", result);
    }

    // ── Shared scanner edge cases ────────────────────────────────────────

    #[test]
    fn scanner_handles_empty_bytecode() {
        let abc = mk_abc(&[], &[], &[]);
        let result = extract_attack_objects(&[], &abc);
        assert!(result.is_empty());
    }

    #[test]
    fn scanner_survives_unknown_opcode() {
        // Opcode 0xFF (undefined) → default branch in scan_method's match
        // should be a no-op. Bytecode that reaches a newobject after an
        // unknown opcode should still extract correctly.
        let abc = mk_abc(&["a_air", "damage"], &[], &[]);
        let mut bc = Vec::new();
        bc.push(0xFF);            // garbage / unknown opcode
        push_string(&mut bc, 1);
        push_string(&mut bc, 2); push_byte(&mut bc, 5);
        newobject(&mut bc, 1);     // inner hitbox { damage: 5 }
        newobject(&mut bc, 1);     // outer { a_air: inner }
        let result = extract_attack_objects(&bc, &abc);
        assert!(result.contains_key("aerial_neutral"),
            "unknown opcode must not derail recognition; got: {:?}", result.keys().collect::<Vec<_>>());
    }

    #[test]
    fn scanner_returnvalue_pops_one() {
        // After returnvalue, the scanner should pop the top of stack but
        // KEEP scanning subsequent ops (which may produce more
        // newobjects). This pins the behaviour against the OLD
        // decode_costume_objects' break-on-return, which we deliberately
        // unified to pop-and-continue.
        let abc = mk_abc(&["a_air", "damage"], &[], &[]);
        let mut bc = Vec::new();
        push_byte(&mut bc, 0);
        bc.push(0x48); // returnvalue — pops the dummy 0
        // Even after returnvalue, the scanner continues. Build a
        // recognised attack obj next:
        push_string(&mut bc, 1);
        push_string(&mut bc, 2); push_byte(&mut bc, 5);
        newobject(&mut bc, 1);
        newobject(&mut bc, 1);
        let result = extract_attack_objects(&bc, &abc);
        assert!(result.contains_key("aerial_neutral"),
            "post-returnvalue opcodes must still be processed; got: {:?}", result.keys().collect::<Vec<_>>());
    }
}
