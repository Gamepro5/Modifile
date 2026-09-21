//! Embeds the application icon into the Windows executable.
//!
//! This is what Explorer, the Start menu and a pinned shortcut read. The
//! running window's icon is a separate thing that the program sets for itself
//! at startup — see `icon()` in `main.rs` — and both are needed: neither one
//! covers the other's case.
//!
//! Two different notions of "windows" are in play here, and conflating them is
//! what broke the Linux build:
//!
//! * `winresource` is a build-dependency declared under `cfg(windows)`. For
//!   build-dependencies Cargo resolves that against the **host**, so on a
//!   Linux runner the crate is simply absent. A `if target_os == "windows"`
//!   check does not help — that runs after this file has already been
//!   compiled, and compiling it needs the crate to exist. Hence `#[cfg]` on
//!   the function itself, which is evaluated for the host and so agrees with
//!   the manifest.
//! * `CARGO_CFG_TARGET_OS` is the **target**, and is checked below so that
//!   cross-compiling from Windows to Linux does not hand a Windows resource
//!   to a Linux linker.
//!
//! Release builds both artifacts natively — Windows on a Windows runner, Linux
//! on a Linux one — so nothing is lost by keying the icon to the host.

fn main() {
    println!("cargo:rerun-if-changed=assets/modifile.ico");
    embed_icon();
}

#[cfg(windows)]
fn embed_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/modifile.ico");
    if let Err(error) = res.compile() {
        // Not fatal. Embedding needs a resource compiler from the Windows SDK,
        // and a machine without one should still get a working binary — just
        // one wearing the default icon. Failing the build over decoration
        // would be the wrong trade.
        println!("cargo:warning=could not embed the application icon: {error}");
    }
}

/// Nothing to do: an ELF binary has no resource table, and the window icon is
/// set at runtime from the bundled PNG.
#[cfg(not(windows))]
fn embed_icon() {}
