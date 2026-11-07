# Architecture

## The one rule

**The core decides; the browser does.** Everything that can be decided from an extension's files and an API call, without
knowing how a particular browser renders pages, is decided here, once, and tested without a browser. Everything that needs
the browser (opening a tab, running a script in a page, asking the user) is a callback.

```
 extension JS (background / popup / content script)
     │  chrome.storage.local.get(...)            ← Shim/webext-shim.js builds chrome.* / browser.*
     ▼
 bridge (browser-provided: IPC from the renderer to the Browser process)
     │  ApiCall JSON
     ▼
 ┌────────────────────────── webext (Rust core, Browser process) ──────────────────────────┐
 │ Dispatcher::dispatch                                                                      │
 │   1. registry lookup           unknown API?                    → UnknownApi               │
 │   2. manifest version          MV2-only API in an MV3 ext?     → NotAvailable             │
 │   3. status                    planned / unsupported?          → NotAvailable             │
 │   4. permission                needs "storage" and not granted → PermissionDenied         │
 │   5a. implemented/partial      handled here (storage, i18n, alarms, permissions, ...)     │
 │   5b. delegated                ExtensionHost::call_browser ──────────────► the browser    │
 └────────────────────────────────────────────────────────────────────────────────────────────┘
     │  result JSON / error
     ▼
 bridge → shim → Promise resolves / callback runs / chrome.runtime.lastError is set
```

Events travel the other way: the core (or the browser) calls `ExtensionHost::dispatch_event(extension, "storage.onChanged",
args)`, and the browser hands them to `api.dispatchEvent(name, args)` of the shim in every context of that extension.

## Crates

| Crate | Purpose | `unsafe` |
|---|---|---|
| `webext` | All logic. Depends only on `serde_json`, `sha2`, `base64`, `miniz_oxide`, `crc32fast` | forbidden |
| `webext-ffi` | C ABI over `webext`; owns the C↔Rust ownership rules | allowed, each use justified |
| `webext-cli` | `webext-coverage` | forbidden |

## Modules of `webext`

| Module | Responsibility |
|---|---|
| `zip`, `package` | Read `.zip`/`.xpi`/`.crx` (CRX2/CRX3). Bounded, CRC-checked, path-safe. CRX3 signed-data construction and key/ID binding; the actual signature verification is a `SignatureVerifier` the browser supplies |
| `encoding` | SHA-256, lenient base64, Chrome extension IDs |
| `manifest` | `manifest.json` → `Manifest`; MV2/MV3 and Chrome/Firefox normalisation; diagnostics (errors reject, warnings inform) |
| `loader` | Package/directory → validated extension: referenced files exist, locales consistent, ID derived (`key` → CRX header → gecko id) |
| `match_pattern` | Match patterns, globs, a small URL splitter |
| `content_scripts` | Which declared content scripts apply to a document (matches, excludes, globs, frames, `about:blank`) |
| `resources` | `web_accessible_resources` access decisions |
| `permissions` | API and host permissions, optional permissions, `activeTab` |
| `registry` | The API registry: every member, availability per browser, our status, permission, manifest-version range |
| `dispatch` | `Dispatcher`: routing and the implemented APIs (`runtime`, `storage`, `i18n`, `alarms`, `permissions`) |
| `storage`, `i18n`, `alarms` | The semantics of those APIs, independent of dispatch, with injected time |
| `host` | `ExtensionHost` (what the browser provides), `ApiCall`, `ApiError` |
| `analyze`, `render` | Static analysis of an extension against the registry; coverage reports |

## The registry is the source of truth

`Data/api-support.json` is read by four things that therefore cannot disagree:

1. **Dispatch** decides what exists, what needs which permission, and what is routed to the host.
2. **The shim's surface**: `Registry::surface_json` gives each extension only the namespaces its permissions allow, so a
   missing permission yields `undefined`, as in a real browser.
3. **The coverage report** (`docs/COVERAGE.md`, `webext-coverage registry`).
4. **The analyzer** (`webext-coverage inspect`).

Tests tie it to the code: every `implemented`/`partial` function must have a handler; every `delegated` one must reach the
host; no `planned` one may succeed; the events the core emits equal the ones marked implemented; the shim defines every
function, event and constant the registry lists; `docs/COVERAGE.md` must equal the generated output.

Availability (does Chrome/Firefox have this member?) is synced from MDN's browser-compat-data by `Tools/sync_registry.py`
and verified by `Tools/verify_registry.py`. Statuses are ours.

## Error model

`ApiError { kind, message }`. `kind` is for the browser's logging and tests; extensions only see `message`, delivered as a
rejected Promise or `chrome.runtime.lastError`. A `Runtime` error is an ordinary failure of a valid call (quota exceeded);
`Unhandled` is a bug in the registry/handler pairing and is caught by tests.

## Threading and reentrancy

Single-threaded by design: a handle is used from one thread (the Browser process main thread). The dispatcher keeps its state
in `RefCell`s and never holds a borrow while calling the host, so a host callback may call back into the library, and a host
may answer a delegated call or permission prompt synchronously or later. Tests cover both (`permission_prompt_answered_asynchronously`).

## Why Rust, and why a C ABI

Packages and manifests come from the internet and are parsed in the most privileged process of a browser, so memory safety
matters; Ladybird itself is moving this way (its HTML parser, URL, regex, HTTP and the LibJS runtime are Rust). The C ABI keeps
the boundary tiny and stable: strings in as (pointer, length), strings out as library-owned `WebExtString` released with
`webext_string_free` (always followed by a NUL byte for C callers), so the two sides may use different allocators. The header
is hand-written and a test checks it declares exactly what the crate exports.

## Security properties the core provides

- Archive entries with `..`, absolute paths, backslashes, drive letters, NULs, encryption or ZIP64 are rejected; each entry and
  the total are size-bounded; decompression is bounded by the *declared* size so a lying header cannot cause a bomb; CRC-32 is
  verified.
- JSON nesting is bounded (`serde_json` recursion limit); the analyzer, match-pattern and glob code is iterative and
  panic-free on any UTF-8 input (deterministic fuzz tests in `crates/webext/tests/robustness.rs`).
- Permission checks run before any handler or host call; optional permissions can only be requested if declared; required ones
  cannot be removed.
- Panics are contained at the FFI boundary.

What the **browser** must provide: signature verification, process isolation of extension contexts, enforcement of the
extension CSP (`csp.extensionPages` in `webext_extension_info`), and not serving extension files to web pages without
`webext_is_web_accessible`.
