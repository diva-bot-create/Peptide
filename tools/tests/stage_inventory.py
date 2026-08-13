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

BLIND SPOT, and it is a real one: this only sees TIMELINE animation, i.e. a clip with more
than one frame. An object animated by AS3 (weather systems, spawned actors) is a 1-frame
clip and reads here as correctly-baked static art. bowserscastle's `bowsers_embers_bg` is
exactly that — 1 frame, driven by an EmberWeather class, and a documented unported gap that
this check cannot see. The actor/class axis is PORTING_STAGES phase 2, not this inventory;
the two are complementary and neither alone is a clean bill of health.

Observed on a 4-stage sample: kingdom1 loses 4 animations, all on the `cambg` parallax
plane (clouds 500 frames, bouncy 360, lefttroopa/righttroopa 263 each). bowserscastle,
casinonightzone and battlefield lose none.

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


def convert_once(stage, outdir):
    """ONE converter run that both emits the package and dumps the placement tree.

    The first version ran the converter twice per stage (once with --info for the tree,
    once with --out to emit), which doubled a 110-stage sweep for nothing.
    """
    src = os.path.join(SSFS, "stages", f"{stage}.ssf")
    env = dict(os.environ, PEPTIDE_STAGE_TREE="1")
    r = subprocess.run([BIN, "ssf2", "stage", src, "--out", outdir],
                       capture_output=True, text=True, env=env)
    return r.stderr + r.stdout


def ssf2_objects(text, include_unnamed=False):
    """Every placed instance from the SSF2 placement tree."""
    r = None
    out = []
    for m in re.finditer(
            r'^(\s*)d(\d+) (\S+) inst="([^"]*)" sym="([^"]*)" plane="([^"]*)" '
            r'@\((-?\d+),(-?\d+)\) frames=(\d+)',
            text, re.M):
        indent, depth, kind, inst, sym, plane, x, y, frames = m.groups()
        name = inst or sym
        if not name and not include_unnamed:
            continue
        out.append({"depth": int(depth), "kind": kind, "name": name, "inst": inst,
                    "sym": sym, "plane": plane, "x": int(x), "y": int(y),
                    "frames": int(frames), "nest": len(indent) // 2})
    return out


def spawn_layers(stage, outdir):
    """`<entity id>` -> the stage container its spawn line reparents it into.

    A separately-emitted object (a backdrop vfx, a hazard) is placed by a line in the stage
    Script, not by anything in its own `.entity`, so its DEPTH only exists in that script.
    Fraymakers has eleven named containers; reading which one each object actually lands in
    is what makes a wrong plane visible instead of invisible.
    """
    sid = f"{stage}ssf2"
    script = os.path.join(outdir, sid, "library", "scripts", "stage", f"{sid}Script.hx")
    out = {}
    if not os.path.exists(script):
        return out
    text = open(script, encoding="utf8").read()
    # var _bg3 = match.createVfx(... getContent("<id>") ...);
    #   if (_bg3 != null) { self.getBackgroundBehindContainer().addChild(_bg3.getSprite()); }
    for var, ent in re.findall(r'var (_\w+) = match\.create\w+\([^;]*?getContent\("([^"]+)"\)', text):
        m = re.search(re.escape(var) + r'\s*!=\s*null\s*\)\s*\{\s*self\.get(\w+?)Container\(\)', text)
        if m:
            # getBackgroundBehindContainer -> BACKGROUND_BEHIND
            name = re.sub(r'(?<!^)(?=[A-Z])', '_', m.group(1)).upper()
            out[ent] = name
        else:
            out.setdefault(ent, "(game objects)")
    return out


def fm_objects(stage, outdir):
    """Every emitted Fraymakers object: main-entity layers + separate entities."""
    sid = f"{stage}ssf2"
    base = os.path.join(outdir, sid, "library", "entities")
    spawned = spawn_layers(stage, outdir)
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
                             "type": l.get("type"), "frames": frames,
                             "layer": "(baked in stack)"})
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
                     "name": nm, "type": "ENTITY", "frames": frames,
                     "layer": spawned.get(nm, "(not spawned)")})
    return objs


# SSF2 framework / Flash-generated classes are not stage content. `*_fla.*` are timeline
# symbols the placement tree already covers; the rest are engine plumbing.
FRAMEWORK = re.compile(r"^(SSF2|IState|EState|FrameTimer|ItemSettings|CState|PState|TState|"
                       r"ControlBits|Main|Vec2|Point|Rect)", re.I)


def actor_classes(stage):
    """Hand-written AS3 classes in the stage file — its behavioural objects.

    This is the blind spot the placement tree cannot cover: a weather system or spawned
    actor is a CLASS, not a placed multi-frame clip, so it reads as 1-frame static art (or
    doesn't appear at all). PORTING_STAGES phase 2 calls for exactly this inventory.
    """
    src = os.path.join(SSFS, "stages", f"{stage}.ssf")
    r = subprocess.run(["cargo", "run", "-q", "-p", "ssf2_converter", "--features", "dev-tools",
                        "--bin", "ssf2_objgraph", "--", src, "scripts"],
                       capture_output=True, text=True, cwd=ROOT)
    out = []
    for m in re.finditer(r"^  class  (\S+)", r.stdout + r.stderr, re.M):
        c = m.group(1)
        if "_fla." in c or FRAMEWORK.match(c) or c.lower() == stage.lower():
            continue
        if c.lower() == f"{stage}_bg".lower():
            continue
        # Hand-written ACTORS are PascalCase (BowsersCastleLava, EmberWeather, Thwomp).
        # snake_case entries (global_wind_wave, podoboo_jump, stage_bowserscastle) are
        # exported LIBRARY SYMBOLS — timeline content the placement tree already accounts
        # for, not behaviour classes. Listing them here reports the same object twice and
        # buries the one finding that matters.
        if not re.match(r"^[A-Z][A-Za-z0-9]*$", c):
            continue
        out.append(c)
    return sorted(set(out))


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

    outdir = os.path.join(ROOT, "build", "stage_inventory")
    text = convert_once(stage, outdir)
    ssf2 = ssf2_objects(text, include_unnamed)
    fm = fm_objects(stage, outdir)
    sid = norm(stage + "ssf2")

    # index the FM side by normalized name with the stage id stripped
    # Index by BOTH the layer name and the animation name: a main-entity object can be
    # identified by either (the platform sprites are animations named platformSprite0..3
    # whose layers are generically named), and indexing only one silently loses the match.
    fm_index = collections.defaultdict(list)
    for o in fm:
        for raw in {o["name"], o.get("anim", "")}:
            if not raw:
                continue
            k = norm(raw).replace(sid, "").replace(norm(stage), "")
            if k:
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
    print(f"{'SSF2 object':30}{'plane':11}{'pos':>13}  {'->':3} {'Fraymakers':34}{'frames':>7}  {'fm layer':<22}")
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
            fm_layer = h.get("layer", "")
            # flag a background-plane object that landed in a foreground container, or the
            # reverse: that's a depth error the counts alone can't show
            sp, fl = (o['plane'] or '').lower(), fm_layer.lower()
            mismatch = (("fore" in sp) != ("foreground" in fl)) and fl.startswith(("back", "fore", "char"))
            flag = "  <-- PLANE/LAYER MISMATCH" if mismatch else ""
            print(f"{o['name'][:29]:30}{o['plane'][:10]:11}{pos:>13}  ->  {tag:34}{h['frames']:>7}  {fm_layer:<22}{flag}")
        elif NON_ART.search(o["name"]):
            skipped += 1   # collision geometry / scaffolding: never art, correctly absent
        elif o["frames"] > 1:
            # ANIMATED and not emitted as its own object: baked into the composite sprite,
            # so it renders but no longer moves. This is the actual defect.
            gaps += 1
            missing.append(o)
        else:
            skipped += 1  # 1 frame: static art, correctly baked into the stage sprite
    print("-" * 112)
    print(f"ported={ported}  non-art or static-baked={skipped}  LOST ANIMATION={gaps}\n")

    # ── behavioural classes (the placement tree's blind spot) ─────────────────
    acts = actor_classes(stage)
    if acts:
        print(f"AS3 behaviour classes ({len(acts)}):")
        act_missing = []
        for c in acts:
            k = norm(c)
            hit = [h for kk, v in fm_index.items() if kk and (k in kk or kk in k) for h in v]
            # a *Platform class is emitted as the platformSprite* family, not by name
            if not hit and k.endswith("platform"):
                hit = [h for kk, v in fm_index.items() if "platformsprite" in kk for h in v]
            if hit:
                print(f"  {c[:38]:40}-> {hit[0]['where']}:{hit[0]['name'][:34]}")
            else:
                act_missing.append(c)
        for c in act_missing:
            print(f"  {c[:38]:40}NO COUNTERPART")
        print(f"  ({len(acts) - len(act_missing)} ported, {len(act_missing)} unported)\n")
        gaps += len(act_missing)
    if missing:
        print("ANIMATED SSF2 objects with no emitted counterpart (baked in, so they no longer move):")
        for o in sorted(missing, key=lambda o: -o["frames"]):
            print(f"  {o['name'][:38]:40}plane={o['plane'] or '?':10}frames={o['frames']:>5}  @({o['x']},{o['y']})")
    sys.exit(1 if gaps else 0)
