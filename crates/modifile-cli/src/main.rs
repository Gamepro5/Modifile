//! `modifile` — the command line front end.
//!
//! Everything the GUI does, this does first. The GUI is a view over the same
//! core, not a separate implementation.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use modifile_core::deploy::LinkMode;
use modifile_core::engine::{format_bytes, DeployOptions, Event, ModpackSource};
use modifile_core::pack::Target;
use modifile_core::profile::{ModEntry, ProfileId};
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
        /// Save a CurseForge API key instead. You obtain this yourself from
        /// Overwolf; it cannot be shipped with an open-source app.
        #[arg(long)]
        curseforge: bool,
        /// Fetch CurseForge files whose authors disabled third-party downloads.
        ///
        /// The bytes are the same ones your browser would get, but the API
        /// withholds the URL deliberately and your key's terms cover this — if
        /// it is noticed, the key you obtained is what gets revoked. Your call.
        #[arg(long)]
        curseforge_direct: Option<bool>,
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
    /// Show a profile's mods, versions and whether it is active.
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
    /// Add a mod from a file you downloaded yourself.
    ///
    /// For mods no API will hand over — a CurseForge project whose author
    /// disabled third-party downloads, a private beta, your own build. Once
    /// imported it is managed like any other mod; only updates stay manual.
    AddFile {
        profile: String,
        file: PathBuf,
        /// Attach it to a known project, e.g. `curseforge:12345`, so the list
        /// shows what it actually is.
        #[arg(long)]
        id: Option<String>,
    },
    /// Remove a mod from a profile.
    Rm { profile: String, id: String },
    /// Point a target at a game directory autodetection missed.
    Root {
        profile: String,
        target: String,
        path: PathBuf,
    },
    /// Check each mod's source for new versions and download anything missing.
    ///
    /// One profile by default. `--all` does every profile of every game, which
    /// is a thing you should have to ask for: it is a lot of downloading, and
    /// it changes profiles you were not looking at.
    #[command(alias = "sync")]
    Update {
        /// The profile to update. Omit it with `--all`.
        profile: Option<String>,
        /// Update every profile of every game.
        #[arg(long)]
        all: bool,
    },
    /// Put this profile's mods into the game. Modifile is not a launcher —
    /// afterwards you start the game however you normally do.
    #[command(alias = "deploy")]
    Activate {
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
    /// Take a profile's mods back out, leaving the game vanilla.
    #[command(alias = "undeploy")]
    Deactivate {
        game: String,
        target: String,
        /// Confirm a remote server is stopped.
        #[arg(long)]
        confirm_stopped: bool,
        /// Also delete installed files that have changed since — a build you
        /// dropped in by hand, or a file a game update overwrote.
        #[arg(long)]
        force: bool,
    },
    /// Install or check this profile's mod loader (Fabric, Quilt, …).
    Loader {
        profile: String,
        /// Install it rather than only reporting what is there.
        #[arg(long)]
        install: bool,
    },
    /// Rename a profile, keeping its mods, lock and configs.
    Rename { from: String, to: String },
    /// Delete a profile, its versions and its saved settings.
    ///
    /// Downloads are shared by hash with every other profile, so they are left
    /// alone; `modifile gc` is what clears the ones nothing wants.
    Delete {
        profile: String,
        /// Required, because nothing here can be rebuilt afterwards.
        #[arg(long)]
        yes: bool,
    },
    /// List a mod's releases, and which of them this game can install.
    ///
    /// The companion to `--pin`: it is hard to hold a mod at a version whose
    /// tag you have to guess.
    Versions { profile: String, id: String },
    /// Hold a mod at a version, or release it to follow the newest again.
    Hold {
        profile: String,
        id: String,
        /// The release tag to hold at. Omit with --latest to clear it.
        version: Option<String>,
        /// Follow the newest release again.
        #[arg(long)]
        latest: bool,
    },
    /// Set which game version and mod loader a profile is for.
    Set {
        profile: String,
        /// e.g. fabric, forge, neoforge, quilt.
        #[arg(long)]
        loader: Option<String>,
        /// e.g. 1.20.1.
        #[arg(long)]
        game_version: Option<String>,
    },
    /// Write a profile to a file you can send to a friend.
    Export {
        profile: String,
        /// Defaults to <profile>.mfpack in the current directory.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Leave your config files out of the pack.
        #[arg(long)]
        no_configs: bool,
        /// A line describing the setup, shown on import.
        #[arg(long, default_value = "")]
        note: String,
    },
    /// Create a profile from a file someone sent you.
    Import {
        file: PathBuf,
        /// Name it something other than the exporter's name.
        #[arg(long)]
        name: Option<String>,
        /// Take the newest release of each mod instead of the exact versions
        /// the exporter was running.
        #[arg(long)]
        latest: bool,
    },
    /// Check a deployment is still intact — this is how you find out a game
    /// patch clobbered your mods.
    Verify { profile: String },
    /// Put changed or missing files back to what the lockfile says.
    ///
    /// Checksums every stored copy first, because a deployed file is a hard
    /// link to the stored one: whatever damaged the game's copy in place
    /// damaged the store's copy too. Anything failing its checksum is thrown
    /// away and downloaded again.
    Repair { profile: String },
    /// Inspect, reset, or import a profile's config files.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Show which game packs are current, and refresh them.
    Packs {
        /// Replace packs even when they cannot be proven untouched. The old
        /// copy is saved alongside as `<name>.toml.bak`.
        #[arg(long)]
        refresh: bool,
    },
    /// Find a mod by name, and where its source code lives.
    ///
    /// Searches Modrinth, which is keyless and publishes each project's source
    /// repository — the practical way to find the GitHub for a mod you only
    /// know from a CurseForge page.
    Search {
        /// Words to look for.
        query: Vec<String>,
        /// Narrow to a profile's game version and loader.
        #[arg(long)]
        profile: Option<String>,
    },
    /// Import or inspect a modpack.
    ///
    /// A modpack is a profile somebody else assembled — a game version, a
    /// loader, a pinned mod list and a tree of config files. Importing one
    /// writes exactly that and stops; the mods are fetched by the next
    /// `modifile update`, through the same verification and trust checks
    /// every other mod goes through.
    Pack {
        #[command(subcommand)]
        action: PackAction,
    },
    /// Activate a profile and start the game.
    ///
    /// A convenience, not a supervisor. By default this activates, launches,
    /// and exits — nothing of Modifile's is left running, exactly as if you
    /// had started the game from Steam.
    ///
    /// With `--revert-on-exit` it instead waits for the game to close and then
    /// deactivates, so the game is vanilla again afterwards. Killing Modifile
    /// during that wait leaves the mods installed, which is the ordinary
    /// activated state — there is nothing half-applied to recover from.
    Play {
        profile: String,
        /// Which target to start. Defaults to the client.
        #[arg(long)]
        target: Option<String>,
        /// Obsolete, and accepted only so an old command line still runs.
        /// Play no longer modifies the game install, so there is nothing to
        /// revert.
        #[arg(long, hide = true)]
        revert_on_exit: bool,
        /// Set the command used to start this profile's game, instead of
        /// playing. Use an empty string to clear it.
        #[arg(long)]
        set_command: Option<String>,
    },
    /// Update Modifile itself.
    ///
    /// Checks the project's GitHub releases for a newer version, verifies the
    /// download against the digest GitHub publishes for it, and swaps the
    /// binaries in place. The previous ones are kept aside until the next run
    /// in case the swap goes wrong.
    #[command(alias = "self-update")]
    Selfupdate {
        /// Report what is available and stop.
        #[arg(long)]
        check: bool,
        /// Install without asking.
        #[arg(long)]
        yes: bool,
        /// Look for new versions on startup. Off means never check.
        #[arg(long)]
        check_on_startup: Option<bool>,
        /// Install updates without asking, whenever one is found.
        #[arg(long)]
        automatic: Option<bool>,
    },
    /// Show or change what Modifile is willing to install.
    ///
    /// By default it refuses a mod that ships a compiled file, publishes no
    /// source anywhere and declares no licence — there is simply nothing to
    /// check. That is the right default and the wrong one for some libraries:
    /// most Thunderstore mods for Unity games are closed-source binaries, so a
    /// modpack for one of those games is largely unusable until you decide to
    /// accept that.
    Trust {
        /// Install mods that publish no source code at all.
        #[arg(long)]
        allow_no_source: Option<bool>,
    },
    /// Show what is downloaded and which profiles still want it.
    Storage {
        /// Delete everything nothing references.
        #[arg(long)]
        clean: bool,
    },
    /// Delete store entries no profile references.
    Gc,
}

#[derive(Subcommand)]
enum PackAction {
    /// Create a profile from a modpack.
    ///
    /// Takes a Modrinth `.mrpack`, a CurseForge pack zip, or a Thunderstore
    /// package — as a file you downloaded, a link, or an id such as
    /// `thunderstore:Author-PackName` or `modrinth:some-pack`.
    ///
    /// Modrinth and Thunderstore need no API key. CurseForge needs the one
    /// you supply with `modifile auth --curseforge`.
    Add {
        /// A file path, a URL, or a pack id.
        source: String,
        /// Name the profile something other than the pack's own name.
        #[arg(long)]
        name: Option<String>,
        /// Which game the pack is for. Only needed for Thunderstore packs,
        /// which do not record their game anywhere.
        #[arg(long)]
        game: Option<String>,
        /// Download the mods straight away, rather than leaving it to
        /// `modifile update`.
        #[arg(long)]
        update: bool,
    },
    /// Read a modpack and print what is in it, without importing anything.
    Info {
        /// A file path, a URL, or a pack id.
        source: String,
        /// Which game the pack is for, for Thunderstore packs.
        #[arg(long)]
        game: Option<String>,
    },
    /// Find a modpack by name.
    Search {
        query: Vec<String>,
        /// Which game's packs to look for. Optional when you have one pack.
        #[arg(long)]
        game: Option<String>,
    },
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

    let mut engine = Engine::open(paths.clone(), load_token(&paths))?;
    // A binary replaced by an update is parked aside until nothing is running
    // it, which is the run after the one that installed it. This is that run.
    engine.tidy_after_update();
    for (path, error) in &engine.pack_errors {
        eprintln!("warning: ignoring pack {}: {error}", path.display());
    }
    // A pack that is silently out of date makes fixes look like they did not
    // ship, so say so once rather than letting the user chase a ghost.
    let stale: Vec<String> = modifile_core::pack_status(&paths)
        .into_iter()
        .filter(|(_, s)| *s == modifile_core::PackState::Edited)
        .map(|(n, _)| n)
        .collect();
    if !stale.is_empty() {
        eprintln!(
            "note: {} is out of date and was kept in case you edited it. \
             Run `modifile packs --refresh` to update. Fixes in those packs are not active.",
            stale.join(", ")
        );
    }

    match cli.command {
        Command::Init => cmd_init(&paths),
        Command::Games => cmd_games(&engine),
        Command::Auth {
            token,
            curseforge,
            curseforge_direct,
        } => cmd_auth(&paths, token, curseforge, curseforge_direct),
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
        Command::AddFile { profile, file, id } => {
            cmd_add_file(&engine, &profile, &file, id.as_deref())
        }
        Command::Rm { profile, id } => cmd_rm(&engine, &profile, &id),
        Command::Root {
            profile,
            target,
            path,
        } => cmd_root(&engine, &profile, &target, path),
        Command::Update { profile, all } => match (all, profile) {
            (true, _) => runtime.block_on(cmd_sync_all(&engine)),
            (false, Some(name)) => runtime.block_on(cmd_sync(&engine, &name)),
            (false, None) => Err(modifile_core::Error::other(
                "which profile? Name one, or pass --all to update every profile of \
                 every game.",
            )),
        },
        Command::Set {
            profile,
            loader,
            game_version,
        } => {
            let mut loaded = load_profile(&engine, &profile)?;
            let pack = engine.pack_for(&loaded)?;
            if let Some(loader) = &loader {
                let allowed = &pack.pack.versions.loaders;
                if !allowed.is_empty()
                    && !allowed.iter().any(|l| l.eq_ignore_ascii_case(loader))
                {
                    return Err(modifile_core::Error::other(format!(
                        "`{loader}` is not a loader for {} — options: {}",
                        pack.pack.game.name,
                        allowed.join(", ")
                    )));
                }
                loaded.loader = Some(loader.to_ascii_lowercase());
            }
            if let Some(v) = &game_version {
                loaded.game_version = Some(v.clone());
            }
            loaded.save(&engine.paths.profile_file(&loaded.id()))?;
            println!(
                "`{profile}` is now for {} {}",
                loaded.game_version.as_deref().unwrap_or("any version"),
                loaded.loader.as_deref().unwrap_or("")
            );
            Ok(())
        }
        Command::Loader { profile, install } => {
            runtime.block_on(cmd_loader(&engine, &profile, install))
        }
        Command::Rename { from, to } => {
            let name = engine.rename_profile(&resolve(&engine, &from)?, &to)?;
            println!("`{from}` is now `{name}`.");
            Ok(())
        }
        Command::Delete { profile, yes } => cmd_delete(&engine, &profile, yes),
        Command::Versions { profile, id } => {
            runtime.block_on(cmd_versions(&engine, &profile, &id))
        }
        Command::Hold {
            profile,
            id,
            version,
            latest,
        } => cmd_hold(&engine, &profile, &id, version, latest),
        Command::Export {
            profile,
            out,
            no_configs,
            note,
        } => cmd_export(&engine, &profile, out, !no_configs, note),
        Command::Import { file, name, latest } => {
            cmd_import(&engine, &file, name.as_deref(), !latest)
        }
        Command::Activate {
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
        Command::Deactivate {
            game,
            target,
            confirm_stopped,
            force,
        } => cmd_undeploy(
            &engine,
            &game,
            &target,
            DeployOptions {
                force,
                assume_stopped: confirm_stopped,
            },
        ),
        Command::Verify { profile } => cmd_verify(&engine, &profile),
        Command::Repair { profile } => runtime.block_on(cmd_repair(&engine, &profile)),
        Command::Config { action } => cmd_config(&engine, action),
        Command::Packs { refresh } => cmd_packs(&paths, refresh),
        Command::Search { query, profile } => {
            runtime.block_on(cmd_search(&engine, &query.join(" "), profile.as_deref()))
        }
        Command::Pack { action } => match action {
            PackAction::Add {
                source,
                name,
                game,
                update,
            } => runtime.block_on(cmd_pack_add(
                &engine,
                &source,
                name.as_deref(),
                game.as_deref(),
                update,
            )),
            PackAction::Info { source, game } => {
                runtime.block_on(cmd_pack_info(&engine, &source, game.as_deref()))
            }
            PackAction::Search { query, game } => runtime.block_on(cmd_pack_search(
                &engine,
                &query.join(" "),
                game.as_deref(),
            )),
        },
        Command::Play {
            profile,
            target,
            revert_on_exit,
            set_command,
        } => cmd_play(
            &engine,
            &profile,
            target.as_deref(),
            revert_on_exit,
            set_command.as_deref(),
        ),
        Command::Selfupdate {
            check,
            yes,
            check_on_startup,
            automatic,
        } => runtime.block_on(cmd_selfupdate(
            &engine,
            check,
            yes,
            check_on_startup,
            automatic,
        )),
        Command::Trust { allow_no_source } => cmd_trust(&mut engine, allow_no_source),
        Command::Storage { clean } => cmd_storage(&engine, clean),
        Command::Gc => cmd_gc(&engine),
    }
}

fn cmd_export(
    engine: &Engine,
    name: &str,
    out: Option<PathBuf>,
    include_configs: bool,
    note: String,
) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;

    let path = out
        .unwrap_or_else(|| PathBuf::from(format!("{name}.{}", modifile_core::mfpack::EXTENSION)));
    let bundle = engine.export_mfpack(pack, &profile, include_configs, note, &path)?;

    println!(
        "Wrote {} — {} mod(s), {} config file(s).",
        path.display(),
        bundle.mod_count(),
        bundle.config_count()
    );

    // A bundle carries the configs the *profile* owns, and it owns them from
    // the moment it is activated. Someone who tuned their mods in the game
    // folder and never activated would otherwise get a settings-less file
    // without being told why.
    if include_configs && bundle.config_count() == 0 {
        let live: usize = engine
            .targets(pack, &profile)
            .iter()
            .filter_map(|(target, root)| Some((target, root.as_ref()?)))
            .flat_map(|(target, root)| pack.state_dirs(target, root))
            .map(|(_, dir)| modifile_core::state::list_files(&dir).len())
            .sum();
        if live > 0 {
            println!(
                "  None were included: {live} settings file(s) are in the game folder but \
                 do not belong to `{name}` yet. Run `modifile config import {name} \
                 --from-game` (or activate the profile) and export again."
            );
        }
    }
    println!("Send that file to anyone; they run `modifile import <file>`.");
    Ok(())
}

// ---------------------------------------------------------------------------
// Modpacks
// ---------------------------------------------------------------------------

fn cmd_play(
    engine: &Engine,
    name: &str,
    target: Option<&str>,
    _revert_on_exit: bool,
    set_command: Option<&str>,
) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;

    if let Some(command) = set_command {
        engine.set_launch_command(pack.id(), Some(command))?;
        return match command.trim().is_empty() {
            true => {
                println!("Cleared the launch command for {}.", pack.pack.game.name);
                Ok(())
            }
            false => {
                println!("{} will start with: {command}", pack.pack.game.name);
                Ok(())
            }
        };
    }

    // The client unless told otherwise: nobody means "start the dedicated
    // server" by "play".
    let targets = engine.targets(pack, &profile);
    let (chosen, root) = targets
        .iter()
        .filter(|(t, root)| root.is_some() && target.is_none_or(|want| want == t.id))
        .find(|(t, _)| target.is_some() || t.kind == modifile_core::TargetKind::Client)
        .or_else(|| targets.iter().find(|(_, root)| root.is_some()))
        .map(|(t, root)| (t.clone(), root.clone().unwrap()))
        .ok_or_else(|| {
            modifile_core::Error::other(format!(
                "no game directory is known for `{name}`. Set one with `modifile root`."
            ))
        })?;

    // Play installs into the profile's own tree, which is not where Activate
    // puts things, so this runs whether or not the profile is activated.
    let lock = modifile_core::profile::Lock::load(&engine.paths.lock_file(&profile.id()))?;
    let plan = engine.plan_instanced(pack, &profile, &lock, &chosen, &root)?;
    let report = engine.deploy(
        pack,
        &chosen,
        &plan,
        &profile.id(),
        modifile_core::engine::DeployOptions::default(),
    )?;
    if report.linked > 0 {
        println!("{} file(s) placed in this profile's own folder.", report.linked);
    }

    let method = engine.play(pack, &profile, &chosen, &root)?;
    println!("Starting {} — {}.", chosen.name, method.describe());
    println!();
    println!(
        "`{name}` has its own folder and the game was pointed at it for this run. Your 
         {} install was not modified, so there is nothing to undo when you quit — and 
         nothing to lose if the machine loses power mid-session.",
        pack.pack.game.name
    );
    Ok(())
}

/// Update every profile of every game.
///
/// Sequential on purpose: each profile prints its own block, and interleaving
/// a dozen of them would make the output useless. One profile failing does not
/// stop the rest — the point of asking for all of them is not having to babysit
/// it.
async fn cmd_sync_all(engine: &Engine) -> Result<()> {
    let ids = engine.all_profiles();
    if ids.is_empty() {
        println!("No profiles to update.");
        return Ok(());
    }

    println!("Updating {} profile(s) across every game.\n", ids.len());
    let mut failed = Vec::new();
    for id in &ids {
        println!("──── {id} ────");
        if let Err(e) = cmd_sync(engine, &id.qualified()).await {
            eprintln!("  {id}: {e}");
            failed.push(id.clone());
        }
        println!();
    }

    if failed.is_empty() {
        println!("All {} profile(s) updated.", ids.len());
    } else {
        println!(
            "{} of {} updated; {} could not be: {}.",
            ids.len() - failed.len(),
            ids.len(),
            failed.len(),
            failed
                .iter()
                .map(|id| id.qualified())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}

async fn cmd_selfupdate(
    engine: &Engine,
    check_only: bool,
    yes: bool,
    check_on_startup: Option<bool>,
    automatic: Option<bool>,
) -> Result<()> {
    // Settings first, so `--automatic true` on its own is a way to set it.
    let mut changed = false;
    if let Some(on) = check_on_startup {
        engine.set_update_checks(on)?;
        changed = true;
    }
    if let Some(on) = automatic {
        engine.set_auto_update(on)?;
        changed = true;
    }
    if changed {
        println!(
            "Check for new versions on startup: {}",
            yes_no(engine.checks_for_updates())
        );
        println!(
            "Install them without asking:       {}",
            yes_no(engine.auto_updates())
        );
        if check_on_startup.is_some() && automatic.is_none() {
            return Ok(());
        }
        if automatic.is_some() && !check_only {
            return Ok(());
        }
    }

    let current = modifile_core::selfupdate::current_version();
    println!("This is Modifile {current}.");

    let Some(available) = engine.check_for_update().await? else {
        println!("Nothing newer has been published.");
        return Ok(());
    };

    println!();
    println!("Modifile {} is available.", available.version);
    if !available.published_at.is_empty() {
        println!("  published {}", available.published_at);
    }
    println!("  {} ({})", available.asset.name, format_bytes(available.size()));
    println!("  {}", available.web_url);

    if check_only {
        println!();
        println!("Install it with `modifile selfupdate --yes`.");
        return Ok(());
    }

    if !yes {
        println!();
        println!("This replaces the `modifile` and `modifile-gui` binaries where they are");
        println!("installed. Run it again with --yes to go ahead.");
        return Ok(());
    }

    println!();
    println!("Downloading and verifying…");
    let report = engine.install_update(&available).await?;

    println!(
        "Updated to {}: {}.",
        available.version,
        report.replaced.join(", ")
    );
    if report.left_behind > 0 {
        println!(
            "  {} old binary/binaries are still in use and will be cleared on the next run.",
            report.left_behind
        );
    }
    println!("Restart Modifile to use the new version.");
    Ok(())
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

fn cmd_trust(engine: &mut Engine, allow_no_source: Option<bool>) -> Result<()> {
    if let Some(on) = allow_no_source {
        engine.set_allow_no_source(on)?;
    }

    let allowed = engine.allows_no_source();
    println!(
        "Mods with no source code anywhere: {}",
        if allowed { "allowed" } else { "refused" }
    );
    if allowed {
        println!(
            "  A compiled file with no published source and no licence cannot be checked \
             by anyone — not by Modifile, not by you. You have accepted that."
        );
        println!("  Turn it back off with `modifile trust --allow-no-source false`.");
    } else {
        println!(
            "  Mods that publish source, or carry a licence, or come with build \
             attestations are installed as normal."
        );
        println!(
            "  Most Thunderstore mods for Unity games are closed-source binaries. If that \
             is what you want to run, `modifile trust --allow-no-source true`."
        );
    }
    Ok(())
}

fn print_pack_report(report: &modifile_core::engine::ModpackReport) {
    println!("{} ({})", report.pack, report.format);
    let versions = [
        report.game_version.clone(),
        report.loader.clone(),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    if !versions.is_empty() {
        println!("  {versions}");
    }
    println!(
        "  {} mod(s){}{}",
        report.mods,
        if report.untraced > 0 {
            format!(", {} file(s) held by hash", report.untraced)
        } else {
            String::new()
        },
        if report.overrides > 0 {
            format!(", {} pack file(s)", report.overrides)
        } else {
            String::new()
        }
    );

    if let Some(trust) = &report.trust {
        println!(
            "  the pack's own files are {} — {}",
            trust.level.label(),
            trust.level.explain()
        );
    }
    for note in &report.notes {
        println!("  note: {note}");
    }
    if !report.skipped.is_empty() {
        println!();
        println!("Not taken:");
        for (name, why) in &report.skipped {
            println!("  {name}: {why}");
        }
    }
}

async fn cmd_pack_add(
    engine: &Engine,
    source: &str,
    name: Option<&str>,
    game: Option<&str>,
    update: bool,
) -> Result<()> {
    let reporter: modifile_core::engine::Reporter = Arc::new(|event: Event| {
        if let Event::Pack { stage, detail } = event {
            println!("  {stage} {detail}");
        }
    });

    let report = engine
        .import_modpack(ModpackSource::parse(source), name, game, Some(reporter))
        .await?;

    println!();
    print_pack_report(&report);
    println!();
    println!("Created `{}`.", report.profile);

    if update {
        println!();
        return cmd_sync(engine, &report.profile).await;
    }

    println!();
    println!("Next: `modifile update {}` to download the mods,", report.profile);
    println!("then  `modifile activate {}` to put them in the game.", report.profile);
    Ok(())
}

async fn cmd_pack_info(engine: &Engine, source: &str, game: Option<&str>) -> Result<()> {
    // Reading a pack means fetching it, and a fetched pack is already in the
    // store — so this is genuinely free next time, and importing it later
    // downloads nothing twice.
    let report = engine
        .inspect_modpack(ModpackSource::parse(source), game)
        .await?;
    print_pack_report(&report);
    println!();
    println!("Nothing was imported. `modifile pack add` creates a profile from it.");
    Ok(())
}

async fn cmd_pack_search(engine: &Engine, query: &str, game: Option<&str>) -> Result<()> {
    if query.trim().is_empty() {
        return Err(modifile_core::Error::other("give me something to search for"));
    }

    let pack = match game {
        Some(id) => engine.pack(id).ok_or_else(|| {
            modifile_core::Error::NotFound(format!("game pack `{id}`"))
        })?,
        None if engine.packs.len() == 1 => &engine.packs[0],
        None => {
            return Err(modifile_core::Error::other(
                "say which game's packs to search: --game <id>",
            ))
        }
    };

    println!("Searching {} modpacks for \"{query}\"…\n", pack.pack.game.name);
    let hits = engine.search_modpacks(pack, query).await?;
    if hits.is_empty() {
        println!("Nothing found.");
        return Ok(());
    }

    for hit in &hits {
        println!("{}", hit.label());
        println!("  {}  ·  {}", hit.id, hit.popularity());
        if !hit.description.is_empty() {
            println!("  {}", hit.description);
        }
        println!();
    }
    println!("Install one with `modifile pack add <id or link>`.");
    Ok(())
}

fn cmd_import(
    engine: &Engine,
    file: &std::path::Path,
    name: Option<&str>,
    pin_versions: bool,
) -> Result<()> {
    // Either format: a `.mfpack` or one of the older JSON bundles.
    let bundle = engine.read_shared(file)?;
    let game = engine
        .pack(&bundle.game)
        .map(|p| p.pack.game.name.clone())
        .unwrap_or_else(|| bundle.game.clone());

    println!("{} — {game}", bundle.name);
    if !bundle.description.is_empty() {
        println!("  {}", bundle.description);
    }
    println!(
        "  {} mod(s), {} config file(s){}",
        bundle.mod_count(),
        bundle.config_count(),
        if pin_versions {
            ", held at the exporter's versions"
        } else {
            ", taking the newest versions"
        }
    );

    let created = engine.import_profile(&bundle, name, pin_versions)?;
    println!();
    println!("Created `{created}`.");
    if pin_versions {
        // Update checks on this profile will report nothing to do, forever,
        // and that is correct. Saying so now is cheaper than the alternative.
        println!(
            "Every mod is held at the exporter's version, so `update` will not move them —\n\
             it downloads those versions and names any that newer releases have passed.\n\
             Release one with `modifile hold {created} <mod> --latest`."
        );
    }
    println!("Next: modifile update {created} && modifile activate {created}");
    Ok(())
}

fn cmd_config(engine: &Engine, action: ConfigAction) -> Result<()> {
    match action {
        ConfigAction::Show { profile } => {
            let profile = load_profile(engine, &profile)?;
            let pack = engine.pack_for(&profile)?;
            let mut any = false;
            for (target, _) in engine.targets(pack, &profile) {
                let files = engine.saved_configs(pack, &profile.id(), &target);
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
                total += engine.reset_configs(pack, &loaded.id(), &t, root.as_deref())?;
            }
            println!(
                "Discarded {total} saved config file(s). Run `modifile activate {profile}` to \
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
                total += engine.import_configs(pack, &loaded.id(), &t, &source, root.as_deref())?;
            }
            println!("Imported {total} config file(s) into `{profile}`.");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

fn display(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

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

/// Find the profile someone named.
///
/// Profile names are scoped to their game, so `main` is only ambiguous when
/// two games both have one — and then it says so rather than guessing. A
/// `game/name` spelling is always unambiguous.
fn load_profile(engine: &Engine, spec: &str) -> Result<Profile> {
    let id = resolve(engine, spec)?;
    Profile::load(&engine.paths.profile_file(&id))
}

fn resolve(engine: &Engine, spec: &str) -> Result<ProfileId> {
    engine.resolve_profile(spec).map_err(|e| {
        // A missing profile is the common case and deserves the better message.
        if matches!(e, modifile_core::Error::NotFound(_)) {
            modifile_core::Error::NotFound(format!(
                "profile `{spec}` — create it with `modifile new {spec} --game <id>`"
            ))
        } else {
            e
        }
    })
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

fn cmd_auth(
    paths: &Paths,
    token: Option<String>,
    curseforge: bool,
    curseforge_direct: Option<bool>,
) -> Result<()> {
    // A setting, not a credential: handle it and stop.
    if let Some(on) = curseforge_direct {
        let marker = paths.curseforge_direct_file();
        if on {
            modifile_core::paths::write_atomic(&marker, b"on")?;
            println!("Direct CurseForge downloads: ON.");
            println!(
                "Files whose authors disabled third-party downloads will now be fetched \
                 from the CDN. If Overwolf notices, the key you obtained is what gets \
                 revoked — and that also costs you search and update checks."
            );
        } else {
            std::fs::remove_file(&marker).ok();
            println!("Direct CurseForge downloads: OFF. Blocked mods will be reported, not fetched.");
        }
        return Ok(());
    }

    let token = match token {
        Some(t) => t,
        None => {
            if curseforge {
                eprintln!(
                    "Paste your CurseForge API key (get one at \
                     https://console.curseforge.com/ — it is issued to you personally):"
                );
            } else {
                eprintln!("Paste a GitHub token (it is only used for read-only API calls):");
            }
            let mut buf = String::new();
            std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buf)?;
            buf
        }
    };
    let token = token.trim();
    if token.is_empty() {
        return Err(modifile_core::Error::other("nothing given"));
    }

    if curseforge {
        modifile_core::paths::write_atomic(
            &paths.curseforge_key_file(),
            token.as_bytes(),
        )?;
        println!("CurseForge key saved. Mods whose authors disabled third-party downloads");
        println!("will still be unavailable — that is their setting, not a key problem.");
    } else {
        modifile_core::paths::write_atomic(&paths.token_file(), token.as_bytes())?;
        println!("Token saved. API budget is now 5000 requests/hour and revalidations are free.");
    }
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
    // Only within this game: another game may well have a `main` too, and
    // that is the point.
    let id = ProfileId::new(game, name);
    let path = engine.paths.profile_file(&id);
    if path.exists() {
        return Err(modifile_core::Error::other(format!(
            "`{game}` already has a profile called `{name}`"
        )));
    }
    let mut profile = Profile::new(name, game);
    profile.targets = targets;
    profile.save(&path)?;
    println!("Created {}", path.display());
    Ok(())
}

fn cmd_profiles(engine: &Engine) -> Result<()> {
    let ids = engine.all_profiles();
    let mut any = false;
    let mut current_game = String::new();

    for id in &ids {
        let Ok(profile) = Profile::load(&engine.paths.profile_file(id)) else {
            continue;
        };
        // Grouped under their game, because that is now part of the identity
        // and two of these may legitimately share a name.
        if current_game != id.game {
            if any {
                println!();
            }
            let title = engine
                .pack(&id.game)
                .map(|p| p.pack.game.name.clone())
                .unwrap_or_else(|| id.game.clone());
            println!("{title} ({})", id.game);
            current_game = id.game.clone();
        }
        let enabled = profile.mods.iter().filter(|m| m.enabled).count();
        let active = engine.active_profiles(&id.game).contains(&id.name);
        println!(
            "  {:<22} {} mod(s){}",
            profile.name,
            enabled,
            if active { "  [mods installed]" } else { "" }
        );
        any = true;
    }

    if !any {
        println!("No profiles yet. Try `modifile new my-profile --game valheim`.");
    } else {
        println!();
        println!(
            "Names are per game, so two games can both have a `main`. Where that is \
             ambiguous, say `game/name`."
        );
    }
    Ok(())
}

fn cmd_show(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let lock = Lock::load(&engine.paths.lock_file(&profile.id()))?;

    println!("{} — {}", profile.name, pack.pack.game.name);
    println!();

    for (target, root) in engine.targets(pack, &profile) {
        let manifest = engine.manifest(pack.id(), &target.id)?;
        let state = match (&root, &manifest) {
            (None, _) => "no game directory".to_string(),
            (Some(_), None) => "not active".to_string(),
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
        // "held" rather than "pinned", and carrying what it is being held
        // back from, because a pin whose cost is invisible is one nobody
        // revisits.
        let held = locked
            .and_then(|l| l.upstream.clone())
            .filter(|_| entry.pin.is_some() && !entry.manual)
            .map(|latest| format!("held,{latest} out"));
        let flags = [
            (!entry.enabled).then_some("disabled".to_string()),
            held.or_else(|| entry.pin.as_ref().map(|_| "held".to_string())),
            entry.targets.as_ref().map(|_| "scoped".to_string()),
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
    profile.save(&engine.paths.profile_file(&profile.id()))?;
    println!("Added {id} to `{name}`. Run `modifile update {name}` to resolve it.");
    Ok(())
}

fn cmd_delete(engine: &Engine, name: &str, yes: bool) -> Result<()> {
    let profile = load_profile(engine, name)?;
    if !yes {
        println!(
            "This deletes `{name}` ({} mod(s)), the versions it resolved to and the settings \
             it was keeping. Downloads are shared with your other profiles and are left alone.",
            profile.mods.len()
        );
        println!("Re-run with --yes to go ahead.");
        return Ok(());
    }
    engine.delete_profile(&profile.id())?;
    println!("Deleted `{name}`.");
    println!("Unused downloads can be cleared with `modifile gc`.");
    Ok(())
}

async fn cmd_versions(engine: &Engine, name: &str, id: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let id: ModId = id.parse()?;
    let lock = Lock::load(&engine.paths.lock_file(&profile.id()))?;
    let installed = lock.get(&id).map(|l| l.version.clone());
    let pinned = profile.find(&id).and_then(|e| e.pin.clone());

    let versions = engine.versions(&profile, &id).await?;
    if versions.is_empty() {
        println!("{id} has no releases.");
        return Ok(());
    }

    for version in &versions {
        // Marks in the left margin, so the line you want is findable without
        // reading every word of it.
        let mark = match (&installed, &pinned) {
            (Some(v), _) if *v == version.tag => "*",
            _ => " ",
        };
        let held = if pinned.as_deref() == Some(version.tag.as_str()) {
            " (held here)"
        } else {
            ""
        };
        let date = version.published_at.split('T').next().unwrap_or_default();
        match &version.asset {
            Some(asset) => println!(
                "{mark} {:<20} {date}  {asset} ({}){}{held}",
                version.tag,
                format_bytes(version.size),
                if version.prerelease { " prerelease" } else { "" }
            ),
            None => println!(
                "{mark} {:<20} {date}  — nothing this game can install",
                version.tag
            ),
        }
    }
    println!();
    println!("* = installed. Hold one with `modifile hold {name} {id} <version>`.");
    Ok(())
}

fn cmd_hold(
    engine: &Engine,
    name: &str,
    id: &str,
    version: Option<String>,
    latest: bool,
) -> Result<()> {
    let mut profile = load_profile(engine, name)?;
    let id: ModId = id.parse()?;
    if profile.find(&id).is_none() {
        return Err(modifile_core::Error::NotFound(format!(
            "{id} is not in `{name}`"
        )));
    }
    if version.is_none() && !latest {
        return Err(modifile_core::Error::other(format!(
            "give a version to hold at, or --latest to follow releases again. \
             `modifile versions {name} {id}` lists them."
        )));
    }

    let pin = if latest { None } else { version };
    for entry in &mut profile.mods {
        if entry.id == id {
            entry.pin = pin.clone();
        }
    }
    profile.save(&engine.paths.profile_file(&profile.id()))?;
    match &pin {
        Some(v) => println!("{id} is held at {v}."),
        None => println!("{id} will take the newest release."),
    }
    println!("Run `modifile update {name}` to fetch it.");
    Ok(())
}

async fn cmd_loader(engine: &Engine, name: &str, install: bool) -> Result<()> {
    use modifile_core::loader::LoaderState;

    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;

    for (target, root) in engine.targets(pack, &profile) {
        let Some(root) = root else { continue };
        let Some((def, state)) = engine.loader_state(pack, &profile, &root) else {
            println!("{}: no mod loader set for this profile.", target.name);
            continue;
        };

        match &state {
            LoaderState::Installed { version } => {
                println!("{}: {} {version} installed.", target.name, def.name)
            }
            LoaderState::WrongVersion { version } => println!(
                "{}: {} {version} is installed, but this profile is for {}.",
                target.name,
                def.name,
                profile.game_version.as_deref().unwrap_or("?")
            ),
            LoaderState::NotInstalled => {
                println!("{}: {} is not installed.", target.name, def.name)
            }
            LoaderState::Manual { page } => println!(
                "{}: {} has to be installed with its own installer — {page}",
                target.name, def.name
            ),
        }

        if install && !matches!(state, LoaderState::Installed { .. }) {
            match engine.install_loader(pack, &profile, &root).await {
                Ok(version) => {
                    println!("  installed {} {version}", def.name);
                    println!("  it now appears in the Minecraft launcher's version list");
                }
                Err(e) => println!("  {e}"),
            }
        }
    }

    if !install {
        println!();
        println!("Add --install to install or update it.");
    }
    Ok(())
}

fn cmd_add_file(
    engine: &Engine,
    name: &str,
    file: &std::path::Path,
    id: Option<&str>,
) -> Result<()> {
    let mut profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let id = id.map(|s| s.parse::<ModId>()).transpose()?;

    let entry = engine.import_file(pack, &mut profile, file, id)?;
    profile.save(&engine.paths.profile_file(&profile.id()))?;

    // Write it straight into the lock: there is nothing to resolve later.
    let lock_path = engine.paths.lock_file(&profile.id());
    let mut lock = Lock::load(&lock_path)?;
    lock.mods.retain(|m| m.id != entry.id);
    lock.mods.push(entry.clone());
    lock.save(&lock_path)?;

    println!(
        "Added {} from {} ({}, {}).",
        entry.id,
        display(file),
        format_bytes(entry.size),
        entry.trust.level.short()
    );
    for note in &entry.trust.notes {
        println!("  note: {note}");
    }
    println!("Run `modifile activate {name}` to install it.");
    println!("It will not update on its own — re-run this with a newer file.");
    Ok(())
}

fn cmd_rm(engine: &Engine, name: &str, id: &str) -> Result<()> {
    let mut profile = load_profile(engine, name)?;
    let id: ModId = id.parse()?;
    if !profile.remove(&id) {
        return Err(modifile_core::Error::NotFound(format!("{id} in `{name}`")));
    }
    profile.save(&engine.paths.profile_file(&profile.id()))?;
    println!("Removed {id}. Run `modifile activate {name}` to take it out of the game.");
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
    profile.save(&engine.paths.profile_file(&profile.id()))?;
    println!("`{target}` -> {}", path.display());
    Ok(())
}

async fn cmd_sync(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let lock_path = engine.paths.lock_file(&profile.id());
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
        Event::UpdateAvailable {
            id,
            have,
            latest,
            page,
        } => {
            println!("  UPDATE   {id}: you have {have}, {latest} is out");
            println!("           {page}");
        }
        Event::HeldBack { id, have, latest } => {
            println!("  HELD     {id}: held at {have}, {latest} is out")
        }
        Event::Installed { id, version, trust } => {
            println!("  ready    {id} {version} [{}]", trust.level.label())
        }
        Event::Failed { id, error } => eprintln!("  FAILED   {id}: {error}"),
        Event::Pack { stage, detail } => println!("  {stage} {detail}"),
    });

    println!("Checking `{name}` for updates...");
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
    // Updating leaves the previous version behind under its own hash. Nothing
    // points at it, so clear it now rather than quietly hoarding every build
    // this profile has ever had.
    match engine.gc() {
        Ok((entries, bytes)) if entries > 0 => {
            println!(
                "Removed {entries} superseded download(s), freeing {}.",
                format_bytes(bytes)
            );
        }
        Ok(_) => {}
        Err(e) => eprintln!("note: could not tidy old downloads: {e}"),
    }

    // "No build for your version yet" is a waiting state, not a failure. The
    // mod stays in the profile and is skipped until one appears.
    let waiting: Vec<_> = failures.iter().filter(|f| f.waiting).collect();
    let broken: Vec<_> = failures.iter().filter(|f| !f.waiting).collect();

    // A mod that failed to check keeps its previous download in the lock, so
    // it is counted as failed rather than ready.
    println!(
        "{} mod(s) ready{}{}.",
        lock.mods
            .iter()
            .filter(|m| !broken.iter().any(|b| b.id == m.id))
            .count(),
        if waiting.is_empty() {
            String::new()
        } else {
            format!(", {} waiting for an update", waiting.len())
        },
        if broken.is_empty() {
            String::new()
        } else {
            format!(", {} failed", broken.len())
        }
    );

    if !waiting.is_empty() {
        println!();
        println!("Not built for your version yet — kept in the profile, skipped on activate:");
        for issue in &waiting {
            println!("  {}", issue.id);
        }
    }

    // A held mod is not an error and not an update, so it gets its own
    // paragraph. Folding it into "N mods ready" is how a profile imported at
    // someone else's versions passes for a current one indefinitely.
    let held: Vec<_> = lock
        .mods
        .iter()
        .filter(|entry| {
            entry.upstream.is_some()
                && profile
                    .find(&entry.id)
                    .map(|e| e.pin.is_some() && !e.manual)
                    .unwrap_or(false)
        })
        .collect();
    if !held.is_empty() {
        println!();
        println!("Held at a chosen version — newer releases exist and were not taken:");
        for entry in &held {
            println!(
                "  {} {} -> {} available",
                entry.id,
                entry.version,
                entry.upstream.clone().unwrap_or_default()
            );
        }
        println!("  Take one: modifile hold {name} <mod> --latest, then update again.");
    }
    // A mod refused by the trust policy is not broken — it is a decision, and
    // the same decision every time. Printing ninety identical paragraphs
    // buries the one sentence that would let someone act on it, which is
    // exactly what importing a Thunderstore modpack used to produce.
    let (refused, other): (Vec<&modifile_core::engine::SyncIssue>, Vec<_>) = broken
        .iter()
        .partition(|issue| issue.message.contains("your policy refuses it"));

    for issue in &other {
        eprintln!("  {}: {}", issue.id, issue.message);
    }
    if !refused.is_empty() {
        println!();
        println!(
            "{} mod(s) publish no source code and no licence, so there is nothing to \
             check, and Modifile refused them:",
            refused.len()
        );
        for issue in refused.iter().take(8) {
            println!("  {}", issue.id);
        }
        if refused.len() > 8 {
            println!("  …and {} more", refused.len() - 8);
        }
        println!(
            "  This is most Thunderstore mods for Unity games. To run them anyway: \
             `modifile trust --allow-no-source true`, then update again."
        );
    }
    println!("Next: modifile activate {name}");
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
    let lock = Lock::load(&engine.paths.lock_file(&profile.id()))?;
    if lock.mods.is_empty() && !profile.mods.is_empty() {
        return Err(modifile_core::Error::other(format!(
            "`{name}` has no lockfile — run `modifile update {name}` first"
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
        let report = engine.deploy(pack, &target, &plan, &profile.id(), options)?;
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
        if report.adopted > 0 {
            println!(
                "  {} settings file(s) already in the game folder now belong to `{}`",
                report.adopted, profile.name
            );
        }
        if report.captured > 0 {
            println!(
                "  {} settings file(s) saved into the profile that was active before",
                report.captured
            );
        }
        if report.restored > 0 && report.adopted == 0 {
            println!(
                "  {} settings file(s) restored from this profile",
                report.restored
            );
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
    if let Some(loader) = &plan.missing_loader {
        println!();
        println!(
            "  WARNING: {loader} is not installed, so the game will not load any of these 
             mods. Run `modifile loader <profile> --install` first."
        );
        println!();
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

    if !report.skipped.is_empty() {
        println!();
        println!(
            "{} file(s) were LEFT IN THE GAME because they no longer match what was \
             installed — something replaced them since:",
            report.skipped.len()
        );
        for (path, _) in &report.skipped {
            println!("  {}", display(path));
        }
        println!();
        println!(
            "The game is not fully vanilla. Delete them yourself, or run \
             `modifile deactivate {game} {target} --force` to remove them."
        );
    }
    Ok(())
}

fn cmd_verify(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let pack = engine.pack_for(&profile)?;
    let mut clean = true;
    let mut disturbed = false;

    let lock = Lock::load(&engine.paths.lock_file(&profile.id()))?;
    for (target, root) in engine.targets(pack, &profile) {
        let Some(root) = root else { continue };
        let scan = engine.scan(pack, &profile, &lock, &target, &root)?;

        println!(
            "{} [{}]: {} intact, {} changed, {} missing, {} not installed by Modifile",
            target.name,
            target.kind.label(),
            scan.intact,
            scan.modified.len(),
            scan.missing.len(),
            scan.foreign.len()
        );

        for entry in &scan.modified {
            println!("  changed   {}", display(&entry.rel));
        }
        for entry in &scan.missing {
            println!("  missing   {}", display(&entry.rel));
        }
        for entry in &scan.foreign {
            println!(
                "  {}  {}",
                if entry.conflicts { "BLOCKING" } else { "foreign " },
                display(&entry.rel)
            );
        }

        let blocking = scan.blocking().count();
        if blocking > 0 {
            println!();
            println!(
                "  {blocking} file(s) marked BLOCKING sit exactly where this profile's mods go. \
                 Activating will skip those mods and leave your files in place. Run \
                 `modifile activate {name} --force` to replace them."
            );
        }
        clean &= scan.is_clean();
        disturbed |= !scan.modified.is_empty() || !scan.missing.is_empty();
    }

    if clean {
        println!();
        println!("Everything in the game folder is exactly what this profile installed.");
    } else if disturbed {
        // Only say this when files *we placed* went wrong — a hand-installed
        // mod is not a game update clobbering anything.
        println!();
        // Not `activate`: that deliberately leaves a changed file alone, so
        // sending someone there was advice that quietly did nothing.
        println!(
            "Files Modifile installed have changed or gone. A game update is the usual cause; \
             `modifile repair {name}` checksums the stored copies, re-downloads anything \
             damaged, and puts the recorded versions back."
        );
    }
    Ok(())
}

/// Put a disturbed install back to exactly what the lockfile says.
async fn cmd_repair(engine: &Engine, name: &str) -> Result<()> {
    let profile = load_profile(engine, name)?;
    let lock_path = engine.paths.lock_file(&profile.id());
    let lock = Lock::load(&lock_path)?;

    // The store first. A deployed file is a hard link to the stored one, so
    // anything that wrote to it in place wrote through to the store, and
    // re-linking a damaged entry would only reinstall the damage.
    println!("Checking stored copies against their checksums...");
    let mut unrecorded = 0;
    for (id, health) in engine.store_health(&lock) {
        match health {
            modifile_core::store::EntryHealth::Good => {}
            modifile_core::store::EntryHealth::Damaged { files } => {
                println!("  DAMAGED  {id}: {} file(s) no longer match", files.len());
                for rel in files.iter().take(5) {
                    println!("           {rel}");
                }
            }
            modifile_core::store::EntryHealth::Missing => {
                println!("  missing  {id}: not in the store")
            }
            modifile_core::store::EntryHealth::Unrecorded => unrecorded += 1,
        }
    }
    if unrecorded > 0 {
        println!(
            "  {unrecorded} entr(ies) predate checksums and cannot be checked. They get a \
             record the next time they are downloaded."
        );
    }

    let discarded = engine.discard_damaged(&lock)?;
    if !discarded.is_empty() {
        println!("Discarded {} damaged download(s).", discarded.len());
    }

    println!();
    println!("Fetching anything missing...");
    let pack = engine.pack_for(&profile)?;
    let previous = Lock::load(&lock_path)?;
    // Download, not update: this puts back the versions that were installed.
    let (fresh, failures) = engine.download(pack, &profile, &previous, None).await?;
    fresh.save(&lock_path)?;
    for issue in failures.iter().filter(|f| !f.waiting) {
        eprintln!("  {}: {}", issue.id, issue.message);
    }

    println!();
    let mut dropped = 0;
    for (target, root) in engine.targets(pack, &profile) {
        if root.is_none() {
            continue;
        }
        let (count, failed) =
            engine.drop_tampered(&profile.game, &target.id, DeployOptions::default())?;
        dropped += count;
        for rel in failed {
            eprintln!("  could not replace {}", display(&rel));
        }
    }
    println!("Cleared {dropped} changed file(s).");
    println!();
    println!("Next: modifile activate {name}");
    Ok(())
}

fn cmd_packs(paths: &Paths, refresh: bool) -> Result<()> {
    use modifile_core::PackState;

    if refresh {
        let report = modifile_core::install_bundled_packs_with(paths, true)?;
        for (name, backup) in &report.replaced {
            println!("refreshed {name}  (your copy saved as {})", display(backup));
        }
        for name in report.updated.iter().chain(report.written.iter()) {
            println!("refreshed {name}");
        }
        if report.replaced.is_empty() && report.updated.is_empty() && report.written.is_empty() {
            println!("Every pack was already current.");
        }
        return Ok(());
    }

    let status = modifile_core::pack_status(paths);
    let stale: Vec<&String> = status
        .iter()
        .filter(|(_, s)| *s != PackState::Current)
        .map(|(n, _)| n)
        .collect();

    for (name, state) in &status {
        let label = match state {
            PackState::Current => "current",
            PackState::Missing => "not installed",
            PackState::Outdated => "out of date (updates automatically)",
            PackState::Edited => "OUT OF DATE — kept because it may be your edit",
        };
        println!("{name:<24} {label}");
    }

    if !stale.is_empty() {
        println!();
        println!(
            "Some packs are not the ones this build ships, so fixes in them are not \
             reaching you. Run `modifile packs --refresh` to replace them; your current \
             copies are saved as .bak files."
        );
    }
    Ok(())
}

async fn cmd_search(engine: &Engine, query: &str, profile: Option<&str>) -> Result<()> {
    if query.trim().is_empty() {
        return Err(modifile_core::Error::other("give me something to search for"));
    }

    // Which game to search for is the profile's, or the only pack there is.
    let (pack, filter) = match profile {
        Some(name) => {
            let loaded = load_profile(engine, name)?;
            let pack = engine.pack_for(&loaded)?;
            (
                pack,
                modifile_core::source::modrinth::VersionFilter {
                    game_version: loaded.game_version.clone(),
                    loader: loaded.loader.clone(),
                },
            )
        }
        None if engine.packs.len() == 1 => (
            &engine.packs[0],
            modifile_core::source::modrinth::VersionFilter::default(),
        ),
        None => {
            return Err(modifile_core::Error::other(
                "say which game to search: --profile <name>",
            ))
        }
    };

    println!("Searching {} for \"{query}\"…\n", pack.pack.game.name);
    let hits = engine.search(pack, query, &filter, true).await?;
    if hits.is_empty() {
        println!("Nothing found.");
        return Ok(());
    }

    for hit in &hits {
        let blocked = hit.installable == Some(false);
        println!(
            "{}{}",
            hit.label(),
            if blocked { "   [not downloadable]" } else { "" }
        );
        if !hit.description.is_empty() {
            let short: String = hit.description.chars().take(96).collect();
            println!("    {short}");
        }
        let mut facts = vec![hit.id.to_string(), hit.popularity()];
        if let Some(license) = &hit.license {
            facts.push(license.clone());
        }
        if let Some(src) = &hit.source_url {
            facts.push(src.clone());
        }
        println!("    {}", facts.join("  ·  "));
        println!();
    }

    println!(
        "Add one with: modifile add <profile> <id>\n\
         `[not downloadable]` means the project publishes nothing this can install — on \
         CurseForge that is the author switching off third-party downloads, so it has to \
         be fetched by hand."
    );
    Ok(())
}

fn cmd_storage(engine: &Engine, clean: bool) -> Result<()> {
    let report = engine.storage()?;
    if report.items.is_empty() {
        println!("Nothing downloaded yet.");
        return Ok(());
    }

    println!(
        "{} download(s), {} total\n",
        report.items.len(),
        format_bytes(report.total_bytes())
    );

    for item in &report.items {
        let state = if item.is_orphan() {
            "UNUSED".to_string()
        } else if item.only_inactive() {
            "kept for switched-off profiles".to_string()
        } else {
            "in use".to_string()
        };
        println!("{:>10}  {:<34} {state}", format_bytes(item.size), item.name());
        for used in &item.used_by {
            println!(
                "            in `{}`{}",
                used.profile,
                if used.active { " (active)" } else { "" }
            );
        }
    }

    let orphan_bytes = report.orphan_bytes();
    let orphans = report.orphans().count();
    println!();
    if orphans == 0 {
        println!("Nothing to clean up — every download is wanted by some profile.");
    } else if clean {
        let (removed, freed) = engine.gc()?;
        println!("Removed {removed} unused download(s), freeing {}.", format_bytes(freed));
    } else {
        println!(
            "{orphans} download(s) totalling {} are not referenced by any profile. \
             Run `modifile storage --clean` to remove them.",
            format_bytes(orphan_bytes)
        );
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
