//! End to end: the real JavaScript shim (Shim/webext-shim.js, run by Node) talking to the real Rust core.
//!
//! The test plays the browser. A Node driver hosts one shim instance per extension context (a background page, a content
//! script in a tab, ...). Whatever a shim sends over its bridge is run through `Dispatcher::dispatch`; whatever the core
//! hands to `ExtensionHost::deliver_to_context` is forwarded to the right shim. This exercises the actual protocol between
//! the two halves: ports, one-shot messages, listeners, `lastError`, and context teardown.

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;
use webext::bus::{ContextInfo, NO_RECEIVER};
use webext::dispatch::{Dispatcher, InstallOptions};
use webext::host::*;
use webext::manifest::parse_manifest;
use webext::permissions::PermissionRequest;
use webext::registry::Registry;

const EXT: &str = "abcdefghijklmnopabcdefghijklmnop";

type SharedStdin = Rc<RefCell<ChildStdin>>;

fn send(stdin: &SharedStdin, v: Value) {
    let mut w = stdin.borrow_mut();
    writeln!(w, "{v}").unwrap();
    w.flush().unwrap();
}

/// The part of the browser the core calls into: it forwards bus envelopes to the shim of the right context.
struct Host {
    stdin: SharedStdin,
}

impl ExtensionHost for Host {
    fn read_storage(&self, _: &str, _: &str) -> Option<String> {
        None
    }
    fn write_storage(&self, _: &str, _: &str, _: &str) {}
    fn now_ms(&self) -> i64 {
        0
    }
    fn ui_locale(&self) -> String {
        "en-US".into()
    }
    fn dispatch_event(&self, _: &str, _: &str, _: Vec<Value>) {}
    fn deliver_to_context(&self, context_id: u64, message: Value) {
        send(&self.stdin, json!({"op": "deliver", "ctx": context_id, "msg": message}));
    }
    fn call_browser(&self, _: &ApiCall, reply: Reply) {
        reply(Err(ApiError::runtime("not available in this test")));
    }
    fn request_permissions(&self, _: &str, _: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
        reply(false);
    }
}

struct World {
    child: Child,
    stdin: SharedStdin,
    lines: Receiver<String>,
    dispatcher: Dispatcher,
    kinds: HashMap<u64, ContextKind>,
    results: HashMap<String, Result<Value, String>>,
    flushed: Vec<String>,
    /// Errors the shim reported through its console (for example a listener that threw).
    logs: Vec<String>,
    next_id: u64,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn node_available() -> bool {
    Command::new("node").arg("--version").output().is_ok()
}

impl World {
    fn new(manifest: &str) -> World {
        let driver: PathBuf = [env!("CARGO_MANIFEST_DIR"), "..", "..", "Shim", "test", "e2e-driver.js"].iter().collect();
        let mut child = Command::new("node")
            .arg(driver)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("start node");
        let stdin: SharedStdin = Rc::new(RefCell::new(child.stdin.take().unwrap()));
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let dispatcher = Dispatcher::new(Rc::new(Host { stdin: stdin.clone() }), Registry::builtin());
        dispatcher.install(
            EXT,
            parse_manifest(manifest).manifest.expect("valid manifest"),
            InstallOptions { base_url: format!("chrome-extension://{EXT}/"), locales: vec![], files: webext::loader::Files::new() },
        );
        World { child, stdin, lines, dispatcher, kinds: HashMap::new(), results: HashMap::new(), flushed: vec![], logs: vec![], next_id: 0 }
    }

    /// Registers a context with the core and starts a shim for it.
    fn context(&mut self, id: u64, kind: ContextKind, tab: Option<i64>, frame: Option<i64>) {
        self.dispatcher
            .context_created(ContextInfo {
                id,
                extension_id: EXT.into(),
                kind,
                tab_id: tab,
                frame_id: frame,
                url: Some(format!("https://page.test/{id}")),
            })
            .unwrap();
        self.kinds.insert(id, kind);
        let init = self.dispatcher.init_data_for_context(id).unwrap();
        send(&self.stdin, json!({"op": "create", "ctx": id, "init": init}));
    }

    fn destroy(&mut self, id: u64) {
        self.dispatcher.context_destroyed(id);
        send(&self.stdin, json!({"op": "destroy", "ctx": id}));
        self.kinds.remove(&id);
    }

    /// Reads one line from the driver and acts on it.
    fn pump_one(&mut self) {
        let line = match self.lines.recv_timeout(Duration::from_secs(15)) {
            Ok(l) => l,
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for the Node driver (a message was probably never answered)"),
            Err(RecvTimeoutError::Disconnected) => panic!("the Node driver exited"),
        };
        let m: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad line from driver: {line}: {e}"));
        match m["op"].as_str().unwrap() {
            "call" => {
                let ctx = m["ctx"].as_u64().unwrap();
                let seq = m["seq"].as_u64().unwrap();
                let caller = Caller { context: self.kinds[&ctx], context_id: Some(ctx), tab_id: None, frame_id: None, url: None };
                let call = ApiCall {
                    extension_id: EXT.into(),
                    function: m["function"].as_str().unwrap().into(),
                    args: m["args"].as_array().cloned().unwrap_or_default(),
                    caller,
                };
                let stdin = self.stdin.clone();
                // The reply may come later (a one-shot message waits for its recipients), so it writes itself.
                self.dispatcher.dispatch(
                    call,
                    Box::new(move |r| {
                        send(
                            &stdin,
                            match r {
                                Ok(v) => json!({"op": "reply", "seq": seq, "ok": true, "value": v}),
                                Err(e) => json!({"op": "reply", "seq": seq, "ok": false, "error": e.message}),
                            },
                        );
                    }),
                );
            }
            "result" => {
                let id = m["id"].as_str().unwrap().to_string();
                let r = if m["ok"].as_bool().unwrap() { Ok(m["value"].clone()) } else { Err(m["error"].as_str().unwrap().to_string()) };
                self.results.insert(id, r);
            }
            "flushed" => self.flushed.push(m["id"].as_str().unwrap().to_string()),
            "log" => self.logs.push(m["text"].as_str().unwrap_or_default().to_string()),
            other => panic!("unexpected op {other}"),
        }
    }

    /// Fails the test if the shim reported an error nobody expected.
    fn assert_no_logs(&self) {
        assert!(self.logs.is_empty(), "the shim reported errors: {:#?}", self.logs);
    }

    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    /// Runs `code` in the context (an async function body with `chrome`, `browser` and a per-context scratch object `g`).
    fn eval(&mut self, ctx: u64, code: &str) -> Result<Value, String> {
        let id = self.fresh_id("e");
        send(&self.stdin, json!({"op": "eval", "ctx": ctx, "id": id, "code": code}));
        loop {
            if let Some(r) = self.results.remove(&id) {
                return r;
            }
            self.pump_one();
        }
    }

    fn ok(&mut self, ctx: u64, code: &str) -> Value {
        self.eval(ctx, code).unwrap_or_else(|e| panic!("eval failed in context {ctx}: {e}\n{code}"))
    }

    /// Lets everything in flight (calls, envelopes, promise chains) finish.
    fn settle(&mut self) {
        for _ in 0..3 {
            let id = self.fresh_id("f");
            send(&self.stdin, json!({"op": "flush", "id": id}));
            while !self.flushed.contains(&id) {
                self.pump_one();
            }
        }
    }
}

macro_rules! world {
    ($manifest:expr) => {{
        if !node_available() {
            eprintln!("SKIPPED: needs `node` on PATH");
            return;
        }
        World::new($manifest)
    }};
}

const MV3: &str = r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage","tabs"]}"#;
const BG: u64 = 1;
const POPUP: u64 = 2;
const CS: u64 = 10;
const CS_SUB: u64 = 11;

fn standard(w: &mut World) {
    w.context(BG, ContextKind::Background, None, None);
    w.context(CS, ContextKind::ContentScript, Some(5), Some(0));
}

#[test]
fn port_between_content_script_and_background() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.ok(
        BG,
        "g.got = []; chrome.runtime.onConnect.addListener(port => { g.port = port; port.onMessage.addListener((m, p) => { g.got.push(m); p.postMessage({ echo: m }); }); port.onDisconnect.addListener(() => { g.disconnected = true; }); });",
    );
    assert_eq!(w.ok(CS, "g.reply = []; g.p = chrome.runtime.connect({ name: 'chat' }); g.p.onMessage.addListener(m => g.reply.push(m)); g.p.postMessage('hi'); g.p.postMessage({ n: 2 }); return g.p.name;"), "chat");
    w.settle();

    assert_eq!(
        w.ok(BG, "return [g.got, g.port.name, g.port.sender.id, g.port.sender.tab.id, g.port.sender.frameId];"),
        json!([["hi", {"n": 2}], "chat", EXT, 5, 0])
    );
    assert_eq!(w.ok(CS, "return g.reply;"), json!([{"echo": "hi"}, {"echo": {"n": 2}}]), "replies come back in order");

    // The initiating end has no `sender`; hanging up from either side reaches the other.
    assert_eq!(w.ok(CS, "return 'sender' in g.p;"), false);
    w.ok(CS, "g.p.disconnect();");
    w.settle();
    assert_eq!(w.ok(BG, "return g.disconnected === true;"), true);
    assert!(w.eval(CS, "g.p.postMessage('after');").unwrap_err().contains("disconnected port"));
    assert!(w.eval(BG, "g.port.postMessage('after');").unwrap_err().contains("disconnected port"));
    w.assert_no_logs();
}

#[test]
fn background_hanging_up_notifies_the_content_script() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.ok(BG, "chrome.runtime.onConnect.addListener(port => { g.port = port; });");
    w.ok(CS, "g.p = chrome.runtime.connect(); g.events = []; g.p.onDisconnect.addListener(() => g.events.push(['disconnect', chrome.runtime.lastError === undefined]));");
    w.settle();
    w.ok(BG, "g.port.disconnect();");
    w.settle();
    assert_eq!(w.ok(CS, "return g.events;"), json!([["disconnect", true]]), "a normal hang-up carries no lastError");
    w.assert_no_logs();
}

#[test]
fn connecting_with_no_listener_sets_last_error() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.ok(CS, "g.seen = []; g.p = chrome.runtime.connect({ name: 'x' }); g.p.onDisconnect.addListener(() => g.seen.push(chrome.runtime.lastError && chrome.runtime.lastError.message));");
    w.settle();
    assert_eq!(w.ok(CS, "return g.seen;"), json!([NO_RECEIVER]));
    assert_eq!(w.ok(CS, "return chrome.runtime.lastError === undefined;"), true, "lastError is only set during the callback");
    w.assert_no_logs();
}

#[test]
fn closing_the_tab_disconnects_the_background_port() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.ok(BG, "g.log = []; chrome.runtime.onConnect.addListener(port => { port.onDisconnect.addListener(() => g.log.push('gone')); });");
    w.ok(CS, "g.p = chrome.runtime.connect();");
    w.settle();
    w.destroy(CS);
    w.settle();
    assert_eq!(w.ok(BG, "return g.log;"), json!(["gone"]));
    w.assert_no_logs();
}

#[test]
fn send_message_with_async_response_in_both_dialects() {
    let mut w = world!(MV3);
    standard(&mut w);
    // Chrome style: return true and answer later.
    w.ok(BG, "chrome.runtime.onMessage.addListener((msg, sender, sendResponse) => { if (msg.kind !== 'chrome') return; setTimeout(() => sendResponse({ got: msg, from: sender.tab.id }), 5); return true; });");
    assert_eq!(
        w.ok(CS, "return await chrome.runtime.sendMessage({ kind: 'chrome', n: 1 });"),
        json!({"got": {"kind": "chrome", "n": 1}, "from": 5})
    );

    // Firefox style: return a promise.
    w.ok(BG, "browser.runtime.onMessage.addListener(async (msg) => msg.kind === 'firefox' ? 'promised ' + msg.n : undefined);");
    assert_eq!(w.ok(CS, "return await browser.runtime.sendMessage({ kind: 'firefox', n: 2 });"), "promised 2");

    // Callback style, and the explicit-extension-id form.
    assert_eq!(
        w.ok(
            CS,
            &format!("return await new Promise(r => chrome.runtime.sendMessage('{EXT}', {{ kind: 'chrome', n: 3 }}, resp => r(resp)));")
        ),
        json!({"got": {"kind": "chrome", "n": 3}, "from": 5})
    );
    w.assert_no_logs();
}

#[test]
fn send_message_failure_modes() {
    let mut w = world!(MV3);
    standard(&mut w);
    // Nobody is listening: the standard error, as a rejection and as lastError.
    assert_eq!(w.eval(CS, "return await chrome.runtime.sendMessage('x');").unwrap_err(), NO_RECEIVER);
    assert_eq!(w.ok(CS, "return await new Promise(r => chrome.runtime.sendMessage('x', () => r(chrome.runtime.lastError && chrome.runtime.lastError.message)));"), NO_RECEIVER);

    // A listener exists but does not answer: resolves with undefined.
    w.ok(BG, "chrome.runtime.onMessage.addListener(() => { /* not for us */ });");
    assert_eq!(w.ok(CS, "return (await chrome.runtime.sendMessage('x')) === undefined;"), true);
    // A throwing listener neither answers nor breaks the sender.
    w.ok(BG, "chrome.runtime.onMessage.addListener(() => { throw new Error('boom'); });");
    assert_eq!(w.ok(CS, "return (await chrome.runtime.sendMessage('x')) === undefined;"), true);
    assert_eq!(w.logs.len(), 1, "the throwing listener is reported once, for the one message that reached it: {:?}", w.logs);
    assert!(w.logs.iter().all(|l| l.contains("boom")));
}

#[test]
fn tabs_messaging_from_the_background_to_content_scripts() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.context(CS_SUB, ContextKind::ContentScript, Some(5), Some(3));
    w.ok(CS, "chrome.runtime.onMessage.addListener((m, s, send) => { send('top:' + m); });");
    w.ok(CS_SUB, "chrome.runtime.onMessage.addListener((m, s, send) => { send('sub:' + m); });");
    w.ok(CS_SUB, "chrome.runtime.onConnect.addListener(port => { port.onMessage.addListener(m => port.postMessage('sub-port:' + m)); });");

    assert_eq!(w.ok(BG, "return await browser.tabs.sendMessage(5, 'hello', { frameId: 3 });"), "sub:hello");
    assert_eq!(w.ok(BG, "return await browser.tabs.sendMessage(5, 'hello', { frameId: 0 });"), "top:hello");
    assert_eq!(w.eval(BG, "return await browser.tabs.sendMessage(99, 'hello');").unwrap_err(), NO_RECEIVER);

    w.ok(BG, "g.seen = []; g.port = chrome.tabs.connect(5, { frameId: 3, name: 'tabport' }); g.port.onMessage.addListener(m => g.seen.push(m)); g.port.postMessage('ping');");
    w.settle();
    assert_eq!(w.ok(BG, "return g.seen;"), json!(["sub-port:ping"]));
    w.assert_no_logs();
}

#[test]
fn content_scripts_see_a_trimmed_api_and_pages_do_not_leak_in() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.context(POPUP, ContextKind::ExtensionPage, None, None);
    assert_eq!(w.ok(CS, "return [typeof chrome.tabs, typeof chrome.alarms, typeof chrome.runtime.connect, typeof chrome.storage.local.get, typeof chrome.runtime.openOptionsPage];"), json!(["undefined", "undefined", "function", "function", "undefined"]));
    assert_eq!(w.ok(BG, "return [typeof chrome.tabs.query, typeof chrome.runtime.openOptionsPage];"), json!(["function", "function"]));

    // runtime.connect from a content script reaches an extension page as well as the background.
    w.ok(POPUP, "g.n = 0; chrome.runtime.onConnect.addListener(() => { g.n++; });");
    w.ok(BG, "g.n = 0; chrome.runtime.onConnect.addListener(() => { g.n++; });");
    w.ok(CS, "chrome.runtime.connect();");
    w.settle();
    assert_eq!(w.ok(POPUP, "return g.n;"), 1);
    assert_eq!(w.ok(BG, "return g.n;"), 1);
    w.assert_no_logs();
}

#[test]
fn many_concurrent_messages_are_each_answered_correctly() {
    let mut w = world!(MV3);
    standard(&mut w);
    w.ok(BG, "chrome.runtime.onMessage.addListener((m, s, send) => { setTimeout(() => send(m * 2), m % 3); return true; });");
    let answers = w.ok(CS, "return await Promise.all(Array.from({ length: 50 }, (_, i) => chrome.runtime.sendMessage(i)));");
    assert_eq!(answers, json!((0..50).map(|i| i * 2).collect::<Vec<_>>()));
    w.assert_no_logs();
}
