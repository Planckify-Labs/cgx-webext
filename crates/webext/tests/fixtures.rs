//! End-to-end tests over the fixture extensions in Tests/Fixtures: load -> analyze -> install -> drive the API.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use webext::analyze::{Support, Verdict, analyze};
use webext::dispatch::{Dispatcher, InstallOptions};
use webext::host::*;
use webext::loader::{Files, IdSource, load_extension};
use webext::manifest::Severity;
use webext::permissions::PermissionRequest;
use webext::registry::{Registry, Status};

fn fixture(name: &str) -> Files {
    let dir: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", "Tests", "Fixtures", name].iter().collect();
    Files::from_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

#[derive(Default)]
struct Host {
    storage: RefCell<std::collections::HashMap<String, String>>,
    events: RefCell<Vec<(String, Vec<Value>)>>,
    now: RefCell<i64>,
}

impl ExtensionHost for Host {
    fn read_storage(&self, id: &str, key: &str) -> Option<String> {
        self.storage.borrow().get(&format!("{id}/{key}")).cloned()
    }
    fn write_storage(&self, id: &str, key: &str, json: &str) {
        self.storage.borrow_mut().insert(format!("{id}/{key}"), json.into());
    }
    fn now_ms(&self) -> i64 {
        *self.now.borrow()
    }
    fn ui_locale(&self) -> String {
        "pt-BR".into()
    }
    fn dispatch_event(&self, _id: &str, event: &str, args: Vec<Value>) {
        self.events.borrow_mut().push((event.into(), args));
    }
    fn deliver_to_context(&self, _context_id: u64, _message: Value) {}
    fn call_browser(&self, call: &ApiCall, reply: Reply) {
        reply(Ok(json!({ "tabs": [], "via": call.function })));
    }
    fn request_permissions(&self, _id: &str, _r: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
        reply(true);
    }
}

fn call(d: &Dispatcher, id: &str, f: &str, args: Vec<Value>) -> ApiResult {
    let out = Rc::new(RefCell::new(None));
    let sink = out.clone();
    d.dispatch(
        ApiCall { extension_id: id.into(), function: f.into(), args, caller: Caller::background() },
        Box::new(move |r| *sink.borrow_mut() = Some(r)),
    );
    out.borrow_mut().take().expect("synchronous reply")
}

#[test]
fn hello_content_script_needs_only_the_host() {
    let files = fixture("hello-content-script");
    let ext = load_extension(&files, None).unwrap();
    assert!(ext.diagnostics.iter().all(|d| d.severity != Severity::Error));
    let report = analyze(&Registry::builtin(), &ext.manifest, &files);
    assert_eq!(report.verdict, Verdict::NeedsHost);
    assert!(report.apis.is_empty());
    assert!(report.features.iter().any(|f| f.name == "content scripts (isolated world)"));
}

#[test]
fn storage_popup_full_flow() {
    let files = fixture("storage-popup");
    let ext = load_extension(&files, None).unwrap();
    assert_eq!(ext.display_name, "Storage Popup");
    assert_eq!(ext.id.as_deref(), Some("storage-popup@example.org"));
    assert_eq!(ext.id_source, Some(IdSource::GeckoId));
    assert_eq!(ext.locales.len(), 2);

    let reg = Registry::builtin();
    let report = analyze(&reg, &ext.manifest, &files);
    assert_eq!(report.verdict, Verdict::NeedsHost, "blockers: {:?}", report.blockers);
    let used: Vec<&str> = report.apis.iter().map(|a| a.name.as_str()).collect();
    for expected in [
        "storage.local.get",
        "storage.local.set",
        "alarms.create",
        "alarms.onAlarm",
        "runtime.onInstalled",
        "runtime.onMessage",
        "i18n.getMessage",
        "runtime.getURL",
        "tabs.query",
        "storage.onChanged",
    ] {
        assert!(used.contains(&expected), "{expected} not detected in {used:?}");
    }
    let tabs_query = report.apis.iter().find(|a| a.name == "tabs.query").unwrap();
    assert_eq!(tabs_query.support, Support::Known(Status::Implemented));
    assert_eq!(tabs_query.files, ["popup.js"]);

    // Install it and replay what its scripts do.
    let host = Rc::new(Host::default());
    let d = Dispatcher::new(host.clone(), reg);
    let id = ext.id.clone().unwrap();
    d.install(&id, ext.manifest, InstallOptions { base_url: "moz-extension://1234/".into(), locales: ext.locales, files: files.clone() });

    d.notify_installed(&id, "install", None);
    call(&d, &id, "storage.local.set", vec![json!({"visits": 0, "installedAt": 1})]).unwrap();
    call(&d, &id, "alarms.create", vec![json!("heartbeat"), json!({"periodInMinutes": 1})]).unwrap();

    *host.now.borrow_mut() = 61_000;
    d.tick();
    let tick = host.events.borrow().iter().filter(|(e, _)| e == "alarms.onAlarm").count();
    assert_eq!(tick, 1);

    let visits = call(&d, &id, "storage.local.get", vec![json!({"visits": 0})]).unwrap()["visits"].as_i64().unwrap();
    call(&d, &id, "storage.local.set", vec![json!({"visits": visits + 1})]).unwrap();
    assert_eq!(call(&d, &id, "storage.local.get", vec![json!("visits")]).unwrap(), json!({"visits": 1}));

    // The popup's localized text honours the UI locale (pt-BR) with fallback to en.
    assert_eq!(call(&d, &id, "i18n.getMessage", vec![json!("visitCount"), json!(["7"])]).unwrap(), "Ticks até agora: 7");
    assert_eq!(call(&d, &id, "i18n.getMessage", vec![json!("extDescription")]).unwrap(), "Counts alarm ticks.");
    assert_eq!(call(&d, &id, "runtime.getURL", vec![json!("popup.html")]).unwrap(), "moz-extension://1234/popup.html");

    // tabs.query is answered from the browser's snapshot; an optional permission can be requested.
    d.host_event(
        "browserState",
        &json!({"windows": [{"id": 1, "focused": true, "tabs": [
        {"id": 5, "url": "https://a.test/", "title": "A", "active": true}, {"id": 6, "url": "https://b.test/"}]}]}),
    );
    let active = call(&d, &id, "tabs.query", vec![json!({"active": true, "currentWindow": true})]).unwrap();
    assert_eq!(active.as_array().map(|a| a.iter().map(|t| t["id"].as_i64()).collect::<Vec<_>>()), Some(vec![Some(5)]));
    assert!(host.events.borrow().iter().any(|(e, _)| e == "tabs.onCreated"), "tabs that appear fire tabs.onCreated");
    assert_eq!(call(&d, &id, "permissions.request", vec![json!({"permissions": ["history"]})]).unwrap(), json!(true));

    // history.search is implemented and routes to host.history.search once permission is granted.
    assert_eq!(call(&d, &id, "history.search", vec![json!({"text": ""})]).unwrap(), json!([]));
    // Unimplemented APIs like gcm still fail clearly.
    assert_eq!(call(&d, &id, "gcm.register", vec![]).unwrap_err().kind, ErrorKind::NotAvailable);

    // Surface handed to the shim: storage, alarms, i18n visible; cookies are not.
    let init = d.init_data_for(&id).unwrap();
    assert!(init["surface"]["storage"].is_object() && init["surface"]["cookies"].is_null());
    assert!(init["surface"]["history"].is_object(), "history was granted at runtime");
    // getUILanguage is a language tag, which Intl accepts ("en-US", never "en_US").
    assert!(!init["uiLanguage"].as_str().unwrap().contains('_'));
    let sync_functions = init["syncFunctions"].as_array().unwrap();
    assert!(sync_functions.contains(&json!("i18n.getMessage")));
    // Firefox's publicSuffix answers synchronously (uBlock Origin relies on it).
    assert!(sync_functions.contains(&json!("publicSuffix.getDomain")));
}

#[test]
fn mv2_blocking_webrequest_needs_the_host() {
    let files = fixture("mv2-webrequest-blocker");
    let ext = load_extension(&files, None).unwrap();
    assert_eq!(ext.manifest.host_permissions, ["<all_urls>"]);
    let report = analyze(&Registry::builtin(), &ext.manifest, &files);
    assert_eq!(report.verdict, Verdict::NeedsHost);
    assert!(report.blockers.is_empty());
    assert!(report.features.iter().any(|f| f.name == "blocking webRequest"));
    let listener = report.apis.iter().find(|a| a.name == "webRequest.onBeforeRequest").expect("the listener is seen");
    assert_eq!(listener.support, Support::Known(Status::Implemented));
    // browserAction resolves to action through the alias table; the core keeps the badge.
    let badge = report.apis.iter().find(|a| a.name == "action.setBadgeText").expect("alias resolved");
    assert_eq!(badge.support, Support::Known(Status::Implemented));
}

#[test]
fn mv3_dnr_blocker_needs_the_host_with_rulesets_detected() {
    let files = fixture("mv3-dnr-adblock");
    let ext = load_extension(&files, None).unwrap();
    assert_eq!(ext.manifest.declarative_net_request_rulesets.len(), 1);
    let report = analyze(&Registry::builtin(), &ext.manifest, &files);
    assert_eq!(report.verdict, Verdict::NeedsHost);
    assert!(report.blockers.is_empty());
    assert!(report.features.iter().any(|f| f.name == "declarative_net_request rulesets"));
}

#[test]
fn removing_a_referenced_file_is_detected() {
    let mut files = Files::new();
    for (path, data) in fixture("mv3-dnr-adblock").iter() {
        if path != "rules.json" {
            files.insert(path, data.to_vec());
        }
    }
    let errs = load_extension(&files, None).unwrap_err();
    assert!(errs.iter().any(|d| d.field == "declarative_net_request.rule_resources"));
}
