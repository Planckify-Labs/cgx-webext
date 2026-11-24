# Real-world compatibility

<!-- Numbers below come from `Tools/real_world.sh`. Regenerate them with it; they are a snapshot, not a test. -->

How do real, popular, signed extensions fare? The first section is from running them in Ladybird. The static snapshot further down comes from `Tools/real_world.sh`, which downloads extensions from addons.mozilla.org and runs `webext-coverage inspect` on each (static analysis only).

## Run in Ladybird (24 November 2026)

Fifteen extensions downloaded from the Chrome Web Store and addons.mozilla.org, each run alone in a fresh profile on a
private X server with `WEBEXT_TRACE=1`, on a page that exercises it (adblock-tester.com for blockers, a YouTube video for
the YouTube ones, Wikipedia for Read Aloud), with their toolbar popup opened. `Tools/run_extension_suite.py` repeats this.

| Extension | Store | Popup | Does its job | What is missing |
|---|---|---|---|---|
| uBlock Origin | Firefox | works | **yes**: 28-32 requests blocked on the test page | `dns.resolve` (CNAME uncloaking), `filterResponseData` |
| AdGuard | Firefox | works | **yes**: requests blocked, counted in the popup | nothing found |
| uBlock Origin Lite | Chrome | works | yes (static rulesets) | nothing found |
| AdGuard | Chrome | works | blocks (e.g. Google's ad script) | its "Blocked" counter stays 0: it gets `onRuleMatchedDebug`, which Chrome sends to unpacked extensions only (roadmap item 6) |
| Bitwarden | Chrome | works (login and onboarding screens) | not checked (needs an account) | nothing found |
| Bitwarden | Firefox | works | not checked (needs an account) | nothing found |
| Tampermonkey | Chrome | works | not checked | nothing found |
| Violentmonkey | Firefox | works | not checked | nothing found |
| SponsorBlock | Chrome | works, on a YouTube video | runs (no segments exist for the test video, so skipping was not seen) | nothing found |
| Return YouTube Dislike | Chrome | – | no errors; dislike count not checked | nothing found |
| Video DownloadHelper | Chrome | works, lists the video's formats | not checked (downloading needs `downloads`) | `downloads` primitives |
| Session Buddy | Chrome | opens its page | not checked | nothing found |
| Read Aloud | Chrome | works | **no**: asks for a cloud voice account | `tts` primitives (roadmap item 3) |
| OneTab | Chrome | collects the tabs, opens its page | **no**: its page says "OneTab needs to restart" | its background does not answer the page's `corePing` (cause not found) |
| Stylus | Chrome | **blank** | **no** | `navigator.serviceWorker` for extension pages (roadmap item 1) |

Fixed while running them: Firefox's synchronous `publicSuffix` (uBlock's popup was blank), `tabs.onReplaced` for Firefox
(AdGuard never finished starting), Chrome's enum objects such as `declarativeNetRequest.RuleActionType` (AdGuard),
`tabs.query` by extension-page URL, CORS-free fetches from extension pages to permitted hosts and `sender.origin`
(Bitwarden), `importScripts` for service-worker backgrounds (Bitwarden, Tampermonkey, Read Aloud), top-level scope for
injected scripts (Read Aloud), `getUILanguage()` as a language tag and a localized `getManifest()` for Firefox builds
(Violentmonkey), `windows.getLastFocused` without a focused window (OneTab), and the Hub process split, which had left
the toolbar, popups and window-related calls disconnected on Linux.

## Run in Ladybird (11 November 2026)

These six were installed from the stores into Ladybird and their popups opened (off-screen, `--force-cpu-painting`, with
`--log-all-js-exceptions` and `WEBEXT_TRACE=1`). Coverage is `webext-coverage inspect` on the installed package.

| Extension | Store | Coverage | Popup | Does its job | What is missing |
|---|---|---:|---|---|---|
| Ambient light for YouTube 2.38.17 (MV3) | Chrome | 100% | works | not confirmed | its content script now runs in an isolated world and its `import()`ed module gets `chrome.*` (it reads and writes `chrome.storage` as it starts; checked headless on a YouTube watch page, 12 November 2026). The glow itself was not seen: the video does not play without user interaction in a headless run, so nothing was drawn. Not checked in a window |
| SponsorBlock 6.1.7 (MV2) | Firefox | 98% | works | not checked on a sponsored video | nothing known (`webNavigation.onCommitted` exists now; not run again) |
| Dark Reader 4.9.135 (MV2) | Firefox | 92% | works, knows the site | **no** | its dark-theme detector reports light pages as dark on Ladybird and removes the theme (cause not found yet); `theme.*` for its optional browser-theme feature |
| AnyDoc Translator 1.4.31 (MV3) | Chrome | 90% | works | not checked | `cookies`, `i18n.detectLanguage`, `webNavigation`, native messaging, `externally_connectable` |
| uBlock Origin 1.76.0 (MV2) | Firefox | 78% | works (slow on the first open after install, while it compiles filter lists) | **yes** for network filtering: on a page that loads Google's ad, tag-manager and publisher-tag scripts it cancels or replaces all three and leaves an unrelated script alone (checked headless, November 2026). Cosmetic filtering not checked. As in Firefox it holds every request until its filter lists are ready: about 25 seconds on a first run in Ladybird, during which pages stay blank or without images (later starts not measured) | `webRequest.filterResponseData` (HTML filtering), `dom.openOrClosedShadowRoot` |
| uBlock Origin Lite 2026.1110 (MV3) | Chrome | 71% | not checked | **yes** in the same test, from its static rulesets (redirects to its surrogate scripts, blocks the tag manager) | `userScripts`, `offscreen`, `storage.session.setAccessLevel` for its optional modes |
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
- The analysis below predates the network APIs (`webRequest`, `declarativeNetRequest`, `webNavigation`), which now exist; the table above is current.
- The scan is lexical (see the README for its limits), so counts are good estimates, not proofs.

## Reproduce

```sh
Tools/real_world.sh                         # the five above
Tools/real_world.sh ublock-origin           # one extension, by its addons.mozilla.org slug
cargo run -p webext-cli -- inspect my.xpi   # any .crx/.xpi/.zip or unpacked directory
```
