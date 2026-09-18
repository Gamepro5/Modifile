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

## Quick start (GUI)

Run `modifile-gui`. Everything is doable from the window — you never need the
terminal.

1. **＋ New profile** — pick the game, name the profile.
2. **Add** a mod by pasting a GitHub repo (`WeakAuras/WeakAuras2`, or the URL).
3. **1 · Sync** downloads them. **2 · Deploy** installs them.
4. Launch the game however you normally do.

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
modifile sync wow-main                    # resolve + fetch
modifile deploy wow-main                  # link into the game
```

Then launch the game however you normally do.

`modifile auth` saves a GitHub token. Without one you get GitHub's 60 requests/hour,
and an unauthenticated `304 Not Modified` still spends one of them — with a
token you get 5000/hour and revalidations become free.

### The rest of the commands

| command | what it does |
|---|---|
| `modifile show <profile>` | mods, versions, trust, deployment state |
| `modifile root <profile> <target> <path>` | point a target at a directory autodetection missed |
| `modifile verify <profile>` | check the deployment is intact — finds files a game patch clobbered |
| `modifile undeploy <game> <target>` | remove everything, leaving a clean game directory |
| `modifile gc` | delete store entries no profile references |
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

## Config files belong to the profile

A mod's `.dll` is immutable content and is hard-linked out of the shared store.
Its config file is the opposite — the game rewrites it on startup, you edit it,
and two profiles want different values. Hard-linking one would write your edit
straight back into the shared store and leak it into every other profile using
that mod.

So directories listed under `[state]` are profile-owned. Switching profiles
captures the outgoing profile's configs into its own keeping, then restores the
incoming profile's. A raiding profile and a hardcore profile can run the same
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

| rung | meaning |
|---|---|
| **verified** | GitHub build provenance ties this exact artifact to a commit in this repo |
| **readable** | the artifact is source — you can read what will run |
| **claimed** | compiled artifact, public repo, no proof the binary matches it |
| **blocked** | compiled artifact with no detected open-source license |

Textures, fonts and sounds are opaque but inert, and do not demote a mod. Only
files that can *execute* code you cannot read do.

Every artifact's SHA-256 is pinned in the profile's lockfile, so a release asset
quietly re-uploaded under the same tag is detected.

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
