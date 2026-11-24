//! Platform, UI-surface, productivity, and settings extension APIs:
//! `publicSuffix`, `notifications`, `idle`, `omnibox`, `search`, `tabGroups` (+ `tabs.group`/`ungroup`),
//! `userScripts`, `offscreen`, `sidePanel`, `sidebarAction`, `theme`, `power`, `contextualIdentities`,
//! `find`, `system.*`, `fontSettings`, `privacy`/`browserSettings` settings store, `permissions` host-access requests,
//! `contextMenus` rest, and `commands.openShortcutSettings`.

use crate::dispatch::Dispatcher;
use crate::host::*;
use crate::manifest::{ContentScript, RunAt, ScriptWorld};
use crate::match_pattern::{MatchDialect, MatchPattern, split_url};
use crate::registry::Member;
use crate::ui;
use psl::Psl;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub const TAB_GROUP_ID_NONE: i64 = -1;

pub fn handles(full_name: &str) -> bool {
    let ns = full_name.split('.').next().unwrap_or("");
    matches!(
        ns,
        "publicSuffix"
            | "notifications"
            | "idle"
            | "omnibox"
            | "search"
            | "tabGroups"
            | "userScripts"
            | "offscreen"
            | "sidePanel"
            | "sidebarAction"
            | "theme"
            | "power"
            | "contextualIdentities"
            | "find"
            | "system"
            | "fontSettings"
            | "identity"
            | "tts"
            | "clipboard"
            | "dns"
            | "captivePortal"
    ) || matches!(
        full_name,
        "tabs.group"
            | "tabs.ungroup"
            | "tabs.getZoomSettings"
            | "tabs.setZoomSettings"
            | "tabs.hide"
            | "tabs.show"
            | "tabs.captureTab"
            | "tabs.print"
            | "tabs.printPreview"
            | "tabs.saveAsPDF"
            | "tabs.toggleReaderMode"
            | "tabs.warmup"
            | "tabs.moveInSuccession"
            | "permissions.addHostAccessRequest"
            | "permissions.removeHostAccessRequest"
            | "contextMenus.overrideContext"
            | "contextMenus.getTargetElement"
            | "commands.openShortcutSettings"
            | "runtime.sendNativeMessage"
            | "runtime.requestUpdateCheck"
            | "runtime.restart"
    )
}

// ---- PlatformState owned by Dispatcher -----------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct PlatformState {
    pub notifications: NotificationStore,
    pub idle: IdleState,
    pub omnibox: OmniboxState,
    pub tab_groups: TabGroupStore,
    pub user_scripts: UserScriptStore,
    pub offscreen: HashMap<String, OffscreenDoc>,
    pub side_panels: HashMap<String, SidePanelState>,
    pub sidebars: HashMap<String, SidebarActionState>,
    pub themes: ThemeStore,
    pub power_levels: HashMap<String, String>,
    pub containers: ContainerStore,
    pub fonts: FontStore,
    pub settings: SettingStore,
    pub host_access_requests: HashMap<String, Vec<Value>>,
    pub target_elements: HashMap<i64, Value>,
    pub tts_speaking: bool,
    pub tts_paused: bool,
    pub auth_tokens: HashMap<String, Vec<String>>,
    pub zoom_settings: HashMap<i64, Value>,
    pub hidden_tabs: HashSet<i64>,
    pub tab_successors: HashMap<i64, i64>,
    pub captive_portal_state: String,
    pub captive_portal_last_checked: i64,
}

impl PlatformState {
    pub fn drop_extension(&mut self, id: &str) {
        self.notifications.drop_extension(id);
        self.idle.intervals.remove(id);
        self.idle.last_reported.remove(id);
        self.omnibox.default_suggestions.remove(id);
        self.user_scripts.scripts.remove(id);
        self.user_scripts.worlds.remove(id);
        self.offscreen.remove(id);
        self.side_panels.remove(id);
        self.sidebars.remove(id);
        self.themes.drop_extension(id);
        self.power_levels.remove(id);
        self.settings.drop_extension(id);
        self.host_access_requests.remove(id);
    }

    pub fn forget_tab(&mut self, tab_id: i64) {
        self.tab_groups.tab_groups.remove(&tab_id);
        for sp in self.side_panels.values_mut() {
            sp.per_tab.remove(&tab_id);
        }
        for sb in self.sidebars.values_mut() {
            sb.per_tab.remove(&tab_id);
        }
    }

    pub fn effective_power_level(&self) -> Option<&'static str> {
        if self.power_levels.values().any(|l| l == "display") {
            Some("display")
        } else if self.power_levels.values().any(|l| l == "system") {
            Some("system")
        } else {
            None
        }
    }
}

// ---- Notifications -------------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct NotificationStore {
    pub active: HashMap<(String, String), Value>,
    pub permission_level: String,
    next_id: u64,
}

impl Default for NotificationStore {
    fn default() -> Self {
        NotificationStore { active: HashMap::new(), permission_level: "granted".into(), next_id: 1 }
    }
}

impl NotificationStore {
    fn fresh_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        format!("notif-{id}")
    }

    fn drop_extension(&mut self, ext_id: &str) {
        self.active.retain(|(e, _), _| e != ext_id);
    }
}

// ---- Idle ----------------------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct IdleState {
    pub idle_seconds: u64,
    pub locked: bool,
    pub intervals: HashMap<String, u64>,
    pub last_reported: HashMap<String, String>,
}

impl IdleState {
    pub fn state_for(&self, interval: u64) -> &'static str {
        if self.locked {
            "locked"
        } else if self.idle_seconds >= interval {
            "idle"
        } else {
            "active"
        }
    }
}

// ---- Omnibox -------------------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct OmniboxState {
    pub default_suggestions: HashMap<String, String>,
    pub last_suggestions: HashMap<String, Vec<Value>>,
}

// ---- Tab Groups ----------------------------------------------------------------------------------------------------

const VALID_GROUP_COLORS: &[&str] = &["grey", "blue", "red", "yellow", "green", "pink", "purple", "cyan", "orange"];

#[derive(Debug, Clone)]
pub struct TabGroup {
    pub id: i64,
    pub window_id: i64,
    pub title: String,
    pub color: String,
    pub collapsed: bool,
}

impl TabGroup {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "windowId": self.window_id,
            "title": self.title,
            "color": self.color,
            "collapsed": self.collapsed,
        })
    }
}

#[derive(Debug)]
pub struct TabGroupStore {
    pub groups: HashMap<i64, TabGroup>,
    pub tab_groups: HashMap<i64, i64>, // tab_id -> group_id
    next_id: i64,
}

impl Default for TabGroupStore {
    fn default() -> Self {
        TabGroupStore { groups: HashMap::new(), tab_groups: HashMap::new(), next_id: 1 }
    }
}

impl TabGroupStore {
    fn fresh_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn prune_empty(&mut self) -> Vec<TabGroup> {
        let used: HashSet<i64> = self.tab_groups.values().copied().collect();
        let empty_ids: Vec<i64> = self.groups.keys().copied().filter(|id| !used.contains(id)).collect();
        empty_ids.into_iter().filter_map(|id| self.groups.remove(&id)).collect()
    }
}

// ---- User Scripts --------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RegisteredUserScript {
    pub id: String,
    pub script: ContentScript,
    pub inline_code: Vec<String>,
    pub world_id: Option<String>,
    pub world_name: String, // "USER_SCRIPT" or "MAIN"
}

impl RegisteredUserScript {
    pub fn to_json(&self) -> Value {
        let mut js: Vec<Value> = self.script.js.iter().map(|f| json!({ "file": f })).collect();
        for code in &self.inline_code {
            js.push(json!({ "code": code }));
        }
        let mut obj = json!({
            "id": self.id,
            "matches": self.script.matches,
            "excludeMatches": self.script.exclude_matches,
            "includeGlobs": self.script.include_globs,
            "excludeGlobs": self.script.exclude_globs,
            "js": js,
            "allFrames": self.script.all_frames,
            "runAt": match self.script.run_at {
                RunAt::DocumentStart => "document_start",
                RunAt::DocumentEnd => "document_end",
                RunAt::DocumentIdle => "document_idle",
            },
            "world": self.world_name,
        });
        if let Some(wid) = &self.world_id {
            obj["worldId"] = json!(wid);
        }
        obj
    }
}

#[derive(Debug, Clone, Default)]
pub struct UserScriptWorldConfig {
    pub world_id: String,
    pub csp: Option<String>,
    pub messaging: bool,
}

impl UserScriptWorldConfig {
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        if !self.world_id.is_empty() {
            o.insert("worldId".into(), json!(self.world_id));
        }
        if let Some(c) = &self.csp {
            o.insert("csp".into(), json!(c));
        }
        o.insert("messaging".into(), json!(self.messaging));
        Value::Object(o)
    }
}

#[derive(Debug, Default)]
pub struct UserScriptStore {
    pub scripts: HashMap<String, Vec<RegisteredUserScript>>,
    pub worlds: HashMap<String, HashMap<String, UserScriptWorldConfig>>,
}

// ---- Offscreen -----------------------------------------------------------------------------------------------------

const VALID_OFFSCREEN_REASONS: &[&str] = &[
    "TESTING",
    "AUDIO_PLAYBACK",
    "IFRAME_SCRIPTING",
    "DOM_SCRAPING",
    "BLOBS",
    "DOM_PARSER",
    "USER_MEDIA",
    "DISPLAY_MEDIA",
    "WEB_RTC",
    "CLIPBOARD",
    "LOCAL_STORAGE",
    "WORKERS",
    "BATTERY_STATUS",
    "MATCH_MEDIA",
    "GEOLOCATION",
];

#[derive(Debug, Clone)]
pub struct OffscreenDoc {
    pub url: String,
    pub reasons: Vec<String>,
    pub justification: String,
}

// ---- SidePanel & SidebarAction -------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SidePanelOptions {
    pub path: Option<String>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct SidePanelState {
    pub global: SidePanelOptions,
    pub per_tab: HashMap<i64, SidePanelOptions>,
    pub open_panel_on_action_click: bool,
    pub side: String,
}

#[derive(Debug, Clone, Default)]
pub struct SidebarProps {
    pub title: Option<String>,
    pub panel: Option<String>,
    pub icon: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct SidebarActionState {
    pub global: SidebarProps,
    pub per_tab: HashMap<i64, SidebarProps>,
    pub open_windows: HashSet<i64>,
}

// ---- Theme ---------------------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ThemeStore {
    pub global: Option<(String, Value)>,
    pub per_window: HashMap<i64, (String, Value)>,
}

impl ThemeStore {
    fn drop_extension(&mut self, ext_id: &str) {
        if self.global.as_ref().is_some_and(|(e, _)| e == ext_id) {
            self.global = None;
        }
        self.per_window.retain(|_, (e, _)| e != ext_id);
    }
}

// ---- Contextual Identities (Firefox Containers) --------------------------------------------------------------------

const CONTAINER_COLORS: &[(&str, &str)] = &[
    ("blue", "#37adff"),
    ("turquoise", "#00c79a"),
    ("green", "#51cd00"),
    ("yellow", "#ffcb00"),
    ("orange", "#ff9f00"),
    ("red", "#ff613d"),
    ("pink", "#ff4bda"),
    ("purple", "#af51f5"),
    ("toolbar", "#737373"),
];

const CONTAINER_ICONS: &[&str] =
    &["fingerprint", "briefcase", "dollar", "cart", "circle", "gift", "vacation", "food", "fruit", "pet", "tree", "chill", "fence"];

#[derive(Debug, Clone)]
pub struct Container {
    pub cookie_store_id: String,
    pub name: String,
    pub color: String,
    pub icon: String,
}

impl Container {
    pub fn to_json(&self) -> Value {
        let color_code = CONTAINER_COLORS.iter().find(|(c, _)| *c == self.color).map(|(_, hex)| *hex).unwrap_or("#37adff");
        json!({
            "cookieStoreId": self.cookie_store_id,
            "name": self.name,
            "color": self.color,
            "colorCode": color_code,
            "icon": self.icon,
            "iconUrl": format!("resource://usercontext-content/{}.svg", self.icon),
        })
    }
}

#[derive(Debug)]
pub struct ContainerStore {
    pub items: Vec<Container>,
    next_id: u64,
}

impl Default for ContainerStore {
    fn default() -> Self {
        ContainerStore {
            items: vec![
                Container {
                    cookie_store_id: "firefox-container-1".into(),
                    name: "Personal".into(),
                    color: "blue".into(),
                    icon: "fingerprint".into(),
                },
                Container {
                    cookie_store_id: "firefox-container-2".into(),
                    name: "Work".into(),
                    color: "orange".into(),
                    icon: "briefcase".into(),
                },
                Container {
                    cookie_store_id: "firefox-container-3".into(),
                    name: "Banking".into(),
                    color: "green".into(),
                    icon: "dollar".into(),
                },
                Container {
                    cookie_store_id: "firefox-container-4".into(),
                    name: "Shopping".into(),
                    color: "pink".into(),
                    icon: "cart".into(),
                },
            ],
            next_id: 5,
        }
    }
}

// ---- Font Settings -------------------------------------------------------------------------------------------------

#[derive(Debug)]
pub struct FontStore {
    pub fonts: HashMap<(String, String), String>, // (script, genericFamily) -> fontId
    pub default_font_size: i64,
    pub default_fixed_font_size: i64,
    pub minimum_font_size: i64,
}

impl Default for FontStore {
    fn default() -> Self {
        FontStore { fonts: HashMap::new(), default_font_size: 16, default_fixed_font_size: 13, minimum_font_size: 0 }
    }
}

// ---- BrowserSetting Store (privacy & browserSettings) --------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SettingStore {
    /// setting_key -> Vec<(extension_id, value)> in set order; last entry wins.
    overrides: HashMap<String, Vec<(String, Value)>>,
}

impl SettingStore {
    fn default_value(key: &str) -> Value {
        match key {
            // privacy.network
            "privacy.network.networkPredictionEnabled" => json!(true),
            "privacy.network.webRTCIPHandlingPolicy" => json!("default"),
            "privacy.network.peerConnectionEnabled" => json!(true),
            "privacy.network.httpsOnlyMode" => json!("never"),
            "privacy.network.globalPrivacyControl" => json!(false),
            // privacy.services
            "privacy.services.autofillEnabled"
            | "privacy.services.autofillAddressEnabled"
            | "privacy.services.autofillCreditCardEnabled"
            | "privacy.services.passwordSavingEnabled"
            | "privacy.services.safeBrowsingEnabled"
            | "privacy.services.searchSuggestEnabled"
            | "privacy.services.spellingServiceEnabled"
            | "privacy.services.translationServiceEnabled" => json!(true),
            // privacy.websites
            "privacy.websites.thirdPartyCookiesAllowed"
            | "privacy.websites.hyperlinkAuditingEnabled"
            | "privacy.websites.referrersEnabled"
            | "privacy.websites.protectedContentEnabled" => json!(true),
            "privacy.websites.doNotTrackEnabled" | "privacy.websites.resistFingerprinting" | "privacy.websites.firstPartyIsolate" => {
                json!(false)
            }
            "privacy.websites.cookieConfig" => json!({ "behavior": "allow_all", "nonPersistentCookies": false }),
            "privacy.websites.trackingProtectionMode" => json!("private_browsing"),
            // browserSettings
            "browserSettings.allowPopupsForUserEvents"
            | "browserSettings.cacheEnabled"
            | "browserSettings.useDocumentFonts"
            | "browserSettings.zoomFullPage"
            | "browserSettings.zoomSiteSpecific" => json!(true),
            "browserSettings.closeTabsByDoubleClick"
            | "browserSettings.ftpProtocolEnabled"
            | "browserSettings.openBookmarksInNewTabs"
            | "browserSettings.openSearchResultsInNewTabs"
            | "browserSettings.openUrlbarResultsInNewTabs"
            | "browserSettings.verticalTabs"
            | "browserSettings.webNotificationsDisabled" => json!(false),
            "browserSettings.colorManagement.mode" => json!("tagged_only"),
            "browserSettings.colorManagement.useNativeSRGB" => json!(false),
            "browserSettings.colorManagement.useWebRenderCompositor" => json!(false),
            "browserSettings.contextMenuShowEvent" => json!("mouseup"),
            "browserSettings.homepageOverride" => json!("about:home"),
            "browserSettings.imageAnimationBehavior" => json!("normal"),
            "browserSettings.newTabPageOverride" => json!("about:newtab"),
            "browserSettings.newTabPosition" => json!("afterCurrent"),
            "browserSettings.overrideContentColorScheme" => json!("auto"),
            "browserSettings.overrideDocumentColors" => json!("high-contrast-only"),
            "browserSettings.tlsVersionRestrictionConfig" => json!({ "minimum": "tls1.2", "maximum": "tls1.3" }),
            _ => json!(null),
        }
    }

    pub fn get(&self, key: &str, ext_id: &str) -> Value {
        let stack = self.overrides.get(key);
        match stack.and_then(|s| s.last()) {
            Some((owner, val)) => {
                let level = if owner == ext_id { "controlled_by_this_extension" } else { "controlled_by_other_extensions" };
                json!({ "value": val, "levelOfControl": level })
            }
            None => json!({ "value": Self::default_value(key), "levelOfControl": "controllable_by_this_extension" }),
        }
    }

    pub fn set(&mut self, key: &str, ext_id: &str, value: Value) -> Value {
        let stack = self.overrides.entry(key.to_string()).or_default();
        stack.retain(|(e, _)| e != ext_id);
        stack.push((ext_id.to_string(), value.clone()));
        json!({ "value": value, "levelOfControl": "controlled_by_this_extension" })
    }

    pub fn clear(&mut self, key: &str, ext_id: &str) -> Value {
        if let Some(stack) = self.overrides.get_mut(key) {
            stack.retain(|(e, _)| e != ext_id);
            if stack.is_empty() {
                self.overrides.remove(key);
            }
        }
        self.get(key, ext_id)
    }

    fn drop_extension(&mut self, ext_id: &str) {
        self.overrides.retain(|_, stack| {
            stack.retain(|(e, _)| e != ext_id);
            !stack.is_empty()
        });
    }
}

// ---- Public Suffix helpers -----------------------------------------------------------------------------------------

fn extract_hostname(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let host = if trimmed.contains("://") {
        split_url(trimmed)?.host
    } else {
        trimmed.split('/').next().unwrap_or(trimmed).split(':').next().unwrap_or(trimmed).to_string()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() || host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    Some(host)
}

// ---- Dispatcher entry points ---------------------------------------------------------------------------------------

impl Dispatcher {
    pub(crate) fn platform_api(&self, member: &Member, call: &ApiCall, reply: Reply) {
        match member.namespace.as_str() {
            "publicSuffix" => reply(self.public_suffix_api(&member.name, call)),
            "notifications" => self.notifications_api(&member.name, call, reply),
            "idle" => reply(self.idle_api(&member.name, call)),
            "omnibox" => reply(self.omnibox_api(&member.name, call)),
            "search" => self.search_api(&member.name, call, reply),
            "tabGroups" => self.tab_groups_api(&member.name, call, reply),
            "tabs" if member.name == "group" => self.tabs_group(call, reply),
            "tabs" if member.name == "ungroup" => self.tabs_ungroup(call, reply),
            "userScripts" => self.user_scripts_api(&member.name, call, reply),
            "offscreen" => self.offscreen_api(&member.name, call, reply),
            "sidePanel" => self.side_panel_api(&member.name, call, reply),
            "sidebarAction" => self.sidebar_action_api(&member.name, call, reply),
            "theme" => self.theme_api(&member.name, call, reply),
            "power" => self.power_api(&member.name, call, reply),
            "contextualIdentities" => reply(self.contextual_identities_api(&member.name, call)),
            "find" => self.find_api(&member.name, call, reply),
            "system" => self.system_api(&member.name, call, reply),
            "fontSettings" => self.font_settings_api(&member.name, call, reply),
            "identity" => self.identity_api(&member.name, call, reply),
            "tts" => self.tts_api(&member.name, call, reply),
            "clipboard" => self.clipboard_api(&member.name, call, reply),
            "dns" => self.dns_api(&member.name, call, reply),
            "captivePortal" => reply(self.captive_portal_api(&member.name, call)),
            "tabs" => self.tabs_extra_api(&member.name, call, reply),
            "runtime" => self.runtime_extra_api(&member.name, call, reply),
            "permissions" => reply(self.permissions_host_access_api(&member.name, call)),
            "contextMenus" => reply(self.context_menus_extra_api(&member.name, call)),
            "commands" => self.host_call(call, "host.commands.openShortcutSettings", json!({}), reply, |_, _| Ok(Value::Null)),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, member.full_name.clone()))),
        }
    }

    /// Internal calls from the shim for `privacy.*` / `browserSettings.*` `BrowserSetting` objects and `omnibox.suggest`.
    pub(crate) fn platform_internal(&self, call: &ApiCall, reply: Reply) {
        match call.function.as_str() {
            "__setting.get" | "__setting.set" | "__setting.clear" => {
                let Some(key) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("setting key must be a string")));
                };
                let perm = if key.starts_with("privacy.") {
                    "privacy"
                } else if key.starts_with("browserSettings.") {
                    "browserSettings"
                } else {
                    return reply(Err(ApiError::invalid(format!("Unknown setting '{key}'"))));
                };
                let allowed = self.with_ext(call, |e| Ok(e.permissions.has_api(perm))).unwrap_or(false);
                if !allowed {
                    return reply(Err(ApiError::new(ErrorKind::PermissionDenied, format!("'{key}' requires the \"{perm}\" permission"))));
                }
                match call.function.as_str() {
                    "__setting.get" => {
                        let val = self.platform.borrow().settings.get(key, &call.extension_id);
                        reply(Ok(val));
                    }
                    "__setting.set" => {
                        let details = call.arg(1);
                        let Some(value) = details.get("value").cloned() else {
                            return reply(Err(ApiError::invalid("details.value is required")));
                        };
                        let state = self.platform.borrow_mut().settings.set(key, &call.extension_id, value);
                        self.broadcast_permitted(perm, &format!("__setting.onChange:{key}"), vec![state]);
                        reply(Ok(json!(true)));
                    }
                    "__setting.clear" => {
                        let state = self.platform.borrow_mut().settings.clear(key, &call.extension_id);
                        self.broadcast_permitted(perm, &format!("__setting.onChange:{key}"), vec![state]);
                        reply(Ok(json!(true)));
                    }
                    _ => unreachable!(),
                }
            }
            "__omnibox.suggest" => {
                let list = call.arg(0).as_array().cloned().unwrap_or_default();
                self.platform.borrow_mut().omnibox.last_suggestions.insert(call.extension_id.clone(), list.clone());
                let hc = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.omnibox.suggest".into(),
                    args: vec![json!({ "suggestions": list })],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&hc, Box::new(|_| {}));
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::UnknownApi, call.function.clone()))),
        }
    }

    pub(crate) fn platform_host_event(&self, name: &str, args: &Value) -> Option<Value> {
        let id = args.get("extensionId").and_then(Value::as_str).unwrap_or("");
        match name {
            "idleState" => {
                let secs = args.get("idleSeconds").and_then(Value::as_u64).unwrap_or(0);
                let locked = args.get("locked").and_then(Value::as_bool).unwrap_or(false);
                let mut to_fire = vec![];
                {
                    let mut p = self.platform.borrow_mut();
                    p.idle.idle_seconds = secs;
                    p.idle.locked = locked;
                    for ext_id in self.installed_ids() {
                        if !self.with_extension(&ext_id, |e| e.permissions.has_api("idle")).unwrap_or(false) {
                            continue;
                        }
                        let interval = p.idle.intervals.get(&ext_id).copied().unwrap_or(60);
                        let st = p.idle.state_for(interval);
                        let prev = p.idle.last_reported.get(&ext_id).map(String::as_str);
                        if prev != Some(st) {
                            p.idle.last_reported.insert(ext_id.clone(), st.to_string());
                            to_fire.push((ext_id, st));
                        }
                    }
                }
                for (ext_id, st) in to_fire {
                    self.host.dispatch_event(&ext_id, "idle.onStateChanged", vec![json!(st)]);
                }
                Some(Value::Null)
            }
            "notificationEvent" => {
                let kind = args.get("event").and_then(Value::as_str).unwrap_or("");
                let notif_id = args.get("notificationId").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "clicked" => self.host.dispatch_event(id, "notifications.onClicked", vec![json!(notif_id)]),
                    "closed" => {
                        let by_user = args.get("byUser").and_then(Value::as_bool).unwrap_or(false);
                        self.platform.borrow_mut().notifications.active.remove(&(id.to_string(), notif_id.to_string()));
                        self.host.dispatch_event(id, "notifications.onClosed", vec![json!(notif_id), json!(by_user)]);
                    }
                    "buttonClicked" => {
                        let idx = args.get("buttonIndex").and_then(Value::as_i64).unwrap_or(0);
                        self.host.dispatch_event(id, "notifications.onButtonClicked", vec![json!(notif_id), json!(idx)]);
                    }
                    "shown" => self.host.dispatch_event(id, "notifications.onShown", vec![json!(notif_id)]),
                    "permissionChanged" => {
                        let level = args.get("level").and_then(Value::as_str).unwrap_or("granted");
                        self.platform.borrow_mut().notifications.permission_level = level.to_string();
                        self.broadcast_permitted("notifications", "notifications.onPermissionLevelChanged", vec![json!(level)]);
                    }
                    _ => {}
                }
                Some(Value::Null)
            }
            "omniboxEvent" => {
                let kind = args.get("event").and_then(Value::as_str).unwrap_or("");
                let text = args.get("text").and_then(Value::as_str).unwrap_or("");
                match kind {
                    "inputStarted" => self.host.dispatch_event(id, "omnibox.onInputStarted", vec![]),
                    "inputChanged" => self.host.dispatch_event(id, "omnibox.onInputChanged", vec![json!(text)]),
                    "inputEntered" => {
                        let disp = args.get("disposition").and_then(Value::as_str).unwrap_or("currentTab");
                        self.host.dispatch_event(id, "omnibox.onInputEntered", vec![json!(text), json!(disp)]);
                    }
                    "inputCancelled" => self.host.dispatch_event(id, "omnibox.onInputCancelled", vec![]),
                    "deleteSuggestion" => self.host.dispatch_event(id, "omnibox.onDeleteSuggestion", vec![json!(text)]),
                    _ => {}
                }
                Some(Value::Null)
            }
            "contextMenuShown" => {
                self.broadcast_permitted("contextMenus", "contextMenus.onShown", vec![args.clone()]);
                Some(Value::Null)
            }
            "contextMenuHidden" => {
                self.broadcast_permitted("contextMenus", "contextMenus.onHidden", vec![]);
                Some(Value::Null)
            }
            "sidePanelOpened" => {
                self.host.dispatch_event(id, "sidePanel.onOpened", vec![args.clone()]);
                Some(Value::Null)
            }
            "managedStorageChanged" => {
                self.host.dispatch_event(id, "storage.managed.onChanged", vec![args.get("changes").cloned().unwrap_or_else(|| json!({}))]);
                Some(Value::Null)
            }
            "captivePortalState" => {
                let state = args.get("state").and_then(Value::as_str).unwrap_or("not_captive").to_string();
                let now = self.host.now_ms();
                {
                    let mut p = self.platform.borrow_mut();
                    p.captive_portal_state = state.clone();
                    p.captive_portal_last_checked = now;
                }
                self.broadcast_permitted("captivePortal", "captivePortal.onStateChanged", vec![json!({ "state": state })]);
                if state == "unlocked_portal" || state == "not_captive" {
                    let status = if state == "unlocked_portal" { "captive" } else { "clear" };
                    self.broadcast_permitted("captivePortal", "captivePortal.onConnectivityAvailable", vec![json!(status)]);
                }
                Some(Value::Null)
            }
            "runtimeEvent" => {
                let kind = args.get("event").and_then(Value::as_str).unwrap_or("");
                let payload = args.get("args").and_then(Value::as_array).cloned().unwrap_or_default();
                let full = match kind {
                    "suspend" => "runtime.onSuspend",
                    "suspendCanceled" => "runtime.onSuspendCanceled",
                    "updateAvailable" => "runtime.onUpdateAvailable",
                    "browserUpdateAvailable" => "runtime.onBrowserUpdateAvailable",
                    "restartRequired" => "runtime.onRestartRequired",
                    "performanceWarning" => "runtime.onPerformanceWarning",
                    "connectExternal" => "runtime.onConnectExternal",
                    "userScriptMessage" => "runtime.onUserScriptMessage",
                    "userScriptConnect" => "runtime.onUserScriptConnect",
                    _ => return Some(Value::Null),
                };
                if id.is_empty() {
                    self.broadcast_permitted("", full, payload);
                } else {
                    self.host.dispatch_event(id, full, payload);
                }
                Some(Value::Null)
            }
            _ => None,
        }
    }

    // ---- publicSuffix -----------------------------------------------------------------------------------------------

    fn public_suffix_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        let Some(input) = call.arg(0).as_str() else {
            return Err(ApiError::invalid("host must be a string"));
        };
        let Some(host) = extract_hostname(input) else {
            return Ok(if op == "isKnownSuffix" { json!(false) } else { Value::Null });
        };
        let list = psl::List;
        match op {
            "getDomain" => Ok(match list.domain(host.as_bytes()) {
                Some(d) => json!(std::str::from_utf8(d.as_bytes()).unwrap_or("")),
                None => Value::Null,
            }),
            "getKnownSuffix" => Ok(match list.suffix(host.as_bytes()) {
                Some(s) if s.is_known() => json!(std::str::from_utf8(s.as_bytes()).unwrap_or("")),
                _ => Value::Null,
            }),
            "isKnownSuffix" => Ok(json!(list.suffix(host.as_bytes()).is_some_and(|s| s.is_known() && s.as_bytes() == host.as_bytes()))),
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("publicSuffix.{op}"))),
        }
    }

    // ---- notifications ----------------------------------------------------------------------------------------------

    fn notifications_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "create" => {
                let (explicit_id, opts) = match call.arg(0) {
                    Value::String(s) => (Some(s.clone()), call.arg(1)),
                    _ => (None, call.arg(0)),
                };
                if !opts.is_object() {
                    return reply(Err(ApiError::invalid("options must be an object")));
                }
                let kind = opts.get("type").and_then(Value::as_str).unwrap_or("basic");
                if !matches!(kind, "basic" | "image" | "list" | "progress") {
                    return reply(Err(ApiError::invalid(format!("Invalid notification type '{kind}'"))));
                }
                let Some(title) = opts.get("title").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("options.title is required")));
                };
                let Some(message) = opts.get("message").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("options.message is required")));
                };
                let base_url = self.with_ext(call, |e| Ok(e.base_url.clone())).unwrap_or_default();
                let icon_url = opts
                    .get("iconUrl")
                    .and_then(Value::as_str)
                    .map(|p| {
                        if p.contains("://") || p.starts_with("data:") {
                            p.to_string()
                        } else {
                            format!("{base_url}{}", ui::normalize_extension_path(p, &base_url))
                        }
                    })
                    .unwrap_or_default();
                let notif_id = explicit_id.filter(|s| !s.is_empty()).unwrap_or_else(|| self.platform.borrow_mut().notifications.fresh_id());
                let mut normalized = opts.clone();
                normalized["type"] = json!(kind);
                normalized["title"] = json!(title);
                normalized["message"] = json!(message);
                normalized["iconUrl"] = json!(icon_url);
                normalized["notificationId"] = json!(notif_id);
                self.platform.borrow_mut().notifications.active.insert((call.extension_id.clone(), notif_id.clone()), normalized.clone());
                let hc = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.notifications.show".into(),
                    args: vec![normalized],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&hc, Box::new(|_| {}));
                reply(Ok(json!(notif_id)));
            }
            "update" => {
                let Some(notif_id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("notificationId must be a string")));
                };
                let opts = call.arg(1);
                if !opts.is_object() {
                    return reply(Err(ApiError::invalid("options must be an object")));
                }
                let updated = {
                    let mut p = self.platform.borrow_mut();
                    match p.notifications.active.get_mut(&(call.extension_id.clone(), notif_id.to_string())) {
                        Some(existing) => {
                            if let (Some(dst), Some(src)) = (existing.as_object_mut(), opts.as_object()) {
                                for (k, v) in src {
                                    dst.insert(k.clone(), v.clone());
                                }
                            }
                            Some(existing.clone())
                        }
                        None => None,
                    }
                };
                if let Some(updated) = updated {
                    let hc = ApiCall {
                        extension_id: call.extension_id.clone(),
                        function: "host.notifications.show".into(),
                        args: vec![updated],
                        caller: call.caller.clone(),
                    };
                    self.host.call_browser(&hc, Box::new(|_| {}));
                    reply(Ok(json!(true)));
                } else {
                    reply(Ok(json!(false)));
                }
            }
            "clear" => {
                let Some(notif_id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("notificationId must be a string")));
                };
                let existed =
                    self.platform.borrow_mut().notifications.active.remove(&(call.extension_id.clone(), notif_id.to_string())).is_some();
                if existed {
                    let hc = ApiCall {
                        extension_id: call.extension_id.clone(),
                        function: "host.notifications.clear".into(),
                        args: vec![json!({ "notificationId": notif_id })],
                        caller: call.caller.clone(),
                    };
                    self.host.call_browser(&hc, Box::new(|_| {}));
                    self.host.dispatch_event(&call.extension_id, "notifications.onClosed", vec![json!(notif_id), json!(false)]);
                }
                reply(Ok(json!(existed)));
            }
            "getAll" => {
                let mut out = Map::new();
                for (e, nid) in self.platform.borrow().notifications.active.keys() {
                    if e == &call.extension_id {
                        out.insert(nid.clone(), json!(true));
                    }
                }
                reply(Ok(Value::Object(out)));
            }
            "getPermissionLevel" => reply(Ok(json!(self.platform.borrow().notifications.permission_level))),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("notifications.{op}")))),
        }
    }

    // ---- idle -------------------------------------------------------------------------------------------------------

    fn idle_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        match op {
            "queryState" => {
                let Some(secs) = call.arg(0).as_i64() else {
                    return Err(ApiError::invalid("detectionIntervalInSeconds must be an integer"));
                };
                if secs < 15 {
                    return Err(ApiError::invalid("detectionIntervalInSeconds must be at least 15"));
                }
                Ok(json!(self.platform.borrow().idle.state_for(secs as u64)))
            }
            "setDetectionInterval" => {
                let Some(secs) = call.arg(0).as_i64() else {
                    return Err(ApiError::invalid("intervalInSeconds must be an integer"));
                };
                if secs < 15 {
                    return Err(ApiError::invalid("intervalInSeconds must be at least 15"));
                }
                self.platform.borrow_mut().idle.intervals.insert(call.extension_id.clone(), secs as u64);
                Ok(Value::Null)
            }
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("idle.{op}"))),
        }
    }

    // ---- omnibox ----------------------------------------------------------------------------------------------------

    fn omnibox_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        match op {
            "setDefaultSuggestion" => {
                let Some(desc) = call.arg(0).get("description").and_then(Value::as_str) else {
                    return Err(ApiError::invalid("suggestion.description must be a string"));
                };
                self.platform.borrow_mut().omnibox.default_suggestions.insert(call.extension_id.clone(), desc.to_string());
                Ok(Value::Null)
            }
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("omnibox.{op}"))),
        }
    }

    // ---- search -----------------------------------------------------------------------------------------------------

    fn search_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "query" => {
                let info = call.arg(0);
                let Some(text) = info.get("text").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                    return reply(Err(ApiError::invalid("queryInfo.text must be a non-empty string")));
                };
                let disp = info.get("disposition").and_then(Value::as_str).unwrap_or("CURRENT_TAB");
                if !matches!(disp, "CURRENT_TAB" | "NEW_TAB" | "NEW_WINDOW") {
                    return reply(Err(ApiError::invalid(format!("Invalid disposition: '{disp}'"))));
                }
                let mut req = json!({ "text": text, "disposition": disp });
                if let Some(tid) = info.get("tabId").and_then(Value::as_i64) {
                    req["tabId"] = json!(tid);
                }
                self.host_call(call, "host.search.query", req, reply, |_, _| Ok(Value::Null));
            }
            "search" => {
                let props = call.arg(0);
                let Some(query) = props.get("query").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                    return reply(Err(ApiError::invalid("searchProperties.query must be a non-empty string")));
                };
                let disp = props.get("disposition").and_then(Value::as_str).unwrap_or("NEW_TAB");
                let mut req = json!({ "text": query, "disposition": disp });
                if let Some(eng) = props.get("engine").and_then(Value::as_str) {
                    req["engine"] = json!(eng);
                }
                if let Some(tid) = props.get("tabId").and_then(Value::as_i64) {
                    req["tabId"] = json!(tid);
                }
                self.host_call(call, "host.search.query", req, reply, |_, _| Ok(Value::Null));
            }
            "get" => {
                self.host_call(call, "host.search.engines", json!({}), reply, |_, v| {
                    Ok(if v.is_array() { v } else { Value::Array(vec![]) })
                });
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("search.{op}")))),
        }
    }

    // ---- tabGroups & tabs.group / tabs.ungroup ----------------------------------------------------------------------

    fn tabs_group(&self, call: &ApiCall, reply: Reply) {
        let opts = call.arg(0);
        if !opts.is_object() {
            return reply(Err(ApiError::invalid("options must be an object")));
        }
        let tab_ids: Result<Vec<i64>, ApiError> = match opts.get("tabIds") {
            Some(Value::Number(n)) => n.as_i64().map(|i| vec![i]).ok_or_else(|| ApiError::invalid("tabIds must be integers")),
            Some(Value::Array(a)) if !a.is_empty() => {
                a.iter().map(|v| v.as_i64().ok_or_else(|| ApiError::invalid("tabIds must be integers"))).collect()
            }
            _ => Err(ApiError::invalid("options.tabIds must be a tab id or non-empty array of tab ids")),
        };
        let tab_ids = match tab_ids {
            Ok(v) => v,
            Err(e) => return reply(Err(e)),
        };
        let mut window_id = None;
        {
            let state = self.browser.borrow();
            for &tid in &tab_ids {
                let Some(t) = state.tab(tid) else {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tid}."))));
                };
                if window_id.is_none() {
                    window_id = Some(t.window_id);
                }
            }
        }
        let target_window = opts.get("createProperties").and_then(|c| c.get("windowId")).and_then(Value::as_i64).or(window_id).unwrap_or(1);
        let (group_id, created_group, removed_groups) = {
            let mut p = self.platform.borrow_mut();
            let (gid, created) = if let Some(existing_id) = opts.get("groupId").and_then(Value::as_i64) {
                if !p.tab_groups.groups.contains_key(&existing_id) {
                    return reply(Err(ApiError::runtime(format!("No group with id: {existing_id}."))));
                }
                (existing_id, None)
            } else {
                let gid = p.tab_groups.fresh_id();
                let group = TabGroup { id: gid, window_id: target_window, title: String::new(), color: "grey".into(), collapsed: false };
                p.tab_groups.groups.insert(gid, group.clone());
                (gid, Some(group))
            };
            for tid in &tab_ids {
                p.tab_groups.tab_groups.insert(*tid, gid);
            }
            let removed = p.tab_groups.prune_empty();
            (gid, created, removed)
        };
        if let Some(g) = created_group {
            self.broadcast_permitted("tabGroups", "tabGroups.onCreated", vec![g.to_json()]);
        }
        for g in removed_groups {
            self.broadcast_permitted("tabGroups", "tabGroups.onRemoved", vec![g.to_json()]);
        }
        reply(Ok(json!(group_id)));
    }

    fn tabs_ungroup(&self, call: &ApiCall, reply: Reply) {
        let tab_ids: Result<Vec<i64>, ApiError> = match call.arg(0) {
            Value::Number(n) => n.as_i64().map(|i| vec![i]).ok_or_else(|| ApiError::invalid("tabIds must be integers")),
            Value::Array(a) if !a.is_empty() => {
                a.iter().map(|v| v.as_i64().ok_or_else(|| ApiError::invalid("tabIds must be integers"))).collect()
            }
            _ => Err(ApiError::invalid("tabIds must be a tab id or non-empty array of tab ids")),
        };
        let tab_ids = match tab_ids {
            Ok(v) => v,
            Err(e) => return reply(Err(e)),
        };
        let removed_groups = {
            let mut p = self.platform.borrow_mut();
            for tid in &tab_ids {
                p.tab_groups.tab_groups.remove(tid);
            }
            p.tab_groups.prune_empty()
        };
        for g in removed_groups {
            self.broadcast_permitted("tabGroups", "tabGroups.onRemoved", vec![g.to_json()]);
        }
        reply(Ok(Value::Null));
    }

    fn tab_groups_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "get" => {
                let Some(gid) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("groupId must be an integer")));
                };
                match self.platform.borrow().tab_groups.groups.get(&gid) {
                    Some(g) => reply(Ok(g.to_json())),
                    None => reply(Err(ApiError::runtime(format!("No group with id: {gid}.")))),
                }
            }
            "query" => {
                let q = call.arg(0);
                if !q.is_null() && !q.is_object() {
                    return reply(Err(ApiError::invalid("queryInfo must be an object")));
                }
                let mut groups: Vec<TabGroup> = self
                    .platform
                    .borrow()
                    .tab_groups
                    .groups
                    .values()
                    .filter(|g| {
                        if let Some(c) = q.get("collapsed").and_then(Value::as_bool)
                            && g.collapsed != c
                        {
                            return false;
                        }
                        if let Some(col) = q.get("color").and_then(Value::as_str)
                            && g.color != col
                        {
                            return false;
                        }
                        if let Some(t) = q.get("title").and_then(Value::as_str)
                            && g.title != t
                        {
                            return false;
                        }
                        if let Some(wid) = q.get("windowId").and_then(Value::as_i64)
                            && g.window_id != wid
                        {
                            return false;
                        }
                        true
                    })
                    .cloned()
                    .collect();
                groups.sort_by_key(|g| g.id);
                reply(Ok(Value::Array(groups.iter().map(TabGroup::to_json).collect())));
            }
            "update" => {
                let Some(gid) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("groupId must be an integer")));
                };
                let props = call.arg(1);
                if !props.is_object() {
                    return reply(Err(ApiError::invalid("updateProperties must be an object")));
                }
                if let Some(col) = props.get("color").and_then(Value::as_str)
                    && !VALID_GROUP_COLORS.contains(&col)
                {
                    return reply(Err(ApiError::invalid(format!("Invalid group color '{col}'"))));
                }
                let updated = {
                    let mut p = self.platform.borrow_mut();
                    let Some(g) = p.tab_groups.groups.get_mut(&gid) else {
                        return reply(Err(ApiError::runtime(format!("No group with id: {gid}."))));
                    };
                    if let Some(t) = props.get("title").and_then(Value::as_str) {
                        g.title = t.to_string();
                    }
                    if let Some(col) = props.get("color").and_then(Value::as_str) {
                        g.color = col.to_string();
                    }
                    if let Some(c) = props.get("collapsed").and_then(Value::as_bool) {
                        g.collapsed = c;
                    }
                    g.to_json()
                };
                self.broadcast_permitted("tabGroups", "tabGroups.onUpdated", vec![updated.clone()]);
                reply(Ok(updated));
            }
            "move" => {
                let Some(gid) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("groupId must be an integer")));
                };
                let props = call.arg(1);
                if !props.is_object() || props.get("index").and_then(Value::as_i64).is_none() {
                    return reply(Err(ApiError::invalid("moveProperties.index must be an integer")));
                }
                let updated = {
                    let mut p = self.platform.borrow_mut();
                    let Some(g) = p.tab_groups.groups.get_mut(&gid) else {
                        return reply(Err(ApiError::runtime(format!("No group with id: {gid}."))));
                    };
                    if let Some(wid) = props.get("windowId").and_then(Value::as_i64) {
                        g.window_id = wid;
                    }
                    g.to_json()
                };
                self.broadcast_permitted("tabGroups", "tabGroups.onMoved", vec![updated.clone()]);
                reply(Ok(updated));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("tabGroups.{op}")))),
        }
    }

    // ---- userScripts ------------------------------------------------------------------------------------------------

    fn parse_user_script(v: &Value, base_url: &str, existing: Option<&RegisteredUserScript>) -> Result<RegisteredUserScript, String> {
        let id = v.get("id").and_then(Value::as_str).ok_or("User script needs an id")?.to_string();
        if id.is_empty() || id.starts_with('_') {
            return Err(format!("User script ID '{id}' must not be empty or start with '_'"));
        }
        let mut s = existing.map(|e| e.script.clone()).unwrap_or_default();
        let mut inline_code = existing.map(|e| e.inline_code.clone()).unwrap_or_default();
        let mut world_name = existing.map(|e| e.world_name.clone()).unwrap_or_else(|| "USER_SCRIPT".into());
        let mut world_id = existing.and_then(|e| e.world_id.clone());

        if let Some(matches) = v.get("matches").and_then(Value::as_array) {
            let mut m = vec![];
            for item in matches {
                let pat = item.as_str().ok_or("matches must contain strings")?;
                MatchPattern::parse(pat, MatchDialect::Chrome).map_err(|e| format!("Invalid match pattern '{pat}': {e}"))?;
                m.push(pat.to_string());
            }
            s.matches = m;
        }
        if let Some(ex) = v.get("excludeMatches").and_then(Value::as_array) {
            s.exclude_matches = ex.iter().filter_map(Value::as_str).map(String::from).collect();
        }
        if let Some(ig) = v.get("includeGlobs").and_then(Value::as_array) {
            s.include_globs = ig.iter().filter_map(Value::as_str).map(String::from).collect();
        }
        if let Some(eg) = v.get("excludeGlobs").and_then(Value::as_array) {
            s.exclude_globs = eg.iter().filter_map(Value::as_str).map(String::from).collect();
        }
        if let Some(js) = v.get("js").and_then(Value::as_array) {
            s.js.clear();
            inline_code.clear();
            for item in js {
                if let Some(file) = item.get("file").and_then(Value::as_str) {
                    s.js.push(ui::normalize_extension_path(file, base_url));
                } else if let Some(code) = item.get("code").and_then(Value::as_str) {
                    inline_code.push(code.to_string());
                } else {
                    return Err("Each js entry must specify 'file' or 'code'".into());
                }
            }
        }
        if let Some(b) = v.get("allFrames").and_then(Value::as_bool) {
            s.all_frames = b;
        }
        if let Some(r) = v.get("runAt").and_then(Value::as_str) {
            s.run_at = match r {
                "document_start" => RunAt::DocumentStart,
                "document_end" => RunAt::DocumentEnd,
                "document_idle" => RunAt::DocumentIdle,
                other => return Err(format!("Invalid runAt '{other}'")),
            };
        }
        if let Some(w) = v.get("world").and_then(Value::as_str) {
            if !matches!(w, "USER_SCRIPT" | "MAIN") {
                return Err(format!("Invalid world '{w}'"));
            }
            world_name = w.to_string();
            s.world = if w == "MAIN" { ScriptWorld::Main } else { ScriptWorld::Isolated };
        }
        if let Some(wid) = v.get("worldId").and_then(Value::as_str) {
            if wid.starts_with('_') {
                return Err("worldId must not start with '_'".into());
            }
            world_id = Some(wid.to_string());
        }
        if s.matches.is_empty() && s.include_globs.is_empty() {
            return Err(format!("User script '{id}' must specify 'matches'"));
        }
        if s.js.is_empty() && inline_code.is_empty() {
            return Err(format!("User script '{id}' must specify at least one js entry"));
        }
        Ok(RegisteredUserScript { id, script: s, inline_code, world_id, world_name })
    }

    fn user_scripts_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let base_url = self.with_ext(call, |e| Ok(e.base_url.clone())).unwrap_or_default();
        match op {
            "register" => {
                let Some(arr) = call.arg(0).as_array() else {
                    return reply(Err(ApiError::invalid("scripts must be an array")));
                };
                let mut parsed = vec![];
                let mut seen = HashSet::new();
                for item in arr {
                    let s = match Self::parse_user_script(item, &base_url, None) {
                        Ok(s) => s,
                        Err(e) => return reply(Err(ApiError::invalid(e))),
                    };
                    if !seen.insert(s.id.clone()) {
                        return reply(Err(ApiError::runtime(format!("Duplicate script ID '{}'", s.id))));
                    }
                    parsed.push(s);
                }
                let mut p = self.platform.borrow_mut();
                let list = p.user_scripts.scripts.entry(call.extension_id.clone()).or_default();
                for s in &parsed {
                    if list.iter().any(|e| e.id == s.id) {
                        return reply(Err(ApiError::runtime(format!("Duplicate script ID '{}'", s.id))));
                    }
                }
                list.extend(parsed);
                reply(Ok(Value::Null));
            }
            "unregister" => {
                let filter_ids: Option<Vec<String>> = call
                    .arg(0)
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect());
                let mut p = self.platform.borrow_mut();
                let list = p.user_scripts.scripts.entry(call.extension_id.clone()).or_default();
                match filter_ids {
                    None => list.clear(),
                    Some(ids) => {
                        for id in &ids {
                            if !list.iter().any(|s| &s.id == id) {
                                return reply(Err(ApiError::runtime(format!("Nonexistent script ID '{id}'"))));
                            }
                        }
                        list.retain(|s| !ids.contains(&s.id));
                    }
                }
                reply(Ok(Value::Null));
            }
            "getScripts" => {
                let filter_ids: Option<Vec<String>> = call
                    .arg(0)
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect());
                let p = self.platform.borrow();
                let out: Vec<Value> = p
                    .user_scripts
                    .scripts
                    .get(&call.extension_id)
                    .into_iter()
                    .flatten()
                    .filter(|s| filter_ids.as_ref().is_none_or(|ids| ids.contains(&s.id)))
                    .map(RegisteredUserScript::to_json)
                    .collect();
                reply(Ok(Value::Array(out)));
            }
            "update" => {
                let Some(arr) = call.arg(0).as_array() else {
                    return reply(Err(ApiError::invalid("scripts must be an array")));
                };
                let mut p = self.platform.borrow_mut();
                let list = p.user_scripts.scripts.entry(call.extension_id.clone()).or_default();
                let mut updated = vec![];
                for item in arr {
                    let Some(id) = item.get("id").and_then(Value::as_str) else {
                        return reply(Err(ApiError::invalid("Each script update needs an id")));
                    };
                    let Some(existing) = list.iter().find(|s| s.id == id) else {
                        return reply(Err(ApiError::runtime(format!("Nonexistent script ID '{id}'"))));
                    };
                    match Self::parse_user_script(item, &base_url, Some(existing)) {
                        Ok(s) => updated.push(s),
                        Err(e) => return reply(Err(ApiError::invalid(e))),
                    }
                }
                for u in updated {
                    if let Some(slot) = list.iter_mut().find(|s| s.id == u.id) {
                        *slot = u;
                    }
                }
                reply(Ok(Value::Null));
            }
            "configureWorld" => {
                let props = call.arg(0);
                if !props.is_object() {
                    return reply(Err(ApiError::invalid("properties must be an object")));
                }
                let world_id = props.get("worldId").and_then(Value::as_str).unwrap_or("").to_string();
                if world_id.starts_with('_') {
                    return reply(Err(ApiError::invalid("worldId must not start with '_'")));
                }
                let cfg = UserScriptWorldConfig {
                    world_id: world_id.clone(),
                    csp: props.get("csp").and_then(Value::as_str).map(String::from),
                    messaging: props.get("messaging").and_then(Value::as_bool).unwrap_or(false),
                };
                self.platform.borrow_mut().user_scripts.worlds.entry(call.extension_id.clone()).or_default().insert(world_id, cfg);
                reply(Ok(Value::Null));
            }
            "getWorldConfigurations" => {
                let p = self.platform.borrow();
                let mut out: Vec<Value> = p
                    .user_scripts
                    .worlds
                    .get(&call.extension_id)
                    .map(|m| {
                        let mut v: Vec<&UserScriptWorldConfig> = m.values().collect();
                        v.sort_by(|a, b| a.world_id.cmp(&b.world_id));
                        v.into_iter().map(UserScriptWorldConfig::to_json).collect()
                    })
                    .unwrap_or_default();
                if out.is_empty() {
                    out.push(UserScriptWorldConfig::default().to_json());
                }
                reply(Ok(Value::Array(out)));
            }
            "resetWorldConfiguration" => {
                let world_id = call.arg(0).as_str().unwrap_or("");
                if let Some(m) = self.platform.borrow_mut().user_scripts.worlds.get_mut(&call.extension_id) {
                    m.remove(world_id);
                }
                reply(Ok(Value::Null));
            }
            "execute" => {
                let inj = call.arg(0);
                let Some(tab_id) = inj.get("target").and_then(|t| t.get("tabId")).and_then(Value::as_i64) else {
                    return reply(Err(ApiError::invalid("injection.target.tabId is required")));
                };
                let Some(js) = inj.get("js").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
                    return reply(Err(ApiError::invalid("injection.js must be a non-empty array")));
                };
                let mut chunks = vec![];
                for item in js {
                    if let Some(code) = item.get("code").and_then(Value::as_str) {
                        chunks.push(code.to_string());
                    } else if let Some(file) = item.get("file").and_then(Value::as_str) {
                        let norm = ui::normalize_extension_path(file, &base_url);
                        match self.with_ext(call, |e| {
                            e.files.text(&norm).ok_or_else(|| ApiError::runtime(format!("Could not load file: '{file}'.")))
                        }) {
                            Ok(src) => chunks.push(src),
                            Err(e) => return reply(Err(e)),
                        }
                    } else {
                        return reply(Err(ApiError::invalid("Each js entry must specify 'file' or 'code'")));
                    }
                }
                let world = inj.get("world").and_then(Value::as_str).unwrap_or("USER_SCRIPT");
                let host_world = if world == "MAIN" { "MAIN" } else { "ISOLATED" };
                let req = json!({
                    "tabId": tab_id,
                    "frameIds": inj.get("target").and_then(|t| t.get("frameIds")).cloned().unwrap_or_else(|| json!([0])),
                    "world": host_world,
                    "code": chunks.join("\n;\n"),
                });
                self.host_call(call, "host.scripting.inject", req, reply, |_, v| Ok(if v.is_array() { v } else { json!([]) }));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("userScripts.{op}")))),
        }
    }

    // ---- offscreen --------------------------------------------------------------------------------------------------

    fn offscreen_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "createDocument" => {
                if self.platform.borrow().offscreen.contains_key(&call.extension_id) {
                    return reply(Err(ApiError::runtime("Only a single offscreen document may be created.")));
                }
                let params = call.arg(0);
                let Some(url) = params.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("parameters.url is required")));
                };
                let Some(reasons_arr) = params.get("reasons").and_then(Value::as_array).filter(|a| !a.is_empty()) else {
                    return reply(Err(ApiError::invalid("parameters.reasons must be a non-empty array")));
                };
                let mut reasons = vec![];
                for r in reasons_arr {
                    let Some(rs) = r.as_str() else {
                        return reply(Err(ApiError::invalid("reasons must be strings")));
                    };
                    if !VALID_OFFSCREEN_REASONS.contains(&rs) {
                        return reply(Err(ApiError::invalid(format!("Invalid offscreen reason '{rs}'"))));
                    }
                    reasons.push(rs.to_string());
                }
                let Some(justification) = params.get("justification").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
                    return reply(Err(ApiError::invalid("parameters.justification is required")));
                };
                let full_url = self
                    .with_ext(call, |e| Ok(format!("{}{}", e.base_url, ui::normalize_extension_path(url, &e.base_url))))
                    .unwrap_or_else(|_| url.to_string());
                let doc = OffscreenDoc { url: full_url.clone(), reasons: reasons.clone(), justification: justification.to_string() };
                self.platform.borrow_mut().offscreen.insert(call.extension_id.clone(), doc);
                let hc = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.offscreen.create".into(),
                    args: vec![json!({ "url": full_url, "reasons": reasons })],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&hc, Box::new(|_| {}));
                reply(Ok(Value::Null));
            }
            "closeDocument" => {
                if self.platform.borrow_mut().offscreen.remove(&call.extension_id).is_none() {
                    return reply(Err(ApiError::runtime("No current offscreen document.")));
                }
                let hc = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.offscreen.close".into(),
                    args: vec![json!({})],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&hc, Box::new(|_| {}));
                reply(Ok(Value::Null));
            }
            "hasDocument" => reply(Ok(json!(self.platform.borrow().offscreen.contains_key(&call.extension_id)))),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("offscreen.{op}")))),
        }
    }

    // ---- sidePanel (Chrome) & sidebarAction (Firefox) ---------------------------------------------------------------

    fn side_panel_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let default_path = self.with_ext(call, |e| Ok(e.manifest.side_panel_path.clone())).unwrap_or_default();
        match op {
            "setOptions" => {
                let opts = call.arg(0);
                if !opts.is_object() {
                    return reply(Err(ApiError::invalid("options must be an object")));
                }
                let tab_id = opts.get("tabId").and_then(Value::as_i64);
                let path = opts.get("path").and_then(Value::as_str).map(String::from);
                let enabled = opts.get("enabled").and_then(Value::as_bool);
                {
                    let mut p = self.platform.borrow_mut();
                    let st = p.side_panels.entry(call.extension_id.clone()).or_default();
                    let target = match tab_id {
                        Some(t) => st.per_tab.entry(t).or_default(),
                        None => &mut st.global,
                    };
                    if let Some(pa) = path {
                        target.path = Some(pa);
                    }
                    if let Some(en) = enabled {
                        target.enabled = Some(en);
                    }
                }
                self.host.dispatch_event(&call.extension_id, "__ui.changed", vec![json!("sidePanel")]);
                reply(Ok(Value::Null));
            }
            "getOptions" => {
                let tab_id = call.arg(0).get("tabId").and_then(Value::as_i64);
                let p = self.platform.borrow();
                let st = p.side_panels.get(&call.extension_id);
                let tab_opt = tab_id.and_then(|t| st.and_then(|s| s.per_tab.get(&t)));
                let path = tab_opt
                    .and_then(|o| o.path.clone())
                    .or_else(|| st.and_then(|s| s.global.path.clone()))
                    .or_else(|| (!default_path.is_empty()).then_some(default_path));
                let enabled = tab_opt.and_then(|o| o.enabled).or_else(|| st.and_then(|s| s.global.enabled)).unwrap_or(true);
                let mut out = json!({ "enabled": enabled });
                if let Some(pa) = path {
                    out["path"] = json!(pa);
                }
                if let Some(t) = tab_id {
                    out["tabId"] = json!(t);
                }
                reply(Ok(out));
            }
            "setPanelBehavior" => {
                let behavior = call.arg(0);
                if !behavior.is_object() {
                    return reply(Err(ApiError::invalid("behavior must be an object")));
                }
                if let Some(b) = behavior.get("openPanelOnActionClick").and_then(Value::as_bool) {
                    self.platform.borrow_mut().side_panels.entry(call.extension_id.clone()).or_default().open_panel_on_action_click = b;
                }
                reply(Ok(Value::Null));
            }
            "getPanelBehavior" => {
                let open = self.platform.borrow().side_panels.get(&call.extension_id).is_some_and(|s| s.open_panel_on_action_click);
                reply(Ok(json!({ "openPanelOnActionClick": open })));
            }
            "open" => {
                let opts = call.arg(0);
                let tab_id = opts.get("tabId").and_then(Value::as_i64);
                let window_id = opts.get("windowId").and_then(Value::as_i64);
                if tab_id.is_none() && window_id.is_none() {
                    return reply(Err(ApiError::invalid("At least one of tabId or windowId must be provided")));
                }
                self.host_call(call, "host.sidePanel.open", opts.clone(), reply, |_, _| Ok(Value::Null));
            }
            "getLayout" => {
                let side = self
                    .platform
                    .borrow()
                    .side_panels
                    .get(&call.extension_id)
                    .map(|s| s.side.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "right".into());
                reply(Ok(json!({ "side": side })));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("sidePanel.{op}")))),
        }
    }

    fn sidebar_action_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let default_panel = self.with_ext(call, |e| Ok(e.manifest.sidebar_default_panel.clone())).unwrap_or_default();
        let default_title = self.with_ext(call, |e| Ok(e.i18n.localize(&e.manifest.name))).unwrap_or_default();
        let base_url = self.with_ext(call, |e| Ok(e.base_url.clone())).unwrap_or_default();
        match op {
            "setTitle" | "setPanel" | "setIcon" => {
                let details = call.arg(0);
                if !details.is_object() {
                    return reply(Err(ApiError::invalid("details must be an object")));
                }
                let tab_id = details.get("tabId").and_then(Value::as_i64);
                let mut p = self.platform.borrow_mut();
                let st = p.sidebars.entry(call.extension_id.clone()).or_default();
                let target = match tab_id {
                    Some(t) => st.per_tab.entry(t).or_default(),
                    None => &mut st.global,
                };
                match op {
                    "setTitle" => target.title = details.get("title").and_then(Value::as_str).map(String::from),
                    "setPanel" => {
                        target.panel = details.get("panel").and_then(Value::as_str).map(|pa| {
                            if pa.is_empty() || pa.contains("://") {
                                pa.to_string()
                            } else {
                                format!("{base_url}{}", ui::normalize_extension_path(pa, &base_url))
                            }
                        });
                    }
                    "setIcon" => target.icon = Some(details.clone()),
                    _ => {}
                }
                self.host.dispatch_event(&call.extension_id, "__ui.changed", vec![json!("sidebarAction")]);
                reply(Ok(Value::Null));
            }
            "getTitle" => {
                let tab_id = call.arg(0).get("tabId").and_then(Value::as_i64);
                let p = self.platform.borrow();
                let st = p.sidebars.get(&call.extension_id);
                let title = tab_id
                    .and_then(|t| st.and_then(|s| s.per_tab.get(&t)).and_then(|o| o.title.clone()))
                    .or_else(|| st.and_then(|s| s.global.title.clone()))
                    .unwrap_or(default_title);
                reply(Ok(json!(title)));
            }
            "getPanel" => {
                let tab_id = call.arg(0).get("tabId").and_then(Value::as_i64);
                let p = self.platform.borrow();
                let st = p.sidebars.get(&call.extension_id);
                let panel = tab_id
                    .and_then(|t| st.and_then(|s| s.per_tab.get(&t)).and_then(|o| o.panel.clone()))
                    .or_else(|| st.and_then(|s| s.global.panel.clone()))
                    .unwrap_or_else(|| {
                        if default_panel.is_empty() {
                            String::new()
                        } else {
                            format!("{base_url}{}", ui::normalize_extension_path(&default_panel, &base_url))
                        }
                    });
                reply(Ok(json!(panel)));
            }
            "open" | "close" | "toggle" => {
                let wid = self.browser.borrow().current_window(call.caller.tab_id).unwrap_or(1);
                {
                    let mut p = self.platform.borrow_mut();
                    let st = p.sidebars.entry(call.extension_id.clone()).or_default();
                    match op {
                        "open" => {
                            st.open_windows.insert(wid);
                        }
                        "close" => {
                            st.open_windows.remove(&wid);
                        }
                        "toggle" if !st.open_windows.remove(&wid) => {
                            st.open_windows.insert(wid);
                        }
                        _ => {}
                    }
                }
                let hc = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: format!("host.sidebarAction.{op}"),
                    args: vec![json!({ "windowId": wid })],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&hc, Box::new(|_| {}));
                reply(Ok(Value::Null));
            }
            "isOpen" => {
                let wid = call
                    .arg(0)
                    .get("windowId")
                    .and_then(Value::as_i64)
                    .or_else(|| self.browser.borrow().current_window(call.caller.tab_id))
                    .unwrap_or(1);
                let open = self.platform.borrow().sidebars.get(&call.extension_id).is_some_and(|s| s.open_windows.contains(&wid));
                reply(Ok(json!(open)));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("sidebarAction.{op}")))),
        }
    }

    // ---- theme (Firefox) --------------------------------------------------------------------------------------------

    fn theme_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "getCurrent" => {
                let wid = call.arg(0).as_i64();
                let p = self.platform.borrow();
                let theme = wid
                    .and_then(|w| p.themes.per_window.get(&w).map(|(_, v)| v.clone()))
                    .or_else(|| p.themes.global.as_ref().map(|(_, v)| v.clone()))
                    .unwrap_or_else(|| json!({}));
                reply(Ok(theme));
            }
            "update" => {
                let (wid, details) = match call.arg(0) {
                    Value::Number(n) => (n.as_i64(), call.arg(1)),
                    _ => (None, call.arg(0)),
                };
                if !details.is_object() {
                    return reply(Err(ApiError::invalid("details must be an object")));
                }
                {
                    let mut p = self.platform.borrow_mut();
                    match wid {
                        Some(w) => {
                            p.themes.per_window.insert(w, (call.extension_id.clone(), details.clone()));
                        }
                        None => {
                            p.themes.global = Some((call.extension_id.clone(), details.clone()));
                        }
                    }
                }
                let mut info = json!({ "theme": details });
                if let Some(w) = wid {
                    info["windowId"] = json!(w);
                }
                self.broadcast_permitted("theme", "theme.onUpdated", vec![info]);
                self.host.dispatch_event(&call.extension_id, "__ui.changed", vec![json!("theme")]);
                reply(Ok(Value::Null));
            }
            "reset" => {
                let wid = call.arg(0).as_i64();
                {
                    let mut p = self.platform.borrow_mut();
                    match wid {
                        Some(w) => {
                            p.themes.per_window.remove(&w);
                        }
                        None => {
                            p.themes.global = None;
                        }
                    }
                }
                let mut info = json!({ "theme": {} });
                if let Some(w) = wid {
                    info["windowId"] = json!(w);
                }
                self.broadcast_permitted("theme", "theme.onUpdated", vec![info]);
                self.host.dispatch_event(&call.extension_id, "__ui.changed", vec![json!("theme")]);
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("theme.{op}")))),
        }
    }

    // ---- power (Chrome) ---------------------------------------------------------------------------------------------

    fn power_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let level = match op {
            "requestKeepAwake" => {
                let Some(lvl) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("level must be 'system' or 'display'")));
                };
                if !matches!(lvl, "system" | "display") {
                    return reply(Err(ApiError::invalid(format!("Invalid power level '{lvl}'"))));
                }
                let mut p = self.platform.borrow_mut();
                p.power_levels.insert(call.extension_id.clone(), lvl.to_string());
                p.effective_power_level()
            }
            "releaseKeepAwake" => {
                let mut p = self.platform.borrow_mut();
                p.power_levels.remove(&call.extension_id);
                p.effective_power_level()
            }
            _ => return reply(Err(ApiError::new(ErrorKind::Unhandled, format!("power.{op}")))),
        };
        let hc = ApiCall {
            extension_id: call.extension_id.clone(),
            function: "host.power.setLevel".into(),
            args: vec![json!({ "level": level })],
            caller: call.caller.clone(),
        };
        self.host.call_browser(&hc, Box::new(|_| {}));
        reply(Ok(Value::Null));
    }

    // ---- contextualIdentities (Firefox) -----------------------------------------------------------------------------

    fn contextual_identities_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        match op {
            "get" => {
                let Some(csid) = call.arg(0).as_str() else {
                    return Err(ApiError::invalid("cookieStoreId must be a string"));
                };
                let p = self.platform.borrow();
                p.containers
                    .items
                    .iter()
                    .find(|c| c.cookie_store_id == csid)
                    .map(Container::to_json)
                    .ok_or_else(|| ApiError::runtime(format!("Invalid contextual identity: {csid}")))
            }
            "query" => {
                let filter_name = call.arg(0).get("name").and_then(Value::as_str);
                let p = self.platform.borrow();
                let out: Vec<Value> =
                    p.containers.items.iter().filter(|c| filter_name.is_none_or(|n| c.name == n)).map(Container::to_json).collect();
                Ok(Value::Array(out))
            }
            "create" => {
                let details = call.arg(0);
                let Some(name) = details.get("name").and_then(Value::as_str) else {
                    return Err(ApiError::invalid("details.name is required"));
                };
                let Some(color) = details.get("color").and_then(Value::as_str) else {
                    return Err(ApiError::invalid("details.color is required"));
                };
                if !CONTAINER_COLORS.iter().any(|(c, _)| *c == color) {
                    return Err(ApiError::invalid(format!("Invalid container color '{color}'")));
                }
                let Some(icon) = details.get("icon").and_then(Value::as_str) else {
                    return Err(ApiError::invalid("details.icon is required"));
                };
                if !CONTAINER_ICONS.contains(&icon) {
                    return Err(ApiError::invalid(format!("Invalid container icon '{icon}'")));
                }
                let created = {
                    let mut p = self.platform.borrow_mut();
                    let id = p.containers.next_id;
                    p.containers.next_id += 1;
                    let c = Container {
                        cookie_store_id: format!("firefox-container-{id}"),
                        name: name.to_string(),
                        color: color.to_string(),
                        icon: icon.to_string(),
                    };
                    let j = c.to_json();
                    p.containers.items.push(c);
                    j
                };
                self.broadcast_permitted(
                    "contextualIdentities",
                    "contextualIdentities.onCreated",
                    vec![json!({ "contextualIdentity": created })],
                );
                Ok(created)
            }
            "update" => {
                let Some(csid) = call.arg(0).as_str() else {
                    return Err(ApiError::invalid("cookieStoreId must be a string"));
                };
                let details = call.arg(1);
                if !details.is_object() {
                    return Err(ApiError::invalid("details must be an object"));
                }
                if let Some(color) = details.get("color").and_then(Value::as_str)
                    && !CONTAINER_COLORS.iter().any(|(c, _)| *c == color)
                {
                    return Err(ApiError::invalid(format!("Invalid container color '{color}'")));
                }
                if let Some(icon) = details.get("icon").and_then(Value::as_str)
                    && !CONTAINER_ICONS.contains(&icon)
                {
                    return Err(ApiError::invalid(format!("Invalid container icon '{icon}'")));
                }
                let updated = {
                    let mut p = self.platform.borrow_mut();
                    let Some(c) = p.containers.items.iter_mut().find(|c| c.cookie_store_id == csid) else {
                        return Err(ApiError::runtime(format!("Invalid contextual identity: {csid}")));
                    };
                    if let Some(n) = details.get("name").and_then(Value::as_str) {
                        c.name = n.to_string();
                    }
                    if let Some(col) = details.get("color").and_then(Value::as_str) {
                        c.color = col.to_string();
                    }
                    if let Some(ic) = details.get("icon").and_then(Value::as_str) {
                        c.icon = ic.to_string();
                    }
                    c.to_json()
                };
                self.broadcast_permitted(
                    "contextualIdentities",
                    "contextualIdentities.onUpdated",
                    vec![json!({ "contextualIdentity": updated })],
                );
                Ok(updated)
            }
            "remove" => {
                let Some(csid) = call.arg(0).as_str() else {
                    return Err(ApiError::invalid("cookieStoreId must be a string"));
                };
                let removed = {
                    let mut p = self.platform.borrow_mut();
                    let Some(pos) = p.containers.items.iter().position(|c| c.cookie_store_id == csid) else {
                        return Err(ApiError::runtime(format!("Invalid contextual identity: {csid}")));
                    };
                    p.containers.items.remove(pos).to_json()
                };
                self.broadcast_permitted(
                    "contextualIdentities",
                    "contextualIdentities.onRemoved",
                    vec![json!({ "contextualIdentity": removed })],
                );
                Ok(removed)
            }
            "move" => {
                let ids: Vec<String> = match call.arg(0) {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(a) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
                    _ => return Err(ApiError::invalid("cookieStoreIds must be a string or array")),
                };
                let Some(position) = call.arg(1).as_i64() else {
                    return Err(ApiError::invalid("position must be an integer"));
                };
                let mut p = self.platform.borrow_mut();
                let mut moved = vec![];
                for id in &ids {
                    let Some(pos) = p.containers.items.iter().position(|c| &c.cookie_store_id == id) else {
                        return Err(ApiError::runtime(format!("Invalid contextual identity: {id}")));
                    };
                    moved.push(p.containers.items.remove(pos));
                }
                let insert_at = if position < 0 { p.containers.items.len() } else { (position as usize).min(p.containers.items.len()) };
                for (i, item) in moved.into_iter().enumerate() {
                    p.containers.items.insert(insert_at + i, item);
                }
                Ok(Value::Null)
            }
            "getSupportedColors" => Ok(json!(CONTAINER_COLORS.iter().map(|(c, _)| *c).collect::<Vec<_>>())),
            "getSupportedIcons" => Ok(json!(CONTAINER_ICONS)),
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("contextualIdentities.{op}"))),
        }
    }

    // ---- find (Firefox) ---------------------------------------------------------------------------------------------

    fn find_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "find" => {
                let Some(query) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("queryphrase must be a string")));
                };
                let opts = call.arg(1);
                let mut req = if opts.is_object() { opts.clone() } else { json!({}) };
                req["query"] = json!(query);
                self.host_call(call, "host.find.find", req, reply, |_, v| Ok(if v.is_object() { v } else { json!({ "count": 0 }) }));
            }
            "highlightResults" => {
                let opts = if call.arg(0).is_object() { call.arg(0).clone() } else { json!({}) };
                self.host_call(call, "host.find.highlightResults", opts, reply, |_, _| Ok(Value::Null));
            }
            "removeHighlighting" => {
                let mut req = json!({});
                if let Some(tid) = call.arg(0).as_i64() {
                    req["tabId"] = json!(tid);
                }
                self.host_call(call, "host.find.removeHighlighting", req, reply, |_, _| Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("find.{op}")))),
        }
    }

    // ---- system.* (Chrome) ------------------------------------------------------------------------------------------

    fn system_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "cpu.getInfo" => {
                let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
                let arch = std::env::consts::ARCH;
                let processors: Vec<Value> =
                    (0..cpus).map(|_| json!({ "usage": { "user": 0, "kernel": 0, "idle": 0, "total": 0 } })).collect();
                reply(Ok(json!({
                    "numOfProcessors": cpus,
                    "archName": arch,
                    "modelName": "",
                    "features": [],
                    "processors": processors,
                    "temperatures": [],
                })));
            }
            "memory.getInfo" => {
                self.host_call(call, "host.system.memory", json!({}), reply, |_, v| {
                    Ok(if v.is_object() && v.get("capacity").is_some() { v } else { json!({ "capacity": 0, "availableCapacity": 0 }) })
                });
            }
            "storage.getInfo" => {
                self.host_call(call, "host.system.storage", json!({}), reply, |_, v| Ok(if v.is_array() { v } else { json!([]) }));
            }
            "display.getInfo" => {
                self.host_call(call, "host.system.display", json!({}), reply, |_, v| Ok(if v.is_array() { v } else { json!([]) }));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("system.{op}")))),
        }
    }

    // ---- fontSettings (Chrome) --------------------------------------------------------------------------------------

    fn font_settings_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let script_and_family = |v: &Value| -> Result<(String, String), ApiError> {
            let Some(family) = v.get("genericFamily").and_then(Value::as_str) else {
                return Err(ApiError::invalid("details.genericFamily is required"));
            };
            if !matches!(family, "standard" | "sansserif" | "serif" | "fixed" | "cursive" | "fantasy" | "math") {
                return Err(ApiError::invalid(format!("Invalid genericFamily '{family}'")));
            }
            let script = v.get("script").and_then(Value::as_str).unwrap_or("Zyyy").to_string();
            Ok((script, family.to_string()))
        };
        match op {
            "getFont" => match script_and_family(call.arg(0)) {
                Err(e) => reply(Err(e)),
                Ok(key) => {
                    let font_id = self.platform.borrow().fonts.fonts.get(&key).cloned().unwrap_or_default();
                    reply(Ok(json!({ "fontId": font_id, "levelOfControl": "controllable_by_this_extension" })));
                }
            },
            "setFont" => match script_and_family(call.arg(0)) {
                Err(e) => reply(Err(e)),
                Ok((script, family)) => {
                    let Some(font_id) = call.arg(0).get("fontId").and_then(Value::as_str) else {
                        return reply(Err(ApiError::invalid("details.fontId is required")));
                    };
                    self.platform.borrow_mut().fonts.fonts.insert((script.clone(), family.clone()), font_id.to_string());
                    self.broadcast_permitted(
                        "fontSettings",
                        "fontSettings.onFontChanged",
                        vec![json!({ "fontId": font_id, "genericFamily": family, "script": script, "levelOfControl": "controlled_by_this_extension" })],
                    );
                    reply(Ok(Value::Null));
                }
            },
            "clearFont" => match script_and_family(call.arg(0)) {
                Err(e) => reply(Err(e)),
                Ok((script, family)) => {
                    self.platform.borrow_mut().fonts.fonts.remove(&(script.clone(), family.clone()));
                    self.broadcast_permitted(
                        "fontSettings",
                        "fontSettings.onFontChanged",
                        vec![json!({ "fontId": "", "genericFamily": family, "script": script, "levelOfControl": "controllable_by_this_extension" })],
                    );
                    reply(Ok(Value::Null));
                }
            },
            "getFontList" => {
                self.host_call(call, "host.fontSettings.getFontList", json!({}), reply, |_, v| {
                    Ok(if v.is_array() { v } else { json!([]) })
                });
            }
            "getDefaultFontSize" => {
                let sz = self.platform.borrow().fonts.default_font_size;
                reply(Ok(json!({ "pixelSize": sz, "levelOfControl": "controllable_by_this_extension" })));
            }
            "setDefaultFontSize" => {
                let Some(sz) = call.arg(0).get("pixelSize").and_then(Value::as_i64).filter(|&n| n >= 0) else {
                    return reply(Err(ApiError::invalid("details.pixelSize must be a non-negative integer")));
                };
                self.platform.borrow_mut().fonts.default_font_size = sz;
                self.broadcast_permitted(
                    "fontSettings",
                    "fontSettings.onDefaultFontSizeChanged",
                    vec![json!({ "pixelSize": sz, "levelOfControl": "controlled_by_this_extension" })],
                );
                reply(Ok(Value::Null));
            }
            "clearDefaultFontSize" => {
                self.platform.borrow_mut().fonts.default_font_size = 16;
                self.broadcast_permitted(
                    "fontSettings",
                    "fontSettings.onDefaultFontSizeChanged",
                    vec![json!({ "pixelSize": 16, "levelOfControl": "controllable_by_this_extension" })],
                );
                reply(Ok(Value::Null));
            }
            "getDefaultFixedFontSize" => {
                let sz = self.platform.borrow().fonts.default_fixed_font_size;
                reply(Ok(json!({ "pixelSize": sz, "levelOfControl": "controllable_by_this_extension" })));
            }
            "setDefaultFixedFontSize" => {
                let Some(sz) = call.arg(0).get("pixelSize").and_then(Value::as_i64).filter(|&n| n >= 0) else {
                    return reply(Err(ApiError::invalid("details.pixelSize must be a non-negative integer")));
                };
                self.platform.borrow_mut().fonts.default_fixed_font_size = sz;
                self.broadcast_permitted(
                    "fontSettings",
                    "fontSettings.onDefaultFixedFontSizeChanged",
                    vec![json!({ "pixelSize": sz, "levelOfControl": "controlled_by_this_extension" })],
                );
                reply(Ok(Value::Null));
            }
            "clearDefaultFixedFontSize" => {
                self.platform.borrow_mut().fonts.default_fixed_font_size = 13;
                self.broadcast_permitted(
                    "fontSettings",
                    "fontSettings.onDefaultFixedFontSizeChanged",
                    vec![json!({ "pixelSize": 13, "levelOfControl": "controllable_by_this_extension" })],
                );
                reply(Ok(Value::Null));
            }
            "getMinimumFontSize" => {
                let sz = self.platform.borrow().fonts.minimum_font_size;
                reply(Ok(json!({ "pixelSize": sz, "levelOfControl": "controllable_by_this_extension" })));
            }
            "setMinimumFontSize" => {
                let Some(sz) = call.arg(0).get("pixelSize").and_then(Value::as_i64).filter(|&n| n >= 0) else {
                    return reply(Err(ApiError::invalid("details.pixelSize must be a non-negative integer")));
                };
                self.platform.borrow_mut().fonts.minimum_font_size = sz;
                self.broadcast_permitted(
                    "fontSettings",
                    "fontSettings.onMinimumFontSizeChanged",
                    vec![json!({ "pixelSize": sz, "levelOfControl": "controlled_by_this_extension" })],
                );
                reply(Ok(Value::Null));
            }
            "clearMinimumFontSize" => {
                self.platform.borrow_mut().fonts.minimum_font_size = 0;
                self.broadcast_permitted(
                    "fontSettings",
                    "fontSettings.onMinimumFontSizeChanged",
                    vec![json!({ "pixelSize": 0, "levelOfControl": "controllable_by_this_extension" })],
                );
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("fontSettings.{op}")))),
        }
    }

    // ---- permissions host-access requests (Chrome) & contextMenus extras (Firefox) ----------------------------------

    fn permissions_host_access_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        let req = call.arg(0);
        if !req.is_object() {
            return Err(ApiError::invalid("request must be an object"));
        }
        let has_target = req.get("documentId").and_then(Value::as_str).is_some() || req.get("tabId").and_then(Value::as_i64).is_some();
        if !has_target {
            return Err(ApiError::invalid("request must specify either documentId or tabId"));
        }
        if let Some(pat) = req.get("pattern").and_then(Value::as_str) {
            MatchPattern::parse(pat, MatchDialect::Chrome).map_err(|e| ApiError::invalid(format!("Invalid pattern: {e}")))?;
        }
        let mut p = self.platform.borrow_mut();
        let list = p.host_access_requests.entry(call.extension_id.clone()).or_default();
        match op {
            "addHostAccessRequest" => {
                list.retain(|existing| existing != req);
                list.push(req.clone());
                Ok(Value::Null)
            }
            "removeHostAccessRequest" => {
                list.retain(|existing| existing != req);
                Ok(Value::Null)
            }
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("permissions.{op}"))),
        }
    }

    fn context_menus_extra_api(&self, op: &str, call: &ApiCall) -> ApiResult {
        match op {
            "overrideContext" => {
                let opts = call.arg(0);
                if !opts.is_object() {
                    return Err(ApiError::invalid("contextOptions must be an object"));
                }
                if let Some(ctx) = opts.get("context").and_then(Value::as_str)
                    && !matches!(ctx, "bookmark" | "tab")
                {
                    return Err(ApiError::invalid(format!("Invalid override context '{ctx}'")));
                }
                Ok(Value::Null)
            }
            "getTargetElement" => {
                let Some(id) = call.arg(0).as_i64() else {
                    return Err(ApiError::invalid("targetElementId must be an integer"));
                };
                Ok(self.platform.borrow().target_elements.get(&id).cloned().unwrap_or(Value::Null))
            }
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("contextMenus.{op}"))),
        }
    }

    // ---- identity ---------------------------------------------------------------------------------------------------

    fn identity_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "getRedirectURL" => {
                let path = call.arg(0).as_str().unwrap_or("").trim_start_matches('/');
                let (ext_id, firefox) = self
                    .with_ext(call, |e| Ok((e.id.clone(), crate::browser_api::is_firefox(e))))
                    .unwrap_or_else(|_| (call.extension_id.clone(), false));
                let host = if firefox {
                    let clean: String = ext_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                    format!("{clean}.extensions.allizom.org")
                } else {
                    format!("{ext_id}.chromiumapp.org")
                };
                reply(Ok(json!(format!("https://{host}/{path}"))));
            }
            "launchWebAuthFlow" => {
                let details = call.arg(0);
                let Some(url) = details.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                let Some(parts) = split_url(url) else {
                    return reply(Err(ApiError::invalid("Invalid auth URL.")));
                };
                if parts.scheme != "https" && parts.scheme != "http" {
                    return reply(Err(ApiError::invalid("Auth URL must use http or https.")));
                }
                let interactive = details.get("interactive").and_then(Value::as_bool).unwrap_or(false);
                self.host_call(
                    call,
                    "host.identity.launchWebAuthFlow",
                    json!({ "url": url, "interactive": interactive }),
                    reply,
                    |_, v| Ok(v),
                );
            }
            "getAuthToken" => {
                let details = if call.arg(0).is_object() { call.arg(0).clone() } else { json!({}) };
                let ext_id = call.extension_id.clone();
                self.host_call(call, "host.identity.getAuthToken", details, reply, move |this, v| {
                    if let Some(tok) = v.get("token").and_then(Value::as_str).or_else(|| v.as_str()) {
                        let mut p = this.platform.borrow_mut();
                        let list = p.auth_tokens.entry(ext_id).or_default();
                        if !list.iter().any(|t| t == tok) {
                            list.push(tok.to_string());
                        }
                    }
                    Ok(v)
                });
            }
            "getProfileUserInfo" => {
                let details = if call.arg(0).is_object() { call.arg(0).clone() } else { json!({}) };
                self.host_call(call, "host.identity.getProfileUserInfo", details, reply, |_, v| {
                    Ok(if v.is_object() { v } else { json!({ "email": "", "id": "" }) })
                });
            }
            "removeCachedAuthToken" => {
                let Some(tok) = call.arg(0).get("token").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.token must be a string")));
                };
                if let Some(list) = self.platform.borrow_mut().auth_tokens.get_mut(&call.extension_id) {
                    list.retain(|t| t != tok);
                }
                reply(Ok(Value::Null));
            }
            "clearAllCachedAuthTokens" => {
                self.platform.borrow_mut().auth_tokens.remove(&call.extension_id);
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("identity.{op}")))),
        }
    }

    // ---- tts (Chrome) -----------------------------------------------------------------------------------------------

    fn tts_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "speak" => {
                let Some(utterance) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("utterance must be a string")));
                };
                let opts = call.arg(1);
                if let Some(rate) = opts.get("rate").and_then(Value::as_f64)
                    && !(0.1..=10.0).contains(&rate)
                {
                    return reply(Err(ApiError::invalid("options.rate must be between 0.1 and 10.0")));
                }
                if let Some(pitch) = opts.get("pitch").and_then(Value::as_f64)
                    && !(0.0..=2.0).contains(&pitch)
                {
                    return reply(Err(ApiError::invalid("options.pitch must be between 0.0 and 2.0")));
                }
                if let Some(vol) = opts.get("volume").and_then(Value::as_f64)
                    && !(0.0..=1.0).contains(&vol)
                {
                    return reply(Err(ApiError::invalid("options.volume must be between 0.0 and 1.0")));
                }
                {
                    let mut p = self.platform.borrow_mut();
                    p.tts_speaking = true;
                    p.tts_paused = false;
                }
                let mut req = if opts.is_object() { opts.clone() } else { json!({}) };
                req["utterance"] = json!(utterance);
                self.host_call(call, "host.tts.speak", req, reply, |_, _| Ok(Value::Null));
            }
            "stop" => {
                {
                    let mut p = self.platform.borrow_mut();
                    p.tts_speaking = false;
                    p.tts_paused = false;
                }
                self.host_call(call, "host.tts.stop", json!({}), reply, |_, _| Ok(Value::Null));
            }
            "pause" => {
                self.platform.borrow_mut().tts_paused = true;
                self.host_call(call, "host.tts.pause", json!({}), reply, |_, _| Ok(Value::Null));
            }
            "resume" => {
                self.platform.borrow_mut().tts_paused = false;
                self.host_call(call, "host.tts.resume", json!({}), reply, |_, _| Ok(Value::Null));
            }
            "isSpeaking" => reply(Ok(json!(self.platform.borrow().tts_speaking && !self.platform.borrow().tts_paused))),
            "getVoices" => {
                self.host_call(call, "host.tts.getVoices", json!({}), reply, |_, v| Ok(if v.is_array() { v } else { json!([]) }));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("tts.{op}")))),
        }
    }

    // ---- clipboard (Firefox) ----------------------------------------------------------------------------------------

    fn clipboard_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        if op != "setImageData" {
            return reply(Err(ApiError::new(ErrorKind::Unhandled, format!("clipboard.{op}"))));
        }
        let image_type = call.arg(1).as_str().unwrap_or("");
        if !matches!(image_type, "jpeg" | "png") {
            return reply(Err(ApiError::invalid("imageType must be 'jpeg' or 'png'")));
        }
        self.host_call(call, "host.clipboard.setImageData", json!({ "imageData": call.arg(0), "imageType": image_type }), reply, |_, _| {
            Ok(Value::Null)
        });
    }

    // ---- dns (Firefox) ----------------------------------------------------------------------------------------------

    fn dns_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        if op != "resolve" {
            return reply(Err(ApiError::new(ErrorKind::Unhandled, format!("dns.{op}"))));
        }
        let Some(hostname) = call.arg(0).as_str().filter(|h| !h.is_empty()) else {
            return reply(Err(ApiError::invalid("hostname must be a non-empty string")));
        };
        let flags = call.arg(1).as_array().cloned().unwrap_or_default();
        self.host_call(call, "host.dns.resolve", json!({ "hostname": hostname, "flags": flags }), reply, |_, v| {
            Ok(if v.is_object() { v } else { json!({ "addresses": [], "isTRR": false }) })
        });
    }

    // ---- captivePortal (Firefox) ------------------------------------------------------------------------------------

    fn captive_portal_api(&self, op: &str, _call: &ApiCall) -> ApiResult {
        let p = self.platform.borrow();
        match op {
            "getState" => {
                let s = if p.captive_portal_state.is_empty() { "not_captive" } else { p.captive_portal_state.as_str() };
                Ok(json!(s))
            }
            "getLastChecked" => Ok(json!(p.captive_portal_last_checked)),
            "canonicalURL" => Ok(json!("http://detectportal.firefox.com/canonical.html")),
            _ => Err(ApiError::new(ErrorKind::Unhandled, format!("captivePortal.{op}"))),
        }
    }

    // ---- tabs extras (zoomSettings, hide/show, captureTab, print, readerMode, warmup, moveInSuccession) --------------

    fn tabs_extra_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let default_tab = || -> Result<i64, ApiError> {
            let state = self.browser.borrow();
            let win = state.current_window(call.caller.tab_id).ok_or_else(|| ApiError::runtime("No current window"))?;
            state.active_tab_of(win).map(|t| t.id).ok_or_else(|| ApiError::runtime("No active tab"))
        };
        match op {
            "getZoomSettings" => {
                let tid = match call.arg(0).as_i64() {
                    Some(id) => id,
                    None => match default_tab() {
                        Ok(id) => id,
                        Err(e) => return reply(Err(e)),
                    },
                };
                if self.browser.borrow().tab(tid).is_none() {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tid}."))));
                }
                let settings = self
                    .platform
                    .borrow()
                    .zoom_settings
                    .get(&tid)
                    .cloned()
                    .unwrap_or_else(|| json!({ "mode": "automatic", "scope": "per-origin", "defaultZoomFactor": 1.0 }));
                reply(Ok(settings));
            }
            "setZoomSettings" => {
                let (tid, settings) = match call.arg(0) {
                    Value::Number(n) => (n.as_i64(), call.arg(1)),
                    _ => (default_tab().ok(), call.arg(0)),
                };
                let Some(tid) = tid else {
                    return reply(Err(ApiError::runtime("No active tab")));
                };
                if self.browser.borrow().tab(tid).is_none() {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tid}."))));
                }
                if !settings.is_object() {
                    return reply(Err(ApiError::invalid("zoomSettings must be an object")));
                }
                let mode = settings.get("mode").and_then(Value::as_str).unwrap_or("automatic");
                let scope = settings.get("scope").and_then(Value::as_str).unwrap_or("per-origin");
                if !matches!(mode, "automatic" | "manual" | "disabled") {
                    return reply(Err(ApiError::invalid(format!("Invalid zoom mode '{mode}'"))));
                }
                if !matches!(scope, "per-origin" | "per-tab") {
                    return reply(Err(ApiError::invalid(format!("Invalid zoom scope '{scope}'"))));
                }
                let default_zoom = settings.get("defaultZoomFactor").and_then(Value::as_f64).unwrap_or(1.0);
                self.platform
                    .borrow_mut()
                    .zoom_settings
                    .insert(tid, json!({ "mode": mode, "scope": scope, "defaultZoomFactor": default_zoom }));
                reply(Ok(Value::Null));
            }
            "hide" | "show" => {
                if op == "hide" && !self.with_ext(call, |e| Ok(e.permissions.has_api("tabHide"))).unwrap_or(false) {
                    return reply(Err(ApiError::new(ErrorKind::PermissionDenied, "'tabs.hide' requires the \"tabHide\" permission")));
                }
                let ids: Result<Vec<i64>, ApiError> = match call.arg(0) {
                    Value::Number(n) => n.as_i64().map(|i| vec![i]).ok_or_else(|| ApiError::invalid("tabIds must be integers")),
                    Value::Array(a) => a.iter().map(|v| v.as_i64().ok_or_else(|| ApiError::invalid("tabIds must be integers"))).collect(),
                    _ => Err(ApiError::invalid("tabIds must be an integer or array of integers")),
                };
                let ids = match ids {
                    Ok(v) => v,
                    Err(e) => return reply(Err(e)),
                };
                let mut changed = vec![];
                {
                    let state = self.browser.borrow();
                    let mut p = self.platform.borrow_mut();
                    for id in ids {
                        if let Some(t) = state.tab(id) {
                            if op == "hide" && !t.pinned && !t.active {
                                p.hidden_tabs.insert(id);
                                changed.push(id);
                            } else if op == "show" {
                                p.hidden_tabs.remove(&id);
                                changed.push(id);
                            }
                        }
                    }
                }
                if op == "hide" {
                    reply(Ok(json!(changed)));
                } else {
                    reply(Ok(Value::Null));
                }
            }
            "captureTab" => {
                let (tid, opts) = match call.arg(0) {
                    Value::Number(n) => (n.as_i64(), call.arg(1)),
                    _ => (default_tab().ok(), call.arg(0)),
                };
                let Some(tid) = tid else {
                    return reply(Err(ApiError::runtime("No active tab")));
                };
                let format = opts.get("format").and_then(Value::as_str).unwrap_or("png");
                let quality = opts.get("quality").and_then(Value::as_i64).unwrap_or(92);
                self.host_call(call, "host.tabs.capture", json!({ "tabId": tid, "format": format, "quality": quality }), reply, |_, v| {
                    Ok(v.get("dataUrl").cloned().unwrap_or(Value::Null))
                });
            }
            "print" | "printPreview" | "saveAsPDF" | "toggleReaderMode" => {
                let mut req = json!({});
                if let Some(tid) = call.arg(0).as_i64() {
                    req["tabId"] = json!(tid);
                } else if call.arg(0).is_object() {
                    req = call.arg(0).clone();
                }
                let prim = format!("host.tabs.{op}");
                self.host_call(call, &prim, req, reply, |_, v| Ok(v));
            }
            "warmup" => {
                let Some(tid) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("tabId must be an integer")));
                };
                if self.browser.borrow().tab(tid).is_none() {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tid}."))));
                }
                reply(Ok(Value::Null));
            }
            "moveInSuccession" => {
                let Some(arr) = call.arg(0).as_array() else {
                    return reply(Err(ApiError::invalid("tabIds must be an array")));
                };
                let ids: Vec<i64> = arr.iter().filter_map(Value::as_i64).collect();
                let successor = call.arg(1).as_i64().unwrap_or(-1);
                let mut p = self.platform.borrow_mut();
                for pair in ids.windows(2) {
                    p.tab_successors.insert(pair[0], pair[1]);
                }
                if let Some(&last) = ids.last() {
                    if successor >= 0 {
                        p.tab_successors.insert(last, successor);
                    } else {
                        p.tab_successors.remove(&last);
                    }
                }
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("tabs.{op}")))),
        }
    }

    // ---- runtime extras (native messaging, update check, restart) ---------------------------------------------------

    fn runtime_extra_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "sendNativeMessage" => {
                if !self.with_ext(call, |e| Ok(e.permissions.has_api("nativeMessaging"))).unwrap_or(false) {
                    return reply(Err(ApiError::new(
                        ErrorKind::PermissionDenied,
                        "'runtime.sendNativeMessage' requires the \"nativeMessaging\" permission",
                    )));
                }
                let Some(app) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("application must be a string")));
                };
                let valid = !app.is_empty()
                    && !app.starts_with('.')
                    && !app.ends_with('.')
                    && !app.contains("..")
                    && app.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'));
                if !valid {
                    return reply(Err(ApiError::invalid(format!("Invalid native messaging host name '{app}'."))));
                }
                self.host_call(
                    call,
                    "host.runtime.sendNativeMessage",
                    json!({ "application": app, "message": call.arg(1) }),
                    reply,
                    |_, v| Ok(v),
                );
            }
            "requestUpdateCheck" => reply(Ok(json!({ "status": "no_update", "version": "" }))),
            "restart" => self.host_call(call, "host.runtime.restart", json!({}), reply, |_, _| Ok(Value::Null)),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("runtime.{op}")))),
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

    #[derive(Default)]
    struct FakeHost {
        calls: RefCell<Vec<(String, Value)>>,
        events: RefCell<Vec<(String, String, Vec<Value>)>>,
        now: RefCell<i64>,
    }

    impl ExtensionHost for FakeHost {
        fn read_storage(&self, _: &str, _: &str) -> Option<String> {
            None
        }
        fn write_storage(&self, _: &str, _: &str, _: &str) {}
        fn now_ms(&self) -> i64 {
            *self.now.borrow()
        }
        fn ui_locale(&self) -> String {
            "en-US".into()
        }
        fn dispatch_event(&self, id: &str, event: &str, args: Vec<Value>) {
            self.events.borrow_mut().push((id.into(), event.into(), args));
        }
        fn deliver_to_context(&self, _: u64, _: Value) {}
        fn call_browser(&self, call: &ApiCall, reply: Reply) {
            self.calls.borrow_mut().push((call.function.clone(), call.args.first().cloned().unwrap_or(Value::Null)));
            reply(Ok(Value::Null));
        }
        fn request_permissions(&self, _: &str, _: &PermissionRequest, reply: Box<dyn FnOnce(bool)>) {
            reply(true);
        }
    }

    const ID: &str = "abcdefghijklmnopabcdefghijklmnop";

    fn setup(manifest_json: &str) -> (Dispatcher, Rc<FakeHost>) {
        let host = Rc::new(FakeHost::default());
        let d = Dispatcher::new(host.clone(), Registry::builtin());
        let m = crate::manifest::parse_manifest(manifest_json).manifest.unwrap();
        d.install(ID, m, InstallOptions { base_url: format!("chrome-extension://{ID}/"), locales: vec![], files: Files::new() });
        (d, host)
    }

    fn call(d: &Dispatcher, f: &str, args: Vec<Value>) -> ApiResult {
        call_as(d, Caller::background(), f, args)
    }

    fn call_as(d: &Dispatcher, caller: Caller, f: &str, args: Vec<Value>) -> ApiResult {
        let out = Rc::new(RefCell::new(None));
        let sink = out.clone();
        d.dispatch(ApiCall { extension_id: ID.into(), function: f.into(), args, caller }, Box::new(move |r| *sink.borrow_mut() = Some(r)));
        out.borrow_mut().take().expect("synchronous reply")
    }

    #[test]
    fn public_suffix_uses_real_psl() {
        let (d, _) = setup(r#"{"manifest_version":3,"name":"P","version":"1"}"#);
        assert_eq!(call(&d, "publicSuffix.getDomain", vec![json!("www.bbc.co.uk")]).unwrap(), "bbc.co.uk");
        assert_eq!(call(&d, "publicSuffix.getKnownSuffix", vec![json!("https://a.b.co.uk/path")]).unwrap(), "co.uk");
        assert_eq!(call(&d, "publicSuffix.isKnownSuffix", vec![json!("co.uk")]).unwrap(), true);
        assert_eq!(call(&d, "publicSuffix.isKnownSuffix", vec![json!("bbc.co.uk")]).unwrap(), false);
        assert_eq!(call(&d, "publicSuffix.getDomain", vec![json!("127.0.0.1")]).unwrap(), Value::Null);
    }

    #[test]
    fn storage_access_level_controls_content_script_access() {
        let (d, _) = setup(r#"{"manifest_version":3,"name":"S","version":"1","permissions":["storage"]}"#);
        let cs = Caller { context: ContextKind::ContentScript, context_id: None, tab_id: Some(1), frame_id: Some(0), url: None };
        // session defaults to TRUSTED_CONTEXTS -> content script denied.
        assert_eq!(call_as(&d, cs.clone(), "storage.session.get", vec![]).unwrap_err().kind, ErrorKind::PermissionDenied);
        // Unlocking from background lets content scripts read session storage.
        call(&d, "storage.session.setAccessLevel", vec![json!({"accessLevel": "TRUSTED_AND_UNTRUSTED_CONTEXTS"})]).unwrap();
        call(&d, "storage.session.set", vec![json!({"k": 42})]).unwrap();
        assert_eq!(call_as(&d, cs.clone(), "storage.session.get", vec![json!("k")]).unwrap(), json!({"k": 42}));
        // Content scripts cannot call setAccessLevel.
        assert_eq!(
            call_as(&d, cs, "storage.local.setAccessLevel", vec![json!({"accessLevel": "TRUSTED_CONTEXTS"})]).unwrap_err().kind,
            ErrorKind::PermissionDenied
        );
        assert_eq!(call(&d, "storage.managed.getBytesInUse", vec![]).unwrap(), json!(0));
    }

    #[test]
    fn management_enable_disable_warnings_and_events() {
        let (d, host) = setup(r#"{"manifest_version":3,"name":"M","version":"1","permissions":["management"]}"#);
        let other = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let m2 = crate::manifest::parse_manifest(r#"{"manifest_version":3,"name":"Other","version":"1","permissions":["bookmarks"]}"#)
            .manifest
            .unwrap();
        d.install(other, m2, InstallOptions { base_url: format!("chrome-extension://{other}/"), locales: vec![], files: Files::new() });
        assert!(host.events.borrow().iter().any(|(to, e, _)| to == ID && e == "management.onInstalled"));

        call(&d, "management.setEnabled", vec![json!(other), json!(false)]).unwrap();
        assert_eq!(call(&d, "management.get", vec![json!(other)]).unwrap()["enabled"], false);
        assert!(host.events.borrow().iter().any(|(_, e, _)| e == "management.onDisabled"));

        let warnings = call(&d, "management.getPermissionWarningsById", vec![json!(other)]).unwrap();
        assert!(warnings.as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("bookmarks")));
        let from_manifest = call(
            &d,
            "management.getPermissionWarningsByManifest",
            vec![json!(r#"{"manifest_version":3,"name":"X","version":"1","permissions":["history"]}"#)],
        )
        .unwrap();
        assert!(!from_manifest.as_array().unwrap().is_empty());

        call(&d, "management.uninstall", vec![json!(other)]).unwrap();
        assert!(!d.is_installed(other));
        assert!(host.events.borrow().iter().any(|(_, e, _)| e == "management.onUninstalled"));
    }

    #[test]
    fn notifications_idle_omnibox_and_tab_groups() {
        let (d, host) =
            setup(r#"{"manifest_version":3,"name":"N","version":"1","permissions":["notifications","idle","tabGroups","tabs"]}"#);
        let nid = call(&d, "notifications.create", vec![json!({"type": "basic", "title": "Hi", "message": "Body", "iconUrl": "icon.png"})])
            .unwrap();
        let nid = nid.as_str().unwrap().to_string();
        assert_eq!(call(&d, "notifications.getAll", vec![]).unwrap()[&nid], true);
        assert_eq!(call(&d, "notifications.update", vec![json!(&nid), json!({"message": "Updated"})]).unwrap(), true);
        assert_eq!(call(&d, "notifications.clear", vec![json!(&nid)]).unwrap(), true);

        // Idle per-extension interval.
        call(&d, "idle.setDetectionInterval", vec![json!(30)]).unwrap();
        d.host_event("idleState", &json!({"idleSeconds": 45, "locked": false}));
        assert_eq!(call(&d, "idle.queryState", vec![json!(30)]).unwrap(), "idle");
        assert_eq!(call(&d, "idle.queryState", vec![json!(60)]).unwrap(), "active");
        assert!(host.events.borrow().iter().any(|(_, e, a)| e == "idle.onStateChanged" && a[0] == "idle"));

        // Omnibox.
        call(&d, "omnibox.setDefaultSuggestion", vec![json!({"description": "Search %s"})]).unwrap();
        d.host_event("omniboxEvent", &json!({"extensionId": ID, "event": "inputChanged", "text": "rust"}));
        assert!(host.events.borrow().iter().any(|(_, e, a)| e == "omnibox.onInputChanged" && a[0] == "rust"));

        // TabGroups + tabs.group / tabs.ungroup.
        d.host_event(
            "browserState",
            &json!({"windows": [{"id": 1, "focused": true, "tabs": [{"id": 1, "url": "https://a.test/"}, {"id": 2, "url": "https://b.test/"}]}]}),
        );
        let gid = call(&d, "tabs.group", vec![json!({"tabIds": [1, 2]})]).unwrap().as_i64().unwrap();
        let updated = call(&d, "tabGroups.update", vec![json!(gid), json!({"title": "Dev", "color": "blue", "collapsed": true})]).unwrap();
        assert_eq!(updated["title"], "Dev");
        assert_eq!(call(&d, "tabGroups.query", vec![json!({"color": "blue"})]).unwrap().as_array().unwrap().len(), 1);
        call(&d, "tabs.ungroup", vec![json!([1, 2])]).unwrap();
        assert!(call(&d, "tabGroups.get", vec![json!(gid)]).is_err());
    }

    #[test]
    fn user_scripts_offscreen_side_panels_theme_power_containers_fonts_and_settings() {
        let (d, host) = setup(
            r#"{"manifest_version":3,"name":"All","version":"1","permissions":[
                "userScripts","offscreen","sidePanel","theme","power","contextualIdentities",
                "fontSettings","privacy","browserSettings","identity","tts","system.cpu","captivePortal","nativeMessaging","tabHide"
            ]}"#,
        );
        // userScripts
        call(&d, "userScripts.register", vec![json!([{"id": "u1", "matches": ["https://a.test/*"], "js": [{"code": "1+1"}]}])]).unwrap();
        assert_eq!(call(&d, "userScripts.getScripts", vec![]).unwrap().as_array().unwrap().len(), 1);
        call(&d, "userScripts.configureWorld", vec![json!({"csp": "script-src 'self'", "messaging": true})]).unwrap();
        assert_eq!(call(&d, "userScripts.getWorldConfigurations", vec![]).unwrap()[0]["messaging"], true);
        call(&d, "userScripts.unregister", vec![json!({"ids": ["u1"]})]).unwrap();

        // offscreen + runtime.getContexts
        assert_eq!(call(&d, "offscreen.hasDocument", vec![]).unwrap(), false);
        call(&d, "offscreen.createDocument", vec![json!({"url": "off.html", "reasons": ["DOM_PARSER"], "justification": "parse"})])
            .unwrap();
        assert_eq!(call(&d, "offscreen.hasDocument", vec![]).unwrap(), true);
        let ctxs = call(&d, "runtime.getContexts", vec![json!({"contextTypes": ["OFFSCREEN_DOCUMENT"]})]).unwrap();
        assert_eq!(ctxs.as_array().unwrap().len(), 1);
        call(&d, "offscreen.closeDocument", vec![]).unwrap();

        // sidePanel & sidebarAction
        call(&d, "sidePanel.setOptions", vec![json!({"path": "panel.html", "enabled": true})]).unwrap();
        assert_eq!(call(&d, "sidePanel.getOptions", vec![json!({})]).unwrap()["path"], "panel.html");
        call(&d, "sidebarAction.setTitle", vec![json!({"title": "My Side"})]).unwrap();
        assert_eq!(call(&d, "sidebarAction.getTitle", vec![json!({})]).unwrap(), "My Side");
        call(&d, "sidebarAction.open", vec![]).unwrap();
        assert_eq!(call(&d, "sidebarAction.isOpen", vec![json!({})]).unwrap(), true);

        // theme & power
        call(&d, "theme.update", vec![json!({"colors": {"frame": "#112233"}})]).unwrap();
        assert_eq!(call(&d, "theme.getCurrent", vec![]).unwrap()["colors"]["frame"], "#112233");
        call(&d, "power.requestKeepAwake", vec![json!("display")]).unwrap();
        assert!(host.calls.borrow().iter().any(|(f, a)| f == "host.power.setLevel" && a["level"] == "display"));
        call(&d, "power.releaseKeepAwake", vec![]).unwrap();

        // contextualIdentities
        assert_eq!(call(&d, "contextualIdentities.query", vec![json!({})]).unwrap().as_array().unwrap().len(), 4);
        let c = call(&d, "contextualIdentities.create", vec![json!({"name": "Dev", "color": "purple", "icon": "tree"})]).unwrap();
        let cid = c["cookieStoreId"].as_str().unwrap().to_string();
        call(&d, "contextualIdentities.update", vec![json!(&cid), json!({"name": "Dev2"})]).unwrap();
        assert_eq!(call(&d, "contextualIdentities.get", vec![json!(&cid)]).unwrap()["name"], "Dev2");
        call(&d, "contextualIdentities.remove", vec![json!(&cid)]).unwrap();

        // fontSettings
        call(&d, "fontSettings.setFont", vec![json!({"genericFamily": "sansserif", "fontId": "Inter"})]).unwrap();
        assert_eq!(call(&d, "fontSettings.getFont", vec![json!({"genericFamily": "sansserif"})]).unwrap()["fontId"], "Inter");
        call(&d, "fontSettings.setDefaultFontSize", vec![json!({"pixelSize": 18})]).unwrap();
        assert_eq!(call(&d, "fontSettings.getDefaultFontSize", vec![]).unwrap()["pixelSize"], 18);

        // privacy & browserSettings via internal __setting.*
        assert_eq!(call(&d, "__setting.get", vec![json!("privacy.network.networkPredictionEnabled")]).unwrap()["value"], true);
        call(&d, "__setting.set", vec![json!("privacy.network.networkPredictionEnabled"), json!({"value": false})]).unwrap();
        assert_eq!(
            call(&d, "__setting.get", vec![json!("privacy.network.networkPredictionEnabled")]).unwrap()["levelOfControl"],
            "controlled_by_this_extension"
        );
        call(&d, "__setting.clear", vec![json!("privacy.network.networkPredictionEnabled")]).unwrap();
        assert_eq!(call(&d, "__setting.get", vec![json!("privacy.network.networkPredictionEnabled")]).unwrap()["value"], true);

        // identity, tts, system.cpu, captivePortal, runtime extras
        assert_eq!(call(&d, "identity.getRedirectURL", vec![json!("cb")]).unwrap(), format!("https://{ID}.chromiumapp.org/cb"));
        call(&d, "tts.speak", vec![json!("Hello"), json!({"rate": 1.2})]).unwrap();
        assert_eq!(call(&d, "tts.isSpeaking", vec![]).unwrap(), true);
        call(&d, "tts.stop", vec![]).unwrap();
        assert_eq!(call(&d, "tts.isSpeaking", vec![]).unwrap(), false);
        assert!(call(&d, "system.cpu.getInfo", vec![]).unwrap()["numOfProcessors"].as_u64().unwrap() >= 1);
        d.host_event("captivePortalState", &json!({"state": "unlocked_portal"}));
        assert_eq!(call(&d, "captivePortal.getState", vec![]).unwrap(), "unlocked_portal");
        assert_eq!(call(&d, "runtime.requestUpdateCheck", vec![]).unwrap()["status"], "no_update");
        call(&d, "runtime.sendNativeMessage", vec![json!("com.example.host"), json!({"ping": 1})]).unwrap();
    }
}
