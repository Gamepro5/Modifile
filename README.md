# Modifile

A universal mod manager. It files your mods into place and then gets out of the
way — nothing to keep open, no duplicated game installs, no ads, no account.

Two binaries, no runtime dependencies:

| | size |
|---|---|
| `modifile` (CLI) | 6.6 MB |
| `modifile-gui` (desktop) | 12.0 MB |

## Four rules it is built around

1. **Nothing runs while you play.** Mods are hard-linked into the game
   directory and stay there. Launch from Steam, a shortcut, or anywhere else.
   There *is* a Play button, and it does not break this: it gives the profile a
   directory of its own and points the game at it for that run, so nothing has
   to watch the session and nothing has to be undone afterwards. See
   [Play](#play).
2. **Profiles do not duplicate the game.** One content-addressed store, hard
   links into the game directory. A profile costs a text file. Switching
   profiles moves zero bytes.
3. **Game support is data, not code.** A game pack is a TOML file. It can
   express paths and globs and nothing else — it cannot execute anything, which
   matters when packs come from strangers.
4. **Only auditable mods.** GitHub Releases, public source, and a trust ladder
   that refuses to pretend a compiled binary is readable source.

## Download

Prebuilt archives for Windows and Linux are on the
[releases page](https://github.com/Gamepro5/modifile/releases). Each one holds
both binaries and depends on nothing else — unpack it and run. The Linux build
is made on Ubuntu 22.04, so glibc 2.35 or newer is enough. Verify a download
against `SHA256SUMS.txt` if you like.

Or build it yourself; see [Building](#building).

Mods come from GitHub, GitLab, Gitea/Forgejo, Modrinth, Thunderstore and
CurseForge, and **modpacks** from Modrinth, Thunderstore and CurseForge — so one
tool covers a Minecraft `.mrpack` and a R.E.P.O. Thunderstore pack without
keeping two launchers around.

## Quick start (GUI)

Run `modifile-gui`. Everything is doable from the window — you never need the
terminal.

1. Pick your game from the rail down the left, or the **Choose a game** grid.
2. **＋ New profile**, or **Import a modpack…** to start from someone else's.
3. **Browse mods** to search, or paste a link. **Check for updates** downloads
   them; **Activate** puts them in the game.
4. Launch the game however you normally do — or press **▶ Play**, which gives
   the profile its own folder and leaves your install untouched.

**Modifile is still not a launcher**, in the sense that matters: nothing has to
be running for your mods to work, and nothing watches your session. Play hands
the game a directory and gets out of the way. See [Play](#play).

The window is arranged the way a storefront client is, because that part of
CurseForge is genuinely good: a rail of games, a page per game, tabs for
profiles and browsing. What is missing is the rest of it — no ads, no account,
no background service, no duplicated game installs. Mod icons, screenshots and
game art are fetched from the same indexes the mods come from and cached on
disk; **Settings → Artwork** turns the lot off and frees the textures, and
everything still works without a single picture.

**Deactivate** takes a profile's mods back out so the game runs
vanilla again; nothing is lost, your settings are saved into the profile and you
can activate it again whenever.

If your game folder wasn't found automatically, press **Choose folder…** in the
**Game folders** card on that game's page. The choice is remembered for every
profile of that game. **Settings** holds the GitHub token, the artwork and
update switches, and the download cleanup — and **Settings → Games & folders**
shows every game's detected paths at once.

## Quick start (CLI)

```sh
modifile init                             # write the bundled packs
modifile games                            # what was found on this machine
modifile new wow-main --game wow --target retail
modifile add wow-main WeakAuras/WeakAuras2
modifile update wow-main                  # check GitHub, download
modifile activate wow-main                # put the mods in the game
modifile deactivate wow retail            # take them out again — vanilla
```

**Profile names are per game.** Every game can have its own `main`; the name
only has to be unique within the game it belongs to. Commands take a bare name
while it is unambiguous, and ask you to qualify it when it is not:

```sh
modifile show main            # fine, until two games have one
modifile show valheim/main    # always unambiguous
```

Profiles that already existed are moved into their game's directory
automatically on first run, with their versions and saved settings.

**Updates are per profile.** `modifile update <profile>` touches that profile
and nothing else; in the GUI, *Check for updates* does the profile on screen.
Updating everything is a separate, explicit thing — `modifile update --all`, or
*Check every game for updates* on the all-games page — because it downloads a
lot and changes profiles you were not looking at.

A mod can be restricted to one side of the game, which matters for a dedicated
server:

```sh
modifile add vh-server Grantapher/ValheimPlus --target server
```

In the GUI each mod row has a **client + server / client only / server only**
control, shown for games that actually have a dedicated server.

Then launch the game however you normally do.

`modifile auth` saves a GitHub token. Without one you get GitHub's 60 requests/hour,
and an unauthenticated `304 Not Modified` still spends one of them — with a
token you get 5000/hour and revalidations become free.

### The rest of the commands

| command | what it does |
|---|---|
| `modifile show <profile>` | mods, versions, trust, active state |
| `modifile rename <old> <new>` | rename a profile, keeping mods, downloads and settings |
| `modifile delete <profile> --yes` | delete a profile; downloads and other profiles untouched |
| `modifile versions <profile> <mod>` | every release of a mod, and which this game can install |
| `modifile hold <profile> <mod> <version>` | run a specific version instead of the newest |
| `modifile hold <profile> <mod> --latest` | follow the newest release again |
| `modifile export <profile>` | write a shareable `.modifile.json` |
| `modifile import <file>` | create a profile from one someone sent you |
| `modifile update <profile>` | check that profile's mods and download what is missing |
| `modifile update --all` | every profile of every game, deliberately |
| `modifile pack search <query> --game <id>` | find a modpack |
| `modifile pack info <file\|link\|id>` | what a modpack contains, importing nothing |
| `modifile pack add <file\|link\|id>` | turn a modpack into a profile |
| `modifile play <profile>` | start the game with this profile's own folder |
| `modifile play <profile> --set-command <cmd>` | how to start this game, for one no pack can name |
| `modifile selfupdate --check` | is there a newer Modifile? |
| `modifile selfupdate --yes` | install it |
| `modifile trust` | what Modifile is currently willing to install |
| `modifile trust --allow-no-source true` | accept mods nobody can audit |
| `modifile root <profile> <target> <path>` | point a target at a directory autodetection missed |
| `modifile verify <profile>` | check the deployment is intact — finds files a game patch clobbered |
| `modifile repair <profile>` | checksum the stored copies and put changed or missing files back |
| `modifile undeploy <game> <target>` | remove everything, leaving a clean game directory |
| `modifile storage` | what is downloaded and which profiles still want it |
| `modifile storage --clean` | delete downloads nothing references |
| `modifile config show <profile>` | the config files this profile keeps |
| `modifile config reset <profile> --yes` | back to the mods' shipped defaults |
| `modifile config import <profile> --from <other>` | copy another profile's settings |
| `modifile config import <profile> --from-game` | adopt settings already in the game folder |

## Building

```sh
cargo build --release              # both binaries
cargo build --release -p modifile-cli   # CLI only, no GUI dependencies
```

### Linux

The GUI is egui on glow — no webview, no Electron — but it still needs the
usual windowing and GL headers at build time, and `aws-lc-sys` (via rustls)
needs a C toolchain:

```sh
# Debian / Ubuntu
sudo apt install build-essential cmake pkg-config \
     libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
     libxkbcommon-dev libwayland-dev libgl1-mesa-dev

# Fedora
sudo dnf install gcc gcc-c++ cmake pkgconf-pkg-config \
     libX11-devel libXcursor-devel libXrandr-devel libXi-devel \
     libxkbcommon-devel wayland-devel mesa-libGL-devel
```

At runtime the **Browse…** button uses the XDG desktop portal
(`xdg-desktop-portal` plus a backend such as `xdg-desktop-portal-gtk`). It is
optional: every folder chooser also takes a typed path, so the GUI works fully
without a portal installed.

On a headless server, build with `-p modifile-cli` and skip all of the above.

### Release archives

`build_all.sh` (Linux) and `build_all.ps1` (Windows) build both binaries and
package them the way the releases are packaged, into `dist/`:

```sh
./build_all.sh --bootstrap      # installs the toolchain and headers, once
./build_all.sh                  # dist/modifile-vX.Y.Z-x86_64-linux.tar.gz
```

```powershell
.\build_all.ps1                 # Windows natively, Linux through WSL
.\build_all.ps1 -WindowsOnly    # just the zip
```

There is nothing to cross-compile with: the Linux build needs a C toolchain for
`aws-lc-sys` and the windowing headers for the GUI, so `build_all.ps1` hands
that half to WSL rather than pretending. For an actual release, push a tag and
let the runners do it — they build each platform on its own machine, and Linux
on an older distro than yours:

```sh
git tag v1.0
git push origin v1.0            # .github/workflows/release.yml
```

That builds both platforms and attaches the archives and `SHA256SUMS.txt` to a
draft release, leaving the notes for you to edit before publishing. Publishing
a release from the GitHub web UI works too — the archives are uploaded into it
when the build finishes. Either way the tag may be written `v1.0` or `1.0`, but
it has to start with a digit or a `v`.

## Supported games

Bundled packs: **World of Warcraft** (retail, Classic Era, progression Classic),
**Valheim** (client + dedicated server), **Minecraft** (client + dedicated
server), **R.E.P.O.** Adding a game means writing a TOML file, not recompiling.

Dedicated servers are first-class. A Valheim profile deploys to the client and
the server from one mod list; a Minecraft profile sends `.jar` files to both
sides but keeps shaderpacks off the server, because a server has no renderer.

## Writing a game pack

A pack is a `.toml` file in the packs directory. This is the whole schema.

```toml
schema = 1

[game]
id = "valheim"
name = "Valheim"

# Logical path names, relative to a target's root directory.
[paths]
plugins = "BepInEx/plugins"
config  = "BepInEx/config"

# Directories the *profile* owns rather than the mods. Captured when you switch
# away, restored when you switch back, so two profiles keep two different
# configs. Never hard-linked — see "Config files" below.
[state]
paths = ["config"]

# Which file to take out of a release that may carry a dozen.
[assets]
prefer = ["*-{flavor}.zip"]   # {flavor} comes from the target
accept = ["*.zip", "*.dll"]
reject = ["*sources*", "*.pdb"]
unpack = ["*.zip"]            # everything else installs as a single file

# Where each file in the archive lands. First match wins.
[[install]]
match   = "**/plugins/**"     # glob over the archive-relative path
into    = "plugins"           # a name from [paths]
strip   = "plugins/"          # drop this prefix
flatten = false               # or discard directory structure entirely
targets = ["client"]          # omit for all targets
mutable = false               # true = a default the user will edit: copied,
                              # only if absent, and never removed
skip    = false               # true = matched and deliberately not installed

# A target may also name how to start the game. Paths inside the game folder
# only — a pack can never name a command. See "Play".
launch = ["Wow.exe", "World of Warcraft.app"]

# How this game can be pointed at a profile's own folder. Omit it and the game
# gets no Play button, which is the right answer for one that cannot be
# redirected. See "Play".
[instance]
kind = "doorstop"                                # or "minecraft-launcher"
target = "BepInEx/core/BepInEx.Preloader.dll"    # what doorstop invokes
# The only things an instanced profile writes into the real game folder,
# because the game loads them by name at startup. Inert unless Modifile
# launches the game with redirection switched on.
game_files = ["winhttp.dll", "doorstop_config.ini"]

# A target is a client, a dedicated server, or a game flavor.
[[targets]]
id      = "server"
name    = "Valheim Dedicated Server"
kind    = "server"            # client | server
markers = ["valheim_server.exe", "valheim_server.x86_64"]  # any one identifies it
processes = ["valheim_server.exe"]   # deploying is refused while these run
candidates = ["${HOME}/.steam/steam/steamapps/common/Valheim dedicated server"]
asset_reject = ["*-client*"]  # rejections only this target applies

[targets.steam]
app_id = 896660
dir    = "Valheim dedicated server"   # under steamapps/common
```

Steam libraries are discovered by parsing `libraryfolders.vdf`, so a game on a
second drive is found without configuration.

`unpack` is declared rather than sniffed on purpose: a Minecraft `.jar` is a zip
that must be installed **unopened**, and file magic cannot tell you that.

`skip` exists because matching is first-wins and the general rules carry no
target restriction. A modpack's `client-overrides/config/**` has to be claimed
for the server by *something*, or it falls through to the plain config rule and
gets planted on a dedicated server — which is the one thing naming the tree
"client" was meant to prevent. The same rule keeps Thunderstore's `manifest.json`
and `icon.png` out of a game directory.

In `[search]`, `thunderstore_community` is the slug in the site's own URL —
`thunderstore.io/c/repo/` is `"repo"`. It is what lets a downloaded Thunderstore
pack be matched to a game, because a Thunderstore package records no game
anywhere. `curseforge_class_id` and `curseforge_modpack_class_id` are per-game
numbers, not universal ones.

## Downloads are not an archive

Updating a mod does not keep the old version around. The superseded download is
removed as part of the update, because nothing references it and re-downloading
is one request.

A download is *not* waste just because its profile is switched off — that is
what profiles are for — so those are kept and labelled as such:

```
3 download(s), 1.4 MB total

    1.4 MB  ValheimPlus 0.10.1.2               kept for switched-off profiles
            in `main`
   31.5 KB  unused (0ed9d9ec1820)              UNUSED
```

`modifile storage` shows the list; **Settings → Downloads** in the GUI shows the
same thing with a Delete button per unused item. A download is only ever called
unused when *no* profile's lockfile mentions it.

## Sharing a profile

```sh
modifile export raiding --note "Valheim co-op, tuned configs"
# -> raiding.modifile.json
```

One JSON file you can drop in Discord. It holds the mod list, the exact versions
you are running, and your config files. Your friend runs `modifile import
raiding.modifile.json` (or **Import a shared profile…** in the GUI) and gets a
profile pinned to your versions, with your settings already in place.

What a bundle deliberately does *not* contain is the mods themselves, or hashes
presented as trustworthy. Their copy resolves every mod from GitHub on their own
machine, verifies each download against GitHub's published digest, and runs the
trust ladder locally. A bundle from a stranger can waste your time; it cannot
hand you a binary nobody else can see.

Importing never overwrites an existing profile — a second import of the same
file becomes `raiding-2`. Pass `--latest` to take the newest release of each mod
instead of the exporter's versions, and `--no-configs` when exporting to leave
your settings out.

### Held versions

An import that keeps the sender's versions — **Exactly as they had it**, or
`import` without `--latest` — holds every mod at the version they were running.
That is the point of it: a shared setup is one that was known to work together.
The consequence is that checking for updates will not move those mods, ever, and
an update check that stays quiet about it is indistinguishable from being up to
date.

So it does not stay quiet. Every check asks what the newest release is even for
a held mod, and says which ones have been overtaken:

```
  HELD     Grantapher/ValheimPlus: held at 0.9.9.15, 0.9.9.21 is out
```

The GUI marks those mods `held at 0.9.9.15 · 0.9.9.21 out`, and its update button
reads **Download these versions** rather than *Check for updates* when every mod
in the profile is held — because that is what pressing it does.

Moving one, or all of them:

```sh
modifile versions raiding Grantapher/ValheimPlus   # every release, and what installs
modifile hold raiding Grantapher/ValheimPlus 0.9.9.21
modifile hold raiding Grantapher/ValheimPlus --latest
```

In the GUI the same choices are on each mod's version menu — including **Choose
a version…**, which lists what that mod has actually published — and *More →
Take the newest for all held mods* releases the lot at once.

## Mods that were already there

Modifile never deletes a file it did not place. If you modded by hand before, or
another manager left something behind, **Refresh** in the GUI (or
`modifile verify <profile>`) reports every file in your mod folders as one of:

- **intact** — placed by Modifile, unchanged
- **changed** — placed by Modifile, then edited or overwritten by a game update
- **missing** — placed by Modifile, now gone
- **foreign** — Modifile did not put it there

A foreign file that sits exactly where one of your profile's mods goes is marked
**BLOCKING**, because activating will skip that mod rather than overwrite your
file — so the mod silently would not install. Either delete the old file, or use
**Replace them and activate** / `modifile activate <profile> --force` to let
Modifile take it over.

The GUI runs this check whenever you open an active profile, not only when you
press **Refresh**. A profile whose files a game patch ate should not look
perfectly installed until someone thinks to ask.

### Putting a damaged install back

**Put these files back**, or `modifile repair <profile>`, restores every changed
or missing file to the version in the lockfile. Foreign files are never touched.

It checksums the store first, and that order matters. A deployed file is a hard
link to the stored one — the same bytes under two names — so a tool that writes
to the game's copy in place has written through into the store as well. Linking
that entry back into the game folder would only reinstall the damage. Anything
that fails its checksum is deleted and downloaded again; everything else is
restored from the copy you already have, with no download at all.

Store files are kept read-only for the same reason, so the common version of
this — something overwriting a mod file in place — fails at the write instead of
quietly corrupting the only good copy. Config files are exempt: they are seeded
as real copies and belong to the profile, because the game is supposed to
rewrite them.

Entries downloaded before checksums existed report as *unrecorded* rather than
damaged, and get a record the next time they are fetched.

## Mod loaders

Most games need something installed *into* the game before it reads mods at
all — BepInEx for Unity games, Fabric or NeoForge for Minecraft. Install the
mods without it and everything lands correctly while the game ignores all of
it, which is the most confusing way for modding to fail. So Modifile installs
loaders too, and warns at activation when one is missing.

A loader is declared in the game's pack, so adding a game means adding its
loader as well — no new build of Modifile. Three kinds cover what exists:

```toml
# 1. `archive` — extract over the game. Most Unity modding.
[[loaders]]
id = "bepinex"
name = "BepInEx"
kind = "archive"
source = "github:BepInEx/BepInEx"      # any source Modifile supports
windows_assets = ["bepinex_win_x64_*.zip"]
linux_assets   = ["bepinex_linux_x64_*.zip"]
markers = ["BepInEx/core/BepInEx.dll", "winhttp.dll"]   # already installed?
into = ""            # subfolder to unpack into; empty means the game root
targets = ["client", "server"]   # omit for all targets

# 2. `fabric-meta` — a metadata service hands out a finished version profile,
#    so installing is writing one JSON file. No installer, no Java.
[[loaders]]
id = "fabric"
kind = "fabric-meta"
meta = "https://meta.fabricmc.net/v2"
prefix = "fabric"

# 3. `installer` — it patches the game and has to actually run, so Modifile
#    links to it rather than pretending.
[[loaders]]
id = "neoforge"
kind = "installer"
page = "https://neoforged.net/"
```

Games that need no loader — World of Warcraft reads addons natively — simply
declare none, and the loader UI never appears.

A game with exactly one loader does not make you choose it. Installing covers
every target the profile uses, so a dedicated server gets its own copy in its
own directory. Files you have edited, configs especially, survive a reinstall.

**Which build gets installed is decided by the game, not by your computer.** A
`winhttp.dll` loader is useless to a native Linux build and essential to a
Windows one running under Proton, so Modifile looks at what is actually in the
game folder — a `.exe` means the Windows loader even on Linux, a `.x86_64` means
the Linux loader even when driven from a Windows desktop over a file share.
Only when the folder says nothing does it fall back to the host.

## Config files belong to the profile

A mod's `.dll` is immutable content and is hard-linked out of the shared store.
Its config file is the opposite — the game rewrites it on startup, you edit it,
and two profiles want different values. Hard-linking one would write your edit
straight back into the shared store and leak it into every other profile using
that mod.

So directories listed under `[state]` are profile-owned. The **first** time you
activate a profile, settings already sitting in the game folder are adopted into
it — you do not lose the ValheimPlus config you spent an evening tuning. After
that, switching profiles captures the outgoing profile's configs into its own
keeping and restores the incoming profile's. A raiding profile and a hardcore profile can run the same
ValheimPlus build with completely different `valheim_plus.cfg` files, and
neither ever touches the other.

Defaults shipped inside a mod archive are marked `mutable = true`: copied in
only when nothing is there yet, then yours. That is also why a mod *update*
never overwrites your settings — the new default is only used if no file exists.

**Your edits are never the only copy.** The mod's pristine default stays in the
content-addressed store, immutable and keyed by artifact hash, so resetting is
always possible:

```sh
modifile config show raiding                  # what this profile keeps
modifile config reset raiding --yes           # back to the mods' defaults
modifile config import raiding --from hardcore   # copy another profile's
modifile config import raiding --from-game       # adopt what's in the game folder
modifile config import raiding --from-folder ./backup
```

`--from-game` is for an install you modded by hand before using Modifile: it
takes what's already sitting in the game folder and makes it this profile's.
The same controls are in the GUI under **CONFIGS**.

Extensions are not assumed anywhere. `[state]` captures the whole directory —
`.cfg`, Forge's `.toml`, Fabric's `.json`, nested folders, anything a mod
writes. Install rules match the config *directory* rather than a file
extension, for the same reason.

**Known limit:** a mod that stores settings outside the declared `[state]`
directories — next to its DLL, or in a file at the game root — is not covered.
Add that path to the pack's `[state] paths` to fix it, no rebuild needed.

## Mod changes are blocked while the game runs

Swapping a plugin under a live game corrupts both the game's state and ours —
the DLLs are mapped into memory, and the game rewrites its configs on exit,
which would overwrite whatever was just captured. So deploy and undeploy refuse
while the game is running, with no override flag.

A pack declares the executables that count via `processes`. That list is
authoritative rather than "anything running from the game folder", because WoW
ships `Utils/WowVoiceProxy.exe`, which outlives the game and would otherwise
block modding permanently. Packs that declare no names — Minecraft, whose
process is `javaw.exe` — fall back to folder-based detection.

### Servers on another machine

A game folder on a file share works — point a profile at the UNC path
(`\\host\share\ValheimServer\server`) or the mount point. Two things change:

- **Deployment falls back to copying.** Hard links cannot span machines, so the
  zero-bytes-per-profile property applies to the local store only. Plugins are
  a few megabytes, so this is rarely worth caring about.
- **The running-game guard cannot see that machine.** Its process list is not
  ours. Rather than silently passing, deploy refuses with an explanation and
  requires you to state that the server is stopped — `--confirm-stopped` on the
  CLI, a checkbox on the target card in the GUI. Locally there is still no
  override, because locally we can actually check.

## Bundled packs update themselves

A pack is a bug-fixable artifact. A bundled pack you have *not* edited is
refreshed in place when a new build ships a fix. One you have edited is never
overwritten; it is reported so you know a newer version exists. "Edited" is
decided by hashing against `packs/.bundled.json`.

## The trust ladder

A public repo is not the same as auditable code. WoW addons ship Lua, so the
artifact *is* the source. Valheim plugins and Minecraft mods ship compiled
binaries, and nothing about a public repo proves the binary came from it.

| badge | meaning |
|---|---|
| **verified build** | GitHub proves this exact file was built from the public source |
| **readable source** | the mod ships as source — every file can be opened and read |
| **unverified binary** | a compiled file; the source is public but nothing proves the file matches it |
| **no licence** | a compiled file with no open-source licence to check against — refused |

Textures, fonts and sounds are opaque but inert, and do not demote a mod. Only
files that can *execute* code you cannot read do.

Every artifact's SHA-256 is pinned in the profile's lockfile, so a release asset
quietly re-uploaded under the same tag is detected.

## Where mods come from

| source | key needed | notes |
|---|---|---|
| **GitHub** | no | release assets; build attestations checkable |
| **GitLab** | no | gitlab.com and self-hosted |
| **Gitea / Forgejo** | no | Codeberg and self-hosted |
| **Modrinth** | no | publishes each project's source repository |
| **Thunderstore** | no | where BepInEx games' mods are — Valheim, R.E.P.O. |
| **CurseForge** | **yours** | see below |

Paste a URL from any of them, or use a short id:

```sh
modifile add wow-main WeakAuras/WeakAuras2           # GitHub (the default)
modifile add mc modrinth:sodium
modifile add mc https://modrinth.com/mod/lithium
modifile add x gitlab:group/project
modifile add x https://codeberg.org/owner/repo
modifile add x gitea:git.example.com/owner/repo      # self-hosted
modifile add repo-main thunderstore:Zehs/REPOLib
modifile add repo-main https://thunderstore.io/c/repo/p/Zehs/REPOLib/
```

### Finding a mod

```sh
modifile search auction --profile wow-main
```

Which index is searched is a property of the game, declared in its pack. For
Minecraft that is Modrinth; **for World of Warcraft it is GitHub**, because
Modrinth carries no WoW addons and the addons Modifile can install are exactly
the ones publishing GitHub releases. Results are marked `[no releases]` when
nothing installable is published, and each one shows where its source lives — so
search also answers "what is this mod's repository?".

### CurseForge

Supported, but on CurseForge's terms rather than ours:

- **You supply the key.** Overwolf issues them after a human review and forbids
  sharing, so an open-source binary cannot carry one. `modifile auth
  --curseforge <key>`, or Settings in the GUI.
- **Some mods cannot be fetched at all.** Authors can switch off third-party
  distribution; the API then returns no download URL. No key changes that.
- **Nothing to audit.** CurseForge publishes no licence or source through its
  API, so its mods usually land on the bottom rung and are refused unless you
  tick *"Allow mods with no public source code"*.

For most mods, searching Modrinth or GitHub finds the same thing without any of
that.

## Modifile updates itself

Modifile checks its own GitHub releases when it starts, and offers a newer
version if one is published. One conditional request, cached like everything
else, and silent when there is nothing new.

```sh
modifile selfupdate --check                  # is there anything newer?
modifile selfupdate --yes                    # install it
modifile selfupdate --check-on-startup false # stop looking
modifile selfupdate --automatic true         # install without asking
```

In the GUI a strip appears across the top — *"Modifile 1.2 is available"*, with
**What's new** and **Update now** — and Settings has both switches.

**Checking is on by default; installing is not.** Replacing the program someone
is running is not a thing to do on a default, so it asks. Turn on *Install them
without asking* and it stops asking.

**It is verified before anything is replaced.** GitHub publishes a SHA-256 for
every release asset through its API, and the release carries a `SHA256SUMS.txt`;
both are checked. A release publishing neither is refused rather than installed
on trust. What that proves is that the bytes are the ones GitHub is serving —
not that they are benign. Nothing downloaded can prove that, and this project
does not tell that lie about mods either.

**A failed update leaves the working copy alone.** The new binary is written
beside the old one and swapped in by rename; if the swap fails halfway the
previous binary is put back. Windows will not delete a running executable, so
the old one is parked aside and cleared on the next launch. Your profiles,
mods, downloads and settings are never touched by an update.

### Publishing one (for whoever maintains this)

```sh
# 1. The crate version and the tag must agree.
#    Edit workspace.package.version in Cargo.toml to 1.2.0
git commit -am "1.2"
git tag v1.2 && git push origin v1.2

# 2. The workflow builds both platforms and opens a DRAFT release.
# 3. Check the archives, edit the notes, press Publish.
```

Nobody is offered an update until you press Publish — drafts are invisible to
the API, which is the point of building them as drafts.

**The version in `Cargo.toml` must match the tag.** Modifile decides whether it
is out of date by comparing its built-in version against the newest published
tag, so a binary built from `0.1.0` and released as `v1.1` believes it is
permanently behind itself and re-offers the same update for ever. The release
workflow refuses to build a tag that disagrees, so this cannot ship by accident.

## Play

Some people want a launcher. The obvious way to build one — put the mods in the
game folder, take them out when the game closes — is also a bad one: a crash, a
power cut, or simply closing Modifile leaves the install modified.

Other launchers do not have this problem because **they do not revert anything.
They redirect.** r2modman never puts mods in the game folder at all; it starts
the game with `--doorstop-target-assembly <profile>/BepInEx/…`, and BepInEx
reads its plugins and configs from there. Prism does the same for Minecraft
with `gameDir`. The install stays vanilla permanently, so there is nothing to
undo and a power cut costs nothing.

Modifile does that:

```sh
modifile play raiding      # give this profile its own folder, point the game at it
```

| | what happens | game install |
|---|---|---|
| **Activate** | mods go into the game folder and stay | modified until you deactivate |
| **▶ Play** | the profile gets its own folder; the game is pointed at it for that run | **never touched** |

**Instances are nearly free here.** Other launchers duplicate mod files per
profile. Modifile deploys by hard link from one content-addressed store, so ten
instances of the same 900 MB pack cost 900 MB and some directory entries — a
real 91-mod pack measures *909 MB on disk, 0 duplicated*. The only per-instance
data is saves and configs, which you wanted separate anyway.

**One file does go into the game folder**, and only one: the injector. A
doorstop shim is a DLL the game loads by name at startup, so it cannot live
anywhere else. It does nothing on its own — started from Steam or a shortcut,
the game finds a doorstop that has not been told to do anything and runs
vanilla. Minecraft needs not even that.

Measured on a real R.E.P.O. pack:

```
game folder        instance
REPO.exe           BepInEx/core/      18 files
winhttp.dll        BepInEx/plugins/  932 files
doorstop_config.ini  …               954 total
```

### Which games can do this

| game | Play | why |
|---|---|---|
| **Valheim, R.E.P.O.** | yes | BepInEx, via UnityDoorstop |
| **Minecraft** | yes | the launcher takes a `gameDir` per profile |
| **World of Warcraft** | **no** | addons must live in `Interface/AddOns`; there is no redirection |

A game that cannot be instanced gets **no Play button**, and says why. Giving it
one would mean modifying the install and putting it back — exactly the design
this replaced.

Minecraft is the honest half-measure: Modifile writes a launcher profile
pointing at the instance, and the launcher owns the process, so Play opens the
launcher and you pick the profile there.

**Modifile will not start a game unmodded and say nothing.** Play on a profile
that has not been set up refuses rather than launching vanilla and letting you
find out twenty minutes later.

### How a game gets started

Steam first, where it applies — that keeps Proton, cloud saves and the overlay,
all of which running the executable directly would lose. Otherwise the pack may
name an executable *inside the game folder you chose*:

```toml
[[targets]]
launch = ["Wow.exe", "World of Warcraft.app"]   # first one that exists wins
```

**A pack may not name a command.** Packs are data contributed by strangers and
cannot execute anything; that rule does not get relaxed for a Play button. A
pack-declared path may not be absolute, may not contain `..`, may not carry a
drive letter, and must still resolve inside the game directory after symlinks
are followed — there is a test asserting the bundled packs stay on the right
side of that line.

A free-form command line is a **user** setting, kept in `launch.json` and never
readable from a pack. That is the escape hatch for a game whose launcher lives
elsewhere — Minecraft's does, so its pack declares nothing and says so:

```sh
modifile play mc --set-command "java -jar /path/to/launcher.jar"
```

## Modpacks

**A modpack is a profile somebody else assembled.** It carries exactly what a
profile holds — a game version, a loader, a pinned mod list and a tree of config
files — so importing one writes a profile and stops. The mods are fetched by the
next `modifile update`, through the same resolve, verify and trust path every
other mod takes. A pack gets no shortcut past the trust ladder for arriving in
bulk.

```sh
modifile pack search "optimized" --game minecraft   # find one
modifile pack info <file|link|id>                   # what's in it, importing nothing
modifile pack add <file|link|id>                    # -> a profile
modifile update <that profile>                      # download its mods
modifile activate <that profile>                    # put them in the game
```

Three formats, and the differences are the stores' choices rather than ours:

| format | games | key needed |
|---|---|---|
| **Modrinth `.mrpack`** | Minecraft | no |
| **Thunderstore** | any BepInEx game — Valheim, R.E.P.O. | no |
| **CurseForge** | Minecraft | **yours** |

A source is a file you downloaded, a link you copied, or an id that
`pack search` printed:

```sh
modifile pack add ./Fabulously.Optimized-v15.mrpack
modifile pack add https://modrinth.com/modpack/fabulously-optimized/version/9TjRcKTW
modifile pack add modrinth:fabulously-optimized          # newest version
modifile pack add thunderstore:Blazed/REPO_The_God_Pack
modifile pack add curseforge:1075252
```

**Why the pinned versions matter.** A pack names an exact build of every mod,
because that is the combination its author actually ran. Modifile keeps those
pins, so `modifile update` will not quietly walk a 90-mod pack forward one mod
at a time into a combination nobody has tested. The mod list says which pins
newer releases have passed, and `modifile hold <profile> <mod> --latest` takes
one when you want it.

A pack's own files — CurseForge and Modrinth call it `overrides/`, Thunderstore
just ships them — are installed by the game pack's ordinary rules. Its configs
become profile state you can edit, and two profiles from two packs keep two
different sets.

### Thunderstore packs and the trust ladder

Most Thunderstore mods for Unity games are compiled DLLs with no published
source and no licence. There is genuinely nothing to check, so the default
policy refuses them, and a 90-mod R.E.P.O. pack will mostly not install until
you say otherwise:

```sh
modifile trust                             # what is currently allowed
modifile trust --allow-no-source true      # accept binaries nobody can audit
```

That is a real decision, not a formality, which is why it is a separate command
rather than a flag buried in the import. The GUI has the same switch in
Settings.

### Known limits

- CurseForge and Modrinth pack formats are Minecraft-only by definition
  (`manifestType: "minecraftModpack"`, and `.mrpack` defines only Minecraft).
  Thunderstore's format is the one that carries packs for other games.
- A pack's loader *version* is recorded and reported, not pinned — Modifile
  installs the newest stable build of that loader.
- Thunderstore's API serves one version at a time and rate-limits hard.
  Modifile holds its own concurrency down and retries with backoff, which is why
  importing a large pack takes a few seconds longer than it looks like it should.
- Dependency resolution is still the pack's job, not Modifile's. A pack lists
  what it needs; nothing here works out what a pack forgot.

## What it deliberately does not do

- **No Nexus.** It gates download links behind a premium account, so there is
  nothing a client like this can fetch. CurseForge *is* supported now, on its
  own terms — you supply the key, and some mods still cannot be fetched at all.
- **No dependency resolution or loader/version matching for Minecraft.** It
  installs the release you asked for. Pin versions when a mod is
  version-sensitive. A modpack does this for you, because its author already
  worked out the combination.
- **Attestation checking is API-based, not full offline Sigstore verification.**
  The certificate chain and transparency log are not yet verified locally.

## Safety properties

- Files are removed only when size and mtime still match what was deployed. A
  file a game patch overwrote is left alone and reported, never deleted.
- A destination that already holds a file we did not place is skipped and
  reported, not clobbered, unless `--force`.
- Archive extraction rejects absolute paths, `..`, and drive letters (zip-slip).
- A pack may name an executable to launch, but only inside the game directory
  you chose, and only after canonicalisation — so a symlink out of the tree is
  refused too. A command line can only ever come from you.
- The GitHub token is sent to GitHub hosts and nowhere else. It used to travel
  with every download, including to CurseForge and Modrinth CDNs.
- Deployment state lives outside the game directory, so a game update or a Steam
  file verification cannot destroy the bookkeeping along with the mods.
- Game packs are pure data and cannot execute anything.

## Layout

```
crates/modifile-core/  engine, packs, store, deploy, trust, modpacks, sources
crates/modifile-cli/   the `modifile` command
crates/modifile-gui/   egui desktop app (no webview, no Electron)
  main.rs                state, background work, the profile page
  shell.rs               game rail, game grid, a game's page
  browse.rs  detail.rs   finding mods, and the page about one
  art.rs                 icons and screenshots, cached and switchable
packs/                 bundled game packs
```

`cargo test` runs 165 tests, including ones that exercise the bundled packs
directly — flavor selection, client/server routing, zip-slip rejection,
modpack override routing, and the refusal to let a pack name an executable
outside the game folder.
