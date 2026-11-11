//! The parts of the browser UI an extension controls: its toolbar button (`action`, `browserAction`, `pageAction`), its
//! context-menu items (`contextMenus` / `menus`) and its keyboard shortcuts (`commands`).
//!
//! The core keeps all of this state; the browser draws it. The browser asks for the state of the toolbar buttons
//! ([`crate::dispatch::Dispatcher::ui_state`]), the items for a context menu it is about to show
//! ([`crate::dispatch::Dispatcher::context_menu_items`]) and the shortcuts to register, and reports clicks and key presses
//! back. It is told to redraw through the `__ui.changed` pseudo-event.

use crate::match_pattern::{MatchDialect, MatchPattern, MatchPatternSet, glob_match};
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// An RGBA colour, as the badge APIs take and return it.
pub type Rgba = [u8; 4];

/// Parses a CSS colour as `action.setBadgeBackgroundColor` accepts it: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`,
/// `rgb()`/`rgba()`, a few named colours, or an `[r, g, b, a]` array.
pub fn parse_color(v: &Value) -> Result<Rgba, String> {
    if let Some(items) = v.as_array() {
        if items.len() != 4 {
            return Err("A color array must have four components".into());
        }
        let mut out = [0u8; 4];
        for (i, item) in items.iter().enumerate() {
            out[i] = item.as_u64().filter(|n| *n <= 255).ok_or("Color components must be integers from 0 to 255")? as u8;
        }
        return Ok(out);
    }
    let s = v.as_str().ok_or("color must be a string or an array")?.trim().to_ascii_lowercase();
    let named = match s.as_str() {
        "black" => Some([0, 0, 0, 255]),
        "white" => Some([255, 255, 255, 255]),
        "red" => Some([255, 0, 0, 255]),
        "green" => Some([0, 128, 0, 255]),
        "blue" => Some([0, 0, 255, 255]),
        "yellow" => Some([255, 255, 0, 255]),
        "orange" => Some([255, 165, 0, 255]),
        "purple" => Some([128, 0, 128, 255]),
        "gray" | "grey" => Some([128, 128, 128, 255]),
        "transparent" => Some([0, 0, 0, 0]),
        _ => None,
    };
    if let Some(c) = named {
        return Ok(c);
    }
    let bad = || format!("'{s}' is not a valid color");
    if let Some(hex) = s.strip_prefix('#') {
        let digit = |i: usize, len: usize| u8::from_str_radix(&hex[i..i + len], 16).map_err(|_| bad());
        return match hex.len() {
            3 | 4 => {
                let mut out = [255u8; 4];
                for (i, slot) in out.iter_mut().enumerate().take(hex.len()) {
                    *slot = digit(i, 1)? * 17;
                }
                Ok(out)
            }
            6 | 8 => {
                let mut out = [255u8; 4];
                for (i, slot) in out.iter_mut().enumerate().take(hex.len() / 2) {
                    *slot = digit(i * 2, 2)?;
                }
                Ok(out)
            }
            _ => Err(bad()),
        };
    }
    if let Some(inner) = s.strip_prefix("rgba(").or_else(|| s.strip_prefix("rgb(")).and_then(|r| r.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split([',', ' ', '/']).filter(|p| !p.is_empty()).collect();
        if parts.len() < 3 || parts.len() > 4 {
            return Err(bad());
        }
        let mut out = [255u8; 4];
        for (i, p) in parts.iter().take(3).enumerate() {
            out[i] = p.parse::<f64>().map_err(|_| bad())?.clamp(0.0, 255.0).round() as u8;
        }
        if let Some(a) = parts.get(3) {
            let a = if let Some(pct) = a.strip_suffix('%') {
                pct.parse::<f64>().map_err(|_| bad())? / 100.0
            } else {
                a.parse::<f64>().map_err(|_| bad())?
            };
            out[3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        return Ok(out);
    }
    Err(bad())
}

/// An icon an extension set: file paths by size, or pixels.
#[derive(Debug, Clone, PartialEq)]
pub enum Icon {
    /// `(size, path inside the extension)`.
    Paths(Vec<(u32, String)>),
    /// `(size, width, height, RGBA bytes as base64)`.
    Pixels(Vec<(u32, u32, u32, String)>),
}

impl Icon {
    pub fn to_json(&self) -> Value {
        match self {
            Icon::Paths(paths) => {
                json!({ "kind": "paths", "paths": paths.iter().map(|(s, p)| json!({ "size": s, "path": p })).collect::<Vec<_>>() })
            }
            Icon::Pixels(images) => json!({
                "kind": "pixels",
                "images": images.iter().map(|(s, w, h, d)| json!({ "size": s, "width": w, "height": h, "rgba": d })).collect::<Vec<_>>(),
            }),
        }
    }
}

/// Normalises a path an extension gave for one of its files: no leading `/` or `./`, and the extension's own base URL
/// stripped.
pub fn normalize_extension_path(path: &str, base_url: &str) -> String {
    let path = path.strip_prefix(base_url).unwrap_or(path);
    let mut p = path.trim_start_matches('/');
    while let Some(rest) = p.strip_prefix("./") {
        p = rest.trim_start_matches('/');
    }
    p.split(['?', '#']).next().unwrap_or("").to_string()
}

/// Resolves a path an extension gave relative to the page that called the API, as Chrome and Firefox do (`../icons/a.png`
/// from `background/index.html` is `icons/a.png`). Absolute paths (`/a.png`) and the extension's own URLs are taken from the
/// root. The result never leaves the extension's root.
pub fn resolve_extension_path(path: &str, base_url: &str, caller_url: Option<&str>) -> String {
    let path = path.split(['?', '#']).next().unwrap_or("");
    let (dir, rest) = if let Some(rooted) = path.strip_prefix(base_url) {
        (String::new(), rooted.to_string())
    } else if let Some(rooted) = path.strip_prefix('/') {
        (String::new(), rooted.to_string())
    } else {
        let caller_path = caller_url.and_then(|u| u.strip_prefix(base_url)).unwrap_or("");
        let caller_path = caller_path.split(['?', '#']).next().unwrap_or("");
        let dir = caller_path.rfind('/').map(|i| caller_path[..=i].to_string()).unwrap_or_default();
        (dir, path.to_string())
    };
    let mut parts: Vec<&str> = vec![];
    for segment in dir.split('/').chain(rest.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// `setIcon` details: `{ path }` (a string or a size map) or `{ imageData }` (one image or a size map). The shim turns
/// `ImageData` objects into `{ width, height, data }` with base64 RGBA, since they do not survive JSON.
pub fn parse_icon(details: &Value, base_url: &str, caller_url: Option<&str>) -> Result<Option<Icon>, String> {
    if let Some(image) = details.get("imageData").filter(|v| !v.is_null()) {
        let one = |size: u32, v: &Value| -> Result<(u32, u32, u32, String), String> {
            let w = v.get("width").and_then(Value::as_u64).ok_or("imageData needs a width")? as u32;
            let h = v.get("height").and_then(Value::as_u64).ok_or("imageData needs a height")? as u32;
            let data = v.get("data").and_then(Value::as_str).ok_or("imageData must be an ImageData object")?.to_string();
            let bytes = crate::encoding::base64_decode(&data).ok_or("imageData has invalid pixel data")?;
            if bytes.len() as u64 != u64::from(w) * u64::from(h) * 4 {
                return Err("imageData has the wrong number of pixels".into());
            }
            Ok((if size == 0 { w } else { size }, w, h, data))
        };
        if image.get("width").is_some() {
            return Ok(Some(Icon::Pixels(vec![one(0, image)?])));
        }
        let map = image.as_object().ok_or("imageData must be an ImageData object or a dictionary of them")?;
        let mut images = vec![];
        for (size, v) in map {
            images.push(one(size.parse().map_err(|_| format!("'{size}' is not an icon size"))?, v)?);
        }
        images.sort();
        return Ok(Some(Icon::Pixels(images)));
    }
    match details.get("path") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(p)) => Ok(Some(Icon::Paths(vec![(0, resolve_extension_path(p, base_url, caller_url))]))),
        Some(Value::Object(map)) => {
            let mut paths = vec![];
            for (size, v) in map {
                let p = v.as_str().ok_or("icon paths must be strings")?;
                paths.push((
                    size.parse().map_err(|_| format!("'{size}' is not an icon size"))?,
                    resolve_extension_path(p, base_url, caller_url),
                ));
            }
            paths.sort();
            Ok(Some(Icon::Paths(paths)))
        }
        Some(_) => Err("path must be a string or a dictionary".into()),
    }
}

/// What can be set on a toolbar button, globally or for one tab. `None` means "not set here": a tab falls back to the global
/// value, the global value to the manifest.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActionProps {
    pub title: Option<String>,
    pub badge_text: Option<String>,
    pub badge_background: Option<Rgba>,
    pub badge_text_color: Option<Rgba>,
    pub popup: Option<String>,
    pub icon: Option<Icon>,
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct ActionState {
    pub global: ActionProps,
    pub tabs: HashMap<i64, ActionProps>,
    /// Whether the user put the button on the toolbar (`action.getUserSettings().isOnToolbar`). Kept by the browser.
    pub pinned: bool,
}

impl ActionState {
    pub fn props_mut(&mut self, tab: Option<i64>) -> &mut ActionProps {
        match tab {
            Some(t) => self.tabs.entry(t).or_default(),
            None => &mut self.global,
        }
    }

    /// The effective value of one property for a tab (or globally): the tab's, else the global one.
    pub fn get<T: Clone>(&self, tab: Option<i64>, f: impl Fn(&ActionProps) -> Option<T>) -> Option<T> {
        tab.and_then(|t| self.tabs.get(&t)).and_then(&f).or_else(|| f(&self.global))
    }

    pub fn forget_tab(&mut self, tab: i64) {
        self.tabs.remove(&tab);
    }
}

/// One context-menu item, as an extension created it.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    /// A string or an integer, as the extension gave it (or the integer the core chose).
    pub id: Value,
    pub parent_id: Option<Value>,
    pub title: String,
    /// `normal`, `checkbox`, `radio` or `separator`.
    pub kind: String,
    pub contexts: Vec<String>,
    pub checked: bool,
    pub enabled: bool,
    pub visible: bool,
    pub document_url_patterns: Vec<String>,
    pub target_url_patterns: Vec<String>,
}

pub const MENU_CONTEXTS: &[&str] = &[
    "all",
    "page",
    "frame",
    "selection",
    "link",
    "editable",
    "image",
    "video",
    "audio",
    "launcher",
    "browser_action",
    "page_action",
    "action",
    "tab",
    "tools_menu",
    "bookmark",
    "password",
];

fn string_list(v: Option<&Value>, what: &str) -> Result<Option<Vec<String>>, String> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| i.as_str().map(String::from).ok_or(format!("'{what}' must contain only strings")))
            .collect::<Result<_, _>>()
            .map(Some),
        Some(_) => Err(format!("'{what}' must be an array of strings")),
    }
}

/// The ids extensions use: a string or an integer.
pub fn menu_id_key(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(format!("s:{s}")),
        Value::Number(n) if n.is_i64() => Some(format!("n:{n}")),
        _ => None,
    }
}

#[derive(Debug, Clone, Default)]
pub struct Menus {
    /// In creation order, which is display order.
    pub items: Vec<MenuItem>,
    pub next_generated_id: i64,
}

impl Menus {
    pub fn find(&self, id: &Value) -> Option<usize> {
        let key = menu_id_key(id)?;
        self.items.iter().position(|i| menu_id_key(&i.id).as_deref() == Some(&key))
    }

    fn apply(item: &mut MenuItem, props: &Value) -> Result<(), String> {
        if let Some(t) = props.get("title").filter(|v| !v.is_null()) {
            item.title = t.as_str().ok_or("'title' must be a string")?.to_string();
        }
        if let Some(t) = props.get("type").filter(|v| !v.is_null()) {
            let t = t.as_str().ok_or("'type' must be a string")?;
            if !matches!(t, "normal" | "checkbox" | "radio" | "separator") {
                return Err(format!("'{t}' is not a valid menu item type"));
            }
            item.kind = t.into();
        }
        if let Some(c) = string_list(props.get("contexts"), "contexts")? {
            if c.is_empty() {
                return Err("'contexts' must not be empty".into());
            }
            if let Some(bad) = c.iter().find(|c| !MENU_CONTEXTS.contains(&c.as_str())) {
                return Err(format!("'{bad}' is not a valid menu context"));
            }
            item.contexts = c;
        }
        for (key, slot) in [("checked", &mut item.checked), ("enabled", &mut item.enabled), ("visible", &mut item.visible)] {
            if let Some(v) = props.get(key).filter(|v| !v.is_null()) {
                *slot = v.as_bool().ok_or(format!("'{key}' must be a boolean"))?;
            }
        }
        for (key, slot) in [("documentUrlPatterns", &mut item.document_url_patterns), ("targetUrlPatterns", &mut item.target_url_patterns)]
        {
            if let Some(list) = string_list(props.get(key), key)? {
                // Firefox also takes patterns for other schemes in targetUrlPatterns (`abp:*`), matched as globs.
                for pattern in &list {
                    let valid = MatchPattern::parse(pattern, MatchDialect::Chrome).is_ok()
                        || (key == "targetUrlPatterns" && is_scheme_glob(pattern));
                    if !valid {
                        return Err(format!("Invalid url pattern '{pattern}' in '{key}'"));
                    }
                }
                *slot = list;
            }
        }
        Ok(())
    }

    /// `contextMenus.create(createProperties)`. `needs_id` is true for event pages and service workers, which must name
    /// their items so they survive restarts.
    pub fn create(&mut self, props: &Value, needs_id: bool) -> Result<Value, String> {
        if !props.is_object() {
            return Err("createProperties must be an object".into());
        }
        let id = match props.get("id") {
            None | Some(Value::Null) => {
                if needs_id {
                    return Err(
                        "Extensions using event pages or Service Workers must pass an id parameter to chrome.contextMenus.create".into()
                    );
                }
                self.next_generated_id += 1;
                json!(self.next_generated_id)
            }
            Some(v) if menu_id_key(v).is_some() => v.clone(),
            Some(_) => return Err("'id' must be a string or an integer".into()),
        };
        if self.find(&id).is_some() {
            return Err(format!("Cannot create item with duplicate id {}", id.as_str().map_or_else(|| id.to_string(), String::from)));
        }
        let parent_id = props.get("parentId").filter(|v| !v.is_null()).cloned();
        if let Some(p) = &parent_id
            && self.find(p).is_none()
        {
            return Err(format!("Cannot find menu item with id {}", p.as_str().map_or_else(|| p.to_string(), String::from)));
        }
        let mut item = MenuItem {
            id: id.clone(),
            parent_id,
            title: String::new(),
            kind: "normal".into(),
            contexts: vec!["page".into()],
            checked: false,
            enabled: true,
            visible: true,
            document_url_patterns: vec![],
            target_url_patterns: vec![],
        };
        Self::apply(&mut item, props)?;
        if item.kind != "separator" && item.title.is_empty() {
            return Err("Title missing".into());
        }
        if item.kind == "radio" && props.get("checked").is_none() {
            // The first radio item of a group starts checked.
            item.checked = !self.items.iter().any(|i| i.kind == "radio" && i.parent_id == item.parent_id);
        }
        self.items.push(item);
        Ok(id)
    }

    pub fn update(&mut self, id: &Value, props: &Value) -> Result<(), String> {
        let at = self
            .find(id)
            .ok_or_else(|| format!("Cannot find menu item with id {}", id.as_str().map_or_else(|| id.to_string(), String::from)))?;
        if let Some(p) = props.get("parentId").filter(|v| !v.is_null()) {
            if self.find(p).is_none() {
                return Err("Cannot find the new parent".into());
            }
            // An item cannot become its own descendant.
            let mut cursor = Some(p.clone());
            while let Some(c) = cursor {
                if menu_id_key(&c) == menu_id_key(id) {
                    return Err("Cannot set a menu item's parent to one of its descendants".into());
                }
                cursor = self.find(&c).and_then(|i| self.items[i].parent_id.clone());
            }
        }
        let mut item = self.items[at].clone();
        Self::apply(&mut item, props)?;
        if let Some(p) = props.get("parentId").filter(|v| !v.is_null()) {
            item.parent_id = Some(p.clone());
        }
        self.items[at] = item;
        Ok(())
    }

    /// Removes the item and its descendants. Returns false if there was no such item.
    pub fn remove(&mut self, id: &Value) -> bool {
        let Some(key) = menu_id_key(id) else { return false };
        if self.find(id).is_none() {
            return false;
        }
        let mut doomed = vec![key];
        loop {
            let more: Vec<String> = self
                .items
                .iter()
                .filter(|i| i.parent_id.as_ref().and_then(menu_id_key).is_some_and(|p| doomed.contains(&p)))
                .filter_map(|i| menu_id_key(&i.id))
                .filter(|k| !doomed.contains(k))
                .collect();
            if more.is_empty() {
                break;
            }
            doomed.extend(more);
        }
        self.items.retain(|i| menu_id_key(&i.id).is_none_or(|k| !doomed.contains(&k)));
        true
    }

    pub fn to_json(&self) -> Value {
        Value::Array(
            self.items
                .iter()
                .map(|i| {
                    json!({
                        "id": i.id, "parentId": i.parent_id, "title": i.title, "type": i.kind, "contexts": i.contexts,
                        "checked": i.checked, "enabled": i.enabled, "visible": i.visible,
                        "documentUrlPatterns": i.document_url_patterns, "targetUrlPatterns": i.target_url_patterns,
                    })
                })
                .collect(),
        )
    }

    pub fn from_json(v: &Value) -> Menus {
        let mut menus = Menus::default();
        for entry in v.as_array().into_iter().flatten() {
            let mut props = entry.clone();
            // Re-create in order so that validation (parents first) holds.
            if let Some(o) = props.as_object_mut() {
                o.retain(|_, v| !v.is_null());
            }
            let _ = menus.create(&props, false);
            if let Some(n) = entry.get("id").and_then(Value::as_i64) {
                menus.next_generated_id = menus.next_generated_id.max(n);
            }
        }
        menus
    }

    /// Toggles a checkbox or selects a radio item (unchecking its siblings) when the user clicks it. Returns
    /// `(wasChecked, checked)` for checkable items.
    pub fn click(&mut self, id: &Value) -> Option<(bool, bool)> {
        let at = self.find(id)?;
        let item = self.items[at].clone();
        match item.kind.as_str() {
            "checkbox" => {
                self.items[at].checked = !item.checked;
                Some((item.checked, !item.checked))
            }
            "radio" => {
                // A radio group is a run of adjacent radio items with the same parent.
                let siblings: Vec<usize> =
                    self.items.iter().enumerate().filter(|(_, i)| i.parent_id == item.parent_id).map(|(n, _)| n).collect();
                let pos = siblings.iter().position(|n| *n == at)?;
                let mut start = pos;
                while start > 0 && self.items[siblings[start - 1]].kind == "radio" {
                    start -= 1;
                }
                let mut end = pos;
                while end + 1 < siblings.len() && self.items[siblings[end + 1]].kind == "radio" {
                    end += 1;
                }
                for n in &siblings[start..=end] {
                    self.items[*n].checked = *n == at;
                }
                Some((item.checked, true))
            }
            _ => None,
        }
    }
}

/// What was under the pointer when a context menu opened, as the browser reports it.
#[derive(Debug, Clone, Default)]
pub struct MenuTarget {
    pub page_url: String,
    pub frame_url: Option<String>,
    pub frame_id: i64,
    pub link_url: Option<String>,
    pub src_url: Option<String>,
    /// `image`, `video` or `audio`.
    pub media_type: Option<String>,
    pub selection_text: Option<String>,
    pub editable: bool,
    /// The menu of an extension's toolbar button rather than of a page.
    pub action: bool,
}

impl MenuTarget {
    pub fn from_json(v: &Value) -> MenuTarget {
        let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        MenuTarget {
            page_url: s("pageUrl").unwrap_or_default(),
            frame_url: s("frameUrl"),
            frame_id: v.get("frameId").and_then(Value::as_i64).unwrap_or(0),
            link_url: s("linkUrl"),
            src_url: s("srcUrl"),
            media_type: s("mediaType"),
            selection_text: s("selectionText"),
            editable: v.get("editable").and_then(Value::as_bool).unwrap_or(false),
            action: v.get("contexts").and_then(Value::as_array).is_some_and(|c| c.iter().any(|x| x == "action")),
        }
    }

    /// The contexts this target is in, as `contexts` in `createProperties` names them.
    pub fn contexts(&self, action_kind: &str) -> Vec<&'static str> {
        if self.action {
            return match action_kind {
                "page_action" => vec!["page_action"],
                "browser_action" => vec!["browser_action", "action"],
                _ => vec!["action", "browser_action"],
            };
        }
        let mut c = vec![];
        if self.selection_text.is_some() {
            c.push("selection");
        }
        if self.link_url.is_some() {
            c.push("link");
        }
        if self.editable {
            c.push("editable");
        }
        match self.media_type.as_deref() {
            Some("image") => c.push("image"),
            Some("video") => c.push("video"),
            Some("audio") => c.push("audio"),
            _ => {}
        }
        if self.frame_id != 0 {
            c.push("frame");
        }
        if c.is_empty() {
            c.push("page");
        }
        c
    }

    /// `OnClickData` for `contextMenus.onClicked`.
    pub fn click_data(&self, item: &MenuItem, checked: Option<(bool, bool)>) -> Value {
        let mut o = Map::new();
        o.insert("menuItemId".into(), item.id.clone());
        if let Some(p) = &item.parent_id {
            o.insert("parentMenuItemId".into(), p.clone());
        }
        o.insert("editable".into(), json!(self.editable));
        if !self.action {
            o.insert("pageUrl".into(), json!(self.page_url));
            o.insert("frameId".into(), json!(self.frame_id));
        }
        for (key, v) in [
            ("frameUrl", &self.frame_url),
            ("linkUrl", &self.link_url),
            ("srcUrl", &self.src_url),
            ("mediaType", &self.media_type),
            ("selectionText", &self.selection_text),
        ] {
            if let Some(v) = v {
                o.insert(key.into(), json!(v));
            }
        }
        if let Some((was, now)) = checked {
            o.insert("wasChecked".into(), json!(was));
            o.insert("checked".into(), json!(now));
        }
        Value::Object(o)
    }
}

/// `scheme:glob`, such as `abp:*`.
fn is_scheme_glob(pattern: &str) -> bool {
    pattern.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty() && scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    })
}

/// Chrome shows at most this many characters of the selection in `%s`.
const SELECTION_IN_TITLE: usize = 32;

/// The items of one extension that apply to `target`, as a tree: `[{ id, title, type, checked, enabled, children }]`. `id`
/// is the item's id as JSON text, to be handed back in `contextMenuClicked`.
pub fn menu_tree(menus: &Menus, target: &MenuTarget, action_kind: &str) -> Vec<Value> {
    let contexts = target.contexts(action_kind);
    let applies = |item: &MenuItem| -> bool {
        if !item.visible {
            return false;
        }
        let in_context = item.contexts.iter().any(|c| c == "all" && !target.action || contexts.contains(&c.as_str()));
        if !in_context {
            return false;
        }
        if !item.document_url_patterns.is_empty() {
            let url = if target.frame_id != 0 { target.frame_url.as_deref().unwrap_or(&target.page_url) } else { &target.page_url };
            let set = MatchPatternSet::from_patterns(item.document_url_patterns.iter().map(String::as_str), MatchDialect::Chrome)
                .unwrap_or_default();
            if !set.matches(url) {
                return false;
            }
        }
        if !item.target_url_patterns.is_empty() {
            let Some(url) = target.link_url.as_deref().or(target.src_url.as_deref()) else { return true };
            let matches = item.target_url_patterns.iter().any(|p| match MatchPattern::parse(p, MatchDialect::Chrome) {
                Ok(pattern) => pattern.matches(url),
                Err(_) => is_scheme_glob(p) && glob_match(p, url, false),
            });
            if !matches {
                return false;
            }
        }
        true
    };
    let selection: String = target.selection_text.as_deref().unwrap_or("").chars().take(SELECTION_IN_TITLE).collect();
    fn build(menus: &Menus, parent: Option<&Value>, applies: &dyn Fn(&MenuItem) -> bool, selection: &str) -> Vec<Value> {
        menus
            .items
            .iter()
            .filter(|i| i.parent_id.as_ref().and_then(menu_id_key) == parent.and_then(menu_id_key))
            .filter(|i| applies(i))
            .map(|i| {
                json!({
                    "id": i.id.to_string(),
                    "title": i.title.replace("%s", selection),
                    "type": i.kind,
                    "checked": i.checked,
                    "enabled": i.enabled,
                    "children": build(menus, Some(&i.id), applies, selection),
                })
            })
            .collect()
    }
    build(menus, None, &applies, &selection)
}

/// The keyboard shortcuts of an extension: the manifest's suggestions, changed by the user or by `commands.update`.
#[derive(Debug, Clone, Default)]
pub struct Commands {
    /// name -> shortcut ("" = none), for the commands whose shortcut differs from the manifest.
    pub overrides: HashMap<String, String>,
    /// name -> description, from `commands.update`.
    pub descriptions: HashMap<String, String>,
}

/// The commands that open the toolbar button's popup (or click it).
pub fn is_action_command(name: &str) -> bool {
    matches!(name, "_execute_action" | "_execute_browser_action" | "_execute_page_action")
}

/// Normalises a shortcut to Chrome's form (`Ctrl+Shift+Y`), or rejects it. Empty means "no shortcut".
pub fn normalize_shortcut(s: &str) -> Result<String, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(String::new());
    }
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    let (key, modifiers) = parts.split_last().ok_or("empty shortcut")?;
    let mut mods: Vec<&str> = vec![];
    for m in modifiers {
        let canonical = match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "Ctrl",
            "alt" => "Alt",
            "shift" => "Shift",
            "command" | "cmd" => "Command",
            "macctrl" => "MacCtrl",
            _ => return Err(format!("'{m}' is not a valid modifier")),
        };
        if mods.contains(&canonical) {
            return Err(format!("'{m}' appears twice"));
        }
        mods.push(canonical);
    }
    let media = ["MediaNextTrack", "MediaPlayPause", "MediaPrevTrack", "MediaStop"];
    let named = ["Comma", "Period", "Home", "End", "PageUp", "PageDown", "Space", "Insert", "Delete", "Up", "Down", "Left", "Right"];
    let key_ok = (key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric()))
        || named.iter().any(|n| n.eq_ignore_ascii_case(key))
        || media.iter().any(|n| n.eq_ignore_ascii_case(key))
        || (key.len() >= 2 && key[..1].eq_ignore_ascii_case("f") && key[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)));
    if !key_ok {
        return Err(format!("'{key}' is not a valid key"));
    }
    let is_media = media.iter().any(|n| n.eq_ignore_ascii_case(key));
    let is_function = key.len() >= 2 && key[..1].eq_ignore_ascii_case("f") && key[1..].parse::<u8>().is_ok();
    if mods.is_empty() && !is_media && !is_function {
        return Err("A shortcut needs Ctrl or Alt (or Command on macOS)".into());
    }
    if !mods.is_empty() && !mods.iter().any(|m| matches!(*m, "Ctrl" | "Alt" | "Command" | "MacCtrl")) && !is_function {
        return Err("A shortcut needs Ctrl or Alt (or Command on macOS)".into());
    }
    // Chrome's order: Ctrl/Command/MacCtrl, Alt, Shift.
    mods.sort_by_key(|m| match *m {
        "Ctrl" | "Command" => 0,
        "MacCtrl" => 1,
        "Alt" => 2,
        _ => 3,
    });
    let key = if key.len() == 1 {
        key.to_ascii_uppercase()
    } else {
        named.iter().chain(media.iter()).find(|n| n.eq_ignore_ascii_case(key)).map_or_else(|| key.to_ascii_uppercase(), |n| n.to_string())
    };
    let mut out = mods.join("+");
    if !out.is_empty() {
        out.push('+');
    }
    out.push_str(&key);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_against_the_calling_page() {
        let base = "moz-extension://u/";
        let bg = Some("moz-extension://u/background/index.html");
        assert_eq!(resolve_extension_path("../icons/a.png", base, bg), "icons/a.png");
        assert_eq!(resolve_extension_path("b.png", base, bg), "background/b.png");
        assert_eq!(resolve_extension_path("/c.png", base, bg), "c.png");
        assert_eq!(resolve_extension_path("moz-extension://u/d/e.png", base, bg), "d/e.png");
        assert_eq!(resolve_extension_path("../../../x.png", base, bg), "x.png");
        assert_eq!(resolve_extension_path("./f.png?v=1", base, None), "f.png");
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color(&json!("#f00")).unwrap(), [255, 0, 0, 255]);
        assert_eq!(parse_color(&json!("#00ff0080")).unwrap(), [0, 255, 0, 128]);
        assert_eq!(parse_color(&json!("rgb(1, 2, 3)")).unwrap(), [1, 2, 3, 255]);
        assert_eq!(parse_color(&json!("rgba(1,2,3,0.5)")).unwrap(), [1, 2, 3, 128]);
        assert_eq!(parse_color(&json!([1, 2, 3, 4])).unwrap(), [1, 2, 3, 4]);
        assert_eq!(parse_color(&json!("Red")).unwrap(), [255, 0, 0, 255]);
        assert!(parse_color(&json!("#12")).is_err());
        assert!(parse_color(&json!([1, 2, 3])).is_err());
        assert!(parse_color(&json!("hsl(1,2,3)")).is_err());
    }

    #[test]
    fn icons() {
        let base = "chrome-extension://abc/";
        assert_eq!(parse_icon(&json!({"path": "/img/a.png"}), base, None).unwrap(), Some(Icon::Paths(vec![(0, "img/a.png".into())])));
        assert_eq!(
            parse_icon(&json!({"path": {"32": "./b.png", "16": "chrome-extension://abc/a.png"}}), base, None).unwrap(),
            Some(Icon::Paths(vec![(16, "a.png".into()), (32, "b.png".into())]))
        );
        let pixels = crate::encoding::base64_encode(&[0u8; 16]);
        assert!(matches!(
            parse_icon(&json!({"imageData": {"width": 2, "height": 2, "data": pixels}}), base, None).unwrap(),
            Some(Icon::Pixels(_))
        ));
        assert!(parse_icon(&json!({"imageData": {"width": 3, "height": 2, "data": pixels}}), base, None).is_err());
        assert_eq!(parse_icon(&json!({}), base, None).unwrap(), None);
    }

    #[test]
    fn action_state_falls_back_from_tab_to_global() {
        let mut a = ActionState::default();
        a.props_mut(None).badge_text = Some("g".into());
        a.props_mut(Some(5)).badge_text = Some("t".into());
        assert_eq!(a.get(Some(5), |p| p.badge_text.clone()).as_deref(), Some("t"));
        assert_eq!(a.get(Some(6), |p| p.badge_text.clone()).as_deref(), Some("g"));
        a.forget_tab(5);
        assert_eq!(a.get(Some(5), |p| p.badge_text.clone()).as_deref(), Some("g"));
    }

    #[test]
    fn menus_create_update_remove() {
        let mut m = Menus::default();
        assert_eq!(m.create(&json!({"title": "A"}), false).unwrap(), json!(1));
        assert!(m.create(&json!({"title": "B"}), true).is_err());
        m.create(&json!({"id": "p", "title": "Parent", "contexts": ["all"]}), true).unwrap();
        m.create(&json!({"id": "c", "parentId": "p", "title": "Child %s", "contexts": ["selection"]}), true).unwrap();
        assert!(m.create(&json!({"id": "c", "title": "dup"}), true).is_err());
        assert!(m.create(&json!({"id": "x", "parentId": "nope", "title": "x"}), true).is_err());
        assert!(m.create(&json!({"id": "y", "title": "y", "contexts": ["nonsense"]}), true).is_err());
        assert!(m.create(&json!({"id": "z"}), true).is_err());
        assert!(m.update(&json!("p"), &json!({"parentId": "c"})).is_err());
        m.update(&json!("c"), &json!({"title": "Look up \"%s\""})).unwrap();

        let target = MenuTarget { page_url: "https://a.test/".into(), selection_text: Some("hello".into()), ..Default::default() };
        let tree = menu_tree(&m, &target, "action");
        assert_eq!(tree.len(), 1, "item 1 is 'page' only, and a selection is not the page context");
        assert_eq!(tree[0]["children"][0]["title"], "Look up \"hello\"");
        assert_eq!(tree[0]["children"][0]["id"], "\"c\"");

        assert!(m.remove(&json!("p")));
        assert!(m.find(&json!("c")).is_none());
        assert!(!m.remove(&json!("p")));
        let restored = Menus::from_json(&m.to_json());
        assert_eq!(restored.items, m.items);
    }

    #[test]
    fn menus_patterns_and_action_context() {
        let mut m = Menus::default();
        m.create(&json!({"id": "a", "title": "Only a.test", "documentUrlPatterns": ["https://a.test/*"]}), true).unwrap();
        m.create(&json!({"id": "b", "title": "Button", "contexts": ["action"]}), true).unwrap();
        m.create(&json!({"id": "l", "title": "Link", "contexts": ["link"], "targetUrlPatterns": ["*://*.example.com/*"]}), true).unwrap();
        m.create(&json!({"id": "abp", "title": "Subscribe", "contexts": ["link"], "targetUrlPatterns": ["abp:*"]}), true).unwrap();
        assert!(m.create(&json!({"id": "bad", "title": "x", "documentUrlPatterns": ["abp:*"]}), true).is_err());
        let page = |url: &str| MenuTarget { page_url: url.into(), ..Default::default() };
        assert_eq!(menu_tree(&m, &page("https://a.test/x"), "action").len(), 1);
        assert_eq!(menu_tree(&m, &page("https://b.test/x"), "action").len(), 0);
        let button = MenuTarget { action: true, ..Default::default() };
        assert_eq!(menu_tree(&m, &button, "action")[0]["id"], "\"b\"");
        let link = |u: &str| MenuTarget { page_url: "https://b.test/".into(), link_url: Some(u.into()), ..Default::default() };
        assert_eq!(menu_tree(&m, &link("https://www.example.com/"), "action").len(), 1);
        assert_eq!(menu_tree(&m, &link("https://other.test/"), "action").len(), 0);
    }

    #[test]
    fn checkbox_and_radio_clicks() {
        let mut m = Menus::default();
        m.create(&json!({"id": "c", "type": "checkbox", "title": "C"}), true).unwrap();
        m.create(&json!({"id": "r1", "type": "radio", "title": "1"}), true).unwrap();
        m.create(&json!({"id": "r2", "type": "radio", "title": "2"}), true).unwrap();
        assert!(m.items[1].checked && !m.items[2].checked);
        assert_eq!(m.click(&json!("c")), Some((false, true)));
        assert_eq!(m.click(&json!("r2")), Some((false, true)));
        assert!(!m.items[1].checked && m.items[2].checked);
        assert_eq!(m.click(&json!("nope")), None);
    }

    #[test]
    fn shortcuts() {
        assert_eq!(normalize_shortcut("shift+ctrl+y").unwrap(), "Ctrl+Shift+Y");
        assert_eq!(normalize_shortcut("Alt+Comma").unwrap(), "Alt+Comma");
        assert_eq!(normalize_shortcut("MediaPlayPause").unwrap(), "MediaPlayPause");
        assert_eq!(normalize_shortcut("").unwrap(), "");
        assert!(normalize_shortcut("Y").is_err());
        assert!(normalize_shortcut("Shift+Y").is_err());
        assert!(normalize_shortcut("Ctrl+Ctrl+Y").is_err());
        assert!(normalize_shortcut("Hyper+Y").is_err());
        assert!(normalize_shortcut("Ctrl+Escape").is_err());
    }
}
