#!/usr/bin/env python3
"""stage_live_check.py — verify a converted stage's elements are ALIVE and ANIMATING in Fraymakers.

The static inventory (stage_inventory.py) proves the right objects were EMITTED. This proves
they actually run: every backdrop element the converter spawns is located in the live match,
its animation clock is read, and it's sampled twice to tell a looping element from a stalled
one. Those are different failures with identical static output.

The join is by POSITION. Each `createVfx` line carries the element's stage coords, and the live
Vfx reports the same coords (the placement lives on the vfx, not baked into its art), so a live
object can be matched back to the element that spawned it. Nothing else identifies them —
Fraymakers leaves the display wrappers unnamed.

Reported per element:
  present   the element exists in the live match at its spawn position
  layer     the stage container it was reparented into (from the spawn line)
  frames    live total vs the emitted animation length -- a mismatch means the clock the
            engine is running isn't the one that was authored
  moving    the frame advanced between two samples: a LOOPING element vs a STALLED one

Usage:
  tools/tests/stage_live_check.py <stage>        # e.g. bowserscastle
"""
import json, os, re, subprocess, sys, time, glob

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
BIN = os.path.join(ROOT, "build", "release", "peptide")
LOG = "/tmp/stage_live_check.log"


def spawned_elements(sid):
    """Element id -> (x, y, layer) from the emitted stage script's spawn lines."""
    path = os.path.join(ROOT, "stages", sid, "library", "scripts", "stage", f"{sid}Script.hx")
    if not os.path.exists(path):
        sys.exit(f"no converted stage at {path} — run `peptide ssf2 stage <stage>.ssf` first")
    text = open(path, encoding="utf8").read()
    out = {}
    for var, ent, x, y in re.findall(
            r'var (_\w+) = match\.createVfx\(new VfxStats\(\{[^}]*?getContent\("([^"]+)"\)[^}]*?'
            r'x:\s*([-\d.]+),\s*y:\s*([-\d.]+)', text):
        m = re.search(re.escape(var) + r'\s*!=\s*null\s*\)\s*\{\s*self\.get(\w+?)Container\(\)', text)
        layer = re.sub(r'(?<!^)(?=[A-Z])', '_', m.group(1)).upper() if m else "?"
        out[ent] = (float(x), float(y), layer)
    return out


def emitted_frames(sid, eid):
    """The element's authored animation length (sum of keyframe lengths)."""
    p = os.path.join(ROOT, "stages", sid, "library", "entities", f"{eid}.entity")
    if not os.path.exists(p):
        return None
    d = json.load(open(p, encoding="utf8"))
    kfs = {k["$id"]: k for k in d.get("keyframes", [])}
    best = 0
    for a in d.get("animations", []):
        for lid in a["layers"]:
            layer = next((l for l in d["layers"] if l["$id"] == lid), None)
            if layer:
                best = max(best, sum(kfs[k].get("length", 1) for k in layer.get("keyframes", []) if k in kfs))
    return best or None


def tell(cmd):
    subprocess.run([BIN, "tell", cmd], capture_output=True, text=True)


def live_vfx(mark):
    """{(x,y): (frame, total)} for every live Vfx, from a `tree` after byte offset `mark`."""
    tell("tree")
    time.sleep(38)
    tail = open(LOG, encoding="utf8", errors="replace").read()[mark:]
    out = {}
    for x, y, f, t in re.findall(r"Vfx @\(([-\d.]+),([-\d.]+)\) frame=(\d+)/(\d+)", tail):
        out[(round(float(x), 1), round(float(y), 1))] = (int(f), int(t))
    return out, len(open(LOG, encoding="utf8", errors="replace").read())


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(__doc__)
        sys.exit(2)
    stage = sys.argv[1]
    sid = f"{stage}ssf2"

    elements = spawned_elements(sid)
    print(f"[{stage}] {len(elements)} elements spawned by the converted stage script", file=sys.stderr)

    subprocess.run(["pkill", "-f", "peptide session"], capture_output=True)
    time.sleep(2)
    subprocess.run(["rm", "-rf", os.path.expanduser("~/.peptide/session")], capture_output=True)
    f = open(LOG, "w")
    proc = subprocess.Popen([BIN, "session", "--char", "mario", "--stage", sid], stdout=f, stderr=f)
    for _ in range(50):
        time.sleep(3)
        if re.search(r"LAUNCHED|CRASH", open(LOG, encoding="utf8", errors="replace").read()):
            break
    tell("awaitmatch p0 90")
    time.sleep(3)

    try:
        mark = len(open(LOG, encoding="utf8", errors="replace").read())
        first, mark = live_vfx(mark)
        second, _ = live_vfx(mark)
    finally:
        tell("exit")
        time.sleep(2)
        proc.terminate()

    print(f"\n{'element':44}{'layer':22}{'frames':>13}  {'clock':>13}  verdict")
    print("-" * 118)
    bad = 0
    for eid, (x, y, layer) in sorted(elements.items()):
        key = (round(x, 1), round(y, 1))
        want = emitted_frames(sid, eid)
        a, b = first.get(key), second.get(key)
        if a is None:
            bad += 1
            verdict, clock, frames = "NOT IN THE LIVE MATCH", "-", f"{want or '?'}"
            print(f"{eid[:43]:44}{layer[:21]:22}{frames:>13}  {clock:>13}  {verdict}")
            continue
        frames = f"{a[1]} vs {want}" if want else str(a[1])
        clock = f"{a[0]}->{b[0] if b else '?'}/{a[1]}"
        notes = []
        if want and a[1] != want:
            notes.append(f"LENGTH MISMATCH (engine {a[1]}, authored {want})")
            bad += 1
        if b and a[0] == b[0] and a[1] > 1:
            notes.append("STALLED (frame did not advance)")
            bad += 1
        print(f"{eid[:43]:44}{layer[:21]:22}{frames:>13}  {clock:>13}  {'; '.join(notes) or 'ok'}")

    print("-" * 118)
    print(f"{len(elements) - bad}/{len(elements)} elements live, correct length, and advancing\n")
    sys.exit(1 if bad else 0)
