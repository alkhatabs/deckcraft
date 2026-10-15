//! Host-page bridge (Siraaj R&D): lets the page that embeds DeckCraft open a presentation, drive
//! the editor with control requests and receive saved bytes, over `postMessage`. It replaces the
//! desktop's TCP control port, which a browser cannot open.
//!
//! Parent → editor (`event.source` must be `window.parent`):
//! - `{type: "siraaj.open", name, bytes: ArrayBuffer}`: open a presentation.
//! - `{type: "siraaj.save"}`: reply with `siraaj.saved` carrying the active presentation as `.pptx`.
//! - `{type: "siraaj.control", id, method, params}`: one control request (`docs/control-protocol.md`).
//!
//! Editor → parent: `siraaj.ready`, `siraaj.reply {id, reply}`, `siraaj.saved {name, bytes}`,
//! `siraaj.dirty {dirty}`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};

use deckcraft_ui_egui::ControlRequest;
use serde_json::{Value, json};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;

type Pending = Rc<RefCell<Vec<(JsValue, Receiver<Value>)>>>;
pub type Opened = Rc<RefCell<Vec<(String, Vec<u8>)>>>;

pub struct Bridge {
    pending: Pending,
    pub opened: Opened,
    pub save_requested: Rc<Cell<bool>>,
}

fn post(msg: &JsValue, transfer: Option<&JsValue>) {
    let Some(window) = web_sys::window() else { return };
    let Some(parent) = window.parent().ok().flatten() else { return };
    // The host page is served from the same origin as the editor; never broadcast to others.
    let Ok(origin) = window.location().origin() else { return };
    let _ = match transfer {
        Some(t) => parent.post_message_with_transfer(msg, &origin, &js_sys::Array::of1(t)),
        None => parent.post_message(msg, &origin),
    };
}

fn message(ty: &str, fields: &[(&str, JsValue)]) -> JsValue {
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &"type".into(), &ty.into());
    for (k, v) in fields {
        let _ = js_sys::Reflect::set(&o, &(*k).into(), v);
    }
    o.into()
}

fn json_to_js(v: &Value) -> JsValue {
    js_sys::JSON::parse(&v.to_string()).unwrap_or(JsValue::NULL)
}

fn js_to_json(v: &JsValue) -> Value {
    js_sys::JSON::stringify(v).ok().and_then(|s| s.as_string()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null)
}

fn field(o: &JsValue, k: &str) -> JsValue {
    js_sys::Reflect::get(o, &k.into()).unwrap_or(JsValue::UNDEFINED)
}

/// Saved or exported bytes go to the parent page instead of a browser download.
pub fn save_to_parent(name: &str, bytes: &[u8]) {
    let buf = js_sys::Uint8Array::from(bytes).buffer();
    let msg = message("siraaj.saved", &[("name", name.into()), ("bytes", buf.clone().into())]);
    post(&msg, Some(&buf.into()));
}

pub fn report_dirty(dirty: bool) {
    post(&message("siraaj.dirty", &[("dirty", dirty.into())]), None);
}

/// Starts the listener. Control requests arrive on the returned receiver (give it to
/// `SlideApp::with_control`); call [`Bridge::poll_replies`] after each pass to answer them.
pub fn start(ctx: egui::Context) -> (Bridge, Receiver<ControlRequest>) {
    let (tx, rx): (Sender<ControlRequest>, Receiver<ControlRequest>) = std::sync::mpsc::channel();
    let pending: Pending = Rc::default();
    let opened: Opened = Rc::default();
    let save_requested: Rc<Cell<bool>> = Rc::default();
    let (pending_l, opened_l, save_l) = (pending.clone(), opened.clone(), save_requested.clone());
    let on_message = wasm_bindgen::closure::Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
        let parent = web_sys::window().and_then(|w| w.parent().ok().flatten());
        let from_parent = match (e.source(), parent) {
            (Some(s), Some(p)) => js_sys::Object::is(&s, &p),
            _ => false,
        };
        if !from_parent {
            return;
        }
        let data = e.data();
        let id = field(&data, "id");
        match field(&data, "type").as_string().as_deref() {
            Some("siraaj.open") => {
                let name = field(&data, "name").as_string().unwrap_or_else(|| "presentation.pptx".into());
                let bytes = js_sys::Uint8Array::new(&field(&data, "bytes")).to_vec();
                opened_l.borrow_mut().push((name, bytes));
            }
            Some("siraaj.save") => save_l.set(true),
            Some("siraaj.control") => {
                let method = field(&data, "method").as_string().unwrap_or_default();
                let params = Some(js_to_json(&field(&data, "params"))).filter(|v| !v.is_null()).unwrap_or(json!({}));
                let (req, reply_rx) = ControlRequest::new(method, params);
                if tx.send(req).is_ok() {
                    pending_l.borrow_mut().push((id, reply_rx));
                }
            }
            _ => {}
        }
        ctx.request_repaint();
    });
    if let Some(w) = web_sys::window() {
        let _ = w.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref());
    }
    on_message.forget();
    post(&message("siraaj.ready", &[]), None);
    (Bridge { pending, opened, save_requested }, rx)
}

impl Bridge {
    pub fn poll_replies(&self) {
        self.pending.borrow_mut().retain(|(id, rx)| match rx.try_recv() {
            Ok(v) => {
                post(&message("siraaj.reply", &[("id", id.clone()), ("reply", json_to_js(&v))]), None);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(_) => false,
        });
    }
}
