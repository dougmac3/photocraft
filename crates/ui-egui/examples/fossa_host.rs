//! A stand-in for Fossa Direct's window: a host strip on top and PhotoCraft's desk in the rest,
//! drawn in-process in one eframe window (`photocraft_ui_egui::fossa_native`).
//!
//! `cargo run --release -p photocraft-ui-egui --example fossa_host [-- image.png]`
//! "Add to shot" grabs the document as a PNG; files the desk saves are listed in the strip.

use photocraft_ui_egui::fossa_native::Desk;

struct Host {
    desk: Option<Desk>,
    file: Option<(String, Vec<u8>)>,
    said: String,
}

impl eframe::App for Host {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let all = ui.max_rect();
        let strip = egui::Rect::from_min_size(all.min, egui::vec2(all.width(), 44.0));
        ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(28, 22, 44));
        let desk = self.desk.get_or_insert_with(|| {
            let mut d = Desk::new(ui.ctx(), frame.wgpu_render_state());
            if let Some((name, bytes)) = self.file.take() {
                d.open(&name, bytes);
            }
            d
        });
        for (name, bytes) in desk.take_saved() {
            self.said = format!("saved {name} ({} bytes)", bytes.len());
        }
        ui.scope_builder(egui::UiBuilder::new().max_rect(strip.shrink2(egui::vec2(12.0, 6.0))), |ui| {
            ui.horizontal_centered(|ui| {
                ui.label("Fossa host");
                if ui.button("Add to shot").clicked() {
                    self.said = match desk.grab_png() {
                        Ok(png) => format!("grabbed a {} byte PNG", png.len()),
                        Err(e) => e,
                    };
                }
                ui.label(&self.said);
            });
        });
        let inner = egui::Rect::from_min_max(egui::pos2(all.left(), strip.bottom()), all.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(inner).id_salt("desk_paint"), |ui| desk.ui(ui, frame));
    }
}

fn main() -> eframe::Result {
    let file = std::env::args().nth(1).and_then(|p| {
        let name = std::path::Path::new(&p).file_name()?.to_string_lossy().to_string();
        Some((name, std::fs::read(&p).ok()?))
    });
    let options = eframe::NativeOptions { viewport: egui::ViewportBuilder::default().with_inner_size([1600.0, 1000.0]), ..Default::default() };
    eframe::run_native("Fossa host", options, Box::new(|_cc| Ok(Box::new(Host { desk: None, file, said: String::new() }))))
}
