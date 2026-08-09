# Contributing to simtop

Thanks for wanting to help. simtop is small on purpose — one library crate,
one backend trait, two frontends — and the contribution bar is: keep it that
way, and don't break the contracts the tests pin.

## Before you start

- Read the [README](README.md), then skim [AGENTS.md](AGENTS.md). AGENTS.md
  is the source of truth for architecture and conventions; this document is
  the human-facing short version.
- Open an issue before making a behavioral change or adding a command, so
  the design is agreed before the code.
- Bug reports are welcome as issues: include the exact command, the exit
  code, and (for `--json` runs) the error envelope.

## Environment

- **macOS 15+ with Xcode 16.** simtop drives CoreSimulator; it compiles
  elsewhere only as stubs and refuses to run (see `simtop::require_macos`).
  Most development needs at least one simulator installed.
- **Rust stable >= 1.74** (edition 2021). No feature flags, no
  dev-dependencies.

## Development loop

```bash
cargo build                 # compile
cargo run                   # launch the TUI
cargo run -- list --json    # exercise a one-shot command
cargo test                  # unit tests + contract tests
cargo fmt && cargo clippy   # keep the tree clean (not CI-enforced)
```

## Tests and contracts

- `tests/model_contract.rs` pins the wire format: schema version, serde
  field names, snapshot JSON shape, event envelopes.
- `tests/error_contract.rs` pins error machine codes, exit-code
  determinism, and the error-report JSON shape.
- If you change any serialized shape, error code, or exit-code mapping,
  `cargo test` will — and should — fail until the contract tests are
  updated deliberately. That is the design, not an annoyance.
- Tests are deterministic and never invoke simctl or Xcode; keep it that
  way. There is no test CI job, so **run `cargo test` locally before
  pushing.**

## Conventions (short version; AGENTS.md is authoritative)

- **Architecture**: `src/main.rs` is a thin shim. New logic goes in the
  library crate; new operations are added to the `SimulatorBackend` trait
  in `src/backend/mod.rs` once, and both frontends get them for free. No
  globals or statics — the backend is constructed once and injected.
- **Errors**: hand-rolled `SimtopError { code, message, source }` with
  stable `ErrorCode` categories and deterministic exit codes. Never
  renumber, reorder, or rename existing variants — automation depends on
  them.
- **Async**: tokio + `#[async_trait]` on command and backend paths. Sync C
  ABI goes behind `tokio::task::spawn_blocking`. Unsafe is confined to
  `src/native.rs`, guarded by RAII frees.
- **No shell passthrough**: no command strings built from user input.
  `simctl` arguments are typed arrays; UDIDs, bundle IDs (reverse-DNS), and
  URL schemes are shape-validated in the shared backend validators.
- **Selectors**: exact UDID first, then unique case-insensitive name.
  Ambiguity is `INVALID_ARGUMENT`, missing is `DEVICE_NOT_FOUND` — never a
  silent pick.
- **Model**: public fields, serde `snake_case`, `SCHEMA_VERSION = 1`.
- **Style**: imports grouped std → external → `crate::`; derives typically
  `Debug, Clone, PartialEq, Eq` plus serde; domain nouns over abbreviations
  (`SimDevice`, not `Dev`). Run `cargo fmt`.

## Submitting changes

- Small, focused PRs — one logical change each. A PR that touches the trait,
  the model, and the error codes at once is three PRs.
- Run `cargo test`, `cargo fmt`, and `cargo clippy` before pushing. There is
  no lint/test CI; the only workflow (`release.yml`) builds on tags.
- Update the README command reference and JSON examples for any
  user-visible change.
- The Objective-C bridge (`native/`) is compiled with `-fno-objc-arc`;
  follow the existing retain/release discipline and the `@try/@catch` →
  typed-error pattern. It is dlopen'd at runtime and must never be linked.

## Releasing (maintainers)

- Tag `v<semver>` and push. `release.yml` builds `aarch64`/`x86_64`,
  lipos a universal binary, smoke-tests it, and drafts a GitHub Release with
  the archive and SHA-256. Manual runs need the `version` input.
- Before the first release, replace the placeholders in
  `Formula/simtop.rb` (`RELEASE_OWNER`, `RELEASE_VERSION`,
  `RELEASE_SHA256`). Until they are filled, the formula deliberately refuses
  to load — do not remove that guard.
- The artifact is intentionally not codesigned; there are no signing
  secrets in this repository. Do not add a codesign step that would only
  fail or silently no-op without them.
