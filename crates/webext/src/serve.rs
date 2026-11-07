//! Serving an extension's files to the browser: URL → extension → file, with MIME types, the extension CSP, the
//! `web_accessible_resources` rules and the scripts that give extension pages their `chrome.*` / `browser.*`.

use crate::dispatch::{Dispatcher, Extension};
use crate::resources::{Initiator, is_web_accessible};

/// Where the shim and the bootstrap are served from. Only extension pages (the extension's own origin) may load them.
pub const VIRTUAL_PREFIX: &str = "__cgx__/";
/// The generated page that hosts background scripts and service workers.
pub const BACKGROUND_PAGE: &str = "__cgx__/background.html";
pub const SHIM_JS: &str = include_str!("../../../Shim/webext-shim.js");
pub const BOOT_JS: &str = include_str!("../../../Shim/boot.js");

fn html_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

/// The page that runs a `background.scripts` list or a `background.service_worker` as an event page. Paths are absolute so
/// that the page can live under `/__cgx__/`.
pub fn generate_background_page(manifest: &crate::manifest::Manifest) -> String {
    use crate::manifest::BackgroundKind;
    let b = &manifest.background;
    let (scripts, module): (Vec<&String>, bool) = match b.kind {
        BackgroundKind::Scripts => (b.scripts.iter().collect(), b.is_module),
        BackgroundKind::ServiceWorker => (vec![&b.service_worker], b.is_module),
        // A background page is served from its own file, not generated.
        BackgroundKind::Page | BackgroundKind::None => (vec![], false),
    };
    let mut html = String::from("<!doctype html><html><head><meta charset=\"utf-8\"><title>Background</title></head><body>");
    for script in scripts {
        let src = format!("/{}", script.trim_start_matches(['/', '.']).trim_start_matches('/'));
        html.push_str(&format!("<script{} src=\"{}\"></script>", if module { " type=\"module\"" } else { "" }, html_escape_attr(&src)));
    }
    html.push_str("</body></html>");
    html
}

/// Embeds a string in JavaScript source safely, including `</script>` and line separators.
fn js_string(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string().replace("\u{2028}", "\\u2028").replace("\u{2029}", "\\u2029").replace("</", "<\\/")
}

fn build_content_script_program(bundles: &[&serde_json::Value]) -> String {
    let mut stages: Vec<(&str, String, Vec<String>)> =
        vec![("document_start", String::new(), vec![]), ("document_end", String::new(), vec![]), ("document_idle", String::new(), vec![])];
    for bundle in bundles {
        let stage = bundle["runAt"].as_str().unwrap_or("document_idle");
        let Some(slot) = stages.iter_mut().find(|(s, _, _)| *s == stage) else { continue };
        for file in bundle["js"].as_array().into_iter().flatten() {
            let source = file["source"].as_str().unwrap_or_default();
            let url = file["url"].as_str().unwrap_or_default();
            // Files of one stage share one function scope, as files of one content-script world share one global scope.
            slot.1.push_str(&format!("\n// ---- {} ----\n{}\n;", url.replace(['\n', '\r'], " "), source));
        }
        for css in bundle["css"].as_array().into_iter().flatten() {
            slot.2.push(js_string(css.as_str().unwrap_or_default()));
        }
    }
    // Deliberately not strict: content scripts are often sloppy-mode code, and nested functions inherit strictness.
    let mut out = String::from("(function (bridge) {\nvar module = { exports: {} };\n(function (module) {\n");
    out.push_str(SHIM_JS);
    out.push_str(
        "\n})(module);\nvar shim = module.exports;\nvar init = JSON.parse(bridge.contextInit());\nif (!init) return;\nvar api = shim.createApi(init, {\n  call: function (name, args) { return bridge.call(name, JSON.stringify(args === undefined ? [] : args)).then(function (t) { return t === '' ? undefined : JSON.parse(t); }); },\n  callSync: function (name, args) { var t = bridge.callSync(name, JSON.stringify(args === undefined ? [] : args)); return t === '' ? undefined : JSON.parse(t); }\n});\nfunction addStyles(list) {\n  if (!list.length) return;\n  var apply = function () { var root = document.head || document.documentElement; if (!root) return false; list.forEach(function (css) { var s = document.createElement('style'); s.textContent = css; root.appendChild(s); }); return true; };\n  if (!apply()) document.addEventListener('readystatechange', function once() { if (apply()) document.removeEventListener('readystatechange', once); });\n}\n",
    );
    out.push_str("var stageOrder = ['document_start', 'document_end', 'document_idle'];\nvar stageStyles = [");
    out.push_str(&stages.iter().map(|(_, _, css)| format!("[{}]", css.join(", "))).collect::<Vec<_>>().join(", "));
    out.push_str("];\n");
    // All stages are one generator function, paused between stages, so that every file of every stage shares one scope:
    // in Chrome and Firefox the content scripts of an extension share a global scope, and extensions rely on top-level
    // declarations of one file being visible in later ones. `chrome` and `browser` are parameters, so the scripts see this
    // extension's API, not the page's or another extension's. It is a function expression, not `new Function`: a page
    // whose CSP lacks 'unsafe-eval' would block `new Function`, and a page that replaced `Function` could intercept it.
    // LIMITATION: a script that uses `yield` as an identifier does not parse; an uncaught error ends later stages.
    out.push_str("var scripts = (function* (chrome, browser) {\n");
    for (i, (_, code, _)) in stages.iter().enumerate() {
        out.push_str(code);
        out.push('\n');
        if i + 1 < stages.len() {
            out.push_str("yield;\n");
        }
    }
    out.push_str(
        "}).call(globalThis, api.chrome, api.browser);\n\
var reached = -1;\n\
function runStage(name) {\n\
  var target = stageOrder.indexOf(name);\n\
  while (reached < target) {\n\
    reached++;\n\
    addStyles(stageStyles[reached]);\n\
    try { scripts.next(); } catch (e) { console.error(e); }\n\
  }\n\
}\n\
bridge.setMessageHandler(function (text) {\n\
  var m = JSON.parse(text);\n\
  if (m.kind === 'run') runStage(m.stage);\n\
  else if (m.kind === 'event') api.dispatchEvent(m.name, m.args);\n\
  else if (m.kind === 'envelope') api.handleHostMessage(m.envelope);\n\
});\n\
// In Chrome and Firefox, document_start scripts see the document element. The browser calls this program before the\n\
// parser has inserted it, so wait for it: mutation observers run before the parser runs the first page script.\n\
if (document.documentElement) {\n\
  runStage('document_start');\n\
} else {\n\
  var observer = new MutationObserver(function () { if (document.documentElement) { observer.disconnect(); runStage('document_start'); } });\n\
  observer.observe(document, { childList: true });\n\
}\n\
})",
    );
    out
}

const INJECTED_TAGS: &str = "<script src=\"/__cgx__/shim.js\"></script><script src=\"/__cgx__/boot.js\"></script>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub status: u16,
    pub mime: String,
    /// `Content-Security-Policy` to send, for extension pages.
    pub csp: Option<String>,
    pub extension_id: String,
    pub body: Vec<u8>,
}

impl Served {
    fn error(status: u16, extension_id: &str, message: &str) -> Served {
        Served {
            status,
            mime: "text/plain; charset=utf-8".into(),
            csp: None,
            extension_id: extension_id.into(),
            body: message.as_bytes().to_vec(),
        }
    }
}

/// `scheme://host/path?query#fragment` → (`scheme://host/`, `path`) with the path percent-decoded; `None` for the path if it
/// cannot be decoded (the URL still belongs to that origin, it just names no file).
fn split_extension_url(url: &str) -> Option<(String, Option<String>)> {
    let (scheme, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(end);
    let path = tail.split(['?', '#']).next().unwrap_or("");
    let path = path.strip_prefix('/').unwrap_or(path);
    Some((format!("{}://{}/", scheme.to_ascii_lowercase(), host.to_ascii_lowercase()), percent_decode(path)))
}

fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn mime_for_path(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Inserts the shim and bootstrap tags so they run before any script of the page.
pub fn inject_boot_scripts(html: &str) -> String {
    if html.contains("/__cgx__/boot.js") {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    // After the opening <head ...> if there is one, else after <html ...>, else at the very start.
    for tag in ["<head", "<html"] {
        if let Some(start) = lower.find(tag) {
            let after_name = start + tag.len();
            // `<header` or `<htmlx` must not match.
            if lower.as_bytes().get(after_name).is_some_and(|b| b.is_ascii_alphanumeric()) {
                continue;
            }
            if let Some(close) = lower[after_name..].find('>') {
                let at = after_name + close + 1;
                return format!("{}{}{}", &html[..at], INJECTED_TAGS, &html[at..]);
            }
        }
    }
    format!("{INJECTED_TAGS}{html}")
}

fn initiator_of<'a>(base_origin: &str, initiator: Option<&'a str>) -> Option<Initiator<'a>> {
    let Some(origin) = initiator else {
        // Navigation started by the browser itself (typed URL, command line, extensions manager).
        return Some(Initiator::SameExtension);
    };
    let origin_with_slash = if origin.ends_with('/') { origin.to_string() } else { format!("{origin}/") };
    if origin_with_slash.eq_ignore_ascii_case(base_origin) {
        return Some(Initiator::SameExtension);
    }
    for scheme in ["chrome-extension://", "moz-extension://"] {
        if origin.to_ascii_lowercase().starts_with(scheme) {
            // Another extension: identified by its (extension id) host.
            return Some(Initiator::OtherExtension(origin[scheme.len()..].trim_end_matches('/')));
        }
    }
    Some(Initiator::Web(origin))
}

/// A UUID-shaped, URL-safe host derived from an extension id that is not itself URL-safe (Firefox ids such as
/// `ublock@example.org`). Firefox uses random UUIDs; a derived one is stable across runs without having to persist anything.
fn uuid_host(id: &str) -> String {
    let h = crate::encoding::sha256(id.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    b[6] = (b[6] & 0x0F) | 0x40; // version 4 layout
    b[8] = (b[8] & 0x3F) | 0x80; // RFC 4122 variant
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// The origin an extension is served from: `chrome-extension://<id>/` for Chrome-style extensions and
/// `moz-extension://<uuid>/` for Firefox-style ones (those with a gecko id, or that only use Firefox features).
pub fn default_base_url(id: &str, manifest: &crate::manifest::Manifest) -> String {
    use crate::manifest::Dialect;
    let firefox = match manifest.dialect {
        Dialect::Firefox => true,
        Dialect::Chrome => false,
        Dialect::Both => !manifest.gecko_id.is_empty(),
    };
    if firefox {
        format!("moz-extension://{}/", uuid_host(id))
    } else if crate::encoding::is_valid_chrome_extension_id(id) {
        format!("chrome-extension://{id}/")
    } else {
        format!("chrome-extension://{}/", crate::encoding::chrome_extension_id_from_public_key(id.as_bytes()))
    }
}

impl Dispatcher {
    /// Where to load an extension's background context, if it has one: its own page, or the generated page that runs its
    /// scripts or service worker.
    pub fn background_url(&self, id: &str) -> Option<String> {
        use crate::manifest::BackgroundKind;
        self.with_extension(id, |ext| {
            let b = &ext.manifest.background;
            match b.kind {
                BackgroundKind::None => None,
                BackgroundKind::Page => Some(format!("{}{}", ext.base_url, b.page.trim_start_matches(['/', '.']).trim_start_matches('/'))),
                BackgroundKind::Scripts | BackgroundKind::ServiceWorker => Some(format!("{}{BACKGROUND_PAGE}", ext.base_url)),
            }
        })
        .flatten()
    }

    /// Whether a document at `url` is the extension's background context (as opposed to a popup, options page or tab).
    pub fn is_background_url(&self, id: &str, url: &str) -> bool {
        let Some(background) = self.background_url(id) else { return false };
        let strip = |u: &str| u.split(['?', '#']).next().unwrap_or("").to_ascii_lowercase();
        strip(url) == strip(&background)
    }

    /// Everything the renderer needs to run the content scripts that match a document, for every installed extension:
    /// `[{ "extensionId", "runAt", "world", "js": [{ "url", "source" }], "css": ["source", ...] }]`, in installation order
    /// and manifest order. Sources are inlined so the renderer needs no further round trips.
    pub fn content_script_bundles(&self, url: &str, is_top_frame: bool, fallback_url: Option<&str>) -> serde_json::Value {
        use crate::content_scripts::{Target, matching_content_scripts};
        use crate::manifest::{RunAt, ScriptWorld};
        let target = Target { url, is_top_frame, fallback_url };
        let mut bundles = vec![];
        for id in self.installed_ids() {
            self.with_extension(&id, |ext| {
                for (_, script) in matching_content_scripts(&ext.manifest, &target) {
                    let js: Vec<serde_json::Value> = script
                        .js
                        .iter()
                        .filter_map(|path| {
                            let source = ext.files.get(path)?;
                            Some(serde_json::json!({ "url": format!("{}{}", ext.base_url, path.trim_start_matches('/')), "source": String::from_utf8_lossy(source) }))
                        })
                        .collect();
                    let css: Vec<serde_json::Value> = script.css.iter().filter_map(|path| Some(serde_json::Value::String(String::from_utf8_lossy(ext.files.get(path)?).into_owned()))).collect();
                    bundles.push(serde_json::json!({
                        "extensionId": ext.id,
                        "runAt": match script.run_at { RunAt::DocumentStart => "document_start", RunAt::DocumentEnd => "document_end", RunAt::DocumentIdle => "document_idle" },
                        "world": match script.world { ScriptWorld::Main => "MAIN", ScriptWorld::Isolated => "ISOLATED" },
                        "js": js,
                        "css": css,
                    }));
                }
            });
        }
        serde_json::Value::Array(bundles)
    }

    /// The programs that run an extension's content scripts in a document, one per extension with matching scripts:
    /// `[{ "extensionId", "program" }]`.
    ///
    /// Each `program` is the source of one JavaScript *function expression*. The browser evaluates it in the page, calls the
    /// resulting function with the extension bridge of that document (an `ExtensionBridge` bound to the extension), and from
    /// then on delivers `{"kind":"run","stage":"document_end"}` and `{"kind":"run","stage":"document_idle"}` through the
    /// bridge at those points of loading. `document_start` scripts run during the call itself. Nothing is put on the page's
    /// global object: the shim, the API objects and the bridge are only reachable from the content scripts' closures.
    ///
    /// LIMITATION: the scripts share the page's JavaScript world (built-ins, prototypes, globals they declare with
    /// `window.x =`). A hostile page can tamper with them. Isolated worlds are an engine task; see docs/INTEGRATION.md.
    ///
    /// A store page (see [`crate::store`]) also gets the store program first, under the bridge key
    /// [`crate::store::STORE_BRIDGE_KEY`]. It is called the same way, and the browser must only honour that bridge in documents
    /// it knows to be on the store's origin.
    pub fn content_script_programs(&self, url: &str, is_top_frame: bool, fallback_url: Option<&str>) -> serde_json::Value {
        let bundles = self.content_script_bundles(url, is_top_frame, fallback_url);
        let mut programs = vec![];
        if let Some(store) = crate::store::store_for_url(url) {
            programs
                .push(serde_json::json!({ "extensionId": crate::store::STORE_BRIDGE_KEY, "program": crate::store::store_program(store) }));
        }
        let mut per_extension: Vec<(String, Vec<&serde_json::Value>)> = vec![];
        for bundle in bundles.as_array().into_iter().flatten() {
            let id = bundle["extensionId"].as_str().unwrap_or_default().to_string();
            match per_extension.iter_mut().find(|(e, _)| *e == id) {
                Some((_, list)) => list.push(bundle),
                None => per_extension.push((id, vec![bundle])),
            }
        }
        programs.extend(
            per_extension
                .into_iter()
                .map(|(id, bundles)| serde_json::json!({ "extensionId": id, "program": build_content_script_program(&bundles) })),
        );
        serde_json::Value::Array(programs)
    }

    /// The extension whose base URL is the origin of `url`, if any.
    pub fn extension_for_url(&self, url: &str) -> Option<String> {
        let (base, _) = split_extension_url(url)?;
        self.find_extension_by_base_url(&base)
    }

    /// Answers a request for an extension URL. `initiator` is the serialized origin of the requester, or `None` for a
    /// navigation the browser started itself. Returns `None` if the URL belongs to no installed extension.
    pub fn serve(&self, url: &str, initiator: Option<&str>) -> Option<Served> {
        let (base, path) = split_extension_url(url)?;
        let id = self.find_extension_by_base_url(&base)?;
        Some(
            self.with_extension(&id, |ext| match &path {
                Some(path) => serve_from(ext, &base, path, initiator),
                None => Served::error(404, &ext.id, "not found"),
            })
            .unwrap_or_else(|| Served::error(404, &id, "extension was removed")),
        )
    }
}

fn serve_from(ext: &Extension, base: &str, path: &str, initiator: Option<&str>) -> Served {
    let Some(who) = initiator_of(base, initiator) else { return Served::error(403, &ext.id, "forbidden") };
    let same = who == Initiator::SameExtension;

    // The shim and bootstrap are for the extension's own pages only.
    if let Some(virtual_name) = path.strip_prefix(VIRTUAL_PREFIX) {
        if !same {
            return Served::error(403, &ext.id, "forbidden");
        }
        let js = |body: &str| Served {
            status: 200,
            mime: mime_for_path("x.js").into(),
            csp: None,
            extension_id: ext.id.clone(),
            body: body.as_bytes().to_vec(),
        };
        return match virtual_name {
            "shim.js" => js(SHIM_JS),
            "boot.js" => js(BOOT_JS),
            "background.html" => Served {
                status: 200,
                mime: mime_for_path("x.html").into(),
                csp: Some(ext.manifest.csp_extension_pages.clone()),
                extension_id: ext.id.clone(),
                body: inject_boot_scripts(&generate_background_page(&ext.manifest)).into_bytes(),
            },
            _ => Served::error(404, &ext.id, "not found"),
        };
    }

    let path = if path.is_empty() { "index.html" } else { path };
    if !is_web_accessible(&ext.manifest, path, &who) {
        return Served::error(403, &ext.id, "This resource is not web accessible");
    }
    let Some(data) = ext.files.get(path) else { return Served::error(404, &ext.id, "not found") };
    let mime = mime_for_path(path);
    let is_html = mime.starts_with("text/html");
    let body = if is_html && same { inject_boot_scripts(&String::from_utf8_lossy(data)).into_bytes() } else { data.to_vec() };
    Served {
        status: 200,
        mime: mime.into(),
        csp: is_html.then(|| ext.manifest.csp_extension_pages.clone()),
        extension_id: ext.id.clone(),
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::InstallOptions;
    use crate::host::*;
    use crate::loader::Files;
    use crate::manifest::parse_manifest;
    use crate::permissions::PermissionRequest;
    use crate::registry::Registry;
    use serde_json::Value;
    use std::rc::Rc;

    struct NullHost;
    impl ExtensionHost for NullHost {
        fn read_storage(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn write_storage(&self, _: &str, _: &str, _: &str) {}
        fn now_ms(&self) -> i64 {
            0
        }
        fn ui_locale(&self) -> String {
            "en".into()
        }
        fn dispatch_event(&self, _: &str, _: &str, _: Vec<Value>) {}
        fn deliver_to_context(&self, _: u64, _: Value) {}
        fn call_browser(&self, _: &ApiCall, reply: Reply) {
            reply(Ok(Value::Null))
        }
        fn request_permissions(&self, _: &str, _: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
            reply(false)
        }
    }

    const BASE: &str = "chrome-extension://abcdefghijklmnopabcdefghijklmnop/";

    fn dispatcher() -> Dispatcher {
        let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
        let manifest = parse_manifest(
            r#"{"manifest_version":3,"name":"x","version":"1","content_security_policy":{"extension_pages":"script-src 'self'; object-src 'none'"},
                "web_accessible_resources":[{"resources":["public/*"],"matches":["https://*.example.com/*"]}]}"#,
        )
        .manifest
        .unwrap();
        let mut files = Files::new();
        files.insert(
            "popup.html",
            b"<!doctype html><html><head><title>t</title></head><body>hi<script src=\"popup.js\"></script></body></html>".to_vec(),
        );
        files.insert("popup.js", b"console.log(1)".to_vec());
        files.insert("public/logo.png", vec![0x89, b'P', b'N', b'G']);
        files.insert("dir/a b.txt", b"spaced".to_vec());
        d.install("abcdefghijklmnopabcdefghijklmnop", manifest, InstallOptions { base_url: BASE.into(), locales: vec![], files });
        d
    }

    fn get(d: &Dispatcher, path: &str, initiator: Option<&str>) -> Served {
        d.serve(&format!("{BASE}{path}"), initiator).expect("belongs to the extension")
    }

    #[test]
    fn serves_files_with_mime_and_boot_scripts_in_html() {
        let d = dispatcher();
        let page = get(&d, "popup.html", None);
        assert_eq!((page.status, page.mime.as_str()), (200, "text/html; charset=utf-8"));
        let html = String::from_utf8(page.body).unwrap();
        assert!(
            html.starts_with(
                "<!doctype html><html><head><script src=\"/__cgx__/shim.js\"></script><script src=\"/__cgx__/boot.js\"></script><title>"
            ),
            "{html}"
        );
        assert_eq!(page.csp.as_deref(), Some("script-src 'self'; object-src 'none'"));
        let js = get(&d, "popup.js", None);
        assert_eq!(
            (js.status, js.mime.as_str(), js.body.as_slice(), js.csp),
            (200, "text/javascript; charset=utf-8", b"console.log(1)".as_slice(), None)
        );
    }

    #[test]
    fn urls_are_decoded_and_normalised() {
        let d = dispatcher();
        assert_eq!(get(&d, "dir/a%20b.txt?x=1#frag", None).body, b"spaced");
        assert_eq!(get(&d, "popup.js?v=3", None).status, 200);
        let upper = d.serve("CHROME-EXTENSION://ABCDEFGHIJKLMNOPABCDEFGHIJKLMNOP/popup.js", None).unwrap();
        assert_eq!(upper.status, 200);
        assert_eq!(get(&d, "missing.js", None).status, 404);
        assert_eq!(get(&d, "bad%zz.js", None).status, 404, "an undecodable path matches no file");
        assert!(d.serve("https://example.com/popup.js", None).is_none());
        assert!(d.serve("chrome-extension://ponmlkjihgfedcbaponmlkjihgfedcba/popup.js", None).is_none(), "another, unknown extension");
        assert!(d.serve("not a url", None).is_none());
    }

    #[test]
    fn web_pages_can_only_load_web_accessible_resources() {
        let d = dispatcher();
        let page = Some("https://www.example.com");
        assert_eq!(get(&d, "public/logo.png", page).status, 200);
        assert_eq!(get(&d, "popup.js", page).status, 403);
        assert_eq!(get(&d, "popup.html", page).status, 403);
        assert_eq!(get(&d, "public/logo.png", Some("https://evil.test")).status, 403);
        assert_eq!(get(&d, "public/logo.png", Some("null")).status, 403, "opaque origins match nothing");
        // Another extension is refused unless the manifest lists it.
        assert_eq!(get(&d, "popup.js", Some("chrome-extension://ponmlkjihgfedcbaponmlkjihgfedcba")).status, 403);
        // The extension's own pages may load anything.
        assert_eq!(get(&d, "popup.js", Some(&BASE[..BASE.len() - 1])).status, 200);
        assert_eq!(get(&d, "popup.js", Some(BASE)).status, 200);
    }

    #[test]
    fn web_pages_cannot_get_the_shim_or_bootstrap() {
        let d = dispatcher();
        assert_eq!(get(&d, "__cgx__/shim.js", None).status, 200);
        assert_eq!(get(&d, "__cgx__/boot.js", Some(BASE)).status, 200);
        assert!(get(&d, "__cgx__/boot.js", None).body.starts_with(b"/*"));
        assert_eq!(get(&d, "__cgx__/shim.js", Some("https://www.example.com")).status, 403);
        assert_eq!(get(&d, "__cgx__/other.js", None).status, 404);
    }

    #[test]
    fn html_injection_edge_cases() {
        let inj = |h: &str| inject_boot_scripts(h);
        assert!(inj("<head><meta></head>").starts_with("<head><script src=\"/__cgx__/shim.js\">"));
        assert!(inj("<HEAD lang=x>body").starts_with("<HEAD lang=x><script"));
        assert!(inj("<html lang=en><body>b</body></html>").starts_with("<html lang=en><script"));
        assert!(inj("<header>not a head</header>").starts_with("<script"), "<header> is not <head>");
        assert!(inj("plain text").starts_with("<script"));
        assert_eq!(inject_boot_scripts(&inj("<head></head>")), inj("<head></head>"), "idempotent");
        assert!(inj("").contains("boot.js"));
    }

    #[test]
    fn generated_background_pages() {
        let page = |manifest: &str| generate_background_page(&parse_manifest(manifest).manifest.unwrap());
        assert_eq!(
            page(r#"{"manifest_version":2,"name":"x","version":"1","background":{"scripts":["a.js","lib/b.js","/c.js"]}}"#),
            "<!doctype html><html><head><meta charset=\"utf-8\"><title>Background</title></head><body><script src=\"/a.js\"></script><script src=\"/lib/b.js\"></script><script src=\"/c.js\"></script></body></html>"
        );
        let sw = page(r#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"sw.js","type":"module"}}"#);
        assert!(sw.contains("<script type=\"module\" src=\"/sw.js\"></script>"), "{sw}");
        // Attribute values are escaped.
        assert!(
            page(r#"{"manifest_version":2,"name":"x","version":"1","background":{"scripts":["a\"><b.js"]}}"#)
                .contains("src=\"/a&quot;&gt;&lt;b.js\"")
        );
        assert!(!page(r#"{"manifest_version":3,"name":"x","version":"1"}"#).contains("<script"));
    }

    #[test]
    fn background_page_is_served_and_identified() {
        let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
        let id = "abcdefghijklmnopabcdefghijklmnop";
        let manifest =
            parse_manifest(r#"{"manifest_version":2,"name":"x","version":"1","background":{"scripts":["bg.js"]}}"#).manifest.unwrap();
        let mut files = Files::new();
        files.insert("bg.js", b"x".to_vec());
        d.install(id, manifest, InstallOptions { base_url: BASE.into(), locales: vec![], files });

        let url = d.background_url(id).unwrap();
        assert_eq!(url, format!("{BASE}__cgx__/background.html"));
        assert!(d.is_background_url(id, &url) && d.is_background_url(id, &format!("{url}?x=1#y")));
        assert!(!d.is_background_url(id, &format!("{BASE}popup.html")));
        let served = d.serve(&url, None).unwrap();
        let html = String::from_utf8(served.body).unwrap();
        assert_eq!(served.status, 200);
        assert!(html.contains("/__cgx__/boot.js") && html.contains("<script src=\"/bg.js\">"), "{html}");
        // Boot scripts come before the extension's own scripts.
        assert!(html.find("boot.js").unwrap() < html.find("/bg.js").unwrap());
        assert!(served.csp.is_some());
        // Web pages cannot load the generated page.
        assert_eq!(d.serve(&url, Some("https://evil.test")).unwrap().status, 403);

        // A background *page* is loaded from its own file; no background means no url.
        let page_manifest =
            parse_manifest(r#"{"manifest_version":2,"name":"x","version":"1","background":{"page":"/bg/page.html"}}"#).manifest.unwrap();
        d.install(
            "ponmlkjihgfedcbaponmlkjihgfedcba",
            page_manifest,
            InstallOptions {
                base_url: "chrome-extension://ponmlkjihgfedcbaponmlkjihgfedcba/".into(),
                locales: vec![],
                files: Files::new(),
            },
        );
        assert_eq!(
            d.background_url("ponmlkjihgfedcbaponmlkjihgfedcba").unwrap(),
            "chrome-extension://ponmlkjihgfedcbaponmlkjihgfedcba/bg/page.html"
        );
        d.install(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1"}"#).manifest.unwrap(),
            InstallOptions {
                base_url: "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".into(),
                locales: vec![],
                files: Files::new(),
            },
        );
        assert_eq!(d.background_url("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"), None);
        assert!(!d.is_background_url("nobody", &url));
    }

    #[test]
    fn content_script_bundles_inline_sources_for_matching_documents() {
        let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
        let manifest = parse_manifest(
            r#"{"manifest_version":3,"name":"x","version":"1","content_scripts":[
                {"matches":["https://*.example.com/*"],"js":["a.js","missing.js"],"css":["a.css"],"run_at":"document_start"},
                {"matches":["https://example.com/only/*"],"js":["b.js"],"world":"MAIN","all_frames":true}]}"#,
        )
        .manifest
        .unwrap();
        let mut files = Files::new();
        files.insert("a.js", b"console.log('a')".to_vec());
        files.insert("b.js", b"console.log('b')".to_vec());
        files.insert("a.css", b"body{color:red}".to_vec());
        d.install("abcdefghijklmnopabcdefghijklmnop", manifest, InstallOptions { base_url: BASE.into(), locales: vec![], files });

        let all = d.content_script_bundles("https://example.com/only/page", true, None);
        let all = all.as_array().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0]["runAt"], "document_start");
        assert_eq!(
            all[0]["js"],
            serde_json::json!([{"url": format!("{BASE}a.js"), "source": "console.log('a')"}]),
            "missing files are skipped"
        );
        assert_eq!(all[0]["css"], serde_json::json!(["body{color:red}"]));
        assert_eq!((all[1]["runAt"].as_str(), all[1]["world"].as_str()), (Some("document_idle"), Some("MAIN")));
        // Frames only get all_frames scripts; unrelated pages get nothing.
        let frame = d.content_script_bundles("https://example.com/only/page", false, None);
        assert_eq!(frame.as_array().unwrap().len(), 1);
        assert_eq!(d.content_script_bundles("https://other.test/", true, None), serde_json::json!([]));
        assert_eq!(
            d.content_script_bundles("chrome-extension://abcdefghijklmnopabcdefghijklmnop/x.html", true, None),
            serde_json::json!([])
        );
    }

    #[test]
    fn content_script_programs_group_by_extension_and_escape_sources() {
        let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
        let manifest = parse_manifest(
            r#"{"manifest_version":3,"name":"x","version":"1","content_scripts":[
                {"matches":["https://a.test/*"],"js":["start.js"],"css":["s.css"],"run_at":"document_start"},
                {"matches":["https://a.test/*"],"js":["idle.js"]}]}"#,
        )
        .manifest
        .unwrap();
        let mut files = Files::new();
        files.insert("start.js", "var hit = '</script>\u{2028}';".as_bytes().to_vec());
        files.insert("idle.js", b"console.log(1)".to_vec());
        files.insert("s.css", b"body{}".to_vec());
        d.install("abcdefghijklmnopabcdefghijklmnop", manifest, InstallOptions { base_url: BASE.into(), locales: vec![], files });

        let programs = d.content_script_programs("https://a.test/x", true, None);
        let programs = programs.as_array().unwrap();
        assert_eq!(programs.len(), 1, "one program per extension");
        let program = programs[0]["program"].as_str().unwrap();
        assert!(program.starts_with("(function (bridge) {") && program.ends_with("})"));
        // The program runs as a script, never inside HTML, so `</script>` in a source is harmless; the program must
        // still be valid JavaScript with it (tests/content_script_program.rs runs programs).
        assert!(program.contains("var stageStyles = [[\"body{}\"], [], []];"), "{program}");
        assert!(program.contains("console.log(1)"));
        // Behaviour (nothing leaks onto the page's global, stages run in order) is tested by running the program in
        // tests/content_script_program.rs.
        assert_eq!(d.content_script_programs("https://b.test/", true, None), serde_json::json!([]));
    }

    #[test]
    fn mime_types() {
        for (path, mime) in [
            ("a.html", "text/html; charset=utf-8"),
            ("a.MJS", "text/javascript; charset=utf-8"),
            ("a.svg", "image/svg+xml"),
            ("a.woff2", "font/woff2"),
            ("a.wasm", "application/wasm"),
            ("noext", "application/octet-stream"),
            ("a.unknown", "application/octet-stream"),
        ] {
            assert_eq!(mime_for_path(path), mime, "{path}");
        }
    }

    #[test]
    fn default_base_urls_follow_the_dialect() {
        let m = |json: &str| parse_manifest(json).manifest.unwrap();
        let chrome = m(r#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"s.js"}}"#);
        assert_eq!(default_base_url("abcdefghijklmnopabcdefghijklmnop", &chrome), "chrome-extension://abcdefghijklmnopabcdefghijklmnop/");
        // A Chrome extension whose id is not a valid Chrome id still gets a well-formed host.
        let weird = default_base_url("not a chrome id", &chrome);
        assert!(
            weird.starts_with("chrome-extension://")
                && crate::encoding::is_valid_chrome_extension_id(&weird["chrome-extension://".len()..weird.len() - 1])
        );

        let firefox =
            m(r#"{"manifest_version":2,"name":"x","version":"1","browser_specific_settings":{"gecko":{"id":"ublock@raymondhill.net"}}}"#);
        let url = default_base_url("ublock@raymondhill.net", &firefox);
        assert!(url.starts_with("moz-extension://") && url.ends_with('/'));
        let host = &url["moz-extension://".len()..url.len() - 1];
        assert_eq!(host.len(), 36);
        assert_eq!(host.matches('-').count(), 4);
        assert!(host.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') && host.as_bytes()[14] == b'4');
        assert_eq!(url, default_base_url("ublock@raymondhill.net", &firefox), "stable across runs");
        assert_ne!(url, default_base_url("other@example.org", &firefox));
        // Portable manifests with a gecko id are served the Firefox way, without one the Chrome way.
        let both = m(
            r#"{"manifest_version":3,"name":"x","version":"1","browser_specific_settings":{"gecko":{"id":"a@b.c"}},"background":{"service_worker":"s.js","scripts":["b.js"]}}"#,
        );
        assert!(default_base_url("a@b.c", &both).starts_with("moz-extension://"));
    }

    #[test]
    fn extension_lookup_by_url() {
        let d = dispatcher();
        assert_eq!(d.extension_for_url(&format!("{BASE}x.js")).as_deref(), Some("abcdefghijklmnopabcdefghijklmnop"));
        assert_eq!(d.extension_for_url("chrome-extension://other/x.js"), None);
    }
}
