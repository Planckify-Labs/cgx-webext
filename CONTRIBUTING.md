# Contributing

```sh
scripts/check.sh            # everything CI runs, offline
scripts/test-cmake-module.sh
```

## Adding or changing API support

1. Edit the **status** of the member in `Data/api-support.json`.
2. If it becomes `implemented`/`partial`, add a handler in `crates/webext/src/dispatch.rs`; the test
   `every_implemented_function_has_a_handler` fails until you do. Events the core emits go in `EMITTED_EVENTS`.
3. If it becomes `delegated`, nothing else is needed in the core; document what the host must do in
   `docs/INTEGRATION.md`.
4. `cargo run -p webext-cli -- registry --markdown > docs/COVERAGE.md` (a test fails if you forget).

Availability (which browser has which API) comes from MDN's browser-compat-data:

```sh
python3 Tools/verify_registry.py          # report differences (needs network)
python3 Tools/sync_registry.py --write    # merge availability; never touches statuses
```

## Style

`cargo fmt`, `cargo clippy -- -D warnings`. The core crate forbids `unsafe`; only `webext-ffi` may use it, and every
`unsafe` block needs a `SAFETY` comment. Any new C ABI function must be added to `webext.h` (a test checks the two agree)
and exercised in `crates/webext-ffi/tests/smoke.c`.
