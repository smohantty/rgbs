# rgbs

Rust reimplementation of the old GBS local build path.

Current scope:

- local package builds only
- target architectures limited to `armv7l` and `aarch64`
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
cargo run -p rgbs-cli -- doctor
cargo run -p rgbs-cli -- fix --dry-run
cargo run -p rgbs-cli -- -c path/to/custom.gbs.conf build -A aarch64
```

## Install

Install the current CLI crate:

```bash
cargo install --path crates/rgbs-cli
```

After install, the command is:

```bash
rgbs build --help
rgbs doctor
rgbs fix --dry-run
rgbs -c path/to/custom.gbs.conf build -A aarch64
```

## Usage

Current command shape:

```bash
rgbs build [OPTIONS] --arch <ARCH> [GITDIR]
```

Common examples:

```bash
rgbs doctor
rgbs doctor -A armv7l
rgbs doctor -A aarch64
rgbs fix --dry-run
rgbs fix --dry-run -A armv7l
rgbs fix --dry-run --with-source-build -A aarch64
rgbs -c path/to/custom.gbs.conf build -A aarch64
rgbs build -A armv7l
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

- `-c`, `--config`
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

Supported target architectures:

- `armv7l`
- `aarch64`

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

During `rgbs build`, concise cargo-style stage status lines are streamed to stderr so users can see which step is active while stdout remains reserved for the final JSON result.

Each build also writes persistent logs under:

```text
<buildroot>/logs/<arch>/<session>/
```

Current log files:

- `progress.log` for the same high-level stage progress shown to the user
- `debug.log` for command traces and stderr/stdout from invoked tools
- `resolved-plan.json` for the resolved build plan snapshot used for that run
- `config-snapshot.conf` for a redacted merged dump of the relevant `gbs.conf` sections used for that run

## Spec Selection

By default, `rgbs` looks for spec files under the resolved `packaging_dir` from `.gbs.conf`. The default packaging dir is `packaging`.

Current selection order is:

- `--spec <FILE>`, if provided
- `<gitdir>/<packaging_dir>/<repo-name>.spec`, if it exists
- the first `.spec` file in the packaging dir after lexical sort

If no spec file exists under the packaging dir, the build fails.

## Config Loading

`rgbs build` supports an explicit config file with `-c` / `--config`.

Example:

```bash
rgbs -c ~/gbs-my.conf build -A aarch64
```

Current config precedence is:

- explicit `-c` / `--config` file, when provided
- repo-root `.gbs.conf`
- current directory `.gbs.conf`
- `~/.gbs.conf`
- `/etc/gbs.conf`

`-c` adds a highest-priority config layer; it does not disable the normal hierarchy underneath it.

## Doctor

`rgbs doctor` checks the host environment and recommends missing prerequisites.

What it checks today:

- required runtime tools for `rgbs build`
- recommended extras like `bwrap` and `createrepo_c`
- source-build prerequisites for building `rgbs` itself from source
- common native host toolchain commands
- common cross-compiler names when you pass `-A`
- target arch validation limited to `armv7l` and `aarch64`

Examples:

```bash
rgbs doctor
rgbs doctor -A armv7l
rgbs doctor -A aarch64
```

Scope note:

- `doctor` checks host prerequisites and common toolchain expectations
- package-specific `BuildRequires` are still resolved from the spec and repos during `rgbs build`

## Fix

`rgbs fix` installs missing host-side prerequisites on Ubuntu using `apt-get`.

What it does today:

- installs missing required runtime tools for `rgbs build`
- installs recommended extras like `bubblewrap` and `createrepo_c`
- installs common host toolchain packages
- installs target cross-toolchain packages when `-A` selects a different target arch
- optionally installs `rgbs` source-build prerequisites with `--with-source-build`

Examples:

```bash
rgbs fix --dry-run
rgbs fix --dry-run -A armv7l
rgbs fix --dry-run --with-source-build -A aarch64
rgbs fix -A armv7l -y
```

Notes:

- `fix` is currently Ubuntu-only
- it uses `apt-get` under the hood for stable scripted installs
- use `--dry-run` to preview the exact packages and install command
- use `--update` if you want `apt-get update` before install

## Output

The build command prints JSON describing the completed build. When `--perf` is enabled, the JSON includes a `performance` section with:

- total wall time
- repository, spec, solver, download, buildroot, staging, `rpmbuild`, and artifact timings
- cache and reuse indicators

When available, the JSON also includes a `logs` section pointing at the per-build log directory and files under the configured buildroot.

## Notes

- `--noinit` requires a previously prepared buildroot state
- `--keep-packs` is intended for building multiple packages against a shared prepared root
- `bwrap` is preferred when available, but the implementation still falls back to host `rpmbuild` when the prepared root is not self-contained enough
- exact old-GBS `--incremental` behavior is intentionally deferred past v1
- the CLI crate is still named `rgbs-cli`, but the released binary is now `rgbs`
