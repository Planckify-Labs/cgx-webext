//! Static compatibility analysis of an extension: which APIs and manifest features does it use, and how well are
//! they supported? This powers the `webext-coverage inspect` command.

use crate::loader::Files;
use crate::manifest::{BackgroundKind, Manifest, ScriptWorld};
use crate::registry::{Engine, Kind, Registry, Status};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Support {
    /// A known API with this status.
    Known(Status),
    /// The namespace exists but the member is not in the registry (could be a property of a result, a typo, or an API
    /// we have not catalogued).
    UnknownMember,
}

#[derive(Debug, Clone)]
pub struct ApiUse {
    /// Canonical registry name, or the best-effort dotted name for unknown members.
    pub name: String,
    pub support: Support,
    pub kind: Option<Kind>,
    pub in_chrome: bool,
    pub in_firefox: bool,
    /// Files in which it appears.
    pub files: Vec<String>,
    pub occurrences: usize,
}

#[derive(Debug, Clone)]
pub struct FeatureUse {
    pub name: &'static str,
    pub status: Status,
    pub note: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Everything used is implemented in the core.
    Ready,
    /// Everything used is implemented or delegated; works once the host browser implements the delegated parts.
    NeedsHost,
    /// Uses at least one API or feature that is planned/unsupported.
    Blocked,
}

#[derive(Debug, Clone)]
pub struct Report {
    pub apis: Vec<ApiUse>,
    pub features: Vec<FeatureUse>,
    pub counts: BTreeMap<&'static str, usize>,
    pub verdict: Verdict,
    /// Names of the planned/unsupported APIs and features that block the extension.
    pub blockers: Vec<String>,
    /// Percentage of distinct known APIs and features that the core provides on its own.
    pub core_percent: f64,
    /// Percentage that works once the host implements the delegated calls.
    pub with_host_percent: f64,
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Whether an expression only combines `chrome`/`browser` with feature tests, e.g.
/// `typeof browser !== 'undefined' ? browser : chrome` or `globalThis.browser ?? chrome`.
fn is_api_root_expression(expr: &str) -> bool {
    const ALLOWED: &[&str] =
        &["typeof", "browser", "chrome", "undefined", "object", "function", "globalThis", "window", "self", "void", "null"];
    let words: Vec<&str> = expr.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$')).filter(|w| !w.is_empty()).collect();
    !words.is_empty() && words.iter().all(|w| ALLOWED.contains(w)) && words.iter().any(|w| *w == "browser" || *w == "chrome")
}

/// Names that refer to the API root in a file, found from declarations like `const api = browser ?? chrome` and
/// destructuring like `const { storage, runtime: rt } = chrome`.
struct Aliases {
    roots: Vec<String>,
    /// local name -> real namespace
    namespaces: Vec<(String, String)>,
}

fn find_aliases(source: &str) -> Aliases {
    let mut out = Aliases { roots: vec![], namespaces: vec![] };
    let bytes = source.as_bytes();
    // Every offset used with `source[..]` below is next to an ASCII byte we matched, so it is a char boundary; bytes of
    // multi-byte characters are never ASCII.
    let text = |from: usize, to: usize| source.get(from..to).unwrap_or("");
    let mut i = 0;
    while i < bytes.len() {
        let rest = &bytes[i..];
        let kw = ["const", "let", "var"].iter().find(|k| {
            rest.starts_with(k.as_bytes())
                && (i == 0 || !is_ident_byte(bytes[i - 1]))
                && rest.get(k.len()).is_some_and(|b| b.is_ascii_whitespace())
        });
        let Some(kw) = kw else {
            i += 1;
            continue;
        };
        let mut j = i + kw.len();
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let (pattern_start, destructure) = if bytes.get(j) == Some(&b'{') { (j + 1, true) } else { (j, false) };
        let pattern_end = if destructure {
            match bytes[pattern_start.min(bytes.len())..].iter().position(|&b| b == b'}') {
                Some(e) => pattern_start + e,
                None => break,
            }
        } else {
            let mut e = pattern_start;
            while e < bytes.len() && is_ident_byte(bytes[e]) {
                e += 1;
            }
            e
        };
        let mut k = pattern_end + usize::from(destructure);
        while k < bytes.len() && bytes[k].is_ascii_whitespace() {
            k += 1;
        }
        if bytes.get(k) == Some(&b'=') && bytes.get(k + 1) != Some(&b'=') {
            let expr_start = k + 1;
            let mut expr_end = bytes[expr_start.min(bytes.len())..]
                .iter()
                .position(|&b| b == b';' || b == b'\n')
                .map_or(bytes.len(), |e| expr_start + e)
                .min(expr_start + 200);
            // The 200-byte cap may fall inside a multi-byte character; back up to a boundary.
            while expr_end > expr_start && !source.is_char_boundary(expr_end) {
                expr_end -= 1;
            }
            if is_api_root_expression(text(expr_start, expr_end)) {
                let pattern = text(pattern_start, pattern_end);
                if destructure {
                    for part in pattern.split(',') {
                        let part = part.trim();
                        let (ns, local) = part.split_once(':').map_or((part, part), |(a, b)| (a.trim(), b.trim()));
                        if !ns.is_empty() && ns.bytes().all(is_ident_byte) && !local.is_empty() && local.bytes().all(is_ident_byte) {
                            out.namespaces.push((local.to_string(), ns.to_string()));
                        }
                    }
                } else if !pattern.is_empty() {
                    out.roots.push(pattern.to_string());
                }
            }
        }
        i = pattern_end.max(i + 1);
    }
    out
}

/// Finds `chrome.a.b.c` / `browser.a.b.c` property chains in JavaScript source, following aliases of the API root. This
/// is a lexical scan, not a parse: it cannot see through arbitrary aliasing or computed access (`chrome["storage"]`).
/// Each returned chain starts at the namespace: `["storage", "local", "get"]`.
pub fn scan_api_chains(source: &str) -> Vec<Vec<String>> {
    let aliases = find_aliases(source);
    let bytes = source.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < bytes.len() {
        if !is_ident_byte(bytes[i]) || (i > 0 && is_ident_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        // Parse a dotted chain (`.` or `?.`) of identifiers starting here.
        let mut chain: Vec<&str> = vec![];
        let mut j = i;
        loop {
            let start = j;
            while j < bytes.len() && is_ident_byte(bytes[j]) {
                j += 1;
            }
            if j == start {
                break;
            }
            chain.push(source.get(start..j).unwrap_or(""));
            if bytes.get(j) == Some(&b'.') {
                j += 1;
            } else if bytes.get(j) == Some(&b'?') && bytes.get(j + 1) == Some(&b'.') {
                j += 2;
            } else {
                break;
            }
        }
        // A chain preceded by `.` is a member access on something else (`foo.chrome.storage`).
        let preceded_by_dot = i > 0 && bytes[i - 1] == b'.';
        i = j.max(i + 1);
        if preceded_by_dot {
            continue;
        }

        let mut parts = chain.as_slice();
        while parts.len() > 1 && matches!(parts[0], "globalThis" | "window" | "self") {
            parts = &parts[1..];
        }
        let first = parts[0];
        let resolved: Option<Vec<String>> = if first == "chrome" || first == "browser" || aliases.roots.iter().any(|r| r == first) {
            Some(parts[1..].iter().map(|s| s.to_string()).collect())
        } else if let Some((_, ns)) = aliases.namespaces.iter().find(|(local, _)| local == first) {
            Some(std::iter::once(ns.clone()).chain(parts[1..].iter().map(|s| s.to_string())).collect())
        } else {
            None
        };
        if let Some(chain) = resolved
            && chain.len() >= 2
        {
            out.push(chain);
        }
    }
    out
}

fn resolve(reg: &Registry, chain: &[String]) -> Option<(String, Option<crate::registry::Member>)> {
    // The longest prefix that is a registry member wins: `storage.local.get.bind` resolves to `storage.local.get`.
    for k in (1..=chain.len()).rev() {
        if let Some(m) = reg.lookup(&chain[..k].join(".")) {
            return Some((m.full_name.clone(), Some(m.clone())));
        }
    }
    let ns = reg.canonical_name(&chain[0]);
    if !reg.namespaces().contains(&ns) || chain.len() < 2 {
        return None;
    }
    // References that are not calls into the API: types, enums and constants (`webRequest.ResourceType`,
    // `offscreen.Reason.CLIPBOARD`, `storage.sync.MAX_ITEMS`) start a segment with a capital letter.
    if chain[1..].iter().any(|seg| seg.starts_with(|c: char| c.is_ascii_uppercase())) {
        return None;
    }
    // A container of known members, such as the `storage.local` area object itself.
    let prefix = format!("{}.{}.", ns, chain[1..].join("."));
    if reg.members().iter().any(|m| m.full_name.starts_with(&prefix)) {
        return None;
    }
    // Known namespace, unknown member.
    Some((format!("{}.{}", chain[0], chain[1..].join(".")), None))
}

pub fn manifest_features(m: &Manifest) -> Vec<FeatureUse> {
    let mut f = vec![];
    let mut add = |name: &'static str, status: Status, note: &'static str| f.push(FeatureUse { name, status, note });
    match m.background.kind {
        BackgroundKind::None => {}
        BackgroundKind::Page => add("background page", Status::Delegated, "host must run a hidden page at the extension origin"),
        BackgroundKind::Scripts => {
            add("background scripts / event page", Status::Delegated, "host must run a hidden page at the extension origin")
        }
        BackgroundKind::ServiceWorker => add(
            "background service worker",
            Status::Delegated,
            "host runs it as an event page; true service-worker lifetime is not modelled",
        ),
    }
    if !m.content_scripts.is_empty() {
        add("content scripts (isolated world)", Status::Delegated, "host needs isolated-world script injection");
    }
    if m.content_scripts.iter().any(|c| c.world == ScriptWorld::Main) {
        add("content scripts (world: MAIN)", Status::Delegated, "host needs main-world injection");
    }
    if m.action.is_some() {
        add("toolbar action / popup", Status::Delegated, "host UI");
    }
    if !m.options_page.is_empty() {
        add("options page", Status::Delegated, "host UI");
    }
    if !m.web_accessible_resources.is_empty() {
        add("web_accessible_resources", Status::Delegated, "host must enforce access to extension resources from web pages");
    }
    if !m.declarative_net_request_rulesets.is_empty() {
        add("declarative_net_request rulesets", Status::Planned, "network blocking is not implemented yet");
    }
    if m.uses_permission("webRequestBlocking") {
        add("blocking webRequest", Status::Planned, "needs request interception in the network stack");
    }
    if !m.commands.is_empty() {
        add("keyboard commands", Status::Planned, "");
    }
    if !m.url_overrides.is_empty() {
        add("chrome_url_overrides", Status::Planned, "");
    }
    if !m.side_panel_path.is_empty() {
        add("side panel", Status::Planned, "");
    }
    if !m.sidebar_default_panel.is_empty() {
        add("sidebar_action", Status::Planned, "");
    }
    if !m.omnibox_keyword.is_empty() {
        add("omnibox keyword", Status::Planned, "");
    }
    if m.raw.get("devtools_page").is_some() {
        add("devtools_page", Status::Planned, "");
    }
    if m.raw.get("chrome_settings_overrides").is_some() {
        add("chrome_settings_overrides", Status::Unsupported, "search/homepage overrides are deliberately not supported");
    }
    if m.raw.get("externally_connectable").is_some() {
        add("externally_connectable", Status::Planned, "");
    }
    if m.raw.get("native_messaging").is_some() || m.uses_permission("nativeMessaging") {
        add("native messaging", Status::Planned, "");
    }
    f
}

pub fn analyze(reg: &Registry, manifest: &Manifest, files: &Files) -> Report {
    let mut uses: BTreeMap<String, ApiUse> = BTreeMap::new();
    for (path, data) in files.iter() {
        let lower = path.to_ascii_lowercase();
        if !(lower.ends_with(".js")
            || lower.ends_with(".mjs")
            || lower.ends_with(".cjs")
            || lower.ends_with(".html")
            || lower.ends_with(".htm"))
            || data.len() > 16 * 1024 * 1024
        {
            continue;
        }
        let text = String::from_utf8_lossy(data);
        for chain in scan_api_chains(&text) {
            let Some((name, member)) = resolve(reg, &chain) else { continue };
            let entry = uses.entry(name.clone()).or_insert_with(|| match &member {
                Some(m) => ApiUse {
                    name: name.clone(),
                    support: Support::Known(m.status),
                    kind: Some(m.kind),
                    in_chrome: m.available_in(Engine::Chrome),
                    in_firefox: m.available_in(Engine::Firefox),
                    files: vec![],
                    occurrences: 0,
                },
                None => ApiUse {
                    name: name.clone(),
                    support: Support::UnknownMember,
                    kind: None,
                    in_chrome: false,
                    in_firefox: false,
                    files: vec![],
                    occurrences: 0,
                },
            });
            entry.occurrences += 1;
            if !entry.files.iter().any(|f| f == path) {
                entry.files.push(path.to_string());
            }
        }
    }
    let apis: Vec<ApiUse> = uses.into_values().collect();
    let features = manifest_features(manifest);

    let mut counts: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut blockers = vec![];
    let mut tally = |counts: &mut BTreeMap<&'static str, usize>, status: Status, name: &str| {
        *counts.entry(status.as_str()).or_default() += 1;
        if matches!(status, Status::Planned | Status::Unsupported) {
            blockers.push(name.to_string());
        }
    };
    for a in &apis {
        match &a.support {
            Support::Known(s) => tally(&mut counts, *s, &a.name),
            Support::UnknownMember => *counts.entry("unknown").or_default() += 1,
        }
    }
    for f in &features {
        tally(&mut counts, f.status, f.name);
    }
    let known = counts.iter().filter(|(k, _)| **k != "unknown").map(|(_, v)| *v).sum::<usize>();
    let get = |k: &str| counts.get(k).copied().unwrap_or(0);
    let core = get("implemented") + get("partial");
    let pct = |n: usize| if known == 0 { 100.0 } else { n as f64 * 100.0 / known as f64 };
    let verdict = if !blockers.is_empty() {
        Verdict::Blocked
    } else if get("delegated") > 0 {
        Verdict::NeedsHost
    } else {
        Verdict::Ready
    };
    let (core_percent, with_host_percent) = (pct(core), pct(core + get("delegated")));
    Report { apis, features, counts, verdict, blockers, core_percent, with_host_percent }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn chains(s: &str) -> Vec<String> {
        scan_api_chains(s).iter().map(|c| c.join(".")).collect()
    }

    #[test]
    fn scanner_finds_chains() {
        assert_eq!(chains("chrome.storage.local.get(['a'], cb)"), ["storage.local.get"]);
        assert_eq!(
            chains("browser.runtime.onMessage.addListener(f); chrome.tabs.query({})"),
            ["runtime.onMessage.addListener", "tabs.query"]
        );
        assert_eq!(chains("await globalThis.chrome.alarms.create('a', {})"), ["alarms.create"]);
        assert_eq!(chains("window.browser.i18n.getMessage('x')"), ["i18n.getMessage"]);
        assert_eq!(chains("if (chrome.runtime.lastError) {}"), ["runtime.lastError"]);
    }

    #[test]
    fn scanner_follows_aliases_and_destructuring() {
        let src =
            "const api = typeof browser !== 'undefined' ? browser : chrome;\napi.storage.local.get('a'); api.runtime?.sendMessage({});";
        assert_eq!(chains(src), ["storage.local.get", "runtime.sendMessage"]);
        assert_eq!(chains("var b = globalThis.browser ?? chrome; b.tabs.query({})"), ["tabs.query"]);
        assert_eq!(chains("const ext = chrome; ext.alarms.create('x', {})"), ["alarms.create"]);
        assert_eq!(
            chains("const { storage, runtime: rt } = chrome;\nstorage.local.set({}); rt.onMessage.addListener(f);"),
            ["storage.local.set", "runtime.onMessage.addListener"]
        );
        // Only declarations that really alias the API root count.
        assert!(chains("const api = makeApi(chrome);\napi.storage.local.get()").is_empty());
        assert!(chains("const api = somethingElse;\napi.storage.local.get()").is_empty());
        assert!(chains("const x = chrome.runtime.id;\nx.storage.get()").len() == 1);
    }

    #[test]
    fn scanner_survives_non_ascii_source() {
        // Regression: slicing at arbitrary byte offsets panicked on real extensions (Dark Reader, Vimium, Bitwarden...).
        let src = "const s = 'ɵ⌨↔ç';\nconst api = browser ?? chrome; // ⌨⌨⌨\napi.storage.local.get('ɵ');\nconst { runtime } = chrome; const x = 'é'.repeat(300);";
        assert_eq!(chains(src), ["storage.local.get"]);
        // A multi-byte character exactly where the 200-byte expression cap falls.
        let long = format!("const a = {}é;\nchrome.tabs.query({{}})", "x".repeat(199));
        assert_eq!(chains(&long), ["tabs.query"]);
        // Arbitrary bytes that are valid UTF-8 must never panic.
        for unit in ["é", "日本", "😀", "\u{0}", "const", "const {", "chrome.", "= chrome", "const é = chrome;"] {
            let _ = scan_api_chains(&unit.repeat(50));
        }
    }

    #[test]
    fn scanner_ignores_lookalikes() {
        assert!(chains("mychrome.storage.get()").is_empty());
        assert!(chains("foo.chrome.storage.get()").is_empty());
        assert!(chains("var s = 'chrome is great'; chrome.").is_empty());
        assert!(chains("chromeless.storage").is_empty());
        assert!(chains("// no api here").is_empty());
    }

    fn report(manifest: &str, js: &str) -> Report {
        let m = parse_manifest(manifest).manifest.unwrap();
        let mut f = Files::new();
        f.insert("bg.js", js.as_bytes().to_vec());
        analyze(&Registry::builtin(), &m, &f)
    }

    const M: &str = r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage","tabs"]}"#;

    #[test]
    fn resolves_longest_member_prefix_and_classifies() {
        let r = report(
            M,
            "chrome.storage.local.get.bind(x); chrome.tabs.query({}); chrome.runtime.lastError.message; chrome.downloads.download({});",
        );
        let status = |n: &str| r.apis.iter().find(|a| a.name == n).map(|a| a.support.clone());
        assert_eq!(status("storage.local.get"), Some(Support::Known(Status::Implemented)));
        assert_eq!(status("tabs.query"), Some(Support::Known(Status::Delegated)));
        assert_eq!(status("runtime.lastError"), Some(Support::Known(Status::Implemented)));
        assert_eq!(status("downloads.download"), Some(Support::Known(Status::Planned)));
        assert_eq!(r.verdict, Verdict::Blocked);
        assert_eq!(r.blockers, ["downloads.download"]);
    }

    #[test]
    fn verdicts() {
        assert_eq!(report(M, "chrome.storage.local.set({}); chrome.i18n.getMessage('x')").verdict, Verdict::Ready);
        assert_eq!(report(M, "chrome.storage.local.set({}); chrome.tabs.query({})").verdict, Verdict::NeedsHost);
        let r = report(M, "chrome.storage.local.set({}); chrome.tabs.query({})");
        assert!((r.core_percent - 50.0).abs() < 1e-9 && (r.with_host_percent - 100.0).abs() < 1e-9);
        // Nothing used at all is trivially ready.
        assert_eq!(report(M, "console.log(1)").verdict, Verdict::Ready);
    }

    #[test]
    fn unknown_members_are_reported_but_do_not_block() {
        let r = report(M, "chrome.storage.nonsense.thing(); chrome.madeup.thing();");
        let unknown: Vec<_> = r.apis.iter().filter(|a| a.support == Support::UnknownMember).map(|a| a.name.as_str()).collect();
        assert_eq!(unknown, ["storage.nonsense.thing"]);
        assert_eq!(r.verdict, Verdict::Ready);
        assert_eq!(r.counts.get("unknown"), Some(&1));
    }

    #[test]
    fn types_constants_and_containers_are_not_unknown_apis() {
        let r = report(
            M,
            "const a = chrome.storage.local; chrome.runtime.PlatformOs.MAC; chrome.webRequest.ResourceType.WEBSOCKET; \
             chrome.offscreen.Reason.CLIPBOARD; chrome.sessions.MAX_SESSION_RESULTS; chrome.storage.local.get('x');",
        );
        let names: Vec<_> = r.apis.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["storage.local.get"], "only the real call should be reported");
        assert!(!r.counts.contains_key("unknown"));
    }

    #[test]
    fn manifest_features_affect_verdict() {
        let m = r#"{"manifest_version":3,"name":"x","version":"1","declarative_net_request":{"rule_resources":[{"id":"r","enabled":true,"path":"r.json"}]},"permissions":["declarativeNetRequest"],"background":{"service_worker":"s.js"}}"#;
        let r = report(m, "");
        assert_eq!(r.verdict, Verdict::Blocked);
        assert!(r.blockers.contains(&"declarative_net_request rulesets".to_string()));
        assert!(r.features.iter().any(|f| f.name == "background service worker"));
    }

    #[test]
    fn aliases_and_occurrence_counts() {
        let r = report(
            r#"{"manifest_version":2,"name":"x","version":"1"}"#,
            "browser.browserAction.setTitle({}); chrome.browserAction.setTitle({});",
        );
        let a = r.apis.iter().find(|a| a.name == "action.setTitle").unwrap();
        assert_eq!(a.occurrences, 2);
        assert_eq!(a.files, ["bg.js"]);
    }
}
