# how an SSF2 stage package is built

everything here was learned by authoring one from scratch and watching it fail, over and over, in
ways that named neither the file nor the mistake. peptide's fixture (`crates/ssf2-converter/src/
test_fixture.rs`) is the worked example: it is the only stage in existence built to these rules
rather than by the official tooling, so if a rule here is wrong, the fixture stops loading.

the short version: **a package is not a file the game reads, it is a file the game runs.** the
order things happen in is part of the format, and most of what follows is about ordering and about
what has to EXIST before something else asks for it.

## how to diagnose one

don't guess. the patched engine reports a failing package by file and line, with an explanation
where peptide knows one:

```
SCRIPTERR: StageData.as:409 <ctor>: TypeError: Error #1034
      a boundary is a CLIP, not a shape. wrap deathBoundary/camBoundary in their own sprites
```

that comes from `inject_error_locator` (`abc_inject.rs`), which wraps engine methods in a handler
per source line. the runtime shipped to players strips line information out of error objects, so
the location comes from the `debugline` markers left in the bytecode instead. each error is
reported ONCE, at the innermost frame that saw it, then rethrown untouched.

the explanations live in `PACKAGE_FAULT_HINTS`. when a new failure site is understood, add a line
there so the next person reads the answer instead of deriving it.

`inject_method_probe` is the other half: it reports every entry to a named method, with its source
location, so "how far did it get" is answerable. it is opt-in (`PEPTIDE_PROBE_LOAD`) because it is
noisy, not because it is risky.

## the file

| requirement | what goes wrong without it |
| --- | --- |
| the DAT container: zlib around `u32 swf_len`, `u32 index_count`, `N x u32` index, then the SWF | the game only ever opens this form. a bare SWF in its data directory is not a file it can read |
| `FileAttributes` with the ActionScript-3 flag, FIRST tag | the player runs the whole file as AVM1, ignores the code, and every class link binds to nothing |
| TWO frames | a load completes when the frame carrying the document class is built. one frame and the load never completes: no error, no timeout, a loading screen that spins and reports failure when the queue gives up |
| the code (`DoAbc2`) BEFORE the class links (`SymbolClass`) | a link names a class; a class not yet defined is not one it can bind to. the link is dropped silently |
| both in the FIRST frame | the root is built when its frame is shown. a link arriving on frame two arrives after the root has already been made an anonymous clip, and nothing later can change what it is |
| symbol 0 linked to `Main` | without it the loaded content is an anonymous clip rather than a package, and every question the game asks it comes back undefined |
| a class defined for EVERY link | the player throws while binding symbols. because that happens as the data directory is read, the whole boot dies rather than just this package |

## the code

a package carries its own copy of the small api layer it builds on, in the top level namespace.
the official tooling compiles that copy in. it is NOT optional and the game's own equivalent is not
visible to a loaded package, so naming that instead fails outright.

* **one class per script.** shared names already exist in the game, and a script initialiser that
  trips over an existing name stops there, taking every later class in that script with it.
* **`getscopeobject 0` before the class value** in each class-defining script. `initproperty`
  stores INTO something, so that something has to be under the value. leaving it out is a stack
  underflow and the package is refused whole.
* **late-bind every class reference** (`findpropstrict` + `getproperty`, not `getlex`). naming a
  class directly binds it at VERIFY time, before its own script has run, and the player rejects the
  entire method for illegal early binding rather than reporting a missing name.

### what `Main` must do

`Main extends` the package's asset base, and its constructor registers what the package is:
`id`, `guid`, `resources` (movieclip and sound linkage names), `music`, `stage` (the stage CLASS),
and `camera`. the reader in `abc_parser::extract_main_package_metadata` finds the id as the
bytecode pattern `pushstring "id"; pushstring VALUE`.

the asset base provides `register` / `getProp` (a property bag) plus `initAPI`, `deinitAPI` and
`getAPIVersion`. two of those carry weight:

* **`initAPI(api)` must RETURN the class carrying `BASE_CLASSES`.** the game calls it with its own
  api object and uses what comes back AS that class. returning nothing means the game reads the
  class map off null.
* **`BASE_CLASSES` is a MAP**, from every api class name the game knows to the package's own class
  object for each, built in a class initialiser because its values are class objects. the game
  reads entries straight out without checking first, so a missing name comes back undefined and is
  then used. the names are the game's, not yours.

the stage base is a **forwarder**: every method is one call through to the same name on the api
object the constructor was handed. a shipped package's `getBackground` is nine bytes.

the stage class itself is a thin shim on that base, constructed WITH one argument which it hands
to `super`. a zero-argument version is rejected.

## the clip tree

```
stageMC                     the whole stage, linked stage_<id>
  background                art behind the fighters
  terrain                   collision, boundaries and the spawn beacons
  foreground                art in front
<id>_BG                     a camera background, linked, at the ROOT
```

* every part must EXIST. the game walks this by name and does not check as it goes, so a missing
  layer surfaces as an undefined value inside the engine naming neither the part nor the package.
* **everything the game picks out of a stage it picks out as a CLIP.** a bare shape assigned where
  a clip is expected fails to coerce. wrap collision and boundary geometry in sprites of their own.
* **what a clip IS comes from its LINKAGE, not from the name it is placed under.** this is the
  single most useful thing to know about a terrain clip, and it holds across every shipped stage:
  * solid ground: linked `<something>_terrain_mc` (`venuslighthouse_terrain_mc`,
    `suzakucastle_terrain_mc`, `multimanbattlefield_TerrainMC`)
  * drop-through: linked `..._platform` (`terrainGround_platform_`, `devlounge_dynamicPlatform`)
  * ledges: `ledge_mc_left_` / `ledge_mc_right_`
  * spawns: `pN_Start_` / `pN_Spawn_`, capitalised
  * boundaries: all built from ONE `boundary_clip` linkage and told apart by the instance name
    they are placed under (`deathBoundary`, `camBoundary`, `smashBallBoundary`)

  so collision clips carry a linkage and NO instance name, and boundaries carry both. a clip named
  `terrainGround` on the placement but linked to nothing is not classified as anything.
* a collision clip contains exactly ONE unnamed, unlinked shape. that is all: the shape is the
  geometry and the linkage on the clip around it is the meaning. the fixture matches this now,
  shape for shape.
* the camera block's `backgrounds` list is indexed without being checked, so an empty list is a
  read off the end rather than "no parallax".

**Y grows DOWNWARD.** the floor carries the largest y of anything a fighter touches, beacons sit at
smaller y because they are above it, and a fall is y increasing. authored the other way round you
get a stage that looks right in a diagram and puts its ground above everyone's head.

## being found at all

the id to file map is an obfuscated manifest carried inside the game, so a package outside it is
never opened: no error, because nothing tried. `inject_extra_resource` sidesteps that. the game
already registers one package by hand ahead of the manifest-driven ones, so peptide prepends
another in that same shape at the menu entry point, touching no shipped file.

WHERE that runs is load-bearing. the resource table's own initialiser runs while the table is still
null, and the queue call is verified before the definitions it names have run; both of those homes
take the resource system down.

## known gap: collision

geometry and linkage are necessary and not sufficient. a fighter still falls through the fixture's
floor, and the reason is the api layer rather than the clips.

the package's api classes are a CHAIN, each level forwarding its own calls through to the game:

```
<package root>            holds the game's api object, constructed with it
  SSF2CollisionBoundary   getType getOwnStats getMC destroy getX getY
    SSF2Platform          getFallthrough getAccelFriction getXSpeed getStartPosition ...
  SSF2GameObject          the base for characters, items, enemies, projectiles
  SSF2Stage               getBackground getForeground getCameraBackgrounds getLedges ...
```

every one of them is constructed with one argument, and the GAME constructs them: it takes the
class out of the package's class map and builds it. a name present but empty is therefore an object
the game can build and cannot use, which is exactly the fixture's floor. no error is raised, since
nothing has gone wrong from the game's point of view.

no stage constructs its own platforms, and no platform class is linked to a clip, so
`BowsersCastlePlatform extends SSF2Platform` and friends are shims the game instantiates, the same
shape as a stage class. building that chain out is the remaining work, and what a shipped package does is now known
precisely. read off `SSF2CollisionBoundary` and `SSF2Platform` in any stage:

* `flags = 0x09`, ie SEALED and PROTECTEDNS, with a protected namespace named after the class
  (kind `0x18`). the fixture's classes are declared with no flags and no protected namespace.
* the constructor takes ONE argument and declares a return TYPE, not the any-type.
* `getType` is redeclared at every level of the chain, and every level marks it with the OVERRIDE
  attribute (trait kind `0x21` rather than `0x01`). that cuts both ways: redeclaring an inherited
  method without the attribute is refused, and marking one as an override when the base does not
  declare it is refused just as firmly. the package root declares `getType`, `initialize`,
  `update`, `isDisposed` and `dispose` for the levels below it to override.

three attempts have added this chain and all three stopped the package loading: four classes at
once, then only the boundary and the platform, then the same with the override attributes and the
root methods in place. each was reverted rather than left broken. probing narrows it usefully: the
package no longer COMPLETES loading, which is the signature of the file being rejected while its
code is verified rather than anything going wrong at runtime. so the remaining difference is in how
these classes are DECLARED, and the two candidates left unmatched are the class flags and the
protected namespace.
