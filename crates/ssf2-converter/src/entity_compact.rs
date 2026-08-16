//! Compacting a finished entity: the same animation, written once.
//!
//! The emitters build an entity the honest way -- walk the source frame by frame, and for every
//! placement on every frame write down a symbol saying where that picture is and a keyframe
//! pointing at it. That is the right way to BUILD it and a wasteful way to STORE it, because
//! source content repeats itself constantly: a decoration holds still for two hundred frames, a
//! splash is invisible for all but a handful, a backdrop keeps the same picture while only its
//! neighbours change. Written literally, one stage came to 43,092 symbols that were really 388.
//!
//! So this is a post-pass, deliberately: it runs on the finished entity, it belongs to no
//! emitter, and every emitter gets it for free. It only ever removes redundancy -- what the
//! entity DRAWS is identical before and after, which is what makes it safe to apply everywhere
//! rather than a thing each caller has to opt into and reason about.
//!
//! Three redundancies, in order:
//!
//! 1. **The same symbol written many times.** Keyframes reference symbols by id, so two
//!    placements with identical properties can share one entry.
//! 2. **Fully transparent placements.** An IMAGE at `alpha: 0` draws nothing, so the keyframe
//!    can simply hold nothing. (With one exception, below, that is the whole reason this is
//!    careful code rather than a one-liner.)
//! 3. **Runs of identical keyframes.** A keyframe already carries a `length`; a hundred
//!    consecutive frames of the same picture are one keyframe of length 100, not a hundred.
//!
//! The exception worth stating: a tween interpolates from its own keyframe's symbol to the NEXT
//! keyframe's symbol, so under a tween an `alpha: 0` symbol is not "nothing", it is one END of a
//! fade. Blanking it would turn a fade-in into a hard cut. A symbol is only blanked when neither
//! its own keyframe nor the one before it is tweened, and runs are only merged between untweened
//! keyframes.

use serde_json::Value;

/// What a pass removed. Worth reporting: a conversion that suddenly stops compacting is a sign
/// that something upstream started emitting subtly-different symbols per frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Compaction {
    pub symbols_before: usize,
    pub symbols_after: usize,
    pub keyframes_before: usize,
    pub keyframes_after: usize,
    /// Fully-transparent placements that became empty keyframes.
    pub blanked: usize,
}

impl Compaction {
    /// True when there was anything to remove, so callers can stay quiet about the ones that
    /// were already tight.
    pub fn saved_anything(&self) -> bool {
        self.symbols_after < self.symbols_before || self.keyframes_after < self.keyframes_before
    }
}

/// Everything about a symbol except which one it is.
fn symbol_content(sym: &Value) -> String {
    match sym.as_object() {
        Some(map) => {
            let mut fields: Vec<(&String, &Value)> = map.iter().filter(|(k, _)| *k != "$id").collect();
            fields.sort_by(|a, b| a.0.cmp(b.0));
            serde_json::to_string(&fields).unwrap_or_default()
        }
        None => sym.to_string(),
    }
}

fn is_invisible_image(sym: &Value) -> bool {
    sym.get("type").and_then(Value::as_str) == Some("IMAGE")
        && sym.get("alpha").and_then(Value::as_f64).map(|a| a <= 0.0).unwrap_or(false)
}

/// Two keyframes that draw the same thing, and so can be one keyframe for longer.
///
/// Restricted to IMAGE keyframes on purpose: a FRAME_SCRIPT carries code that runs on the frame
/// it sits on, so merging two of those would change WHEN the code runs, not just how it is
/// written down.
fn mergeable(a: &Value, b: &Value) -> bool {
    let img = |k: &Value| k.get("type").and_then(Value::as_str) == Some("IMAGE");
    let untweened = |k: &Value| !k.get("tweened").and_then(Value::as_bool).unwrap_or(false);
    img(a) && img(b) && untweened(a) && untweened(b) && a.get("symbol") == b.get("symbol")
}

/// Remove the redundancy from a finished entity, in place.
pub fn compact(entity: &mut Value) -> Compaction {
    let mut st = Compaction {
        symbols_before: entity.get("symbols").and_then(Value::as_array).map_or(0, Vec::len),
        keyframes_before: entity.get("keyframes").and_then(Value::as_array).map_or(0, Vec::len),
        ..Default::default()
    };

    // 1. Share identical symbols. Build id -> surviving id, keeping the first of each content.
    let mut canonical: std::collections::HashMap<String, String> = Default::default();
    {
        let mut by_content: std::collections::HashMap<String, String> = Default::default();
        if let Some(syms) = entity.get("symbols").and_then(Value::as_array) {
            for s in syms {
                let Some(id) = s.get("$id").and_then(Value::as_str) else { continue };
                let keep = by_content.entry(symbol_content(s)).or_insert_with(|| id.to_string());
                canonical.insert(id.to_string(), keep.clone());
            }
        }
        if let Some(kfs) = entity.get_mut("keyframes").and_then(Value::as_array_mut) {
            for k in kfs {
                let Some(cur) = k.get("symbol").and_then(Value::as_str) else { continue };
                if let Some(keep) = canonical.get(cur) {
                    if keep != cur { k["symbol"] = Value::String(keep.clone()); }
                }
            }
        }
    }

    // Which symbols draw nothing, by surviving id.
    let invisible: std::collections::HashSet<String> = entity.get("symbols")
        .and_then(Value::as_array)
        .map(|syms| syms.iter().filter(|s| is_invisible_image(s))
            .filter_map(|s| s.get("$id").and_then(Value::as_str))
            .filter_map(|id| canonical.get(id).cloned())
            .collect())
        .unwrap_or_default();

    // 2 + 3, per layer, because both need the keyframe ORDER: which keyframe a tween reaches
    // forward to, and which keyframes sit next to each other.
    let layer_orders: Vec<Vec<String>> = entity.get("layers").and_then(Value::as_array)
        .map(|ls| ls.iter().map(|l| l.get("keyframes").and_then(Value::as_array)
            .map(|ks| ks.iter().filter_map(|k| k.as_str().map(str::to_string)).collect())
            .unwrap_or_default()).collect())
        .unwrap_or_default();

    let mut kf_by_id: std::collections::HashMap<String, Value> = entity.get("keyframes")
        .and_then(Value::as_array)
        .map(|ks| ks.iter().filter_map(|k| k.get("$id").and_then(Value::as_str)
            .map(|id| (id.to_string(), k.clone()))).collect())
        .unwrap_or_default();

    let mut merged_away: std::collections::HashSet<String> = Default::default();
    let mut new_layer_orders: Vec<Vec<String>> = Vec::with_capacity(layer_orders.len());

    for order in &layer_orders {
        // 2. Blank the placements that draw nothing -- except where a tween needs them as one
        // end of a fade.
        for (i, id) in order.iter().enumerate() {
            let tweened_here = kf_by_id.get(id)
                .and_then(|k| k.get("tweened")).and_then(Value::as_bool).unwrap_or(false);
            let tweened_before = i.checked_sub(1).and_then(|p| order.get(p))
                .and_then(|p| kf_by_id.get(p))
                .and_then(|k| k.get("tweened")).and_then(Value::as_bool).unwrap_or(false);
            if tweened_here || tweened_before { continue; }
            let Some(kf) = kf_by_id.get_mut(id) else { continue };
            let draws_nothing = kf.get("symbol").and_then(Value::as_str)
                .map(|s| invisible.contains(s)).unwrap_or(false);
            if draws_nothing {
                kf["symbol"] = Value::Null;
                st.blanked += 1;
            }
        }

        // 3. Fold runs of identical keyframes into one longer keyframe.
        let mut kept: Vec<String> = Vec::with_capacity(order.len());
        for id in order {
            if !kf_by_id.contains_key(id) { continue; }
            let fold = match kept.last() {
                Some(prev) => {
                    let (a, b) = (&kf_by_id[prev], &kf_by_id[id]);
                    mergeable(a, b)
                }
                None => false,
            };
            if fold {
                let add = kf_by_id[id].get("length").and_then(Value::as_u64).unwrap_or(1);
                let prev = kept.last().unwrap().clone();
                let now = kf_by_id[&prev].get("length").and_then(Value::as_u64).unwrap_or(1);
                kf_by_id.get_mut(&prev).unwrap()["length"] = Value::from(now + add);
                merged_away.insert(id.clone());
            } else {
                kept.push(id.clone());
            }
        }
        new_layer_orders.push(kept);
    }

    // Write the layers' keyframe lists back.
    if let Some(layers) = entity.get_mut("layers").and_then(Value::as_array_mut) {
        for (layer, order) in layers.iter_mut().zip(new_layer_orders) {
            layer["keyframes"] = Value::from(order);
        }
    }

    // Rebuild the keyframe pool: survivors, in their original order, with merged lengths.
    if let Some(kfs) = entity.get_mut("keyframes").and_then(Value::as_array_mut) {
        kfs.retain(|k| k.get("$id").and_then(Value::as_str)
            .map(|id| !merged_away.contains(id)).unwrap_or(true));
        for k in kfs.iter_mut() {
            if let Some(id) = k.get("$id").and_then(Value::as_str).map(str::to_string) {
                if let Some(updated) = kf_by_id.get(&id) { *k = updated.clone(); }
            }
        }
        st.keyframes_after = kfs.len();
    }

    // Finally drop symbols nothing points at any more -- the duplicates, and whatever was
    // blanked out of its last reference.
    let referenced: std::collections::HashSet<String> = entity.get("keyframes")
        .and_then(Value::as_array)
        .map(|ks| ks.iter().filter_map(|k| k.get("symbol").and_then(Value::as_str))
            .map(str::to_string).collect())
        .unwrap_or_default();
    if let Some(syms) = entity.get_mut("symbols").and_then(Value::as_array_mut) {
        syms.retain(|s| s.get("$id").and_then(Value::as_str)
            .map(|id| referenced.contains(id)).unwrap_or(true));
        st.symbols_after = syms.len();
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An entity with one layer: `frames` is (symbol id, tweened) per keyframe.
    fn entity(symbols: Value, frames: &[(Option<&str>, bool)]) -> Value {
        let kfs: Vec<Value> = frames.iter().enumerate().map(|(i, (sym, tw))| json!({
            "$id": format!("k{i}"), "type": "IMAGE", "length": 1,
            "symbol": sym.map(Value::from).unwrap_or(Value::Null),
            "tweened": tw, "tweenType": "LINEAR",
        })).collect();
        let ids: Vec<String> = (0..frames.len()).map(|i| format!("k{i}")).collect();
        json!({ "symbols": symbols, "keyframes": kfs,
                "layers": [{ "$id": "L", "keyframes": ids }] })
    }

    fn img(id: &str, x: f64, alpha: f64) -> Value {
        json!({ "$id": id, "type": "IMAGE", "imageAsset": "A", "x": x, "y": 0.0,
                "alpha": alpha, "scaleX": 1.0, "scaleY": 1.0, "rotation": 0.0 })
    }

    /// The headline case: a decoration that holds still is one symbol and one keyframe.
    #[test]
    fn a_held_pose_collapses() {
        let mut e = entity(
            json!([img("a", 5.0, 1.0), img("b", 5.0, 1.0), img("c", 5.0, 1.0)]),
            &[(Some("a"), false), (Some("b"), false), (Some("c"), false)],
        );
        let st = compact(&mut e);
        assert_eq!(st.symbols_after, 1, "three identical symbols are one");
        assert_eq!(st.keyframes_after, 1, "three identical keyframes are one");
        assert_eq!(e["keyframes"][0]["length"], 3, "and it lasts as long as the three did");
    }

    /// Sharing a symbol must not merge keyframes that draw DIFFERENT things.
    #[test]
    fn distinct_poses_survive() {
        let mut e = entity(
            json!([img("a", 5.0, 1.0), img("b", 9.0, 1.0)]),
            &[(Some("a"), false), (Some("b"), false)],
        );
        let st = compact(&mut e);
        assert_eq!(st.symbols_after, 2);
        assert_eq!(st.keyframes_after, 2);
    }

    /// A fully transparent placement draws nothing, so it holds nothing.
    #[test]
    fn invisible_placements_become_empty() {
        let mut e = entity(
            json!([img("a", 5.0, 1.0), img("b", 5.0, 0.0)]),
            &[(Some("a"), false), (Some("b"), false)],
        );
        let st = compact(&mut e);
        assert_eq!(st.blanked, 1);
        assert_eq!(e["keyframes"][1]["symbol"], Value::Null);
        assert_eq!(st.symbols_after, 1, "the transparent symbol is nobody's target now");
    }

    /// ...unless a tween needs it as one end of a fade. This is the case that makes blanking
    /// careful code: `a` fades out into `b`, and dropping `b` would make it a hard cut.
    #[test]
    fn a_fade_keeps_its_transparent_end() {
        let mut e = entity(
            json!([img("a", 5.0, 1.0), img("b", 5.0, 0.0)]),
            &[(Some("a"), true), (Some("b"), false)],
        );
        let st = compact(&mut e);
        assert_eq!(st.blanked, 0, "the far end of a fade is not nothing");
        assert_eq!(e["keyframes"][1]["symbol"], "b");
        assert_eq!(st.symbols_after, 2);
    }

    /// A tween is never folded into its neighbour: the run between them is the motion.
    #[test]
    fn tweened_keyframes_are_not_merged() {
        let mut e = entity(
            json!([img("a", 5.0, 1.0), img("b", 5.0, 1.0)]),
            &[(Some("a"), true), (Some("b"), true)],
        );
        let st = compact(&mut e);
        assert_eq!(st.keyframes_after, 2);
    }

    /// Consecutive empty frames are one gap, not many.
    #[test]
    fn runs_of_nothing_collapse() {
        let mut e = entity(json!([img("a", 5.0, 1.0)]),
            &[(Some("a"), false), (None, false), (None, false), (None, false)]);
        let st = compact(&mut e);
        assert_eq!(st.keyframes_after, 2, "the pose, then one gap");
        assert_eq!(e["keyframes"][1]["length"], 3);
    }

    /// Frame scripts are left alone: merging them would change when code runs.
    #[test]
    fn frame_scripts_are_never_merged() {
        let mut e = json!({
            "symbols": [],
            "keyframes": [
                { "$id": "k0", "type": "FRAME_SCRIPT", "length": 1, "code": "" },
                { "$id": "k1", "type": "FRAME_SCRIPT", "length": 1, "code": "" },
            ],
            "layers": [{ "$id": "L", "keyframes": ["k0", "k1"] }],
        });
        let st = compact(&mut e);
        assert_eq!(st.keyframes_after, 2);
    }

    /// Total played length is preserved -- a compaction that shortens an animation is a bug.
    #[test]
    fn total_length_is_preserved() {
        let frames: Vec<(Option<&str>, bool)> = (0..20)
            .map(|i| (if i % 7 == 0 { Some("b") } else { Some("a") }, false)).collect();
        let mut e = entity(json!([img("a", 5.0, 1.0), img("b", 9.0, 1.0)]), &frames);
        compact(&mut e);
        let total: u64 = e["keyframes"].as_array().unwrap().iter()
            .map(|k| k["length"].as_u64().unwrap()).sum();
        assert_eq!(total, 20);
    }
}
