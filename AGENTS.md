# Repository Guidelines

## Project Overview

`simtop` is a terminal UI (ratatui) for managing iOS simulators: listing, booting/shutting down, creating/deleting, installing/launching apps, screenshots, and logs. It ships two frontends on one backend — an interactive TUI and one-shot CLI subcommands (`simtop list`, `simtop boot <udid>`, ...). macOS-only (15+, Xcode 16); it drives Apple's `simctl` plus a dynamically-loaded Objective-C CoreSimulator bridge for the hot paths. JSON output mode is designed for scripting.

The app has full mouse support for clicking and mouse usage in the TUI. THis is a needed feature, and the ability to have an equal 
expereicne regardless of if it is the keyboard ofr mouse being used is mandatory non-negotiable. Mouse support is first class citizen, 
as is keyboard user

## Architecture & Data Flow

The binary is a thin shim; all logic lives in the library crate:

```
src/main.rs  →  simtop::cli::run()  →  clap parse
   →  XcodeEnvironment resolution (src/xcode.rs)
   →  backend::connect(dev_dir, timeout) → Box<dyn SimulatorBackend>
   →  tui::run(backend)          # no subcommand
   →  run_command(backend, cmd)  # one-shot subcommands
```

- **Backend abstraction** (`src/backend/mod.rs`): one async, object-safe trait is the sole interface consumed by both TUI and CLI. Methods: `snapshot`, `list_devices`, `boot`, `shutdown`, `open`, `create`, `delete`, `install`, `launch`, `terminate`, `uninstall`, `open_url`, `screenshot`, `logs`. All return `Result<_, SimtopError>`.
- **`HybridBackend`** (`src/backend/hybrid.rs`) selects the implementation per operation:
  - Native CoreSimulator bridge (via `src/native.rs`, `Arc<NativeSimulator>`, `spawn_blocking`) preferred for discovery/lifecycle: `list`, `boot`, `shutdown`, `create`, `delete`.
  - Typed `SimctlClient` (`src/backend/simctl.rs`, `tokio::process::Command`, argument arrays — never shell) for app ops, URL, screenshots, logs; `open` shells out to `/usr/bin/open` on Simulator.app.
  - If the native bridge is missing or reports `Unsupported`, fall back **exactly once** to simctl; other native failures are authoritative (no retry). Bridge load failure is nonfatal (stderr warning).
- **TUI data flow** (`src/tui.rs`): `App` holds state plus a bounded mpsc channel (cap 64). Backend calls run as `tokio::spawn` tasks sending `Action` results back; the event loop polls crossterm with ≤100ms timeout and redraws only when dirty.
- **Output** (`src/output.rs`): `Output { json: bool }` centralizes formatting. Human mode renders tables/text; JSON mode emits one-line flushed envelopes — success `{"schema":1,"command":"...","ok":true,"data":...}`, error `ok:false` with `{code,message,exit_code}`. JSON results go to stdout; all diagnostics to stderr. `--json` never prompts.

## Key Directories

| Path | Purpose |
|---|---|
| `src/` | Library crate (`simtop`): `cli`, `model`, `error`, `output`, `backend/`, `native`, `xcode`, `tui` |
| `src/backend/` | `mod.rs` (trait + `connect()` + shared validators), `simctl.rs`, `hybrid.rs` |
| `native/` | Objective-C CoreSimulator bridge (`SimtopCoreSimulator.h`/`.m`), C ABI, compiled by `build.rs` |
| `tests/` | Integration contract tests for `model` and `error` (no simulator needed) |
| `Formula/` | Homebrew formula (placeholder — raises until release coordinates are filled in) |
| `.github/workflows/` | `release.yml` — tagged releases only, no CI test job |

## Development Commands

```bash
cargo run                 # interactive TUI
cargo run -- list --json  # one-shot, machine-readable
cargo test                # unit tests + integration contract tests
cargo build --release --locked
cargo fmt && cargo clippy # not enforced by CI; keep code formatted
```

Release: tag `v*` and push — `release.yml` builds `aarch64`/`x86_64`, lipo's a universal binary, and drafts a GitHub release. No test/lint CI exists; run `cargo test` locally before pushing.

## Code Conventions & Common Patterns

- **Module layout**: responsibility-focused modules, all wired publicly in `src/lib.rs`. Re-exports: `error::{ErrorCode, Result, SimtopError}`.
- **Dependency injection**: `Box<dyn SimulatorBackend>` is constructed once and injected into TUI/CLI — no globals or statics. Follow this when adding features.
- **Error handling**: custom `SimtopError { code: ErrorCode, message, source }` (not thiserror/anyhow), `type Result<T> = Result<T, SimtopError>`. Propagate with `?`; use `map_err` at FFI and serde boundaries. `ErrorCode` variants have **stable machine codes and deterministic exit codes** (2–12 per variant, `Internal=70`, `1` = aborted). Never renumber — `tests/error_contract.rs` pins them.
- **Async**: tokio + `#[async_trait]` throughout command/backend paths. Synchronous C ABI is isolated behind `tokio::task::spawn_blocking`; `NativeSimulator` exposes sync methods under a `Mutex`.
- **Unsafe**: confined to `src/native.rs` FFI, guarded by RAII frees. The ObjC side is compiled `-fno-objc-arc` (manual retain/release) with `@try/@catch` → typed errors; CoreSimulator is dlopen'd at runtime, never linked.
- **Selector resolution**: exact UDID first, then unique case-insensitive name; ambiguous → `InvalidArgument`, missing → `DeviceNotFound`. Convenience selectors like `booted` are rejected.
- **Validation**: no shell passthrough or interpolation anywhere; UDIDs, bundle IDs (reverse-DNS), and URL schemes are shape-validated in shared backend validators.
- **Model**: public fields, serde `snake_case`, `SCHEMA_VERSION = 1`. The wire format is a tested contract (`tests/model_contract.rs`) — changing serde names/shapes/JSON envelopes requires updating those tests deliberately.
- **Style**: imports grouped std → external → `crate::`; derives typically `Debug, Clone, PartialEq, Eq` plus serde; naming uses explicit domain nouns (`SimDevice`, `DeviceSnapshot`, `DeviceState`).

## Important Files

| File | Role |
|---|---|
| `src/main.rs` / `src/lib.rs` | Entry point (`#[tokio::main]` → `cli::run`), module map, macOS gate |
| `src/cli.rs` | clap derive CLI, dispatch, selector resolution |
| `src/tui.rs` | TUI state machine, event loop, rendering, theme |
| `src/model.rs` | Schema-v1 domain types (devices, apps, snapshots, events) |
| `src/error.rs` | Error taxonomy, machine codes, exit-code mapping |
| `src/backend/mod.rs` | `SimulatorBackend` trait — the contract to implement for new ops |
| `src/backend/hybrid.rs` | Native-vs-simctl policy and fallback logic |
| `src/native.rs` + `native/` | C ABI wrapper / ObjC CoreSimulator bridge |
| `src/xcode.rs` | Developer-dir resolution: `--developer-dir` > `$DEVELOPER_DIR` > `xcode-select -p` |
| `build.rs` | Compiles `native/SimtopCoreSimulator.m` (cc), links Foundation/objc, `simtop_no_native` stub off-macOS |
| `tests/model_contract.rs`, `tests/error_contract.rs` | Wire-contract pinning (serde shapes, exit codes, JSON envelopes) |

## Runtime/Tooling Preferences

- Rust ≥ 1.74 (edition 2021), stable toolchain; no feature flags, no dev-dependencies.
- macOS 15+ deployment target (`MACOSX_DEPLOYMENT_TARGET` overridable), Xcode 16 required.
- CoreSimulator.framework is **not** linked at build time — resolved and dlopen'd at runtime (developer dir, then system fallback).
- Non-macOS builds compile as stubs (`simtop_no_native` cfg) — the tool is functionally macOS-only.
- Deps of note: ratatui 0.29, crossterm 0.28, clap 4 (derive), tokio 1 (full), serde/serde_json, thiserror is a transitive-only dep — errors are hand-rolled by design.

## Testing & QA

- `cargo test` runs everything: `#[cfg(test)]` unit modules inside `src/` (cli, native, output, xcode, backend/*) plus integration tests in `tests/`.
- Integration tests are **contract tests**, not behavior tests: `tests/model_contract.rs` (18 tests) pins schema version, serde field names, unknown-state preservation, snapshot JSON shape and round-trips, event envelopes; `tests/error_contract.rs` (9 tests) pins machine codes, exit-code determinism/uniqueness, `ErrorReport` JSON shape, and `std::io`/serde conversion categories.
- Tests are deterministic, platform-independent, and never invoke simctl/Xcode — they exercise only the public API via `simtop::model` / `simtop::error`.
- If you change any serialized shape, error code, or exit-code mapping, `cargo test` will (and should) fail until the contract tests are updated deliberately.
- No coverage gates, no test CI. Verify with `cargo test` before pushing.
