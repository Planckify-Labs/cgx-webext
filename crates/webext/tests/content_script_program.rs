//! Runs a generated content-script program the way the browser does (evaluate the function expression in a page-like
//! global, call it with a bridge) under Node, and checks what the page and the scripts can see.

use serde_json::{Value, json};
use std::process::Command;
use std::rc::Rc;
use webext::dispatch::{Dispatcher, InstallOptions};
use webext::host::*;
use webext::loader::Files;
use webext::manifest::parse_manifest;
use webext::permissions::PermissionRequest;
use webext::registry::Registry;

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

const HARNESS: &str = r#"
const vm = require('node:vm');
const programs = JSON.parse(process.argv[process.argv.length - 1]);
const log = [];
// A minimal page: a global with a document whose head exists.
const appended = [];
// The context brings its own built-ins; only what a page would have is added.
const page = { console: { error: (e) => log.push('error:' + (e && e.message)) }, log,
  document: { head: { appendChild: (n) => appended.push(n.textContent) }, documentElement: {}, createElement: () => ({}), addEventListener() {} } };
page.globalThis = page;
// A document element that the "parser" inserts later, when the test says so (WAIT_FOR_ROOT).
const waitForRoot = process.env.WAIT_FOR_ROOT === '1';
const observers = [];
if (waitForRoot) {
  const root = page.document.documentElement;
  page.document.documentElement = null;
  page.MutationObserver = function (cb) { this.observe = () => observers.push(cb); this.disconnect = () => {}; };
  page.insertRoot = () => { page.document.documentElement = root; observers.splice(0).forEach((cb) => cb([])); };
}
const ctx = vm.createContext(page);
const before = new Set(Object.getOwnPropertyNames(ctx));
const handlers = {};
const mainRunners = {};
for (const { extensionId, program, world } of programs) {
  const fn = vm.runInContext(program, ctx);
  if (world === 'MAIN') { mainRunners[extensionId] = fn(); continue; }
  const bridge = {
    contextInit: () => JSON.stringify({ id: extensionId, baseUrl: 'chrome-extension://' + extensionId + '/', manifest: { manifest_version: 3 },
      uiLanguage: 'en', syncFunctions: [], contextId: 7, surface: { runtime: { functions: ['getURL'], events: ['onMessage'], properties: ['id', 'lastError'] } } }),
    call: () => Promise.resolve('null'),
    callSync: () => '',
    setMessageHandler: (h) => { handlers[extensionId] = h; },
    installFirefoxHelpers: () => log.push('firefox helpers'),
  };
  fn(bridge);
}
if (waitForRoot) { log.push('before root'); page.insertRoot(); }
const send = (id, m) => handlers[id](JSON.stringify(m));
for (const run of Object.values(mainRunners)) { run('document_end'); run('document_idle'); }
for (const id of Object.keys(handlers)) send(id, { kind: 'run', stage: 'document_end' });
for (const id of Object.keys(handlers)) { send(id, { kind: 'run', stage: 'document_idle' }); send(id, { kind: 'run', stage: 'document_idle' }); }
// This context is the extension's isolated world (or, for MAIN programs, the page): `chrome` is the one global the program
// puts there on purpose.
const leaked = Object.getOwnPropertyNames(ctx).filter((n) => !before.has(n) && n !== 'chrome' && n !== 'browser');
console.log(JSON.stringify({ log, leaked, appended, pageChrome: typeof ctx.chrome, pageBrowser: typeof ctx.browser, pageChromeKeys: ctx.chrome ? Object.keys(ctx.chrome) : null }));
"#;

#[test]
fn document_start_waits_for_the_document_element() {
    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
    let mut files = Files::new();
    files.insert("s.js", b"log.push('start sees root: ' + (document.documentElement !== null));".to_vec());
    d.install(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        parse_manifest(r#"{"manifest_version":3,"name":"a","version":"1","content_scripts":[{"matches":["https://page.test/*"],"js":["s.js"],"run_at":"document_start"}]}"#).manifest.unwrap(),
        InstallOptions { base_url: "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".into(), locales: vec![], files },
    );
    let programs = d.content_script_programs("https://page.test/", true, None);
    let out = Command::new("node").env("WAIT_FOR_ROOT", "1").arg("-e").arg(HARNESS).arg(programs.to_string()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["log"], json!(["before root", "start sees root: true"]));
}

#[test]
fn programs_survive_a_page_without_eval() {
    if Command::new("node").arg("--version").output().is_err() {
        return;
    }
    let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
    let mut files = Files::new();
    files.insert("a.js", b"log.push('ran without eval');".to_vec());
    d.install(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        parse_manifest(
            r#"{"manifest_version":3,"name":"a","version":"1","content_scripts":[{"matches":["https://page.test/*"],"js":["a.js"]}]}"#,
        )
        .manifest
        .unwrap(),
        InstallOptions { base_url: "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".into(), locales: vec![], files },
    );
    let programs = d.content_script_programs("https://page.test/", true, None);
    // Like a page whose CSP forbids eval: code generation from strings throws.
    let harness = HARNESS
        .replace("const ctx = vm.createContext(page);", "const ctx = vm.createContext(page, { codeGeneration: { strings: false } });");
    let out = Command::new("node").arg("-e").arg(harness).arg(programs.to_string()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(result["log"], json!(["ran without eval"]));
}

#[test]
fn programs_run_in_order_with_their_own_api_and_leak_nothing() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("SKIPPED: needs `node`");
        return;
    }
    let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
    let install = |id: &str, scripts: &[(&str, &str)], manifest: &str| {
        let mut files = Files::new();
        for (path, source) in scripts {
            files.insert(path, source.as_bytes().to_vec());
        }
        d.install(
            id,
            parse_manifest(manifest).manifest.unwrap(),
            InstallOptions { base_url: format!("chrome-extension://{id}/"), locales: vec![], files },
        );
    };
    install(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        &[
            (
                "start.js",
                "log.push('A start ' + chrome.runtime.id); function shared() { return 'shared'; } const fromStart = 'const-from-start';",
            ),
            ("start2.js", "log.push('A start2 ' + shared());"),
            ("end.js", "var tricky = '</script>\u{2028}'; log.push('A end');"),
            // A Chrome-style extension gets `chrome` only, as in Chrome: extensions test `typeof browser`.
            (
                "idle.js",
                "log.push('A idle ' + (typeof chrome.runtime.getURL) + ' ' + (typeof browser) + ' ' + shared() + ' ' + fromStart); undeclaredGlobal = 1;",
            ),
            ("boom.js", "throw new Error('boom');"),
            ("s.css", "body { color: red }"),
        ],
        r#"{"manifest_version":3,"name":"a","version":"1","content_scripts":[
            {"matches":["https://page.test/*"],"js":["start.js","start2.js"],"css":["s.css"],"run_at":"document_start"},
            {"matches":["https://page.test/*"],"js":["end.js"],"run_at":"document_end"},
            {"matches":["https://page.test/*"],"js":["idle.js","boom.js"]}]}"#,
    );
    install(
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        &[("b.js", "log.push('B idle ' + chrome.runtime.id);")],
        r#"{"manifest_version":3,"name":"b","version":"1","content_scripts":[{"matches":["https://page.test/*"],"js":["b.js"]}]}"#,
    );

    let programs = d.content_script_programs("https://page.test/x", true, None);
    let out = Command::new("node").arg("-e").arg(HARNESS).arg(programs.to_string()).output().expect("run node");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();

    assert_eq!(
        result["log"],
        json!([
            "A start aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "A start2 shared",
            "A end",
            "A idle function undefined shared const-from-start",
            "error:boom",
            "B idle bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ]),
        "stages run in order, once; files of a stage share scope; each extension sees its own id; an error is contained"
    );
    assert_eq!(result["appended"], json!(["body { color: red }"]));
    // Sloppy-mode globals a content script creates on purpose are visible, as in a main-world script; nothing else is.
    assert_eq!(result["leaked"], json!(["undeclaredGlobal"]), "the shim, the API and the bridge must not leak onto the page");
    // In its isolated world a Chrome extension has `chrome`, as a global (`window.chrome.runtime` works), and no `browser`.
    assert_eq!((result["pageChrome"].as_str(), result["pageBrowser"].as_str()), (Some("object"), Some("undefined")));
    assert_eq!(result["pageChromeKeys"], json!(["runtime"]));
}

#[test]
fn main_world_scripts_are_a_program_of_their_own_without_the_api() {
    if Command::new("node").arg("--version").output().is_err() {
        eprintln!("SKIPPED: needs `node`");
        return;
    }
    let d = Dispatcher::new(Rc::new(NullHost), Registry::builtin());
    let mut files = Files::new();
    files.insert("iso.js", b"log.push('isolated ' + chrome.runtime.id);".to_vec());
    files.insert("main.js", b"log.push('main ' + typeof chrome.runtime + ' ' + typeof browser);".to_vec());
    d.install(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        parse_manifest(
            r#"{"manifest_version":3,"name":"a","version":"1","content_scripts":[
                {"matches":["https://page.test/*"],"js":["iso.js"]},
                {"matches":["https://page.test/*"],"js":["main.js"],"world":"MAIN"}]}"#,
        )
        .manifest
        .unwrap(),
        InstallOptions { base_url: "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/".into(), locales: vec![], files },
    );
    let programs = d.content_script_programs("https://page.test/", true, None);
    let worlds: Vec<&str> = programs.as_array().unwrap().iter().map(|p| p["world"].as_str().unwrap()).collect();
    assert_eq!(worlds, ["ISOLATED", "MAIN"], "one program per world, in the same document");
    // The MAIN program is called with no bridge and must not mention one.
    let main = programs[1]["program"].as_str().unwrap();
    assert!(!main.contains("bridge") && !main.contains("createApi"), "the MAIN program carries no API and no bridge");

    let out = Command::new("node").arg("-e").arg(HARNESS).arg(programs.to_string()).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let result: Value = serde_json::from_slice(&out.stdout).unwrap();
    // Both ran. (This harness has one global for both programs, so what `chrome` is to the MAIN script is not checked here;
    // tests/Ladybird/worlds checks it in a browser, where the two programs run in different realms.)
    let log: Vec<&str> = result["log"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(log.iter().any(|l| l.starts_with("main ")) && log.iter().any(|l| l.starts_with("isolated ")), "{result}");
}
