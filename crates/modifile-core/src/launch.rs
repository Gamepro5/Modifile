//! Starting the game.
//!
//! Modifile's first rule is that nothing runs while you play, and that rule
//! earns its keep — it is why a crashed mod manager cannot take your session
//! with it. But "I would like a Play button" is a reasonable thing to want,
//! and refusing it does not make the rule truer. So:
//!
//! - **Play is a shortcut, not a supervisor.** It activates the profile and
//!   starts the game. By default nothing of Modifile's is running afterwards,
//!   exactly as if you had launched from Steam.
//! - **Watching is opt-in and per profile.** With "return to vanilla when I
//!   quit" set, Modifile waits for the game to exit and then deactivates. What
//!   is running for that session is a process watcher, nothing more.
//! - **Losing the watcher loses nothing.** Kill Modifile mid-session and the
//!   mods stay installed — which is the ordinary activated state, not a broken
//!   one. There is no half-applied condition to recover from.
//!
//! ## What a pack may do
//!
//! A game pack is data contributed by strangers and cannot execute anything.
//! That rule is not relaxed here. A pack may name only:
//!
//! 1. a Steam app id, which starts the game through the OS URL handler, or
//! 2. a path *relative to the game directory the user themselves chose*,
//!    which must stay inside it.
//!
//! A free-form command line is a **user** setting, stored in Modifile's own
//! state and never readable from a pack. The difference matters: a malicious
//! pack that could name `cmd.exe /c ...` would turn "add this game" into
//! "run this code".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Context, Error, Result};
use crate::pack::Target;

/// How to start a game.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchMethod {
    /// Through Steam, which handles Proton, cloud saves and the overlay.
    /// Preferred wherever it applies, because launching the executable
    /// directly bypasses all of that.
    Steam { app_id: u32 },
    /// An executable inside the game directory.
    Exe { path: PathBuf },
    /// Whatever the user told us to run.
    Custom { command: String },
}

/// Extra arguments handed to the game at launch.
///
/// This is how an instance works: the game is told, for this run only, to read
/// its mods from somewhere else. Nothing is written to make it permanent, so
/// the same game started from Steam or a desktop shortcut is vanilla.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchArgs(pub Vec<String>);

impl LaunchArgs {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The UnityDoorstop arguments that point BepInEx at an instance.
    ///
    /// BepInEx works out its own root from where its preloader was loaded
    /// from, so naming an assembly inside the instance moves plugins, configs
    /// and patchers there with it. This is exactly what r2modman does, and the
    /// reason neither it nor this has to put anything in the game folder.
    pub fn doorstop(target_assembly: &Path) -> Self {
        Self(vec![
            "--doorstop-enabled".into(),
            "true".into(),
            "--doorstop-target-assembly".into(),
            target_assembly.to_string_lossy().into_owned(),
        ])
    }
}

impl LaunchMethod {
    /// A line describing what pressing Play will do, for saying so beforehand.
    pub fn describe(&self) -> String {
        match self {
            LaunchMethod::Steam { app_id } => {
                format!("through Steam (app {app_id})")
            }
            LaunchMethod::Exe { path } => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            LaunchMethod::Custom { command } => command.clone(),
        }
    }
}

/// Work out how to start this target, preferring the most correct route.
///
/// `custom` is the user's own command for this game, which wins over
/// everything: someone who has set one has a reason, usually a launcher this
/// knows nothing about.
pub fn resolve(target: &Target, root: &Path, custom: Option<&str>) -> Option<LaunchMethod> {
    if let Some(command) = custom.map(str::trim).filter(|c| !c.is_empty()) {
        return Some(LaunchMethod::Custom {
            command: command.to_string(),
        });
    }
    if let Some(steam) = &target.steam {
        return Some(LaunchMethod::Steam {
            app_id: steam.app_id,
        });
    }
    // Whatever the pack named, first one that is really there.
    target
        .launch
        .iter()
        .find_map(|name| safe_executable(root, name))
        .map(|path| LaunchMethod::Exe { path })
}

/// Resolve one pack-declared executable name against the game directory.
///
/// Everything here is a refusal. A pack may point at a file inside the folder
/// the user chose and nothing else, so: no absolute paths, no `..`, no drive
/// letters, no UNC, and the resolved path must still be under the root after
/// the filesystem has had its say — which is what catches a symlink pointing
/// somewhere else.
fn safe_executable(root: &Path, name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty()
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains("..")
        || name.chars().nth(1) == Some(':')
    {
        return None;
    }

    let candidate = root.join(name);
    if !candidate.is_file() {
        return None;
    }

    // Compare canonical forms so a link out of the tree is caught.
    let real_root = root.canonicalize().ok()?;
    let real = candidate.canonicalize().ok()?;
    if !real.starts_with(&real_root) {
        return None;
    }
    Some(real)
}

/// Start the game and return immediately.
///
/// Deliberately does not wait: the game is not our child process in any
/// meaningful sense, and holding a handle to it would make Modifile something
/// the game depends on.
pub fn spawn(method: &LaunchMethod, root: &Path, args: &LaunchArgs) -> Result<()> {
    match method {
        LaunchMethod::Steam { app_id } if args.is_empty() => {
            open_url(&format!("steam://rungameid/{app_id}"))
        }
        // Steam's URL scheme cannot carry arguments reliably, but its
        // executable can. Going through Steam rather than round it keeps
        // Proton, the overlay and playtime working, which launching the game
        // binary directly would all quietly lose.
        LaunchMethod::Steam { app_id } => {
            let Some(steam) = crate::steam::executable() else {
                return Err(Error::other(format!(
                    "this profile needs to start the game with extra arguments, and that \
                     has to go through Steam itself — which could not be found. Set a \
                     launch command for this game, or add `-applaunch {app_id} …` to \
                     Steam's own launch options for it."
                )));
            };
            let mut command = std::process::Command::new(&steam);
            command.arg("-applaunch").arg(app_id.to_string());
            command.args(&args.0);
            command
                .spawn()
                .map(|_| ())
                .ctx(format!("starting {} through Steam", app_id))
        }
        LaunchMethod::Exe { path } => {
            let mut command = std::process::Command::new(path);
            // Games routinely load assets by relative path.
            command.current_dir(path.parent().unwrap_or(root));
            command.args(&args.0);
            command
                .spawn()
                .map(|_| ())
                .ctx(format!("starting {}", path.display()))
        }
        LaunchMethod::Custom { command } => {
            // The user's own shell line, run the way they would run it. This
            // is only ever reachable from a setting they typed.
            let line = if args.is_empty() {
                command.clone()
            } else {
                format!("{command} {}", args.0.join(" "))
            };
            let command = &line;
            let mut process = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C", command]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c", command]);
                c
            };
            process.current_dir(root);
            process
                .spawn()
                .map(|_| ())
                .ctx(format!("running `{command}`"))
        }
    }
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(windows)]
    let mut command = {
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };

    command
        .spawn()
        .map(|_| ())
        .ctx(format!("opening {url}"))
}

// ---------------------------------------------------------------------------
// Watching a session
// ---------------------------------------------------------------------------

/// What happened while waiting for a game to finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEnd {
    /// It started and has now exited. The only case that should revert.
    Exited,
    /// It never appeared. Steam may have shown a dialog, or the launcher may
    /// still be sitting there waiting for a click. Reverting here would pull
    /// the mods out from under a game that is about to start.
    NeverStarted,
    /// Someone asked us to stop watching.
    Abandoned,
}

/// Wait for the game to start, then wait for it to stop.
///
/// Both halves matter. Launching through Steam returns instantly and the game
/// appears seconds later, so a watcher that only looked for "gone" would decide
/// the session was over before it began — and, with reverting on, would take
/// the mods out while the game was loading them.
///
/// `keep_going` is checked between polls so the caller can cancel.
///
/// Takes the "is it running" question as a closure rather than a path and a
/// process list. The transition it implements — not yet, then yes, then gone —
/// is the part worth getting right, and it is only testable if the answer can
/// be supplied. `watch_target` is the ordinary way in.
pub fn watch_with(
    is_running: &dyn Fn() -> bool,
    startup_grace: std::time::Duration,
    poll: std::time::Duration,
    keep_going: &dyn Fn() -> bool,
) -> SessionEnd {
    let deadline = std::time::Instant::now() + startup_grace;
    let mut started = false;

    loop {
        if !keep_going() {
            return SessionEnd::Abandoned;
        }
        let running = is_running();

        if running {
            started = true;
        } else if started {
            return SessionEnd::Exited;
        } else if std::time::Instant::now() >= deadline {
            return SessionEnd::NeverStarted;
        }

        std::thread::sleep(poll);
    }
}

/// Watch one target's game directory, the ordinary case.
pub fn watch(
    root: &Path,
    processes: &[String],
    startup_grace: std::time::Duration,
    poll: std::time::Duration,
    keep_going: &dyn Fn() -> bool,
) -> SessionEnd {
    watch_with(
        &|| crate::process::find_running(root, processes).is_some(),
        startup_grace,
        poll,
        keep_going,
    )
}

// ---------------------------------------------------------------------------
// The user's own launch commands
// ---------------------------------------------------------------------------

/// Per-game launch commands the user has set.
///
/// Kept apart from packs on purpose. A pack is data from a stranger and may not
/// name a command; this file is the user's own, and may.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaunchSettings {
    /// Game pack id -> command line.
    #[serde(default)]
    pub commands: std::collections::BTreeMap<String, String>,
}

impl LaunchSettings {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        crate::paths::write_atomic(path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn get(&self, game: &str) -> Option<&str> {
        self.commands.get(game).map(String::as_str).filter(|c| !c.trim().is_empty())
    }

    pub fn set(&mut self, game: &str, command: Option<&str>) {
        match command.map(str::trim).filter(|c| !c.is_empty()) {
            Some(command) => {
                self.commands.insert(game.to_string(), command.to_string());
            }
            None => {
                self.commands.remove(game);
            }
        }
    }
}

/// The error for a target nothing knows how to start.
pub fn no_method(game: &str, target: &str) -> Error {
    Error::other(format!(
        "nothing here knows how to start {target}. Its pack declares no Steam app id and \
         no executable, so there is nothing Modifile is allowed to guess at — a pack \
         cannot name a command to run. Set one yourself for `{game}` in Settings, or \
         launch the game the way you normally do; the mods are already in place either way."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "modifile-launch-{tag}-{}",
            crate::paths::now_millis()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_pack_cannot_escape_the_game_folder() {
        // The whole safety argument for letting packs name an executable.
        let root = scratch("escape");
        std::fs::write(root.join("game.exe"), b"x").unwrap();

        for evil in [
            "../../../Windows/System32/cmd.exe",
            "/bin/sh",
            "C:/Windows/System32/cmd.exe",
            "\\\\server\\share\\evil.exe",
            "subdir/../../escape.exe",
            "",
        ] {
            assert!(
                safe_executable(&root, evil).is_none(),
                "should refuse `{evil}`"
            );
        }

        // And the ordinary case still works.
        assert!(safe_executable(&root, "game.exe").is_some());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_named_executable_must_actually_exist() {
        let root = scratch("missing");
        assert!(safe_executable(&root, "not-here.exe").is_none());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn steam_wins_over_a_named_executable() {
        // Launching through Steam keeps Proton, the overlay and cloud saves;
        // running the binary directly loses all three.
        let root = scratch("steam");
        std::fs::write(root.join("game.exe"), b"x").unwrap();

        let mut target = Target {
            id: "client".into(),
            name: "Game".into(),
            kind: crate::pack::TargetKind::Client,
            flavor: None,
            asset_reject: vec![],
            markers: vec![],
            paths: Default::default(),
            processes: vec![],
            steam: Some(crate::pack::SteamHint {
                app_id: 42,
                dir: "Game".into(),
            }),
            candidates: vec![],
            launch: vec!["game.exe".into()],
        };

        assert_eq!(
            resolve(&target, &root, None),
            Some(LaunchMethod::Steam { app_id: 42 })
        );

        // Without Steam, the declared executable is used.
        target.steam = None;
        assert!(matches!(
            resolve(&target, &root, None),
            Some(LaunchMethod::Exe { .. })
        ));

        // The user's own command beats both.
        assert_eq!(
            resolve(&target, &root, Some("my-launcher --go")),
            Some(LaunchMethod::Custom {
                command: "my-launcher --go".into()
            })
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_target_with_nothing_declared_has_no_method() {
        let root = scratch("none");
        let target = Target {
            id: "client".into(),
            name: "Game".into(),
            kind: crate::pack::TargetKind::Client,
            flavor: None,
            asset_reject: vec![],
            markers: vec![],
            paths: Default::default(),
            processes: vec![],
            steam: None,
            candidates: vec![],
            launch: vec![],
        };
        assert_eq!(resolve(&target, &root, None), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_game_that_never_starts_is_not_a_finished_session() {
        // The case that would otherwise pull mods out from under a game still
        // loading: report it, and let the caller decline to revert.
        let root = scratch("never");
        let end = watch(
            &root,
            &["definitely-not-running-xyzzy.exe".to_string()],
            std::time::Duration::from_millis(120),
            std::time::Duration::from_millis(20),
            &|| true,
        );
        assert_eq!(end, SessionEnd::NeverStarted);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_session_is_start_then_stop_not_merely_stop() {
        // The transition that matters. A watcher that only looked for "gone"
        // would call the session finished before the game had appeared — and,
        // with reverting on, pull the mods out while it was still loading.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tick = AtomicUsize::new(0);

        let end = watch_with(
            // Not running, not running, running, running, gone.
            &|| matches!(tick.fetch_add(1, Ordering::Relaxed), 2 | 3),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(1),
            &|| true,
        );
        assert_eq!(end, SessionEnd::Exited);
        // It must not have given up during the two polls before it appeared.
        assert!(tick.load(Ordering::Relaxed) >= 5);
    }

    #[test]
    fn a_game_still_running_keeps_the_watch_open() {
        // Never returns Exited while the process is alive.
        let end = watch_with(
            &|| true,
            std::time::Duration::from_millis(50),
            std::time::Duration::from_millis(5),
            // Cancelled from outside, which is the only way out here.
            &{
                let count = std::cell::Cell::new(0);
                move || {
                    count.set(count.get() + 1);
                    count.get() < 6
                }
            },
        );
        assert_eq!(end, SessionEnd::Abandoned);
    }

    #[test]
    fn watching_can_be_cancelled() {
        let root = scratch("cancel");
        let end = watch(
            &root,
            &["nothing.exe".to_string()],
            std::time::Duration::from_secs(60),
            std::time::Duration::from_millis(10),
            &|| false,
        );
        assert_eq!(end, SessionEnd::Abandoned);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn launch_settings_round_trip() {
        let dir = scratch("settings");
        let path = dir.join("launch.json");

        let mut settings = LaunchSettings::default();
        settings.set("minecraft", Some("  java -jar launcher.jar  "));
        settings.set("valheim", Some("   "));
        settings.save(&path).unwrap();

        let back = LaunchSettings::load(&path);
        assert_eq!(back.get("minecraft"), Some("java -jar launcher.jar"));
        // Blank means "no command", not a command that is blank.
        assert_eq!(back.get("valheim"), None);

        let mut back = back;
        back.set("minecraft", None);
        assert_eq!(back.get("minecraft"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
