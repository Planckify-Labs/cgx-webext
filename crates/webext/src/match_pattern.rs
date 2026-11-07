//! WebExtensions match patterns (`*://*.example.com/*`, `<all_urls>`) and glob matching.
//!
//! <https://developer.chrome.com/docs/extensions/develop/concepts/match-patterns>
//! <https://developer.mozilla.org/docs/Mozilla/Add-ons/WebExtensions/Match_patterns>

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUrl {
    /// Lowercase, without the trailing colon.
    pub scheme: String,
    /// Lowercase; empty for `file:` and opaque URLs.
    pub host: String,
    pub port: Option<u16>,
    /// Starts with `/`, fragment removed. Empty for opaque URLs such as `about:blank`.
    pub path_and_query: String,
}

fn default_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" | "ws" => Some(80),
        "https" | "wss" => Some(443),
        "ftp" => Some(21),
        _ => None,
    }
}

fn parse_port(s: &str) -> Option<u16> {
    if s.is_empty() || s.len() > 5 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A deliberately small URL splitter, used only to evaluate match patterns. The embedding browser should pass
/// already-canonicalised URLs from its own URL parser.
pub fn split_url(url: &str) -> Option<ParsedUrl> {
    let (scheme, rest) = url.split_once(':')?;
    if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')) {
        return None;
    }
    let mut out = ParsedUrl { scheme: scheme.to_ascii_lowercase(), host: String::new(), port: None, path_and_query: String::new() };
    let Some(rest) = rest.strip_prefix("//") else {
        return Some(out); // Opaque URL.
    };

    let path_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(path_start);
    let tail = tail.split('#').next().unwrap_or("");
    out.path_and_query = if tail.is_empty() || tail.starts_with('?') { format!("/{tail}") } else { tail.to_string() };

    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        let close = stripped.find(']')?;
        let after = &stripped[close + 1..];
        if !after.is_empty() {
            let port_text = after.strip_prefix(':')?;
            if !port_text.is_empty() {
                out.port = Some(parse_port(port_text)?);
            }
        }
        &authority[..close + 2]
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        if !p.is_empty() {
            out.port = Some(parse_port(p)?);
        }
        h
    } else {
        authority
    };
    out.host = host.to_ascii_lowercase();
    Some(out)
}

/// Glob matching with `*` (any run of characters) and `?` (exactly one character). Iterative; worst case O(n·m), no
/// recursion, so hostile patterns cannot overflow the stack.
pub fn glob_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let eq = |a: char, b: char| if case_insensitive { a.to_lowercase().eq(b.to_lowercase()) } else { a == b };
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && p[pi] != '*' && (p[pi] == '?' || eq(p[pi], t[ti])) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MatchDialect {
    /// A `*` scheme matches `http` and `https`.
    #[default]
    Chrome,
    /// A `*` scheme matches `http`, `https`, `ws` and `wss`.
    Firefox,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPatternError(pub &'static str);

impl std::fmt::Display for MatchPatternError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for MatchPatternError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchPattern {
    source: String,
    all_urls: bool,
    schemes: Vec<&'static str>,
    any_host: bool,
    subdomains: bool,
    host: String,
    /// `None` means any port.
    port: Option<u16>,
    path: String,
}

impl MatchPattern {
    pub fn parse(pattern: &str, dialect: MatchDialect) -> Result<MatchPattern, MatchPatternError> {
        let err = MatchPatternError;
        let mut mp = MatchPattern {
            source: pattern.to_string(),
            all_urls: false,
            schemes: vec![],
            any_host: false,
            subdomains: false,
            host: String::new(),
            port: None,
            path: String::new(),
        };
        if pattern == "<all_urls>" {
            mp.all_urls = true;
            mp.any_host = true;
            mp.path = "/*".into();
            mp.schemes = vec!["http", "https", "ws", "wss", "ftp", "file"];
            return Ok(mp);
        }

        let (scheme, rest) = pattern.split_once("://").ok_or(err("missing '://'"))?;
        mp.schemes = match scheme {
            "*" if dialect == MatchDialect::Firefox => vec!["http", "https", "ws", "wss"],
            "*" => vec!["http", "https"],
            "http" => vec!["http"],
            "https" => vec!["https"],
            "ws" => vec!["ws"],
            "wss" => vec!["wss"],
            "ftp" => vec!["ftp"],
            "file" => vec!["file"],
            _ => return Err(err("unsupported scheme")),
        };

        let slash = rest.find('/').ok_or(err("missing path"))?;
        let (host, path) = rest.split_at(slash);
        mp.path = path.to_string();

        if scheme == "file" {
            // The host of a file: URL is ignored. Chrome documents `file:///*`; Firefox also accepts `file://*/*`, which
            // real extensions (uBlock Origin) use, so a lone `*` is accepted too.
            if !host.is_empty() && host != "*" {
                return Err(err("file: patterns must have an empty host"));
            }
            return Ok(mp);
        }
        if host.is_empty() {
            return Err(err("missing host"));
        }

        let mut host = host;
        if !host.starts_with('[') {
            if let Some((h, port_text)) = host.rsplit_once(':') {
                host = h;
                if port_text != "*" {
                    mp.port = Some(parse_port(port_text).ok_or(err("invalid port"))?);
                }
            } else {
                // No port in the pattern: any port.
            }
        }

        if host == "*" {
            mp.any_host = true;
        } else if let Some(domain) = host.strip_prefix("*.") {
            mp.subdomains = true;
            mp.host = domain.to_ascii_lowercase();
        } else {
            mp.host = host.to_ascii_lowercase();
        }
        if !mp.any_host && (mp.host.is_empty() || mp.host.contains('*')) {
            return Err(err("invalid wildcard in host"));
        }
        Ok(mp)
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn matches(&self, url: &str) -> bool {
        split_url(url).is_some_and(|u| self.matches_parsed(&u))
    }

    pub fn matches_parsed(&self, url: &ParsedUrl) -> bool {
        if !self.schemes.contains(&url.scheme.as_str()) {
            return false;
        }
        if url.scheme == "file" {
            return self.all_urls || glob_match(&self.path, &url.path_and_query, false);
        }
        if url.host.is_empty() {
            return false;
        }
        if !self.any_host {
            let exact = url.host == self.host;
            let sub = url.host.len() > self.host.len() + 1
                && url.host.ends_with(&self.host)
                && url.host.as_bytes()[url.host.len() - self.host.len() - 1] == b'.';
            if self.subdomains {
                if !exact && !sub {
                    return false;
                }
            } else if !exact {
                return false;
            }
        }
        if let Some(port) = self.port
            && url.port.or_else(|| default_port(&url.scheme)) != Some(port)
        {
            return false;
        }
        glob_match(&self.path, &url.path_and_query, false)
    }

    /// `<all_urls>` or a pattern with a wildcard host, port and path (e.g. `*://*/*` or `https://*/*`), which browsers treat
    /// as access to all websites.
    pub fn grants_all_hosts(&self) -> bool {
        self.all_urls || (self.any_host && self.port.is_none() && self.path == "/*")
    }
}

#[derive(Debug, Clone, Default)]
pub struct MatchPatternSet {
    patterns: Vec<MatchPattern>,
}

impl MatchPatternSet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, pattern: &str, dialect: MatchDialect) -> Result<(), MatchPatternError> {
        self.patterns.push(MatchPattern::parse(pattern, dialect)?);
        Ok(())
    }

    pub fn from_patterns<'a>(patterns: impl IntoIterator<Item = &'a str>, dialect: MatchDialect) -> Result<Self, MatchPatternError> {
        let mut set = Self::new();
        for p in patterns {
            set.add(p, dialect)?;
        }
        Ok(set)
    }

    pub fn matches(&self, url: &str) -> bool {
        split_url(url).is_some_and(|u| self.matches_parsed(&u))
    }

    pub fn matches_parsed(&self, url: &ParsedUrl) -> bool {
        self.patterns.iter().any(|p| p.matches_parsed(url))
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    pub fn grants_all_hosts(&self) -> bool {
        self.patterns.iter().any(MatchPattern::grants_all_hosts)
    }

    pub fn patterns(&self) -> &[MatchPattern] {
        &self.patterns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pat(s: &str) -> MatchPattern {
        MatchPattern::parse(s, MatchDialect::Chrome).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn documented_examples() {
        // Examples from the MDN / Chrome match-pattern documentation.
        let p = pat("https://*/path");
        assert!(p.matches("https://www.google.com/path"));
        assert!(p.matches("https://mozilla.org/path"));
        assert!(!p.matches("http://mozilla.org/path"));
        assert!(!p.matches("https://mozilla.org/path/"));
        assert!(!p.matches("https://mozilla.org/a"));
        assert!(!p.matches("https://mozilla.org/"));

        let p = pat("https://*/path/");
        assert!(p.matches("https://mozilla.org/path/"));
        assert!(!p.matches("https://mozilla.org/path"));

        let p = pat("https://mozilla.org/*");
        assert!(p.matches("https://mozilla.org/"));
        assert!(p.matches("https://mozilla.org/path"));
        assert!(p.matches("https://mozilla.org/another"));
        assert!(p.matches("https://mozilla.org/path/to/doc"));
        assert!(!p.matches("http://mozilla.org/"));
        assert!(!p.matches("https://mozilla.com/path"));

        let p = pat("https://*.mozilla.org/*");
        assert!(p.matches("https://mozilla.org/"));
        assert!(p.matches("https://www.mozilla.org/"));
        assert!(p.matches("https://developer.mozilla.org/en-US/"));
        assert!(!p.matches("https://evilmozilla.org/"));
        assert!(!p.matches("https://mozilla.org.evil.com/"));

        let p = pat("*://mozilla.org/");
        assert!(p.matches("http://mozilla.org/"));
        assert!(p.matches("https://mozilla.org/"));
        assert!(!p.matches("ws://mozilla.org/"));
        assert!(!p.matches("ftp://mozilla.org/"));

        // Firefox also accepts a wildcard host for file: patterns; real extensions use it.
        assert!(pat("file://*/*").matches("file:///etc/hosts"));
        assert!(pat("file://*/foo*").matches("file:///foo/bar"));
        assert!(!pat("file://*/foo*").matches("file:///bar"));

        let p = pat("file:///foo*");
        assert!(p.matches("file:///foo/bar.html"));
        assert!(p.matches("file:///foo"));
        assert!(!p.matches("file:///bar"));
    }

    #[test]
    fn all_urls() {
        let p = pat("<all_urls>");
        assert!(p.matches("http://a.com/"));
        assert!(p.matches("https://a.com/x?y=1"));
        assert!(p.matches("file:///etc/hosts"));
        assert!(p.matches("ftp://a.com/"));
        assert!(!p.matches("about:blank"));
        assert!(!p.matches("data:text/html,hi"));
        assert!(!p.matches("chrome://settings/"));
        assert!(p.grants_all_hosts());
        assert!(pat("*://*/*").grants_all_hosts());
        assert!(pat("https://*/*").grants_all_hosts());
        assert!(!pat("https://*/path").grants_all_hosts());
        assert!(!pat("*://example.com/*").grants_all_hosts());
    }

    #[test]
    fn query_matched_fragment_ignored() {
        let p = pat("https://a.com/p?x=*");
        assert!(p.matches("https://a.com/p?x=1"));
        assert!(!p.matches("https://a.com/p"));
        assert!(pat("https://a.com/*").matches("https://a.com/p#frag"));
        assert!(pat("https://a.com/p").matches("https://a.com/p#frag"));
        assert!(pat("https://a.com/*").matches("https://a.com"));
    }

    #[test]
    fn userinfo_ports_and_case() {
        assert!(pat("https://a.com/*").matches("https://user:pw@A.COM/x"));
        assert!(pat("https://a.com/*").matches("https://a.com:8443/x"));
        assert!(pat("https://a.com:8443/*").matches("https://a.com:8443/x"));
        assert!(!pat("https://a.com:8443/*").matches("https://a.com/x"));
        assert!(pat("https://a.com:443/*").matches("https://a.com/x"));
        assert!(pat("*://localhost:*/*").matches("http://localhost:3000/"));
        assert!(pat("*://[::1]/*").matches("http://[::1]:8080/"));
    }

    #[test]
    fn dialects() {
        assert!(!MatchPattern::parse("*://a.com/*", MatchDialect::Chrome).unwrap().matches("wss://a.com/"));
        assert!(MatchPattern::parse("*://a.com/*", MatchDialect::Firefox).unwrap().matches("wss://a.com/"));
    }

    #[test]
    fn invalid_patterns() {
        for bad in [
            "",
            "example.com",
            "https://",
            "https://example.com",
            "foo://a.com/*",
            "https://a*.com/*",
            "https://*.*.com/*",
            "file://host/*",
            "https://a.com:99999/*",
            "https:///x",
        ] {
            assert!(MatchPattern::parse(bad, MatchDialect::Chrome).is_err(), "{bad:?} should be invalid");
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "", false));
        assert!(glob_match("a*c", "abbbc", false));
        assert!(glob_match("a?c", "abc", false));
        assert!(!glob_match("a?c", "ac", false));
        assert!(glob_match("*.js", "x/y/z.js", false));
        assert!(!glob_match("A", "a", false));
        assert!(glob_match("A", "a", true));
        assert!(glob_match("*a*a*a*a*b", &"a".repeat(30).chars().chain("b".chars()).collect::<String>(), false));
        assert!(!glob_match("*a*a*a*a*c", &"a".repeat(2000), false));
    }

    #[test]
    fn set_semantics() {
        let set = MatchPatternSet::from_patterns(["https://a.com/*", "http://b.com/*"], MatchDialect::Chrome).unwrap();
        assert!(set.matches("http://b.com/x"));
        assert!(!set.matches("http://c.com/x"));
        assert!(MatchPatternSet::new().is_empty());
    }
}
