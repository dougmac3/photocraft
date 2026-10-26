//! PhotoCraft as a desk inside Fossa Direct's window: in-process, drawn in a rect of Fossa's own
//! egui window, never a window of its own.
//!
//! The same contract as the web bridge (apps/photocraft-web/src/fossa_bridge.rs): Fossa hands
//! files in ([`Desk::open`], as a drop would), and Save and Export hand them back
//! ([`Desk::take_saved`]) instead of writing to disk. [`crate::menus::FOSSA_HOSTED`] is set, so
//! the File menu is Fossa's and window commands are dropped (`menus::viewport_cmd`).
//!
//! While the desk runs it has its own style, theme and input options (swapped in and out around
//! each frame, so Fossa's look is left as it was), its fonts are added beside Fossa's
//! (`theme::install_fonts_with`), and keys reach it only while the pointer is over the desk or a
//! field of its has the keyboard.

use std::sync::atomic::Ordering;

use eframe::egui_wgpu;
use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_engine::Session;

use crate::{FileDialogAnswer, FileDialogRequest, Inbox, PhotocraftApp, Services};

pub struct Desk {
    app: PhotocraftApp,
    /// Files from Fossa, opened by the app's next frame.
    inbox: Inbox,
    /// Files saved or exported since the last [`Desk::take_saved`].
    saved: Inbox,
    ctx: egui::Context,
    /// PhotoCraft's own style and options, kept between frames.
    options: Option<egui::Options>,
}

/// The parts of [`egui::Options`] PhotoCraft sets for itself (`PhotocraftApp::setup_context`,
/// `theme::apply`, `wheel_nav::configure`).
fn adopt(to: &mut egui::Options, from: &egui::Options) {
    to.dark_style = from.dark_style.clone();
    to.light_style = from.light_style.clone();
    to.theme_preference = from.theme_preference;
    to.fallback_theme = from.fallback_theme;
    to.zoom_with_keyboard = from.zoom_with_keyboard;
    to.input_options = from.input_options.clone();
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(|| "photocraft".into(), |n| n.to_string_lossy().to_string())
}

/// Keys and clipboard events: what a desk the pointer isn't over must not act on.
fn is_key_event(e: &egui::Event) -> bool {
    matches!(e, egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) | egui::Event::Ime(_))
}

impl Desk {
    /// `render_state` is the host window's wgpu state (eframe::Frame::wgpu_render_state()), for the GPU canvas.
    pub fn new(ctx: &egui::Context, render_state: Option<&egui_wgpu::RenderState>) -> Self {
        crate::menus::FOSSA_HOSTED.store(true, Ordering::Relaxed);
        let inbox = Inbox::default();
        let saved = Inbox::default();
        let mut app = PhotocraftApp::new(Session::new(), services(inbox.clone(), saved.clone()));
        if let Some(rs) = render_state {
            app.set_wgpu(rs.clone());
        }
        Self { app, inbox, saved, ctx: ctx.clone(), options: None }
    }

    /// Open a file Fossa hands in (a PNG/JPEG frame, a project file), as a drop would.
    pub fn open(&mut self, name: &str, bytes: Vec<u8>) {
        self.inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name.to_string(), bytes));
        self.ctx.request_repaint();
    }

    /// Files the editor saved or exported since the last call, (file name, bytes).
    pub fn take_saved(&mut self) -> Vec<(String, Vec<u8>)> {
        std::mem::take(&mut *self.saved.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// The active document flattened to a PNG, at the document's size.
    pub fn grab_png(&mut self) -> Result<Vec<u8>, String> {
        let st = self.app.session.active().ok_or("no document is open")?;
        photocraft_io::export(&st.doc, "frame.png", &photocraft_io::ExportOptions::default()).map(|r| r.bytes).map_err(|e| e.to_string())
    }

    /// Draw and run the editor inside `ui`'s rect of the host window, one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let host = ctx.options(|o| o.clone());
        let embed = ctx.embed_viewports();
        if let Some(own) = &self.options {
            ctx.options_mut(|o| adopt(o, own));
        }
        // PhotoCraft's few extra windows (Camera Raw, New Window) open inside this one.
        ctx.set_embed_viewports(true);
        let over = ctx.pointer_hover_pos().is_some_and(|p| rect.contains(p));
        let focused = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id)).is_some_and(|r| rect.intersects(r.rect));
        let held: Vec<egui::Event> = if over || focused {
            Vec::new()
        } else {
            ctx.input_mut(|i| {
                let (keys, rest) = std::mem::take(&mut i.events).into_iter().partition(is_key_event);
                i.events = rest;
                keys
            })
        };
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).id_salt("photocraft"), |ui| {
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            eframe::App::logic(&mut self.app, &ctx, frame);
            ui.set_style(ctx.global_style());
            eframe::App::ui(&mut self.app, ui, frame);
        });
        if !held.is_empty() {
            ctx.input_mut(|i| i.events.extend(held));
        }
        let mut own = ctx.options(|o| o.clone());
        // Fossa's window follows Fossa's theme, never the desk's.
        own.sync_window_theme = false;
        self.options = Some(own);
        ctx.options_mut(|o| adopt(o, &host));
        ctx.set_embed_viewports(embed);
    }
}

/// The desk's services: files come from Fossa and go back to it; nothing reads or writes the disk.
fn services(inbox: Inbox, saved: Inbox) -> Services {
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
            photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|doc: &photocraft_doc::Document, path: &str, settings: &crate::ExportSettings| {
            photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        file_dialog: Some(Box::new(|request, _parent, reply| match request {
            // No save dialog: the suggested name is the name the file goes back to Fossa under.
            FileDialogRequest::Save { suggested } => reply.send(Some(FileDialogAnswer::SaveTo(file_name(&suggested)))),
            // Files come from Fossa (`Desk::open`), not from a dialog of PhotoCraft's own.
            _ => reply.send(None),
        })),
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            saved.lock().unwrap_or_else(|e| e.into_inner()).push((file_name(path), bytes.to_vec()));
            Ok(())
        })),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        inbox: Some(inbox),
        ..Default::default()
    }
}

/// An offscreen picture of the desk in a host window (no window on the desktop):
/// `FOSSA_DESK_SHOT=shot.png cargo test --release -p photocraft-ui-egui fossa_desk_shot -- --ignored`.
#[cfg(test)]
mod tests {
    use super::*;

    struct Host {
        desk: Option<Desk>,
        png: Option<Vec<u8>>,
    }

    impl eframe::App for Host {
        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let all = ui.max_rect();
            let strip = egui::Rect::from_min_size(all.min, egui::vec2(all.width(), 40.0));
            ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(30, 24, 48));
            ui.painter().text(strip.left_center() + egui::vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, "Fossa host", egui::FontId::proportional(16.0), egui::Color32::WHITE);
            let desk = self.desk.get_or_insert_with(|| {
                let mut d = Desk::new(ui.ctx(), frame.wgpu_render_state());
                if let Some(png) = self.png.take() {
                    d.open("from Fossa.png", png);
                }
                d
            });
            let inner = egui::Rect::from_min_max(egui::pos2(all.left(), strip.bottom()), all.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(inner).id_salt("desk_paint"), |ui| desk.ui(ui, frame));
        }
    }

    fn test_png(w: u32, h: u32) -> Vec<u8> {
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                rgba.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 160, 255]);
            }
        }
        let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba).unwrap();
        photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).unwrap()
    }

    #[test]
    #[ignore]
    fn fossa_desk_shot() {
        let png = test_png(1280, 720);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1600.0, 1000.0))
            .wgpu()
            // PhotoCraft's own accessibility is tested by its own suites, not this picture.
            .with_accessibility_check(false)
            .build_eframe(move |_cc| Host { desk: None, png: Some(png) });
        for _ in 0..20 {
            h.run_steps(2);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if let Ok(path) = std::env::var("FOSSA_DESK_SHOT") {
            match h.render() {
                Ok(img) => img.save(&path).unwrap(),
                Err(e) => panic!("render: {e}"),
            }
        }
        let desk = h.state_mut().desk.as_mut().unwrap();
        let out = desk.grab_png().unwrap();
        let img = photocraft_codecs::decode_as(photocraft_codecs::Format::Png, &out).unwrap();
        assert_eq!((img.width(), img.height()), (1280, 720));
        assert!(crate::menus::fossa_hosted());
    }
}
