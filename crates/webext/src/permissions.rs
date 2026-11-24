//! API permissions and host permissions: what an extension declared, what it was granted, and `activeTab`.

use crate::manifest::Manifest;
use crate::match_pattern::{MatchDialect, MatchPattern, MatchPatternSet, split_url};
use std::collections::{BTreeSet, HashMap};

/// Permission strings that are not host patterns. Unknown strings in a manifest are warned about by browsers but do not
/// make an extension invalid.
pub const KNOWN_API_PERMISSIONS: &[&str] = &[
    "activeTab",
    "alarms",
    "background",
    "bookmarks",
    "browserSettings",
    "browsingData",
    "captivePortal",
    "clipboardRead",
    "clipboardWrite",
    "contentSettings",
    "contextMenus",
    "contextualIdentities",
    "cookies",
    "debugger",
    "declarativeContent",
    "declarativeNetRequest",
    "declarativeNetRequestFeedback",
    "declarativeNetRequestWithHostAccess",
    "desktopCapture",
    "dns",
    "downloads",
    "downloads.open",
    "downloads.shelf",
    "downloads.ui",
    "find",
    "fontSettings",
    "gcm",
    "geolocation",
    "history",
    "identity",
    "idle",
    "management",
    "menus",
    "menus.overrideContext",
    "nativeMessaging",
    "notifications",
    "offscreen",
    "pageCapture",
    "pkcs11",
    "power",
    "privacy",
    "proxy",
    "readingList",
    "scripting",
    "search",
    "sessions",
    "sidePanel",
    "storage",
    "system.cpu",
    "system.display",
    "system.memory",
    "system.storage",
    "tabCapture",
    "tabGroups",
    "tabHide",
    "tabs",
    "theme",
    "topSites",
    "tts",
    "ttsEngine",
    "unlimitedStorage",
    "userScripts",
    "webNavigation",
    "webRequest",
    "webRequestAuthProvider",
    "webRequestBlocking",
    "webRequestFilterResponse",
    "webRequestFilterResponse.serviceWorkerScript",
];

pub fn is_known_api_permission(p: &str) -> bool {
    KNOWN_API_PERMISSIONS.contains(&p)
}

/// The argument/result shape of `permissions.contains/request/remove/getAll`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionRequest {
    pub permissions: Vec<String>,
    pub origins: Vec<String>,
}

impl PermissionRequest {
    pub fn from_json(v: &serde_json::Value) -> Result<PermissionRequest, String> {
        let obj = v.as_object().ok_or("Value must be an object with 'permissions' and/or 'origins'")?;
        let list = |key: &str| -> Result<Vec<String>, String> {
            match obj.get(key) {
                None => Ok(vec![]),
                Some(serde_json::Value::Array(items)) => {
                    items.iter().map(|i| i.as_str().map(String::from).ok_or(format!("'{key}' must contain only strings"))).collect()
                }
                Some(_) => Err(format!("'{key}' must be an array")),
            }
        };
        let mut req = PermissionRequest { permissions: list("permissions")?, origins: list("origins")? };
        // MV2 style: host patterns may be passed in "permissions".
        let (hosts, api): (Vec<String>, Vec<String>) =
            req.permissions.drain(..).partition(|p| crate::manifest::looks_like_host_permission(p));
        req.permissions = api;
        req.origins.extend(hosts);
        for o in &req.origins {
            MatchPattern::parse(o, MatchDialect::Firefox).map_err(|e| format!("'{o}' is not a valid origin pattern: {e}"))?;
        }
        Ok(req)
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "permissions": self.permissions, "origins": self.origins })
    }

    pub fn is_empty(&self) -> bool {
        self.permissions.is_empty() && self.origins.is_empty()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PermissionState {
    required_api: BTreeSet<String>,
    required_hosts: Vec<String>,
    optional_api: BTreeSet<String>,
    optional_hosts: Vec<String>,
    granted_api: BTreeSet<String>,
    granted_hosts: Vec<String>,
    required_set: MatchPatternSet,
    effective_hosts: MatchPatternSet,
    optional_set: MatchPatternSet,
    /// `activeTab` grants: tab id -> origin patterns, valid until the tab navigates or closes.
    active_tab: HashMap<i64, MatchPatternSet>,
}

impl PermissionState {
    pub fn from_manifest(m: &Manifest) -> PermissionState {
        let mut s = PermissionState {
            required_api: m.permissions.iter().cloned().collect(),
            required_hosts: m.host_permissions.clone(),
            optional_api: m.optional_permissions.iter().cloned().collect(),
            optional_hosts: m.optional_host_permissions.clone(),
            ..Default::default()
        };
        // "tabs"-less extensions still get chrome.tabs; see Registry. Content-script matches imply no API permission.
        s.rebuild();
        s
    }

    fn rebuild(&mut self) {
        let parse = |l: &[String]| MatchPatternSet::from_patterns(l.iter().map(String::as_str), MatchDialect::Firefox).unwrap_or_default();
        self.required_set = parse(&self.required_hosts);
        let mut all = self.required_hosts.clone();
        all.extend(self.granted_hosts.iter().cloned());
        self.effective_hosts = parse(&all);
        self.optional_set = parse(&self.optional_hosts);
    }

    pub fn has_api(&self, p: &str) -> bool {
        self.required_api.contains(p) || self.granted_api.contains(p)
    }

    /// Whether the extension may access the given URL via host permissions alone.
    pub fn has_host(&self, url: &str) -> bool {
        self.effective_hosts.matches(url)
    }

    pub fn has_host_for_tab(&self, tab_id: i64, url: &str) -> bool {
        self.has_host(url) || (self.has_api("activeTab") && self.active_tab.get(&tab_id).is_some_and(|s| s.matches(url)))
    }

    /// Records a user gesture on an extension action for `activeTab`: temporary access to the tab's current origin.
    pub fn grant_active_tab(&mut self, tab_id: i64, url: &str) {
        if !self.has_api("activeTab") {
            return;
        }
        if let Some(u) = split_url(url)
            && !u.host.is_empty()
        {
            let pattern = format!("{}://{}/*", u.scheme, u.host);
            if let Ok(set) = MatchPatternSet::from_patterns([pattern.as_str()], MatchDialect::Firefox) {
                self.active_tab.insert(tab_id, set);
            }
        }
    }

    pub fn revoke_active_tab(&mut self, tab_id: i64) {
        self.active_tab.remove(&tab_id);
    }

    pub fn grants_all_hosts(&self) -> bool {
        self.effective_hosts.grants_all_hosts()
    }

    fn origin_covered_by(&self, set: &MatchPatternSet, list: &[String], origin: &str) -> bool {
        if list.iter().any(|o| o == origin) || set.grants_all_hosts() {
            return true;
        }
        // Approximation of pattern containment: test a representative URL for the requested pattern.
        let sample = origin
            .replacen("*://", "https://", 1)
            .replacen("://*.", "://sub.", 1)
            .replacen("://*/", "://host.example/", 1)
            .replace('*', "x");
        set.matches(&sample)
    }

    /// `permissions.contains`: is everything in the request currently held?
    pub fn contains(&self, req: &PermissionRequest) -> bool {
        req.permissions.iter().all(|p| self.has_api(p))
            && req.origins.iter().all(|o| {
                let mut all = self.required_hosts.clone();
                all.extend(self.granted_hosts.iter().cloned());
                MatchPatternSet::from_patterns(all.iter().map(String::as_str), MatchDialect::Firefox)
                    .is_ok_and(|set| self.origin_covered_by(&set, &all, o))
            })
    }

    /// Whether a request may even be put to the user: everything must be declared optional (or already held).
    pub fn check_requestable(&self, req: &PermissionRequest) -> Result<(), String> {
        for p in &req.permissions {
            if !self.has_api(p) && !self.optional_api.contains(p) {
                return Err(format!("Optional permission must be listed in the manifest to be requested: '{p}'"));
            }
        }
        let held: Vec<String> = self.required_hosts.iter().chain(&self.granted_hosts).cloned().collect();
        let held_set = MatchPatternSet::from_patterns(held.iter().map(String::as_str), MatchDialect::Firefox).unwrap_or_default();
        for o in &req.origins {
            if !self.origin_covered_by(&held_set, &held, o) && !self.origin_covered_by(&self.optional_set, &self.optional_hosts, o) {
                return Err(format!("Origin must be listed in optional_host_permissions to be requested: '{o}'"));
            }
        }
        Ok(())
    }

    /// Records a grant. Returns the subset that was newly added.
    pub fn grant(&mut self, req: &PermissionRequest) -> PermissionRequest {
        let mut added = PermissionRequest::default();
        for p in &req.permissions {
            if !self.has_api(p) {
                self.granted_api.insert(p.clone());
                added.permissions.push(p.clone());
            }
        }
        for o in &req.origins {
            if !self.granted_hosts.contains(o) && !self.required_hosts.contains(o) {
                self.granted_hosts.push(o.clone());
                added.origins.push(o.clone());
            }
        }
        self.rebuild();
        added
    }

    /// `permissions.remove`: only previously-granted optional permissions can be removed. Returns what was removed, or an
    /// error if the request names a required permission.
    pub fn revoke(&mut self, req: &PermissionRequest) -> Result<PermissionRequest, String> {
        for p in &req.permissions {
            if self.required_api.contains(p) {
                return Err(format!("You cannot remove required permissions: '{p}'"));
            }
        }
        for o in &req.origins {
            if self.required_hosts.contains(o) {
                return Err(format!("You cannot remove required permissions: '{o}'"));
            }
        }
        let mut removed = PermissionRequest::default();
        for p in &req.permissions {
            if self.granted_api.remove(p) {
                removed.permissions.push(p.clone());
            }
        }
        for o in &req.origins {
            if let Some(pos) = self.granted_hosts.iter().position(|g| g == o) {
                self.granted_hosts.remove(pos);
                removed.origins.push(o.clone());
            }
        }
        self.rebuild();
        Ok(removed)
    }

    /// `permissions.getAll`.
    pub fn all(&self) -> PermissionRequest {
        PermissionRequest {
            permissions: self.required_api.iter().chain(&self.granted_api).cloned().collect(),
            origins: self.required_hosts.iter().chain(&self.granted_hosts).cloned().collect(),
        }
    }

    /// State to persist across sessions: the optional permissions the user granted.
    pub fn granted_to_json(&self) -> serde_json::Value {
        serde_json::json!({ "permissions": self.granted_api, "origins": self.granted_hosts })
    }

    pub fn restore_granted(&mut self, v: &serde_json::Value) {
        if let Ok(req) = PermissionRequest::from_json(v) {
            // Only restore what the manifest still allows as optional.
            let allowed: Vec<String> = req.permissions.into_iter().filter(|p| self.optional_api.contains(p)).collect();
            let origins: Vec<String> = req
                .origins
                .into_iter()
                .filter(|o| self.check_requestable(&PermissionRequest { permissions: vec![], origins: vec![o.clone()] }).is_ok())
                .collect();
            self.grant(&PermissionRequest { permissions: allowed, origins });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn state(json: &str) -> PermissionState {
        PermissionState::from_manifest(&parse_manifest(json).manifest.expect("valid manifest"))
    }

    const M: &str = r#"{"manifest_version":3,"name":"x","version":"1",
        "permissions":["storage","activeTab"],"host_permissions":["https://*.a.com/*"],
        "optional_permissions":["history","bookmarks"],"optional_host_permissions":["https://*.b.com/*","*://c.com/*"]}"#;

    fn req(perms: &[&str], origins: &[&str]) -> PermissionRequest {
        PermissionRequest {
            permissions: perms.iter().map(|s| s.to_string()).collect(),
            origins: origins.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn required_permissions() {
        let s = state(M);
        assert!(s.has_api("storage"));
        assert!(!s.has_api("history"));
        assert!(s.has_host("https://www.a.com/x"));
        assert!(!s.has_host("https://b.com/x"));
        assert!(s.contains(&req(&["storage"], &["https://a.com/*"])));
        assert!(!s.contains(&req(&["history"], &[])));
    }

    #[test]
    fn request_grant_revoke_cycle() {
        let mut s = state(M);
        assert!(s.check_requestable(&req(&["history"], &["https://x.b.com/*"])).is_ok());
        assert!(s.check_requestable(&req(&["cookies"], &[])).is_err());
        assert!(s.check_requestable(&req(&[], &["https://evil.com/*"])).is_err());
        let added = s.grant(&req(&["history"], &["https://*.b.com/*"]));
        assert_eq!(added, req(&["history"], &["https://*.b.com/*"]));
        assert!(s.has_api("history"));
        assert!(s.has_host("https://q.b.com/p"));
        assert!(s.contains(&req(&["history"], &["https://q.b.com/*"])));
        // Granting again adds nothing new.
        assert!(s.grant(&req(&["history"], &[])).is_empty());
        let removed = s.revoke(&req(&["history"], &["https://*.b.com/*"])).unwrap();
        assert_eq!(removed, req(&["history"], &["https://*.b.com/*"]));
        assert!(!s.has_api("history") && !s.has_host("https://q.b.com/p"));
    }

    #[test]
    fn required_permissions_cannot_be_removed() {
        let mut s = state(M);
        assert!(s.revoke(&req(&["storage"], &[])).is_err());
        assert!(s.revoke(&req(&[], &["https://*.a.com/*"])).is_err());
        assert!(s.has_api("storage"));
    }

    #[test]
    fn active_tab_is_temporary_and_scoped() {
        let mut s = state(M);
        assert!(!s.has_host_for_tab(7, "https://z.com/p"));
        s.grant_active_tab(7, "https://z.com/page");
        assert!(s.has_host_for_tab(7, "https://z.com/other"));
        assert!(!s.has_host_for_tab(7, "https://y.com/"));
        assert!(!s.has_host_for_tab(8, "https://z.com/other"));
        s.revoke_active_tab(7);
        assert!(!s.has_host_for_tab(7, "https://z.com/other"));
        // Without the permission, nothing is granted.
        let mut plain = state(r#"{"manifest_version":3,"name":"x","version":"1"}"#);
        plain.grant_active_tab(1, "https://z.com/");
        assert!(!plain.has_host_for_tab(1, "https://z.com/"));
    }

    #[test]
    fn request_json_forms() {
        let r = PermissionRequest::from_json(&serde_json::json!({"permissions":["tabs","https://a.com/*"],"origins":["https://b.com/*"]}))
            .unwrap();
        assert_eq!(r.permissions, ["tabs"]);
        assert_eq!(r.origins, ["https://b.com/*", "https://a.com/*"]);
        assert!(PermissionRequest::from_json(&serde_json::json!("x")).is_err());
        assert!(PermissionRequest::from_json(&serde_json::json!({"origins":["junk"]})).is_err());
        assert!(PermissionRequest::from_json(&serde_json::json!({"permissions":[1]})).is_err());
    }

    #[test]
    fn persisted_grants_are_filtered_by_current_manifest() {
        let mut s = state(M);
        s.restore_granted(&serde_json::json!({"permissions":["history","cookies"],"origins":["https://*.b.com/*","https://evil.com/*"]}));
        assert!(s.has_api("history") && !s.has_api("cookies"));
        assert!(s.has_host("https://q.b.com/") && !s.has_host("https://evil.com/"));
    }

    #[test]
    fn known_permissions() {
        assert!(is_known_api_permission("storage"));
        assert!(!is_known_api_permission("superpowers"));
    }
}
