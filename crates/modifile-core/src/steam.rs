//! Steam library discovery.
//!
//! Enough VDF parsing to pull `"path"` values out of `libraryfolders.vdf` and
//! no more. A full VDF parser would be a dependency we do not need.

use std::path::PathBuf;

/// Every Steam library root on this machine, including the install root.
pub fn library_paths() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for steam in steam_roots() {
        if !steam.is_dir() {
            continue;
        }
        if !roots.contains(&steam) {
            roots.push(steam.clone());
        }
        let vdf = steam.join("steamapps").join("libraryfolders.vdf");
        let Ok(text) = std::fs::read_to_string(&vdf) else {
            continue;
        };
        for path in parse_library_paths(&text) {
            let path = PathBuf::from(path);
            if path.is_dir() && !roots.contains(&path) {
                roots.push(path);
            }
        }
    }
    roots
}

/// Where Steam itself might be installed.
fn steam_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: Option<PathBuf>| {
        if let Some(p) = p {
            out.push(p);
        }
    };

    if cfg!(windows) {
        for var in ["ProgramFiles(x86)", "ProgramFiles", "ProgramW6432"] {
            push(std::env::var_os(var).map(|v| PathBuf::from(v).join("Steam")));
        }
        // A Steam moved off C: still keeps its libraries listed in the VDF of
        // whichever install the user launches, so a miss here is recoverable.
        push(Some(PathBuf::from("C:\\Steam")));
    } else {
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            push(Some(home.join(".steam").join("steam")));
            push(Some(home.join(".steam").join("root")));
            push(Some(home.join(".local").join("share").join("Steam")));
            push(Some(
                home.join(".var")
                    .join("app")
                    .join("com.valvesoftware.Steam")
                    .join("data")
                    .join("Steam"),
            ));
        }
    }
    out
}

/// Pull the value of every `"path"` key. VDF escapes backslashes, so Windows
/// paths arrive doubled.
fn parse_library_paths(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("\"path\"") else {
            continue;
        };
        let Some(start) = rest.find('"') else { continue };
        let after = &rest[start + 1..];
        let Some(end) = after.find('"') else { continue };
        out.push(after[..end].replace("\\\\", "\\"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_library_paths;

    #[test]
    fn extracts_windows_and_unix_paths() {
        let vdf = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"C:\\Program Files (x86)\\Steam"
		"label"		""
	}
	"1"
	{
		"path"		"/home/user/games/SteamLibrary"
	}
}
"#;
        let paths = parse_library_paths(vdf);
        assert_eq!(
            paths,
            vec![
                "C:\\Program Files (x86)\\Steam".to_string(),
                "/home/user/games/SteamLibrary".to_string()
            ]
        );
    }

    #[test]
    fn ignores_other_keys() {
        assert!(parse_library_paths("\"contentid\"\t\"12345\"").is_empty());
    }
}
