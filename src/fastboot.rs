//! fastboot — the SINGLE definition of "what a quick boot launches".
//!
//! A quick (fast / headless) boot lands the engine straight in a live match instead
//! of parking the bridge at READY. The start-match is socket-driven on both engines
//! (Fraymakers `s`, SSF2 `SPAWN`+`GO`), so SOMETHING has to fire it once the engine is
//! ready. That decision — *whether* to autostart, *which* character, and the exact
//! launch command — used to live in four places (FM CLI, SSF2 CLI, FM GUI page-JS,
//! SSF2 GUI host) that didn't even agree. This module is the one home for it, shared by
//! every frontend (CLI + GUI) and parameterized by engine. Tweak fastboot here and both
//! the CLI and the GUI move together.
//!
//! The command is expressed in the engine-agnostic `spawn …` vocabulary (the same one a
//! user types); each transport translates it on the way out (FM: `command_to_wire` →
//! socket; SSF2: `run_command` → reflection). So this module never touches a socket.
//!
//! ── Quick-boot feature parity (OOP seam: shared policy here, per-engine integrations) ──
//!
//! A quick boot is a SET of features. The decision/orchestration is engine-agnostic and
//! lives host-side (this module + the session layer); each feature's *implementation* is
//! owned by the platform-specific integration, because they're different bytecode VMs.
//! Keep this table honest when either engine's quick boot changes:
//!
//!   Feature                 Fraymakers (src/main.rs)             SSF2 (crates/…/abc_inject.rs)
//!   ─────────────────────   ──────────────────────────────────   ──────────────────────────────────
//!   skip menus / title      no-op the final boot load-step        inject_quickboot rewrites the
//!                           (skip Title + ugc load)                initial-menu entry point
//!   preload match assets     bake char + the `s` handler           inject_quickboot queues the
//!                           resolves + fetches stage/assist        char + stage assets up front
//!   loading presentation     the start-match path builds + shows   inject_quickboot shows a loading
//!     (boot → match)         a loading screen (NATIVE; see note)    screen, held until the match starts
//!   readiness signal         READY at boot complete                responsiveness heuristic
//!                                                                   (disclaimer is skipped)
//!   auto-spawn into match    `command(Fraymakers, …)` → `s`         `command(Ssf2, …)` → `spawn`
//!
//! NOTE on FM loading presentation: the start-match path already builds and shows the loading
//! screen on the quick-boot path, but it is not visibly held -- under investigation whether the
//! factory is unset on a fast boot or the screen merely flashes because the `s` handler
//! pre-fetches the match content. SSF2's is held because its on-demand asset decrypt genuinely
//! takes seconds.

use crate::config::Config;

/// Which engine a boot is targeting. The transport/streaming differences live in the
/// session layers; here we only need it to pick the right launch command shape.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Fraymakers,
    Ssf2,
}

/// The inputs that decide a quick boot, independent of CLI-vs-GUI. `char_name` is the
/// explicitly chosen character (CLI `--char`, GUI picked project); `None` falls back to
/// the configured default. `full` = a bare bridge boot (CLI `--full`/`--attach`, GUI
/// "regular boot") that must NOT autostart — you drive it by hand.
#[derive(Clone, Debug, Default)]
pub struct BootOptions {
    pub char_name: Option<String>,
    /// CLI `--stage`. Overrides the configured stage for BOTH the patch-time bake and the
    /// autostart command, so the stage that gets loaded is the stage that gets launched.
    pub stage_name: Option<String>,
    pub full: bool,
}

impl BootOptions {
    /// Build from CLI args. `--full`, `--no-boot`, and `--attach` all mean "bridge only,
    /// no autostart"; `--char X` names the character and `--stage X` the stage (else the
    /// config defaults are used).
    pub fn from_cli(args: &[String]) -> Self {
        let full = args.iter().any(|a| a == "--full" || a == "--no-boot" || a == "--attach");
        let char_name = arg_val(args, "--char");
        let stage_name = arg_val(args, "--stage");
        BootOptions { char_name, stage_name, full }
    }
}

/// The canonical start-match command for a quick boot, or `None` to NOT autostart
/// (a `--full`/bridge boot, or no character available). The ONE place this is decided.
///
/// The character/stage/assist all come from the same source the bake uses (CLI `--char`
/// or the config default + `Config::stage()`/`assist()`), so a fast boot launches exactly
/// what was baked. Stage/assist are made EXPLICIT for Fraymakers on purpose: the engine's
/// `s` handler only fetch-loads the args it's given, so omitting them leaves their content
/// refs null → the null-namespace crash. SSF2 resolves its stage from config inside
/// `Ssf2Target::spawn`, so only the character is needed.
pub fn command(engine: Engine, opts: &BootOptions) -> Option<String> {
    if opts.full {
        return None;
    }
    let cfg = Config::load();
    // CLI `--char` may itself be a comma roster; else fall back to the configured roster
    // (`FRAY_ROSTER`/config `roster`, single `current_char` when unset).
    let ch = opts.char_name.clone().unwrap_or_else(|| cfg.roster());
    let ch = ch.trim();
    if ch.is_empty() {
        return None;
    }
    Some(match engine {
        // Spawn the full roster (up to 4 players). A SINGLE char is mirrored to an explicit
        // 2-char roster: a lone player lets the engine fall back to its native default player 2,
        // which freezes the match on frame 1; an explicit player 2 loads fine. A multi-char
        // roster is passed through untouched.
        Engine::Fraymakers => {
            let roster = if ch.contains(',') { ch.to_string() } else { format!("{ch},{ch}") };
            let stage = opts.stage_name.clone().unwrap_or_else(|| cfg.stage());
            format!("spawn {roster} {stage} {}", cfg.assist())
        }
        // SSF2 also supports the full roster (Ssf2Target::spawn builds an N-slot versus Game),
        // so pass the comma roster through unchanged. A `--stage` is passed on when given, the
        // same as for Fraymakers — without it the two engines boot different stages from the
        // same command, which makes a cross-engine comparison compare the wrong things.
        // Assists are Fraymakers-only, so there's no third token here.
        Engine::Ssf2 => match opts.stage_name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(stage) => format!("spawn {ch} {stage}"),
            None => format!("spawn {ch}"),
        },
    })
}

/// `--key value` lookup (shared shape with the session arg parsers).
fn arg_val(args: &[String], key: &str) -> Option<String> {
    args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn from_cli_full_means_no_autostart() {
        for flag in ["--full", "--no-boot", "--attach"] {
            assert!(BootOptions::from_cli(&argv(&[flag])).full, "{flag} should mean full/no-autostart");
        }
        assert!(!BootOptions::from_cli(&argv(&["--char", "mario"])).full);
        assert_eq!(BootOptions::from_cli(&argv(&["--char", "mario"])).char_name.as_deref(), Some("mario"));
    }

    #[test]
    fn stage_arg_reaches_both_engines() {
        // A cross-engine comparison is only valid if both engines booted the SAME stage.
        let opts = BootOptions::from_cli(&argv(&["--char", "mario", "--stage", "bowserscastle"]));
        for engine in [Engine::Fraymakers, Engine::Ssf2] {
            let cmd = command(engine, &opts).expect("should autostart");
            assert!(cmd.contains("bowserscastle"), "{engine:?} dropped the stage: {cmd}");
        }
    }

    #[test]
    fn stage_arg_drives_the_autostart_stage() {
        // `--stage` has to reach BOTH the bake and the launch: the patcher's custom-stage
        // self-bootstrap keys off the baked name, so a stage that's baked but not launched
        // (or launched but not baked) leaves setupStage with a null resource.
        let opts = BootOptions::from_cli(&argv(&["--char", "mario", "--stage", "bowserscastlessf2"]));
        assert_eq!(opts.stage_name.as_deref(), Some("bowserscastlessf2"));
        let cmd = command(Engine::Fraymakers, &opts).expect("should autostart");
        assert!(cmd.contains("bowserscastlessf2"), "stage missing from autostart: {cmd}");
    }

    #[test]
    fn full_boot_never_autostarts() {
        let opts = BootOptions { char_name: Some("mario".into()), stage_name: None, full: true };
        assert_eq!(command(Engine::Fraymakers, &opts), None);
        assert_eq!(command(Engine::Ssf2, &opts), None);
    }

    #[test]
    fn empty_char_never_autostarts() {
        let opts = BootOptions { char_name: Some("   ".into()), stage_name: None, full: false };
        assert_eq!(command(Engine::Fraymakers, &opts), None);
        assert_eq!(command(Engine::Ssf2, &opts), None);
    }

    #[test]
    fn ssf2_quick_boot_is_char_only() {
        // SSF2 resolves stage/assist inside its own spawn, so the command is char-only
        // and config-independent.
        let opts = BootOptions { char_name: Some("mario".into()), stage_name: None, full: false };
        assert_eq!(command(Engine::Ssf2, &opts).as_deref(), Some("spawn mario"));
    }

    #[test]
    fn fraymakers_quick_boot_makes_stage_assist_and_player2_explicit() {
        // FM's `s` handler only fetch-loads the args it's given, so the command must carry an
        // explicit stage + assist, AND an explicit player-2 roster (a single char lets the
        // engine fall back to a native default p2 that freezes the match). 4 tokens: spawn,
        // <char>,<char>, stage, assist.
        let opts = BootOptions { char_name: Some("mario".into()), stage_name: None, full: false };
        let cmd = command(Engine::Fraymakers, &opts).expect("a char + non-full boot autostarts");
        assert!(cmd.starts_with("spawn mario,mario "), "got: {cmd}");
        assert_eq!(cmd.split_whitespace().count(), 4, "spawn + roster + stage + assist; got: {cmd}");
    }

    #[test]
    fn fraymakers_quick_boot_keeps_an_explicit_roster() {
        let opts = BootOptions { char_name: Some("mario,zelda".into()), stage_name: None, full: false };
        let cmd = command(Engine::Fraymakers, &opts).expect("autostarts");
        assert!(cmd.starts_with("spawn mario,zelda "), "an explicit roster is passed through: {cmd}");
    }

    #[test]
    fn fraymakers_quick_boot_passes_a_full_four_player_roster() {
        // A 4-char roster is passed through verbatim (no mirroring); stage+assist still
        // get appended so the engine `s` handler keeps the extra players at parts[4+].
        let opts = BootOptions { char_name: Some("sandbag,mario,sandbag,mario".into()), stage_name: None, full: false };
        let cmd = command(Engine::Fraymakers, &opts).expect("autostarts");
        assert!(cmd.starts_with("spawn sandbag,mario,sandbag,mario "), "got: {cmd}");
        assert_eq!(cmd.split_whitespace().count(), 4, "spawn + roster + stage + assist; got: {cmd}");
    }

    #[test]
    fn ssf2_quick_boot_passes_the_full_roster() {
        // SSF2's backend builds an N-slot versus Game, so a multi-char roster boots the whole
        // match (no stage/assist tokens — SSF2 resolves the stage from config in its own spawn).
        let opts = BootOptions { char_name: Some("mario,zelda,kirby".into()), stage_name: None, full: false };
        assert_eq!(command(Engine::Ssf2, &opts).as_deref(), Some("spawn mario,zelda,kirby"));
    }
}
