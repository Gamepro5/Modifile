//! `modifile` — the command line front end.
//!
//! Everything the GUI does, this does first. The GUI is a view over the same
//! core, not a separate implementation.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use modifile_core::deploy::LinkMode;
use modifile_core::engine::{format_bytes, DeployOptions, Event};
use modifile_core::pack::Target;
use modifile_core::profile::ModEntry;
use modifile_core::{Engine, Lock, ModId, Paths, Profile, Result};

#[derive(Parser)]
#[command(
    name = "modifile",
    about = "Cross-game mod manager. Links mods into your game, then gets out of the way.",
    version
)]
struct Cli {
    /// Override the data directory (default: your platform's app data dir).
    #[arg(long, global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the data directory and write the bundled game packs.
    Init,
    /// List game packs and where each target was found on this machine.
    Games,
    /// Save a GitHub token. Raises the API budget from 60/hour to 5000/hour
    /// and makes cached revalidations free.
    Auth {
        /// Omit to read from stdin so the token stays out of your shell history.
        token: Option<String>,
    },
    /// Create a profile.
    New {
        name: String,
        #[arg(long)]
        game: String,
        /// Limit to specific targets, e.g. --target client --target server.
        #[arg(long = "target")]
        targets: Vec<String>,
    },
    /// List profiles.
    Profiles,
    /// Show a profile, its lock, and its deployment state.
    Show { profile: String },
    /// Add a mod, e.g. `modifile add wow-main WeakAuras/WeakAuras2`.
    Add {
        profile: String,
        /// `owner/repo`, `github:owner/repo`, or a GitHub URL.
        id: String,
        /// Restrict to specific targets.
        #[arg(long = "target")]
        targets: Vec<String>,
        /// Hold at an exact release tag.
        #[arg(long)]
        pin: Option<String>,
        /// Allow prereleases.
        #[arg(long)]
        prerelease: bool,
    },
    /// Remove a mod from a profile.
    Rm { profile: String, id: String },
    /// Point a target at a game directory autodetection missed.
    Root {
        profile: String,
        target: String,
        path: PathBuf,
    },
    /// Resolve the profile against GitHub and fetch anything missing.
    Sync { profile: String },
    /// Link the profile's mods into the game. Then launch the game however you
    /// like — nothing needs to stay running.
    Deploy {
        profile: String,
        /// Only this target.
        #[arg(long)]
        target: Option<String>,
        /// Show what would happen and stop.
        #[arg(long)]
        dry_run: bool,
        /// Overwrite files we did not place. Use after checking what they are.
        #[arg(long)]
        force: bool,
        /// For a game directory on another machine, which this cannot inspect:
        /// confirm you have stopped the server.
        #[arg(long)]
        confirm_stopped: bool,
    },
    /// Remove a deployment, leaving a clean game directory.
    Undeploy {
        game: String,
        target: String,
        /// Confirm a remote server is stopped.
        #[arg(long)]
        confirm_stopped: bool,
    },
    /// Check a deployment is still intact — this is how you find out a game
    /// patch clobbered your mods.
    Verify { profile: String },
    /// Inspect, reset, or import a profile's config files.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Delete store entries no profile references.
    Gc,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// List the config files this profile is keeping.
    Show { profile: String },
    /// Throw away saved configs so the next deploy restores the mods' shipped
    /// defaults. The defaults live in the store and are never lost.
    Reset {
        profile: String,
        #[arg(long)]
        target: Option<String>,
        /// Required, because this discards your edits.
        #[arg(long)]
        yes: bool,
    },
    /// Copy configs in from another profile, the game folder, or a backup.
    Import {
        profile: String,
        /// Another profile's name.
        #[arg(long, conflicts_with_all = ["from_game", "from_folder"])]
        from: Option<String>,
        /// Adopt whatever is in the game folder right now.
        #[arg(long, conflicts_with_all = ["from", "from_folder"])]
        from_game: bool,
        /// A folder on disk.
        #[arg(long, conflicts_with_all = ["from", "from_game"])]
        from_folder: Option<PathBuf>,
        #[arg(long)]
        target: Option<String>,
    },
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        let mut source = std::error::Error::source(&e);
        while let Some(inner) = source {
            eprintln!("  caused by: {inner}");
            source = inner.source();
        }
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = match &cli.home {
        Some(home) => Paths::rooted(home),
        None => Paths::discover()?,
    };
    paths.ensure()?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(modifile_core::Error::Io)?;

    let engine = Engine::open(paths.clone(), load_token(&paths))?;
    for (path, error) in &engine.pack_errors {
        eprintln!("warning: ignoring pack {}: {error}", path.display());
    }

    match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Games => cmd_games(&engine),
        Command::Auth { token } => cmd_auth(&paths, token),
        Command::New {
            name,
            game,
            targets,
        } => cmd_new(&engine, &name, &game, targets),
        Command::Profiles => cmd_profiles(&engine),
        Command::Show { profile } => cmd_show(&engine, &profile),
        Command::Add {
            profile,
            id,
            targets,
            pin,
            prerelease,
        } => cmd_add(&engine, &profile, &id, targets, pin, prerelease),
        Command::Rm { profile, id } => cmd_rm(&engine, &profile, &id),
        Command::Root {
            profile,
            target,
            path,
        } => cmd_root(&engine, &profile, &target, path),
        Command::Sync { profile } => runtime.block_on(cmd_sync(&engine, &profile)),
        Command::Deploy {
            profile,
            target,
            dry_run,
            force,
            confirm_stopped,
        } => cmd_deploy(
            &engine,
            &profile,
            target.as_deref(),
            dry_run,
            DeployOptions {
                force,
                assume_stopped: confirm_stopped,
            },
        ),
        Command::Undeploy {
            game,
            target,
            confirm_stopped,
        } => cmd_undeploy(
            &engine,
            &game,
            &target,
            DeployOptions {
                force: false,
                assume_stopped: confirm_stopped,
            },
        ),
        Command::Verify { profile } => cmd_verify(&engine, &profile),
        Command::Config { action } => cmd_config(&engine, action),
        Command::Gc => cmd_gc(&engine),
    }
}

fn cmd_config(engine: &Engine, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show { profile } => {
            let profile = load_profile(engine, &profile)?;
            let pack = engine.pack_for(&profile)?;
            let mut any = false;
            for (target, _) in engine.targets(pack, &profile) {
                let files = engine.saved_configs(pack, &profile.name, &target);
                if files.is_empty() {
                    continue;
                }
                any = true;
                println!("{} [{}]", target.name, target.kind.label());
                for file in files {
                    println!("  {}", file.display().to_string().replace('\\', "/"));
                }
            }
            if !any {
                println!(
                    "No saved configs yet. They are captured the first time you switch away \
                     from this profile or run `modifile undeploy`."
                );
            }
        }

        ConfigAction::Reset {
            profile,
            target,
            yes,
        } => {
            if !yes {
                return Err(modifile_core::Error::other(
                    "this discards your edited configs — pass --yes to confirm",
                ));
            }
            let loaded = load_profile(engine, &profile)?;
            let pack = engine.pack_for(&loaded)?;
            let mut total = 0;
            for (t, root) in engine.targets(pack, &loaded) {
                if target.as_deref().map(|o| o != t.id).unwrap_or(false) {
                    continue;
                }
                total += engine.reset_configs(pack, &loaded.name, &t, root.as_deref())?;
            }
            println!(
                "Discarded {total} saved config file(s). Run `modifile deploy {profile}` to \
                 restore the mods' defaults."
            );
        }

        ConfigAction::Import {
            profile,
            from,
            from_game,
            from_folder,
            target,
        } => {
            let source = match (from, from_game, from_folder) {
                (Some(other), _, _) => modifile_core::engine::ConfigSource::Profile(other),
                (_, true, _) => modifile_core::engine::ConfigSource::Game,
                (_, _, Some(path)) => modifile_core::engine::ConfigSource::Folder(path),
                _ => {
                    return Err(modifile_core::Error::other(
                        "pick a source: --from <profile>, --from-game, or --from-folder <path>",
                    ))
                }
            };
            let loaded = load_profile(engine, &profile)?;
            let pack = engine.pack_for(&loaded)?;
            let mut total = 0;
            for (t, root) in engine.targets(pack, &loaded) {
                if target.as_deref().map(|o| o != t.id).unwrap_or(false) {
                    continue;
                }
                total += engine.import_configs(pack, &loaded.name, &t, &source, root.as_deref())?;
            }
            println!("Imported {total} config file(s) into `{profile}`.");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn load_token(paths: &Paths) -> Option<String> {
    for var in ["MODIFILE_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }
    std::fs::read_to_string(paths.home.join("token"))
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn load_profile(engine: &Engine, name: &str) -> Result<Profile> {
    let path = engine.paths.profile_file(name);
    if !path.exists() {
        return Err(modifile_core::Error::NotFound(format!(
            "profile `{name}` — create it with `modifile new {name} --game <id>`"
        )));
    }
    Profile::load(&path)
}

fn cmd_init(paths: &Paths) -> Result<()> {
    let report = modifile_core::install_bundled_packs(paths)?;
    println!("data directory: {}", paths.home.display());
    println!(
        "packs:          {} ({} new, {} updated)",
        paths.packs.display(),
        report.written.len(),
        report.updated.len()
    );
    for name in &report.kept {
        println!("  kept your edited {name} (a newer bundled version exists)");
    }
    println!("store:          {}", paths.store.display());
    println!();
    println!("Next: modifile new my-profile --game valheim");
    Ok(())
}

fn cmd_auth(paths: &Paths, token: Option<String>) -> Result<()> {
    let token = match token {
        Some(t) => t,
        None => {
            eprintln!("Paste a GitHub token (it is only used for read-only API calls):");
            let mut buf = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buf)?;
            buf
        }
    };
    let token = token.trim();
    if token.is_empty() {
        return Err(modifile_core::Error::other("no token given"));
    }
    modifile_core::paths::write_atomic(&paths.home.join("token"), token.as_bytes())?;
    println!("Token saved. API budget is now 5000 requests/hour and revalidations are free.");
    Ok(())
}

fn cmd_games(engine: &Engine) -> Result<()> {
    if engine.packs.is_empty() {
        println!("No game packs. Run `modifile init`.");
        return Ok(());
    }
    for pack in &engine.packs {
        println!("{} ({})", pack.pack.game.name, pack.id());
        for target in &pack.pack.targets {
            let found = engine
                .roots
                .get(pack.id(), &target.id)
                .filter(|p| p.is_dir())
                .cloned()
                .or_else(|| pack.detect(target).into_iter().next());
            let location = match &found {
                Some(p) => p.display().to_string(),
                None => "not found — set one with `modifile root`".to_string(),
            };
            println!(
                "  {:<14} {:<7} {}",
                target.id,
                target.kind.label(),
                location
            );
            if let Some(running) = found
                .as_ref()
                .and_then(|root| modifile_core::Engine::running(target, root))
            {
                println!(
                    "  {:<14} {:<7} RUNNING: {running} — mod changes are blocked",
                    "", ""
                );
            }
        }
    }
    Ok(())
}

fn cmd_new(engine: &Engine, name: &str, game: &str, targets: Vec<String>) -> Result<()> {
    if engine.pack(game).is_none() {
        return Err(modifile_core::Error::NotFound(format!(
            "game pack `{game}` — see `modifile games`"
        )));
    }
    let path = engine.paths.profile_file(name);
    if path.exists() {
        return Err(modifile_core::Error::other(format!(
            "profile `{name}` already exists"
        )));
    }
    let mut profile = Profile::new(name, game);
    profile.targets = targets;
    profile.save(&path)?;
    println!("Created {}", path.display());
    Ok(())
}

fn cmd_profiles(engine: &Engine) -> Result<()> {
    let mut any = false;
    if let Ok(entries) = std::fs::read_dir(&engine.paths.profiles) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            if let Ok(profile) = Profile::load(&path) {
                let enabled = profile.mods.iter().filter(|m| m.enabled).count();
                println!(
                    "{:<20} {:<12} {} mod(s)",
                    profile.name, profile.game, enabled
                );
                any = true;
            }
        }
    }
    if !any {
        println!("No profiles yet. Try `modifile new my-profile --game valheim`.");
    }
    Ok(())
}

fn cmd_show(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let lock = Lock::load(&engine.paths.lock_file(name))?;

    println!("{} — {}", profile.name, pack.pack.game.name);
    println!();

    for (target, root) in engine.targets(pack, &profile) {
        let manifest = engine.manifest(pack.id(), &target.id)?;
        let state = match (&root, &manifest) {
            (None, _) => "no game directory".to_string(),
            (Some(_), None) => "not deployed".to_string(),
            (Some(_), Some(m)) => format!(
                "{} file(s) from `{}` via {}",
                m.files.len(),
                m.profile,
                m.mode.map(LinkMode::label).unwrap_or("?")
            ),
        };
        println!("  [{}] {} — {}", target.kind.label(), target.name, state);
        if let Some(root) = root {
            println!("       {}", root.display());
        }
    }

    println!();
    if profile.mods.is_empty() {
        println!("  no mods — add one with `modifile add {name} owner/repo`");
        return Ok(());
    }
    for entry in &profile.mods {
        let locked = lock.get(&entry.id);
        let version = locked.map(|l| l.version.as_str()).unwrap_or("unresolved");
        let trust = locked
            .map(|l| l.trust.level.label().to_string())
            .unwrap_or_else(|| "-".to_string());
        let flags = [
            (!entry.enabled).then_some("disabled"),
            entry.pin.as_ref().map(|_| "pinned"),
            entry.targets.as_ref().map(|_| "scoped"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");

        println!(
            "  {:<40} {:<16} {:<9} {}",
            entry.id.to_string(),
            version,
            trust,
            flags
        );
    }
    Ok(())
}

fn cmd_add(
    engine: &Engine,
    name: &str,
    id: &str,
    targets: Vec<String>,
    pin: Option<String>,
    prerelease: bool,
) -> Result<()> {
    let mut profile = load_profile(engine, name)?;
    let id: ModId = id.parse()?;

    let mut entry = ModEntry::new(id.clone());
    entry.targets = (!targets.is_empty()).then_some(targets);
    entry.pin = pin;
    entry.prerelease = prerelease;

    if !profile.add(entry) {
        println!("{id} is already in `{name}`");
        return Ok(());
    }
    profile.save(&engine.paths.profile_file(name))?;
    println!("Added {id} to `{name}`. Run `modifile sync {name}` to resolve it.");
    Ok(())
}

fn cmd_rm(engine: &Engine, name: &str, id: &str) -> Result<()> {
    let mut profile = load_profile(engine, name)?;
    let id: ModId = id.parse()?;
    if !profile.remove(&id) {
        return Err(modifile_core::Error::NotFound(format!("{id} in `{name}`")));
    }
    profile.save(&engine.paths.profile_file(name))?;
    println!("Removed {id}. Run `modifile deploy {name}` to take it out of the game.");
    Ok(())
}

fn cmd_root(engine: &Engine, name: &str, target: &str, path: PathBuf) -> Result<()> {
    let mut profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let Some(target_def) = pack.target(target) else {
        return Err(modifile_core::Error::NotFound(format!(
            "target `{target}` in pack `{}`",
            pack.id()
        )));
    };
    if !path.is_dir() {
        return Err(modifile_core::Error::NotFound(format!("{}", path.display())));
    }
    if !pack.matches_markers(target_def, &path) {
        eprintln!(
            "warning: {} does not look like {} (no marker file found), using it anyway",
            path.display(),
            target_def.name
        );
    }
    profile.roots.insert(target.to_string(), path.clone());
    profile.save(&engine.paths.profile_file(name))?;
    println!("`{target}` -> {}", path.display());
    Ok(())
}

async fn cmd_sync(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let lock_path = engine.paths.lock_file(name);
    let previous = Lock::load(&lock_path)?;

    if !engine.github.http().has_token() {
        eprintln!(
            "note: no GitHub token. Unauthenticated API budget is 60 requests/hour and even\n\
             cached revalidations spend it. Run `modifile auth` if you hit the ceiling."
        );
    }

    let reporter = Arc::new(|event: Event| match event {
        Event::Resolving(_) => {}
        Event::Resolved { id, version } => println!("  resolved {id} -> {version}"),
        Event::Downloading { id, asset, size } => {
            println!("  fetching {id}: {asset} ({})", format_bytes(size))
        }
        Event::Cached { id, version } => println!("  cached   {id} {version}"),
        Event::Installed { id, version, trust } => {
            println!("  ready    {id} {version} [{}]", trust.level.label())
        }
        Event::Failed { id, error } => eprintln!("  FAILED   {id}: {error}"),
    });

    println!("Syncing `{name}`...");
    let (lock, failures) = engine
        .sync(pack, &profile, &previous, Some(reporter))
        .await?;
    lock.save(&lock_path)?;

    println!();
    for entry in &lock.mods {
        for note in &entry.trust.notes {
            println!("note: {}: {note}", entry.id);
        }
    }
    println!(
        "{} mod(s) locked, {} failed.",
        lock.mods.len(),
        failures.len()
    );
    if !failures.is_empty() {
        for (id, error) in &failures {
            eprintln!("  {id}: {error}");
        }
    }
    println!("Next: modifile deploy {name}");
    Ok(())
}

fn cmd_deploy(
    engine: &Engine,
    name: &str,
    only: Option<&str>,
    dry_run: bool,
    options: DeployOptions,
) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let lock = Lock::load(&engine.paths.lock_file(name))?;
    if lock.mods.is_empty() && !profile.mods.is_empty() {
        return Err(modifile_core::Error::other(format!(
            "`{name}` has no lockfile — run `modifile sync {name}` first"
        )));
    }

    let mut deployed_any = false;
    for (target, root) in engine.targets(pack, &profile) {
        if only.map(|o| o != target.id).unwrap_or(false) {
            continue;
        }
        let Some(root) = root else {
            eprintln!(
                "skipping {}: no game directory (set one with `modifile root {name} {} <path>`)",
                target.name, target.id
            );
            continue;
        };

        let plan = engine.plan(pack, &profile, &lock, &target, &root)?;
        print_plan(&target, &plan);

        if dry_run {
            continue;
        }
        let report = engine.deploy(pack, &target, &plan, &profile.name, options)?;
        deployed_any = true;

        println!(
            "  {} file(s) linked via {} ({} on disk, 0 duplicated)",
            report.linked,
            report.mode.map(LinkMode::label).unwrap_or("?"),
            format_bytes(report.bytes)
        );
        if report.removed > 0 {
            println!("  {} file(s) from the previous profile removed", report.removed);
        }
        if report.captured > 0 {
            println!(
                "  {} config file(s) saved into the previous profile",
                report.captured
            );
        }
        if report.restored > 0 {
            println!("  {} config file(s) restored from this profile", report.restored);
        }
        if report.seeded > 0 {
            println!("  {} default config file(s) created", report.seeded);
        }
        for (path, reason) in &report.skipped {
            eprintln!("  skipped {}: {reason}", path.display());
        }
    }

    if deployed_any {
        println!();
        println!("Done. Launch the game normally — nothing needs to stay running.");
    }
    Ok(())
}

fn print_plan(target: &Target, plan: &modifile_core::deploy::Plan) {
    println!(
        "{} [{}] -> {}",
        target.name,
        target.kind.label(),
        plan.root.display()
    );
    println!(
        "  {} file(s), {}",
        plan.files.len(),
        format_bytes(plan.total_bytes())
    );
    for conflict in &plan.conflicts {
        println!(
            "  conflict {}: {} wins over {}",
            conflict.rel.display(),
            conflict.winner,
            conflict.losers.join(", ")
        );
    }
    for id in &plan.empty_mods {
        println!("  note: {id} contributed no files for this target");
    }
}

fn cmd_undeploy(
    engine: &Engine,
    game: &str,
    target: &str,
    options: DeployOptions,
) -> Result<()> {
    let report = engine.undeploy(game, target, options)?;
    println!("{} file(s) removed.", report.removed);
    for (path, reason) in &report.skipped {
        eprintln!("  left {}: {reason}", path.display());
    }
    Ok(())
}

fn cmd_verify(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let mut clean = true;

    for (target, _) in engine.targets(pack, &profile) {
        let Some(manifest) = engine.manifest(pack.id(), &target.id)? else {
            continue;
        };
        let report = modifile_core::deploy::verify(&manifest);
        println!(
            "{} [{}]: {} ok, {} missing, {} modified",
            target.name,
            target.kind.label(),
            report.ok,
            report.missing.len(),
            report.modified.len()
        );
        for path in report.missing.iter().chain(report.modified.iter()) {
            println!("  {}", path.display());
        }
        clean &= report.is_clean();
    }

    if !clean {
        println!();
        println!("A game update most likely overwrote these. Re-run `modifile deploy {name}`.");
    }
    Ok(())
}

fn cmd_gc(engine: &Engine) -> Result<()> {
    let before = engine.store.size_bytes();
    let (entries, bytes) = engine.gc()?;
    println!(
        "Removed {entries} unreferenced store entr(ies), freeing {}. Store is now {}.",
        format_bytes(bytes),
        format_bytes(before.saturating_sub(bytes))
    );
    Ok(())
}
