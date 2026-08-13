//! dump_method — disasm a method by index / iinit / cinit, and print signatures.
//!
//! Build: cargo build --release -p ssf2_converter --features dev-tools --bin dump_method
//! Usage:
//!   dump_method <swf> m <methodIndex>          -- disasm raw method index
//!   dump_method <swf> iinit <Class>            -- disasm instance constructor (+sig)
//!   dump_method <swf> cinit <Class>            -- disasm class static init
//!   dump_method <swf> sig <methodIndex>        -- print param/return types only
//!   dump_method <swf> sigclass <Class>         -- print signature of every method on a class
//!   dump_method <swf> find <name>              -- every method whose disasm mentions <name>

use ssf2_converter::abc_codec::{self, Abc, Multiname, TraitKindData};

fn load(path: &str) -> Abc {
    let data = std::fs::read(path).expect("read swf");
    let buf = swf::decompress_swf(&data[..]).expect("decompress");
    let parsed = swf::parse_swf(&buf).expect("parse swf");
    let abc_bytes = parsed.tags.iter().find_map(|t| {
        if let swf::Tag::DoAbc2(a) = t { Some(a.data.to_vec()) } else { None }
    }).expect("DoAbc2");
    abc_codec::parse(&abc_bytes).expect("parse abc")
}

fn mn(abc: &Abc, idx: u32) -> String {
    if idx == 0 { return "*".into(); }
    if let Some(q) = abc.multiname_qualified(idx) { return q; }
    abc.multiname_local(idx).unwrap_or_else(|| format!("mn#{idx}"))
}

fn body_for<'a>(abc: &'a Abc, method: u32) -> Option<&'a abc_codec::MethodBody> {
    abc.bodies.iter().find(|b| b.method == method)
}

fn print_sig(abc: &Abc, m: u32, label: &str) {
    let Some(info) = abc.methods.get(m as usize) else { println!("{label}: no method#{m}"); return; };
    let params: Vec<String> = info.param_types.iter().enumerate().map(|(k, t)| {
        let ty = mn(abc, *t);
        let nm = info.param_names.get(k).and_then(|n| abc.strings.get(n.wrapping_sub(1) as usize).cloned()).unwrap_or_default();
        if nm.is_empty() { ty } else { format!("{nm}:{ty}") }
    }).collect();
    let opt = if info.flags & 0x08 != 0 { format!(" [{} optional]", info.options.len()) } else { String::new() };
    println!("{label}: method#{m}({}) : {}{opt}", params.join(", "), mn(abc, info.return_type));
}

fn rd_u30(b: &[u8], i: &mut usize) -> u32 {
    let mut r = 0u32; let mut s = 0;
    while *i < b.len() { let x = b[*i] as u32; *i += 1; r |= (x & 0x7f) << s; s += 7; if x & 0x80 == 0 { break; } }
    r
}
fn rd_s24(b: &[u8], i: &mut usize) -> i32 {
    // `find` disassembles every body in the file, so a truncated tail has to read as 0
    // rather than panic partway through the scan.
    if *i + 3 > b.len() { *i = b.len(); return 0; }
    let v = (b[*i] as i32) | ((b[*i+1] as i32) << 8) | ((b[*i+2] as i32) << 16);
    *i += 3;
    if v & 0x800000 != 0 { v - 0x1000000 } else { v }
}

fn disasm(abc: &Abc, code: &[u8]) {
    print!("{}", disasm_text(abc, code));
}

fn disasm_text(abc: &Abc, code: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0usize;
    while i < code.len() {
        let off = i;
        let op = code[i]; i += 1;
        let text = match op {
            0x24 => { let v = code[i] as i8; i += 1; format!("pushbyte {v}") }
            0x25 => { let v = rd_u30(code, &mut i); format!("pushshort {v}") }
            0x26 => "pushtrue".into(), 0x27 => "pushfalse".into(),
            0x20 => "pushnull".into(), 0x21 => "pushundefined".into(),
            0x28 => "pushnan".into(),
            0x2c => { let v = rd_u30(code, &mut i); format!("pushstring {:?}", abc.strings.get(v.wrapping_sub(1) as usize)) }
            0x2d => { let v = rd_u30(code, &mut i); format!("pushint {:?}", abc.ints.get(v.wrapping_sub(1) as usize)) }
            0x2e => { let v = rd_u30(code, &mut i); format!("pushuint {:?}", abc.uints.get(v.wrapping_sub(1) as usize)) }
            0x2f => { let v = rd_u30(code, &mut i); format!("pushdouble {:?}", abc.doubles.get(v.wrapping_sub(1) as usize)) }
            0x30 => "pushscope".into(), 0x31 => "pushwith".into(),
            0xd0 => "getlocal_0 (this)".into(), 0xd1 => "getlocal_1".into(),
            0xd2 => "getlocal_2".into(), 0xd3 => "getlocal_3".into(),
            0x62 => { let v = rd_u30(code, &mut i); format!("getlocal {v}") }
            0xd4 => "setlocal_0".into(), 0xd5 => "setlocal_1".into(),
            0xd6 => "setlocal_2".into(), 0xd7 => "setlocal_3".into(),
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
            0x43 => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callmethod {} ({n} args)", m) }
            0x45 => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callsuper {} ({n} args)", mn(abc, m)) }
            0x4e => { let m = rd_u30(code, &mut i); let n = rd_u30(code, &mut i); format!("callsupervoid {} ({n} args)", mn(abc, m)) }
            0x53 => { let n = rd_u30(code, &mut i); format!("applytype ({n})") }
            0x55 => { let n = rd_u30(code, &mut i); format!("newobject ({n})") }
            0x56 => { let n = rd_u30(code, &mut i); format!("newarray ({n})") }
            0x58 => { let v = rd_u30(code, &mut i); format!("newclass {v}") }
            0x47 => "returnvoid".into(), 0x48 => "returnvalue".into(),
            0xa0 => "add".into(), 0xa1 => "subtract".into(), 0xa2 => "multiply".into(),
            0xa3 => "divide".into(), 0xa4 => "modulo".into(), 0x90 => "negate".into(),
            0xc0 => "increment_i".into(), 0x91 => "increment".into(), 0x93 => "decrement".into(),
            0x96 => "not".into(), 0xab => "equals".into(), 0xac => "strictequals".into(),
            0xad => "lessthan".into(), 0xae => "lessequals".into(),
            0xaf => "greaterthan".into(), 0xb0 => "greaterequals".into(),
            0x12 => { let t = rd_s24(code, &mut i); format!("iffalse -> {}", i as i32 + t) }
            0x13 => { let t = rd_s24(code, &mut i); format!("iftrue -> {}", i as i32 + t) }
            0x14 => { let t = rd_s24(code, &mut i); format!("ifne -> {}", i as i32 + t) }
            0x15 => { let t = rd_s24(code, &mut i); format!("iflt -> {}", i as i32 + t) }
            0x16 => { let t = rd_s24(code, &mut i); format!("ifle -> {}", i as i32 + t) }
            0x17 => { let t = rd_s24(code, &mut i); format!("ifgt -> {}", i as i32 + t) }
            0x18 => { let t = rd_s24(code, &mut i); format!("ifge -> {}", i as i32 + t) }
            0x19 => { let t = rd_s24(code, &mut i); format!("ifstricteq -> {}", i as i32 + t) }
            0x1a => { let t = rd_s24(code, &mut i); format!("ifstrictne -> {}", i as i32 + t) }
            0x11 => { let t = rd_s24(code, &mut i); format!("iftrue2 -> {}", i as i32 + t) }
            0x0c => { let t = rd_s24(code, &mut i); format!("ifnlt -> {}", i as i32 + t) }
            0x0d => { let t = rd_s24(code, &mut i); format!("ifnle -> {}", i as i32 + t) }
            0x0e => { let t = rd_s24(code, &mut i); format!("ifngt -> {}", i as i32 + t) }
            0x0f => { let t = rd_s24(code, &mut i); format!("ifnge -> {}", i as i32 + t) }
            0x10 => { let t = rd_s24(code, &mut i); format!("jump -> {}", i as i32 + t) }
            0x1b => {
                let def = rd_s24(code, &mut i);
                let n = rd_u30(code, &mut i);
                for _ in 0..=n { rd_s24(code, &mut i); }
                format!("lookupswitch (def {def}, {n}+1 cases)")
            }
            0x09 => "label".into(),
            0x29 => "pop".into(), 0x2a => "dup".into(), 0x2b => "swap".into(),
            0x65 => { let v = rd_u30(code, &mut i); format!("getscopeobject {v}") }
            0x6c => { let v = rd_u30(code, &mut i); format!("getslot {v}") }
            0x6d => { let v = rd_u30(code, &mut i); format!("setslot {v}") }
            0x80 => { let v = rd_u30(code, &mut i); format!("coerce {}", mn(abc, v)) }
            0x73 => "convert_i".into(), 0x74 => "convert_u".into(), 0x75 => "convert_d".into(),
            0x76 => "convert_b".into(), 0x70 => "convert_s".into(), 0x82 => "coerce_a".into(),
            0x85 => "coerce_s".into(),
            0x57 => "newactivation".into(),
            0x32 => { let a = rd_u30(code, &mut i); let b = rd_u30(code, &mut i); format!("hasnext2 {a},{b}") }
            0x08 => { let v = rd_u30(code, &mut i); format!("kill {v}") }
            0xef => { i += 8; "debug".into() }
            0xf0 => { rd_u30(code, &mut i); "debugline".into() }
            0xf1 => { rd_u30(code, &mut i); "debugfile".into() }
            _ => format!("op 0x{op:02x}"),
        };
        out.push_str(&format!("{off:>5}: {text}\n"));
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 { eprintln!("usage: dump_method <swf> <cmd> [args]"); std::process::exit(1); }
    let abc = load(&args[1]);
    match args[2].as_str() {
        "m" => {
            let m: u32 = args[3].parse().unwrap();
            print_sig(&abc, m, "sig");
            if let Some(b) = body_for(&abc, m) { disasm(&abc, &b.code); } else { println!("// no body (native/abstract)"); }
        }
        "sig" => {
            let m: u32 = args[3].parse().unwrap();
            print_sig(&abc, m, "sig");
        }
        "iinit" => {
            let ci = abc.find_class_by_name(&args[3]).expect("no class");
            let m = abc.instances[ci].iinit;
            print_sig(&abc, m, &format!("{}::iinit", &args[3]));
            if let Some(b) = body_for(&abc, m) { disasm(&abc, &b.code); }
        }
        "cinit" => {
            let ci = abc.find_class_by_name(&args[3]).expect("no class");
            let m = abc.classes[ci].cinit;
            println!("// {}::cinit method#{m}", &args[3]);
            if let Some(b) = body_for(&abc, m) { disasm(&abc, &b.code); }
        }
        "sigclass" => {
            let ci = abc.find_class_by_name(&args[3]).expect("no class");
            for t in &abc.instances[ci].traits {
                let m = match &t.data {
                    TraitKindData::Method { method, .. } => ("method", *method),
                    TraitKindData::Getter { method, .. } => ("getter", *method),
                    TraitKindData::Setter { method, .. } => ("setter", *method),
                    _ => continue,
                };
                let nm = abc.multiname_local(t.name).unwrap_or_default();
                print_sig(&abc, m.1, &format!("  {} {}", m.0, nm));
            }
            println!("== static ==");
            for t in &abc.classes[ci].traits {
                let m = match &t.data {
                    TraitKindData::Method { method, .. } => ("method", *method),
                    TraitKindData::Getter { method, .. } => ("getter", *method),
                    TraitKindData::Setter { method, .. } => ("setter", *method),
                    _ => continue,
                };
                let nm = abc.multiname_local(t.name).unwrap_or_default();
                print_sig(&abc, m.1, &format!("  {} {}", m.0, nm));
            }
        }
        // A character's helpers live in nested closures, not class traits, so there's no
        // index to look up — scan every body for one that mentions the name.
        "find" => {
            let needle = &args[3];
            for b in &abc.bodies {
                // a nested `function NAME(...)` is an activation-object trait on the
                // ENCLOSING body, initialized by newfunction — the name never appears in
                // the instruction stream, so the traits have to be checked separately.
                let defs: Vec<String> = b.traits.iter()
                    .filter(|t| abc.multiname_local(t.name).as_deref() == Some(needle.as_str()))
                    .map(|t| match &t.data {
                        TraitKindData::Method { method, .. } => format!("  DEFINES {needle} = method#{method}"),
                        other => format!("  DEFINES {needle} ({other:?})"),
                    })
                    .collect();
                let text = disasm_text(&abc, &b.code);
                let hits: Vec<&str> = text.lines().filter(|l| l.contains(needle.as_str())).collect();
                if defs.is_empty() && hits.is_empty() { continue; }
                print_sig(&abc, b.method, &format!("method#{}", b.method));
                for d in defs { println!("{d}"); }
                for line in hits { println!("  {line}"); }
            }
        }
        other => { let _: Option<Multiname> = None; eprintln!("unknown {other}"); }
    }
}
