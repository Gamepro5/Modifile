//! Embeds the application icon into the Windows executable.
//!
//! This is what Explorer, the Start menu and a pinned shortcut read. The
//! running window's icon is a separate thing that the program sets for itself
//! at startup — see `icon()` in `main.rs` — and both are needed: neither one
//! covers the other's case.

fn main() {
    println!("cargo:rerun-if-changed=assets/modifile.ico");

    // The *target's* OS, not the host's. A build script runs on the host, so
    // `cfg!(windows)` here would answer the wrong question when cross
    // compiling.
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
