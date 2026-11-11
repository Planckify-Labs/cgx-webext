//! The APIs that act on the browser: `tabs`, `windows`, `action` / `browserAction` / `pageAction`, `contextMenus` / `menus`,
//! `commands`, `scripting` (and Firefox's `tabs.executeScript`), and the parts of `runtime`, `management` and `i18n` that
//! need the browser.
//!
//! The semantics live here. The host contributes a snapshot of its windows and tabs ([`Dispatcher::host_event`] with
//! `browserState`) and a handful of primitive commands, called through [`crate::host::ExtensionHost::call_browser`] with
//! names starting with `host.`:
//!
//! | primitive | arguments (one object) | result |
//! |---|---|---|
//! | `host.tabs.create` | `windowId?, index?, url?, active, pinned, openerTabId?` | `{ tabId }` |
//! | `host.tabs.update` | `tabId, url?, active?, pinned?, muted?, highlighted?` | `null` |
//! | `host.tabs.remove` | `tabIds` | `null` |
//! | `host.tabs.reload` | `tabId, bypassCache` | `null` |
//! | `host.tabs.navigateHistory` | `tabId, delta` | `null` |
//! | `host.tabs.duplicate` | `tabId` | `{ tabId }` |
//! | `host.tabs.move` | `tabIds, windowId, index` (-1 = end) | `null` |
//! | `host.tabs.discard` | `tabId` | `null` |
//! | `host.tabs.setZoom` | `tabId, zoomFactor` | `null` |
//! | `host.tabs.capture` | `tabId, format, quality` | `{ dataUrl }` |
//! | `host.tabs.frames` | `tabId` | `[{ frameId, parentFrameId, url, documentId? }]` |
//! | `host.scripting.inject` | see [`Dispatcher::inject`] | `[{ frameId, documentId?, result?, error? }]` |
//! | `host.windows.create` | `urls, tabId?, left?, top?, width?, height?, focused, incognito, type, state?` | `{ windowId }` |
//! | `host.windows.update` | `windowId, left?, top?, width?, height?, focused?, state?, drawAttention?` | `null` |
//! | `host.windows.remove` | `windowId` | `null` |
//! | `host.action.openPopup` | `windowId?, popupUrl` | `null` |
//! | `host.runtime.reload` | (none) | `null` |
//!
//! The host must report the new snapshot before it answers a primitive that changed it, so that the result (a `tabs.Tab`,
//! a `windows.Window`) reflects the change. Primitives carry the calling extension's id in `ApiCall::extension_id`; the core
//! has already checked permissions and arguments.

use crate::browser::{self, BrowserState, Change, Visibility, WINDOW_ID_CURRENT, WINDOW_ID_NONE, diff, event_for, tab_json, window_json};
use crate::dispatch::{Dispatcher, Extension};
use crate::host::*;
use crate::manifest::{ActionKind, BackgroundKind, ContentScript, RunAt, ScriptWorld};
use crate::registry::Member;
use crate::ui::{self, ActionProps, Icon, MenuTarget};
use serde_json::{Map, Value, json};

/// Namespaces (and single members) whose functions this module implements.
pub fn handles(full_name: &str) -> bool {
    let ns = full_name.split('.').next().unwrap_or("");
    matches!(
        ns,
        "tabs"
            | "windows"
            | "action"
            | "browserAction"
            | "pageAction"
            | "contextMenus"
            | "menus"
            | "commands"
            | "scripting"
            | "contentScripts"
    ) || matches!(
        full_name,
        "runtime.openOptionsPage"
            | "runtime.reload"
            | "runtime.getPlatformInfo"
            | "runtime.getBrowserInfo"
            | "runtime.setUninstallURL"
            | "runtime.getContexts"
            | "runtime.getVersion"
            | "management.getSelf"
            | "management.getAll"
            | "management.get"
            | "i18n.getAcceptLanguages"
            | "extension.isAllowedIncognitoAccess"
            | "extension.isAllowedFileSchemeAccess"
            | "webRequest.handlerBehaviorChanged"
    )
}

/// The browser Chrome cannot script: other extensions, the stores, and the browser's own pages.
const CANNOT_ACCESS: &str = "Cannot access contents of the page. Extension manifest must request permission to access the respective host.";

fn no_tab(id: i64) -> ApiError {
    ApiError::runtime(format!("No tab with id: {id}."))
}

fn no_window(id: i64) -> ApiError {
    ApiError::runtime(format!("No window with id: {id}."))
}

/// Whether the extension's background stops and starts again, so that its context-menu items must outlive it.
pub fn keeps_menus(m: &crate::manifest::Manifest) -> bool {
    match m.background.kind {
        BackgroundKind::ServiceWorker => true,
        BackgroundKind::Page | BackgroundKind::Scripts => !m.background.persistent,
        BackgroundKind::None => false,
    }
}

pub fn is_firefox(e: &Extension) -> bool {
    e.base_url.starts_with("moz-extension://")
}

/// A content script registered with `scripting.registerContentScripts`.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredScript {
    pub id: String,
    pub script: ContentScript,
    pub persist: bool,
}

impl RegisteredScript {
    fn parse(v: &Value, base_url: &str, existing: Option<&RegisteredScript>) -> Result<RegisteredScript, String> {
        let id = v.get("id").and_then(Value::as_str).ok_or("A content script needs an id")?.to_string();
        if id.is_empty() || id.starts_with('_') {
            return Err(format!("Content script's ID '{id}' must not be empty or start with '_'"));
        }
        let mut s = existing.map(|e| e.script.clone()).unwrap_or_default();
        let list = |key: &str| -> Result<Option<Vec<String>>, String> {
            match v.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Array(a)) => a
                    .iter()
                    .map(|i| i.as_str().map(String::from).ok_or(format!("'{key}' must contain strings")))
                    .collect::<Result<_, _>>()
                    .map(Some),
                Some(_) => Err(format!("'{key}' must be an array")),
            }
        };
        if let Some(m) = list("matches")? {
            for p in &m {
                crate::match_pattern::MatchPattern::parse(p, crate::match_pattern::MatchDialect::Chrome)
                    .map_err(|e| format!("Script with ID '{id}' has invalid match pattern '{p}': {e}"))?;
            }
            s.matches = m;
        }
        if let Some(m) = list("excludeMatches")? {
            s.exclude_matches = m;
        }
        if let Some(js) = list("js")? {
            s.js = js.iter().map(|p| ui::normalize_extension_path(p, base_url)).collect();
        }
        if let Some(css) = list("css")? {
            s.css = css.iter().map(|p| ui::normalize_extension_path(p, base_url)).collect();
        }
        if let Some(b) = v.get("allFrames").and_then(Value::as_bool) {
            s.all_frames = b;
        }
        if let Some(b) = v.get("matchOriginAsFallback").and_then(Value::as_bool) {
            s.match_origin_as_fallback = b;
        }
        if let Some(r) = v.get("runAt").and_then(Value::as_str) {
            s.run_at = match r {
                "document_start" => RunAt::DocumentStart,
                "document_end" => RunAt::DocumentEnd,
                "document_idle" => RunAt::DocumentIdle,
                other => return Err(format!("'{other}' is not a valid runAt")),
            };
        }
        if let Some(w) = v.get("world").and_then(Value::as_str) {
            s.world = if w == "MAIN" { ScriptWorld::Main } else { ScriptWorld::Isolated };
        }
        if s.matches.is_empty() {
            return Err(format!("Script with ID '{id}' must specify 'matches'."));
        }
        if s.js.is_empty() && s.css.is_empty() {
            return Err(format!("Script with ID '{id}' must specify at least one js or css file."));
        }
        let persist = v.get("persistAcrossSessions").and_then(Value::as_bool).unwrap_or(existing.is_none_or(|e| e.persist));
        Ok(RegisteredScript { id, script: s, persist })
    }

    pub fn to_json(&self) -> Value {
        let s = &self.script;
        json!({
            "id": self.id,
            "matches": s.matches,
            "excludeMatches": s.exclude_matches,
            "js": s.js,
            "css": s.css,
            "allFrames": s.all_frames,
            "matchOriginAsFallback": s.match_origin_as_fallback,
            "runAt": match s.run_at { RunAt::DocumentStart => "document_start", RunAt::DocumentEnd => "document_end", RunAt::DocumentIdle => "document_idle" },
            "world": match s.world { ScriptWorld::Main => "MAIN", ScriptWorld::Isolated => "ISOLATED" },
            "persistAcrossSessions": self.persist,
        })
    }

    pub fn list_from_json(v: &Value) -> Vec<RegisteredScript> {
        v.as_array().into_iter().flatten().filter_map(|s| RegisteredScript::parse(s, "", None).ok()).collect()
    }
}

/// Where a script or stylesheet goes: `scripting`'s `InjectionTarget`, or Firefox's `tabs.executeScript` details.
struct Injection {
    tab_id: i64,
    frame_ids: Option<Vec<i64>>,
    all_frames: bool,
    /// For results: Chrome's `InjectionResult` objects, or Firefox's plain array of results.
    firefox_results: bool,
}

/// `chrome.runtime.PlatformInfo` for this build.
fn platform_info() -> Value {
    let os = if cfg!(target_os = "macos") {
        "mac"
    } else if cfg!(target_os = "windows") {
        "win"
    } else if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "openbsd") {
        "openbsd"
    } else if cfg!(target_os = "freebsd") {
        "fuchsia" // Chrome has no FreeBSD value; Chromium's FreeBSD ports report it like this. Unlikely to matter.
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "x86_64") {
        "x86-64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else if cfg!(target_arch = "x86") {
        "x86-32"
    } else if cfg!(target_arch = "arm") {
        "arm"
    } else if cfg!(target_arch = "mips64") {
        "mips64"
    } else {
        "x86-64"
    };
    json!({ "os": os, "arch": arch, "nacl_arch": arch })
}

impl Dispatcher {
    // ---- Plumbing ---------------------------------------------------------------------------------------------------

    /// Runs `f` with what the extension may see of tabs.
    pub(crate) fn with_visibility<R>(&self, id: &str, f: impl FnOnce(&Visibility) -> R) -> Option<R> {
        let exts = self.extensions.borrow();
        let e = exts.get(id)?;
        let perms = &e.permissions;
        let access = |tab: i64, url: &str| perms.has_host_for_tab(tab, url);
        let vis = Visibility { all_tabs: perms.has_api("tabs"), can_access: &access, firefox: is_firefox(e) };
        Some(f(&vis))
    }

    /// The URL of the page that made the call, for resolving relative paths.
    fn caller_url(&self, call: &ApiCall) -> Option<String> {
        call.caller.url.clone().or_else(|| call.caller.context_id.and_then(|id| self.bus.borrow().context(id).and_then(|c| c.url.clone())))
    }

    fn tab_for(&self, call: &ApiCall, tab_id: i64) -> ApiResult {
        let state = self.browser.borrow();
        let tab = state.tab(tab_id).ok_or_else(|| no_tab(tab_id))?;
        Ok(self.with_visibility(&call.extension_id, |v| tab_json(tab, v)).unwrap_or(Value::Null))
    }

    fn window_for(&self, call: &ApiCall, window_id: i64, populate: bool) -> ApiResult {
        let state = self.browser.borrow();
        let w = state.window(window_id).ok_or_else(|| no_window(window_id))?;
        Ok(self.with_visibility(&call.extension_id, |v| window_json(w, populate, v)).unwrap_or(Value::Null))
    }

    fn current_window(&self, call: &ApiCall) -> Option<i64> {
        self.browser.borrow().current_window(call.caller.tab_id)
    }

    /// A tab id argument, or the active tab of the caller's current window when it is absent.
    fn tab_or_active(&self, call: &ApiCall, v: &Value) -> Result<i64, ApiError> {
        if let Some(id) = v.as_i64() {
            if self.browser.borrow().tab(id).is_none() {
                return Err(no_tab(id));
            }
            return Ok(id);
        }
        if !v.is_null() {
            return Err(ApiError::invalid("tabId must be an integer"));
        }
        let state = self.browser.borrow();
        let window = state.current_window(call.caller.tab_id).ok_or_else(|| ApiError::runtime("No current window"))?;
        state.active_tab_of(window).map(|t| t.id).ok_or_else(|| ApiError::runtime("No active tab"))
    }

    fn resolve_window(&self, call: &ApiCall, id: Option<i64>) -> Result<Option<i64>, ApiError> {
        match id {
            None => Ok(None),
            Some(WINDOW_ID_CURRENT) => Ok(self.current_window(call)),
            Some(id) if self.browser.borrow().window(id).is_some() => Ok(Some(id)),
            Some(id) => Err(no_window(id)),
        }
    }

    /// A URL for a new or updated tab: relative URLs are resolved against the extension's root, as in Chrome.
    fn resolve_url(&self, call: &ApiCall, url: &Value) -> Result<Option<String>, ApiError> {
        let Some(url) = url.as_str() else {
            return if url.is_null() { Ok(None) } else { Err(ApiError::invalid("url must be a string")) };
        };
        let has_scheme = url
            .split_once(':')
            .is_some_and(|(s, _)| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')));
        if has_scheme {
            let scheme = url.split(':').next().unwrap_or("").to_ascii_lowercase();
            if scheme == "javascript" {
                return Err(ApiError::runtime("Cannot navigate to a javascript: URL"));
            }
            if matches!(scheme.as_str(), "chrome-extension" | "moz-extension") {
                // Only the extension's own pages, or other extensions' web-accessible ones.
                let mine = self.with_ext(call, |e| Ok(url.to_ascii_lowercase().starts_with(&e.base_url.to_ascii_lowercase())))?;
                if !mine && self.serve(url, Some(&self.with_ext(call, |e| Ok(e.base_url.clone()))?)).is_none_or(|s| s.status != 200) {
                    return Err(ApiError::runtime(format!("Cannot navigate to {url}")));
                }
            }
            return Ok(Some(url.to_string()));
        }
        // Relative URLs are relative to the calling page, as in Chrome and Firefox.
        let caller = self.caller_url(call);
        self.with_ext(call, |e| Ok(Some(format!("{}{}", e.base_url, ui::resolve_extension_path(url, &e.base_url, caller.as_deref())))))
    }

    /// Calls a host primitive and shapes its answer with `then`.
    fn host_call(
        &self,
        call: &ApiCall,
        function: &str,
        args: Value,
        reply: Reply,
        then: impl FnOnce(&Dispatcher, Value) -> ApiResult + 'static,
    ) {
        let primitive =
            ApiCall { extension_id: call.extension_id.clone(), function: function.into(), args: vec![args], caller: call.caller.clone() };
        let this = self.clone();
        self.host.call_browser(
            &primitive,
            Box::new(move |result| match result {
                Ok(v) => reply(then(&this, v)),
                Err(e) => reply(Err(ApiError::runtime(e.message))),
            }),
        );
    }

    fn ui_changed(&self, id: &str, what: &str) {
        self.host.dispatch_event(id, "__ui.changed", vec![json!(what)]);
    }

    // ---- From the host ----------------------------------------------------------------------------------------------

    /// Everything the browser tells the core about itself, or asks it, that is not an extension's API call. `args` is a JSON
    /// object; the result is JSON (`null` when there is nothing to say).
    ///
    /// | name | args | result |
    /// |---|---|---|
    /// | `browserState` | a [`BrowserState`] snapshot | `null`; fires `tabs.*`/`windows.*` events |
    /// | `uiState` | `tabId?` | toolbar state of every extension, see [`Dispatcher::ui_state`] |
    /// | `actionClicked` | `extensionId, tabId, button?, modifiers?` | `{ popup: url }` or `{ clicked: true }` |
    /// | `setActionPinned` | `extensionId, pinned` | `null` |
    /// | `contextMenuItems` | `tabId?` and a [`MenuTarget`] | `[{ extensionId, name, items }]` |
    /// | `contextMenuClicked` | `extensionId, itemId, tabId?` and the [`MenuTarget`] | `null` |
    /// | `commands` | - | `[{ extensionId, name, description, shortcut }]` |
    /// | `command` | `extensionId, name, tabId?` | like `actionClicked`, or `null` |
    /// | `setShortcut` | `extensionId, name, shortcut` | `{ ok: true }` or `{ error }` |
    /// | `grantActiveTab` | `extensionId, tabId` | `null` |
    pub fn host_event(&self, name: &str, args: &Value) -> Value {
        let id = args.get("extensionId").and_then(Value::as_str).unwrap_or("");
        let tab = args.get("tabId").and_then(Value::as_i64);
        match name {
            "browserState" => match self.set_browser_state(args) {
                Ok(()) => Value::Null,
                Err(e) => json!({ "error": e }),
            },
            "uiState" => self.ui_state(tab),
            "actionClicked" => self.action_clicked(id, tab, args),
            "setActionPinned" => {
                let pinned = args.get("pinned").and_then(Value::as_bool).unwrap_or(true);
                if self.extensions.borrow_mut().get_mut(id).map(|e| e.action.pinned = pinned).is_some() {
                    self.persist_ui(id);
                    self.host.dispatch_event(id, "action.onUserSettingsChanged", vec![json!({ "isOnToolbar": pinned })]);
                    self.ui_changed(id, "action");
                }
                Value::Null
            }
            "contextMenuItems" => self.context_menu_items(tab, &MenuTarget::from_json(args)),
            "contextMenuClicked" => {
                self.context_menu_clicked(id, args.get("itemId").and_then(Value::as_str).unwrap_or(""), tab, &MenuTarget::from_json(args));
                Value::Null
            }
            "commands" => self.all_commands(),
            "command" => self.command(id, args.get("name").and_then(Value::as_str).unwrap_or(""), tab),
            "setShortcut" => match self.set_shortcut(
                id,
                args.get("name").and_then(Value::as_str).unwrap_or(""),
                args.get("shortcut").and_then(Value::as_str).unwrap_or(""),
            ) {
                Ok(()) => json!({ "ok": true }),
                Err(e) => json!({ "error": e }),
            },
            "grantActiveTab" => {
                if let Some(tab) = tab {
                    self.grant_active_tab(id, tab);
                }
                Value::Null
            }
            other => json!({ "error": format!("unknown host event '{other}'") }),
        }
    }

    /// Takes a new snapshot of the browser and fires the events that describe the change.
    pub fn set_browser_state(&self, v: &Value) -> Result<(), String> {
        let new = BrowserState::from_json(v)?;
        let old = std::mem::replace(&mut *self.browser.borrow_mut(), new.clone());
        let changes = diff(&old, &new);
        if changes.is_empty() {
            return Ok(());
        }
        {
            let mut exts = self.extensions.borrow_mut();
            for c in &changes {
                match c {
                    // activeTab lasts until the tab leaves the origin, and per-tab action state until the tab closes.
                    Change::TabLeftOrigin(t) => exts.values_mut().for_each(|e| e.permissions.revoke_active_tab(*t)),
                    Change::TabRemoved { tab_id, .. } => exts.values_mut().for_each(|e| {
                        e.permissions.revoke_active_tab(*tab_id);
                        e.action.forget_tab(*tab_id);
                    }),
                    _ => {}
                }
            }
        }
        let mut events = vec![];
        for id in self.installed_ids() {
            self.with_visibility(&id, |vis| {
                for c in &changes {
                    if let Some((name, args)) = event_for(c, &new, vis) {
                        events.push((id.clone(), name, args));
                    }
                }
            });
        }
        // No borrow is held: listeners may call back in.
        for (id, name, args) in events {
            self.host.dispatch_event(&id, name, args);
        }
        Ok(())
    }

    fn grant_active_tab(&self, id: &str, tab: i64) {
        let url = self.browser.borrow().tab(tab).map(|t| t.url.clone());
        if let (Some(url), Some(e)) = (url, self.extensions.borrow_mut().get_mut(id)) {
            e.permissions.grant_active_tab(tab, &url);
        }
    }

    // ---- Toolbar button ---------------------------------------------------------------------------------------------

    fn action_kind(e: &Extension) -> &'static str {
        match e.manifest.action.as_ref().map(|a| a.kind) {
            Some(ActionKind::Action) => "action",
            Some(ActionKind::BrowserAction) => "browser_action",
            Some(ActionKind::PageAction) => "page_action",
            None => "none",
        }
    }

    fn popup_url(e: &Extension, tab: Option<i64>) -> Option<String> {
        let popup = e
            .action
            .get(tab, |p| p.popup.clone())
            .or_else(|| e.manifest.action.as_ref().map(|a| a.default_popup.clone()))
            .unwrap_or_default();
        let popup = ui::normalize_extension_path(&popup, &e.base_url);
        (!popup.is_empty()).then(|| format!("{}{popup}", e.base_url))
    }

    fn action_enabled(e: &Extension, tab: Option<i64>) -> bool {
        let default = !matches!(e.manifest.action.as_ref().map(|a| a.kind), Some(ActionKind::PageAction));
        e.action.get(tab, |p| p.enabled).unwrap_or(default)
    }

    fn action_title(e: &Extension, tab: Option<i64>) -> String {
        e.action.get(tab, |p| p.title.clone()).unwrap_or_else(|| {
            let t = e.manifest.action.as_ref().map(|a| e.i18n.localize(&a.default_title)).unwrap_or_default();
            if t.is_empty() { e.i18n.localize(&e.manifest.name) } else { t }
        })
    }

    fn action_icon(e: &Extension, tab: Option<i64>) -> Option<Icon> {
        e.action.get(tab, |p| p.icon.clone()).or_else(|| {
            let set = e
                .manifest
                .action
                .as_ref()
                .map(|a| a.default_icon.0.clone())
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| e.manifest.icons.0.clone());
            (!set.is_empty()).then(|| Icon::Paths(set.iter().map(|(s, p)| (*s, ui::normalize_extension_path(p, &e.base_url))).collect()))
        })
    }

    /// What the browser needs to draw every extension's toolbar button and menu entry, for `tab` (the active tab of the
    /// window being drawn): `[{ id, name, version, icons, optionsUrl, siteAccess, action: { kind, title, badgeText,
    /// badgeBackgroundColor, badgeTextColor, popupUrl, enabled, pinned, icon } }]`.
    pub fn ui_state(&self, tab: Option<i64>) -> Value {
        let url = tab.and_then(|t| self.browser.borrow().tab(t).map(|t| t.url.clone()));
        let exts = self.extensions.borrow();
        let mut ids: Vec<&String> = exts.keys().collect();
        ids.sort();
        Value::Array(
            ids.into_iter()
                .filter_map(|id| exts.get(id))
                .map(|e| {
                    let m = &e.manifest;
                    let site_access = match &url {
                        None => "none",
                        Some(u) if e.permissions.has_host(u) => "granted",
                        Some(u) if tab.is_some_and(|t| e.permissions.has_host_for_tab(t, u)) => "granted",
                        Some(_) if e.permissions.has_api("activeTab") => "on_click",
                        Some(_) => "none",
                    };
                    let options = if m.options_page.is_empty() { Value::Null } else { json!(format!("{}{}", e.base_url, ui::normalize_extension_path(&m.options_page, &e.base_url))) };
                    let a = &e.action;
                    json!({
                        "id": e.id,
                        "name": e.i18n.localize(&m.name),
                        "version": m.version,
                        "baseUrl": e.base_url,
                        "icons": m.icons.0.iter().map(|(s, p)| json!({ "size": s, "path": ui::normalize_extension_path(p, &e.base_url) })).collect::<Vec<_>>(),
                        "optionsUrl": options,
                        "siteAccess": site_access,
                        "action": {
                            "kind": Self::action_kind(e),
                            "title": Self::action_title(e, tab),
                            "badgeText": a.get(tab, |p| p.badge_text.clone()).unwrap_or_default(),
                            "badgeBackgroundColor": a.get(tab, |p| p.badge_background),
                            "badgeTextColor": a.get(tab, |p| p.badge_text_color),
                            "popupUrl": Self::popup_url(e, tab),
                            "enabled": Self::action_enabled(e, tab),
                            "pinned": a.pinned,
                            "icon": Self::action_icon(e, tab).map(|i| i.to_json()),
                        },
                    })
                })
                .collect(),
        )
    }

    /// The user clicked the extension's toolbar button (or its entry in the extensions menu). The browser opens the popup
    /// if there is one; otherwise `action.onClicked` fires. Either way the extension gets `activeTab` for the tab.
    pub fn action_clicked(&self, id: &str, tab: Option<i64>, args: &Value) -> Value {
        let info = self
            .extensions
            .borrow()
            .get(id)
            .map(|e| (Self::popup_url(e, tab), Self::action_enabled(e, tab), Self::action_kind(e), is_firefox(e)));
        let Some((popup, enabled, kind, firefox)) = info else { return Value::Null };
        if !enabled {
            return Value::Null;
        }
        if let Some(t) = tab {
            self.grant_active_tab(id, t);
        }
        if let Some(popup) = popup {
            return json!({ "popup": popup });
        }
        let tab_json = tab.and_then(|t| self.browser.borrow().tab(t).cloned()).and_then(|t| self.with_visibility(id, |v| tab_json(&t, v)));
        let mut listener_args = vec![tab_json.unwrap_or(Value::Null)];
        if firefox {
            let modifiers = args.get("modifiers").cloned().unwrap_or_else(|| json!([]));
            listener_args.push(json!({ "modifiers": modifiers, "button": args.get("button").and_then(Value::as_i64).unwrap_or(0) }));
        }
        let event = if kind == "page_action" { "pageAction.onClicked" } else { "action.onClicked" };
        self.host.dispatch_event(id, event, listener_args);
        json!({ "clicked": true })
    }

    fn persist_ui(&self, id: &str) {
        let data =
            self.extensions.borrow().get(id).map(|e| json!({ "pinned": e.action.pinned, "shortcuts": e.commands.overrides }).to_string());
        if let Some(data) = data {
            self.host.write_storage(id, "ui", &data);
        }
    }

    fn action_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let details = call.arg(0);
        // `enable`, `disable`, `isEnabled`, `show`, `hide` take a bare tab id; the rest take `{ tabId, ... }`.
        let tab = details.as_i64().or_else(|| details.get("tabId").and_then(Value::as_i64));
        if let Some(t) = tab
            && self.browser.borrow().tab(t).is_none()
            && !self.browser.borrow().windows.is_empty()
        {
            return reply(Err(no_tab(t)));
        }
        if op == "openPopup" {
            let window = details.get("windowId").and_then(Value::as_i64);
            let active = self
                .browser
                .borrow()
                .current_window(call.caller.tab_id)
                .and_then(|w| self.browser.borrow().active_tab_of(window.unwrap_or(w)).map(|t| t.id));
            let popup = self.with_ext(call, |e| Ok(Self::popup_url(e, active)));
            return match popup {
                Ok(Some(url)) => {
                    self.host_call(call, "host.action.openPopup", json!({ "windowId": window, "popupUrl": url }), reply, |_, _| {
                        Ok(Value::Null)
                    })
                }
                Ok(None) => reply(Err(ApiError::runtime("Extension does not have a popup on the active tab."))),
                Err(e) => reply(Err(e)),
            };
        }
        let caller = self.caller_url(call);
        let result = self.with_ext_mut(call, |e| {
            let set = |e: &mut Extension, f: &dyn Fn(&mut ActionProps)| f(e.action.props_mut(tab));
            let read = |key: &str| details.get(key).cloned().unwrap_or(Value::Null);
            Ok(match op {
                "setTitle" => {
                    let title = read("title");
                    let t = title.as_str().map(String::from);
                    if !title.is_null() && t.is_none() {
                        return Err(ApiError::invalid("title must be a string"));
                    }
                    set(e, &|p| p.title = t.clone());
                    Some(Value::Null)
                }
                "getTitle" => Some(json!(Self::action_title(e, tab))),
                "setBadgeText" => {
                    let text = read("text");
                    let t = text.as_str().map(String::from);
                    if !text.is_null() && t.is_none() {
                        return Err(ApiError::invalid("text must be a string"));
                    }
                    // Clearing a tab's text shows the global one again; clearing the global one empties it.
                    let t = if tab.is_none() { Some(t.unwrap_or_default()) } else { t };
                    set(e, &|p| p.badge_text = t.clone());
                    Some(Value::Null)
                }
                "getBadgeText" => Some(json!(e.action.get(tab, |p| p.badge_text.clone()).unwrap_or_default())),
                "setBadgeBackgroundColor" | "setBadgeTextColor" => {
                    let color = read("color");
                    let c = if color.is_null() { None } else { Some(ui::parse_color(&color).map_err(ApiError::invalid)?) };
                    if op == "setBadgeBackgroundColor" {
                        set(e, &|p| p.badge_background = c);
                    } else {
                        set(e, &|p| p.badge_text_color = c);
                    }
                    Some(Value::Null)
                }
                "getBadgeBackgroundColor" => Some(json!(e.action.get(tab, |p| p.badge_background).unwrap_or([0, 0, 0, 0]))),
                "getBadgeTextColor" => Some(json!(e.action.get(tab, |p| p.badge_text_color).unwrap_or([0, 0, 0, 0]))),
                "setPopup" => {
                    let popup = read("popup");
                    let p = popup
                        .as_str()
                        .map(|s| if s.is_empty() { String::new() } else { ui::resolve_extension_path(s, &e.base_url, caller.as_deref()) });
                    if popup.is_null() && tab.is_none() {
                        return Err(ApiError::invalid("popup must be a string"));
                    }
                    if let Some(path) = p.as_deref().filter(|p| !p.is_empty())
                        && e.files.get(path).is_none()
                    {
                        return Err(ApiError::runtime(format!("Could not find popup '{path}'")));
                    }
                    set(e, &|q| q.popup = p.clone());
                    Some(Value::Null)
                }
                "getPopup" => Some(json!(Self::popup_url(e, tab).unwrap_or_default())),
                "setIcon" => {
                    let icon = ui::parse_icon(details, &e.base_url, caller.as_deref()).map_err(ApiError::invalid)?;
                    let Some(icon) = icon else { return Err(ApiError::invalid("Either path or imageData property must be specified.")) };
                    if let Icon::Paths(paths) = &icon
                        && let Some((_, missing)) = paths.iter().find(|(_, p)| e.files.get(p).is_none())
                    {
                        return Err(ApiError::runtime(format!("Failed to set icon '{missing}': Failed to fetch")));
                    }
                    set(e, &|p| p.icon = Some(icon.clone()));
                    Some(Value::Null)
                }
                "enable" | "show" => {
                    set(e, &|p| p.enabled = Some(true));
                    Some(Value::Null)
                }
                "disable" | "hide" => {
                    set(e, &|p| p.enabled = Some(false));
                    Some(Value::Null)
                }
                "isEnabled" | "isShown" => Some(json!(Self::action_enabled(e, tab))),
                "getUserSettings" => Some(json!({ "isOnToolbar": e.action.pinned })),
                _ => None,
            })
        });
        match result {
            Ok(Some(v)) => {
                if op.starts_with("set") || matches!(op, "enable" | "disable" | "show" | "hide") {
                    self.ui_changed(&call.extension_id, "action");
                }
                reply(Ok(v))
            }
            Ok(None) => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("action.{op}")))),
            Err(e) => reply(Err(e)),
        }
    }

    // ---- Context menus ----------------------------------------------------------------------------------------------

    fn persist_menus(&self, id: &str) {
        let data = self.extensions.borrow().get(id).filter(|e| keeps_menus(&e.manifest)).map(|e| e.menus.to_json().to_string());
        if let Some(data) = data {
            self.host.write_storage(id, "menus", &data);
        }
    }

    fn context_menus_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        let result = self.with_ext_mut(call, |e| {
            let needs_id = !is_firefox(e) && keeps_menus(&e.manifest);
            match op {
                "create" => {
                    let mut props = call.arg(0).clone();
                    // The shim names items the extension did not name, so that `create` can return the id at once.
                    let generated = props.get("__generatedId").and_then(Value::as_bool).unwrap_or(false);
                    if let Some(o) = props.as_object_mut() {
                        o.remove("__generatedId");
                        o.remove("onclick");
                    }
                    if generated && needs_id {
                        return Err(ApiError::runtime(
                            "Extensions using event pages or Service Workers must pass an id parameter to chrome.contextMenus.create",
                        ));
                    }
                    e.menus.create(&props, needs_id).map_err(ApiError::runtime)
                }
                "update" => e.menus.update(call.arg(0), call.arg(1)).map(|_| Value::Null).map_err(ApiError::runtime),
                "remove" => {
                    if e.menus.remove(call.arg(0)) {
                        Ok(Value::Null)
                    } else {
                        Err(ApiError::runtime(format!(
                            "Cannot find menu item with id {}",
                            call.arg(0).as_str().map_or_else(|| call.arg(0).to_string(), String::from)
                        )))
                    }
                }
                "removeAll" => {
                    e.menus.items.clear();
                    Ok(Value::Null)
                }
                // Firefox rebuilds an open menu; ours is built when it opens, so there is nothing to refresh.
                "refresh" => Ok(Value::Null),
                _ => Err(ApiError::new(ErrorKind::Unhandled, format!("contextMenus.{op}"))),
            }
        });
        if result.is_ok() && op != "refresh" {
            self.persist_menus(&call.extension_id);
        }
        result
    }

    /// The items to add to a context menu the browser is about to show: `[{ extensionId, name, items }]` (see
    /// [`ui::menu_tree`]), one entry per extension with items that apply. The browser groups an extension's items under its
    /// name when there is more than one top-level item, as Chrome does.
    pub fn context_menu_items(&self, _tab: Option<i64>, target: &MenuTarget) -> Value {
        let exts = self.extensions.borrow();
        let mut ids: Vec<&String> = exts.keys().collect();
        ids.sort();
        Value::Array(
            ids.into_iter()
                .filter_map(|id| exts.get(id))
                .filter(|e| crate::dispatch::permission_granted(&e.permissions, "contextMenus"))
                .filter_map(|e| {
                    let items = ui::menu_tree(&e.menus, target, Self::action_kind(e));
                    (!items.is_empty()).then(|| json!({ "extensionId": e.id, "name": e.i18n.localize(&e.manifest.name), "items": items }))
                })
                .collect(),
        )
    }

    pub fn context_menu_clicked(&self, id: &str, item_id_json: &str, tab: Option<i64>, target: &MenuTarget) {
        let Ok(item_id) = serde_json::from_str::<Value>(item_id_json) else { return };
        let data = {
            let mut exts = self.extensions.borrow_mut();
            let Some(e) = exts.get_mut(id) else { return };
            let Some(at) = e.menus.find(&item_id) else { return };
            if !e.menus.items[at].enabled {
                return;
            }
            let checked = e.menus.click(&item_id);
            let item = e.menus.items[at].clone();
            target.click_data(&item, checked)
        };
        self.persist_menus(id);
        if let Some(t) = tab {
            self.grant_active_tab(id, t);
        }
        let tab_json = tab.and_then(|t| self.browser.borrow().tab(t).cloned()).and_then(|t| self.with_visibility(id, |v| tab_json(&t, v)));
        self.host.dispatch_event(id, "contextMenus.onClicked", vec![data, tab_json.unwrap_or(Value::Null)]);
    }

    // ---- Commands ---------------------------------------------------------------------------------------------------

    fn commands_of(e: &Extension) -> Vec<Value> {
        let mut list: Vec<Value> = e
            .manifest
            .commands
            .iter()
            .map(|c| {
                let shortcut = e
                    .commands
                    .overrides
                    .get(&c.name)
                    .cloned()
                    .unwrap_or_else(|| ui::normalize_shortcut(&c.suggested_key).unwrap_or_default());
                let description = e.commands.descriptions.get(&c.name).cloned().unwrap_or_else(|| e.i18n.localize(&c.description));
                json!({ "name": c.name, "description": description, "shortcut": shortcut })
            })
            .collect();
        // Chrome lists the action command even when the manifest does not, with no shortcut.
        let action_command = match e.manifest.action.as_ref().map(|a| a.kind) {
            Some(ActionKind::Action) => Some("_execute_action"),
            Some(ActionKind::BrowserAction) => Some("_execute_browser_action"),
            Some(ActionKind::PageAction) => Some("_execute_page_action"),
            None => None,
        };
        if let Some(name) = action_command
            && !list.iter().any(|c| c["name"] == name)
        {
            list.push(json!({ "name": name, "description": "", "shortcut": e.commands.overrides.get(name).cloned().unwrap_or_default() }));
        }
        list
    }

    fn all_commands(&self) -> Value {
        let exts = self.extensions.borrow();
        let mut ids: Vec<&String> = exts.keys().collect();
        ids.sort();
        let mut taken: Vec<String> = vec![];
        let mut out = vec![];
        for e in ids.into_iter().filter_map(|id| exts.get(id)) {
            for mut c in Self::commands_of(e) {
                let shortcut = c["shortcut"].as_str().unwrap_or("").to_string();
                // The first extension to claim a shortcut keeps it.
                if !shortcut.is_empty() && taken.contains(&shortcut) {
                    c["shortcut"] = json!("");
                    c["conflict"] = json!(true);
                } else if !shortcut.is_empty() {
                    taken.push(shortcut);
                }
                c["extensionId"] = json!(e.id);
                out.push(c);
            }
        }
        Value::Array(out)
    }

    fn command(&self, id: &str, name: &str, tab: Option<i64>) -> Value {
        if ui::is_action_command(name) {
            return self.action_clicked(id, tab, &Value::Null);
        }
        let known = self.extensions.borrow().get(id).is_some_and(|e| e.manifest.commands.iter().any(|c| c.name == name));
        if !known {
            return Value::Null;
        }
        if let Some(t) = tab {
            self.grant_active_tab(id, t);
        }
        let tab_json = tab.and_then(|t| self.browser.borrow().tab(t).cloned()).and_then(|t| self.with_visibility(id, |v| tab_json(&t, v)));
        let mut args = vec![json!(name)];
        if let Some(t) = tab_json {
            args.push(t);
        }
        self.host.dispatch_event(id, "commands.onCommand", args);
        Value::Null
    }

    fn set_shortcut(&self, id: &str, name: &str, shortcut: &str) -> Result<(), String> {
        let shortcut = ui::normalize_shortcut(shortcut)?;
        {
            let mut exts = self.extensions.borrow_mut();
            if !shortcut.is_empty() {
                for (other_id, other) in exts.iter() {
                    if let Some(c) = Self::commands_of(other)
                        .iter()
                        .find(|c| c["shortcut"] == shortcut.as_str() && !(other_id == id && c["name"] == name))
                    {
                        return Err(format!(
                            "{shortcut} is already used by \"{}\" of {}",
                            c["name"].as_str().unwrap_or(""),
                            other.i18n.localize(&other.manifest.name)
                        ));
                    }
                }
            }
            let e = exts.get_mut(id).ok_or("no such extension")?;
            if !Self::commands_of(e).iter().any(|c| c["name"] == name) {
                return Err(format!("no command named '{name}'"));
            }
            e.commands.overrides.insert(name.into(), shortcut.clone());
        }
        self.persist_ui(id);
        self.host.dispatch_event(id, "commands.onChanged", vec![json!({ "name": name, "newShortcut": shortcut })]);
        self.ui_changed(id, "commands");
        Ok(())
    }

    fn commands_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        match op {
            "getAll" => self.with_ext(call, |e| Ok(Value::Array(Self::commands_of(e)))),
            "update" => {
                let detail = call.arg(0);
                let name = detail.get("name").and_then(Value::as_str).ok_or_else(|| ApiError::invalid("name must be a string"))?;
                if let Some(d) = detail.get("description").and_then(Value::as_str) {
                    self.with_ext_mut(call, |e| {
                        e.commands.descriptions.insert(name.into(), d.into());
                        Ok(())
                    })?;
                }
                if let Some(s) = detail.get("shortcut").and_then(Value::as_str) {
                    self.set_shortcut(&call.extension_id, name, s).map_err(ApiError::runtime)?;
                }
                Ok(Value::Null)
            }
            "reset" => {
                let name = call.arg(0).as_str().ok_or_else(|| ApiError::invalid("name must be a string"))?.to_string();
                self.with_ext_mut(call, |e| {
                    e.commands.overrides.remove(&name);
                    e.commands.descriptions.remove(&name);
                    Ok(())
                })?;
                self.persist_ui(&call.extension_id);
                self.ui_changed(&call.extension_id, "commands");
                Ok(Value::Null)
            }
            _ => Err(ApiError::new(ErrorKind::NotAvailable, format!("commands.{op} is not supported by this browser yet"))),
        }
    }

    // ---- Running code in pages --------------------------------------------------------------------------------------

    pub(crate) fn rebuild_content_scripts(&self, id: &str) {
        if let Some(e) = self.extensions.borrow_mut().get_mut(id) {
            let n = e.static_content_scripts;
            e.manifest.content_scripts.truncate(n);
            let registered: Vec<ContentScript> = e.registered_scripts.iter().map(|r| r.script.clone()).collect();
            e.manifest.content_scripts.extend(registered);
        }
    }

    fn persist_scripts(&self, id: &str) {
        let data =
            self.extensions.borrow().get(id).map(|e| {
                Value::Array(e.registered_scripts.iter().filter(|r| r.persist).map(RegisteredScript::to_json).collect()).to_string()
            });
        if let Some(data) = data {
            self.host.write_storage(id, "scripts", &data);
        }
    }

    fn registered_scripts_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        let filter_ids = |v: &Value| -> Option<Vec<String>> {
            v.get("ids").and_then(Value::as_array).map(|a| a.iter().filter_map(|i| i.as_str().map(String::from)).collect())
        };
        let result = self.with_ext_mut(call, |e| match op {
            "registerContentScripts" | "updateContentScripts" => {
                let list = call.arg(0).as_array().ok_or_else(|| ApiError::invalid("scripts must be an array"))?;
                let mut next = e.registered_scripts.clone();
                for s in list {
                    let id = s.get("id").and_then(Value::as_str).unwrap_or("");
                    let existing = next.iter().position(|r| r.id == id);
                    match (op, existing) {
                        ("registerContentScripts", Some(_)) => return Err(ApiError::runtime(format!("Duplicate script ID '{id}'"))),
                        ("updateContentScripts", None) => {
                            return Err(ApiError::runtime(format!("Script with ID '{id}' does not exist or is not fully registered")));
                        }
                        _ => {}
                    }
                    let parsed = RegisteredScript::parse(s, &e.base_url, existing.map(|i| &next[i])).map_err(ApiError::runtime)?;
                    for f in parsed.script.js.iter().chain(&parsed.script.css) {
                        if e.files.get(f).is_none() {
                            return Err(ApiError::runtime(format!("Could not load file: '{f}'.")));
                        }
                    }
                    match existing {
                        Some(i) => next[i] = parsed,
                        None => next.push(parsed),
                    }
                }
                e.registered_scripts = next;
                Ok(Value::Null)
            }
            "unregisterContentScripts" => {
                match filter_ids(call.arg(0)) {
                    Some(ids) => {
                        if let Some(missing) = ids.iter().find(|i| !e.registered_scripts.iter().any(|r| &r.id == *i)) {
                            return Err(ApiError::runtime(format!("Nonexistent script ID '{missing}'")));
                        }
                        e.registered_scripts.retain(|r| !ids.contains(&r.id));
                    }
                    None => e.registered_scripts.clear(),
                }
                Ok(Value::Null)
            }
            "getRegisteredContentScripts" => {
                let ids = filter_ids(call.arg(0));
                Ok(Value::Array(
                    e.registered_scripts
                        .iter()
                        .filter(|r| ids.as_ref().is_none_or(|ids| ids.contains(&r.id)))
                        .map(RegisteredScript::to_json)
                        .collect(),
                ))
            }
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("scripting.{op}"))),
        });
        if result.is_ok() && op != "getRegisteredContentScripts" {
            self.rebuild_content_scripts(&call.extension_id);
            self.persist_scripts(&call.extension_id);
        }
        result
    }

    /// The JavaScript to inject for `files` or `func`, in the extension's content-script world or the page's.
    ///
    /// The host evaluates it as a classic script of the frame's document (not subject to the page's CSP), after setting the
    /// global property named `binding` to the extension's content-script API (`{ chrome, browser }`); the code deletes that
    /// property before anything else runs. The script's completion value, awaited if it is a promise, is the result.
    fn injection_code(sources: &[String], binding: &str, isolated: bool, firefox: bool) -> String {
        let body = sources.join("\n;\n");
        if !isolated {
            return body;
        }
        let key = serde_json::to_string(binding).unwrap_or_default();
        let browser = if firefox { "let browser = __cgxApi && __cgxApi.browser;" } else { "" };
        // A block, so that `chrome` is bound for this code only and the block's completion value is the code's.
        format!(
            "{{ let __cgxApi = globalThis[{key}]; try {{ delete globalThis[{key}]; }} catch (e) {{}} let chrome = __cgxApi && __cgxApi.chrome; {browser} void 0;\n{body}\n}}"
        )
    }

    fn fresh_binding(&self) -> String {
        let n = self.next_serial.get() + 1;
        self.next_serial.set(n);
        let seed = format!("{n}:{}", self.host.now_ms());
        let h = crate::encoding::sha256(seed.as_bytes());
        format!("__cgx_{}", h[..12].iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    /// Frames, permissions, then the host. `kind` is `js`, `css` or `removeCss`; `payload` the code or stylesheet.
    #[allow(clippy::too_many_arguments)]
    fn inject(
        &self,
        call: &ApiCall,
        target: Injection,
        kind: &'static str,
        payload: String,
        world_main: bool,
        run_at: &str,
        css_origin: &str,
        reply: Reply,
    ) {
        let tab_url = match self.browser.borrow().tab(target.tab_id) {
            Some(t) => t.url.clone(),
            None => return reply(Err(no_tab(target.tab_id))),
        };
        let this = self.clone();
        let call2 = call.clone();
        let run_at = run_at.to_string();
        let css_origin = css_origin.to_string();
        self.host_call(
            call,
            "host.tabs.frames",
            json!({ "tabId": target.tab_id }),
            Box::new(move |frames: ApiResult| {
                let frames = match frames {
                    Ok(f) => f,
                    Err(e) => return reply(Err(e)),
                };
                let frames: Vec<(i64, String)> = frames
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|f| Some((f.get("frameId")?.as_i64()?, f.get("url").and_then(Value::as_str).unwrap_or("").to_string())))
                    .collect();
                let frames = if frames.is_empty() { vec![(0, tab_url.clone())] } else { frames };
                let chosen: Vec<(i64, String)> = match &target.frame_ids {
                    Some(ids) => {
                        let mut out = vec![];
                        for id in ids {
                            match frames.iter().find(|(f, _)| f == id) {
                                Some(f) => out.push(f.clone()),
                                None => {
                                    return reply(Err(ApiError::runtime(format!(
                                        "No frame with id {id} in tab with id {}",
                                        target.tab_id
                                    ))));
                                }
                            }
                        }
                        out
                    }
                    None if target.all_frames => frames.clone(),
                    None => frames.iter().filter(|(f, _)| *f == 0).cloned().collect(),
                };
                // about:blank and similar frames take their creator's access; use the tab's URL for them.
                let effective = |url: &str| {
                    if crate::match_pattern::split_url(url).is_some_and(|u| !u.host.is_empty() || u.scheme == "file") {
                        url.to_string()
                    } else {
                        tab_url.clone()
                    }
                };
                let allowed = |url: &str| -> bool {
                    let url = effective(url);
                    if crate::store::store_for_url(&url).is_some() {
                        return false;
                    }
                    this.extensions.borrow().get(&call2.extension_id).is_some_and(|e| e.permissions.has_host_for_tab(target.tab_id, &url))
                };
                let mut permitted = vec![];
                for (id, url) in &chosen {
                    if allowed(url) {
                        permitted.push(*id);
                    } else if *id == 0 || target.frame_ids.is_some() {
                        let message = if crate::store::store_for_url(&effective(url)).is_some() {
                            "The extensions gallery cannot be scripted.".to_string()
                        } else {
                            CANNOT_ACCESS.to_string()
                        };
                        return reply(Err(ApiError::runtime(message)));
                    }
                }
                let firefox = match this.with_ext(&call2, |e| Ok(is_firefox(e))) {
                    Ok(v) => v,
                    Err(e) => return reply(Err(e)),
                };
                let binding = this.fresh_binding();
                let code = if kind == "js" { Self::injection_code(&[payload], &binding, !world_main, firefox) } else { payload };
                let args = json!({
                    "tabId": target.tab_id,
                    "frameIds": permitted,
                    "kind": kind,
                    "code": code,
                    "binding": binding,
                    "world": if world_main { "MAIN" } else { "ISOLATED" },
                    "runAt": run_at,
                    "cssOrigin": css_origin,
                    "setupProgram": this.content_script_setup_program(&call2.extension_id),
                });
                let firefox_results = target.firefox_results;
                this.host_call(&call2, "host.scripting.inject", args, reply, move |_, results| {
                    let mut list: Vec<Value> = results.as_array().cloned().unwrap_or_default();
                    list.sort_by_key(|r| r.get("frameId").and_then(Value::as_i64).unwrap_or(0));
                    if kind != "js" {
                        return Ok(Value::Null);
                    }
                    if firefox_results {
                        // Firefox rejects when the script in the top frame threw.
                        if let Some(err) = list
                            .iter()
                            .find(|r| r.get("frameId").and_then(Value::as_i64) == Some(0))
                            .and_then(|r| r.get("error"))
                            .and_then(Value::as_str)
                        {
                            return Err(ApiError::runtime(err.to_string()));
                        }
                        return Ok(Value::Array(list.into_iter().map(|r| r.get("result").cloned().unwrap_or(Value::Null)).collect()));
                    }
                    Ok(Value::Array(
                        list.into_iter()
                            .map(|r| {
                                let mut o = Map::new();
                                o.insert("frameId".into(), r.get("frameId").cloned().unwrap_or(json!(0)));
                                if let Some(d) = r.get("documentId") {
                                    o.insert("documentId".into(), d.clone());
                                }
                                if let Some(v) = r.get("result") {
                                    o.insert("result".into(), v.clone());
                                }
                                if let Some(e) = r.get("error") {
                                    o.insert("error".into(), e.clone());
                                }
                                Value::Object(o)
                            })
                            .collect(),
                    ))
                });
            }),
            |_, v| Ok(v),
        );
    }

    /// Loads files of the extension for injection, as text.
    fn load_sources(&self, call: &ApiCall, files: &[String]) -> Result<Vec<String>, ApiError> {
        self.with_ext(call, |e| {
            files
                .iter()
                .map(|f| {
                    let path = ui::normalize_extension_path(f, &e.base_url);
                    e.files
                        .get(&path)
                        .map(|b| String::from_utf8_lossy(b).into_owned())
                        .ok_or_else(|| ApiError::runtime(format!("Could not load file: '{path}'.")))
                })
                .collect()
        })
    }

    fn scripting_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        if matches!(op, "registerContentScripts" | "updateContentScripts" | "unregisterContentScripts" | "getRegisteredContentScripts") {
            return reply(self.registered_scripts_api(op, call));
        }
        let details = call.arg(0);
        let target = details.get("target").cloned().unwrap_or(Value::Null);
        let Some(tab_id) = target.get("tabId").and_then(Value::as_i64) else {
            return reply(Err(ApiError::invalid("target.tabId must be an integer")));
        };
        let frame_ids = target.get("frameIds").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<_>>());
        let all_frames = target.get("allFrames").and_then(Value::as_bool).unwrap_or(false);
        if all_frames && frame_ids.is_some() {
            return reply(Err(ApiError::invalid("Cannot specify both 'allFrames' and 'frameIds'.")));
        }
        let injection = Injection { tab_id, frame_ids, all_frames, firefox_results: false };
        let files: Option<Vec<String>> =
            details.get("files").and_then(Value::as_array).map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect());
        let run_at =
            if details.get("injectImmediately").and_then(Value::as_bool).unwrap_or(false) { "document_start" } else { "document_idle" };
        match op {
            "executeScript" => {
                let func = details.get("__funcSource").and_then(Value::as_str);
                let sources = match (&files, func) {
                    (Some(_), Some(_)) | (None, None) => {
                        return reply(Err(ApiError::invalid("Exactly one of 'func' and 'files' must be specified")));
                    }
                    (Some(files), None) => match self.load_sources(call, files) {
                        Ok(s) => s,
                        Err(e) => return reply(Err(e)),
                    },
                    (None, Some(f)) => {
                        let args = details.get("args").cloned().unwrap_or_else(|| json!([]));
                        let args =
                            args.as_array().map(|a| a.iter().map(Value::to_string).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                        vec![format!("({f})({args})")]
                    }
                };
                let main = details.get("world").and_then(Value::as_str) == Some("MAIN");
                self.inject(call, injection, "js", sources.join("\n;\n"), main, run_at, "author", reply)
            }
            "insertCSS" | "removeCSS" => {
                let css = match (&files, details.get("css").and_then(Value::as_str)) {
                    (Some(_), Some(_)) | (None, None) => {
                        return reply(Err(ApiError::invalid("Exactly one of 'css' and 'files' must be specified")));
                    }
                    (Some(files), None) => match self.load_sources(call, files) {
                        Ok(s) => s.join("\n"),
                        Err(e) => return reply(Err(e)),
                    },
                    (None, Some(css)) => css.to_string(),
                };
                let origin = details.get("origin").and_then(Value::as_str).unwrap_or("AUTHOR").to_ascii_lowercase();
                self.inject(call, injection, if op == "insertCSS" { "css" } else { "removeCss" }, css, false, run_at, &origin, reply)
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("scripting.{op}")))),
        }
    }

    /// Firefox's (and Chrome MV2's) `tabs.executeScript(tabId?, details)`, `insertCSS`, `removeCSS`.
    fn tabs_inject(&self, op: &str, call: &ApiCall, reply: Reply) {
        let (tab_arg, details) =
            if call.arg(0).is_object() { (Value::Null, call.arg(0).clone()) } else { (call.arg(0).clone(), call.arg(1).clone()) };
        let tab_id = match self.tab_or_active(call, &tab_arg) {
            Ok(t) => t,
            Err(e) => return reply(Err(e)),
        };
        let frame_ids = details.get("frameId").and_then(Value::as_i64).map(|f| vec![f]);
        let all_frames = details.get("allFrames").and_then(Value::as_bool).unwrap_or(false);
        let injection = Injection { tab_id, frame_ids, all_frames, firefox_results: true };
        let run_at = details.get("runAt").and_then(Value::as_str).unwrap_or("document_idle").to_string();
        let source = match (details.get("code").and_then(Value::as_str), details.get("file").and_then(Value::as_str)) {
            (Some(code), None) => code.to_string(),
            (None, Some(file)) => match self.load_sources(call, &[file.to_string()]) {
                Ok(mut s) => s.remove(0),
                Err(e) => return reply(Err(e)),
            },
            _ => return reply(Err(ApiError::invalid("Exactly one of 'code' and 'file' must be specified"))),
        };
        let kind = match op {
            "executeScript" => "js",
            "insertCSS" => "css",
            _ => "removeCss",
        };
        let origin = details.get("cssOrigin").and_then(Value::as_str).unwrap_or("author").to_string();
        self.inject(call, injection, kind, source, false, &run_at, &origin, reply)
    }

    // ---- tabs and windows -------------------------------------------------------------------------------------------

    fn tabs_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let a0 = call.arg(0).clone();
        let a1 = call.arg(1).clone();
        let tab_then = |id_key: &'static str| {
            move |this: &Dispatcher, call: &ApiCall, v: Value| -> ApiResult {
                let id = v.get(id_key).and_then(Value::as_i64).ok_or_else(|| ApiError::runtime("The browser did not report the tab"))?;
                this.tab_for(call, id)
            }
        };
        match op {
            "get" => {
                let r = a0.as_i64().ok_or_else(|| ApiError::invalid("tabId must be an integer")).and_then(|id| self.tab_for(call, id));
                reply(r)
            }
            "getCurrent" => reply(match call.caller.tab_id.filter(|_| call.caller.context != ContextKind::Background) {
                Some(id) if self.browser.borrow().tab(id).is_some() => self.tab_for(call, id),
                _ => Ok(Value::Null),
            }),
            "query" => {
                let current = self.current_window(call);
                let state = self.browser.borrow();
                let r = self.with_visibility(&call.extension_id, |v| browser::query(&state, &a0, current, v)).unwrap_or(Ok(vec![]));
                reply(r.map(Value::Array).map_err(ApiError::invalid))
            }
            "create" => {
                let props = if a0.is_null() { json!({}) } else { a0 };
                let url = match self.resolve_url(call, props.get("url").unwrap_or(&Value::Null)) {
                    Ok(u) => u,
                    Err(e) => return reply(Err(e)),
                };
                let window = match self.resolve_window(call, props.get("windowId").and_then(Value::as_i64)) {
                    Ok(w) => w,
                    Err(e) => return reply(Err(e)),
                };
                if let Some(opener) = props.get("openerTabId").and_then(Value::as_i64)
                    && self.browser.borrow().tab(opener).is_none()
                {
                    return reply(Err(no_tab(opener)));
                }
                let args = json!({
                    "windowId": window,
                    "index": props.get("index").and_then(Value::as_i64),
                    "url": url,
                    "active": props.get("active").or(props.get("selected")).and_then(Value::as_bool).unwrap_or(true),
                    "pinned": props.get("pinned").and_then(Value::as_bool).unwrap_or(false),
                    "openerTabId": props.get("openerTabId").and_then(Value::as_i64),
                });
                let c = call.clone();
                self.host_call(call, "host.tabs.create", args, reply, move |this, v| tab_then("tabId")(this, &c, v))
            }
            "update" | "discard" | "reload" | "goBack" | "goForward" | "remove" | "duplicate" | "getZoom" | "setZoom"
            | "detectLanguage" => {
                // The tab id is optional for most of these: without it they act on the active tab of the current window.
                let (tab_value, rest) = match op {
                    "update" | "reload" if a0.is_object() => (Value::Null, a0.clone()),
                    "setZoom" if a0.is_number() && a1.is_null() && call.args.len() == 1 => (Value::Null, a0.clone()),
                    _ => (a0.clone(), a1.clone()),
                };
                if op == "remove" {
                    let ids: Vec<i64> = match &a0 {
                        Value::Array(list) => list.iter().filter_map(Value::as_i64).collect(),
                        v => v.as_i64().into_iter().collect(),
                    };
                    if ids.is_empty() {
                        return reply(Err(ApiError::invalid("tabIds must be an integer or an array of integers")));
                    }
                    if let Some(missing) = ids.iter().find(|id| self.browser.borrow().tab(**id).is_none()) {
                        return reply(Err(no_tab(*missing)));
                    }
                    return self.host_call(call, "host.tabs.remove", json!({ "tabIds": ids }), reply, |_, _| Ok(Value::Null));
                }
                let tab_id = match self.tab_or_active(call, &tab_value) {
                    Ok(t) => t,
                    Err(e) => return reply(Err(e)),
                };
                let c = call.clone();
                match op {
                    "update" => {
                        let url = match self.resolve_url(call, rest.get("url").unwrap_or(&Value::Null)) {
                            Ok(u) => u,
                            Err(e) => return reply(Err(e)),
                        };
                        let mut args = json!({ "tabId": tab_id, "url": url });
                        for key in ["active", "pinned", "muted", "highlighted", "autoDiscardable"] {
                            if let Some(b) = rest.get(key).and_then(Value::as_bool) {
                                args[key] = json!(b);
                            }
                        }
                        if rest.get("selected").and_then(Value::as_bool) == Some(true) {
                            args["active"] = json!(true);
                        }
                        self.host_call(call, "host.tabs.update", args, reply, move |this, _| this.tab_for(&c, tab_id))
                    }
                    "discard" => self
                        .host_call(call, "host.tabs.discard", json!({ "tabId": tab_id }), reply, move |this, _| this.tab_for(&c, tab_id)),
                    "reload" => {
                        let bypass = rest.get("bypassCache").and_then(Value::as_bool).unwrap_or(false);
                        self.host_call(call, "host.tabs.reload", json!({ "tabId": tab_id, "bypassCache": bypass }), reply, |_, _| {
                            Ok(Value::Null)
                        })
                    }
                    "goBack" | "goForward" => {
                        let delta = if op == "goBack" { -1 } else { 1 };
                        self.host_call(call, "host.tabs.navigateHistory", json!({ "tabId": tab_id, "delta": delta }), reply, |_, _| {
                            Ok(Value::Null)
                        })
                    }
                    "duplicate" => self.host_call(call, "host.tabs.duplicate", json!({ "tabId": tab_id }), reply, move |this, v| {
                        tab_then("tabId")(this, &c, v)
                    }),
                    "getZoom" => reply(Ok(json!(self.browser.borrow().tab(tab_id).map_or(1.0, |t| t.zoom)))),
                    "setZoom" => {
                        let factor = rest.as_f64().unwrap_or(0.0);
                        let factor = if factor == 0.0 { 1.0 } else { factor };
                        if !(0.25..=5.0).contains(&factor) {
                            return reply(Err(ApiError::runtime("Zoom value is out of range.")));
                        }
                        self.host_call(call, "host.tabs.setZoom", json!({ "tabId": tab_id, "zoomFactor": factor }), reply, |_, _| {
                            Ok(Value::Null)
                        })
                    }
                    "detectLanguage" => {
                        // The document's declared language, as a BCP 47 primary tag; "und" when it declares none.
                        let code = "(function () { var l = (document.documentElement && document.documentElement.lang) || ''; return l.split(/[-_]/)[0].toLowerCase() || 'und'; })()".to_string();
                        let injection = Injection { tab_id, frame_ids: Some(vec![0]), all_frames: false, firefox_results: true };
                        let c2 = call.clone();
                        let reply2: Reply = Box::new(move |r: ApiResult| reply(r.map(|v| v.get(0).cloned().unwrap_or(json!("und")))));
                        // Reading the declared language needs no host permission in Chrome.
                        self.inject_unchecked(&c2, injection, code, reply2)
                    }
                    _ => unreachable!(),
                }
            }
            "move" => {
                let ids: Vec<i64> = match &a0 {
                    Value::Array(list) => list.iter().filter_map(Value::as_i64).collect(),
                    v => v.as_i64().into_iter().collect(),
                };
                if let Some(missing) = ids.iter().find(|id| self.browser.borrow().tab(**id).is_none()) {
                    return reply(Err(no_tab(*missing)));
                }
                let index = a1.get("index").and_then(Value::as_i64).unwrap_or(-1);
                let window = match self.resolve_window(call, a1.get("windowId").and_then(Value::as_i64)) {
                    Ok(w) => w.or_else(|| ids.first().and_then(|id| self.browser.borrow().tab(*id).map(|t| t.window_id))),
                    Err(e) => return reply(Err(e)),
                };
                let single = !a0.is_array();
                let c = call.clone();
                self.host_call(
                    call,
                    "host.tabs.move",
                    json!({ "tabIds": ids, "windowId": window, "index": index }),
                    reply,
                    move |this, _| {
                        let tabs: Vec<Value> = ids.iter().filter_map(|id| this.tab_for(&c, *id).ok()).collect();
                        Ok(if single { tabs.into_iter().next().unwrap_or(Value::Null) } else { Value::Array(tabs) })
                    },
                )
            }
            "highlight" => {
                let window = match self.resolve_window(call, a0.get("windowId").and_then(Value::as_i64)) {
                    Ok(w) => w.or_else(|| self.current_window(call)),
                    Err(e) => return reply(Err(e)),
                };
                let Some(window) = window else { return reply(Err(ApiError::runtime("No current window"))) };
                let indices: Vec<u64> = match a0.get("tabs") {
                    Some(Value::Array(l)) => l.iter().filter_map(Value::as_u64).collect(),
                    Some(v) => v.as_u64().into_iter().collect(),
                    None => vec![],
                };
                // Ladybird's tab strip has no multi-selection: the first tab is activated, which highlights it.
                let first =
                    indices.first().and_then(|i| self.browser.borrow().window(window).and_then(|w| w.tabs.get(*i as usize).map(|t| t.id)));
                let Some(first) = first else {
                    return reply(Err(ApiError::runtime(format!("No tab at index: {}.", indices.first().copied().unwrap_or(0)))));
                };
                let c = call.clone();
                self.host_call(call, "host.tabs.update", json!({ "tabId": first, "active": true }), reply, move |this, _| {
                    this.window_for(&c, window, true)
                })
            }
            "captureVisibleTab" => {
                let (window_arg, options) = if a0.is_object() { (None, a0.clone()) } else { (a0.as_i64(), a1.clone()) };
                let window = match self.resolve_window(call, window_arg) {
                    Ok(w) => w.or_else(|| self.current_window(call)),
                    Err(e) => return reply(Err(e)),
                };
                let Some(tab) = window.and_then(|w| self.browser.borrow().active_tab_of(w).cloned()) else {
                    return reply(Err(ApiError::runtime("No active tab")));
                };
                let allowed = self.with_ext(call, |e| {
                    Ok(e.permissions.has_host_for_tab(tab.id, &tab.url) && e.permissions.has_host_for_tab(tab.id, &tab.url)
                        || e.permissions.grants_all_hosts())
                });
                if !matches!(allowed, Ok(true)) {
                    return reply(Err(ApiError::runtime("Either the '<all_urls>' or 'activeTab' permission is required.")));
                }
                let format = options.get("format").and_then(Value::as_str).unwrap_or("jpeg");
                let quality = options.get("quality").and_then(Value::as_i64).unwrap_or(92);
                self.host_call(
                    call,
                    "host.tabs.capture",
                    json!({ "tabId": tab.id, "format": format, "quality": quality }),
                    reply,
                    |_, v| v.get("dataUrl").cloned().ok_or_else(|| ApiError::runtime("Failed to capture tab: unknown error")),
                )
            }
            "executeScript" | "insertCSS" | "removeCSS" => self.tabs_inject(op, call, reply),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("tabs.{op}")))),
        }
    }

    /// Injection that does not need host permission (reading the document's language).
    fn inject_unchecked(&self, call: &ApiCall, target: Injection, code: String, reply: Reply) {
        let binding = self.fresh_binding();
        let args = json!({
            "tabId": target.tab_id, "frameIds": target.frame_ids.unwrap_or_else(|| vec![0]), "kind": "js", "code": code, "binding": binding,
            "world": "MAIN", "runAt": "document_start", "cssOrigin": "author", "setupProgram": self.content_script_setup_program(&call.extension_id),
        });
        self.host_call(call, "host.scripting.inject", args, reply, |_, results| {
            Ok(Value::Array(results.as_array().into_iter().flatten().map(|r| r.get("result").cloned().unwrap_or(Value::Null)).collect()))
        })
    }

    fn windows_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let a0 = call.arg(0).clone();
        let a1 = call.arg(1).clone();
        let populate = |v: &Value| v.get("populate").and_then(Value::as_bool).unwrap_or(false);
        let types_ok = |v: &Value, w: &crate::browser::WindowState| {
            v.get("windowTypes").and_then(Value::as_array).is_none_or(|types| types.iter().any(|t| t.as_str() == Some(&w.kind)))
        };
        match op {
            "get" => {
                let Some(id) = a0.as_i64() else { return reply(Err(ApiError::invalid("windowId must be an integer"))) };
                let id = if id == WINDOW_ID_CURRENT { self.current_window(call).unwrap_or(WINDOW_ID_NONE) } else { id };
                reply(self.window_for(call, id, populate(&a1)))
            }
            "getCurrent" | "getLastFocused" => {
                let id = if op == "getCurrent" { self.current_window(call) } else { self.browser.borrow().last_focused_window };
                match id {
                    Some(id) => reply(self.window_for(call, id, populate(&a0))),
                    None => reply(Err(ApiError::runtime("No current window"))),
                }
            }
            "getAll" => {
                let state = self.browser.borrow();
                let list = self
                    .with_visibility(&call.extension_id, |v| {
                        state.windows.iter().filter(|w| types_ok(&a0, w)).map(|w| window_json(w, populate(&a0), v)).collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                reply(Ok(Value::Array(list)))
            }
            "create" => {
                let data = if a0.is_null() { json!({}) } else { a0 };
                let urls: Vec<Value> = match data.get("url") {
                    None | Some(Value::Null) => vec![],
                    Some(Value::Array(list)) => list.clone(),
                    Some(v) => vec![v.clone()],
                };
                let mut resolved = vec![];
                for u in &urls {
                    match self.resolve_url(call, u) {
                        Ok(Some(u)) => resolved.push(u),
                        Ok(None) => {}
                        Err(e) => return reply(Err(e)),
                    }
                }
                let tab_id = data.get("tabId").and_then(Value::as_i64);
                if let Some(t) = tab_id
                    && self.browser.borrow().tab(t).is_none()
                {
                    return reply(Err(no_tab(t)));
                }
                let kind = data.get("type").and_then(Value::as_str).unwrap_or("normal");
                if !matches!(kind, "normal" | "popup" | "panel" | "detached_panel") {
                    return reply(Err(ApiError::invalid(format!("'{kind}' is not a valid window type"))));
                }
                let state = data.get("state").and_then(Value::as_str);
                if state.is_some_and(|s| s != "normal") && ["left", "top", "width", "height"].iter().any(|k| data.get(*k).is_some()) {
                    return reply(Err(ApiError::invalid(format!(
                        "Invalid value for state: {} windows cannot have bounds.",
                        state.unwrap_or("")
                    ))));
                }
                let args = json!({
                    "urls": resolved, "tabId": tab_id,
                    "left": data.get("left"), "top": data.get("top"), "width": data.get("width"), "height": data.get("height"),
                    "focused": data.get("focused").and_then(Value::as_bool).unwrap_or(true),
                    "incognito": data.get("incognito").and_then(Value::as_bool).unwrap_or(false),
                    "type": if kind == "detached_panel" { "panel" } else { kind },
                    "state": state,
                });
                let c = call.clone();
                self.host_call(call, "host.windows.create", args, reply, move |this, v| {
                    let id = v
                        .get("windowId")
                        .and_then(Value::as_i64)
                        .ok_or_else(|| ApiError::runtime("The browser did not report the window"))?;
                    this.window_for(&c, id, true)
                })
            }
            "update" => {
                let Some(id) = a0.as_i64() else { return reply(Err(ApiError::invalid("windowId must be an integer"))) };
                let id = match self.resolve_window(call, Some(id)) {
                    Ok(Some(id)) => id,
                    Ok(None) => return reply(Err(ApiError::runtime("No current window"))),
                    Err(e) => return reply(Err(e)),
                };
                let mut args = json!({ "windowId": id });
                for key in ["left", "top", "width", "height", "focused", "state", "drawAttention"] {
                    if let Some(v) = a1.get(key).filter(|v| !v.is_null()) {
                        args[key] = v.clone();
                    }
                }
                let c = call.clone();
                self.host_call(call, "host.windows.update", args, reply, move |this, _| this.window_for(&c, id, false))
            }
            "remove" => {
                let Some(id) = a0.as_i64() else { return reply(Err(ApiError::invalid("windowId must be an integer"))) };
                let id = match self.resolve_window(call, Some(id)) {
                    Ok(Some(id)) => id,
                    Ok(None) => return reply(Err(ApiError::runtime("No current window"))),
                    Err(e) => return reply(Err(e)),
                };
                self.host_call(call, "host.windows.remove", json!({ "windowId": id }), reply, |_, _| Ok(Value::Null))
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("windows.{op}")))),
        }
    }

    // ---- runtime, management, i18n ----------------------------------------------------------------------------------

    fn runtime_api(&self, name: &str, call: &ApiCall, reply: Reply) {
        match name {
            "runtime.getPlatformInfo" => reply(Ok(platform_info())),
            "runtime.getBrowserInfo" => {
                reply(Ok(json!({ "name": "Ladybird", "vendor": "Ladybird", "version": env!("CARGO_PKG_VERSION"), "buildID": "0" })))
            }
            "runtime.getVersion" => reply(self.with_ext(call, |e| Ok(json!(e.manifest.version)))),
            "runtime.setUninstallURL" => {
                let url = call.arg(0).as_str().unwrap_or("").to_string();
                if url.len() > 1023 {
                    return reply(Err(ApiError::runtime("URL is too long.")));
                }
                if !(url.is_empty() || url.starts_with("http://") || url.starts_with("https://")) {
                    return reply(Err(ApiError::runtime("Invalid URL: must use http or https.")));
                }
                reply(self.with_ext_mut(call, |e| {
                    e.uninstall_url = (!url.is_empty()).then_some(url);
                    Ok(Value::Null)
                }))
            }
            "runtime.reload" => self.host_call(call, "host.runtime.reload", json!({}), reply, |_, _| Ok(Value::Null)),
            "runtime.openOptionsPage" => {
                let url = self.with_ext(call, |e| {
                    Ok((!e.manifest.options_page.is_empty())
                        .then(|| format!("{}{}", e.base_url, ui::normalize_extension_path(&e.manifest.options_page, &e.base_url))))
                });
                let url = match url {
                    Ok(Some(u)) => u,
                    Ok(None) => return reply(Err(ApiError::runtime("Could not create an options page."))),
                    Err(e) => return reply(Err(e)),
                };
                // An options page that is already open is brought forward rather than opened twice.
                let existing =
                    self.browser.borrow().tabs().find(|t| t.url.split('#').next() == Some(url.as_str())).map(|t| (t.id, t.window_id));
                match existing {
                    Some((tab, window)) => {
                        let c = call.clone();
                        self.host_call(call, "host.tabs.update", json!({ "tabId": tab, "active": true }), Box::new(move |_| {}), |_, _| {
                            Ok(Value::Null)
                        });
                        self.host_call(&c, "host.windows.update", json!({ "windowId": window, "focused": true }), reply, |_, _| {
                            Ok(Value::Null)
                        });
                    }
                    None => self.host_call(call, "host.tabs.create", json!({ "url": url, "active": true }), reply, |_, _| Ok(Value::Null)),
                }
            }
            "runtime.getContexts" => {
                let filter = call.arg(0).clone();
                let list: Vec<Value> = {
                    let bus = self.bus.borrow();
                    let state = self.browser.borrow();
                    bus.contexts_of(&call.extension_id)
                        .filter(|c| c.kind != ContextKind::ContentScript)
                        .map(|c| {
                            let kind = match (c.kind, c.tab_id) {
                                (ContextKind::Background, _) => "BACKGROUND",
                                (_, Some(_)) => "TAB",
                                _ => "POPUP",
                            };
                            let url = c.url.clone().unwrap_or_default();
                            let window = c.tab_id.and_then(|t| state.tab(t)).map(|t| t.window_id).unwrap_or(-1);
                            json!({
                                "contextType": kind,
                                "contextId": format!("{:08x}-0000-4000-8000-{:012x}", c.id, c.id),
                                "tabId": c.tab_id.unwrap_or(-1),
                                "windowId": window,
                                "frameId": c.frame_id.unwrap_or(if kind == "BACKGROUND" { -1 } else { 0 }),
                                "documentUrl": url,
                                "documentOrigin": browser::origin_of(&url),
                                "incognito": false,
                            })
                        })
                        .filter(|c| {
                            let matches_list = |key: &str, field: &str| {
                                filter.get(key).and_then(Value::as_array).is_none_or(|l| l.iter().any(|v| *v == c[field]))
                            };
                            matches_list("contextTypes", "contextType")
                                && matches_list("tabIds", "tabId")
                                && matches_list("windowIds", "windowId")
                                && matches_list("frameIds", "frameId")
                                && matches_list("documentUrls", "documentUrl")
                                && matches_list("documentOrigins", "documentOrigin")
                                && matches_list("contextIds", "contextId")
                        })
                        .collect()
                };
                reply(Ok(Value::Array(list)))
            }
            "management.getSelf" => {
                reply(self.management_info(&call.extension_id).ok_or_else(|| ApiError::runtime("Extension context invalidated.")))
            }
            "management.getAll" => reply(Ok(self.management_list())),
            "management.get" => {
                let id = call.arg(0).as_str().unwrap_or("");
                reply(self.management_info(id).ok_or_else(|| ApiError::runtime(format!("Failed to find extension with id {id}."))))
            }
            // Flushes the browser's cache of webRequest decisions; there is none to flush.
            "webRequest.handlerBehaviorChanged" => reply(Ok(Value::Null)),
            "i18n.getAcceptLanguages" => {
                let locale = self.host.ui_locale();
                let primary = locale.split(['-', '_']).next().unwrap_or("").to_string();
                let mut list = vec![locale.clone()];
                if !primary.is_empty() && primary != locale {
                    list.push(primary);
                }
                reply(Ok(json!(list)))
            }
            // Private windows and file:// access are not offered to extensions yet, as in a browser where the user did not
            // allow them.
            "extension.isAllowedIncognitoAccess" | "extension.isAllowedFileSchemeAccess" => reply(Ok(json!(false))),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, name.to_string()))),
        }
    }

    /// Firefox's `contentScripts.register(options)`: like a manifest content script, for this session only. Inline `code`
    /// entries become files of the extension that only content scripts can reach. Answers `{ id }`, which the shim turns
    /// into a `RegisteredContentScript` whose `unregister()` calls [`Dispatcher::unregister_content_script`].
    fn content_scripts_register(&self, call: &ApiCall) -> ApiResult {
        let options = call.arg(0);
        if !options.is_object() {
            return Err(ApiError::invalid("contentScriptOptions must be an object"));
        }
        let serial = self.next_serial.get() + 1;
        self.next_serial.set(serial);
        let id = format!("__contentScripts_{serial}");
        let strings = |key: &str| -> Vec<String> {
            options
                .get(key)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default()
        };
        let matches = strings("matches");
        if matches.is_empty() {
            return Err(ApiError::invalid("matches must not be empty"));
        }
        for p in &matches {
            crate::match_pattern::MatchPattern::parse(p, crate::match_pattern::MatchDialect::Firefox)
                .map_err(|e| ApiError::invalid(format!("Invalid url pattern: {p} ({e})")))?;
        }
        self.with_ext_mut(call, |e| {
            let mut script = ContentScript {
                matches,
                exclude_matches: strings("excludeMatches"),
                include_globs: strings("includeGlobs"),
                exclude_globs: strings("excludeGlobs"),
                ..Default::default()
            };
            for (key, extension) in [("js", "js"), ("css", "css")] {
                for (n, entry) in options.get(key).and_then(Value::as_array).into_iter().flatten().enumerate() {
                    let path = if let Some(file) = entry.get("file").and_then(Value::as_str) {
                        let path = ui::normalize_extension_path(file, &e.base_url);
                        if e.files.get(&path).is_none() {
                            return Err(ApiError::runtime(format!("Unable to load script: {file}")));
                        }
                        path
                    } else if let Some(code) = entry.get("code").and_then(Value::as_str) {
                        let path = format!("{}registered/{id}-{n}.{extension}", crate::serve::VIRTUAL_PREFIX);
                        e.files.insert(&path, code.as_bytes().to_vec());
                        path
                    } else {
                        return Err(ApiError::invalid(format!("each {key} entry needs a file or code")));
                    };
                    if key == "js" { script.js.push(path) } else { script.css.push(path) }
                }
            }
            script.all_frames = options.get("allFrames").and_then(Value::as_bool).unwrap_or(false);
            script.match_about_blank = options.get("matchAboutBlank").and_then(Value::as_bool).unwrap_or(false);
            script.run_at = match options.get("runAt").and_then(Value::as_str).unwrap_or("document_idle") {
                "document_start" => RunAt::DocumentStart,
                "document_end" => RunAt::DocumentEnd,
                _ => RunAt::DocumentIdle,
            };
            if options.get("world").and_then(Value::as_str) == Some("MAIN") {
                script.world = ScriptWorld::Main;
            }
            e.registered_scripts.push(RegisteredScript { id: id.clone(), script, persist: false });
            Ok(())
        })?;
        self.rebuild_content_scripts(&call.extension_id);
        Ok(json!({ "id": id }))
    }

    /// `RegisteredContentScript.unregister()` (the shim's `__contentScripts.unregister`).
    pub(crate) fn unregister_content_script(&self, call: &ApiCall) -> ApiResult {
        let id = call.arg(0).as_str().unwrap_or("").to_string();
        self.with_ext_mut(call, |e| {
            e.registered_scripts.retain(|r| r.id != id);
            Ok(())
        })?;
        self.rebuild_content_scripts(&call.extension_id);
        Ok(Value::Null)
    }

    // ---- Entry ------------------------------------------------------------------------------------------------------

    pub(crate) fn browser_api(&self, member: &Member, call: &ApiCall, reply: Reply) {
        let op = member.name.as_str();
        match member.namespace.as_str() {
            "tabs" => self.tabs_api(op, call, reply),
            "windows" => self.windows_api(op, call, reply),
            "action" | "browserAction" | "pageAction" => self.action_api(op, call, reply),
            "contextMenus" | "menus" => reply(self.context_menus_api(op, call)),
            "commands" => reply(self.commands_api(op, call)),
            "scripting" => self.scripting_api(op, call, reply),
            "contentScripts" => reply(self.content_scripts_register(call)),
            _ => self.runtime_api(&member.full_name, call, reply),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::InstallOptions;
    use crate::loader::Files;
    use crate::permissions::PermissionRequest;
    use crate::registry::Registry;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A browser with real windows and tabs, implementing the `host.*` primitives the way a browser must: change its
    /// state, report the new snapshot, then answer.
    /// window id -> (type, [(tab id, url, active)])
    type FakeWindows = Vec<(i64, String, Vec<(i64, String, bool)>)>;

    #[derive(Default)]
    struct FakeBrowser {
        dispatcher: RefCell<Option<Dispatcher>>,
        windows: RefCell<FakeWindows>,
        focused: RefCell<i64>,
        next_id: RefCell<i64>,
        events: RefCell<Vec<(String, String, Vec<Value>)>>,
        primitives: RefCell<Vec<(String, Value)>>,
        storage: RefCell<std::collections::HashMap<(String, String), String>>,
        /// What injected code evaluates to, per frame.
        inject_result: RefCell<Value>,
        deliveries: RefCell<Vec<(u64, Value)>>,
    }

    impl FakeBrowser {
        fn fresh_id(&self) -> i64 {
            *self.next_id.borrow_mut() += 1;
            *self.next_id.borrow() + 100
        }

        fn snapshot(&self) -> Value {
            let focused = *self.focused.borrow();
            json!({ "windows": self.windows.borrow().iter().map(|(id, kind, tabs)| json!({
                "id": id, "type": kind, "focused": *id == focused,
                "tabs": tabs.iter().map(|(t, url, active)| json!({ "id": t, "url": url, "title": format!("title of {url}"), "active": active })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>() })
        }

        fn push(&self) {
            let d = self.dispatcher.borrow().clone().unwrap();
            assert_eq!(d.host_event("browserState", &self.snapshot()), Value::Null);
        }

        fn activate(&self, tab: i64) {
            for (_, _, tabs) in self.windows.borrow_mut().iter_mut() {
                if tabs.iter().any(|(t, _, _)| *t == tab) {
                    for entry in tabs.iter_mut() {
                        entry.2 = entry.0 == tab;
                    }
                }
            }
        }

        fn navigate(&self, tab: i64, url: &str) {
            for (_, _, tabs) in self.windows.borrow_mut().iter_mut() {
                for entry in tabs.iter_mut().filter(|e| e.0 == tab) {
                    entry.1 = url.into();
                }
            }
            self.push();
        }

        fn events_named(&self, name: &str) -> Vec<Vec<Value>> {
            self.events.borrow().iter().filter(|(_, n, _)| n == name).map(|(_, _, a)| a.clone()).collect()
        }
    }

    impl ExtensionHost for FakeBrowser {
        fn read_storage(&self, id: &str, key: &str) -> Option<String> {
            self.storage.borrow().get(&(id.into(), key.into())).cloned()
        }
        fn write_storage(&self, id: &str, key: &str, json: &str) {
            self.storage.borrow_mut().insert((id.into(), key.into()), json.into());
        }
        fn now_ms(&self) -> i64 {
            0
        }
        fn ui_locale(&self) -> String {
            "en-US".into()
        }
        fn dispatch_event(&self, id: &str, event: &str, args: Vec<Value>) {
            self.events.borrow_mut().push((id.into(), event.into(), args));
        }
        fn deliver_to_context(&self, context: u64, envelope: Value) {
            self.deliveries.borrow_mut().push((context, envelope));
        }
        fn request_permissions(&self, _: &str, _: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
            reply(false)
        }
        fn call_browser(&self, call: &ApiCall, reply: Reply) {
            let a = call.arg(0).clone();
            self.primitives.borrow_mut().push((call.function.clone(), a.clone()));
            let result = match call.function.as_str() {
                "host.tabs.create" => {
                    let id = self.fresh_id();
                    let window = a["windowId"].as_i64().unwrap_or(*self.focused.borrow());
                    if a["active"] == json!(true) {
                        for (w, _, tabs) in self.windows.borrow_mut().iter_mut().filter(|(w, _, _)| *w == window) {
                            let _ = w;
                            tabs.iter_mut().for_each(|t| t.2 = false);
                        }
                    }
                    for (_, _, tabs) in self.windows.borrow_mut().iter_mut().filter(|(w, _, _)| *w == window) {
                        tabs.push((id, a["url"].as_str().unwrap_or("about:blank").into(), a["active"] == json!(true)));
                    }
                    self.push();
                    json!({ "tabId": id })
                }
                "host.tabs.update" => {
                    let tab = a["tabId"].as_i64().unwrap();
                    if a["active"] == json!(true) {
                        self.activate(tab);
                    }
                    if let Some(url) = a["url"].as_str() {
                        self.navigate(tab, url);
                    }
                    self.push();
                    Value::Null
                }
                "host.tabs.remove" => {
                    let ids: Vec<i64> = a["tabIds"].as_array().unwrap().iter().filter_map(Value::as_i64).collect();
                    for (_, _, tabs) in self.windows.borrow_mut().iter_mut() {
                        tabs.retain(|t| !ids.contains(&t.0));
                        // Like a real tab strip, closing the active tab activates another one.
                        if !tabs.iter().any(|t| t.2)
                            && let Some(first) = tabs.first_mut()
                        {
                            first.2 = true;
                        }
                    }
                    self.push();
                    Value::Null
                }
                "host.windows.create" => {
                    let id = self.fresh_id();
                    let tab = self.fresh_id();
                    let url = a["urls"].get(0).and_then(Value::as_str).unwrap_or("about:newtab").to_string();
                    self.windows.borrow_mut().push((id, a["type"].as_str().unwrap_or("normal").into(), vec![(tab, url, true)]));
                    *self.focused.borrow_mut() = id;
                    self.push();
                    json!({ "windowId": id })
                }
                "host.windows.update" => {
                    if a["focused"] == json!(true) {
                        *self.focused.borrow_mut() = a["windowId"].as_i64().unwrap();
                    }
                    self.push();
                    Value::Null
                }
                "host.tabs.frames" => {
                    json!([{ "frameId": 0, "parentFrameId": -1, "url": self.dispatcher.borrow().as_ref().unwrap().browser.borrow().tab(a["tabId"].as_i64().unwrap()).unwrap().url }])
                }
                "host.scripting.inject" => Value::Array(
                    a["frameIds"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|f| json!({ "frameId": f, "result": self.inject_result.borrow().clone() }))
                        .collect(),
                ),
                _ => Value::Null,
            };
            reply(Ok(result));
        }
    }

    const ID: &str = "abcdefghijklmnopabcdefghijklmnop";

    fn setup(manifest: &str, files: &[(&str, &str)]) -> (Dispatcher, Rc<FakeBrowser>) {
        let host = Rc::new(FakeBrowser::default());
        let d = Dispatcher::new(host.clone(), Registry::builtin());
        *host.dispatcher.borrow_mut() = Some(d.clone());
        let mut f = Files::new();
        for (path, body) in files {
            f.insert(path, body.as_bytes().to_vec());
        }
        let m = crate::manifest::parse_manifest(manifest).manifest.expect("valid manifest");
        d.install(ID, m, InstallOptions { base_url: format!("chrome-extension://{ID}/"), locales: vec![], files: f });
        *host.windows.borrow_mut() =
            vec![(1, "normal".into(), vec![(10, "https://a.test/".into(), true), (11, "https://b.test/".into(), false)])];
        *host.focused.borrow_mut() = 1;
        host.push();
        host.events.borrow_mut().clear();
        (d, host)
    }

    fn call(d: &Dispatcher, function: &str, args: Vec<Value>) -> ApiResult {
        call_as(d, function, args, Caller::background())
    }

    fn call_as(d: &Dispatcher, function: &str, args: Vec<Value>, caller: Caller) -> ApiResult {
        let out: Rc<RefCell<Option<ApiResult>>> = Rc::default();
        let sink = out.clone();
        d.dispatch(
            ApiCall { extension_id: ID.into(), function: function.into(), args, caller },
            Box::new(move |r| *sink.borrow_mut() = Some(r)),
        );
        let result = out.borrow_mut().take();
        result.expect("answered synchronously")
    }

    fn ids(v: &Value) -> Vec<i64> {
        v.as_array().unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
    }

    #[test]
    fn tabs_are_trimmed_by_permission() {
        let (d, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1","host_permissions":["https://b.test/*"]}"#, &[]);
        let all = call(&d, "tabs.query", vec![json!({})]).unwrap();
        assert_eq!(ids(&all), vec![10, 11]);
        assert!(all[0].get("url").is_none(), "no permission for a.test");
        assert_eq!(all[1]["url"], "https://b.test/");
        assert_eq!(ids(&call(&d, "tabs.query", vec![json!({"url": "<all_urls>"})]).unwrap()), vec![11]);
        assert_eq!(call(&d, "tabs.get", vec![json!(99)]).unwrap_err().message, "No tab with id: 99.");

        let (with_tabs, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["tabs"]}"#, &[]);
        assert_eq!(call(&with_tabs, "tabs.get", vec![json!(10)]).unwrap()["title"], "title of https://a.test/");
    }

    #[test]
    fn creating_and_closing_tabs_goes_through_the_browser() {
        let (d, b) = setup(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["tabs"]}"#, &[("page.html", "")]);
        let tab = call(&d, "tabs.create", vec![json!({"url": "page.html"})]).unwrap();
        assert_eq!(tab["url"], format!("chrome-extension://{ID}/page.html"), "relative URLs resolve against the extension");
        assert_eq!(tab["active"], true);
        let created = b.events_named("tabs.onCreated");
        assert_eq!(created.len(), 1);
        assert_eq!(b.events_named("tabs.onActivated")[0][0]["previousTabId"], 10);

        let id = tab["id"].as_i64().unwrap();
        let updated = call(&d, "tabs.update", vec![json!(id), json!({"url": "https://c.test/"})]).unwrap();
        assert_eq!(updated["url"], "https://c.test/");
        let changes: Vec<Value> = b.events_named("tabs.onUpdated").iter().map(|a| a[1].clone()).collect();
        assert!(changes.iter().any(|c| c["url"] == "https://c.test/"));

        call(&d, "tabs.remove", vec![json!([id])]).unwrap();
        assert_eq!(b.events_named("tabs.onRemoved")[0][1], json!({ "windowId": 1, "isWindowClosing": false }));
        assert!(call(&d, "tabs.create", vec![json!({"url": "javascript:alert(1)"})]).is_err());
        // update without a tab id acts on the active tab.
        call(&d, "tabs.update", vec![json!({"url": "https://d.test/"})]).unwrap();
        assert_eq!(b.primitives.borrow().last().unwrap().1["tabId"], 10);
    }

    #[test]
    fn windows() {
        let (d, b) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#, &[("popup.html", "")]);
        let w = call(&d, "windows.create", vec![json!({"url": "popup.html", "type": "popup", "width": 400, "height": 300})]).unwrap();
        assert_eq!(w["type"], "popup");
        assert_eq!(w["tabs"].as_array().unwrap().len(), 1);
        assert_eq!(b.events_named("windows.onCreated").len(), 1);
        assert_eq!(b.events_named("windows.onFocusChanged")[0][0], w["id"]);
        let all = call(&d, "windows.getAll", vec![json!({"populate": true, "windowTypes": ["normal"]})]).unwrap();
        assert_eq!(ids(&all), vec![1]);
        assert_eq!(call(&d, "windows.getLastFocused", vec![]).unwrap()["id"], w["id"]);
        assert!(call(&d, "windows.create", vec![json!({"state": "maximized", "width": 3})]).is_err());
    }

    #[test]
    fn current_window_follows_the_caller() {
        let (d, b) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#, &[]);
        b.windows.borrow_mut().push((2, "normal".into(), vec![(20, "https://c.test/".into(), true)]));
        b.push();
        let from_tab = Caller { context: ContextKind::ExtensionPage, context_id: None, tab_id: Some(20), frame_id: None, url: None };
        assert_eq!(
            ids(&call_as(&d, "tabs.query", vec![json!({"active": true, "currentWindow": true})], from_tab.clone()).unwrap()),
            vec![20]
        );
        assert_eq!(ids(&call(&d, "tabs.query", vec![json!({"active": true, "currentWindow": true})]).unwrap()), vec![10]);
        assert_eq!(call_as(&d, "tabs.getCurrent", vec![], from_tab).unwrap()["id"], 20);
        assert_eq!(call(&d, "tabs.getCurrent", vec![]).unwrap(), Value::Null);
    }

    #[test]
    fn active_tab_is_granted_by_the_toolbar_button_and_ends_with_the_origin() {
        let (d, b) = setup(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["activeTab","scripting"],"action":{}}"#, &[]);
        *b.inject_result.borrow_mut() = json!("hello");
        let run = |d: &Dispatcher| {
            call(d, "scripting.executeScript", vec![json!({"target": {"tabId": 10}, "__funcSource": "() => document.title"})])
        };
        assert_eq!(run(&d).unwrap_err().message, CANNOT_ACCESS);

        assert_eq!(d.host_event("actionClicked", &json!({"extensionId": ID, "tabId": 10})), json!({"clicked": true}));
        assert_eq!(b.events_named("action.onClicked")[0][0]["url"], "https://a.test/", "the tab is visible to the click listener");
        assert_eq!(run(&d).unwrap(), json!([{ "frameId": 0, "result": "hello" }]));
        let injected = b.primitives.borrow().iter().rev().find(|(n, _)| n == "host.scripting.inject").unwrap().1.clone();
        assert_eq!(injected["world"], "ISOLATED");
        assert!(injected["code"].as_str().unwrap().contains("(() => document.title)()"));
        assert!(injected["code"].as_str().unwrap().contains(injected["binding"].as_str().unwrap()));

        b.navigate(10, "https://a.test/other");
        assert!(run(&d).is_ok(), "same origin keeps the grant");
        b.navigate(10, "https://elsewhere.test/");
        assert_eq!(run(&d).unwrap_err().message, CANNOT_ACCESS);
    }

    #[test]
    fn injection_targets_and_firefox_results() {
        let (d, b) = setup(r#"{"manifest_version":2,"name":"x","version":"1","permissions":["<all_urls>"]}"#, &[("inject.js", "1 + 1")]);
        *b.inject_result.borrow_mut() = json!(2);
        assert_eq!(call(&d, "tabs.executeScript", vec![json!({"file": "inject.js"})]).unwrap(), json!([2]));
        assert_eq!(b.primitives.borrow().last().unwrap().1["tabId"], 10, "no tab id: the active tab");
        assert_eq!(
            call(&d, "tabs.executeScript", vec![json!(11), json!({"code": "x", "frameId": 7})]).unwrap_err().message,
            "No frame with id 7 in tab with id 11"
        );
        assert!(call(&d, "tabs.executeScript", vec![json!({"file": "missing.js"})]).unwrap_err().message.contains("Could not load file"));
        call(&d, "tabs.insertCSS", vec![json!(11), json!({"code": "body{}"})]).unwrap();
        assert_eq!(b.primitives.borrow().last().unwrap().1["kind"], "css");
    }

    #[test]
    fn toolbar_button_state() {
        let (d, b) = setup(
            r#"{"manifest_version":3,"name":"Ext","version":"1","action":{"default_title":"Hi","default_popup":"popup.html","default_icon":{"16":"i16.png"}},"options_page":"opt.html"}"#,
            &[("popup.html", ""), ("other.html", ""), ("i16.png", ""), ("opt.html", "")],
        );
        call(&d, "action.setBadgeText", vec![json!({"text": "3"})]).unwrap();
        call(&d, "action.setBadgeText", vec![json!({"text": "T", "tabId": 11})]).unwrap();
        call(&d, "action.setBadgeBackgroundColor", vec![json!({"color": "#00ff00"})]).unwrap();
        assert_eq!(call(&d, "action.getBadgeText", vec![json!({"tabId": 10})]).unwrap(), "3");
        assert_eq!(call(&d, "action.getBadgeText", vec![json!({"tabId": 11})]).unwrap(), "T");
        assert_eq!(call(&d, "action.getBadgeBackgroundColor", vec![json!({})]).unwrap(), json!([0, 255, 0, 255]));
        assert!(b.events_named("__ui.changed").len() >= 3, "the browser is told to redraw");

        let ui = d.host_event("uiState", &json!({"tabId": 11}));
        let a = &ui[0]["action"];
        assert_eq!((a["badgeText"].as_str(), a["title"].as_str()), (Some("T"), Some("Hi")));
        assert_eq!(a["popupUrl"], format!("chrome-extension://{ID}/popup.html"));
        assert_eq!(a["icon"]["paths"][0]["path"], "i16.png");
        assert_eq!(ui[0]["optionsUrl"], format!("chrome-extension://{ID}/opt.html"));

        assert!(call(&d, "action.setPopup", vec![json!({"popup": "nope.html"})]).is_err());
        call(&d, "action.setPopup", vec![json!({"popup": "other.html", "tabId": 10})]).unwrap();
        assert_eq!(
            d.host_event("actionClicked", &json!({"extensionId": ID, "tabId": 10})),
            json!({"popup": format!("chrome-extension://{ID}/other.html")})
        );
        call(&d, "action.disable", vec![json!(10)]).unwrap();
        assert_eq!(call(&d, "action.isEnabled", vec![json!(10)]).unwrap(), false);
        assert_eq!(d.host_event("actionClicked", &json!({"extensionId": ID, "tabId": 10})), Value::Null);

        // Per-tab state goes with the tab.
        b.windows.borrow_mut()[0].2.retain(|t| t.0 != 11);
        b.push();
        assert_eq!(d.host_event("uiState", &json!({"tabId": 10}))[0]["action"]["badgeText"], "3");
        assert!(call(&d, "action.setIcon", vec![json!({"path": "missing.png"})]).is_err());
    }

    #[test]
    fn page_actions_start_hidden() {
        let (d, _) = setup(r#"{"manifest_version":2,"name":"x","version":"1","page_action":{"default_title":"P"}}"#, &[]);
        assert_eq!(call(&d, "pageAction.isShown", vec![json!({"tabId": 10})]).unwrap(), false);
        call(&d, "pageAction.show", vec![json!(10)]).unwrap();
        assert_eq!(call(&d, "pageAction.isShown", vec![json!({"tabId": 10})]).unwrap(), true);
        assert_eq!(call(&d, "pageAction.isShown", vec![json!({"tabId": 11})]).unwrap(), false);
    }

    #[test]
    fn context_menus_from_the_browser() {
        let (d, b) = setup(
            r#"{"manifest_version":3,"name":"Menus","version":"1","permissions":["contextMenus"],"background":{"service_worker":"sw.js"}}"#,
            &[("sw.js", "")],
        );
        assert!(
            call(&d, "contextMenus.create", vec![json!({"title": "no id", "__generatedId": true})])
                .unwrap_err()
                .message
                .contains("must pass an id")
        );
        call(&d, "contextMenus.create", vec![json!({"id": "sel", "title": "Search \"%s\"", "contexts": ["selection"]})]).unwrap();
        call(&d, "contextMenus.create", vec![json!({"id": "chk", "title": "Enabled", "type": "checkbox", "contexts": ["all"]})]).unwrap();
        let menu = d.host_event("contextMenuItems", &json!({"tabId": 10, "pageUrl": "https://a.test/", "selectionText": "ladybird"}));
        assert_eq!(menu[0]["name"], "Menus");
        assert_eq!(menu[0]["items"][0]["title"], "Search \"ladybird\"");
        assert_eq!(menu[0]["items"].as_array().unwrap().len(), 2);

        d.host_event("contextMenuClicked", &json!({"extensionId": ID, "itemId": "\"chk\"", "tabId": 10, "pageUrl": "https://a.test/"}));
        let clicked = b.events_named("contextMenus.onClicked");
        assert_eq!(clicked[0][0]["menuItemId"], "chk");
        assert_eq!((clicked[0][0]["wasChecked"].as_bool(), clicked[0][0]["checked"].as_bool()), (Some(false), Some(true)));
        assert_eq!(clicked[0][1]["id"], 10);
        // Service workers' menus outlive them.
        assert!(b.storage.borrow().contains_key(&(ID.to_string(), "menus".to_string())));
    }

    #[test]
    fn keyboard_commands() {
        let (d, b) = setup(
            r#"{"manifest_version":3,"name":"x","version":"1","action":{"default_popup":"p.html"},
                "commands":{"toggle":{"suggested_key":{"default":"Ctrl+Shift+Y"},"description":"Toggle"},"_execute_action":{"suggested_key":{"default":"Alt+P"}}}}"#,
            &[("p.html", "")],
        );
        let all = call(&d, "commands.getAll", vec![]).unwrap();
        let toggle = all.as_array().unwrap().iter().find(|c| c["name"] == "toggle").unwrap();
        assert_eq!(toggle["shortcut"], "Ctrl+Shift+Y");
        d.host_event("command", &json!({"extensionId": ID, "name": "toggle", "tabId": 10}));
        assert_eq!(b.events_named("commands.onCommand")[0][0], "toggle");
        assert_eq!(
            d.host_event("command", &json!({"extensionId": ID, "name": "_execute_action", "tabId": 10}))["popup"],
            format!("chrome-extension://{ID}/p.html")
        );
        assert_eq!(
            d.host_event("setShortcut", &json!({"extensionId": ID, "name": "toggle", "shortcut": "Alt+P"}))["error"]
                .as_str()
                .map(|e| e.contains("already used")),
            Some(true)
        );
        assert_eq!(
            d.host_event("setShortcut", &json!({"extensionId": ID, "name": "toggle", "shortcut": "ctrl+alt+t"})),
            json!({"ok": true})
        );
        assert_eq!(d.host_event("commands", &json!({}))[0]["shortcut"], "Ctrl+Alt+T");
    }

    #[test]
    fn registered_content_scripts_run_and_persist() {
        let (d, b) = setup(
            r#"{"manifest_version":3,"name":"x","version":"1","permissions":["scripting"],"host_permissions":["<all_urls>"]}"#,
            &[("cs.js", "/* cs */")],
        );
        call(
            &d,
            "scripting.registerContentScripts",
            vec![json!([{"id": "one", "matches": ["https://a.test/*"], "js": ["cs.js"], "runAt": "document_start"}])],
        )
        .unwrap();
        assert!(
            call(&d, "scripting.registerContentScripts", vec![json!([{"id": "one", "matches": ["https://a.test/*"], "js": ["cs.js"]}])])
                .is_err()
        );
        assert!(
            call(&d, "scripting.registerContentScripts", vec![json!([{"id": "two", "matches": ["https://a.test/*"], "js": ["nope.js"]}])])
                .is_err()
        );
        let programs = d.content_script_programs("https://a.test/page", true, None);
        assert!(programs[0]["program"].as_str().unwrap().contains("/* cs */"));
        assert_eq!(call(&d, "scripting.getRegisteredContentScripts", vec![]).unwrap()[0]["id"], "one");
        assert!(b.storage.borrow().contains_key(&(ID.to_string(), "scripts".to_string())));
        call(&d, "scripting.unregisterContentScripts", vec![json!({"ids": ["one"]})]).unwrap();
        assert_eq!(d.content_script_programs("https://a.test/page", true, None), json!([]));
    }

    #[test]
    fn options_page_opens_once() {
        let (d, b) = setup(r#"{"manifest_version":3,"name":"x","version":"1","options_page":"opt.html"}"#, &[("opt.html", "")]);
        call(&d, "runtime.openOptionsPage", vec![]).unwrap();
        let opened: Vec<_> = b.primitives.borrow().iter().filter(|(n, _)| n == "host.tabs.create").cloned().collect();
        assert_eq!(opened.len(), 1);
        call(&d, "runtime.openOptionsPage", vec![]).unwrap();
        assert_eq!(b.primitives.borrow().iter().filter(|(n, _)| n == "host.tabs.create").count(), 1, "an open options page is focused");
        let (none, _) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#, &[]);
        assert_eq!(call(&none, "runtime.openOptionsPage", vec![]).unwrap_err().message, "Could not create an options page.");
    }

    #[test]
    fn uninstall_url_opens_after_removal() {
        let (d, b) = setup(r#"{"manifest_version":3,"name":"x","version":"1"}"#, &[]);
        assert!(call(&d, "runtime.setUninstallURL", vec![json!("ftp://x")]).is_err());
        call(&d, "runtime.setUninstallURL", vec![json!("https://bye.test/")]).unwrap();
        d.uninstall(ID);
        assert_eq!(
            b.primitives.borrow().last().unwrap(),
            &("host.tabs.create".to_string(), json!({"url": "https://bye.test/", "active": true}))
        );
    }

    #[test]
    fn message_senders_carry_the_tab_from_the_snapshot() {
        // Dark Reader reads `port.sender.tab.url` when its content script connects.
        let (d, b) = setup(r#"{"manifest_version":2,"name":"x","version":"1","permissions":["<all_urls>"]}"#, &[]);
        let ctx = |id, kind, tab| crate::bus::ContextInfo {
            id,
            extension_id: ID.into(),
            kind,
            tab_id: tab,
            frame_id: Some(0),
            url: Some("https://a.test/".into()),
        };
        d.context_created(ctx(1, ContextKind::Background, None)).unwrap();
        d.context_created(ctx(2, ContextKind::ContentScript, Some(10))).unwrap();
        let from_tab = Caller { context: ContextKind::ContentScript, context_id: Some(2), tab_id: Some(10), frame_id: Some(0), url: None };
        call_as(&d, "runtime.connect", vec![json!({"name": "p"}), json!("2.1")], from_tab).unwrap();
        let connect = b
            .deliveries
            .borrow()
            .iter()
            .find(|(c, e)| *c == 1 && e["type"] == "connect")
            .map(|(_, e)| e.clone())
            .expect("background got the port");
        assert_eq!(connect["sender"]["tab"]["url"], "https://a.test/");
        assert_eq!(connect["sender"]["tab"]["windowId"], 1);

        // The page that just started in the tab is the tab's URL, before the browser reports the navigation.
        d.context_created(crate::bus::ContextInfo {
            url: Some("https://new.test/".into()),
            ..ctx(3, ContextKind::ContentScript, Some(10))
        })
        .unwrap();
        let from_new = Caller { context: ContextKind::ContentScript, context_id: Some(3), tab_id: Some(10), frame_id: Some(0), url: None };
        call_as(&d, "runtime.connect", vec![json!({"name": "q"}), json!("3.1")], from_new).unwrap();
        let connect = b.deliveries.borrow().iter().rev().find(|(c, e)| *c == 1 && e["type"] == "connect").map(|(_, e)| e.clone()).unwrap();
        assert_eq!(connect["sender"]["tab"]["url"], "https://new.test/");
    }

    #[test]
    fn firefox_content_scripts_register() {
        let (d, _) = setup(r#"{"manifest_version":2,"name":"x","version":"1","permissions":["<all_urls>"]}"#, &[("a.js", "/* file */")]);
        let r = call(
            &d,
            "contentScripts.register",
            vec![json!({"matches": ["https://a.test/*"], "js": [{"file": "a.js"}, {"code": "/* inline */"}], "runAt": "document_start"})],
        )
        .unwrap();
        let program = d.content_script_programs("https://a.test/x", true, None)[0]["program"].as_str().unwrap().to_string();
        assert!(program.contains("/* file */") && program.contains("/* inline */"));
        assert!(
            call(&d, "contentScripts.register", vec![json!({"matches": ["https://a.test/*"], "js": [{"file": "missing.js"}]})]).is_err()
        );
        call(&d, "__contentScripts.unregister", vec![r["id"].clone()]).unwrap();
        assert_eq!(d.content_script_programs("https://a.test/x", true, None), json!([]));
    }

    #[test]
    fn management_self_needs_no_permission() {
        let (d, _) = setup(r#"{"manifest_version":3,"name":"Me","version":"2.0"}"#, &[]);
        assert_eq!(call(&d, "management.getSelf", vec![]).unwrap()["version"], "2.0");
        assert_eq!(call(&d, "management.getAll", vec![]).unwrap_err().kind, ErrorKind::PermissionDenied);
        assert!(d.surface_for(ID).unwrap()["management"]["functions"].as_array().unwrap().iter().all(|f| f == "getSelf"));
    }
}
