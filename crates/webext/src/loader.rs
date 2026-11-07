//! Turns a package or an unpacked directory into a validated, ready-to-install extension.

use crate::encoding::{base64_decode, chrome_extension_id_from_public_key};
use crate::i18n::I18n;
use crate::manifest::{BackgroundKind, Diagnostic, Manifest, Severity, parse_manifest};
use crate::package::{Package, PackageError};
use std::collections::BTreeMap;
use std::io;
use std::path::Path;

pub const MAX_TOTAL_BYTES: usize = 512 * 1024 * 1024;

/// All files of an extension, keyed by forward-slash relative path.
#[derive(Debug, Default, Clone)]
pub struct Files {
    map: BTreeMap<String, Vec<u8>>,
}

impl Files {
    pub fn new() -> Files {
        Files::default()
    }

    pub fn insert(&mut self, path: &str, data: Vec<u8>) {
        self.map.insert(path.to_string(), data);
    }

    pub fn from_package(pkg: &Package) -> Result<Files, PackageError> {
        let mut files = Files::new();
        let mut total = 0usize;
        for entry in pkg.archive().entries().iter().filter(|e| !e.is_directory()) {
            total = total.saturating_add(entry.uncompressed_size as usize);
            if total > MAX_TOTAL_BYTES {
                return Err(PackageError::Zip(crate::zip::ZipError::TooLarge));
            }
            files.map.insert(entry.name.clone(), pkg.archive().read_entry(entry, crate::zip::DEFAULT_MAX_ENTRY_SIZE)?);
        }
        Ok(files)
    }

    /// Reads an unpacked extension directory. Symlinks are skipped so that a directory cannot point outside itself.
    pub fn from_dir(root: &Path) -> io::Result<Files> {
        fn walk(root: &Path, dir: &Path, files: &mut Files, total: &mut usize) -> io::Result<()> {
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                let meta = std::fs::symlink_metadata(entry.path())?;
                if meta.file_type().is_symlink() {
                    continue;
                }
                if meta.is_dir() {
                    walk(root, &entry.path(), files, total)?;
                } else if meta.is_file() {
                    *total = total.saturating_add(meta.len() as usize);
                    if *total > MAX_TOTAL_BYTES {
                        return Err(io::Error::other("extension directory is too large"));
                    }
                    let rel = entry
                        .path()
                        .strip_prefix(root)
                        .map_err(io::Error::other)?
                        .components()
                        .map(|c| c.as_os_str().to_string_lossy().into_owned())
                        .collect::<Vec<_>>()
                        .join("/");
                    files.map.insert(rel, std::fs::read(entry.path())?);
                }
            }
            Ok(())
        }
        let mut files = Files::new();
        let mut total = 0;
        walk(root, root, &mut files, &mut total)?;
        Ok(files)
    }

    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.map.get(normalize_path(path).as_str()).map(Vec::as_slice)
    }

    pub fn text(&self, path: &str) -> Option<String> {
        self.get(path).map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn contains(&self, path: &str) -> bool {
        self.get(path).is_some()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.map.iter().map(|(k, v)| (k.as_str(), v.as_slice()))
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Strips a leading `/` or `./` as allowed in manifest paths, and any query/fragment.
pub fn normalize_path(p: &str) -> String {
    let p = p.split(['?', '#']).next().unwrap_or("");
    let mut p = p;
    loop {
        if let Some(rest) = p.strip_prefix("./") {
            p = rest;
        } else if let Some(rest) = p.strip_prefix('/') {
            p = rest;
        } else {
            break;
        }
    }
    p.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdSource {
    /// Derived from the manifest `key` (Chrome).
    ManifestKey,
    /// Declared in the CRX header (Chrome).
    CrxHeader,
    /// `browser_specific_settings.gecko.id` (Firefox).
    GeckoId,
}

#[derive(Debug)]
pub struct LoadedExtension {
    /// `None` for unpacked extensions with neither a key nor a gecko id; the host assigns one.
    pub id: Option<String>,
    pub id_source: Option<IdSource>,
    pub manifest: Manifest,
    /// The name after resolving `__MSG_*__` with the default locale.
    pub display_name: String,
    /// `(locale, messages.json contents)` for `Dispatcher::install`.
    pub locales: Vec<(String, String)>,
    pub diagnostics: Vec<Diagnostic>,
}

fn diag(severity: Severity, field: &str, message: String) -> Diagnostic {
    Diagnostic { severity, field: field.into(), message }
}

pub fn load_extension(files: &Files, crx_extension_id: Option<&str>) -> Result<LoadedExtension, Vec<Diagnostic>> {
    let Some(text) = files.text("manifest.json") else {
        return Err(vec![diag(Severity::Error, "manifest.json", "the package has no manifest.json".into())]);
    };
    let parsed = parse_manifest(&text);
    let mut diagnostics = parsed.diagnostics;
    let Some(manifest) = parsed.manifest else { return Err(diagnostics) };

    // Locales.
    let mut locales = vec![];
    for name in files.names() {
        if let Some(rest) = name.strip_prefix("_locales/")
            && let Some((locale, "messages.json")) = rest.split_once('/')
        {
            locales.push((locale.to_string(), files.text(name).unwrap_or_default()));
        }
    }
    if !locales.is_empty() && manifest.default_locale.is_empty() {
        diagnostics.push(diag(
            Severity::Error,
            "default_locale",
            "the package has _locales but the manifest declares no default_locale".into(),
        ));
    }
    if !manifest.default_locale.is_empty() && !files.contains(&format!("_locales/{}/messages.json", manifest.default_locale)) {
        diagnostics.push(diag(Severity::Error, "default_locale", format!("_locales/{}/messages.json is missing", manifest.default_locale)));
    }

    // Referenced files.
    let mut require = |field: &str, path: &str, severity: Severity| {
        if !path.is_empty() && !files.contains(path) {
            diagnostics.push(diag(severity, field, format!("file '{path}' is referenced but missing from the package")));
        }
    };
    for s in &manifest.background.scripts {
        require("background.scripts", s, Severity::Error);
    }
    require("background.service_worker", &manifest.background.service_worker, Severity::Error);
    require("background.page", &manifest.background.page, Severity::Error);
    for (i, cs) in manifest.content_scripts.iter().enumerate() {
        for f in &cs.js {
            require(&format!("content_scripts[{i}].js"), f, Severity::Error);
        }
        for f in &cs.css {
            require(&format!("content_scripts[{i}].css"), f, Severity::Error);
        }
    }
    if let Some(a) = &manifest.action {
        require("action.default_popup", &a.default_popup, Severity::Error);
        for (_, icon) in &a.default_icon.0 {
            require("action.default_icon", icon, Severity::Warning);
        }
    }
    for (_, icon) in &manifest.icons.0 {
        require("icons", icon, Severity::Warning);
    }
    require("options_page", &manifest.options_page, Severity::Error);
    for r in &manifest.declarative_net_request_rulesets {
        require("declarative_net_request.rule_resources", &r.path, Severity::Error);
    }
    if manifest.background.kind == BackgroundKind::None
        && manifest.content_scripts.is_empty()
        && manifest.action.is_none()
        && manifest.options_page.is_empty()
        && manifest.declarative_net_request_rulesets.is_empty()
    {
        diagnostics.push(diag(
            Severity::Warning,
            "manifest.json",
            "the extension declares no background, content scripts, action, options or rulesets and will do nothing".into(),
        ));
    }

    // Identity.
    let (id, id_source) = if let Some(key) = Some(&manifest.key).filter(|k| !k.is_empty()) {
        match base64_decode(key) {
            Some(der) => (Some(chrome_extension_id_from_public_key(&der)), Some(IdSource::ManifestKey)),
            None => {
                diagnostics.push(diag(Severity::Error, "key", "the key is not valid base64".into()));
                (None, None)
            }
        }
    } else if let Some(crx) = crx_extension_id {
        (Some(crx.to_string()), Some(IdSource::CrxHeader))
    } else if !manifest.gecko_id.is_empty() {
        (Some(manifest.gecko_id.clone()), Some(IdSource::GeckoId))
    } else {
        (None, None)
    };
    if let (Some(IdSource::ManifestKey), Some(crx), Some(key_id)) = (id_source, crx_extension_id, &id)
        && crx != key_id
    {
        diagnostics.push(diag(Severity::Error, "key", format!("the manifest key yields ID {key_id} but the CRX declares {crx}")));
    }

    if diagnostics.iter().any(|d| d.severity == Severity::Error) {
        return Err(diagnostics);
    }

    let mut i18n = I18n::new(id.as_deref().unwrap_or(""), &manifest.default_locale, &manifest.default_locale);
    for (locale, text) in &locales {
        if let Err(e) = i18n.add_locale(locale, text) {
            diagnostics.push(diag(Severity::Warning, "_locales", e));
        }
    }
    let display_name = i18n.localize(&manifest.name);

    Ok(LoadedExtension { id, id_source, manifest, display_name, locales, diagnostics })
}

/// Convenience: parse a package file's bytes and load it, using the CRX3 header's ID when present.
pub fn load_package(data: Vec<u8>) -> Result<LoadedExtension, Vec<Diagnostic>> {
    let pkg = Package::open(data).map_err(|e| vec![diag(Severity::Error, "package", e.to_string())])?;
    let files = Files::from_package(&pkg).map_err(|e| vec![diag(Severity::Error, "package", e.to_string())])?;
    let crx_id = pkg.crx_header().and_then(|h| h.extension_id());
    load_extension(&files, crx_id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zip::testutil::build_zip;

    fn files(list: &[(&str, &str)]) -> Files {
        let mut f = Files::new();
        for (p, c) in list {
            f.insert(p, c.as_bytes().to_vec());
        }
        f
    }

    const MV3: &str = r#"{"manifest_version":3,"name":"__MSG_name__","version":"1.0","default_locale":"en",
        "background":{"service_worker":"sw.js"},
        "content_scripts":[{"matches":["https://*/*"],"js":["cs.js"],"css":["cs.css"]}],
        "action":{"default_popup":"popup.html","default_icon":"icon.png"}}"#;

    #[test]
    fn complete_extension_loads() {
        let f = files(&[
            ("manifest.json", MV3),
            ("sw.js", ""),
            ("cs.js", ""),
            ("cs.css", ""),
            ("popup.html", ""),
            ("icon.png", ""),
            ("_locales/en/messages.json", r#"{"name":{"message":"My Extension"}}"#),
            ("_locales/de/messages.json", r#"{"name":{"message":"Meine Erweiterung"}}"#),
        ]);
        let e = load_extension(&f, None).unwrap();
        assert_eq!(e.display_name, "My Extension");
        assert_eq!(e.locales.len(), 2);
        assert!(e.id.is_none());
        assert!(e.diagnostics.iter().all(|d| d.severity == Severity::Warning));
    }

    #[test]
    fn missing_files_are_errors() {
        let f = files(&[("manifest.json", MV3), ("_locales/en/messages.json", "{}")]);
        let errs = load_extension(&f, None).unwrap_err();
        let fields: Vec<_> = errs.iter().filter(|d| d.severity == Severity::Error).map(|d| d.field.as_str()).collect();
        for expected in ["background.service_worker", "content_scripts[0].js", "content_scripts[0].css", "action.default_popup"] {
            assert!(fields.contains(&expected), "{expected} not reported in {fields:?}");
        }
    }

    #[test]
    fn locale_consistency() {
        let m = r#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"sw.js"}}"#;
        let errs = load_extension(&files(&[("manifest.json", m), ("sw.js", ""), ("_locales/en/messages.json", "{}")]), None).unwrap_err();
        assert!(errs.iter().any(|d| d.field == "default_locale"));
        let m2 = r#"{"manifest_version":3,"name":"x","version":"1","default_locale":"fr","background":{"service_worker":"sw.js"}}"#;
        let errs = load_extension(&files(&[("manifest.json", m2), ("sw.js", "")]), None).unwrap_err();
        assert!(errs.iter().any(|d| d.field == "default_locale"));
    }

    #[test]
    fn missing_or_broken_manifest() {
        assert!(load_extension(&Files::new(), None).is_err());
        assert!(load_extension(&files(&[("manifest.json", "{")]), None).is_err());
    }

    #[test]
    fn identity_sources() {
        let key = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA";
        let m = format!(r#"{{"manifest_version":3,"name":"x","version":"1","key":"{key}","background":{{"service_worker":"s.js"}}}}"#);
        let e = load_extension(&files(&[("manifest.json", &m), ("s.js", "")]), None).unwrap();
        assert_eq!(e.id_source, Some(IdSource::ManifestKey));
        let der = base64_decode(key).unwrap();
        assert_eq!(e.id.unwrap(), chrome_extension_id_from_public_key(&der));

        let g = r#"{"manifest_version":2,"name":"x","version":"1","browser_specific_settings":{"gecko":{"id":"x@y.z"}},"background":{"scripts":["b.js"]}}"#;
        let e = load_extension(&files(&[("manifest.json", g), ("b.js", "")]), None).unwrap();
        assert_eq!((e.id.as_deref(), e.id_source), (Some("x@y.z"), Some(IdSource::GeckoId)));

        let e = load_extension(&files(&[("manifest.json", g), ("b.js", "")]), Some("abcdefghijklmnopabcdefghijklmnop")).unwrap();
        assert_eq!(e.id_source, Some(IdSource::CrxHeader));

        // A key that disagrees with the CRX is rejected.
        let errs = load_extension(&files(&[("manifest.json", &m), ("s.js", "")]), Some("abcdefghijklmnopabcdefghijklmnop")).unwrap_err();
        assert!(errs.iter().any(|d| d.field == "key"));
        let bad = r#"{"manifest_version":3,"name":"x","version":"1","key":"$$$","background":{"service_worker":"s.js"}}"#;
        assert!(load_extension(&files(&[("manifest.json", bad), ("s.js", "")]), None).is_err());
    }

    #[test]
    fn path_normalisation() {
        let f = files(&[("manifest.json", "{}"), ("a/b.js", "x")]);
        assert!(f.contains("/a/b.js") && f.contains("./a/b.js") && f.contains("a/b.js?v=1"));
        assert!(!f.contains("a/c.js"));
    }

    #[test]
    fn load_from_zip_bytes() {
        let zip = build_zip(
            &[
                ("manifest.json", br#"{"manifest_version":3,"name":"x","version":"1","background":{"service_worker":"s.js"}}"#),
                ("s.js", b""),
            ],
            true,
        );
        let e = load_package(zip).unwrap();
        assert_eq!(e.manifest.name, "x");
        assert!(load_package(vec![1, 2, 3]).is_err());
    }

    #[test]
    fn directory_loading_skips_symlinks() {
        let dir = std::env::temp_dir().join(format!("webext-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("manifest.json"), "{}").unwrap();
        std::fs::write(dir.join("sub/a.js"), "x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.join("evil")).unwrap();
        let f = Files::from_dir(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(f.contains("manifest.json") && f.contains("sub/a.js"));
        assert!(!f.contains("evil"));
        assert_eq!(f.len(), 2);
    }
}
