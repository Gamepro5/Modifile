//! The bundled packs are data, so they need tests like any other data. These
//! pin the behaviour that is easy to get silently wrong: picking the right
//! asset for a game flavor, and routing archive contents to the right folder.

use modifile_core::pack::{CompiledPack, Pack};
use modifile_core::BUNDLED_PACKS;

fn pack(name: &str) -> CompiledPack {
    let (_, body) = BUNDLED_PACKS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no bundled pack named {name}"));
    let parsed: Pack = toml::from_str(body).expect("pack parses");
    CompiledPack::new(parsed).expect("pack compiles")
}

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn every_bundled_pack_loads() {
    for (name, _) in BUNDLED_PACKS {
        let p = pack(name);
        assert!(!p.pack.targets.is_empty(), "{name} has no targets");
        assert!(!p.pack.install.is_empty(), "{name} has no install rules");
    }
}

// --- World of Warcraft ----------------------------------------------------

#[test]
fn wow_retail_refuses_classic_builds() {
    let wow = pack("wow.toml");
    let retail = wow.target("client").unwrap();
    let assets = names(&["WeakAuras-5.20.1.zip", "WeakAuras-5.20.1-classic.zip"]);

    let chosen = wow.select_asset(&assets, retail).expect("retail asset");
    assert_eq!(assets[chosen], "WeakAuras-5.20.1.zip");
}

#[test]
fn wow_classic_era_prefers_its_own_flavor() {
    let era = pack("wow-classic-era.toml");
    let classic = era.target("client").unwrap();
    let assets = names(&["WeakAuras-5.20.1.zip", "WeakAuras-5.20.1-classic.zip"]);

    let chosen = era.select_asset(&assets, classic).expect("classic asset");
    assert_eq!(assets[chosen], "WeakAuras-5.20.1-classic.zip");
}

#[test]
fn wow_rejects_nolib_and_source_archives() {
    let wow = pack("wow.toml");
    let retail = wow.target("client").unwrap();
    let assets = names(&["Details-1.0-nolib.zip", "Details-1.0.zip"]);

    let chosen = wow.select_asset(&assets, retail).unwrap();
    assert_eq!(assets[chosen], "Details-1.0.zip");

    // Nothing usable at all is a miss, not a wrong pick.
    assert!(wow
        .select_asset(&names(&["Details-1.0-nolib.zip"]), retail)
        .is_none());
}

#[test]
fn wow_addon_folders_land_under_interface_addons() {
    let wow = pack("wow.toml");
    let retail = wow.target("client").unwrap();

    let rule = wow
        .rule_for("WeakAuras/Core.lua", "client")
        .expect("addon file matches a rule");
    let dest = wow
        .destination_for("WeakAuras/Core.lua", retail, rule)
        .unwrap();
    assert_eq!(
        dest,
        std::path::Path::new("Interface/AddOns")
            .join("WeakAuras")
            .join("Core.lua")
    );
}

#[test]
fn wow_ignores_root_level_readmes() {
    let wow = pack("wow.toml");
    assert!(wow.rule_for("README.md", "client").is_none());
}

#[test]
fn wow_flavors_are_separate_games() {
    // Retail, Classic Era and progression Classic target different APIs, and an
    // addon built for one will not run on another. Modelling them as targets of
    // one game let a single profile install the same addon list into all three.
    let ids: Vec<&str> = ["wow.toml", "wow-classic-era.toml", "wow-classic.toml"]
        .iter()
        .map(|n| {
            let p = pack(n);
            Box::leak(p.pack.game.id.clone().into_boxed_str()) as &str
        })
        .collect();
    assert_eq!(ids, vec!["wow", "wow-classic-era", "wow-classic"]);

    for name in ["wow.toml", "wow-classic-era.toml", "wow-classic.toml"] {
        let p = pack(name);
        assert_eq!(
            p.pack.targets.len(),
            1,
            "{name} should be one game with one client, not several flavors"
        );
    }
}

#[test]
fn wow_allows_changes_while_the_game_runs() {
    // Addons are Lua read at load time, so blocking mid-session is nuisance
    // rather than safety. Valheim is the opposite: its plugins are mapped DLLs.
    for name in ["wow.toml", "wow-classic-era.toml", "wow-classic.toml"] {
        assert!(
            pack(name).pack.running.allow_changes,
            "{name} should allow mod changes while running"
        );
    }
    assert!(!pack("valheim.toml").pack.running.allow_changes);
    assert!(!pack("minecraft.toml").pack.running.allow_changes);
}

#[test]
fn wow_unpacks_zips() {
    assert!(pack("wow.toml").should_unpack("WeakAuras-5.20.1.zip"));
}

// --- Valheim --------------------------------------------------------------

#[test]
fn valheim_flat_dll_goes_to_plugins() {
    let valheim = pack("valheim.toml");
    let client = valheim.target("client").unwrap();

    let rule = valheim.rule_for("Mod.dll", "client").expect("dll rule");
    let dest = valheim.destination_for("Mod.dll", client, rule).unwrap();
    assert_eq!(dest, std::path::Path::new("BepInEx/plugins").join("Mod.dll"));
}

#[test]
fn valheim_nested_bepinex_tree_is_preserved() {
    let valheim = pack("valheim.toml");
    let client = valheim.target("client").unwrap();

    let path = "BepInEx/plugins/Author-Mod/Mod.dll";
    let rule = valheim.rule_for(path, "client").expect("nested rule");
    let dest = valheim.destination_for(path, client, rule).unwrap();
    assert_eq!(
        dest,
        std::path::Path::new("BepInEx/plugins")
            .join("Author-Mod")
            .join("Mod.dll")
    );
}

#[test]
fn valheim_server_is_a_real_target_sharing_the_client_layout() {
    let valheim = pack("valheim.toml");
    let server = valheim.target("server").unwrap();
    assert_eq!(server.kind, modifile_core::TargetKind::Server);

    // The same rule applies, so one profile covers both sides.
    let rule = valheim.rule_for("Mod.dll", "server").expect("server rule");
    let dest = valheim.destination_for("Mod.dll", server, rule).unwrap();
    assert_eq!(dest, std::path::Path::new("BepInEx/plugins").join("Mod.dll"));
}

#[test]
fn valheim_takes_a_bare_dll_asset_without_unpacking_it() {
    let valheim = pack("valheim.toml");
    let client = valheim.target("client").unwrap();
    let assets = names(&["Mod.dll"]);

    assert!(valheim.select_asset(&assets, client).is_some());
    assert!(!valheim.should_unpack("Mod.dll"));
}

#[test]
fn valheim_can_install_its_mod_loader() {
    // The failure this prevents: mods install perfectly into BepInEx/plugins,
    // BepInEx is not there, and the game silently loads none of them. That is
    // the most confusing possible outcome, so the loader is installable here.
    let valheim = pack("valheim.toml");
    let bepinex = valheim
        .loader("bepinex")
        .expect("Valheim must be able to install BepInEx");

    assert_eq!(bepinex.kind, modifile_core::pack::LoaderKind::Archive);
    assert!(!bepinex.source.is_empty(), "needs somewhere to get it from");
    assert!(
        !bepinex.markers.is_empty(),
        "needs a way to tell whether it is already installed"
    );
    // One build per operating system, and all three must be covered.
    assert!(!bepinex.windows_assets.is_empty());
    assert!(!bepinex.linux_assets.is_empty());
}

#[test]
fn minecraft_loaders_split_installable_from_manual() {
    let mc = pack("minecraft.toml");
    use modifile_core::pack::LoaderKind;

    // Fabric and Quilt publish a metadata service, so Modifile installs them.
    assert_eq!(mc.loader("fabric").unwrap().kind, LoaderKind::FabricMeta);
    assert_eq!(mc.loader("quilt").unwrap().kind, LoaderKind::FabricMeta);
    // Forge and NeoForge patch the game, so we point at their installer.
    assert_eq!(mc.loader("neoforge").unwrap().kind, LoaderKind::Installer);
    assert!(!mc.loader("neoforge").unwrap().page.is_empty());
}

#[test]
fn valheim_configs_are_profile_owned() {
    let valheim = pack("valheim.toml");
    assert!(
        valheim.pack.state.paths.contains(&"config".to_string()),
        "BepInEx rewrites configs, so the profile must own that directory"
    );
}

#[test]
fn valheim_installs_configs_whatever_the_extension() {
    // Regression: matching `*.cfg` meant a mod shipping config/MyMod.json
    // matched no rule at all and was silently not installed. BepInEx's
    // convention is .cfg but mods ship json, yml, ini and subfolders.
    let valheim = pack("valheim.toml");
    let client = valheim.target("client").unwrap();

    for path in [
        "BepInEx/config/MyMod.cfg",
        "BepInEx/config/MyMod.json",
        "BepInEx/config/MyMod.yml",
        "BepInEx/config/nested/deeper.ini",
    ] {
        let rule = valheim
            .rule_for(path, "client")
            .unwrap_or_else(|| panic!("no rule matched {path}"));
        assert!(rule.mutable, "{path} must be treated as editable state");
        assert!(
            valheim.destination_for(path, client, rule).is_some(),
            "{path} must resolve to a destination"
        );
    }
}

#[test]
fn valheim_config_files_are_never_hardlinked() {
    // The bug this guards: a hard-linked config means the game writes the
    // user's edit straight back into the shared content-addressed store.
    let valheim = pack("valheim.toml");
    let rule = valheim
        .rule_for("BepInEx/config/valheim_plus.cfg", "client")
        .unwrap();
    assert!(rule.mutable);

    // A plugin binary is the opposite: immutable, so it links.
    let dll = valheim.rule_for("Mod.dll", "client").unwrap();
    assert!(!dll.mutable);
}

#[test]
fn valheim_ignores_thunderstore_packaging_files() {
    // Thunderstore archives carry these at the root; none belong in a game.
    let valheim = pack("valheim.toml");
    for junk in ["manifest.json", "icon.png", "README.md", "CHANGELOG.md"] {
        assert!(
            valheim.rule_for(junk, "client").is_none(),
            "{junk} is packaging metadata and must not be installed"
        );
    }
}

// --- Minecraft ------------------------------------------------------------

#[test]
fn minecraft_prefers_the_jar_and_rejects_sources() {
    let mc = pack("minecraft.toml");
    let client = mc.target("client").unwrap();
    let assets = names(&["sodium-0.5.8-sources.jar", "sodium-0.5.8.jar"]);

    let chosen = mc.select_asset(&assets, client).unwrap();
    assert_eq!(assets[chosen], "sodium-0.5.8.jar");
}

#[test]
fn minecraft_jars_are_installed_unopened() {
    // A .jar is a zip. Sniffing file magic would wrongly explode it into the
    // mods folder, which is exactly why unpacking is declared per pack.
    assert!(!pack("minecraft.toml").should_unpack("sodium-0.5.8.jar"));
}

#[test]
fn minecraft_shaderpacks_are_client_only() {
    let mc = pack("minecraft.toml");
    let path = "shaderpacks/BSL.zip";

    assert!(mc.rule_for(path, "client").is_some());
    assert!(
        mc.rule_for(path, "server").is_none(),
        "a dedicated server has no renderer and must not receive shaderpacks"
    );
}

#[test]
fn minecraft_mods_reach_both_sides() {
    let mc = pack("minecraft.toml");
    for target_id in ["client", "server"] {
        let target = mc.target(target_id).unwrap();
        let rule = mc.rule_for("sodium.jar", target_id).expect("jar rule");
        let dest = mc.destination_for("sodium.jar", target, rule).unwrap();
        assert_eq!(dest, std::path::Path::new("mods").join("sodium.jar"));
    }
}

#[test]
fn minecraft_configs_are_profile_owned_and_not_cfg_files() {
    // Forge writes config/*.toml, Fabric writes config/*.json. Directory-based
    // matching is the only thing that covers both.
    let mc = pack("minecraft.toml");
    assert!(mc.pack.state.paths.contains(&"config".to_string()));

    for path in ["config/forge-client.toml", "config/sodium-options.json"] {
        let rule = mc
            .rule_for(path, "client")
            .unwrap_or_else(|| panic!("no rule matched {path}"));
        assert!(rule.mutable, "{path} must be editable state");
    }
}

#[test]
fn minecraft_client_and_server_reject_each_others_builds() {
    let mc = pack("minecraft.toml");
    let assets = names(&["mod-1.0-client.jar", "mod-1.0-server.jar"]);

    let client = mc.select_asset(&assets, mc.target("client").unwrap()).unwrap();
    assert_eq!(assets[client], "mod-1.0-client.jar");

    let server = mc.select_asset(&assets, mc.target("server").unwrap()).unwrap();
    assert_eq!(assets[server], "mod-1.0-server.jar");
}

// --- modpack overrides ------------------------------------------------------
//
// A modpack's overrides/ tree is routed by the same install rules as any other
// archive, which is the whole reason importing one needs no new deploy code.
// These pin that routing, because getting it wrong is silent: files simply do
// not appear, and the game starts with the wrong settings rather than failing.

/// Where one archive path ends up for one target, or `None` if nothing claims it.
fn place(mc: &CompiledPack, path: &str, target: &str) -> Option<(std::path::PathBuf, bool)> {
    let t = mc.target(target).unwrap();
    let rule = mc.rule_for(path, target)?;
    let dest = mc.destination_for(path, t, rule)?;
    Some((dest, rule.mutable))
}

#[test]
fn modpack_overrides_land_where_the_pack_meant_them() {
    let mc = pack("minecraft.toml");
    let sep = std::path::MAIN_SEPARATOR.to_string();

    let cases = [
        // Jars in a pack's overrides are mods like any other: linked, tracked,
        // and removed again on deactivate.
        ("overrides/mods/extra.jar", "mods/extra.jar", false),
        // Configs are the profile's to edit.
        ("overrides/config/sodium.json", "config/sodium.json", true),
        ("overrides/resourcepacks/faithful.zip", "resourcepacks/faithful.zip", false),
        ("overrides/shaderpacks/bsl.zip", "shaderpacks/bsl.zip", false),
        // Everything else a pack ships lands at the game root.
        ("overrides/options.txt", "options.txt", true),
        ("overrides/kubejs/server_scripts/s.js", "kubejs/server_scripts/s.js", true),
        ("overrides/defaultconfigs/x.toml", "defaultconfigs/x.toml", true),
    ];

    for (path, want, mutable) in cases {
        let (dest, is_mutable) = place(&mc, path, "client")
            .unwrap_or_else(|| panic!("nothing claimed {path}"));
        assert_eq!(dest, std::path::PathBuf::from(want.replace('/', &sep)), "{path}");
        assert_eq!(is_mutable, mutable, "{path} mutability");
    }
}

#[test]
fn a_packs_index_is_not_installed() {
    // The manifest describes the pack; it is not part of the game.
    let mc = pack("minecraft.toml");
    for path in ["manifest.json", "modrinth.index.json"] {
        assert!(
            mc.rule_for(path, "client").is_none(),
            "{path} should not be installed into the game folder"
        );
    }
}

#[test]
fn side_specific_overrides_stay_on_their_own_side() {
    // The trap this guards: the general config rule carries no target, so
    // without the side-specific rules coming first, a client-only config would
    // be planted on a dedicated server.
    let mc = pack("minecraft.toml");

    assert!(place(&mc, "client-overrides/config/gui.json", "client").is_some());
    assert!(
        place(&mc, "client-overrides/config/gui.json", "server").is_none(),
        "a client override must never reach the server"
    );

    assert!(place(&mc, "server-overrides/server.properties", "server").is_some());
    assert!(
        place(&mc, "server-overrides/server.properties", "client").is_none(),
        "a server override must never reach the client"
    );

    // And they still route by kind, rather than all landing at the root.
    let sep = std::path::MAIN_SEPARATOR.to_string();
    let (dest, _) = place(&mc, "client-overrides/mods/clientonly.jar", "client").unwrap();
    assert_eq!(dest, std::path::PathBuf::from("mods/clientonly.jar".replace('/', &sep)));
}

// --- BepInEx as a modpack dependency ----------------------------------------
//
// Thunderstore modpacks list BepInEx as an ordinary dependency, so its package
// arrives through the same path as any mod. Getting this wrong is silent in the
// worst way: every file installs, and the game then loads no mods at all.

#[test]
fn bepinex_core_never_lands_in_plugins() {
    for name in ["repo.toml", "valheim.toml"] {
        let p = pack(name);
        let target = p.pack.targets[0].clone();
        let sep = std::path::MAIN_SEPARATOR.to_string();

        // The Thunderstore BepInExPack nests everything under BepInExPack/.
        let cases = [
            ("BepInExPack/BepInEx/core/BepInEx.dll", "BepInEx/core/BepInEx.dll"),
            ("BepInExPack/BepInEx/core/0Harmony.dll", "BepInEx/core/0Harmony.dll"),
            ("BepInExPack/BepInEx/core/Mono.Cecil.dll", "BepInEx/core/Mono.Cecil.dll"),
            // The shim that actually starts BepInEx sits beside the game exe.
            ("BepInExPack/winhttp.dll", "winhttp.dll"),
            ("BepInExPack/doorstop_config.ini", "doorstop_config.ini"),
        ];

        for (archive_path, want) in cases {
            let rule = p
                .rule_for(archive_path, &target.id)
                .unwrap_or_else(|| panic!("{name}: nothing claimed {archive_path}"));
            let dest = p
                .destination_for(archive_path, &target, rule)
                .unwrap_or_else(|| panic!("{name}: no destination for {archive_path}"));
            assert_eq!(
                dest,
                std::path::PathBuf::from(want.replace('/', &sep)),
                "{name}: {archive_path} went to the wrong place"
            );
        }
    }
}

#[test]
fn thunderstore_packaging_files_are_not_installed() {
    // Every Thunderstore package carries these. None belong in a game folder.
    for name in ["repo.toml", "valheim.toml"] {
        let p = pack(name);
        let target = p.pack.targets[0].id.clone();
        for junk in ["manifest.json", "icon.png", "README.md", "CHANGELOG.md"] {
            assert!(
                p.rule_for(junk, &target).is_none(),
                "{name}: {junk} should not be installed"
            );
        }
    }
}

#[test]
fn repo_content_bundles_keep_their_folders() {
    // MoreHead cosmetics are found by scanning for a Decorations/ directory
    // under plugins. Flattening them is how every one of them stops loading.
    let p = pack("repo.toml");
    let target = p.target("client").unwrap();
    let sep = std::path::MAIN_SEPARATOR.to_string();

    for (archive_path, want) in [
        ("Decorations/Alex_head.hhh", "BepInEx/plugins/Decorations/Alex_head.hhh"),
        ("DecapitatedMonsters.repobundle", "BepInEx/plugins/DecapitatedMonsters.repobundle"),
    ] {
        let rule = p
            .rule_for(archive_path, "client")
            .unwrap_or_else(|| panic!("nothing claimed {archive_path}"));
        let dest = p.destination_for(archive_path, target, rule).unwrap();
        assert_eq!(dest, std::path::PathBuf::from(want.replace('/', &sep)));
    }
}

// --- launching --------------------------------------------------------------

#[test]
fn a_pack_never_names_anything_outside_the_game_folder() {
    // The safety line for the Play button: a pack may point at a file inside
    // the folder the user chose, and nothing else. This asserts the bundled
    // packs stay on the right side of it, so a careless edit is caught here
    // rather than by someone's antivirus.
    for (name, _) in BUNDLED_PACKS {
        let p = pack(name);
        for target in &p.pack.targets {
            for entry in &target.launch {
                assert!(
                    !entry.contains("..")
                        && !entry.starts_with('/')
                        && !entry.starts_with('\\')
                        && entry.chars().nth(1) != Some(':'),
                    "{name}: `{entry}` escapes the game folder"
                );
                // A command line is not a path, and this is the field that
                // must never become one.
                assert!(
                    !entry.contains(' ') || entry.ends_with(".app"),
                    "{name}: `{entry}` looks like a command, not an executable path"
                );
            }
        }
    }
}

#[test]
fn every_game_can_be_started_somehow() {
    // Either Steam knows it, or the pack names an executable. A game with
    // neither is one whose Play button can only ever be greyed out, which is
    // worth knowing when the pack is written rather than when it is used.
    for (name, _) in BUNDLED_PACKS {
        let p = pack(name);
        let client = p
            .pack
            .targets
            .iter()
            .find(|t| t.kind == modifile_core::TargetKind::Client);
        let Some(client) = client else { continue };

        let startable = client.steam.is_some() || !client.launch.is_empty();
        // Minecraft is the honest exception: its launcher lives outside the
        // game directory, so there is nothing a pack is allowed to name.
        if *name == "minecraft.toml" {
            assert!(
                !startable,
                "minecraft's launcher is outside the game folder; if that changed, \
                 update this test and the pack together"
            );
            continue;
        }
        assert!(
            startable,
            "{name}: the client declares neither a Steam app id nor an executable"
        );
    }
}

// --- instanced play ---------------------------------------------------------
//
// The safety argument for the Play button. An instanced profile puts its mods
// in a directory of its own, so the game install is never modified and there
// is nothing to undo when the game closes — or crashes, or the power goes out.
// These pin the part that makes that true.

#[test]
fn games_that_can_be_instanced_say_so() {
    // Play exists only for these. A game that cannot hand its mods to another
    // directory gets no Play button rather than one that modifies the install.
    for name in ["repo.toml", "valheim.toml", "minecraft.toml"] {
        assert!(
            pack(name).instancing().is_some(),
            "{name} should declare how it can be instanced"
        );
    }
    for name in ["wow.toml", "wow-classic.toml", "wow-classic-era.toml"] {
        assert!(
            pack(name).instancing().is_none(),
            "{name} reads addons from one fixed place and must not claim otherwise"
        );
    }
}

#[test]
fn only_the_injector_is_allowed_in_the_game_folder() {
    // Everything else belongs to the instance. If this ever widens, an
    // "instanced" profile would start modifying the real install again.
    for name in ["repo.toml", "valheim.toml"] {
        let p = pack(name);

        for injector in [
            "winhttp.dll",
            "doorstop_config.ini",
            "run_bepinex.sh",
            ".doorstop_version",
        ] {
            assert!(
                p.belongs_in_game_dir(std::path::Path::new(injector)),
                "{name}: {injector} has to sit beside the executable"
            );
        }

        for mine in [
            "BepInEx/plugins/SomeMod.dll",
            "BepInEx/config/SomeMod.cfg",
            "BepInEx/core/BepInEx.dll",
            "BepInEx/patchers/Thing.dll",
            "options.txt",
        ] {
            assert!(
                !p.belongs_in_game_dir(std::path::Path::new(mine)),
                "{name}: {mine} belongs to the instance, not the game folder"
            );
        }
    }
}

#[test]
fn minecraft_needs_nothing_in_the_game_folder() {
    // Its launcher takes a gameDir, so the vanilla install is untouched
    // entirely — there is not even an injector to plant.
    let mc = pack("minecraft.toml");
    let rules = mc.instancing().expect("minecraft can be instanced");
    assert!(rules.game_files.is_empty());
    assert!(!mc.belongs_in_game_dir(std::path::Path::new("mods/sodium.jar")));
}

#[test]
fn a_doorstop_pack_names_the_assembly_to_invoke() {
    // BepInEx works out its whole root from where this was loaded from, which
    // is the entire mechanism. Without it there is nothing to point at.
    for name in ["repo.toml", "valheim.toml"] {
        let p = pack(name);
        let rules = p.instancing().expect("declared above");
        assert_eq!(rules.kind, modifile_core::pack::InstanceKind::Doorstop);
        assert_eq!(
            rules.target.as_deref(),
            Some("BepInEx/core/BepInEx.Preloader.dll"),
            "{name}"
        );
    }
}
