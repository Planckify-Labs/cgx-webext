# Integrating with a browser

## Ladybird

```sh
Integration/ladybird/apply.sh /path/to/ladybird --check   # does it apply?
Integration/ladybird/apply.sh /path/to/ladybird           # apply the patch and copy the adapter
cmake -S /path/to/ladybird -B Build/webext -DENABLE_WEBEXT=ON -DWEBEXT_SOURCE_DIR=/path/to/cgx-webext
```

What gets changed in Ladybird:

| File | Change |
|---|---|
| `Meta/CMake/cmake_options.cmake` | `ENABLE_WEBEXT` (default OFF) and `WEBEXT_SOURCE_DIR` |
| `CMakeLists.txt` | `add_subdirectory(${WEBEXT_SOURCE_DIR})` when enabled |
| `Libraries/LibWebView/CMakeLists.txt` | compile `Extensions/ExtensionManager.cpp` and link `WebExt::WebExt` when enabled |
| `Libraries/LibWebView/Extensions/ExtensionManager.{h,cpp}` | **new**: the C++ adapter (`WebView::ExtensionManager`) |

With `ENABLE_WEBEXT` off, nothing is compiled or linked.

The patch is pinned to the Ladybird commit in `Integration/ladybird/SUPPORTED_COMMIT`. `apply.sh` warns if yours differs.
Two scripts tell you whether a newer Ladybird still works:

- `Integration/ladybird/apply.sh <checkout> --check`: the CMake edits still apply.
- `Integration/ladybird/check-adapter.sh <checkout>`: the adapter still type-checks against that checkout's headers.

The weekly workflow `.github/workflows/upstream.yml` runs both against Ladybird's `master`.

**What has been verified.** The patch applies to a clean checkout at the pinned commit, and a full Ladybird release build
with `-DENABLE_WEBEXT=ON` compiles and runs. In that build the demo extension (`Integration/ladybird/demo`) works in
its Chrome and its Firefox form, and Dark Reader installs, loads again after a restart, and is removed through the
buttons of both the Chrome Web Store and addons.mozilla.org. See [docs/RUNNING.md](../docs/RUNNING.md).

Why the CMake module does not use Ladybird's `import_rust_crate()`: that helper requires every crate to export panic- and
demangler-hook symbols named after the crate and defined in Ladybird's private `RustPanic.rs`. Depending on that would make
this project break whenever Ladybird reorganises that code. The module in `CMakeLists.txt` only needs `cargo`, runs it from
Ladybird's source root so the same pinned toolchain is used, and builds with unwinding enabled because the FFI layer
contains panics.

## Other browsers

Any browser that can link a C library can use `crates/webext-ffi`. Implement the callbacks in `WebExtHostCallbacks` and
follow [docs/INTEGRATION.md](../docs/INTEGRATION.md). The Ladybird adapter is a worked example of the C++ side.
