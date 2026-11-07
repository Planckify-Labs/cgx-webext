//! Installing from the extension stores: the Chrome Web Store (`https://chromewebstore.google.com/`) and
//! addons.mozilla.org (`https://addons.mozilla.org/`).
//!
//! The stores' own "Add to Chrome" / "Add to Firefox" buttons talk to private browser APIs: `chrome.webstorePrivate` and
//! `chrome.management` on the Chrome Web Store, `navigator.mozAddonManager` on addons.mozilla.org. This module provides
//! the browser-independent half of those APIs:
//!
//! - which documents are store pages ([`store_for_url`]),
//! - the program that defines the private APIs in a store page ([`store_program`]), which the browser runs there the way
//!   it runs content scripts, through a bridge whose key is [`STORE_BRIDGE_KEY`],
//! - what to tell the user before installing ([`permission_warnings`]),
//! - checking a downloaded package against what the store asked for ([`check_store_package`]),
//! - the `chrome.management.ExtensionInfo` and AMO `Addon` objects describing installed extensions.
//!
//! The browser does the rest: it answers the bridge calls, downloads packages, asks the user, installs, and keeps the
//! packages. See `docs/STORES.md` for the protocol between the page program and the browser.

use crate::dispatch::Dispatcher;
use crate::i18n::I18n;
use crate::loader::{Files, load_extension};
use crate::manifest::{Manifest, parse_manifest};
use crate::package::Package;
use serde_json::{Value, json};

/// The bridge key of the store program. Extension ids never contain `_`, so it cannot collide with one.
pub const STORE_BRIDGE_KEY: &str = "__store__";

/// The page program for the Chrome Web Store, a JavaScript function expression called with the bridge.
pub const CHROME_STORE_JS: &str = include_str!("../../../Shim/store-chrome.js");
/// The page program for addons.mozilla.org.
pub const FIREFOX_STORE_JS: &str = include_str!("../../../Shim/store-firefox.js");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Store {
    ChromeWebStore,
    Mozilla,
}

impl Store {
    pub fn as_str(self) -> &'static str {
        match self {
            Store::ChromeWebStore => "chrome",
            Store::Mozilla => "firefox",
        }
    }

    /// The origin of the store's pages.
    pub fn origin(self) -> &'static str {
        match self {
            Store::ChromeWebStore => "https://chromewebstore.google.com",
            Store::Mozilla => "https://addons.mozilla.org",
        }
    }
}

/// The store a document belongs to, judged by its URL. Only the stores' exact HTTPS origins count: a page elsewhere must
/// not reach the install APIs.
pub fn store_for_url(url: &str) -> Option<Store> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Credentials or a port would make it a different origin (or a confusing URL); neither is how the stores are reached.
    if authority.contains(['@', ':']) {
        return None;
    }
    match authority.to_ascii_lowercase().as_str() {
        "chromewebstore.google.com" => Some(Store::ChromeWebStore),
        "addons.mozilla.org" => Some(Store::Mozilla),
        _ => None,
    }
}

/// The program to run in a store page: a function expression, called with the store bridge before the page's own scripts.
pub fn store_program(store: Store) -> &'static str {
    match store {
        Store::ChromeWebStore => CHROME_STORE_JS,
        Store::Mozilla => FIREFOX_STORE_JS,
    }
}

/// Where the Chrome Web Store serves the current package of an extension (it redirects to the `.crx` file).
/// `chrome_version` is the version the browser reports, e.g. `153.0.0.0`.
pub fn chrome_web_store_download_url(id: &str, chrome_version: &str) -> Option<String> {
    if !crate::encoding::is_valid_chrome_extension_id(id) {
        return None;
    }
    let version: String = chrome_version.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
    let version = if version.is_empty() { "153.0.0.0".to_string() } else { version };
    Some(format!(
        "https://clients2.google.com/service/update2/crx?response=redirect&prodversion={version}&acceptformat=crx2,crx3&x=id%3D{id}%26installsource%3Dondemand%26uc"
    ))
}

const ALL_HOSTS: [&str; 5] = ["<all_urls>", "*://*/*", "http://*/*", "https://*/*", "*://*/"];

fn is_all_hosts(pattern: &str) -> bool {
    ALL_HOSTS.contains(&pattern) || pattern.starts_with("*://*/") || pattern == "file:///*"
}

/// The host part of a match pattern, for a warning ("Read and change your data on example.com").
fn pattern_host(pattern: &str) -> Option<String> {
    let (_, rest) = pattern.split_once("://")?;
    let host = rest.split('/').next().unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(host.strip_prefix("*.").map(|h| format!("all {h} sites")).unwrap_or_else(|| host.to_string()))
}

/// Everything the extension may access: API permissions, then host patterns (host permissions and content-script matches).
fn requested_access(m: &Manifest) -> (Vec<String>, Vec<String>) {
    let mut hosts: Vec<String> = m.host_permissions.clone();
    for script in &m.content_scripts {
        hosts.extend(script.matches.iter().cloned());
    }
    hosts.sort();
    hosts.dedup();
    let mut apis = m.permissions.clone();
    apis.sort();
    apis.dedup();
    (apis, hosts)
}

/// What to tell the user an extension will be able to do, in the words Chrome uses. Permissions that carry no warning in
/// Chrome (`storage`, `alarms`, `contextMenus`, ...) are left out.
pub fn permission_warnings(m: &Manifest) -> Vec<String> {
    let (apis, hosts) = requested_access(m);
    let mut out: Vec<String> = vec![];
    let mut push = |s: &str| {
        if !out.iter().any(|w| w == s) {
            out.push(s.to_string());
        }
    };
    let has = |p: &str| apis.iter().any(|a| a == p);

    let all_hosts = hosts.iter().any(|h| is_all_hosts(h)) || has("debugger") || has("proxy") || has("pageCapture");
    if all_hosts {
        push("Read and change all your data on all websites");
    } else {
        let mut names: Vec<String> = hosts.iter().filter_map(|h| pattern_host(h)).collect();
        names.dedup();
        match names.len() {
            0 => {}
            1 => push(&format!("Read and change your data on {}", names[0])),
            2 => push(&format!("Read and change your data on {} and {}", names[0], names[1])),
            3 => push(&format!("Read and change your data on {}, {}, and {}", names[0], names[1], names[2])),
            n => push(&format!("Read and change your data on {}, {}, and {} other sites", names[0], names[1], n - 2)),
        }
    }

    let table: &[(&str, &str)] = &[
        ("debugger", "Access the page debugger backend"),
        ("history", "Read and change your browsing history on all your signed-in devices"),
        ("tabs", "Read your browsing history"),
        ("webNavigation", "Read your browsing history"),
        ("declarativeNetRequestFeedback", "Read your browsing history"),
        ("sessions", "Read your browsing history"),
        ("topSites", "Read a list of your most frequently visited websites"),
        ("bookmarks", "Read and change your bookmarks"),
        ("downloads", "Manage your downloads"),
        ("downloads.open", "Open downloaded files"),
        ("clipboardRead", "Read data you copy and paste"),
        ("clipboardWrite", "Modify data you copy and paste"),
        ("geolocation", "Detect your physical location"),
        ("management", "Manage your apps, extensions, and themes"),
        ("nativeMessaging", "Communicate with cooperating native applications"),
        ("notifications", "Display notifications"),
        ("privacy", "Change your privacy-related settings"),
        (
            "contentSettings",
            "Change your settings that control websites' access to features such as cookies, JavaScript, plugins, geolocation, microphone, camera etc.",
        ),
        ("declarativeNetRequest", "Block content on any page"),
        ("desktopCapture", "Capture content of your screen"),
        ("tabCapture", "Read and change all your data on all websites"),
        ("tts", "Read all text spoken using synthesized speech"),
        ("ttsEngine", "Read all text spoken using synthesized speech"),
        ("system.storage", "Identify and eject storage devices"),
        ("readingList", "Read and change entries in the reading list"),
        ("favicon", "Read the icons of the websites you visit"),
    ];
    for (permission, warning) in table {
        if has(permission) {
            push(warning);
        }
    }
    out
}

/// What the store install prompt shows, from the manifest the store sent (Chrome Web Store's `beginInstallWithManifest3`)
/// before anything is downloaded: `{ "ok", "name", "version", "warnings" }`. `localized_name` is the store's name for
/// the item, used when the manifest's name is a `__MSG_*__` placeholder.
pub fn preview_manifest(manifest_json: &str, localized_name: &str) -> Value {
    let parsed = parse_manifest(manifest_json);
    let Some(manifest) = parsed.manifest else {
        let errors: Vec<String> = parsed.errors().map(|d| format!("{}: {}", d.field, d.message)).collect();
        return json!({ "ok": false, "error": errors.join("; ") });
    };
    let name = if !localized_name.trim().is_empty() {
        localized_name.trim().to_string()
    } else if manifest.name.starts_with("__MSG_") {
        String::new()
    } else {
        manifest.name.clone()
    };
    json!({ "ok": true, "name": name, "version": manifest.version, "warnings": permission_warnings(&manifest) })
}

/// What a downloaded store package turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub struct PackageSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    pub warnings: Vec<String>,
}

impl PackageSummary {
    pub fn to_json(&self) -> Value {
        json!({ "ok": true, "id": self.id, "name": self.name, "version": self.version, "warnings": self.warnings })
    }
}

/// What a store asked for, to hold a downloaded package to.
#[derive(Debug, Default, Clone)]
pub struct StoreExpectation<'a> {
    /// The extension id the store installs (Chrome Web Store), checked against the id the package carries.
    pub id: Option<&'a str>,
    /// `sha256:<hex>` of the file (addons.mozilla.org's `createInstall({ hash })`).
    pub hash: Option<&'a str>,
    /// The manifest the user approved (Chrome Web Store): the package must not ask for more than it.
    pub approved_manifest: Option<&'a str>,
}

/// Checks a downloaded package before it is installed: it must parse and load, have the hash and id the store named, and
/// request no permission the approved manifest did not. Returns what to show the user, or why the package is refused.
pub fn check_store_package(bytes: &[u8], expect: &StoreExpectation<'_>) -> Result<PackageSummary, String> {
    if let Some(hash) = expect.hash.filter(|h| !h.is_empty()) {
        let Some(hex) = hash.strip_prefix("sha256:") else { return Err(format!("unsupported hash '{hash}'")) };
        let actual: String = crate::encoding::sha256(bytes).iter().map(|b| format!("{b:02x}")).collect();
        if !actual.eq_ignore_ascii_case(hex) {
            return Err("the downloaded file does not have the hash the store gave".into());
        }
    }

    let pkg = Package::open(bytes.to_vec()).map_err(|e| e.to_string())?;
    let files = Files::from_package(&pkg).map_err(|e| e.to_string())?;
    let crx_id = pkg.crx_header().and_then(|h| h.extension_id());
    let loaded = load_extension(&files, crx_id.as_deref()).map_err(|diags| {
        diags
            .iter()
            .filter(|d| d.severity == crate::manifest::Severity::Error)
            .map(|d| format!("{}: {}", d.field, d.message))
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    let id = loaded.id.clone().or_else(|| pkg.mozilla_signed_id()).ok_or("the package carries no extension id")?;

    if let Some(expected) = expect.id
        && expected != id
    {
        return Err(format!("the store offered {expected} but the package is {id}"));
    }

    if let Some(approved) = expect.approved_manifest {
        let approved = parse_manifest(approved).manifest.ok_or("the approved manifest does not parse")?;
        let (approved_apis, approved_hosts) = requested_access(&approved);
        let (apis, hosts) = requested_access(&loaded.manifest);
        let extra: Vec<&String> =
            apis.iter().filter(|p| !approved_apis.contains(p)).chain(hosts.iter().filter(|h| !approved_hosts.contains(h))).collect();
        if !extra.is_empty() {
            return Err(format!(
                "the package asks for permissions that were not approved: {}",
                extra.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }

    Ok(PackageSummary {
        id,
        name: loaded.display_name,
        version: loaded.manifest.version.clone(),
        warnings: permission_warnings(&loaded.manifest),
    })
}

fn icon_list(set: &crate::manifest::IconSet, base_url: &str) -> Vec<Value> {
    set.0.iter().map(|(size, path)| json!({ "size": size, "url": format!("{base_url}{}", path.trim_start_matches(['/', '.'])) })).collect()
}

fn localized(i18n: &I18n, text: &str) -> String {
    i18n.localize(text)
}

impl Dispatcher {
    /// `chrome.management.ExtensionInfo` for an installed extension.
    pub fn management_info(&self, id: &str) -> Option<Value> {
        self.with_extension(id, |e| {
            let m = &e.manifest;
            let granted = e.permissions.all();
            json!({
                "id": e.id,
                "name": localized(&e.i18n, &m.name),
                "shortName": localized(&e.i18n, if m.short_name.is_empty() { &m.name } else { &m.short_name }),
                "description": localized(&e.i18n, &m.description),
                "version": m.version,
                "versionName": m.version_name,
                "mayDisable": true,
                "mayEnable": true,
                "enabled": true,
                "isApp": false,
                "type": "extension",
                "installType": "normal",
                "offlineEnabled": false,
                "homepageUrl": m.homepage_url,
                "updateUrl": m.update_url,
                "optionsUrl": if m.options_page.is_empty() { String::new() } else { format!("{}{}", e.base_url, m.options_page.trim_start_matches(['/', '.'])) },
                "icons": icon_list(&m.icons, &e.base_url),
                "permissions": granted.permissions,
                "hostPermissions": granted.origins,
            })
        })
    }

    /// The `chrome.management.ExtensionInfo` of every installed extension, in installation order.
    pub fn management_list(&self) -> Value {
        Value::Array(self.installed_ids().iter().filter_map(|id| self.management_info(id)).collect())
    }

    /// The fields of an AMO `Addon` (see Firefox's AddonManager.webidl) for an installed extension.
    pub fn amo_addon(&self, id: &str) -> Option<Value> {
        self.with_extension(id, |e| {
            json!({
                "id": e.id,
                "version": e.manifest.version,
                "type": "extension",
                "name": localized(&e.i18n, &e.manifest.name),
                "description": localized(&e.i18n, &e.manifest.description),
                "isEnabled": true,
                "isActive": true,
                "canUninstall": true,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::mozilla_signed_id_from_pkcs7;
    use crate::zip::testutil::build_zip;

    #[test]
    fn only_the_store_origins_are_stores() {
        assert_eq!(store_for_url("https://chromewebstore.google.com/detail/x/abc"), Some(Store::ChromeWebStore));
        assert_eq!(store_for_url("https://addons.mozilla.org/en-US/firefox/addon/darkreader/"), Some(Store::Mozilla));
        assert_eq!(store_for_url("https://ADDONS.mozilla.org"), Some(Store::Mozilla));
        assert_eq!(store_for_url("http://chromewebstore.google.com/"), None);
        assert_eq!(store_for_url("https://chromewebstore.google.com.evil.example/"), None);
        assert_eq!(store_for_url("https://evil.example/?https://addons.mozilla.org/"), None);
        assert_eq!(store_for_url("https://addons.mozilla.org:8443/"), None);
        assert_eq!(store_for_url("https://user@addons.mozilla.org/"), None);
        assert_eq!(store_for_url("https://sub.addons.mozilla.org/"), None);
    }

    #[test]
    fn programs_are_function_expressions() {
        for store in [Store::ChromeWebStore, Store::Mozilla] {
            let program = store_program(store);
            let code_start = program.lines().find(|l| !l.trim_start().starts_with(['/', '*']) && !l.trim().is_empty()).unwrap();
            assert!(code_start.starts_with("(function (bridge)"), "{store:?}: {code_start}");
            assert!(program.trim_end().ends_with("})"), "{store:?}");
        }
    }

    #[test]
    fn download_url_needs_a_valid_id() {
        let url = chrome_web_store_download_url("eimadpbcbfnmbkopoojfekhnkhdbieeh", "153.0.0.0").unwrap();
        assert!(url.starts_with("https://clients2.google.com/service/update2/crx?response=redirect&prodversion=153.0.0.0&"));
        assert!(url.contains("x=id%3Deimadpbcbfnmbkopoojfekhnkhdbieeh%26"));
        assert_eq!(chrome_web_store_download_url("../../etc", "1"), None);
        assert!(chrome_web_store_download_url("eimadpbcbfnmbkopoojfekhnkhdbieeh", "1&x=y").unwrap().contains("prodversion=1&acceptformat"));
    }

    #[test]
    fn warnings_follow_chrome() {
        let m = parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage","tabs","alarms"],"host_permissions":["<all_urls>"]}"#).manifest.unwrap();
        assert_eq!(permission_warnings(&m), vec!["Read and change all your data on all websites", "Read your browsing history"]);
        let m = parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1","content_scripts":[{"matches":["https://example.com/*","https://*.example.org/*"],"js":["a.js"]}]}"#)
            .manifest
            .unwrap();
        assert_eq!(permission_warnings(&m), vec!["Read and change your data on all example.org sites and example.com"]);
        let m = parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage"]}"#).manifest.unwrap();
        assert!(permission_warnings(&m).is_empty());
    }

    #[test]
    fn manifest_preview_prefers_the_store_name() {
        let v = preview_manifest(r#"{"manifest_version":3,"name":"__MSG_n__","version":"2.1","permissions":["tabs"]}"#, "Dark Reader");
        assert_eq!(v["name"], "Dark Reader");
        assert_eq!(v["version"], "2.1");
        assert_eq!(v["warnings"][0], "Read your browsing history");
        assert_eq!(preview_manifest("{", "")["ok"], false);
    }

    fn xpi(manifest: &str) -> Vec<u8> {
        build_zip(&[("manifest.json", manifest.as_bytes()), ("bg.js", b"")], false)
    }

    #[test]
    fn package_check_enforces_hash_id_and_permissions() {
        let bytes = xpi(
            r#"{"manifest_version":2,"name":"A","version":"1","background":{"scripts":["bg.js"]},"permissions":["tabs","<all_urls>"],"browser_specific_settings":{"gecko":{"id":"a@example.org"}}}"#,
        );
        let hash = format!("sha256:{}", crate::encoding::sha256(&bytes).iter().map(|b| format!("{b:02x}")).collect::<String>());

        let summary = check_store_package(&bytes, &StoreExpectation { hash: Some(&hash), ..Default::default() }).unwrap();
        assert_eq!(summary.id, "a@example.org");
        assert_eq!(summary.name, "A");
        assert_eq!(summary.warnings.len(), 2);

        assert!(
            check_store_package(&bytes, &StoreExpectation { hash: Some("sha256:00"), ..Default::default() }).unwrap_err().contains("hash")
        );
        assert!(
            check_store_package(&bytes, &StoreExpectation { hash: Some("md5:00"), ..Default::default() })
                .unwrap_err()
                .contains("unsupported")
        );
        assert!(
            check_store_package(&bytes, &StoreExpectation { id: Some("b@example.org"), ..Default::default() })
                .unwrap_err()
                .contains("offered")
        );

        let fewer = r#"{"manifest_version":2,"name":"A","version":"1","permissions":["tabs"]}"#;
        let err = check_store_package(&bytes, &StoreExpectation { approved_manifest: Some(fewer), ..Default::default() }).unwrap_err();
        assert!(err.contains("<all_urls>"), "{err}");
        let same = r#"{"manifest_version":2,"name":"A","version":"1","permissions":["<all_urls>","tabs"]}"#;
        assert!(check_store_package(&bytes, &StoreExpectation { approved_manifest: Some(same), ..Default::default() }).is_ok());

        assert!(check_store_package(b"not a zip", &StoreExpectation::default()).is_err());
    }

    #[test]
    fn signed_id_is_the_addon_common_name() {
        let cn = |name: &str| {
            let mut v = vec![0x30, 0x10, 0x06, 0x03, 0x55, 0x04, 0x03, 0x0c, name.len() as u8];
            v.extend_from_slice(name.as_bytes());
            v
        };
        let mut der = cn("production-signing-ca.addons.mozilla.org");
        der.extend(cn("root-ca-production-amo"));
        der.extend(cn("{01234567-89ab-cdef-0123-456789abcdef}"));
        assert_eq!(mozilla_signed_id_from_pkcs7(&der).as_deref(), Some("{01234567-89ab-cdef-0123-456789abcdef}"));
        assert_eq!(mozilla_signed_id_from_pkcs7(&cn("addon@darkreader.org")).as_deref(), Some("addon@darkreader.org"));
        assert_eq!(mozilla_signed_id_from_pkcs7(&cn("Mozilla AMO Production Signing Service")), None);
        assert_eq!(mozilla_signed_id_from_pkcs7(&[0x06, 0x03, 0x55]), None);
    }
}
