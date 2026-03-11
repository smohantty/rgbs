# rgbs

Rust reimplementation of the old GBS local build path.

Current scope:

- local package builds only
- `.gbs.conf` compatibility for local build use cases
- rpm-md repository resolution and `build.conf` handling
- `libsolv`-based build dependency resolution
- buildroot reuse, source staging, `rpmbuild`, and artifact collection

Current status is tracked in [STATUS.md](STATUS.md). The design and phase plan live in [Plan.md](Plan.md).

## Prerequisites

Build-time tools:

- Rust toolchain with Cargo
- C toolchain for the vendored `libsolv`
- `cmake`
- Expat development libraries required by the vendored `libsolv` build

Runtime tools used by `rgbs build`:

- `rpm`
- `rpmbuild`
- `rpmspec`
- `tar`
- `git`

Optional but recommended runtime tools:

- `bwrap` for isolated builds
- `createrepo_c` for refreshing output repo metadata

## Build

Build the whole workspace:

```bash
cargo build
```

Run tests:

```bash
cargo test
```

Run the CLI without installing:

```bash
cargo run -p rgbs-cli -- build --help
```

## Install

Install the current CLI crate:

```bash
cargo install --path crates/rgbs-cli
```

After install, the command is:

```bash
rgbs build --help
```

## Usage

Current command shape:

```bash
rgbs build [OPTIONS] --arch <ARCH> [GITDIR]
```

Common examples:

```bash
rgbs build -A aarch64
rgbs build -A aarch64 path/to/package
rgbs build -A aarch64 -P profile.myprofile
rgbs build -A aarch64 -R https://example.com/repo
rgbs build -A aarch64 --include-all
rgbs build -A aarch64 --keep-packs
rgbs build -A aarch64 --noinit
rgbs build -A aarch64 --perf
```

Supported flags on the current CLI:

- `-A`, `--arch`
- `-P`, `--profile`
- `-R`, `--repository`
- `-D`, `--dist`
- `-B`, `--buildroot`
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
- `--perf` with `--time` as an alias

## Build Flow

`rgbs` currently runs this pipeline:

1. load `.gbs.conf`
2. resolve repos and `build.conf`
3. inspect the spec and evaluated `BuildRequires`
4. solve dependencies with `libsolv`
5. download and cache RPMs
6. create or reuse the buildroot
7. stage sources and spec
8. run `rpmbuild`
9. collect RPM/SRPM artifacts into the output repo layout

## Output

The build command prints JSON describing the completed build. When `--perf` is enabled, the JSON includes a `performance` section with:

- total wall time
- repository, spec, solver, download, buildroot, staging, `rpmbuild`, and artifact timings
- cache and reuse indicators

## Notes

- `--noinit` requires a previously prepared buildroot state
- `--keep-packs` is intended for building multiple packages against a shared prepared root
- `bwrap` is preferred when available, but the implementation still falls back to host `rpmbuild` when the prepared root is not self-contained enough
- exact old-GBS `--incremental` behavior is intentionally deferred past v1
- the CLI crate is still named `rgbs-cli`, but the released binary is now `rgbs`
