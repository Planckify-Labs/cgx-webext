# Security policy

Extension packages, manifests and API calls are untrusted input, and this library sits inside a browser's most privileged
process. Please report suspected vulnerabilities privately (a GitHub security advisory on this repository) instead of in
a public issue.

Areas that deserve the most scrutiny:

- `crates/webext/src/zip.rs` and `package.rs`: archive and CRX parsing.
- `crates/webext-ffi/src/lib.rs`: the only `unsafe` code; ownership of strings across the C boundary.
- `crates/webext/src/permissions.rs` and `dispatch.rs`: permission checks before any API runs.
- `crates/webext/src/resources.rs`: which web pages may load which extension files.

Not provided by this library (the embedding browser must do it): cryptographic verification of CRX3 and Mozilla (`.xpi`)
signatures, isolation of extension processes, and enforcing the extension Content-Security-Policy.
