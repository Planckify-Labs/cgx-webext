//! `webext-coverage`: inspect extensions and report how well this project supports them.

use std::path::Path;
use std::process::ExitCode;
use webext::analyze::{Verdict, analyze};
use webext::loader::{Files, load_extension};
use webext::manifest::Severity;
use webext::package::Package;
use webext::registry::{Engine, Registry};
use webext::render;

const USAGE: &str = "\
webext-coverage - how much of the Chrome/Firefox extension ecosystem do we support?

USAGE:
  webext-coverage registry [--engine chrome|firefox] [--json | --markdown]
      Coverage of the whole API surface, straight from Data/api-support.json.

  webext-coverage inspect <extension.crx|.xpi|.zip|directory>... [--json] [--fail-on-blocked]
      Static analysis of extensions: manifest problems, APIs used and their support status.
      Exit status is 1 if any extension fails to load, or (with --fail-on-blocked) is blocked.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("registry") => registry(&args[1..]),
        Some("inspect") => inspect(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown command '{other}'\n\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn registry(args: &[String]) -> ExitCode {
    let reg = Registry::builtin();
    let mut engine = None;
    let (mut json, mut markdown) = (false, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--markdown" => markdown = true,
            "--engine" => match it.next().map(String::as_str) {
                Some("chrome") => engine = Some(Engine::Chrome),
                Some("firefox") => engine = Some(Engine::Firefox),
                _ => {
                    eprintln!("--engine expects 'chrome' or 'firefox'");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("unknown option '{other}'");
                return ExitCode::from(2);
            }
        }
    }
    if markdown {
        print!("{}", render::registry_markdown(&reg));
    } else if json {
        println!("{}", serde_json::to_string_pretty(&render::registry_json(&reg)).expect("serialisable"));
    } else {
        print!("{}", render::registry_text(&reg, engine));
    }
    ExitCode::SUCCESS
}

fn load(path: &Path) -> Result<(Files, Option<String>), String> {
    if path.is_dir() {
        return Files::from_dir(path).map(|f| (f, None)).map_err(|e| e.to_string());
    }
    let data = std::fs::read(path).map_err(|e| e.to_string())?;
    let pkg = Package::open(data).map_err(|e| e.to_string())?;
    let crx_id = pkg.crx_header().and_then(|h| h.extension_id());
    Files::from_package(&pkg).map(|f| (f, crx_id)).map_err(|e| e.to_string())
}

fn inspect(args: &[String]) -> ExitCode {
    let json = args.iter().any(|a| a == "--json");
    let fail_on_blocked = args.iter().any(|a| a == "--fail-on-blocked");
    let paths: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    if paths.is_empty() {
        eprintln!("inspect needs at least one extension path\n\n{USAGE}");
        return ExitCode::from(2);
    }

    let reg = Registry::builtin();
    let mut failed = false;
    let mut json_out = vec![];
    for p in paths {
        let (files, crx_id) = match load(Path::new(p)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{p}: cannot read: {e}");
                failed = true;
                continue;
            }
        };
        match load_extension(&files, crx_id.as_deref()) {
            Err(diags) => {
                failed = true;
                eprintln!("{p}: cannot load:");
                for d in diags {
                    eprintln!("  {:?} {}: {}", d.severity, d.field, d.message);
                }
            }
            Ok(ext) => {
                let report = analyze(&reg, &ext.manifest, &files);
                if json {
                    json_out.push(render::report_json(p, &report));
                } else {
                    print!(
                        "{}",
                        render::report_text(
                            &format!("{p} ({} {}, MV{})", ext.display_name, ext.manifest.version, ext.manifest.manifest_version),
                            &report
                        )
                    );
                    for d in ext.diagnostics.iter().filter(|d| d.severity == Severity::Warning) {
                        println!("  warning {}: {}", d.field, d.message);
                    }
                    println!();
                }
                if fail_on_blocked && report.verdict == Verdict::Blocked {
                    failed = true;
                }
            }
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&json_out).expect("serialisable"));
    }
    if failed { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
