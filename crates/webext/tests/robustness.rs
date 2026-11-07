//! Deterministic fuzz-style tests: every parser that sees untrusted input must reject garbage without panicking.
//! (A panic in the browser process is a denial of service, so "returns an error" is the only acceptable failure mode.)

use webext::analyze::scan_api_chains;
use webext::content_scripts::{Target, matching_content_scripts};
use webext::i18n::I18n;
use webext::loader::{Files, load_extension};
use webext::manifest::parse_manifest;
use webext::match_pattern::{MatchDialect, MatchPattern, glob_match, split_url};
use webext::package::{Package, parse_crx_header};
use webext::permissions::PermissionRequest;
use webext::zip::ZipArchive;

/// xorshift64*: deterministic, so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        (0..self.below(max + 1)).map(|_| self.next() as u8).collect()
    }
}

/// Text built from fragments that stress parsers: multi-byte characters, quotes, escapes, JS keywords, URL syntax.
fn nasty_text(rng: &mut Rng) -> String {
    const PIECES: &[&str] = &[
        "é",
        "日本語",
        "😀",
        "\u{0}",
        "\u{feff}",
        "\"",
        "\\",
        "\\u",
        "\\ud800",
        "{",
        "}",
        "[",
        "]",
        ",",
        ":",
        "//",
        "/*",
        "*/",
        "\n",
        " ",
        "const ",
        "let ",
        "var ",
        "chrome",
        "browser",
        ".",
        "?.",
        "=",
        ";",
        "storage",
        "local",
        "get",
        "(",
        ")",
        "https://",
        "://",
        "*",
        "?",
        "<all_urls>",
        "file://",
        "@",
        "[::1]",
        ":99999",
        "manifest_version",
        "3",
        "name",
        "version",
        "permissions",
        "content_scripts",
        "matches",
        "-1",
        "1e999",
        "0.0",
        "true",
        "null",
    ];
    (0..rng.below(40)).map(|_| PIECES[rng.below(PIECES.len())]).collect()
}

#[test]
fn text_parsers_never_panic() {
    let mut rng = Rng(0x1234_5678_9abc_def1);
    for _ in 0..20_000 {
        let text = nasty_text(&mut rng);
        let _ = parse_manifest(&text);
        let _ = scan_api_chains(&text);
        let _ = split_url(&text);
        let _ = MatchPattern::parse(&text, MatchDialect::Chrome);
        let _ = MatchPattern::parse(&text, MatchDialect::Firefox);
        let _ = glob_match(&text, &nasty_text(&mut rng), true);
        let _ = PermissionRequest::from_json(&serde_json::Value::String(text.clone()));
        let mut i18n = I18n::new("id", "en", "pt-BR");
        let _ = i18n.add_locale("en", &text);
        let _ = i18n.localize(&text);
        let _ = i18n.get_message(&text, std::slice::from_ref(&text));
    }
}

#[test]
fn manifest_shaped_garbage_never_panics() {
    // Valid JSON of the right overall shape but wrong types and odd values, which is where field handling bugs hide.
    let mut rng = Rng(42);
    let values = [
        "null",
        "true",
        "1",
        "-5",
        "1e999",
        "\"x\"",
        "[]",
        "{}",
        "[1,\"a\",null]",
        "{\"a\":1}",
        "[[],[]]",
        "\"__MSG_x__\"",
        "\"<all_urls>\"",
    ];
    let keys = [
        "manifest_version",
        "name",
        "version",
        "permissions",
        "optional_permissions",
        "host_permissions",
        "background",
        "content_scripts",
        "action",
        "browser_action",
        "web_accessible_resources",
        "content_security_policy",
        "options_ui",
        "commands",
        "declarative_net_request",
        "icons",
        "browser_specific_settings",
        "applications",
        "chrome_url_overrides",
        "key",
        "default_locale",
        "side_panel",
        "sidebar_action",
    ];
    for _ in 0..20_000 {
        let mut fields = vec![];
        if rng.below(4) != 0 {
            fields.push(format!("\"manifest_version\":{}", 2 + rng.below(2)));
        }
        for _ in 0..rng.below(8) {
            fields.push(format!("\"{}\":{}", keys[rng.below(keys.len())], values[rng.below(values.len())]));
        }
        let _ = parse_manifest(&format!("{{{}}}", fields.join(",")));
    }
}

#[test]
fn binary_parsers_never_panic_on_random_bytes() {
    let mut rng = Rng(7);
    for _ in 0..20_000 {
        let data = rng.bytes(300);
        let _ = ZipArchive::open(data.clone());
        let _ = parse_crx_header(&data);
        let _ = Package::open(data.clone());
        let mut with_magic = b"Cr24".to_vec();
        with_magic.extend_from_slice(&[(2 + rng.below(2)) as u8, 0, 0, 0]);
        with_magic.extend_from_slice(&data);
        let _ = Package::open(with_magic);
    }
}

/// Builds a small valid zip so mutations start from structure the parser accepts.
fn sample_zip() -> Vec<u8> {
    let files: [(&str, &[u8]); 2] =
        [("manifest.json", br#"{"manifest_version":3,"name":"x","version":"1"}"#), ("a/b.js", b"chrome.storage.local.get('k')")];
    let mut out = vec![];
    let mut central = vec![];
    for (name, content) in files {
        let crc = crc32fast::hash(content);
        let offset = out.len() as u32;
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(content);
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&(content.len() as u32).to_le_bytes());
        central.extend_from_slice(&(content.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let cd_offset = out.len() as u32;
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 2, 0, 2, 0]);
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

#[test]
fn mutated_valid_archives_never_panic() {
    let base = sample_zip();
    assert!(Package::open(base.clone()).is_ok(), "the unmutated sample must be valid");
    let mut rng = Rng(99);
    for _ in 0..30_000 {
        let mut data = base.clone();
        match rng.below(4) {
            0 => data.truncate(rng.below(data.len() + 1)),
            1 => {
                for _ in 0..1 + rng.below(6) {
                    let i = rng.below(data.len());
                    data[i] = rng.next() as u8;
                }
            }
            2 => {
                let i = rng.below(data.len());
                data.insert(i, rng.next() as u8);
            }
            _ => {
                let i = rng.below(data.len());
                data.remove(i);
            }
        }
        if let Ok(pkg) = Package::open(data) {
            // Whatever opens must also be readable without panicking.
            if let Ok(files) = Files::from_package(&pkg) {
                let _ = load_extension(&files, None);
            }
        }
    }
}

#[test]
fn content_script_matching_never_panics_on_odd_urls() {
    let manifest = parse_manifest(
        r#"{"manifest_version":3,"name":"x","version":"1","content_scripts":[{"matches":["<all_urls>"],"js":["a.js"],"include_globs":["*a*?b*"],"all_frames":true,"match_about_blank":true}]}"#,
    )
    .manifest
    .unwrap();
    let mut rng = Rng(5);
    for _ in 0..20_000 {
        let url = nasty_text(&mut rng);
        let fallback = nasty_text(&mut rng);
        let target = Target { url: &url, is_top_frame: rng.below(2) == 0, fallback_url: Some(&fallback) };
        let _ = matching_content_scripts(&manifest, &target);
    }
}

#[test]
fn deeply_nested_json_is_rejected_not_overflowed() {
    for depth in [10, 200, 5_000, 100_000] {
        let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        // serde_json enforces a recursion limit; either outcome is fine as long as the process survives.
        let _ = parse_manifest(&text);
        let _ = parse_manifest(&format!("{{\"manifest_version\":3,\"name\":\"x\",\"version\":\"1\",\"content_scripts\":{text}}}"));
    }
}
