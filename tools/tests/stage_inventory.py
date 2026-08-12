#!/usr/bin/env python3
"""stage_inventory.py — map EVERY object on a stage, SSF2 side against Fraymakers side.

PORTING_STAGES phase 1 says "the goal: a complete list of every symbol, plane, actor,
bitmap, and clip before you form any opinion about the stage", and phase 4 says the live
tree "catches everything the file can't tell you". That procedure exists as a set of debug
dumps you read by eye. This turns the static half into a paired inventory you can diff, so
"did we port every object" stops being a judgement call.

SSF2 side  <- PEPTIDE_STAGE_TREE: every placed instance with depth, kind (MC/shape),
              instance name, symbol/linkage, resolved PLANE (the layer), and world position.
Fraymakers <- the emitted stage package: the main entity's layers (art, collision, spawn
              points) plus every separately-emitted entity (backdrop elements, hazards),
              each with its animation count and frame count.

Reported per object: name, plane/layer, position, and on the FM side the animation and
frame count it became.

READ "NO COUNTERPART" CAREFULLY. It does not mean "not ported". The stage's art is
RASTERIZED AND COMPOSITED into one sprite (STATUS "stage porting"), so a STATIC object is
correctly present as pixels in that sprite while having no named object of its own. What
actually matters is the ANIMATED objects: those need their own layer/entity to keep
playing, and one baked into the composite has silently lost its animation. So a
no-counterpart row is a QUESTION — "does this thing move?" — not a defect.

Objects that are never art (collision, masks, spawn/boundary scaffolding, container clips,
HUD chrome, hazard attackBoxes) are classified out; see AGENT_CONTEXT's "SSF2 stage linkage
vocabulary".

Observed on a 4-stage sample: gaps cluster hard by PLANE — 6 on `cambg` (kingdom1's
clouds / troopas / plant, i.e. the parallax elements) and 6 on `terrain`. A whole plane
going unrepresented is a much stronger signal than any single object, and is the thing to
chase first.

Usage:
  tools/tests/stage_inventory.py <stage>            # e.g. bowserscastle
  tools/tests/stage_inventory.py <stage> --all      # include unnamed shape leaves
"""
import json, os, re, subprocess, sys, glob, collections

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
SSFS = os.environ.get("SSF2_SSFS_DIR", os.path.join(ROOT, "..", "ssf2-ssfs"))
BIN = os.path.join(ROOT, "build", "release", "peptide")

# Linkage families that are deliberately NOT art (AGENT_CONTEXT "SSF2 stage linkage
# vocabulary"): collision geometry the engine renders invisibly, and scaffolding.
NON_ART = re.compile(
    r"terrain|platform|collison|collision|ledge_mc|_start_|_spawn_|boundary|"
    r"warningbounds|warning|itemgen|mask|light_source|smashball|"
    # wrappers and hazard collision, not art: `stageMC`/`stance` are container clips and
    # `attackBox*` becomes the hazard's HITBOX, not a drawn object.
    r"^stagemc$|^stance$|attackbox|hitbox|"
    # HUD furniture is chrome, not stage art; `weightPlat*`-style moving platforms are
    # kept as static COLLISION (see STATUS "auto-detected moving platforms"), not as a
    # drawn object, so their absence from the art inventory is correct.
    r"hud|weightplat",
    re.I)


def ssf2_objects(stage, include_unnamed=False):
    """Every placed instance from the SSF2 placement tree."""
    src = os.path.join(SSFS, "stages", f"{stage}.ssf")
    env = dict(os.environ, PEPTIDE_STAGE_TREE="1")
    r = subprocess.run([BIN, "ssf2", "stage", src, "--info"],
                       capture_output=True, text=True, env=env)
    out = []
    for m in re.finditer(
            r'^(\s*)d(\d+) (\S+) inst="([^"]*)" sym="([^"]*)" plane="([^"]*)" @\((-?\d+),(-?\d+)\)',
            r.stderr + r.stdout, re.M):
        indent, depth, kind, inst, sym, plane, x, y = m.groups()
        name = inst or sym
        if not name and not include_unnamed:
            continue
        out.append({"depth": int(depth), "kind": kind, "name": name, "inst": inst,
                    "sym": sym, "plane": plane, "x": int(x), "y": int(y),
                    "nest": len(indent) // 2})
    return out


def fm_objects(stage, outdir):
    """Every emitted Fraymakers object: main-entity layers + separate entities."""
    src = os.path.join(SSFS, "stages", f"{stage}.ssf")
    subprocess.run([BIN, "ssf2", "stage", src, "--out", outdir],
                   capture_output=True, text=True)
    sid = f"{stage}ssf2"
    base = os.path.join(outdir, sid, "library", "entities")
    objs = []
    main = os.path.join(base, f"{sid}.entity")
    if os.path.exists(main):
        d = json.load(open(main, encoding="utf8"))
        layers = {l["$id"]: l for l in d["layers"]}
        kfs = {k["$id"]: k for k in d["keyframes"]}
        for a in d["animations"]:
            for lid in a["layers"]:
                l = layers.get(lid)
                if not l:
                    continue
                frames = sum(kfs[k].get("length", 1) for k in l.get("keyframes", []) if k in kfs)
                objs.append({"where": "main", "anim": a["name"], "name": l.get("name") or "",
                             "type": l.get("type"), "frames": frames})
    for ent in sorted(glob.glob(os.path.join(base, "*.entity"))):
        if os.path.basename(ent) == f"{sid}.entity":
            continue
        d = json.load(open(ent, encoding="utf8"))
        layers = {l["$id"]: l for l in d["layers"]}
        kfs = {k["$id"]: k for k in d["keyframes"]}
        nm = os.path.basename(ent)[:-len(".entity")]
        frames = 0
        for a in d.get("animations", []):
            for lid in a["layers"]:
                l = layers.get(lid)
                if l and l.get("type") == "IMAGE":
                    frames = max(frames, sum(kfs[k].get("length", 1)
                                             for k in l.get("keyframes", []) if k in kfs))
        objs.append({"where": "entity", "anim": ",".join(a["name"] for a in d.get("animations", [])),
                     "name": nm, "type": "ENTITY", "frames": frames})
    return objs


def norm(s):
    """Loose key for pairing: the emitted names carry the stage id and separators vary."""
    return re.sub(r"[^a-z0-9]", "", s.lower())


if __name__ == "__main__":
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if not args:
        print(__doc__)
        sys.exit(2)
    stage = args[0]
    include_unnamed = "--all" in sys.argv

    ssf2 = ssf2_objects(stage, include_unnamed)
    fm = fm_objects(stage, os.path.join(ROOT, "build", "stage_inventory"))
    sid = norm(stage + "ssf2")

    # index the FM side by normalized name with the stage id stripped
    fm_index = collections.defaultdict(list)
    for o in fm:
        k = norm(o["name"]).replace(sid, "").replace(norm(stage), "")
        fm_index[k].append(o)

    named = [o for o in ssf2 if o["name"]]
    # one row per distinct SSF2 object name (the tree repeats a clip per frame placement)
    seen, rows = set(), []
    for o in named:
        if o["name"] in seen:
            continue
        seen.add(o["name"])
        rows.append(o)

    print(f"\n=== {stage}: {len(rows)} distinct SSF2 objects, {len(fm)} emitted Fraymakers objects ===\n")
    print(f"{'SSF2 object':34}{'plane':12}{'pos':>13}  {'->':3} {'Fraymakers':38}{'frames':>7}")
    print("-" * 112)
    ported = gaps = skipped = 0
    missing = []
    for o in rows:
        key = norm(o["name"]).replace(sid, "").replace(norm(stage), "")
        # match both directions: SSF2 `thwomp_mc` emits as `…Thwomp`, so neither name
        # contains the other in a fixed order once normalized.
        hits = fm_index.get(key) or [h for k, v in fm_index.items()
                                     if key and k and (key in k or k in key) for h in v]
        pos = f"({o['x']},{o['y']})"
        if hits:
            ported += 1
            h = hits[0]
            tag = f"{h['where']}:{h['name'][:30]}"
            print(f"{o['name'][:33]:34}{o['plane'][:11]:12}{pos:>13}  ->  {tag:38}{h['frames']:>7}")
        elif NON_ART.search(o["name"]):
            skipped += 1   # collision geometry / scaffolding: never art, correctly absent
        else:
            gaps += 1
            missing.append(o)
    print("-" * 112)
    print(f"ported={ported}  non-art (correctly absent)={skipped}  NO COUNTERPART={gaps}\n")
    if missing:
        print("SSF2 objects with no emitted counterpart:")
        for o in missing:
            print(f"  {o['name'][:40]:42}plane={o['plane'] or '?':12}@({o['x']},{o['y']})  {o['kind']}")
    sys.exit(1 if gaps else 0)
