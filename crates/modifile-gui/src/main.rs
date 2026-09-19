//! The desktop front end.
//!
//! egui on glow: one static binary, no webview, no Node, no bundled browser.
//!
//! This is a complete front end, not a viewer over the CLI. Anything you can do
//! with `modifile` you can do here: pick game folders, save a token, create
//! profiles, add mods, sync, deploy. If a task needs the terminal, that is a
//! bug in this file.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod theme;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use eframe::egui;
use modifile_core::deploy::LinkMode;
use modifile_core::engine::{format_bytes, Event};
use modifile_core::pack::Target;
use modifile_core::profile::ModEntry;
use modifile_core::{Engine, Lock, ModId, Paths, Profile, TrustLevel};

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([820.0, 560.0])
            .with_title("Modifile"),
        ..Default::default()
    };
    eframe::run_native(
        "modifile",
        options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    )
}

/// Background threads report terminal states here; progress goes to the log.
enum Msg {
    Error(String),
    /// An update finished, with what actually changed.
    Synced(Vec<SyncOutcome>),
    /// A search finished.
    Found(Vec<modifile_core::source::SearchHit>),
    /// Background work finished with nothing to report but its completion.
    /// Distinct from `Synced` so it does not wipe the update panel.
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutcomeKind {
    New,
    Updated,
    Unchanged,
    /// A manually-added mod whose source has something newer. Nothing can fetch
    /// it for you, so this is a nudge with a link rather than an action.
    NeedsManualUpdate,
    /// Fine, but not built for this profile's game version or loader yet.
    Waiting,
    Failed,
}

/// What happened to one mod during an update. Kept as data so the UI can show
/// it as a table rather than making the user read a log.
#[derive(Clone)]
struct SyncOutcome {
    id: ModId,
    from: Option<String>,
    to: Option<String>,
    kind: OutcomeKind,
    detail: String,
    /// Where to go to get it, for the manual-update case.
    page: Option<String>,
}

impl SyncOutcome {
    fn label(&self) -> String {
        match self.kind {
            OutcomeKind::New => format!("added {}", self.to.clone().unwrap_or_default()),
            OutcomeKind::Updated => format!(
                "{} -> {}",
                self.from.clone().unwrap_or_default(),
                self.to.clone().unwrap_or_default()
            ),
            OutcomeKind::Unchanged => {
                format!("{} (already newest)", self.to.clone().unwrap_or_default())
            }
            OutcomeKind::NeedsManualUpdate => format!(
                "{} -> {} available",
                self.from.clone().unwrap_or_default(),
                self.to.clone().unwrap_or_default()
            ),
            OutcomeKind::Waiting => "no build yet".to_string(),
            OutcomeKind::Failed => "failed".to_string(),
        }
    }

    fn color(&self) -> egui::Color32 {
        match self.kind {
            OutcomeKind::New => theme::ACCENT,
            OutcomeKind::Updated => theme::GOOD,
            OutcomeKind::Unchanged => theme::MUTED,
            OutcomeKind::NeedsManualUpdate => theme::WARN,
            OutcomeKind::Waiting => theme::MUTED,
            OutcomeKind::Failed => theme::BAD,
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Profile,
    Games,
    Settings,
}

struct ModRow {
    id: ModId,
    enabled: bool,
    version: String,
    trust: Option<TrustLevel>,
    note: String,
    pinned: bool,
    /// `None` means every target of the game. `Some` restricts it — this is how
    /// a mod is marked server-only or client-only.
    targets: Option<Vec<String>>,
    /// Bytes this mod occupies in the download store. Non-zero even when the
    /// profile is switched off, which is the point.
    size: u64,
    /// Files from this mod are in a game folder right now.
    installed: bool,
    /// Fine, but nothing built for this profile's version yet.
    waiting: bool,
    prerelease: bool,
}

struct TargetRow {
    target: Target,
    root: Option<PathBuf>,
    /// True when the path came from the user rather than autodetection.
    remembered: bool,
    deployed: Option<(String, usize, Option<LinkMode>)>,
    /// Set while the game is running; mod changes are refused until it closes.
    running: Option<String>,
    /// A folder on another machine, whose processes we cannot see.
    remote: bool,
}

/// A profile as the sidebar needs it: which game it belongs to, and whether its
/// mods are currently in that game.
#[derive(Clone)]
struct ProfileEntry {
    name: String,
    game_name: String,
    active: bool,
    mods: usize,
}

struct App {
    paths: Paths,
    engine: Option<Engine>,
    runtime: tokio::runtime::Runtime,

    view: View,
    profiles: Vec<ProfileEntry>,
    selected: Option<String>,

    profile: Option<Profile>,
    lock: Lock,
    targets: Vec<TargetRow>,
    rows: Vec<ModRow>,
    /// Config files this profile is keeping, across all its targets.
    config_files: Vec<PathBuf>,
    /// Config files sitting in the game folder right now. Shown when the
    /// profile has none saved yet, so an empty panel is not mistaken for an
    /// empty game folder.
    live_config_files: usize,
    /// Target ids of this profile's game, split by side. Empty `server_ids`
    /// means the game has no dedicated server and the side control is hidden.
    client_ids: Vec<String>,
    server_ids: Vec<String>,
    /// Last game-folder scan per target. Empty until the user refreshes.
    scans: Vec<modifile_core::deploy::FolderScan>,
    /// Set by a button inside the scan panel, acted on after the panel is drawn
    /// so we are not mutating while borrowing it.
    pending_force_deploy: bool,
    /// Files the last deactivate could not remove because they had changed.
    leftovers: usize,
    pending_force_undeploy: bool,
    /// Last storage listing, loaded on demand rather than every frame.
    storage: Option<modifile_core::storage::StorageReport>,
    /// What the last update actually changed.
    last_sync: Vec<SyncOutcome>,

    add_input: String,
    token_input: String,
    curseforge_input: String,
    curseforge_direct: bool,
    /// Set by the Games view; acted on after the panel is drawn.
    refresh_packs: bool,
    /// Loader definition, its state, and the game root it was checked in.
    loader_state: Option<(
        modifile_core::pack::LoaderDef,
        modifile_core::loader::LoaderState,
        PathBuf,
    )>,
    pending_install_loader: bool,
    /// Install mods that publish no source code at all. Off by default.
    allow_no_source: bool,
    version_input: String,
    search_input: String,
    search_results: Vec<modifile_core::source::SearchHit>,
    searching: bool,
    /// Open folder chooser, as (game id, target).
    root_dialog: Option<(String, Target)>,
    root_input: String,
    show_rename: bool,
    rename_input: String,
    /// Ticked by the user for game folders on another machine, which we cannot
    /// check ourselves. Deliberately not remembered between runs.
    assume_stopped: bool,
    new_profile_name: String,
    new_profile_game: String,
    show_new_profile: bool,

    log: Arc<Mutex<Vec<String>>>,
    /// The log is a detail view, closed unless asked for.
    log_open: bool,
    busy: bool,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let paths = Paths::discover().unwrap_or_else(|_| Paths::rooted("."));
        let _ = paths.ensure();
        // A fresh install is useful immediately: the bundled packs land on
        // first run, so the Games list is never empty.
        let _ = modifile_core::install_bundled_packs(&paths);

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");

        let token = load_token(&paths);
        // `paths` is moved into the struct below, so read settings off it first.
        let paths_probe = paths.clone();
        let mut app = Self {
            engine: Engine::open(paths.clone(), token.clone()).ok(),
            paths,
            runtime,
            view: View::Profile,
            profiles: Vec::new(),
            selected: None,
            profile: None,
            lock: Lock::default(),
            targets: Vec::new(),
            rows: Vec::new(),
            config_files: Vec::new(),
            live_config_files: 0,
            client_ids: Vec::new(),
            server_ids: Vec::new(),
            scans: Vec::new(),
            pending_force_deploy: false,
            leftovers: 0,
            pending_force_undeploy: false,
            storage: None,
            last_sync: Vec::new(),
            add_input: String::new(),
            token_input: token.unwrap_or_default(),
            // Load the saved key like the GitHub token does. Leaving it blank
            // made a stored key look lost, and saving the blank field wiped it.
            curseforge_input: std::fs::read_to_string(paths_probe.curseforge_key_file())
                .ok()
                .map(|k| k.trim().to_string())
                .unwrap_or_default(),
            // Read the real setting rather than assuming off.
            curseforge_direct: paths_probe.curseforge_direct_file().exists(),
            refresh_packs: false,
            loader_state: None,
            pending_install_loader: false,
            allow_no_source: paths_probe.allow_no_source_file().exists(),
            version_input: String::new(),
            search_input: String::new(),
            search_results: Vec::new(),
            searching: false,
            root_dialog: None,
            root_input: String::new(),
            show_rename: false,
            rename_input: String::new(),
            assume_stopped: false,
            new_profile_name: String::new(),
            new_profile_game: String::new(),
            show_new_profile: false,
            log: Arc::new(Mutex::new(Vec::new())),
            log_open: false,
            busy: false,
            tx,
            rx,
        };
        app.reload_profiles();
        if let Some(first) = app.profiles.first().cloned() {
            app.select(&first.name);
        }
        app
    }

    fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    fn reload_profiles(&mut self) {
        let mut found = Vec::new();
        // Which profile currently occupies each game, cached per game so a
        // dozen profiles do not mean a dozen directory scans.
        let mut active_by_game: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();

        if let Ok(entries) = std::fs::read_dir(&self.paths.profiles) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                    continue;
                }
                let Ok(profile) = Profile::load(&path) else {
                    continue;
                };
                let game_name = self
                    .engine()
                    .and_then(|e| e.pack(&profile.game))
                    .map(|p| p.pack.game.name.clone())
                    .unwrap_or_else(|| profile.game.clone());

                let active = if let Some(engine) = self.engine() {
                    active_by_game
                        .entry(profile.game.clone())
                        .or_insert_with(|| engine.active_profiles(&profile.game))
                        .contains(&profile.name)
                } else {
                    false
                };

                found.push(ProfileEntry {
                    name: profile.name.clone(),
                    game_name,
                    active,
                    mods: profile.mods.iter().filter(|m| m.enabled).count(),
                });
            }
        }
        // Group by game, then name, so the sidebar can just walk the list.
        found.sort_by(|a, b| {
            a.game_name
                .cmp(&b.game_name)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        self.profiles = found;
    }

    fn is_active(&self, name: &str) -> bool {
        self.profiles
            .iter()
            .any(|p| p.name == name && p.active)
    }

    fn select(&mut self, name: &str) {
        // Captured before reassigning, otherwise the comparison below is always
        // false and a stale update summary follows you to the next profile.
        let switching_profile = self.selected.as_deref() != Some(name);

        self.selected = Some(name.to_string());
        self.view = View::Profile;
        self.rows.clear();
        self.targets.clear();
        self.config_files.clear();
        // A scan and an update result describe one profile; they are
        // meaningless for another.
        self.scans.clear();
        if switching_profile {
            self.last_sync.clear();
            self.leftovers = 0;
            // Results are for one game's index; they mean nothing for another.
            self.search_results.clear();
            self.search_input.clear();
        }
        self.lock = Lock::default();

        let Ok(profile) = Profile::load(&self.paths.profile_file(name)) else {
            self.profile = None;
            return;
        };
        self.lock = Lock::load(&self.paths.lock_file(name)).unwrap_or_default();

        let mut targets = Vec::new();
        let mut configs = Vec::new();
        let mut client_ids = Vec::new();
        let mut server_ids = Vec::new();
        let mut live_configs = 0usize;
        if let Some(engine) = self.engine() {
            if let Ok(pack) = engine.pack_for(&profile) {
                for t in &pack.pack.targets {
                    match t.kind {
                        modifile_core::TargetKind::Client => client_ids.push(t.id.clone()),
                        modifile_core::TargetKind::Server => server_ids.push(t.id.clone()),
                    }
                }
                for (target, root) in engine.targets(pack, &profile) {
                    configs.extend(engine.saved_configs(pack, name, &target));
                    if let Some(root) = &root {
                        for (_, dir) in pack.state_dirs(&target, root) {
                            live_configs += modifile_core::state::list_files(&dir).len();
                        }
                    }
                    let remembered = engine.roots.get(pack.id(), &target.id).is_some()
                        || profile.roots.contains_key(&target.id);
                    let deployed = engine
                        .manifest(pack.id(), &target.id)
                        .ok()
                        .flatten()
                        .map(|m| (m.profile, m.files.len(), m.mode));
                    // Checked here rather than per frame: enumerating processes
                    // costs tens of milliseconds and the answer only matters
                    // when the user is about to act.
                    let remote = root
                        .as_ref()
                        .map(|r| modifile_core::paths::is_network_path(r))
                        .unwrap_or(false);
                    let running = root
                        .as_ref()
                        .filter(|_| !remote)
                        .and_then(|r| Engine::running(&target, r))
                        .map(|found| found.to_string());
                    targets.push(TargetRow {
                        target,
                        root,
                        remembered,
                        deployed,
                        running,
                        remote,
                    });
                }
            }
        }
        self.targets = targets;
        self.config_files = configs;
        self.live_config_files = live_configs;
        self.version_input = profile.game_version.clone().unwrap_or_default();
        self.client_ids = client_ids;
        self.loader_state = self.engine().and_then(|engine| {
            let pack = engine.pack_for(&profile).ok()?;
            let (_, root) = engine
                .targets(pack, &profile)
                .into_iter()
                .find(|(t, r)| t.kind == modifile_core::TargetKind::Client && r.is_some())?;
            let root = root?;
            let (def, state) = engine.loader_state(pack, &profile, &root)?;
            Some((def, state, root))
        });
        self.server_ids = server_ids;

        // Which mods currently have files in a game folder, from the manifests.
        let mut installed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(engine) = self.engine() {
            if let Ok(pack) = engine.pack_for(&profile) {
                for row in &self.targets {
                    if let Ok(Some(manifest)) = engine.manifest(pack.id(), &row.target.id) {
                        if manifest.active {
                            installed_ids.extend(manifest.files.iter().map(|f| f.mod_id.clone()));
                        }
                    }
                }
            }
        }

        for entry in &profile.mods {
            let locked = self.lock.get(&entry.id);
            let size = locked
                .and_then(|l| self.engine().map(|e| e.store.size_of(&l.sha256)))
                .unwrap_or(0);
            // Listed but unresolved means the last check found no build for
            // this profile's version — greyed rather than alarming.
            let waiting = locked.is_none()
                && self
                    .last_sync
                    .iter()
                    .any(|o| o.id == entry.id && o.kind == OutcomeKind::Waiting);
            self.rows.push(ModRow {
                size,
                waiting,
                installed: installed_ids.contains(&entry.id.to_string()),
                id: entry.id.clone(),
                enabled: entry.enabled,
                version: locked
                    .map(|l| l.version.clone())
                    .unwrap_or_else(|| "—".into()),
                trust: locked.map(|l| l.trust.level),
                note: locked.map(|l| l.trust.notes.join("; ")).unwrap_or_default(),
                pinned: entry.pin.is_some(),
                prerelease: entry.prerelease,
                targets: entry.targets.clone(),
            });
        }
        self.profile = Some(profile);
    }

    fn refresh(&mut self) {
        // Cheapest way to stay honest: re-read from disk after every mutation.
        if let Some(engine) = self.engine.as_mut() {
            engine.roots =
                modifile_core::roots::GlobalRoots::load(&self.paths.roots_file()).unwrap_or_default();
        }
        // The profile list carries which profile is active, and that changes on
        // activate and deactivate. Leaving it out meant a deactivated profile
        // kept its green dot and "Active" banner until the app restarted.
        self.reload_profiles();
        if let Some(name) = self.selected.clone() {
            self.select(&name);
        }
    }

    fn log_line(&self, line: impl Into<String>) {
        if let Ok(mut log) = self.log.lock() {
            log.push(line.into());
            if log.len() > 500 {
                let excess = log.len() - 500;
                log.drain(..excess);
            }
        }
    }

    // --- actions ----------------------------------------------------------

    fn do_sync(&mut self, ctx: &egui::Context) {
        let Some(profile) = self.profile.clone() else {
            return;
        };
        let paths = self.paths.clone();
        let log = self.log.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let allow_no_source = self.allow_no_source;
        self.busy = true;
        self.log_line(format!("Checking {} for updates…", profile.name));

        let handle = self.runtime.handle().clone();
        std::thread::spawn(move || {
            let push = {
                let log = log.clone();
                let ctx = ctx.clone();
                move |line: String| {
                    if let Ok(mut log) = log.lock() {
                        log.push(line);
                    }
                    ctx.request_repaint();
                }
            };

            let result = handle.block_on(async {
                let mut engine = Engine::open(paths.clone(), load_token(&paths))?;
                if allow_no_source {
                    engine.policy.minimum = modifile_core::TrustLevel::Blocked;
                }
                let pack = engine.pack_for(&profile)?;
                let lock_path = paths.lock_file(&profile.name);
                let previous = Lock::load(&lock_path)?;

                // Update notices arrive as events during the run, so collect
                // them here and fold them into the results table afterwards.
                let manual: Arc<Mutex<Vec<SyncOutcome>>> = Arc::new(Mutex::new(Vec::new()));
                let reporter = {
                    let push = push.clone();
                    let manual = manual.clone();
                    Arc::new(move |event: Event| match event {
                        Event::UpdateAvailable {
                            id,
                            have,
                            latest,
                            page,
                        } => {
                            push(format!("{id}: {latest} is available (you have {have})"));
                            if let Ok(mut list) = manual.lock() {
                                list.push(SyncOutcome {
                                    id,
                                    from: Some(have),
                                    to: Some(latest),
                                    kind: OutcomeKind::NeedsManualUpdate,
                                    detail: "Nothing can download this for you — get it from \
                                             the page, then use From file…"
                                        .to_string(),
                                    page: Some(page),
                                });
                            }
                        }
                        Event::Resolved { id, version } => {
                            push(format!("resolved {id} -> {version}"))
                        }
                        Event::Downloading { id, asset, size } => {
                            push(format!("downloading {id}: {asset} ({})", format_bytes(size)))
                        }
                        Event::Cached { id, version } => {
                            push(format!("already have {id} {version}"))
                        }
                        Event::Failed { id, error } => push(format!("FAILED {id}: {error}")),
                        _ => {}
                    })
                };

                let (lock, failures) =
                    engine.sync(pack, &profile, &previous, Some(reporter)).await?;
                lock.save(&lock_path)?;

                // Diff old against new so the UI can say what actually moved,
                // rather than leaving the user to reconstruct it from a log.
                let mut outcomes: Vec<SyncOutcome> = lock
                    .mods
                    .iter()
                    .map(|now| {
                        let before = previous.get(&now.id).map(|l| l.version.clone());
                        let kind = match &before {
                            None => OutcomeKind::New,
                            Some(v) if v != &now.version => OutcomeKind::Updated,
                            Some(_) => OutcomeKind::Unchanged,
                        };
                        SyncOutcome {
                            id: now.id.clone(),
                            from: before,
                            to: Some(now.version.clone()),
                            kind,
                            detail: now.trust.level.short().to_string(),
                            page: None,
                        }
                    })
                    .collect();
                outcomes.extend(failures.iter().map(|issue| SyncOutcome {
                    id: issue.id.clone(),
                    from: previous.get(&issue.id).map(|l| l.version.clone()),
                    to: None,
                    // A mod with no build for this version is waiting, not
                    // broken — it stays listed and greyed rather than red.
                    kind: if issue.waiting {
                        OutcomeKind::Waiting
                    } else {
                        OutcomeKind::Failed
                    },
                    detail: issue.message.clone(),
                    page: None,
                }));

                // An update leaves the previous version behind under its own
                // hash with nothing pointing at it. Clear it now rather than
                // hoarding every build ever downloaded.
                if let Ok((entries, bytes)) = engine.gc() {
                    if entries > 0 {
                        push(format!(
                            "removed {entries} superseded download(s), freeing {}",
                            format_bytes(bytes)
                        ));
                    }
                }
                // Fold in any "a newer version exists but nothing can fetch it"
                // notices raised while resolving.
                if let Ok(mut list) = manual.lock() {
                    outcomes.append(&mut list);
                }
                Ok::<_, modifile_core::Error>(outcomes)
            });

            match result {
                Ok(outcomes) => {
                    let _ = tx.send(Msg::Synced(outcomes));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(e.to_string()));
                }
            }
            ctx.request_repaint();
        });
    }

    fn do_deploy(&mut self, force: bool) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let lock = self.lock.clone();
        let mut messages = Vec::new();
        let mut deployed = false;

        for row in &self.targets {
            let Some(root) = &row.root else {
                messages.push(format!(
                    "{}: no folder set — press Choose folder first",
                    row.target.name
                ));
                continue;
            };
            match engine
                .plan(pack, &profile, &lock, &row.target, root)
                .and_then(|plan| {
                    let report = engine.deploy(
                        pack,
                        &row.target,
                        &plan,
                        &profile.name,
                        modifile_core::engine::DeployOptions {
                            force,
                            assume_stopped: self.assume_stopped,
                        },
                    )?;
                    Ok((plan, report))
                }) {
                Ok((plan, report)) => {
                    deployed = true;
                    messages.push(format!(
                        "{}: {} file(s) linked ({}), {} removed",
                        row.target.name,
                        report.linked,
                        format_bytes(report.bytes),
                        report.removed
                    ));
                    if report.adopted > 0 {
                        messages.push(format!(
                            "  {} settings file(s) already in the game folder now belong to \
                             this profile",
                            report.adopted
                        ));
                    }
                    if report.captured > 0 {
                        messages.push(format!(
                            "  {} settings file(s) saved into the previously active profile",
                            report.captured
                        ));
                    }
                    if report.seeded > 0 {
                        messages.push(format!(
                            "  {} default settings file(s) created",
                            report.seeded
                        ));
                    }
                    for conflict in &plan.conflicts {
                        messages.push(format!(
                            "  {} — {} wins over {}",
                            display_path(&conflict.rel),
                            conflict.winner,
                            conflict.losers.join(", ")
                        ));
                    }
                    for (path, reason) in &report.skipped {
                        messages.push(format!("  skipped {}: {reason}", display_path(path)));
                    }
                }
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        if deployed {
            messages.push("Done. Launch the game normally — nothing needs to stay open.".into());
        }
        for message in messages {
            self.log_line(message);
        }

        // Keep the folder panel honest: if the user was looking at a scan, show
        // them the state after the change rather than a stale one.
        let had_scan = !self.scans.is_empty();
        self.refresh();
        if had_scan {
            self.do_scan();
        }
    }

    fn do_verify(&mut self) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let mut messages = Vec::new();
        for row in &self.targets {
            let Ok(Some(manifest)) = engine.manifest(pack.id(), &row.target.id) else {
                continue;
            };
            let report = modifile_core::deploy::verify(&manifest);
            messages.push(if report.is_clean() {
                format!("{}: all {} file(s) intact", row.target.name, report.ok)
            } else {
                format!(
                    "{}: {} intact, {} missing, {} changed — a game update probably \
                     overwrote them; press Deploy again",
                    row.target.name,
                    report.ok,
                    report.missing.len(),
                    report.modified.len()
                )
            });
        }
        if messages.is_empty() {
            messages.push("Nothing is installed in the game yet.".into());
        }
        for message in messages {
            self.log_line(message);
        }
    }

    fn do_undeploy(&mut self) {
        self.undeploy_with(false);
    }

    /// `force` also deletes installed files that have since changed — a build
    /// dropped in by hand, or one a game update overwrote.
    fn undeploy_with(&mut self, force: bool) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let game = pack.id().to_string();
        let ids: Vec<String> = self.targets.iter().map(|r| r.target.id.clone()).collect();
        let mut messages = Vec::new();
        let mut leftovers = 0usize;
        for target in ids {
            match engine.undeploy(
                &game,
                &target,
                modifile_core::engine::DeployOptions {
                    force,
                    assume_stopped: self.assume_stopped,
                },
            ) {
                Ok(report) => {
                    messages.push(format!("{target}: {} file(s) removed", report.removed));
                    // Silence here was the bug: files could be left behind and
                    // nothing said so, which reads as "deactivate did nothing".
                    if !report.skipped.is_empty() {
                        leftovers += report.skipped.len();
                        messages.push(format!(
                            "  {} file(s) LEFT IN THE GAME — they no longer match what was \
                             installed, so something replaced them since:",
                            report.skipped.len()
                        ));
                        for (path, _) in &report.skipped {
                            messages.push(format!("    {}", display_path(path)));
                        }
                    }
                }
                Err(e) => messages.push(format!("{target}: {e}")),
            }
        }
        for message in messages {
            self.log_line(message);
        }
        self.leftovers = leftovers;
        if leftovers > 0 {
            self.log_line(
                "  The game is not fully vanilla. Use \"Delete leftover files\" to remove them.",
            );
        }
        self.refresh();
    }

    /// Search whichever index this game's pack nominates.
    fn do_search(&mut self, ctx: &egui::Context) {
        let (Some(profile), query) = (self.profile.clone(), self.search_input.trim().to_string())
        else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let paths = self.paths.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.searching = true;
        self.search_results.clear();

        let handle = self.runtime.handle().clone();
        std::thread::spawn(move || {
            let result = handle.block_on(async {
                let engine = Engine::open(paths.clone(), load_token(&paths))?;
                let pack = engine.pack_for(&profile)?;
                let filter = modifile_core::source::modrinth::VersionFilter {
                    game_version: profile.game_version.clone(),
                    loader: profile.loader.clone(),
                };
                engine.search(pack, &query, &filter, true).await
            });
            match result {
                Ok(hits) => {
                    let _ = tx.send(Msg::Found(hits));
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(e.to_string()));
                    let _ = tx.send(Msg::Found(Vec::new()));
                }
            }
            ctx.request_repaint();
        });
    }

    /// Add a mod straight from a search result.
    fn add_id(&mut self, id: &ModId) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            if profile.add(ModEntry::new(id.clone())) {
                let _ = profile.save(&path);
                self.log_line(format!(
                    "Added {id}. Press Check for updates to download it."
                ));
            } else {
                self.log_line(format!("{id} is already in this profile."));
            }
        }
        self.refresh();
    }

    /// Import a file the user downloaded themselves.
    fn do_add_file(&mut self) {
        let (Some(name), Some(engine)) = (self.selected.clone(), self.engine()) else {
            return;
        };
        let Ok(mut profile) = Profile::load(&self.paths.profile_file(&name)) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };

        let Some(file) = rfd::FileDialog::new()
            .set_title("Choose the mod file you downloaded")
            .add_filter("Mod files", &["zip", "jar", "dll"])
            .pick_file()
        else {
            self.log_line(
                "No file chosen. If the dialog did not open, use the CLI: \
                 modifile add-file <profile> <file>",
            );
            return;
        };

        match engine.import_file(pack, &mut profile, &file, None) {
            Ok(entry) => {
                let _ = profile.save(&self.paths.profile_file(&name));
                let lock_path = self.paths.lock_file(&name);
                let mut lock = Lock::load(&lock_path).unwrap_or_default();
                lock.mods.retain(|m| m.id != entry.id);
                lock.mods.push(entry.clone());
                let _ = lock.save(&lock_path);

                self.log_line(format!(
                    "Added {} from your file ({}). Press Activate to install it.",
                    entry.id,
                    format_bytes(entry.size)
                ));
                self.log_line("  It will not update on its own — add a newer file to update.");
                self.refresh();
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    fn do_add(&mut self) {
        let input = self.add_input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let Some(name) = self.selected.clone() else {
            return;
        };
        match input.parse::<ModId>() {
            Ok(id) => {
                let path = self.paths.profile_file(&name);
                if let Ok(mut profile) = Profile::load(&path) {
                    if profile.add(ModEntry::new(id.clone())) {
                        let _ = profile.save(&path);
                        self.log_line(format!(
                            "Added {id}. Press Check for updates to download it."
                        ));
                    } else {
                        self.log_line(format!("{id} is already in this profile."));
                    }
                }
                self.add_input.clear();
                self.refresh();
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    fn do_remove(&mut self, id: &ModId) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            profile.remove(id);
            let _ = profile.save(&path);
            self.log_line(format!(
                "Removed {id} from the profile. Press Activate to take it out of the game."
            ));
        }
        self.refresh();
    }

    /// Mark a mod client-only, server-only, or both.
    fn do_set_side(&mut self, id: &ModId, side: Side) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let targets = side.target_ids(&self.client_ids, &self.server_ids);
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            if let Some(entry) = profile.mods.iter_mut().find(|m| &m.id == id) {
                entry.targets = targets;
            }
            let _ = profile.save(&path);
            self.log_line(format!(
                "{id} is now {}. Press Activate to apply it.",
                side.label()
            ));
        }
        self.refresh();
    }

    fn do_toggle(&mut self, id: &ModId) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            if let Some(entry) = profile.mods.iter_mut().find(|m| &m.id == id) {
                entry.enabled = !entry.enabled;
            }
            let _ = profile.save(&path);
        }
        self.refresh();
    }

    /// Open the folder chooser for a target.
    ///
    /// This is a dialog rather than a straight call to the native picker on
    /// purpose. On Linux `rfd` goes through the XDG desktop portal, and on a
    /// box without `xdg-desktop-portal` installed it returns nothing at all —
    /// which would leave the user with a button that does nothing and no way
    /// to proceed. Typing a path always works, so that is the primary control
    /// and Browse is the convenience.
    fn open_root_dialog(&mut self, game: &str, target: &Target) {
        self.root_input = self
            .engine()
            .and_then(|e| e.roots.get(game, &target.id).cloned())
            .or_else(|| {
                self.targets
                    .iter()
                    .find(|r| r.target.id == target.id)
                    .and_then(|r| r.root.clone())
            })
            .map(|p| display_path(&p))
            .unwrap_or_default();
        self.root_dialog = Some((game.to_string(), target.clone()));
    }

    /// Native picker, if this system has one. `None` also means "the portal is
    /// missing", which is not an error worth shouting about.
    fn browse_for_folder(&self, target: &Target, start: &str) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new().set_title(format!("Where is {}?", target.name));
        if !start.is_empty() && Path::new(start).is_dir() {
            dialog = dialog.set_directory(start);
        }
        dialog.pick_folder()
    }

    fn commit_root(&mut self, game: &str, target: &Target, path: PathBuf) {
        // Say if the folder does not look right, but allow it anyway —
        // unusual installs are exactly why this control exists.
        let looks_right = self
            .engine()
            .and_then(|e| e.pack(game))
            .map(|pack| pack.matches_markers(target, &path))
            .unwrap_or(false);

        if let Some(engine) = self.engine.as_mut() {
            match engine.set_root(game, &target.id, path.clone()) {
                Ok(()) => {
                    let note = if looks_right {
                        ""
                    } else {
                        " (warning: no expected game file found here)"
                    };
                    self.log_line(format!("{} -> {}{note}", target.name, display_path(&path)));
                }
                Err(e) => self.log_line(e.to_string()),
            }
        }
        self.root_dialog = None;
        self.refresh();
    }

    fn root_dialog_window(&mut self, ctx: &egui::Context) {
        let Some((game, target)) = self.root_dialog.clone() else {
            return;
        };
        let mut open = true;
        let mut commit: Option<PathBuf> = None;
        let mut cancel = false;

        egui::Window::new(format!("Where is {}?", target.name))
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Folder");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.root_input)
                            .desired_width(400.0)
                            .hint_text("/home/you/ValheimServer/server"),
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(picked) = self.browse_for_folder(&target, &self.root_input) {
                            self.root_input = display_path(&picked);
                        } else {
                            self.log_line(
                                "No folder chosen. If the Browse button does nothing, type the \
                                 path here instead — on Linux it needs xdg-desktop-portal.",
                            );
                        }
                    }
                });

                ui.add_space(6.0);
                let typed = Path::new(self.root_input.trim());
                let exists = !self.root_input.trim().is_empty() && typed.is_dir();
                let looks_right = exists
                    && self
                        .engine()
                        .and_then(|e| e.pack(&game))
                        .map(|pack| pack.matches_markers(&target, typed))
                        .unwrap_or(false);

                let (message, color) = if self.root_input.trim().is_empty() {
                    (
                        format!("Look for the folder containing {}", markers_hint(&target)),
                        theme::MUTED,
                    )
                } else if !exists {
                    ("That folder does not exist.".to_string(), theme::BAD)
                } else if looks_right {
                    ("Looks right.".to_string(), theme::GOOD)
                } else {
                    (
                        format!(
                            "No {} here — you can still use it, but check the path.",
                            markers_hint(&target)
                        ),
                        theme::WARN,
                    )
                };
                ui.label(egui::RichText::new(message).small().color(color));

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(exists, egui::Button::new("Use this folder"))
                        .clicked()
                    {
                        commit = Some(typed.to_path_buf());
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
                ui.add_space(2.0);
            });

        if let Some(path) = commit {
            self.commit_root(&game, &target, path);
        } else if cancel || !open {
            self.root_dialog = None;
        }
    }

    fn forget_root(&mut self, game: &str, target_id: &str) {
        if let Some(engine) = self.engine.as_mut() {
            let _ = engine.clear_root(game, target_id);
        }
        self.log_line("Folder forgotten; back to autodetection.");
        self.refresh();
    }

    /// Replace bundled packs with the ones this build ships, backing up first.
    fn do_refresh_packs(&mut self) {
        match modifile_core::install_bundled_packs_with(&self.paths, true) {
            Ok(report) => {
                for (name, backup) in &report.replaced {
                    self.log_line(format!(
                        "updated {name} (your copy saved as {})",
                        display_path(backup)
                    ));
                }
                for name in report.updated.iter().chain(report.written.iter()) {
                    self.log_line(format!("updated {name}"));
                }
                // Packs are read at startup, so reload the engine to pick them up.
                self.engine = Engine::open(self.paths.clone(), load_token(&self.paths)).ok();
                self.log_line("Game packs are current. New games appear in the list.");
                self.reload_profiles();
                self.refresh();
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    fn set_curseforge_direct(&mut self, on: bool) {
        if let Some(engine) = self.engine.as_mut() {
            match engine.set_curseforge_direct(on) {
                Ok(()) => {
                    self.curseforge_direct = on;
                    self.log_line(if on {
                        "Blocked CurseForge mods will now be downloaded directly."
                    } else {
                        "Blocked CurseForge mods will be reported, not downloaded."
                    });
                }
                Err(e) => self.log_line(e.to_string()),
            }
        }
    }

    /// `clear` distinguishes "I want no key" from an accidentally empty field,
    /// which previously deleted a working key without asking.
    fn save_curseforge_key(&mut self, clear: bool) {
        let key = if clear {
            String::new()
        } else {
            self.curseforge_input.clone()
        };
        if !clear && key.trim().is_empty() {
            self.log_line("Nothing to save. Use Clear if you want to remove the saved key.");
            return;
        }
        if let Some(engine) = self.engine.as_mut() {
            match engine.set_curseforge_key(&key) {
                Ok(()) if clear => {
                    self.curseforge_input.clear();
                    self.log_line("CurseForge key cleared.");
                }
                Ok(()) => self.log_line("CurseForge key saved."),
                Err(e) => self.log_line(e.to_string()),
            }
        }
    }

    /// Same shape as the CurseForge key: an empty field is a mistake, not an
    /// instruction to delete a working credential.
    fn save_token(&mut self, clear: bool) {
        let token = if clear {
            String::new()
        } else {
            self.token_input.clone()
        };
        if !clear && token.trim().is_empty() {
            self.log_line("Nothing to save. Use Clear if you want to remove the saved token.");
            return;
        }
        if let Some(engine) = self.engine.as_mut() {
            match engine.set_token(&token) {
                Ok(()) if clear => {
                    self.token_input.clear();
                    self.log_line("Token cleared.");
                }
                Ok(()) => self.log_line("Token saved. 5000 requests/hour."),
                Err(e) => self.log_line(e.to_string()),
            }
        }
    }

    fn create_profile(&mut self) {
        let name = self.new_profile_name.trim().to_string();
        let game = self.new_profile_game.clone();
        if name.is_empty() || game.is_empty() {
            self.log_line("A profile needs a name and a game.");
            return;
        }
        let path = self.paths.profile_file(&name);
        if path.exists() {
            self.log_line(format!("Profile `{name}` already exists."));
            return;
        }
        if let Err(e) = Profile::new(&name, &game).save(&path) {
            self.log_line(e.to_string());
            return;
        }
        self.show_new_profile = false;
        self.new_profile_name.clear();
        self.reload_profiles();
        self.select(&name);
        self.log_line(format!(
            "Created `{name}`. Add mods below, then press Check for updates."
        ));
    }

    fn open_new_profile(&mut self) {
        if self.new_profile_game.is_empty() {
            self.new_profile_game = self
                .engine()
                .and_then(|e| e.packs.first())
                .map(|p| p.id().to_string())
                .unwrap_or_default();
        }
        self.show_new_profile = true;
    }

    fn open_rename(&mut self) {
        if let Some(name) = self.selected.clone() {
            self.rename_input = name;
            self.show_rename = true;
        }
    }

    fn commit_rename(&mut self) {
        let (Some(from), to) = (self.selected.clone(), self.rename_input.trim().to_string())
        else {
            return;
        };
        if to.is_empty() || to == from {
            self.show_rename = false;
            return;
        }
        let Some(engine) = self.engine() else { return };
        match engine.rename_profile(&from, &to) {
            Ok(name) => {
                self.log_line(format!("`{from}` is now `{name}`."));
                self.show_rename = false;
                self.reload_profiles();
                self.select(&name);
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    /// Write this profile to a file the user can send to a friend.
    fn do_export_bundle(&mut self, include_configs: bool) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let bundle = match engine.export_profile(pack, &profile, include_configs, String::new()) {
            Ok(b) => b,
            Err(e) => {
                self.log_line(e.to_string());
                return;
            }
        };

        let suggested = format!("{}.modifile.json", profile.name);
        let picked = rfd::FileDialog::new()
            .set_title("Save shared profile")
            .set_file_name(&suggested)
            .add_filter("Modifile profile", &["json"])
            .save_file();

        // Without a portal there is no dialog, so fall back to the data folder
        // rather than doing nothing.
        let path = picked.unwrap_or_else(|| self.paths.home.join(&suggested));
        match bundle.save(&path) {
            Ok(()) => {
                self.log_line(format!(
                    "Exported {} mod(s) and {} config file(s) to {}",
                    bundle.mod_count(),
                    bundle.config_count(),
                    display_path(&path)
                ));
                self.log_line("  Send that file to anyone — they use Import a shared profile.");
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    fn do_import_bundle(&mut self, pin_versions: bool) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Open a shared profile")
            .add_filter("Modifile profile", &["json"])
            .pick_file()
        else {
            self.log_line(
                "No file chosen. If the dialog did not open, copy the .modifile.json into the \
                 data folder and use the CLI: modifile import <file>",
            );
            return;
        };

        let bundle = match modifile_core::share::Bundle::load(&path) {
            Ok(b) => b,
            Err(e) => {
                self.log_line(e.to_string());
                return;
            }
        };
        let Some(engine) = self.engine() else { return };
        match engine.import_profile(&bundle, None, pin_versions) {
            Ok(name) => {
                self.log_line(format!(
                    "Imported `{name}` — {} mod(s), {} config file(s), {}.",
                    bundle.mod_count(),
                    bundle.config_count(),
                    if pin_versions {
                        "pinned to the sender's versions"
                    } else {
                        "taking the newest versions"
                    }
                ));
                self.log_line("  Press Check for updates to download them, then Activate.");
                self.reload_profiles();
                self.select(&name);
            }
            Err(e) => self.log_line(e.to_string()),
        }
    }

    fn run_gc(&mut self) {
        if let Some(engine) = self.engine() {
            match engine.gc() {
                Ok((entries, bytes)) => self.log_line(format!(
                    "Removed {entries} unused download(s), freeing {}.",
                    format_bytes(bytes)
                )),
                Err(e) => self.log_line(e.to_string()),
            }
        }
    }
}

fn load_token(paths: &Paths) -> Option<String> {
    for var in ["MODIFILE_GITHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.trim().is_empty() {
                return Some(token.trim().to_string());
            }
        }
    }
    std::fs::read_to_string(paths.token_file())
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Draw the active/inactive dot.
///
/// Painted rather than written. eframe's bundled fonts cover Latin text and
/// emoji but not the geometric-shapes block, so `●` and `○` render as empty
/// tofu squares. A circle from the painter always looks the same everywhere.
fn status_dot(ui: &mut egui::Ui, active: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if active {
            painter.circle_filled(rect.center(), 4.5, theme::GOOD);
        } else {
            painter.circle_stroke(
                rect.center(),
                4.0,
                egui::Stroke::new(1.2, theme::MUTED),
            );
        }
    }
    response
}

/// Which side of a game a mod installs on.
///
/// A pack's targets are richer than this (WoW has three client flavors), but
/// "client or server" is how people actually think about a mod, so that is what
/// the UI offers. It maps onto whichever target ids have that kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    Both,
    ClientOnly,
    ServerOnly,
}

impl Side {
    /// Work out the side from a mod's stored target list.
    fn of(targets: &Option<Vec<String>>, client: &[String], server: &[String]) -> Side {
        let Some(targets) = targets else {
            return Side::Both;
        };
        let touches_client = targets.iter().any(|t| client.contains(t));
        let touches_server = targets.iter().any(|t| server.contains(t));
        match (touches_client, touches_server) {
            (true, false) => Side::ClientOnly,
            (false, true) => Side::ServerOnly,
            _ => Side::Both,
        }
    }

    /// The target ids this side corresponds to, or `None` for "all of them".
    fn target_ids(self, client: &[String], server: &[String]) -> Option<Vec<String>> {
        match self {
            Side::Both => None,
            Side::ClientOnly => Some(client.to_vec()),
            Side::ServerOnly => Some(server.to_vec()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Side::Both => "client + server",
            Side::ClientOnly => "client only",
            Side::ServerOnly => "server only",
        }
    }

    fn menu_label(self) -> &'static str {
        match self {
            Side::Both => "Client and server",
            Side::ClientOnly => "Client only",
            Side::ServerOnly => "Server only",
        }
    }

    fn explain(self) -> &'static str {
        match self {
            Side::Both => "Installed on every target this profile covers.",
            Side::ClientOnly => "Installed on the game client only — never on a dedicated server.",
            Side::ServerOnly => "Installed on the dedicated server only — never on your client.",
        }
    }

    fn color(self) -> egui::Color32 {
        match self {
            Side::Both => theme::MUTED,
            Side::ClientOnly => theme::MUTED,
            Side::ServerOnly => theme::ACCENT,
        }
    }
}

/// The file that identifies a target, for telling the user what to look for.
fn markers_hint(target: &Target) -> String {
    match target.markers.first() {
        Some(first) => first.clone(),
        None => "the game files".to_string(),
    }
}

/// Show a folder in the system file manager.
fn reveal(path: &Path) {
    let _ = if cfg!(windows) {
        std::process::Command::new("explorer").arg(path).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Error(e) => {
                    self.busy = false;
                    self.log_line(format!("error: {e}"));
                }
                Msg::Synced(outcomes) => {
                    self.busy = false;
                    self.last_sync = outcomes;
                    self.refresh();
                }
                Msg::Done => {
                    self.busy = false;
                    self.refresh();
                }
                Msg::Found(hits) => {
                    self.searching = false;
                    if hits.is_empty() {
                        self.log_line("Nothing found.");
                    }
                    self.search_results = hits;
                }
            }
        }

        self.top_bar(ui);
        self.status_bar(ui);
        self.sidebar(ui);
        self.log_panel(ui);

        egui::CentralPanel::default()
            .frame(theme::panel_frame())
            .show(ui, |ui| {
                // The whole page scrolls. Without this the lower sections were
                // simply unreachable and the only fix was resizing the window.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.view {
                        View::Profile => self.profile_view(ui),
                        View::Games => self.games_view(ui),
                        View::Settings => self.settings_view(ui),
                    });
            });

        if self.show_new_profile {
            let ctx = ui.ctx().clone();
            self.new_profile_window(&ctx);
        }
        if self.root_dialog.is_some() {
            let ctx = ui.ctx().clone();
            self.root_dialog_window(&ctx);
        }
        if self.show_rename {
            let ctx = ui.ctx().clone();
            self.rename_window(&ctx);
        }
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top")
            .frame(theme::bar_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Modifile");
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new("universal mod manager").color(theme::MUTED),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let ready = self.profile.is_some() && !self.busy;
                        let has_mods = !self.rows.is_empty();
                        let has_lock = !self.lock.mods.is_empty();
                        let running = self
                            .targets
                            .iter()
                            .find_map(|t| t.running.clone());

                        let active = self
                            .selected
                            .as_deref()
                            .map(|n| self.is_active(n))
                            .unwrap_or(false);

                        // One button in one place: it says what pressing it will
                        // do right now, rather than offering both states at once.
                        if active {
                            if ui
                                .add_enabled(
                                    ready && running.is_none(),
                                    egui::Button::new("Deactivate").fill(theme::ACCENT_DIM),
                                )
                                .on_hover_text(
                                    "Take these mods out of the game folder so it runs vanilla. \
                                     Nothing is lost — your settings are saved into the profile \
                                     and you can activate it again any time.",
                                )
                                .on_disabled_hover_text(match &running {
                                    Some(found) => format!("The game is running ({found})."),
                                    None => String::new(),
                                })
                                .clicked()
                            {
                                self.do_undeploy();
                            }
                        } else if ui
                            .add_enabled(
                                ready && has_lock && running.is_none(),
                                egui::Button::new("Activate").fill(theme::ACCENT_DIM),
                            )
                            .on_hover_text(
                                "Put these mods into the game folder. You still start the game \
                                 yourself afterwards — Modifile is not a launcher.",
                            )
                            .on_disabled_hover_text(match &running {
                                Some(found) => format!(
                                    "The game is running ({found}).\nClose it before changing mods."
                                ),
                                None => "Check for updates first, to download the mods".to_string(),
                            })
                            .clicked()
                        {
                            self.do_deploy(false);
                        }

                        if ui
                            .add_enabled(ready && has_mods, egui::Button::new("Check for updates"))
                            .on_hover_text(
                                "Ask GitHub for the newest version of each mod and download \
                                 anything missing. Does not touch the game folder.",
                            )
                            .on_disabled_hover_text("Add a mod first")
                            .clicked()
                        {
                            let ctx = ui.ctx().clone();
                            self.do_sync(&ctx);
                        }

                        ui.add_space(6.0);
                        ui.menu_button("More", |ui| {
                            // With a single toggle button, this is how you push
                            // a change made while the profile is already active.
                            if active
                                && ui
                                    .button("Apply changes to the game")
                                    .on_hover_text(
                                        "Re-run the install for this profile, picking up mods \
                                         you added or removed since activating",
                                    )
                                    .clicked()
                            {
                                self.do_deploy(false);
                                ui.close();
                            }
                            if ui.button("Rename profile…").clicked() {
                                self.open_rename();
                                ui.close();
                            }
                            if ui
                                .button("Export to a file…")
                                .on_hover_text("Mod list, versions and your settings")
                                .clicked()
                            {
                                self.do_export_bundle(true);
                                ui.close();
                            }
                            if ui
                                .button("Export without my settings…")
                                .on_hover_text("Mod list and versions only")
                                .clicked()
                            {
                                self.do_export_bundle(false);
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Check installed files").clicked() {
                                self.do_verify();
                                ui.close();
                            }
                            if ui.button("Activate, overwriting other files").clicked() {
                                self.do_deploy(true);
                                ui.close();
                            }
                        });
                        if self.busy {
                            ui.add(egui::Spinner::new());
                        }
                    });
                });
            });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(theme::bar_frame())
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let store = self
                        .engine()
                        .map(|e| e.store.size_bytes())
                        .unwrap_or_default();
                    ui.label(
                        egui::RichText::new(format!("downloads {}", format_bytes(store)))
                            .color(theme::MUTED),
                    );
                    ui.separator();

                    let authed = self
                        .engine()
                        .map(|e| e.github.http().has_token())
                        .unwrap_or(false);
                    if authed {
                        ui.label(egui::RichText::new("GitHub token set").color(theme::MUTED));
                    } else if ui
                        .link(egui::RichText::new("no GitHub token — limited to 60 checks/hour").color(theme::WARN))
                        .clicked()
                    {
                        self.view = View::Settings;
                    }
                });
            });
    }

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("nav")
            .exact_size(216.0)
            .frame(theme::panel_frame())
            .show(ui, |ui| {
                ui.add_space(4.0);
                if ui
                    .add_sized(
                        [ui.available_width(), 30.0],
                        egui::Button::new("+  New profile").fill(theme::ACCENT_DIM),
                    )
                    .clicked()
                {
                    self.open_new_profile();
                }
                ui.add_space(10.0);

                ui.menu_button("Import a shared profile…", |ui| {
                    if ui
                        .button("Exactly as they had it")
                        .on_hover_text("Pins every mod to the version the sender was running")
                        .clicked()
                    {
                        self.do_import_bundle(true);
                        ui.close();
                    }
                    if ui
                        .button("But take the newest versions")
                        .on_hover_text("Same mod list, latest release of each")
                        .clicked()
                    {
                        self.do_import_bundle(false);
                        ui.close();
                    }
                });
                ui.add_space(12.0);

                if self.profiles.is_empty() {
                    ui.label(egui::RichText::new("PROFILES").small().color(theme::MUTED));
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("none yet").color(theme::MUTED));
                }

                // Grouped by game, so a dozen profiles across three games stay
                // legible. The active one in each game is marked with a dot.
                let profiles = self.profiles.clone();
                let mut current_game: Option<String> = None;
                for entry in profiles {
                    if current_game.as_deref() != Some(entry.game_name.as_str()) {
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(entry.game_name.to_uppercase())
                                .small()
                                .color(theme::MUTED),
                        );
                        ui.add_space(2.0);
                        current_game = Some(entry.game_name.clone());
                    }

                    let selected = self.view == View::Profile
                        && self.selected.as_deref() == Some(entry.name.as_str());

                    ui.horizontal(|ui| {
                        status_dot(ui, entry.active).on_hover_text(if entry.active {
                            "Active — these mods are in the game folder right now"
                        } else {
                            "Not active"
                        });
                        if ui
                            .selectable_label(selected, &entry.name)
                            .on_hover_text(format!("{} mod(s)", entry.mods))
                            .clicked()
                        {
                            self.select(&entry.name);
                        }
                    });
                }

                ui.add_space(14.0);
                ui.separator();
                ui.add_space(6.0);

                if ui
                    .selectable_label(self.view == View::Games, "Games & folders")
                    .clicked()
                {
                    self.view = View::Games;
                }
                if ui
                    .selectable_label(self.view == View::Settings, "Settings")
                    .clicked()
                {
                    self.view = View::Settings;
                    // Load the listing on open, so the panel is never a button
                    // you have to discover before it shows anything.
                    self.refresh_storage();
                }
            });
    }

    /// The activity log.
    ///
    /// Collapsed to a single line by default. Everything that matters now has a
    /// proper panel of its own — what changed, what is in the game folder, what
    /// is stored — so this is a detail view, not something that should be
    /// permanently eating a third of the window.
    fn log_panel(&mut self, ui: &mut egui::Ui) {
        let (last, count) = match self.log.lock() {
            Ok(log) => (log.last().cloned(), log.len()),
            Err(_) => (None, 0),
        };

        if !self.log_open {
            egui::Panel::bottom("log")
                .resizable(false)
                .frame(theme::bar_frame())
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let summary = last
                            .clone()
                            .unwrap_or_else(|| "Ready.".to_string())
                            .trim_start()
                            .to_string();
                        let colour = if summary.contains("FAILED") || summary.starts_with("error") {
                            theme::BAD
                        } else {
                            theme::MUTED
                        };
                        ui.label(egui::RichText::new(summary).small().color(colour));

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button(if count > 0 {
                                    format!("details ({count})")
                                } else {
                                    "details".to_string()
                                })
                                .clicked()
                            {
                                self.log_open = true;
                            }
                        });
                    });
                });
            return;
        }

        egui::Panel::bottom("log")
            .resizable(true)
            .default_size(150.0)
            .frame(theme::panel_frame())
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("ACTIVITY").small().color(theme::MUTED));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("hide").clicked() {
                            self.log_open = false;
                        }
                        if ui.small_button("clear").clicked() {
                            if let Ok(mut log) = self.log.lock() {
                                log.clear();
                            }
                        }
                    });
                });
                ui.add_space(2.0);
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if let Ok(log) = self.log.lock() {
                            if log.is_empty() {
                                ui.label(
                                    egui::RichText::new("Nothing yet.").color(theme::MUTED),
                                );
                            }
                            for line in log.iter() {
                                let color = if line.contains("FAILED") || line.starts_with("error")
                                {
                                    theme::BAD
                                } else if line.starts_with("  ") {
                                    theme::MUTED
                                } else {
                                    theme::TEXT
                                };
                                ui.label(egui::RichText::new(line).monospace().color(color));
                            }
                        }
                    });
            });
    }

    // --- views ------------------------------------------------------------

    fn profile_view(&mut self, ui: &mut egui::Ui) {
        if self.profile.is_none() {
            self.welcome(ui);
            return;
        }
        let profile = self.profile.clone().expect("checked above");
        let game_name = self
            .engine()
            .and_then(|e| e.pack(&profile.game))
            .map(|p| p.pack.game.name.clone())
            .unwrap_or_else(|| profile.game.clone());

        let active = self.is_active(&profile.name);

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(&profile.name);
            ui.label(egui::RichText::new(&game_name).color(theme::MUTED));
            if ui.small_button("rename").clicked() {
                self.open_rename();
            }
        });

        // The single most important fact about a profile, stated plainly.
        ui.add_space(6.0);
        egui::Frame::NONE
            .fill(if active { theme::GOOD_DIM } else { theme::CARD })
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    status_dot(ui, active);
                    if active {
                        ui.label(egui::RichText::new("Active").strong().color(theme::GOOD));
                        ui.label(
                            egui::RichText::new(format!(
                                "— these mods are in your {game_name} folder now. \
                                 Start the game as you normally would; Modifile does not \
                                 launch it."
                            ))
                            .color(theme::MUTED),
                        );
                    } else {
                        ui.label(egui::RichText::new("Not active").strong());
                        ui.label(
                            egui::RichText::new(
                                "— the game is vanilla. Press Activate to put these mods in.",
                            )
                            .color(theme::MUTED),
                        );
                    }
                });
            });

        // Deactivating can leave files behind, and saying nothing about it reads
        // as "deactivate did not work".
        if self.leftovers > 0 {
            ui.add_space(6.0);
            egui::Frame::NONE
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "{} file(s) are still in the game folder.",
                            self.leftovers
                        ))
                        .strong()
                        .color(theme::WARN),
                    );
                    ui.label(
                        egui::RichText::new(
                            "They no longer match what Modifile installed — something replaced \
                             them, usually your own build dropped in by hand or a game update. \
                             They were left alone rather than deleted.",
                        )
                        .color(theme::MUTED),
                    );
                    ui.add_space(6.0);
                    if ui
                        .button("Delete leftover files")
                        .on_hover_text("Removes them so the game is truly vanilla")
                        .clicked()
                    {
                        self.pending_force_undeploy = true;
                    }
                });
        }

        ui.add_space(10.0);
        self.version_section(ui, &profile);
        self.targets_section(ui, &profile.game);
        ui.add_space(12.0);
        self.last_update_section(ui);
        self.folder_section(ui);
        ui.add_space(12.0);
        self.configs_section(ui);
        ui.add_space(12.0);
        self.mods_section(ui);

        // Acted on after drawing, so the panel is not mutated mid-borrow.
        if std::mem::take(&mut self.pending_force_undeploy) {
            self.undeploy_with(true);
        }
    }

    /// Which game version and mod loader this profile is for.
    ///
    /// Only shown for games where it matters. Minecraft mods ship one build per
    /// loader, so without this a profile happily mixes a NeoForge build of one
    /// mod with a Fabric build of another and the game stops starting.
    fn version_section(&mut self, ui: &mut egui::Ui, profile: &Profile) {
        let Some(rules) = self
            .engine()
            .and_then(|e| e.pack(&profile.game))
            .map(|p| p.pack.versions.clone())
        else {
            return;
        };
        if !rules.applies() {
            return;
        }

        let missing = (!rules.loaders.is_empty() && profile.loader.is_none())
            || (rules.needs_game_version && profile.game_version.is_none());
        let mut set_loader: Option<String> = None;
        let mut commit_version = false;

        ui.label(
            egui::RichText::new("GAME VERSION")
                .small()
                .color(theme::MUTED),
        );
        ui.add_space(4.0);
        egui::Frame::NONE
            .fill(if missing { theme::CARD } else { theme::CARD })
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if rules.needs_game_version {
                        ui.label("Version");
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut self.version_input)
                                    .desired_width(90.0)
                                    .hint_text("1.21.1"),
                            )
                            .lost_focus()
                        {
                            commit_version = true;
                        }
                        ui.add_space(10.0);
                    }
                    if !rules.loaders.is_empty() {
                        ui.label("Loader");
                        let current = profile
                            .loader
                            .clone()
                            .unwrap_or_else(|| "choose…".to_string());
                        egui::ComboBox::from_id_salt("loader")
                            .selected_text(current)
                            .width(130.0)
                            .show_ui(ui, |ui| {
                                for loader in &rules.loaders {
                                    if ui
                                        .selectable_label(
                                            profile.loader.as_deref() == Some(loader.as_str()),
                                            loader,
                                        )
                                        .clicked()
                                    {
                                        set_loader = Some(loader.clone());
                                    }
                                }
                            });
                    }
                });
                if missing {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Required before mods can be downloaded. Each mod is published as \
                             a separate build per loader, and mixing them gives you a game \
                             that will not start.",
                        )
                        .small()
                        .color(theme::WARN),
                    );
                }

                // Whether the loader itself is installed in the game — the step
                // people otherwise do on the loader's own website first.
                if let Some((def, state, _root)) = &self.loader_state {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        use modifile_core::loader::LoaderState;
                        let (text, colour) = match state {
                            LoaderState::Installed { version } => (
                                format!("{} {version} is installed", def.name),
                                theme::GOOD,
                            ),
                            LoaderState::WrongVersion { version } => (
                                format!("{} {version} is installed — wrong game version", def.name),
                                theme::WARN,
                            ),
                            LoaderState::NotInstalled => {
                                (format!("{} is not installed", def.name), theme::WARN)
                            }
                            LoaderState::Manual { .. } => (
                                format!("{} needs its own installer", def.name),
                                theme::MUTED,
                            ),
                        };
                        ui.label(egui::RichText::new(text).color(colour));

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| match state {
                                LoaderState::Manual { page } => {
                                    ui.hyperlink_to("Get the installer", page);
                                }
                                LoaderState::Installed { .. } => {
                                    if ui
                                        .small_button("Reinstall")
                                        .on_hover_text("Fetch the newest build of the loader")
                                        .clicked()
                                    {
                                        self.pending_install_loader = true;
                                    }
                                }
                                _ => {
                                    if ui
                                        .button(format!("Install {}", def.name))
                                        .on_hover_text(
                                            "Writes the loader into the game and adds it to \
                                             the Minecraft launcher. No separate installer, \
                                             no Java needed.",
                                        )
                                        .clicked()
                                    {
                                        self.pending_install_loader = true;
                                    }
                                }
                            },
                        );
                    });
                }
            });
        ui.add_space(12.0);

        if std::mem::take(&mut self.pending_install_loader) {
            let ctx = ui.ctx().clone();
            self.do_install_loader(&ctx);
        }
        if let Some(loader) = set_loader {
            self.set_profile_versions(Some(loader), None);
        }
        if commit_version {
            let v = self.version_input.trim().to_string();
            self.set_profile_versions(None, Some(v));
        }
    }

    /// Install the profile's mod loader into the game.
    ///
    /// Runs off the UI thread: it is two network calls plus a file write, but a
    /// slow mirror should not freeze the window.
    fn do_install_loader(&mut self, ctx: &egui::Context) {
        let (Some(profile), Some((_, _, root))) =
            (self.profile.clone(), self.loader_state.clone())
        else {
            return;
        };
        let paths = self.paths.clone();
        let log = self.log.clone();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        self.busy = true;

        let handle = self.runtime.handle().clone();
        std::thread::spawn(move || {
            let result = handle.block_on(async {
                let engine = Engine::open(paths.clone(), load_token(&paths))?;
                let pack = engine.pack_for(&profile)?;
                engine.install_loader(pack, &profile, &root).await
            });
            if let Ok(mut log) = log.lock() {
                match &result {
                    Ok(version) => {
                        log.push(format!("Installed mod loader {version}."));
                        log.push(
                            "  It now appears in the Minecraft launcher's version list."
                                .to_string(),
                        );
                    }
                    Err(e) => log.push(format!("error: {e}")),
                }
            }
            let _ = tx.send(Msg::Done);
            ctx.request_repaint();
        });
    }

    fn set_profile_versions(&mut self, loader: Option<String>, game_version: Option<String>) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            if let Some(loader) = loader {
                profile.loader = Some(loader);
            }
            if let Some(v) = game_version {
                profile.game_version = (!v.is_empty()).then_some(v);
            }
            let _ = profile.save(&path);
        }
        self.refresh();
    }

    /// The result of the last update, as a table.
    ///
    /// "What changed" is the whole reason to press the button, and reading it
    /// out of a scrolling log is no way to find out.
    fn last_update_section(&mut self, ui: &mut egui::Ui) {
        if self.last_sync.is_empty() {
            return;
        }
        let updated = self.count_kind(OutcomeKind::Updated);
        let new = self.count_kind(OutcomeKind::New);
        let failed = self.count_kind(OutcomeKind::Failed);
        let manual = self.count_kind(OutcomeKind::NeedsManualUpdate);
        let waiting = self.count_kind(OutcomeKind::Waiting);
        let unchanged = self.count_kind(OutcomeKind::Unchanged);
        let mut dismiss = false;

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("LAST UPDATE").small().color(theme::MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("dismiss").clicked() {
                    dismiss = true;
                }
            });
        });
        ui.add_space(4.0);

        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                let mut parts = Vec::new();
                if updated > 0 {
                    parts.push(format!("{updated} updated"));
                }
                if new > 0 {
                    parts.push(format!("{new} added"));
                }
                if unchanged > 0 {
                    parts.push(format!("{unchanged} already newest"));
                }
                if waiting > 0 {
                    parts.push(format!("{waiting} waiting for an update"));
                }
                if manual > 0 {
                    parts.push(format!("{manual} need downloading by hand"));
                }
                if failed > 0 {
                    parts.push(format!("{failed} failed"));
                }
                ui.label(
                    egui::RichText::new(if parts.is_empty() {
                        "Nothing to do.".to_string()
                    } else {
                        parts.join(", ")
                    })
                    .strong()
                    .color(if failed > 0 || manual > 0 {
                        theme::WARN
                    } else {
                        theme::TEXT
                    }),
                );
                ui.add_space(4.0);

                // Changes first — the unchanged ones are noise here.
                let mut rows: Vec<&SyncOutcome> = self.last_sync.iter().collect();
                rows.sort_by_key(|o| match o.kind {
                    OutcomeKind::Failed => 0,
                    OutcomeKind::NeedsManualUpdate => 1,
                    OutcomeKind::Waiting => 2,
                    OutcomeKind::Updated => 3,
                    OutcomeKind::New => 4,
                    OutcomeKind::Unchanged => 5,
                });

                for outcome in rows {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new(outcome.id.display())
                            .color(if outcome.kind == OutcomeKind::Unchanged {
                                theme::MUTED
                            } else {
                                theme::TEXT
                            }),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // A manual update is actionable only via its page.
                            if let Some(page) = &outcome.page {
                                ui.hyperlink_to(
                                    egui::RichText::new("get it").small(),
                                    page,
                                );
                            }
                            ui.label(
                                egui::RichText::new(outcome.label())
                                    .monospace()
                                    .small()
                                    .color(outcome.color()),
                            )
                            .on_hover_text(&outcome.detail);
                        });
                    });
                }

                if updated > 0 || new > 0 {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "These are downloaded but not in the game yet — press Activate (or \
                             More > Apply changes) to install them.",
                        )
                        .small()
                        .color(theme::MUTED),
                    );
                }
            });
        ui.add_space(12.0);

        if dismiss {
            self.last_sync.clear();
        }
    }

    fn count_kind(&self, kind: OutcomeKind) -> usize {
        self.last_sync.iter().filter(|o| o.kind == kind).count()
    }

    /// What is actually in the game folder, versus what Modifile put there.
    ///
    /// Only shown when there is something to say. A clean folder gets one quiet
    /// line rather than a panel of zeroes.
    fn folder_section(&mut self, ui: &mut egui::Ui) {
        let mut rescan = false;

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("GAME FOLDER").small().color(theme::MUTED));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Refresh")
                    .on_hover_text("Re-read the game folder and see what changed")
                    .clicked()
                {
                    rescan = true;
                }
            });
        });
        ui.add_space(4.0);

        if self.scans.is_empty() {
            ui.label(
                egui::RichText::new("Press Refresh to check what is in the game folder.")
                    .color(theme::MUTED),
            );
        }

        for scan in &self.scans {
            let name = self
                .targets
                .iter()
                .find(|t| t.target.id == scan.target)
                .map(|t| t.target.name.clone())
                .unwrap_or_else(|| scan.target.clone());

            egui::Frame::NONE
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.label(egui::RichText::new(&name).strong());
                    ui.add_space(2.0);

                    if scan.is_clean() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} file(s), all placed by Modifile and unchanged.",
                                scan.intact
                            ))
                            .color(theme::GOOD),
                        );
                        return;
                    }

                    // Pre-installed mods, split by whether they actually block
                    // anything — that is the difference between "worth knowing"
                    // and "you must act".
                    let blocking: Vec<_> = scan.blocking().collect();
                    if !blocking.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} file(s) installed by something else are sitting where this \
                                 profile's mods go:",
                                blocking.len()
                            ))
                            .color(theme::BAD),
                        );
                        for entry in blocking.iter().take(8) {
                            ui.label(
                                egui::RichText::new(format!("   {}", display_path(&entry.rel)))
                                    .small()
                                    .monospace()
                                    .color(theme::MUTED),
                            );
                        }
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Activate will leave these alone and skip the mod, so the mod \
                                 will not be installed. Use \"Replace them\" to let Modifile \
                                 take them over — the old file is deleted.",
                            )
                            .small()
                            .color(theme::MUTED),
                        );
                        ui.add_space(6.0);
                        if ui
                            .button("Replace them and activate")
                            .on_hover_text("Deletes those files and installs this profile's versions")
                            .clicked()
                        {
                            self.pending_force_deploy = true;
                        }
                        ui.add_space(6.0);
                    }

                    let harmless = scan.foreign.len() - blocking.len();
                    if harmless > 0 {
                        ui.label(
                            egui::RichText::new(format!(
                                "{harmless} other file(s) Modifile did not install. They are \
                                 left completely alone.",
                            ))
                            .color(theme::WARN),
                        );
                    }

                    if !scan.modified.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} file(s) we installed have changed since — edited by hand, or \
                                 overwritten by a game update. Activate will not remove or \
                                 replace them.",
                                scan.modified.len()
                            ))
                            .color(theme::WARN),
                        );
                        for entry in scan.modified.iter().take(6) {
                            ui.label(
                                egui::RichText::new(format!("   {}", display_path(&entry.rel)))
                                    .small()
                                    .monospace()
                                    .color(theme::MUTED),
                            );
                        }
                    }

                    if !scan.missing.is_empty() {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} installed file(s) have gone missing. Activate puts them back.",
                                scan.missing.len()
                            ))
                            .color(theme::WARN),
                        );
                    }
                });
            ui.add_space(4.0);
        }

        if rescan {
            self.do_scan();
        }
        if std::mem::take(&mut self.pending_force_deploy) {
            self.do_deploy(true);
        }
    }

    /// Re-read every target's game folder.
    fn do_scan(&mut self) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let lock = self.lock.clone();
        let mut scans = Vec::new();
        let mut messages = Vec::new();

        for row in &self.targets {
            let Some(root) = &row.root else { continue };
            match engine.scan(pack, &profile, &lock, &row.target, root) {
                Ok(scan) => {
                    messages.push(format!(
                        "{}: {} ours, {} not ours, {} changed, {} missing",
                        row.target.name,
                        scan.intact,
                        scan.foreign.len(),
                        scan.modified.len(),
                        scan.missing.len()
                    ));
                    scans.push(scan);
                }
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        self.scans = scans;
        for message in messages {
            self.log_line(message);
        }
    }

    /// Config files belong to the profile, so this is where you reset them to
    /// the mods' defaults or pull them in from somewhere else.
    fn configs_section(&mut self, ui: &mut egui::Ui) {
        let saved = self.config_files.len();
        let mut reset = false;
        let mut import: Option<modifile_core::engine::ConfigSource> = None;
        let others: Vec<String> = self
            .profiles
            .iter()
            .filter(|p| Some(p.name.as_str()) != self.selected.as_deref())
            .map(|p| p.name.clone())
            .collect();

        ui.label(egui::RichText::new("CONFIGS").small().color(theme::MUTED));
        ui.add_space(4.0);
        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let live = self.live_config_files;
                    ui.label(
                        egui::RichText::new(match (saved, live) {
                            (0, 0) => "No settings files yet. Mods create them the first time \
                                       they run."
                                .to_string(),
                            (0, live) => format!(
                                "{live} settings file(s) are in the game folder. Activating this \
                                 profile adopts them, and from then on they belong to it alone."
                            ),
                            (saved, _) => {
                                format!("{saved} settings file(s) kept for this profile only")
                            }
                        })
                        .color(if saved == 0 && live > 0 {
                            theme::WARN
                        } else {
                            theme::MUTED
                        }),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.menu_button("Import from…", |ui| {
                            if ui
                                .button("This game's current folder")
                                .on_hover_text(
                                    "Adopt settings already in the game folder, e.g. from \
                                     modding it by hand",
                                )
                                .clicked()
                            {
                                import = Some(modifile_core::engine::ConfigSource::Game);
                                ui.close();
                            }
                            if ui
                                .button("A folder on disk…")
                                .on_hover_text("Restore settings from a backup")
                                .clicked()
                            {
                                if let Some(dir) = rfd::FileDialog::new()
                                    .set_title("Choose a folder of settings")
                                    .pick_folder()
                                {
                                    import = Some(
                                        modifile_core::engine::ConfigSource::Folder(dir),
                                    );
                                }
                                ui.close();
                            }
                            ui.separator();
                            if others.is_empty() {
                                ui.label(
                                    egui::RichText::new("no other profiles").color(theme::MUTED),
                                );
                            }
                            for other in &others {
                                if ui.button(other).clicked() {
                                    import = Some(
                                        modifile_core::engine::ConfigSource::Profile(
                                            other.clone(),
                                        ),
                                    );
                                    ui.close();
                                }
                            }
                        });
                        if ui
                            .add_enabled(saved > 0, egui::Button::new("Reset to defaults"))
                            .on_hover_text(
                                "Discard this profile's settings. The mods' originals are kept \
                                 in the download store, so nothing is lost permanently.",
                            )
                            .clicked()
                        {
                            reset = true;
                        }
                    });
                });
            });

        if reset {
            self.do_reset_configs();
        }
        if let Some(source) = import {
            self.do_import_configs(source);
        }
    }

    fn do_reset_configs(&mut self) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let mut messages = Vec::new();
        for row in &self.targets {
            match engine.reset_configs(pack, &profile.name, &row.target, row.root.as_deref()) {
                Ok(count) if count > 0 => messages.push(format!(
                    "{}: discarded {count} settings file(s) — press Activate to restore defaults",
                    row.target.name
                )),
                Ok(_) => {}
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        for message in messages {
            self.log_line(message);
        }
        self.refresh();
    }

    fn do_import_configs(&mut self, source: modifile_core::engine::ConfigSource) {
        let (Some(profile), Some(engine)) = (self.profile.clone(), self.engine()) else {
            return;
        };
        let Ok(pack) = engine.pack_for(&profile) else {
            return;
        };
        let mut messages = Vec::new();
        for row in &self.targets {
            match engine.import_configs(
                pack,
                &profile.name,
                &row.target,
                &source,
                row.root.as_deref(),
            ) {
                Ok(count) if count > 0 => {
                    messages.push(format!("{}: imported {count} config file(s)", row.target.name))
                }
                Ok(_) => {}
                Err(e) => messages.push(format!("{}: {e}", row.target.name)),
            }
        }
        if messages.is_empty() {
            messages.push("Nothing to import.".to_string());
        }
        for message in messages {
            self.log_line(message);
        }
        self.refresh();
    }

    /// What a first-time user sees. It has to answer "what do I do now".
    fn welcome(&mut self, ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.heading("Welcome to Modifile");
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Mods from GitHub, linked into your game. Nothing stays running while you play.",
                )
                .color(theme::MUTED),
            );
            ui.add_space(22.0);
        });

        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(18, 16))
            .show(ui, |ui| {
                for (step, text) in [
                    ("1", "Make a profile and pick which game it is for."),
                    ("2", "Add mods by pasting a GitHub repo, like WeakAuras/WeakAuras2."),
                    ("3", "Press Check for updates to download them, then Activate."),
                    ("4", "Launch the game however you normally do."),
                ] {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(step).strong().color(theme::ACCENT));
                        ui.label(text);
                    });
                    ui.add_space(6.0);
                }
            });

        ui.add_space(18.0);
        ui.vertical_centered(|ui| {
            if ui
                .add_sized([230.0, 34.0], egui::Button::new("Create your first profile").fill(theme::ACCENT_DIM))
                .clicked()
            {
                self.open_new_profile();
            }
            ui.add_space(8.0);
            if ui.link("See which games were found on this PC").clicked() {
                self.view = View::Games;
            }
        });
    }

    fn targets_section(&mut self, ui: &mut egui::Ui, game: &str) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("GAME FOLDERS").small().color(theme::MUTED));
            ui.label(
                egui::RichText::new("where mods get installed")
                    .small()
                    .color(theme::MUTED),
            );
        });
        ui.add_space(4.0);

        let mut choose: Option<Target> = None;
        let mut forget: Option<String> = None;

        for row in &self.targets {
            egui::Frame::NONE
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 8))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (badge, color) = match row.target.kind {
                            modifile_core::TargetKind::Server => ("SERVER", theme::ACCENT),
                            modifile_core::TargetKind::Client => ("CLIENT", theme::MUTED),
                        };
                        ui.label(egui::RichText::new(badge).small().color(color));
                        ui.label(egui::RichText::new(&row.target.name).strong());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if row.root.is_some() {
                                if ui.small_button("Change…").clicked() {
                                    choose = Some(row.target.clone());
                                }
                                if row.remembered && ui.small_button("Forget").clicked() {
                                    forget = Some(row.target.id.clone());
                                }
                            } else if ui.button("Choose folder…").clicked() {
                                choose = Some(row.target.clone());
                            }

                            match &row.deployed {
                                Some((name, count, mode)) => {
                                    let response = ui.label(
                                        egui::RichText::new(format!(
                                            "{count} file(s) from `{name}` ({})",
                                            mode.map(LinkMode::label).unwrap_or("?")
                                        ))
                                        .color(theme::GOOD),
                                    );
                                    // "copy" surprises people who were told
                                    // profiles cost no disk, so say why.
                                    if *mode == Some(LinkMode::Copy) {
                                        response.on_hover_text(
                                            "Files were copied rather than hard-linked, because \
                                             the game is on a different drive from Modifile's \
                                             download store. It works exactly the same, it just \
                                             uses disk space per profile.\n\nTo get hard links \
                                             back, set MODIFILE_HOME to a folder on the same \
                                             drive as the game.",
                                        );
                                    } else if let Some(mode) = mode {
                                        response.on_hover_text(mode.explain());
                                    }
                                }
                                None if row.root.is_some() => {
                                    ui.label(
                                        egui::RichText::new("nothing installed")
                                            .color(theme::MUTED),
                                    );
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new("folder not found")
                                            .color(theme::WARN),
                                    );
                                }
                            }
                        });
                    });
                    if let Some(root) = &row.root {
                        ui.label(
                            egui::RichText::new(display_path(root))
                                .small()
                                .color(theme::MUTED),
                        );
                    }
                    if let Some(found) = &row.running {
                        ui.label(
                            egui::RichText::new(format!(
                                "running now ({found}) — close the game to change mods"
                            ))
                            .small()
                            .color(theme::WARN),
                        );
                    }
                    if row.remote {
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut self.assume_stopped, "");
                            ui.label(
                                egui::RichText::new(
                                    "on another machine — tick to confirm the server is stopped",
                                )
                                .small()
                                .color(theme::WARN),
                            );
                        });
                    }
                });
            ui.add_space(4.0);
        }

        if let Some(target) = choose {
            self.open_root_dialog(game, &target);
        }
        if let Some(target_id) = forget {
            self.forget_root(game, &target_id);
        }
    }

    fn mods_section(&mut self, ui: &mut egui::Ui) {
        // Two ways in: search by name, or paste a link. Previously only the
        // second existed, which meant you had to already know where a mod was.
        ui.label(egui::RichText::new("FIND MODS").small().color(theme::MUTED));
        ui.add_space(4.0);
        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let field = ui.add(
                        egui::TextEdit::singleline(&mut self.search_input)
                            .desired_width(280.0)
                            .hint_text("search by name, e.g. auction"),
                    );
                    let go = ui.add_enabled(!self.searching, egui::Button::new("Search"));
                    if go.clicked()
                        || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        let ctx = ui.ctx().clone();
                        self.do_search(&ctx);
                    }
                    if self.searching {
                        ui.add(egui::Spinner::new());
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button("From file…")
                            .on_hover_text(
                                "For a mod you downloaded yourself — a CurseForge project \
                                 whose author blocked third-party downloads, a private beta, \
                                 your own build. Managed like any other afterwards.",
                            )
                            .clicked()
                        {
                            self.do_add_file();
                        }
                        let add = ui.button("Add");
                        let paste = ui.add(
                            egui::TextEdit::singleline(&mut self.add_input)
                                .desired_width(220.0)
                                .hint_text("…or paste any mod link"),
                        );
                        if add.clicked()
                            || (paste.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                        {
                            self.do_add();
                        }
                    });
                });

                if !self.search_results.is_empty() {
                    ui.add_space(8.0);
                    let mut chosen: Option<ModId> = None;
                    let mut dismiss = false;

                    for hit in &self.search_results {
                        let already = self.rows.iter().any(|r| r.id == hit.id);
                        let installable = hit.installable != Some(false);

                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(
                                    !already && installable,
                                    egui::Button::new(if already { "added" } else { "Add" }),
                                )
                                .on_disabled_hover_text(if already {
                                    "Already in this profile"
                                } else {
                                    "This publishes no release, so there is nothing to install"
                                })
                                .clicked()
                            {
                                chosen = Some(hit.id.clone());
                            }
                            ui.label(
                                egui::RichText::new(hit.id.kind.label())
                                    .small()
                                    .color(theme::MUTED),
                            );
                            ui.label(hit.label()).on_hover_text(hit.id.to_string());
                            if !installable {
                                ui.label(
                                    egui::RichText::new("not downloadable")
                                        .small()
                                        .color(theme::WARN),
                                )
                                .on_hover_text(
                                    "The author has switched off third-party downloads, so \
                                     no tool but CurseForge's own can fetch it.",
                                );
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if let Some(src) = &hit.source_url {
                                        ui.hyperlink_to(
                                            egui::RichText::new("source").small(),
                                            src,
                                        );
                                    }
                                    if let Some(license) = &hit.license {
                                        ui.label(
                                            egui::RichText::new(license)
                                                .small()
                                                .color(theme::MUTED),
                                        );
                                    }
                                    ui.label(
                                        egui::RichText::new(hit.popularity())
                                            .small()
                                            .color(theme::MUTED),
                                    );
                                },
                            );
                        });
                        if !hit.description.is_empty() {
                            let short: String = hit.description.chars().take(110).collect();
                            ui.label(
                                egui::RichText::new(short).small().color(theme::MUTED),
                            );
                        }
                        ui.add_space(4.0);
                    }

                    if ui.small_button("clear results").clicked() {
                        dismiss = true;
                    }
                    if let Some(id) = chosen {
                        self.add_id(&id);
                    }
                    if dismiss {
                        self.search_results.clear();
                    }
                }
            });

        ui.add_space(12.0);
        ui.label(egui::RichText::new("MODS").small().color(theme::MUTED));
        ui.add_space(6.0);

        if self.rows.is_empty() {
            egui::Frame::NONE
                .fill(theme::CARD)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(14, 12))
                .show(ui, |ui| {
                    ui.label("No mods yet.");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Paste a GitHub repository above. Any mod that publishes its \
                             releases on GitHub works — owner/repo, or the full URL.",
                        )
                        .color(theme::MUTED),
                    );
                });
            return;
        }

        let mut toggle: Option<ModId> = None;
        let mut remove: Option<ModId> = None;
        let mut set_side: Option<(ModId, Side)> = None;
        let mut set_pin: Option<(ModId, Option<String>)> = None;
        let mut set_prerelease: Option<(ModId, bool)> = None;
        // Only games that actually have a dedicated server get the control.
        let has_server = !self.server_ids.is_empty();
        let client_ids = self.client_ids.clone();
        let server_ids = self.server_ids.clone();

        // No scroll area of its own: the whole page scrolls, and an unbounded
        // nested one would claim infinite height inside it.
        {
            let ui = &mut *ui;
            {
                for row in &self.rows {
                    egui::Frame::NONE
                        .fill(theme::CARD)
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(10, 7))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let mut enabled = row.enabled;
                                if ui
                                    .checkbox(&mut enabled, "")
                                    .on_hover_text("Include this mod when the profile is activated")
                                    .changed()
                                {
                                    toggle = Some(row.id.clone());
                                }

                                // Where it comes from, so a Modrinth and a
                                // GitHub mod are never confused for each other.
                                ui.label(
                                    egui::RichText::new(row.id.kind.label())
                                        .small()
                                        .color(theme::MUTED),
                                );

                                let label = row.id.display();
                                let text = if row.waiting {
                                    egui::RichText::new(label.clone()).color(theme::MUTED)
                                } else if row.enabled {
                                    egui::RichText::new(label).color(theme::TEXT)
                                } else {
                                    egui::RichText::new(label)
                                        .color(theme::MUTED)
                                        .strikethrough()
                                };
                                ui.label(text).on_hover_text(row.id.web_url());

                                // Pin / prerelease, which previously only
                                // existed as flags on `modifile add`.
                                ui.menu_button(
                                    egui::RichText::new(if row.pinned {
                                        format!("pinned {}", row.version)
                                    } else {
                                        "latest".to_string()
                                    })
                                    .small()
                                    .color(if row.pinned { theme::WARN } else { theme::MUTED }),
                                    |ui| {
                                        if ui
                                            .selectable_label(!row.pinned, "Always take the newest")
                                            .clicked()
                                        {
                                            set_pin = Some((row.id.clone(), None));
                                            ui.close();
                                        }
                                        let can_pin = row.version != "—";
                                        if ui
                                            .add_enabled(
                                                can_pin,
                                                egui::Button::new(format!(
                                                    "Hold at {}",
                                                    row.version
                                                ))
                                                .frame(false),
                                            )
                                            .clicked()
                                        {
                                            set_pin =
                                                Some((row.id.clone(), Some(row.version.clone())));
                                            ui.close();
                                        }
                                        ui.separator();
                                        let mut pre = row.prerelease;
                                        if ui.checkbox(&mut pre, "Include prereleases").changed() {
                                            set_prerelease = Some((row.id.clone(), pre));
                                            ui.close();
                                        }
                                    },
                                )
                                .response
                                .on_hover_text(if row.pinned {
                                    "Held at this version; updates will not move it"
                                } else {
                                    "Takes the newest release on every update"
                                });

                                // Which side of the game this mod installs on.
                                if has_server {
                                    let side = Side::of(&row.targets, &client_ids, &server_ids);
                                    ui.menu_button(
                                        egui::RichText::new(side.label()).small().color(side.color()),
                                        |ui| {
                                            for option in
                                                [Side::Both, Side::ClientOnly, Side::ServerOnly]
                                            {
                                                if ui
                                                    .selectable_label(
                                                        side == option,
                                                        option.menu_label(),
                                                    )
                                                    .clicked()
                                                {
                                                    set_side = Some((row.id.clone(), option));
                                                    ui.close();
                                                }
                                            }
                                        },
                                    )
                                    .response
                                    .on_hover_text(side.explain());
                                }

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .small_button("remove")
                                            .on_hover_text("Take this mod out of the profile")
                                            .clicked()
                                        {
                                            remove = Some(row.id.clone());
                                        }
                                        if let Some(level) = row.trust {
                                            let mut tip = level.explain().to_string();
                                            if !row.note.is_empty() {
                                                tip.push_str("\n\n");
                                                tip.push_str(&row.note);
                                            }
                                            ui.label(
                                                egui::RichText::new(level.short())
                                                    .small()
                                                    .color(theme::trust_color(level)),
                                            )
                                            .on_hover_text(tip);
                                        }
                                        // Downloaded / installed state, visible
                                        // whether or not the profile is active.
                                        let (state, colour, tip) = if row.waiting {
                                            (
                                                "no build yet",
                                                theme::MUTED,
                                                "This mod has no build for your game version \
                                                 or loader yet. It stays in the profile and \
                                                 is skipped when you activate; it installs \
                                                 itself once a compatible build appears.",
                                            )
                                        } else if row.size == 0 {
                                            (
                                                "not downloaded",
                                                theme::WARN,
                                                "Press Check for updates to fetch it",
                                            )
                                        } else if row.installed {
                                            (
                                                "in game",
                                                theme::GOOD,
                                                "Downloaded, and its files are in the game folder",
                                            )
                                        } else {
                                            (
                                                "downloaded",
                                                theme::MUTED,
                                                "Downloaded and kept, but not in the game folder",
                                            )
                                        };
                                        ui.label(
                                            egui::RichText::new(state).small().color(colour),
                                        )
                                        .on_hover_text(tip);

                                        if row.size > 0 {
                                            ui.label(
                                                egui::RichText::new(format_bytes(row.size))
                                                    .small()
                                                    .monospace()
                                                    .color(theme::MUTED),
                                            );
                                        }
                                        ui.label(
                                            egui::RichText::new(&row.version)
                                                .monospace()
                                                .color(theme::MUTED),
                                        );
                                    },
                                );
                            });
                        });
                    ui.add_space(3.0);
                }
            }
        }

        if let Some(id) = toggle {
            self.do_toggle(&id);
        }
        if let Some(id) = remove {
            self.do_remove(&id);
        }
        if let Some((id, side)) = set_side {
            self.do_set_side(&id, side);
        }
        if let Some((id, pin)) = set_pin {
            self.edit_entry(&id, |entry| entry.pin = pin.clone());
            self.log_line(match pin {
                Some(v) => format!("{id} held at {v}."),
                None => format!("{id} will take the newest release."),
            });
        }
        if let Some((id, pre)) = set_prerelease {
            self.edit_entry(&id, |entry| entry.prerelease = pre);
            self.log_line(format!(
                "{id} {} prereleases.",
                if pre { "now includes" } else { "no longer includes" }
            ));
        }
    }

    /// Change one mod's settings in the profile on disk.
    fn edit_entry(&mut self, id: &ModId, change: impl Fn(&mut ModEntry)) {
        let Some(name) = self.selected.clone() else {
            return;
        };
        let path = self.paths.profile_file(&name);
        if let Ok(mut profile) = Profile::load(&path) {
            if let Some(entry) = profile.mods.iter_mut().find(|m| &m.id == id) {
                change(entry);
            }
            let _ = profile.save(&path);
        }
        self.refresh();
    }

    fn games_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Games & folders");
        ui.label(
            egui::RichText::new(
                "Modifile looks for these on startup. Point it at anything it missed.",
            )
            .color(theme::MUTED),
        );
        ui.add_space(12.0);

        struct Entry {
            game: String,
            name: String,
            description: String,
            targets: Vec<(Target, Option<PathBuf>, bool)>,
        }

        let entries: Vec<Entry> = self
            .engine()
            .map(|engine| {
                engine
                    .packs
                    .iter()
                    .map(|pack| Entry {
                        game: pack.id().to_string(),
                        name: pack.pack.game.name.clone(),
                        description: pack.pack.game.description.clone(),
                        targets: pack
                            .pack
                            .targets
                            .iter()
                            .map(|t| {
                                let remembered = engine.roots.get(pack.id(), &t.id).cloned();
                                let found = remembered
                                    .clone()
                                    .filter(|p| p.is_dir())
                                    .or_else(|| pack.detect(t).into_iter().next());
                                (t.clone(), found, remembered.is_some())
                            })
                            .collect(),
                    })
                    .collect()
            })
            .unwrap_or_default();

        let mut choose: Option<(String, Target)> = None;
        let mut forget: Option<(String, String)> = None;

        // The page already scrolls; no nested scroll area here.
        {
            let ui = &mut *ui;
            {
                for entry in &entries {
                    egui::Frame::NONE
                        .fill(theme::CARD)
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(&entry.name).strong());
                            if !entry.description.is_empty() {
                                let first = entry
                                    .description
                                    .lines()
                                    .next()
                                    .unwrap_or_default()
                                    .to_string();
                                ui.label(
                                    egui::RichText::new(first).small().color(theme::MUTED),
                                );
                            }
                            ui.add_space(6.0);

                            for (target, found, remembered) in &entry.targets {
                                ui.horizontal(|ui| {
                                    let badge = match target.kind {
                                        modifile_core::TargetKind::Server => "SERVER",
                                        modifile_core::TargetKind::Client => "CLIENT",
                                    };
                                    ui.label(
                                        egui::RichText::new(badge).small().color(theme::MUTED),
                                    );
                                    ui.label(&target.name);

                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if ui.small_button(if found.is_some() {
                                                "Change…"
                                            } else {
                                                "Choose folder…"
                                            })
                                            .clicked()
                                            {
                                                choose =
                                                    Some((entry.game.clone(), target.clone()));
                                            }
                                            if *remembered && ui.small_button("Forget").clicked() {
                                                forget = Some((
                                                    entry.game.clone(),
                                                    target.id.clone(),
                                                ));
                                            }
                                            match found {
                                                Some(path) => ui.label(
                                                    egui::RichText::new(display_path(path))
                                                        .small()
                                                        .color(theme::GOOD),
                                                ),
                                                None => ui.label(
                                                    egui::RichText::new("not found")
                                                        .small()
                                                        .color(theme::MUTED),
                                                ),
                                            };
                                        },
                                    );
                                });
                            }
                        });
                    ui.add_space(6.0);
                }

                // Packs held back are silent poison: every fix in them fails to
                // arrive and nothing says why. Surface it where games live.
                let stale: Vec<String> = modifile_core::pack_status(&self.paths)
                    .into_iter()
                    .filter(|(_, s)| *s != modifile_core::PackState::Current)
                    .map(|(n, s)| {
                        format!(
                            "{n} ({})",
                            match s {
                                modifile_core::PackState::Missing => "not installed",
                                modifile_core::PackState::Outdated => "out of date",
                                _ => "out of date, kept in case you edited it",
                            }
                        )
                    })
                    .collect();

                if !stale.is_empty() {
                    ui.add_space(8.0);
                    egui::Frame::NONE
                        .fill(theme::CARD)
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(12, 10))
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("Game packs are out of date")
                                    .strong()
                                    .color(theme::WARN),
                            );
                            ui.add_space(4.0);
                            for line in &stale {
                                ui.label(
                                    egui::RichText::new(format!("   {line}"))
                                        .small()
                                        .color(theme::MUTED),
                                );
                            }
                            ui.add_space(6.0);
                            ui.label(
                                egui::RichText::new(
                                    "These are not the packs this build ships, so fixes in them \
                                     are not active — new games, new install rules, download \
                                     settings. Your current copies are saved as .bak files.",
                                )
                                .small()
                                .color(theme::MUTED),
                            );
                            ui.add_space(8.0);
                            if ui.button("Update game packs").clicked() {
                                self.refresh_packs = true;
                            }
                        });
                }

                ui.add_space(8.0);
                egui::Frame::NONE
                    .fill(theme::CARD)
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Add another game").strong());
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(
                                "Support for a game is a single .toml file — no new version of \
                                 Modifile needed. Drop one in the packs folder and restart.",
                            )
                            .color(theme::MUTED),
                        );
                        ui.add_space(8.0);
                        if ui.button("Open packs folder").clicked() {
                            reveal(&self.paths.packs);
                        }
                    });

                if let Some(engine) = self.engine() {
                    for (path, error) in &engine.pack_errors {
                        ui.add_space(6.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "ignored {}: {error}",
                                display_path(path)
                            ))
                            .color(theme::BAD),
                        );
                    }
                }
            }
        }

        if std::mem::take(&mut self.refresh_packs) {
            self.do_refresh_packs();
        }
        if let Some((game, target)) = choose {
            self.open_root_dialog(&game, &target);
        }
        if let Some((game, target_id)) = forget {
            self.forget_root(&game, &target_id);
        }
    }

    fn settings_view(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("Settings");
        ui.add_space(12.0);

        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("GitHub token").strong());
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Optional, and free. Without one GitHub allows 60 update checks per \
                         hour for your whole machine; with one, 5000. It is only used for \
                         read-only requests.",
                    )
                    .color(theme::MUTED),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.token_input)
                            .desired_width(340.0)
                            .password(true)
                            .hint_text("ghp_…"),
                    );
                    if ui.button("Save").clicked() {
                        self.save_token(false);
                    }
                    if ui
                        .button("Clear")
                        .on_hover_text("Remove the saved token")
                        .clicked()
                    {
                        self.save_token(true);
                    }
                });
                ui.add_space(6.0);
                ui.hyperlink_to(
                    "Create one on GitHub (no scopes needed)",
                    "https://github.com/settings/tokens/new",
                );
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("CurseForge API key").strong());
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Needed only for CurseForge mods. The key is issued to you personally \
                         after a review, and the terms forbid sharing one — so Modifile cannot \
                         ship a key and has to use yours. Modrinth and GitHub need nothing.",
                    )
                    .color(theme::MUTED),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.curseforge_input)
                            .desired_width(340.0)
                            .password(true)
                            .hint_text("$2a$10$…"),
                    );
                    if ui.button("Save").clicked() {
                        self.save_curseforge_key(false);
                    }
                    if ui
                        .button("Clear")
                        .on_hover_text("Remove the saved key")
                        .clicked()
                    {
                        self.save_curseforge_key(true);
                    }
                });
                ui.add_space(4.0);
                let saved = self
                    .engine()
                    .map(|e| e.has_curseforge_key())
                    .unwrap_or(false);
                ui.label(
                    egui::RichText::new(if saved {
                        "Key saved — CurseForge mods and search are available."
                    } else {
                        "No key — CurseForge mods cannot be resolved."
                    })
                    .small()
                    .color(if saved { theme::GOOD } else { theme::MUTED }),
                );
                ui.add_space(6.0);
                ui.hyperlink_to("Request a key", "https://console.curseforge.com/");

                ui.add_space(12.0);
                ui.separator();
                ui.add_space(8.0);

                // The author-blocked download setting. Off by default, and the
                // cost of turning it on is stated rather than buried.
                let mut direct = self.curseforge_direct;
                if ui
                    .checkbox(
                        &mut direct,
                        "Download mods whose authors blocked third-party apps",
                    )
                    .changed()
                {
                    self.set_curseforge_direct(direct);
                }
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Some authors switch off third-party downloads, and CurseForge's API \
                         then hands out no file. The bytes are the same ones your browser \
                         would get and you are entitled to them — but the API withholds the \
                         link deliberately, and your key's terms cover this. If it is \
                         noticed, your key is what gets revoked, which also costs you search \
                         and update checks.",
                    )
                    .small()
                    .color(if direct { theme::WARN } else { theme::MUTED }),
                );
                if !direct {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Left off, blocked mods are reported with a link, and you can add \
                             the downloaded file with \"From file…\".",
                        )
                        .small()
                        .color(theme::MUTED),
                    );
                }
            });

        ui.add_space(10.0);
        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.label(egui::RichText::new("What you will install").strong());
                ui.add_space(4.0);
                let mut allow = self.allow_no_source;
                if ui
                    .checkbox(
                        &mut allow,
                        "Allow mods with no public source code at all",
                    )
                    .on_hover_text(
                        "Off by default. A compiled file with no published code and no licence \
                         cannot be checked by anyone — common on CurseForge.",
                    )
                    .changed()
                {
                    self.allow_no_source = allow;
                    let marker = self.paths.allow_no_source_file();
                    if allow {
                        let _ = modifile_core::paths::write_atomic(&marker, b"on");
                    } else {
                        std::fs::remove_file(&marker).ok();
                    }
                    self.log_line(if allow {
                        "Mods with no public source will now be installed."
                    } else {
                        "Mods with no public source will be refused."
                    });
                }
            });

        ui.add_space(10.0);
        self.storage_panel(ui);
    }

    /// Everything downloaded, who still wants it, and a way to delete it.
    ///
    /// A download is *not* waste because its profile is switched off — that is
    /// what profiles are for. Only "nothing references this at all" is waste,
    /// and the two are shown differently.
    fn storage_panel(&mut self, ui: &mut egui::Ui) {
        let mut refresh = false;
        let mut forget: Option<String> = None;
        let mut clean = false;

        egui::Frame::NONE
            .fill(theme::CARD)
            .corner_radius(8.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Downloads").strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Refresh").clicked() {
                            refresh = true;
                        }
                        if ui.small_button("Open data folder").clicked() {
                            reveal(&self.paths.home);
                        }
                    });
                });
                ui.add_space(4.0);

                let Some(report) = &self.storage else {
                    ui.label(
                        egui::RichText::new("Press Refresh to see what is stored.")
                            .color(theme::MUTED),
                    );
                    return;
                };

                ui.label(
                    egui::RichText::new(format!(
                        "{} download(s), {} total. Profiles share them, so the same mod in \
                         five profiles is stored once.",
                        report.items.len(),
                        format_bytes(report.total_bytes())
                    ))
                    .color(theme::MUTED),
                );
                ui.add_space(8.0);

                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for item in &report.items {
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(format_bytes(item.size))
                                        .monospace()
                                        .color(theme::MUTED),
                                );
                                ui.label(&item.name());

                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if item.is_orphan() {
                                            if ui
                                                .small_button("Delete")
                                                .on_hover_text("Nothing uses this")
                                                .clicked()
                                            {
                                                forget = Some(item.sha256.clone());
                                            }
                                            ui.label(
                                                egui::RichText::new("unused")
                                                    .small()
                                                    .color(theme::WARN),
                                            );
                                        } else {
                                            let who: Vec<String> = item
                                                .used_by
                                                .iter()
                                                .map(|u| {
                                                    if u.active {
                                                        format!("{} (active)", u.profile)
                                                    } else {
                                                        u.profile.clone()
                                                    }
                                                })
                                                .collect();
                                            ui.label(
                                                egui::RichText::new(who.join(", "))
                                                    .small()
                                                    .color(if item.only_inactive() {
                                                        theme::MUTED
                                                    } else {
                                                        theme::GOOD
                                                    }),
                                            );
                                        }
                                    },
                                );
                            });
                        }
                    });

                let orphans = report.orphans().count();
                ui.add_space(8.0);
                if orphans == 0 {
                    ui.label(
                        egui::RichText::new("Nothing to clean up — every download is wanted.")
                            .small()
                            .color(theme::MUTED),
                    );
                } else if ui
                    .button(format!(
                        "Delete {orphans} unused download(s) ({})",
                        format_bytes(report.orphan_bytes())
                    ))
                    .clicked()
                {
                    clean = true;
                }
            });

        if refresh {
            self.refresh_storage();
        }
        if let Some(sha) = forget {
            if let Some(engine) = self.engine() {
                match engine.forget(&sha) {
                    Ok(freed) => {
                        self.log_line(format!("Deleted an unused download, freeing {}", format_bytes(freed)))
                    }
                    Err(e) => self.log_line(e.to_string()),
                }
            }
            self.refresh_storage();
        }
        if clean {
            self.run_gc();
            self.refresh_storage();
        }
    }

    fn refresh_storage(&mut self) {
        match self.engine().map(|e| e.storage()) {
            Some(Ok(report)) => self.storage = Some(report),
            Some(Err(e)) => self.log_line(e.to_string()),
            None => {}
        }
    }

    fn rename_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_rename;
        let mut commit = false;
        egui::Window::new("Rename profile")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.rename_input)
                        .desired_width(280.0),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Mods, downloads and saved settings all come with it.")
                        .small()
                        .color(theme::MUTED),
                );
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button("Rename").clicked()
                        || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        commit = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_rename = false;
                    }
                });
                ui.add_space(2.0);
            });
        if commit {
            self.commit_rename();
        } else if !open {
            self.show_rename = false;
        }
    }

    fn new_profile_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_new_profile;
        egui::Window::new("New profile")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Which game?");
                let packs: Vec<(String, String)> = self
                    .engine()
                    .map(|e| {
                        e.packs
                            .iter()
                            .map(|p| (p.id().to_string(), p.pack.game.name.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let current = packs
                    .iter()
                    .find(|(id, _)| id == &self.new_profile_game)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_else(|| "select a game".to_string());

                egui::ComboBox::from_id_salt("game")
                    .selected_text(current)
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for (id, name) in &packs {
                            ui.selectable_value(&mut self.new_profile_game, id.clone(), name);
                        }
                    });

                ui.add_space(10.0);
                ui.label("Name it");
                let field = ui.add(
                    egui::TextEdit::singleline(&mut self.new_profile_name)
                        .desired_width(280.0)
                        .hint_text("raiding, modded-server, vanilla-plus…"),
                );
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(
                        "Profiles swap in seconds and share downloads, so make as many as you like.",
                    )
                    .small()
                    .color(theme::MUTED),
                );

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Create").clicked()
                        || (field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        self.create_profile();
                    }
                    if ui.button("Cancel").clicked() {
                        self.show_new_profile = false;
                    }
                });
                ui.add_space(2.0);
            });

        if !open {
            self.show_new_profile = false;
        }
    }
}
