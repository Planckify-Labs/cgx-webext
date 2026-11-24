//! The browser's windows and tabs, as the core sees them.
//!
//! The host does not implement `tabs.query`, `windows.getAll`, `tabs.onUpdated` and friends. It reports what it has (a
//! [`BrowserState`] snapshot, pushed whenever something changes) and carries out a few primitive commands (`host.tabs.create`,
//! `host.tabs.update`, ...). Everything an extension can observe is computed here from the snapshot:
//!
//! - the `tabs.Tab` and `windows.Window` objects, with `url`, `title` and `favIconUrl` only for extensions allowed to see
//!   them (the `tabs` permission, a host permission for the tab's URL, or an `activeTab` grant);
//! - which tabs a `tabs.query` matches;
//! - the events (`tabs.onCreated`, `onUpdated`, `onActivated`, `onMoved`, `onAttached`/`onDetached`, `onRemoved`,
//!   `onHighlighted`, `onZoomChange`, `windows.onCreated`, `onRemoved`, `onFocusChanged`, `onBoundsChanged`), found by
//!   comparing the previous snapshot with the new one.
//!
//! So every browser gets the same semantics, and they are tested here rather than in each browser.

use crate::match_pattern::{MatchDialect, MatchPatternSet, glob_match, split_url};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub const TAB_ID_NONE: i64 = -1;
pub const WINDOW_ID_NONE: i64 = -1;
pub const WINDOW_ID_CURRENT: i64 = -2;

#[derive(Debug, Clone, PartialEq)]
pub struct TabState {
    pub id: i64,
    /// Filled from the window that holds the tab.
    pub window_id: i64,
    /// Filled from the tab's position in its window.
    pub index: usize,
    pub active: bool,
    pub pinned: bool,
    pub highlighted: bool,
    pub audible: bool,
    pub muted: bool,
    pub discarded: bool,
    /// `loading`, `complete` or `unloaded`.
    pub status: String,
    pub url: String,
    pub pending_url: Option<String>,
    pub title: String,
    pub fav_icon_url: Option<String>,
    pub incognito: bool,
    pub width: i64,
    pub height: i64,
    pub opener_tab_id: Option<i64>,
    pub zoom: f64,
    pub last_accessed: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowState {
    pub id: i64,
    /// `normal`, `popup`, `panel`, `app` or `devtools`.
    pub kind: String,
    /// `normal`, `minimized`, `maximized`, `fullscreen` or `locked-fullscreen`.
    pub state: String,
    pub focused: bool,
    pub incognito: bool,
    pub always_on_top: bool,
    pub left: i64,
    pub top: i64,
    pub width: i64,
    pub height: i64,
    pub tabs: Vec<TabState>,
}

/// A snapshot of every window and tab, as the host reports it:
///
/// ```json
/// { "windows": [ { "id": 1, "type": "normal", "state": "normal", "focused": true, "incognito": false,
///                  "left": 0, "top": 0, "width": 1280, "height": 800,
///                  "tabs": [ { "id": 7, "active": true, "pinned": false, "url": "https://a.test/", "title": "A",
///                              "favIconUrl": "...", "status": "complete", "audible": false, "muted": false,
///                              "width": 1280, "height": 700, "openerTabId": 3, "zoom": 1.0 } ] } ],
///   "lastFocusedWindowId": 1 }
/// ```
///
/// Tab order in `tabs` is the order of the tab strip. Tab and window ids must be unique and never reused in a session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BrowserState {
    pub windows: Vec<WindowState>,
    /// The window that was focused most recently, even if no Ladybird window is focused now.
    pub last_focused_window: Option<i64>,
}

fn int(v: &Value, key: &str) -> Option<i64> {
    v.get(key).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
}

fn boolean(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(String::from)
}

impl BrowserState {
    pub fn from_json(v: &Value) -> Result<BrowserState, String> {
        let windows = v.get("windows").and_then(Value::as_array).ok_or("'windows' must be an array")?;
        let mut state = BrowserState { windows: vec![], last_focused_window: int(v, "lastFocusedWindowId") };
        let mut seen_tabs = HashSet::new();
        let mut seen_windows = HashSet::new();
        for w in windows {
            let id = int(w, "id").ok_or("every window needs an integer 'id'")?;
            if !seen_windows.insert(id) {
                return Err(format!("window id {id} appears twice"));
            }
            let mut window = WindowState {
                id,
                kind: string(w, "type").unwrap_or_else(|| "normal".into()),
                state: string(w, "state").unwrap_or_else(|| "normal".into()),
                focused: boolean(w, "focused"),
                incognito: boolean(w, "incognito"),
                always_on_top: boolean(w, "alwaysOnTop"),
                left: int(w, "left").unwrap_or(0),
                top: int(w, "top").unwrap_or(0),
                width: int(w, "width").unwrap_or(0),
                height: int(w, "height").unwrap_or(0),
                tabs: vec![],
            };
            for (index, t) in w.get("tabs").and_then(Value::as_array).into_iter().flatten().enumerate() {
                let tab_id = int(t, "id").ok_or("every tab needs an integer 'id'")?;
                if !seen_tabs.insert(tab_id) {
                    return Err(format!("tab id {tab_id} appears twice"));
                }
                window.tabs.push(TabState {
                    id: tab_id,
                    window_id: id,
                    index,
                    active: boolean(t, "active"),
                    pinned: boolean(t, "pinned"),
                    // A tab strip without multi-selection highlights exactly the active tab.
                    highlighted: t.get("highlighted").and_then(Value::as_bool).unwrap_or_else(|| boolean(t, "active")),
                    audible: boolean(t, "audible"),
                    muted: boolean(t, "muted"),
                    discarded: boolean(t, "discarded"),
                    status: string(t, "status").unwrap_or_else(|| "complete".into()),
                    url: string(t, "url").unwrap_or_default(),
                    pending_url: string(t, "pendingUrl").filter(|s| !s.is_empty()),
                    title: string(t, "title").unwrap_or_default(),
                    fav_icon_url: string(t, "favIconUrl").filter(|s| !s.is_empty()),
                    incognito: t.get("incognito").and_then(Value::as_bool).unwrap_or(window.incognito),
                    width: int(t, "width").unwrap_or(window.width),
                    height: int(t, "height").unwrap_or(window.height),
                    opener_tab_id: int(t, "openerTabId"),
                    zoom: t.get("zoom").and_then(Value::as_f64).filter(|z| *z > 0.0).unwrap_or(1.0),
                    last_accessed: t.get("lastAccessed").and_then(Value::as_f64),
                });
            }
            state.windows.push(window);
        }
        if state.last_focused_window.is_none() {
            state.last_focused_window = state.windows.iter().find(|w| w.focused).map(|w| w.id);
        }
        Ok(state)
    }

    pub fn tab(&self, id: i64) -> Option<&TabState> {
        self.tabs().find(|t| t.id == id)
    }

    pub fn window(&self, id: i64) -> Option<&WindowState> {
        self.windows.iter().find(|w| w.id == id)
    }

    pub fn tabs(&self) -> impl Iterator<Item = &TabState> {
        self.windows.iter().flat_map(|w| w.tabs.iter())
    }

    pub fn focused_window(&self) -> Option<i64> {
        self.windows.iter().find(|w| w.focused).map(|w| w.id)
    }

    /// The window Chrome calls "current" for a caller: the window of the caller's tab, else the last focused window (for
    /// background contexts and popups).
    pub fn current_window(&self, caller_tab: Option<i64>) -> Option<i64> {
        caller_tab
            .and_then(|t| self.tab(t))
            .map(|t| t.window_id)
            .or(self.last_focused_window.filter(|id| self.window(*id).is_some()))
            .or_else(|| self.windows.iter().find(|w| w.kind == "normal").map(|w| w.id))
    }

    pub fn active_tab_of(&self, window_id: i64) -> Option<&TabState> {
        self.window(window_id).and_then(|w| w.tabs.iter().find(|t| t.active))
    }
}

/// What an extension may see of a tab. Built by the dispatcher from the extension's permissions.
pub struct Visibility<'a> {
    /// The `tabs` permission: every tab's URL and title.
    pub all_tabs: bool,
    /// Host permissions and `activeTab` grants, per tab URL.
    pub can_access: &'a dyn Fn(i64, &str) -> bool,
    /// Firefox-style extensions get Firefox's extra `Tab` fields.
    pub firefox: bool,
}

impl Visibility<'_> {
    pub fn sees_url(&self, tab: &TabState) -> bool {
        self.all_tabs || (self.can_access)(tab.id, &tab.url)
    }
}

pub fn tab_json(tab: &TabState, vis: &Visibility) -> Value {
    let mut o = Map::new();
    o.insert("id".into(), json!(tab.id));
    o.insert("index".into(), json!(tab.index));
    o.insert("windowId".into(), json!(tab.window_id));
    o.insert("active".into(), json!(tab.active));
    o.insert("highlighted".into(), json!(tab.highlighted));
    o.insert("selected".into(), json!(tab.highlighted));
    o.insert("pinned".into(), json!(tab.pinned));
    o.insert("audible".into(), json!(tab.audible));
    o.insert("discarded".into(), json!(tab.discarded));
    o.insert("autoDiscardable".into(), json!(true));
    o.insert("frozen".into(), json!(false));
    o.insert("mutedInfo".into(), json!({ "muted": tab.muted }));
    o.insert("status".into(), json!(tab.status));
    o.insert("incognito".into(), json!(tab.incognito));
    o.insert("width".into(), json!(tab.width));
    o.insert("height".into(), json!(tab.height));
    o.insert("groupId".into(), json!(-1));
    if let Some(opener) = tab.opener_tab_id {
        o.insert("openerTabId".into(), json!(opener));
    }
    if let Some(t) = tab.last_accessed {
        o.insert("lastAccessed".into(), json!(t));
    }
    if vis.sees_url(tab) {
        o.insert("url".into(), json!(tab.url));
        o.insert("title".into(), json!(tab.title));
        if let Some(p) = &tab.pending_url {
            o.insert("pendingUrl".into(), json!(p));
        }
        if let Some(f) = &tab.fav_icon_url {
            o.insert("favIconUrl".into(), json!(f));
        }
    }
    if vis.firefox {
        o.insert("cookieStoreId".into(), json!(if tab.incognito { "firefox-private" } else { "firefox-default" }));
        o.insert("attention".into(), json!(false));
        o.insert("hidden".into(), json!(false));
        o.insert("isArticle".into(), json!(false));
        o.insert("isInReaderMode".into(), json!(false));
        o.insert("successorTabId".into(), json!(TAB_ID_NONE));
        o.insert("sharingState".into(), json!({ "camera": false, "microphone": false }));
    }
    Value::Object(o)
}

pub fn window_json(w: &WindowState, populate: bool, vis: &Visibility) -> Value {
    let mut o = json!({
        "id": w.id,
        "focused": w.focused,
        "top": w.top,
        "left": w.left,
        "width": w.width,
        "height": w.height,
        "incognito": w.incognito,
        "type": w.kind,
        "state": w.state,
        "alwaysOnTop": w.always_on_top,
    });
    if populate {
        o["tabs"] = Value::Array(w.tabs.iter().map(|t| tab_json(t, vis)).collect());
    }
    if vis.firefox {
        o["title"] = json!(w.tabs.iter().find(|t| t.active).filter(|t| vis.sees_url(t)).map(|t| t.title.clone()));
    }
    o
}

fn as_bool_filter(q: &Value, key: &str) -> Option<bool> {
    q.get(key).and_then(Value::as_bool)
}

/// `tabs.query(queryInfo)`. `current_window` is the caller's current window (see [`BrowserState::current_window`]).
pub fn query(state: &BrowserState, q: &Value, current_window: Option<i64>, vis: &Visibility) -> Result<Vec<Value>, String> {
    if !q.is_null() && !q.is_object() {
        return Err("queryInfo must be an object".into());
    }
    let url_patterns = match q.get("url") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(vec![s.clone()]),
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .map(|i| i.as_str().map(String::from).ok_or("'url' must be a string or an array of strings"))
                .collect::<Result<_, _>>()?,
        ),
        Some(_) => return Err("'url' must be a string or an array of strings".into()),
    };
    // Match patterns cannot name extension pages, but both browsers let tabs.query find them by URL
    // (`chrome-extension://<id>/page.html`, with `*` wildcards), so those are matched as globs instead.
    let is_extension_page_pattern = |p: &str| p.starts_with("chrome-extension://") || p.starts_with("moz-extension://");
    let url_filter = match &url_patterns {
        Some(list) => {
            let (extension_pages, web): (Vec<&String>, Vec<&String>) = list.iter().partition(|p| is_extension_page_pattern(p));
            let web_set = if web.is_empty() {
                None
            } else {
                Some(
                    MatchPatternSet::from_patterns(web.iter().map(|p| p.as_str()), MatchDialect::Chrome)
                        .map_err(|e| format!("Invalid url pattern '{}': {e}", list.join(", ")))?,
                )
            };
            Some((web_set, extension_pages.into_iter().cloned().collect::<Vec<String>>()))
        }
        None => None,
    };
    let title = q.get("title").and_then(Value::as_str);
    let window_id = q
        .get("windowId")
        .and_then(Value::as_i64)
        .map(|id| if id == WINDOW_ID_CURRENT { current_window.unwrap_or(WINDOW_ID_NONE) } else { id });
    let current = as_bool_filter(q, "currentWindow");
    let last_focused = as_bool_filter(q, "lastFocusedWindow");
    let status = q.get("status").and_then(Value::as_str);
    let window_type = q.get("windowType").and_then(Value::as_str);
    let index = q.get("index").and_then(Value::as_u64);
    let group = q.get("groupId").and_then(Value::as_i64);
    let muted = as_bool_filter(q, "muted");

    let mut out = vec![];
    for w in &state.windows {
        if window_id.is_some_and(|id| id != w.id) {
            continue;
        }
        if let Some(c) = current
            && (Some(w.id) == current_window) != c
        {
            continue;
        }
        if let Some(l) = last_focused
            && (Some(w.id) == state.last_focused_window) != l
        {
            continue;
        }
        if window_type.is_some_and(|t| t != w.kind) {
            continue;
        }
        for t in &w.tabs {
            let bools = [
                ("active", t.active),
                ("pinned", t.pinned),
                ("audible", t.audible),
                ("highlighted", t.highlighted),
                ("discarded", t.discarded),
                ("autoDiscardable", true),
                ("frozen", false),
            ];
            if bools.iter().any(|(k, v)| as_bool_filter(q, k).is_some_and(|want| want != *v)) {
                continue;
            }
            if muted.is_some_and(|m| m != t.muted) {
                continue;
            }
            if status.is_some_and(|s| s != t.status) || index.is_some_and(|i| i != t.index as u64) {
                continue;
            }
            if group.is_some_and(|g| g != -1) {
                continue;
            }
            // Filtering by URL or title only finds tabs whose URL the extension may see, as in Chrome.
            if let Some((web_set, extension_pages)) = &url_filter {
                let matches =
                    web_set.as_ref().is_some_and(|set| set.matches(&t.url)) || extension_pages.iter().any(|p| glob_match(p, &t.url, false));
                if !vis.sees_url(t) || !matches {
                    continue;
                }
            }
            if let Some(pattern) = title
                && (!vis.sees_url(t) || !glob_match(pattern, &t.title, false))
            {
                continue;
            }
            if let Some(store) = q.get("cookieStoreId").and_then(Value::as_str)
                && store != if t.incognito { "firefox-private" } else { "firefox-default" }
            {
                continue;
            }
            out.push(tab_json(t, vis));
        }
    }
    Ok(out)
}

/// The origin part of a URL, for `activeTab` (which lasts until the tab leaves the origin it was granted for).
pub fn origin_of(url: &str) -> String {
    split_url(url).map(|u| format!("{}://{}{}", u.scheme, u.host, u.port.map(|p| format!(":{p}")).unwrap_or_default())).unwrap_or_default()
}

/// One change between two snapshots, before it is shaped for a particular extension.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    WindowCreated(i64),
    WindowRemoved(i64),
    FocusChanged(i64),
    BoundsChanged(i64),
    TabCreated(i64),
    TabRemoved {
        tab_id: i64,
        window_id: i64,
        window_closing: bool,
    },
    TabUpdated {
        tab_id: i64,
        fields: Vec<&'static str>,
    },
    TabActivated {
        tab_id: i64,
        window_id: i64,
        previous: Option<i64>,
    },
    TabHighlighted {
        window_id: i64,
        tab_ids: Vec<i64>,
    },
    TabMoved {
        tab_id: i64,
        window_id: i64,
        from: usize,
        to: usize,
    },
    TabDetached {
        tab_id: i64,
        old_window: i64,
        old_position: usize,
    },
    TabAttached {
        tab_id: i64,
        new_window: i64,
        new_position: usize,
    },
    TabZoom {
        tab_id: i64,
        old: f64,
        new: f64,
    },
    /// The tab's document moved to another origin: `activeTab` grants for it end.
    TabLeftOrigin(i64),
}

/// The tabs that moved within a window, given the old and new order of the tabs present in both. The tabs outside a longest
/// common ordering are the ones the user moved; the others only shifted because of them.
fn moved_tabs(old_order: &[i64], new_order: &[i64]) -> Vec<i64> {
    let position: HashMap<i64, usize> = old_order.iter().enumerate().map(|(i, id)| (*id, i)).collect();
    let seq: Vec<usize> = new_order.iter().filter_map(|id| position.get(id).copied()).collect();
    // Longest increasing subsequence, O(n log n), with back-pointers to recover it.
    let mut tails: Vec<usize> = vec![];
    let mut tail_index: Vec<usize> = vec![];
    let mut previous: Vec<Option<usize>> = vec![None; seq.len()];
    for (i, value) in seq.iter().enumerate() {
        let at = tails.partition_point(|t| t < value);
        if at > 0 {
            previous[i] = Some(tail_index[at - 1]);
        }
        if at == tails.len() {
            tails.push(*value);
            tail_index.push(i);
        } else {
            tails[at] = *value;
            tail_index[at] = i;
        }
    }
    let mut keep = HashSet::new();
    let mut cursor = tail_index.last().copied();
    while let Some(i) = cursor {
        keep.insert(seq[i]);
        cursor = previous[i];
    }
    seq.iter().filter(|p| !keep.contains(p)).map(|p| old_order[*p]).collect()
}

/// Every change from `old` to `new`, in the order Chrome fires the events.
pub fn diff(old: &BrowserState, new: &BrowserState) -> Vec<Change> {
    let mut out = vec![];
    let old_windows: HashSet<i64> = old.windows.iter().map(|w| w.id).collect();
    let new_windows: HashSet<i64> = new.windows.iter().map(|w| w.id).collect();
    let old_tabs: HashMap<i64, &TabState> = old.tabs().map(|t| (t.id, t)).collect();
    let new_tabs: HashMap<i64, &TabState> = new.tabs().map(|t| (t.id, t)).collect();

    for w in &new.windows {
        if !old_windows.contains(&w.id) {
            out.push(Change::WindowCreated(w.id));
        }
    }
    for t in new.tabs() {
        if !old_tabs.contains_key(&t.id) {
            out.push(Change::TabCreated(t.id));
        }
    }
    // Tabs that changed window.
    for t in new.tabs() {
        if let Some(o) = old_tabs.get(&t.id)
            && o.window_id != t.window_id
        {
            out.push(Change::TabDetached { tab_id: t.id, old_window: o.window_id, old_position: o.index });
            out.push(Change::TabAttached { tab_id: t.id, new_window: t.window_id, new_position: t.index });
        }
    }
    // Tabs moved within a window.
    for w in &new.windows {
        let Some(ow) = old.window(w.id) else { continue };
        let staying: HashSet<i64> = w.tabs.iter().map(|t| t.id).filter(|id| ow.tabs.iter().any(|o| o.id == *id)).collect();
        let old_order: Vec<i64> = ow.tabs.iter().map(|t| t.id).filter(|id| staying.contains(id)).collect();
        let new_order: Vec<i64> = w.tabs.iter().map(|t| t.id).filter(|id| staying.contains(id)).collect();
        if old_order != new_order {
            for id in moved_tabs(&old_order, &new_order) {
                out.push(Change::TabMoved { tab_id: id, window_id: w.id, from: old_tabs[&id].index, to: new_tabs[&id].index });
            }
        }
    }
    for t in new.tabs() {
        let Some(o) = old_tabs.get(&t.id) else { continue };
        let mut fields = vec![];
        if o.status != t.status {
            fields.push("status");
        }
        if o.url != t.url {
            fields.push("url");
        }
        if o.title != t.title {
            fields.push("title");
        }
        if o.fav_icon_url != t.fav_icon_url && t.fav_icon_url.is_some() {
            fields.push("favIconUrl");
        }
        if o.pinned != t.pinned {
            fields.push("pinned");
        }
        if o.audible != t.audible {
            fields.push("audible");
        }
        if o.muted != t.muted {
            fields.push("mutedInfo");
        }
        if o.discarded != t.discarded {
            fields.push("discarded");
        }
        if !fields.is_empty() {
            if fields.contains(&"url") && origin_of(&o.url) != origin_of(&t.url) {
                out.push(Change::TabLeftOrigin(t.id));
            }
            out.push(Change::TabUpdated { tab_id: t.id, fields });
        }
        if (o.zoom - t.zoom).abs() > f64::EPSILON {
            out.push(Change::TabZoom { tab_id: t.id, old: o.zoom, new: t.zoom });
        }
    }
    for w in &new.windows {
        let active = w.tabs.iter().find(|t| t.active).map(|t| t.id);
        let old_active = old.window(w.id).and_then(|ow| ow.tabs.iter().find(|t| t.active)).map(|t| t.id);
        if let Some(active) = active
            && old_active != Some(active)
        {
            out.push(Change::TabActivated { tab_id: active, window_id: w.id, previous: old_active.filter(|p| new_tabs.contains_key(p)) });
        }
        let highlighted: Vec<i64> = w.tabs.iter().filter(|t| t.highlighted).map(|t| t.id).collect();
        let old_highlighted: Vec<i64> =
            old.window(w.id).map(|ow| ow.tabs.iter().filter(|t| t.highlighted).map(|t| t.id).collect()).unwrap_or_default();
        if !highlighted.is_empty() && highlighted != old_highlighted {
            out.push(Change::TabHighlighted { window_id: w.id, tab_ids: highlighted });
        }
    }
    for o in old.tabs() {
        if !new_tabs.contains_key(&o.id) {
            out.push(Change::TabRemoved { tab_id: o.id, window_id: o.window_id, window_closing: !new_windows.contains(&o.window_id) });
        }
    }
    for w in &old.windows {
        if !new_windows.contains(&w.id) {
            out.push(Change::WindowRemoved(w.id));
        }
    }
    let old_focus = old.focused_window();
    let new_focus = new.focused_window();
    if old_focus != new_focus {
        out.push(Change::FocusChanged(new_focus.unwrap_or(WINDOW_ID_NONE)));
    }
    for w in &new.windows {
        if let Some(o) = old.window(w.id)
            && (o.left, o.top, o.width, o.height) != (w.left, w.top, w.width, w.height)
        {
            out.push(Change::BoundsChanged(w.id));
        }
    }
    out
}

/// The event (name and listener arguments) an extension receives for a change, or `None` if this extension does not get it
/// (an `onUpdated` whose only changes are fields it may not see).
pub fn event_for(change: &Change, state: &BrowserState, vis: &Visibility) -> Option<(&'static str, Vec<Value>)> {
    Some(match change {
        Change::WindowCreated(id) => ("windows.onCreated", vec![window_json(state.window(*id)?, false, vis)]),
        Change::WindowRemoved(id) => ("windows.onRemoved", vec![json!(id)]),
        Change::FocusChanged(id) => ("windows.onFocusChanged", vec![json!(id)]),
        Change::BoundsChanged(id) => ("windows.onBoundsChanged", vec![window_json(state.window(*id)?, false, vis)]),
        Change::TabCreated(id) => ("tabs.onCreated", vec![tab_json(state.tab(*id)?, vis)]),
        Change::TabRemoved { tab_id, window_id, window_closing } => {
            ("tabs.onRemoved", vec![json!(tab_id), json!({ "windowId": window_id, "isWindowClosing": window_closing })])
        }
        Change::TabUpdated { tab_id, fields } => {
            let tab = state.tab(*tab_id)?;
            let full = tab_json(tab, vis);
            let mut info = Map::new();
            for f in fields {
                if let Some(v) = full.get(*f) {
                    info.insert((*f).into(), v.clone());
                }
            }
            if info.is_empty() {
                return None;
            }
            ("tabs.onUpdated", vec![json!(tab_id), Value::Object(info), full])
        }
        Change::TabActivated { tab_id, window_id, previous } => {
            let mut info = json!({ "tabId": tab_id, "windowId": window_id });
            if let Some(p) = previous {
                info["previousTabId"] = json!(p);
            }
            ("tabs.onActivated", vec![info])
        }
        Change::TabHighlighted { window_id, tab_ids } => ("tabs.onHighlighted", vec![json!({ "windowId": window_id, "tabIds": tab_ids })]),
        Change::TabMoved { tab_id, window_id, from, to } => {
            ("tabs.onMoved", vec![json!(tab_id), json!({ "windowId": window_id, "fromIndex": from, "toIndex": to })])
        }
        Change::TabDetached { tab_id, old_window, old_position } => {
            ("tabs.onDetached", vec![json!(tab_id), json!({ "oldWindowId": old_window, "oldPosition": old_position })])
        }
        Change::TabAttached { tab_id, new_window, new_position } => {
            ("tabs.onAttached", vec![json!(tab_id), json!({ "newWindowId": new_window, "newPosition": new_position })])
        }
        Change::TabZoom { tab_id, old, new } => (
            "tabs.onZoomChange",
            vec![
                json!({ "tabId": tab_id, "oldZoomFactor": old, "newZoomFactor": new, "zoomSettings": { "mode": "automatic", "scope": "per-origin" } }),
            ],
        ),
        Change::TabLeftOrigin(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(v: Value) -> BrowserState {
        BrowserState::from_json(&v).unwrap()
    }

    fn two_windows() -> BrowserState {
        state(json!({
            "windows": [
                { "id": 1, "focused": true, "width": 1000, "height": 700, "tabs": [
                    { "id": 10, "url": "https://a.test/x", "title": "A", "active": true },
                    { "id": 11, "url": "https://b.test/", "title": "B" } ] },
                { "id": 2, "type": "popup", "tabs": [ { "id": 20, "url": "https://c.test/", "title": "C", "active": true } ] }
            ]
        }))
    }

    fn sees_all() -> Visibility<'static> {
        Visibility { all_tabs: true, can_access: &|_, _| false, firefox: false }
    }

    #[test]
    fn parses_and_derives_positions() {
        let s = two_windows();
        assert_eq!(s.last_focused_window, Some(1));
        assert_eq!(s.tab(11).unwrap().index, 1);
        assert_eq!(s.tab(20).unwrap().window_id, 2);
        assert!(s.tab(10).unwrap().highlighted);
        assert!(BrowserState::from_json(&json!({"windows": [{"id": 1, "tabs": [{"id": 1}, {"id": 1}]}]})).is_err());
        assert!(BrowserState::from_json(&json!({})).is_err());
    }

    #[test]
    fn tab_objects_hide_urls_without_permission() {
        let s = two_windows();
        let none = Visibility { all_tabs: false, can_access: &|_, url| url.starts_with("https://b.test"), firefox: false };
        let a = tab_json(s.tab(10).unwrap(), &none);
        assert!(a.get("url").is_none() && a.get("title").is_none());
        let b = tab_json(s.tab(11).unwrap(), &none);
        assert_eq!(b["url"], "https://b.test/");
        let ff = tab_json(s.tab(10).unwrap(), &Visibility { firefox: true, ..sees_all() });
        assert_eq!(ff["cookieStoreId"], "firefox-default");
    }

    #[test]
    fn query_filters() {
        let s = two_windows();
        let v = sees_all();
        let ids = |q: Value, current: Option<i64>| -> Vec<i64> {
            query(&s, &q, current, &v).unwrap().iter().map(|t| t["id"].as_i64().unwrap()).collect()
        };
        assert_eq!(ids(json!({}), Some(1)), vec![10, 11, 20]);
        assert_eq!(ids(json!({"active": true, "currentWindow": true}), Some(1)), vec![10]);
        assert_eq!(ids(json!({"active": true, "lastFocusedWindow": true}), None), vec![10]);
        assert_eq!(ids(json!({"windowId": WINDOW_ID_CURRENT}), Some(2)), vec![20]);
        assert_eq!(ids(json!({"url": "https://*.test/x*"}), None), vec![10]);
        assert_eq!(ids(json!({"url": ["*://b.test/*", "*://c.test/*"]}), None), vec![11, 20]);
        assert_eq!(ids(json!({"title": "?"}), None), vec![10, 11, 20]);
        assert_eq!(ids(json!({"windowType": "popup"}), None), vec![20]);
        assert_eq!(ids(json!({"index": 1}), None), vec![11]);
        assert!(query(&s, &json!({"url": "not a pattern"}), None, &v).is_err());
        // Extension pages are found by URL too (AdGuard looks for its own post-install page this way).
        let mut with_page = two_windows();
        with_page.windows[0].tabs[1].url = "chrome-extension://abc/pages/post-install.html".into();
        let found = query(&with_page, &json!({"url": "chrome-extension://abc/pages/post-install.html"}), None, &v).unwrap();
        assert_eq!(found.iter().map(|t| t["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![11]);
        let mixed = query(&with_page, &json!({"url": ["chrome-extension://abc/*", "*://c.test/*"]}), None, &v).unwrap();
        assert_eq!(mixed.iter().map(|t| t["id"].as_i64().unwrap()).collect::<Vec<_>>(), vec![11, 20]);
        // Without permission to see URLs, a URL filter matches nothing.
        let blind = Visibility { all_tabs: false, can_access: &|_, _| false, firefox: false };
        assert!(query(&s, &json!({"url": "<all_urls>"}), None, &blind).unwrap().is_empty());
    }

    #[test]
    fn current_window_resolution() {
        let s = two_windows();
        assert_eq!(s.current_window(Some(20)), Some(2));
        assert_eq!(s.current_window(None), Some(1));
        assert_eq!(s.current_window(Some(999)), Some(1));
    }

    #[test]
    fn moved_tabs_are_found() {
        assert_eq!(moved_tabs(&[1, 2, 3, 4], &[1, 2, 3, 4]), Vec::<i64>::new());
        assert_eq!(moved_tabs(&[1, 2, 3, 4], &[4, 1, 2, 3]), vec![4]);
        assert_eq!(moved_tabs(&[1, 2, 3, 4], &[2, 3, 4, 1]), vec![1]);
    }

    #[test]
    fn diff_produces_chrome_events_in_order() {
        let old = two_windows();
        let new = state(json!({
            "windows": [
                { "id": 1, "focused": false, "width": 1000, "height": 700, "tabs": [
                    { "id": 11, "url": "https://b.test/", "title": "B", "active": true },
                    { "id": 10, "url": "https://a.test/x", "title": "A2", "status": "loading" },
                    { "id": 12, "url": "https://new.test/", "title": "N" } ] },
                { "id": 3, "focused": true, "tabs": [ { "id": 30, "url": "about:blank", "active": true } ] }
            ]
        }));
        let changes = diff(&old, &new);
        assert_eq!(changes[0], Change::WindowCreated(3));
        assert!(changes.contains(&Change::TabCreated(12)));
        assert!(changes.contains(&Change::TabCreated(30)));
        // Swapping two tabs is one move; which of the two "moved" is ambiguous.
        assert_eq!(changes.iter().filter(|c| matches!(c, Change::TabMoved { window_id: 1, .. })).count(), 1);
        assert!(changes.contains(&Change::TabUpdated { tab_id: 10, fields: vec!["status", "title"] }));
        assert!(changes.contains(&Change::TabActivated { tab_id: 11, window_id: 1, previous: Some(10) }));
        assert!(changes.contains(&Change::TabRemoved { tab_id: 20, window_id: 2, window_closing: true }));
        assert!(changes.contains(&Change::WindowRemoved(2)));
        assert!(changes.contains(&Change::FocusChanged(3)));
        let created = changes.iter().position(|c| *c == Change::TabCreated(12)).unwrap();
        let removed_window = changes.iter().position(|c| *c == Change::WindowRemoved(2)).unwrap();
        let removed_tab = changes.iter().position(|c| matches!(c, Change::TabRemoved { .. })).unwrap();
        assert!(created < removed_tab && removed_tab < removed_window);
    }

    #[test]
    fn diff_detaches_and_attaches() {
        let old = two_windows();
        let new = state(json!({
            "windows": [
                { "id": 1, "focused": true, "width": 1000, "height": 700, "tabs": [ { "id": 10, "url": "https://a.test/x", "title": "A", "active": true } ] },
                { "id": 2, "type": "popup", "tabs": [ { "id": 20, "url": "https://c.test/", "title": "C", "active": true }, { "id": 11, "url": "https://b.test/", "title": "B" } ] }
            ]
        }));
        let changes = diff(&old, &new);
        assert_eq!(
            changes[..2],
            [
                Change::TabDetached { tab_id: 11, old_window: 1, old_position: 1 },
                Change::TabAttached { tab_id: 11, new_window: 2, new_position: 1 }
            ]
        );
    }

    #[test]
    fn updates_hide_fields_the_extension_may_not_see() {
        let old = two_windows();
        let mut new = old.clone();
        new.windows[0].tabs[0].title = "Secret".into();
        new.windows[0].tabs[0].url = "https://other.test/".into();
        let changes = diff(&old, &new);
        assert!(changes.contains(&Change::TabLeftOrigin(10)));
        let update = changes.iter().find(|c| matches!(c, Change::TabUpdated { .. })).unwrap();
        let blind = Visibility { all_tabs: false, can_access: &|_, _| false, firefox: false };
        assert!(event_for(update, &new, &blind).is_none());
        let (name, args) = event_for(update, &new, &sees_all()).unwrap();
        assert_eq!(name, "tabs.onUpdated");
        assert_eq!(args[1], json!({ "url": "https://other.test/", "title": "Secret" }));

        let mut loading = old.clone();
        loading.windows[0].tabs[0].status = "loading".into();
        let change = &diff(&old, &loading)[0];
        let (_, args) = event_for(change, &loading, &blind).unwrap();
        assert_eq!(args[1], json!({ "status": "loading" }));
    }
}
