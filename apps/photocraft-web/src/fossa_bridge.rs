//! Running inside Fossa: the page that embeds PhotoCraft hands it files and takes the results back.
//!
//! Off unless the page is opened with `?fossa=<embedding page's origin>`. Then:
//! - a message `{fossa: "open", name, bytes}` from that origin opens the file, as a drop would;
//! - Save and Export send `{fossa: "saved", name, bytes}` to that origin instead of downloading;
//! - `{fossa: "ready"}` is sent once the app is listening.
//!
//! Messages from any other origin are ignored.

use std::sync::{Arc, Mutex};

use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;

type Inbox = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// The embedding page's origin, when PhotoCraft was opened by Fossa.
pub fn host() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let value = search.trim_start_matches('?').split('&').find_map(|kv| kv.strip_prefix("fossa="))?;
    let origin = js_sys::decode_uri_component(value).ok()?.as_string()?;
    (origin.starts_with("http://127.0.0.1:") || origin.starts_with("http://localhost:")).then_some(origin)
}

fn parent() -> Option<web_sys::Window> {
    web_sys::window()?.parent().ok().flatten()
}

fn post(origin: &str, kind: &str, name: Option<&str>, bytes: Option<&[u8]>) -> Result<(), String> {
    let msg = js_sys::Object::new();
    let set = |k: &str, v: &JsValue| js_sys::Reflect::set(&msg, &JsValue::from_str(k), v).map(|_| ()).map_err(|e| format!("{e:?}"));
    set("fossa", &JsValue::from_str(kind))?;
    if let Some(n) = name {
        set("name", &JsValue::from_str(n))?;
    }
    if let Some(b) = bytes {
        set("bytes", &js_sys::Uint8Array::from(b).into())?;
    }
    parent().ok_or("no embedding page")?.post_message(&msg, origin).map_err(|e| format!("{e:?}"))
}

/// Save and Export, when inside Fossa: the file goes back to Fossa, never to a download.
pub fn send_saved(origin: &str, path: &str, bytes: &[u8]) -> Result<(), String> {
    let name = std::path::Path::new(path).file_name().map_or_else(|| "photocraft".into(), |n| n.to_string_lossy().to_string());
    post(origin, "saved", Some(&name), Some(bytes))
}

/// Listen for files from Fossa and say we are ready for them.
pub fn listen(origin: String, inbox: Inbox, ctx: egui::Context) {
    use wasm_bindgen::closure::Closure;
    let Some(window) = web_sys::window() else { return };
    let from = origin.clone();
    let cb = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        if e.origin() != from {
            return;
        }
        let data = e.data();
        let get = |k: &str| js_sys::Reflect::get(&data, &JsValue::from_str(k)).ok();
        if get("fossa").and_then(|v| v.as_string()).as_deref() != Some("open") {
            return;
        }
        let name = get("name").and_then(|v| v.as_string()).unwrap_or_else(|| "from Fossa.png".into());
        let Some(bytes) = get("bytes").and_then(|v| v.dyn_into::<js_sys::Uint8Array>().ok()) else { return };
        inbox.lock().unwrap_or_else(|p| p.into_inner()).push((name, bytes.to_vec()));
        ctx.request_repaint();
    });
    if window.add_event_listener_with_callback("message", cb.as_ref().unchecked_ref()).is_ok() {
        cb.forget();
    }
    if let Err(e) = post(&origin, "ready", None, None) {
        log::error!("fossa bridge: {e}");
    }
}
