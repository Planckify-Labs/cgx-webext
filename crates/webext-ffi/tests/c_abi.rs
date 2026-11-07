//! Builds the static library, compiles tests/smoke.c against include/webext.h with the system C compiler, and runs it.
//! Also keeps the header and the Rust exports in sync.

use std::path::{Path, PathBuf};
use std::process::Command;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok() || Command::new(tool).arg("-v").output().is_ok()
}

#[test]
fn header_declares_every_export_and_nothing_else() {
    let src = std::fs::read_to_string(crate_dir().join("src/lib.rs")).unwrap();
    let header = std::fs::read_to_string(crate_dir().join("include/webext.h")).unwrap();
    let mut exported: Vec<String> = src
        .split("pub unsafe extern \"C\" fn ")
        .skip(1)
        .chain(src.split("pub extern \"C\" fn ").skip(1))
        .map(|rest| rest.split('(').next().unwrap().to_string())
        .collect();
    exported.sort();
    exported.dedup();
    assert!(exported.len() >= 14, "found only {exported:?}");

    let mut declared: Vec<String> = header
        .lines()
        .filter(|l| !l.trim_start().starts_with(['*', '/']) && l.contains("webext_") && l.contains('('))
        .filter_map(|l| l.split("webext_").nth(1).and_then(|r| r.split('(').next()).map(|n| format!("webext_{n}")))
        .collect();
    declared.sort();
    declared.dedup();
    assert_eq!(exported, declared, "include/webext.h and src/lib.rs disagree");
}

#[test]
fn abi_version_matches_header() {
    let header = std::fs::read_to_string(crate_dir().join("include/webext.h")).unwrap();
    assert!(header.contains(&format!("#define WEBEXT_ABI_VERSION {}u", webext_ffi::ABI_VERSION)));
}

#[test]
fn c_program_drives_the_library() {
    if !have("cc") || !have("zip") {
        eprintln!("SKIPPED: needs `cc` and `zip` on PATH");
        return;
    }
    let tmp = std::env::temp_dir().join(format!("webext-ffi-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let pkg_dir = tmp.join("pkg");
    std::fs::create_dir_all(pkg_dir.join("public")).unwrap();
    std::fs::write(
        pkg_dir.join("manifest.json"),
        r#"{"manifest_version":3,"name":"C ABI Test","version":"1.0","permissions":["storage","alarms"],"optional_permissions":["history"],
            "background":{"service_worker":"bg.js"},
            "content_scripts":[{"matches":["https://example.com/*"],"js":["content.js"]}],
            "web_accessible_resources":[{"resources":["public/*"],"matches":["https://example.com/*"]}]}"#,
    )
    .unwrap();
    std::fs::write(pkg_dir.join("bg.js"), "// bg").unwrap();
    std::fs::write(pkg_dir.join("page.html"), "<!doctype html><html><head></head><body>x</body></html>").unwrap();
    std::fs::write(pkg_dir.join("content.js"), "// content script").unwrap();
    std::fs::write(pkg_dir.join("public/logo.png"), "png").unwrap();
    let zip_path = tmp.join("ext.zip");
    let status = Command::new("zip").current_dir(&pkg_dir).args(["-q", "-r"]).arg(&zip_path).arg(".").status().unwrap();
    assert!(status.success());

    // Locate the target dir from this test binary: target/<profile>/deps/c_abi-hash.
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let release = profile_dir.file_name().is_some_and(|n| n == "release");
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "-q", "-p", "webext-ffi"]);
    if release {
        build.arg("--release");
    }
    assert!(build.status().unwrap().success(), "cargo build -p webext-ffi failed");
    let lib = profile_dir.join("libwebext_ffi.a");
    assert!(lib.exists(), "{} not found", lib.display());

    let exe_path = tmp.join("smoke");
    let out = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-g"])
        .arg("-I")
        .arg(crate_dir().join("include"))
        .arg(crate_dir().join("tests/smoke.c"))
        .arg(&lib)
        .args(["-lpthread", "-ldl", "-lm"])
        .arg("-o")
        .arg(&exe_path)
        .output()
        .unwrap();
    assert!(out.status.success(), "C compile failed:\n{}", String::from_utf8_lossy(&out.stderr));

    let run = Command::new(&exe_path).arg(&zip_path).arg(&pkg_dir).output().unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    assert!(
        run.status.success(),
        "smoke test failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "OK");
    let _: &Path = &crate_dir();
}
