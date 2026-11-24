//! API dispatch: validates a call against the registry (existence, manifest version, status, permission), then either
//! handles it in the core or routes it to the host.

use crate::alarms::AlarmManager;
use crate::bus::{Bus, ContextInfo, Effect, NO_RECEIVER, Target};
use crate::host::*;
use crate::i18n::I18n;
use crate::loader::Files;
use crate::manifest::Manifest;
use crate::permissions::{PermissionRequest, PermissionState};
use crate::registry::{Kind, Member, Registry, Status};
use crate::storage::{AccessLevel, Change, Quota, StorageArea, changes_to_json};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// Events the core itself emits. A test checks this list against the registry.
pub const EMITTED_EVENTS: &[&str] = &[
    "storage.onChanged",
    "storage.local.onChanged",
    "storage.sync.onChanged",
    "storage.session.onChanged",
    "storage.managed.onChanged",
    "alarms.onAlarm",
    "permissions.onAdded",
    "permissions.onRemoved",
    "runtime.onInstalled",
    "runtime.onStartup",
    "runtime.onSuspend",
    "runtime.onSuspendCanceled",
    "runtime.onUpdateAvailable",
    "runtime.onBrowserUpdateAvailable",
    "runtime.onRestartRequired",
    "runtime.onPerformanceWarning",
    "runtime.onConnectExternal",
    "runtime.onUserScriptMessage",
    "runtime.onUserScriptConnect",
    // Delivered through the message bus as envelopes (see `bus`) rather than through `ExtensionHost::dispatch_event`.
    "runtime.onMessage",
    "runtime.onConnect",
    // From browser snapshots (see `browser::diff`).
    "tabs.onCreated",
    "tabs.onUpdated",
    "tabs.onRemoved",
    "tabs.onActivated",
    "tabs.onMoved",
    "tabs.onAttached",
    "tabs.onDetached",
    "tabs.onHighlighted",
    "tabs.onZoomChange",
    "windows.onCreated",
    "windows.onRemoved",
    "windows.onFocusChanged",
    "windows.onBoundsChanged",
    // From the browser UI (see `browser_api`).
    "action.onClicked",
    "action.onUserSettingsChanged",
    "pageAction.onClicked",
    "contextMenus.onClicked",
    "contextMenus.onShown",
    "contextMenus.onHidden",
    "commands.onCommand",
    "commands.onChanged",
    "management.onInstalled",
    "management.onUninstalled",
    "management.onEnabled",
    "management.onDisabled",
    // From the network stack and navigations (see `network`).
    "declarativeNetRequest.onRuleMatchedDebug",
    "webRequest.onBeforeRequest",
    "webRequest.onBeforeSendHeaders",
    "webRequest.onSendHeaders",
    "webRequest.onHeadersReceived",
    "webRequest.onAuthRequired",
    "webRequest.onBeforeRedirect",
    "webRequest.onResponseStarted",
    "webRequest.onCompleted",
    "webRequest.onErrorOccurred",
    "webNavigation.onBeforeNavigate",
    "webNavigation.onCommitted",
    "webNavigation.onDOMContentLoaded",
    "webNavigation.onCompleted",
    "webNavigation.onErrorOccurred",
    "webNavigation.onHistoryStateUpdated",
    "webNavigation.onReferenceFragmentUpdated",
    "webNavigation.onCreatedNavigationTarget",
    "webNavigation.onTabReplaced",
    // Browser data APIs (see `data_api`).
    "cookies.onChanged",
    "bookmarks.onCreated",
    "bookmarks.onRemoved",
    "bookmarks.onChanged",
    "bookmarks.onMoved",
    "bookmarks.onChildrenReordered",
    "bookmarks.onImportBegan",
    "bookmarks.onImportEnded",
    "history.onVisited",
    "history.onVisitRemoved",
    "history.onTitleChanged",
    "downloads.onCreated",
    "downloads.onErased",
    "downloads.onChanged",
    "downloads.onDeterminingFilename",
    "sessions.onChanged",
    // Platform, UI surfaces and settings APIs (see `platform_api`).
    "notifications.onClicked",
    "notifications.onClosed",
    "notifications.onButtonClicked",
    "notifications.onShown",
    "notifications.onPermissionLevelChanged",
    "idle.onStateChanged",
    "omnibox.onInputStarted",
    "omnibox.onInputChanged",
    "omnibox.onInputEntered",
    "omnibox.onInputCancelled",
    "omnibox.onDeleteSuggestion",
    "tabGroups.onCreated",
    "tabGroups.onMoved",
    "tabGroups.onRemoved",
    "tabGroups.onUpdated",
    "sidePanel.onOpened",
    "theme.onUpdated",
    "contextualIdentities.onCreated",
    "contextualIdentities.onRemoved",
    "contextualIdentities.onUpdated",
    "fontSettings.onDefaultFixedFontSizeChanged",
    "fontSettings.onDefaultFontSizeChanged",
    "fontSettings.onFontChanged",
    "fontSettings.onMinimumFontSizeChanged",
    "captivePortal.onConnectivityAvailable",
    "captivePortal.onStateChanged",
];

/// Everything the core keeps for one installed extension.
pub struct Extension {
    pub id: String,
    pub manifest: Manifest,
    /// `chrome-extension://<id>/` or `moz-extension://<uuid>/`; chosen by the host.
    pub base_url: String,
    pub permissions: PermissionState,
    pub local: StorageArea,
    pub sync: StorageArea,
    pub session: StorageArea,
    pub alarms: AlarmManager,
    pub i18n: I18n,
    /// The extension's files, served by [`Dispatcher::serve`].
    pub files: Files,
    /// The toolbar button's state (`action.*`, `browserAction.*`, `pageAction.*`).
    pub action: crate::ui::ActionState,
    /// Context-menu items (`contextMenus.*`, `menus.*`); kept across restarts.
    pub menus: crate::ui::Menus,
    /// Shortcut changes (`commands.update`, or the user's).
    pub commands: crate::ui::Commands,
    /// `runtime.setUninstallURL`.
    pub uninstall_url: Option<String>,
    /// How many of `manifest.content_scripts` come from the manifest; the rest are `scripting.registerContentScripts`.
    pub static_content_scripts: usize,
    pub registered_scripts: Vec<crate::browser_api::RegisteredScript>,
    /// `declarativeNetRequest` rules.
    pub dnr: crate::dnr::ExtensionRules,
    /// Install order: where two extensions disagree about a request, the one installed last wins.
    pub seq: u64,
    /// Whether the extension is enabled (`management.setEnabled`).
    pub enabled: bool,
}

pub struct InstallOptions {
    pub base_url: String,
    /// `(locale, messages.json contents)` for every `_locales/*/messages.json` in the package.
    pub locales: Vec<(String, String)>,
    /// All files of the package, for serving.
    pub files: Files,
}

#[derive(Clone)]
pub struct Dispatcher {
    pub(crate) host: Rc<dyn ExtensionHost>,
    registry: Rc<Registry>,
    pub(crate) extensions: Rc<RefCell<HashMap<String, Extension>>>,
    pub(crate) bus: Rc<RefCell<Bus>>,
    /// The browser's windows and tabs, as last reported by the host (see `crate::browser`).
    pub(crate) browser: Rc<RefCell<crate::browser::BrowserState>>,
    /// Calls waiting for the host to run injected code; see `crate::browser_api`.
    pub(crate) next_serial: Rc<std::cell::Cell<u64>>,
    /// `webRequest` listeners, requests waiting for them, and the frames of each tab; see `crate::network`.
    pub(crate) network: Rc<RefCell<crate::network::NetworkState>>,
    /// Browser-data state (`bookmarks`, `downloads`, `sessions`); see `crate::data_api`.
    pub(crate) data: Rc<RefCell<crate::data_api::DataState>>,
    /// UI-surface, productivity, and settings state; see `crate::platform_api`.
    pub(crate) platform: Rc<RefCell<crate::platform_api::PlatformState>>,
}

fn err(kind: ErrorKind, m: impl Into<String>) -> ApiError {
    ApiError::new(kind, m)
}

/// Every string in `value` with its `__MSG_*__` references replaced, as Firefox shows the manifest to extensions.
fn localize_strings(value: &Value, i18n: &I18n) -> Value {
    match value {
        Value::String(text) if text.contains("__MSG_") => Value::String(i18n.localize(text)),
        Value::Array(items) => Value::Array(items.iter().map(|v| localize_strings(v, i18n)).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), localize_strings(v, i18n))).collect()),
        other => other.clone(),
    }
}

impl Dispatcher {
    pub fn new(host: Rc<dyn ExtensionHost>, registry: Registry) -> Dispatcher {
        Dispatcher {
            host,
            registry: Rc::new(registry),
            extensions: Rc::new(RefCell::new(HashMap::new())),
            bus: Rc::new(RefCell::new(Bus::new())),
            browser: Rc::new(RefCell::new(crate::browser::BrowserState::default())),
            next_serial: Rc::new(std::cell::Cell::new(0)),
            network: Rc::new(RefCell::new(crate::network::NetworkState::default())),
            data: Rc::new(RefCell::new(crate::data_api::DataState::default())),
            platform: Rc::new(RefCell::new(crate::platform_api::PlatformState::default())),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Registers an extension, restoring persisted storage and optional-permission grants from the host.
    pub fn install(&self, id: &str, manifest: Manifest, options: InstallOptions) {
        let unlimited = manifest.uses_permission("unlimitedStorage");
        let mut permissions = PermissionState::from_manifest(&manifest);
        if let Some(saved) = self.host.read_storage(id, "permissions").and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
            permissions.restore_granted(&saved);
        }
        let local_quota = if unlimited { Quota::UNLIMITED } else { Quota::LOCAL };
        let load = |key: &str, quota: Quota| match self.host.read_storage(id, key) {
            Some(text) => StorageArea::from_json(&text, quota),
            None => StorageArea::new(quota),
        };
        let mut i18n = I18n::new(id, &manifest.default_locale, &self.host.ui_locale());
        for (locale, text) in &options.locales {
            // A broken locale file must not prevent installation; the message simply falls back.
            let _ = i18n.add_locale(locale, text);
        }
        let read_json = |key: &str| self.host.read_storage(id, key).and_then(|t| serde_json::from_str::<Value>(&t).ok());
        // Event pages and service workers keep their menu items across restarts; persistent pages create them again.
        let menus = if crate::browser_api::keeps_menus(&manifest) {
            read_json("menus").map(|v| crate::ui::Menus::from_json(&v)).unwrap_or_default()
        } else {
            Default::default()
        };
        let mut commands = crate::ui::Commands::default();
        let mut action = crate::ui::ActionState { pinned: manifest.action.is_some(), ..Default::default() };
        if let Some(saved) = read_json("ui") {
            if let Some(map) = saved.get("shortcuts").and_then(Value::as_object) {
                commands.overrides = map.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect();
            }
            if let Some(p) = saved.get("pinned").and_then(Value::as_bool) {
                action.pinned = p;
            }
        }
        let static_content_scripts = manifest.content_scripts.len();
        let registered_scripts = read_json("scripts").map(|v| crate::browser_api::RegisteredScript::list_from_json(&v)).unwrap_or_default();
        let mut dnr = crate::dnr::ExtensionRules::from_manifest(&manifest.declarative_net_request_rulesets, |p| options.files.text(p));
        if let Some(saved) = read_json("dnr") {
            dnr.restore(&saved, |p| options.files.text(p));
        }
        self.next_serial.set(self.next_serial.get() + 1);
        let ext = Extension {
            dnr,
            seq: self.next_serial.get(),
            enabled: true,
            action,
            menus,
            commands,
            uninstall_url: None,
            static_content_scripts,
            registered_scripts,
            id: id.to_string(),
            local: load("local", local_quota),
            sync: load("sync", Quota::SYNC),
            session: StorageArea::new(Quota::SESSION).with_access_level(AccessLevel::TrustedContexts),
            alarms: AlarmManager::default(),
            permissions,
            i18n,
            files: options.files,
            base_url: options.base_url,
            manifest,
        };
        let has_rules = ext.dnr.has_rules();
        self.extensions.borrow_mut().insert(id.to_string(), ext);
        self.rebuild_content_scripts(id);
        if has_rules {
            // The browser has to start reporting requests.
            self.host.dispatch_event(id, "__network.changed", vec![]);
        }
        if let Some(info) = self.management_info(id) {
            let watchers: Vec<String> = self
                .installed_ids()
                .into_iter()
                .filter(|other| other != id && self.with_extension(other, |e| e.permissions.has_api("management")).unwrap_or(false))
                .collect();
            for other in watchers {
                self.host.dispatch_event(&other, "management.onInstalled", vec![info.clone()]);
            }
        }
    }

    pub fn uninstall(&self, id: &str) -> bool {
        let removed = self.extensions.borrow_mut().remove(id);
        // runtime.setUninstallURL: the page opens once the extension is gone.
        if let Some(url) = removed.as_ref().and_then(|e| e.uninstall_url.clone()) {
            let call = ApiCall {
                extension_id: id.into(),
                function: "host.tabs.create".into(),
                args: vec![json!({ "url": url, "active": true })],
                caller: Caller::background(),
            };
            self.host.call_browser(&call, Box::new(|_| {}));
        }
        let removed = removed.is_some();
        self.network_extension_removed(id);
        self.data.borrow_mut().sessions.drop_extension(id);
        self.platform.borrow_mut().drop_extension(id);
        if removed {
            self.host.dispatch_event(id, "__network.changed", vec![]);
            self.broadcast_permitted("management", "management.onUninstalled", vec![json!(id)]);
        }
        let effects = self.bus.borrow_mut().drop_extension(id);
        self.run_effects(effects);
        removed
    }

    // ---- Contexts and messaging -----------------------------------------------------------------------------------

    /// Registers a place extension code runs (background page, popup, a frame's content scripts). Call before any API call
    /// is made from it. Ids are chosen by the browser and must not be reused while the context lives.
    pub fn context_created(&self, info: ContextInfo) -> Result<(), ApiError> {
        if !self.extensions.borrow().contains_key(&info.extension_id) {
            return Err(err(ErrorKind::InvalidArguments, format!("extension '{}' is not installed", info.extension_id)));
        }
        self.bus.borrow_mut().context_created(info)
    }

    /// The context is gone (tab closed, navigated away, background shut down). Ports it was on are closed and messages
    /// waiting for it are settled.
    pub fn context_destroyed(&self, context_id: u64) {
        let effects = self.bus.borrow_mut().context_destroyed(context_id);
        self.run_effects(effects);
        self.network_context_destroyed(context_id);
    }

    fn run_effects(&self, effects: Vec<Effect>) {
        // No bus or extension borrow is held here, so delivery and replies may re-enter the dispatcher.
        for effect in effects {
            match effect {
                Effect::Deliver(context, envelope) => self.host.deliver_to_context(context, envelope),
                Effect::Complete(reply, result) => reply(result),
            }
        }
    }

    fn sender_json(&self, info: &ContextInfo) -> Value {
        let mut sender = json!({ "id": info.extension_id });
        if let Some(url) = &info.url {
            sender["url"] = json!(url);
            // Chrome's MessageSender.origin; Bitwarden refuses connections from senders without one.
            let origin = crate::browser::origin_of(url);
            sender["origin"] = json!(if origin.is_empty() || origin.ends_with("://") { "null".to_string() } else { origin });
        }
        if let Some(frame) = info.frame_id {
            sender["frameId"] = json!(frame);
        }
        if let Some(tab) = info.tab_id {
            // The tab as the receiving extension may see it, from the browser's snapshot; the host's description only if the
            // snapshot does not have it.
            let mut state = self.browser.borrow().tab(tab).cloned();
            // A document in the tab's top frame is the tab's page, even if the browser has not reported the navigation yet
            // (content scripts connect at document_start, before the tab's snapshot shows the new URL).
            if let (Some(t), Some(url), Some(0)) = (state.as_mut(), info.url.as_ref(), info.frame_id)
                && t.url != *url
            {
                t.url = url.clone();
                t.status = "loading".into();
            }
            sender["tab"] = state
                .and_then(|t| self.with_visibility(&info.extension_id, |v| crate::browser::tab_json(&t, v)))
                .or_else(|| self.host.describe_tab(tab))
                .unwrap_or_else(|| json!({ "id": tab }));
        }
        sender
    }

    /// The calling context, which must be registered and belong to the extension named in the call.
    pub(crate) fn calling_context(&self, call: &ApiCall) -> Result<ContextInfo, ApiError> {
        let id =
            call.caller.context_id.ok_or_else(|| err(ErrorKind::InvalidArguments, "this call did not come from a registered context"))?;
        match self.bus.borrow().context(id) {
            Some(info) if info.extension_id == call.extension_id => Ok(info.clone()),
            _ => Err(err(ErrorKind::Runtime, "Extension context invalidated.")),
        }
    }

    /// `runtime.sendMessage([extensionId], message, [options])`
    fn runtime_send_message(&self, call: &ApiCall, reply: Reply) {
        let sender = match self.calling_context(call) {
            Ok(s) => s,
            Err(e) => return reply(Err(e)),
        };
        if call.args.is_empty() {
            return reply(Err(ApiError::invalid("sendMessage needs a message")));
        }
        let rest = match call.args[0].as_str() {
            Some(target) if call.args.len() >= 2 => {
                if target != call.extension_id {
                    // Messaging other extensions (externally_connectable) is not supported.
                    return reply(Err(ApiError::runtime(NO_RECEIVER)));
                }
                &call.args[1..]
            }
            _ => &call.args[..],
        };
        let sender_json = self.sender_json(&sender);
        let effects = self.bus.borrow_mut().send_message(sender.id, rest[0].clone(), Target::Runtime, sender_json, reply);
        self.run_effects(effects);
    }

    /// `tabs.sendMessage(tabId, message, [options])`
    fn tabs_send_message(&self, call: &ApiCall, reply: Reply) {
        let sender = match self.calling_context(call) {
            Ok(s) => s,
            Err(e) => return reply(Err(e)),
        };
        let target = match parse_tab_target(call.args.first(), call.args.get(2)) {
            Ok(t) => t,
            Err(e) => return reply(Err(e)),
        };
        if call.args.len() < 2 {
            return reply(Err(ApiError::invalid("sendMessage needs a message")));
        }
        let sender_json = self.sender_json(&sender);
        let effects = self.bus.borrow_mut().send_message(sender.id, call.args[1].clone(), target, sender_json, reply);
        self.run_effects(effects);
    }

    /// `runtime.connect([extensionId], [connectInfo])` and `tabs.connect(tabId, [connectInfo])`. The shim appends the id it
    /// chose for the port as the last argument; other callers get one generated and returned as `{ "portId": ... }`.
    fn connect(&self, call: &ApiCall, for_tab: bool) -> ApiResult {
        let sender = self.calling_context(call)?;
        let args = &call.args;
        let (target, rest): (Target, &[Value]) = if for_tab {
            let info = args.get(1).filter(|v| v.is_object());
            (parse_tab_target(args.first(), info)?, args.get(1..).unwrap_or(&[]))
        } else {
            match args.first().and_then(Value::as_str) {
                Some(other) if other != call.extension_id => return Err(ApiError::runtime(NO_RECEIVER)),
                Some(_) => (Target::Runtime, &args[1..]),
                None => (Target::Runtime, args.get(if args.first().is_some_and(Value::is_null) { 1 } else { 0 }..).unwrap_or(&[])),
            }
        };
        let name = rest.iter().find(|v| v.is_object()).and_then(|v| v.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
        let explicit_id = rest.iter().rev().find_map(Value::as_str).map(String::from);

        let sender_json = self.sender_json(&sender);
        let mut bus = self.bus.borrow_mut();
        let port_id = explicit_id.unwrap_or_else(|| bus.fresh_port_id(sender.id));
        let effects = bus.connect(sender.id, port_id.clone(), &name, target, sender_json)?;
        drop(bus);
        self.run_effects(effects);
        Ok(json!({ "portId": port_id }))
    }

    /// Whether an extension's own page may fetch a URL without CORS: in Chrome and Firefox, host permissions make those
    /// origins the extension's for its own pages (not for its content scripts, which fetch as the page).
    fn allows_fetch(&self, call: &ApiCall) -> Result<Value, ApiError> {
        let ctx = self.calling_context(call)?;
        if !matches!(ctx.kind, ContextKind::Background | ContextKind::ExtensionPage) {
            return Ok(json!(false));
        }
        let url = call.args.first().and_then(Value::as_str).ok_or_else(|| ApiError::invalid("url must be a string"))?;
        Ok(json!(self.with_extension(&ctx.extension_id, |e| e.permissions.has_host(url)).unwrap_or(false)))
    }

    /// `importScripts` in a background service worker (run as a page here): the text of one of the extension's own files,
    /// which the browser then evaluates in that context. Only the extension's background and pages may ask, and only for
    /// files of their own package.
    fn read_script(&self, call: &ApiCall) -> Result<Value, ApiError> {
        let ctx = self.calling_context(call)?;
        if !matches!(ctx.kind, ContextKind::Background | ContextKind::ExtensionPage) {
            return Err(ApiError::runtime("importScripts is only available to the extension's own pages"));
        }
        let path = call.args.first().and_then(Value::as_str).ok_or_else(|| ApiError::invalid("path must be a string"))?;
        let path = path.split(['?', '#']).next().unwrap_or("").trim_start_matches('/');
        if path.split('/').any(|segment| segment == "..") {
            return Err(ApiError::invalid("path must stay inside the extension"));
        }
        let text = self
            .with_extension(&ctx.extension_id, |e| e.files.get(path).map(|bytes| String::from_utf8_lossy(bytes).into_owned()))
            .flatten()
            .ok_or_else(|| ApiError::runtime(format!("importScripts: the extension has no file '{path}'")))?;
        Ok(Value::String(text))
    }

    /// Calls the shim makes to answer the bus: `__bus.accept`, `__bus.post`, `__bus.disconnect`, `__bus.reply`. They are not
    /// part of the extension-visible API, so they bypass the registry; the bus checks that the caller is an end of the port
    /// or an addressee of the message.
    fn bus_internal(&self, call: ApiCall, reply: Reply) {
        let ctx = match self.calling_context(&call) {
            Ok(c) => c.id,
            Err(e) => return reply(Err(e)),
        };
        let arg = |i: usize| call.args.get(i).unwrap_or(&Value::Null);
        let port_id = || arg(0).as_str().ok_or_else(|| ApiError::invalid("portId must be a string"));
        let effects: Result<Vec<Effect>, ApiError> = (|| {
            let mut bus = self.bus.borrow_mut();
            Ok(match call.function.as_str() {
                "__bus.accept" => bus.accept(ctx, port_id()?, arg(1).as_bool().unwrap_or(false)),
                "__bus.post" => bus.post(ctx, port_id()?, arg(1).clone())?,
                "__bus.disconnect" => bus.disconnect(ctx, port_id()?),
                "__bus.reply" => {
                    let request = arg(0).as_u64().ok_or_else(|| ApiError::invalid("requestId must be a number"))?;
                    bus.reply(ctx, request, arg(1).as_bool().unwrap_or(false), arg(2).as_bool().unwrap_or(false), arg(3).clone())?
                }
                other => return Err(err(ErrorKind::UnknownApi, format!("'{other}' is not a known internal call"))),
            })
        })();
        match effects {
            Ok(effects) => {
                self.run_effects(effects);
                reply(Ok(Value::Null));
            }
            Err(e) => reply(Err(e)),
        }
    }

    pub fn is_installed(&self, id: &str) -> bool {
        self.extensions.borrow().contains_key(id)
    }

    /// Installed extension ids, in a stable order.
    pub fn installed_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.extensions.borrow().keys().cloned().collect();
        ids.sort();
        ids
    }

    pub(crate) fn find_extension_by_base_url(&self, base: &str) -> Option<String> {
        self.extensions.borrow().values().find(|e| e.base_url.eq_ignore_ascii_case(base)).map(|e| e.id.clone())
    }

    /// Read access to an installed extension, for the host UI.
    pub fn with_extension<R>(&self, id: &str, f: impl FnOnce(&Extension) -> R) -> Option<R> {
        self.extensions.borrow().get(id).map(f)
    }

    /// The API surface to inject into an extension context (see `Shim/webext-shim.js`).
    pub fn surface_for(&self, id: &str) -> Option<Value> {
        let exts = self.extensions.borrow();
        let ext = exts.get(id)?;
        Some(self.registry.surface_json(ext.manifest.manifest_version, engine_of(ext), &|p| permission_granted(&ext.permissions, p)))
    }

    /// Everything a context needs at startup, as JSON: id, manifest, base URL and the visible API surface.
    pub fn init_data_for(&self, id: &str) -> Option<Value> {
        let exts = self.extensions.borrow();
        let ext = exts.get(id)?;
        Some(json!({
            "id": ext.id,
            "baseUrl": ext.base_url,
            // Firefox's runtime.getManifest() answers with __MSG_*__ strings localized; Chrome's leaves them as written.
            "manifest": if crate::browser_api::is_firefox(ext) { localize_strings(&ext.manifest.raw, &ext.i18n) } else { ext.manifest.raw.clone() },
            // i18n.getUILanguage answers a language tag ("en-US"); @@ui_locale keeps Chrome's "en_US" form.
            "uiLanguage": ext.i18n.ui_locale().replace('_', "-"),
            // Chrome-style extensions get only `chrome`, Firefox-style ones `browser` too: extensions test `typeof browser`.
            "dialect": if crate::browser_api::is_firefox(ext) { "firefox" } else { "chrome" },
            "syncFunctions": self.registry.sync_functions(),
            "surface": self.registry.surface_json(ext.manifest.manifest_version, engine_of(ext), &|p| permission_granted(&ext.permissions, p)),
        }))
    }

    /// What the browser needs to host the extension: the background context to start, the CSP to enforce on extension pages,
    /// the toolbar action, options page, icons and permissions. Names and titles have `__MSG_*__` already resolved.
    pub fn info_for(&self, id: &str) -> Option<Value> {
        use crate::manifest::{ActionKind, BackgroundKind, Dialect};
        let exts = self.extensions.borrow();
        let e = exts.get(id)?;
        let m = &e.manifest;
        let icons =
            |set: &crate::manifest::IconSet| set.0.iter().map(|(size, path)| json!({ "size": size, "path": path })).collect::<Vec<_>>();
        let b = &m.background;
        Some(json!({
            "id": e.id,
            "name": e.i18n.localize(&m.name),
            "version": m.version,
            "description": e.i18n.localize(&m.description),
            "manifestVersion": m.manifest_version,
            "defaultLocale": m.default_locale,
            "dialect": match m.dialect { Dialect::Chrome => "chrome", Dialect::Firefox => "firefox", Dialect::Both => "both" },
            "baseUrl": e.base_url,
            "csp": { "extensionPages": m.csp_extension_pages, "sandbox": m.csp_sandbox },
            "background": {
                "kind": match b.kind { BackgroundKind::None => "none", BackgroundKind::Page => "page", BackgroundKind::Scripts => "scripts", BackgroundKind::ServiceWorker => "service_worker" },
                "scripts": b.scripts, "page": b.page, "serviceWorker": b.service_worker, "persistent": b.persistent, "module": b.is_module,
            },
            "action": m.action.as_ref().map(|a| json!({
                "kind": match a.kind { ActionKind::Action => "action", ActionKind::BrowserAction => "browser_action", ActionKind::PageAction => "page_action" },
                "title": e.i18n.localize(&a.default_title), "popup": a.default_popup, "icons": icons(&a.default_icon),
            })),
            "options": { "page": m.options_page, "openInTab": m.options_open_in_tab },
            "icons": icons(&m.icons),
            "permissions": e.permissions.all().permissions,
            "hostPermissions": e.permissions.all().origins,
            "optionalPermissions": m.optional_permissions,
            "optionalHostPermissions": m.optional_host_permissions,
            "dnrRulesets": m.declarative_net_request_rulesets.iter().map(|r| json!({ "id": r.id, "path": r.path, "enabled": r.enabled })).collect::<Vec<_>>(),
            "commands": m.commands.iter().map(|c| json!({ "name": c.name, "description": c.description, "suggestedKey": c.suggested_key })).collect::<Vec<_>>(),
        }))
    }

    /// Like [`Dispatcher::init_data_for`] for one registered context: adds `contextId` and `contextKind` and, for content
    /// scripts, trims the API surface to what they may use.
    pub fn init_data_for_context(&self, context_id: u64) -> Option<Value> {
        let info = self.bus.borrow().context(context_id)?.clone();
        let mut data = self.init_data_for(&info.extension_id)?;
        data["contextId"] = json!(context_id);
        data["contextKind"] = json!(info.kind.as_str());
        if info.kind == ContextKind::ContentScript {
            restrict_surface_for_content_script(&mut data["surface"]);
        }
        Some(data)
    }

    pub fn notify_installed(&self, id: &str, reason: &str, previous_version: Option<&str>) {
        let mut details = json!({ "reason": reason });
        if let Some(v) = previous_version {
            details["previousVersion"] = json!(v);
        }
        self.host.dispatch_event(id, "runtime.onInstalled", vec![details]);
    }

    pub fn notify_startup(&self, id: &str) {
        self.host.dispatch_event(id, "runtime.onStartup", vec![]);
    }

    /// Fires due alarms. Call when the host's timer (see `schedule_alarm_check`) elapses.
    pub fn tick(&self) {
        let now = self.host.now_ms();
        let mut fired: Vec<(String, Value)> = vec![];
        let mut next: Option<i64> = None;
        {
            let mut exts = self.extensions.borrow_mut();
            for ext in exts.values_mut() {
                for alarm in ext.alarms.take_due(now) {
                    fired.push((ext.id.clone(), alarm.to_json()));
                }
                if let Some(due) = ext.alarms.next_due_ms() {
                    next = Some(next.map_or(due, |n: i64| n.min(due)));
                }
            }
        }
        for (id, alarm) in fired {
            self.host.dispatch_event(&id, "alarms.onAlarm", vec![alarm]);
        }
        if let Some(at) = next {
            self.host.schedule_alarm_check(at);
        }
    }

    pub fn dispatch(&self, call: ApiCall, reply: Reply) {
        if call.function.starts_with("__bus.") {
            return self.bus_internal(call, reply);
        }
        if call.function.starts_with("__webRequest.") {
            return self.web_request_internal(&call, reply);
        }
        if call.function.starts_with("__setting.") || call.function.starts_with("__omnibox.") {
            return self.platform_internal(&call, reply);
        }
        if call.function == "__contentScripts.unregister" {
            return reply(self.unregister_content_script(&call));
        }
        if call.function == "__runtime.readScript" {
            return reply(self.read_script(&call));
        }
        if call.function == "__permissions.allowsFetch" {
            return reply(self.allows_fetch(&call));
        }
        match self.route(&call) {
            Err(e) => reply(Err(e)),
            Ok((member, _)) if member.status == Status::Delegated => {
                let mut canonical = call.clone();
                canonical.function = member.full_name.clone();
                self.host.call_browser(&canonical, reply);
            }
            Ok((member, _)) => self.handle(&member, &call, reply),
        }
    }

    /// Checks everything that can be decided without running the call.
    fn route(&self, call: &ApiCall) -> Result<(Member, ()), ApiError> {
        let member = self
            .registry
            .lookup(&call.function)
            .ok_or_else(|| err(ErrorKind::UnknownApi, format!("'{}' is not a known extension API", call.function)))?
            .clone();
        let exts = self.extensions.borrow();
        let ext = exts.get(&call.extension_id).ok_or_else(|| err(ErrorKind::Runtime, "Extension context invalidated."))?;

        if !member.available_in_manifest_version(ext.manifest.manifest_version) {
            return Err(err(
                ErrorKind::NotAvailable,
                format!("'{}' is not available in manifest_version {}", member.full_name, ext.manifest.manifest_version),
            ));
        }
        if matches!(member.status, Status::Planned | Status::Unsupported) {
            return Err(err(ErrorKind::NotAvailable, format!("'{}' is not supported by this browser yet", member.full_name)));
        }
        if member.kind != Kind::Function {
            return Err(err(ErrorKind::InvalidArguments, format!("'{}' is not a function", member.full_name)));
        }
        if let Some(p) = &member.permission
            && !permission_granted(&ext.permissions, p)
        {
            return Err(err(ErrorKind::PermissionDenied, format!("'{}' requires the \"{p}\" permission", member.full_name)));
        }
        if call.caller.context == ContextKind::ContentScript && !content_script_allows(&member.namespace, &member.name) {
            return Err(err(ErrorKind::PermissionDenied, format!("'{}' is not available in content scripts", member.full_name)));
        }
        Ok((member, ()))
    }

    fn handle(&self, member: &Member, call: &ApiCall, reply: Reply) {
        let result = match member.full_name.as_str() {
            "runtime.getManifest" => self.with_ext(call, |e| Ok(e.manifest.raw.clone())),
            "runtime.getURL" | "extension.getURL" => self.with_ext(call, |e| {
                let path = call.arg(0).as_str().ok_or_else(|| ApiError::invalid("path must be a string"))?;
                Ok(json!(format!("{}{}", e.base_url, path.trim_start_matches('/'))))
            }),
            "i18n.getMessage" => self.i18n_get_message(call),
            "i18n.getUILanguage" => Ok(json!(self.host.ui_locale())),
            "i18n.detectLanguage" => match call.arg(0).as_str() {
                Some(text) => Ok(crate::i18n::detect_language(text)),
                None => Err(ApiError::invalid("text must be a string")),
            },
            "i18n.getPreferredSystemLanguages" => Ok(json!([self.host.ui_locale()])),
            "permissions.contains" => self.permissions_contains(call),
            "permissions.getAll" => self.with_ext(call, |e| Ok(e.permissions.all().to_json())),
            "permissions.remove" => self.permissions_remove(call),
            "permissions.request" => return self.permissions_request(call, reply),
            "runtime.sendMessage" => return self.runtime_send_message(call, reply),
            "tabs.sendMessage" => return self.tabs_send_message(call, reply),
            "runtime.connect" => self.connect(call, false),
            "tabs.connect" => self.connect(call, true),
            n if crate::browser_api::handles(n) => return self.browser_api(member, call, reply),
            n if crate::network::handles(n) => return self.network_api(member, call, reply),
            n if crate::data_api::handles(n) => return self.data_api(member, call, reply),
            n if crate::platform_api::handles(n) => return self.platform_api(member, call, reply),
            n if n.starts_with("storage.") => self.storage(member, call),
            n if n.starts_with("alarms.") => self.alarms(member, call),
            other => Err(err(ErrorKind::Unhandled, format!("registry marks '{other}' as handled by the core, but no handler exists"))),
        };
        reply(result);
    }

    pub(crate) fn with_ext<R>(&self, call: &ApiCall, f: impl FnOnce(&Extension) -> Result<R, ApiError>) -> Result<R, ApiError> {
        let exts = self.extensions.borrow();
        f(exts.get(&call.extension_id).ok_or_else(|| err(ErrorKind::Runtime, "Extension context invalidated."))?)
    }

    pub(crate) fn with_ext_mut<R>(&self, call: &ApiCall, f: impl FnOnce(&mut Extension) -> Result<R, ApiError>) -> Result<R, ApiError> {
        let mut exts = self.extensions.borrow_mut();
        f(exts.get_mut(&call.extension_id).ok_or_else(|| err(ErrorKind::Runtime, "Extension context invalidated."))?)
    }

    fn i18n_get_message(&self, call: &ApiCall) -> ApiResult {
        let name = call.arg(0).as_str().ok_or_else(|| ApiError::invalid("messageName must be a string"))?;
        let subs: Vec<String> = match call.arg(1) {
            Value::Null => vec![],
            Value::String(s) => vec![s.clone()],
            Value::Array(items) => {
                if items.len() > 9 {
                    return Err(ApiError::invalid("Too many substitutions: at most 9 are allowed"));
                }
                items
                    .iter()
                    .map(|i| i.as_str().map(String::from).ok_or_else(|| ApiError::invalid("substitutions must be strings")))
                    .collect::<Result<_, _>>()?
            }
            _ => return Err(ApiError::invalid("substitutions must be a string or an array of strings")),
        };
        self.with_ext(call, |e| Ok(json!(e.i18n.get_message(name, &subs))))
    }

    fn storage(&self, member: &Member, call: &ApiCall) -> ApiResult {
        let parts: Vec<&str> = member.full_name.split('.').collect();
        let [_, area_name, op] = parts[..] else { return Err(err(ErrorKind::Unhandled, member.full_name.clone())) };

        if area_name == "managed" {
            return match op {
                "get" => Ok(json!({})),
                "getBytesInUse" => Ok(json!(0)),
                _ => Err(err(ErrorKind::Unhandled, member.full_name.clone())),
            };
        }
        if op == "setAccessLevel" {
            if call.caller.context == ContextKind::ContentScript {
                return Err(err(ErrorKind::PermissionDenied, "Context cannot set the storage access level."));
            }
            let level = AccessLevel::parse(call.arg(0)).map_err(ApiError::invalid)?;
            return self.with_ext_mut(call, |e| {
                let area = match area_name {
                    "local" => &mut e.local,
                    "sync" => &mut e.sync,
                    "session" => &mut e.session,
                    _ => return Err(err(ErrorKind::Unhandled, member.full_name.clone())),
                };
                area.set_access_level(level);
                Ok(Value::Null)
            });
        }

        let mut changes: Vec<Change> = vec![];
        let result = self.with_ext_mut(call, |e| {
            let area = match area_name {
                "local" => &mut e.local,
                "sync" => &mut e.sync,
                "session" => &mut e.session,
                _ => return Err(err(ErrorKind::Unhandled, member.full_name.clone())),
            };
            if call.caller.context == ContextKind::ContentScript && !area.allows_untrusted() {
                return Err(err(ErrorKind::PermissionDenied, "Access to storage is not allowed from this context."));
            }
            let map = |e: crate::storage::StorageError| ApiError::runtime(e.to_string());
            match op {
                "get" => area.get(call.arg(0)).map_err(map),
                "getBytesInUse" => area.bytes_in_use(call.arg(0)).map(|n| json!(n)).map_err(map),
                "getKeys" => Ok(json!(area.keys())),
                "set" => {
                    changes = area.set(call.arg(0)).map_err(map)?;
                    Ok(Value::Null)
                }
                "remove" => {
                    changes = area.remove(call.arg(0)).map_err(map)?;
                    Ok(Value::Null)
                }
                "clear" => {
                    changes = area.clear();
                    Ok(Value::Null)
                }
                _ => Err(err(ErrorKind::Unhandled, member.full_name.clone())),
            }
        });

        if result.is_ok() && !changes.is_empty() {
            if area_name != "session" {
                let data = self.with_ext(call, |e| Ok(if area_name == "local" { e.local.to_json() } else { e.sync.to_json() }))?;
                self.host.write_storage(&call.extension_id, area_name, &data);
            }
            // Listeners registered on `storage.onChanged` receive (changes, areaName).
            self.host.dispatch_event(&call.extension_id, "storage.onChanged", vec![changes_to_json(&changes), json!(area_name)]);
            // Each area's own event gets the changes only.
            let area_event = match area_name {
                "local" => "storage.local.onChanged",
                "sync" => "storage.sync.onChanged",
                _ => "storage.session.onChanged",
            };
            self.host.dispatch_event(&call.extension_id, area_event, vec![changes_to_json(&changes)]);
        }
        result
    }

    fn alarms(&self, member: &Member, call: &ApiCall) -> ApiResult {
        let now = self.host.now_ms();
        // `create`/`get`/`clear` take an optional leading name.
        let (name, rest) = match call.arg(0) {
            Value::String(s) => (s.clone(), 1),
            _ => (String::new(), 0),
        };
        let op = member.name.as_str();
        let mut next_due = None;
        let result = self.with_ext_mut(call, |e| match op {
            "create" => {
                e.alarms.create(&name, call.arg(rest), now).map_err(ApiError::invalid)?;
                next_due = e.alarms.next_due_ms();
                Ok(Value::Null)
            }
            "get" => Ok(e.alarms.get(&name).map_or(Value::Null, |a| a.to_json())),
            "getAll" => Ok(Value::Array(e.alarms.get_all().iter().map(|a| a.to_json()).collect())),
            "clear" => Ok(json!(e.alarms.clear(&name))),
            "clearAll" => Ok(json!(e.alarms.clear_all())),
            _ => Err(err(ErrorKind::Unhandled, member.full_name.clone())),
        });
        if let Some(at) = next_due {
            self.host.schedule_alarm_check(at);
        }
        result
    }

    fn permissions_contains(&self, call: &ApiCall) -> ApiResult {
        let req = PermissionRequest::from_json(call.arg(0)).map_err(ApiError::invalid)?;
        self.with_ext(call, |e| Ok(json!(e.permissions.contains(&req))))
    }

    fn permissions_remove(&self, call: &ApiCall) -> ApiResult {
        let req = PermissionRequest::from_json(call.arg(0)).map_err(ApiError::invalid)?;
        let removed = self.with_ext_mut(call, |e| e.permissions.revoke(&req).map_err(ApiError::runtime))?;
        if removed.is_empty() {
            return Ok(json!(false));
        }
        self.persist_permissions(&call.extension_id);
        self.host.dispatch_event(&call.extension_id, "permissions.onRemoved", vec![removed.to_json()]);
        Ok(json!(true))
    }

    fn permissions_request(&self, call: &ApiCall, reply: Reply) {
        let req = match PermissionRequest::from_json(call.arg(0)).map_err(ApiError::invalid) {
            Ok(r) => r,
            Err(e) => return reply(Err(e)),
        };
        let pre = self.with_ext(call, |e| {
            if e.permissions.contains(&req) {
                return Ok(true);
            }
            e.permissions.check_requestable(&req).map_err(ApiError::runtime)?;
            Ok(false)
        });
        match pre {
            Err(e) => return reply(Err(e)),
            Ok(true) => return reply(Ok(json!(true))),
            Ok(false) => {}
        }

        let this = self.clone();
        let id = call.extension_id.clone();
        let req_for_grant = req.clone();
        // No RefCell borrow is held here: the host may answer synchronously.
        self.host.request_permissions(
            &call.extension_id,
            &req,
            Box::new(move |granted| {
                if !granted {
                    return reply(Ok(json!(false)));
                }
                let added = this.extensions.borrow_mut().get_mut(&id).map(|e| e.permissions.grant(&req_for_grant));
                match added {
                    Some(added) => {
                        this.persist_permissions(&id);
                        if !added.is_empty() {
                            this.host.dispatch_event(&id, "permissions.onAdded", vec![added.to_json()]);
                        }
                        reply(Ok(json!(true)));
                    }
                    None => reply(Err(err(ErrorKind::Runtime, "Extension context invalidated."))),
                }
            }),
        );
    }

    fn persist_permissions(&self, id: &str) {
        let data = self.extensions.borrow().get(id).map(|e| e.permissions.granted_to_json().to_string());
        if let Some(data) = data {
            self.host.write_storage(id, "permissions", &data);
        }
    }
}

/// The part of the API a content script may use. Content scripts run next to untrusted pages, so everything else (tabs,
/// cookies, alarms, ...) stays out of reach.
pub fn content_script_allows(namespace: &str, member: &str) -> bool {
    match namespace {
        "i18n" | "storage" | "extension" => true,
        "runtime" => {
            matches!(member, "connect" | "sendMessage" | "getManifest" | "getURL" | "onConnect" | "onMessage" | "id" | "lastError")
        }
        _ => false,
    }
}

fn restrict_surface_for_content_script(surface: &mut Value) {
    if let Some(map) = surface.as_object_mut() {
        map.retain(|ns, _| matches!(ns.as_str(), "i18n" | "storage" | "extension" | "runtime"));
        if let Some(runtime) = map.get_mut("runtime") {
            for list in ["functions", "events", "properties"] {
                if let Some(items) = runtime.get_mut(list).and_then(Value::as_array_mut) {
                    items.retain(|m| m.as_str().is_some_and(|m| content_script_allows("runtime", m)));
                }
            }
        }
    }
}

fn parse_tab_target(tab: Option<&Value>, options: Option<&Value>) -> Result<Target, ApiError> {
    let tab_id = tab.and_then(Value::as_i64).ok_or_else(|| ApiError::invalid("tabId must be an integer"))?;
    let frame_id = options.and_then(|o| o.get("frameId")).and_then(Value::as_i64);
    Ok(Target::Tab { tab_id, frame_id })
}

/// The browser an extension was written for, which decides the API it sees.
fn engine_of(ext: &Extension) -> crate::registry::Engine {
    if crate::browser_api::is_firefox(ext) { crate::registry::Engine::Firefox } else { crate::registry::Engine::Chrome }
}

/// `contextMenus` and Firefox's `menus` are interchangeable.
pub(crate) fn permission_granted(p: &PermissionState, permission: &str) -> bool {
    match permission {
        "contextMenus" | "menus" => p.has_api("contextMenus") || p.has_api("menus"),
        "declarativeNetRequest" => p.has_api("declarativeNetRequest") || p.has_api("declarativeNetRequestWithHostAccess"),
        other => p.has_api(other),
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use std::collections::HashMap;

    type PermissionAnswer = Box<dyn FnOnce(bool)>;

    #[derive(Default)]
    pub struct MockHost {
        pub storage: RefCell<HashMap<(String, String), String>>,
        pub now: RefCell<i64>,
        pub events: RefCell<Vec<(String, String, Vec<Value>)>>,
        pub browser_calls: RefCell<Vec<String>>,
        pub permission_answer: RefCell<bool>,
        pub wakeups: RefCell<Vec<i64>>,
        pub pending_permission: RefCell<Option<PermissionAnswer>>,
        pub defer_permission: RefCell<bool>,
        pub deliveries: RefCell<Vec<(u64, Value)>>,
    }

    impl ExtensionHost for MockHost {
        fn read_storage(&self, id: &str, key: &str) -> Option<String> {
            self.storage.borrow().get(&(id.into(), key.into())).cloned()
        }
        fn write_storage(&self, id: &str, key: &str, json: &str) {
            self.storage.borrow_mut().insert((id.into(), key.into()), json.into());
        }
        fn now_ms(&self) -> i64 {
            *self.now.borrow()
        }
        fn ui_locale(&self) -> String {
            "en-US".into()
        }
        fn dispatch_event(&self, id: &str, event: &str, args: Vec<Value>) {
            self.events.borrow_mut().push((id.into(), event.into(), args));
        }
        fn deliver_to_context(&self, context_id: u64, message: Value) {
            self.deliveries.borrow_mut().push((context_id, message));
        }
        fn call_browser(&self, call: &ApiCall, reply: Reply) {
            self.browser_calls.borrow_mut().push(call.function.clone());
            reply(Ok(json!({ "routed": call.function })));
        }
        fn request_permissions(&self, _id: &str, _req: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
            if *self.defer_permission.borrow() {
                *self.pending_permission.borrow_mut() = Some(reply);
            } else {
                reply(*self.permission_answer.borrow());
            }
        }
        fn schedule_alarm_check(&self, at: i64) {
            self.wakeups.borrow_mut().push(at);
        }
    }

    pub const ID: &str = "abcdefghijklmnopabcdefghijklmnop";

    pub fn setup(manifest_json: &str) -> (Dispatcher, Rc<MockHost>) {
        let host = Rc::new(MockHost::default());
        let d = Dispatcher::new(host.clone(), Registry::builtin());
        let m = crate::manifest::parse_manifest(manifest_json).manifest.expect("valid manifest");
        d.install(ID, m, InstallOptions { base_url: format!("chrome-extension://{ID}/"), locales: vec![], files: Files::new() });
        (d, host)
    }

    pub fn call(d: &Dispatcher, function: &str, args: Vec<Value>) -> ApiResult {
        call_from(d, function, args, Caller::background())
    }

    pub fn call_from(d: &Dispatcher, function: &str, args: Vec<Value>, caller: Caller) -> ApiResult {
        let out: Rc<RefCell<Option<ApiResult>>> = Rc::new(RefCell::new(None));
        let sink = out.clone();
        d.dispatch(
            ApiCall { extension_id: ID.into(), function: function.into(), args, caller },
            Box::new(move |r| *sink.borrow_mut() = Some(r)),
        );
        let result = out.borrow_mut().take();
        result.expect("reply was not called synchronously")
    }

    pub type Slot = Rc<RefCell<Option<ApiResult>>>;

    pub fn register(d: &Dispatcher, id: u64, kind: ContextKind, tab: Option<i64>, frame: Option<i64>) {
        d.context_created(crate::bus::ContextInfo {
            id,
            extension_id: ID.into(),
            kind,
            tab_id: tab,
            frame_id: frame,
            url: Some(format!("https://example.test/{id}")),
        })
        .unwrap();
    }

    /// Starts a call from a registered context; the reply may arrive later.
    pub fn start(d: &Dispatcher, ctx: u64, kind: ContextKind, function: &str, args: Vec<Value>) -> Slot {
        let out: Slot = Rc::new(RefCell::new(None));
        let sink = out.clone();
        let caller = Caller { context: kind, context_id: Some(ctx), tab_id: None, frame_id: None, url: None };
        d.dispatch(
            ApiCall { extension_id: ID.into(), function: function.into(), args, caller },
            Box::new(move |r| *sink.borrow_mut() = Some(r)),
        );
        out
    }

    pub fn call_ctx(d: &Dispatcher, ctx: u64, kind: ContextKind, function: &str, args: Vec<Value>) -> ApiResult {
        let slot = start(d, ctx, kind, function, args);
        let result = slot.borrow_mut().take();
        result.expect("reply was not called synchronously")
    }

    /// The envelopes delivered to a context so far, in order.
    pub fn delivered_to(host: &MockHost, ctx: u64) -> Vec<Value> {
        host.deliveries.borrow().iter().filter(|(c, _)| *c == ctx).map(|(_, v)| v.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    const M3: &str = r#"{"manifest_version":3,"name":"x","version":"1","default_locale":"en",
        "permissions":["storage","alarms","tabs"],"optional_permissions":["history"],"optional_host_permissions":["https://*.b.com/*"]}"#;

    fn kind(r: ApiResult) -> ErrorKind {
        r.expect_err("expected an error").kind
    }

    #[test]
    fn routing_errors() {
        let (d, _) = setup(M3);
        assert_eq!(kind(call(&d, "nope.nothing", vec![])), ErrorKind::UnknownApi);
        // Unsupported APIs report that before anything else, which is more useful than a permission error.
        assert_eq!(kind(call(&d, "proxy.onRequest", vec![])), ErrorKind::NotAvailable);
        assert_eq!(kind(call(&d, "gcm.register", vec![])), ErrorKind::NotAvailable);
        let (no_perm, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#);
        assert_eq!(kind(call(&no_perm, "alarms.create", vec![json!({"delayInMinutes": 1})])), ErrorKind::PermissionDenied);
        assert_eq!(kind(call(&no_perm, "storage.local.get", vec![])), ErrorKind::PermissionDenied);
        // Planned APIs fail clearly rather than silently succeeding.
        let (d2, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["gcm"]}"#);
        assert_eq!(kind(call(&d2, "gcm.register", vec![])), ErrorKind::NotAvailable);
        // MV2-only API in an MV3 extension.
        assert_eq!(kind(call(&d, "tabs.executeScript", vec![])), ErrorKind::NotAvailable);
        // Events are not callable.
        assert_eq!(kind(call(&d, "alarms.onAlarm", vec![])), ErrorKind::InvalidArguments);
        // Unknown extension.
        let r = {
            let out = Rc::new(RefCell::new(None));
            let s = out.clone();
            d.dispatch(
                ApiCall {
                    extension_id: "other".into(),
                    function: "runtime.getManifest".into(),
                    args: vec![],
                    caller: Caller::background(),
                },
                Box::new(move |r| *s.borrow_mut() = Some(r)),
            );
            out.borrow_mut().take().unwrap()
        };
        assert_eq!(kind(r), ErrorKind::Runtime);
    }

    #[test]
    fn runtime_basics() {
        let (d, _) = setup(M3);
        assert_eq!(call(&d, "runtime.getManifest", vec![]).unwrap()["name"], "x");
        assert_eq!(call(&d, "runtime.getURL", vec![json!("/a/b.html")]).unwrap(), format!("chrome-extension://{ID}/a/b.html"));
        assert_eq!(kind(call(&d, "runtime.getURL", vec![json!(5)])), ErrorKind::InvalidArguments);
    }

    #[test]
    fn storage_roundtrip_persistence_and_events() {
        let (d, host) = setup(M3);
        call(&d, "storage.local.set", vec![json!({"a": 1, "b": [1, 2]})]).unwrap();
        assert_eq!(call(&d, "storage.local.get", vec![json!(["a"])]).unwrap(), json!({"a": 1}));
        assert_eq!(call(&d, "storage.local.get", vec![]).unwrap(), json!({"a": 1, "b": [1, 2]}));
        assert_eq!(call(&d, "storage.local.getKeys", vec![]).unwrap(), json!(["a", "b"]));
        assert!(call(&d, "storage.local.getBytesInUse", vec![]).unwrap().as_u64().unwrap() > 0);

        let events = host.events.borrow();
        let (_, name, args) = &events[0];
        assert_eq!(name, "storage.onChanged");
        assert_eq!(args[0], json!({"a": {"newValue": 1}, "b": {"newValue": [1, 2]}}));
        assert_eq!(args[1], "local");
        drop(events);

        // Persisted through the host and restored on reinstall.
        assert!(host.storage.borrow().contains_key(&(ID.to_string(), "local".to_string())));
        let d2 = Dispatcher::new(host.clone(), Registry::builtin());
        d2.install(
            ID,
            crate::manifest::parse_manifest(M3).manifest.unwrap(),
            InstallOptions { base_url: String::new(), locales: vec![], files: Files::new() },
        );
        assert_eq!(call(&d2, "storage.local.get", vec![json!("a")]).unwrap(), json!({"a": 1}));

        // Removing and clearing report changes; no-op set emits nothing.
        let before = host.events.borrow().len();
        call(&d, "storage.local.set", vec![json!({"a": 1})]).unwrap();
        assert_eq!(host.events.borrow().len(), before);
        call(&d, "storage.local.remove", vec![json!("a")]).unwrap();
        call(&d, "storage.local.clear", vec![]).unwrap();
        // Each change fires storage.onChanged (changes, area) and the area's own onChanged (changes).
        let events: Vec<(String, usize)> = host.events.borrow()[before..].iter().map(|(_, e, a)| (e.clone(), a.len())).collect();
        let expected = [("storage.onChanged", 2), ("storage.local.onChanged", 1)];
        assert_eq!(events, [expected, expected].concat().iter().map(|(e, n)| (e.to_string(), *n)).collect::<Vec<_>>());
    }

    #[test]
    fn storage_session_is_not_persisted_and_hidden_from_content_scripts() {
        let (d, host) = setup(M3);
        call(&d, "storage.session.set", vec![json!({"s": 1})]).unwrap();
        assert!(!host.storage.borrow().contains_key(&(ID.to_string(), "session".to_string())));
        let cs = Caller { context: ContextKind::ContentScript, context_id: None, tab_id: Some(1), frame_id: Some(0), url: None };
        assert_eq!(kind(call_from(&d, "storage.session.get", vec![], cs.clone())), ErrorKind::PermissionDenied);
        assert!(call_from(&d, "storage.local.get", vec![], cs).is_ok());
    }

    #[test]
    fn storage_quota_errors_surface_as_runtime_errors() {
        let (d, host) = setup(M3);
        let e = call(&d, "storage.sync.set", vec![json!({"big": "x".repeat(9000)})]).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Runtime);
        assert!(e.message.contains("QUOTA_BYTES_PER_ITEM"));
        assert!(host.events.borrow().is_empty());
        assert_eq!(call(&d, "storage.managed.get", vec![]).unwrap(), json!({}));
    }

    #[test]
    fn unlimited_storage_permission() {
        let (d, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage","unlimitedStorage"]}"#);
        assert!(call(&d, "storage.local.set", vec![json!({"big": "x".repeat(11 * 1024 * 1024)})]).is_ok());
    }

    #[test]
    fn i18n() {
        let host = Rc::new(MockHost::default());
        let d = Dispatcher::new(host, Registry::builtin());
        let m = crate::manifest::parse_manifest(M3).manifest.unwrap();
        d.install(
            ID,
            m,
            InstallOptions {
                base_url: String::new(),
                locales: vec![("en".into(), r#"{"hi":{"message":"Hello $1"}}"#.into())],
                files: Files::new(),
            },
        );
        assert_eq!(call(&d, "i18n.getMessage", vec![json!("hi"), json!("Bob")]).unwrap(), "Hello Bob");
        assert_eq!(call(&d, "i18n.getMessage", vec![json!("hi"), json!(["A"])]).unwrap(), "Hello A");
        assert_eq!(call(&d, "i18n.getMessage", vec![json!("nope")]).unwrap(), "");
        assert_eq!(call(&d, "i18n.getUILanguage", vec![]).unwrap(), "en-US");
        assert_eq!(kind(call(&d, "i18n.getMessage", vec![json!(1)])), ErrorKind::InvalidArguments);
        assert_eq!(
            kind(call(&d, "i18n.getMessage", vec![json!("hi"), json!(["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"])])),
            ErrorKind::InvalidArguments
        );
    }

    #[test]
    fn alarms_end_to_end() {
        let (d, host) = setup(M3);
        *host.now.borrow_mut() = 1_000;
        call(&d, "alarms.create", vec![json!("a"), json!({"delayInMinutes": 1})]).unwrap();
        call(&d, "alarms.create", vec![json!({"periodInMinutes": 1})]).unwrap(); // Unnamed.
        assert_eq!(host.wakeups.borrow().last(), Some(&61_000));
        assert_eq!(call(&d, "alarms.getAll", vec![]).unwrap().as_array().unwrap().len(), 2);
        assert_eq!(call(&d, "alarms.get", vec![json!("a")]).unwrap()["scheduledTime"], 61_000);
        assert_eq!(call(&d, "alarms.get", vec![json!("zz")]).unwrap(), Value::Null);

        *host.now.borrow_mut() = 61_000;
        d.tick();
        let fired: Vec<_> =
            host.events.borrow().iter().filter(|(_, n, _)| n == "alarms.onAlarm").map(|(_, _, a)| a[0]["name"].clone()).collect();
        assert_eq!(fired.len(), 2);
        assert!(fired.contains(&json!("a")) && fired.contains(&json!("")));
        // The one-shot is gone, the periodic one remains and the host was told when to call back.
        assert_eq!(call(&d, "alarms.getAll", vec![]).unwrap().as_array().unwrap().len(), 1);
        assert_eq!(host.wakeups.borrow().last(), Some(&121_000));
        assert_eq!(call(&d, "alarms.clearAll", vec![]).unwrap(), json!(true));
        assert_eq!(kind(call(&d, "alarms.create", vec![json!({})])), ErrorKind::InvalidArguments);
    }

    #[test]
    fn permissions_flow() {
        let (d, host) = setup(M3);
        assert_eq!(call(&d, "permissions.contains", vec![json!({"permissions": ["storage"]})]).unwrap(), json!(true));
        assert_eq!(call(&d, "permissions.contains", vec![json!({"permissions": ["history"]})]).unwrap(), json!(false));
        // Not declared optional: rejected before the user is asked.
        assert_eq!(kind(call(&d, "permissions.request", vec![json!({"permissions": ["cookies"]})])), ErrorKind::Runtime);
        // User declines.
        assert_eq!(call(&d, "permissions.request", vec![json!({"permissions": ["history"]})]).unwrap(), json!(false));
        assert_eq!(call(&d, "permissions.contains", vec![json!({"permissions": ["history"]})]).unwrap(), json!(false));
        // User accepts: granted, event fired, persisted.
        *host.permission_answer.borrow_mut() = true;
        assert_eq!(
            call(&d, "permissions.request", vec![json!({"permissions": ["history"], "origins": ["https://x.b.com/*"]})]).unwrap(),
            json!(true)
        );
        assert!(host.events.borrow().iter().any(|(_, n, a)| n == "permissions.onAdded" && a[0]["permissions"] == json!(["history"])));
        assert!(host.storage.borrow().contains_key(&(ID.to_string(), "permissions".to_string())));
        assert!(call(&d, "permissions.getAll", vec![]).unwrap()["permissions"].as_array().unwrap().contains(&json!("history")));
        // Already held: no prompt.
        *host.permission_answer.borrow_mut() = false;
        assert_eq!(call(&d, "permissions.request", vec![json!({"permissions": ["history"]})]).unwrap(), json!(true));
        // Restored after reinstall.
        let d2 = Dispatcher::new(host.clone(), Registry::builtin());
        d2.install(
            ID,
            crate::manifest::parse_manifest(M3).manifest.unwrap(),
            InstallOptions { base_url: String::new(), locales: vec![], files: Files::new() },
        );
        assert_eq!(call(&d2, "permissions.contains", vec![json!({"permissions": ["history"]})]).unwrap(), json!(true));
        // Removal.
        assert_eq!(call(&d, "permissions.remove", vec![json!({"permissions": ["history"]})]).unwrap(), json!(true));
        assert!(host.events.borrow().iter().any(|(_, n, _)| n == "permissions.onRemoved"));
        assert_eq!(kind(call(&d, "permissions.remove", vec![json!({"permissions": ["storage"]})])), ErrorKind::Runtime);
        assert_eq!(kind(call(&d, "permissions.contains", vec![json!("x")])), ErrorKind::InvalidArguments);
    }

    #[test]
    fn permission_prompt_answered_asynchronously() {
        let (d, host) = setup(M3);
        *host.defer_permission.borrow_mut() = true;
        let out: Rc<RefCell<Option<ApiResult>>> = Rc::new(RefCell::new(None));
        let s = out.clone();
        d.dispatch(
            ApiCall {
                extension_id: ID.into(),
                function: "permissions.request".into(),
                args: vec![json!({"permissions": ["history"]})],
                caller: Caller::background(),
            },
            Box::new(move |r| *s.borrow_mut() = Some(r)),
        );
        assert!(out.borrow().is_none(), "must wait for the user");
        // Other calls keep working while the prompt is open (no borrow is held across the host call).
        assert!(call(&d, "storage.local.get", vec![]).is_ok());
        let answer = host.pending_permission.borrow_mut().take().unwrap();
        answer(true);
        assert_eq!(out.borrow_mut().take().unwrap().unwrap(), json!(true));
    }

    #[test]
    fn delegated_calls_are_routed_with_canonical_names() {
        let (d, host) =
            setup(r#"{"manifest_version":2,"name":"x","version":"1","permissions":["cookies","contextMenus"],"browser_action":{}}"#);
        assert_eq!(call(&d, "cookies.getAll", vec![json!({})]).unwrap(), json!([]));
        assert_eq!(host.browser_calls.borrow().as_slice(), ["host.cookies.getAll"]);
        // Aliases reach the same state as the canonical names, which the core keeps itself.
        call(&d, "browserAction.setTitle", vec![json!({"title": "T"})]).unwrap();
        assert_eq!(call(&d, "action.getTitle", vec![json!({})]).unwrap(), json!("T"));
        assert_eq!(call(&d, "menus.create", vec![json!({"id": "m", "title": "M"})]).unwrap(), json!("m"));
        assert!(call(&d, "contextMenus.remove", vec![json!("m")]).is_ok());
        assert_eq!(host.browser_calls.borrow().len(), 1);
        // Permission checks run before delegation.
        let (d2, host2) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#);
        assert_eq!(kind(call(&d2, "cookies.getAll", vec![json!({})])), ErrorKind::PermissionDenied);
        assert_eq!(kind(call(&d2, "scripting.executeScript", vec![json!({})])), ErrorKind::PermissionDenied);
        assert!(host2.browser_calls.borrow().is_empty());
    }

    #[test]
    fn menus_permission_alias() {
        let (d, _) = setup(r#"{"manifest_version":2,"name":"x","version":"1","permissions":["menus"]}"#);
        assert!(call(&d, "contextMenus.create", vec![json!({"title": "x"})]).is_ok());
    }

    #[test]
    fn lifecycle_events() {
        let (d, host) = setup(M3);
        d.notify_installed(ID, "update", Some("0.9"));
        d.notify_startup(ID);
        let events = host.events.borrow();
        assert_eq!(events[0].1, "runtime.onInstalled");
        assert_eq!(events[0].2[0], json!({"reason": "update", "previousVersion": "0.9"}));
        assert_eq!(events[1].1, "runtime.onStartup");
    }

    #[test]
    fn info_describes_how_to_host_the_extension() {
        let host = Rc::new(MockHost::default());
        let d = Dispatcher::new(host, Registry::builtin());
        let m = crate::manifest::parse_manifest(
            r#"{"manifest_version":3,"name":"__MSG_n__","version":"2","default_locale":"en","permissions":["storage"],"host_permissions":["https://a.com/*"],
                "background":{"service_worker":"sw.js","type":"module"},"action":{"default_popup":"p.html","default_title":"__MSG_n__","default_icon":{"16":"i.png"}},
                "content_security_policy":{"extension_pages":"script-src 'self'; object-src 'none'"},"options_ui":{"page":"o.html","open_in_tab":true}}"#,
        )
        .manifest
        .unwrap();
        d.install(
            ID,
            m,
            InstallOptions {
                base_url: "moz-extension://u/".into(),
                locales: vec![("en".into(), r#"{"n":{"message":"Nice"}}"#.into())],
                files: Files::new(),
            },
        );
        let info = d.info_for(ID).unwrap();
        assert_eq!(info["name"], "Nice");
        assert_eq!(
            info["background"],
            json!({"kind": "service_worker", "scripts": [], "page": "", "serviceWorker": "sw.js", "persistent": false, "module": true})
        );
        assert_eq!(info["csp"]["extensionPages"], "script-src 'self'; object-src 'none'");
        assert_eq!(info["action"]["title"], "Nice");
        assert_eq!(info["action"]["icons"], json!([{"size": 16, "path": "i.png"}]));
        assert_eq!(info["options"], json!({"page": "o.html", "openInTab": true}));
        assert_eq!(info["hostPermissions"], json!(["https://a.com/*"]));
        assert_eq!(info["baseUrl"], "moz-extension://u/");
        assert!(d.info_for("other").is_none());
    }

    #[test]
    fn init_data_exposes_only_permitted_surface() {
        let (d, _) = setup(M3);
        let init = d.init_data_for(ID).unwrap();
        assert_eq!(init["id"], ID);
        let surface = &init["surface"];
        assert!(surface.get("storage").is_some() && surface.get("alarms").is_some());
        assert!(surface.get("cookies").is_none(), "cookies permission was not requested");
        assert!(d.init_data_for("other").is_none());
        assert!(d.uninstall(ID) && !d.is_installed(ID));
    }

    // ---- Messaging -------------------------------------------------------------------------------------------------

    const BG: u64 = 1;
    const POPUP: u64 = 2;
    const CS: u64 = 10;
    const CS_SUB: u64 = 11;

    fn messaging_setup() -> (Dispatcher, Rc<MockHost>) {
        let (d, host) = setup(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage","alarms","tabs"]}"#);
        register(&d, BG, ContextKind::Background, None, None);
        register(&d, POPUP, ContextKind::ExtensionPage, None, None);
        register(&d, CS, ContextKind::ContentScript, Some(5), Some(0));
        register(&d, CS_SUB, ContextKind::ContentScript, Some(5), Some(3));
        (d, host)
    }

    #[test]
    fn import_scripts_reads_only_own_files_for_extension_pages() {
        let host = Rc::new(MockHost::default());
        let d = Dispatcher::new(host.clone(), Registry::builtin());
        let m =
            crate::manifest::parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"sw.js"}}"#)
                .manifest
                .unwrap();
        let mut files = Files::new();
        files.insert("lib/sdk.js", b"self.SDK = 1;".to_vec());
        d.install(ID, m, InstallOptions { base_url: format!("chrome-extension://{ID}/"), locales: vec![], files });
        register(&d, BG, ContextKind::Background, None, None);
        register(&d, CS, ContextKind::ContentScript, Some(5), Some(0));
        let read = |ctx, kind, path: &str| call_ctx(&d, ctx, kind, "__runtime.readScript", vec![json!(path)]);
        assert_eq!(read(BG, ContextKind::Background, "/lib/sdk.js").unwrap(), json!("self.SDK = 1;"));
        assert_eq!(read(BG, ContextKind::Background, "lib/sdk.js?v=2").unwrap(), json!("self.SDK = 1;"));
        assert!(read(BG, ContextKind::Background, "missing.js").is_err());
        assert!(read(BG, ContextKind::Background, "../lib/sdk.js").is_err());
        assert!(read(CS, ContextKind::ContentScript, "lib/sdk.js").is_err(), "content scripts cannot importScripts");
    }

    #[test]
    fn firefox_get_manifest_is_localized() {
        let install = |manifest: &str, base_url: &str| {
            let host = Rc::new(MockHost::default());
            let d = Dispatcher::new(host, Registry::builtin());
            let m = crate::manifest::parse_manifest(manifest).manifest.unwrap();
            let locales = vec![("en".to_string(), r#"{"extName":{"message":"Violentmonkey"}}"#.to_string())];
            d.install(ID, m, InstallOptions { base_url: base_url.into(), locales, files: Files::new() });
            d.init_data_for(ID).unwrap()["manifest"]["name"].clone()
        };
        let firefox = install(
            r#"{"manifest_version":2,"name":"__MSG_extName__","version":"1","default_locale":"en","browser_specific_settings":{"gecko":{"id":"vm@x"}}}"#,
            "moz-extension://u/",
        );
        assert_eq!(firefox, "Violentmonkey");
        let chrome =
            install(r#"{"manifest_version":3,"name":"__MSG_extName__","version":"1","default_locale":"en"}"#, "chrome-extension://x/");
        assert_eq!(chrome, "__MSG_extName__");
    }

    #[test]
    fn extension_pages_fetch_permitted_hosts_without_cors() {
        let (d, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1","host_permissions":["https://api.example.com/*"]}"#);
        register(&d, BG, ContextKind::Background, None, None);
        register(&d, CS, ContextKind::ContentScript, Some(5), Some(0));
        let allows = |ctx, kind, url: &str| call_ctx(&d, ctx, kind, "__permissions.allowsFetch", vec![json!(url)]).unwrap();
        assert_eq!(allows(BG, ContextKind::Background, "https://api.example.com/v1/config"), json!(true));
        assert_eq!(allows(BG, ContextKind::Background, "https://other.example.com/"), json!(false));
        // Content scripts fetch as the page they run in.
        assert_eq!(allows(CS, ContextKind::ContentScript, "https://api.example.com/v1/config"), json!(false));
    }

    #[test]
    fn port_from_content_script_to_background_end_to_end() {
        let (d, host) = messaging_setup();
        let opened =
            call_ctx(&d, CS, ContextKind::ContentScript, "runtime.connect", vec![Value::Null, json!({"name": "chat"}), json!("10.1")])
                .unwrap();
        assert_eq!(opened, json!({"portId": "10.1"}));

        // Background and popup are told; the sender description includes the tab.
        let connect = &delivered_to(&host, BG)[0];
        assert_eq!(
            (connect["type"].as_str(), connect["name"].as_str(), connect["portId"].as_str()),
            (Some("connect"), Some("chat"), Some("10.1"))
        );
        assert_eq!(connect["sender"]["id"], ID);
        assert_eq!(connect["sender"]["tab"], json!({"id": 5}));
        assert_eq!(connect["sender"]["frameId"], 0);
        assert_eq!(connect["sender"]["origin"], "https://example.test");
        assert_eq!(delivered_to(&host, POPUP).len(), 1);
        assert!(delivered_to(&host, CS_SUB).is_empty(), "other content scripts never see runtime.connect");

        // The background listens, the popup does not.
        call_ctx(&d, BG, ContextKind::Background, "__bus.accept", vec![json!("10.1"), json!(true)]).unwrap();
        call_ctx(&d, POPUP, ContextKind::ExtensionPage, "__bus.accept", vec![json!("10.1"), json!(false)]).unwrap();

        call_ctx(&d, CS, ContextKind::ContentScript, "__bus.post", vec![json!("10.1"), json!({"hello": 1})]).unwrap();
        assert_eq!(delivered_to(&host, BG).last().unwrap()["message"], json!({"hello": 1}));
        call_ctx(&d, BG, ContextKind::Background, "__bus.post", vec![json!("10.1"), json!("reply")]).unwrap();
        assert_eq!(delivered_to(&host, CS).last().unwrap(), &json!({"type": "port_message", "portId": "10.1", "message": "reply"}));

        call_ctx(&d, BG, ContextKind::Background, "__bus.disconnect", vec![json!("10.1")]).unwrap();
        assert_eq!(delivered_to(&host, CS).last().unwrap()["type"], "port_disconnect");
        assert!(call_ctx(&d, CS, ContextKind::ContentScript, "__bus.post", vec![json!("10.1"), json!(1)]).is_err());
    }

    #[test]
    fn connect_without_a_listener_reports_no_receiver() {
        let (d, host) = messaging_setup();
        call_ctx(&d, CS, ContextKind::ContentScript, "runtime.connect", vec![json!({"name": "x"})]).unwrap();
        let portid = delivered_to(&host, BG)[0]["portId"].as_str().unwrap().to_string();
        call_ctx(&d, BG, ContextKind::Background, "__bus.accept", vec![json!(portid), json!(false)]).unwrap();
        call_ctx(&d, POPUP, ContextKind::ExtensionPage, "__bus.accept", vec![json!(portid), json!(false)]).unwrap();
        let last = delivered_to(&host, CS).pop().unwrap();
        assert_eq!((last["type"].as_str(), last["error"].as_str()), (Some("port_disconnect"), Some(NO_RECEIVER)));
    }

    #[test]
    fn port_ids_are_generated_when_the_caller_does_not_choose() {
        let (d, host) = messaging_setup();
        let a = call_ctx(&d, BG, ContextKind::Background, "tabs.connect", vec![json!(5), json!({"name": "n", "frameId": 3})]).unwrap();
        let id = a["portId"].as_str().unwrap();
        assert!(id.starts_with("1."), "{id}");
        // frameId in connectInfo narrows the target to that frame.
        assert_eq!(delivered_to(&host, CS_SUB).len(), 1);
        assert!(delivered_to(&host, CS).is_empty());
        let b = call_ctx(&d, BG, ContextKind::Background, "tabs.connect", vec![json!(5)]).unwrap();
        assert_ne!(a, b);
        assert_eq!(delivered_to(&host, CS).len(), 1);
    }

    #[test]
    fn runtime_send_message_resolves_with_the_first_answer() {
        let (d, host) = messaging_setup();
        let slot = start(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!({"ask": "time"})]);
        assert!(slot.borrow().is_none(), "waits for the recipients");
        let envelope = delivered_to(&host, BG).pop().unwrap();
        assert_eq!((envelope["type"].as_str(), &envelope["message"]), (Some("message"), &json!({"ask": "time"})));
        assert_eq!(envelope["sender"]["tab"], json!({"id": 5}));
        let request = envelope["requestId"].clone();

        call_ctx(&d, POPUP, ContextKind::ExtensionPage, "__bus.reply", vec![request.clone(), json!(false), json!(false), Value::Null])
            .unwrap();
        assert!(slot.borrow().is_none(), "the popup had no listener; keep waiting for the background");
        call_ctx(&d, BG, ContextKind::Background, "__bus.reply", vec![request, json!(true), json!(true), json!("12:00")]).unwrap();
        assert_eq!(slot.borrow_mut().take().unwrap().unwrap(), json!({"response": "12:00"}));
    }

    #[test]
    fn send_message_argument_forms() {
        let (d, host) = messaging_setup();
        // (extensionId, message): own id is accepted and addresses the same extension.
        let _slot = start(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!(ID), json!("m")]);
        assert_eq!(delivered_to(&host, BG).pop().unwrap()["message"], "m");
        // A single string is the message, not an extension id.
        let _slot = start(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!("only-message")]);
        assert_eq!(delivered_to(&host, BG).pop().unwrap()["message"], "only-message");
        // Another extension's id: messaging across extensions is not supported.
        let r =
            call_ctx(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!("pnmlkjihgfedcbapnmlkjihgfedcbap"), json!("m")]);
        assert_eq!(r.unwrap_err().message, NO_RECEIVER);
        assert_eq!(
            call_ctx(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![]).unwrap_err().kind,
            ErrorKind::InvalidArguments
        );
    }

    #[test]
    fn send_message_with_nobody_to_ask_fails_at_once() {
        let (d, _host) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#);
        register(&d, CS, ContextKind::ContentScript, Some(5), Some(0));
        let r = call_ctx(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!("hi")]);
        assert_eq!(r.unwrap_err().message, NO_RECEIVER);
        // An unregistered caller cannot use messaging at all.
        assert_eq!(call(&d, "runtime.sendMessage", vec![json!("hi")]).unwrap_err().kind, ErrorKind::InvalidArguments);
    }

    #[test]
    fn tabs_send_message_reaches_the_content_scripts_of_that_tab() {
        let (d, host) = messaging_setup();
        let slot = start(&d, BG, ContextKind::Background, "tabs.sendMessage", vec![json!(5), json!("to-tab"), json!({"frameId": 3})]);
        assert!(delivered_to(&host, CS).is_empty());
        let envelope = delivered_to(&host, CS_SUB).pop().unwrap();
        assert_eq!(envelope["message"], "to-tab");
        assert_eq!(envelope["sender"]["id"], ID);
        assert!(envelope["sender"].get("tab").is_none(), "the background is not in a tab");
        call_ctx(
            &d,
            CS_SUB,
            ContextKind::ContentScript,
            "__bus.reply",
            vec![envelope["requestId"].clone(), json!(true), json!(true), json!(42)],
        )
        .unwrap();
        assert_eq!(slot.borrow_mut().take().unwrap().unwrap(), json!({"response": 42}));

        let none = call_ctx(&d, BG, ContextKind::Background, "tabs.sendMessage", vec![json!(99), json!("x")]);
        assert_eq!(none.unwrap_err().message, NO_RECEIVER);
        assert_eq!(
            call_ctx(&d, BG, ContextKind::Background, "tabs.sendMessage", vec![json!("five"), json!("x")]).unwrap_err().kind,
            ErrorKind::InvalidArguments
        );
    }

    #[test]
    fn closing_a_tab_ends_its_ports_and_unblocks_waiting_senders() {
        let (d, host) = messaging_setup();
        call_ctx(&d, BG, ContextKind::Background, "tabs.connect", vec![json!(5), Value::Null, json!("1.1")]).unwrap();
        call_ctx(&d, CS, ContextKind::ContentScript, "__bus.accept", vec![json!("1.1"), json!(true)]).unwrap();
        call_ctx(&d, CS_SUB, ContextKind::ContentScript, "__bus.accept", vec![json!("1.1"), json!(true)]).unwrap();
        let slot = start(&d, BG, ContextKind::Background, "tabs.sendMessage", vec![json!(5), json!("hello")]);

        d.context_destroyed(CS);
        assert!(slot.borrow().is_none(), "the subframe's script is still being waited on");
        d.context_destroyed(CS_SUB);
        assert!(slot.borrow_mut().take().unwrap().is_err(), "the recipients died without ever saying they had a listener");
        let last = delivered_to(&host, BG).pop().unwrap();
        assert_eq!((last["type"].as_str(), last["portId"].as_str()), (Some("port_disconnect"), Some("1.1")));
        // Calls from a destroyed context are refused.
        assert!(call_ctx(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!(1)]).is_err());
    }

    #[test]
    fn content_scripts_only_get_a_small_api() {
        let (d, _host) = messaging_setup();
        let cs = ContextKind::ContentScript;
        assert_eq!(call_ctx(&d, CS, cs, "tabs.query", vec![json!({})]).unwrap_err().kind, ErrorKind::PermissionDenied);
        assert_eq!(
            call_ctx(&d, CS, cs, "alarms.create", vec![json!({"delayInMinutes": 1})]).unwrap_err().kind,
            ErrorKind::PermissionDenied
        );
        assert_eq!(call_ctx(&d, CS, cs, "runtime.openOptionsPage", vec![]).unwrap_err().kind, ErrorKind::PermissionDenied);
        assert!(call_ctx(&d, CS, cs, "runtime.getManifest", vec![]).is_ok());
        assert!(call_ctx(&d, CS, cs, "storage.local.get", vec![]).is_ok());
        // The background context of the same extension keeps full access.
        assert!(call_ctx(&d, BG, ContextKind::Background, "alarms.create", vec![json!({"delayInMinutes": 1})]).is_ok());

        let background = d.init_data_for_context(BG).unwrap();
        let content = d.init_data_for_context(CS).unwrap();
        assert!(background["surface"]["tabs"].is_object() && background["surface"]["alarms"].is_object());
        assert_eq!((content["contextId"].as_u64(), content["contextKind"].as_str()), (Some(CS), Some("content_script")));
        let namespaces: Vec<&str> = content["surface"].as_object().unwrap().keys().map(String::as_str).collect();
        assert!(namespaces.iter().all(|n| ["i18n", "storage", "extension", "runtime"].contains(n)), "{namespaces:?}");
        let runtime_fns: Vec<&str> =
            content["surface"]["runtime"]["functions"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(runtime_fns.contains(&"connect") && runtime_fns.contains(&"sendMessage") && !runtime_fns.contains(&"openOptionsPage"));
        assert!(d.init_data_for_context(12345).is_none());
    }

    #[test]
    fn bus_internal_calls_cannot_be_forged() {
        let (d, host) = messaging_setup();
        // Another extension with its own context.
        let other = "pnmlkjihgfedcbapnmlkjihgfedcbap";
        d.install(
            other,
            crate::manifest::parse_manifest(r#"{"manifest_version":3,"name":"o","version":"1"}"#).manifest.unwrap(),
            InstallOptions { base_url: String::new(), locales: vec![], files: Files::new() },
        );
        d.context_created(crate::bus::ContextInfo {
            id: 50,
            extension_id: other.into(),
            kind: ContextKind::Background,
            tab_id: None,
            frame_id: None,
            url: None,
        })
        .unwrap();

        call_ctx(&d, CS, ContextKind::ContentScript, "runtime.connect", vec![Value::Null, json!({}), json!("10.1")]).unwrap();
        call_ctx(&d, BG, ContextKind::Background, "__bus.accept", vec![json!("10.1"), json!(true)]).unwrap();

        // Context 50 belongs to another extension: it can neither pose as ID's context nor join the port.
        let spoof = Caller { context: ContextKind::Background, context_id: Some(50), tab_id: None, frame_id: None, url: None };
        let out = Rc::new(RefCell::new(None));
        let sink = out.clone();
        d.dispatch(
            ApiCall { extension_id: ID.into(), function: "__bus.post".into(), args: vec![json!("10.1"), json!("evil")], caller: spoof },
            Box::new(move |r| *sink.borrow_mut() = Some(r)),
        );
        assert!(out.borrow_mut().take().unwrap().is_err());
        assert!(delivered_to(&host, CS).iter().all(|m| m["message"] != "evil"));

        // A context of the right extension that is not an end of the port.
        let r = call_ctx(&d, CS_SUB, ContextKind::ContentScript, "__bus.post", vec![json!("10.1"), json!("evil")]);
        assert_eq!(r.unwrap_err().kind, ErrorKind::PermissionDenied);
        // Unknown internal call, and no context id.
        assert_eq!(call_ctx(&d, BG, ContextKind::Background, "__bus.nope", vec![]).unwrap_err().kind, ErrorKind::UnknownApi);
        assert_eq!(call(&d, "__bus.post", vec![]).unwrap_err().kind, ErrorKind::InvalidArguments);
        // Claiming a port id that belongs to another context.
        let r = call_ctx(&d, CS_SUB, ContextKind::ContentScript, "runtime.connect", vec![Value::Null, json!({}), json!("10.9")]);
        assert_eq!(r.unwrap_err().kind, ErrorKind::InvalidArguments);
    }

    #[test]
    fn contexts_must_belong_to_an_installed_extension_and_uninstall_cleans_up() {
        let (d, host) = messaging_setup();
        let ghost = d.context_created(crate::bus::ContextInfo {
            id: 77,
            extension_id: "nope".into(),
            kind: ContextKind::Background,
            tab_id: None,
            frame_id: None,
            url: None,
        });
        assert!(ghost.is_err());
        let slot = start(&d, CS, ContextKind::ContentScript, "runtime.sendMessage", vec![json!("pending")]);
        call_ctx(&d, CS, ContextKind::ContentScript, "runtime.connect", vec![json!({}), json!("10.5")]).unwrap();
        assert!(slot.borrow().is_none());
        assert!(d.uninstall(ID));
        assert!(slot.borrow_mut().take().unwrap().is_err(), "a call waiting on a removed extension must not hang");
        assert!(d.init_data_for_context(CS).is_none());
        drop(host);
    }

    // The registry is the source of truth for coverage numbers. These tests make it impossible for it to claim more
    // (or less) than the code does.
    #[test]
    fn every_implemented_function_has_a_handler() {
        let reg = Registry::builtin();
        let mut missing = vec![];
        for m in reg.members().iter().filter(|m| m.kind == Kind::Function && m.status.handled_by_core()) {
            let manifest = format!(
                r#"{{"manifest_version":{},"name":"x","version":"1","permissions":["{}"]}}"#,
                m.manifest_versions.as_ref().map_or(3, |v| v[0]),
                m.permission.clone().unwrap_or_else(|| "storage".into())
            );
            let (d, _) = setup(&manifest);
            if let Err(e) = call(&d, &m.full_name, vec![])
                && e.kind == ErrorKind::Unhandled
            {
                missing.push(m.full_name.clone());
            }
        }
        assert!(missing.is_empty(), "registry says implemented but there is no handler: {missing:?}");
    }

    #[test]
    fn every_delegated_function_reaches_the_host() {
        let reg = Registry::builtin();
        let mut not_routed = vec![];
        for m in reg.members().iter().filter(|m| m.kind == Kind::Function && m.status == Status::Delegated) {
            let manifest = format!(
                r#"{{"manifest_version":{},"name":"x","version":"1","permissions":["{}"]}}"#,
                m.manifest_versions.as_ref().map_or(3, |v| v[0]),
                m.permission.clone().unwrap_or_else(|| "storage".into())
            );
            let (d, host) = setup(&manifest);
            let _ = call(&d, &m.full_name, vec![]);
            if host.browser_calls.borrow().as_slice() != [m.full_name.clone()] {
                not_routed.push(m.full_name.clone());
            }
        }
        assert!(not_routed.is_empty(), "delegated but not routed to the host: {not_routed:?}");
    }

    #[test]
    fn planned_functions_never_succeed() {
        let reg = Registry::builtin();
        for m in reg.members().iter().filter(|m| m.kind == Kind::Function && matches!(m.status, Status::Planned | Status::Unsupported)) {
            let manifest = format!(
                r#"{{"manifest_version":{},"name":"x","version":"1","permissions":["{}"]}}"#,
                m.manifest_versions.as_ref().map_or(3, |v| v[0]),
                m.permission.clone().unwrap_or_else(|| "storage".into())
            );
            let (d, host) = setup(&manifest);
            let r = call(&d, &m.full_name, vec![]);
            assert!(r.is_err(), "{} is planned but call succeeded", m.full_name);
            assert!(host.browser_calls.borrow().is_empty(), "{} is planned but was routed to the host", m.full_name);
        }
    }

    #[test]
    fn emitted_events_match_the_registry() {
        let reg = Registry::builtin();
        let mut claimed: Vec<String> =
            reg.members().iter().filter(|m| m.kind == Kind::Event && m.status.handled_by_core()).map(|m| m.full_name.clone()).collect();
        claimed.sort();
        let mut actual: Vec<String> = EMITTED_EVENTS.iter().map(|s| s.to_string()).collect();
        actual.sort();
        assert_eq!(claimed, actual);
    }
}
