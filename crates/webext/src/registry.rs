//! The API registry: every extension API function/event/property we know about, whether Chrome and/or Firefox have it,
//! and how much of it we support. The registry is data (`Data/api-support.json`); dispatch, permission checks and the
//! coverage report all read it, so none of them can drift from each other.

use serde_json::Value;
use std::collections::HashMap;

pub const BUILTIN_REGISTRY_JSON: &str = include_str!("../../../Data/api-support.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Status {
    /// The core handles it completely and it is tested.
    Implemented,
    /// Works with documented limits (see `notes`).
    Partial,
    /// The core validates the call (existence, permission, manifest version) and routes it to the host browser, which
    /// must implement it.
    Delegated,
    Planned,
    Unsupported,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Implemented => "implemented",
            Status::Partial => "partial",
            Status::Delegated => "delegated",
            Status::Planned => "planned",
            Status::Unsupported => "unsupported",
        }
    }

    fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "implemented" => Status::Implemented,
            "partial" => Status::Partial,
            "delegated" => Status::Delegated,
            "planned" => Status::Planned,
            "unsupported" => Status::Unsupported,
            _ => return None,
        })
    }

    /// Whether the core itself provides the behaviour (as opposed to the host or nobody).
    pub fn handled_by_core(self) -> bool {
        matches!(self, Status::Implemented | Status::Partial)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Function,
    Event,
    Property,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Function => "function",
            Kind::Event => "event",
            Kind::Property => "property",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Engine {
    Chrome,
    Firefox,
}

#[derive(Debug, Clone)]
pub struct Member {
    /// `namespace.member`, e.g. `storage.local.get`.
    pub full_name: String,
    pub namespace: String,
    pub name: String,
    pub kind: Kind,
    pub status: Status,
    pub in_chrome: bool,
    pub in_firefox: bool,
    /// Manifest versions this member exists in; `None` means all.
    pub manifest_versions: Option<Vec<u32>>,
    /// API permission required to use it.
    pub permission: Option<String>,
    pub note: Option<String>,
}

impl Member {
    pub fn available_in(&self, engine: Engine) -> bool {
        match engine {
            Engine::Chrome => self.in_chrome,
            Engine::Firefox => self.in_firefox,
        }
    }

    pub fn available_in_manifest_version(&self, mv: u32) -> bool {
        self.manifest_versions.as_ref().is_none_or(|v| v.contains(&mv))
    }
}

#[derive(Debug, Clone, Default)]
pub struct StatusCounts {
    pub implemented: usize,
    pub partial: usize,
    pub delegated: usize,
    pub planned: usize,
    pub unsupported: usize,
}

impl StatusCounts {
    pub fn total(&self) -> usize {
        self.implemented + self.partial + self.delegated + self.planned + self.unsupported
    }

    fn add(&mut self, s: Status) {
        match s {
            Status::Implemented => self.implemented += 1,
            Status::Partial => self.partial += 1,
            Status::Delegated => self.delegated += 1,
            Status::Planned => self.planned += 1,
            Status::Unsupported => self.unsupported += 1,
        }
    }

    /// Share of members the core provides on its own, in percent.
    pub fn core_percent(&self) -> f64 {
        percent(self.implemented + self.partial, self.total())
    }

    /// Share of members that work once the host implements the delegated calls, in percent.
    pub fn with_host_percent(&self) -> f64 {
        percent(self.implemented + self.partial + self.delegated, self.total())
    }
}

fn percent(n: usize, d: usize) -> f64 {
    if d == 0 { 0.0 } else { n as f64 * 100.0 / d as f64 }
}

#[derive(Debug)]
pub struct Registry {
    members: Vec<Member>,
    index: HashMap<String, usize>,
    aliases: HashMap<String, String>,
    namespaces: Vec<String>,
    sync_functions: Vec<String>,
}

#[derive(Debug)]
pub struct RegistryError(pub String);

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid API registry: {}", self.0)
    }
}

impl std::error::Error for RegistryError {}

impl Registry {
    pub fn builtin() -> Registry {
        Registry::from_json(BUILTIN_REGISTRY_JSON).expect("the built-in registry must be valid (enforced by tests)")
    }

    pub fn from_json(text: &str) -> Result<Registry, RegistryError> {
        let err = |m: String| RegistryError(m);
        let root: Value = serde_json::from_str(text).map_err(|e| err(e.to_string()))?;
        let namespaces = root.get("namespaces").and_then(Value::as_object).ok_or_else(|| err("missing 'namespaces'".into()))?;

        let mut aliases = HashMap::new();
        if let Some(a) = root.get("aliases").and_then(Value::as_object) {
            for (k, v) in a {
                let target = v.as_str().ok_or_else(|| err(format!("alias '{k}' must be a string")))?;
                if !namespaces.contains_key(target) {
                    return Err(err(format!("alias '{k}' points to unknown namespace '{target}'")));
                }
                aliases.insert(k.clone(), target.to_string());
            }
        }

        let sync_functions: Vec<String> = root
            .get("sync_functions")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect())
            .unwrap_or_default();
        let mut reg = Registry { members: vec![], index: HashMap::new(), aliases, namespaces: vec![], sync_functions };
        for (ns_name, ns) in namespaces {
            reg.namespaces.push(ns_name.clone());
            let ns_permission = ns.get("permission").and_then(Value::as_str).map(String::from);
            let engines: Vec<&str> = ns
                .get("engines")
                .and_then(Value::as_array)
                .ok_or_else(|| err(format!("{ns_name}: missing 'engines'")))?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for e in &engines {
                if !matches!(*e, "chrome" | "firefox") {
                    return Err(err(format!("{ns_name}: unknown engine '{e}'")));
                }
            }
            let only = ns.get("only").and_then(Value::as_object);
            let mv = ns.get("mv").and_then(Value::as_object);
            let perm_overrides = ns.get("perm_overrides").and_then(Value::as_object);
            let notes = ns.get("notes").and_then(Value::as_object);

            for (section, kind) in [("functions", Kind::Function), ("events", Kind::Event), ("properties", Kind::Property)] {
                let Some(groups) = ns.get(section).and_then(Value::as_object) else { continue };
                for (status_name, list) in groups {
                    let status =
                        Status::parse(status_name).ok_or_else(|| err(format!("{ns_name}.{section}: unknown status '{status_name}'")))?;
                    let list = list.as_array().ok_or_else(|| err(format!("{ns_name}.{section}.{status_name}: expected array")))?;
                    for item in list {
                        let name = item.as_str().ok_or_else(|| err(format!("{ns_name}: member names must be strings")))?;
                        let full_name = format!("{ns_name}.{name}");
                        if reg.index.contains_key(&full_name) {
                            return Err(err(format!("duplicate member '{full_name}'")));
                        }
                        let mut in_chrome = engines.contains(&"chrome");
                        let mut in_firefox = engines.contains(&"firefox");
                        if let Some(restrict) = only.and_then(|o| o.get(name)).and_then(Value::as_str) {
                            match restrict {
                                "chrome" => in_firefox = false,
                                "firefox" => in_chrome = false,
                                other => return Err(err(format!("{full_name}: bad 'only' value '{other}'"))),
                            }
                            if (restrict == "chrome" && !engines.contains(&"chrome"))
                                || (restrict == "firefox" && !engines.contains(&"firefox"))
                            {
                                return Err(err(format!("{full_name}: restricted to an engine its namespace lacks")));
                            }
                        }
                        let manifest_versions = mv
                            .and_then(|m| m.get(name))
                            .and_then(Value::as_array)
                            .map(|a| a.iter().filter_map(Value::as_u64).map(|n| n as u32).collect());
                        let permission = perm_overrides
                            .and_then(|p| p.get(name))
                            .and_then(Value::as_str)
                            .map(String::from)
                            .or_else(|| ns_permission.clone());
                        let note = notes.and_then(|n| n.get(name).or_else(|| n.get("*"))).and_then(Value::as_str).map(String::from);
                        let idx = reg.members.len();
                        reg.members.push(Member {
                            full_name: full_name.clone(),
                            namespace: ns_name.clone(),
                            name: name.to_string(),
                            kind,
                            status,
                            in_chrome,
                            in_firefox,
                            manifest_versions,
                            permission,
                            note,
                        });
                        reg.index.insert(full_name, idx);
                    }
                }
            }
        }
        for f in &reg.sync_functions {
            match reg.index.get(f).map(|&i| &reg.members[i]) {
                Some(m) if m.kind == Kind::Function && m.status.handled_by_core() => {}
                _ => return Err(err(format!("sync function '{f}' must be an implemented or partial function"))),
            }
        }
        Ok(reg)
    }

    /// Functions that extensions call synchronously (`runtime.getURL`, `i18n.getMessage`, ...). The browser must give the
    /// JavaScript shim a synchronous way to call these.
    pub fn sync_functions(&self) -> &[String] {
        &self.sync_functions
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }

    pub fn namespaces(&self) -> &[String] {
        &self.namespaces
    }

    /// Resolves aliases such as `browserAction.setTitle` to `action.setTitle`.
    pub fn canonical_name(&self, full_name: &str) -> String {
        if let Some((ns, rest)) = full_name.split_once('.')
            && let Some(target) = self.aliases.get(ns)
        {
            return format!("{target}.{rest}");
        }
        full_name.to_string()
    }

    pub fn lookup(&self, full_name: &str) -> Option<&Member> {
        self.index.get(&self.canonical_name(full_name)).map(|&i| &self.members[i])
    }

    pub fn aliases(&self) -> &HashMap<String, String> {
        &self.aliases
    }

    pub fn members_in<'a>(&'a self, engine: Engine) -> impl Iterator<Item = &'a Member> + 'a {
        self.members.iter().filter(move |m| m.available_in(engine))
    }

    pub fn counts(&self, engine: Engine) -> StatusCounts {
        let mut c = StatusCounts::default();
        for m in self.members_in(engine) {
            c.add(m.status);
        }
        c
    }

    pub fn counts_by_namespace(&self, engine: Engine) -> Vec<(String, StatusCounts)> {
        let mut out: Vec<(String, StatusCounts)> = vec![];
        for m in self.members_in(engine) {
            match out.iter_mut().find(|(n, _)| *n == m.namespace) {
                Some((_, c)) => c.add(m.status),
                None => {
                    let mut c = StatusCounts::default();
                    c.add(m.status);
                    out.push((m.namespace.clone(), c));
                }
            }
        }
        out
    }

    /// The API surface visible to an extension: namespaces and members whose permission is granted, as JSON for the
    /// JavaScript shim: `{ "storage": { "functions": ["local.get", ...], "events": [...], "properties": [...] }, ... }`.
    /// Planned/unsupported members are included so that calling them fails with a clear error instead of `undefined`.
    pub fn surface_json(&self, manifest_version: u32, has_permission: &dyn Fn(&str) -> bool) -> Value {
        let mut out = serde_json::Map::new();
        for m in &self.members {
            if !m.available_in_manifest_version(manifest_version) {
                continue;
            }
            if let Some(p) = &m.permission
                && !has_permission(p)
            {
                continue;
            }
            let entry =
                out.entry(m.namespace.clone()).or_insert_with(|| serde_json::json!({ "functions": [], "events": [], "properties": [] }));
            let list = match m.kind {
                Kind::Function => "functions",
                Kind::Event => "events",
                Kind::Property => "properties",
            };
            entry[list].as_array_mut().expect("initialised as array").push(Value::String(m.name.clone()));
        }
        Value::Object(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_parses_and_is_consistent() {
        let r = Registry::builtin();
        assert!(r.members().len() > 400, "registry unexpectedly small: {}", r.members().len());
        for m in r.members() {
            assert!(m.in_chrome || m.in_firefox, "{} is in no engine", m.full_name);
        }
    }

    #[test]
    fn lookup_and_aliases() {
        let r = Registry::builtin();
        let m = r.lookup("storage.local.get").unwrap();
        assert_eq!(m.status, Status::Implemented);
        assert_eq!(m.permission.as_deref(), Some("storage"));
        assert_eq!(r.lookup("browserAction.setTitle").unwrap().full_name, "action.setTitle");
        assert!(r.lookup("nope.nothing").is_none());
        // Engine- and version-specific availability.
        let sp = r.lookup("sidePanel.open").unwrap();
        assert!(sp.in_chrome && !sp.in_firefox);
        let sa = r.lookup("sidebarAction.open").unwrap();
        assert!(!sa.in_chrome && sa.in_firefox);
        let ex = r.lookup("tabs.executeScript").unwrap();
        assert!(ex.available_in_manifest_version(2) && !ex.available_in_manifest_version(3));
        assert_eq!(ex.permission.as_deref(), Some("tabs"));
    }

    #[test]
    fn rejects_bad_registries() {
        assert!(Registry::from_json("{}").is_err());
        assert!(Registry::from_json(r#"{"namespaces":{"a":{"engines":["opera"],"functions":{}}}}"#).is_err());
        assert!(Registry::from_json(r#"{"namespaces":{"a":{"engines":["chrome"],"functions":{"nope":["x"]}}}}"#).is_err());
        assert!(Registry::from_json(r#"{"namespaces":{"a":{"engines":["chrome"],"functions":{"planned":["x","x"]}}}}"#).is_err());
        assert!(
            Registry::from_json(r#"{"namespaces":{"a":{"engines":["chrome"],"functions":{"planned":["x"]},"only":{"x":"firefox"}}}}"#)
                .is_err()
        );
        assert!(
            Registry::from_json(r#"{"sync_functions":["a.x"],"namespaces":{"a":{"engines":["chrome"],"functions":{"planned":["x"]}}}}"#)
                .is_err()
        );
        assert!(Registry::from_json(r#"{"aliases":{"b":"zzz"},"namespaces":{"a":{"engines":["chrome"]}}}"#).is_err());
    }

    #[test]
    fn counts_add_up() {
        let r = Registry::builtin();
        for engine in [Engine::Chrome, Engine::Firefox] {
            let c = r.counts(engine);
            assert_eq!(c.total(), r.members_in(engine).count());
            let by_ns: usize = r.counts_by_namespace(engine).iter().map(|(_, c)| c.total()).sum();
            assert_eq!(by_ns, c.total());
            assert!(c.core_percent() <= c.with_host_percent());
        }
    }

    #[test]
    fn surface_respects_permissions_and_manifest_version() {
        let r = Registry::builtin();
        let none = r.surface_json(3, &|_| false);
        assert!(none.get("storage").is_none());
        assert!(none.get("runtime").is_some());
        assert!(none["tabs"]["functions"].as_array().unwrap().iter().all(|f| f != "executeScript"));
        let with = r.surface_json(3, &|p| p == "storage");
        assert!(with["storage"]["functions"].as_array().unwrap().iter().any(|f| f == "local.get"));
        let mv2 = r.surface_json(2, &|_| true);
        assert!(mv2["tabs"]["functions"].as_array().unwrap().iter().any(|f| f == "executeScript"));
    }
}
