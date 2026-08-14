//! Does every content id the emitted scripts ask for actually exist?
//!
//! `getContent(id)` does NOT validate: the engine builds `private::<resource>.<id>` from whatever
//! it is handed, so an id nothing emitted is a DANGLING reference rather than an error. What the
//! engine then draws is the resource's first animation -- for a character, `revival`, which is how
//! a converted move came to draw the bag on its blue revival platform mid-attack.
//!
//! Nothing checked this, which is why that survived. This walks a finished package the way the
//! engine would: collect every id the scripts ask for, collect everything the package ships
//! (manifest content, entities, sprites, audio), and report the difference. Cheap, and it turns a
//! class of silent visual fault into a line in the conversion log.

use std::collections::BTreeSet;
use std::path::Path;

/// One unresolvable reference: the id asked for, and the file that asks.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DanglingRef {
    pub id: String,
    pub asked_by: String,
}

/// Every content id a package SHIPS: manifest entries plus the assets on disk.
pub fn shipped_ids(lib: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    // Content declared in the manifest -- structures live only here, with no file of their own.
    if let Ok(txt) = std::fs::read_to_string(lib.join("manifest.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
            if let Some(items) = v.get("content").and_then(|c| c.as_array()) {
                out.extend(items.iter().filter_map(|c| c.get("id")?.as_str().map(String::from)));
            }
        }
    }
    for sub in ["entities", "sprites", "audio", "sounds"] {
        collect_assets(&lib.join(sub), &mut out);
    }
    out
}

fn collect_assets(dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() { collect_assets(&path, out); continue; }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        for ext in [".entity", ".png", ".wav"] {
            if let Some(stem) = name.strip_suffix(ext) { out.insert(stem.to_string()); }
        }
    }
}

/// Content ids the package's scripts ask for but do not ship.
pub fn audit(lib: &Path) -> Vec<DanglingRef> {
    let shipped = shipped_ids(lib);
    let mut out = Vec::new();
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    collect_scripts(&lib.join("scripts"), &mut sources);
    collect_scripts(&lib.join("entities"), &mut sources);
    for path in sources {
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        for id in content_ids(&text) {
            if shipped.contains(&id) { continue; }
            let asked_by = path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string();
            let d = DanglingRef { id, asked_by };
            if !out.contains(&d) { out.push(d); }
        }
    }
    out.sort();
    out
}

fn collect_scripts(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() { collect_scripts(&path, out); continue; }
        match path.extension().and_then(|x| x.to_str()) {
            Some("hx") | Some("entity") => out.push(path),
            _ => {}
        }
    }
}

/// The ids inside `getContent("…")`, ignoring any inside a comment.
pub fn content_ids(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let code = match line.find("//") { Some(i) => &line[..i], None => line };
        let mut rest = code;
        while let Some(i) = rest.find("getContent(\"") {
            rest = &rest[i + "getContent(\"".len()..];
            if let Some(j) = rest.find('"') {
                out.push(rest[..j].to_string());
                rest = &rest[j..];
            } else { break; }
        }
    }
    out
}

/// Audit a finished package and log what it finds. Returns the count so a caller can decide.
pub fn audit_and_log(lib: &Path, what: &str) -> usize {
    let dangling = audit(lib);
    for d in &dangling {
        log::warn!("{what}: script {} asks for content '{}' that this package does not ship — \
                    the engine resolves it to a dangling reference and draws the first animation \
                    instead", d.asked_by, d.id);
    }
    dangling.len()
}
