//! Access control for an extension's files: which requests may load which resource.

use crate::manifest::Manifest;
use crate::match_pattern::{MatchDialect, MatchPatternSet, glob_match};

/// Who is asking for a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Initiator<'a> {
    /// A page or script of the same extension; always allowed.
    SameExtension,
    /// A web page (or frame) at this URL.
    Web(&'a str),
    /// Another extension.
    OtherExtension(&'a str),
}

fn normalize(path: &str) -> &str {
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.trim_start_matches('/')
}

/// Whether `initiator` may load `path` from the extension, per `web_accessible_resources`. Resource entries are globs
/// where `*` matches any run of characters.
pub fn is_web_accessible(manifest: &Manifest, path: &str, initiator: &Initiator<'_>) -> bool {
    if *initiator == Initiator::SameExtension {
        return true;
    }
    let path = normalize(path);
    manifest.web_accessible_resources.iter().any(|entry| {
        if !entry.resources.iter().any(|g| glob_match(normalize(g), path, false)) {
            return false;
        }
        match initiator {
            Initiator::SameExtension => true,
            Initiator::Web(url) => {
                MatchPatternSet::from_patterns(entry.matches.iter().map(String::as_str), MatchDialect::Chrome).is_ok_and(|s| s.matches(url))
            }
            Initiator::OtherExtension(id) => entry.extension_ids.iter().any(|e| e == "*" || e == id),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn mv3() -> Manifest {
        parse_manifest(
            r#"{"manifest_version":3,"name":"x","version":"1","web_accessible_resources":[
                {"resources":["img/*.png","inject.js"],"matches":["https://*.a.com/*"]},
                {"resources":["shared/*"],"extension_ids":["abcdefghijklmnopabcdefghijklmnop"]}]}"#,
        )
        .manifest
        .unwrap()
    }

    #[test]
    fn web_pages_need_a_matching_entry() {
        let m = mv3();
        assert!(is_web_accessible(&m, "img/logo.png", &Initiator::Web("https://www.a.com/p")));
        assert!(is_web_accessible(&m, "/inject.js", &Initiator::Web("https://a.com/")));
        assert!(!is_web_accessible(&m, "img/logo.png", &Initiator::Web("https://evil.com/")));
        assert!(!is_web_accessible(&m, "secret.js", &Initiator::Web("https://a.com/")));
        assert!(!is_web_accessible(&m, "img/logo.svg", &Initiator::Web("https://a.com/")));
        assert!(!is_web_accessible(&m, "shared/x", &Initiator::Web("https://a.com/")));
    }

    #[test]
    fn query_and_leading_slash_are_ignored() {
        let m = mv3();
        assert!(is_web_accessible(&m, "/inject.js?v=3#frag", &Initiator::Web("https://a.com/")));
    }

    #[test]
    fn other_extensions_by_id() {
        let m = mv3();
        assert!(is_web_accessible(&m, "shared/lib.js", &Initiator::OtherExtension("abcdefghijklmnopabcdefghijklmnop")));
        assert!(!is_web_accessible(&m, "shared/lib.js", &Initiator::OtherExtension("ponmlkjihgfedcbaponmlkjihgfedcba")));
        assert!(!is_web_accessible(&m, "img/a.png", &Initiator::OtherExtension("abcdefghijklmnopabcdefghijklmnop")));
    }

    #[test]
    fn same_extension_always_allowed_and_default_is_closed() {
        let m = mv3();
        assert!(is_web_accessible(&m, "anything/at/all", &Initiator::SameExtension));
        let none = parse_manifest(r#"{"manifest_version":3,"name":"x","version":"1"}"#).manifest.unwrap();
        assert!(!is_web_accessible(&none, "a.js", &Initiator::Web("https://a.com/")));
    }

    #[test]
    fn mv2_list_is_open_to_all_web_pages() {
        let m = parse_manifest(r#"{"manifest_version":2,"name":"x","version":"1","web_accessible_resources":["pub/*"]}"#).manifest.unwrap();
        assert!(is_web_accessible(&m, "pub/a.png", &Initiator::Web("https://anywhere.com/")));
        assert!(!is_web_accessible(&m, "priv/a.png", &Initiator::Web("https://anywhere.com/")));
    }
}
