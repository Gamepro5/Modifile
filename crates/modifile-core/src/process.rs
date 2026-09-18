//! Is the game running right now?
//!
//! Changing mods under a live game is never safe. The game has its plugin DLLs
//! mapped into memory, it rewrites its config files on exit — which would
//! overwrite the profile state we just captured — and on Windows it holds file
//! handles that make a clean swap impossible anyway.
//!
//! Detection is deliberately conservative: it would rather refuse a legitimate
//! deploy than let one happen mid-session.

use std::path::Path;

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

#[derive(Debug, Clone)]
pub struct RunningGame {
    pub pid: u32,
    pub name: String,
    /// Why we think this process is the game, in words a user can act on.
    pub reason: String,
}

impl std::fmt::Display for RunningGame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (pid {}) — {}", self.name, self.pid, self.reason)
    }
}

/// Look for a process that is this game.
///
/// When the pack names the game's executables, those names are authoritative.
/// Matching on "anything running from the game folder" is tempting but wrong:
/// WoW ships `Utils/WowVoiceProxy.exe`, which keeps running long after the game
/// exits, and would block modding forever.
///
/// Only when a pack declares no names — Minecraft, whose process is `javaw.exe`
/// and where matching that would block on any Java program — do we fall back to
/// "running out of the game folder".
pub fn find_running(root: &Path, names: &[String]) -> Option<RunningGame> {
    let mut system = System::new();
    // Only what we need: paths and cwd, no CPU or memory sampling.
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always)
            .with_cwd(sysinfo::UpdateKind::Always),
    );

    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    for (pid, process) in system.processes() {
        let name = process.name().to_string_lossy().to_string();

        let under = |p: Option<&Path>| -> bool {
            p.and_then(|p| p.canonicalize().ok())
                .map(|p| p.starts_with(&root))
                .unwrap_or(false)
        };
        let path_known = process.exe().is_some() || process.cwd().is_some();
        let in_folder = under(process.exe()) || under(process.cwd());

        if names.is_empty() {
            if in_folder {
                return Some(RunningGame {
                    pid: pid.as_u32(),
                    name,
                    reason: "running from the game folder".to_string(),
                });
            }
            continue;
        }

        if names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            // A second copy of the game elsewhere should not block this one —
            // but if the OS will not tell us the path, trust the name.
            if in_folder || !path_known {
                return Some(RunningGame {
                    pid: pid.as_u32(),
                    name: name.clone(),
                    reason: format!("`{name}` is running"),
                });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn this_exe() -> (PathBuf, String) {
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().to_string();
        (exe.parent().unwrap().to_path_buf(), name)
    }

    #[test]
    fn blocks_when_a_named_process_runs_from_the_folder() {
        // The test runner itself is a real process in a real folder.
        let (dir, name) = this_exe();
        assert!(find_running(&dir, &[name]).is_some());
    }

    #[test]
    fn ignores_helper_processes_in_the_game_folder() {
        // Regression: WoW ships Utils/WowVoiceProxy.exe, which outlives the
        // game. Matching anything under the game folder blocked modding
        // permanently. Only declared executables count.
        let (dir, _) = this_exe();
        assert!(
            find_running(&dir, &["some-other-game.exe".to_string()]).is_none(),
            "a process in the folder that is not a declared executable must not block"
        );
    }

    #[test]
    fn falls_back_to_the_folder_when_no_names_are_declared() {
        // Minecraft's process is javaw.exe, so its pack declares no names and
        // relies on this.
        let (dir, _) = this_exe();
        assert!(find_running(&dir, &[]).is_some());
    }

    #[test]
    fn a_copy_of_the_game_elsewhere_does_not_block() {
        let (_, name) = this_exe();
        assert!(
            find_running(Path::new("/some/other/install"), &[name]).is_none(),
            "the same executable running from a different folder is a different install"
        );
    }

    #[test]
    fn quiet_when_nothing_matches() {
        assert!(find_running(
            Path::new("/no/such/folder/anywhere"),
            &["definitely-not-a-real-process-xyzzy".to_string()]
        )
        .is_none());
    }
}
