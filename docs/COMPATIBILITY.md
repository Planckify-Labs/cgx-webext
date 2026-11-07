# Real-world compatibility

<!-- Numbers below come from `Tools/real_world.sh`. Regenerate them with it; they are a snapshot, not a test. -->

How do real, popular, signed extensions fare? `Tools/real_world.sh` downloads them from addons.mozilla.org and runs `webext-coverage inspect` on each. This is **static analysis only**: it reads the manifest and scans the JavaScript for `chrome.*`/`browser.*` use. Nothing here has been *run* in a browser.

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
