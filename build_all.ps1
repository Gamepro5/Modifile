#Requires -Version 5.1
<#
.SYNOPSIS
    Build release archives of both Modifile binaries for Windows and Linux.

.DESCRIPTION
    Windows is built natively with cargo. Linux is built by handing the job to
    build_all.sh inside WSL, because the Linux build needs a C toolchain for
    aws-lc-sys and X11/Wayland/GL headers for the GUI — none of which exist on
    the Windows side, so there is no honest way to cross-compile it here.

    The archives land in dist\ alongside a SHA256SUMS.txt, ready to attach to a
    GitHub Release. Pushing a v* tag does the same thing on GitHub's runners
    (see .github\workflows\release.yml), and that is the better path for a real
    release: it builds Linux on Ubuntu 22.04, so the binary also runs on older
    distros than whatever WSL happens to have installed.

.EXAMPLE
    .\build_all.ps1
    Build and package both platforms into dist\.

.EXAMPLE
    .\build_all.ps1 -Bootstrap
    Install the Rust toolchain and headers inside WSL first (prompts for sudo),
    then build. Only needed once.

.EXAMPLE
    .\build_all.ps1 -WindowsOnly
    Skip the Linux half entirely.
#>
[CmdletBinding()]
param(
    # Build only the Windows archive.
    [switch]$WindowsOnly,
    # Build only the Linux archive.
    [switch]$LinuxOnly,
    # Install the Linux build prerequisites inside WSL before building.
    [switch]$Bootstrap,
    # Skip modifile-gui and its dependencies; CLI only.
    [switch]$CliOnly,
    # Where the archives go. Default: dist\ next to this script.
    [string]$OutDir,
    # WSL distribution to build Linux in. Default: your default distro.
    [string]$Distro
)

$ErrorActionPreference = 'Stop'
$root = $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $root 'dist' }

function Say($text) { Write-Host "`n==> $text" -ForegroundColor Cyan }
function Warn($text) { Write-Host "warning: $text" -ForegroundColor Yellow }

$cargoToml = Join-Path $root 'Cargo.toml'
$match = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1
if (-not $match) { throw "could not read version from $cargoToml" }
$version = $match.Matches[0].Groups[1].Value

New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$doWindows = -not $LinuxOnly
$doLinux = -not $WindowsOnly
$linuxFailed = $false

# ---------------------------------------------------------------- Windows ---

if ($doWindows) {
    Say "Building modifile v$version for Windows"
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        throw "cargo not found on PATH. Install Rust from https://rustup.rs"
    }

    if ($CliOnly) {
        cargo build --release --locked -p modifile-cli
    } else {
        cargo build --release --locked
    }
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

    $name = "modifile-v$version-x86_64-windows"
    $stage = Join-Path $OutDir $name
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null

    $release = Join-Path $root 'target\release'
    Copy-Item (Join-Path $release 'modifile.exe') $stage
    if (-not $CliOnly) { Copy-Item (Join-Path $release 'modifile-gui.exe') $stage }
    Copy-Item (Join-Path $root 'README.md') $stage
    $license = Join-Path $root 'LICENSE'
    if (Test-Path $license) { Copy-Item $license $stage }

    $zip = Join-Path $OutDir "$name.zip"
    if (Test-Path $zip) { Remove-Item -Force $zip }
    Say "Packaging $name.zip"
    Compress-Archive -Path $stage -DestinationPath $zip
    Remove-Item -Recurse -Force $stage
}

# ------------------------------------------------------ Linux, through WSL ---

if ($doLinux) {
    $env:WSL_UTF8 = '1'
    $wsl = Get-Command wsl.exe -ErrorAction SilentlyContinue

    if (-not $wsl) {
        Warn "WSL not found, skipping the Linux build."
        Warn "Install it with 'wsl --install', or push a v$version tag and let"
        Warn ".github\workflows\release.yml build Linux on a GitHub runner."
        $linuxFailed = $true
    } else {
        $distroArgs = @()
        if ($Distro) { $distroArgs = @('-d', $Distro) }

        # A relative path would resolve against WSL's own cwd, not this one.
        $wslRoot = (& wsl.exe @distroArgs wslpath -a ($root -replace '\\', '/')) | Select-Object -First 1
        $wslRoot = $wslRoot.Trim()
        if (-not $wslRoot) { throw "could not translate $root into a WSL path" }

        # Cargo writes thousands of small files; on /mnt/c that is painfully
        # slow, so the target directory lives on the Linux filesystem.
        $cmd = "cd '$wslRoot' && bash ./build_all.sh --out '$wslRoot/dist'"
        $cmd += ' --target-dir "$HOME/.cache/modifile-target"'
        if ($Bootstrap) { $cmd += ' --bootstrap' }
        if ($CliOnly) { $cmd += ' --cli-only' }

        Say "Building modifile v$version for Linux in WSL"
        if ($Bootstrap) { Write-Host "(--bootstrap will ask for your WSL sudo password)" }

        & wsl.exe @distroArgs -e bash -lc $cmd
        if ($LASTEXITCODE -ne 0) {
            $linuxFailed = $true
            Warn "The WSL build failed (exit $LASTEXITCODE)."
            Warn "First time here? Run: .\build_all.ps1 -LinuxOnly -Bootstrap"
        }
    }
}

# -------------------------------------------------------------- Checksums ---

$archives = Get-ChildItem -Path $OutDir -File |
    Where-Object { $_.Name -like "modifile-v$version-*" }

if ($archives) {
    # sha256sum's own format, so `sha256sum -c` works on the Linux side.
    $lines = foreach ($a in $archives) {
        $hash = (Get-FileHash -Algorithm SHA256 $a.FullName).Hash.ToLower()
        "$hash  $($a.Name)"
    }
    $sums = Join-Path $OutDir 'SHA256SUMS.txt'
    Set-Content -Path $sums -Value $lines -Encoding ascii

    Say "Archives in $OutDir"
    $lines | ForEach-Object { Write-Host "  $_" }
}

if ($linuxFailed) {
    Write-Host ""
    Warn "Linux archive was not produced."
    exit 1
}
