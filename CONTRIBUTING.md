# Contributing

Thanks for working on `dsh desktop`. This document covers commit conventions and the checks a change must pass before it lands.
Architecture and setup details live in [docs/architecture.md](docs/architecture.md) and [docs/development.md](docs/development.md); the product requirements are in [项目要求.md](项目要求.md).

## Commit messages

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/): `<type>(<scope>)!: <subject>`.

```text
feat(launcher): parse the dsh web: line from stdout
fix(env): fall back to /bin/sh when the login shell probe times out
docs(readme): record the M0 status table
ci: cache cargo artifacts on the three-platform matrix
chore(icons): regenerate placeholder PNG exports
```

Rules:

- Types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `build`, `ci`, `chore`, `revert`.
- Scopes: `core`, `config`, `cli`, `locate`, `env`, `launcher`, `url`, `log`, `tauri`, `ui`, `tray`, `menu`, `window`, `icons`, `ci`, `release`, `docs`. Skip the scope only when the change really is repository-wide.
- Subject line: imperative mood, lowercase, no trailing period, ≤ 72 characters.
- Wrap the body at 100 columns and explain *why*; the diff already explains *what*.
- Mark a breaking change with `!` after the scope and a `BREAKING CHANGE:` footer.
- Reference the requirement id in the body when one applies, e.g. `Refs: FR-2`.
- One logical change per commit; do not mix formatting-only churn with behavior changes.

## Required checks

Run the same gates CI runs:

```sh
pwsh -File scripts/check.ps1   # Windows
./scripts/check.sh             # Linux / macOS
```

which is exactly:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

A change is not ready while any of these fails; `-D warnings` means warnings are errors, so fix them instead of silencing them. CI additionally runs `cargo build --release -p dsh-desktop` on `ubuntu-latest`, `macos-latest` and `windows-latest`, so code that only builds on your machine will still fail review.

When a change touches packaging, also build a bundle locally:

```sh
cargo tauri build --config src-tauri/tauri.release.conf.json --bundles <your platform bundles>
```

## Scope and layout rules

- GUI-free logic belongs in `crates/dsh-lite-core` and must be unit-testable without a display; `src-tauri` only glues it to windows, trays and menus.
- Platform branching lives only in `src-tauri/src/platform/`; read [docs/architecture.md §1.1](docs/architecture.md#11-平台差异层) before adding a `#[cfg(target_os = ...)]` anywhere else.
- Do not add a Node/pnpm/bundler step: `src-ui/` is hand-written HTML/CSS/JS by design.
- Do not modify `src-tauri/tauri.conf.json` to express release-only differences; put them in `src-tauri/tauri.release.conf.json`.
- Comments in code are English; user-facing documentation is Chinese.
- Every behavior change ships with a test that describes the behavior, not the implementation.

## Security rules

- Never commit credentials, certificates, signing keys, `.env` files, or a tokenized `dsh web:` URL.
- Logs, crash reports, window titles and issue reports must not contain the one-time token; redact before writing.
- The shell must not read, cache or upload any credentials; `DEEPSEEK_API_KEY` is read by the `dsh` child process only.
- Report a security issue privately to the maintainers instead of opening a public issue.

## Pull requests

1. Keep the PR focused; split unrelated refactors out.
2. Describe the behavior change, the requirement id it serves, and how you verified it (commands run).
3. State the platform you tested on, and whether the build used MSVC or the local MinGW toolchain — packaging claims are only valid on MSVC.
4. Update the affected docs (`README.md`, `docs/*.md`) in the same PR.
