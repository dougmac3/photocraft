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
//! field of its has the keyboard. Its windows, menus, tooltips and dialogs stay inside the rect it
//! is drawn in ([`content_rect`], [`show_modal`], `Desk::fit_areas`), and its context-wide
//! plugins act only on what is inside it ([`owns`]).
//!
//! The agent drives it in-process through the control channel ([`Desk::control`], the methods of
//! `control.rs`), and Fossa can draw one panel of it alone ([`Desk::panel_ui`]) narrowed to a
//! target ([`Desk::focus`]).

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use eframe::egui_wgpu;
use photocraft_codecs::{ChannelLayout, EncodeOptions, Image};
use photocraft_engine::Session;
use serde_json::{Value, json};

use crate::{FileDialogAnswer, FileDialogRequest, Inbox, PhotocraftApp, Services};

thread_local! {
    /// The rect the desk is drawing in, while it draws.
    static DRAWING: Cell<Option<egui::Rect>> = const { Cell::new(None) };
    /// The panel [`Desk::panel_ui`] is drawing alone, while it draws.
    static SOLO: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The area PhotoCraft lays its windows, dialogs and menus out in: the desk's rect while a desk
/// draws, the whole window otherwise. Use it wherever PhotoCraft would read `ctx.content_rect()`.
pub fn content_rect(ctx: &egui::Context) -> egui::Rect {
    DRAWING.with(Cell::get).unwrap_or_else(|| ctx.content_rect())
}

/// The panel drawn alone this frame ([`Desk::panel_ui`]), if any.
pub(crate) fn solo_panel() -> Option<String> {
    SOLO.with(|s| s.borrow().clone())
}

fn rects_id() -> egui::Id {
    egui::Id::new("photocraft-desk-rects")
}

/// Whether PhotoCraft's context-wide plugins (text field menu, single-line drag, desk input)
/// may act at `pos`: always in PhotoCraft's own window; inside Fossa only over a rect a desk drew
/// in this pass or the last one, never over Fossa's own widgets.
pub(crate) fn owns(ctx: &egui::Context, pos: egui::Pos2) -> bool {
    if !crate::menus::fossa_hosted() {
        return true;
    }
    let pass = ctx.cumulative_pass_nr();
    ctx.data(|d| d.get_temp::<(u64, Vec<egui::Rect>)>(rects_id()))
        .is_some_and(|(at, rects)| at + 1 >= pass && rects.iter().any(|r| r.contains(pos)))
}

fn note_rect(ctx: &egui::Context, rect: egui::Rect) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let entry = d.get_temp_mut_or_default::<(u64, Vec<egui::Rect>)>(rects_id());
        if entry.0 != pass {
            *entry = (pass, Vec::new());
        }
        entry.1.push(rect);
    });
}

/// `modal.show(ctx, content)`, kept inside the desk while one draws: centred in its rect, the
/// backdrop over the rect only, and without egui's modal layer, which would block Fossa's own
/// widgets (the chat) behind it. The backdrop still takes every press inside the desk.
pub fn show_modal<T>(modal: egui::Modal, ctx: &egui::Context, content: impl FnOnce(&mut egui::Ui) -> T) -> egui::ModalResponse<T> {
    let Some(rect) = DRAWING.with(Cell::get) else { return modal.show(ctx, content) };
    let egui::Modal { area, backdrop_color, frame } = modal;
    let any_popup_open = egui::Popup::is_any_open(ctx);
    let egui::InnerResponse { inner: (inner, backdrop_response), response } = area.constrain_to(rect).show(ctx, |ui| {
        let mut backdrop = ui.new_child(egui::UiBuilder::new().sense(egui::Sense::CLICK | egui::Sense::DRAG).max_rect(rect));
        backdrop.set_min_size(rect.size());
        ui.painter().rect_filled(rect, 0.0, backdrop_color);
        let backdrop_response = backdrop.response();
        let frame = frame.unwrap_or_else(|| egui::Frame::popup(ui.style()));
        let inner = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::CLICK | egui::Sense::DRAG), |ui| frame.show(ui, content).inner).inner;
        (inner, backdrop_response)
    });
    egui::ModalResponse { response, backdrop_response, inner, is_top_modal: true, any_popup_open }
}

fn shots_id() -> egui::Id {
    egui::Id::new("photocraft-shot-requests")
}

/// The layers panel's "Add to shot": queue layer `id` for [`Desk::take_shot_requests`].
pub(crate) fn request_shot(ctx: &egui::Context, id: u64) {
    ctx.data_mut(|d| d.get_temp_mut_or_default::<Vec<u64>>(shots_id()).push(id));
    ctx.request_repaint();
}

/// What the desk's input plugin needs from the app between frames.
#[derive(Default)]
struct Shared {
    /// The control channel's synthetic input, fed one step per frame like `raw_input_hook` does.
    synthetic: Vec<egui::Event>,
    /// A step was fed this frame: the app treats this frame's input as automation's.
    took: bool,
    /// The desk had the keyboard last frame (pointer over it, or a field of its focused).
    keys: bool,
    /// The Type tool is editing text in the desk.
    typing: bool,
    /// Preferences > Zoom with trackpad pinch.
    pinch: bool,
}

/// The part of `PhotocraftApp::raw_input_hook` a hosted desk needs (the host's app gets that
/// hook, not PhotoCraft): clipboard keys and pinch folding while the desk has the keyboard or the
/// pointer, and the control channel's synthetic input.
struct DeskInput(Arc<Mutex<Shared>>);

impl egui::Plugin for DeskInput {
    fn debug_name(&self) -> &'static str {
        "PhotoCraft desk input"
    }

    fn input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        let mut s = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let moved = raw.events.iter().rev().find_map(|e| if let egui::Event::PointerMoved(p) = e { Some(*p) } else { None });
        let over = moved.or_else(|| ctx.input(|i| i.pointer.latest_pos())).is_some_and(|p| owns(ctx, p));
        if s.keys || over {
            crate::shortcuts::clipboard_keys(ctx, ctx.text_edit_focused() || s.typing, raw);
        }
        if over && s.pinch && cfg!(any(target_os = "windows", target_os = "linux")) {
            crate::wheel_nav::fold_legacy_pinch(ctx, raw);
        }
        let step = crate::synthetic_step(&mut s.synthetic);
        if !step.is_empty() {
            s.took = true;
            raw.events.extend(step);
            ctx.request_repaint();
        }
    }
}

pub struct Desk {
    app: PhotocraftApp,
    /// Files from Fossa, opened by the app's next frame.
    inbox: Inbox,
    /// Files saved or exported since the last [`Desk::take_saved`].
    saved: Inbox,
    ctx: egui::Context,
    /// PhotoCraft's own style and options, kept between frames.
    options: Option<egui::Options>,
    shared: Arc<Mutex<Shared>>,
    /// The pass the app's `logic` last ran in: once a pass, however many of its panels draw.
    logic_pass: Option<u64>,
    /// The areas (windows, menus, tooltips, dialogs) the desk shows.
    layers: HashSet<egui::LayerId>,
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

/// The chrome panels [`Desk::panel_ui`] draws alone, before the dock's tabs.
const CHROME_PANELS: [&str; 4] = ["canvas", "toolbar", "optionsBar", "statusBar"];

/// A dock tab's panel id: its name in camelCase ("Layer Comps" is `layerComps`).
fn tab_id(name: &str) -> String {
    let mut out = String::new();
    for (i, word) in name.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).enumerate() {
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(if i == 0 { first.to_ascii_lowercase() } else { first.to_ascii_uppercase() });
            out.extend(chars);
        }
    }
    out
}

/// The dock group and tab index of panel `id`.
fn dock_tab(id: &str, pro: bool) -> Option<(crate::dock::Group, usize)> {
    crate::dock::Group::ALL.into_iter().find_map(|g| g.tabs(pro).iter().position(|t| tab_id(t) == id).map(|i| (g, i)))
}

/// Draw panel `panel` alone in `ui` (from `PhotocraftApp::ui`, in place of the chrome and canvas).
pub(crate) fn draw_solo(app: &mut PhotocraftApp, ui: &mut egui::Ui, panel: &str) {
    let t = crate::theme::Tokens::get(ui.ctx());
    match panel {
        "toolbar" => crate::panels::toolbar(app, ui),
        "optionsBar" => crate::panels::options_bar(app, ui),
        "statusBar" => crate::panels::status_bar(app, ui),
        "canvas" => {
            let backdrop = crate::prefs_ui::pasteboard_color(app).unwrap_or(t.canvas);
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(backdrop)).show(ui, |ui| crate::canvas::document_area(app, ui));
        }
        tab => {
            let Some((group, index)) = dock_tab(tab, t.pro) else { return };
            egui::CentralPanel::default().frame(egui::Frame::NONE.fill(t.dock).inner_margin(egui::Margin::same(8))).show(ui, |ui| {
                if group.scrolls_itself(index) {
                    crate::panels::dock_body(app, ui, group, index);
                } else {
                    egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| crate::panels::dock_body(app, ui, group, index));
                }
            });
        }
    }
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
        let shared = Arc::new(Mutex::new(Shared::default()));
        // One input plugin per context: a later desk takes it over.
        if ctx.with_plugin::<DeskInput, _>(|p| p.0 = shared.clone()).is_none() {
            ctx.add_plugin(DeskInput(shared.clone()));
        }
        Self { app, inbox, saved, ctx: ctx.clone(), options: None, shared, logic_pass: None, layers: HashSet::new() }
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

    /// What the selection holds, merged (every visible layer, as Copy Merged takes it), cropped to
    /// the selection and transparent outside it, as a PNG. `Err` when nothing is selected.
    pub fn grab_selection_png(&mut self) -> Result<Vec<u8>, String> {
        let st = self.app.session.active().ok_or("no document is open")?;
        if st.doc.selection.is_none() {
            return Err("nothing is selected".into());
        }
        let mut tmp = Session::new();
        tmp.add_document((*st.doc).clone(), None);
        let copied = tmp.execute("edit.copyMerged", json!({})).map_err(|e| e.to_string())?;
        let size = |i: usize| copied.pointer(&format!("/bounds/{i}")).and_then(Value::as_u64).filter(|v| *v > 0).ok_or("the selected area is empty");
        tmp.execute("file.new", json!({"width": size(2)?, "height": size(3)?, "background": "transparent"})).map_err(|e| e.to_string())?;
        tmp.execute("edit.paste", json!({})).map_err(|e| e.to_string())?;
        let doc = &tmp.active().ok_or("no document")?.doc;
        photocraft_io::export(doc, "selection.png", &photocraft_io::ExportOptions::default()).map(|r| r.bytes).map_err(|e| e.to_string())
    }

    /// Layer `layer_id` of the active document alone (shown, blended normally), at the document's
    /// size and place, transparent elsewhere, as a PNG.
    pub fn grab_layer_png(&mut self, layer_id: u64) -> Result<Vec<u8>, String> {
        let st = self.app.session.active().ok_or("no document is open")?;
        let l = st.doc.layer(photocraft_doc::LayerId(layer_id)).ok_or_else(|| format!("no layer {layer_id}"))?;
        let mut alone = l.clone();
        alone.visible = true;
        alone.blend = if alone.is_group() { photocraft_color::BlendMode::PassThrough } else { photocraft_color::BlendMode::Normal };
        alone.clipped = false;
        let mut doc = (*st.doc).clone();
        doc.layers = vec![alone];
        doc.selection = None;
        photocraft_io::export(&doc, "layer.png", &photocraft_io::ExportOptions::default()).map(|r| r.bytes).map_err(|e| e.to_string())
    }

    /// Layers asked for with the layers panel's "Add to shot" since the last call: (layer name,
    /// PNG from [`Desk::grab_layer_png`]). A layer that can't be grabbed any more is left out.
    pub fn take_shot_requests(&mut self) -> Vec<(String, Vec<u8>)> {
        let ids = self.ctx.data_mut(|d| d.remove_temp::<Vec<u64>>(shots_id())).unwrap_or_default();
        ids.into_iter()
            .filter_map(|id| {
                let name = self.app.session.active()?.doc.layer(photocraft_doc::LayerId(id))?.name.clone();
                self.grab_layer_png(id).ok().map(|png| (name, png))
            })
            .collect()
    }

    /// Run control method `method` (`control.rs`: `engine.execute`, `engine.commands`,
    /// `ui.inspect`, `ui.set`, `ui.dialog.*`, `ui.pointer`, ...) on the desk's app, in-process,
    /// between frames. The method's result, or its error. Synthetic input (`ui.click`, `ui.key`,
    /// ...) replies `{"queued": true}` and is fed over the next frames; a command that runs as a
    /// background job replies `{"job", "pending": true}` (see `jobs.list`).
    pub fn control(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let (req, _reply) = crate::ControlRequest::new(method, params);
        let out = match crate::control::handle(&mut self.app, &self.ctx, &req) {
            crate::control::Outcome::Done(v) => v,
            crate::control::Outcome::AfterInput => json!({"ok": true, "result": {"queued": true}}),
            crate::control::Outcome::AfterJob(job) => json!({"ok": true, "result": {"job": job, "pending": true}}),
            crate::control::Outcome::Screenshot { .. } => json!({"ok": false, "error": "a desk has no window to capture: use Desk::grab_png"}),
        };
        self.app.sync_mask_targets();
        self.feed_input();
        self.ctx.request_repaint();
        if out.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(out.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(out.get("error").and_then(Value::as_str).unwrap_or("the control request failed").to_string())
        }
    }

    /// The `engine.commands` listing, each command with `fields`: its parameters typed as far as
    /// its parameter description allows (id, kind, optional, default, min/max, choices, unit,
    /// note). `typed` is false where some of the description is free text (kept in `params`).
    pub fn commands(&mut self) -> Value {
        let listed = self.app.session.execute("command.list", json!({})).unwrap_or(Value::Null);
        let rows = listed.as_array().cloned().or_else(|| listed.get("commands").and_then(Value::as_array).cloned()).unwrap_or_default();
        Value::Array(
            rows.into_iter()
                .map(|mut row| {
                    let (fields, typed) = row.get("params").and_then(Value::as_str).map_or((Vec::new(), true), param_fields);
                    if let Some(o) = row.as_object_mut() {
                        o.insert("fields".into(), Value::Array(fields));
                        o.insert("typed".into(), Value::Bool(typed));
                    }
                    row
                })
                .collect(),
        )
    }

    /// The ids of the panels [`Desk::panel_ui`] can draw alone.
    pub fn panels(&self) -> Vec<String> {
        let tabs = crate::dock::Group::ALL.into_iter().flat_map(|g| g.tabs(true).iter().map(|t| tab_id(t)));
        CHROME_PANELS.iter().map(|s| s.to_string()).chain(tabs).collect()
    }

    /// Narrow what the panels show to `target`, any of: `document` (index), `layer` (id: the
    /// layers panel, Properties and the canvas act on it), `tool`, the view (`zoom`, `center`
    /// `[x, y]`, `rotation`, `fit`, or `region` `[x, y, w, h]` in document pixels to fit in the
    /// canvas), `effect` (the Layer Style dialog on that effect of the layer).
    pub fn focus(&mut self, target: Value) -> Result<(), String> {
        if let Some(d) = target.get("document") {
            self.control("engine.execute", json!({"command": "document.activate", "params": {"document": d}}))?;
        }
        if let Some(l) = target.get("layer") {
            self.control("engine.execute", json!({"command": "layer.select", "params": {"layer": l, "mode": "replace"}}))?;
        }
        let mut set = serde_json::Map::new();
        for k in ["tool", "zoom", "center", "rotation", "fit"] {
            if let Some(v) = target.get(k) {
                set.insert(k.into(), v.clone());
            }
        }
        if let Some(r) = target.get("region").and_then(Value::as_array) {
            let n: Vec<f64> = r.iter().filter_map(Value::as_f64).collect();
            let [x, y, w, h] = n[..] else { return Err("region must be [x, y, w, h]".into()) };
            if !(w > 0.0 && h > 0.0) {
                return Err("region must have a size".into());
            }
            let view = self.app.last_canvas_rect;
            let zoom = (f64::from(view.width()) / w).min(f64::from(view.height()) / h) * f64::from(self.app.canvas_ppp()) * 0.9;
            set.insert("zoom".into(), json!(zoom));
            set.insert("center".into(), json!([x + w / 2.0, y + h / 2.0]));
        }
        if !set.is_empty() {
            self.control("ui.set", Value::Object(set))?;
        }
        if let Some(effect) = target.get("effect") {
            self.control("ui.dialog.open", json!({"kind": "layerStyle", "effect": effect}))?;
        }
        Ok(())
    }

    /// Draw and run the editor inside `ui`'s rect of the host window, one frame.
    pub fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.draw(ui, frame, None);
    }

    /// Draw only panel `panel` ([`Desk::panels`]) of the editor in `ui`'s rect, with its dialogs
    /// and menus, under the same rules as [`Desk::ui`]. An unknown id draws nothing.
    pub fn panel_ui(&mut self, panel: &str, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.draw(ui, frame, Some(panel));
    }

    fn draw(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame, solo: Option<&str>) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        note_rect(&ctx, rect);
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
        if std::mem::take(&mut self.shared.lock().unwrap_or_else(|e| e.into_inner()).took) {
            self.app.automation_input = true;
        }
        let before: HashSet<egui::LayerId> = ctx.memory(|m| m.areas().visible_layer_ids()).into_iter().collect();
        DRAWING.with(|d| d.set(Some(rect)));
        SOLO.with(|s| *s.borrow_mut() = solo.map(str::to_owned));
        let pass = ctx.cumulative_pass_nr();
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect).id_salt(("photocraft", solo)), |ui| {
            ui.set_clip_rect(rect.intersect(ui.clip_rect()));
            if self.logic_pass != Some(pass) {
                self.logic_pass = Some(pass);
                eframe::App::logic(&mut self.app, &ctx, frame);
            }
            ui.set_style(ctx.global_style());
            eframe::App::ui(&mut self.app, ui, frame);
        });
        DRAWING.with(|d| d.set(None));
        SOLO.with(|s| *s.borrow_mut() = None);
        self.fit_areas(&ctx, rect, &before);
        if !held.is_empty() {
            ctx.input_mut(|i| i.events.extend(held));
        }
        self.feed_input();
        {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            s.keys = over || focused;
            s.typing = self.app.ui.text_edit.is_some();
            s.pinch = self.app.session.prefs().enhanced_controls.zoom_with_trackpad_pinch;
        }
        let mut own = ctx.options(|o| o.clone());
        // Fossa's window follows Fossa's theme, never the desk's.
        own.sync_window_theme = false;
        self.options = Some(own);
        ctx.options_mut(|o| adopt(o, &host));
        ctx.set_embed_viewports(embed);
    }

    /// Hand queued synthetic input to the input plugin, which feeds it a step a frame.
    fn feed_input(&mut self) {
        if !self.app.synthetic.is_empty() {
            let events = std::mem::take(&mut self.app.synthetic);
            self.shared.lock().unwrap_or_else(|e| e.into_inner()).synthetic.extend(events);
        }
    }

    /// Keep the desk's areas (windows, menus, popups, tooltips) inside `rect`: an area shown first
    /// in the desk's frame is the desk's, and one that runs past `rect` is moved back in (a layer
    /// transform, which moves its input with it); one larger than `rect` keeps its top-left in it.
    fn fit_areas(&mut self, ctx: &egui::Context, rect: egui::Rect, before: &HashSet<egui::LayerId>) {
        let after: HashSet<egui::LayerId> = ctx.memory(|m| m.areas().visible_layer_ids()).into_iter().collect();
        self.layers.retain(|l| after.contains(l));
        self.layers.extend(after.iter().filter(|l| !before.contains(l)).copied());
        let shift = |lo: f32, hi: f32, min: f32, max: f32| if lo < min || hi - lo > max - min { min - lo } else if hi > max { max - hi } else { 0.0 };
        for layer in &self.layers {
            let Some(area) = ctx.memory(|m| m.area_rect(layer.id)) else { continue };
            let delta = egui::vec2(shift(area.left(), area.right(), rect.left(), rect.right()), shift(area.top(), area.bottom(), rect.top(), rect.bottom()));
            ctx.set_transform_layer(*layer, egui::emath::TSTransform::from_translation(delta));
        }
    }
}

/// A command's parameter description (`CommandSpec::params`, JSON-ish: `{"radius":1..500=1,
/// "mode":"a|b"="a","layer":id?}`) as typed fields, and whether all of it was understood.
fn param_fields(desc: &str) -> (Vec<Value>, bool) {
    let desc = desc.trim();
    let Some(body) = desc.strip_prefix('{') else { return (Vec::new(), desc.is_empty()) };
    let Some(end) = matching(body) else { return (Vec::new(), false) };
    // Anything after the object other than its result (an arrow, then the reply) is prose.
    let rest = body[end + 1..].trim();
    let mut typed = rest.is_empty() || rest.starts_with('\u{2192}');
    let mut fields = Vec::new();
    for item in split_top(&body[..end], ',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        match field(item) {
            Some(f) => {
                typed &= f.get("kind").and_then(Value::as_str) != Some("untyped");
                fields.push(f);
            }
            None => typed = false,
        }
    }
    (fields, typed)
}

/// The index of the `}` closing an object whose `{` came just before `s`.
fn matching(s: &str) -> Option<usize> {
    let (mut depth, mut quoted) = (0i32, false);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '{' | '[' | '(' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' | ']' | ')' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// `s` split at `sep` outside quotes and brackets.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut depth, mut quoted, mut start, mut out) = (0i32, false, 0, Vec::new());
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '{' | '[' | '(' => depth += 1,
            '}' | ']' | ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// One `"key":type?=default (note)` item as a field; `None` when it isn't of that form.
fn field(item: &str) -> Option<Value> {
    let rest = item.strip_prefix('"')?;
    let close = rest.find('"')?;
    let id = &rest[..close];
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let mut spec = rest[close + 1..].trim_start().strip_prefix(':')?.trim();
    // A trailing parenthesised note.
    let mut note = None;
    if spec.ends_with(')')
        && let Some(open) = spec.rfind('(')
    {
        note = Some(spec[open + 1..spec.len() - 1].trim().to_string());
        spec = spec[..open].trim();
    }
    let parts = split_top(spec, '=');
    let (ty, default) = match parts.as_slice() {
        [ty] => (ty.trim(), None),
        [ty, d] => (ty.trim(), Some(d.trim())),
        _ => (spec, None),
    };
    let optional = ty.ends_with('?') || default.is_some();
    let ty = ty.trim_end_matches('?').trim();
    let mut f = serde_json::Map::new();
    f.insert("id".into(), json!(id));
    f.insert("optional".into(), json!(optional));
    let kind = kind_of(ty, &mut f);
    f.insert("kind".into(), json!(kind));
    if let Some(d) = default {
        let v = serde_json::from_str::<Value>(d).unwrap_or_else(|_| json!(d.trim_matches('"')));
        f.insert("default".into(), v);
    }
    if let Some(n) = note {
        f.insert("note".into(), json!(n));
    }
    if kind == "untyped" {
        f.insert("spec".into(), json!(ty));
    }
    Some(Value::Object(f))
}

/// The kind of type `ty`, adding its range, choices or unit to `f`.
fn kind_of(ty: &str, f: &mut serde_json::Map<String, Value>) -> &'static str {
    let num = |s: &str| s.trim().parse::<f64>().ok();
    match ty {
        "bool" | "true" | "false" => return "bool",
        "id" | "layerId" => return "id",
        "index" | "n" | "int" | "i" | "j" | "row" | "i32" | "i64" | "u32" | "u64" | "usize" => return "integer",
        "str" | "string" | "name" | "text" | "char" | "folder" | "path" | "group" | "base64 bytes" => return "string",
        "color" => return "color",
        "ppi" => {
            f.insert("unit".into(), json!(ty));
            return "number";
        }
        "json" => return "json",
        "f32" | "f64" | "number" => return "number",
        "px" | "pt" | "deg" | "%" => {
            f.insert("unit".into(), json!(ty));
            return "number";
        }
        _ => {}
    }
    // "a|b|c": choices.
    if ty.len() >= 2 && ty.starts_with('"') && ty.ends_with('"') && !ty[1..ty.len() - 1].contains('"') {
        f.insert("choices".into(), json!(ty[1..ty.len() - 1].split('|').collect::<Vec<_>>()));
        return "enum";
    }
    // min..max, maybe with a unit after it.
    if let Some((lo, hi)) = ty.split_once("..") {
        let (hi, unit) = hi.trim().split_once(' ').map_or((hi, None), |(h, u)| (h, Some(u.trim())));
        if let (Some(l), Some(h)) = (num(lo), num(hi)) {
            f.insert("min".into(), json!(l));
            f.insert("max".into(), json!(h));
            if let Some(u) = unit {
                f.insert("unit".into(), json!(u));
            }
            return if lo.contains('.') || hi.contains('.') { "number" } else { "integer" };
        }
    }
    if ty.starts_with('[') {
        return "array";
    }
    if ty.starts_with('{') {
        return "object";
    }
    // A number standing for itself (a choice of a union, `24|32`).
    if let Some(n) = num(ty) {
        f.insert("choices".into(), json!([n]));
        return "enum";
    }
    // a|b: each part typed on its own.
    let parts = split_top(ty, '|');
    if parts.len() > 1 {
        let mut of = Vec::new();
        for part in parts {
            let mut sub = serde_json::Map::new();
            let kind = kind_of(part.trim(), &mut sub);
            if kind == "untyped" {
                return "untyped";
            }
            sub.insert("kind".into(), json!(kind));
            of.push(Value::Object(sub));
        }
        f.insert("of".into(), Value::Array(of));
        return "union";
    }
    "untyped"
}

/// The config folder tests use instead of the user's.
#[cfg(test)]
static TEST_CONFIG: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Where the desk keeps PhotoCraft's preferences: `<config dir>/Fossa Desk/PhotoCraft`, or
/// `$FOSSA_DESK_CONFIG/PhotoCraft` when that is set (tests).
fn prefs_file() -> Option<std::path::PathBuf> {
    let var = |k: &str| std::env::var_os(k).map(std::path::PathBuf::from).filter(|p| p.is_absolute());
    #[cfg(test)]
    if let Some(dir) = TEST_CONFIG.get() {
        return Some(dir.join("PhotoCraft").join("preferences.json"));
    }
    let base = var("FOSSA_DESK_CONFIG").or_else(|| {
        let config = if cfg!(windows) {
            var("APPDATA")
        } else if cfg!(target_os = "macos") {
            var("HOME").map(|h| h.join("Library").join("Application Support"))
        } else {
            var("XDG_CONFIG_HOME").or_else(|| var("HOME").map(|h| h.join(".config")))
        };
        config.map(|c| c.join("Fossa Desk"))
    })?;
    Some(base.join("PhotoCraft").join("preferences.json"))
}

/// The desk's services: files come from Fossa and go back to it, or from a file dialog of its
/// own (File > Open, Place, folders); Save and Export never write the disk.
fn services(inbox: Inbox, saved: Inbox) -> Services {
    Services {
        import: Some(Box::new(|name: &str, bytes: &[u8], opts: &photocraft_io::ImportOptions| {
            photocraft_io::import_with_options(name, bytes, opts).map(|r| (r.document, r.warnings)).map_err(|e| e.to_string())
        })),
        export: Some(Box::new(|doc: &photocraft_doc::Document, path: &str, settings: &crate::ExportSettings| {
            photocraft_io::export(doc, path, &settings.io_options()).map(|r| (r.bytes, r.warnings)).map_err(|e| e.to_string())
        })),
        file_dialog: Some(Box::new(|request, parent, reply| match request {
            // No save dialog: the suggested name is the name the file goes back to Fossa under.
            FileDialogRequest::Save { suggested } => reply.send(Some(FileDialogAnswer::SaveTo(file_name(&suggested)))),
            other => pick(other, parent, reply),
        })),
        write: Some(Box::new(move |path: &str, bytes: &[u8]| {
            saved.lock().unwrap_or_else(|e| e.into_inner()).push((file_name(path), bytes.to_vec()));
            Ok(())
        })),
        encode_png: Some(Box::new(|w, h, rgba| {
            let img = Image::from_u8(w, h, ChannelLayout::Rgba, rgba.to_vec()).map_err(|e| e.to_string())?;
            photocraft_codecs::encode(&img, photocraft_codecs::Format::Png, &EncodeOptions::default()).map_err(|e| e.to_string())
        })),
        load_prefs: Some(Box::new(|| std::fs::read_to_string(prefs_file()?).ok())),
        save_prefs: Some(Box::new(|text: &str| {
            let path = prefs_file().ok_or("no config directory")?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            photocraft_format::atomic_write(&path, text.as_bytes()).map_err(|e| e.to_string())
        })),
        inbox: Some(inbox),
        ..Default::default()
    }
}

/// An Open or folder dialog that never blocks the frame loop: rfd's async dialog is made here, on
/// the UI thread, with Fossa's window as its parent, and a thread waits for the answer.
#[cfg(not(target_arch = "wasm32"))]
fn pick(request: FileDialogRequest, parent: Option<&eframe::Frame>, reply: crate::FileDialogReply) {
    use std::future::Future;
    use std::pin::Pin;
    fn answer(picked: Vec<std::path::PathBuf>, make: fn(Vec<String>) -> FileDialogAnswer) -> FileDialogAnswer {
        match picked.iter().map(|p| crate::file_open::path_text(p, false)).collect() {
            Ok(paths) => make(paths),
            Err(e) => FileDialogAnswer::Unavailable(e),
        }
    }
    fn folder(paths: Vec<String>) -> FileDialogAnswer {
        paths.into_iter().next().map_or(FileDialogAnswer::Unavailable("no folder was chosen".into()), FileDialogAnswer::FolderTo)
    }
    let mut dialog = rfd::AsyncFileDialog::new();
    if let Some(parent) = parent {
        dialog = dialog.set_parent(parent);
    }
    let answer: Pin<Box<dyn Future<Output = Option<FileDialogAnswer>> + Send>> = match request {
        FileDialogRequest::Open { multiple, initial_dir, extensions } => {
            if let Some(dir) = initial_dir {
                dialog = dialog.set_directory(dir);
            }
            if let Some(exts) = extensions {
                dialog = dialog.add_filter("Supported Files", &exts);
            }
            if multiple {
                let picked = dialog.pick_files();
                Box::pin(async move { picked.await.map(|files| answer(files.iter().map(|f| f.path().to_path_buf()).collect(), FileDialogAnswer::Paths)) })
            } else {
                let picked = dialog.pick_file();
                Box::pin(async move { picked.await.map(|file| answer(vec![file.path().to_path_buf()], FileDialogAnswer::Paths)) })
            }
        }
        FileDialogRequest::Folder { initial_dir } => {
            if let Some(dir) = initial_dir {
                dialog = dialog.set_directory(dir);
            }
            let picked = dialog.pick_folder();
            Box::pin(async move { picked.await.map(|dir| answer(vec![dir.path().to_path_buf()], folder)) })
        }
        FileDialogRequest::Save { .. } => Box::pin(async { None }),
    };
    // Without the thread the reply is dropped unanswered, which the app takes as Cancel.
    if let Err(e) = std::thread::Builder::new().name("desk file dialog".into()).spawn(move || reply.send(pollster::block_on(answer))) {
        log::error!("couldn't wait for the file dialog: {e}");
    }
}

#[cfg(target_arch = "wasm32")]
fn pick(_: FileDialogRequest, _: Option<&eframe::Frame>, reply: crate::FileDialogReply) {
    reply.send(None);
}

/// Offscreen proof of the desk in a host window (no window on the desktop):
/// `FOSSA_DESK_SHOT=desk.png FOSSA_PANEL_SHOT=panel.png cargo test --release -p photocraft-ui-egui fossa_desk_shot -- --ignored`.
#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable;

    /// What the host draws: the whole desk, or one panel of it in a 900 x 320 card.
    #[derive(Clone, Copy, PartialEq)]
    enum Show {
        Desk,
        Panel(&'static str),
    }

    struct Host {
        desk: Option<Desk>,
        png: Option<Vec<u8>>,
        show: Show,
        /// The rect the desk or the panel was drawn in.
        rect: egui::Rect,
        /// Fossa's own text field (its chat), typed into while the desk runs.
        chat: String,
    }

    impl eframe::App for Host {
        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let all = ui.max_rect();
            let strip = egui::Rect::from_min_size(all.min, egui::vec2(all.width(), 40.0));
            ui.painter().rect_filled(strip, 0.0, egui::Color32::from_rgb(30, 24, 48));
            ui.painter().text(strip.left_center() + egui::vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, "Fossa host", egui::FontId::proportional(16.0), egui::Color32::WHITE);
            ui.scope_builder(egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(strip.min + egui::vec2(160.0, 8.0), egui::vec2(300.0, 24.0))), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.chat).id(egui::Id::new("host-chat")));
            });
            let desk = self.desk.get_or_insert_with(|| {
                let mut d = Desk::new(ui.ctx(), frame.wgpu_render_state());
                if let Some(png) = self.png.take() {
                    d.open("from Fossa.png", png);
                }
                d
            });
            let inner = egui::Rect::from_min_max(egui::pos2(all.left(), strip.bottom()), all.max);
            match self.show {
                Show::Desk => {
                    self.rect = inner;
                    ui.scope_builder(egui::UiBuilder::new().max_rect(inner).id_salt("desk_paint"), |ui| desk.ui(ui, frame));
                }
                Show::Panel(p) => {
                    let card = egui::Rect::from_min_size(inner.min + egui::vec2(40.0, 40.0), egui::vec2(900.0, 320.0));
                    self.rect = card;
                    ui.painter().rect_filled(card.expand(6.0), 8.0, egui::Color32::from_rgb(60, 50, 90));
                    ui.scope_builder(egui::UiBuilder::new().max_rect(card).id_salt("panel_paint"), |ui| desk.panel_ui(p, ui, frame));
                }
            }
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

    fn settle(h: &mut egui_kittest::Harness<'_, Host>, n: usize) {
        for _ in 0..n {
            h.run_steps(2);
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }

    fn save(h: &mut egui_kittest::Harness<'_, Host>, var: &str) {
        if let Ok(path) = std::env::var(var) {
            match h.render() {
                Ok(img) => img.save(&path).unwrap(),
                Err(e) => panic!("render: {e}"),
            }
        }
    }

    /// Screen rects of the visible areas of `order`.
    fn areas(ctx: &egui::Context, order: egui::Order) -> Vec<egui::Rect> {
        let layers: Vec<egui::LayerId> = ctx.memory(|m| m.areas().visible_layer_ids()).into_iter().filter(|l| l.order == order).collect();
        layers
            .into_iter()
            .filter_map(|l| {
                let r = ctx.memory(|m| m.area_rect(l.id))?;
                let t = ctx.layer_transform_to_global(l).unwrap_or_default();
                Some(t.mul_rect(r))
            })
            .collect()
    }

    /// Where `p`, a point of an area's own layout (as the accessibility tree reports it), shows
    /// on screen once `Desk::fit_areas` moved the area into the desk.
    fn on_screen(ctx: &egui::Context, p: egui::Pos2) -> egui::Pos2 {
        let layers: Vec<egui::LayerId> = ctx.memory(|m| m.areas().visible_layer_ids()).into_iter().filter(|l| l.order == egui::Order::Foreground).collect();
        layers
            .into_iter()
            .find(|l| ctx.memory(|m| m.area_rect(l.id)).is_some_and(|r| r.contains(p)))
            .and_then(|l| ctx.layer_transform_to_global(l))
            .map_or(p, |t| t * p)
    }

    fn png_size(png: &[u8]) -> (u32, u32) {
        let img = photocraft_codecs::decode_as(photocraft_codecs::Format::Png, png).unwrap();
        (img.width(), img.height())
    }

    fn click(h: &mut egui_kittest::Harness<'_, Host>, pos: egui::Pos2, button: egui::PointerButton) {
        h.event(egui::Event::PointerMoved(pos));
        h.event(egui::Event::PointerButton { pos, button, pressed: true, modifiers: Default::default() });
        settle(h, 1);
        h.event(egui::Event::PointerButton { pos, button, pressed: false, modifiers: Default::default() });
        settle(h, 2);
    }

    #[test]
    fn param_descriptions_type_what_they_say() {
        let (f, typed) = param_fields(r#"{"radius":1..500=1,"applyAtCanvasBounds":bool=false}"#);
        assert!(typed);
        assert_eq!(f[0], json!({"id": "radius", "optional": true, "kind": "integer", "min": 1.0, "max": 500.0, "default": 1}));
        assert_eq!(f[1]["kind"], "bool");
        let (f, typed) = param_fields(r#"{"mode":"replace|add"="replace","layer":id?,"tolerance":0.5..10 px=2} then prose"#);
        assert!(!typed, "prose after the object");
        assert_eq!(f[0]["choices"], json!(["replace", "add"]));
        assert_eq!(f[0]["default"], "replace");
        assert_eq!(f[1], json!({"id": "layer", "optional": true, "kind": "id"}));
        assert_eq!((f[2]["kind"].as_str(), f[2]["unit"].as_str()), (Some("number"), Some("px")));
        let (f, typed) = param_fields(r#"{"layer":id?,"enabled":bool? (default: toggle)}"#);
        assert!(typed && f[1]["note"] == "default: toggle" && f[1]["kind"] == "bool");
        assert_eq!(param_fields("{}"), (vec![], true));
        assert!(!param_fields(r#"{"x":px, {CHAR}}"#).1);
        let (f, typed) = param_fields(r#"{"layer":id|name,"bits":8|16,"target":"pixels"|"mask"|{"channel":i}}"#);
        assert!(typed, "{f:?}");
        assert_eq!(f[0]["kind"], "union");
        assert_eq!(f[1]["of"], json!([{"kind": "enum", "choices": [8.0]}, {"kind": "enum", "choices": [16.0]}]));
        assert_eq!(f[2]["of"].as_array().map(Vec::len), Some(3));
        assert_eq!(tab_id("Layer Comps"), "layerComps");
        assert_eq!(tab_id("Info"), "info");
    }

    #[test]
    #[ignore]
    fn fossa_desk_shot() {
        let config = std::env::temp_dir().join(format!("fossa-desk-test-{}", std::process::id()));
        TEST_CONFIG.set(config.clone()).unwrap();
        // Every check runs, and the test fails at the end with all that went wrong.
        let mut fails: Vec<String> = Vec::new();
        macro_rules! check {
            ($cond:expr, $($msg:tt)+) => {
                if !$cond {
                    let m = format!($($msg)+);
                    eprintln!("FAIL: {m}");
                    fails.push(m);
                }
            };
        }
        let png = test_png(1280, 720);
        let mut h = egui_kittest::Harness::builder()
            .with_size(egui::vec2(1600.0, 1000.0))
            .wgpu()
            // PhotoCraft's own accessibility is tested by its own suites, not this picture.
            .with_accessibility_check(false)
            .build_eframe(move |_cc| Host { desk: None, png: Some(png), show: Show::Desk, rect: egui::Rect::NOTHING, chat: String::new() });
        settle(&mut h, 20);
        let ctx = h.ctx.clone();

        // The control channel in-process: a new layer and a brush stroke on the opened frame.
        let desk = h.state_mut().desk.as_mut().unwrap();
        let cmds = desk.commands();
        let rows = cmds.as_array().cloned().unwrap_or_default();
        let typed = rows.iter().filter(|r| r["typed"] == true).count();
        let fields: Vec<&Value> = rows.iter().flat_map(|r| r["fields"].as_array().into_iter().flatten()).collect();
        let untyped_fields = fields.iter().filter(|f| f["kind"] == "untyped").count();
        eprintln!("commands: {} ({typed} fully typed, {} with free text); fields: {} ({} untyped)", rows.len(), rows.len() - typed, fields.len(), untyped_fields);
        if let Ok(path) = std::env::var("FOSSA_COMMANDS_OUT") {
            std::fs::write(path, serde_json::to_string_pretty(&cmds).unwrap()).unwrap();
        }
        check!(rows.len() > 100 && typed > 0, "commands listed and typed");
        let panels = desk.panels();
        eprintln!("panels: {}", panels.join(", "));
        check!(panels.iter().any(|p| p == "layers") && panels.iter().any(|p| p == "canvas"), "panel ids");
        let r = desk.control("engine.execute", json!({"command": "layer.new.layer", "params": {"name": "Stroke"}}));
        check!(r.is_ok(), "layer.new.layer: {r:?}");
        let r = desk.control("ui.set", json!({"tool": "brush", "brushSize": 60}));
        check!(r.is_ok(), "ui.set tool: {r:?}");
        let stroke = json!({"events": [{"kind": "down", "x": 200, "y": 200}, {"kind": "move", "x": 600, "y": 300}, {"kind": "move", "x": 1000, "y": 500}, {"kind": "up", "x": 1000, "y": 500}]});
        let r = desk.control("ui.pointer", stroke);
        check!(r.is_ok(), "ui.pointer: {r:?}");
        check!(desk.control("nope.method", json!({})).is_err(), "unknown method is an error");
        settle(&mut h, 10);
        save(&mut h, "FOSSA_DESK_SHOT");

        // Grabs: the frame, a selection, the new layer.
        let desk = h.state_mut().desk.as_mut().unwrap();
        let g = desk.grab_png().map(|p| png_size(&p));
        check!(g == Ok((1280, 720)), "grab_png: {g:?}");
        check!(desk.grab_selection_png().is_err(), "nothing is selected yet");
        let r = desk.control("engine.execute", json!({"command": "select.rect", "params": {"x": 100, "y": 50, "width": 300, "height": 200}}));
        check!(r.is_ok(), "select.rect: {r:?}");
        let g = desk.grab_selection_png().map(|p| png_size(&p));
        check!(g == Ok((300, 200)), "grab_selection_png: {g:?}");
        let id = desk.app.session.active().and_then(|d| d.active_layer).map_or(0, |l| l.0);
        let g = desk.grab_layer_png(id).map(|p| png_size(&p));
        check!(g == Ok((1280, 720)), "grab_layer_png: {g:?}");
        let _ = desk.control("engine.execute", json!({"command": "select.deselect"}));

        // A dialog: inside the desk, its backdrop over the desk only, Fossa's chat still usable.
        let r = desk.control("ui.dialog.open", json!({"kind": "newDocument"}));
        check!(r.is_ok(), "ui.dialog.open: {r:?}");
        settle(&mut h, 8);
        let rect = h.state().rect;
        let fg = areas(&ctx, egui::Order::Foreground);
        check!(!fg.is_empty(), "the dialog is shown");
        for r in &fg {
            check!(rect.expand(1.0).contains_rect(*r), "dialog area {r:?} inside the desk {rect:?}");
        }
        save(&mut h, "FOSSA_DIALOG_SHOT");
        click(&mut h, egui::pos2(300.0, 20.0), egui::PointerButton::Primary);
        h.event(egui::Event::Text("hi".into()));
        settle(&mut h, 2);
        check!(h.state().chat == "hi", "Fossa's field takes input while a desk dialog is open: {:?}", h.state().chat);
        let desk = h.state_mut().desk.as_mut().unwrap();
        if let Some(dialog) = desk.app.ui.dialogs.last().map(|d| d.id) {
            let r = desk.control("ui.dialog.cancel", json!({"dialog": dialog}));
            check!(r.is_ok(), "ui.dialog.cancel: {r:?}");
        }
        settle(&mut h, 4);
        check!(h.state().desk.as_ref().unwrap().app.ui.dialogs.is_empty(), "the dialog closed");

        // One panel alone, narrowed to the new layer; its context menu opens inside the card and
        // its Add to shot queues the layer.
        h.state_mut().show = Show::Panel("layers");
        let r = h.state_mut().desk.as_mut().unwrap().focus(json!({"layer": id}));
        check!(r.is_ok(), "focus: {r:?}");
        settle(&mut h, 10);
        let card = h.state().rect;
        let row = h.query_by_label("Stroke").map(|n| n.rect().center());
        check!(row.is_some_and(|p| card.contains(p)), "the layer row {row:?} is in the card {card:?}");
        if let Some(row) = row {
            click(&mut h, row, egui::PointerButton::Secondary);
            settle(&mut h, 4);
        }
        let menus = areas(&ctx, egui::Order::Foreground);
        check!(!menus.is_empty(), "the context menu is open");
        for r in &menus {
            check!(card.expand(1.0).contains_rect(*r), "menu {r:?} inside the card {card:?}");
        }
        save(&mut h, "FOSSA_MENU_SHOT");
        let add = h.query_by_label("Add to shot").map(|n| on_screen(&ctx, n.rect().center()));
        check!(add.is_some(), "the menu has Add to shot");
        if let Some(add) = add {
            click(&mut h, add, egui::PointerButton::Primary);
            settle(&mut h, 4);
        }
        let shots = h.state_mut().desk.as_mut().unwrap().take_shot_requests();
        let names: Vec<&str> = shots.iter().map(|s| s.0.as_str()).collect();
        check!(names == ["Stroke"], "the menu's Add to shot queued the layer: {names:?}");
        check!(shots.first().is_none_or(|s| png_size(&s.1) == (1280, 720)), "the shot is the frame's size");
        settle(&mut h, 2);
        save(&mut h, "FOSSA_PANEL_SHOT");

        // Every symbol in the UI's own strings has a glyph in the desk's fonts (letters of other
        // scripts come from the system's fonts on demand, cjk_fonts.rs).
        let font = egui::FontId::new(13.0, crate::theme::hosted_family(&egui::FontFamily::Proportional));
        let missing: Vec<String> = ui_symbols().into_iter().filter(|c| !ctx.fonts_mut(|f| f.has_glyph(&font, *c))).map(|c| format!("{c} U+{:04X}", u32::from(c))).collect();
        check!(missing.is_empty(), "no glyph for {missing:?}");

        // Preferences persist in the desk's own config folder.
        let r = h.state_mut().desk.as_mut().unwrap().control("ui.set", json!({"brushSize": 42}));
        check!(r.is_ok(), "ui.set brushSize: {r:?}");
        settle(&mut h, 4);
        let saved = config.join("PhotoCraft").join("preferences.json").is_file();
        let _ = std::fs::remove_dir_all(&config);
        check!(saved, "preferences saved in the desk's config folder");
        assert!(fails.is_empty(), "{} checks failed:\n{}", fails.len(), fails.join("\n"));
    }

    /// Symbols the UI draws: the non-ASCII, non-letter characters of string literals in src/
    /// outside tests.
    fn ui_symbols() -> std::collections::BTreeSet<char> {
        let mut out = std::collections::BTreeSet::new();
        let mut dirs = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = dirs.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                    continue;
                }
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                if !name.ends_with(".rs") || name.contains("test") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                let code = text.split("#[cfg(test)]").next().unwrap_or("");
                for line in code.lines().filter(|l| !l.trim_start().starts_with("//")) {
                    for (i, lit) in line.split('"').enumerate() {
                        if i % 2 == 1 {
                            out.extend(lit.chars().filter(|c| !c.is_ascii() && !c.is_alphabetic() && !is_mark(*c)));
                        }
                    }
                }
            }
        }
        out
    }

    /// Combining marks (Devanagari and Thai vowel signs), which come with their script's font.
    fn is_mark(c: char) -> bool {
        matches!(u32::from(c), 0x0300..=0x036F | 0x0900..=0x0DFF | 0x0E31..=0x0E4E)
    }
}
