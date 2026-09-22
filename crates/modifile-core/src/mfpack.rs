//! `.mfpack` — Modifile's modpack format.
//!
//! A `.mfpack` is a zip archive holding an index and, optionally, the config
//! files the pack's author tuned:
//!
//! ```text
//! mfpack.json                          the index: what game, what loader, which mods
//! overrides/<target>/config/foo.cfg    config files, one tree per target id
//! overrides/<target>/config/bar.json
//! ```
//!
//! **Why a zip and not one JSON file.** Shared setups used to be a single JSON
//! document, which was ideal for pasting into Discord and hopeless as a
//! modpack. Configs went in as text, and anything that was not valid UTF-8 had
//! to be base64'd — a third larger, unreadable, and undiffable. A modpack's
//! `config/` tree runs to hundreds of files and megabytes, and some of them
//! are binary. In a zip they are just files: byte-exact, compressed, and
//! openable by anyone with an archive manager, which matters for a format
//! whose whole purpose is that other people receive it.
//!
//! That JSON format is gone rather than deprecated. It is recognised on the
//! way in only so it can be refused by name instead of by parse error — see
//! `Engine::read_shared`.
//!
//! **What it deliberately does not contain** is the mods themselves. The same
//! rule the profile bundle follows, for the same reason: the importer resolves
//! every mod from its own source on their own machine, verifies the download
//! against that source's published digest, and runs the trust ladder locally.
//! So an `.mfpack` from a stranger can waste your time, but it cannot hand you
//! a binary nobody else can see. That is the property that would be lost by
//! shipping jars inside the archive, and it is worth more than the
//! convenience.
//!
//! The index is a [`Bundle`], which is the contents of a shared setup with no
//! opinion about the file it travels in. Keeping the two apart is what made
//! replacing the container straightforward, and everything downstream —
//! `to_profile`, `write_configs`, the trust ladder — was untouched by it.

use std::collections::BTreeMap;
use std::io::{Read, Seek, Write};
use std::path::Path;

use crate::error::{Context, Error, Result};
use crate::share::{Bundle, ConfigFile};

/// Bumped only for a change an older reader would misread. A new optional
/// field does not need it — `serde(default)` covers that.
pub const MFPACK_VERSION: u32 = 1;

pub const INDEX_NAME: &str = "mfpack.json";
pub const OVERRIDES_DIR: &str = "overrides";
pub const EXTENSION: &str = "mfpack";

/// Refuse an archive whose contents do not fit in memory.
///
/// A `.mfpack` holds text configs, so a real one is kilobytes to a few
/// megabytes. These caps are what stops a hand-made archive that claims to be
/// small and expands to fill the disk — the decompressed size is checked as it
/// is read, not trusted from the header.
const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;

/// Write a pack.
///
/// Configs are stored under `overrides/<target>/…` exactly as they sit in the
/// profile, so unzipping one gives a tree a person can read.
pub fn write(path: &Path, bundle: &Bundle) -> Result<()> {
    let file = std::fs::File::create(path).ctx(format!("writing {}", path.display()))?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let options: zip::write::SimpleFileOptions =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    let mut index = bundle.clone();
    index.mfpack = MFPACK_VERSION;
    // The configs travel as real files in the archive, so carrying them in the
    // index as well would store everything twice.
    index.configs = BTreeMap::new();

    zip.start_file(INDEX_NAME, options)?;
    zip.write_all(&serde_json::to_vec_pretty(&index)?)?;

    for (target, files) in &bundle.configs {
        for (rel, config) in files {
            // Written with forward slashes: the zip spec says so, and an
            // archive made on Windows has to open on Linux.
            let name = format!("{OVERRIDES_DIR}/{target}/{}", rel.replace('\\', "/"));
            zip.start_file(name, options)?;
            zip.write_all(&config.bytes()?)?;
        }
    }

    zip.finish()?;
    Ok(())
}

/// Read a pack.
///
/// Every path inside is treated as hostile until proven otherwise: an
/// `.mfpack` is a file from someone else, and the paths in it decide where
/// bytes land.
pub fn read(path: &Path) -> Result<Bundle> {
    let file = std::fs::File::open(path).ctx(format!("opening {}", path.display()))?;
    read_from(std::io::BufReader::new(file), &path.display().to_string())
}

pub fn read_from<R: Read + Seek>(source: R, what: &str) -> Result<Bundle> {
    let mut zip = zip::ZipArchive::new(source)?;
    if zip.len() > MAX_ENTRIES {
        return Err(Error::other(format!(
            "{what} holds {} entries, which is not a modpack",
            zip.len()
        )));
    }

    let mut index: Option<Bundle> = None;
    let mut configs: BTreeMap<String, BTreeMap<String, ConfigFile>> = BTreeMap::new();
    let mut total: u64 = 0;

    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");

        let mut bytes = Vec::new();
        // Read through a limited reader rather than trusting the header's
        // declared size, which an attacker writes.
        let read = std::io::copy(
            &mut (&mut entry).take(MAX_ENTRY_BYTES + 1),
            &mut bytes,
        )?;
        if read > MAX_ENTRY_BYTES {
            return Err(Error::other(format!("{what}: `{name}` is too large to be a config")));
        }
        total += read;
        if total > MAX_TOTAL_BYTES {
            return Err(Error::other(format!("{what} unpacks to more than it should")));
        }

        if name == INDEX_NAME {
            index = Some(serde_json::from_slice(&bytes).map_err(|e| {
                Error::other(format!("{what}: its {INDEX_NAME} is not readable: {e}"))
            })?);
            continue;
        }

        let Some(rest) = name.strip_prefix(&format!("{OVERRIDES_DIR}/")) else {
            // Anything else is not part of the format. Ignored rather than
            // refused, so a pack carrying a README still opens.
            continue;
        };
        // `overrides/<target>/<path…>`
        let Some((target, rel)) = rest.split_once('/') else {
            continue;
        };
        if target.is_empty() || crate::share::safe_relative(rel).is_none() {
            continue;
        }

        configs
            .entry(target.to_string())
            .or_default()
            .insert(rel.to_string(), ConfigFile::from_bytes(bytes));
    }

    let Some(mut bundle) = index else {
        return Err(Error::other(format!(
            "{what} has no {INDEX_NAME}, so it is not a Modifile pack. If it came from \
             CurseForge, Modrinth or Thunderstore, use `pack add` instead."
        )));
    };
    if bundle.mfpack > MFPACK_VERSION {
        return Err(Error::other(format!(
            "{what} was written by a newer Modifile (pack format {}, this build reads {MFPACK_VERSION}). \
             Update Modifile and open it again.",
            bundle.mfpack
        )));
    }
    bundle.configs = configs;
    Ok(bundle)
}

/// Whether a file is a zip, and so possibly a pack.
///
/// Decided by the first bytes, not the extension: a pack renamed on the way
/// through a chat client is still a pack, and something called `.mfpack` that
/// is not a zip is not one.
pub fn looks_like_pack(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 4];
    match file.read_exact(&mut magic) {
        // "PK\x03\x04" — a local file header, the start of every zip.
        Ok(()) => magic == [0x50, 0x4b, 0x03, 0x04],
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("modifile-mfpack-{tag}-{}", crate::paths::now_millis()));
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn sample() -> Bundle {
        let mut files = BTreeMap::new();
        files.insert(
            "config/valheim_plus.cfg".to_string(),
            ConfigFile::Text {
                text: "[Server]\nenabled = true\n".to_string(),
            },
        );
        files.insert(
            "config/cache.dat".to_string(),
            ConfigFile::from_bytes(vec![0xff, 0xfe, 0x00, 0x80]),
        );
        let mut configs = BTreeMap::new();
        configs.insert("client".to_string(), files);

        Bundle {
            mfpack: MFPACK_VERSION,
            name: "hardcore".into(),
            game: "valheim".into(),
            description: "a pack".into(),
            targets: vec!["client".into()],
            game_version: Some("0.217.46".into()),
            loader: Some("bepinex".into()),
            mods: Vec::new(),
            configs,
            exported_by: "modifile test".into(),
        }
    }

    #[test]
    fn a_pack_round_trips() {
        let dir = scratch("round-trip");
        let path = dir.join("hardcore.mfpack");
        let original = sample();
        write(&path, &original).expect("write");

        assert!(looks_like_pack(&path), "a pack must be a zip");

        let back = read(&path).expect("read");
        assert_eq!(back.name, "hardcore");
        assert_eq!(back.game, "valheim");
        // The reason the format carries these at all.
        assert_eq!(back.game_version.as_deref(), Some("0.217.46"));
        assert_eq!(back.loader.as_deref(), Some("bepinex"));
        assert_eq!(back.config_count(), 2);

        let client = &back.configs["client"];
        assert_eq!(
            client["config/valheim_plus.cfg"].bytes().unwrap(),
            b"[Server]\nenabled = true\n"
        );
        // Binary survives byte for byte, which is the point of using a zip.
        assert_eq!(
            client["config/cache.dat"].bytes().unwrap(),
            vec![0xff, 0xfe, 0x00, 0x80]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Configs are stored as plain files, so an author can unzip a pack, edit
    /// it by hand and zip it back up.
    #[test]
    fn the_layout_is_what_the_docs_say() {
        let dir = scratch("layout");
        let path = dir.join("p.mfpack");
        write(&path, &sample()).expect("write");

        let file = std::fs::File::open(&path).expect("open");
        let mut zip = zip::ZipArchive::new(file).expect("zip");
        let mut names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "mfpack.json".to_string(),
                "overrides/client/config/cache.dat".to_string(),
                "overrides/client/config/valheim_plus.cfg".to_string(),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pack is a file from a stranger, and the paths in it say where bytes
    /// go. Anything climbing out is dropped, not normalised.
    #[test]
    fn hostile_paths_are_dropped() {
        let dir = scratch("hostile");
        let path = dir.join("evil.mfpack");

        let file = std::fs::File::create(&path).expect("create");
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file(INDEX_NAME, options).expect("index");
        zip.write_all(&serde_json::to_vec(&sample()).unwrap())
            .expect("write index");
        for evil in [
            "overrides/client/../../../../etc/passwd",
            "overrides/client/C:/Windows/evil.dll",
        ] {
            zip.start_file(evil, options).expect("entry");
            zip.write_all(b"pwned").expect("write");
        }
        zip.start_file("overrides/client/config/ok.cfg", options)
            .expect("entry");
        zip.write_all(b"fine").expect("write");
        zip.finish().expect("finish");

        let back = read(&path).expect("read");
        let client = &back.configs["client"];
        assert_eq!(client.len(), 1, "only the safe path should survive");
        assert!(client.contains_key("config/ok.cfg"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_newer_pack_format_is_refused_rather_than_misread() {
        let dir = scratch("newer");
        let path = dir.join("future.mfpack");

        // Built by hand, because `write` stamps the version this build
        // speaks — as it should. A pack from the future can only come from a
        // future Modifile, so the only way to test reading one is to forge it.
        let mut future = sample();
        future.mfpack = MFPACK_VERSION + 1;
        let file = std::fs::File::create(&path).expect("create");
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(INDEX_NAME, zip::write::SimpleFileOptions::default())
            .expect("index");
        zip.write_all(&serde_json::to_vec(&future).unwrap())
            .expect("write index");
        zip.finish().expect("finish");

        let error = read(&path).expect_err("must refuse");
        assert!(
            error.to_string().contains("newer Modifile"),
            "unhelpful message: {error}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The two importers read different formats, and the index filename is
    /// what tells them apart. `import_modpack` keys its "wrong door" message
    /// off this exact name, so it must not drift.
    #[test]
    fn the_index_is_the_marker_that_identifies_one_of_ours() {
        assert_eq!(INDEX_NAME, "mfpack.json");
        assert_eq!(EXTENSION, "mfpack");

        let dir = scratch("marker");
        let path = dir.join("p.mfpack");
        write(&path, &sample()).expect("write");

        let file = std::fs::File::open(&path).expect("open");
        let mut zip = zip::ZipArchive::new(file).expect("zip");
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == INDEX_NAME),
            "a pack must carry {INDEX_NAME} at the root, found {names:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_zip_that_is_not_a_pack_says_so() {
        let dir = scratch("not-a-pack");
        let path = dir.join("random.zip");
        let file = std::fs::File::create(&path).expect("create");
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("readme.txt", zip::write::SimpleFileOptions::default())
            .expect("entry");
        zip.write_all(b"hello").expect("write");
        zip.finish().expect("finish");

        let error = read(&path).expect_err("must refuse");
        assert!(error.to_string().contains("pack add"), "unhelpful: {error}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
