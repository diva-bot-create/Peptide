/// AVM2 bytecode decompiler — structured CFG reconstruction.
///
/// Algorithm:
///   Pass 1: scan bytecode → collect branch targets → split into BasicBlocks
///   Pass 2: reconstruct structured control flow (if/else/while)
///   Pass 3: emit Fraymakers Haxe with SSF2→FM API translation

use std::collections::{BTreeMap, BTreeSet};
use crate::abc_parser::AbcFile;

// ─── SSF2 → Fraymakers API map ────────────────────────────────────────────────

struct ApiEntry { fm: &'static str, comment: &'static str }

macro_rules! api {
    ($fm:expr) => { ApiEntry { fm: $fm, comment: "" } };
    ($fm:expr, $c:expr) => { ApiEntry { fm: $fm, comment: $c } };
}

fn lookup_api(name: &str) -> Option<ApiEntry> {
    Some(match name {
        // physics / movement
        "getX"              => api!("self.getX()"),
        "getY"              => api!("self.getY()"),
        "setX"              => api!("self.setX"),
        "setY"              => api!("self.setY"),
        "getXSpeed"         => api!("self.getXSpeed()"),
        "getYSpeed"         => api!("self.getYSpeed()"),
        // SSF2's setXSpeed is WORLD-space; Fraymakers' is FACING-RELATIVE. The names map 1:1
        // but the ARGUMENT does not: the call site wraps it in flipX so the direction survives
        // (see the rewrite below, and the measurement behind it). setYSpeed has no facing.
        // The SSF2 second boolean arg is dropped below.
        "setXSpeed"         => api!("self.setXSpeed"),
        "setYSpeed"         => api!("self.setYSpeed"),
        "getNetXSpeed"      => api!("self.getNetXVelocity()"),
        "getNetYSpeed"      => api!("self.getNetYVelocity()"),
        "setXSpeedScaled"   => api!("self.setXVelocityScaled"),
        "setYSpeedScaled"   => api!("self.setYVelocityScaled"),
        "faceLeft"          => api!("self.faceLeft()"),
        "faceRight"         => api!("self.faceRight()"),
        "flip"              => api!("self.flip()"),
        "flipX"             => api!("self.flipX"),
        "isFacingLeft"      => api!("self.isFacingLeft()"),
        "isFacingRight"     => api!("self.isFacingRight()"),
        "isOnGround" | "isOnFloor" => api!("self.isOnFloor()"),
        "resetMomentum"     => api!("self.resetMomentum()"),
        "toggleGravity"     => api!("self.toggleGravity"),
        "getKnockback"      => api!("self.getKnockback()"),
        "setKnockback"      => api!("self.setKnockback"),
        "move"              => api!("self.move"),
        "moveAbsolute"      => api!("self.moveAbsolute"),
        // state
        "getState"          => api!("self.getState()"),
        "setState"          => api!("self.setState", "// prefer self.toState(CState.X)"),
        "toState"           => api!("self.toState"),
        "inState"           => api!("self.inState"),
        "inStateGroup"      => api!("self.inStateGroup"),
        "getPreviousState"  => api!("self.getPreviousState()"),
        // animation
        "playAnimation"     => api!("self.playAnimation"),
        "playFrame"         => api!("self.playFrame"),
        "playFrameLabel"    => api!("self.playFrameLabel"),
        "getCurrentFrame"   => api!("self.getCurrentFrame()"),
        "getTotalFrames"    => api!("self.getTotalFrames()"),
        "finalFramePlayed"  => api!("self.finalFramePlayed()"),
        "getAnimation"      => api!("self.getAnimation()"),
        "hasAnimation"      => api!("self.hasAnimation"),
        "updateAnimationStats" => api!("self.updateAnimationStats"),
        "updateHitboxStats" => api!("self.updateHitboxStats"),
        // timers / events
        "addTimer"          => api!("self.addTimer"),
        "removeTimer"       => api!("self.removeTimer"),
        "addEventListener"  => api!("self.addEventListener"),
        "removeEventListener" => api!("self.removeEventListener"),
        // combat
        "getDamage"         => api!("self.getDamage()"),
        "setDamage"         => api!("self.setDamage"),
        "addDamage"         => api!("self.addDamage"),
        "getHitstop"        => api!("self.getHitstop()"),
        "getHitstun"        => api!("self.getHitstun()"),
        "startHitstop"      => api!("self.startHitstop"),
        "startHitstun"      => api!("self.startHitstun"),
        "refreshAttackID" | "reactivateHitboxes" => api!("self.reactivateHitboxes()"),
        "attemptHit"        => api!("self.attemptHit"),
        "attemptGrab"       => api!("self.attemptGrab"),
        "releaseCharacter"  => api!("self.releaseCharacter"),
        "releaseAllCharacters" => api!("self.releaseAllCharacters"),
        "getGrabbedFoe"     => api!("self.getGrabbedFoe()"),
        "getAllGrabbedFoes"  => api!("self.getAllGrabbedFoes()"),
        "getOwner"          => api!("self.getOwner()"),
        "setOwner"          => api!("self.setOwner"),
        // match objects
        "getPlayer"         => api!("match.getCharacter", "// TODO: adjust index"),
        "getPlayers"        => api!("match.getCharacters()"),
        "getProjectile"     => api!("match.getProjectile"),
        "getItem"           => api!("match.getItem"),
        "getStage"          => api!("match.getStage()"),
        "createProjectile"  => api!("match.createProjectile"),
        // audio
        "playSound"         => api!("AudioClip.play"),
        "stopSound"         => api!("AudioClip.stop"),
        // display
        "getTopLayer"       => api!("self.getTopLayer()"),
        "getBottomLayer"    => api!("self.getBottomLayer()"),
        "getViewRootContainer" => api!("self.getViewRootContainer()"),
        "getDamageCounterContainer" => api!("self.getDamageCounterContainer()"),
        // scale / rotation
        "getScaleX"         => api!("self.getScaleX()"),
        "getScaleY"         => api!("self.getScaleY()"),
        "setScaleX"         => api!("self.setScaleX"),
        "setScaleY"         => api!("self.setScaleY"),
        "getRotation"       => api!("self.getRotation()"),
        "setRotation"       => api!("self.setRotation"),
        "kill"              => api!("self.kill()"),
        "getResource"       => api!("self.getResource()"),
        "getFoes"           => api!("self.getFoes()"),
        // SSF2-specific with no direct FM equivalent
        "toFlying"          => api!("/* TODO: self.toState(CState.FALL_SPECIAL) */", "// SSF2: toFlying()"),
        "getClosestLedge"   => api!("/* TODO: no FM equivalent */", "// SSF2: getClosestLedge()"),
        "replaceAttackStats" => api!("self.updateAnimationStats", "// SSF2: replaceAttackStats"),
        "replaceAttackBoxStats" => api!("self.updateHitboxStats", "// TODO: replaceAttackBoxStats"),
        "resetRotation"     => api!("/* deactivate hitbox */", "// SSF2: resetRotation()"),
        "bringInFront"      => api!("/* TODO: self.getTopLayer().addChild(...) */", "// SSF2: bringInFront"),
        "bringBehind"       => api!("/* TODO: self.getBottomLayer().addChild(...) */", "// SSF2: bringBehind"),
        _ => return None,
    })
}

// ─── Expression AST ───────────────────────────────────────────────────────────

/// The value of an expression that is just a number, including a negated one. Used to tell an
/// authored constant (which is in SSF2 units and needs converting) from a computed value (which
/// has already come back from the engine in its own units).

/// Whether an argument came back OUT of the engine, and is therefore already in Fraymakers units.
/// Those must not be rescaled: `decel(getXSpeed(), …)` would shrink a little more each frame.
fn reads_engine_speed(expr: &str) -> bool {
    ["getXSpeed", "getYSpeed", "getNetXVelocity", "getNetYVelocity", "currentVelocity"]
        .iter()
        .any(|needle| expr.contains(needle))
}

fn literal_value(e: &Expr) -> Option<f64> {
    match e {
        Expr::Num(n) => Some(*n),
        Expr::UnOp("-", inner) => literal_value(inner).map(|v| -v),
        _ => None,
    }
}

/// Render a number the way the rest of the emitter does: whole values stay whole, so a rescaled
/// zero is still `0` rather than `0.0`.
fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 { format!("{}", v.round() as i64) } else { format!("{:.4}", v) }
}

#[derive(Debug, Clone)]
pub enum Expr {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
    This,
    Local(u32),
    GetProperty(Box<Expr>, String),
    Call(Box<Expr>, String, Vec<Expr>),
    New(String, Vec<Expr>),
    Array(Vec<Expr>),
    Object(Vec<(String, Expr)>),
    BinOp(&'static str, Box<Expr>, Box<Expr>),
    UnOp(&'static str, Box<Expr>),
    GetLex(String),
    Closure(Vec<String>, Vec<Stmt>),  // params + body stmts, rendered depth-aware
    Unknown,
}

impl Expr {
    /// Whether re-rendering this expression a second time would re-do something.
    ///
    /// Reading a local propagates whatever was assigned to it, which reads nicely for a constant
    /// and is a BUG for a call: the source assigns once and uses the local, and re-rendering the
    /// call at each use performs it again. clocktown assigns one flaming rock and then sets its x,
    /// its y, and pushes it -- inlined, that spawns four rocks and positions three of them.
    ///
    /// Only side-effect-free shapes may be propagated. A property read is included: it is a read,
    /// and keeping it inlined preserves the existing (widely relied upon) output for the ordinary
    /// `_v1 = self.foo; ... _v1 ...` case.
    fn is_propagatable(&self) -> bool {
        match self {
            Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Null | Expr::This
            | Expr::Local(_) | Expr::GetLex(_) | Expr::Unknown => true,
            Expr::GetProperty(o, _) => o.is_propagatable(),
            Expr::UnOp(_, e) => e.is_propagatable(),
            Expr::BinOp(_, a, b) => a.is_propagatable() && b.is_propagatable(),
            Expr::Array(v) => v.iter().all(Expr::is_propagatable),
            Expr::Object(v) => v.iter().all(|(_, e)| e.is_propagatable()),
            // a call, a constructor, or a closure body: performing it twice is not the same as
            // performing it once
            Expr::Call(..) | Expr::New(..) | Expr::Closure(..) => false,
        }
    }

    fn render(&self) -> String {
        match self {
            Expr::Num(v) => {
                if *v == v.round() && v.abs() < 1_000_000.0 { format!("{}", *v as i64) }
                else { format!("{:.4}", v).trim_end_matches('0').trim_end_matches('.').to_string() }
            }
            // Escape control chars too: a string carrying a literal newline/tab would
            // otherwise break across source lines (a Haxe string can't span lines), e.g.
            // `Engine.log("a:\nb:")` decompiled with a raw newline → unparseable.
            Expr::Str(s) => format!("\"{}\"", s
                .replace('\\', "\\\\").replace('"', "\\\"")
                .replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t")),
            Expr::Bool(b)   => b.to_string(),
            Expr::Null      => "null".to_string(),
            Expr::This      => "self".to_string(),
            Expr::Local(n)  => format!("_v{}", n),
            Expr::GetLex(n)              => n.clone(),
            Expr::Closure(params, stmts)  => render_closure(params, stmts, 0),
            // A decompiler-unreconstructable expression. Renders as a bare comment so
            // that in STATEMENT position (`/* ? */ foo();`) it's harmless whitespace.
            // In CONDITION position a bare comment is a parse error (`if (/* ? */)`) that
            // kills the whole script — `render_condition` substitutes `false` there.
            Expr::Unknown => "/* ? */".to_string(),
            Expr::GetProperty(obj, name) => {
                format!("{}.{}", obj.render(), name)
            }
            Expr::Call(obj, method, args) => {
                let mut rendered: Vec<String> = args.iter().map(|a| a.render()).collect();
                // Array index access
                if method == "[" {
                    return format!("{}[{}]", obj.render(), rendered.join(", "));
                }
                // Neutralize a non-function callback passed to a timer/event API. SSF2
                // tolerated a bad callback (silent no-op), but Fraymakers invokes it and
                // crashes — e.g. the `effects` instance var rendered as `effects.get()`
                // (an Array) passed to addTimer crashes IntervalTimer.process. The callback
                // must be a function reference (self.foo / foo / function(){}); anything else
                // is replaced with a no-op closure + a TODO.
                let cb_idx = match method.as_str() {
                    "addTimer" => Some(2),
                    "addEventListener" | "removeEventListener" => Some(1),
                    _ => None,
                };
                if let Some(i) = cb_idx {
                    if let Some(cb) = rendered.get_mut(i) {
                        if !is_callback_ref(cb) {
                            *cb = format!(
                                "function(){{}} /*TODO: SSF2 passed a non-function callback `{}` here; neutralized to avoid a runtime crash*/",
                                cb.replace("*/", "* /")
                            );
                        }
                    }
                }
                // SSF2's setXSpeed is WORLD-space and Fraymakers' is FACING-RELATIVE, so the
                // two are only the same while facing right. Measured in both engines rather than
                // assumed: facing LEFT, SSF2's setXSpeed(15) moves the character RIGHT (+147),
                // and Fraymakers' moves it LEFT (-305). `flipX` converts world to facing-relative
                // -- setXSpeed(flipX(15)) facing left moves RIGHT, matching SSF2 -- so the wrap
                // is what makes a move go the way it does in the original. Without it every
                // converted move with sideways momentum reverses when the character faces left.
                //
                // setYSpeed needs no wrap: there is no facing on the vertical axis.
                match method.as_str() {
                    "setXSpeed" | "setYSpeed" => {
                        rendered.truncate(1);
                        // RESCALE the value: a speed written into a move is a per-frame distance,
                        // and neither the frame nor the distance means the same thing on the other
                        // side -- SSF2 steps 30 times a second in a world the conversion makes
                        // `size_multiplier` bigger.
                        //
                        // A value the ENGINE handed back is already in Fraymakers units, so
                        // scaling it would shrink it again on every frame it is applied
                        // (`decel(getXSpeed(), …)` reads a speed back). Everything else is a
                        // number the source author wrote, whether it arrived as a literal or
                        // through a variable holding one -- sandbag's aerial down special dashes
                        // at `leftSpeed`/`rightSpeed`, set to -15 and 15 in its constructor, and
                        // scaling only literals left that move running at the raw SSF2 speed.
                        let k = crate::mappings::character_stats().scaling.velocity_scale();
                        if let Some(v) = args.first().and_then(literal_value) {
                            rendered[0] = fmt_num(v * k);
                        } else if !reads_engine_speed(&rendered[0]) {
                            rendered[0] = format!("{} * {}", rendered[0], fmt_num(k));
                        }
                        // Zero needs no orientation: a stop is a stop whichever way you face,
                        // and wrapping it only makes the emitted script harder to read.
                        let is_zero = args.first().and_then(literal_value).is_some_and(|v| v == 0.0);
                        if method == "setXSpeed" && !is_zero {
                            rendered[0] = format!("self.flipX({})", rendered[0]);
                        }
                    }
                    _ => {}
                }
                let arg_str = rendered.join(", ");
                // Check if obj is self and method is an API call
                let obj_str = obj.render();
                if obj_str == "self" || obj_str == "this" {
                    if let Some(entry) = lookup_api(method) {
                        let fm = entry.fm;
                        let comment = entry.comment;
                        let suffix = if !comment.is_empty() { format!(" {}", comment) } else { String::new() };
                        // No-arg methods stored as "self.xxx()"
                        if fm.ends_with("()") {
                            return format!("{}{}", fm, suffix);
                        }
                        return format!("{}({}){}", fm, arg_str, suffix);
                    }
                }
                format!("{}.{}({})", obj_str, method, arg_str)
            }
            Expr::New(cls, args) => {
                let arg_str = args.iter().map(|a| a.render()).collect::<Vec<_>>().join(", ");
                format!("new {}({})", cls, arg_str)
            }
            Expr::Array(items) => {
                format!("[{}]", items.iter().map(|i| i.render()).collect::<Vec<_>>().join(", "))
            }
            Expr::Object(pairs) => {
                if pairs.is_empty() { return "{}".to_string(); }
                let items = pairs.iter()
                    .map(|(k, v)| format!("{}: {}", k, v.render()))
                    .collect::<Vec<_>>().join(", ");
                format!("{{ {} }}", items)
            }
            Expr::BinOp(op, l, r) => format!("{} {} {}", l.render(), op, r.render()),
            Expr::UnOp("!", e) => match e.as_ref() {
                // Simplify !(a == b) → a != b, !(a != b) → a == b
                Expr::BinOp("==", l, r) => format!("{} != {}", l.render(), r.render()),
                Expr::BinOp("!=", l, r) => format!("{} == {}", l.render(), r.render()),
                Expr::BinOp("<",  l, r) => format!("{} >= {}", l.render(), r.render()),
                Expr::BinOp(">",  l, r) => format!("{} <= {}", l.render(), r.render()),
                Expr::BinOp(">=", l, r) => format!("{} < {}",  l.render(), r.render()),
                Expr::BinOp("<=", l, r) => format!("{} > {}",  l.render(), r.render()),
                _ => format!("!{}", e.render()),
            },
            Expr::UnOp(op, e)     => format!("{}{}", op, e.render()),
        }
    }

    #[allow(dead_code)]
    fn is_self(&self) -> bool {
        matches!(self, Expr::This)
            || matches!(self, Expr::GetLex(n) if n == "this")
    }
}

// ─── Statement AST ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum Stmt {
    VarDecl(u32, Expr),
    NamedAssign(String, Expr), // named variable assignment (for activation slots)
    SetProp(Expr, String, Expr),
    Expr(Expr),
    Return(Option<Expr>),
    If(Expr, Vec<Stmt>, Vec<Stmt>),
    While(Expr, Vec<Stmt>),
    Comment(String),
}

/// Is a rendered expression usable as a function callback (for addTimer /
/// addEventListener)? Accepts a function reference (`foo`, `self.foo`, `self.a.b`)
/// or an inline closure (`function(...){...}`). Rejects anything that evaluates to
/// a VALUE — a method call (`effects.get()`), array/object literal, number/string,
/// or the `null`/`true`/`false` keywords — which would crash when invoked.
fn is_callback_ref(s: &str) -> bool {
    let s = s.trim();
    if s.starts_with("function(") || s.starts_with("function (") {
        return true;
    }
    if s.is_empty() || s == "null" || s == "true" || s == "false" {
        return false;
    }
    // dotted identifier path only — no call `()`, index `[]`, or literal punctuation.
    let mut chars = s.chars();
    let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Build an `If`, normalizing the AVM2 "skip-the-body" branch shape. The compiler
/// emits the NEGATION of the source condition with an empty then-branch and the
/// body in the else (`if (x != 0) {} else { body }`). Detect that shape and emit
/// the readable, correct-direction form `if (x == 0) { body }` by negating the
/// condition and dropping the empty else. `Expr::render` already folds
/// `!(a <cmp> b)` into the flipped comparison, so this stays clean. No-op when the
/// then-branch is non-empty (a normal if / if-else).
fn make_if(cond: Expr, then_b: Vec<Stmt>, else_b: Vec<Stmt>) -> Stmt {
    if then_b.is_empty() && !else_b.is_empty() {
        Stmt::If(Expr::UnOp("!", Box::new(cond)), else_b, vec![])
    } else {
        Stmt::If(cond, then_b, else_b)
    }
}

/// Collect every AVM2 register written by a `Stmt::VarDecl` in this body (recursing into
/// if/while branches, but NOT into nested closures — those have their own scope). Used to emit
/// `var` declarations: the decompiler renders register writes as bare `_vN = …`, which hscript
/// rejects (a local must be `var`-declared; an undeclared assignment is a runtime error or an
/// accidental shared global). Declaring once at the function top makes a register written inside
/// a branch function-scoped (matching AVM2 register semantics).
fn collect_local_writes(stmts: &[Stmt], out: &mut std::collections::BTreeSet<u32>) {
    for s in stmts {
        match s {
            Stmt::VarDecl(n, v) => {
                if !(*n == 0 && matches!(v, Expr::This)) { out.insert(*n); }
            }
            Stmt::If(_, t, e) => { collect_local_writes(t, out); collect_local_writes(e, out); }
            Stmt::While(_, b) => collect_local_writes(b, out),
            _ => {}
        }
    }
}

/// `var _vN = null;` lines for the register temps a body writes. Skips locals 1..=param_count
/// (those render as `argN` — already function parameters). Local 0 is normally `self` and never
/// collected, but a rebound local 0 (`setlocal_0` of a non-`this` value) is collected and gets a
/// `var _v0` so its `_v0 = …` write and reads compile.
fn render_local_decls(stmts: &[Stmt], param_count: usize, depth: usize) -> String {
    let mut writes = std::collections::BTreeSet::new();
    collect_local_writes(stmts, &mut writes);
    let tab = "\t".repeat(depth);
    let mut out = String::new();
    for &n in &writes {
        if (1..=param_count).contains(&(n as usize)) { continue; }
        out.push_str(&format!("{}var _v{} = null;\n", tab, n));
    }
    out
}

fn render_closure(params: &[String], stmts: &[Stmt], depth: usize) -> String {
    let param_str = params.join(", ");
    if stmts.is_empty() {
        return format!("function({}) {{}}", param_str);
    }
    // Single return expression: inline
    if stmts.len() == 1 {
        if let Stmt::Return(Some(e)) = &stmts[0] {
            return format!("function({}) {{ return {}; }}", param_str, e.render());
        }
    }
    let tab = "\t".repeat(depth);
    let decls = render_local_decls(stmts, params.len(), depth + 1);
    let body = render_stmts(stmts, depth + 1);
    format!("function({}) {{\n{}{}{}}}", param_str, decls, body, tab)
}

fn render_stmts(stmts: &[Stmt], depth: usize) -> String {
    let mut out = String::new();
    let tab = "\t".repeat(depth);
    for s in stmts {
        match s {
            Stmt::NamedAssign(name, v) => {
                let val_str = if let Expr::Closure(params, stmts) = v {
                    render_closure(params, stmts, depth)
                } else { v.render() };
                out.push_str(&format!("{}var {} = {};\n", tab, name, val_str));
            }
            Stmt::Comment(c)   => out.push_str(&format!("{}// {}\n", tab, c)),
            Stmt::Return(None) => out.push_str(&format!("{}return;\n", tab)),
            Stmt::Return(Some(e)) => out.push_str(&format!("{}return {};\n", tab, e.render())),
            Stmt::VarDecl(n, v) => {
                // Local 0 is AVM2 `this`. If the value being assigned IS
                // `Expr::This`, the assignment is a no-op (the `this = this`
                // setup line emitted by some compilers) — drop it.
                if *n == 0 && matches!(v, Expr::This) { continue; }
                // Local 0 reassignment with a non-`this` value: SSF2
                // bytecode occasionally rebinds local_0, but in Fraymakers
                // Haxe `self` is final and assigning to it doesn't compile.
                // Render it under a synthetic name (`_v0`) instead so the
                // statement is at least a syntactically valid local — the
                // user can choose to use it or delete it.
                let var_name = format!("_v{}", n);
                let val_str = if let Expr::Closure(params, stmts) = v {
                    render_closure(params, stmts, depth)
                } else { v.render() };
                out.push_str(&format!("{}{} = {};\n", tab, var_name, val_str));
            }
            Stmt::SetProp(obj, name, val) => {
                out.push_str(&format!("{}{}.{} = {};\n", tab, obj.render(), name, val.render()));
            }
            Stmt::Expr(e) => out.push_str(&format!("{}{};\n", tab, e.render())),
            Stmt::If(cond, then_b, else_b) => {
                out.push_str(&format!("{}if ({}) {{\n", tab, render_condition(cond)));
                out.push_str(&render_stmts(then_b, depth + 1));
                if !else_b.is_empty() {
                    out.push_str(&format!("{}}} else {{\n", tab));
                    out.push_str(&render_stmts(else_b, depth + 1));
                }
                out.push_str(&format!("{}}}\n", tab));
            }
            Stmt::While(cond, body) => {
                out.push_str(&format!("{}while ({}) {{\n", tab, render_condition(cond)));
                out.push_str(&render_stmts(body, depth + 1));
                out.push_str(&format!("{}}}\n", tab));
            }
        }
    }
    out
}

/// Render an `if`/`while` condition, guaranteeing it's a parse-valid expression.
/// A decompiler-unreconstructable sub-expression renders as a `/* ? */` comment, which
/// is fine in statement position but a fatal parse error in CONDITION position (a comment
/// can't be an operand — `if (/* ? */)`, `if (x && /* ? */)`, `if (f(/* ? */))` all break,
/// and an hscript parse error kills the ENTIRE character script). When the rendered
/// condition contains such a marker, substitute a falsy placeholder so that one block goes
/// dead while the rest of the script still loads and runs.
fn render_condition(cond: &Expr) -> String {
    let r = cond.render();
    if r.contains("/* ? */") {
        "false /* ? */".to_string()
    } else {
        r
    }
}

// ─── Opcode constants ─────────────────────────────────────────────────────────

/// A chain longer than this is not a chain; the walk stops rather than looping on malformed input.
const MAX_CHAIN_LINKS: usize = 32;
/// A chain is at least two terms. (The older pair-shaped handling also claims this case, but gets
/// the BODY wrong on a plain two-term `&&`: it emits the right condition with an empty body and
/// leaves the real statements behind an unrecoverable second `if`.)
const MIN_CHAIN_TERMS: usize = 2;

const OP_NOP: u8         = 0x02;
const OP_THROW: u8       = 0x03;
const OP_KILL: u8        = 0x08;
const OP_LABEL: u8       = 0x09;
// The NEGATED comparison branches. AS3 emits these for the common
// `if (a < b) { … }` shape: the branch jumps PAST the then-block when the source
// condition is false, so `ifnlt` means "take the branch when !(a < b)". They carry
// the same 3-byte signed offset as their positive twins — miss one and every
// instruction boundary after it is wrong, which drops the rest of the method.
// Rendered with the plain inverted operator (ifnlt -> `>=`); AVM2 keeps them
// separate only for NaN, which decompiled character code never relies on.
const OP_IFNLT: u8       = 0x0C;
const OP_IFNLE: u8       = 0x0D;
const OP_IFNGT: u8       = 0x0E;
const OP_IFNGE: u8       = 0x0F;
const OP_JUMP: u8        = 0x10;
const OP_IFTRUE: u8      = 0x11;
const OP_IFFALSE: u8     = 0x12;
const OP_IFEQ: u8        = 0x13;
const OP_IFNE: u8        = 0x14;
const OP_IFLT: u8        = 0x15;
const OP_IFLE: u8        = 0x16;
const OP_IFGT: u8        = 0x17;
const OP_IFGE: u8        = 0x18;
const OP_IFSTRICTEQ: u8  = 0x19;
const OP_IFSTRICTNE: u8  = 0x1A;
const OP_PUSHNULL: u8    = 0x20;
const OP_PUSHBYTE: u8    = 0x24;
const OP_PUSHSHORT: u8   = 0x25;
const OP_PUSHTRUE: u8    = 0x26;
const OP_PUSHFALSE: u8   = 0x27;
const OP_PUSHNAN: u8     = 0x28;
const OP_POP: u8         = 0x29;
const OP_DUP: u8         = 0x2A;
const OP_SWAP: u8        = 0x2B;
const OP_PUSHSTRING: u8  = 0x2C;
const OP_PUSHINT: u8     = 0x2D;
const OP_PUSHUINT: u8    = 0x2E;
const OP_PUSHDOUBLE: u8  = 0x2F;
const OP_PUSHSCOPE: u8   = 0x30;
const OP_NEWFUNCTION: u8 = 0x40;
const OP_CALL: u8        = 0x41;
const OP_CONSTRUCT: u8   = 0x42;
const OP_CALLMETHOD: u8  = 0x43;
const OP_CALLSTATIC: u8  = 0x44;
const OP_CALLSUPER: u8   = 0x45;
const OP_CALLPROPERTY: u8 = 0x46;
const OP_RETURNVOID: u8  = 0x47;
const OP_RETURNVALUE: u8 = 0x48;
const OP_CONSTRUCTSUPER: u8 = 0x49;
const OP_CONSTRUCTPROP: u8 = 0x4A;
const OP_CALLPROPLEX: u8 = 0x4C;
const OP_CALLPROPVOID: u8 = 0x4F;
const OP_NEWOBJECT: u8   = 0x55;
const OP_NEWARRAY: u8    = 0x56;
const OP_NEWACTIVATION: u8 = 0x57;
const OP_NEWCLASS: u8    = 0x58;
const OP_GETDESCENDANTS: u8 = 0x59;
const OP_NEWCATCH: u8    = 0x5A;
const OP_FINDPROP: u8    = 0x5C;
const OP_FINDPROPSTRICT: u8 = 0x5D;
const OP_FINDDEF: u8     = 0x5E;
const OP_GETLEX: u8      = 0x60;
const OP_SETPROPERTY: u8 = 0x61;
const OP_GETLOCAL: u8    = 0x62;
const OP_SETLOCAL: u8    = 0x63;
const OP_GETGLOBALSCOPE: u8 = 0x64;
const OP_GETSCOPEOBJECT: u8 = 0x65;
const OP_GETPROPERTY: u8 = 0x66;
const OP_GETOUTERSCOPE: u8 = 0x67;
const OP_INITPROPERTY: u8 = 0x68;
const OP_DELETEPROPERTY: u8 = 0x6A;
const OP_GETSLOT: u8     = 0x6C;
const OP_SETSLOT: u8     = 0x6D;
const OP_GETGLOBALSLOT: u8 = 0x6E;
const OP_SETGLOBALSLOT: u8 = 0x6F;
const OP_CONVERT_S: u8   = 0x70;
const OP_ESC_XELEM: u8   = 0x71;
const OP_ESC_XATTR: u8   = 0x72;
const OP_CONVERT_I: u8   = 0x73; // yes, 0x73 not 0x83 — check spec
const OP_CONVERT_U: u8   = 0x74;
const OP_CONVERT_D: u8   = 0x75;
const OP_CONVERT_B: u8   = 0x76;
const OP_CONVERT_O: u8   = 0x77;
const OP_CHECKFILTER: u8 = 0x78;
const OP_COERCE: u8      = 0x80;
const OP_COERCE_B: u8    = 0x81;
const OP_COERCE_A: u8    = 0x82;
const OP_COERCE_I: u8    = 0x83;
const OP_COERCE_D: u8    = 0x84;
const OP_COERCE_S: u8    = 0x85;
const OP_ASTYPE: u8      = 0x86;
const OP_ASTYPELATE: u8  = 0x87;
const OP_COERCE_U: u8    = 0x88;
const OP_COERCE_O: u8    = 0x89;
const OP_NEGATE: u8      = 0x90;
const OP_INCREMENT: u8   = 0x91;
const OP_INCLOCAL: u8    = 0x92;
const OP_DECREMENT: u8   = 0x93;
const OP_DECLOCAL: u8    = 0x94;
const OP_TYPEOF: u8      = 0x95;
const OP_NOT: u8         = 0x96;
const OP_BITNOT: u8      = 0x97;
const OP_ADD: u8         = 0xA0;
const OP_SUBTRACT: u8    = 0xA1;
const OP_MULTIPLY: u8    = 0xA2;
const OP_DIVIDE: u8      = 0xA3;
const OP_MODULO: u8      = 0xA4;
const OP_LSHIFT: u8      = 0xA5;
const OP_RSHIFT: u8      = 0xA6;
const OP_URSHIFT: u8     = 0xA7;
const OP_BITAND: u8      = 0xA8;
const OP_BITOR: u8       = 0xA9;
const OP_BITXOR: u8      = 0xAA;
const OP_EQUALS: u8      = 0xAB;
const OP_STRICTEQUALS: u8 = 0xAC;
const OP_LESSTHAN: u8    = 0xAD;
const OP_LESSEQUALS: u8  = 0xAE;
const OP_GREATERTHAN: u8 = 0xAF;
const OP_GREATEREQUALS: u8 = 0xB0;
const OP_INSTANCEOF: u8  = 0xB1;
const OP_ISTYPE: u8      = 0xB2;
const OP_ISTYPELATE: u8  = 0xB3;
const OP_IN: u8          = 0xB4;
const OP_INCREMENT_I: u8 = 0xC0;
const OP_DECREMENT_I: u8 = 0xC1;
const OP_INCLOCAL_I: u8  = 0xC2;
const OP_DECLOCAL_I: u8  = 0xC3;
const OP_NEGATE_I: u8    = 0xC4;
const OP_ADD_I: u8       = 0xC5;
const OP_SUBTRACT_I: u8  = 0xC6;
const OP_MULTIPLY_I: u8  = 0xC7;
const OP_GETLOCAL0: u8   = 0xD0;
const OP_GETLOCAL1: u8   = 0xD1;
const OP_GETLOCAL2: u8   = 0xD2;
const OP_GETLOCAL3: u8   = 0xD3;
const OP_SETLOCAL0: u8   = 0xD4;
const OP_SETLOCAL1: u8   = 0xD5;
const OP_SETLOCAL2: u8   = 0xD6;
const OP_SETLOCAL3: u8   = 0xD7;
const OP_DEBUG: u8       = 0xEF;
const OP_DEBUGLINE: u8   = 0xF0;
const OP_DEBUGFILE: u8   = 0xF1;
const OP_BKPTLINE: u8    = 0xF2;

// ─── Pass 1: measure instruction sizes to find branch targets ─────────────────

/// The comparison a two-operand branch tests, as the Haxe operator to render. `None`
/// for a non-comparison opcode (including iftrue/iffalse, which take one operand).
///
/// The `ifn*` half is the NEGATED form AS3 emits for the ordinary `if (a < b) { … }`
/// shape: the branch jumps PAST the then-block when the source condition is false, so
/// `ifnlt` is taken when `!(a < b)` and renders as `>=`. AVM2 keeps the two families
/// separate only to preserve NaN behaviour, which decompiled character code never
/// relies on, so the plain inversion is the faithful rendering.
fn cmp_branch_op(op: u8) -> Option<&'static str> {
    Some(match op {
        OP_IFEQ | OP_IFSTRICTEQ => "==",
        OP_IFNE | OP_IFSTRICTNE => "!=",
        OP_IFLT | OP_IFNGE       => "<",
        OP_IFLE | OP_IFNGT       => "<=",
        OP_IFGT | OP_IFNLE       => ">",
        OP_IFGE | OP_IFNLT       => ">=",
        _ => return None,
    })
}

fn instr_size(bc: &[u8], pos: usize) -> usize {
    if pos >= bc.len() { return 1; }
    let op = bc[pos];
    match op {
        // no operands
        OP_NOP | OP_THROW | OP_LABEL | OP_PUSHNULL | OP_PUSHTRUE | OP_PUSHFALSE | OP_PUSHNAN
        | OP_POP | OP_DUP | OP_SWAP | OP_PUSHSCOPE | OP_NEWACTIVATION | OP_GETGLOBALSCOPE
        | OP_RETURNVOID | OP_RETURNVALUE | OP_NEGATE | OP_INCREMENT | OP_DECREMENT | OP_TYPEOF
        | OP_NOT | OP_BITNOT | OP_ADD | OP_SUBTRACT | OP_MULTIPLY | OP_DIVIDE | OP_MODULO
        | OP_LSHIFT | OP_RSHIFT | OP_URSHIFT | OP_BITAND | OP_BITOR | OP_BITXOR
        | OP_EQUALS | OP_STRICTEQUALS | OP_LESSTHAN | OP_LESSEQUALS | OP_GREATERTHAN | OP_GREATEREQUALS
        | OP_INSTANCEOF | OP_ISTYPELATE | OP_IN | OP_COERCE_A | OP_COERCE_B | OP_COERCE_D
        | OP_COERCE_I | OP_COERCE_S | OP_COERCE_U | OP_COERCE_O | OP_ASTYPELATE
        | OP_INCREMENT_I | OP_DECREMENT_I | OP_NEGATE_I | OP_ADD_I | OP_SUBTRACT_I | OP_MULTIPLY_I
        | OP_CONVERT_S | OP_CONVERT_I | OP_CONVERT_U | OP_CONVERT_D | OP_CONVERT_B | OP_CONVERT_O
        | OP_CHECKFILTER | OP_ESC_XELEM | OP_ESC_XATTR
        | 0xD0 | 0xD1 | 0xD2 | 0xD3 | 0xD4 | 0xD5 | 0xD6 | 0xD7  // getlocal_0..3, setlocal_0..3
            => 1,
        // 1-byte operand
        OP_PUSHBYTE | OP_KILL | OP_GETSCOPEOBJECT | OP_GETOUTERSCOPE | OP_NEWCATCH
            => 2,
        // 3-byte signed offset (branch instructions)
        OP_JUMP | OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFLT | OP_IFLE | OP_IFGT | OP_IFGE
        | OP_IFNLT | OP_IFNLE | OP_IFNGT | OP_IFNGE | OP_IFSTRICTEQ | OP_IFSTRICTNE
            => 4,
        // variable-length u30 operand(s)
        OP_PUSHSTRING | OP_PUSHINT | OP_PUSHUINT | OP_PUSHDOUBLE | OP_PUSHSHORT
        | OP_GETLEX | OP_FINDPROP | OP_FINDPROPSTRICT | OP_FINDDEF
        | OP_GETPROPERTY | OP_SETPROPERTY | OP_INITPROPERTY | OP_DELETEPROPERTY
        | OP_GETLOCAL | OP_SETLOCAL | OP_GETGLOBALSLOT | OP_SETGLOBALSLOT
        | OP_GETSLOT | OP_SETSLOT | OP_GETDESCENDANTS
        | OP_COERCE | OP_ASTYPE | OP_ISTYPE
        | OP_NEWFUNCTION | OP_NEWCLASS
        | OP_INCLOCAL | OP_DECLOCAL | OP_INCLOCAL_I | OP_DECLOCAL_I
        | OP_DEBUGLINE | OP_DEBUGFILE | OP_BKPTLINE
            => 1 + u30_len(bc, pos + 1),
        // one u30 operand (not in the multi-u30 group)
        OP_NEWOBJECT | OP_NEWARRAY
            => 1 + u30_len(bc, pos + 1),
        // single u30 (argc only)
        OP_CONSTRUCT | OP_CONSTRUCTSUPER
            => 1 + u30_len(bc, pos + 1),
        // two u30 operands (multiname idx + argc)
        OP_CALLPROPERTY | OP_CALLPROPVOID | OP_CALLPROPLEX | OP_CONSTRUCTPROP
        | OP_CALLMETHOD | OP_CALLSTATIC | OP_CALLSUPER | OP_CALL
            => 1 + u30_len(bc, pos + 1) + u30_len(bc, pos + 1 + u30_len(bc, pos + 1)),
        // Debug: 1 byte + u30 + u30 + u30
        OP_DEBUG => {
            let a = u30_len(bc, pos + 2);
            let b = u30_len(bc, pos + 2 + a);
            let c = u30_len(bc, pos + 2 + a + b);
            2 + a + b + c
        }
        _ => 1, // conservative fallback
    }
}

fn u30_len(bc: &[u8], mut pos: usize) -> usize {
    let start = pos;
    while pos < bc.len() {
        let b = bc[pos]; pos += 1;
        if b & 0x80 == 0 { break; }
        if pos - start >= 5 { break; }
    }
    pos - start
}

fn read_u30_at(bc: &[u8], pos: &mut usize) -> u32 {
    let mut r = 0u32; let mut shift = 0;
    while *pos < bc.len() {
        let b = bc[*pos] as u32; *pos += 1;
        r |= (b & 0x7F) << shift; shift += 7;
        if b & 0x80 == 0 { break; }
    }
    r
}

fn read_s24_at(bc: &[u8], pos: &mut usize) -> i32 {
    if *pos + 3 > bc.len() { *pos = bc.len(); return 0; }
    let v = (bc[*pos] as i32) | ((bc[*pos+1] as i32) << 8) | ((bc[*pos+2] as i32) << 16);
    *pos += 3;
    if v & 0x800000 != 0 { v | -0x1000000 } else { v }
}

// ─── Pass 1: build basic blocks ───────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Block {
    start: usize,
    end: usize,       // exclusive byte offset
    term: Terminator,
}

#[derive(Debug, Clone)]
enum Terminator {
    Return,
    Jump(usize),      // absolute target offset
    Branch { cond_inv: bool, target: usize, fallthrough: usize },
    // branch with two-value compare (pops 2 from stack)
    BranchCmp { op: &'static str, target: usize, fallthrough: usize },
    Throw,
    Fall(usize),      // just falls to next instruction
}

fn build_blocks(bc: &[u8]) -> Vec<Block> {
    // Collect all block-start offsets
    let mut starts: BTreeSet<usize> = BTreeSet::new();
    starts.insert(0);

    let mut pos = 0;
    while pos < bc.len() {
        let op = bc[pos];
        let sz = instr_size(bc, pos);
        match op {
            OP_JUMP | OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFLT | OP_IFLE | OP_IFGT | OP_IFGE
        | OP_IFNLT | OP_IFNLE | OP_IFNGT | OP_IFNGE | OP_IFSTRICTEQ | OP_IFSTRICTNE => {
                let mut p = pos + 1;
                let offset = read_s24_at(bc, &mut p);
                let after_branch = pos + sz;
                let target = (after_branch as i64 + offset as i64) as usize;
                starts.insert(after_branch);
                if target < bc.len() { starts.insert(target); }
            }
            OP_RETURNVOID | OP_RETURNVALUE | OP_THROW => {
                starts.insert(pos + sz);
            }
            _ => {}
        }
        pos += sz;
    }

    // Build blocks
    let starts_vec: Vec<usize> = starts.into_iter().collect();
    let mut blocks = Vec::new();

    for (idx, &bstart) in starts_vec.iter().enumerate() {
        if bstart >= bc.len() { break; }
        let bend = starts_vec.get(idx + 1).copied().unwrap_or(bc.len()).min(bc.len());

        // Find terminator of this block (last instruction)
        let mut term = Terminator::Fall(bend);
        let mut p = bstart;
        while p < bend {
            let op = bc[p];
            let sz = instr_size(bc, p);
            let next = p + sz;
            match op {
                OP_RETURNVOID | OP_RETURNVALUE => { term = Terminator::Return; }
                OP_THROW => { term = Terminator::Throw; }
                OP_JUMP => {
                    let mut q = p + 1;
                    let off = read_s24_at(bc, &mut q);
                    let target = (next as i64 + off as i64) as usize;
                    term = Terminator::Jump(target);
                }
                OP_IFTRUE => {
                    let mut q = p + 1;
                    let off = read_s24_at(bc, &mut q);
                    let target = (next as i64 + off as i64) as usize;
                    term = Terminator::Branch { cond_inv: false, target, fallthrough: next };
                }
                OP_IFFALSE => {
                    let mut q = p + 1;
                    let off = read_s24_at(bc, &mut q);
                    let target = (next as i64 + off as i64) as usize;
                    term = Terminator::Branch { cond_inv: true, target, fallthrough: next };
                }
                _ if cmp_branch_op(op).is_some() => {
                    let mut q = p + 1; let off = read_s24_at(bc, &mut q);
                    let target = (next as i64 + off as i64) as usize;
                    term = Terminator::BranchCmp { op: cmp_branch_op(op).unwrap(), target, fallthrough: next };
                }
                _ => {}
            }
            p = next;
        }

        blocks.push(Block { start: bstart, end: bend, term });
    }

    blocks
}

// ─── Pass 2: decode each block to a list of Stmts + a final Expr for branches ─

struct BlockDecoder<'a> {
    bc: &'a [u8],
    abc: &'a AbcFile,
    stack: Vec<Expr>,
    stmts: Vec<Stmt>,
    locals: BTreeMap<u32, Option<Expr>>,
    activation_slots: BTreeMap<u32, String>,
    param_locals: BTreeMap<u32, String>,
    has_activation: bool,
    /// Next synthetic register for a materialised stack value (see `OP_DUP`). Numbered far above
    /// any real register so it cannot collide with the method's own locals.
    next_temp: u32,
}

/// Where synthetic registers start. AS3 methods do not come close to this many locals.
const TEMP_REG_BASE: u32 = 9000;

impl<'a> BlockDecoder<'a> {
    fn new(bc: &'a [u8], abc: &'a AbcFile) -> Self {
        let mut locals = BTreeMap::new();
        locals.insert(0, Some(Expr::This));
        Self { bc, abc, stack: Vec::new(), stmts: Vec::new(), locals,
               activation_slots: BTreeMap::new(), param_locals: BTreeMap::new(),
               has_activation: false, next_temp: TEMP_REG_BASE }
    }

    fn pop(&mut self) -> Expr {
        self.stack.pop().unwrap_or(Expr::Unknown)
    }

    fn string(&self, idx: u32) -> String {
        self.abc.strings.get(idx as usize).map(|s| s.to_string()).unwrap_or_default()
    }

    fn multiname(&self, idx: u32) -> String {
        self.abc.multinames.get(idx as usize).map(|m| m.name.to_string()).unwrap_or_default()
    }

    fn get_local(&self, n: u32) -> Expr {
        if n == 0 {
            // Register 0 is the implicit `this`. Most bodies never write it, so
            // reads render as `self`. But a frame script can `setlocal_0` with a
            // non-`this` value (rebinding `this`-as-local). When that's happened,
            // reads must follow the rebind to the synthetic `_v0` local — that
            // write is rendered as `_v0 = …` and declared `var _v0`, so reading
            // `self` here would silently drop the rebound value and read the
            // wrong thing. Until a rebind, `self.locals` has no entry for 0.
            return match self.locals.get(&0) {
                Some(Some(v)) if !matches!(v, Expr::This) => Expr::Local(0),
                _ => Expr::This,
            };
        }
        // Check if it's a named param
        if let Some(name) = self.param_locals.get(&n) {
            return Expr::GetLex(name.clone());
        }
        // Propagate the assigned value only when doing so cannot repeat an effect; otherwise
        // read the variable the assignment already wrote (see `Expr::is_propagatable`).
        match self.locals.get(&n).and_then(|v| v.clone()) {
            Some(v) if v.is_propagatable() => v,
            _ => Expr::Local(n),
        }
    }

    fn set_local(&mut self, n: u32, v: Expr) {
        // Skip activation object assignments (newactivation, dup, setlocal N pattern)
        if matches!(&v, Expr::GetLex(s) if s == "_act") { return; }
        self.locals.insert(n, Some(v.clone()));
        self.stmts.push(Stmt::VarDecl(n, v));
    }

    fn decode_range(&mut self, start: usize, end: usize) -> Option<Expr> {
        let mut pos = start;
        while pos < end && pos < self.bc.len() {
            let op = self.bc[pos];
            pos += 1;
            match op {
                OP_NOP | OP_LABEL | OP_GETGLOBALSCOPE => {}
                OP_PUSHSCOPE => { self.stack.pop(); }
                OP_NEWACTIVATION => {
                    // Creates a closure activation object for capturing variables.
                    // We push a sentinel so subsequent dup/pushscope work correctly.
                    self.has_activation = true;
                    self.stack.push(Expr::GetLex("_act".into()));
                }
                OP_DEBUGLINE | OP_DEBUGFILE | OP_BKPTLINE => { read_u30_at(self.bc, &mut pos); }
                OP_DEBUG => {
                    pos += 1; // first byte
                    read_u30_at(self.bc, &mut pos);
                    read_u30_at(self.bc, &mut pos);
                    read_u30_at(self.bc, &mut pos);
                }
                OP_GETSCOPEOBJECT => {
                    let n = self.bc[pos]; pos += 1;
                    // When we have an activation, getscopeobject 1 = the activation record
                    // We don't push it explicitly — setslot/getslot will be resolved by name
                    // Push _act as a transparent marker that setslot/getslot will consume
                    if self.has_activation && n >= 1 {
                        self.stack.push(Expr::GetLex("_act".into()));
                    } else {
                        self.stack.push(Expr::This); // scope 0 = global/this
                    }
                }
                OP_GETOUTERSCOPE | OP_NEWCATCH => { pos += 1; self.stack.push(Expr::Unknown); }
                OP_KILL => { let _n = pos; pos += 1; }

                OP_PUSHSTRING => { let idx = read_u30_at(self.bc, &mut pos); self.stack.push(Expr::Str(self.string(idx))); }
                OP_PUSHDOUBLE => { let idx = read_u30_at(self.bc, &mut pos); let v = self.abc.doubles.get(idx as usize).copied().unwrap_or(0.0); self.stack.push(Expr::Num(v)); }
                OP_PUSHBYTE   => { let v = self.bc[pos] as i8 as f64; pos += 1; self.stack.push(Expr::Num(v)); }
                OP_PUSHSHORT  => { let v = read_u30_at(self.bc, &mut pos) as i16 as f64; self.stack.push(Expr::Num(v)); }
                OP_PUSHINT    => { let idx = read_u30_at(self.bc, &mut pos); let v = self.abc.ints.get(idx as usize).copied().unwrap_or(0) as f64; self.stack.push(Expr::Num(v)); }
                OP_PUSHUINT   => { let idx = read_u30_at(self.bc, &mut pos); let v = self.abc.uints.get(idx as usize).copied().unwrap_or(0) as f64; self.stack.push(Expr::Num(v)); }
                OP_PUSHTRUE   => self.stack.push(Expr::Bool(true)),
                OP_PUSHFALSE  => self.stack.push(Expr::Bool(false)),
                OP_PUSHNULL | OP_PUSHNAN => self.stack.push(Expr::Null),
                0x21 => self.stack.push(Expr::Null), // OP_PUSHUNDEFINED (haXe/tamarin extension)

                OP_GETLEX => { let idx = read_u30_at(self.bc, &mut pos); self.stack.push(Expr::GetLex(self.multiname(idx))); }
                OP_FINDPROPSTRICT | OP_FINDPROP | OP_FINDDEF => {
                    let idx = read_u30_at(self.bc, &mut pos);
                    let name = self.multiname(idx);
                    // findpropstrict pushes the scope object that owns `name`.
                    // For self-methods, that scope object IS self — mark with a special sentinel
                    // so callproperty on it renders as self.name().
                    self.stack.push(Expr::GetProperty(Box::new(Expr::This), name));
                }

                OP_GETLOCAL0 => self.stack.push(self.get_local(0)),
                OP_GETLOCAL1 => self.stack.push(self.get_local(1)),
                OP_GETLOCAL2 => self.stack.push(self.get_local(2)),
                OP_GETLOCAL3 => self.stack.push(self.get_local(3)),
                OP_GETLOCAL  => { let n = read_u30_at(self.bc, &mut pos); self.stack.push(self.get_local(n)); }

                OP_SETLOCAL0 => { let v = self.pop(); self.set_local(0, v); }
                OP_SETLOCAL1 => { let v = self.pop(); self.set_local(1, v); }
                OP_SETLOCAL2 => { let v = self.pop(); self.set_local(2, v); }
                OP_SETLOCAL3 => { let v = self.pop(); self.set_local(3, v); }
                OP_SETLOCAL  => { let n = read_u30_at(self.bc, &mut pos); let v = self.pop(); self.set_local(n, v); }

                OP_INCLOCAL | OP_INCLOCAL_I => { let n = read_u30_at(self.bc, &mut pos); let cur = self.get_local(n); self.stmts.push(Stmt::VarDecl(n, Expr::BinOp("+", Box::new(cur), Box::new(Expr::Num(1.0))))); }
                OP_DECLOCAL | OP_DECLOCAL_I => { let n = read_u30_at(self.bc, &mut pos); let cur = self.get_local(n); self.stmts.push(Stmt::VarDecl(n, Expr::BinOp("-", Box::new(cur), Box::new(Expr::Num(1.0))))); }

                OP_GETPROPERTY => {
                    let idx = read_u30_at(self.bc, &mut pos);
                    let name = self.multiname(idx);
                    let obj = self.pop();
                    if name.is_empty() {
                        // MultinameL (runtime name): stack before = [..., receiver, name]
                        // We popped name first (obj=name above), now pop receiver.
                        let receiver = self.pop();
                        // Emit as receiver[name]
                        self.stack.push(Expr::Call(
                            Box::new(receiver),
                            "[".into(),
                            vec![obj]  // obj is actually the index/name here
                        ));
                    } else {
                        self.stack.push(Expr::GetProperty(Box::new(obj), name));
                    }
                }
                OP_SETPROPERTY | OP_INITPROPERTY => {
                    let idx = read_u30_at(self.bc, &mut pos);
                    let name = self.multiname(idx);
                    let val = self.pop(); let obj = self.pop();
                    self.stmts.push(Stmt::SetProp(obj, name, val));
                }
                OP_DELETEPROPERTY => { read_u30_at(self.bc, &mut pos); self.pop(); self.stack.push(Expr::Bool(true)); }
                OP_GETSLOT => {
                    let slot = read_u30_at(self.bc, &mut pos);
                    let obj = self.pop();
                    // If reading from activation object, map to named variable
                    let slot_name = self.activation_slots.get(&slot)
                        .cloned()
                        .unwrap_or_else(|| format!("_s{}", slot));
                    let is_act = matches!(&obj, Expr::GetLex(n) if n == "_act");
                    if is_act {
                        self.stack.push(Expr::GetLex(slot_name));
                    } else {
                        self.stack.push(Expr::GetProperty(Box::new(obj), slot_name));
                    }
                }
                OP_SETSLOT => {
                    let slot = read_u30_at(self.bc, &mut pos);
                    let val = self.pop();
                    let obj = self.pop();
                    let slot_name = self.activation_slots.get(&slot)
                        .cloned()
                        .unwrap_or_else(|| format!("_s{}", slot));
                    let is_act = matches!(&obj, Expr::GetLex(n) if n == "_act")
                        || matches!(&obj, Expr::This);
                    if is_act {
                        let is_trivial = matches!(&val, Expr::Null)
                            || matches!(&val, Expr::GetLex(n) if n == "_act" || n.starts_with("_scope"))
                            // skip "var x = x" (param captured into same-named slot)
                            || matches!(&val, Expr::GetLex(vn) if *vn == slot_name);
                        if !is_trivial {
                            self.stmts.push(Stmt::NamedAssign(slot_name.clone(), val));
                        }
                        self.activation_slots.insert(slot, slot_name);
                    } else {
                        self.stmts.push(Stmt::SetProp(obj, slot_name, val));
                    }
                }

                OP_CALLPROPERTY | OP_CALLPROPLEX => {
                    let mn_idx = read_u30_at(self.bc, &mut pos);
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let name = self.multiname(mn_idx);
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    let obj = self.pop();
                    // Collapse findpropstrict + callproperty with same name → self.name()
                    let obj = collapse_findprop(obj, &name);
                    self.stack.push(Expr::Call(Box::new(obj), name, args));
                }
                OP_CALLPROPVOID => {
                    let mn_idx = read_u30_at(self.bc, &mut pos);
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let name = self.multiname(mn_idx);
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    let obj = self.pop();
                    let obj = collapse_findprop(obj, &name);
                    let call = Expr::Call(Box::new(obj), name, args);
                    self.stmts.push(Stmt::Expr(call));
                }
                OP_CALL => {
                    let _mn_idx = read_u30_at(self.bc, &mut pos); // always 0 for generic call
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    let _recv = self.pop();
                    let func = self.pop();
                    self.stack.push(Expr::Call(Box::new(func), "".into(), args));
                }
                OP_CALLMETHOD | OP_CALLSTATIC => {
                    let _idx = read_u30_at(self.bc, &mut pos);
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    let obj = self.pop();
                    self.stack.push(Expr::Call(Box::new(obj), "/* method */".into(), args));
                }
                OP_CALLSUPER => {
                    let mn_idx = read_u30_at(self.bc, &mut pos);
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let name = self.multiname(mn_idx);
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    self.pop(); // receiver
                    self.stmts.push(Stmt::Expr(Expr::Call(Box::new(Expr::GetLex("super".into())), name, args)));
                }
                OP_CONSTRUCTPROP => {
                    let mn_idx = read_u30_at(self.bc, &mut pos);
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let name = self.multiname(mn_idx);
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    self.pop();
                    self.stack.push(Expr::New(name, args));
                }
                OP_CONSTRUCT => {
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    let cls = self.pop();
                    self.stack.push(Expr::Call(Box::new(cls), "new".into(), args));
                }
                OP_CONSTRUCTSUPER => {
                    let argc = read_u30_at(self.bc, &mut pos) as usize;
                    let mut args: Vec<Expr> = (0..argc.min(self.stack.len())).map(|_| self.pop()).collect();
                    args.reverse();
                    self.pop();
                    self.stmts.push(Stmt::Comment(format!("super({})", args.iter().map(|a| a.render()).collect::<Vec<_>>().join(", "))));
                }

                OP_NEWOBJECT => {
                    // newobject pops `count` key/value PAIRS off the stack — count can
                    // never exceed half the current stack depth in valid bytecode. A
                    // mis-parsed range (CFG-reconstruction edge case on complex methods,
                    // e.g. chibirobo/dedede) can read garbage as a near-max-u30 count and
                    // allocate ~hundreds of GB. Clamp to the available pairs.
                    let raw = read_u30_at(self.bc, &mut pos) as usize;
                    let count = raw.min(self.stack.len() / 2);
                    let mut pairs = Vec::new();
                    let mut items: Vec<Expr> = (0..count*2).map(|_| self.pop()).collect();
                    items.reverse();
                    for chunk in items.chunks(2) {
                        if let (Expr::Str(k), v) = (&chunk[0], chunk[1].clone()) {
                            pairs.push((k.clone(), v));
                        }
                    }
                    self.stack.push(Expr::Object(pairs));
                }
                OP_NEWARRAY => {
                    // newarray pops `count` items — bounded by the stack depth in valid
                    // bytecode; clamp so a mis-parsed huge count can't OOM (see OP_NEWOBJECT).
                    let raw = read_u30_at(self.bc, &mut pos) as usize;
                    let count = raw.min(self.stack.len());
                    let mut items: Vec<Expr> = (0..count).map(|_| self.pop()).collect();
                    items.reverse();
                    self.stack.push(Expr::Array(items));
                }
                OP_NEWFUNCTION => {
                    let fn_idx = read_u30_at(self.bc, &mut pos);
                    // Find the method body and decompile it inline
                    let closure_expr = decompile_closure(fn_idx, self.abc);
                    self.stack.push(closure_expr);
                }
                OP_NEWCLASS    => { read_u30_at(self.bc, &mut pos); self.stack.push(Expr::GetLex("/* class */".into())); }

                OP_COERCE | OP_ASTYPE | OP_ISTYPE => { read_u30_at(self.bc, &mut pos); }
                OP_COERCE_A | OP_COERCE_B | OP_COERCE_I | OP_COERCE_D | OP_COERCE_S | OP_COERCE_U | OP_COERCE_O => {}
                // `value as Type` (type from the stack). The coercion is a no-op
                // for valid code (value already IS the type), so keep the value —
                // matches how OP_ASTYPE (type as operand) is handled above and
                // turns `/* ? : as-cast */` into the real underlying expression.
                OP_ASTYPELATE  => { self.pop(); /* drop the type, keep the value */ }
                // convert_b is a boolean cast — keep value on stack unchanged
                OP_CONVERT_B => {}
                OP_CONVERT_S | OP_CONVERT_I | OP_CONVERT_U | OP_CONVERT_D | OP_CONVERT_O | OP_CHECKFILTER => {}
                OP_ESC_XELEM | OP_ESC_XATTR => {}
                OP_TYPEOF => { let e = self.pop(); self.stack.push(Expr::Call(Box::new(Expr::GetLex("typeof".into())), "".into(), vec![e])); }

                OP_POP  => {
                    let e = self.pop();
                    // Skip bare variable refs (dup residues) and unknown
                    let skip = matches!(&e, Expr::Unknown | Expr::This | Expr::Null | Expr::Bool(_))
                        || matches!(&e, Expr::GetLex(_) | Expr::GetProperty(_, _) | Expr::Local(_));
                    if !skip { self.stmts.push(Stmt::Expr(e)); }
                }
                OP_DUP  => {
                    let top = self.stack.last().cloned().unwrap_or(Expr::Unknown);
                    // `dup` copies a stack SLOT, not the work that produced it. Copying the
                    // expression is fine while it only reads, and wrong the moment it does
                    // something: `_v6 = spawn(); _v6.setX(...)` compiles to a call, a dup, a store
                    // and a use, so duplicating the call expression spawns the thing twice.
                    // Materialise it once and let both copies refer to that.
                    // ...but ONLY for the assign-then-use shape, where the very next thing is a
                    // store. `dup` also serves the "test it, and the other branch pops the spare
                    // copy" pattern, and there the copies are consumed by branch handling that
                    // never sees a statement list of ours -- materialising into it drops the
                    // assignment on the floor and leaves reads of a variable nobody wrote.
                    let next_is_store = matches!(self.bc.get(pos),
                        Some(&OP_SETLOCAL) | Some(&0xD4) | Some(&0xD5) | Some(&0xD6) | Some(&0xD7));
                    if top.is_propagatable() || !next_is_store {
                        self.stack.push(top);
                    } else {
                        let tmp = self.next_temp;
                        self.next_temp += 1;
                        self.stmts.push(Stmt::VarDecl(tmp, top));
                        let last = self.stack.len() - 1;
                        self.stack[last] = Expr::Local(tmp);
                        self.stack.push(Expr::Local(tmp));
                    }
                }
                OP_SWAP => { let len = self.stack.len(); if len >= 2 { self.stack.swap(len-1, len-2); } }

                OP_NEGATE | OP_NEGATE_I => { let e = self.pop(); self.stack.push(Expr::UnOp("-", Box::new(e))); }
                OP_NOT => { let e = self.pop(); self.stack.push(Expr::UnOp("!", Box::new(e))); }
                OP_BITNOT => { let e = self.pop(); self.stack.push(Expr::UnOp("~", Box::new(e))); }
                OP_INCREMENT | OP_INCREMENT_I => { let e = self.pop(); self.stack.push(Expr::BinOp("+", Box::new(e), Box::new(Expr::Num(1.0)))); }
                OP_DECREMENT | OP_DECREMENT_I => { let e = self.pop(); self.stack.push(Expr::BinOp("-", Box::new(e), Box::new(Expr::Num(1.0)))); }

                OP_ADD | OP_ADD_I => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("+", Box::new(l), Box::new(r))); }
                OP_SUBTRACT | OP_SUBTRACT_I => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("-", Box::new(l), Box::new(r))); }
                OP_MULTIPLY | OP_MULTIPLY_I => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("*", Box::new(l), Box::new(r))); }
                OP_DIVIDE  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("/", Box::new(l), Box::new(r))); }
                OP_MODULO  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("%", Box::new(l), Box::new(r))); }
                OP_LSHIFT  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("<<", Box::new(l), Box::new(r))); }
                OP_RSHIFT  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp(">>", Box::new(l), Box::new(r))); }
                OP_URSHIFT => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp(">>>", Box::new(l), Box::new(r))); }
                OP_BITAND  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("&", Box::new(l), Box::new(r))); }
                OP_BITOR   => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("|", Box::new(l), Box::new(r))); }
                OP_BITXOR  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("^", Box::new(l), Box::new(r))); }
                OP_EQUALS | OP_STRICTEQUALS => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("==", Box::new(l), Box::new(r))); }
                OP_LESSTHAN    => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("<",  Box::new(l), Box::new(r))); }
                OP_LESSEQUALS  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("<=", Box::new(l), Box::new(r))); }
                OP_GREATERTHAN => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp(">",  Box::new(l), Box::new(r))); }
                OP_GREATEREQUALS => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp(">=", Box::new(l), Box::new(r))); }
                OP_INSTANCEOF  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("instanceof", Box::new(l), Box::new(r))); }
                OP_ISTYPELATE  => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("is", Box::new(l), Box::new(r))); }
                OP_IN          => { let r = self.pop(); let l = self.pop(); self.stack.push(Expr::BinOp("in", Box::new(l), Box::new(r))); }
                OP_GETDESCENDANTS => { read_u30_at(self.bc, &mut pos); self.pop(); self.stack.push(Expr::Unknown); }
                OP_THROW => { let e = self.pop(); self.stmts.push(Stmt::Comment(format!("throw {}", e.render()))); }
                OP_RETURNVOID  => { self.stmts.push(Stmt::Return(None)); return None; }
                OP_RETURNVALUE => { let v = self.pop(); self.stmts.push(Stmt::Return(Some(v))); return None; }

                // Branch instructions — return the raw condition expression
                // and stop decoding this block. The branch target is resolved
                // by the structured decoder (which keys on the pre-computed
                // Terminator), so we don't need to consume the s24 offset here.
                // The Branch { cond_inv } flag controls the then/else swap in
                // StructuredDecoder; for iffalse: cond_inv=true means the
                // condition is inverted — we DON'T negate here.
                OP_IFTRUE | OP_IFFALSE | OP_IFEQ | OP_IFNE | OP_IFSTRICTEQ | OP_IFSTRICTNE
                | OP_IFLT | OP_IFLE | OP_IFGT | OP_IFGE
                | OP_IFNLT | OP_IFNLE | OP_IFNGT | OP_IFNGE => {
                    let cond = match cmp_branch_op(op) {
                        Some(cmp) => { let r = self.pop(); let l = self.pop(); Expr::BinOp(cmp, Box::new(l), Box::new(r)) }
                        // iftrue/iffalse test the single value on the stack; the
                        // Branch { cond_inv } flag handles the iffalse negation.
                        None => self.pop(),
                    };
                    return Some(cond);
                }
                // OP_JUMP: structured decoder resolves the branch target
                // from the Terminator; no need to consume the offset here.
                OP_JUMP => return None,

                _ => {
                    // unknown — try to skip operands using instr_size
                    // (pos already advanced by 1 above)
                }
            }
            if self.stack.len() > 64 { self.stack.drain(0..32); }
        }
        // Don't drain — carry propagates via out_carry in decode_from_with_stack_out
        None
    }
}

// ─── Pass 3: structured CFG reconstruction ────────────────────────────────────

struct StructuredDecoder<'a> {
    blocks: Vec<Block>,
    bc: &'a [u8],
    abc: &'a AbcFile,
    visited: BTreeSet<usize>,
    activation_slots: BTreeMap<u32, String>,
    param_locals: BTreeMap<u32, String>, // local_idx -> param name
}

impl<'a> StructuredDecoder<'a> {
    #[allow(dead_code)]
    fn new(bc: &'a [u8], abc: &'a AbcFile) -> Self {
        let blocks = build_blocks(bc);
        Self { blocks, bc, abc, visited: BTreeSet::new(),
               activation_slots: BTreeMap::new(), param_locals: BTreeMap::new() }
    }
    fn new_with_slots(bc: &'a [u8], abc: &'a AbcFile, slots: BTreeMap<u32, String>) -> Self {
        let blocks = build_blocks(bc);
        Self { blocks, bc, abc, visited: BTreeSet::new(),
               activation_slots: slots, param_locals: BTreeMap::new() }
    }

    fn block_at(&self, offset: usize) -> Option<&Block> {
        self.blocks.iter().find(|b| b.start == offset)
    }

    fn decode_from(&mut self, start: usize, stop_at: Option<usize>) -> Vec<Stmt> {
        self.decode_from_with_stack(start, stop_at, Vec::new())
    }

    fn decode_from_with_stack(&mut self, start: usize, stop_at: Option<usize>, initial_stack: Vec<Expr>) -> Vec<Stmt> {
        self.decode_from_with_stack_out(start, stop_at, initial_stack, &mut vec![])
    }

    fn decode_from_with_stack_out(
        &mut self, start: usize, stop_at: Option<usize>,
        initial_stack: Vec<Expr>, out_carry: &mut Vec<Expr>
    ) -> Vec<Stmt> {
        let mut result = Vec::new();
        let mut cur = start;
        let mut carry_stack: Vec<Expr> = initial_stack;

        loop {
            if Some(cur) == stop_at { break; }
            if self.visited.contains(&cur) { break; }
            if cur >= self.bc.len() { break; }

            let block = match self.block_at(cur) {
                Some(b) => b.clone(),
                None => break,
            };

            self.visited.insert(cur);

            // Decode the block body
            let mut dec = BlockDecoder::new(self.bc, self.abc);
            dec.activation_slots = self.activation_slots.clone();
            dec.param_locals = self.param_locals.clone();
            dec.has_activation = !dec.activation_slots.is_empty();
            // Pre-seed stack from previous block (for dup-across-blocks patterns)
            dec.stack = std::mem::take(&mut carry_stack);
            let cond_expr = dec.decode_range(block.start, block.end);
            let stmts = dec.stmts;
            // Save leftover stack for next block (Fall/Jump only)
            carry_stack = dec.stack.clone();
            for (k, v) in dec.activation_slots { self.activation_slots.insert(k, v); }

            match &block.term {
                Terminator::Return | Terminator::Throw => {
                    result.extend(stmts);
                    break;
                }
                Terminator::Fall(next) | Terminator::Jump(next) => {
                    let next = *next;
                    result.extend(stmts);
                    // carry_stack propagates naturally for fall-through/jump
                    cur = next;
                }
                Terminator::Branch { cond_inv, target, fallthrough } => {
                    let target = *target;
                    let fallthrough = *fallthrough;
                    let inv = *cond_inv;

                    let raw_cond = cond_expr.unwrap_or_else(|| dec.stack.pop().unwrap_or(Expr::Unknown));
                    // If cond_inv, the branch fires when condition is FALSE
                    // Standard AVM2: iftrue → branch if true; iffalse → branch if false
                    // Our Block stores: iftrue → Branch { cond_inv: false, target }
                    //                   iffalse → Branch { cond_inv: true, target }
                    // "target" fires when condition holds (after possibly inverting)
                    // fallthrough = else branch

                    // Detect backward jump (while loop): target < block.start
                    if target < block.start {
                        // Back-edge: the condition block (this block) is at the BOTTOM of the loop.
                        // target = loop body start; fallthrough = after-loop
                        result.extend(stmts);
                        let body = self.decode_from(target, Some(block.start));
                        let cond = if inv { Expr::UnOp("!", Box::new(raw_cond)) } else { raw_cond };
                        result.push(Stmt::While(cond, body));
                        cur = fallthrough;
                    } else {
                        // Forward branch: if/else
                        // iftrue X: if cond, jump to X (then-block = X, else = fallthrough)
                        // iffalse X: if !cond, jump to X (then-block = fallthrough, else = X)
                        result.extend(stmts);

                        // Find the merge point (where both branches rejoin)
                        // Simple heuristic: the merge point is the minimum of:
                        //   - the next block after the target (if target ends with Jump)
                        //   - the next block after the fallthrough
                        let (then_start, else_start) = if inv {
                            // iffalse: cond false → jump to target (else); true → fallthrough (then)
                            (fallthrough, target)
                        } else {
                            // iftrue: cond true → target (then); false → fallthrough (else)
                            (target, fallthrough)
                        };

                        let merge = self.find_merge(then_start, else_start);

                        // Short-circuit && / || pattern detection:
                        // When then_start starts with OP_POP and falls directly to else_start
                        // (a Branch), this is AS3's short-circuit evaluation:
                        //   outer_cond && inner_cond [|| alt_cond]
                        // Collapse the whole thing into a single if-condition.
                        // A uniform chain of any length first; the pair-shaped handling below only
                        // knows one length and quietly reshapes anything longer.
                        let collapse = self
                            .try_collapse_chain(raw_cond.clone(), then_start, else_start, &carry_stack)
                            .and_then(|(cond, consumer)| {
                                let cb = self.block_at(consumer)?.clone();
                                let Terminator::Branch { cond_inv, target, fallthrough } = cb.term
                                    else { return None };
                                // iffalse: true -> fallthrough (body); iftrue: true -> target
                                let (body, after) = if cond_inv { (fallthrough, target) }
                                                    else { (target, fallthrough) };
                                // an empty body means this is not the shape it looks like; let the
                                // ordinary path decode it rather than dropping its statements
                                (body != after).then(|| {
                                    self.visited.insert(then_start);
                                    self.visited.insert(else_start);
                                    (cond, body, Some(after))
                                })
                            })
                            .or_else(|| self.try_collapse_and(
                                then_start, else_start, raw_cond.clone(), &carry_stack
                            ));

                        let saved_carry = carry_stack.clone();
                        carry_stack.clear();

                        if let Some((combined_cond, body_start, body_merge)) = collapse {
                            let body = self.decode_from(body_start, body_merge);
                            result.push(Stmt::If(combined_cond, body, vec![]));
                            cur = body_merge.unwrap_or(usize::MAX);
                        } else {
                            // Capture carry from then_b to propagate to merge block.
                            // BOTH branches are seeded with the predecessor's residual
                            // stack (saved_carry): a value pushed before the branch lives
                            // there, and an empty else-seed left the else body underflowing
                            // to /* ? */. Safe w.r.t. the ternary heuristic below: that only
                            // fires when saved_carry is empty (else then_leftover.len()!=1),
                            // in which case this seed is also empty — no behavior change.
                            let mut then_leftover = vec![];
                            let then_b = self.decode_from_with_stack_out(then_start, merge, saved_carry.clone(), &mut then_leftover);
                            let mut else_leftover = vec![];
                            let else_b = if else_start != merge.unwrap_or(usize::MAX) {
                                self.decode_from_with_stack_out(else_start, merge, saved_carry, &mut else_leftover)
                            } else {
                                vec![]
                            };

                            // Ternary select: both branches produce no stmts but leave a carry value
                            if then_b.is_empty() && else_b.is_empty()
                                && then_leftover.len() == 1 && else_leftover.len() == 1
                            {
                                let then_val = then_leftover.remove(0);
                                let else_val = else_leftover.remove(0);
                                let ternary = Expr::GetLex(format!(
                                    "({} ? {} : {})",
                                    raw_cond.render(), then_val.render(), else_val.render()
                                ));
                                carry_stack = vec![ternary];
                            } else {
                                result.push(make_if(raw_cond, then_b, else_b));
                                carry_stack = if !then_leftover.is_empty() { then_leftover } else { else_leftover };
                            }
                            cur = merge.unwrap_or(usize::MAX);
                        }
                    }
                }
                Terminator::BranchCmp { op, target, fallthrough } => {
                    let target = *target;
                    let fallthrough = *fallthrough;
                    let cond = cond_expr.unwrap_or_else(|| {
                        let r = dec.stack.pop().unwrap_or(Expr::Unknown);
                        let l = dec.stack.pop().unwrap_or(Expr::Unknown);
                        Expr::BinOp(op, Box::new(l), Box::new(r))
                    });

                    result.extend(stmts);

                    if target < block.start {
                        // Back edge: while loop. Body = from target to this block.
                        let body = self.decode_from(target, Some(block.start));
                        result.push(Stmt::While(cond, body));
                        cur = fallthrough;
                    } else {
                        // Seed both branch bodies with THIS block's residual operand
                        // stack (what's left after the comparison popped its operands).
                        // A value pushed before the branch (e.g. a `<root>` receiver for
                        // a call in the body) lives there; without it the body decoder
                        // underflows to Expr::Unknown and emits a dangling `/* ? */`
                        // receiver/condition. (Previously these branches decoded from an
                        // empty stack.) Strictly more faithful — the body genuinely
                        // continues from the predecessor's stack state.
                        let carry = dec.stack.clone();
                        let merge = self.find_merge(target, fallthrough);
                        let mut t_out = vec![];
                        let then_b = self.decode_from_with_stack_out(target, merge, carry.clone(), &mut t_out);
                        let else_b = if fallthrough != merge.unwrap_or(usize::MAX) {
                            let mut e_out = vec![];
                            self.decode_from_with_stack_out(fallthrough, merge, carry, &mut e_out)
                        } else { vec![] };
                        result.push(make_if(cond, then_b, else_b));
                        cur = merge.unwrap_or(usize::MAX);
                    }
                }
            }
        }
        *out_carry = carry_stack;
        result
    }

    /// The instruction immediately BEFORE a block's terminator, as (offset, opcode).
    ///
    /// A short-circuit link is recognised by this being a `dup`: the value is kept for the next
    /// link to test, which is exactly what distinguishes "one term of a chain" from "the condition
    /// this branch consumes".
    fn op_before_term(&self, b: &Block) -> Option<(usize, u8)> {
        let (mut pos, mut prev) = (b.start, None);
        while pos < b.end {
            let size = instr_size(self.bc, pos).max(1);
            if pos + size >= b.end { break; }
            prev = Some((pos, self.bc[pos]));
            pos += size;
        }
        prev
    }

    /// Fold a short-circuit chain of ANY length into one condition.
    ///
    /// AS3 writes `A && B && C && D` as a chain: evaluate A, `dup` it, `iffalse` to the next link,
    /// `pop` the spare copy, evaluate B, and so on, with the LAST link's branch being the one that
    /// actually skips the body. A false anywhere cascades down the chain to that skip. `||` is the
    /// same shape with `iftrue`.
    ///
    /// The chain has no fixed length, and assuming one is how conditions get silently rewritten:
    /// forced into a three-term `(A && B) || C` mould, clocktown's four-term `&&` came out with the
    /// wrong operator, one term missing, and the remainder unrecoverable -- so the block guarding
    /// its falling rocks rendered as `if (false)`. Nothing announced it.
    ///
    /// Only a UNIFORM chain is folded here (all `iffalse`, or all `iftrue`). A genuinely mixed
    /// condition compiles to a different shape and is left to the caller's other handling.
    ///
    /// Returns the combined condition and the block whose branch consumes it.
    fn try_collapse_chain(&mut self, first_cond: Expr, then_start: usize, else_start: usize,
                          _initial_stack: &[Expr]) -> Option<(Expr, usize)>
    {
        let mut terms = vec![first_cond];
        let mut op: Option<&'static str> = None;
        let (mut term_at, mut branch_at) = (then_start, else_start);

        for _ in 0..MAX_CHAIN_LINKS {
            // The next term is computed in the block BETWEEN this branch and the next: the spare
            // copy is popped, the term evaluated, and it falls into the block that tests it.
            let tb = self.block_at(term_at)?.clone();
            if !matches!(tb.term, Terminator::Fall(n) if n == branch_at) { return None; }
            // A link BEGINS by discarding the duplicated copy. Without that this matches two
            // independent `if`s that merely sit next to each other -- sandbag's LEFT/RIGHT pair
            // became `if (LEFT && null)` with the LEFT body swallowed, `null` being the very
            // placeholder seeded below for a `pop` that was never there.
            if self.bc.get(tb.start) != Some(&OP_POP) { return None; }

            let mut dec = BlockDecoder::new(self.bc, self.abc);
            dec.activation_slots = self.activation_slots.clone();
            dec.param_locals = self.param_locals.clone();
            dec.has_activation = !dec.activation_slots.is_empty();
            dec.stack = vec![Expr::Null]; // the `pop` of the duplicated copy consumes this
            let term = dec.decode_range(tb.start, tb.end)
                .or_else(|| dec.stack.last().cloned())
                .unwrap_or(Expr::Unknown);
            if matches!(term, Expr::Unknown) { return None; }
            terms.push(term);

            let bb = self.block_at(branch_at)?.clone();
            let Terminator::Branch { cond_inv, target, fallthrough } = bb.term else { return None };

            // `iffalse` short-circuits on false, which is `&&`; `iftrue` is `||`
            let this_op = if cond_inv { "&&" } else { "||" };
            match op {
                None => op = Some(this_op),
                Some(prev) if prev == this_op => {}
                Some(_) => return None, // mixed: a different shape, left to other handling
            }

            match self.op_before_term(&bb) {
                // A link's branch block is EXACTLY `dup` then the branch: the term itself was
                // computed in the block that fell into it. A block that does anything else before
                // the dup is not a link, and treating it as one swallows whatever it was doing.
                Some((at, OP_DUP)) if at == bb.start => { term_at = fallthrough; branch_at = target; }
                // consumes it: this branch is the one that skips the body
                _ => {
                    if terms.len() < MIN_CHAIN_TERMS { return None; }
                    let o = op.unwrap_or("&&");
                    let combined = terms.into_iter()
                        .reduce(|a, b| Expr::BinOp(o, Box::new(a), Box::new(b)))?;
                    return Some((combined, branch_at));
                }
            }
        }
        None
    }

    /// Find the merge point after an if/else.
    /// Detect short-circuit && pattern:
    ///   then_block = a Fall-only block that ends by falling into else_start
    ///   else_start  = a Branch block (the second condition check)
    /// If detected, returns (combined_and_cond, inner_then_start, inner_merge)
    fn try_collapse_and(
        &mut self,
        then_start: usize,
        else_start: usize,
        first_cond: Expr,
        initial_stack: &[Expr],
    ) -> Option<(Expr, usize, Option<usize>)> {
        // then_start block must be a Fall that exits to else_start
        let then_block = self.block_at(then_start)?.clone();
        if !matches!(then_block.term, Terminator::Fall(n) if n == else_start) {
            return None;
        }
        // else_start must be a Branch or BranchCmp
        let else_block = self.block_at(else_start)?.clone();
        // For && collapse: the "inner_then" is the continuation AFTER the second check.
        // We process from fallthrough of the else_block so the condition block is included.
        // inner_start = fallthrough of else_block (where condition was not taken)
        // which falls into the merge where the final condition is consumed.
        let _inner_start = match &else_block.term {
            Terminator::Branch { fallthrough, .. } => *fallthrough,
            Terminator::BranchCmp { fallthrough, .. } => *fallthrough,
            _ => return None,
        };
        // inner_then: start of the body to decode (the fallthrough of else_block,
        // which computes the final condition and falls into the merge point)
        // inner_else: the skip target (where we jump if condition is false)
        let (inner_then, inner_else) = match &else_block.term {
            Terminator::Branch { cond_inv: _, target, fallthrough } => {
                // Use fallthrough as start so blocks between else_block and merge are processed
                (*fallthrough, *target)
            }
            Terminator::BranchCmp { target, fallthrough, .. } => (*fallthrough, *target),
            _ => return None,
        };

        // The glue block must start with OP_POP (consuming the dup residue).
        // This is the hallmark of the AS3 short-circuit && pattern.
        // Without it, we could spuriously collapse unrelated if-chains.
        if then_block.start < self.bc.len() && self.bc[then_block.start] != OP_POP {
            return None;
        }

        // Decode the glue block (then_start..else_start) to extract the second condition
        let mut dec = BlockDecoder::new(self.bc, self.abc);
        dec.activation_slots = self.activation_slots.clone();
        dec.param_locals = self.param_locals.clone();
        dec.has_activation = !dec.activation_slots.is_empty();
        dec.stack = initial_stack.to_vec();
        let _cond2_opt = dec.decode_range(then_start, else_start);
        // After glue block, dec.stack has the value that the else_block will branch on
        // (e.g., hasEventListener result)

        // Also decode the else_start block to get its condition expression
        let mut dec2 = BlockDecoder::new(self.bc, self.abc);
        dec2.activation_slots = self.activation_slots.clone();
        dec2.param_locals = self.param_locals.clone();
        dec2.has_activation = !dec2.activation_slots.is_empty();
        dec2.stack = dec.stack.clone();
        let cond3_opt = dec2.decode_range(else_start, else_block.end);

        let second_cond = cond3_opt
            .or_else(|| dec2.stack.last().cloned())
            .unwrap_or(Expr::Unknown);

        if matches!(second_cond, Expr::Unknown) {
            return None;
        }

        // Decode the fallthrough-of-else_block (inner_then) to get the OR alternative condition
        // inner_then is the path taken when second_cond is false
        // inner_then computes the fallback condition (e.g., arg0 != null)
        let alt_cond = if inner_then != inner_else {
            // Find the exact end of the inner_then block
            let inner_then_end = self.block_at(inner_then)
                .map(|b| b.end)
                .unwrap_or(inner_then + 16.min(self.bc.len() - inner_then));
            let mut dec3 = BlockDecoder::new(self.bc, self.abc);
            dec3.activation_slots = self.activation_slots.clone();
            dec3.param_locals = self.param_locals.clone();
            dec3.has_activation = !dec3.activation_slots.is_empty();
            // inner_then block has a residue pop of the dup from else_block;
            // seed the stack with a dummy so pop consumes it cleanly
            dec3.stack = vec![Expr::Null]; // dup residue placeholder
            let r = dec3.decode_range(inner_then, inner_then_end);
            // Prefer condition expr returned by decode_range; fall back to stack top
            r.or_else(|| dec3.stack.last().cloned())
        } else { None };

        // Build the combined condition:
        // (first_cond && second_cond) || alt_cond
        let first_and_second = Expr::BinOp("&&", Box::new(first_cond), Box::new(second_cond));
        let combined = if let Some(alt) = alt_cond {
            if !matches!(alt, Expr::Unknown | Expr::Null) {
                // Wrap && side in parens: (A && B) || C
                let lhs = Expr::GetLex(format!("({})", first_and_second.render()));
                Expr::BinOp("||", Box::new(lhs), Box::new(alt))
            } else {
                first_and_second
            }
        } else {
            first_and_second
        };

        // inner_else is the block that consumes the combined condition value (e.g. iffalse→end).
        // We need: body_start = where execution goes when condition is TRUE
        //          body_merge = where both paths rejoin (after-if continuation)
        let (body_start, body_merge) = if let Some(final_block) = self.block_at(inner_else).cloned() {
            match &final_block.term {
                Terminator::Branch { cond_inv, target, fallthrough } => {
                    // iffalse (cond_inv=true): true → fallthrough (body), false → target (skip)
                    // iftrue  (cond_inv=false): true → target (body), false → fallthrough (skip)
                    let (body, after) = if *cond_inv {
                        (*fallthrough, *target)
                    } else {
                        (*target, *fallthrough)
                    };
                    (body, Some(after))
                }
                _ => (inner_else, self.find_merge_inner(inner_then, inner_else)),
            }
        } else {
            (inner_else, self.find_merge_inner(inner_then, inner_else))
        };

        // Guard against a zero-length body range. When body_start == body_merge the
        // collapsed `if (A && B) { ... }` would decode an EMPTY body, silently dropping
        // the branch's statements (e.g. mario's rise(): the `pressed2 = true` that arms
        // the up-special launch). Refuse the collapse so the caller falls back to the
        // nested-if path, which preserves the body. Return None WITHOUT marking blocks
        // visited, so the fallback can still decode them.
        if body_merge == Some(body_start) {
            return None;
        }

        // Mark all consumed blocks as visited
        self.visited.insert(then_start);
        self.visited.insert(else_start);
        self.visited.insert(inner_then);  // fallthrough of else_block
        self.visited.insert(inner_else);  // final condition-consumption block

        Some((combined, body_start, body_merge))
    }

    fn find_merge(&self, then_start: usize, else_start: usize) -> Option<usize> {
        self.find_merge_inner(then_start, else_start)
    }
    fn find_merge_inner(&self, then_start: usize, else_start: usize) -> Option<usize> {
        let then_final_exit = self.chain_exit(then_start, else_start);
        let else_final_exit = self.chain_exit(else_start, then_start);

        match (then_final_exit, else_final_exit) {
            (Some(a), Some(b)) if a == b => Some(a),
            // A one-sided exit can be chain_exit's branch-stop AT the branch's own start (the
            // then-body is a single block ending in an inner if). Taking that as the merge yields
            // an empty then-body with the real body leaking out below the if — so only trust an
            // exit that actually left its own side; otherwise fall back to the skip target.
            (Some(a), None) if a != then_start => Some(a),
            (None, Some(b)) if b != else_start => Some(b),
            _ => {
                if else_start > then_start { Some(else_start) } else { None }
            }
        }
    }

    /// Walk a straight Fall/Jump chain and return the first offset that exits the chain.
    /// Stops at Branch blocks (returns the branch start — the merge is AT the branch, not after).
    /// Stops at Return/Throw (returns None).
    fn chain_exit(&self, start: usize, exclude: usize) -> Option<usize> {
        let mut cur = start;
        for _ in 0..32 {
            if cur == exclude { return Some(cur); }
            let block = self.block_at(cur)?;
            match &block.term {
                Terminator::Fall(next) | Terminator::Jump(next) => {
                    let next = *next;
                    if next == exclude { return Some(next); }
                    cur = next;
                }
                Terminator::Return | Terminator::Throw => return None,
                // Branch block: the merge is AT this block's start (both paths arrive here)
                Terminator::Branch { .. } | Terminator::BranchCmp { .. } => return Some(cur),
            }
        }
        None
    }

    /// Get the first unconditional jump target of a block (its exit).
    #[allow(dead_code)]
    fn block_exit_target(&self, start: usize) -> Option<usize> {
        let block = self.block_at(start)?;
        match &block.term {
            Terminator::Jump(t) => Some(*t),
            Terminator::Fall(t) => Some(*t),
            Terminator::Return | Terminator::Throw => None,
            Terminator::Branch { fallthrough, .. } => Some(*fallthrough),
            Terminator::BranchCmp { fallthrough, .. } => Some(*fallthrough),
        }
    }
}

// ─── Public API ───────────────────────────────────────────────────────────────

/// Collapse `findpropstrict 'X' + callproperty 'X'` pattern.
/// findpropstrict pushes `This.X` (a GetProperty sentinel);
/// if callproperty name matches that property, collapse to just `This`.
fn collapse_findprop(obj: Expr, call_name: &str) -> Expr {
    match &obj {
        Expr::GetProperty(inner, prop_name) if prop_name == call_name => {
            // The object was pushed by findpropstrict — the real receiver is the inner object
            *inner.clone()
        }
        _ => obj,
    }
}

/// Build activation slot name map from method body traits.
/// Slot trait kind=0 (Var) has slot_idx → name mapping.
fn slots_from_traits(traits: &[crate::abc_parser::Trait]) -> BTreeMap<u32, String> {
    let mut slots = BTreeMap::new();
    for t in traits {
        if t.kind == 0 || t.kind == 6 {  // Slot or Const
            if t.slot_idx > 0 && !t.name.is_empty() {
                slots.insert(t.slot_idx, t.name.clone());
            }
        }
    }
    slots
}

/// Pre-scan bytecode to map activation slot indices to parameter names.
/// Pattern: getscopeobject 1 → getlocal_N → setslot M  means slot M = param N.
/// Falls back to trait names if available.
fn infer_activation_slots(bc: &[u8], params: &[String]) -> BTreeMap<u32, String> {
    let mut slots: BTreeMap<u32, String> = BTreeMap::new();
    let mut i = 0;
    while i < bc.len() {
        let op = bc[i]; i += 1;
        match op {
            // getscopeobject 1, getlocal_N, setslot M → slot M = param N-1
            0x65 if i < bc.len() && bc[i] == 1 => {
                i += 1; // consume the operand
                if i < bc.len() {
                    let next_op = bc[i]; i += 1;
                    let local_n = match next_op {
                        0xD0 => Some(0u32),
                        0xD1 => Some(1),
                        0xD2 => Some(2),
                        0xD3 => Some(3),
                        0x62 => {
                            let n = read_u30_at(bc, &mut i);
                            Some(n)
                        }
                        _ => { i -= 1; None }
                    };
                    if let Some(n) = local_n {
                        if i < bc.len() {
                            let set_op = bc[i]; i += 1;
                            if set_op == 0x6D { // setslot
                                let slot = read_u30_at(bc, &mut i);
                                let name = if n == 0 {
                                    "self".to_string()
                                } else if (n as usize) <= params.len() {
                                    params[n as usize - 1].clone()
                                } else {
                                    format!("_v{}", n)
                                };
                                slots.insert(slot, name);
                            } else {
                                i -= 1;
                            }
                        }
                    }
                }
            }
            _ => { i += instr_size(bc, i - 1) - 1; }
        }
    }
    slots
}

/// Decompile an ABC method body to a Fraymakers Haxe function string.
/// Decompile a closure (newfunction) into a Haxe function literal.
fn decompile_closure(method_idx: u32, abc: &AbcFile) -> Expr {
    // Find method body
    let body = match abc.method_bodies.iter().find(|b| b.method_idx == method_idx) {
        Some(b) => b,
        None => return Expr::GetLex(format!("/* closure method_{} not found */", method_idx)),
    };

    // Get param count from method info
    let param_count = abc.methods.get(method_idx as usize)
        .map(|m| m.param_count as usize)
        .unwrap_or(0);

    // Build param names: arg0, arg1, ...
    let params: Vec<String> = (0..param_count).map(|i| format!("arg{}", i)).collect();

    if body.bytecode.is_empty() {
        return Expr::Closure(params, vec![]);
    }

    let mut activation_slots = infer_activation_slots(&body.bytecode, &params);
    for (slot, name) in slots_from_traits(&body.activation_traits) {
        activation_slots.entry(slot).or_insert(name);
    }
    let mut decoder = StructuredDecoder::new_with_slots(&body.bytecode, abc, activation_slots);
    for (i, param) in params.iter().enumerate() {
        decoder.param_locals.insert((i + 1) as u32, param.clone());
    }
    let stmts = decoder.decode_from(0, None);
    let stmts: Vec<Stmt> = stmts.into_iter().filter(|s| {
        !matches!(s, Stmt::VarDecl(0, Expr::This))
    }).collect();
    let stmts = collapse_duplicate_ifs(stmts);
    Expr::Closure(params, stmts)
}

/// Guarantee counter `while` loops terminate.
///
/// SSF2 AS3 commonly mutated an array *during* iteration via `arr.splice(i, 1)`
/// (which shrinks the array, so the remove branch intentionally does NOT do
/// `i++`). Our decompiler drops the splice when mapping the API, leaving a loop
/// like:
///
///   while (i < arr.length) {
///     if (arr[i] == null) { i = i + 1; }   // null path advances
///     else { ...removeChild...; }          // remove path does NOT advance
///   }
///
/// With the splice gone, the remove path neither advances `i` nor shrinks the
/// array → infinite loop → engine freeze. (AS3→Haxe loop forms differ; see
/// haxedev as3-to-haxe guide.) This pass restores progress: for a counter
/// `while (i < ….length)` whose body is a single if/else where exactly one
/// branch advances `i`, append `i = i + 1;` to the branch that lacks it. The
/// guard is only added when a path is genuinely missing the advance, so correct
/// loops are never double-stepped.
fn guard_loop_termination(stmts: Vec<Stmt>) -> Vec<Stmt> {
    // Does `cond` look like `<counter> < <bound>` / `<counter> <= <bound>` where
    // <counter> is a bare local-variable identifier? Return the counter's rendered
    // name. We accept ANY simple identifier — both the i/j/k/l names produced by
    // rename_loop_counters AND un-renamed `_vN` locals that keep their slot name
    // (these arise when the counter is initialized to a non-zero value like
    // `_v4 = 1`, or not immediately before the `while`, or inside a branch — cases
    // rename_loop_counters skips) — and ANY right-hand bound (a `.length`, a
    // constant like `< 8`, or an expression like `< _v6 + 200`). Restricting the
    // lhs to a bare identifier keeps us to genuine local counters and never touches
    // property/array-based conditions (e.g. `self.index < n`), which we can't
    // safely auto-advance.
    fn counter_of_cond(cond: &Expr) -> Option<String> {
        if let Expr::BinOp(op, l, _r) = cond {
            if *op == "<" || *op == "<=" {
                let lname = l.render();
                if is_simple_ident(&lname) {
                    return Some(lname);
                }
            }
        }
        None
    }
    fn is_simple_ident(s: &str) -> bool {
        let mut cs = s.chars();
        match cs.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }
    // True if the loop body manages its own termination, so we must NOT inject an
    // increment: either it splices the iterated array (legitimate AS3
    // mutate-in-place), or SOME top-level (straight-line, unconditional) statement
    // assigns the counter. Checking top-level — not just the last statement, and
    // not statements nested in branches — means a correctly-formed loop that
    // advances the counter anywhere unconditionally is left untouched (no
    // double-step), while a loop whose only advance is inside an if/else branch
    // (like removeAllEffects, which spins forever on the non-advancing branch) is
    // still guarded.
    fn body_self_advances(body: &[Stmt], name: &str) -> bool {
        if body.iter().any(stmt_has_splice) {
            return true; // array shrinks each iter → terminates without i++
        }
        body.iter().any(|s| stmt_assigns_name(s, name))
    }
    fn stmt_assigns_name(s: &Stmt, name: &str) -> bool {
        match s {
            // Renamed counters + the injected guard: Stmt::Expr(GetLex("i = i + 1")).
            Stmt::Expr(Expr::GetLex(code)) => {
                let c = code.replace(' ', "");
                c.starts_with(&format!("{name}="))
                    || c.starts_with(&format!("{name}++"))
                    || c.starts_with(&format!("{name}--"))
            }
            // Un-renamed `_vN` local writes: Stmt::VarDecl(n, _) renders as `_vN = …`.
            Stmt::VarDecl(n, _) => format!("_v{n}") == name,
            // Activation-slot named variables.
            Stmt::NamedAssign(nm, _) => nm == name,
            _ => false,
        }
    }
    fn stmt_has_splice(s: &Stmt) -> bool {
        match s {
            Stmt::Expr(Expr::GetLex(code)) => code.contains(".splice("),
            Stmt::Expr(e) | Stmt::Return(Some(e)) => expr_has_splice(e),
            Stmt::VarDecl(_, e) | Stmt::NamedAssign(_, e) => expr_has_splice(e),
            Stmt::If(_, t, el) => t.iter().chain(el).any(stmt_has_splice),
            _ => false,
        }
    }
    fn expr_has_splice(e: &Expr) -> bool {
        match e {
            Expr::Call(o, m, args) => m == "splice" || expr_has_splice(o)
                || args.iter().any(expr_has_splice),
            Expr::GetProperty(o, _) => expr_has_splice(o),
            Expr::BinOp(_, a, b) => expr_has_splice(a) || expr_has_splice(b),
            _ => false,
        }
    }
    fn incr_stmt(name: &str) -> Stmt {
        Stmt::Expr(Expr::GetLex(format!("{name} = {name} + 1")))
    }

    fn fix_body(body: Vec<Stmt>) -> Vec<Stmt> {
        body.into_iter().map(fix_stmt).collect()
    }
    // Recurse into closures (event-handler lambdas) so loops nested inside them
    // are guarded too — many SSF2 array-walk loops live in closures, not at the
    // top level of a function.
    fn fix_expr(e: Expr) -> Expr {
        match e {
            Expr::Closure(params, body) => Expr::Closure(params, fix_body(body)),
            Expr::Call(o, m, args) => Expr::Call(
                Box::new(fix_expr(*o)), m, args.into_iter().map(fix_expr).collect()),
            Expr::New(c, args) => Expr::New(c, args.into_iter().map(fix_expr).collect()),
            Expr::GetProperty(o, p) => Expr::GetProperty(Box::new(fix_expr(*o)), p),
            Expr::BinOp(op, a, b) => Expr::BinOp(op, Box::new(fix_expr(*a)), Box::new(fix_expr(*b))),
            Expr::UnOp(op, a) => Expr::UnOp(op, Box::new(fix_expr(*a))),
            Expr::Array(items) => Expr::Array(items.into_iter().map(fix_expr).collect()),
            Expr::Object(pairs) => Expr::Object(
                pairs.into_iter().map(|(k, v)| (k, fix_expr(v))).collect()),
            other => other,
        }
    }
    fn fix_stmt(s: Stmt) -> Stmt {
        match s {
            Stmt::While(cond, body) => {
                let body = fix_body(body); // recurse into nested loops first
                if let Some(name) = counter_of_cond(&cond) {
                    // If NO control-flow path through the body unconditionally
                    // advances the counter as its final straight-line action (the
                    // AS3 splice/increment was lost in decompilation), the loop can
                    // spin forever → engine freeze. Fix: append one unconditional
                    // `i = i + 1` to the END of the loop body. GUARANTEES
                    // termination. If some inner branch also advances (e.g. a
                    // null-skip `i++`), that path over-steps by one — harmless (it
                    // skips an already-handled slot); termination, not exact
                    // iteration, is what prevents the freeze. Correctly-formed
                    // for-loops (last statement already advances) and array-splice
                    // loops are detected by ends_with_counter_advance and left
                    // untouched, so we never double-step a working loop.
                    if !body_self_advances(&body, &name) {
                        let mut body = body;
                        body.push(incr_stmt(&name));
                        return Stmt::While(cond, body);
                    }
                }
                Stmt::While(cond, body)
            }
            Stmt::If(c, t, e) => Stmt::If(c, fix_body(t), fix_body(e)),
            // Recurse into closures held by Expr-bearing statements.
            Stmt::Expr(e) => Stmt::Expr(fix_expr(e)),
            Stmt::VarDecl(n, e) => Stmt::VarDecl(n, fix_expr(e)),
            Stmt::NamedAssign(n, e) => Stmt::NamedAssign(n, fix_expr(e)),
            Stmt::SetProp(o, n, v) => Stmt::SetProp(fix_expr(o), n, fix_expr(v)),
            Stmt::Return(opt) => Stmt::Return(opt.map(fix_expr)),
            other => other,
        }
    }

    stmts.into_iter().map(fix_stmt).collect()
}

/// Rename loop-counter locals (_vN initialized to 0 before a While) to i/j/k.
fn rename_loop_counters(stmts: Vec<Stmt>) -> Vec<Stmt> {
    use std::collections::HashMap;
    let mut counter_map: HashMap<u32, String> = HashMap::new();
    let counter_names = ["i", "j", "k", "l"];
    let mut name_idx = 0usize;
    for i in 0..stmts.len() {
        if let Stmt::VarDecl(n, Expr::Num(v)) = &stmts[i] {
            if *v == 0.0 {
                let followed_by_while = stmts.get(i + 1).is_some_and(|s| matches!(s, Stmt::While(..)));
                if followed_by_while && name_idx < counter_names.len() {
                    counter_map.insert(*n, counter_names[name_idx].to_string());
                    name_idx += 1;
                }
            }
        }
    }
    if counter_map.is_empty() { return stmts; }
    // Walk statements: initializer VarDecl → NamedAssign("i", 0) with var
    // All other VarDecl(n) for counter n → plain assignment expression
    stmts.into_iter().map(|stmt| {
        // Top-level initializer: VarDecl(n, 0) followed by While → declare with var
        if let Stmt::VarDecl(n, Expr::Num(v)) = &stmt {
            if *v == 0.0 {
                if let Some(name) = counter_map.get(n) {
                    return Stmt::NamedAssign(name.clone(), Expr::Num(0.0));
                }
            }
        }
        rename_locals_in_stmt(stmt, &counter_map)
    }).collect()
}

fn rename_locals_in_stmts(stmts: Vec<Stmt>, map: &std::collections::HashMap<u32, String>) -> Vec<Stmt> {
    stmts.into_iter().map(|s| rename_locals_in_stmt(s, map)).collect()
}

fn rename_locals_in_stmt(stmt: Stmt, map: &std::collections::HashMap<u32, String>) -> Stmt {
    match stmt {
        Stmt::VarDecl(n, e) => {
            let e2 = rename_locals_in_expr(e, map);
            if let Some(name) = map.get(&n) {
                // Use Expr statement: "name = value" (no var keyword — already declared)
                Stmt::Expr(Expr::GetLex(format!("{} = {}", name, e2.render())))
            } else {
                Stmt::VarDecl(n, e2)
            }
        }
        Stmt::NamedAssign(name, e) => Stmt::NamedAssign(name, rename_locals_in_expr(e, map)),
        Stmt::SetProp(obj, name, val) => Stmt::SetProp(
            rename_locals_in_expr(obj, map), name, rename_locals_in_expr(val, map)),
        Stmt::Expr(e) => Stmt::Expr(rename_locals_in_expr(e, map)),
        Stmt::Return(e) => Stmt::Return(e.map(|e| rename_locals_in_expr(e, map))),
        Stmt::If(cond, then_b, else_b) => Stmt::If(
            rename_locals_in_expr(cond, map),
            rename_locals_in_stmts(then_b, map),
            rename_locals_in_stmts(else_b, map)),
        Stmt::While(cond, body) => Stmt::While(
            rename_locals_in_expr(cond, map),
            rename_locals_in_stmts(body, map)),
        other => other,
    }
}

fn rename_locals_in_expr(expr: Expr, map: &std::collections::HashMap<u32, String>) -> Expr {
    match expr {
        Expr::Local(n) => {
            if let Some(name) = map.get(&n) {
                Expr::GetLex(name.clone())
            } else {
                Expr::Local(n)
            }
        }
        Expr::GetProperty(obj, name) => Expr::GetProperty(Box::new(rename_locals_in_expr(*obj, map)), name),
        Expr::Call(obj, method, args) => Expr::Call(
            Box::new(rename_locals_in_expr(*obj, map)), method,
            args.into_iter().map(|a| rename_locals_in_expr(a, map)).collect()),
        Expr::BinOp(op, l, r) => Expr::BinOp(op,
            Box::new(rename_locals_in_expr(*l, map)),
            Box::new(rename_locals_in_expr(*r, map))),
        Expr::UnOp(op, e) => Expr::UnOp(op, Box::new(rename_locals_in_expr(*e, map))),
        Expr::Array(items) => Expr::Array(items.into_iter().map(|i| rename_locals_in_expr(i, map)).collect()),
        Expr::Object(pairs) => Expr::Object(pairs.into_iter().map(|(k,v)| (k, rename_locals_in_expr(v, map))).collect()),
        Expr::Closure(params, stmts) => Expr::Closure(params, rename_locals_in_stmts(stmts, map)),
        other => other,
    }
}

/// Post-process AST to collapse redundant duplicate-condition if-blocks.
/// Pattern: If(X, [If(X, body, []), ...tail], []) -> If(X, [body..., tail...], [])
/// This arises from dup+iftrue carry patterns producing double-guarded blocks.
fn collapse_duplicate_ifs(stmts: Vec<Stmt>) -> Vec<Stmt> {
    stmts.into_iter().map(|stmt| match stmt {
        Stmt::If(cond, mut then_b, else_b) => {
            // Recurse first
            then_b = collapse_duplicate_ifs(then_b);
            let else_b = collapse_duplicate_ifs(else_b);
            // Check if then_b starts with If(same_cond, inner_body, [])
            if else_b.is_empty() && !then_b.is_empty() {
                if let Stmt::If(inner_cond, inner_body, inner_else) = then_b[0].clone() {
                    if inner_else.is_empty() && inner_cond.render() == cond.render() {
                        // Collapse: hoist inner_body into then_b, dropping the duplicate guard
                        let mut new_then = inner_body;
                        new_then.extend(then_b.into_iter().skip(1));
                        return Stmt::If(cond, new_then, vec![]);
                    }
                }
            }
            Stmt::If(cond, then_b, else_b)
        }
        Stmt::While(cond, body) => Stmt::While(cond, collapse_duplicate_ifs(body)),
        other => other,
    }).collect()
}

pub fn decompile_method(
    body: &crate::abc_parser::MethodBody,
    abc: &AbcFile,
    name: &str,
    params: &[String],
) -> String {
    if body.bytecode.is_empty() {
        return format!("function {}({}) {{\n}}\n\n", name, params.join(", "));
    }

    // Build activation slot names: start from bytecode inference, then overlay trait names
    let mut activation_slots = infer_activation_slots(&body.bytecode, params);
    for (slot, name) in slots_from_traits(&body.activation_traits) {
        // Trait names win unless the slot is already mapped to a param name
        activation_slots.entry(slot).or_insert(name);
    }

    let mut decoder = StructuredDecoder::new_with_slots(&body.bytecode, abc, activation_slots);
    // Map param locals: local_0 = this, local_1 = param0, local_2 = param1, etc.
    for (i, param) in params.iter().enumerate() {
        let local_idx = (i + 1) as u32;
        decoder.param_locals.insert(local_idx, param.clone());
    }
    let stmts = decoder.decode_from(0, None);

    // Remove redundant local0 (= this) assignments
    let stmts: Vec<Stmt> = stmts.into_iter().filter(|s| {
        !matches!(s, Stmt::VarDecl(0, Expr::This))
    }).collect();
    let stmts = collapse_duplicate_ifs(stmts);
    let stmts = rename_loop_counters(stmts);
    let stmts = guard_loop_termination(stmts);

    let param_str = params.join(", ");
    let mut out = format!("function {}({}) {{\n", name, param_str);
    out.push_str(&render_local_decls(&stmts, params.len(), 1));
    out.push_str(&render_stmts(&stmts, 1));
    out.push_str("}\n\n");
    out
}

#[cfg(test)]
mod make_if_tests {
    use super::{make_if, render_stmts, render_local_decls, Expr, Stmt};

    #[test]
    fn register_temps_get_var_declarations() {
        // _v2 written at top level, _v3 written only inside an if-branch: BOTH must be declared
        // with `var` at the function top (a branch-scoped var would be invisible after the block).
        let stmts = vec![
            Stmt::VarDecl(2, Expr::Num(1.0)),
            Stmt::If(Expr::Bool(true), vec![Stmt::VarDecl(3, Expr::Num(2.0))], vec![]),
        ];
        let decls = render_local_decls(&stmts, 0, 1);
        assert!(decls.contains("var _v2 = null;"), "missing _v2 decl: {decls}");
        assert!(decls.contains("var _v3 = null;"), "missing _v3 (branch) decl: {decls}");
    }

    #[test]
    fn param_registers_are_not_redeclared() {
        // local 1 == arg0 (a param, rendered as arg0); it must NOT get a `var _v1` shadow.
        let stmts = vec![Stmt::VarDecl(1, Expr::Num(1.0)), Stmt::VarDecl(2, Expr::Num(2.0))];
        let decls = render_local_decls(&stmts, 1, 1); // 1 param
        assert!(!decls.contains("_v1"), "param register declared as _v1: {decls}");
        assert!(decls.contains("var _v2 = null;"), "temp _v2 not declared: {decls}");
    }

    #[test]
    fn rebound_local0_is_compilable_not_self_assign() {
        // A frame script can `setlocal_0` of a non-`this` value (rebinding
        // `this`-as-local). That must NOT render as `self = …` (uncompilable —
        // `self` is final in Fraymakers Haxe). It renders `_v0 = …` and gets a
        // `var _v0` declaration so both the write and any reads compile.
        let rebind = Stmt::VarDecl(0, Expr::Num(5.0));
        let rendered = render_stmts(std::slice::from_ref(&rebind), 0);
        assert!(rendered.contains("_v0 = 5"), "rebound local 0 not _v0: {rendered}");
        assert!(!rendered.contains("self ="), "uncompilable self-assign leaked: {rendered}");
        let decls = render_local_decls(std::slice::from_ref(&rebind), 0, 1);
        assert!(decls.contains("var _v0 = null;"), "rebound local 0 not declared: {decls}");
    }

    #[test]
    fn noop_local0_this_assign_is_dropped() {
        // The `this = this` setup line some compilers emit is a no-op: it's not
        // rendered and gets no `_v0` declaration.
        let setup = Stmt::VarDecl(0, Expr::This);
        assert_eq!(render_stmts(std::slice::from_ref(&setup), 0), "", "this=this not dropped");
        assert_eq!(render_local_decls(std::slice::from_ref(&setup), 0, 1), "", "this=this got a decl");
    }

    /// A call assigned to a local is READ back, not performed again.
    ///
    /// clocktown assigns one flaming rock and then sets its x, its y, and pushes it. Propagating
    /// the call to each of those spawns four rocks.
    #[test]
    fn an_effectful_value_is_not_propagated() {
        let call = Expr::Call(Box::new(Expr::GetLex("match".into())), "spawnEnemy".into(),
                              vec![Expr::GetLex("Rock".into())]);
        assert!(!call.is_propagatable(), "a call must not be inlined at each read");
        let ctor = Expr::New("Point".into(), vec![]);
        assert!(!ctor.is_propagatable(), "a constructor must not be inlined at each read");
    }

    /// Values that only READ still propagate, which is what keeps ordinary output readable.
    #[test]
    fn pure_values_still_propagate() {
        assert!(Expr::Num(3.0).is_propagatable());
        assert!(Expr::GetProperty(Box::new(Expr::This), "m_x".into()).is_propagatable());
        assert!(Expr::BinOp("+", Box::new(Expr::Num(1.0)), Box::new(Expr::Num(2.0))).is_propagatable());
        // ...but not once something effectful is nested inside one
        let impure = Expr::BinOp("+", Box::new(Expr::Num(1.0)),
            Box::new(Expr::Call(Box::new(Expr::This), "f".into(), vec![])));
        assert!(!impure.is_propagatable(), "an effect nested in an expression still counts");
    }

    #[test]
    fn empty_then_is_negated_and_swapped() {
        // if (x != 0) {} else { return; }  ->  if (x == 0) { return; }
        let cond = Expr::BinOp("!=", Box::new(Expr::GetLex("x".into())), Box::new(Expr::Num(0.0)));
        let rendered = render_stmts(&[make_if(cond, vec![], vec![Stmt::Return(None)])], 0);
        assert!(rendered.contains("=="), "condition not flipped to ==: {rendered}");
        assert!(!rendered.contains("!="), "stale != left: {rendered}");
        assert!(!rendered.contains("else"), "empty-then else not dropped: {rendered}");
    }

    #[test]
    fn normal_if_unchanged() {
        // Non-empty then is left as-is (no spurious negation).
        let cond = Expr::BinOp("==", Box::new(Expr::GetLex("y".into())), Box::new(Expr::Num(1.0)));
        let rendered = render_stmts(&[make_if(cond, vec![Stmt::Return(None)], vec![])], 0);
        assert!(rendered.contains("=="), "condition altered: {rendered}");
        assert!(!rendered.contains("!="), "spurious negation: {rendered}");
    }
}

#[cfg(test)]
mod loop_guard_tests {
    use super::{guard_loop_termination, render_stmts, Expr, Stmt};

    fn lt(lhs: Expr, rhs: Expr) -> Expr {
        Expr::BinOp("<", Box::new(lhs), Box::new(rhs))
    }
    fn render(stmts: Vec<Stmt>) -> String {
        let guarded = guard_loop_termination(stmts);
        render_stmts(&guarded, 0)
    }
    fn count(hay: &str, needle: &str) -> usize {
        hay.matches(needle).count()
    }

    // An un-renamed `_vN` counter with a CONSTANT bound (`while (_v4 < 8)`) and an
    // empty body would spin forever — the bytecode lost the increment. The guard
    // must inject `_v4 = _v4 + 1`. This is the exact pacman/link/peach freeze class
    // that the old i/j/k/l + `.length`-only matcher missed.
    #[test]
    fn vn_counter_constant_bound_gets_guarded() {
        let body = vec![Stmt::If(Expr::Bool(true), vec![], vec![])];
        let loop_ = Stmt::While(lt(Expr::Local(4), Expr::Num(8.0)), body);
        let out = render(vec![loop_]);
        assert!(out.contains("_v4 = _v4 + 1"), "missing injected advance:\n{out}");
    }

    // `while (_v7 < _v6 + 200)` — non-`.length`, non-constant bound — must also be
    // guarded (the peach case).
    #[test]
    fn vn_counter_expression_bound_gets_guarded() {
        let bound = Expr::BinOp("+", Box::new(Expr::Local(6)), Box::new(Expr::Num(200.0)));
        let loop_ = Stmt::While(lt(Expr::Local(7), bound), vec![Stmt::If(Expr::Bool(true), vec![], vec![])]);
        let out = render(vec![loop_]);
        assert!(out.contains("_v7 = _v7 + 1"), "missing injected advance:\n{out}");
    }

    // A loop whose only advance is INSIDE a branch (removeAllEffects: spins forever
    // on the non-advancing path) is still guarded — exactly one extra advance is
    // appended at the top level.
    #[test]
    fn branch_only_advance_gets_top_level_guard() {
        let body = vec![Stmt::If(
            Expr::Bool(true),
            vec![Stmt::Expr(Expr::GetLex("i = i + 1".into()))],
            vec![],
        )];
        let loop_ = Stmt::While(lt(Expr::GetLex("i".into()), Expr::GetLex("arr.length".into())), body);
        let out = render(vec![loop_]);
        assert_eq!(count(&out, "i = i + 1"), 2, "expected branch advance + injected guard:\n{out}");
    }

    // A correctly-formed loop that advances the counter at the TOP level (even when
    // the advance is not the LAST statement) must NOT be double-stepped.
    #[test]
    fn self_advancing_loop_not_double_stepped() {
        let body = vec![
            Stmt::Expr(Expr::GetLex("i = i + 1".into())),
            Stmt::Expr(Expr::GetLex("doStuff(i)".into())),
        ];
        let loop_ = Stmt::While(lt(Expr::GetLex("i".into()), Expr::GetLex("n".into())), body);
        let out = render(vec![loop_]);
        assert_eq!(count(&out, "i = i + 1"), 1, "working loop was double-stepped:\n{out}");
    }

    // A property/array-based condition (`self.index < n`) is NOT a bare-identifier
    // counter — we can't safely auto-advance it, so it's left untouched.
    #[test]
    fn property_condition_left_untouched() {
        let cond = lt(
            Expr::GetProperty(Box::new(Expr::This), "index".into()),
            Expr::GetLex("n".into()),
        );
        let loop_ = Stmt::While(cond, vec![Stmt::Expr(Expr::GetLex("tick()".into()))]);
        let out = render(vec![loop_]);
        assert!(!out.contains("+ 1"), "property-conditioned loop should be untouched:\n{out}");
    }

}

#[cfg(test)]
mod branch_op_tests {
    use super::{
        cmp_branch_op, instr_size, OP_IFFALSE, OP_IFGE, OP_IFGT, OP_IFLE, OP_IFLT, OP_IFNGE,
        OP_IFNGT, OP_IFNLE, OP_IFNLT, OP_IFTRUE, OP_RETURNVOID,
    };

    // The negated comparison branches carry a 3-byte signed offset like every other
    // branch. Sizing one as a bare opcode desyncs every instruction boundary after it,
    // and the method decodes as an empty body — which is how 45 of 46 corpus characters
    // were quietly losing chunks of their translated logic.
    #[test]
    fn negated_comparison_branches_are_four_bytes() {
        for op in [OP_IFNLT, OP_IFNLE, OP_IFNGT, OP_IFNGE] {
            let bc = [op, 0x11, 0x22, 0x33, OP_RETURNVOID];
            assert_eq!(
                instr_size(&bc, 0), 4,
                "opcode 0x{op:02x} must consume its s24 offset",
            );
        }
    }

    // Each negated branch is taken when its positive twin's condition is false, so it
    // renders as the plain inversion of that twin.
    #[test]
    fn negated_comparison_branches_invert_their_twin() {
        for (neg, inverse_of_neg) in
            [(OP_IFNLT, OP_IFGE), (OP_IFNLE, OP_IFGT), (OP_IFNGT, OP_IFLE), (OP_IFNGE, OP_IFLT)]
        {
            assert_eq!(cmp_branch_op(neg), cmp_branch_op(inverse_of_neg));
        }
        // and the twin it negates is NOT what it renders as
        assert_eq!(cmp_branch_op(OP_IFNLT), Some(">="));
        assert_eq!(cmp_branch_op(OP_IFLT), Some("<"));
        // iftrue/iffalse are single-operand, not comparisons
        assert_eq!(cmp_branch_op(OP_IFTRUE), None);
        assert_eq!(cmp_branch_op(OP_IFFALSE), None);
    }
}

/// Render one API call the way the emitter would, for tests that pin a conversion RULE rather
/// than a whole character. Arguments are given as source text and parsed back into the smallest
/// expression that represents them, so a caller writes `"17"` or `"self.getXSpeed()"` and gets
/// the same treatment the real path applies.
pub fn render_call_for_test(method: &str, args: &[&str]) -> String {
    let exprs: Vec<Expr> = args.iter()
        .map(|a| match a.trim().parse::<f64>() {
            Ok(n) => Expr::Num(n),
            Err(_) => Expr::GetLex(a.to_string()),
        })
        .collect();
    Expr::Call(Box::new(Expr::This), method.to_string(), exprs).render()
}
