//! Browser-data extension APIs: `cookies`, `bookmarks`, `history`, `topSites`, `browsingData`, `downloads`, and `sessions`.
//!
//! As with `browser_api`, the core owns validation, permission enforcement, filtering, sorting, and event fan-out; the
//! embedding browser persists or executes side effects through `host.*` primitives and notifies the core through
//! [`Dispatcher::host_event`].

use crate::browser::{BrowserState, TabState, WindowState, tab_json, window_json};
use crate::dispatch::Dispatcher;
use crate::host::*;
use crate::match_pattern::split_url;
use crate::registry::Member;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};

pub const MAX_SESSION_RESULTS: usize = 25;

pub fn handles(full_name: &str) -> bool {
    let ns = full_name.split('.').next().unwrap_or("");
    matches!(ns, "cookies" | "bookmarks" | "history" | "topSites" | "browsingData" | "downloads" | "sessions")
}

// ---- Data state owned by the core ----------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct DataState {
    pub bookmarks: BookmarkStore,
    pub downloads: DownloadStore,
    pub sessions: SessionStore,
}

// ---- Bookmarks model -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BookmarkNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub url: Option<String>,
    pub date_added: i64,
    pub date_group_modified: Option<i64>,
    pub children: Vec<String>,
    pub unmodifiable: Option<&'static str>,
}

#[derive(Debug, Clone)]
pub struct BookmarkStore {
    pub nodes: HashMap<String, BookmarkNode>,
    pub root_id: String,
    pub default_parent_id: String,
    next_id: u64,
}

impl Default for BookmarkStore {
    fn default() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            "0".into(),
            BookmarkNode {
                id: "0".into(),
                parent_id: None,
                title: String::new(),
                url: None,
                date_added: 0,
                date_group_modified: Some(0),
                children: vec!["1".into(), "2".into(), "3".into()],
                unmodifiable: Some("managed"),
            },
        );
        for (id, title) in [("1", "Bookmarks Bar"), ("2", "Other Bookmarks"), ("3", "Mobile Bookmarks")] {
            nodes.insert(
                id.into(),
                BookmarkNode {
                    id: id.into(),
                    parent_id: Some("0".into()),
                    title: title.into(),
                    url: None,
                    date_added: 0,
                    date_group_modified: Some(0),
                    children: vec![],
                    unmodifiable: None,
                },
            );
        }
        BookmarkStore { nodes, root_id: "0".into(), default_parent_id: "2".into(), next_id: 10 }
    }
}

impl BookmarkStore {
    fn is_root(&self, id: &str) -> bool {
        id == self.root_id || matches!(id, "0" | "1" | "2" | "3")
    }

    /// Replaces the tree from a host snapshot (a `BookmarkTreeNode` root or array of top-level items).
    pub fn load_snapshot(&mut self, v: &Value) {
        let Some(root) = v.get("root").or(Some(v)) else { return };
        let mut fresh = BookmarkStore::default();
        if root.is_object() && root.get("id").and_then(Value::as_str).is_some() {
            fresh.nodes.clear();
            let rid = root["id"].as_str().unwrap().to_string();
            fresh.root_id = rid.clone();
            fresh.insert_subtree(root, None);
            if let Some(first_folder) = fresh.nodes.get(&rid).and_then(|r| r.children.first()).cloned() {
                fresh.default_parent_id = first_folder;
            }
            *self = fresh;
        } else if let Some(items) = root.get("items").and_then(Value::as_array).or_else(|| root.as_array()) {
            // Host supplied flat list under Bookmarks Bar ("1").
            for item in items {
                let child_id = fresh.insert_subtree(item, Some("1"));
                if let Some(bar) = fresh.nodes.get_mut("1") {
                    bar.children.push(child_id);
                }
            }
            *self = fresh;
        }
    }

    fn insert_subtree(&mut self, v: &Value, parent_id: Option<&str>) -> String {
        let id = v.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| self.fresh_id());
        if let Ok(n) = id.parse::<u64>()
            && n >= self.next_id
        {
            self.next_id = n + 1;
        }
        let title = v.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        let url = v.get("url").and_then(Value::as_str).filter(|u| !u.is_empty()).map(String::from);
        let date_added = v.get("dateAdded").and_then(Value::as_i64).unwrap_or(0);
        let date_group_modified = v.get("dateGroupModified").and_then(Value::as_i64);
        let mut child_ids = vec![];
        if let Some(children) = v.get("children").and_then(Value::as_array) {
            for c in children {
                child_ids.push(self.insert_subtree(c, Some(&id)));
            }
        }
        self.nodes.insert(
            id.clone(),
            BookmarkNode {
                id: id.clone(),
                parent_id: parent_id.map(String::from),
                title,
                url,
                date_added,
                date_group_modified,
                children: child_ids,
                unmodifiable: None,
            },
        );
        id
    }

    fn fresh_id(&mut self) -> String {
        let id = self.next_id;
        self.next_id += 1;
        id.to_string()
    }

    fn index_in_parent(&self, id: &str) -> Option<usize> {
        let parent_id = self.nodes.get(id)?.parent_id.as_deref()?;
        self.nodes.get(parent_id)?.children.iter().position(|c| c == id)
    }

    pub fn node_json(&self, id: &str, include_children: bool) -> Option<Value> {
        let n = self.nodes.get(id)?;
        let mut obj = Map::new();
        obj.insert("id".into(), json!(n.id));
        if let Some(p) = &n.parent_id {
            obj.insert("parentId".into(), json!(p));
        }
        if let Some(idx) = self.index_in_parent(id) {
            obj.insert("index".into(), json!(idx));
        }
        obj.insert("title".into(), json!(n.title));
        if let Some(url) = &n.url {
            obj.insert("url".into(), json!(url));
        }
        obj.insert("dateAdded".into(), json!(n.date_added));
        if n.url.is_none() {
            obj.insert("type".into(), json!("folder"));
            if let Some(m) = n.date_group_modified {
                obj.insert("dateGroupModified".into(), json!(m));
            }
            if include_children {
                let kids: Vec<Value> = n.children.iter().filter_map(|c| self.node_json(c, true)).collect();
                obj.insert("children".into(), Value::Array(kids));
            }
        } else {
            obj.insert("type".into(), json!("bookmark"));
        }
        if let Some(u) = n.unmodifiable {
            obj.insert("unmodifiable".into(), json!(u));
        }
        Some(Value::Object(obj))
    }

    fn is_ancestor<'a>(&'a self, ancestor: &str, mut node: &'a str) -> bool {
        while let Some(n) = self.nodes.get(node) {
            if n.id == ancestor {
                return true;
            }
            match n.parent_id.as_deref() {
                Some(p) => node = p,
                None => return false,
            }
        }
        false
    }

    pub fn create(&mut self, details: &Value, now: i64) -> Result<Value, String> {
        let parent_id = details.get("parentId").and_then(Value::as_str).unwrap_or(&self.default_parent_id).to_string();
        let parent = self.nodes.get(&parent_id).ok_or_else(|| format!("Bookmark parent '{parent_id}' not found."))?;
        if parent.url.is_some() {
            return Err("Can't create a bookmark inside a non-folder.".into());
        }
        let max_idx = parent.children.len();
        let index = match details.get("index").and_then(Value::as_u64) {
            Some(i) if (i as usize) <= max_idx => i as usize,
            Some(_) => return Err("Index out of bounds.".into()),
            None => max_idx,
        };
        let title = details.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        let url = match details.get("url") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                if split_url(s).is_none() {
                    return Err("Invalid URL.".into());
                }
                Some(s.clone())
            }
            Some(_) => return Err("url must be a string.".into()),
        };
        let id = details.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| self.fresh_id());
        self.nodes.insert(
            id.clone(),
            BookmarkNode {
                id: id.clone(),
                parent_id: Some(parent_id.clone()),
                title,
                url: url.clone(),
                date_added: now,
                date_group_modified: url.is_none().then_some(now),
                children: vec![],
                unmodifiable: None,
            },
        );
        if let Some(p) = self.nodes.get_mut(&parent_id) {
            p.children.insert(index, id.clone());
            p.date_group_modified = Some(now);
        }
        Ok(self.node_json(&id, false).unwrap())
    }

    pub fn update(&mut self, id: &str, changes: &Value) -> Result<(Value, Value), String> {
        if self.is_root(id) {
            return Err("Can't modify the root bookmark folders.".into());
        }
        let is_folder = self.nodes.get(id).ok_or_else(|| "Can't find bookmark for id.".to_string())?.url.is_none();
        if let Some(u) = changes.get("url").and_then(Value::as_str) {
            if is_folder {
                return Err("Can't set URL of a bookmark folder.".into());
            }
            if split_url(u).is_none() {
                return Err("Invalid URL.".into());
            }
        }
        let node = self.nodes.get_mut(id).unwrap();
        let mut change_info = Map::new();
        if let Some(t) = changes.get("title").and_then(Value::as_str) {
            node.title = t.to_string();
        }
        change_info.insert("title".into(), json!(node.title));
        if let Some(u) = changes.get("url").and_then(Value::as_str) {
            node.url = Some(u.to_string());
        }
        if let Some(u) = &node.url {
            change_info.insert("url".into(), json!(u));
        }
        Ok((self.node_json(id, false).unwrap(), Value::Object(change_info)))
    }

    pub fn move_node(&mut self, id: &str, dest: &Value, now: i64) -> Result<(Value, Value), String> {
        if self.is_root(id) {
            return Err("Can't modify the root bookmark folders.".into());
        }
        let old_parent = self.nodes.get(id).and_then(|n| n.parent_id.clone()).ok_or("Can't find bookmark for id.")?;
        let old_index = self.index_in_parent(id).unwrap_or(0);
        let new_parent = dest.get("parentId").and_then(Value::as_str).map(String::from).unwrap_or_else(|| old_parent.clone());
        let target = self.nodes.get(&new_parent).ok_or("Destination folder not found.")?;
        if target.url.is_some() {
            return Err("Can't move a bookmark into a non-folder.".into());
        }
        if self.is_ancestor(id, &new_parent) {
            return Err("Can't move a folder to itself or its descendant.".into());
        }
        if let Some(op) = self.nodes.get_mut(&old_parent) {
            op.children.retain(|c| c != id);
            op.date_group_modified = Some(now);
        }
        let max_idx = self.nodes.get(&new_parent).map(|p| p.children.len()).unwrap_or(0);
        let new_index = match dest.get("index").and_then(Value::as_u64) {
            Some(i) => (i as usize).min(max_idx),
            None => max_idx,
        };
        if let Some(np) = self.nodes.get_mut(&new_parent) {
            np.children.insert(new_index, id.to_string());
            np.date_group_modified = Some(now);
        }
        if let Some(n) = self.nodes.get_mut(id) {
            n.parent_id = Some(new_parent.clone());
        }
        let move_info = json!({
            "parentId": new_parent,
            "index": new_index,
            "oldParentId": old_parent,
            "oldIndex": old_index,
        });
        Ok((self.node_json(id, false).unwrap(), move_info))
    }

    pub fn remove(&mut self, id: &str, recursive: bool, now: i64) -> Result<Value, String> {
        if self.is_root(id) {
            return Err("Can't modify the root bookmark folders.".into());
        }
        let node = self.nodes.get(id).ok_or("Can't find bookmark for id.")?;
        if !recursive && !node.children.is_empty() {
            return Err("Can't remove a non-empty folder (use removeTree).".into());
        }
        let parent_id = node.parent_id.clone().unwrap_or_default();
        let index = self.index_in_parent(id).unwrap_or(0);
        let snapshot = self.node_json(id, true).unwrap();
        let mut stack = vec![id.to_string()];
        while let Some(cur) = stack.pop() {
            if let Some(n) = self.nodes.remove(&cur) {
                stack.extend(n.children);
            }
        }
        if let Some(p) = self.nodes.get_mut(&parent_id) {
            p.children.retain(|c| c != id);
            p.date_group_modified = Some(now);
        }
        Ok(json!({ "parentId": parent_id, "index": index, "node": snapshot }))
    }

    pub fn recent(&self, count: usize) -> Vec<Value> {
        let mut bookmarks: Vec<&BookmarkNode> = self.nodes.values().filter(|n| n.url.is_some()).collect();
        bookmarks.sort_by(|a, b| b.date_added.cmp(&a.date_added).then_with(|| b.id.cmp(&a.id)));
        bookmarks.into_iter().take(count).filter_map(|n| self.node_json(&n.id, false)).collect()
    }

    pub fn search(&self, query: &Value) -> Result<Vec<Value>, String> {
        let (text, url_exact, title_exact) = match query {
            Value::String(s) => (Some(s.as_str()), None, None),
            Value::Object(m) => {
                (m.get("query").and_then(Value::as_str), m.get("url").and_then(Value::as_str), m.get("title").and_then(Value::as_str))
            }
            _ => return Err("query must be a string or an object".into()),
        };
        let terms: Vec<String> = text.map(|t| t.split_whitespace().map(|w| w.to_lowercase()).collect()).unwrap_or_default();
        let mut out = vec![];
        for n in self.nodes.values() {
            if n.id == self.root_id {
                continue;
            }
            if let Some(u) = url_exact
                && n.url.as_deref() != Some(u)
            {
                continue;
            }
            if let Some(t) = title_exact
                && n.title != t
            {
                continue;
            }
            if !terms.is_empty() {
                let hay_title = n.title.to_lowercase();
                let hay_url = n.url.as_deref().unwrap_or("").to_lowercase();
                if !terms.iter().all(|term| hay_title.contains(term) || hay_url.contains(term)) {
                    continue;
                }
            }
            if let Some(j) = self.node_json(&n.id, false) {
                out.push(j);
            }
        }
        out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        Ok(out)
    }
}

// ---- Downloads model -----------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DownloadItem {
    pub id: i64,
    pub url: String,
    pub final_url: String,
    pub referrer: String,
    pub filename: String,
    pub incognito: bool,
    pub danger: String,
    pub mime: String,
    pub start_time: String,
    pub end_time: Option<String>,
    pub state: String, // "in_progress" | "interrupted" | "complete"
    pub paused: bool,
    pub can_resume: bool,
    pub error: Option<String>,
    pub bytes_received: i64,
    pub total_bytes: i64,
    pub file_size: i64,
    pub exists: bool,
    pub by_extension_id: Option<String>,
    pub by_extension_name: Option<String>,
}

impl DownloadItem {
    pub fn to_json(&self) -> Value {
        let mut o = Map::new();
        o.insert("id".into(), json!(self.id));
        o.insert("url".into(), json!(self.url));
        o.insert("finalUrl".into(), json!(self.final_url));
        o.insert("referrer".into(), json!(self.referrer));
        o.insert("filename".into(), json!(self.filename));
        o.insert("incognito".into(), json!(self.incognito));
        o.insert("danger".into(), json!(self.danger));
        o.insert("mime".into(), json!(self.mime));
        o.insert("startTime".into(), json!(self.start_time));
        if let Some(e) = &self.end_time {
            o.insert("endTime".into(), json!(e));
        }
        o.insert("state".into(), json!(self.state));
        o.insert("paused".into(), json!(self.paused));
        o.insert("canResume".into(), json!(self.can_resume));
        if let Some(err) = &self.error {
            o.insert("error".into(), json!(err));
        }
        o.insert("bytesReceived".into(), json!(self.bytes_received));
        o.insert("totalBytes".into(), json!(self.total_bytes));
        o.insert("fileSize".into(), json!(self.file_size));
        o.insert("exists".into(), json!(self.exists));
        if let Some(id) = &self.by_extension_id {
            o.insert("byExtensionId".into(), json!(id));
        }
        if let Some(name) = &self.by_extension_name {
            o.insert("byExtensionName".into(), json!(name));
        }
        Value::Object(o)
    }

    pub fn from_json(v: &Value, default_id: i64, now_iso: String) -> DownloadItem {
        let url = v.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        DownloadItem {
            id: v.get("id").and_then(Value::as_i64).unwrap_or(default_id),
            final_url: v.get("finalUrl").and_then(Value::as_str).unwrap_or(&url).to_string(),
            url,
            referrer: v.get("referrer").and_then(Value::as_str).unwrap_or("").to_string(),
            filename: v.get("filename").and_then(Value::as_str).unwrap_or("").to_string(),
            incognito: v.get("incognito").and_then(Value::as_bool).unwrap_or(false),
            danger: v.get("danger").and_then(Value::as_str).unwrap_or("safe").to_string(),
            mime: v.get("mime").and_then(Value::as_str).unwrap_or("application/octet-stream").to_string(),
            start_time: v.get("startTime").and_then(Value::as_str).map(String::from).unwrap_or(now_iso),
            end_time: v.get("endTime").and_then(Value::as_str).map(String::from),
            state: v.get("state").and_then(Value::as_str).unwrap_or("in_progress").to_string(),
            paused: v.get("paused").and_then(Value::as_bool).unwrap_or(false),
            can_resume: v.get("canResume").and_then(Value::as_bool).unwrap_or(false),
            error: v.get("error").and_then(Value::as_str).map(String::from),
            bytes_received: v.get("bytesReceived").and_then(Value::as_i64).unwrap_or(0),
            total_bytes: v.get("totalBytes").and_then(Value::as_i64).unwrap_or(-1),
            file_size: v.get("fileSize").and_then(Value::as_i64).unwrap_or(-1),
            exists: v.get("exists").and_then(Value::as_bool).unwrap_or(true),
            by_extension_id: v.get("byExtensionId").and_then(Value::as_str).map(String::from),
            by_extension_name: v.get("byExtensionName").and_then(Value::as_str).map(String::from),
        }
    }
}

#[derive(Debug)]
pub struct DownloadStore {
    pub items: HashMap<i64, DownloadItem>,
    pub shelf_enabled: bool,
    next_id: i64,
}

impl Default for DownloadStore {
    fn default() -> Self {
        DownloadStore { items: HashMap::new(), shelf_enabled: true, next_id: 1 }
    }
}

impl DownloadStore {
    pub fn fresh_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn insert(&mut self, item: DownloadItem) {
        if item.id >= self.next_id {
            self.next_id = item.id + 1;
        }
        self.items.insert(item.id, item);
    }

    pub fn apply_delta(&mut self, id: i64, delta: &Value) -> Option<Value> {
        let item = self.items.get_mut(&id)?;
        let mut out = Map::new();
        out.insert("id".into(), json!(id));
        macro_rules! field_str {
            ($json_key:literal, $field:ident) => {
                if let Some(v) = delta.get($json_key).and_then(Value::as_str)
                    && item.$field != v
                {
                    let prev = std::mem::replace(&mut item.$field, v.to_string());
                    out.insert($json_key.into(), json!({ "previous": prev, "current": v }));
                }
            };
        }
        macro_rules! field_bool {
            ($json_key:literal, $field:ident) => {
                if let Some(v) = delta.get($json_key).and_then(Value::as_bool)
                    && item.$field != v
                {
                    let prev = std::mem::replace(&mut item.$field, v);
                    out.insert($json_key.into(), json!({ "previous": prev, "current": v }));
                }
            };
        }
        field_str!("url", url);
        field_str!("finalUrl", final_url);
        field_str!("filename", filename);
        field_str!("danger", danger);
        field_str!("mime", mime);
        field_str!("state", state);
        field_bool!("paused", paused);
        field_bool!("canResume", can_resume);
        field_bool!("exists", exists);
        if let Some(v) = delta.get("endTime").and_then(Value::as_str)
            && item.end_time.as_deref() != Some(v)
        {
            let prev = item.end_time.replace(v.to_string());
            out.insert("endTime".into(), json!({ "previous": prev, "current": v }));
        }
        if let Some(v) = delta.get("error").and_then(Value::as_str)
            && item.error.as_deref() != Some(v)
        {
            let prev = item.error.replace(v.to_string());
            out.insert("error".into(), json!({ "previous": prev, "current": v }));
        }
        if let Some(v) = delta.get("bytesReceived").and_then(Value::as_i64) {
            item.bytes_received = v;
        }
        if let Some(v) = delta.get("totalBytes").and_then(Value::as_i64)
            && item.total_bytes != v
        {
            let prev = std::mem::replace(&mut item.total_bytes, v);
            out.insert("totalBytes".into(), json!({ "previous": prev, "current": v }));
        }
        if let Some(v) = delta.get("fileSize").and_then(Value::as_i64)
            && item.file_size != v
        {
            let prev = std::mem::replace(&mut item.file_size, v);
            out.insert("fileSize".into(), json!({ "previous": prev, "current": v }));
        }
        (out.len() > 1).then_some(Value::Object(out))
    }

    pub fn search(&self, query: &Value) -> Result<Vec<DownloadItem>, String> {
        let q = match query {
            Value::Null => Map::new(),
            Value::Object(m) => m.clone(),
            _ => return Err("query must be an object".into()),
        };
        let url_regex = match q.get("urlRegex").and_then(Value::as_str) {
            Some(r) => Some(Regex::new(r).map_err(|e| format!("Invalid urlRegex: {e}"))?),
            None => None,
        };
        let filename_regex = match q.get("filenameRegex").and_then(Value::as_str) {
            Some(r) => Some(Regex::new(r).map_err(|e| format!("Invalid filenameRegex: {e}"))?),
            None => None,
        };
        let terms: Vec<String> = q
            .get("query")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(|s| s.to_lowercase()).collect())
            .unwrap_or_default();

        let mut matches: Vec<DownloadItem> = self
            .items
            .values()
            .filter(|d| {
                if let Some(id) = q.get("id").and_then(Value::as_i64)
                    && d.id != id
                {
                    return false;
                }
                if let Some(u) = q.get("url").and_then(Value::as_str)
                    && d.url != u
                {
                    return false;
                }
                if let Some(f) = q.get("filename").and_then(Value::as_str)
                    && d.filename != f
                {
                    return false;
                }
                if let Some(s) = q.get("state").and_then(Value::as_str)
                    && d.state != s
                {
                    return false;
                }
                if let Some(m) = q.get("mime").and_then(Value::as_str)
                    && d.mime != m
                {
                    return false;
                }
                if let Some(dg) = q.get("danger").and_then(Value::as_str)
                    && d.danger != dg
                {
                    return false;
                }
                if let Some(p) = q.get("paused").and_then(Value::as_bool)
                    && d.paused != p
                {
                    return false;
                }
                if let Some(e) = q.get("exists").and_then(Value::as_bool)
                    && d.exists != e
                {
                    return false;
                }
                if let Some(b) = q.get("bytesReceived").and_then(Value::as_i64)
                    && d.bytes_received != b
                {
                    return false;
                }
                if let Some(t) = q.get("totalBytes").and_then(Value::as_i64)
                    && d.total_bytes != t
                {
                    return false;
                }
                if let Some(t) = q.get("totalBytesGreater").and_then(Value::as_i64)
                    && d.total_bytes <= t
                {
                    return false;
                }
                if let Some(t) = q.get("totalBytesLess").and_then(Value::as_i64)
                    && d.total_bytes >= t
                {
                    return false;
                }
                if let Some(st) = q.get("startedAfter").and_then(Value::as_str)
                    && d.start_time.as_str() <= st
                {
                    return false;
                }
                if let Some(st) = q.get("startedBefore").and_then(Value::as_str)
                    && d.start_time.as_str() >= st
                {
                    return false;
                }
                if let Some(re) = &url_regex
                    && !re.is_match(&d.url)
                {
                    return false;
                }
                if let Some(re) = &filename_regex
                    && !re.is_match(&d.filename)
                {
                    return false;
                }
                if !terms.is_empty() {
                    let hay = format!("{} {}", d.url.to_lowercase(), d.filename.to_lowercase());
                    for term in &terms {
                        if let Some(neg) = term.strip_prefix('-') {
                            if !neg.is_empty() && hay.contains(neg) {
                                return false;
                            }
                        } else if !hay.contains(term.as_str()) {
                            return false;
                        }
                    }
                }
                true
            })
            .cloned()
            .collect();

        if let Some(order_by) = q.get("orderBy").and_then(Value::as_array) {
            let keys: Vec<(bool, String)> = order_by
                .iter()
                .filter_map(Value::as_str)
                .map(|s| match s.strip_prefix('-') {
                    Some(rest) => (true, rest.to_string()),
                    None => (false, s.to_string()),
                })
                .collect();
            matches.sort_by(|a, b| {
                for (desc, key) in &keys {
                    let ord = match key.as_str() {
                        "id" => a.id.cmp(&b.id),
                        "startTime" => a.start_time.cmp(&b.start_time),
                        "url" => a.url.cmp(&b.url),
                        "filename" => a.filename.cmp(&b.filename),
                        "totalBytes" => a.total_bytes.cmp(&b.total_bytes),
                        "bytesReceived" => a.bytes_received.cmp(&b.bytes_received),
                        "state" => a.state.cmp(&b.state),
                        _ => std::cmp::Ordering::Equal,
                    };
                    let ord = if *desc { ord.reverse() } else { ord };
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                a.id.cmp(&b.id)
            });
        } else {
            matches.sort_by_key(|d| d.id);
        }

        let limit = q.get("limit").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(1000);
        if limit > 0 && matches.len() > limit {
            matches.truncate(limit);
        }
        Ok(matches)
    }
}

// ---- Sessions model ------------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum ClosedEntry {
    Tab { session_id: String, last_modified: i64, tab: TabState },
    Window { session_id: String, last_modified: i64, window: WindowState },
}

impl ClosedEntry {
    pub fn session_id(&self) -> &str {
        match self {
            ClosedEntry::Tab { session_id, .. } | ClosedEntry::Window { session_id, .. } => session_id,
        }
    }

    pub fn window_id(&self) -> i64 {
        match self {
            ClosedEntry::Tab { tab, .. } => tab.window_id,
            ClosedEntry::Window { window, .. } => window.id,
        }
    }
}

#[derive(Debug, Default)]
pub struct SessionStore {
    pub recently_closed: Vec<ClosedEntry>,
    /// Per-extension tab values: `(extension_id, tab_id) -> { key -> value }`.
    pub tab_values: HashMap<(String, i64), HashMap<String, Value>>,
    /// Per-extension window values: `(extension_id, window_id) -> { key -> value }`.
    pub window_values: HashMap<(String, i64), HashMap<String, Value>>,
    next_session_id: u64,
}

impl SessionStore {
    pub fn record_closed_tab(&mut self, tab: TabState, now_sec: i64) {
        self.next_session_id += 1;
        let session_id = format!("session-tab-{}", self.next_session_id);
        self.recently_closed.insert(0, ClosedEntry::Tab { session_id, last_modified: now_sec, tab });
        self.recently_closed.truncate(MAX_SESSION_RESULTS);
    }

    pub fn record_closed_window(&mut self, window: WindowState, now_sec: i64) {
        self.next_session_id += 1;
        let session_id = format!("session-win-{}", self.next_session_id);
        self.recently_closed.insert(0, ClosedEntry::Window { session_id, last_modified: now_sec, window });
        self.recently_closed.truncate(MAX_SESSION_RESULTS);
    }

    pub fn forget_tab(&mut self, tab_id: i64) {
        self.tab_values.retain(|(_, tid), _| *tid != tab_id);
    }

    pub fn forget_window(&mut self, window_id: i64) {
        self.window_values.retain(|(_, wid), _| *wid != window_id);
    }

    pub fn drop_extension(&mut self, ext_id: &str) {
        self.tab_values.retain(|(eid, _), _| eid != ext_id);
        self.window_values.retain(|(eid, _), _| eid != ext_id);
    }
}

// ---- Helpers -------------------------------------------------------------------------------------------------------

fn iso_timestamp(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    // Civil date from Unix epoch seconds (Howard Hinnant algorithm).
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let rem = secs.rem_euclid(86_400);
    let h = rem / 3600;
    let min = (rem % 3600) / 60;
    let s = rem % 60;
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}.{millis:03}Z")
}

fn cookie_url_for(cookie: &Value) -> String {
    let secure = cookie.get("secure").and_then(Value::as_bool).unwrap_or(false);
    let scheme = if secure { "https" } else { "http" };
    let domain = cookie.get("domain").and_then(Value::as_str).unwrap_or("").trim_start_matches('.');
    let path = cookie.get("path").and_then(Value::as_str).unwrap_or("/");
    let path = if path.starts_with('/') { path } else { "/" };
    format!("{scheme}://{domain}{path}")
}

fn validate_cookie_same_site(v: Option<&Value>) -> Result<Option<&'static str>, ApiError> {
    match v {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => match s.as_str() {
            "no_restriction" => Ok(Some("no_restriction")),
            "lax" => Ok(Some("lax")),
            "strict" => Ok(Some("strict")),
            "unspecified" => Ok(Some("unspecified")),
            other => Err(ApiError::invalid(format!("Invalid sameSite value: '{other}'"))),
        },
        Some(_) => Err(ApiError::invalid("sameSite must be a string")),
    }
}

// ---- Dispatcher entry points ---------------------------------------------------------------------------------------

impl Dispatcher {
    pub(crate) fn data_api(&self, member: &Member, call: &ApiCall, reply: Reply) {
        match member.namespace.as_str() {
            "cookies" => self.cookies_api(&member.name, call, reply),
            "bookmarks" => self.bookmarks_api(&member.name, call, reply),
            "history" => self.history_api(&member.name, call, reply),
            "topSites" => self.top_sites_api(&member.name, call, reply),
            "browsingData" => self.browsing_data_api(&member.name, call, reply),
            "downloads" => self.downloads_api(&member.name, call, reply),
            "sessions" => self.sessions_api(&member.name, call, reply),
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, member.full_name.clone()))),
        }
    }

    pub(crate) fn data_host_event(&self, name: &str, args: &Value) -> Option<Value> {
        match name {
            "bookmarksState" => {
                self.data.borrow_mut().bookmarks.load_snapshot(args);
                Some(Value::Null)
            }
            "bookmarksImportBegan" => {
                self.broadcast_permitted("bookmarks", "bookmarks.onImportBegan", vec![]);
                Some(Value::Null)
            }
            "bookmarksImportEnded" => {
                self.broadcast_permitted("bookmarks", "bookmarks.onImportEnded", vec![]);
                Some(Value::Null)
            }
            "bookmarksChildrenReordered" => {
                let id = args.get("id").and_then(Value::as_str).unwrap_or("");
                let child_ids = args.get("childIds").cloned().unwrap_or_else(|| json!([]));
                self.broadcast_permitted("bookmarks", "bookmarks.onChildrenReordered", vec![json!(id), json!({ "childIds": child_ids })]);
                Some(Value::Null)
            }
            "cookieChanged" => {
                let Some(cookie) = args.get("cookie") else { return Some(Value::Null) };
                let url = cookie_url_for(cookie);
                let removed = args.get("removed").and_then(Value::as_bool).unwrap_or(false);
                let cause = args.get("cause").and_then(Value::as_str).unwrap_or("explicit");
                let info = json!({ "removed": removed, "cookie": cookie, "cause": cause });
                let targets: Vec<String> = self
                    .installed_ids()
                    .into_iter()
                    .filter(|id| {
                        self.with_extension(id, |e| e.permissions.has_api("cookies") && e.permissions.has_host(&url)).unwrap_or(false)
                    })
                    .collect();
                for id in targets {
                    self.host.dispatch_event(&id, "cookies.onChanged", vec![info.clone()]);
                }
                Some(Value::Null)
            }
            "historyVisited" => {
                self.broadcast_permitted("history", "history.onVisited", vec![args.clone()]);
                Some(Value::Null)
            }
            "historyRemoved" => {
                self.broadcast_permitted("history", "history.onVisitRemoved", vec![args.clone()]);
                Some(Value::Null)
            }
            "historyTitleChanged" => {
                self.broadcast_permitted("history", "history.onTitleChanged", vec![args.clone()]);
                Some(Value::Null)
            }
            "downloadCreated" => {
                let now = iso_timestamp(self.host.now_ms());
                let item = {
                    let mut data = self.data.borrow_mut();
                    let next = data.downloads.fresh_id();
                    let item = DownloadItem::from_json(args, next, now);
                    data.downloads.insert(item.clone());
                    item
                };
                self.broadcast_permitted("downloads", "downloads.onCreated", vec![item.to_json()]);
                Some(json!({ "id": item.id }))
            }
            "downloadChanged" => {
                let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
                let delta = self.data.borrow_mut().downloads.apply_delta(id, args);
                if let Some(d) = delta {
                    self.broadcast_permitted("downloads", "downloads.onChanged", vec![d]);
                }
                Some(Value::Null)
            }
            "downloadErased" => {
                let id = args.get("id").and_then(Value::as_i64).unwrap_or(0);
                if self.data.borrow_mut().downloads.items.remove(&id).is_some() {
                    self.broadcast_permitted("downloads", "downloads.onErased", vec![json!(id)]);
                }
                Some(Value::Null)
            }
            "downloadDeterminingFilename" => {
                self.broadcast_permitted("downloads", "downloads.onDeterminingFilename", vec![args.clone()]);
                Some(Value::Null)
            }
            "sessionsChanged" => {
                self.broadcast_permitted("sessions", "sessions.onChanged", vec![]);
                Some(Value::Null)
            }
            _ => None,
        }
    }

    pub(crate) fn broadcast_permitted(&self, perm: &str, event: &str, args: Vec<Value>) {
        let targets: Vec<String> = self
            .installed_ids()
            .into_iter()
            .filter(|id| self.with_extension(id, |e| perm.is_empty() || e.permissions.has_api(perm)).unwrap_or(false))
            .collect();
        for id in targets {
            self.host.dispatch_event(&id, event, args.clone());
        }
    }

    pub(crate) fn record_sessions_from_browser_diff(&self, old: &BrowserState, new: &BrowserState) {
        let now_sec = self.host.now_ms() / 1000;
        let new_wins: HashSet<i64> = new.windows.iter().map(|w| w.id).collect();
        let new_tabs: HashSet<i64> = new.tabs().map(|t| t.id).collect();
        let mut changed = false;
        {
            let mut data = self.data.borrow_mut();
            for w in &old.windows {
                if !new_wins.contains(&w.id) {
                    if !w.incognito && !w.tabs.is_empty() {
                        data.sessions.record_closed_window(w.clone(), now_sec);
                        changed = true;
                    }
                    data.sessions.forget_window(w.id);
                    for t in &w.tabs {
                        data.sessions.forget_tab(t.id);
                    }
                } else {
                    for t in &w.tabs {
                        if !new_tabs.contains(&t.id) {
                            if !t.incognito {
                                data.sessions.record_closed_tab(t.clone(), now_sec);
                                changed = true;
                            }
                            data.sessions.forget_tab(t.id);
                        }
                    }
                }
            }
        }
        if changed {
            self.broadcast_permitted("sessions", "sessions.onChanged", vec![]);
        }
    }

    // ---- cookies ----------------------------------------------------------------------------------------------------

    fn check_cookie_host(&self, call: &ApiCall, url: &str) -> Result<(), ApiError> {
        if split_url(url).is_none() {
            return Err(ApiError::invalid(format!("Invalid url: '{url}'")));
        }
        let allowed = self.with_ext(call, |e| Ok(e.permissions.has_host(url)))?;
        if !allowed {
            return Err(ApiError::new(ErrorKind::PermissionDenied, format!("No host permissions for cookies at url: \"{url}\".")));
        }
        Ok(())
    }

    fn cookies_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "get" => {
                let details = call.arg(0);
                let Some(url) = details.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                let Some(name) = details.get("name").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.name must be a string")));
                };
                if let Err(e) = self.check_cookie_host(call, url) {
                    return reply(Err(e));
                }
                let mut req = json!({ "url": url, "name": name });
                if let Some(s) = details.get("storeId").and_then(Value::as_str) {
                    req["storeId"] = json!(s);
                }
                if let Some(pk) = details.get("partitionKey").filter(|v| v.is_object()) {
                    req["partitionKey"] = pk.clone();
                }
                self.host_call(call, "host.cookies.get", req, reply, |_, v| Ok(if v.is_object() { v } else { Value::Null }));
            }
            "getAll" => {
                let details = call.arg(0);
                if !details.is_null() && !details.is_object() {
                    return reply(Err(ApiError::invalid("details must be an object")));
                }
                if let Some(url) = details.get("url").and_then(Value::as_str)
                    && let Err(e) = self.check_cookie_host(call, url)
                {
                    return reply(Err(e));
                }
                let ext_id = call.extension_id.clone();
                let req = if details.is_object() { details.clone() } else { json!({}) };
                self.host_call(call, "host.cookies.getAll", req, reply, move |this, v| {
                    let list = v.as_array().cloned().unwrap_or_default();
                    let filtered: Vec<Value> = list
                        .into_iter()
                        .filter(|c| {
                            let u = cookie_url_for(c);
                            this.with_extension(&ext_id, |e| e.permissions.has_host(&u)).unwrap_or(false)
                        })
                        .collect();
                    Ok(Value::Array(filtered))
                });
            }
            "set" => {
                let details = call.arg(0);
                let Some(url) = details.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                if let Err(e) = self.check_cookie_host(call, url) {
                    return reply(Err(e));
                }
                if let Err(e) = validate_cookie_same_site(details.get("sameSite")) {
                    return reply(Err(e));
                }
                let name = details.get("name").and_then(Value::as_str).unwrap_or("");
                let value = details.get("value").and_then(Value::as_str).unwrap_or("");
                let mut req = details.clone();
                req["name"] = json!(name);
                req["value"] = json!(value);
                self.host_call(call, "host.cookies.set", req, reply, |_, v| Ok(v));
            }
            "remove" => {
                let details = call.arg(0);
                let Some(url) = details.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                let Some(name) = details.get("name").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.name must be a string")));
                };
                if let Err(e) = self.check_cookie_host(call, url) {
                    return reply(Err(e));
                }
                let mut req = json!({ "url": url, "name": name });
                if let Some(s) = details.get("storeId").and_then(Value::as_str) {
                    req["storeId"] = json!(s);
                }
                self.host_call(call, "host.cookies.remove", req, reply, |_, v| Ok(if v.is_object() { v } else { Value::Null }));
            }
            "getAllCookieStores" => {
                let firefox = self.with_ext(call, |e| Ok(crate::browser_api::is_firefox(e))).unwrap_or(false);
                let state = self.browser.borrow();
                let mut normal_tabs = vec![];
                let mut private_tabs = vec![];
                for t in state.tabs() {
                    if t.incognito {
                        private_tabs.push(t.id);
                    } else {
                        normal_tabs.push(t.id);
                    }
                }
                let mut stores = vec![json!({
                    "id": if firefox { "firefox-default" } else { "0" },
                    "tabIds": normal_tabs,
                    "incognito": false,
                })];
                if !private_tabs.is_empty() {
                    stores.push(json!({
                        "id": if firefox { "firefox-private" } else { "1" },
                        "tabIds": private_tabs,
                        "incognito": true,
                    }));
                }
                reply(Ok(Value::Array(stores)));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("cookies.{op}")))),
        }
    }

    // ---- bookmarks --------------------------------------------------------------------------------------------------

    fn bookmarks_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        let now = self.host.now_ms();
        match op {
            "get" => {
                let ids: Result<Vec<String>, ApiError> = match call.arg(0) {
                    Value::String(s) => Ok(vec![s.clone()]),
                    Value::Array(a) if !a.is_empty() => a
                        .iter()
                        .map(|v| v.as_str().map(String::from).ok_or_else(|| ApiError::invalid("idList must contain strings")))
                        .collect(),
                    _ => Err(ApiError::invalid("idOrIdList must be a string or non-empty array of strings")),
                };
                let ids = match ids {
                    Ok(v) => v,
                    Err(e) => return reply(Err(e)),
                };
                let data = self.data.borrow();
                let mut out = vec![];
                for id in &ids {
                    match data.bookmarks.node_json(id, false) {
                        Some(n) => out.push(n),
                        None => return reply(Err(ApiError::runtime("Can't find bookmark for id."))),
                    }
                }
                reply(Ok(Value::Array(out)));
            }
            "getChildren" => {
                let Some(id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("id must be a string")));
                };
                let data = self.data.borrow();
                let Some(node) = data.bookmarks.nodes.get(id) else {
                    return reply(Err(ApiError::runtime("Can't find bookmark for id.")));
                };
                let kids: Vec<Value> = node.children.iter().filter_map(|c| data.bookmarks.node_json(c, false)).collect();
                reply(Ok(Value::Array(kids)));
            }
            "getRecent" => {
                let Some(n) = call.arg(0).as_i64().filter(|&n| n >= 1) else {
                    return reply(Err(ApiError::invalid("numberOfItems must be a positive integer")));
                };
                reply(Ok(Value::Array(self.data.borrow().bookmarks.recent(n as usize))));
            }
            "getTree" => {
                let data = self.data.borrow();
                let root = data.bookmarks.node_json(&data.bookmarks.root_id, true).unwrap_or(Value::Null);
                reply(Ok(Value::Array(vec![root])));
            }
            "getSubTree" => {
                let Some(id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("id must be a string")));
                };
                let data = self.data.borrow();
                match data.bookmarks.node_json(id, true) {
                    Some(n) => reply(Ok(Value::Array(vec![n]))),
                    None => reply(Err(ApiError::runtime("Can't find bookmark for id."))),
                }
            }
            "search" => {
                let res = self.data.borrow().bookmarks.search(call.arg(0)).map(Value::Array).map_err(ApiError::invalid);
                reply(res);
            }
            "create" => {
                let details = call.arg(0);
                if !details.is_object() {
                    return reply(Err(ApiError::invalid("bookmark must be an object")));
                }
                let created = match self.data.borrow_mut().bookmarks.create(details, now) {
                    Ok(n) => n,
                    Err(e) => return reply(Err(ApiError::runtime(e))),
                };
                let id = created["id"].as_str().unwrap_or("").to_string();
                let host_call = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.bookmarks.create".into(),
                    args: vec![created.clone()],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&host_call, Box::new(|_| {}));
                self.broadcast_permitted("bookmarks", "bookmarks.onCreated", vec![json!(id), created.clone()]);
                reply(Ok(created));
            }
            "update" => {
                let Some(id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("id must be a string")));
                };
                let changes = call.arg(1);
                if !changes.is_object() {
                    return reply(Err(ApiError::invalid("changes must be an object")));
                }
                let (node, change_info) = match self.data.borrow_mut().bookmarks.update(id, changes) {
                    Ok(pair) => pair,
                    Err(e) => return reply(Err(ApiError::runtime(e))),
                };
                let host_call = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.bookmarks.update".into(),
                    args: vec![json!({ "id": id, "title": node["title"], "url": node.get("url") })],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&host_call, Box::new(|_| {}));
                self.broadcast_permitted("bookmarks", "bookmarks.onChanged", vec![json!(id), change_info]);
                reply(Ok(node));
            }
            "move" => {
                let Some(id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("id must be a string")));
                };
                let dest = call.arg(1);
                if !dest.is_object() {
                    return reply(Err(ApiError::invalid("destination must be an object")));
                }
                let (node, move_info) = match self.data.borrow_mut().bookmarks.move_node(id, dest, now) {
                    Ok(pair) => pair,
                    Err(e) => return reply(Err(ApiError::runtime(e))),
                };
                let host_call = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.bookmarks.move".into(),
                    args: vec![json!({ "id": id, "parentId": move_info["parentId"], "index": move_info["index"] })],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&host_call, Box::new(|_| {}));
                self.broadcast_permitted("bookmarks", "bookmarks.onMoved", vec![json!(id), move_info]);
                reply(Ok(node));
            }
            "remove" | "removeTree" => {
                let Some(id) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("id must be a string")));
                };
                let remove_info = match self.data.borrow_mut().bookmarks.remove(id, op == "removeTree", now) {
                    Ok(info) => info,
                    Err(e) => return reply(Err(ApiError::runtime(e))),
                };
                let host_call = ApiCall {
                    extension_id: call.extension_id.clone(),
                    function: "host.bookmarks.remove".into(),
                    args: vec![json!({ "id": id, "recursive": op == "removeTree" })],
                    caller: call.caller.clone(),
                };
                self.host.call_browser(&host_call, Box::new(|_| {}));
                self.broadcast_permitted("bookmarks", "bookmarks.onRemoved", vec![json!(id), remove_info]);
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("bookmarks.{op}")))),
        }
    }

    // ---- history & topSites -----------------------------------------------------------------------------------------

    fn history_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "search" => {
                let query = call.arg(0);
                let Some(text) = query.get("text").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("query.text must be a string")));
                };
                let now = self.host.now_ms() as f64;
                let start_time = query.get("startTime").and_then(Value::as_f64).unwrap_or(now - 86_400_000.0);
                let end_time = query.get("endTime").and_then(Value::as_f64);
                let max_results = query.get("maxResults").and_then(Value::as_u64).unwrap_or(100);
                let mut req = json!({ "text": text, "startTime": start_time, "maxResults": max_results });
                if let Some(end) = end_time {
                    req["endTime"] = json!(end);
                }
                self.host_call(call, "host.history.search", req, reply, |_, v| Ok(if v.is_array() { v } else { Value::Array(vec![]) }));
            }
            "getVisits" => {
                let Some(url) = call.arg(0).get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                if split_url(url).is_none() {
                    return reply(Err(ApiError::invalid("Invalid URL.")));
                }
                self.host_call(call, "host.history.getVisits", json!({ "url": url }), reply, |_, v| {
                    Ok(if v.is_array() { v } else { Value::Array(vec![]) })
                });
            }
            "addUrl" => {
                let details = call.arg(0);
                let Some(url) = details.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                if split_url(url).is_none() {
                    return reply(Err(ApiError::invalid("Invalid URL.")));
                }
                let mut req = json!({ "url": url });
                if let Some(t) = details.get("title").and_then(Value::as_str) {
                    req["title"] = json!(t);
                }
                if let Some(tr) = details.get("transition").and_then(Value::as_str) {
                    req["transition"] = json!(tr);
                }
                if let Some(vt) = details.get("visitTime").and_then(Value::as_f64) {
                    req["visitTime"] = json!(vt);
                }
                self.host_call(call, "host.history.addUrl", req, reply, |_, _| Ok(Value::Null));
            }
            "deleteUrl" => {
                let Some(url) = call.arg(0).get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("details.url must be a string")));
                };
                if split_url(url).is_none() {
                    return reply(Err(ApiError::invalid("Invalid URL.")));
                }
                let url_owned = url.to_string();
                self.host_call(call, "host.history.deleteUrl", json!({ "url": url }), reply, move |this, _| {
                    this.broadcast_permitted(
                        "history",
                        "history.onVisitRemoved",
                        vec![json!({ "allHistory": false, "urls": [url_owned] })],
                    );
                    Ok(Value::Null)
                });
            }
            "deleteRange" => {
                let range = call.arg(0);
                let (Some(start), Some(end)) =
                    (range.get("startTime").and_then(Value::as_f64), range.get("endTime").and_then(Value::as_f64))
                else {
                    return reply(Err(ApiError::invalid("range must include startTime and endTime")));
                };
                if start > end {
                    return reply(Err(ApiError::invalid("startTime must not be greater than endTime")));
                }
                self.host_call(call, "host.history.deleteRange", json!({ "startTime": start, "endTime": end }), reply, |_, _| {
                    Ok(Value::Null)
                });
            }
            "deleteAll" => {
                self.host_call(call, "host.history.deleteAll", json!({}), reply, |this, _| {
                    this.broadcast_permitted("history", "history.onVisitRemoved", vec![json!({ "allHistory": true, "urls": [] })]);
                    Ok(Value::Null)
                });
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("history.{op}")))),
        }
    }

    fn top_sites_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        if op != "get" {
            return reply(Err(ApiError::new(ErrorKind::Unhandled, format!("topSites.{op}"))));
        }
        let opts = call.arg(0);
        if !opts.is_null() && !opts.is_object() {
            return reply(Err(ApiError::invalid("options must be an object")));
        }
        let limit = opts.get("limit").and_then(Value::as_u64).unwrap_or(12).clamp(1, 100);
        let req = json!({
            "limit": limit,
            "includeFavicon": opts.get("includeFavicon").and_then(Value::as_bool).unwrap_or(false),
            "onePerDomain": opts.get("onePerDomain").and_then(Value::as_bool).unwrap_or(true),
        });
        self.host_call(call, "host.topSites.get", req, reply, |_, v| Ok(if v.is_array() { v } else { Value::Array(vec![]) }));
    }

    // ---- browsingData -----------------------------------------------------------------------------------------------

    fn browsing_data_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        if op == "settings" {
            return reply(Ok(json!({
                "options": { "since": 0, "originTypes": { "unprotectedWeb": true, "protectedWeb": false, "extension": false } },
                "dataToRemove": {
                    "cache": true, "cookies": true, "downloads": true, "formData": false,
                    "history": true, "indexedDB": true, "localStorage": true, "passwords": false, "pluginData": false,
                },
                "dataRemovalPermitted": {
                    "cache": true, "cookies": true, "downloads": true, "formData": true,
                    "history": true, "indexedDB": true, "localStorage": true, "passwords": true, "pluginData": true,
                },
            })));
        }
        let options = call.arg(0);
        if !options.is_object() {
            return reply(Err(ApiError::invalid("options must be an object")));
        }
        if options.get("origins").is_some() && options.get("excludeOrigins").is_some() {
            return reply(Err(ApiError::invalid("Cannot specify both origins and excludeOrigins")));
        }
        let data_types = match op {
            "remove" => {
                let dt = call.arg(1);
                if !dt.is_object() {
                    return reply(Err(ApiError::invalid("dataToRemove must be an object")));
                }
                dt.clone()
            }
            "removeCache" => json!({ "cache": true }),
            "removeCookies" => json!({ "cookies": true }),
            "removeDownloads" => json!({ "downloads": true }),
            "removeFormData" => json!({ "formData": true }),
            "removeHistory" => json!({ "history": true }),
            "removeLocalStorage" => json!({ "localStorage": true }),
            "removePasswords" => json!({ "passwords": true }),
            "removeIndexedDB" => json!({ "indexedDB": true }),
            "removePluginData" => json!({ "pluginData": true }),
            _ => return reply(Err(ApiError::new(ErrorKind::Unhandled, format!("browsingData.{op}")))),
        };
        let req = json!({ "options": options, "dataToRemove": data_types });
        self.host_call(call, "host.browsingData.remove", req, reply, |_, _| Ok(Value::Null));
    }

    // ---- downloads --------------------------------------------------------------------------------------------------

    fn downloads_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "download" => {
                let opts = call.arg(0);
                let Some(url) = opts.get("url").and_then(Value::as_str) else {
                    return reply(Err(ApiError::invalid("options.url must be a string")));
                };
                if split_url(url).is_none() {
                    return reply(Err(ApiError::invalid("Invalid URL.")));
                }
                if let Some(filename) = opts.get("filename").and_then(Value::as_str)
                    && (filename.starts_with('/') || filename.contains(".."))
                {
                    return reply(Err(ApiError::invalid("filename must be a relative path without '..'")));
                }
                let now_iso = iso_timestamp(self.host.now_ms());
                let (ext_id, ext_name) = self
                    .with_ext(call, |e| Ok((e.id.clone(), e.i18n.localize(&e.manifest.name))))
                    .unwrap_or_else(|_| (call.extension_id.clone(), String::new()));
                let id = self.data.borrow_mut().downloads.fresh_id();
                let filename = opts.get("filename").and_then(Value::as_str).unwrap_or("").to_string();
                let item = DownloadItem {
                    id,
                    url: url.to_string(),
                    final_url: url.to_string(),
                    referrer: String::new(),
                    filename,
                    incognito: opts.get("incognito").and_then(Value::as_bool).unwrap_or(false),
                    danger: "safe".into(),
                    mime: "application/octet-stream".into(),
                    start_time: now_iso,
                    end_time: None,
                    state: "in_progress".into(),
                    paused: false,
                    can_resume: false,
                    error: None,
                    bytes_received: 0,
                    total_bytes: -1,
                    file_size: -1,
                    exists: true,
                    by_extension_id: Some(ext_id),
                    by_extension_name: Some(ext_name),
                };
                self.data.borrow_mut().downloads.insert(item.clone());
                let mut req = opts.clone();
                req["id"] = json!(id);
                self.host_call(call, "host.downloads.download", req, reply, move |this, _| {
                    this.broadcast_permitted("downloads", "downloads.onCreated", vec![item.to_json()]);
                    Ok(json!(id))
                });
            }
            "search" => {
                let res = self.data.borrow().downloads.search(call.arg(0)).map_err(ApiError::invalid);
                reply(res.map(|items| Value::Array(items.iter().map(DownloadItem::to_json).collect())));
            }
            "pause" | "resume" | "cancel" => {
                let Some(id) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("downloadId must be an integer")));
                };
                if !self.data.borrow().downloads.items.contains_key(&id) {
                    return reply(Err(ApiError::runtime(format!("Invalid downloadId: {id}"))));
                }
                let op_owned = op.to_string();
                let prim = format!("host.downloads.{op}");
                self.host_call(call, &prim, json!({ "id": id }), reply, move |this, _| {
                    let delta = match op_owned.as_str() {
                        "pause" => json!({ "paused": true, "canResume": true }),
                        "resume" => json!({ "paused": false, "canResume": false }),
                        "cancel" => json!({ "state": "interrupted", "error": "USER_CANCELED", "paused": false, "canResume": false }),
                        _ => json!({}),
                    };
                    if let Some(d) = this.data.borrow_mut().downloads.apply_delta(id, &delta) {
                        this.broadcast_permitted("downloads", "downloads.onChanged", vec![d]);
                    }
                    Ok(Value::Null)
                });
            }
            "erase" => {
                let matched = match self.data.borrow().downloads.search(call.arg(0)) {
                    Ok(v) => v,
                    Err(e) => return reply(Err(ApiError::invalid(e))),
                };
                let ids: Vec<i64> = matched.iter().map(|d| d.id).collect();
                {
                    let mut data = self.data.borrow_mut();
                    for id in &ids {
                        data.downloads.items.remove(id);
                    }
                }
                for id in &ids {
                    self.broadcast_permitted("downloads", "downloads.onErased", vec![json!(id)]);
                }
                reply(Ok(json!(ids)));
            }
            "removeFile" => {
                let Some(id) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("downloadId must be an integer")));
                };
                let state = self.data.borrow().downloads.items.get(&id).map(|d| d.state.clone());
                match state.as_deref() {
                    None => return reply(Err(ApiError::runtime(format!("Invalid downloadId: {id}")))),
                    Some(s) if s != "complete" => return reply(Err(ApiError::runtime("Download is not complete."))),
                    _ => {}
                }
                self.host_call(call, "host.downloads.removeFile", json!({ "id": id }), reply, move |this, _| {
                    if let Some(d) = this.data.borrow_mut().downloads.apply_delta(id, &json!({ "exists": false })) {
                        this.broadcast_permitted("downloads", "downloads.onChanged", vec![d]);
                    }
                    Ok(Value::Null)
                });
            }
            "open" => {
                let allowed = self.with_ext(call, |e| Ok(e.permissions.has_api("downloads.open"))).unwrap_or(false);
                if !allowed {
                    return reply(Err(ApiError::new(
                        ErrorKind::PermissionDenied,
                        "'downloads.open' requires the \"downloads.open\" permission",
                    )));
                }
                let Some(id) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("downloadId must be an integer")));
                };
                if !self.data.borrow().downloads.items.contains_key(&id) {
                    return reply(Err(ApiError::runtime(format!("Invalid downloadId: {id}"))));
                }
                self.host_call(call, "host.downloads.open", json!({ "id": id }), reply, |_, _| Ok(Value::Null));
            }
            "show" => {
                let Some(id) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("downloadId must be an integer")));
                };
                if !self.data.borrow().downloads.items.contains_key(&id) {
                    return reply(Err(ApiError::runtime(format!("Invalid downloadId: {id}"))));
                }
                self.host_call(call, "host.downloads.show", json!({ "id": id }), reply, |_, _| Ok(Value::Null));
            }
            "showDefaultFolder" => {
                self.host_call(call, "host.downloads.showDefaultFolder", json!({}), reply, |_, _| Ok(Value::Null));
            }
            "acceptDanger" | "drag" => {
                let Some(id) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("downloadId must be an integer")));
                };
                if !self.data.borrow().downloads.items.contains_key(&id) {
                    return reply(Err(ApiError::runtime(format!("Invalid downloadId: {id}"))));
                }
                let prim = format!("host.downloads.{op}");
                self.host_call(call, &prim, json!({ "id": id }), reply, |_, _| Ok(Value::Null));
            }
            "setShelfEnabled" => {
                let allowed = self.with_ext(call, |e| Ok(e.permissions.has_api("downloads.shelf"))).unwrap_or(false);
                if !allowed {
                    return reply(Err(ApiError::new(
                        ErrorKind::PermissionDenied,
                        "'downloads.setShelfEnabled' requires the \"downloads.shelf\" permission",
                    )));
                }
                let Some(enabled) = call.arg(0).as_bool() else {
                    return reply(Err(ApiError::invalid("enabled must be a boolean")));
                };
                self.data.borrow_mut().downloads.shelf_enabled = enabled;
                reply(Ok(Value::Null));
            }
            "setUiOptions" => {
                let allowed = self
                    .with_ext(call, |e| Ok(e.permissions.has_api("downloads.ui") || e.permissions.has_api("downloads.shelf")))
                    .unwrap_or(false);
                if !allowed {
                    return reply(Err(ApiError::new(
                        ErrorKind::PermissionDenied,
                        "'downloads.setUiOptions' requires the \"downloads.ui\" permission",
                    )));
                }
                let Some(enabled) = call.arg(0).get("enabled").and_then(Value::as_bool) else {
                    return reply(Err(ApiError::invalid("options.enabled must be a boolean")));
                };
                self.data.borrow_mut().downloads.shelf_enabled = enabled;
                reply(Ok(Value::Null));
            }
            "getFileIcon" => {
                let Some(id) = call.arg(0).as_i64() else {
                    return reply(Err(ApiError::invalid("downloadId must be an integer")));
                };
                if !self.data.borrow().downloads.items.contains_key(&id) {
                    return reply(Err(ApiError::runtime(format!("Invalid downloadId: {id}"))));
                }
                let size = call.arg(1).get("size").and_then(Value::as_u64).unwrap_or(32);
                if size != 16 && size != 32 {
                    return reply(Err(ApiError::invalid("options.size must be 16 or 32")));
                }
                self.host_call(call, "host.downloads.getFileIcon", json!({ "id": id, "size": size }), reply, |_, v| {
                    Ok(if v.is_string() { v } else { json!("") })
                });
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("downloads.{op}")))),
        }
    }

    // ---- sessions ---------------------------------------------------------------------------------------------------

    fn sessions_api(&self, op: &str, call: &ApiCall, reply: Reply) {
        match op {
            "getRecentlyClosed" => {
                let max = call.arg(0).get("maxResults").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(MAX_SESSION_RESULTS);
                if max > MAX_SESSION_RESULTS {
                    return reply(Err(ApiError::invalid(format!("maxResults must be at most {MAX_SESSION_RESULTS}"))));
                }
                let data = self.data.borrow();
                let list: Vec<Value> = self
                    .with_visibility(&call.extension_id, |vis| {
                        data.sessions
                            .recently_closed
                            .iter()
                            .take(max)
                            .map(|entry| match entry {
                                ClosedEntry::Tab { session_id, last_modified, tab } => {
                                    let mut t = tab_json(tab, vis);
                                    t["sessionId"] = json!(session_id);
                                    json!({ "lastModified": last_modified, "tab": t })
                                }
                                ClosedEntry::Window { session_id, last_modified, window } => {
                                    let mut w = window_json(window, true, vis);
                                    w["sessionId"] = json!(session_id);
                                    json!({ "lastModified": last_modified, "window": w })
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                reply(Ok(Value::Array(list)));
            }
            "getDevices" => reply(Ok(Value::Array(vec![]))),
            "restore" => {
                let target_id = call.arg(0).as_str().map(String::from);
                let entry = {
                    let mut data = self.data.borrow_mut();
                    let pos = match &target_id {
                        Some(sid) => data.sessions.recently_closed.iter().position(|e| e.session_id() == sid),
                        None => (!data.sessions.recently_closed.is_empty()).then_some(0),
                    };
                    match pos {
                        Some(i) => data.sessions.recently_closed.remove(i),
                        None => return reply(Err(ApiError::runtime("Invalid session id."))),
                    }
                };
                self.broadcast_permitted("sessions", "sessions.onChanged", vec![]);
                let now_sec = self.host.now_ms() / 1000;
                match entry {
                    ClosedEntry::Tab { tab, .. } => {
                        let req = json!({ "url": tab.url, "active": true, "pinned": tab.pinned });
                        let ext_id = call.extension_id.clone();
                        self.host_call(call, "host.tabs.create", req, reply, move |this, v| {
                            let tid = v.get("tabId").and_then(Value::as_i64);
                            let restored = tid
                                .and_then(|id| this.browser.borrow().tab(id).cloned())
                                .and_then(|t| this.with_visibility(&ext_id, |vis| tab_json(&t, vis)))
                                .unwrap_or_else(|| json!({ "id": tid.unwrap_or(-1), "url": tab.url }));
                            Ok(json!({ "lastModified": now_sec, "tab": restored }))
                        });
                    }
                    ClosedEntry::Window { window, .. } => {
                        let urls: Vec<String> = window.tabs.iter().map(|t| t.url.clone()).collect();
                        let req = json!({ "urls": urls, "focused": true, "incognito": false, "type": window.kind });
                        let ext_id = call.extension_id.clone();
                        self.host_call(call, "host.windows.create", req, reply, move |this, v| {
                            let wid = v.get("windowId").and_then(Value::as_i64);
                            let restored = wid
                                .and_then(|id| this.browser.borrow().window(id).cloned())
                                .and_then(|w| this.with_visibility(&ext_id, |vis| window_json(&w, true, vis)))
                                .unwrap_or_else(|| json!({ "id": wid.unwrap_or(-1) }));
                            Ok(json!({ "lastModified": now_sec, "window": restored }))
                        });
                    }
                }
            }
            "forgetClosedTab" => {
                let (Some(wid), Some(sid)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("windowId and sessionId are required")));
                };
                let removed = {
                    let mut data = self.data.borrow_mut();
                    let before = data.sessions.recently_closed.len();
                    data.sessions
                        .recently_closed
                        .retain(|e| !(matches!(e, ClosedEntry::Tab { .. }) && e.window_id() == wid && e.session_id() == sid));
                    data.sessions.recently_closed.len() != before
                };
                if !removed {
                    return reply(Err(ApiError::runtime("Could not find closed tab for sessionId.")));
                }
                self.broadcast_permitted("sessions", "sessions.onChanged", vec![]);
                reply(Ok(Value::Null));
            }
            "forgetClosedWindow" => {
                let Some(sid) = call.arg(0).as_str() else {
                    return reply(Err(ApiError::invalid("sessionId must be a string")));
                };
                let removed = {
                    let mut data = self.data.borrow_mut();
                    let before = data.sessions.recently_closed.len();
                    data.sessions.recently_closed.retain(|e| !(matches!(e, ClosedEntry::Window { .. }) && e.session_id() == sid));
                    data.sessions.recently_closed.len() != before
                };
                if !removed {
                    return reply(Err(ApiError::runtime("Could not find closed window for sessionId.")));
                }
                self.broadcast_permitted("sessions", "sessions.onChanged", vec![]);
                reply(Ok(Value::Null));
            }
            "setTabValue" => {
                let (Some(tab_id), Some(key)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("tabId and key are required")));
                };
                if self.browser.borrow().tab(tab_id).is_none() {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tab_id}."))));
                }
                let val = call.arg(2).clone();
                self.data
                    .borrow_mut()
                    .sessions
                    .tab_values
                    .entry((call.extension_id.clone(), tab_id))
                    .or_default()
                    .insert(key.to_string(), val);
                reply(Ok(Value::Null));
            }
            "getTabValue" => {
                let (Some(tab_id), Some(key)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("tabId and key are required")));
                };
                if self.browser.borrow().tab(tab_id).is_none() {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tab_id}."))));
                }
                let v = self
                    .data
                    .borrow()
                    .sessions
                    .tab_values
                    .get(&(call.extension_id.clone(), tab_id))
                    .and_then(|m| m.get(key))
                    .cloned()
                    .unwrap_or(Value::Null);
                reply(Ok(v));
            }
            "removeTabValue" => {
                let (Some(tab_id), Some(key)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("tabId and key are required")));
                };
                if self.browser.borrow().tab(tab_id).is_none() {
                    return reply(Err(ApiError::runtime(format!("No tab with id: {tab_id}."))));
                }
                if let Some(m) = self.data.borrow_mut().sessions.tab_values.get_mut(&(call.extension_id.clone(), tab_id)) {
                    m.remove(key);
                }
                reply(Ok(Value::Null));
            }
            "setWindowValue" => {
                let (Some(win_id), Some(key)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("windowId and key are required")));
                };
                if self.browser.borrow().window(win_id).is_none() {
                    return reply(Err(ApiError::runtime(format!("No window with id: {win_id}."))));
                }
                let val = call.arg(2).clone();
                self.data
                    .borrow_mut()
                    .sessions
                    .window_values
                    .entry((call.extension_id.clone(), win_id))
                    .or_default()
                    .insert(key.to_string(), val);
                reply(Ok(Value::Null));
            }
            "getWindowValue" => {
                let (Some(win_id), Some(key)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("windowId and key are required")));
                };
                if self.browser.borrow().window(win_id).is_none() {
                    return reply(Err(ApiError::runtime(format!("No window with id: {win_id}."))));
                }
                let v = self
                    .data
                    .borrow()
                    .sessions
                    .window_values
                    .get(&(call.extension_id.clone(), win_id))
                    .and_then(|m| m.get(key))
                    .cloned()
                    .unwrap_or(Value::Null);
                reply(Ok(v));
            }
            "removeWindowValue" => {
                let (Some(win_id), Some(key)) = (call.arg(0).as_i64(), call.arg(1).as_str()) else {
                    return reply(Err(ApiError::invalid("windowId and key are required")));
                };
                if self.browser.borrow().window(win_id).is_none() {
                    return reply(Err(ApiError::runtime(format!("No window with id: {win_id}."))));
                }
                if let Some(m) = self.data.borrow_mut().sessions.window_values.get_mut(&(call.extension_id.clone(), win_id)) {
                    m.remove(key);
                }
                reply(Ok(Value::Null));
            }
            _ => reply(Err(ApiError::new(ErrorKind::Unhandled, format!("sessions.{op}")))),
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

    type ReplyFn = Box<dyn Fn(&str, &Value) -> ApiResult>;

    #[derive(Default)]
    struct FakeHost {
        calls: RefCell<Vec<(String, Value)>>,
        events: RefCell<Vec<(String, String, Vec<Value>)>>,
        now: RefCell<i64>,
        reply_fn: RefCell<Option<ReplyFn>>,
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
            let arg = call.args.first().cloned().unwrap_or(Value::Null);
            self.calls.borrow_mut().push((call.function.clone(), arg.clone()));
            if let Some(f) = self.reply_fn.borrow().as_ref() {
                reply(f(&call.function, &arg));
            } else {
                reply(Ok(Value::Null));
            }
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
        let out = Rc::new(RefCell::new(None));
        let sink = out.clone();
        d.dispatch(
            ApiCall { extension_id: ID.into(), function: f.into(), args, caller: Caller::background() },
            Box::new(move |r| *sink.borrow_mut() = Some(r)),
        );
        out.borrow_mut().take().expect("synchronous reply")
    }

    #[test]
    fn cookies_enforce_host_permissions_and_filter_events() {
        let (d, host) = setup(
            r#"{"manifest_version":3,"name":"C","version":"1","permissions":["cookies"],"host_permissions":["https://allowed.test/*"]}"#,
        );
        *host.reply_fn.borrow_mut() = Some(Box::new(|f, _| match f {
            "host.cookies.getAll" => Ok(json!([
                { "name": "a", "value": "1", "domain": "allowed.test", "path": "/", "secure": true },
                { "name": "b", "value": "2", "domain": "secret.test", "path": "/", "secure": true }
            ])),
            "host.cookies.get" => Ok(json!({ "name": "a", "value": "1", "domain": "allowed.test", "path": "/" })),
            "host.cookies.set" => Ok(json!({ "name": "k", "value": "v", "domain": "allowed.test", "path": "/" })),
            _ => Ok(Value::Null),
        }));

        // Host permission denied for ungranted origin.
        assert_eq!(
            call(&d, "cookies.get", vec![json!({"url": "https://secret.test/", "name": "b"})]).unwrap_err().kind,
            ErrorKind::PermissionDenied
        );
        // Granted origin works.
        assert_eq!(call(&d, "cookies.get", vec![json!({"url": "https://allowed.test/", "name": "a"})]).unwrap()["value"], "1");
        // getAll strips cookies outside host_permissions.
        let all = call(&d, "cookies.getAll", vec![json!({})]).unwrap();
        assert_eq!(all.as_array().unwrap().len(), 1);
        assert_eq!(all[0]["name"], "a");
        // set validates sameSite.
        assert_eq!(
            call(&d, "cookies.set", vec![json!({"url": "https://allowed.test/", "sameSite": "bogus"})]).unwrap_err().kind,
            ErrorKind::InvalidArguments
        );
        assert!(
            call(&d, "cookies.set", vec![json!({"url": "https://allowed.test/", "name": "k", "value": "v", "sameSite": "lax"})]).is_ok()
        );

        // cookieChanged only reaches extensions with host permission for that cookie.
        d.host_event(
            "cookieChanged",
            &json!({"removed": false, "cause": "explicit", "cookie": {"name": "x", "domain": "secret.test", "path": "/", "secure": true}}),
        );
        d.host_event(
            "cookieChanged",
            &json!({"removed": false, "cause": "explicit", "cookie": {"name": "y", "domain": "allowed.test", "path": "/", "secure": true}}),
        );
        let evs: Vec<_> = host.events.borrow().iter().filter(|(_, e, _)| e == "cookies.onChanged").cloned().collect();
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].2[0]["cookie"]["name"], "y");

        // Cookie stores derive from BrowserState.
        d.host_event("browserState", &json!({"windows": [{"id": 1, "focused": true, "tabs": [{"id": 10, "url": "https://a.test/"}]}]}));
        let stores = call(&d, "cookies.getAllCookieStores", vec![]).unwrap();
        assert_eq!(stores[0]["id"], "0");
        assert_eq!(stores[0]["tabIds"], json!([10]));
    }

    #[test]
    fn bookmarks_crud_search_and_events() {
        let (d, host) = setup(r#"{"manifest_version":3,"name":"B","version":"1","permissions":["bookmarks"]}"#);
        *host.now.borrow_mut() = 1000;
        let folder = call(&d, "bookmarks.create", vec![json!({"parentId": "1", "title": "Work"})]).unwrap();
        let fid = folder["id"].as_str().unwrap().to_string();
        *host.now.borrow_mut() = 2000;
        let b1 = call(&d, "bookmarks.create", vec![json!({"parentId": fid, "title": "Ladybird Docs", "url": "https://ladybird.org/docs"})])
            .unwrap();
        let b1_id = b1["id"].as_str().unwrap().to_string();

        assert_eq!(call(&d, "bookmarks.get", vec![json!(&b1_id)]).unwrap()[0]["title"], "Ladybird Docs");
        assert_eq!(call(&d, "bookmarks.getChildren", vec![json!(&fid)]).unwrap().as_array().unwrap().len(), 1);
        assert_eq!(call(&d, "bookmarks.getRecent", vec![json!(5)]).unwrap().as_array().unwrap().len(), 1);
        assert_eq!(call(&d, "bookmarks.search", vec![json!("ladybird")]).unwrap().as_array().unwrap().len(), 1);
        assert_eq!(call(&d, "bookmarks.search", vec![json!({"url": "https://ladybird.org/docs"})]).unwrap().as_array().unwrap().len(), 1);

        // Update and move.
        let upd = call(&d, "bookmarks.update", vec![json!(&b1_id), json!({"title": "Ladybird"})]).unwrap();
        assert_eq!(upd["title"], "Ladybird");
        let moved = call(&d, "bookmarks.move", vec![json!(&b1_id), json!({"parentId": "2", "index": 0})]).unwrap();
        assert_eq!(moved["parentId"], "2");

        // Can't remove root or non-empty folder without removeTree.
        assert!(call(&d, "bookmarks.remove", vec![json!("1")]).is_err());
        let b2 = call(&d, "bookmarks.create", vec![json!({"parentId": fid, "title": "Child", "url": "https://example.com/"})]).unwrap();
        assert!(call(&d, "bookmarks.remove", vec![json!(&fid)]).is_err());
        assert!(call(&d, "bookmarks.removeTree", vec![json!(&fid)]).is_ok());
        assert!(call(&d, "bookmarks.get", vec![b2["id"].clone()]).is_err());

        let names: Vec<String> = host.events.borrow().iter().map(|(_, e, _)| e.clone()).collect();
        assert!(names.contains(&"bookmarks.onCreated".into()));
        assert!(names.contains(&"bookmarks.onChanged".into()));
        assert!(names.contains(&"bookmarks.onMoved".into()));
        assert!(names.contains(&"bookmarks.onRemoved".into()));
    }

    #[test]
    fn downloads_search_filtering_and_permissions() {
        let (d, host) = setup(r#"{"manifest_version":3,"name":"D","version":"1","permissions":["downloads","downloads.shelf"]}"#);
        let id1 = call(&d, "downloads.download", vec![json!({"url": "https://a.test/report.pdf", "filename": "report.pdf"})])
            .unwrap()
            .as_i64()
            .unwrap();
        let id2 = call(&d, "downloads.download", vec![json!({"url": "https://b.test/archive.zip", "filename": "archive.zip"})])
            .unwrap()
            .as_i64()
            .unwrap();
        d.host_event("downloadChanged", &json!({"id": id1, "state": "complete", "totalBytes": 5000}));

        // Search by regex, query terms, state, orderBy.
        let pdfs = call(&d, "downloads.search", vec![json!({"filenameRegex": "\\.pdf$"})]).unwrap();
        assert_eq!(pdfs.as_array().unwrap().len(), 1);
        assert_eq!(pdfs[0]["id"], id1);

        let not_zip = call(&d, "downloads.search", vec![json!({"query": ["-zip"]})]).unwrap();
        assert_eq!(not_zip.as_array().unwrap().len(), 1);
        assert_eq!(not_zip[0]["id"], id1);

        // downloads.open requires downloads.open permission.
        assert_eq!(call(&d, "downloads.open", vec![json!(id1)]).unwrap_err().kind, ErrorKind::PermissionDenied);
        // downloads.shelf was granted.
        assert!(call(&d, "downloads.setShelfEnabled", vec![json!(false)]).is_ok());
        // Pause & cancel update state and emit onChanged.
        assert!(call(&d, "downloads.pause", vec![json!(id2)]).is_ok());
        assert!(call(&d, "downloads.cancel", vec![json!(id2)]).is_ok());
        let erased = call(&d, "downloads.erase", vec![json!({"id": id2})]).unwrap();
        assert_eq!(erased, json!([id2]));
        assert!(host.events.borrow().iter().any(|(_, e, _)| e == "downloads.onErased"));
    }

    #[test]
    fn sessions_track_closed_tabs_and_values() {
        let (d, host) = setup(r#"{"manifest_version":3,"name":"S","version":"1","permissions":["sessions","tabs"]}"#);
        d.host_event(
            "browserState",
            &json!({"windows": [{"id": 1, "focused": true, "tabs": [
                {"id": 10, "url": "https://one.test/", "title": "One", "active": true},
                {"id": 11, "url": "https://two.test/", "title": "Two"}
            ]}]}),
        );
        call(&d, "sessions.setTabValue", vec![json!(10), json!("note"), json!({"x": 1})]).unwrap();
        assert_eq!(call(&d, "sessions.getTabValue", vec![json!(10), json!("note")]).unwrap(), json!({"x": 1}));
        call(&d, "sessions.setWindowValue", vec![json!(1), json!("wkey"), json!("val")]).unwrap();
        assert_eq!(call(&d, "sessions.getWindowValue", vec![json!(1), json!("wkey")]).unwrap(), json!("val"));

        // Close tab 11 -> appears in getRecentlyClosed and fires sessions.onChanged.
        d.host_event(
            "browserState",
            &json!({"windows": [{"id": 1, "focused": true, "tabs": [{"id": 10, "url": "https://one.test/", "title": "One", "active": true}]}]}),
        );
        let closed = call(&d, "sessions.getRecentlyClosed", vec![]).unwrap();
        assert_eq!(closed.as_array().unwrap().len(), 1);
        assert_eq!(closed[0]["tab"]["url"], "https://two.test/");
        let sid = closed[0]["tab"]["sessionId"].as_str().unwrap().to_string();
        assert!(host.events.borrow().iter().any(|(_, e, _)| e == "sessions.onChanged"));

        // Restore reopens via host.tabs.create.
        call(&d, "sessions.restore", vec![json!(sid)]).unwrap();
        assert!(host.calls.borrow().iter().any(|(f, a)| f == "host.tabs.create" && a["url"] == "https://two.test/"));
        assert_eq!(call(&d, "sessions.getRecentlyClosed", vec![]).unwrap(), json!([]));
    }

    #[test]
    fn history_top_sites_and_browsing_data() {
        let (d, host) = setup(r#"{"manifest_version":3,"name":"H","version":"1","permissions":["history","topSites","browsingData"]}"#);
        call(&d, "history.addUrl", vec![json!({"url": "https://example.com/", "title": "Example"})]).unwrap();
        call(&d, "history.deleteUrl", vec![json!({"url": "https://example.com/"})]).unwrap();
        assert!(host.events.borrow().iter().any(|(_, e, _)| e == "history.onVisitRemoved"));
        assert_eq!(call(&d, "topSites.get", vec![]).unwrap(), json!([]));
        let settings = call(&d, "browsingData.settings", vec![]).unwrap();
        assert_eq!(settings["dataToRemove"]["cookies"], true);
        call(&d, "browsingData.removeCookies", vec![json!({"since": 0})]).unwrap();
        assert!(host.calls.borrow().iter().any(|(f, a)| f == "host.browsingData.remove" && a["dataToRemove"]["cookies"] == true));
    }
}
