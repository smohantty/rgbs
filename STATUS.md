# RGBS V1 Status

Date: 2026-03-11

## Scope

This file tracks the current implementation state of `rgbs` v1 for the local build path.

Compatibility target:

- old GBS local build behavior and artifacts
- not old GBS internal process layout

## Implemented

- workspace layout and Cargo build for:
  - `rgbs-cli`
  - `rgbs-common`
  - `rgbs-config`
  - `rgbs-repo`
  - `rgbs-resolver`
  - `rgbs-spec`
  - `rgbs-builder`
- `.gbs.conf` parsing with:
  - project, home, and system config layering
  - profile-style and legacy `[build]` config support
  - auth inheritance and `passwdx` decoding
  - variable interpolation for supported forms
- `rgbs build` CLI flag support for:
  - `-A` / `--arch`
  - `-P` / `--profile`
  - `-R` / `--repository`
  - `-D` / `--dist`
  - `-B` / `--buildroot`
  - `--define`
  - `--spec`
  - `--include-all`
  - `--noinit`
  - `--clean`
  - `--keep-packs`
  - `--overwrite`
  - `--fail-fast`
  - `--clean-repos`
  - `--skip-srcrpm`
  - `--perf` / `--time`
- repository metadata handling with:
  - rpm-md `repodata` support
  - legacy `builddata/build.xml` compatibility
  - persistent metadata cache
  - `build.conf` materialization and reuse
- spec processing with:
  - spec discovery
  - evaluated `BuildRequires`
  - source tag collection
- dependency solving with:
  - bundled `libsolv`
  - rpm-md loading through the vendored crate plus local FFI shim
  - persistent solver result cache
- RPM fetch and reuse with:
  - download cache
  - authenticated remote fetch support
  - local repo path support
- buildroot handling with:
  - exact-match root reuse
  - shared `--keep-packs` roots
  - persisted active root state for `--noinit`
  - cached `build.conf` reuse during `--noinit`
- source and spec staging with:
  - committed-tree export by default
  - working-tree export for `--include-all`
  - `.gitignore`-aware inclusion of untracked files
  - warnings when dirty files are excluded
- `rpmbuild` execution with:
  - host backend
  - `bwrap` backend when usable
  - host runtime bootstrap for the `bwrap` path when the root is not yet self-contained
- artifact handling with:
  - RPM/SRPM collection
  - output repo layout under the configured buildroot
  - `createrepo_c` refresh when available
- warm-build behavior with:
  - stage reuse
  - cached-build skip behavior unless `--overwrite` forces rebuild
- optional per-build performance reporting with:
  - total wall time
  - per-stage timings
  - cache and reuse indicators

## Remaining Before V1 Signoff

- benchmark harness against old GBS local builds
- fixture-driven artifact parity comparison for representative packages and architectures
- more self-contained prepared roots so the `bwrap` path depends less on host runtime bootstrap

## Explicitly Deferred Past V1

- exact old-GBS `--incremental` semantics
- remote build and OBS flows
- full-build and deps-build flows
- non-local GBS subcommands outside the local build path

## Known Notes

- the installed binary name is currently `rgbs-cli`
- docs and plan use `rgbs build` as the intended command model, but the actual Cargo-installed binary has not been renamed yet
- `bwrap` is preferred for isolation, but host `rpmbuild` remains the fallback when the prepared root is not runnable enough

## Verification Baseline

Current workspace verification:

- `cargo fmt`
- `cargo test`
- `cargo run -q -p rgbs-cli -- build --help`
