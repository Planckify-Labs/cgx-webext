//! `manifest.json` parsing, normalisation across Manifest V2/V3 and Chrome/Firefox dialects, and validation.

use crate::match_pattern::{MatchDialect, MatchPattern};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Uses features only Chrome has.
    Chrome,
    /// Uses features only Firefox has.
    Firefox,
    /// Portable (or both families' keys present).
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackgroundKind {
    #[default]
    None,
    Page,
    Scripts,
    ServiceWorker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunAt {
    #[default]
    DocumentIdle,
    DocumentStart,
    DocumentEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ScriptWorld {
    #[default]
    Isolated,
    Main,
}

/// Which manifest key an action was declared under; they are otherwise equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Action,
    BrowserAction,
    PageAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub field: String,
    pub message: String,
}

/// Icon paths by pixel size. Size 0 stands for the single-string form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IconSet(pub Vec<(u32, String)>);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContentScript {
    pub matches: Vec<String>,
    pub exclude_matches: Vec<String>,
    pub include_globs: Vec<String>,
    pub exclude_globs: Vec<String>,
    pub js: Vec<String>,
    pub css: Vec<String>,
    pub run_at: RunAt,
    pub all_frames: bool,
    pub match_about_blank: bool,
    pub match_origin_as_fallback: bool,
    pub world: ScriptWorld,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub kind: ActionKind,
    pub default_title: String,
    pub default_popup: String,
    pub default_icon: IconSet,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Background {
    pub kind: BackgroundKind,
    pub scripts: Vec<String>,
    pub page: String,
    pub service_worker: String,
    pub persistent: bool,
    pub is_module: bool,
    /// Chrome 121+ and Firefox both accept `scripts` and `service_worker` together; each browser picks what it supports.
    pub has_scripts_and_service_worker: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WebAccessibleResource {
    pub resources: Vec<String>,
    pub matches: Vec<String>,
    pub extension_ids: Vec<String>,
    pub use_dynamic_url: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub suggested_key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleResource {
    pub id: String,
    pub path: String,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct Manifest {
    pub manifest_version: u32,
    pub name: String,
    pub short_name: String,
    pub version: String,
    pub version_name: String,
    pub description: String,
    pub author: String,
    pub homepage_url: String,
    pub default_locale: String,
    pub minimum_chrome_version: String,
    pub update_url: String,
    /// Base64 public key; pins the Chrome extension ID.
    pub key: String,
    pub incognito: String,
    pub icons: IconSet,

    /// API permissions such as `storage`. In MV2 host patterns live in the same JSON array; they are split out here
    /// regardless of manifest version.
    pub permissions: Vec<String>,
    pub optional_permissions: Vec<String>,
    pub host_permissions: Vec<String>,
    pub optional_host_permissions: Vec<String>,

    pub background: Background,
    pub content_scripts: Vec<ContentScript>,
    pub action: Option<Action>,
    pub options_page: String,
    pub options_open_in_tab: bool,
    pub web_accessible_resources: Vec<WebAccessibleResource>,
    pub csp_extension_pages: String,
    pub csp_sandbox: String,
    pub commands: Vec<Command>,
    pub declarative_net_request_rulesets: Vec<RuleResource>,
    pub side_panel_path: String,
    pub sidebar_default_panel: String,
    pub url_overrides: Vec<(String, String)>,
    pub omnibox_keyword: String,

    pub gecko_id: String,
    pub gecko_strict_min_version: String,

    pub dialect: Dialect,
    /// The raw JSON, for keys that are not modelled.
    pub raw: Value,
}

impl Manifest {
    pub fn uses_permission(&self, p: &str) -> bool {
        self.permissions.iter().any(|x| x == p)
    }

    pub fn is_mv3(&self) -> bool {
        self.manifest_version == 3
    }
}

#[derive(Debug, Clone)]
pub struct ManifestParseResult {
    /// `None` if there were fatal errors.
    pub manifest: Option<Manifest>,
    pub diagnostics: Vec<Diagnostic>,
}

impl ManifestParseResult {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.severity == Severity::Error)
    }

    pub fn warnings(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.severity == Severity::Warning)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.severity == Severity::Error)
    }
}

/// True for strings that are match patterns / host permissions rather than API permissions.
pub fn looks_like_host_permission(s: &str) -> bool {
    s == "<all_urls>" || s.contains("://")
}

/// The default Content-Security-Policy for extension pages.
pub fn default_extension_csp() -> &'static str {
    "script-src 'self' 'wasm-unsafe-eval'; object-src 'self'"
}

/// Removes `//` and `/* */` comments outside strings, as accepted by both browsers' manifest parsers, and a UTF-8 BOM.
/// Comments are replaced by whitespace so that error offsets stay meaningful.
pub fn strip_json_comments(text: &str) -> String {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = '\0';
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

struct Ctx {
    diagnostics: Vec<Diagnostic>,
}

impl Ctx {
    fn warn(&mut self, field: &str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic { severity: Severity::Warning, field: field.into(), message: message.into() });
    }

    fn error(&mut self, field: &str, message: impl Into<String>) {
        self.diagnostics.push(Diagnostic { severity: Severity::Error, field: field.into(), message: message.into() });
    }

    fn string(&mut self, obj: &Value, key: &str, required: bool) -> String {
        match obj.get(key) {
            None if required => {
                self.error(key, "required key is missing");
                String::new()
            }
            None => String::new(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => {
                if required {
                    self.error(key, "expected a string");
                } else {
                    self.warn(key, "expected a string; ignoring");
                }
                String::new()
            }
        }
    }

    fn boolean(&mut self, obj: &Value, key: &str, fallback: bool) -> bool {
        match obj.get(key) {
            None => fallback,
            Some(Value::Bool(b)) => *b,
            Some(_) => {
                self.warn(key, "expected a boolean; ignoring");
                fallback
            }
        }
    }

    fn string_list(&mut self, obj: &Value, key: &str, prefix: &str) -> Vec<String> {
        let Some(v) = obj.get(key) else { return vec![] };
        let field = format!("{prefix}{key}");
        let Some(items) = v.as_array() else {
            self.warn(&field, "expected an array of strings; ignoring");
            return vec![];
        };
        let mut out = vec![];
        for item in items {
            match item.as_str() {
                Some(s) => out.push(s.to_string()),
                None => self.warn(&field, "non-string entry ignored"),
            }
        }
        out
    }
}

fn parse_icons(v: &Value) -> IconSet {
    match v {
        Value::String(s) => IconSet(vec![(0, s.clone())]),
        Value::Object(map) => {
            let mut icons: Vec<(u32, String)> =
                map.iter().filter_map(|(k, v)| Some((k.parse::<u32>().ok().filter(|&n| n > 0)?, v.as_str()?.to_string()))).collect();
            icons.sort();
            IconSet(icons)
        }
        _ => IconSet::default(),
    }
}

/// Chrome version strings: one to four dot-separated integers in 0..=65535 without leading zeros.
pub fn is_chrome_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes().all(|b| b.is_ascii_digit())
                && (p.len() == 1 || !p.starts_with('0'))
                && p.parse::<u32>().is_ok_and(|n| n <= 65535)
        })
}

fn parse_content_scripts(ctx: &mut Ctx, root: &Value, m: &mut Manifest) {
    let Some(cs) = root.get("content_scripts") else { return };
    let Some(entries) = cs.as_array() else {
        ctx.warn("content_scripts", "expected an array; ignoring");
        return;
    };
    for (index, entry) in entries.iter().enumerate() {
        let prefix = format!("content_scripts[{index}].");
        if !entry.is_object() {
            ctx.error(&prefix, "expected an object");
            continue;
        }
        let mut script = ContentScript {
            matches: ctx.string_list(entry, "matches", &prefix),
            exclude_matches: ctx.string_list(entry, "exclude_matches", &prefix),
            include_globs: ctx.string_list(entry, "include_globs", &prefix),
            exclude_globs: ctx.string_list(entry, "exclude_globs", &prefix),
            js: ctx.string_list(entry, "js", &prefix),
            css: ctx.string_list(entry, "css", &prefix),
            all_frames: ctx.boolean(entry, "all_frames", false),
            match_about_blank: ctx.boolean(entry, "match_about_blank", false),
            match_origin_as_fallback: ctx.boolean(entry, "match_origin_as_fallback", false),
            ..Default::default()
        };
        match ctx.string(entry, "run_at", false).as_str() {
            "" | "document_idle" => {}
            "document_start" => script.run_at = RunAt::DocumentStart,
            "document_end" => script.run_at = RunAt::DocumentEnd,
            other => ctx.warn(&format!("{prefix}run_at"), format!("unknown value '{other}'; using document_idle")),
        }
        match ctx.string(entry, "world", false).as_str() {
            "" | "ISOLATED" => {}
            "MAIN" => script.world = ScriptWorld::Main,
            other => ctx.warn(&format!("{prefix}world"), format!("unknown value '{other}'; using ISOLATED")),
        }

        let mut valid = true;
        if script.matches.is_empty() {
            ctx.error(&format!("{prefix}matches"), "a content script needs at least one match pattern");
            valid = false;
        }
        for pattern in script.matches.iter().chain(&script.exclude_matches) {
            if let Err(e) = MatchPattern::parse(pattern, MatchDialect::Firefox) {
                ctx.error(&format!("{prefix}matches"), format!("invalid match pattern '{pattern}': {e}"));
                valid = false;
            }
        }
        if script.js.is_empty() && script.css.is_empty() {
            ctx.warn(&prefix, "content script has neither js nor css");
        }
        if valid {
            m.content_scripts.push(std::mem::take(&mut script));
        }
    }
}

fn split_permissions(ctx: &mut Ctx, items: Vec<String>, api: &mut Vec<String>, hosts: &mut Vec<String>, field: &str) {
    for p in items {
        if looks_like_host_permission(&p) {
            match MatchPattern::parse(&p, MatchDialect::Firefox) {
                Ok(_) => hosts.push(p),
                Err(e) => ctx.warn(field, format!("invalid host permission '{p}': {e}")),
            }
        } else {
            api.push(p);
        }
    }
}

fn parse_permissions(ctx: &mut Ctx, root: &Value, m: &mut Manifest) {
    let perms = ctx.string_list(root, "permissions", "");
    split_permissions(ctx, perms, &mut m.permissions, &mut m.host_permissions, "permissions");
    let optional = ctx.string_list(root, "optional_permissions", "");
    split_permissions(ctx, optional, &mut m.optional_permissions, &mut m.optional_host_permissions, "optional_permissions");

    if m.manifest_version == 3 {
        for (key, target) in [("host_permissions", false), ("optional_host_permissions", true)] {
            for p in ctx.string_list(root, key, "") {
                match MatchPattern::parse(&p, MatchDialect::Firefox) {
                    Ok(_) if target => m.optional_host_permissions.push(p),
                    Ok(_) => m.host_permissions.push(p),
                    Err(e) => ctx.warn(key, format!("invalid host permission '{p}': {e}")),
                }
            }
        }
    } else if root.get("host_permissions").is_some() {
        ctx.warn("host_permissions", "host_permissions is only valid in manifest_version 3");
    }
}

fn parse_background(ctx: &mut Ctx, root: &Value, m: &mut Manifest) {
    let Some(bg) = root.get("background") else { return };
    if !bg.is_object() {
        ctx.error("background", "expected an object");
        return;
    }
    let b = &mut m.background;
    b.scripts = ctx.string_list(bg, "scripts", "background.");
    b.page = ctx.string(bg, "page", false);
    b.service_worker = ctx.string(bg, "service_worker", false);
    b.persistent = ctx.boolean(bg, "persistent", m.manifest_version == 2);
    b.is_module = ctx.string(bg, "type", false) == "module";

    if !b.service_worker.is_empty() {
        b.kind = BackgroundKind::ServiceWorker;
        b.has_scripts_and_service_worker = !b.scripts.is_empty();
        if m.manifest_version == 2 {
            ctx.warn("background.service_worker", "service_worker is only valid in manifest_version 3");
        }
    } else if !b.scripts.is_empty() {
        b.kind = BackgroundKind::Scripts;
    } else if !b.page.is_empty() {
        b.kind = BackgroundKind::Page;
    } else {
        ctx.warn("background", "no scripts, page or service_worker declared");
    }
    if m.manifest_version == 3 && bg.get("persistent").and_then(Value::as_bool) == Some(true) {
        ctx.warn("background.persistent", "persistent background contexts are not allowed in manifest_version 3");
    }
}

fn parse_action(ctx: &mut Ctx, root: &Value, m: &mut Manifest) {
    for (key, kind) in
        [("action", ActionKind::Action), ("browser_action", ActionKind::BrowserAction), ("page_action", ActionKind::PageAction)]
    {
        let Some(a) = root.get(key) else { continue };
        if !a.is_object() {
            ctx.warn(key, "expected an object; ignoring");
            continue;
        }
        let wrong = match kind {
            ActionKind::Action => m.manifest_version == 2,
            ActionKind::BrowserAction => m.manifest_version == 3,
            ActionKind::PageAction => false,
        };
        if wrong {
            ctx.warn(key, format!("'{key}' is not valid in manifest_version {}", m.manifest_version));
        }
        if m.action.is_some() {
            continue; // First declaration wins.
        }
        m.action = Some(Action {
            kind,
            default_title: ctx.string(a, "default_title", false),
            default_popup: ctx.string(a, "default_popup", false),
            default_icon: a.get("default_icon").map(parse_icons).unwrap_or_default(),
        });
    }
}

fn parse_war(ctx: &mut Ctx, root: &Value, m: &mut Manifest) {
    let Some(war) = root.get("web_accessible_resources") else { return };
    let Some(items) = war.as_array() else {
        ctx.warn("web_accessible_resources", "expected an array; ignoring");
        return;
    };
    if m.manifest_version == 2 {
        m.web_accessible_resources.push(WebAccessibleResource {
            resources: items.iter().filter_map(|i| i.as_str().map(String::from)).collect(),
            matches: vec!["<all_urls>".into()],
            ..Default::default()
        });
        return;
    }
    for item in items {
        if !item.is_object() {
            ctx.warn("web_accessible_resources", "entries must be objects in manifest_version 3");
            continue;
        }
        let entry = WebAccessibleResource {
            resources: ctx.string_list(item, "resources", "web_accessible_resources[]."),
            matches: ctx.string_list(item, "matches", "web_accessible_resources[]."),
            extension_ids: ctx.string_list(item, "extension_ids", "web_accessible_resources[]."),
            use_dynamic_url: ctx.boolean(item, "use_dynamic_url", false),
        };
        if entry.matches.is_empty() && entry.extension_ids.is_empty() {
            ctx.warn("web_accessible_resources", "entry has neither matches nor extension_ids and is unreachable");
        }
        m.web_accessible_resources.push(entry);
    }
}

fn parse_csp(ctx: &mut Ctx, root: &Value, m: &mut Manifest) {
    m.csp_extension_pages = default_extension_csp().to_string();
    match root.get("content_security_policy") {
        None => {}
        Some(Value::String(s)) => {
            if m.manifest_version == 3 {
                ctx.warn("content_security_policy", "manifest_version 3 expects an object with extension_pages");
            }
            m.csp_extension_pages = s.clone();
        }
        Some(obj @ Value::Object(_)) => {
            let pages = ctx.string(obj, "extension_pages", false);
            if !pages.is_empty() {
                m.csp_extension_pages = pages;
            }
            m.csp_sandbox = ctx.string(obj, "sandbox", false);
        }
        Some(_) => ctx.warn("content_security_policy", "expected a string or object; ignoring"),
    }
}

fn obj_string(v: Option<&Value>, key: &str) -> String {
    v.and_then(|v| v.get(key)).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Parses and validates a `manifest.json`. Fatal problems (invalid JSON, unsupported `manifest_version`, missing
/// `name`/`version`) yield no manifest; everything else is reported as a diagnostic while still producing one.
pub fn parse_manifest(text: &str) -> ManifestParseResult {
    let mut ctx = Ctx { diagnostics: vec![] };
    let finish = |ctx: Ctx, manifest: Option<Manifest>| ManifestParseResult { manifest, diagnostics: ctx.diagnostics };

    let root: Value = match serde_json::from_str(&strip_json_comments(text)) {
        Ok(v) => v,
        Err(e) => {
            ctx.error("manifest.json", format!("invalid JSON: {e}"));
            return finish(ctx, None);
        }
    };
    if !root.is_object() {
        ctx.error("manifest.json", "top level must be an object");
        return finish(ctx, None);
    }

    let manifest_version = match root.get("manifest_version").and_then(Value::as_u64) {
        Some(v @ (2 | 3)) => v as u32,
        Some(_) => {
            ctx.error("manifest_version", "only manifest_version 2 and 3 are supported");
            return finish(ctx, None);
        }
        None => {
            ctx.error("manifest_version", "required key is missing or not an integer");
            return finish(ctx, None);
        }
    };

    let name = ctx.string(&root, "name", true);
    let version = ctx.string(&root, "version", true);
    if root.get("name").is_some() && name.is_empty() {
        ctx.error("name", "name must not be empty");
    }
    if !version.is_empty() && !is_chrome_version(&version) {
        ctx.warn("version", "not a valid Chrome version string (1-4 dot-separated integers); Chrome would reject this");
    }

    let mut m = Manifest {
        manifest_version,
        name,
        version,
        short_name: ctx.string(&root, "short_name", false),
        version_name: ctx.string(&root, "version_name", false),
        description: ctx.string(&root, "description", false),
        author: match root.get("author") {
            Some(Value::String(s)) => s.clone(),
            Some(obj @ Value::Object(_)) => obj_string(Some(obj), "email"),
            _ => String::new(),
        },
        homepage_url: ctx.string(&root, "homepage_url", false),
        default_locale: ctx.string(&root, "default_locale", false),
        minimum_chrome_version: ctx.string(&root, "minimum_chrome_version", false),
        update_url: ctx.string(&root, "update_url", false),
        key: ctx.string(&root, "key", false),
        incognito: match ctx.string(&root, "incognito", false) {
            s if s.is_empty() => "spanning".into(),
            s => s,
        },
        icons: root.get("icons").map(parse_icons).unwrap_or_default(),
        permissions: vec![],
        optional_permissions: vec![],
        host_permissions: vec![],
        optional_host_permissions: vec![],
        background: Background::default(),
        content_scripts: vec![],
        action: None,
        options_page: String::new(),
        options_open_in_tab: false,
        web_accessible_resources: vec![],
        csp_extension_pages: String::new(),
        csp_sandbox: String::new(),
        commands: vec![],
        declarative_net_request_rulesets: vec![],
        side_panel_path: obj_string(root.get("side_panel"), "default_path"),
        sidebar_default_panel: obj_string(root.get("sidebar_action"), "default_panel"),
        url_overrides: vec![],
        omnibox_keyword: obj_string(root.get("omnibox"), "keyword"),
        gecko_id: String::new(),
        gecko_strict_min_version: String::new(),
        dialect: Dialect::Both,
        raw: root.clone(),
    };

    parse_permissions(&mut ctx, &root, &mut m);
    parse_background(&mut ctx, &root, &mut m);
    parse_content_scripts(&mut ctx, &root, &mut m);
    parse_action(&mut ctx, &root, &mut m);
    parse_war(&mut ctx, &root, &mut m);
    parse_csp(&mut ctx, &root, &mut m);

    if let Some(o) = root.get("options_ui").filter(|o| o.is_object()) {
        m.options_page = obj_string(Some(o), "page");
        m.options_open_in_tab = o.get("open_in_tab").and_then(Value::as_bool).unwrap_or(false);
    } else {
        let page = ctx.string(&root, "options_page", false);
        if !page.is_empty() {
            m.options_page = page;
            m.options_open_in_tab = true;
        }
    }

    if let Some(cmds) = root.get("commands").and_then(Value::as_object) {
        for (name, v) in cmds {
            let suggested_key = match v.get("suggested_key") {
                Some(Value::String(s)) => s.clone(),
                Some(obj @ Value::Object(_)) => obj_string(Some(obj), "default"),
                _ => String::new(),
            };
            m.commands.push(Command { name: name.clone(), description: obj_string(Some(v), "description"), suggested_key });
        }
    }

    if let Some(rr) = root.get("declarative_net_request").and_then(|d| d.get("rule_resources")).and_then(Value::as_array) {
        for r in rr.iter().filter(|r| r.is_object()) {
            m.declarative_net_request_rulesets.push(RuleResource {
                id: obj_string(Some(r), "id"),
                path: obj_string(Some(r), "path"),
                enabled: r.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            });
        }
    }

    if let Some(ov) = root.get("chrome_url_overrides").and_then(Value::as_object) {
        for (k, v) in ov {
            if let Some(s) = v.as_str() {
                m.url_overrides.push((k.clone(), s.to_string()));
            }
        }
    }

    // Firefox identity lives under browser_specific_settings (or the legacy "applications").
    let mut has_gecko = false;
    for key in ["browser_specific_settings", "applications"] {
        if let Some(gecko) = root.get(key).and_then(|b| b.get("gecko")).filter(|g| g.is_object()) {
            has_gecko = true;
            if m.gecko_id.is_empty() {
                m.gecko_id = obj_string(Some(gecko), "id");
            }
            if m.gecko_strict_min_version.is_empty() {
                m.gecko_strict_min_version = obj_string(Some(gecko), "strict_min_version");
            }
        }
    }

    // Dialect heuristics: which family's exclusive features does the manifest rely on?
    let chrome_only = !m.minimum_chrome_version.is_empty()
        || !m.key.is_empty()
        || !m.side_panel_path.is_empty()
        || m.uses_permission("offscreen")
        || m.uses_permission("sidePanel")
        || (m.background.kind == BackgroundKind::ServiceWorker && !m.background.has_scripts_and_service_worker && !has_gecko);
    let firefox_only = has_gecko
        || !m.sidebar_default_panel.is_empty()
        || ["menus", "theme", "find", "browserSettings", "contextualIdentities"].iter().any(|p| m.uses_permission(p));
    m.dialect = match (chrome_only, firefox_only) {
        (true, false) => Dialect::Chrome,
        (false, true) => Dialect::Firefox,
        _ => Dialect::Both,
    };

    if m.manifest_version == 3 && m.background.kind == BackgroundKind::ServiceWorker && !m.background.has_scripts_and_service_worker {
        ctx.warn(
            "background.service_worker",
            "Firefox does not support background service workers; add background.scripts for Firefox compatibility",
        );
    }

    let has_errors = ctx.diagnostics.iter().any(|d| d.severity == Severity::Error);
    finish(ctx, if has_errors { None } else { Some(m) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Manifest {
        let r = parse_manifest(text);
        assert!(r.manifest.is_some(), "unexpected errors: {:?}", r.diagnostics);
        r.manifest.unwrap()
    }

    #[test]
    fn minimal_mv3() {
        let m = parse_ok(r#"{"manifest_version":3,"name":"X","version":"1.0"}"#);
        assert_eq!(m.name, "X");
        assert!(m.is_mv3());
        assert_eq!(m.background.kind, BackgroundKind::None);
        assert_eq!(m.csp_extension_pages, default_extension_csp());
        assert_eq!(m.dialect, Dialect::Both);
    }

    #[test]
    fn comments_bom_and_strings_containing_slashes() {
        let text = "\u{feff}{\n// line comment\n\"manifest_version\": 3, /* block\ncomment */ \"name\": \"http://a//b /* not a comment */\", \"version\": \"1\"}";
        let m = parse_ok(text);
        assert_eq!(m.name, "http://a//b /* not a comment */");
    }

    #[test]
    fn fatal_errors() {
        for bad in [
            "",
            "[]",
            r#"{"name":"x","version":"1"}"#,
            r#"{"manifest_version":1,"name":"x","version":"1"}"#,
            r#"{"manifest_version":3,"version":"1"}"#,
            r#"{"manifest_version":3,"name":"x"}"#,
            r#"{"manifest_version":3,"name":"","version":"1"}"#,
            r#"{"manifest_version":3,"name":5,"version":"1"}"#,
        ] {
            let r = parse_manifest(bad);
            assert!(r.manifest.is_none() && r.has_errors(), "{bad:?} should fail");
        }
    }

    #[test]
    fn permissions_are_split_in_both_versions() {
        let m = parse_ok(
            r#"{"manifest_version":2,"name":"x","version":"1","permissions":["storage","tabs","https://*.a.com/*","<all_urls>"],"optional_permissions":["history","*://b.com/*"]}"#,
        );
        assert_eq!(m.permissions, ["storage", "tabs"]);
        assert_eq!(m.host_permissions, ["https://*.a.com/*", "<all_urls>"]);
        assert_eq!(m.optional_permissions, ["history"]);
        assert_eq!(m.optional_host_permissions, ["*://b.com/*"]);

        let m = parse_ok(
            r#"{"manifest_version":3,"name":"x","version":"1","permissions":["storage"],"host_permissions":["*://a.com/*"],"optional_host_permissions":["<all_urls>"]}"#,
        );
        assert_eq!(m.permissions, ["storage"]);
        assert_eq!(m.host_permissions, ["*://a.com/*"]);
        assert_eq!(m.optional_host_permissions, ["<all_urls>"]);
    }

    #[test]
    fn content_scripts() {
        let r = parse_manifest(
            r#"{"manifest_version":3,"name":"x","version":"1","content_scripts":[
                {"matches":["https://*/*"],"js":["a.js"],"run_at":"document_start","all_frames":true,"world":"MAIN"},
                {"matches":["not a pattern"],"js":["b.js"]},
                {"js":["c.js"]}
            ]}"#,
        );
        // Invalid entries are errors: the manifest is rejected, matching Chrome.
        assert!(r.manifest.is_none());
        assert_eq!(r.errors().count(), 2);

        let m = parse_ok(
            r#"{"manifest_version":3,"name":"x","version":"1","content_scripts":[
                {"matches":["https://*/*"],"exclude_matches":["https://a.com/*"],"js":["a.js"],"css":["a.css"],"run_at":"document_start","all_frames":true,"world":"MAIN"}]}"#,
        );
        let cs = &m.content_scripts[0];
        assert_eq!(cs.run_at, RunAt::DocumentStart);
        assert_eq!(cs.world, ScriptWorld::Main);
        assert!(cs.all_frames);
        assert_eq!(cs.css, ["a.css"]);
    }

    #[test]
    fn background_kinds() {
        let sw = parse_ok(r#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"sw.js","type":"module"}}"#);
        assert_eq!(sw.background.kind, BackgroundKind::ServiceWorker);
        assert!(sw.background.is_module);
        assert_eq!(sw.dialect, Dialect::Chrome);

        let both =
            parse_ok(r#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"sw.js","scripts":["bg.js"]}}"#);
        assert!(both.background.has_scripts_and_service_worker);
        assert_eq!(both.dialect, Dialect::Both);

        let mv2 =
            parse_ok(r#"{"manifest_version":2,"name":"x","version":"1","background":{"scripts":["a.js","b.js"],"persistent":false}}"#);
        assert_eq!(mv2.background.kind, BackgroundKind::Scripts);
        assert!(!mv2.background.persistent);

        let page = parse_ok(r#"{"manifest_version":2,"name":"x","version":"1","background":{"page":"bg.html"}}"#);
        assert_eq!(page.background.kind, BackgroundKind::Page);
        assert!(page.background.persistent);
    }

    #[test]
    fn firefox_detection_and_gecko_id() {
        let m = parse_ok(
            r#"{"manifest_version":2,"name":"x","version":"1","browser_specific_settings":{"gecko":{"id":"a@b.c","strict_min_version":"109.0"}}}"#,
        );
        assert_eq!(m.dialect, Dialect::Firefox);
        assert_eq!(m.gecko_id, "a@b.c");
        assert_eq!(m.gecko_strict_min_version, "109.0");
        let legacy = parse_ok(r#"{"manifest_version":2,"name":"x","version":"1","applications":{"gecko":{"id":"old@x"}}}"#);
        assert_eq!(legacy.gecko_id, "old@x");
    }

    #[test]
    fn actions_and_icons() {
        let m = parse_ok(
            r#"{"manifest_version":3,"name":"x","version":"1","icons":{"48":"i48.png","16":"i16.png"},"action":{"default_title":"T","default_popup":"p.html","default_icon":{"19":"a.png"}}}"#,
        );
        assert_eq!(m.icons.0, [(16, "i16.png".to_string()), (48, "i48.png".to_string())]);
        let a = m.action.unwrap();
        assert_eq!(a.kind, ActionKind::Action);
        assert_eq!(a.default_popup, "p.html");

        let mv2 = parse_ok(r#"{"manifest_version":2,"name":"x","version":"1","browser_action":{"default_icon":"i.png"}}"#);
        assert_eq!(mv2.action.unwrap().default_icon.0, [(0, "i.png".to_string())]);
    }

    #[test]
    fn web_accessible_resources_forms() {
        let mv2 = parse_ok(r#"{"manifest_version":2,"name":"x","version":"1","web_accessible_resources":["a.png","b/*"]}"#);
        assert_eq!(mv2.web_accessible_resources[0].resources, ["a.png", "b/*"]);
        assert_eq!(mv2.web_accessible_resources[0].matches, ["<all_urls>"]);
        let mv3 = parse_ok(
            r#"{"manifest_version":3,"name":"x","version":"1","web_accessible_resources":[{"resources":["a.png"],"matches":["https://a.com/*"]}]}"#,
        );
        assert_eq!(mv3.web_accessible_resources[0].matches, ["https://a.com/*"]);
    }

    #[test]
    fn csp_forms() {
        let mv2 = parse_ok(
            r#"{"manifest_version":2,"name":"x","version":"1","content_security_policy":"script-src 'self' 'wasm-unsafe-eval'; object-src 'self'"}"#,
        );
        assert!(mv2.csp_extension_pages.contains("wasm-unsafe-eval"));
        let mv3 = parse_ok(
            r#"{"manifest_version":3,"name":"x","version":"1","content_security_policy":{"extension_pages":"script-src 'self'","sandbox":"sandbox allow-scripts"}}"#,
        );
        assert_eq!(mv3.csp_extension_pages, "script-src 'self'");
        assert_eq!(mv3.csp_sandbox, "sandbox allow-scripts");
    }

    #[test]
    fn options_commands_dnr() {
        let m = parse_ok(
            r#"{"manifest_version":3,"name":"x","version":"1",
                "options_ui":{"page":"o.html","open_in_tab":true},
                "commands":{"toggle":{"description":"Toggle","suggested_key":{"default":"Ctrl+Shift+Y"}},"_execute_action":{}},
                "declarative_net_request":{"rule_resources":[{"id":"r1","enabled":true,"path":"r1.json"}]}}"#,
        );
        assert_eq!(m.options_page, "o.html");
        assert!(m.options_open_in_tab);
        assert_eq!(m.commands.len(), 2);
        assert_eq!(m.commands[0].suggested_key, "Ctrl+Shift+Y");
        assert_eq!(m.declarative_net_request_rulesets, [RuleResource { id: "r1".into(), path: "r1.json".into(), enabled: true }]);
    }

    #[test]
    fn version_validation() {
        assert!(is_chrome_version("1"));
        assert!(is_chrome_version("1.2.3.4"));
        assert!(is_chrome_version("0.0.1"));
        assert!(!is_chrome_version("1.2.3.4.5"));
        assert!(!is_chrome_version("01.2"));
        assert!(!is_chrome_version("1.2.beta"));
        assert!(!is_chrome_version("70000"));
        assert!(!is_chrome_version(""));
        assert!(!is_chrome_version("1..2"));
        let r = parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1.0beta"}"#);
        assert!(r.manifest.is_some());
        assert_eq!(r.warnings().count(), 1);
    }

    #[test]
    fn wrong_typed_optional_fields_only_warn() {
        let r = parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1","description":5,"permissions":"storage","icons":3}"#);
        assert!(r.manifest.is_some());
        assert!(r.warnings().count() >= 2);
    }
}
