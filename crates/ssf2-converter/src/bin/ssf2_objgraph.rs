//! ssf2_objgraph — trace the SSF2 AVM2 object graph for the runtime debug bridge.
//!
//! Unlike `engine_physics disasm` (which only prints `multiname.name`), this tool
//! resolves FULLY-QUALIFIED multinames (namespace + local) on getlex/getproperty/
//! callproperty/constructprop so getlex/property chains can be transcribed exactly.
//!
//! Build: cargo build --release -p ssf2_converter --features dev-tools --bin ssf2_objgraph
//! Usage:
//!   ssf2_objgraph <swf> scripts                 -- list every script trait (globally-reachable getlex targets)
//!   ssf2_objgraph <swf> disasm <Class> <method> -- disasm one method with qualified multinames
//!   ssf2_objgraph <swf> disasm-static <Class> <method>
//!   ssf2_objgraph <swf> cinit <Class>           -- disasm the class static init (sets static slots)
//!   ssf2_objgraph <swf> uses <ClassName>        -- methods that getlex/constructprop a class
//!   ssf2_objgraph <swf> slots <Class>           -- list instance + static slots with types & slot ids
//!   ssf2_objgraph <swf> grepmethod <substr>     -- Class::method whose name contains substr
//!   ssf2_objgraph <swf> weather <StageClass>    -- the stage's particle weather, as the converter reads it

use ssf2_converter::abc_codec::{self, Abc, Multiname, TraitKindData};

fn load(path: &str) -> Abc {
    let data = std::fs::read(path).expect("read swf");
    // SSF2 ships stages/chars as DAT-wrapped archives; unwrap to the inner SWF first
    // (raw swf::decompress_swf fails on the DAT container), then decompress the SWF.
    let inner = ssf2_converter::ssf::decompress(&data).expect("ssf decompress");
    let buf = swf::decompress_swf(&inner[..]).expect("decompress");
    let parsed = swf::parse_swf(&buf).expect("parse swf");
    // a SWF can carry several DoAbc2 tags (per-frame ABC); merge by taking the largest
    // (the engine/stage class table is the bulk one). Fall back to the first.
    let abc_bytes = parsed.tags.iter().filter_map(|t| {
        if let swf::Tag::DoAbc2(a) = t { Some(a.data.to_vec()) } else { None }
    }).max_by_key(|b| b.len()).expect("DoAbc2");
    abc_codec::parse(&abc_bytes).expect("parse abc")
}

/// Fully-qualified multiname for disasm display: "ns::local" or "local".
fn mn(abc: &Abc, idx: u32) -> String {
    if idx == 0 { return "<0>".into(); }
    if let Some(q) = abc.multiname_qualified(idx) {
        // qualified already folds "pkg.local"; also show bare local for readability
        return q;
    }
    match abc.multinames.get(idx as usize - 1) {
        Some(Multiname::Multiname { name, .. }) | Some(Multiname::MultinameA { name, .. }) => {
            abc.strings.get(name.wrapping_sub(1) as usize).cloned().unwrap_or_else(|| format!("mn#{idx}"))
        }
        Some(Multiname::RTQNameL) | Some(Multiname::RTQNameLA) => "<rt-name>".into(),
        Some(Multiname::MultinameL { .. }) | Some(Multiname::MultinameLA { .. }) => "<rt-name-l>".into(),
        _ => abc.multiname_local(idx).unwrap_or_else(|| format!("mn#{idx}")),
    }
}

fn class_local(abc: &Abc, ci: usize) -> String {
    let inst = &abc.instances[ci];
    abc.multiname_local(inst.name).unwrap_or_default()
}

fn find_class(abc: &Abc, name: &str) -> Option<usize> { abc.find_class_by_name(name) }

fn body_for<'a>(abc: &'a Abc, method: u32) -> Option<&'a abc_codec::MethodBody> {
    abc.bodies.iter().find(|b| b.method == method)
}

fn trait_method(t: &abc_codec::Trait) -> Option<u32> {
    match &t.data {
        TraitKindData::Method { method, .. }
        | TraitKindData::Getter { method, .. }
        | TraitKindData::Setter { method, .. }
        | TraitKindData::Function { function: method, .. } => Some(*method),
        _ => None,
    }
}

fn rd_u30(b: &[u8], i: &mut usize) -> u32 {
    let mut r = 0u32; let mut s = 0;
    while *i < b.len() { let x = b[*i] as u32; *i += 1; r |= (x & 0x7f) << s; s += 7; if x & 0x80 == 0 { break; } }
    r
}
fn rd_s24(b: &[u8], i: &mut usize) -> i32 {
    let v = (b[*i] as i32) | ((b[*i+1] as i32) << 8) | ((b[*i+2] as i32) << 16);
    *i += 3;
    if v & 0x800000 != 0 { v - 0x1000000 } else { v }
}

fn disasm(abc: &Abc, code: &[u8]) {
    let mut i = 0usize;
    while i < code.len() {
        let off = i;
        let op = code[i]; i += 1;
        let text = match op {
            0x24 => { let v = code[i] as i8; i += 1; format!("pushbyte {v}") }
            0x25 => { let v = rd_u30(code, &mut i); format!("pushshort {v}") }
            0x26 => "pushtrue".into(),
            0x27 => "pushfalse".into(),
            0x20 => "pushnull".into(),
            0x21 => "pushundefined".into(),
            0x2c => { let v = rd_u30(code, &mut i); format!("pushstring {:?}", abc.strings.get(v.wrapping_sub(1) as usize)) }
            0x2d => { let v = rd_u30(code, &mut i); format!("pushint {:?}", abc.ints.get(v.wrapping_sub(1) as usize)) }
            0x2f => { let v = rd_u30(code, &mut i); format!("pushdouble {:?}", abc.doubles.get(v.wrapping_sub(1) as usize)) }
            0x30 => "pushscope".into(),
            0x31 => "pushwith".into(),
            0xd0 => "getlocal_0 (this)".into(),
            0xd1 => "getlocal_1".into(),
            0xd2 => "getlocal_2".into(),
            0xd3 => "getlocal_3".into(),
            0x62 => { let v = rd_u30(code, &mut i); format!("getlocal {v}") }
            0xd4 => "setlocal_0".into(),
            0xd5 => "setlocal_1".into(),
            0xd6 => "setlocal_2".into(),
            0xd7 => "setlocal_3".into(),
            0x63 => { let v = rd_u30(code, &mut i); format!("setlocal {v}") }
            0x60 => { let v = rd_u30(code, &mut i); format!("getlex {}", mn(abc, v)) }
            0x5c => { let v = rd_u30(code, &mut i); format!("findproperty {}", mn(abc, v)) }
            0x5d => { let v = rd_u30(code, &mut i); format!("findpropstrict {}", mn(abc, v)) }
            0x66 => { let v = rd_u30(code, &mut i); format!("getproperty {}", mn(abc, v)) }
            0x61 => { let v = rd_u30(code, &mut i); format!("setproperty {}", mn(abc, v)) }
            0x68 => { let v = rd_u30(code, &mut i); format!("initproperty {}", mn(abc, v)) }
            0x4a => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("constructprop {} ({n} args)", mn(abc, m)) }
            0x42 => { let n = rd_u30(code, &mut i); format!("construct ({n} args)") }
            0x46 => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callproperty {} ({n} args)", mn(abc, m)) }
            0x4f => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callpropvoid {} ({n} args)", mn(abc, m)) }
            0x4c => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callproplex {} ({n} args)", mn(abc, m)) }
            0x45 => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callsuper {} ({n} args)", mn(abc, m)) }
            0x4e => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callsupervoid {} ({n} args)", mn(abc, m)) }
            0x47 => "returnvoid".into(),
            0x48 => "returnvalue".into(),
            0xa0 => "add".into(), 0xa1 => "subtract".into(), 0xa2 => "multiply".into(),
            0xa3 => "divide".into(), 0xa4 => "modulo".into(), 0x90 => "negate".into(),
            0xc0 => "increment_i".into(), 0x91 => "increment".into(), 0x93 => "decrement".into(),
            0x96 => "not".into(), 0xab => "equals".into(), 0xad => "lessthan".into(),
            0xae => "lessequals".into(), 0xaf => "greaterthan".into(), 0xb0 => "greaterequals".into(),
            0x12 => { let t = rd_s24(code, &mut i); format!("iffalse -> {}", i as i32 + t) }
            0x13 => { let t = rd_s24(code, &mut i); format!("iftrue -> {}", i as i32 + t) }
            0x14 => { let t = rd_s24(code, &mut i); format!("ifne -> {}", i as i32 + t) }
            0x10 => { let t = rd_s24(code, &mut i); format!("jump -> {}", i as i32 + t) }
            0x1b => { i += 4; "lookupswitch".into() } // approximate skip; rarely in our targets
            0x09 => "label".into(),
            0x29 => "pop".into(), 0x2a => "dup".into(), 0x2b => "swap".into(),
            0x65 => { let v = rd_u30(code, &mut i); format!("getscopeobject {v}") }
            0x6c => { let v = rd_u30(code, &mut i); format!("getslot {v}") }
            0x6d => { let v = rd_u30(code, &mut i); format!("setslot {v}") }
            0x80 => { let v = rd_u30(code, &mut i); format!("coerce {}", mn(abc, v)) }
            0x73 => "convert_i".into(), 0x75 => "convert_d".into(),
            0x76 => "convert_b".into(), 0x70 => "convert_s".into(), 0x82 => "coerce_a".into(),
            0x57 => "newactivation".into(),
            0x32 => { let a = rd_u30(code, &mut i); let b = rd_u30(code, &mut i); format!("hasnext2 {a},{b}") }
            0xef => { i += 8; "debug".into() }
            0xf0 => { rd_u30(code, &mut i); "debugline".into() }
            0xf1 => { rd_u30(code, &mut i); "debugfile".into() }
            _ => format!("op 0x{op:02x}"),
        };
        println!("{off:>5}: {text}");
    }
}

fn disasm_class_method(abc: &Abc, cname: &str, mname: &str, static_side: bool) {
    let Some(ci) = find_class(abc, cname) else { eprintln!("no class {cname}"); return; };
    let traits: &[abc_codec::Trait] = if static_side { &abc.classes[ci].traits } else { &abc.instances[ci].traits };
    for t in traits {
        if abc.multiname_local(t.name).as_deref() == Some(mname) {
            if let Some(m) = trait_method(t) {
                let kind = match &t.data { TraitKindData::Getter{..}=>"getter", TraitKindData::Setter{..}=>"setter", _=>"method" };
                println!("// {cname}::{mname} [{kind}] method#{m}{}", if static_side {" (static)"} else {""});
                if let Some(b) = body_for(abc, m) { disasm(abc, &b.code); } else { println!("// (no body — abstract/native)"); }
                println!();
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 { eprintln!("usage: ssf2_objgraph <swf> <cmd> [args]"); std::process::exit(1); }
    let abc = load(&args[1]);
    match args[2].as_str() {
        "placenames" => {
            // every PlaceObject instance NAME in the file, with the sprite it is placed in
            let data = std::fs::read(&args[1]).expect("read swf");
            let inner = ssf2_converter::ssf::decompress(&data).expect("ssf decompress");
            let buf = swf::decompress_swf(&inner[..]).expect("decompress");
            let sw = swf::parse_swf(&buf).expect("parse swf");
            let want = args.get(3).cloned().unwrap_or_default().to_ascii_lowercase();
            let mut show = |owner: String, tags: &[swf::Tag]| {
                for t in tags {
                    if let swf::Tag::PlaceObject(po) = t {
                        if let Some(n) = &po.name {
                            let n = n.to_string_lossy(swf::UTF_8).to_string();
                            if want.is_empty() || n.to_ascii_lowercase().contains(&want) {
                                println!("{owner}: name={n} depth={}", po.depth);
                            }
                        }
                    }
                }
            };
            show("<root>".into(), &sw.tags);
            for t in &sw.tags {
                if let swf::Tag::DefineSprite(sp) = t { show(format!("sprite#{}", sp.id), &sp.tags); }
            }
        }
        "cambg" => {
            let data = std::fs::read(&args[1]).expect("read swf");
            let inner = ssf2_converter::ssf::decompress(&data).expect("ssf decompress");
            let sw = ssf2_converter::swf_parser::parse(&inner).expect("parse");
            for b in &sw.abc_blocks {
                let Ok(a) = ssf2_converter::abc_parser::parse(b) else { continue };
                if let Some(md) = ssf2_converter::abc_parser::extract_main_package_metadata(&a) {
                    if md.id.is_some() { println!("{:#?}", md.camera_backgrounds); return; }
                }
            }
        }
        "orphansprites" => {
            // DefineSprite ids no timeline ever places: engine-instantiated art (a stage resource
            // the engine adds itself), which the placement walk cannot see.
            let data = std::fs::read(&args[1]).expect("read swf");
            let inner = ssf2_converter::ssf::decompress(&data).expect("ssf decompress");
            let buf = swf::decompress_swf(&inner[..]).expect("decompress");
            let sw = swf::parse_swf(&buf).expect("parse swf");
            let mut placed = std::collections::BTreeSet::new();
            let mut sprites = std::collections::BTreeMap::new();
            let mut note = |tags: &[swf::Tag]| {
                for t in tags {
                    if let swf::Tag::PlaceObject(po) = t {
                        match po.action {
                            swf::PlaceObjectAction::Place(c) | swf::PlaceObjectAction::Replace(c) => { placed.insert(c); }
                            _ => {}
                        }
                    }
                }
            };
            note(&sw.tags);
            for t in &sw.tags {
                if let swf::Tag::DefineSprite(sp) = t { sprites.insert(sp.id, sp.tags.len()); note(&sp.tags); }
            }
            for (id, n) in sprites {
                if !placed.contains(&id) { println!("orphan sprite id={id} tags={n}"); }
            }
        }
        "weather" => {
            // what the converter's own weather reader makes of this stage: the particle class, the
            // count and area, and the per-frame drift stepped out of the weather class.
            let data = std::fs::read(&args[1]).expect("read swf");
            let inner = ssf2_converter::ssf::decompress(&data).expect("ssf decompress");
            let sw = ssf2_converter::swf_parser::parse(&inner).expect("parse");
            let stage = args.get(3).cloned().unwrap_or_default();
            for b in &sw.abc_blocks {
                let Ok(a) = ssf2_converter::abc_parser::parse(b) else { continue };
                if let Some(w) = ssf2_converter::abc_parser::extract_stage_weather(&a, &stage) {
                    println!("{w:#?}");
                    return;
                }
            }
            println!("no weather found for stage class {stage:?}");
        }
        "scripts" => {
            for (si, s) in abc.scripts.iter().enumerate() {
                println!("== script #{si} init=method#{} ==", s.init);
                for t in &s.traits {
                    let nm = abc.multiname_qualified(t.name).or_else(|| abc.multiname_local(t.name)).unwrap_or_default();
                    match &t.data {
                        TraitKindData::Class { classi, slot_id } => {
                            let cl = class_local(&abc, *classi as usize);
                            println!("  class  {nm:<48} (classi {classi}, slot {slot_id}) -> {cl}");
                        }
                        TraitKindData::Slot { type_name, slot_id, .. } =>
                            println!("  slot   {nm:<48} type={:?} slot{slot_id}", abc.multiname_local(*type_name)),
                        TraitKindData::Const { type_name, slot_id, .. } =>
                            println!("  const  {nm:<48} type={:?} slot{slot_id}", abc.multiname_local(*type_name)),
                        TraitKindData::Method { method, .. } => println!("  method {nm:<48} method#{method}"),
                        TraitKindData::Function { function, .. } => println!("  func   {nm:<48} fn#{function}"),
                        _ => println!("  other  {nm}"),
                    }
                }
            }
        }
        "methods" => {
            // List every named method trait of a class (instance + static) with its method#, so a
            // stage object's behavior + frame scripts can be enumerated then disassembled.
            let Some(ci) = find_class(&abc, &args[3]) else { eprintln!("no class"); return; };
            println!("== {} instance methods ==", &args[3]);
            for t in &abc.instances[ci].traits {
                if let Some(m) = trait_method(t) {
                    println!("  {:<40} method#{m}", abc.multiname_local(t.name).unwrap_or_default());
                }
            }
            println!("== static methods ==");
            for t in &abc.classes[ci].traits {
                if let Some(m) = trait_method(t) {
                    println!("  {:<40} method#{m}", abc.multiname_local(t.name).unwrap_or_default());
                }
            }
        }
        "disasm" => disasm_class_method(&abc, &args[3], &args[4], false),
        "disasm-static" => disasm_class_method(&abc, &args[3], &args[4], true),
        "iinit" => {
            // disasm a class's instance initializer (the AS3 constructor) — where stage
            // DATs `register(...)` their metadata. Not a named trait, so it needs its own path.
            let Some(ci) = find_class(&abc, &args[3]) else { eprintln!("no class"); return; };
            let iinit = abc.instances[ci].iinit;
            println!("// {}::iinit method#{iinit}", &args[3]);
            if let Some(b) = body_for(&abc, iinit) { disasm(&abc, &b.code); }
        }
        "cinit" => {
            let Some(ci) = find_class(&abc, &args[3]) else { eprintln!("no class"); return; };
            let cinit = abc.classes[ci].cinit;
            println!("// {}::cinit method#{cinit}", &args[3]);
            if let Some(b) = body_for(&abc, cinit) { disasm(&abc, &b.code); }
        }
        "slots" => {
            let Some(ci) = find_class(&abc, &args[3]) else { eprintln!("no class"); return; };
            // a slot trait can carry a DEFAULT value (vkind selects the constant pool; pool
            // vectors store [1..], so vec index = vindex-1). Print it so class constants
            // (e.g. a platform's SINK_SPEED) are readable without a live probe.
            let slot_default = |vindex: u32, vkind: u8| -> String {
                if vindex == 0 { return String::new(); }
                let i = (vindex as usize).saturating_sub(1);
                let v = match vkind {
                    3 => abc.ints.get(i).map(|v| v.to_string()),
                    4 => abc.uints.get(i).map(|v| v.to_string()),
                    6 => abc.doubles.get(i).map(|v| v.to_string()),
                    1 => abc.strings.get(i).map(|s| format!("{s:?}")),
                    11 => Some("true".into()),
                    12 => Some("false".into()),
                    _ => None,
                };
                v.map(|v| format!(" = {v}")).unwrap_or_else(|| format!(" = <vkind {vkind}#{vindex}>"))
            };
            println!("== instance slots ==");
            for t in &abc.instances[ci].traits {
                if let TraitKindData::Slot { slot_id, type_name, vindex, vkind } | TraitKindData::Const { slot_id, type_name, vindex, vkind } = &t.data {
                    println!("  slot{slot_id:<3} {:<32} type={:?}{}", abc.multiname_local(t.name).unwrap_or_default(), abc.multiname_local(*type_name), slot_default(*vindex, *vkind));
                }
            }
            println!("== static slots ==");
            for t in &abc.classes[ci].traits {
                if let TraitKindData::Slot { slot_id, type_name, vindex, vkind } | TraitKindData::Const { slot_id, type_name, vindex, vkind } = &t.data {
                    println!("  slot{slot_id:<3} {:<32} type={:?}{}", abc.multiname_local(t.name).unwrap_or_default(), abc.multiname_local(*type_name), slot_default(*vindex, *vkind));
                }
            }
        }
        "uses" => {
            // Find every Class::method whose body getlex/constructprop/findpropstrict a multiname
            // whose local name == target.
            let target = &args[3];
            for (ci, inst) in abc.instances.iter().enumerate() {
                let cname = class_local(&abc, ci);
                let all = inst.traits.iter().chain(abc.classes[ci].traits.iter());
                for t in all {
                    let Some(m) = trait_method(t) else { continue; };
                    let Some(b) = body_for(&abc, m) else { continue; };
                    if body_refs_name(&abc, &b.code, target) {
                        println!("{cname}::{} method#{m}", abc.multiname_local(t.name).unwrap_or_default());
                    }
                }
            }
        }
        "grepmethod" => {
            let sub = args[3].to_lowercase();
            for (ci, inst) in abc.instances.iter().enumerate() {
                let cname = class_local(&abc, ci);
                for t in inst.traits.iter().chain(abc.classes[ci].traits.iter()) {
                    if let Some(nm) = abc.multiname_local(t.name) {
                        if nm.to_lowercase().contains(&sub) {
                            let m = trait_method(t).map(|m| format!("method#{m}")).unwrap_or_else(|| "slot".into());
                            println!("{cname}::{nm} {m}");
                        }
                    }
                }
            }
        }
        other => eprintln!("unknown cmd {other}"),
    }
}

/// Scan a method body for getlex/findpropstrict/constructprop/getproperty whose
/// multiname local name equals `target`.
fn body_refs_name(abc: &Abc, code: &[u8], target: &str) -> bool {
    let mut i = 0usize;
    while i < code.len() {
        let op = code[i]; i += 1;
        match op {
            0x60 | 0x5c | 0x5d | 0x66 | 0x61 | 0x68 | 0x80 => {
                let v = rd_u30(code, &mut i);
                if abc.multiname_local(v).as_deref() == Some(target) { return true; }
            }
            // call ops: first u30 = multiname, second u30 = argc
            0x4a | 0x46 | 0x4f | 0x4c | 0x45 | 0x4e => {
                let v = rd_u30(code, &mut i); let _argc = rd_u30(code, &mut i);
                if abc.multiname_local(v).as_deref() == Some(target) { return true; }
            }
            0x24 | 0x63 | 0x62 | 0x65 | 0x6c | 0x6d | 0x2c | 0x2d | 0x2e | 0x2f | 0x25 => { let _ = rd_u30(code, &mut i); }
            0x10 | 0x11 | 0x12 | 0x13 | 0x14 | 0x15 | 0x16 | 0x17 | 0x18 | 0x19 | 0x1a => { i += 3; }
            0xef => { i += 8; }
            _ => {}
        }
    }
    false
}
