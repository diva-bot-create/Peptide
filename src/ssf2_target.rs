//! ssf2_target — the SSF2 backend of [`crate::debug_target::DebugTarget`]. It
//! executes the SAME engine-agnostic commands as Fraymakers, but over AVM2
//! reflection (file-IPC), so `e match.getCharacters()[0].physics.currentVelocityX`,
//! `hold down+special`, `seq …`, `spawn sandbag` etc. work with identical syntax.
//!
//! The heart is [`Ssf2Target::eval`]: a small evaluator that parses the same
//! property-path / index / method-call / assignment syntax and walks it via the
//! engine's reflection verbs (`GC`/`GET`/`IDX`/`CALL`/`CALL1`/`SETP`/`READ`),
//! mapping the Fraymakers debug vocabulary (`match`, `p0`, `.body.x`,
//! `.physics.currentVelocityX`, `getCharacter(i)`, `getStateName()`) onto SSF2's
//! live match object graph (the per-player character nodes and their
//! `.X/.Y/.XSpeed/.YSpeed`, `.State`).

use anyhow::{anyhow, Result};
use std::time::Duration;

use crate::debug_target::DebugTarget;
use crate::interpreter::SpawnArgs;
use crate::ssf2_bridge::request;

const T: Duration = Duration::from_secs(4);

pub struct Ssf2Target;

impl Ssf2Target {
    pub fn new() -> Self { Ssf2Target }

    /// Send one reflection verb line ("VERB\ta1\ta2") and return the reply.
    fn op(&self, wire: &str) -> Result<String> { request(wire, T) }

    // ── live display-tree walk (the `tree` command) ────────────────────────

    /// Point the reflection cursor at the display node addressed by `path`
    /// (child indices from the document root).
    fn nav_node(&self, path: &[u32]) -> Result<()> {
        self.op("ROOT")?;
        for i in path { self.op(&format!("CALL1\tgetChildAt\t{i}"))?; }
        Ok(())
    }

    /// Read one property of the node at `path`. Re-navigates first (a GET moves
    /// the cursor onto the property). `"?"` on any error so a node that lacks the
    /// property (e.g. `currentFrame` on a Bitmap) doesn't abort the walk.
    fn node_prop(&self, path: &[u32], prop: &str) -> String {
        let r = (|| -> Result<String> {
            self.nav_node(path)?;
            self.op(&format!("GET\t{prop}"))?;
            self.op("READ")
        })();
        r.unwrap_or_else(|_| "?".into())
    }

    /// Recursive node visit: print this node's line, then its children (bounded by
    /// `max_depth` and a per-container child cap so a huge particle pool can't run away).
    fn walk_node(&self, path: &mut Vec<u32>, max_depth: u32, out: &mut String) -> Result<()> {
        self.nav_node(path)?;
        let class = self.op("READ").unwrap_or_else(|_| "?".into());
        let name = self.node_prop(path, "name");
        let x = self.node_prop(path, "x");
        let y = self.node_prop(path, "y");
        let w = self.node_prop(path, "width");
        let h = self.node_prop(path, "height");
        let vis = self.node_prop(path, "visible");
        let frame = self.node_prop(path, "currentFrame");
        let total = self.node_prop(path, "totalFrames");
        let label = self.node_prop(path, "currentFrameLabel");
        let indent = "  ".repeat(path.len());
        let idx = path.last().map(|i| format!("[{i}] ")).unwrap_or_default();
        let anim = if frame != "?" && frame != "undefined" {
            format!(" frame={frame}/{total}{}", if label != "?" && label != "null" && label != "undefined" { format!(" '{label}'") } else { String::new() })
        } else { String::new() };
        out.push_str(&format!("{indent}{idx}{class} \"{name}\" @({x},{y}) {w}x{h} vis={vis}{anim}\n"));
        if (path.len() as u32) < max_depth {
            let n: u32 = self.node_prop(path, "numChildren").parse().unwrap_or(0);
            for i in 0..n.min(48) {
                path.push(i);
                self.walk_node(path, max_depth, out)?;
                path.pop();
            }
            if n > 48 { out.push_str(&format!("{indent}  … {} more children elided\n", n - 48)); }
        }
        Ok(())
    }
}

impl Default for Ssf2Target { fn default() -> Self { Self::new() } }

// ─────────────────────────── expression model ─────────────────────────────

#[derive(Debug, Clone)]
enum Access {
    /// `name` / `name.member` after a root — a property get.
    Member(String),
    /// `[i]` or `(i)` index into a Vector/Array.
    Index(i64),
    /// `name(args)` — a method call (args are raw token strings).
    Call(String, Vec<String>),
}

/// Parse a dotted expression into (root, accessors, optional assignment value).
/// e.g. `match.getCharacter(0).body.x = 0`
fn parse_path(expr: &str) -> Result<(String, Vec<Access>, Option<String>)> {
    // split off a top-level assignment (`= value`)
    let (path, assign) = split_assign(expr);
    let mut chars = path.trim().chars().peekable();
    let mut accesses: Vec<Access> = Vec::new();

    // read an identifier
    let read_ident = |it: &mut std::iter::Peekable<std::str::Chars>| -> String {
        let mut s = String::new();
        while let Some(&c) = it.peek() {
            if c.is_alphanumeric() || c == '_' || c == '$' { s.push(c); it.next(); } else { break; }
        }
        s
    };

    // root identifier
    let root = read_ident(&mut chars);
    if root.is_empty() { return Err(anyhow!("expected an identifier at start of {expr:?}")); }

    loop {
        match chars.peek().copied() {
            None => break,
            Some('.') => {
                chars.next();
                let name = read_ident(&mut chars);
                if name.is_empty() { return Err(anyhow!("expected member after '.' in {expr:?}")); }
                if chars.peek() == Some(&'(') {
                    let args = read_args(&mut chars)?;
                    accesses.push(Access::Call(name, args));
                } else {
                    accesses.push(Access::Member(name));
                }
            }
            Some('[') => {
                chars.next();
                let mut inner = String::new();
                while let Some(&c) = chars.peek() { if c == ']' { break; } inner.push(c); chars.next(); }
                chars.next(); // ']'
                let i: i64 = inner.trim().parse().map_err(|_| anyhow!("non-integer index [{inner}]"))?;
                accesses.push(Access::Index(i));
            }
            Some(c) if c.is_whitespace() => { chars.next(); }
            Some(c) => return Err(anyhow!("unexpected char {c:?} in {expr:?}")),
        }
    }
    Ok((root, accesses, assign))
}

/// Split `lhs = rhs` at the first top-level `=` (not `==`/`!=`/`<=`/`>=`).
fn split_assign(expr: &str) -> (String, Option<String>) {
    let b = expr.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b'=' if depth == 0 => {
                let prev = if i > 0 { b[i - 1] } else { b' ' };
                let next = if i + 1 < b.len() { b[i + 1] } else { b' ' };
                if !matches!(prev, b'=' | b'!' | b'<' | b'>') && next != b'=' {
                    return (expr[..i].to_string(), Some(expr[i + 1..].trim().to_string()));
                }
            }
            _ => {}
        }
        i += 1;
    }
    (expr.to_string(), None)
}

/// Read a parenthesised, comma-separated argument list (consumes through ')').
fn read_args(it: &mut std::iter::Peekable<std::str::Chars>) -> Result<Vec<String>> {
    it.next(); // '('
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    loop {
        match it.next() {
            None => return Err(anyhow!("unterminated argument list")),
            // Keep nested ()/[] verbatim so an arg that is itself a call survives
            // (e.g. `kill` = `setY(getY() + 3000)`); only the OUTER ')' closes the list.
            Some(c @ ('(' | '[')) => { depth += 1; cur.push(c); }
            Some(')') if depth == 0 => { if !cur.trim().is_empty() { args.push(cur.trim().to_string()); } break; }
            Some(c @ (')' | ']')) => { depth -= 1; cur.push(c); }
            Some(',') if depth == 0 => { args.push(cur.trim().to_string()); cur.clear(); }
            Some(c) => cur.push(c),
        }
    }
    Ok(args)
}

/// Split `s` on `sep` at the TOP level only — separators inside `()`/`[]`/`{}` or a
/// string literal are left intact. Used to drive a `;`-joined multi-statement line
/// (`reset`) and an `[a, b, …]` array literal (`info`) one element at a time on SSF2.
/// Empty/whitespace pieces are dropped; each kept piece is trimmed.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q { quote = None; }
            continue;
        }
        match c {
            '"' | '\'' => { quote = Some(c); cur.push(c); }
            '(' | '[' | '{' => { depth += 1; cur.push(c); }
            ')' | ']' | '}' => { depth -= 1; cur.push(c); }
            _ if c == sep && depth == 0 => {
                if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() { out.push(cur.trim().to_string()); }
    out
}

/// Find a single top-level binary `+`/`-` in `s` (one that has a left operand — so a
/// leading sign or negative literal is NOT mistaken for an operator). Returns
/// `(op, lhs, rhs)`. Used to host-resolve a setter argument that's itself a read plus
/// an offset (`kill` = `setY(getY() + 3000)`).
fn split_binary(s: &str) -> Option<(char, &str, &str)> {
    let b = s.as_bytes();
    let mut depth = 0i32;
    let mut found = None;
    for i in 0..b.len() {
        match b[i] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'+' | b'-' if depth == 0 => {
                let prev = s[..i].trim_end().bytes().last();
                if matches!(prev, Some(c) if c.is_ascii_alphanumeric() || c == b')' || c == b']' || c == b'.') {
                    found = Some(i);
                }
            }
            _ => {}
        }
    }
    found.map(|p| (b[p] as char, &s[..p], &s[p + 1..]))
}

/// Format a resolved numeric value compactly: an integer-valued float prints without a
/// `.0` tail (so SETP gets `0`, not `0.0`), a fractional one prints normally.
fn fmt_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 { format!("{}", n as i64) } else { format!("{n}") }
}

/// Map a Fraymakers hitbox stat name to the SSF2 `AttackDamage` public setter that tune
/// drives, or `None` for a stat with no SSF2 analogue (skipped best-effort). The stored
/// `AttackBoxes["attackBox"]` is an AttackDamage (sealed) with public accessors
/// Damage/KBConstant/Power/Direction/WeightKB; mutating it persists (re-read each hit).
fn tune_accessor(fm: &str) -> Option<&'static str> {
    Some(match fm {
        "damage" => "Damage",
        "baseKnockback" => "KBConstant",
        "knockbackGrowth" => "Power",
        "angle" => "Direction",
        "weightKnockback" | "weightKB" => "WeightKB",
        "shieldDamage" => "ShieldDamage",
        _ => return None,
    })
}

// ─────────────────────────── reflection mapping ───────────────────────────

impl Ssf2Target {
    /// Map a root identifier to the reflection ops that set `cur` to it.
    /// Mirrors the Fraymakers commands.hsx bindings (`match`, `p0`, `p1`).
    /// Map a root identifier to the reflection ops that set `cur` to it, using the
    /// shared [`crate::vocab`] table so the SSF2↔Fraymakers mapping lives at the
    /// interpreter level, not hard-coded here. Unknown roots fall back to a member
    /// read off the document root.
    fn emit_root(&self, root: &str) -> Result<Vec<String>> {
        Ok(match crate::vocab::root_ssf2(root) {
            Some(ops) => ops.iter().map(|s| s.to_string()).collect(),
            None => vec!["ROOT".into(), format!("GET\t{root}")],
        })
    }

    /// Map a method call onto reflection ops, consulting [`crate::vocab::CALLS`]
    /// first (the `commands.hsx` character-access vocabulary) and falling back to a
    /// generic 0/1-arg reflection call for anything engine-agnostic.
    fn emit_call(&self, name: &str, args: &[String]) -> Result<Vec<String>> {
        use crate::vocab::CallLowering;
        if let Some(lowering) = crate::vocab::call_lowering(name) {
            return Ok(match lowering {
                CallLowering::Ops(ops) => ops.iter().map(|s| s.to_string()).collect(),
                CallLowering::OpsThenIndex(ops) => {
                    let i: i64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(0);
                    let mut v: Vec<String> = ops.iter().map(|s| s.to_string()).collect();
                    v.push(format!("IDX\t{i}"));
                    v
                }
            });
        }
        // generic: 0-arg or 1-(numeric)-arg method call by name
        Ok(match args.len() {
            0 => vec![format!("CALL\t{name}")],
            1 => vec![format!("CALL1\t{name}\t{}", args[0])], // numeric coerced engine-side
            _ => return Err(anyhow!("{name}(): only 0/1-arg method calls are supported via reflection")),
        })
    }
}

impl DebugTarget for Ssf2Target {
    fn engine(&self) -> &'static str { "ssf2" }

    /// Evaluate an expression by walking the object graph via reflection.
    fn eval(&mut self, expr: &str) -> Result<String> {
        let expr = expr.trim();
        if expr.is_empty() { return Ok(String::new()); }

        // bare verb passthrough: allow raw reflection verbs (GC/GET/READ/SPAWN/…)
        let head = expr.split_whitespace().next().unwrap_or("");
        if matches!(head, "GC"|"RM"|"STATS"|"MC"|"ROOT"|"GET"|"IDX"|"CALL"|"CALL1"|"CALLS"|"SETP"|"READ"|"PING"|"SPAWN"|"GO"|"QUEUE"|"LOADED"|"LOADNEXT") {
            return self.op(&expr.split_whitespace().collect::<Vec<_>>().join("\t"));
        }

        // Multi-statement line (`stmt; stmt; …`, e.g. the `reset` macro): SSF2's
        // reflection bridge runs one navigation at a time, so drive each statement
        // separately and join the replies, instead of the `;`-joined hscript line FM
        // forwards whole. (A single statement has no top-level `;` and falls through.)
        let stmts = split_top_level(expr, ';');
        if stmts.len() > 1 {
            let mut out = Vec::new();
            for s in &stmts {
                if s.is_empty() { continue; }
                out.push(self.eval(s)?);
            }
            return Ok(out.join("\n"));
        }

        // Array literal (`[a, b, …]`, e.g. the `info` macro): SSF2 can't eval an
        // array literal, so compose the readout host-side — evaluate each element via
        // reflection and join. A missing/sealed element degrades to `?` (e.g. p1 in a
        // 1-player match) rather than failing the whole readout.
        if expr.starts_with('[') && expr.ends_with(']') {
            let inner = &expr[1..expr.len() - 1];
            let parts: Vec<String> = split_top_level(inner, ',').iter()
                .map(|e| self.eval(e).map(|v| v.trim().to_string()).unwrap_or_else(|_| "?".into()))
                .collect();
            return Ok(format!("[{}]", parts.join(", ")));
        }

        // tune (`<player>.updateHitboxStats(<move>, {stat:val,…})`). FM addresses a live
        // hitbox by index; SSF2 addresses by MOVE NAME (its attacks are move-name + per-box
        // data), so on SSF2 the index slot carries a move name. We navigate cur to the
        // player, then mutate stored attack box 0 of that move via the TUNE verb (which
        // sets the AttackDamage public setters Damage/KBConstant/Direction/Power).
        if let Some(rest) = expr.split_once(".updateHitboxStats(") {
            return self.eval_tune(rest.0, rest.1);
        }

        // commands.hsx composite globals that aren't a plain navigation: handle
        // them whole so `log(x)` / `matchStatus()` produce the same effect SSF2-side
        // as the hscript helpers do on Fraymakers.
        if let Some(r) = self.eval_commands_hsx(expr)? { return Ok(r); }

        let (root, accesses, assign) = parse_path(expr)?;
        let mut ops = self.emit_root(&root)?;

        // walk accessors, folding FM passthrough wrappers (body/physics) via the
        // shared vocab table so the FM↔SSF2 naming lives at the interpreter level.
        let mut i = 0;
        while i < accesses.len() {
            match &accesses[i] {
                // `damage._damage` idiom → getDamage()/setDamage(v). FM reads/writes
                // the damage percent through this wrapper field; SSF2 uses methods.
                Access::Member(m) if m == "damage"
                    && matches!(accesses.get(i + 1), Some(Access::Member(n)) if n == "_damage") =>
                {
                    if assign.is_some() && i + 2 == accesses.len() {
                        let v = self.resolve_value(assign.as_ref().unwrap())?;
                        ops.push(format!("CALL1\t{}\t{v}", crate::vocab::DAMAGE_SETTER));
                        return self.run_ops(&ops, false);
                    }
                    ops.push(format!("CALL\t{}", crate::vocab::DAMAGE_GETTER));
                    i += 2; continue;
                }
                Access::Member(m) if crate::vocab::is_passthrough(m) && i + 1 < accesses.len() => {
                    // .body.x → X ; .physics.currentVelocityX → XSpeed
                    if let Access::Member(next) = &accesses[i + 1] {
                        ops.push(format!("GET\t{}", crate::vocab::member_alias(next)));
                        i += 2; continue;
                    }
                    ops.push(format!("GET\t{m}"));
                }
                Access::Member(m) => {
                    let mapped = crate::vocab::member_alias(m);
                    // an assignment targets the LAST member → SETP, handled after the loop
                    if assign.is_some() && i + 1 == accesses.len() {
                        let val = assign.as_ref().unwrap();
                        ops.push(format!("SETP\t{mapped}\t{val}"));
                        return self.run_ops(&ops, false);
                    }
                    ops.push(format!("GET\t{mapped}"));
                }
                Access::Index(n) => ops.push(format!("IDX\t{n}")),
                // Position/velocity setter call (setX/setY/setXVelocity/…) → property
                // SET, and toState(CState.X) → setState(<n>). FM exposes these as live
                // methods; SSF2 writes the property (or calls its state setter). The arg
                // may itself be a read (`kill` = setY(getY()+3000)), resolved host-side.
                Access::Call(name, args)
                    if i + 1 == accesses.len()
                        && (crate::vocab::setter_field(name).is_some() || name == "toState") =>
                {
                    if let Some(field) = crate::vocab::setter_field(name) {
                        let raw = args.first().map(String::as_str).unwrap_or("0");
                        let v = self.resolve_value(raw)?;
                        ops.push(format!("SETP\t{field}\t{v}"));
                    } else {
                        let raw = args.first().map(String::as_str).unwrap_or("");
                        let n = crate::vocab::cstate_value(raw)
                            .ok_or_else(|| anyhow!("toState({raw:?}): no SSF2 state mapping (only neutral STAND is mapped)"))?;
                        ops.push(format!("CALL1\t{}\t{n}", crate::vocab::STATE_SETTER));
                    }
                    return self.run_ops(&ops, false);
                }
                Access::Call(name, args) => ops.extend(self.emit_call(name, args)?),
            }
            i += 1;
        }
        self.run_ops(&ops, true)
    }

    fn spawn(&mut self, args: &SpawnArgs) -> Result<String> {
        // `--versus` (args.force_versus) is a no-op here BY DESIGN: SSF2 has no separate
        // training mode, every match is a versus Game, so the flag that forces versus on
        // Fraymakers is already the only mode on SSF2. Same engine-subset parity as assists
        // being Fraymakers-only and stamina being SSF2-only. We accept it and ignore it.
        let _ = args.force_versus;
        // Stage: caller-supplied, else the configured SSF2 stage (parity with how
        // Fraymakers resolves its stage from config — names differ per engine).
        let stage = args.stage.clone()
            .unwrap_or_else(|| crate::config::Config::load().ssf2_stage());
        // SHARED match rules (match_settings.conf — the same source Fraymakers bakes
        // into its start-match), so an SSF2 spawn plays by the same rules.
        let ms = crate::interpreter::load_match_settings();
        // Players: comma-separated character list; player 0 is the one you control,
        // the rest are idle dummies. (Assists are Fraymakers-only → ignored here.)
        let chars: Vec<String> = if args.characters.is_empty() {
            vec![args.character().to_string()]
        } else { args.characters.clone() };
        let n = chars.len().max(1);
        // Reserve one spare slot (capped at SSF2's 4-player max) so `addCharacter` has a
        // pre-allocated slot to fill live — the match's player containers are fixed-size
        // at startGame, so a slot can't be grown afterward. The spare is marked
        // exist=false + null character below, so startGame skips it (no ghost fighter).
        let slots = (n + 1).min(4);

        // SPAWN: build a VERSUS Game with `slots` player slots, set player 0's character,
        // queue stage+char0, load. (3rd arg = slot count → Game(slots, VERSUS).)
        self.op(&format!("SPAWN\t{}\t{}\t{}", chars[0], stage, slots))?;

        // Add players 1..N: each is an idle dummy — a human slot (so NO CPU AI) that
        // simply never receives input (only player 0 is driven by the input injector),
        // so it stands still. Set character (SETS = string set; SETP coerces to Number),
        // queue its resources, then re-drive the load for them.
        for (i, ch) in chars.iter().enumerate().skip(1) {
            let _ = self.op("GC"); let _ = self.op("GET\tcurrentGame"); let _ = self.op("GET\tPlayerSettings"); let _ = self.op(&format!("IDX\t{i}"));
            let _ = self.op(&format!("SETS\tcharacter\t{ch}"));
            let _ = self.op("SETP\thuman\t1");
            let _ = self.op("SETP\tcostume\t0");
            let _ = self.op(&format!("QUEUE1\t{ch}"));
        }
        if n > 1 { let _ = self.op("MLOAD"); }

        // Mark reserved spare slots [n..slots) as non-participating (exist=false, null
        // character) so startGame's makePlayer loop skips them and the match doesn't count
        // them as fighters. addCharacter later flips exist=true and fills one.
        for i in n..slots {
            let _ = self.op("GC"); let _ = self.op("GET\tcurrentGame"); let _ = self.op("GET\tPlayerSettings"); let _ = self.op(&format!("IDX\t{i}"));
            let _ = self.op("SETP\texist\t0"); // character stays null from PlayerSetting.init() → startGame skips it
        }

        // Apply the FULL match config on the REAL config objects BEFORE startMatch reads
        // them. Global rules live on Game.LevelData (GameSettings, lowercase slots); the
        // per-player STOCK COUNT lives on each PlayerSetting.lives (the source startMatch
        // inits from — Game.Lives is only a display mirror). Best-effort; never fail the
        // spawn over a single setting. cur stays put across the SETPs.
        let ut = if ms.time > 0 { 1 } else { 0 };
        let team = if ms.team_damage { 1 } else { 0 };
        let stam = if ms.using_stamina { 1 } else { 0 };
        let _ = self.op("GC"); let _ = self.op("GET\tcurrentGame"); let _ = self.op("GET\tLevelData");
        let _ = self.op(&format!("SETP\tlives\t{}", ms.lives));
        let _ = self.op("SETP\tusingLives\t1");
        let _ = self.op(&format!("SETP\ttime\t{}", ms.time));
        let _ = self.op(&format!("SETP\tusingTime\t{ut}"));
        let _ = self.op(&format!("SETP\tdamageRatio\t{}", ms.damage_ratio));
        let _ = self.op(&format!("SETP\tteamDamage\t{team}"));
        let _ = self.op(&format!("SETP\tusingStamina\t{stam}"));
        let _ = self.op(&format!("SETP\tstartStamina\t{}", ms.start_stamina));
        let _ = self.op(&format!("SETP\tstartDamage\t{}", ms.start_damage));
        let _ = self.op(&format!("SETP\tsizeRatio\t{}", ms.size_ratio));
        // per-player rules (lives = the real stock source; damage/start-damage)
        for i in 0..n {
            let _ = self.op("GC"); let _ = self.op("GET\tcurrentGame"); let _ = self.op("GET\tPlayerSettings"); let _ = self.op(&format!("IDX\t{i}"));
            let _ = self.op(&format!("SETP\tlives\t{}", ms.lives));
            let _ = self.op(&format!("SETP\tdamageRatio\t{}", ms.damage_ratio));
            let _ = self.op(&format!("SETP\tstartDamage\t{}", ms.start_damage));
        }
        // items (best-effort; ItemFrequency on the Game)
        let _ = self.op("GC"); let _ = self.op("GET\tcurrentGame");
        let _ = self.op(&format!("SETP\tItemFrequency\t{}", ms.item_frequency));

        // wait for the stage library to register (load-ready signal). More slack for
        // multiplayer (extra character resources to decrypt/parse).
        let mut ready = false;
        let iters = (40 + 15 * (n - 1)).max(40);
        for _ in 0..iters {
            std::thread::sleep(Duration::from_millis(400));
            if self.op(&format!("CALLS\tgetLibraryMC\tstage_{stage}")).is_ok()
                && self.op("READ").map(|r| r.contains("[object")).unwrap_or(false) { ready = true; break; }
            // (re-navigating cur each poll; CALLS sets cur, READ reads it)
            let _ = self.op("RM");
        }
        self.op("GO")?;

        Ok(format!("spawn {} on {} (versus, {} player(s): lives={}, time={}, damage_ratio={}, loaded={ready})",
                   chars.join(","), stage, n, ms.lives, ms.time, ms.damage_ratio))
    }

    fn hold(&mut self, mask: u32) -> Result<String> {
        // Translate the host control mask to SSF2's ControlsObject bit layout and set
        // it as the persistent held mask for the controlled player (p0 → Characters[0]).
        // The per-frame applicator (inject_input_applicator) drives it; release = mask 0.
        let ssf2 = crate::vocab::fm_mask_to_ssf2(mask);
        self.op(&format!("HOLD\t0\t{ssf2}"))?;
        Ok(format!("hold mask={mask} (ssf2 controls={ssf2}, p0)"))
    }

    fn seq(&mut self, masks: &[u32]) -> Result<String> {
        // Queue a frame-accurate input timeline for p0. Each host mask is translated to
        // SSF2's bit layout; the engine-side applicator drains one per frame and
        // auto-releases at the end (mirrors Fraymakers draining one `i` line per frame).
        let csv = masks.iter()
            .map(|m| crate::vocab::fm_mask_to_ssf2(*m).to_string())
            .collect::<Vec<_>>().join(",");
        self.op(&format!("SEQ\t0\t{csv}"))?;
        Ok(format!("seq {} frames (ssf2, p0)", masks.len()))
    }

    /// SSF2 has no debug console (the engine's interactive command console is a
    /// Fraymakers-only capability). Declare the gap explicitly rather than silently
    /// degrading to a reflection readout, mirroring how `char_icon` reports its gap.
    fn console(&mut self) -> Result<String> {
        Ok("console: SSF2 has no debug console (Fraymakers-only capability)\n".into())
    }

    /// Walk the LIVE display list from the document root — the stage-triage ground
    /// truth. Re-navigates per node by child-index path (the reflection cursor is
    /// stateful), reading each node's class/name/position/size/visibility/frame.
    /// Display `.x/.y` here are PARENT-RELATIVE display coords; game-space coords
    /// live on the game objects' capital fields (`.X`/`.Y`), readable via `e`.
    fn tree(&mut self, depth: u32) -> Result<String> {
        let mut out = String::new();
        let mut path: Vec<u32> = Vec::new();
        self.walk_node(&mut path, depth.max(1), &mut out)?;
        Ok(out)
    }

    /// Live add-player: drop the `sandbag` dummy into a RESERVED match slot. spawn reserves
    /// a spare slot (exist=false, null character); this finds it, loads the dummy's stats
    /// (Stats.getStats, what the Character ctor reads), then the ADDCHAR verb fills the slot
    /// and constructs the Character live (self-registering into the match). Returns the
    /// engine reply (ADDCHAR surfaces any engine-side error via the bridge).
    fn add_character(&mut self) -> Result<String> {
        let ch = "sandbag";
        // find the first reserved/empty slot (character is null) in PlayerSettings
        let slot = (0..4).find(|i| {
            let _ = self.op("GC"); let _ = self.op("GET\tcurrentGame"); let _ = self.op("GET\tPlayerSettings"); let _ = self.op(&format!("IDX\t{i}"));
            matches!(self.op("GET\tcharacter").and_then(|_| self.op("READ")),
                     Ok(r) if r == "null" || r == "undefined" || r.trim().is_empty())
        });
        let Some(slot) = slot else {
            return Ok("addCharacter: no free slot (match is full at 4 players). spawn fewer to leave room.".into());
        };
        let _ = self.op(&format!("QUEUE1\t{ch}"));
        let _ = self.op("MLOAD");
        // wait for the dummy's stats to load (exactly what the Character ctor reads)
        for _ in 0..30 {
            std::thread::sleep(Duration::from_millis(400));
            let _ = self.op("STATS");
            if self.op(&format!("CALLS\tgetStats\t{ch}")).is_ok()
                && self.op("READ").map(|r| r.contains("[object")).unwrap_or(false) { break; }
        }
        let r = self.op(&format!("ADDCHAR\t{ch}\t{slot}"))?;
        Ok(format!("addCharacter {ch} → slot {slot}: {r}"))
    }
    fn exit(&mut self) -> Result<()> { let _ = std::process::Command::new("pkill").args(["-f", "SSF2-patched"]).status(); Ok(()) }
    fn load(&mut self) -> Result<String> { self.op("LOADED") }

    /// SSF2 matchStatus: cheap when idle (short-circuit if no live match), else the
    /// composed `<id>|<dmg>|<anim>` feed read live from the engine.
    /// SSF2 has no script interpreter, so `animFeed(i)` can't be evaluated as hscript;
    /// the same fields are read by reflection instead and returned in the same shape.
    fn char_anim(&mut self, idx: usize) -> Result<Option<crate::debug_target::AnimState>> {
        if !self.match_live() { return Ok(None); }
        Ok(crate::debug_target::AnimState::parse(&self.compose_anim_feed(idx)?))
    }

    /// Open a recording window by TRUNCATING the file the injected ENTER_FRAME recorder
    /// appends to (`abc_inject::inject_frame_recorder`). The recorder always runs; the
    /// host decides what counts as a window, which means no engine-side gate and no
    /// engine round trip at all. `on: false` is a no-op — there's nothing to disarm.
    fn record(&mut self, on: bool) -> Result<String> {
        if on {
            crate::ssf2_bridge::frames_clear();
            Ok("record: window opened (engine pushes FRAME: telemetry)".into())
        } else {
            Ok("record: the SSF2 recorder always pushes — `trace` reads the window".into())
        }
    }

    /// Close the window: read every frame the engine recorded since `record`.
    ///
    /// This is the whole point. The engine is present at every frame and writes one line
    /// per frame, so a 14-frame attack yields 14 lines no matter how slow the host is —
    /// where polling for the same thing sampled about once per 14 frames and reported
    /// `frames_seen=0`. Reading is a file read, not an engine round trip.
    fn frame_trace(&mut self) -> Result<String> {
        // A no-op round trip first: the engine's pushes only reach the host when the
        // response reader is draining the socket, so ask it something to flush whatever
        // has queued up since the last command.
        let _ = self.eval_quiet("match.characterCount()");
        let pushed = crate::ssf2_bridge::frames_take();
        let lines: Vec<&str> = pushed.iter().map(|s| s.as_str()).filter(|l| !l.trim().is_empty()).collect();
        // collapse to one record per animation: which label, how many frames it held, and
        // where the character was when it ended. That's the comparable unit against
        // Fraymakers, not the raw per-frame dump.
        let mut runs: Vec<(String, usize, String)> = Vec::new();
        for l in &lines {
            let mut f = l.split('|');
            let (Some(label), Some(_frame), Some(x), Some(y)) = (f.next(), f.next(), f.next(), f.next())
            else { continue };
            let pos = format!("({x},{y})");
            match runs.last_mut() {
                Some((lab, n, p)) if lab == label => { *n += 1; *p = pos; }
                _ => runs.push((label.to_string(), 1, pos)),
            }
        }
        let mut out = format!("TRACE:{} frames, {} animations\n", lines.len(), runs.len());
        for (lab, n, pos) in &runs {
            out.push_str(&format!("  {lab} x{n} end={pos}\n"));
        }
        Ok(out)
    }

    fn match_status(&mut self) -> Result<Option<String>> {
        if !self.match_live() { return Ok(None); }
        let feed = self.compose_match_status()?;
        // "MATCHSTATUS:" with no records → treat as empty (no widget rows).
        Ok(if feed.trim_end() == "MATCHSTATUS:" { None } else { Some(feed) })
    }

    /// SSF2 has no stock-icon ripping pipeline yet, so there's no icon to supply —
    /// the matchStatus widget shows a glyph. (A genuine capability gap, declared
    /// explicitly rather than silently skipped in the GUI.)
    fn char_icon(&mut self, _slot: u32) -> Result<Option<String>> { Ok(None) }
}

impl Ssf2Target {
    /// Resolve a setter argument to a numeric string, host-side, so a setter whose
    /// argument is itself a read (`kill` = `setY(getY() + 3000)`) lowers to a single
    /// property SET. Handles a plain number, a `CState.<NAME>`, one top-level `+`/`-`
    /// between two operands, and otherwise a nested expression evaluated via reflection.
    fn resolve_value(&self, raw: &str) -> Result<String> {
        let s = raw.trim();
        if let Ok(n) = s.parse::<f64>() { return Ok(fmt_num(n)); }
        if let Some((op, lhs, rhs)) = split_binary(s) {
            let a: f64 = self.resolve_value(lhs)?.parse().map_err(|_| anyhow!("non-numeric operand {lhs:?}"))?;
            let b: f64 = self.resolve_value(rhs)?.parse().map_err(|_| anyhow!("non-numeric operand {rhs:?}"))?;
            return Ok(fmt_num(if op == '+' { a + b } else { a - b }));
        }
        if let Some(n) = crate::vocab::cstate_value(s) { return Ok(n.to_string()); }
        let r = self.eval_quiet(s)?;
        r.trim().parse::<f64>().map(fmt_num)
            .map_err(|_| anyhow!("value {s:?} did not evaluate to a number (got {r:?})"))
    }

    /// Run a sequence of navigation ops, then READ (or, for an assignment that
    /// already emitted SETP, just confirm).
    fn run_ops(&self, ops: &[String], read: bool) -> Result<String> {
        let mut last = String::new();
        for op in ops { last = self.op(op)?; }
        if read { self.op("READ") } else { Ok(last) }
    }

    /// tune on SSF2: `<player>` + the `updateHitboxStats(...)` arg text
    /// `<move>, { stat: val, … })`. Navigates the bridge's `peptideCur` to the player's
    /// Character, then for each stat sends a TUNE verb that mutates stored attack box 0 of
    /// the named move (SSF2 addresses attacks by move name, not a flat hitbox index).
    fn eval_tune(&self, player: &str, args: &str) -> Result<String> {
        let args = args.trim().trim_end_matches(')').trim();
        // split "<move>, { … }" at the first comma → move name + object text
        let (mv, obj) = args.split_once(',')
            .ok_or_else(|| anyhow!("tune: expected `<move>, {{stats}}` (got {args:?})"))?;
        let mv = mv.trim();
        let obj = obj.trim().trim_start_matches('{').trim_end_matches('}');
        // navigate cur → the target Character (p0/p1/…)
        for op in self.emit_root(player)? { self.op(&op)?; }
        // apply each stat that maps to an SSF2 AttackDamage setter
        let mut applied: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for pair in obj.split(',') {
            let pair = pair.trim();
            if pair.is_empty() { continue; }
            let (k, v) = pair.split_once(':')
                .ok_or_else(|| anyhow!("tune: bad stat `{pair}` (expected key:value)"))?;
            let (k, v) = (k.trim(), v.trim());
            match tune_accessor(k) {
                Some(acc) => {
                    // the TUNE verb reads the box stat back after setting; surface any
                    // engine-side error (e.g. an unknown move name → null) instead of
                    // falsely reporting success.
                    let r = self.op(&format!("TUNE\t{mv}\t{acc}\t{v}"))?;
                    if r.contains("ERR") {
                        return Err(anyhow!("tune {player} {mv}: {r} (is {mv:?} a valid move name?)"));
                    }
                    applied.push(format!("{k}={} (box0 {acc}={})", v, r.trim()));
                }
                None => skipped.push(k.to_string()),
            }
        }
        let mut out = format!("tune {player} {mv}: {}", if applied.is_empty() { "nothing applied".into() } else { applied.join(", ") });
        if !skipped.is_empty() { out.push_str(&format!(" (no SSF2 analogue: {})", skipped.join(", "))); }
        Ok(out)
    }

    /// Handle the `commands.hsx` globals that are whole helpers rather than a plain
    /// property navigation, so they produce the SAME effect on SSF2 as the hscript
    /// definitions do on Fraymakers. Returns `Ok(Some(result))` if it owned the
    /// expression, `Ok(None)` to let the generic evaluator handle it.
    ///
    ///   * `log(<expr>)`     → engine `trace(<expr>)`, replies `logged: <expr>`
    ///                         (commands.hsx: `__td.log(msg); return "logged: "+msg`).
    ///   * `matchStatus()`   → the `MATCHSTATUS:` host-poll feed, composed from
    ///                         per-character reflection reads (id|damage|animation),
    ///                         exactly the shape commands.hsx emits on Fraymakers.
    fn eval_commands_hsx(&self, expr: &str) -> Result<Option<String>> {
        let e = expr.trim();
        // log(<arg>) — strip the call and forward the literal argument text.
        if let Some(inner) = e.strip_prefix("log(").and_then(|s| s.strip_suffix(')')) {
            let msg = inner.trim().trim_matches(|c| c == '"' || c == '\'');
            return Ok(Some(self.op(&format!("LOG\t{msg}"))?));
        }
        // matchStatus() — build the feed host-side (no in-bytecode loop needed).
        if e == "matchStatus()" || e == "matchStatus" {
            return Ok(Some(self.compose_match_status()?));
        }
        // iconFeed(i) — SSF2 has no stock-icon ripping pipeline (unlike Fraymakers'
        // commands.hsx::iconFeed), so the feed is empty and the widget keeps its
        // glyph. Answered here so `e iconFeed(n)` behaves rather than throwing.
        if e.starts_with("iconFeed(") && e.ends_with(')') {
            let i = e["iconFeed(".len()..e.len()-1].trim();
            return Ok(Some(format!("ICON:{i}:")));
        }
        Ok(None)
    }

    /// Is a match live right now? A cheap, short-timeout probe used to skip the
    /// (multi-round-trip) matchStatus compose while sitting at the SSF2 menu, so the
    /// host poll doesn't stall on per-field timeouts when there's nothing to read.
    fn match_live(&self) -> bool {
        let t = Duration::from_millis(600);
        let _ = request("GC", t);
        let _ = request("GET\tstageData", t);
        matches!(request("READ", t), Ok(r) if r != "null" && r != "undefined" && !r.trim().is_empty())
    }

    /// Compose the `MATCHSTATUS:` feed from live reflection, mirroring
    /// `commands.hsx::matchStatus`: one `;`-separated `<id>|<damage>|<animation>`
    /// record per character. Every field is read defensively — a missing/sealed
    /// property degrades to `?`/`0` instead of throwing, so the feed never fails
    /// (a thrown poll on Fraymakers would land in chat; here it would time out).
    fn compose_match_status(&self) -> Result<String> {
        // character count = match.getCharacters().length
        let count: usize = self.eval_quiet("match.characterCount()")
            .ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let mut out = String::from("MATCHSTATUS:");
        for i in 0..count {
            if i > 0 { out.push(';'); }
            let base = format!("match.getCharacter({i})");
            // SSF2 accessors verified live against the engine Character (which
            // extends InteractiveSprite). The Fraymakers analogue is in
            // commands.hsx::matchStatus (gameObjectContent.id | damage._damage |
            // animation.currentAnimation). Each read falls back to the same
            // null-guard sentinel commands.hsx uses on any reflection error.
            let sane = |s: String| if s == "null" || s == "undefined" || s.is_empty() { None } else { Some(s) };
            let id   = self.eval_quiet(&format!("{base}.getLinkageID()")).ok().and_then(sane).unwrap_or_else(|| "?".into());
            let dmg  = self.eval_quiet(&format!("{base}.getDamage()")).ok().and_then(sane).unwrap_or_else(|| "0".into());
            let anim = self.eval_quiet(&format!("{base}.CurrentAnimation.Name")).ok().and_then(sane).unwrap_or_else(|| "?".into());
            out.push_str(&format!("{id}|{dmg}|{anim}"));
        }
        Ok(out)
    }

    /// `commands.hsx::animFeed`: the animation CLOCK for one character, in the shared
    /// `<anim>|<frame>|<total>|<x>|<y>|<state>` shape, read live off the character's
    /// MovieClip. Three ways the two engines disagree, all reconciled here so a
    /// cross-engine frame comparison actually compares the same thing:
    ///
    /// * **identity** — `currentLabel` is the timeline label ("stand"), which is what
    ///   Fraymakers reports as `currentAnimation`. `CurrentAnimation.Name` is the
    ///   hitbox-animation name ("mario_stand") and would never match.
    /// * **origin** — SSF2 MovieClip frames are 1-based, Fraymakers' are 0-based.
    /// * **total** — SSF2's `totalFrames` is the length of the character's WHOLE
    ///   timeline (every animation concatenated, e.g. 95), not of the current
    ///   animation; Fraymakers reports the current animation's length. There's no
    ///   cheap SSF2 equivalent, so this reports `-1` (unknown) and lets the watcher
    ///   end on a label change or a frame wrap instead of on a frame count. Reporting
    ///   SSF2's number here would make every animation look like it ended early.
    ///
    /// Position is the character's own `X`/`Y` fields, not `getX()` (which carries an
    /// offset).
    ///
    /// SAMPLING RATE — the reason a frame COUNT off this is not trustworthy. Every field
    /// is a separate synchronous reflection round trip over file-IPC, so one sample costs
    /// several, and measured against a live match a sample lands roughly every ~14 SSF2
    /// frames. A 14-frame attack is therefore over between two samples: `await` reports
    /// which animation played (correct) but `frames_seen=0` (useless). `state` is dropped
    /// from the sample for that reason — nothing decides on it, and it was pure latency.
    /// Comparing frame counts against Fraymakers needs ENGINE-SIDE recording (have SSF2
    /// log its own label+frame each tick, then read the trace), not faster polling.
    fn compose_anim_feed(&self, idx: usize) -> Result<String> {
        let base = format!("match.getCharacter({idx})");
        let sane = |s: String| {
            let t = s.trim().to_string();
            if t == "null" || t == "undefined" || t.is_empty() { None } else { Some(t) }
        };
        let Some(anim) = self.eval_quiet(&format!("{base}.MC.currentLabel")).ok().and_then(sane)
        else { return Ok(String::new()) };
        let num = |e: &str| self.eval_quiet(e).ok().and_then(sane)
            .and_then(|s| s.parse::<f64>().ok());
        let frame = num(&format!("{base}.MC.currentFrame")).map(|f| f - 1.0).unwrap_or(-1.0);
        let x = num(&format!("{base}.X")).unwrap_or(0.0);
        let y = num(&format!("{base}.Y")).unwrap_or(0.0);
        Ok(format!("{anim}|{frame}|-1|{x}|{y}|?"))
    }

    /// A character's current animation name, read live (None when there's no
    /// match or the read fails). The Fraymakers session gets per-frame `ANIM:`
    /// telemetry PUSHED by the engine; SSF2's bridge is synchronous RPC with
    /// nothing engine-initiated, so the session POLLS this instead to surface
    /// the same state-change feed in the log and overlay.
    pub fn current_anim(&self, idx: usize) -> Option<String> {
        let sane = |s: String| if s == "null" || s == "undefined" || s.is_empty() { None } else { Some(s) };
        self.eval_quiet(&format!("match.getCharacter({idx}).CurrentAnimation.Name"))
            .ok().and_then(sane)
    }

    /// Like `eval`, but never recurses into the commands.hsx interceptor and is
    /// used only for the small reads `match_status` composes. (Plain navigation;
    /// reflection errors surface as `Err` so the caller can substitute a sentinel.)
    fn eval_quiet(&self, expr: &str) -> Result<String> {
        let (root, accesses, _) = parse_path(expr)?;
        let mut ops = self.emit_root(&root)?;
        let mut i = 0;
        while i < accesses.len() {
            match &accesses[i] {
                Access::Member(m) if crate::vocab::is_passthrough(m) && i + 1 < accesses.len() => {
                    if let Access::Member(next) = &accesses[i + 1] {
                        ops.push(format!("GET\t{}", crate::vocab::member_alias(next)));
                        i += 2; continue;
                    }
                    ops.push(format!("GET\t{m}"));
                }
                Access::Member(m) => ops.push(format!("GET\t{}", crate::vocab::member_alias(m))),
                Access::Index(n) => ops.push(format!("IDX\t{n}")),
                Access::Call(name, args) => ops.extend(self.emit_call(name, args)?),
            }
            i += 1;
        }
        self.run_ops(&ops, true)
    }
}
