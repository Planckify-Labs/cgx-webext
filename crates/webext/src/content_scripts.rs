//! Deciding which declared content scripts apply to a document.

use crate::manifest::{ContentScript, Manifest, RunAt, ScriptWorld};
use crate::match_pattern::{MatchDialect, MatchPatternSet, glob_match, split_url};
use serde_json::{Value, json};

/// A document the browser is about to load or has loaded.
#[derive(Debug, Clone)]
pub struct Target<'a> {
    pub url: &'a str,
    pub is_top_frame: bool,
    /// For `about:blank`, `srcdoc` and similar documents without a matchable URL of their own: the URL of the document
    /// that created them (or its origin). Used only by scripts that opt in via `match_about_blank` or
    /// `match_origin_as_fallback`.
    pub fallback_url: Option<&'a str>,
}

fn is_matchable(url: &str) -> bool {
    split_url(url).is_some_and(|u| !u.host.is_empty() || u.scheme == "file")
}

fn script_applies(script: &ContentScript, target: &Target<'_>) -> bool {
    if !target.is_top_frame && !script.all_frames {
        return false;
    }
    let url = if is_matchable(target.url) {
        target.url
    } else if (script.match_about_blank || script.match_origin_as_fallback) && target.fallback_url.is_some_and(is_matchable) {
        target.fallback_url.unwrap_or(target.url)
    } else {
        return false;
    };

    let Ok(includes) = MatchPatternSet::from_patterns(script.matches.iter().map(String::as_str), MatchDialect::Chrome) else {
        return false;
    };
    if !includes.matches(url) {
        return false;
    }
    if let Ok(excludes) = MatchPatternSet::from_patterns(script.exclude_matches.iter().map(String::as_str), MatchDialect::Chrome)
        && excludes.matches(url)
    {
        return false;
    }
    if !script.include_globs.is_empty() && !script.include_globs.iter().any(|g| glob_match(g, url, true)) {
        return false;
    }
    !script.exclude_globs.iter().any(|g| glob_match(g, url, true))
}

/// The content scripts that should run in `target`, in manifest order, with their index in `manifest.content_scripts`.
pub fn matching_content_scripts<'a>(manifest: &'a Manifest, target: &Target<'_>) -> Vec<(usize, &'a ContentScript)> {
    manifest.content_scripts.iter().enumerate().filter(|(_, s)| script_applies(s, target)).collect()
}

/// JSON for the host: `[{ index, js, css, runAt, world, allFrames }]`.
pub fn matching_content_scripts_json(manifest: &Manifest, target: &Target<'_>) -> Value {
    Value::Array(
        matching_content_scripts(manifest, target)
            .into_iter()
            .map(|(index, s)| {
                json!({
                    "index": index,
                    "js": s.js,
                    "css": s.css,
                    "runAt": match s.run_at { RunAt::DocumentStart => "document_start", RunAt::DocumentEnd => "document_end", RunAt::DocumentIdle => "document_idle" },
                    "world": match s.world { ScriptWorld::Main => "MAIN", ScriptWorld::Isolated => "ISOLATED" },
                    "allFrames": s.all_frames,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_manifest;

    fn manifest(scripts: &str) -> Manifest {
        parse_manifest(&format!(r#"{{"manifest_version":3,"name":"x","version":"1","content_scripts":{scripts}}}"#))
            .manifest
            .expect("valid")
    }

    fn top(url: &str) -> Target<'_> {
        Target { url, is_top_frame: true, fallback_url: None }
    }

    fn indexes(m: &Manifest, t: &Target<'_>) -> Vec<usize> {
        matching_content_scripts(m, t).into_iter().map(|(i, _)| i).collect()
    }

    #[test]
    fn match_and_exclude() {
        let m = manifest(
            r#"[{"matches":["https://*.a.com/*"],"exclude_matches":["https://private.a.com/*"],"js":["a.js"]},{"matches":["<all_urls>"],"js":["b.js"]}]"#,
        );
        assert_eq!(indexes(&m, &top("https://www.a.com/x")), [0, 1]);
        assert_eq!(indexes(&m, &top("https://private.a.com/x")), [1]);
        assert_eq!(indexes(&m, &top("https://other.com/")), [1]);
        assert!(indexes(&m, &top("about:blank")).is_empty());
        assert!(indexes(&m, &top("chrome://settings")).is_empty());
    }

    #[test]
    fn frames() {
        let m = manifest(
            r#"[{"matches":["https://a.com/*"],"js":["top.js"]},{"matches":["https://a.com/*"],"js":["all.js"],"all_frames":true}]"#,
        );
        let sub = Target { url: "https://a.com/frame", is_top_frame: false, fallback_url: None };
        assert_eq!(indexes(&m, &sub), [1]);
        assert_eq!(indexes(&m, &top("https://a.com/frame")), [0, 1]);
    }

    #[test]
    fn globs() {
        let m = manifest(
            r#"[{"matches":["https://a.com/*"],"include_globs":["*/docs/*","*/help"],"exclude_globs":["*draft*"],"js":["a.js"]}]"#,
        );
        assert_eq!(indexes(&m, &top("https://a.com/docs/intro")), [0]);
        assert_eq!(indexes(&m, &top("https://a.com/help")), [0]);
        assert!(indexes(&m, &top("https://a.com/blog")).is_empty());
        assert!(indexes(&m, &top("https://a.com/docs/draft-1")).is_empty());
        // ASSUMPTION: globs are matched case-insensitively. Verify against Chrome and Firefox before relying on it.
        assert_eq!(indexes(&m, &top("https://A.com/DOCS/x")), [0]);
    }

    #[test]
    fn about_blank_needs_opt_in_and_a_parent() {
        let m = manifest(
            r#"[{"matches":["https://a.com/*"],"js":["a.js"],"all_frames":true},{"matches":["https://a.com/*"],"js":["b.js"],"all_frames":true,"match_about_blank":true}]"#,
        );
        let blank = Target { url: "about:blank", is_top_frame: false, fallback_url: Some("https://a.com/page") };
        assert_eq!(indexes(&m, &blank), [1]);
        let orphan = Target { url: "about:blank", is_top_frame: false, fallback_url: None };
        assert!(indexes(&m, &orphan).is_empty());
        let wrong_parent = Target { url: "about:blank", is_top_frame: false, fallback_url: Some("https://evil.com/") };
        assert!(indexes(&m, &wrong_parent).is_empty());
    }

    #[test]
    fn json_shape() {
        let m = manifest(r#"[{"matches":["https://a.com/*"],"js":["a.js"],"css":["a.css"],"run_at":"document_start","world":"MAIN"}]"#);
        let j = matching_content_scripts_json(&m, &top("https://a.com/"));
        assert_eq!(
            j,
            json!([{"index": 0, "js": ["a.js"], "css": ["a.css"], "runAt": "document_start", "world": "MAIN", "allFrames": false}])
        );
    }
}
