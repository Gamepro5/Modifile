# Modifile

A universal mod manager. It files your mods into place and then gets out of the
way — no launcher to keep open, no duplicated game installs, no ads, no account.

Two binaries, no runtime dependencies:

| | size |
|---|---|
| `modifile` (CLI) | 5.3 MB |
| `modifile-gui` (desktop) | 10.0 MB |

## Four rules it is built around

1. **Nothing runs while you play.** Mods are hard-linked into the game directory
   and stay there. Launch from Steam, a shortcut, or anywhere else.
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

## Quick start (GUI)

Run `modifile-gui`. Everything is doable from the window — you never need the
terminal.

1. **＋ New profile** — pick the game, name the profile.
2. **Add** a mod by pasting a GitHub repo (`WeakAuras/WeakAuras2`, or the URL).
3. **Check for updates** downloads them. **Activate** puts them in the game.
4. Launch the game however you normally do.

**Modifile is not a launcher.** Activating a profile puts files in your game
folder and then Modifile is done — you start the game from Steam, a shortcut,
or wherever you normally do, and nothing needs to be running.

Profiles are grouped by game in the sidebar, and the active one in each game is
marked with a green dot. **Deactivate** takes its mods back out so the game runs
vanilla again; nothing is lost, your settings are saved into the profile and you
can activate it again whenever.

If your game folder wasn't found automatically, press **Choose folder…** on that
target. The choice is remembered for every profile of that game. **Games &
folders** in the sidebar shows what was detected; **Settings** takes a GitHub
token and cleans up unused downloads.

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
| `modifile export <profile>` | write a shareable `.modifile.json` |
| `modifile import <file>` | create a profile from one someone sent you |
| `modifile root <profile> <target> <path>` | point a target at a directory autodetection missed |
| `modifile verify <profile>` | check the deployment is intact — finds files a game patch clobbered |
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
server). Adding a game means writing a TOML file, not recompiling.

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
instead of the exporter's pinned versions, and `--no-configs` when exporting to
leave your settings out.

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
| **CurseForge** | **yours** | see below |

Paste a URL from any of them, or use a short id:

```sh
modifile add wow-main WeakAuras/WeakAuras2           # GitHub (the default)
modifile add mc modrinth:sodium
modifile add mc https://modrinth.com/mod/lithium
modifile add x gitlab:group/project
modifile add x https://codeberg.org/owner/repo
modifile add x gitea:git.example.com/owner/repo      # self-hosted
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

## What it deliberately does not do

- **No CurseForge, no Nexus.** CurseForge API keys are human-reviewed,
  non-transferable, unshippable in an open-source client, and authors can
  disable third-party distribution entirely. Nexus gates download links behind a
  premium account. Both are out of scope; Modrinth and Thunderstore are keyless
  and are the natural next adapters.
- **No dependency resolution or loader/version matching for Minecraft.** It
  installs the release you asked for. Pin versions when a mod is
  version-sensitive.
- **Attestation checking is API-based, not full offline Sigstore verification.**
  The certificate chain and transparency log are not yet verified locally.

## Safety properties

- Files are removed only when size and mtime still match what was deployed. A
  file a game patch overwrote is left alone and reported, never deleted.
- A destination that already holds a file we did not place is skipped and
  reported, not clobbered, unless `--force`.
- Archive extraction rejects absolute paths, `..`, and drive letters (zip-slip).
- Deployment state lives outside the game directory, so a game update or a Steam
  file verification cannot destroy the bookkeeping along with the mods.
- Game packs are pure data and cannot execute anything.

## Layout

```
crates/modifile-core/  engine, packs, store, deploy, trust, GitHub source
crates/modifile-cli/   the `modifile` command
crates/modifile-gui/   egui desktop app (no webview, no Electron)
packs/                 bundled game packs
```

`cargo test` runs 51 tests, including ones that exercise the bundled packs
directly — flavor selection, client/server routing, and zip-slip rejection.
