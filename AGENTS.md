# AGENTS.md

## Purpose
This is a Rust CLI project (`gbackup-rs`) for backing up Gmail via IMAP.

## Build And Check
- Use `cargo build` for local builds.
- Use `cargo test` for tests (there are currently no unit tests in the repo).
- Use `cargo clippy --all-targets` for lint checks.

## Toolchain
- Toolchain is pinned in `rust-toolchain.toml` to `stable`.
- If dependency resolution changes, update `Cargo.lock` via `cargo update` and re-run `cargo build`.

## Agent Guidance
- Keep changes minimal and targeted.
- Avoid broad dependency upgrades unless needed for build compatibility.
- Prefer fixing issues in this order: reproducible build, correctness, then style.
