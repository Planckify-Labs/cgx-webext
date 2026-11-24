//! C ABI for the webext core.
//!
//! Threading: a handle is single-threaded. Call every function, and expect every callback, on the thread that created
//! it (in a browser: the UI/main thread of the Browser process).
//!
//! Strings: input strings are `WebExtStr` (pointer + length, UTF-8, not NUL-terminated, only valid for the duration of
//! the call). Output strings are `WebExtString`, allocated by this library, followed by a NUL byte that is not counted
//! in `len`, and must be released with `webext_string_free`. Callbacks that return a string must allocate it with `webext_string_from`.

use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use webext::bus::ContextInfo;
use webext::content_scripts::{Target, matching_content_scripts_json};
use webext::dispatch::{Dispatcher, InstallOptions};
use webext::host::{ApiCall, ApiError, ApiResult, ContextKind, ErrorKind, ExtensionHost, Reply};
use webext::loader::{Files, IdSource, load_extension};
use webext::manifest::Severity;
use webext::package::Package;
use webext::permissions::PermissionRequest;
use webext::registry::Registry;
use webext::resources::{Initiator, is_web_accessible};

pub const ABI_VERSION: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WebExtStr {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct WebExtString {
    pub ptr: *mut u8,
    pub len: usize,
    pub cap: usize,
}

impl WebExtString {
    const NULL: WebExtString = WebExtString { ptr: std::ptr::null_mut(), len: 0, cap: 0 };

    fn from_string(s: String) -> WebExtString {
        WebExtString::from_bytes(s.into_bytes())
    }

    /// Always stores a NUL byte after the `len` bytes (not counted in `len`), so C callers may treat text as a C string.
    fn from_bytes(mut b: Vec<u8>) -> WebExtString {
        let len = b.len();
        b.push(0);
        let mut v = std::mem::ManuallyDrop::new(b);
        WebExtString { ptr: v.as_mut_ptr(), len, cap: v.capacity() }
    }

    /// Takes ownership of a string produced by this library (`from_bytes` or `webext_string_from`).
    fn into_string(self) -> Option<String> {
        if self.ptr.is_null() {
            return None;
        }
        // SAFETY: allocated by `from_bytes` as a Vec<u8> of `len + 1` bytes (including the NUL) and `cap` capacity.
        let v = unsafe { Vec::from_raw_parts(self.ptr, self.len + 1, self.cap) };
        Some(String::from_utf8_lossy(&v[..self.len]).into_owned())
    }
}

fn str_of<'a>(s: WebExtStr) -> Option<&'a str> {
    if s.ptr.is_null() {
        return if s.len == 0 { Some("") } else { None };
    }
    // SAFETY: the caller guarantees `ptr` points to `len` readable bytes for the duration of the call.
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(s.ptr, s.len) }).ok()
}

fn view(s: &str) -> WebExtStr {
    WebExtStr { ptr: s.as_ptr(), len: s.len() }
}

#[repr(C)]
pub struct WebExtHostCallbacks {
    pub user_data: *mut c_void,
    /// Return a string allocated with `webext_string_from`, or a null string if nothing is stored.
    pub read_storage: Option<extern "C" fn(*mut c_void, WebExtStr, WebExtStr) -> WebExtString>,
    pub write_storage: Option<extern "C" fn(*mut c_void, WebExtStr, WebExtStr, WebExtStr)>,
    pub now_ms: Option<extern "C" fn(*mut c_void) -> i64>,
    pub ui_locale: Option<extern "C" fn(*mut c_void) -> WebExtString>,
    pub dispatch_event: Option<extern "C" fn(*mut c_void, WebExtStr, WebExtStr, WebExtStr)>,
    /// (user, context id, envelope JSON): hand the envelope to `api.handleHostMessage` in that context. Envelopes to one
    /// context must be delivered in order.
    pub deliver_to_context: Option<extern "C" fn(*mut c_void, u64, WebExtStr)>,
    /// (user, tab id) -> the full `tabs.Tab` JSON object for `MessageSender.tab`, allocated with `webext_string_from`, or a
    /// null string to send only the id.
    pub describe_tab: Option<extern "C" fn(*mut c_void, i64) -> WebExtString>,
    /// Implement the call described by the JSON, then report with `webext_complete(handle, token, ...)`.
    pub call_browser: Option<extern "C" fn(*mut c_void, WebExtStr, u64)>,
    /// Ask the user, then report with `webext_complete_permission(handle, token, granted)`.
    pub request_permissions: Option<extern "C" fn(*mut c_void, WebExtStr, WebExtStr, u64)>,
    pub schedule_alarm_check: Option<extern "C" fn(*mut c_void, i64)>,
}

type PermissionReply = Box<dyn FnOnce(bool)>;

struct CHost {
    cb: WebExtHostCallbacks,
    next_token: Cell<u64>,
    browser_replies: RefCell<HashMap<u64, Reply>>,
    permission_replies: RefCell<HashMap<u64, PermissionReply>>,
}

impl CHost {
    fn token(&self) -> u64 {
        let t = self.next_token.get() + 1;
        self.next_token.set(t);
        t
    }
}

impl ExtensionHost for CHost {
    fn read_storage(&self, id: &str, key: &str) -> Option<String> {
        (self.cb.read_storage?)(self.cb.user_data, view(id), view(key)).into_string()
    }

    fn write_storage(&self, id: &str, key: &str, json: &str) {
        if let Some(f) = self.cb.write_storage {
            f(self.cb.user_data, view(id), view(key), view(json));
        }
    }

    fn now_ms(&self) -> i64 {
        self.cb.now_ms.map_or(0, |f| f(self.cb.user_data))
    }

    fn ui_locale(&self) -> String {
        self.cb.ui_locale.and_then(|f| f(self.cb.user_data).into_string()).unwrap_or_else(|| "en-US".into())
    }

    fn dispatch_event(&self, id: &str, event: &str, args: Vec<Value>) {
        if let Some(f) = self.cb.dispatch_event {
            f(self.cb.user_data, view(id), view(event), view(&Value::Array(args).to_string()));
        }
    }

    fn deliver_to_context(&self, context_id: u64, message: Value) {
        if let Some(f) = self.cb.deliver_to_context {
            f(self.cb.user_data, context_id, view(&message.to_string()));
        }
    }

    fn describe_tab(&self, tab_id: i64) -> Option<Value> {
        let text = self.cb.describe_tab.and_then(|f| f(self.cb.user_data, tab_id).into_string())?;
        serde_json::from_str(&text).ok()
    }

    fn call_browser(&self, call: &ApiCall, reply: Reply) {
        let Some(f) = self.cb.call_browser else {
            return reply(Err(ApiError::new(ErrorKind::NotAvailable, "the host does not implement browser calls")));
        };
        let token = self.token();
        self.browser_replies.borrow_mut().insert(token, reply);
        f(self.cb.user_data, view(&call.to_json().to_string()), token);
    }

    fn request_permissions(&self, id: &str, request: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
        let Some(f) = self.cb.request_permissions else { return reply(false) };
        let token = self.token();
        self.permission_replies.borrow_mut().insert(token, reply);
        f(self.cb.user_data, view(id), view(&request.to_json().to_string()), token);
    }

    fn schedule_alarm_check(&self, at_ms: i64) {
        if let Some(f) = self.cb.schedule_alarm_check {
            f(self.cb.user_data, at_ms);
        }
    }
}

pub struct WebExtHandle {
    dispatcher: Dispatcher,
    host: Rc<CHost>,
}

fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn json_string(v: Value) -> WebExtString {
    WebExtString::from_string(v.to_string())
}

fn error_json(message: impl Into<String>) -> WebExtString {
    json_string(json!({ "ok": false, "errors": [{ "field": "", "message": message.into() }] }))
}

// ---- Library ----------------------------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn webext_abi_version() -> u32 {
    ABI_VERSION
}

/// Allocates a string owned by this library from bytes (for use by host callbacks that must return a string).
///
/// # Safety
/// `ptr` must point to `len` readable bytes (or be null with `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_string_from(ptr: *const u8, len: usize) -> WebExtString {
    if ptr.is_null() {
        return WebExtString::NULL;
    }
    // SAFETY: guaranteed by the caller.
    WebExtString::from_bytes(unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec())
}

/// # Safety
/// `s` must have been returned by this library and not freed before. Null strings are accepted.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_string_free(s: WebExtString) {
    drop(s.into_string());
}

/// Creates an instance. The callbacks are copied; `user_data` must outlive the handle.
///
/// # Safety
/// `callbacks` must point to a valid `WebExtHostCallbacks`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_create(callbacks: *const WebExtHostCallbacks) -> *mut WebExtHandle {
    if callbacks.is_null() {
        return std::ptr::null_mut();
    }
    guard(std::ptr::null_mut(), || {
        // SAFETY: non-null and valid per the contract; the struct is plain data so a bitwise read is a copy.
        let cb = unsafe { std::ptr::read(callbacks) };
        let host =
            Rc::new(CHost { cb, next_token: Cell::new(0), browser_replies: RefCell::default(), permission_replies: RefCell::default() });
        let dispatcher = Dispatcher::new(host.clone(), Registry::builtin());
        Box::into_raw(Box::new(WebExtHandle { dispatcher, host }))
    })
}

/// # Safety
/// `h` must come from `webext_create` and not be used afterwards. Pending replies are dropped without being called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_destroy(h: *mut WebExtHandle) {
    if !h.is_null() {
        // SAFETY: ownership returns to Rust.
        guard((), || drop(unsafe { Box::from_raw(h) }));
    }
}

fn handle<'a>(h: *mut WebExtHandle) -> Option<&'a WebExtHandle> {
    // SAFETY: the caller guarantees `h` is live (see each function's contract).
    unsafe { h.as_ref() }
}

/// Loads a `.crx`, `.xpi` or `.zip`, validates it and installs it.
///
/// Returns JSON: `{ "ok": true, "id", "displayName", "version", "manifestVersion", "idSource", "warnings": [...] }` or
/// `{ "ok": false, "errors": [{ "field", "message" }] }`. If the package carries no ID (an unpacked-style extension),
/// pass one in `id_override`; the override wins over any ID derived from the package. `base_url` is the origin the host
/// serves the extension from, with a trailing slash, e.g. `chrome-extension://<id>/`.
///
/// # Safety
/// `data` must point to `len` readable bytes; `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_install_package(
    h: *mut WebExtHandle,
    data: *const u8,
    len: usize,
    id_override: WebExtStr,
    base_url: WebExtStr,
) -> WebExtString {
    let Some(handle) = handle(h) else { return error_json("null handle") };
    let (Some(id_override), Some(base_url)) = (str_of(id_override), str_of(base_url)) else { return error_json("arguments must be UTF-8") };
    if data.is_null() {
        return error_json("null package data");
    }
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    // The fallback is built lazily: `WebExtString` has no destructor, so an eagerly built unused one would leak.
    catch_unwind(AssertUnwindSafe(|| install_package(handle, bytes, id_override, base_url)))
        .unwrap_or_else(|_| error_json("internal error while installing"))
}

fn diagnostics_json(diags: &[webext::manifest::Diagnostic]) -> Value {
    Value::Array(diags.iter().map(|d| json!({ "severity": if d.severity == Severity::Error { "error" } else { "warning" }, "field": d.field, "message": d.message })).collect())
}

fn install_package(h: &WebExtHandle, bytes: Vec<u8>, id_override: &str, base_url: &str) -> WebExtString {
    // A package with no key, CRX id or gecko id gets an id derived from its contents, stable for the same package.
    let seed: String = webext::encoding::sha256(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    let pkg = match Package::open(bytes) {
        Ok(p) => p,
        Err(e) => return error_json(e.to_string()),
    };
    let files = match Files::from_package(&pkg) {
        Ok(f) => f,
        Err(e) => return error_json(e.to_string()),
    };
    let crx_id = pkg.crx_header().and_then(|c| c.extension_id());
    // An .xpi addons.mozilla.org signed under an id it assigned (the manifest declares none) keeps that id, as in Firefox.
    let signed_id = pkg.mozilla_signed_id();
    let id_override = if id_override.is_empty() && crx_id.is_none() { signed_id.as_deref().unwrap_or("") } else { id_override };
    install_files(h, files, crx_id.as_deref(), id_override, base_url, Some(&seed))
}

/// Shared by package and directory installs. `derive_id_from` lets unpacked extensions without a key or gecko id get a
/// stable id (derived from the directory path, the way Chrome does it).
fn install_files(
    h: &WebExtHandle,
    files: Files,
    crx_id: Option<&str>,
    id_override: &str,
    base_url: &str,
    derive_id_from: Option<&str>,
) -> WebExtString {
    let ext = match load_extension(&files, crx_id) {
        Ok(e) => e,
        Err(diags) => return json_string(json!({ "ok": false, "errors": diagnostics_json(&diags) })),
    };
    let id = if !id_override.is_empty() {
        id_override.to_string()
    } else if let Some(id) = ext.id.clone() {
        id
    } else if let Some(seed) = derive_id_from {
        webext::encoding::chrome_extension_id_from_public_key(seed.as_bytes())
    } else {
        return error_json("the package declares no extension ID; pass id_override");
    };
    if base_url.is_empty() {
        return error_json("base_url must not be empty (use \"auto\" to let the library choose)");
    }
    let base_url = if base_url == "auto" { webext::serve::default_base_url(&id, &ext.manifest) } else { base_url.to_string() };
    let result = json!({
        "ok": true,
        "id": id,
        "baseUrl": base_url,
        "displayName": ext.display_name,
        "version": ext.manifest.version,
        "manifestVersion": ext.manifest.manifest_version,
        "idSource": match ext.id_source { Some(IdSource::ManifestKey) => "manifest_key", Some(IdSource::CrxHeader) => "crx_header", Some(IdSource::GeckoId) => "gecko_id", None if derive_id_from.is_some() && id_override.is_empty() => "derived", None => "override" },
        "warnings": diagnostics_json(&ext.diagnostics),
    });
    h.dispatcher.install(&id, ext.manifest, InstallOptions { base_url, locales: ext.locales, files });
    json_string(result)
}

/// Installs an unpacked extension from a directory (like Chrome's "Load unpacked" and `--load-extension`). Same result
/// JSON as `webext_install_package`. Without a manifest `key` or gecko id the id is derived from the directory path.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_install_directory(
    h: *mut WebExtHandle,
    path: WebExtStr,
    id_override: WebExtStr,
    base_url: WebExtStr,
) -> WebExtString {
    let Some(handle) = handle(h) else { return error_json("null handle") };
    let (Some(path), Some(id_override), Some(base_url)) = (str_of(path), str_of(id_override), str_of(base_url)) else {
        return error_json("arguments must be UTF-8");
    };
    catch_unwind(AssertUnwindSafe(|| {
        let canonical = std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string());
        match Files::from_dir(std::path::Path::new(path)) {
            Ok(files) => install_files(handle, files, None, id_override, base_url, Some(&canonical)),
            Err(e) => error_json(format!("cannot read '{path}': {e}")),
        }
    }))
    .unwrap_or_else(|_| error_json("internal error while installing"))
}

/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_uninstall(h: *mut WebExtHandle, id: WebExtStr) -> bool {
    let (Some(handle), Some(id)) = (handle(h), str_of(id)) else { return false };
    guard(false, || handle.dispatcher.uninstall(id))
}

/// JSON for the JavaScript shim (see `Shim/webext-shim.js`), or a null string if the extension is unknown.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_init_data(h: *mut WebExtHandle, id: WebExtStr) -> WebExtString {
    let (Some(handle), Some(id)) = (handle(h), str_of(id)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || handle.dispatcher.init_data_for(id).map_or(WebExtString::NULL, json_string))
}

/// Registers a place extension code runs, before any API call is made from it. `json`:
/// `{ "id": 7, "extensionId": "...", "kind": "background|extension_page|content_script|other", "tabId": 5, "frameId": 0,
/// "url": "..." }` (`tabId`, `frameId` and `url` optional). Ids are chosen by the browser. Returns false if the JSON is
/// invalid, the extension is not installed or the id is taken.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_context_created(h: *mut WebExtHandle, json: WebExtStr) -> bool {
    let (Some(handle), Some(text)) = (handle(h), str_of(json)) else { return false };
    guard(false, || {
        let Ok(v) = serde_json::from_str::<Value>(text) else { return false };
        let (Some(id), Some(extension_id)) = (v.get("id").and_then(Value::as_u64), v.get("extensionId").and_then(Value::as_str)) else {
            return false;
        };
        let Some(kind) = v.get("kind").and_then(Value::as_str).and_then(ContextKind::parse) else { return false };
        let info = ContextInfo {
            id,
            extension_id: extension_id.to_string(),
            kind,
            tab_id: v.get("tabId").and_then(Value::as_i64),
            frame_id: v.get("frameId").and_then(Value::as_i64),
            url: v.get("url").and_then(Value::as_str).map(String::from),
        };
        handle.dispatcher.context_created(info).is_ok()
    })
}

/// The context is gone (tab closed, navigated, background shut down). Ports on it are closed and messages waiting for it
/// are settled. Calling it for an unknown id is harmless.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_context_destroyed(h: *mut WebExtHandle, context_id: u64) {
    if let Some(handle) = handle(h) {
        guard((), || handle.dispatcher.context_destroyed(context_id));
    }
}

/// Like `webext_init_data` for one registered context: includes `contextId` and `contextKind`, and trims the API surface for
/// content scripts. Null string if the context is unknown.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_init_data_for_context(h: *mut WebExtHandle, context_id: u64) -> WebExtString {
    let Some(handle) = handle(h) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || handle.dispatcher.init_data_for_context(context_id).map_or(WebExtString::NULL, json_string))
}

/// What the browser needs to host an extension (background context, CSP, toolbar action, options page, icons, permissions)
/// as JSON, or a null string if the extension is unknown.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_extension_info(h: *mut WebExtHandle, id: WebExtStr) -> WebExtString {
    let (Some(handle), Some(id)) = (handle(h), str_of(id)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || handle.dispatcher.info_for(id).map_or(WebExtString::NULL, json_string))
}

/// Which content scripts to run in a document: `[{ index, js, css, runAt, world, allFrames }]`.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_content_scripts(
    h: *mut WebExtHandle,
    id: WebExtStr,
    url: WebExtStr,
    is_top_frame: bool,
    fallback_url: WebExtStr,
) -> WebExtString {
    let (Some(handle), Some(id), Some(url), Some(fallback)) = (handle(h), str_of(id), str_of(url), str_of(fallback_url)) else {
        return WebExtString::NULL;
    };
    guard(WebExtString::NULL, || {
        let target = Target { url, is_top_frame, fallback_url: Some(fallback).filter(|f| !f.is_empty()) };
        handle
            .dispatcher
            .with_extension(id, |e| json_string(matching_content_scripts_json(&e.manifest, &target)))
            .unwrap_or(WebExtString::NULL)
    })
}

/// `initiator_kind`: 0 = the extension itself, 1 = a web page (`initiator` is its URL), 2 = another extension
/// (`initiator` is its ID).
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_is_web_accessible(
    h: *mut WebExtHandle,
    id: WebExtStr,
    path: WebExtStr,
    initiator_kind: u32,
    initiator: WebExtStr,
) -> bool {
    let (Some(handle), Some(id), Some(path), Some(initiator)) = (handle(h), str_of(id), str_of(path), str_of(initiator)) else {
        return false;
    };
    guard(false, || {
        let who = match initiator_kind {
            0 => Initiator::SameExtension,
            1 => Initiator::Web(initiator),
            2 => Initiator::OtherExtension(initiator),
            _ => return false,
        };
        handle.dispatcher.with_extension(id, |e| is_web_accessible(&e.manifest, path, &who)).unwrap_or(false)
    })
}

/// Reads a file from an installed extension. Returns false if the extension or file does not exist. The caller must
/// check `webext_is_web_accessible` first when the request comes from a web page.
///
/// # Safety
/// `h` must be live; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_read_file(h: *mut WebExtHandle, id: WebExtStr, path: WebExtStr, out: *mut WebExtString) -> bool {
    let (Some(handle), Some(id), Some(path)) = (handle(h), str_of(id), str_of(path)) else { return false };
    if out.is_null() {
        return false;
    }
    guard(false, || {
        match handle.dispatcher.with_extension(id, |e| e.files.get(path).map(<[u8]>::to_vec)).flatten() {
            Some(bytes) => {
                // SAFETY: `out` is writable per the contract.
                unsafe { out.write(WebExtString::from_bytes(bytes)) };
                true
            }
            None => false,
        }
    })
}

/// Answers a request for an extension URL such as `chrome-extension://<id>/popup.html`.
///
/// `initiator` is the serialized origin of the requester (`https://example.com`, `chrome-extension://<id>`, `null`), or empty
/// for a navigation the browser started itself. Returns a JSON object
/// `{ "status": 200|403|404, "mime": "...", "csp": "..."|null, "extensionId": "..." }` and writes the body to `body_out`,
/// or returns a null string if the URL belongs to no installed extension (the caller then treats it as a network error).
/// Extension pages are served with the shim and bootstrap scripts inserted, and web pages only get what
/// `web_accessible_resources` allows. Never serve a result for a requester you have not identified correctly.
///
/// # Safety
/// `h` must be live; `body_out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_serve(
    h: *mut WebExtHandle,
    url: WebExtStr,
    initiator: WebExtStr,
    body_out: *mut WebExtString,
) -> WebExtString {
    let (Some(handle), Some(url), Some(initiator)) = (handle(h), str_of(url), str_of(initiator)) else { return WebExtString::NULL };
    if body_out.is_null() {
        return WebExtString::NULL;
    }
    guard(WebExtString::NULL, || match handle.dispatcher.serve(url, Some(initiator).filter(|i| !i.is_empty())) {
        Some(served) => {
            // SAFETY: `body_out` is writable per the contract.
            unsafe { body_out.write(WebExtString::from_bytes(served.body)) };
            json_string(json!({ "status": served.status, "mime": served.mime, "csp": served.csp, "extensionId": served.extension_id }))
        }
        None => WebExtString::NULL,
    })
}

/// The content scripts of every installed extension that match a document, with their sources inlined:
/// `[{ "extensionId", "runAt", "world", "js": [{ "url", "source" }], "css": ["source"] }]`. Never null.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_content_script_bundles(
    h: *mut WebExtHandle,
    url: WebExtStr,
    is_top_frame: bool,
    fallback_url: WebExtStr,
) -> WebExtString {
    let empty = || WebExtString::from_string("[]".into());
    let (Some(handle), Some(url), Some(fallback)) = (handle(h), str_of(url), str_of(fallback_url)) else { return empty() };
    catch_unwind(AssertUnwindSafe(|| {
        json_string(handle.dispatcher.content_script_bundles(url, is_top_frame, Some(fallback).filter(|f| !f.is_empty())))
    }))
    .unwrap_or_else(|_| empty())
}

/// The content-script programs for a document (see `Dispatcher::content_script_programs`): `[{ "extensionId", "world", "program" }]`.
/// Never null.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_content_script_programs(
    h: *mut WebExtHandle,
    url: WebExtStr,
    is_top_frame: bool,
    fallback_url: WebExtStr,
) -> WebExtString {
    let empty = || WebExtString::from_string("[]".into());
    let (Some(handle), Some(url), Some(fallback)) = (handle(h), str_of(url), str_of(fallback_url)) else { return empty() };
    catch_unwind(AssertUnwindSafe(|| {
        json_string(handle.dispatcher.content_script_programs(url, is_top_frame, Some(fallback).filter(|f| !f.is_empty())))
    }))
    .unwrap_or_else(|_| empty())
}

/// The URL to load the extension's background context from, or a null string if it has none.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_background_url(h: *mut WebExtHandle, id: WebExtStr) -> WebExtString {
    let (Some(handle), Some(id)) = (handle(h), str_of(id)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || handle.dispatcher.background_url(id).map_or(WebExtString::NULL, WebExtString::from_string))
}

/// Whether a document at `url` is the extension's background context rather than a popup, options page or tab.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_is_background_url(h: *mut WebExtHandle, id: WebExtStr, url: WebExtStr) -> bool {
    let (Some(handle), Some(id), Some(url)) = (handle(h), str_of(id), str_of(url)) else { return false };
    guard(false, || handle.dispatcher.is_background_url(id, url))
}

/// The extension that owns an extension URL (`chrome-extension://...`), or a null string. Lets the browser tell extension
/// pages from other pages, for example to decide what kind of context a new document is.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_extension_for_url(h: *mut WebExtHandle, url: WebExtStr) -> WebExtString {
    let (Some(handle), Some(url)) = (handle(h), str_of(url)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || handle.dispatcher.extension_for_url(url).map_or(WebExtString::NULL, WebExtString::from_string))
}

/// Callback for `webext_dispatch`. `ok` selects between a result JSON value and an error JSON object
/// `{ "kind", "message" }`. The string is valid only during the callback. Called exactly once, possibly before
/// `webext_dispatch` returns.
pub type WebExtReplyFn = extern "C" fn(*mut c_void, bool, WebExtStr);

/// Runs an API call (`ApiCall` JSON, see `ApiCall::to_json`).
///
/// # Safety
/// `h` must be live; `user` must stay valid until `on_reply` runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_dispatch(h: *mut WebExtHandle, call_json: WebExtStr, on_reply: Option<WebExtReplyFn>, user: *mut c_void) {
    let Some(on_reply) = on_reply else { return };
    let respond = move |r: ApiResult| match r {
        Ok(v) => on_reply(user, true, view(&v.to_string())),
        Err(e) => on_reply(user, false, view(&e.to_json().to_string())),
    };
    let (Some(handle), Some(text)) = (handle(h), str_of(call_json)) else {
        return respond(Err(ApiError::invalid("bad arguments")));
    };
    let call = match serde_json::from_str::<Value>(text).map_err(|e| e.to_string()).and_then(|v| ApiCall::from_json(&v)) {
        Ok(c) => c,
        Err(e) => return respond(Err(ApiError::invalid(e))),
    };
    let dispatcher = handle.dispatcher.clone();
    if catch_unwind(AssertUnwindSafe(|| dispatcher.dispatch(call, Box::new(respond)))).is_err() {
        // The reply closure was consumed by the panicking call; nothing more can be reported safely.
    }
}

/// One stage of a network request (see `webext::network`). `on_reply` receives the instruction for the browser as JSON
/// (`{}` to go on, or `cancel`, `redirectUrl`, `requestHeaders`, `responseHeaders`, `authCredentials`): at once, or
/// after the extensions' blocking `webRequest` listeners have answered.
///
/// # Safety
/// `h` must be live; `user` must stay valid until `on_reply` runs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_network_event(
    h: *mut WebExtHandle,
    stage: WebExtStr,
    details_json: WebExtStr,
    on_reply: Option<WebExtReplyFn>,
    user: *mut c_void,
) {
    let Some(on_reply) = on_reply else { return };
    let respond = move |r: ApiResult| match r {
        Ok(v) => on_reply(user, true, view(&v.to_string())),
        Err(e) => on_reply(user, false, view(&e.to_json().to_string())),
    };
    let (Some(handle), Some(stage), Some(text)) = (handle(h), str_of(stage), str_of(details_json)) else {
        return respond(Err(ApiError::invalid("bad arguments")));
    };
    let details = match serde_json::from_str::<Value>(text) {
        Ok(v) => v,
        Err(e) => return respond(Err(ApiError::invalid(e.to_string()))),
    };
    let dispatcher = handle.dispatcher.clone();
    let _ = catch_unwind(AssertUnwindSafe(|| dispatcher.network_event(stage, &details, Box::new(respond))));
}

/// Completes a `call_browser` request. `json` is the result value when `ok`, otherwise an error message string (plain
/// text, not JSON).
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_complete(h: *mut WebExtHandle, token: u64, ok: bool, json: WebExtStr) {
    let (Some(handle), Some(text)) = (handle(h), str_of(json)) else { return };
    let reply = handle.host.browser_replies.borrow_mut().remove(&token);
    if let Some(reply) = reply {
        let result = if ok {
            serde_json::from_str::<Value>(if text.is_empty() { "null" } else { text })
                .map_err(|e| ApiError::runtime(format!("host returned invalid JSON: {e}")))
        } else {
            Err(ApiError::runtime(text))
        };
        reply(result);
    }
}

/// Completes a `request_permissions` request.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_complete_permission(h: *mut WebExtHandle, token: u64, granted: bool) {
    let Some(handle) = handle(h) else { return };
    let reply = handle.host.permission_replies.borrow_mut().remove(&token);
    if let Some(reply) = reply {
        reply(granted);
    }
}

/// Fires due alarms; call when the timer armed via `schedule_alarm_check` elapses.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_tick(h: *mut WebExtHandle) {
    if let Some(handle) = handle(h) {
        guard((), || handle.dispatcher.tick());
    }
}

/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_notify_installed(h: *mut WebExtHandle, id: WebExtStr, reason: WebExtStr, previous_version: WebExtStr) {
    let (Some(handle), Some(id), Some(reason), Some(prev)) = (handle(h), str_of(id), str_of(reason), str_of(previous_version)) else {
        return;
    };
    guard((), || handle.dispatcher.notify_installed(id, reason, Some(prev).filter(|p| !p.is_empty())));
}

/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_notify_startup(h: *mut WebExtHandle, id: WebExtStr) {
    if let (Some(handle), Some(id)) = (handle(h), str_of(id)) {
        guard((), || handle.dispatcher.notify_startup(id));
    }
}

/// What the browser tells the core about itself, or asks it, outside an extension's API call: `browserState` (a snapshot of
/// windows and tabs), `uiState`, `actionClicked`, `contextMenuItems`, `contextMenuClicked`, `commands`, `command`,
/// `setShortcut`, `setActionPinned`, `grantActiveTab`. `args_json` is a JSON object. Returns JSON (`null` when there is
/// nothing to say, `{"error": ...}` on bad input). See `Dispatcher::host_event`.
///
/// # Safety
/// `h` must be a live handle; the strings must be valid for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_host_event(h: *mut WebExtHandle, name: WebExtStr, args_json: WebExtStr) -> WebExtString {
    let (Some(handle), Some(name), Some(args)) = (handle(h), str_of(name), str_of(args_json)) else { return WebExtString::NULL };
    let Ok(args) = serde_json::from_str::<Value>(args) else {
        return WebExtString::from_string(json!({ "error": "args must be JSON" }).to_string());
    };
    guard(WebExtString::NULL, || WebExtString::from_string(handle.dispatcher.host_event(name, &args).to_string()))
}

// ---- Extension stores (see webext::store) ----

/// `"chrome"` for Chrome Web Store pages, `"firefox"` for addons.mozilla.org pages, otherwise a null string.
///
/// # Safety
/// `url` must be readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_store_for_url(url: WebExtStr) -> WebExtString {
    let Some(url) = str_of(url) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || {
        webext::store::store_for_url(url).map_or(WebExtString::NULL, |s| WebExtString::from_string(s.as_str().into()))
    })
}

/// The `chrome.management.ExtensionInfo` of every installed extension (JSON array), or of one (`id` not empty; a null
/// string if it is not installed).
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_store_management_info(h: *mut WebExtHandle, id: WebExtStr) -> WebExtString {
    let (Some(handle), Some(id)) = (handle(h), str_of(id)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || {
        if id.is_empty() {
            json_string(handle.dispatcher.management_list())
        } else {
            handle.dispatcher.management_info(id).map_or(WebExtString::NULL, json_string)
        }
    })
}

/// The addons.mozilla.org `Addon` fields of an installed extension, or a null string.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_store_amo_addon(h: *mut WebExtHandle, id: WebExtStr) -> WebExtString {
    let (Some(handle), Some(id)) = (handle(h), str_of(id)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || handle.dispatcher.amo_addon(id).map_or(WebExtString::NULL, json_string))
}

/// What to ask the user before installing from a manifest a store sent: `{ "ok", "name", "version", "warnings" }` or
/// `{ "ok": false, "error" }`. If `id` names an installed extension instead (and `manifest_json` is empty), its warnings.
///
/// # Safety
/// `h` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_store_preview_manifest(
    h: *mut WebExtHandle,
    manifest_json: WebExtStr,
    localized_name: WebExtStr,
    id: WebExtStr,
) -> WebExtString {
    let (Some(handle), Some(manifest), Some(name), Some(id)) = (handle(h), str_of(manifest_json), str_of(localized_name), str_of(id))
    else {
        return error_json("arguments must be UTF-8");
    };
    guard(WebExtString::NULL, || {
        if manifest.is_empty() {
            let warnings = handle.dispatcher.with_extension(id, |e| webext::store::permission_warnings(&e.manifest));
            return match warnings {
                Some(w) => json_string(json!({ "ok": true, "warnings": w })),
                None => json_string(json!({ "ok": false, "error": "not installed" })),
            };
        }
        json_string(webext::store::preview_manifest(manifest, name))
    })
}

/// Checks a package downloaded from a store before installing it (see `webext::store::check_store_package`). Empty
/// strings mean "no expectation". Returns `{ "ok": true, "id", "name", "version", "warnings" }` or
/// `{ "ok": false, "error" }`.
///
/// # Safety
/// `data` must point to `len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_store_check_package(
    data: *const u8,
    len: usize,
    expected_id: WebExtStr,
    expected_hash: WebExtStr,
    approved_manifest: WebExtStr,
) -> WebExtString {
    let (Some(id), Some(hash), Some(manifest)) = (str_of(expected_id), str_of(expected_hash), str_of(approved_manifest)) else {
        return json_string(json!({ "ok": false, "error": "arguments must be UTF-8" }));
    };
    if data.is_null() {
        return json_string(json!({ "ok": false, "error": "no package" }));
    }
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    let expectation = webext::store::StoreExpectation {
        id: Some(id).filter(|s| !s.is_empty()),
        hash: Some(hash).filter(|s| !s.is_empty()),
        approved_manifest: Some(manifest).filter(|s| !s.is_empty()),
    };
    catch_unwind(AssertUnwindSafe(|| match webext::store::check_store_package(bytes, &expectation) {
        Ok(summary) => json_string(summary.to_json()),
        Err(error) => json_string(json!({ "ok": false, "error": error })),
    }))
    .unwrap_or_else(|_| json_string(json!({ "ok": false, "error": "internal error while checking the package" })))
}

/// Where the Chrome Web Store serves an extension's package (a URL that redirects to the .crx), or a null string for an
/// invalid id.
///
/// # Safety
/// The strings must be readable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn webext_store_chrome_download_url(id: WebExtStr, chrome_version: WebExtStr) -> WebExtString {
    let (Some(id), Some(version)) = (str_of(id), str_of(chrome_version)) else { return WebExtString::NULL };
    guard(WebExtString::NULL, || {
        webext::store::chrome_web_store_download_url(id, version).map_or(WebExtString::NULL, WebExtString::from_string)
    })
}
