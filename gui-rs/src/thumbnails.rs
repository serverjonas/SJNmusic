//! Async thumbnail fetcher. Downloads via `ureq`, decodes via the `image`
//! crate, and posts a `ColorImage` back to the UI thread through an mpsc
//! channel so the `egui::Context` can register the texture. This is the
//! one part of the codebase that mandates off-thread work — the rest of
//! the GUI just calls `DaemonClient` directly.

#![allow(dead_code)] // silenced for compile: `ImageMessage::Failed`'s
                      // `url` and `error` are intentionally retained so
                      // future error-toast / fallback logic can read
                      // them without re-plumbing the channel payload.

use std::io::Read;
use std::sync::mpsc::Sender;

use egui::ColorImage;

/// One in-flight or completed thumbnail.
#[derive(Clone, Debug)]
pub enum ImageMessage {
    Loaded {
        url: String,
        image: ColorImage,
    },
    Failed {
        url: String,
        error: String,
    },
}

/// Spawn one detached worker per image URL. The worker downloads the
/// bytes, decodes them (PNG/JPEG/WebP), and emits an `ImageMessage`
/// back to the supplied channel.
///
/// Never panics: every failure path turns into an `ImageMessage::Failed`
/// so the UI thread can clear its "loading" placeholder instead of
/// holding a spinner forever.
pub fn spawn_fetch(url: String, tx: Sender<ImageMessage>) {
    std::thread::Builder::new()
        .name(format!("sjnmusic-thumb"))
        .spawn(move || run_fetch(url, tx))
        .expect("failed to spawn thumbnail worker");
}

fn run_fetch(url: String, tx: Sender<ImageMessage>) {
    let response = match ureq::get(&url).call() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(ImageMessage::Failed {
                url,
                error: format!("download: {}", e),
            });
            return;
        }
    };
    // `into_reader()` on `Response` returns a boxed `Read` trait object;
    // calling `bytes()` on a trait object needs `Self: Sized`, so we use
    // `read_to_end` instead which works on any `dyn Read`.
    let mut bytes = Vec::new();
    if let Err(e) = response.into_reader().read_to_end(&mut bytes) {
        let _ = tx.send(ImageMessage::Failed {
            url,
            error: format!("read: {}", e),
        });
        return;
    }
    let dyn_img = match image::load_from_memory(&bytes) {
        Ok(i) => i,
        Err(e) => {
            let _ = tx.send(ImageMessage::Failed {
                url,
                error: format!("decode: {}", e),
            });
            return;
        }
    };
    let rgba = dyn_img.to_rgba8();
    let width = rgba.width() as usize;
    let height = rgba.height() as usize;
    let color_image = ColorImage::from_rgba_unmultiplied([width, height], &rgba.into_raw());
    let _ = tx.send(ImageMessage::Loaded {
        url,
        image: color_image,
    });
}
