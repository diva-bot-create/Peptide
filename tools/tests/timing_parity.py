#!/usr/bin/env python3
"""timing_parity.py — check the 30->60fps doubling for EVERY animation of a character.

The converter's whole timing model is one rule: SSF2 runs at 30fps, Fraymakers at 60,
so a converted animation should be exactly TWICE as long as its source. Everything
downstream (frame data, move duration, the feel of a character) rests on it, and until
now it was spot-checked by driving one move at a time in two live engines.

This checks it statically, for every animation, with no engine running:

  expected  <- PEPTIDE_DUMP_ANIM_SPLITS: the splitter reports, for each emitted Fraymakers
               animation, which SSF2 animation it came from and the [start..end) frame
               range it was sliced from. Expected length = (range + appended head) * 2.
  actual    <- the emitted .entity's IMAGE layer, summing keyframe lengths.

Pairing comes from the splitter itself rather than from animation names, which matters:
one SSF2 timeline becomes SEVERAL Fraymakers animations (AGENT_CONTEXT "animations" — a
Jab sprite becomes jab1/jab2/jab3/jab4), so name-matching either misses the split or sums
pieces that shouldn't be summed. An earlier version guessed from name suffixes and
reported 17 false failures on mario that were all its own grouping.

A looping split is reported but not counted: its emitted length is one CYCLE that the
engine repeats, so it isn't a duration to compare.

Usage:
  tools/tests/timing_parity.py <char> [<char> …]
  tools/tests/timing_parity.py --all          # every converted character
"""
import json, os, re, subprocess, sys, glob

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SSFS = os.environ.get("SSF2_SSFS_DIR", os.path.join(ROOT, "..", "ssf2-ssfs"))
BIN = os.path.join(ROOT, "build", "release", "peptide")


def splits(char, outdir):
    """The EXACT source->Fraymakers mapping the splitter produced.

    Each emitted FM animation names its source animation and the [start..end) frame range
    it was sliced from, so the expected length is (range + appended head frames) * 2 —
    no guessing from name suffixes, and splits are handled by construction.
    """
    # `-n <char>` restricts a MULTI-CHARACTER .ssf (bowser = bowser+gigabowser, zelda =
    # zelda+sheik) to the one character being checked. Without it the dump carries both
    # characters' splits, and a dict keyed on the Fraymakers animation name lets the second
    # silently overwrite the first — which is what made bowser report 105 "failures" whose
    # expected lengths came from gigabowser.
    env = dict(os.environ, PEPTIDE_DUMP_ANIM_SPLITS="1", PEPTIDE_DUMP_ANIM_LABELS="1",
               PEPTIDE_DUMP_BOX_FRAMES="1")
    r = subprocess.run([BIN, "convert", os.path.join(SSFS, f"{char}.ssf"), "-n", char,
                        "-o", outdir, "-y"],
                       capture_output=True, text=True, env=env)
    text = r.stderr + r.stdout
    totals = {m.group(1): int(m.group(2))
              for m in re.finditer(r"\[anim-labels\] (\S+) total=(\d+)", text)}
    out = {}
    for m in re.finditer(r"\[anim-split\] (\S+) <- (\S+) \[(\d+)\.\.(\d+|end)\)(.*)", text):
        fm, src, start, end, flags = m.groups()
        start = int(start)
        end = totals.get(src, start) if end == "end" else int(end)
        head = int(re.search(r"\+head(\d+)", flags).group(1)) if "+head" in flags else 0
        out[fm] = {"src": src, "frames": max(0, end - start) + head, "loop": " loop" in flags}
    return out, text


def source_active(char, outdir_unused, text):
    """SSF2 source ACTIVE hitbox frames per animation, from PEPTIDE_DUMP_BOX_FRAMES."""
    out = {}
    for m in re.finditer(r"\[box-frames\] (\S+) total=(\d+) active=(\d+) first=(\d+) last=(\d+)", text):
        out[m.group(1)] = {"active": int(m.group(3)), "first": int(m.group(4)), "last": int(m.group(5))}
    return out


def entity_active(char, outdir):
    """Emitted Fraymakers active HIT_BOX frames per animation.

    Same definition parity_check.py uses: a hitbox is active on a keyframe whose symbol is
    non-null. This is the OTHER half of frame data — animation lengths say how long a move
    lasts, these say which frames of it can actually hit (startup / active / recovery).
    """
    ents = glob.glob(os.path.join(outdir, char, "library", "entities", "*.entity"))
    ent = next((e for e in ents if os.path.basename(e).lower() == f"{char}.entity"), None)
    if not ent:
        return {}
    d = json.load(open(ent, encoding="utf8"))
    layers = {l["$id"]: l for l in d.get("layers", [])}
    kfs = {k["$id"]: k for k in d.get("keyframes", [])}
    out = {}
    for a in d.get("animations", []):
        total = 0
        for lid in a.get("layers", []):
            l = layers.get(lid)
            if not l or l.get("type") != "COLLISION_BOX":
                continue
            meta = l.get("pluginMetadata", {}).get("com.fraymakers.FraymakersMetadata", {})
            if meta.get("collisionBoxType") != "HIT_BOX":
                continue
            for kid in l.get("keyframes", []):
                k = kfs.get(kid, {})
                if k.get("symbol") is not None:
                    total += k.get("length", 1)
        out[a["name"]] = total
    return out


def entity_lengths(char, outdir):
    """Emitted Fraymakers animation lengths, from the IMAGE layer's keyframe spans."""
    ents = glob.glob(os.path.join(outdir, char, "library", "entities", "*.entity"))
    ent = next((e for e in ents if os.path.basename(e).lower() == f"{char}.entity"), None)
    if not ent:
        return {}
    d = json.load(open(ent, encoding="utf8"))
    layers = {l["$id"]: l for l in d["layers"]}
    kfs = {k["$id"]: k for k in d["keyframes"]}
    out = {}
    for a in d["animations"]:
        for lid in a["layers"]:
            l = layers.get(lid)
            if not l or l.get("type") != "IMAGE":
                continue
            out[a["name"]] = sum(kfs[k].get("length", 1) for k in l["keyframes"] if k in kfs)
            break
    return out


def check(char, verbose=False):
    outdir = os.path.join(ROOT, "build", "parity_timing")
    sp, text = splits(char, outdir)
    fm = entity_lengths(char, outdir)
    if not sp or not fm:
        print(f"{char}: no data (splits={len(sp)} fm={len(fm)})")
        return 0, 0

    checked = off = 0
    rows = []
    for name in sorted(fm):
        info = sp.get(name)
        if not info or info["frames"] == 0:
            continue  # emitted without a split record (or a zero-length slice) — nothing to compare
        got, want = fm[name], info["frames"] * 2
        checked += 1
        # A looping split's emitted length is one CYCLE and the engine repeats it, so an
        # exact match isn't required in the same way; still reported, just not counted.
        ok = abs(got - want) <= 1
        if not ok and not info["loop"]:
            off += 1
        if not ok:
            rows.append((name, info, got, want))

    print(f"\n=== {char}: {checked} animations checked, {off} off the 2.00x doubling ===")
    for name, info, got, want in rows:
        tag = " (loop, not counted)" if info["loop"] else ""
        ratio = got / info["frames"] if info["frames"] else 0
        print(f"  {name:26} src={info['src']}[{info['frames']}]  fm={got:4}  expected={want:4}  {ratio:.2f}x{tag}")

    # ── active hitbox frames (startup / active / recovery) ────────────────────
    src_act, fm_act = source_active(char, outdir, text), entity_active(char, outdir)
    b_checked = b_off = 0
    b_rows = []
    for name in sorted(src_act):
        if name not in fm_act:
            continue
        want_a, got_a = src_act[name]["active"] * 2, fm_act[name]
        b_checked += 1
        if abs(got_a - want_a) > 1:
            b_off += 1
            b_rows.append((name, src_act[name]["active"], got_a, want_a))
    print(f"    hitbox active frames: {b_checked} animations, {b_off} off 2.00x")
    for name, s_a, got_a, want_a in b_rows[:12]:
        r = got_a / s_a if s_a else 0
        print(f"      {name:24} ssf2_active={s_a:3}  fm_active={got_a:3}  expected={want_a:3}  {r:.2f}x")
    return checked, off + b_off


if __name__ == "__main__":
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        sys.exit(2)
    if args == ["--all"]:
        args = sorted(os.path.basename(p)[:-4] for p in glob.glob(os.path.join(SSFS, "*.ssf")))
    tot = bad = 0
    for c in args:
        t, b = check(c)
        tot += t
        bad += b
    print(f"\nTOTAL: {tot} animations checked, {bad} off the expected 2.00x doubling")
    sys.exit(1 if bad else 0)
