//! Reading ABC with Ruffle's parser, into the shape this crate already speaks.
//!
//! `abc_parser` hand-rolls the byte-level walk of an ABC file: pools, namespaces, multinames,
//! classes, traits, method bodies. The `swf` crate we already depend on ships that walk
//! (`swf::avm2::read`), written and exercised by a Flash emulator.
//!
//! What this is NOT is a bug fix. Both readers were compared across every stage in the sweep and
//! six characters -- sixteen files, sixteen ABC blocks -- and they agree exactly, so the hand
//! walk was reading this content correctly. (An earlier version of this note blamed it for the
//! slots-among-methods bug. That was wrong: the trait kind was recorded correctly and a caller
//! ignored it, and Ruffle mixes slots into the same trait list too, so nothing about that reader
//! would have prevented it.)
//!
//! The reason to prefer it is smaller surface, not present-day correctness: one implementation of
//! a format instead of two, plus typed traits, real namespaces, method signatures and exception
//! tables that the hand walk skipped and future work would otherwise have to add.
//!
//! This module does the mapping and nothing else. Everything downstream -- every extractor, the
//! decompiler -- keeps its existing types, so the swap is a change of who does the reading rather
//! than a change of what is read.
//!
//! ## The indexing, which is the whole risk
//!
//! ABC pools are 1-based: index 0 is a reserved "none" that the file does not store. The two
//! readers deal with that differently. `abc_parser` puts a dummy back at index 0 of EVERY pool, so
//! `pool[i]` is ABC index `i` and the callers index directly. Ruffle stores only what the file
//! contains, so ABC index `i` is `pool[i - 1]`.
//!
//! This module reproduces `abc_parser`'s convention, dummy for dummy -- including the dummies that
//! look pointless, since being "more correct" here would shift every lookup in the crate by one
//! and rename every symbol in the file without failing anything. The parity test is what
//! established which pools were padded; the first attempt at this file guessed, guessed wrong
//! about the numeric pools, and the test said so immediately.

use crate::abc_parser::{AbcFile, Class, Method, MethodBody, Multiname, Script, Trait};
use anyhow::{anyhow, Result};
use std::sync::Arc;
use swf::avm2::types as rt;

/// ABC trait kinds, as the format numbers them. Ruffle gives us an enum; the rest of this crate
/// reads the raw nibble, so it is put back.
mod trait_kind {
    pub const SLOT: u8 = 0;
    pub const METHOD: u8 = 1;
    pub const GETTER: u8 = 2;
    pub const SETTER: u8 = 3;
    pub const CLASS: u8 = 4;
    pub const FUNCTION: u8 = 5;
    pub const CONST: u8 = 6;
}

/// The string a pool index names, in `abc_parser`'s 1-based numbering.
fn string_at(pool: &[Vec<u8>], idx: u32) -> Arc<str> {
    if idx == 0 { return Arc::from(""); }
    pool.get(idx as usize - 1)
        .map(|b| Arc::from(String::from_utf8_lossy(b).into_owned().as_str()))
        .unwrap_or_else(|| Arc::from(""))
}

/// Which string a namespace is over.
fn namespace_name(pool: &rt::ConstantPool, ns: &rt::Namespace) -> Arc<str> {
    let idx = match ns {
        rt::Namespace::Namespace(i) | rt::Namespace::Package(i) | rt::Namespace::PackageInternal(i)
        | rt::Namespace::Protected(i) | rt::Namespace::Explicit(i)
        | rt::Namespace::StaticProtected(i) | rt::Namespace::Private(i) => i.0,
    };
    string_at(&pool.strings, idx)
}

/// The `(kind, name_idx, ns_idx)` triple `abc_parser` records for a multiname.
///
/// The kind numbers are the format's own. Only the name and namespace are carried: that is what
/// the crate resolves against, and a runtime-qualified name has neither until it runs.
fn multiname_parts(mn: &rt::Multiname) -> (u8, u32, u32) {
    match mn {
        rt::Multiname::QName { namespace, name } => (0x07, name.0, namespace.0),
        rt::Multiname::QNameA { namespace, name } => (0x0D, name.0, namespace.0),
        rt::Multiname::RTQName { name } => (0x0F, name.0, 0),
        rt::Multiname::RTQNameA { name } => (0x10, name.0, 0),
        rt::Multiname::RTQNameL => (0x11, 0, 0),
        rt::Multiname::RTQNameLA => (0x12, 0, 0),
        rt::Multiname::Multiname { namespace_set: _, name } => (0x09, name.0, 0),
        rt::Multiname::MultinameA { namespace_set: _, name } => (0x0E, name.0, 0),
        rt::Multiname::MultinameL { namespace_set: _ } => (0x1B, 0, 0),
        rt::Multiname::MultinameLA { namespace_set: _ } => (0x1C, 0, 0),
        rt::Multiname::TypeName { base_type, .. } => (0x1D, base_type.0, 0),
    }
}

/// A numeric default, resolved against whichever pool the value kind names.
fn default_value(pool: &rt::ConstantPool, v: &rt::DefaultValue) -> Option<f64> {
    match v {
        rt::DefaultValue::Int(i) => pool.ints.get(i.0 as usize - 1).map(|v| *v as f64),
        rt::DefaultValue::Uint(i) => pool.uints.get(i.0 as usize - 1).map(|v| *v as f64),
        rt::DefaultValue::Double(i) => pool.doubles.get(i.0 as usize - 1).copied(),
        rt::DefaultValue::True => Some(1.0),
        rt::DefaultValue::False | rt::DefaultValue::Null | rt::DefaultValue::Undefined => Some(0.0),
        _ => None,
    }
}

fn convert_trait(pool: &rt::ConstantPool, t: &rt::Trait) -> Trait {
    let (_, name_idx, _) = multiname_parts(
        pool.multinames.get(t.name.0 as usize - 1).unwrap_or(&rt::Multiname::RTQNameL));
    let name = string_at(&pool.strings, name_idx).to_string();
    let (kind, method_idx, slot_idx, default) = match &t.kind {
        rt::TraitKind::Slot { slot_id, value, .. } =>
            (trait_kind::SLOT, 0, *slot_id, value.as_ref().and_then(|v| default_value(pool, v))),
        rt::TraitKind::Const { slot_id, value, .. } =>
            (trait_kind::CONST, 0, *slot_id, value.as_ref().and_then(|v| default_value(pool, v))),
        rt::TraitKind::Method { disp_id: _, method } =>
            (trait_kind::METHOD, method.0, 0, None),
        rt::TraitKind::Getter { disp_id: _, method } =>
            (trait_kind::GETTER, method.0, 0, None),
        rt::TraitKind::Setter { disp_id: _, method } =>
            (trait_kind::SETTER, method.0, 0, None),
        rt::TraitKind::Class { slot_id, class } =>
            (trait_kind::CLASS, class.0, *slot_id, None),
        rt::TraitKind::Function { slot_id, function } =>
            (trait_kind::FUNCTION, function.0, *slot_id, None),
    };
    Trait { name, kind, method_idx, slot_idx, default }
}

/// Read an ABC file with Ruffle's parser and present it as this crate's [`AbcFile`].
pub fn parse(data: &[u8]) -> Result<AbcFile> {
    let mut reader = swf::avm2::read::Reader::new(data);
    let abc = reader.read().map_err(|e| anyhow!("ruffle abc read: {e}"))?;
    let pool = &abc.constant_pool;

    // pools, in abc_parser's numbering (see the module note): the reserved slot 0 put back on
    // every one of them, with the same filler values the hand parser uses
    let strings: Vec<Arc<str>> = std::iter::once(Arc::from(""))
        .chain(pool.strings.iter().map(|b| Arc::from(String::from_utf8_lossy(b).into_owned().as_str())))
        .collect();

    let multinames: Vec<Multiname> = std::iter::once(Multiname {
            kind: 0, name_idx: 0, ns_idx: 0, name: Arc::from("") })
        .chain(pool.multinames.iter().map(|mn| {
            let (kind, name_idx, ns_idx) = multiname_parts(mn);
            Multiname { kind, name_idx, ns_idx, name: string_at(&pool.strings, name_idx) }
        }))
        .collect();

    let methods: Vec<Method> = abc.methods.iter().map(|m| Method {
        name_idx: m.name.0,
        name: string_at(&pool.strings, m.name.0),
        param_count: m.params.len() as u32,
        return_type_idx: m.return_type.0,
        optionals: m.params.iter()
            .map(|p| p.default_value.as_ref().and_then(|v| default_value(pool, v)))
            .collect(),
    }).collect();

    // A class is its INSTANCE side (name, supertype, instance traits) plus its static side. The
    // two arrive as separate tables in the same order.
    let classes: Vec<Class> = abc.instances.iter().enumerate().map(|(i, inst)| {
        let qualified = |idx: u32| -> String {
            let Some(mn) = pool.multinames.get(idx as usize - 1) else { return String::new() };
            let (_, name_idx, ns_idx) = multiname_parts(mn);
            let name = string_at(&pool.strings, name_idx).to_string();
            let ns = pool.namespaces.get(ns_idx as usize - 1)
                .map(|n| namespace_name(pool, n).to_string()).unwrap_or_default();
            // The crate keeps a timeline class's package qualified (`sandbag_fla.AirDodge_78`)
            // and everything else local. The test is on the JOINED name, not the namespace: the
            // namespace is `sandbag_fla`, which does not contain "_fla." until the dot that joins
            // it to the class is there.
            let joined = format!("{ns}.{name}");
            if joined != name && joined.contains("_fla.") { joined } else { name }
        };
        Class {
            name: qualified(inst.name.0),
            super_name: {
                let s = inst.super_name.0;
                if s == 0 { String::new() } else {
                    let (_, n, _) = multiname_parts(
                        pool.multinames.get(s as usize - 1).unwrap_or(&rt::Multiname::RTQNameL));
                    string_at(&pool.strings, n).to_string()
                }
            },
            instance_methods: inst.traits.iter().map(|t| convert_trait(pool, t)).collect(),
            class_methods: abc.classes.get(i)
                .map(|c| c.traits.iter().map(|t| convert_trait(pool, t)).collect())
                .unwrap_or_default(),
            constructor_idx: inst.init_method.0,
        }
    }).collect();

    let scripts: Vec<Script> = abc.scripts.iter().map(|s| Script {
        init_method_idx: s.init_method.0,
        traits: s.traits.iter().map(|t| convert_trait(pool, t)).collect(),
    }).collect();

    let method_bodies: Vec<MethodBody> = abc.method_bodies.iter().map(|b| MethodBody {
        method_idx: b.method.0,
        max_stack: b.max_stack,
        local_count: b.num_locals,
        bytecode: b.code.clone(),
        activation_traits: b.traits.iter().map(|t| convert_trait(pool, t)).collect(),
    }).collect();

    Ok(AbcFile {
        strings,
        ints: std::iter::once(0i32).chain(pool.ints.iter().copied()).collect(),
        uints: std::iter::once(0u32).chain(pool.uints.iter().copied()).collect(),
        doubles: std::iter::once(f64::NAN).chain(pool.doubles.iter().copied()).collect(),
        multinames,
        methods,
        classes,
        scripts,
        method_bodies,
    })
}
