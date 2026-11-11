//! The `declarativeNetRequest` rule engine: parsing and validating rules, matching them against requests, and choosing the
//! action, as Chrome and Firefox do.
//!
//! This module knows nothing about the dispatcher or the browser. [`ExtensionRules`] holds one extension's rulesets
//! (static ones from the manifest, the dynamic and the session rules) and answers [`ExtensionRules::evaluate`] for a
//! [`Request`]; `crate::network` combines the answers of all extensions and applies them.
//!
//! Matching has to be fast with large lists (Requirement 10.5: under 1 ms with 100,000 rules). Each [`RuleSet`] indexes its
//! `urlFilter` rules by one 4-byte substring of the filter, the rarest one in the set; a request only tests the rules whose
//! substring occurs in its URL, plus the few rules that have no usable substring.
//!
//! <https://developer.chrome.com/docs/extensions/reference/api/declarativeNetRequest>

use crate::match_pattern::glob_match;
use regex::{Regex, RegexBuilder};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

pub const MAX_NUMBER_OF_DYNAMIC_RULES: usize = 30_000;
pub const MAX_NUMBER_OF_UNSAFE_DYNAMIC_RULES: usize = 5_000;
pub const MAX_NUMBER_OF_SESSION_RULES: usize = 5_000;
pub const MAX_NUMBER_OF_UNSAFE_SESSION_RULES: usize = 5_000;
pub const MAX_NUMBER_OF_REGEX_RULES: usize = 1_000;
pub const MAX_NUMBER_OF_STATIC_RULESETS: usize = 100;
pub const MAX_NUMBER_OF_ENABLED_STATIC_RULESETS: usize = 50;
pub const MAX_NUMBER_OF_DISABLED_STATIC_RULES: usize = 5_000;
pub const GUARANTEED_MINIMUM_STATIC_RULES: usize = 30_000;
/// Static rules beyond the guaranteed minimum come from a pool shared by all extensions (Chrome: 300,000).
pub const GLOBAL_STATIC_RULE_POOL: usize = 300_000;
pub const GETMATCHEDRULES_QUOTA_INTERVAL: i64 = 10;
pub const MAX_GETMATCHEDRULES_CALLS_PER_INTERVAL: usize = 20;
pub const DYNAMIC_RULESET_ID: &str = "_dynamic";
pub const SESSION_RULESET_ID: &str = "_session";

/// Matched rules are remembered this long for `getMatchedRules`, and at most this many.
const MATCHED_RULE_LIFETIME_MS: i64 = 5 * 60 * 1000;
const MAX_MATCHED_RULES: usize = 2_000;
/// Compiled size allowed for one `regexFilter`. Chrome's limit is 2 KB of RE2 program; the `regex` crate's programs are
/// larger for the same pattern, so this is set where the patterns Chrome accepts compile.
const REGEX_SIZE_LIMIT: usize = 256 * 1024;

// ---- Resource types ---------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceType {
    MainFrame,
    SubFrame,
    Stylesheet,
    Script,
    Image,
    Font,
    Object,
    XmlHttpRequest,
    Ping,
    CspReport,
    Media,
    WebSocket,
    WebTransport,
    WebBundle,
    Other,
}

impl ResourceType {
    pub const ALL: [ResourceType; 15] = [
        ResourceType::MainFrame,
        ResourceType::SubFrame,
        ResourceType::Stylesheet,
        ResourceType::Script,
        ResourceType::Image,
        ResourceType::Font,
        ResourceType::Object,
        ResourceType::XmlHttpRequest,
        ResourceType::Ping,
        ResourceType::CspReport,
        ResourceType::Media,
        ResourceType::WebSocket,
        ResourceType::WebTransport,
        ResourceType::WebBundle,
        ResourceType::Other,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ResourceType::MainFrame => "main_frame",
            ResourceType::SubFrame => "sub_frame",
            ResourceType::Stylesheet => "stylesheet",
            ResourceType::Script => "script",
            ResourceType::Image => "image",
            ResourceType::Font => "font",
            ResourceType::Object => "object",
            ResourceType::XmlHttpRequest => "xmlhttprequest",
            ResourceType::Ping => "ping",
            ResourceType::CspReport => "csp_report",
            ResourceType::Media => "media",
            ResourceType::WebSocket => "websocket",
            ResourceType::WebTransport => "webtransport",
            ResourceType::WebBundle => "webbundle",
            ResourceType::Other => "other",
        }
    }

    /// Accepts Chrome's names and the extra ones Firefox's `webRequest` uses (`beacon`, `imageset`, ...).
    pub fn parse(s: &str) -> Option<ResourceType> {
        Some(match s {
            "main_frame" => ResourceType::MainFrame,
            "sub_frame" => ResourceType::SubFrame,
            "stylesheet" => ResourceType::Stylesheet,
            "script" => ResourceType::Script,
            "image" | "imageset" => ResourceType::Image,
            "font" => ResourceType::Font,
            "object" | "object_subrequest" => ResourceType::Object,
            "xmlhttprequest" => ResourceType::XmlHttpRequest,
            "ping" | "beacon" => ResourceType::Ping,
            "csp_report" => ResourceType::CspReport,
            "media" => ResourceType::Media,
            "websocket" => ResourceType::WebSocket,
            "webtransport" => ResourceType::WebTransport,
            "webbundle" => ResourceType::WebBundle,
            "other" | "speculative" | "web_manifest" | "xml_dtd" | "xslt" | "json" => ResourceType::Other,
            _ => return None,
        })
    }

    fn bit(self) -> u16 {
        1 << (self as u16)
    }

    pub fn is_frame(self) -> bool {
        matches!(self, ResourceType::MainFrame | ResourceType::SubFrame)
    }
}

const ALL_TYPES: u16 = (1 << 15) - 1;

const METHODS: [&str; 9] = ["connect", "delete", "get", "head", "options", "patch", "post", "put", "other"];

fn method_bit(method: &str) -> u16 {
    let m = method.to_ascii_lowercase();
    let i = METHODS.iter().position(|x| *x == m).unwrap_or(METHODS.len() - 1);
    1 << i
}

// ---- URLs -------------------------------------------------------------------------------------------------------------

/// The pieces of a URL that rules look at or rewrite. The browser passes URLs from its own parser, so this only splits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UrlParts {
    pub scheme: String,
    pub username: String,
    pub password: String,
    pub host: String,
    pub port: String,
    pub path: String,
    pub query: Option<String>,
    pub fragment: Option<String>,
}

impl UrlParts {
    pub fn parse(url: &str) -> Option<UrlParts> {
        let (scheme, rest) = url.split_once(':')?;
        if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
            return None;
        }
        let rest = rest.strip_prefix("//")?;
        let mut out = UrlParts { scheme: scheme.to_ascii_lowercase(), ..Default::default() };
        let (rest, fragment) = match rest.split_once('#') {
            Some((r, f)) => (r, Some(f.to_string())),
            None => (rest, None),
        };
        out.fragment = fragment;
        let (rest, query) = match rest.split_once('?') {
            Some((r, q)) => (r, Some(q.to_string())),
            None => (rest, None),
        };
        out.query = query;
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, "/"),
        };
        out.path = path.to_string();
        let host_port = match authority.rsplit_once('@') {
            Some((user, h)) => {
                match user.split_once(':') {
                    Some((u, p)) => {
                        out.username = u.to_string();
                        out.password = p.to_string();
                    }
                    None => out.username = user.to_string(),
                }
                h
            }
            None => authority,
        };
        let (host, port) = if host_port.starts_with('[') {
            let close = host_port.find(']')?;
            (&host_port[..=close], host_port[close + 1..].strip_prefix(':').unwrap_or(""))
        } else {
            host_port.rsplit_once(':').unwrap_or((host_port, ""))
        };
        if !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out.host = host.to_ascii_lowercase();
        out.port = port.to_string();
        Some(out)
    }

    pub fn serialize(&self) -> String {
        let mut s = format!("{}://", self.scheme);
        if !self.username.is_empty() || !self.password.is_empty() {
            s.push_str(&self.username);
            if !self.password.is_empty() {
                s.push(':');
                s.push_str(&self.password);
            }
            s.push('@');
        }
        s.push_str(&self.host);
        if !self.port.is_empty() {
            s.push(':');
            s.push_str(&self.port);
        }
        s.push_str(if self.path.is_empty() { "/" } else { &self.path });
        if let Some(q) = &self.query {
            s.push('?');
            s.push_str(q);
        }
        if let Some(f) = &self.fragment {
            s.push('#');
            s.push_str(f);
        }
        s
    }
}

/// The registrable domain ("site") of a host: `a.b.example.co.uk` → `example.co.uk`. IP addresses and hosts without a
/// known suffix are their own site.
pub fn registrable_domain(host: &str) -> &str {
    if host.starts_with('[') || host.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return host;
    }
    match psl::domain(host.as_bytes()) {
        Some(d) => std::str::from_utf8(d.as_bytes()).unwrap_or(host),
        None => host,
    }
}

fn host_of(url: &str) -> Option<String> {
    crate::match_pattern::split_url(url).map(|u| u.host).filter(|h| !h.is_empty())
}

/// Whether `host` is `domain` or one of its subdomains.
fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain || (host.len() > domain.len() && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.')
}

fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'.' | b'_' | b'~' | b'*' | b'/' | b':' | b'@' | b'!' | b'$' | b'\'' | b'(' | b')' | b',')
        {
            out.push(b as char);
        } else if b == b' ' {
            out.push_str("%20");
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---- A request --------------------------------------------------------------------------------------------------------

/// What rules are matched against.
#[derive(Debug, Clone)]
pub struct Request {
    pub url: String,
    lower_url: String,
    /// Byte range of the host inside `url`.
    host_range: (usize, usize),
    pub host: String,
    pub scheme: String,
    method_bit: u16,
    pub resource_type: ResourceType,
    /// Host of the origin that started the request; `None` for navigations the user started and for opaque origins.
    pub initiator_host: Option<String>,
    /// Host of the tab's top document, for `topDomains`.
    pub top_host: Option<String>,
    pub tab_id: i64,
    pub third_party: bool,
    /// Present once the response headers arrived: names lowercased.
    pub response_headers: Option<Vec<(String, String)>>,
}

impl Request {
    pub fn new(url: &str, method: &str, resource_type: ResourceType, initiator: Option<&str>, tab_id: i64) -> Option<Request> {
        let scheme_end = url.find(':')?;
        let scheme = url[..scheme_end].to_ascii_lowercase();
        let (host_range, host) = match url[scheme_end + 1..].strip_prefix("//") {
            Some(rest) => {
                let base = scheme_end + 3;
                let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
                let authority = &rest[..authority_end];
                let host_start = authority.rfind('@').map_or(0, |i| i + 1);
                let host_port = &authority[host_start..];
                let host_len = if host_port.starts_with('[') {
                    host_port.find(']').map_or(host_port.len(), |i| i + 1)
                } else {
                    host_port.rfind(':').unwrap_or(host_port.len())
                };
                ((base + host_start, base + host_start + host_len), host_port[..host_len].to_ascii_lowercase())
            }
            None => ((0, 0), String::new()),
        };
        let initiator_host = initiator.and_then(host_of);
        let third_party = match &initiator_host {
            Some(i) => registrable_domain(i) != registrable_domain(&host),
            None => false,
        };
        Some(Request {
            lower_url: url.to_ascii_lowercase(),
            url: url.to_string(),
            host_range,
            host,
            scheme,
            method_bit: method_bit(method),
            resource_type,
            initiator_host,
            top_host: None,
            tab_id,
            third_party,
            response_headers: None,
        })
    }

    pub fn with_response_headers(mut self, headers: Vec<(String, String)>) -> Request {
        self.response_headers = Some(headers.into_iter().map(|(n, v)| (n.to_ascii_lowercase(), v)).collect());
        self
    }
}

// ---- urlFilter --------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Anchor {
    None,
    Start,
    /// `||`: the start of the host, or right after a dot inside it.
    Domain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Lit(Vec<u8>),
    /// `*`
    Star,
    /// `^`: one character that is not a letter, a digit or one of `_-.%`; also matches the end of the URL.
    Sep,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlFilter {
    left: Anchor,
    right: bool,
    toks: Vec<Tok>,
    case_sensitive: bool,
}

fn is_separator(b: u8) -> bool {
    !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'%'))
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(from);
    }
    if haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let last_start = haystack.len() - needle.len();
    let mut i = from;
    while i <= last_start {
        if haystack[i] == first && &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

impl UrlFilter {
    pub fn parse(pattern: &str, case_sensitive: bool) -> Result<UrlFilter, &'static str> {
        if pattern.is_empty() {
            return Err("urlFilter must not be empty");
        }
        if !pattern.is_ascii() {
            return Err("urlFilter must only contain ASCII characters");
        }
        if pattern.starts_with("||*") {
            return Err("urlFilter must not start with ||*");
        }
        let (left, rest) = if let Some(r) = pattern.strip_prefix("||") {
            (Anchor::Domain, r)
        } else if let Some(r) = pattern.strip_prefix('|') {
            (Anchor::Start, r)
        } else {
            (Anchor::None, pattern)
        };
        let (right, rest) = match rest.strip_suffix('|') {
            Some(r) => (true, r),
            None => (false, rest),
        };
        let mut toks: Vec<Tok> = vec![];
        for b in rest.bytes() {
            let b = if case_sensitive { b } else { b.to_ascii_lowercase() };
            match b {
                b'*' => {
                    if toks.last() != Some(&Tok::Star) {
                        toks.push(Tok::Star);
                    }
                }
                b'^' => toks.push(Tok::Sep),
                _ => match toks.last_mut() {
                    Some(Tok::Lit(l)) => l.push(b),
                    _ => toks.push(Tok::Lit(vec![b])),
                },
            }
        }
        Ok(UrlFilter { left, right, toks, case_sensitive })
    }

    pub fn matches(&self, req: &Request) -> bool {
        let text = if self.case_sensitive { req.url.as_bytes() } else { req.lower_url.as_bytes() };
        match self.left {
            Anchor::Start => self.match_at(&self.toks, text, 0),
            Anchor::None => self.match_floating(&self.toks, text, 0),
            Anchor::Domain => {
                let (start, end) = req.host_range;
                (start..end).any(|i| (i == start || text[i - 1] == b'.') && self.match_at(&self.toks, text, i))
            }
        }
    }

    /// Matches `toks` somewhere at or after `from` (as if a `*` preceded them).
    fn match_floating(&self, toks: &[Tok], text: &[u8], from: usize) -> bool {
        match toks.first() {
            // Whatever is left can be skipped, so a right anchor is satisfied too.
            None => true,
            Some(Tok::Star) => self.match_floating(&toks[1..], text, from),
            Some(Tok::Lit(l)) => {
                let mut at = from;
                while let Some(i) = find(text, l, at) {
                    if self.match_at(&toks[1..], text, i + l.len()) {
                        return true;
                    }
                    at = i + 1;
                }
                false
            }
            Some(Tok::Sep) => (from..=text.len()).any(|i| self.match_at(toks, text, i)),
        }
    }

    /// Matches `toks` starting exactly at `pos`.
    fn match_at(&self, toks: &[Tok], text: &[u8], pos: usize) -> bool {
        match toks.first() {
            None => !self.right || pos == text.len(),
            Some(Tok::Lit(l)) => text[pos..].starts_with(l) && self.match_at(&toks[1..], text, pos + l.len()),
            Some(Tok::Sep) => {
                if pos < text.len() {
                    is_separator(text[pos]) && self.match_at(&toks[1..], text, pos + 1)
                } else {
                    // The end of the URL counts as a separator.
                    self.match_at(&toks[1..], text, pos)
                }
            }
            Some(Tok::Star) => self.match_floating(&toks[1..], text, pos),
        }
    }

    /// The literal pieces, lowercased, for the index.
    fn literals(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.toks.iter().filter_map(|t| match t {
            Tok::Lit(l) => Some(l.to_ascii_lowercase()),
            _ => None,
        })
    }
}

// ---- Rules ------------------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionType {
    Block,
    Allow,
    AllowAllRequests,
    UpgradeScheme,
    Redirect,
    ModifyHeaders,
}

impl ActionType {
    fn parse(s: &str) -> Option<ActionType> {
        Some(match s {
            "block" => ActionType::Block,
            "allow" => ActionType::Allow,
            "allowAllRequests" => ActionType::AllowAllRequests,
            "upgradeScheme" => ActionType::UpgradeScheme,
            "redirect" => ActionType::Redirect,
            "modifyHeaders" => ActionType::ModifyHeaders,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ActionType::Block => "block",
            ActionType::Allow => "allow",
            ActionType::AllowAllRequests => "allowAllRequests",
            ActionType::UpgradeScheme => "upgradeScheme",
            ActionType::Redirect => "redirect",
            ActionType::ModifyHeaders => "modifyHeaders",
        }
    }

    /// Among rules of equal priority: allow, allowAllRequests, block, upgradeScheme, redirect (smaller wins).
    fn rank(self) -> u8 {
        match self {
            ActionType::Allow => 0,
            ActionType::AllowAllRequests => 1,
            ActionType::Block => 2,
            ActionType::UpgradeScheme => 3,
            ActionType::Redirect => 4,
            ActionType::ModifyHeaders => 5,
        }
    }

    /// Rules that can send a request elsewhere or change its headers count against the "unsafe" limits.
    fn is_unsafe(self) -> bool {
        matches!(self, ActionType::Redirect | ActionType::ModifyHeaders)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderOperation {
    Append,
    Set,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderOp {
    /// Lowercase.
    pub header: String,
    pub operation: HeaderOperation,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct QueryTransform {
    remove: Vec<String>,
    add_or_replace: Vec<(String, String, bool)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct UrlTransform {
    scheme: Option<String>,
    username: Option<String>,
    password: Option<String>,
    host: Option<String>,
    port: Option<String>,
    path: Option<String>,
    query: Option<String>,
    query_transform: Option<QueryTransform>,
    fragment: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Redirect {
    Url(String),
    ExtensionPath(String),
    Transform(Box<UrlTransform>),
    RegexSubstitution(String),
}

#[derive(Debug, Clone)]
struct HeaderCondition {
    header: String,
    values: Vec<String>,
    excluded_values: Vec<String>,
}

impl HeaderCondition {
    fn matches(&self, headers: &[(String, String)]) -> bool {
        let mut present = false;
        let mut value_ok = self.values.is_empty();
        for (_, value) in headers.iter().filter(|(n, _)| *n == self.header) {
            present = true;
            if self.excluded_values.iter().any(|p| glob_match(p, value, true)) {
                return false;
            }
            if self.values.iter().any(|p| glob_match(p, value, true)) {
                value_ok = true;
            }
        }
        present && value_ok
    }
}

#[derive(Debug, Clone, Default)]
struct Condition {
    url_filter: Option<UrlFilter>,
    regex: Option<Regex>,
    initiator_domains: Option<Vec<String>>,
    excluded_initiator_domains: Vec<String>,
    request_domains: Option<Vec<String>>,
    excluded_request_domains: Vec<String>,
    top_domains: Option<Vec<String>>,
    excluded_top_domains: Vec<String>,
    /// Bit set of [`ResourceType`]s the rule applies to.
    types: u16,
    /// Bit set of request methods; all bits when the rule does not restrict them.
    methods: u16,
    third_party: Option<bool>,
    tab_ids: Option<Vec<i64>>,
    excluded_tab_ids: Vec<i64>,
    response_headers: Vec<HeaderCondition>,
    excluded_response_headers: Vec<HeaderCondition>,
}

impl Condition {
    fn needs_response(&self) -> bool {
        !self.response_headers.is_empty() || !self.excluded_response_headers.is_empty()
    }

    fn matches(&self, req: &Request) -> bool {
        if self.types & req.resource_type.bit() == 0 || self.methods & req.method_bit == 0 {
            return false;
        }
        if let Some(f) = &self.url_filter
            && !f.matches(req)
        {
            return false;
        }
        if let Some(third) = self.third_party
            && third != req.third_party
        {
            return false;
        }
        let in_list = |list: &[String], host: Option<&str>| host.is_some_and(|h| list.iter().any(|d| domain_matches(h, d)));
        if let Some(list) = &self.request_domains
            && !in_list(list, Some(&req.host))
        {
            return false;
        }
        if in_list(&self.excluded_request_domains, Some(&req.host)) {
            return false;
        }
        if let Some(list) = &self.initiator_domains
            && !in_list(list, req.initiator_host.as_deref())
        {
            return false;
        }
        if in_list(&self.excluded_initiator_domains, req.initiator_host.as_deref()) {
            return false;
        }
        if let Some(list) = &self.top_domains
            && !in_list(list, req.top_host.as_deref())
        {
            return false;
        }
        if in_list(&self.excluded_top_domains, req.top_host.as_deref()) {
            return false;
        }
        if let Some(ids) = &self.tab_ids
            && !ids.contains(&req.tab_id)
        {
            return false;
        }
        if self.excluded_tab_ids.contains(&req.tab_id) {
            return false;
        }
        if let Some(r) = &self.regex
            && !r.is_match(&req.url)
        {
            return false;
        }
        if self.needs_response() {
            let Some(headers) = &req.response_headers else { return false };
            if !self.response_headers.is_empty() && !self.response_headers.iter().any(|c| c.matches(headers)) {
                return false;
            }
            if self.excluded_response_headers.iter().any(|c| c.matches(headers)) {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub id: i64,
    pub priority: i64,
    pub action: ActionType,
    redirect: Option<Redirect>,
    pub request_headers: Vec<HeaderOp>,
    pub response_headers: Vec<HeaderOp>,
    condition: Condition,
    /// The rule as the extension wrote it, with `priority` filled in; what `getDynamicRules` returns.
    pub source: Value,
}

/// Request headers that `append` may be used on (Chrome's list).
const APPENDABLE_REQUEST_HEADERS: [&str; 21] = [
    "accept",
    "accept-encoding",
    "accept-language",
    "access-control-request-headers",
    "cache-control",
    "connection",
    "content-language",
    "cookie",
    "forwarded",
    "if-match",
    "if-none-match",
    "keep-alive",
    "range",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "via",
    "want-digest",
    "x-forwarded-for",
];

fn bad(id: i64, key: &str) -> String {
    format!("Rule with id {id} specifies an incorrect value for the \"{key}\" key.")
}

fn string_list(id: i64, cond: &Map<String, Value>, key: &str, lower: bool) -> Result<Option<Vec<String>>, String> {
    match cond.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                let s = v.as_str().ok_or_else(|| bad(id, &format!("condition.{key}")))?;
                if !s.is_ascii() {
                    return Err(format!("Rule with id {id} cannot have non-ascii characters as part of \"{key}\" key."));
                }
                out.push(if lower { s.to_ascii_lowercase() } else { s.to_string() });
            }
            Ok(Some(out))
        }
        Some(_) => Err(bad(id, &format!("condition.{key}"))),
    }
}

/// A list that restricts the rule: an empty one would match nothing, which Chrome refuses.
fn including_list(id: i64, cond: &Map<String, Value>, key: &str) -> Result<Option<Vec<String>>, String> {
    let list = string_list(id, cond, key, true)?;
    if list.as_ref().is_some_and(Vec::is_empty) {
        return Err(format!("Rule with id {id} cannot have an empty list as the value for the \"{key}\" key."));
    }
    Ok(list)
}

fn int_list(id: i64, cond: &Map<String, Value>, key: &str) -> Result<Option<Vec<i64>>, String> {
    match cond.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) => {
            a.iter().map(|v| v.as_i64().ok_or_else(|| bad(id, &format!("condition.{key}")))).collect::<Result<_, _>>().map(Some)
        }
        Some(_) => Err(bad(id, &format!("condition.{key}"))),
    }
}

fn header_conditions(id: i64, cond: &Map<String, Value>, key: &str) -> Result<Vec<HeaderCondition>, String> {
    let Some(v) = cond.get(key).filter(|v| !v.is_null()) else { return Ok(vec![]) };
    let list = v.as_array().ok_or_else(|| bad(id, &format!("condition.{key}")))?;
    if list.is_empty() {
        return Err(format!("Rule with id {id} cannot have an empty list as the value for the \"{key}\" key."));
    }
    let strings = |o: &Value, k: &str| -> Result<Vec<String>, String> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(vec![]),
            Some(Value::Array(a)) => {
                a.iter().map(|v| v.as_str().map(String::from).ok_or_else(|| bad(id, &format!("condition.{key}")))).collect()
            }
            Some(_) => Err(bad(id, &format!("condition.{key}"))),
        }
    };
    list.iter()
        .map(|o| {
            let header = o
                .get("header")
                .and_then(Value::as_str)
                .filter(|h| is_header_name(h))
                .ok_or_else(|| bad(id, &format!("condition.{key}")))?;
            Ok(HeaderCondition {
                header: header.to_ascii_lowercase(),
                values: strings(o, "values")?,
                excluded_values: strings(o, "excludedValues")?,
            })
        })
        .collect()
}

fn is_header_name(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn header_ops(id: i64, action: &Map<String, Value>, key: &str, request: bool) -> Result<Vec<HeaderOp>, String> {
    let Some(v) = action.get(key).filter(|v| !v.is_null()) else { return Ok(vec![]) };
    let list = v.as_array().ok_or_else(|| bad(id, &format!("action.{key}")))?;
    if list.is_empty() {
        return Err(format!("Rule with id {id} cannot have an empty list as the value for the \"{key}\" key."));
    }
    let mut out = vec![];
    for o in list {
        let header = o.get("header").and_then(Value::as_str).unwrap_or("");
        if !is_header_name(header) {
            return Err(format!("Rule with id {id} must specify a valid header name to be modified."));
        }
        let header = header.to_ascii_lowercase();
        let operation = match o.get("operation").and_then(Value::as_str) {
            Some("append") => HeaderOperation::Append,
            Some("set") => HeaderOperation::Set,
            Some("remove") => HeaderOperation::Remove,
            _ => return Err(bad(id, &format!("action.{key}"))),
        };
        let value = o.get("value").and_then(Value::as_str);
        match (operation, value) {
            (HeaderOperation::Remove, Some(_)) => {
                return Err(format!("Rule with id {id} must not provide a header value for a header to be removed."));
            }
            (HeaderOperation::Append | HeaderOperation::Set, None) => {
                return Err(format!("Rule with id {id} must provide a value for a header to be appended/set."));
            }
            _ => {}
        }
        if request && operation == HeaderOperation::Append && !APPENDABLE_REQUEST_HEADERS.contains(&header.as_str()) {
            return Err(format!(
                "Rule with id {id} must not specify a request header to be appended which does not support multiple values."
            ));
        }
        out.push(HeaderOp { header, operation, value: value.unwrap_or("").to_string() });
    }
    Ok(out)
}

fn parse_redirect(id: i64, action: &Map<String, Value>, has_regex: bool) -> Result<Redirect, String> {
    let r = action
        .get("redirect")
        .and_then(Value::as_object)
        .ok_or_else(|| format!("Rule with id {id} does not specify the value for \"action.redirect\" key."))?;
    let text = |k: &str| r.get(k).and_then(Value::as_str);
    if let Some(path) = text("extensionPath") {
        if !path.starts_with('/') {
            return Err(bad(id, "action.redirect.extensionPath"));
        }
        return Ok(Redirect::ExtensionPath(path.to_string()));
    }
    if let Some(t) = r.get("transform").and_then(Value::as_object) {
        let field = |k: &str| t.get(k).and_then(Value::as_str).map(String::from);
        let mut out = UrlTransform {
            scheme: field("scheme"),
            username: field("username"),
            password: field("password"),
            host: field("host"),
            port: field("port"),
            path: field("path"),
            query: field("query"),
            fragment: field("fragment"),
            query_transform: None,
        };
        if let Some(s) = &out.scheme
            && !matches!(s.as_str(), "http" | "https" | "ftp" | "chrome-extension" | "moz-extension")
        {
            return Err(bad(id, "action.redirect.transform.scheme"));
        }
        if out.port.as_ref().is_some_and(|p| !p.bytes().all(|b| b.is_ascii_digit())) {
            return Err(bad(id, "action.redirect.transform.port"));
        }
        if out.query.as_ref().is_some_and(|q| !q.is_empty() && !q.starts_with('?')) {
            return Err(bad(id, "action.redirect.transform.query"));
        }
        if out.fragment.as_ref().is_some_and(|f| !f.is_empty() && !f.starts_with('#')) {
            return Err(bad(id, "action.redirect.transform.fragment"));
        }
        if let Some(q) = t.get("queryTransform").and_then(Value::as_object) {
            if out.query.is_some() {
                return Err(format!("Rule with id {id} cannot specify both \"query\" and \"queryTransform\" keys."));
            }
            let mut qt = QueryTransform::default();
            for v in q.get("removeParams").and_then(Value::as_array).into_iter().flatten() {
                qt.remove.push(v.as_str().ok_or_else(|| bad(id, "action.redirect.transform.queryTransform"))?.to_string());
            }
            for v in q.get("addOrReplaceParams").and_then(Value::as_array).into_iter().flatten() {
                let key = v.get("key").and_then(Value::as_str).ok_or_else(|| bad(id, "action.redirect.transform.queryTransform"))?;
                let value = v.get("value").and_then(Value::as_str).ok_or_else(|| bad(id, "action.redirect.transform.queryTransform"))?;
                qt.add_or_replace.push((
                    key.to_string(),
                    value.to_string(),
                    v.get("replaceOnly").and_then(Value::as_bool).unwrap_or(false),
                ));
            }
            if qt.remove.is_empty() && qt.add_or_replace.is_empty() {
                return Err(bad(id, "action.redirect.transform.queryTransform"));
            }
            out.query_transform = Some(qt);
        }
        return Ok(Redirect::Transform(Box::new(out)));
    }
    if let Some(url) = text("url") {
        let scheme = url.split_once(':').map(|(s, _)| s.to_ascii_lowercase()).unwrap_or_default();
        if scheme == "javascript" || UrlParts::parse(url).is_none() && scheme != "data" {
            return Err(bad(id, "action.redirect.url"));
        }
        return Ok(Redirect::Url(url.to_string()));
    }
    if let Some(sub) = text("regexSubstitution") {
        if !has_regex {
            return Err(format!(
                "Rule with id {id} specifies an incorrect value for the \"action.redirect.regexSubstitution\" key: it needs \"regexFilter\"."
            ));
        }
        return Ok(Redirect::RegexSubstitution(sub.to_string()));
    }
    Err(format!("Rule with id {id} does not specify the value for \"action.redirect\" key."))
}

/// Why a regular expression cannot be used, as `isRegexSupported` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexProblem {
    SyntaxError,
    MemoryLimitExceeded,
}

pub fn compile_regex(pattern: &str, case_sensitive: bool) -> Result<Regex, RegexProblem> {
    if pattern.is_empty() || !pattern.is_ascii() {
        return Err(RegexProblem::SyntaxError);
    }
    RegexBuilder::new(pattern).case_insensitive(!case_sensitive).size_limit(REGEX_SIZE_LIMIT).build().map_err(|e| match e {
        regex::Error::CompiledTooBig(_) => RegexProblem::MemoryLimitExceeded,
        _ => RegexProblem::SyntaxError,
    })
}

/// RE2's `\1` substitution syntax, as `regex` wants it (`${1}`); `\\` is a literal backslash.
fn substitution_template(sub: &str) -> String {
    let mut out = String::new();
    let mut chars = sub.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.peek() {
                Some(d) if d.is_ascii_digit() => {
                    out.push_str(&format!("${{{d}}}"));
                    chars.next();
                }
                Some('\\') => {
                    out.push('\\');
                    chars.next();
                }
                _ => out.push('\\'),
            },
            '$' => out.push_str("$$"),
            c => out.push(c),
        }
    }
    out
}

impl Rule {
    /// Parses and validates one rule. `session` allows the conditions only session rules may use (`tabIds`).
    pub fn parse(v: &Value, session: bool) -> Result<Rule, String> {
        let obj = v.as_object().ok_or("A rule must be an object.")?;
        let id =
            obj.get("id").and_then(Value::as_i64).filter(|i| *i >= 1).ok_or("Rule with an invalid id: \"id\" must be an integer >= 1.")?;
        let priority = match obj.get("priority") {
            None | Some(Value::Null) => 1,
            Some(p) => p
                .as_i64()
                .filter(|p| *p >= 1)
                .ok_or_else(|| format!("Rule with id {id} has an invalid value for \"priority\" key: it must be >= 1."))?,
        };
        let action = obj.get("action").and_then(Value::as_object).ok_or_else(|| bad(id, "action"))?;
        let kind = action.get("type").and_then(Value::as_str).and_then(ActionType::parse).ok_or_else(|| bad(id, "action.type"))?;
        let empty = Map::new();
        let cond = match obj.get("condition") {
            Some(Value::Object(c)) => c,
            None | Some(Value::Null) => &empty,
            Some(_) => return Err(bad(id, "condition")),
        };

        let case_sensitive = cond.get("isUrlFilterCaseSensitive").and_then(Value::as_bool).unwrap_or(false);
        let url_filter = match cond.get("urlFilter") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(UrlFilter::parse(s, case_sensitive).map_err(|e| format!("Rule with id {id}: {e}."))?),
            Some(_) => return Err(bad(id, "condition.urlFilter")),
        };
        let regex = match cond.get("regexFilter") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => {
                if url_filter.is_some() {
                    return Err(format!("Rule with id {id} cannot specify both \"urlFilter\" and \"regexFilter\" keys."));
                }
                Some(compile_regex(s, case_sensitive).map_err(|p| match p {
                    RegexProblem::SyntaxError => format!("Rule with id {id} specifies an incorrect value for the \"regexFilter\" key."),
                    RegexProblem::MemoryLimitExceeded => {
                        format!("Rule with id {id} specified a more complex regex than allowed as part of the \"regexFilter\" key.")
                    }
                })?)
            }
            Some(_) => return Err(bad(id, "condition.regexFilter")),
        };

        // `domains` / `excludedDomains` are the old names of the initiator lists.
        let initiator_domains = match including_list(id, cond, "initiatorDomains")? {
            Some(l) => Some(l),
            None => including_list(id, cond, "domains")?,
        };
        let mut excluded_initiator_domains = string_list(id, cond, "excludedInitiatorDomains", true)?.unwrap_or_default();
        excluded_initiator_domains.extend(string_list(id, cond, "excludedDomains", true)?.unwrap_or_default());

        let type_bits = |key: &str| -> Result<Option<u16>, String> {
            match string_list(id, cond, key, false)? {
                None => Ok(None),
                Some(list) => {
                    let mut bits = 0;
                    for t in &list {
                        bits |= ResourceType::parse(t).ok_or_else(|| bad(id, &format!("condition.{key}")))?.bit();
                    }
                    Ok(Some(bits))
                }
            }
        };
        let included = type_bits("resourceTypes")?;
        let excluded = type_bits("excludedResourceTypes")?;
        if included == Some(0) {
            return Err(format!("Rule with id {id} cannot have an empty list as the value for the \"resourceTypes\" key."));
        }
        if let (Some(i), Some(e)) = (included, excluded)
            && i & e != 0
        {
            return Err(format!(
                "Rule with id {id} should not have the same resource type in both \"resourceTypes\" and \"excludedResourceTypes\"."
            ));
        }
        let types = match (included, excluded) {
            (Some(i), _) => i,
            (None, Some(e)) => ALL_TYPES & !e,
            // By default a rule does not apply to top-level navigations.
            (None, None) => ALL_TYPES & !ResourceType::MainFrame.bit(),
        };
        if kind == ActionType::AllowAllRequests
            && (included.is_none() || types & !(ResourceType::MainFrame.bit() | ResourceType::SubFrame.bit()) != 0)
        {
            return Err(format!(
                "Rule with id {id} is an \"allowAllRequests\" rule and must specify \"resourceTypes\" with only \"main_frame\" and/or \"sub_frame\"."
            ));
        }

        let method_bits = |key: &str| -> Result<Option<u16>, String> {
            match string_list(id, cond, key, true)? {
                None => Ok(None),
                Some(list) => {
                    let mut bits = 0;
                    for m in &list {
                        if !METHODS.contains(&m.as_str()) {
                            return Err(bad(id, &format!("condition.{key}")));
                        }
                        bits |= method_bit(m);
                    }
                    Ok(Some(bits))
                }
            }
        };
        let all_methods = (1u16 << METHODS.len()) - 1;
        let methods = match (method_bits("requestMethods")?, method_bits("excludedRequestMethods")?) {
            (Some(0), _) => {
                return Err(format!("Rule with id {id} cannot have an empty list as the value for the \"requestMethods\" key."));
            }
            (Some(i), Some(e)) if i & e != 0 => return Err(bad(id, "condition.excludedRequestMethods")),
            (Some(i), _) => i,
            (None, Some(e)) => all_methods & !e,
            (None, None) => all_methods,
        };

        let third_party = match cond.get("domainType").and_then(Value::as_str) {
            None => None,
            Some("firstParty") => Some(false),
            Some("thirdParty") => Some(true),
            Some(_) => return Err(bad(id, "condition.domainType")),
        };
        let tab_ids = int_list(id, cond, "tabIds")?;
        let excluded_tab_ids = int_list(id, cond, "excludedTabIds")?.unwrap_or_default();
        if (tab_ids.is_some() || !excluded_tab_ids.is_empty()) && !session {
            return Err(format!("Rule with id {id} specifies \"tabIds\", which only session rules may use."));
        }
        if tab_ids.as_ref().is_some_and(Vec::is_empty) {
            return Err(format!("Rule with id {id} cannot have an empty list as the value for the \"tabIds\" key."));
        }

        let condition = Condition {
            url_filter,
            initiator_domains,
            excluded_initiator_domains,
            request_domains: including_list(id, cond, "requestDomains")?,
            excluded_request_domains: string_list(id, cond, "excludedRequestDomains", true)?.unwrap_or_default(),
            top_domains: including_list(id, cond, "topDomains")?,
            excluded_top_domains: string_list(id, cond, "excludedTopDomains", true)?.unwrap_or_default(),
            types,
            methods,
            third_party,
            tab_ids,
            excluded_tab_ids,
            response_headers: header_conditions(id, cond, "responseHeaders")?,
            excluded_response_headers: header_conditions(id, cond, "excludedResponseHeaders")?,
            regex,
        };

        let redirect = if kind == ActionType::Redirect { Some(parse_redirect(id, action, condition.regex.is_some())?) } else { None };
        let (request_headers, response_headers) = if kind == ActionType::ModifyHeaders {
            let request = header_ops(id, action, "requestHeaders", true)?;
            let response = header_ops(id, action, "responseHeaders", false)?;
            if request.is_empty() && response.is_empty() {
                return Err(format!("Rule with id {id} does not specify the value for \"requestHeaders\" or \"responseHeaders\" key."));
            }
            (request, response)
        } else {
            (vec![], vec![])
        };

        let mut source = v.clone();
        source["priority"] = json!(priority);
        Ok(Rule { id, priority, action: kind, redirect, request_headers, response_headers, condition, source })
    }

    pub fn matches(&self, req: &Request) -> bool {
        self.condition.matches(req)
    }

    fn is_regex(&self) -> bool {
        self.condition.regex.is_some()
    }

    /// Where this rule sends the request, or `None` when it would not change the URL.
    fn redirect_url(&self, req: &Request, base_url: &str) -> Option<String> {
        let target = match self.redirect.as_ref()? {
            Redirect::Url(u) => u.clone(),
            Redirect::ExtensionPath(p) => format!("{}{}", base_url.trim_end_matches('/'), p),
            Redirect::RegexSubstitution(sub) => {
                let regex = self.condition.regex.as_ref()?;
                regex.replace(&req.url, substitution_template(sub).as_str()).into_owned()
            }
            Redirect::Transform(t) => {
                let mut parts = UrlParts::parse(&req.url)?;
                if let Some(s) = &t.scheme {
                    parts.scheme = s.clone();
                }
                if let Some(u) = &t.username {
                    parts.username = u.clone();
                }
                if let Some(p) = &t.password {
                    parts.password = p.clone();
                }
                if let Some(h) = &t.host {
                    parts.host = h.to_ascii_lowercase();
                }
                if let Some(p) = &t.port {
                    parts.port = p.clone();
                }
                if let Some(p) = &t.path {
                    parts.path = if p.starts_with('/') { p.clone() } else { format!("/{p}") };
                }
                if let Some(q) = &t.query {
                    parts.query = q.strip_prefix('?').map(String::from);
                }
                if let Some(f) = &t.fragment {
                    parts.fragment = f.strip_prefix('#').map(String::from);
                }
                if let Some(qt) = &t.query_transform {
                    let mut pairs: Vec<(String, Option<String>)> = parts
                        .query
                        .as_deref()
                        .unwrap_or("")
                        .split('&')
                        .filter(|p| !p.is_empty())
                        .map(|p| match p.split_once('=') {
                            Some((k, v)) => (k.to_string(), Some(v.to_string())),
                            None => (p.to_string(), None),
                        })
                        .collect();
                    pairs.retain(|(k, _)| !qt.remove.contains(k));
                    for (key, value, replace_only) in &qt.add_or_replace {
                        let encoded_key = percent_encode_query(key);
                        let encoded = Some(percent_encode_query(value));
                        match pairs.iter_mut().find(|(k, _)| *k == *key || *k == encoded_key) {
                            Some(pair) => pair.1 = encoded,
                            None if !replace_only => pairs.push((encoded_key, encoded)),
                            None => {}
                        }
                    }
                    let query: Vec<String> = pairs
                        .into_iter()
                        .map(|(k, v)| match v {
                            Some(v) => format!("{k}={v}"),
                            None => k,
                        })
                        .collect();
                    parts.query = if query.is_empty() { None } else { Some(query.join("&")) };
                }
                // A port that is the scheme's default is not written.
                if matches!((parts.scheme.as_str(), parts.port.as_str()), ("http", "80") | ("https", "443") | ("ftp", "21")) {
                    parts.port.clear();
                }
                parts.serialize()
            }
        };
        let scheme = target.split_once(':').map(|(s, _)| s.to_ascii_lowercase())?;
        if scheme == "javascript" || target == req.url {
            return None;
        }
        Some(target)
    }
}

// ---- A set of rules with its index ------------------------------------------------------------------------------------

const GRAM: usize = 4;

fn gram_key(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Rules in the order they were given, and an index from a substring of each `urlFilter` to the rules that have it.
#[derive(Debug, Default)]
pub struct RuleSet {
    rules: Vec<Rule>,
    by_gram: HashMap<u32, Vec<u32>>,
    /// Rules that every request has to test: no `urlFilter`, or one without four literal characters in a row.
    unindexed: Vec<u32>,
    /// Bit set of the resource types any rule applies to, to skip whole sets early.
    types: u16,
}

impl RuleSet {
    pub fn new(rules: Vec<Rule>) -> RuleSet {
        let mut set = RuleSet { rules, ..Default::default() };
        set.reindex();
        set
    }

    fn reindex(&mut self) {
        self.by_gram.clear();
        self.unindexed.clear();
        self.types = 0;
        // First count how many rules contain each substring, then file each rule under its rarest one, which keeps the
        // lists short even when most filters share pieces such as ".com".
        let mut count: HashMap<u32, u32> = HashMap::new();
        let mut grams_of: Vec<Vec<u32>> = Vec::with_capacity(self.rules.len());
        for rule in &self.rules {
            let mut grams = vec![];
            if let Some(f) = &rule.condition.url_filter {
                for lit in f.literals() {
                    for w in lit.windows(GRAM) {
                        grams.push(gram_key(w));
                    }
                }
            }
            grams.sort_unstable();
            grams.dedup();
            for g in &grams {
                *count.entry(*g).or_default() += 1;
            }
            grams_of.push(grams);
        }
        for (i, grams) in grams_of.iter().enumerate() {
            self.types |= self.rules[i].condition.types;
            match grams.iter().min_by_key(|g| count[g]) {
                Some(g) => self.by_gram.entry(*g).or_default().push(i as u32),
                None => self.unindexed.push(i as u32),
            }
        }
    }

    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    fn regex_count(&self) -> usize {
        self.rules.iter().filter(|r| r.is_regex()).count()
    }

    fn unsafe_count(&self) -> usize {
        self.rules.iter().filter(|r| r.action.is_unsafe()).count()
    }

    /// Calls `f` with every rule that matches the request.
    pub fn for_each_match<'a>(&'a self, req: &Request, mut f: impl FnMut(&'a Rule)) {
        if self.types & req.resource_type.bit() == 0 {
            return;
        }
        for i in &self.unindexed {
            let rule = &self.rules[*i as usize];
            if rule.matches(req) {
                f(rule);
            }
        }
        if self.by_gram.is_empty() {
            return;
        }
        let mut seen: Vec<u32> = vec![];
        for w in req.lower_url.as_bytes().windows(GRAM) {
            let Some(list) = self.by_gram.get(&gram_key(w)) else { continue };
            for i in list {
                // The same substring can occur twice in a URL; a rule must be reported once.
                if seen.contains(i) {
                    continue;
                }
                let rule = &self.rules[*i as usize];
                if rule.matches(req) {
                    seen.push(*i);
                    f(rule);
                }
            }
        }
    }
}

// ---- One extension's rules --------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct StaticRuleset {
    pub id: String,
    pub path: String,
    pub enabled: bool,
    /// Parsed when the ruleset is first enabled.
    pub rules: Option<RuleSet>,
    pub disabled_rule_ids: HashSet<i64>,
    /// Problems found in the file; rules with problems are skipped, as in Chrome.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchedRule {
    pub rule_id: i64,
    pub ruleset_id: String,
    pub tab_id: i64,
    pub time_ms: i64,
}

impl MatchedRule {
    pub fn to_json(&self) -> Value {
        json!({ "rule": { "ruleId": self.rule_id, "rulesetId": self.ruleset_id }, "tabId": self.tab_id, "timeStamp": self.time_ms })
    }
}

/// What one extension wants done with a request.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: ActionType,
    pub rule_id: i64,
    pub ruleset_id: String,
    pub priority: i64,
    /// For `redirect` and `upgradeScheme`.
    pub redirect_url: Option<String>,
    /// The deciding rule looked at response headers, so it can only act once they are there.
    pub from_response: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HeaderRule {
    pub rule_id: i64,
    pub ruleset_id: String,
    pub priority: i64,
    pub request_headers: Vec<HeaderOp>,
    pub response_headers: Vec<HeaderOp>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evaluation {
    pub decision: Option<Decision>,
    /// `modifyHeaders` rules that apply, highest priority first.
    pub header_rules: Vec<HeaderRule>,
}

/// What the extension may do to this request (`declarativeNetRequest` vs `declarativeNetRequestWithHostAccess`, and its
/// host permissions for the request and its initiator).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Not allowed to see the request at all.
    None,
    /// May block, allow and upgrade, but not redirect or change headers.
    BlockOnly,
    Full,
}

#[derive(Debug, Default)]
pub struct ExtensionRules {
    pub static_rulesets: Vec<StaticRuleset>,
    pub dynamic: RuleSet,
    pub session: RuleSet,
    pub matched: VecDeque<MatchedRule>,
    /// `setExtensionActionOptions({ displayActionCountAsBadgeText })`
    pub show_action_count: bool,
    /// Actions taken per tab, for the badge.
    pub action_counts: HashMap<i64, i64>,
    get_matched_calls: VecDeque<i64>,
}

/// Parses the contents of a rules file or an `addRules` list. With `strict` the first bad rule is an error; otherwise bad
/// rules are skipped and described in the second result.
pub fn parse_rules(list: &Value, session: bool, strict: bool) -> Result<(Vec<Rule>, Vec<String>), String> {
    let items = list.as_array().ok_or("rules must be a list")?;
    let mut rules = Vec::with_capacity(items.len());
    let mut warnings = vec![];
    let mut ids = HashSet::new();
    for item in items {
        let parsed = Rule::parse(item, session)
            .and_then(|r| if ids.insert(r.id) { Ok(r) } else { Err(format!("Rule with id {} does not have a unique ID.", r.id)) });
        match parsed {
            Ok(r) => rules.push(r),
            Err(e) if strict => return Err(e),
            Err(e) => warnings.push(e),
        }
    }
    Ok((rules, warnings))
}

impl ExtensionRules {
    /// The rulesets the manifest declares; `read` returns a ruleset file's text.
    pub fn from_manifest(rulesets: &[crate::manifest::RuleResource], read: impl Fn(&str) -> Option<String>) -> ExtensionRules {
        let mut out = ExtensionRules::default();
        for r in rulesets.iter().take(MAX_NUMBER_OF_STATIC_RULESETS) {
            out.static_rulesets.push(StaticRuleset { id: r.id.clone(), path: r.path.clone(), enabled: r.enabled, ..Default::default() });
        }
        out.load_enabled(read);
        out
    }

    /// Parses the files of enabled rulesets that have not been read yet.
    pub fn load_enabled(&mut self, read: impl Fn(&str) -> Option<String>) {
        for set in self.static_rulesets.iter_mut().filter(|s| s.enabled && s.rules.is_none()) {
            let parsed = read(&set.path)
                .ok_or_else(|| format!("{}: file not found", set.path))
                .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| format!("{}: {e}", set.path)))
                .and_then(|v| parse_rules(&v, false, false));
            match parsed {
                Ok((rules, warnings)) => {
                    set.rules = Some(RuleSet::new(rules));
                    set.warnings = warnings;
                }
                Err(e) => {
                    set.rules = Some(RuleSet::default());
                    set.warnings = vec![e];
                }
            }
        }
    }

    pub fn enabled_ruleset_ids(&self) -> Vec<String> {
        self.static_rulesets.iter().filter(|s| s.enabled).map(|s| s.id.clone()).collect()
    }

    pub fn enabled_static_rule_count(&self) -> usize {
        self.static_rulesets.iter().filter(|s| s.enabled).map(|s| s.rules.as_ref().map_or(0, RuleSet::len)).sum()
    }

    /// Whether any rule could apply at all; lets the browser skip asking for requests when nothing is installed.
    pub fn has_rules(&self) -> bool {
        !self.dynamic.is_empty()
            || !self.session.is_empty()
            || self.static_rulesets.iter().any(|s| s.enabled && s.rules.as_ref().is_some_and(|r| !r.is_empty()))
    }

    pub fn has_response_rules(&self) -> bool {
        let needs = |s: &RuleSet| s.rules.iter().any(|r| r.condition.needs_response() || !r.response_headers.is_empty());
        needs(&self.dynamic)
            || needs(&self.session)
            || self.static_rulesets.iter().any(|s| s.enabled && s.rules.as_ref().is_some_and(needs))
    }

    pub fn has_request_header_rules(&self) -> bool {
        let needs = |s: &RuleSet| s.rules.iter().any(|r| !r.request_headers.is_empty());
        needs(&self.dynamic)
            || needs(&self.session)
            || self.static_rulesets.iter().any(|s| s.enabled && s.rules.as_ref().is_some_and(needs))
    }

    /// `updateDynamicRules` / `updateSessionRules`: removals first, then additions; nothing changes on error.
    pub fn update_rules(&mut self, session: bool, options: &Value) -> Result<(), String> {
        let remove: HashSet<i64> = match options.get("removeRuleIds") {
            None | Some(Value::Null) => HashSet::new(),
            Some(Value::Array(a)) => {
                a.iter().map(|v| v.as_i64().ok_or("removeRuleIds must be a list of integers")).collect::<Result<_, _>>()?
            }
            Some(_) => return Err("removeRuleIds must be a list of integers".into()),
        };
        let add = match options.get("addRules") {
            None | Some(Value::Null) => vec![],
            Some(list) => parse_rules(list, session, true)?.0,
        };
        let current = if session { &self.session } else { &self.dynamic };
        let mut rules: Vec<Rule> = current.rules.iter().filter(|r| !remove.contains(&r.id)).cloned().collect();
        for rule in add {
            if rules.iter().any(|r| r.id == rule.id) {
                return Err(format!("Rule with id {} does not have a unique ID.", rule.id));
            }
            rules.push(rule);
        }
        let candidate = RuleSet::new(rules);
        let (max, max_unsafe, what) = if session {
            (MAX_NUMBER_OF_SESSION_RULES, MAX_NUMBER_OF_UNSAFE_SESSION_RULES, "Session")
        } else {
            (MAX_NUMBER_OF_DYNAMIC_RULES, MAX_NUMBER_OF_UNSAFE_DYNAMIC_RULES, "Dynamic")
        };
        if candidate.len() > max {
            return Err(format!("{what} rule count exceeded."));
        }
        if candidate.unsafe_count() > max_unsafe {
            return Err(format!("{what} unsafe rule count exceeded."));
        }
        let other = if session { &self.dynamic } else { &self.session };
        if candidate.regex_count() > MAX_NUMBER_OF_REGEX_RULES
            || candidate.regex_count() + other.regex_count() > 2 * MAX_NUMBER_OF_REGEX_RULES
        {
            return Err(format!("{what} rule count for regex rules exceeded."));
        }
        if session {
            self.session = candidate;
        } else {
            self.dynamic = candidate;
        }
        Ok(())
    }

    /// `updateEnabledRulesets`. `pool_left` is how many static rules beyond the guaranteed minimum the browser still has.
    pub fn update_enabled_rulesets(
        &mut self,
        options: &Value,
        pool_left: usize,
        read: impl Fn(&str) -> Option<String>,
    ) -> Result<(), String> {
        let ids = |key: &str| -> Result<Vec<String>, String> {
            match options.get(key) {
                None | Some(Value::Null) => Ok(vec![]),
                Some(Value::Array(a)) => {
                    a.iter().map(|v| v.as_str().map(String::from).ok_or(format!("{key} must be a list of strings"))).collect()
                }
                Some(_) => Err(format!("{key} must be a list of strings")),
            }
        };
        let (disable, enable) = (ids("disableRulesetIds")?, ids("enableRulesetIds")?);
        for id in disable.iter().chain(&enable) {
            if !self.static_rulesets.iter().any(|s| s.id == *id) {
                return Err(format!("Invalid ruleset id: {id}."));
            }
        }
        let before: Vec<bool> = self.static_rulesets.iter().map(|s| s.enabled).collect();
        let before_count = self.enabled_static_rule_count();
        for set in &mut self.static_rulesets {
            // Enabling wins when an id is in both lists.
            if disable.contains(&set.id) {
                set.enabled = false;
            }
            if enable.contains(&set.id) {
                set.enabled = true;
            }
        }
        self.load_enabled(read);
        let count = self.enabled_static_rule_count();
        let error = if self.static_rulesets.iter().filter(|s| s.enabled).count() > MAX_NUMBER_OF_ENABLED_STATIC_RULESETS {
            Some("The number of enabled static rulesets exceeds the enabled ruleset count limit.")
        } else if count > before_count.max(GUARANTEED_MINIMUM_STATIC_RULES)
            && count - before_count.max(GUARANTEED_MINIMUM_STATIC_RULES) > pool_left
        {
            Some("The set of enabled rulesets exceeds the rule count limit.")
        } else if self
            .static_rulesets
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| s.rules.as_ref())
            .map(RuleSet::regex_count)
            .sum::<usize>()
            > MAX_NUMBER_OF_REGEX_RULES
        {
            Some("The set of enabled rulesets exceeds the regular expression rule count limit.")
        } else {
            None
        };
        if let Some(e) = error {
            for (set, was) in self.static_rulesets.iter_mut().zip(before) {
                set.enabled = was;
            }
            return Err(e.into());
        }
        Ok(())
    }

    /// `updateStaticRules({ rulesetId, disableRuleIds, enableRuleIds })`
    pub fn update_static_rules(&mut self, options: &Value) -> Result<(), String> {
        let id = options.get("rulesetId").and_then(Value::as_str).ok_or("rulesetId must be a string")?;
        let ints = |key: &str| -> Result<Vec<i64>, String> {
            match options.get(key) {
                None | Some(Value::Null) => Ok(vec![]),
                Some(Value::Array(a)) => a.iter().map(|v| v.as_i64().ok_or(format!("{key} must be a list of integers"))).collect(),
                Some(_) => Err(format!("{key} must be a list of integers")),
            }
        };
        let (disable, enable) = (ints("disableRuleIds")?, ints("enableRuleIds")?);
        let total_elsewhere: usize = self.static_rulesets.iter().filter(|s| s.id != id).map(|s| s.disabled_rule_ids.len()).sum();
        let set = self.static_rulesets.iter_mut().find(|s| s.id == id).ok_or_else(|| format!("Invalid ruleset id: {id}."))?;
        let mut next = set.disabled_rule_ids.clone();
        next.extend(disable);
        for e in enable {
            next.remove(&e);
        }
        if total_elsewhere + next.len() > MAX_NUMBER_OF_DISABLED_STATIC_RULES {
            return Err("The number of disabled static rules exceeds the disabled rule count limit.".into());
        }
        set.disabled_rule_ids = next;
        Ok(())
    }

    /// What survives a restart: dynamic rules, which static rulesets are on, and the disabled static rules.
    pub fn to_json(&self) -> Value {
        let disabled: Map<String, Value> = self
            .static_rulesets
            .iter()
            .filter(|s| !s.disabled_rule_ids.is_empty())
            .map(|s| {
                let mut ids: Vec<i64> = s.disabled_rule_ids.iter().copied().collect();
                ids.sort_unstable();
                (s.id.clone(), json!(ids))
            })
            .collect();
        json!({
            "dynamic": self.dynamic.rules.iter().map(|r| r.source.clone()).collect::<Vec<_>>(),
            "enabled": self.enabled_ruleset_ids(),
            "disabledRules": disabled,
            "showActionCount": self.show_action_count,
        })
    }

    pub fn restore(&mut self, saved: &Value, read: impl Fn(&str) -> Option<String>) {
        if let Some(list) = saved.get("dynamic")
            && let Ok((rules, _)) = parse_rules(list, false, false)
        {
            self.dynamic = RuleSet::new(rules);
        }
        if let Some(enabled) = saved.get("enabled").and_then(Value::as_array) {
            for set in &mut self.static_rulesets {
                set.enabled = enabled.iter().any(|v| v.as_str() == Some(&set.id));
            }
        }
        if let Some(disabled) = saved.get("disabledRules").and_then(Value::as_object) {
            for set in &mut self.static_rulesets {
                if let Some(ids) = disabled.get(&set.id).and_then(Value::as_array) {
                    set.disabled_rule_ids = ids.iter().filter_map(Value::as_i64).collect();
                }
            }
        }
        self.show_action_count = saved.get("showActionCount").and_then(Value::as_bool).unwrap_or(false);
        self.load_enabled(read);
    }

    /// Every rule of this extension that matches, with the id of its ruleset.
    fn for_each_match<'a>(&'a self, req: &Request, mut f: impl FnMut(&'a Rule, &'a str)) {
        self.session.for_each_match(req, |r| f(r, SESSION_RULESET_ID));
        self.dynamic.for_each_match(req, |r| f(r, DYNAMIC_RULESET_ID));
        for set in self.static_rulesets.iter().filter(|s| s.enabled) {
            if let Some(rules) = &set.rules {
                rules.for_each_match(req, |r| {
                    if !set.disabled_rule_ids.contains(&r.id) {
                        f(r, &set.id);
                    }
                });
            }
        }
    }

    /// Chooses this extension's action for a request.
    ///
    /// `frame_allow` is the priority of an `allowAllRequests` rule that matched the request's frame or one of its ancestors.
    /// Before the response (`req.response_headers` is `None`) rules with response-header conditions cannot match; once it is
    /// there, only those rules can still block or redirect, because the others had their turn.
    pub fn evaluate(&self, req: &Request, access: Access, frame_allow: Option<i64>, base_url: &str) -> Evaluation {
        if access == Access::None {
            return Evaluation::default();
        }
        let response_stage = req.response_headers.is_some();
        // (priority, rank) of the best rule so far; a higher priority wins, then the action type.
        let mut best: Option<(i64, u8, Decision)> = None;
        let mut allow_priority = frame_allow;
        let mut header_rules: Vec<HeaderRule> = vec![];
        self.for_each_match(req, |rule, ruleset| {
            let decision = |redirect_url: Option<String>| Decision {
                action: rule.action,
                rule_id: rule.id,
                ruleset_id: ruleset.to_string(),
                priority: rule.priority,
                redirect_url,
                from_response: rule.condition.needs_response(),
            };
            let candidate = match rule.action {
                ActionType::ModifyHeaders => {
                    if access == Access::Full {
                        header_rules.push(HeaderRule {
                            rule_id: rule.id,
                            ruleset_id: ruleset.to_string(),
                            priority: rule.priority,
                            request_headers: rule.request_headers.clone(),
                            response_headers: rule.response_headers.clone(),
                        });
                    }
                    return;
                }
                ActionType::Allow | ActionType::AllowAllRequests => {
                    allow_priority = Some(allow_priority.map_or(rule.priority, |p| p.max(rule.priority)));
                    decision(None)
                }
                ActionType::Block => decision(None),
                ActionType::UpgradeScheme => {
                    let upgraded = match req.scheme.as_str() {
                        "http" => format!("https{}", &req.url[4..]),
                        "ws" => format!("wss{}", &req.url[2..]),
                        _ => return,
                    };
                    decision(Some(upgraded))
                }
                ActionType::Redirect => {
                    if access != Access::Full {
                        return;
                    }
                    match rule.redirect_url(req, base_url) {
                        Some(url) => decision(Some(url)),
                        None => return,
                    }
                }
            };
            let key = (rule.priority, rule.action.rank());
            let better = match &best {
                None => true,
                Some((p, rank, _)) => key.0 > *p || (key.0 == *p && key.1 < *rank),
            };
            if better {
                best = Some((key.0, key.1, candidate));
            }
        });

        let mut decision = best.map(|(_, _, d)| d);
        // A frame-wide allow outranks rules of lower or equal priority.
        if let (Some(frame), Some(d)) = (frame_allow, &decision)
            && !matches!(d.action, ActionType::Allow | ActionType::AllowAllRequests)
            && frame >= d.priority
        {
            decision = None;
        }
        // Rules that did not need the response acted before the request was sent.
        if response_stage
            && let Some(d) = &decision
            && !d.from_response
            && !matches!(d.action, ActionType::Allow | ActionType::AllowAllRequests)
        {
            decision = None;
        }
        // Header rules apply when nothing blocks or redirects, and only those above the best allow rule.
        if decision.as_ref().is_some_and(|d| matches!(d.action, ActionType::Block | ActionType::Redirect | ActionType::UpgradeScheme)) {
            header_rules.clear();
        }
        if let Some(allow) = allow_priority {
            header_rules.retain(|h| h.priority > allow);
        }
        header_rules.sort_by_key(|h| std::cmp::Reverse(h.priority));
        Evaluation { decision, header_rules }
    }

    /// Remembers that a rule acted, for `getMatchedRules` and the badge.
    pub fn record_match(&mut self, rule_id: i64, ruleset_id: &str, tab_id: i64, now_ms: i64) {
        while self.matched.front().is_some_and(|m| now_ms - m.time_ms > MATCHED_RULE_LIFETIME_MS) || self.matched.len() >= MAX_MATCHED_RULES
        {
            self.matched.pop_front();
        }
        self.matched.push_back(MatchedRule { rule_id, ruleset_id: ruleset_id.to_string(), tab_id, time_ms: now_ms });
    }

    /// `getMatchedRules` may be called 20 times in 10 minutes outside a user gesture.
    pub fn take_get_matched_quota(&mut self, now_ms: i64) -> bool {
        let window = GETMATCHEDRULES_QUOTA_INTERVAL * 60 * 1000;
        while self.get_matched_calls.front().is_some_and(|t| now_ms - t >= window) {
            self.get_matched_calls.pop_front();
        }
        if self.get_matched_calls.len() >= MAX_GETMATCHEDRULES_CALLS_PER_INTERVAL {
            return false;
        }
        self.get_matched_calls.push_back(now_ms);
        true
    }

    pub fn forget_tab(&mut self, tab_id: i64) {
        self.action_counts.remove(&tab_id);
        self.matched.retain(|m| m.tab_id != tab_id);
    }
}

// ---- Applying header rules --------------------------------------------------------------------------------------------

/// Applies header operations in order (the caller sorts: most recently installed extension first, then priority).
///
/// As in Chrome, the first operation on a header decides what later ones may do: after a `set` only `append`s from the same
/// extension may follow, after a `remove` nothing, and after an `append` only more `append`s. Each item is `(owner, tag,
/// operation)`: the owner is the extension, and the tag comes back through `applied` for each operation that took effect.
/// Returns whether anything changed.
pub fn apply_header_ops<'a>(
    headers: &mut Vec<(String, String)>,
    ops: impl Iterator<Item = (usize, usize, &'a HeaderOp)>,
    request: bool,
    mut applied: impl FnMut(usize),
) -> bool {
    // header -> (first operation, the extension that set it)
    let mut state: HashMap<&str, (HeaderOperation, usize)> = HashMap::new();
    let mut changed = false;
    for (owner, tag, op) in ops {
        let allowed = match state.get(op.header.as_str()) {
            None => true,
            Some((HeaderOperation::Remove, _)) => false,
            Some((HeaderOperation::Set, by)) => op.operation == HeaderOperation::Append && *by == owner,
            Some((HeaderOperation::Append, _)) => op.operation == HeaderOperation::Append,
        };
        if !allowed {
            continue;
        }
        state.entry(op.header.as_str()).or_insert((op.operation, owner));
        let position = headers.iter().position(|(n, _)| n.eq_ignore_ascii_case(&op.header));
        match op.operation {
            HeaderOperation::Remove => {
                if position.is_none() {
                    continue;
                }
                headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&op.header));
            }
            HeaderOperation::Set => {
                headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&op.header));
                headers.push((op.header.clone(), op.value.clone()));
            }
            HeaderOperation::Append => match position {
                // Request headers have one line per name; values are joined. Response headers get another line.
                Some(i) if request => {
                    let separator = if op.header == "cookie" { "; " } else { ", " };
                    let joined = format!("{}{separator}{}", headers[i].1, op.value);
                    headers[i].1 = joined;
                }
                _ => headers.push((op.header.clone(), op.value.clone())),
            },
        }
        changed = true;
        applied(tag);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(url: &str, t: ResourceType) -> Request {
        Request::new(url, "GET", t, Some("https://site.test"), 1).unwrap()
    }

    fn filter(pattern: &str, url: &str) -> bool {
        UrlFilter::parse(pattern, false).unwrap().matches(&req(url, ResourceType::Script))
    }

    #[test]
    fn url_filter_syntax() {
        assert!(filter("abc", "https://example.com/xabcx"));
        assert!(filter("ABC", "https://example.com/abc"), "case-insensitive by default");
        assert!(!filter("abd", "https://example.com/abc"));
        assert!(filter("||example.com", "https://example.com/"));
        assert!(filter("||example.com", "https://a.b.example.com/x"));
        assert!(!filter("||example.com", "https://notexample.com/"));
        assert!(!filter("||example.com", "https://other.test/?u=example.com"), "the domain anchor only looks at the host");
        assert!(filter("||example.com^", "https://example.com/"));
        assert!(filter("||example.com^", "https://example.com:8080/"));
        assert!(!filter("||example.com^", "https://example.com.evil.test/"));
        assert!(filter("|https://ex", "https://example.com/"));
        assert!(!filter("|example", "https://example.com/"));
        assert!(filter(".js|", "https://example.com/a.js"));
        assert!(!filter(".js|", "https://example.com/a.js?x"));
        assert!(filter("/ads/*/banner", "https://example.com/ads/1/2/banner.png"));
        assert!(!filter("/ads/*/banner", "https://example.com/ads/banner"));
        assert!(filter("^ad^", "https://example.com/ad"), "the end of the URL is a separator");
        assert!(filter("^ad^", "https://example.com/ad?x"));
        assert!(!filter("^ad^", "https://example.com/add"));
        assert!(filter("a*b*c", "https://example.com/a1b2c"));
        assert!(filter("|https://*.com/x|", "https://example.com/x"));
        assert!(UrlFilter::parse("", false).is_err());
        assert!(UrlFilter::parse("||*x", false).is_err());
        assert!(UrlFilter::parse("é", false).is_err());

        let sensitive = UrlFilter::parse("/Path", true).unwrap();
        assert!(sensitive.matches(&req("https://example.com/Path", ResourceType::Script)));
        assert!(!sensitive.matches(&req("https://example.com/path", ResourceType::Script)));
    }

    fn rule(v: Value) -> Rule {
        Rule::parse(&v, true).unwrap()
    }

    fn rules(list: Value) -> ExtensionRules {
        let mut e = ExtensionRules::default();
        e.update_rules(true, &json!({ "addRules": list })).unwrap();
        e
    }

    const BASE: &str = "chrome-extension://abc/";

    fn action(e: &ExtensionRules, r: &Request) -> Option<(ActionType, i64)> {
        e.evaluate(r, Access::Full, None, BASE).decision.map(|d| (d.action, d.rule_id))
    }

    #[test]
    fn conditions() {
        let r = rule(
            json!({"id": 1, "action": {"type": "block"}, "condition": {"urlFilter": "||ads.test^", "resourceTypes": ["script", "image"]}}),
        );
        assert!(r.matches(&req("https://ads.test/a.js", ResourceType::Script)));
        assert!(!r.matches(&req("https://ads.test/a.css", ResourceType::Stylesheet)));

        let r = rule(json!({"id": 1, "action": {"type": "block"}, "condition": {"urlFilter": "x"}}));
        assert!(r.matches(&req("https://a.test/x", ResourceType::SubFrame)));
        assert!(!r.matches(&req("https://a.test/x", ResourceType::MainFrame)), "main_frame is excluded unless asked for");

        let r = rule(json!({"id": 1, "action": {"type": "block"}, "condition": {"excludedResourceTypes": ["image"]}}));
        assert!(r.matches(&req("https://a.test/x", ResourceType::MainFrame)));
        assert!(!r.matches(&req("https://a.test/x", ResourceType::Image)));

        let r = rule(json!({"id": 1, "action": {"type": "block"}, "condition": {"domainType": "thirdParty"}}));
        assert!(r.matches(&req("https://cdn.other.test/x", ResourceType::Script)));
        assert!(!r.matches(&req("https://cdn.site.test/x", ResourceType::Script)), "a subdomain of the initiator's site is first-party");

        let r = rule(json!({"id": 1, "action": {"type": "block"}, "condition": {
            "requestDomains": ["a.test"], "excludedRequestDomains": ["safe.a.test"],
            "initiatorDomains": ["site.test"], "requestMethods": ["post"]}}));
        let post = |u: &str| Request::new(u, "POST", ResourceType::XmlHttpRequest, Some("https://www.site.test"), 1).unwrap();
        assert!(r.matches(&post("https://x.a.test/")));
        assert!(!r.matches(&post("https://safe.a.test/")));
        assert!(!r.matches(&post("https://b.test/")));
        assert!(!r.matches(&req("https://x.a.test/", ResourceType::XmlHttpRequest)), "GET is not POST");
        let other = Request::new("https://x.a.test/", "POST", ResourceType::XmlHttpRequest, Some("https://elsewhere.test"), 1).unwrap();
        assert!(!r.matches(&other));

        let r = rule(
            json!({"id": 1, "action": {"type": "block"}, "condition": {"regexFilter": "^https://[a-z]+\\.test/\\d+$", "tabIds": [1]}}),
        );
        assert!(r.matches(&req("https://abc.test/123", ResourceType::Script)));
        assert!(!r.matches(&req("https://abc.test/12a", ResourceType::Script)));
        let tab2 = Request::new("https://abc.test/123", "GET", ResourceType::Script, None, 2).unwrap();
        assert!(!r.matches(&tab2));
    }

    #[test]
    fn validation() {
        let err = |v: Value| Rule::parse(&v, false).unwrap_err();
        assert!(err(json!({"id": 0, "action": {"type": "block"}})).contains("id"));
        assert!(err(json!({"id": 1, "action": {"type": "nope"}})).contains("action.type"));
        assert!(err(json!({"id": 1, "priority": 0, "action": {"type": "block"}})).contains("priority"));
        assert!(err(json!({"id": 1, "action": {"type": "redirect"}})).contains("action.redirect"));
        assert!(err(json!({"id": 1, "action": {"type": "redirect", "redirect": {"url": "javascript:alert(1)"}}})).contains("redirect.url"));
        assert!(err(json!({"id": 1, "action": {"type": "redirect", "redirect": {"regexSubstitution": "x"}}})).contains("regexFilter"));
        assert!(err(json!({"id": 1, "action": {"type": "block"}, "condition": {"urlFilter": "a", "regexFilter": "a"}})).contains("both"));
        assert!(err(json!({"id": 1, "action": {"type": "block"}, "condition": {"regexFilter": "("}})).contains("regexFilter"));
        assert!(err(json!({"id": 1, "action": {"type": "block"}, "condition": {"resourceTypes": []}})).contains("empty list"));
        assert!(err(json!({"id": 1, "action": {"type": "block"}, "condition": {"requestDomains": []}})).contains("empty list"));
        assert!(err(json!({"id": 1, "action": {"type": "block"}, "condition": {"tabIds": [1]}})).contains("session"));
        assert!(
            err(json!({"id": 1, "action": {"type": "allowAllRequests"}, "condition": {"urlFilter": "a"}})).contains("allowAllRequests")
        );
        assert!(err(json!({"id": 1, "action": {"type": "modifyHeaders"}})).contains("requestHeaders"));
        assert!(
            err(json!({"id": 1, "action": {"type": "modifyHeaders", "requestHeaders": [{"header": "x-a", "operation": "append", "value": "1"}]}}))
                .contains("appended")
        );
        assert!(
            err(json!({"id": 1, "action": {"type": "modifyHeaders", "responseHeaders": [{"header": "x-a", "operation": "set"}]}}))
                .contains("value")
        );
        assert!(
            err(json!({"id": 1, "action": {"type": "modifyHeaders", "responseHeaders": [{"header": "x-a", "operation": "remove", "value": "1"}]}}))
                .contains("removed")
        );
        assert_eq!(rule(json!({"id": 7, "action": {"type": "block"}})).source["priority"], 1);
    }

    #[test]
    fn priority_and_action_order() {
        let e = rules(json!([
            {"id": 1, "priority": 1, "action": {"type": "block"}, "condition": {"urlFilter": "||ads.test"}},
            {"id": 2, "priority": 2, "action": {"type": "allow"}, "condition": {"urlFilter": "||ads.test/ok"}},
            {"id": 3, "priority": 2, "action": {"type": "block"}, "condition": {"urlFilter": "||ads.test/ok/still"}},
            {"id": 4, "priority": 3, "action": {"type": "redirect", "redirect": {"url": "https://r.test/"}}, "condition": {"urlFilter": "||ads.test/go"}},
            {"id": 5, "priority": 3, "action": {"type": "upgradeScheme"}, "condition": {"urlFilter": "||ads.test/go"}}
        ]));
        assert_eq!(action(&e, &req("https://ads.test/x.js", ResourceType::Script)), Some((ActionType::Block, 1)));
        assert_eq!(action(&e, &req("https://ads.test/ok.js", ResourceType::Script)), Some((ActionType::Allow, 2)));
        assert_eq!(
            action(&e, &req("https://ads.test/ok/still", ResourceType::Script)),
            Some((ActionType::Allow, 2)),
            "allow beats block at equal priority"
        );
        assert_eq!(
            action(&e, &req("http://ads.test/go", ResourceType::Script)),
            Some((ActionType::UpgradeScheme, 5)),
            "upgrade beats redirect"
        );
        assert_eq!(
            action(&e, &req("https://ads.test/go", ResourceType::Script)),
            Some((ActionType::Redirect, 4)),
            "nothing to upgrade on https"
        );
        assert_eq!(action(&e, &req("https://fine.test/", ResourceType::Script)), None);
        let upgraded = e.evaluate(&req("http://ads.test/go?a=1", ResourceType::Script), Access::Full, None, BASE).decision.unwrap();
        assert_eq!(upgraded.redirect_url.as_deref(), Some("https://ads.test/go?a=1"));
    }

    #[test]
    fn access_limits_what_rules_may_do() {
        let e = rules(json!([
            {"id": 1, "action": {"type": "redirect", "redirect": {"url": "https://r.test/"}}, "condition": {"urlFilter": "a.test"}},
            {"id": 2, "action": {"type": "modifyHeaders", "requestHeaders": [{"header": "x", "operation": "remove"}]}, "condition": {"urlFilter": "a.test"}},
            {"id": 3, "action": {"type": "block"}, "condition": {"urlFilter": "b.test"}}
        ]));
        let a = req("https://a.test/", ResourceType::Script);
        assert_eq!(e.evaluate(&a, Access::BlockOnly, None, BASE), Evaluation::default());
        assert_eq!(e.evaluate(&a, Access::Full, None, BASE).header_rules.len(), 0, "a redirected request gets no header changes");
        let b = req("https://b.test/", ResourceType::Script);
        assert!(e.evaluate(&b, Access::BlockOnly, None, BASE).decision.is_some());
        assert!(e.evaluate(&b, Access::None, None, BASE).decision.is_none());
    }

    #[test]
    fn redirects() {
        let to = |redirect: Value, cond: Value, url: &str| {
            let e = rules(json!([{"id": 1, "action": {"type": "redirect", "redirect": redirect}, "condition": cond}]));
            e.evaluate(&req(url, ResourceType::Script), Access::Full, None, BASE).decision.and_then(|d| d.redirect_url)
        };
        let any = json!({"urlFilter": "a.test"});
        assert_eq!(
            to(json!({"extensionPath": "/stub.js"}), any.clone(), "https://a.test/x").as_deref(),
            Some("chrome-extension://abc/stub.js")
        );
        assert_eq!(
            to(
                json!({"transform": {"scheme": "https", "host": "b.test", "port": "8443", "path": "/p", "fragment": "#f"}}),
                any.clone(),
                "http://a.test/x?q=1"
            )
            .as_deref(),
            Some("https://b.test:8443/p?q=1#f")
        );
        assert_eq!(
            to(
                json!({"transform": {"queryTransform": {"removeParams": ["utm_source", "gclid"], "addOrReplaceParams": [
                    {"key": "a", "value": "new value"}, {"key": "only", "value": "x", "replaceOnly": true}, {"key": "added", "value": "1&2"}]}}}),
                any.clone(),
                "https://a.test/x?utm_source=s&a=1&keep&gclid=2"
            )
            .as_deref(),
            Some("https://a.test/x?a=new%20value&keep&added=1%262")
        );
        assert_eq!(
            to(json!({"transform": {"queryTransform": {"removeParams": ["a"]}}}), any.clone(), "https://a.test/x?a=1").as_deref(),
            Some("https://a.test/x"),
            "an empty query loses its question mark"
        );
        assert_eq!(to(json!({"transform": {"query": ""}}), any.clone(), "https://a.test/x?a=1").as_deref(), Some("https://a.test/x"));
        assert_eq!(
            to(json!({"url": "https://a.test/x"}), any.clone(), "https://a.test/x"),
            None,
            "a redirect to the same URL is no redirect"
        );
        assert_eq!(
            to(
                json!({"regexSubstitution": "https://new.test/\\2/\\1"}),
                json!({"regexFilter": "^https://a\\.test/(\\w+)/(\\w+)$"}),
                "https://a.test/one/two"
            )
            .as_deref(),
            Some("https://new.test/two/one")
        );
    }

    #[test]
    fn modify_headers_respect_allow_rules() {
        let e = rules(json!([
            {"id": 1, "priority": 5, "action": {"type": "modifyHeaders", "requestHeaders": [{"header": "X-High", "operation": "set", "value": "1"}]},
             "condition": {"urlFilter": "a.test"}},
            {"id": 2, "priority": 1, "action": {"type": "modifyHeaders", "responseHeaders": [{"header": "x-low", "operation": "remove"}]},
             "condition": {"urlFilter": "a.test"}},
            {"id": 3, "priority": 3, "action": {"type": "allow"}, "condition": {"urlFilter": "a.test/allowed"}}
        ]));
        let ids = |url: &str| -> Vec<i64> {
            e.evaluate(&req(url, ResourceType::Script), Access::Full, None, BASE).header_rules.iter().map(|h| h.rule_id).collect()
        };
        assert_eq!(ids("https://a.test/"), [1, 2]);
        assert_eq!(ids("https://a.test/allowed"), [1], "only header rules above the allow rule's priority");
        let low = e.evaluate(&req("https://a.test/", ResourceType::Script), Access::Full, Some(9), BASE);
        assert!(low.header_rules.is_empty(), "a frame-wide allow counts too");
    }

    #[test]
    fn frame_allow_outranks_lower_rules() {
        let e = rules(json!([
            {"id": 1, "priority": 2, "action": {"type": "block"}, "condition": {"urlFilter": "ads"}},
            {"id": 2, "priority": 9, "action": {"type": "block"}, "condition": {"urlFilter": "ads/always"}}
        ]));
        let r = req("https://x.test/ads", ResourceType::Image);
        assert!(e.evaluate(&r, Access::Full, None, BASE).decision.is_some());
        assert!(e.evaluate(&r, Access::Full, Some(2), BASE).decision.is_none());
        assert!(e.evaluate(&req("https://x.test/ads/always", ResourceType::Image), Access::Full, Some(2), BASE).decision.is_some());
    }

    #[test]
    fn response_header_conditions_wait_for_the_response() {
        let e = rules(json!([
            {"id": 1, "action": {"type": "block"}, "condition": {"urlFilter": "a.test", "responseHeaders": [{"header": "Content-Type", "values": ["application/pdf*"]}]}},
            {"id": 2, "action": {"type": "block"}, "condition": {"urlFilter": "b.test"}}
        ]));
        let a = req("https://a.test/doc", ResourceType::SubFrame);
        assert!(e.evaluate(&a, Access::Full, None, BASE).decision.is_none(), "cannot match before the response");
        let pdf = a.clone().with_response_headers(vec![("Content-Type".into(), "application/pdf; x".into())]);
        assert_eq!(e.evaluate(&pdf, Access::Full, None, BASE).decision.map(|d| d.rule_id), Some(1));
        let html = a.with_response_headers(vec![("content-type".into(), "text/html".into())]);
        assert!(e.evaluate(&html, Access::Full, None, BASE).decision.is_none());
        let b = req("https://b.test/", ResourceType::Script).with_response_headers(vec![]);
        assert!(e.evaluate(&b, Access::Full, None, BASE).decision.is_none(), "request-stage rules do not act twice");
    }

    #[test]
    fn updating_rules_is_atomic_and_limited() {
        let mut e = ExtensionRules::default();
        e.update_rules(false, &json!({"addRules": [{"id": 1, "action": {"type": "block"}}, {"id": 2, "action": {"type": "block"}}]}))
            .unwrap();
        let dup =
            e.update_rules(false, &json!({"addRules": [{"id": 3, "action": {"type": "block"}}, {"id": 2, "action": {"type": "block"}}]}));
        assert!(dup.unwrap_err().contains("unique"));
        assert_eq!(e.dynamic.len(), 2, "nothing was added");
        e.update_rules(false, &json!({"removeRuleIds": [2], "addRules": [{"id": 2, "action": {"type": "allow"}}]})).unwrap();
        assert_eq!(e.dynamic.rules()[1].action, ActionType::Allow, "an id can be replaced in one call");

        let many: Vec<Value> =
            (1..=MAX_NUMBER_OF_SESSION_RULES as i64 + 1).map(|i| json!({"id": i, "action": {"type": "block"}})).collect();
        assert!(e.update_rules(true, &json!({ "addRules": many })).unwrap_err().contains("exceeded"));
        let regexes: Vec<Value> = (1..=MAX_NUMBER_OF_REGEX_RULES as i64 + 1)
            .map(|i| json!({"id": i, "action": {"type": "block"}, "condition": {"regexFilter": "a"}}))
            .collect();
        assert!(e.update_rules(true, &json!({ "addRules": regexes })).unwrap_err().contains("regex"));

        let saved = e.to_json();
        let mut again = ExtensionRules::default();
        again.restore(&saved, |_| None);
        assert_eq!(again.dynamic.len(), 2);
    }

    #[test]
    fn static_rulesets() {
        let resources = [
            crate::manifest::RuleResource { id: "on".into(), path: "on.json".into(), enabled: true },
            crate::manifest::RuleResource { id: "off".into(), path: "off.json".into(), enabled: false },
        ];
        let read = |path: &str| match path {
            "on.json" => Some(
                r#"[{"id":1,"action":{"type":"block"},"condition":{"urlFilter":"one"}},{"id":2,"action":{"type":"bogus"}},
                    {"id":3,"action":{"type":"block"},"condition":{"urlFilter":"three"}}]"#
                    .to_string(),
            ),
            "off.json" => Some(r#"[{"id":1,"action":{"type":"block"},"condition":{"urlFilter":"other"}}]"#.to_string()),
            _ => None,
        };
        let mut e = ExtensionRules::from_manifest(&resources, read);
        assert_eq!(e.enabled_ruleset_ids(), ["on"]);
        assert_eq!(e.enabled_static_rule_count(), 2, "the bad rule is skipped");
        assert_eq!(e.static_rulesets[0].warnings.len(), 1);
        assert!(e.static_rulesets[1].rules.is_none(), "disabled rulesets are not read");
        let one = req("https://x.test/one", ResourceType::Script);
        let other = req("https://x.test/other", ResourceType::Script);
        assert_eq!(e.evaluate(&one, Access::Full, None, BASE).decision.map(|d| d.ruleset_id), Some("on".into()));
        assert!(e.evaluate(&other, Access::Full, None, BASE).decision.is_none());

        e.update_enabled_rulesets(&json!({"enableRulesetIds": ["off"], "disableRulesetIds": ["on"]}), 0, read).unwrap();
        assert!(e.evaluate(&one, Access::Full, None, BASE).decision.is_none());
        assert!(e.evaluate(&other, Access::Full, None, BASE).decision.is_some());
        assert!(e.update_enabled_rulesets(&json!({"enableRulesetIds": ["nope"]}), 0, read).unwrap_err().contains("Invalid ruleset id"));

        e.update_enabled_rulesets(&json!({"enableRulesetIds": ["on"]}), 0, read).unwrap();
        e.update_static_rules(&json!({"rulesetId": "on", "disableRuleIds": [1]})).unwrap();
        assert!(e.evaluate(&one, Access::Full, None, BASE).decision.is_none());
        e.update_static_rules(&json!({"rulesetId": "on", "enableRuleIds": [1]})).unwrap();
        assert!(e.evaluate(&one, Access::Full, None, BASE).decision.is_some());
    }

    #[test]
    fn header_operations() {
        let op = |header: &str, operation: HeaderOperation, value: &str| HeaderOp { header: header.into(), operation, value: value.into() };
        let mut headers = vec![("Accept".to_string(), "a".to_string()), ("X-Gone".to_string(), "1".to_string())];
        let ops = [
            (0, op("accept", HeaderOperation::Append, "b")),
            (1, op("accept", HeaderOperation::Set, "ignored")),
            (0, op("x-gone", HeaderOperation::Remove, "")),
            (1, op("x-gone", HeaderOperation::Set, "back")),
            (0, op("x-new", HeaderOperation::Set, "1")),
            (1, op("x-new", HeaderOperation::Append, "other extension")),
            (0, op("x-new", HeaderOperation::Append, "2")),
            (0, op("x-missing", HeaderOperation::Remove, "")),
        ];
        let mut applied = vec![];
        assert!(apply_header_ops(&mut headers, ops.iter().map(|(o, op)| (*o, *o, op)), true, |o| applied.push(o)));
        assert_eq!(headers, [("Accept".to_string(), "a, b".to_string()), ("x-new".to_string(), "1, 2".to_string())]);
        assert_eq!(applied, [0, 0, 0, 0]);

        let mut response = vec![("set-cookie".to_string(), "a=1".to_string())];
        let append = [(0, op("set-cookie", HeaderOperation::Append, "b=2"))];
        apply_header_ops(&mut response, append.iter().map(|(o, op)| (*o, *o, op)), false, |_| {});
        assert_eq!(response.len(), 2, "response headers get another line");
    }

    #[test]
    fn matching_is_fast_with_a_large_list() {
        let list: Vec<Value> = (0..100_000)
            .map(|i| match i % 4 {
                0 => json!({"id": i + 1, "action": {"type": "block"}, "condition": {"urlFilter": format!("||ads{i}.example{}.com^", i % 977)}}),
                1 => json!({"id": i + 1, "action": {"type": "block"}, "condition": {"urlFilter": format!("/banner{i}/*.gif")}}),
                2 => json!({"id": i + 1, "action": {"type": "block"}, "condition": {"urlFilter": format!("track{i}.js"), "domainType": "thirdParty"}}),
                _ => json!({"id": i + 1, "action": {"type": "allow"}, "condition": {"urlFilter": format!("||cdn{i}.test/lib/"), "resourceTypes": ["script"]}}),
            })
            .collect();
        let (parsed, warnings) = parse_rules(&Value::Array(list), false, false).unwrap();
        assert!(warnings.is_empty());
        let mut e = ExtensionRules::default();
        e.static_rulesets.push(StaticRuleset { id: "big".into(), enabled: true, rules: Some(RuleSet::new(parsed)), ..Default::default() });

        let hit = req("https://ads40000.example920.com/x.js", ResourceType::Script);
        assert_eq!(action(&e, &hit), Some((ActionType::Block, 40001)));
        let urls: Vec<Request> = (0..200)
            .map(|i| {
                req(
                    &format!("https://static{i}.news-site.test/assets/js/app.bundle.{i}.min.js?v=20261111&cache=bust"),
                    ResourceType::Script,
                )
            })
            .collect();
        let start = std::time::Instant::now();
        let mut found = 0;
        for _ in 0..5 {
            for r in &urls {
                found += e.evaluate(r, Access::Full, None, BASE).decision.is_some() as usize;
            }
        }
        let per_request = start.elapsed() / 1000;
        assert_eq!(found, 0);
        // Requirement 10.5 asks for under 1 ms; debug builds are roughly ten times slower than release ones.
        let limit = if cfg!(debug_assertions) { 10_000 } else { 1_000 };
        assert!(per_request.as_micros() < limit, "{per_request:?} per request");
    }
}
