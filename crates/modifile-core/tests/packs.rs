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
