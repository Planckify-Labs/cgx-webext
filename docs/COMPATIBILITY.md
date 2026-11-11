# Real-world compatibility

<!-- Numbers below come from `Tools/real_world.sh`. Regenerate them with it; they are a snapshot, not a test. -->

How do real, popular, signed extensions fare? The first section is from running them in Ladybird. The static snapshot further down comes from `Tools/real_world.sh`, which downloads extensions from addons.mozilla.org and runs `webext-coverage inspect` on each (static analysis only).

## Run in Ladybird (11 November 2026)

These six were installed from the stores into Ladybird and their popups opened (off-screen, `--force-cpu-painting`, with
`--log-all-js-exceptions` and `WEBEXT_TRACE=1`). Coverage is `webext-coverage inspect` on the installed package.

| Extension | Store | Coverage | Popup | Does its job | What is missing |
|---|---|---:|---|---|---|
| Ambient light for YouTube 2.38.17 (MV3) | Chrome | 100% | works | **no** | isolated worlds: its content script `import()`s a module that needs `chrome.*` in the content-script world |
| SponsorBlock 6.1.7 (MV2) | Firefox | 98% | works | not checked on a sponsored video | `webNavigation.onCommitted` |
| Dark Reader 4.9.135 (MV2) | Firefox | 92% | works, knows the site | **no** | its dark-theme detector reports light pages as dark on Ladybird and removes the theme (cause not found yet); `theme.*` for its optional browser-theme feature |
| AnyDoc Translator 1.4.31 (MV3) | Chrome | 90% | works | not checked | `cookies`, `i18n.detectLanguage`, `webNavigation`, native messaging, `externally_connectable` |
| uBlock Origin 1.76.0 (MV2) | Firefox | 67% | works (slow on the first open after install, while it compiles filter lists) | **no**, blocks nothing | `webRequest` (blocking), `webNavigation` |
| Surfingkeys 1.17.11 (MV2) | Firefox | 57% | works | partly | its in-page panel (an extension page in an iframe) starts without `chrome`; bookmarks, history, downloads, sessions, tab groups, `tts`, `userScripts`, native messaging |

Coverage counts the APIs and manifest features the extension's code uses; the analyzer also lists Chrome-only members
(`dom.openOrClosedShadowRoot`, `fontSettings`) for Firefox builds that only call them when present.

Problems found by running them and fixed in the same session: popups stayed blank because unimplemented APIs were
exposed (extensions feature-detect, e.g. uBlock's `browser.publicSuffix`); secure pages refused extension files (mixed
content, page CSP, Trusted Types for script URLs); content scripts could not `fetch` their web-accessible files;
relative paths in background scripts resolved under a private folder; `sender.tab.url` lagged navigation; `setIcon`
resolved paths against the extension root instead of the calling page. The plan for the rest is in
[specs/webextensions-full-api/roadmap.md](specs/webextensions-full-api/roadmap.md).

## Static snapshot (earlier, outdated)

The table below predates ports, keyboard commands, `tabs`, `windows`, `action`, `contextMenus` and `scripting`, which are
now implemented; its numbers are kept for comparison.

Snapshot (November 2026, Firefox packages; Manifest version in parentheses):

| Extension | Verdict | Core | + host | Blockers |
|---|---|---:|---:|---:|
| uBlock Origin (MV2) | Blocked | 17% | 54% | 19 |
| Dark Reader (MV2) | Blocked | 36% | 78% | 11 |
| Vimium (MV3) | Blocked | 30% | 76% | 15 |
| Stylus (MV2) | Blocked | 22% | 67% | 19 |
| Bitwarden (MV2) | Blocked | 22% | 63% | 34 |

*Core* = share of the APIs and manifest features the extension uses that this library provides by itself. *+ host* also counts what the embedding browser must implement (tabs, windows, content-script injection, UI). *Blocked* means it uses something that is not implemented anywhere yet.

## What blocks them

Blockers shared by more than one of the five, most common first. This is the data-driven roadmap:

| Blocker | Extensions |
|---|---:|
| `runtime.connect` | 4 of 5 |
| `keyboard commands` | 4 of 5 |
| `runtime.onConnect` | 4 of 5 |
| `webRequest.onCompleted` | 3 of 5 |
| `blocking webRequest` | 3 of 5 |
| `webNavigation.onCommitted` | 3 of 5 |
| `webRequest.onBeforeRequest` | 3 of 5 |
| `dom.openOrClosedShadowRoot` | 3 of 5 |
| `webNavigation.getAllFrames` | 3 of 5 |
| `tabs.connect` | 2 of 5 |
| `webNavigation.onDOMContentLoaded` | 2 of 5 |
| `contentScripts.register` | 2 of 5 |
| `webRequest.onBeforeRedirect` | 2 of 5 |
| `webRequest.filterResponseData` | 2 of 5 |
| `webRequest.onErrorOccurred` | 2 of 5 |
| `commands.onCommand` | 2 of 5 |
| `commands.getAll` | 2 of 5 |
| `commands.update` | 2 of 5 |
| `extension.isAllowedFileSchemeAccess` | 2 of 5 |
| `notifications.create` | 2 of 5 |
| `notifications.onClicked` | 2 of 5 |
| `sidebar_action` | 2 of 5 |
| `extension.getBackgroundPage` | 2 of 5 |
| `extension.getViews` | 2 of 5 |

The biggest single item is **ports** (`runtime.connect`, `runtime.onConnect`, `tabs.connect`): long-lived message channels between extension contexts. Next are keyboard `commands`, `webRequest`, and `webNavigation`.

## Reading this honestly

- All five are **blocked today**. None would run yet, even with a browser integration, until the blockers above exist.
- Several blockers are only *partial* use (an extension may guard a call or use it only in an optional feature). The tool cannot tell; running the extension would.
- uBlock Origin and the other request blockers need network interception that the library does not provide; see `docs/INTEGRATION.md`.
- The scan is lexical (see the README for its limits), so counts are good estimates, not proofs.

## Reproduce

```sh
Tools/real_world.sh                         # the five above
Tools/real_world.sh ublock-origin           # one extension, by its addons.mozilla.org slug
cargo run -p webext-cli -- inspect my.xpi   # any .crx/.xpi/.zip or unpacked directory
```
