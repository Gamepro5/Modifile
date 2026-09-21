//! Artwork: mod icons, screenshots and game box art.
//!
//! Three rules shape this, in order:
//!
//! 1. **Nothing blocks the window.** A fetch and a decode happen on a worker
//!    thread; the UI asks for a texture, gets `None` the first time, and draws
//!    a generated tile until the real one arrives.
//! 2. **Nothing is fetched twice.** Images land in a content-addressed folder
//!    under the cache directory, so art survives restarts and a second look at
//!    the same mod costs nothing.
//! 3. **It can be switched off.** Artwork is the only thing in Modifile that
//!    reaches the network just to look nice, so it gets a switch — and turning
//!    it off frees the textures rather than merely hiding them.
//!
//! A mod with no art is not a failure. Every index has projects with no icon,
//! and a generated tile — a colour derived from the id, with its initials — is
//! a better answer than an empty square or a broken-image glyph.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use eframe::egui;

use crate::theme;

/// Decoded art on its way back from a worker thread.
pub struct Loaded {
    pub url: String,
    /// `None` when the fetch or the decode failed. Recorded either way, so a
    /// broken URL is not retried on every frame.
    pub image: Option<egui::ColorImage>,
}

enum Slot {
    Loading,
    Ready(egui::TextureHandle),
    /// Tried, and there is nothing there. Drawn as a generated tile.
    Failed,
}

/// How much decoded artwork to keep resident.
///
/// Textures live in video memory, and a long browse through a few hundred mods
/// would otherwise grow without limit. Bounded by count rather than bytes
/// because every image here is a thumbnail of roughly known size, and counting
/// bytes would mean tracking what the GPU actually allocated.
const MAX_RESIDENT: usize = 256;

pub struct Art {
    dir: PathBuf,
    slots: HashMap<String, Slot>,
    /// Insertion order, for evicting the least recently asked-for.
    order: Vec<String>,
    tx: Sender<crate::Msg>,
    runtime: tokio::runtime::Handle,
    http: modifile_core::http::Http,
    enabled: bool,
    /// Modifile's own icon. Bundled, so it is not part of the LRU above and
    /// is never evicted.
    logo: Option<egui::TextureHandle>,
}

impl Art {
    pub fn new(
        cache_dir: PathBuf,
        tx: Sender<crate::Msg>,
        runtime: tokio::runtime::Handle,
        http: modifile_core::http::Http,
        enabled: bool,
    ) -> Self {
        let dir = cache_dir.join("art");
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            slots: HashMap::new(),
            order: Vec::new(),
            tx,
            runtime,
            http,
            enabled,
            logo: None,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Modifile's own icon, as a texture.
    ///
    /// Bundled rather than fetched: this one *is* ours, unlike the game art,
    /// and the app should not need a network to show its own logo. For the
    /// same reason it ignores the artwork setting — that exists to stop
    /// downloads and free hundreds of megabytes of remote art, and this is
    /// 64 KB that was already in the binary.
    ///
    /// Held here because `load_texture` uploads afresh every call, and this is
    /// drawn every frame.
    pub fn logo(&mut self, ctx: &egui::Context) -> Option<egui::TextureHandle> {
        if self.logo.is_none() {
            self.logo = decode(include_bytes!("../assets/modifile-128.png"))
                .map(|image| ctx.load_texture("modifile-logo", image, egui::TextureOptions::LINEAR));
        }
        self.logo.clone()
    }

    /// Turn artwork on or off.
    ///
    /// Switching it off drops every texture immediately. A setting that only
    /// stopped *new* fetches would leave a few hundred megabytes resident for
    /// the rest of the session, which is exactly what someone turning it off
    /// is trying to avoid.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        if !on {
            self.slots.clear();
            self.order.clear();
        }
    }

    /// How many images are resident right now.
    pub fn resident(&self) -> usize {
        self.slots
            .values()
            .filter(|s| matches!(s, Slot::Ready(_)))
            .count()
    }

    /// Delete the on-disk art cache.
    pub fn clear_disk(&self) -> std::io::Result<u64> {
        let mut freed = 0;
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                if let Ok(meta) = entry.metadata() {
                    freed += meta.len();
                }
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(freed)
    }

    pub fn disk_bytes(&self) -> u64 {
        std::fs::read_dir(&self.dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| e.metadata().ok())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    fn path_for(&self, url: &str) -> PathBuf {
        self.dir
            .join(modifile_core::hash::sha256_bytes(url.as_bytes()))
    }

    /// The texture for a URL, starting a fetch if this is the first ask.
    ///
    /// `None` means "not yet, or never" — the caller draws a placeholder. It
    /// deliberately does not distinguish the two: both mean the same thing to
    /// whoever is drawing.
    pub fn texture(&mut self, ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        if !self.enabled || url.is_empty() {
            return None;
        }

        if let Some(slot) = self.slots.get(url) {
            let found = match slot {
                Slot::Ready(handle) => Some(handle.clone()),
                _ => None,
            };
            self.touch(url);
            return found;
        }

        self.slots.insert(url.to_string(), Slot::Loading);
        self.order.push(url.to_string());
        self.evict();
        self.start(ctx, url.to_string());
        None
    }

    /// Accept a decoded image from a worker thread.
    pub fn accept(&mut self, ctx: &egui::Context, loaded: Loaded) {
        // It may have been evicted, or artwork switched off, while in flight.
        if !self.enabled || !self.slots.contains_key(&loaded.url) {
            return;
        }
        let slot = match loaded.image {
            Some(image) => {
                let handle = ctx.load_texture(&loaded.url, image, egui::TextureOptions::LINEAR);
                Slot::Ready(handle)
            }
            None => Slot::Failed,
        };
        self.slots.insert(loaded.url, slot);
    }

    fn touch(&mut self, url: &str) {
        if let Some(at) = self.order.iter().position(|u| u == url) {
            let url = self.order.remove(at);
            self.order.push(url);
        }
    }

    fn evict(&mut self) {
        while self.order.len() > MAX_RESIDENT {
            let oldest = self.order.remove(0);
            self.slots.remove(&oldest);
        }
    }

    /// Fetch and decode on a worker thread.
    fn start(&self, ctx: &egui::Context, url: String) {
        let path = self.path_for(&url);
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        let http = self.http.clone();
        let runtime = self.runtime.clone();

        std::thread::spawn(move || {
            // A pack may name a file on disk instead of a URL. Read it every
            // time rather than through the cache: the point of pointing at a
            // local file is to be able to replace it and see the new one,
            // which a cache keyed on the unchanged path would prevent. It is
            // also the answer when one of the linked URLs eventually rots —
            // art becomes fixable without waiting for a release.
            let bytes = if is_local(&url) {
                std::fs::read(&url).ok().filter(|b| !b.is_empty())
            } else {
                // The cache is content-addressed by URL, so a hit needs no
                // network.
                match std::fs::read(&path) {
                    Ok(bytes) if !bytes.is_empty() => Some(bytes),
                    _ => runtime.block_on(async {
                        match http.get_bytes(&url).await {
                            Ok(Some(bytes)) => {
                                let _ = modifile_core::paths::write_atomic(&path, &bytes);
                                Some(bytes)
                            }
                            _ => None,
                        }
                    }),
                }
            };

            let image = bytes.and_then(|bytes| decode(&bytes));
            let _ = tx.send(crate::Msg::Art(Box::new(Loaded { url, image })));
            ctx.request_repaint();
        });
    }
}

/// Whether a pack's `icon`/`art` names a file on this machine rather than a
/// URL to fetch.
///
/// Matched by what it is *not*: anything without a scheme we would send to the
/// network. `C:\art\wow.png` has what looks like a `c:` scheme, so testing for
/// "has a scheme" would get Windows paths backwards.
fn is_local(source: &str) -> bool {
    let lower = source.to_ascii_lowercase();
    !(lower.starts_with("http://") || lower.starts_with("https://"))
}

/// Decode to an egui image, shrinking anything larger than we will ever draw.
///
/// Box art arrives at 600×900 and screenshots at 1920×1080; a tile is 120px
/// wide. Keeping the full-size texture would cost tens of megabytes of video
/// memory for pixels nothing can see.
fn decode(bytes: &[u8]) -> Option<egui::ColorImage> {
    const MAX_EDGE: u32 = 640;

    let decoded = image::load_from_memory(bytes).ok()?;
    let decoded = if decoded.width() > MAX_EDGE || decoded.height() > MAX_EDGE {
        decoded.thumbnail(MAX_EDGE, MAX_EDGE)
    } else {
        decoded
    };

    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

// ---------------------------------------------------------------------------
// Generated tiles
// ---------------------------------------------------------------------------

/// A stable colour for a name.
///
/// The same mod gets the same colour every time, on every machine, because it
/// is derived from the name rather than from load order. Saturation and
/// lightness are fixed so no tile comes out unreadable against light text.
pub fn tint(seed: &str) -> egui::Color32 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    let hue = (hash % 360) as f32 / 360.0;
    let (r, g, b) = hsl_to_rgb(hue, 0.42, 0.38);
    egui::Color32::from_rgb(r, g, b)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - (((h * 6.0) % 2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h * 6.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

/// Up to two initials for a name, for a tile with no artwork.
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    match words.as_slice() {
        [] => "?".to_string(),
        [one] => one.chars().take(2).collect::<String>().to_uppercase(),
        // Up to three, because two is not enough to tell apart the names that
        // actually collide: "WoW Classic Era" and "WoW Classic (progression)"
        // are both "WC", and a rail of identical tiles is worse than no tile.
        many => many
            .iter()
            .take(3)
            .filter_map(|w| w.chars().next())
            .collect::<String>()
            .to_uppercase(),
    }
}

/// Draw artwork into `rect`, or a generated tile when there is none.
///
/// Images are cropped to fill rather than stretched: box art is portrait,
/// avatars are square, screenshots are wide, and stretching each to the same
/// box makes every one of them look wrong.
pub fn draw(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    texture: Option<&egui::TextureHandle>,
    seed: &str,
    label: &str,
    rounding: u8,
) {
    let painter = ui.painter();
    let radius = egui::CornerRadius::same(rounding);

    match texture {
        Some(handle) => {
            let size = handle.size_vec2();
            let target = rect.width() / rect.height();
            let source = size.x / size.y;
            // Crop the long edge, keeping the middle.
            let uv = if source > target {
                let keep = target / source;
                let inset = (1.0 - keep) / 2.0;
                egui::Rect::from_min_max(
                    egui::pos2(inset, 0.0),
                    egui::pos2(1.0 - inset, 1.0),
                )
            } else {
                let keep = source / target;
                let inset = (1.0 - keep) / 2.0;
                egui::Rect::from_min_max(
                    egui::pos2(0.0, inset),
                    egui::pos2(1.0, 1.0 - inset),
                )
            };
            // A textured rounded rect, not `Shape::image`: that one takes no
            // corner radius, so every square-cornered icon punched out past
            // the rounded tile it was supposed to sit in. The texture is
            // multiplied by the fill, so the fill has to be white.
            painter.add(
                egui::epaint::RectShape::filled(rect, radius, egui::Color32::WHITE)
                    .with_texture(handle.id(), uv),
            );
        }
        None => {
            painter.rect_filled(rect, radius, tint(seed));
            let text = initials(label);
            painter.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                text,
                egui::FontId::proportional((rect.height() * 0.34).clamp(11.0, 30.0)),
                egui::Color32::from_white_alpha(210),
            );
        }
    }

    // A hairline keeps a tile from bleeding into the panel behind it.
    painter.rect_stroke(
        rect,
        radius,
        egui::Stroke::new(1.0, theme::LINE),
        egui::StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_read_as_the_name() {
        assert_eq!(initials("Fabulously Optimized"), "FO");
        assert_eq!(initials("Sodium"), "SO");
        assert_eq!(initials("World of Warcraft"), "WOW");
        // Never panics on input with nothing to take.
        assert_eq!(initials(""), "?");
        assert_eq!(initials("---"), "?");
    }

    #[test]
    fn similar_names_get_different_tiles() {
        // Two bundled WoW packs used to render the same two letters.
        assert_ne!(
            initials("WoW Classic Era"),
            initials("WoW Classic (progression)")
        );
    }

    #[test]
    fn a_name_always_gets_the_same_colour() {
        assert_eq!(tint("minecraft"), tint("minecraft"));
        assert_ne!(tint("minecraft"), tint("valheim"));
    }

    #[test]
    fn local_art_is_told_from_a_url() {
        assert!(!is_local("https://example.test/a.png"));
        assert!(!is_local("HTTPS://example.test/a.png"));
        assert!(!is_local("http://example.test/a.png"));
        // A Windows path leads with what looks like a scheme, which is why
        // this is decided by naming the network schemes rather than by
        // looking for a colon.
        assert!(is_local(r"C:\art\wow.png"));
        assert!(is_local("/home/me/art/wow.png"));
        assert!(is_local("art/wow.png"));
    }
}
