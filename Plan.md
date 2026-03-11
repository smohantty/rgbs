# RGBS Plan: V1 Local Build

Date: 2026-03-11

## Objective

Build a Rust replacement for the old GBS local build path only.

Given:

- a package git tree
- a spec file under the packaging directory
- a `.gbs.conf` profile pointing at RPM repositories

`rgbs` v1 must:

1. resolve the correct build configuration and repositories
2. create or reuse a chroot build root for the target architecture
3. stage the package sources and spec correctly
4. resolve and install build dependencies
5. run `rpmbuild`
6. emit the same or materially equivalent RPM/SRPM artifacts as old GBS
7. make repeated local builds faster than old GBS

## Scope

### In scope

- `rgbs build`
- local build only
- chroot backend only
- `.gbs.conf` compatibility for local build use cases
- target architecture handling
- repo metadata fetch and cache
- `build.conf` resolution and use
- source/spec staging for local build
- build dependency resolution
- buildroot/chroot creation and reuse
- `rpmbuild` execution
- artifact collection into a GBS-compatible layout
- repeated builds using the same build root when inputs still match
- `--include-all`
- `--noinit`

### Out of scope for v1

- `remotebuild`
- OBS submission
- KVM backend
- `full-build`
- `deps-build`
- Jenkins integration
- image creation
- rewriting all other GBS subcommands

### Explicit v1.1 candidate

- exact old-GBS `--incremental` behavior

V1 should support fast repeated builds on the same build root. Exact incremental semantics are a follow-on feature because they are more package-layout-sensitive.

## Compatibility Boundary

The required compatibility target is the resulting build behavior and artifacts, not the old helper processes.

Old GBS implementation should be used as a behavioral reference only:

- use it to confirm feature coverage, flag semantics, artifact layout, and accepted edge-case behavior
- do not treat its process structure, helper boundaries, temporary path layout, or caching model as design guidance
- prefer simpler and more modern architecture whenever compatibility does not require the old behavior
- assume old GBS contains historical baggage and avoid carrying that baggage forward unless it is required for compatibility

Must preserve:

- repo precedence from `.gbs.conf`
- selected profile behavior
- `build.conf` macro and flag semantics
- target arch behavior
- staged source/spec input to `rpmbuild`
- RPM/SRPM artifact set and layout

Does not need to preserve:

- `depanneur`
- exact temporary paths
- exact shell command shapes
- exact log text

## Expected Result

For representative local packages and architectures, `rgbs build` should produce:

- the same package set
- the same NEVRA
- the same payload contents
- the same `Requires` / `Provides` / scriptlets, or only explicitly understood differences

Operationally, v1 should also produce:

- a reusable build root keyed by exact build inputs
- a persistent repo metadata cache
- a persistent dependency-solver cache
- measurably faster warm builds than old GBS

These speed expectations are target outcomes, not guarantees before benchmarking:

- cold builds: lower setup overhead than old GBS
- warm builds: materially faster due to metadata, solver, and buildroot reuse

## Success Criteria

V1 is successful when it:

1. builds representative local packages for the requested architecture using only the chroot backend
2. consumes `.gbs.conf` profile, repo, and `build.conf` settings compatibly for existing projects
3. produces the same NEVRA, payload contents, dependency metadata, and scriptlets as old GBS, or only explicitly understood and accepted differences
4. reaches at least comparable cold-build performance once parity is in place
5. delivers measurably faster warm builds through metadata, solver, and buildroot reuse

## Current Status

Implemented in tree:

- [x] `.gbs.conf` parsing, profile/repo/buildconf resolution, and CLI flag plumbing for local builds
- [x] repo metadata fetch/cache, `build.conf` materialization, and legacy `build.xml` compatibility handling
- [x] spec discovery, evaluated BuildRequires inspection, and `libsolv`-only dependency resolution with persistent solver caching
- [x] RPM download caching, exact-match buildroot reuse, and shared `--keep-packs` buildroot accumulation
- [x] persisted buildroot state for `--noinit`, including reuse of the cached buildroot path and cached `build.conf`
- [x] source/spec staging with committed-tree export by default and working-tree export for `--include-all`
- [x] `rpmbuild` execution through host or `bwrap`, artifact collection, and `--overwrite`-aware build skipping via persisted build stamps
- [x] optional per-build performance reporting via `--perf` / `--time`, covering stage timings and cache/reuse stats

Remaining before v1 signoff:

- [ ] benchmark harness against old GBS local builds
- [ ] fixture-driven artifact parity comparisons for representative packages and architectures
- [ ] reduce dependence on host runtime bootstrap by making the prepared root more self-contained for the `bwrap` path

Explicitly deferred past v1:

- [ ] exact old-GBS `--incremental` semantics

## User-Facing Command Model

V1 should support a command surface close to:

```bash
rgbs build [GITDIR] -A <arch>
rgbs build [GITDIR] -A <arch> -P <profile>
rgbs build [GITDIR] -A <arch> -R <repo>
rgbs build [GITDIR] -A <arch> -D <build.conf>
rgbs build [GITDIR] -A <arch> --include-all
rgbs build [GITDIR] -A <arch> --noinit
rgbs build [GITDIR] -A <arch> --perf
```

Priority flags to preserve in v1:

- `-A` / `--arch`
- `-P` / `--profile`
- `-R` / `--repository`
- `-D` / `--dist`
- `-B` / `--buildroot`
- `--include-all`
- `--noinit`
- `--clean`
- `--keep-packs`
- `--define`
- `--spec`
- `--skip-srcrpm`
- `--overwrite`
- `--fail-fast`
- `--clean-repos`

## Core Technical Decisions

### 1. Replace `depanneur`

`depanneur` should not be kept as a required runtime layer in `rgbs`.

Reason:

- it is an extra orchestration boundary
- the compatibility target is artifact parity, not helper-program parity
- removing it simplifies control flow and enables better caching and instrumentation

### 2. Keep `rpmbuild` authoritative

Do not reimplement RPM package build semantics in Rust.

V1 should:

- run `rpmbuild` for final package production
- use `rpmspec` and/or librpm for spec evaluation where necessary
- inject `build.conf` and CLI macros conservatively

### 3. Use a real RPM solver

Use `libsolv` for build dependency resolution against rpm-md metadata.

Reason:

- correctness matters more than hand-rolled cleverness here
- it is the right class of tool for RPM dependency solving
- it enables caching of solver inputs and outputs

Implementation note:

- use a bundled Rust crate integration so `rgbs` does not depend on a preinstalled system `libsolv`
- standardize on `libsolv` as the only dependency solver path; do not keep a parallel in-tree Rust fallback solver
- if the chosen crate misses a needed RPM symbol, add a thin local FFI shim rather than reintroducing custom resolution logic
- if the upstream crate bundles `libsolv` but does not compile rpm-md support, vendor and patch the crate build so the RPM path is reproducible inside this repository
- persist solver results keyed by exact repo fingerprint, arch, and evaluated BuildRequires so warm runs can skip both resolution and diagnostic recomputation

### 4. Reuse only on exact fingerprint match

Buildroot reuse is a core speed feature, but it must be safe.

Reuse gates should include:

- arch
- repo metadata fingerprint
- `build.conf` fingerprint
- selected macro fingerprint
- dependency closure fingerprint

No fuzzy reuse in v1.

### 5. Use a compatibility bridge for source export first

Source/spec staging is a high-risk parity area.

V1 should prefer:

- existing `gbs export`, or
- existing `gbp`-based export behavior

if that is the fastest path to parity.

Later, source export can be rewritten in Rust once fixtures and parity tests are strong enough.

## High-Level Architecture

```text
rgbs build
  -> config loader
  -> repo/build.conf resolver
  -> build plan fingerprint
  -> source/spec staging
  -> dependency solver
  -> RPM downloader
  -> chroot manager
  -> rpmbuild runner
  -> artifact collector
```

## Suggested Workspace / Crate Layout

```text
rgbs/
├── Cargo.toml
├── Plan.md
├── crates/
│   ├── rgbs-cli/        # CLI entrypoint, arg parsing, command dispatch
│   ├── rgbs-common/     # shared error types, arch types, logging, utils
│   ├── rgbs-config/     # .gbs.conf parsing and profile resolution
│   ├── rgbs-repo/       # repomd/primary/build.conf fetch and cache
│   ├── rgbs-spec/       # spec discovery and evaluated BuildRequires access
│   ├── rgbs-resolver/   # libsolv integration and dependency closure
│   ├── rgbs-builder/    # chroot creation/reuse and rpmbuild orchestration
│   └── rgbs-cache/      # persistent metadata / solver / fingerprint state
└── tests/
    └── fixtures/        # real package fixtures for artifact parity tests
```

## End-to-End Pipeline

### Stage 1: Load `.gbs.conf`

Inputs:

- project `.gbs.conf`
- user `~/.gbs.conf`
- system `/etc/gbs.conf`

Required behavior:

- profile selection
- repo reference expansion
- path interpolation and tilde expansion
- `passwdx` decode where needed
- local buildroot and packaging-dir handling

Output:

- normalized local-build configuration

### Stage 2: Resolve repos and `build.conf`

Tasks:

- normalize repo URLs/paths
- fetch `repomd.xml`
- locate and fetch primary metadata
- locate and fetch `build.conf`
- compute fingerprints for all of the above

Execution model:

- fetch remote repo metadata in parallel
- stream-parse XML rather than building large DOMs
- support both HTTP repos and local filesystem repos
- support legacy repo layouts where `builddata/build.xml` is the discovery entry point

Persistent cache:

- stored under `~/.cache/rgbs/`
- keyed by full URL hash and checksum
- supports local repo and HTTP repo sources

### Stage 3: Build plan fingerprint

The build plan should at minimum capture:

- arch
- selected profile
- repo URLs
- repo metadata checksums
- `build.conf` checksum
- spec path
- source tree identity
- CLI macro definitions
- build flags affecting dependency closure or root reuse

This fingerprint controls:

- buildroot reuse
- metadata reuse
- dependency-solver reuse
- source-staging reuse

### Stage 4: Stage source and spec

Required v1 cases:

- committed source only
- dirty working tree via `--include-all`

Approach:

- locate packaging dir and spec
- prepare deterministic build input
- use compatibility bridge for export when that reduces parity risk

Preferred v1 behavior:

- use the existing `gbs export` or `gbp` path where that is the fastest route to artifact parity

### Stage 5: Resolve build dependencies

Tasks:

- evaluate BuildRequires with the correct macro context
- resolve dependency closure for the target arch
- respect repo precedence and local repo overrides
- record the solved package set in the build plan

Implementation:

- `libsolv`
- solver cache keyed by repo fingerprint + evaluated spec fingerprint

Preferred spec-evaluation path:

- `rpmspec --query --buildrequires` for BuildRequires after macro expansion
- `rpmspec --query --queryformat` for key metadata such as name/version/release when needed

This reduces drift from hand-written spec parsing.

### Stage 5.5: Download required RPMs

Tasks:

- download the resolved dependency RPMs into a persistent local cache
- skip already cached artifacts when checksum and identity still match
- support bounded parallel downloads
- support HTTP auth from `.gbs.conf` when needed

Required behavior:

- cache directory stays under the build root or global cache in a deterministic layout
- prefer cache placement on the same filesystem as active build roots when possible
- retain HTTP validators so unchanged remote artifacts can be revalidated cheaply
- download failures retry conservatively and fail clearly

### Stage 6: Create or reuse chroot

Chroot identity should be keyed by:

- arch
- repo fingerprint
- `build.conf` fingerprint
- dependency closure fingerprint

Tasks:

- initialize root on first use or `--clean`
- install required RPMs
- preserve root for subsequent builds on exact fingerprint match
- invalidate root when relevant inputs change

Preferred materialization strategy:

- treat reused roots as immutable snapshots
- materialize the working root from a cached snapshot via reflink/clone when the filesystem supports it
- fall back to normal copy/install when cheap cloning is unavailable

### Stage 7: Run `rpmbuild`

Tasks:

- enter chroot
- inject the correct macro environment
- populate standard RPM working directories
- run `rpmbuild`
- capture timings, exit status, and logs

Preferred v1 execution:

- `-ba` for binary + source RPM generation
- `-bb` when `--skip-srcrpm` is selected
- explicit macro injection for CLI `--define` values and `build.conf`-derived settings

### Stage 8: Collect artifacts

Outputs:

- binary RPMs
- source RPMs when enabled
- logs
- local repo metadata

Required behavior:

- artifact layout should remain compatible with the old GBS local repo layout
- refresh local repo metadata after build using `createrepo_c` or equivalent standard tooling

## Buildroot and Artifact Layout

The v1 layout should stay close to GBS so existing workflows remain usable:

```text
~/GBS-ROOT/
├── local/
│   ├── repos/<profile>/<arch>/RPMS/
│   ├── repos/<profile>/<arch>/repodata/
│   ├── cache/<arch>/
│   └── BUILD-ROOTS/scratch.<arch>.0/
```

The exact scratch directory naming can vary if needed, but the logical structure should remain familiar and easy to inspect.

## External Tools Kept In V1

The following tools should remain external subprocess boundaries in v1:

- `rpmbuild`
  Used for final package production.
- `rpmspec` and/or librpm
  Used for macro-correct spec evaluation.
- `rpm`
  Used for chroot/root database initialization and RPM installation into the build root.
- `createrepo_c` or `createrepo`
  Used to refresh local output repo metadata.
- `gbs export` or `gbp`
  Used as a compatibility bridge for source tarball and patch staging until export parity is reimplemented.
- `chroot`, `mount`, and `umount`
  Used for the build environment itself.

Reason:

- these are correctness-critical ecosystem tools
- keeping them external in v1 reduces parity risk and shrinks rewrite scope

## Suggested Rust Building Blocks

These are recommended, not mandatory:

- `clap`
  CLI parsing
- `tokio`
  bounded parallel metadata fetch and download
- `reqwest`
  HTTP client
- `quick-xml`
  streaming XML parse
- `flate2`
  gzip decompression
- `bzip2` and `base64`
  `passwdx` decode
- `sha2`
  plan and cache fingerprints
- `serde` plus compact binary serialization
  metadata and solver cache persistence
- `tracing`
  structured logs and timings
- `indicatif`
  progress reporting

## Performance Strategy

Primary speed wins in v1 should come from:

- parallel repo metadata fetch
- persistent metadata cache keyed by repo identity and checksum
- persistent parsed-metadata cache so warm runs do not need to reparse rpm-md XML
- persistent dependency-solver cache keyed by exact build inputs
- exact-match buildroot reuse
- same-filesystem cache and root placement to enable cheap root materialization
- bounded parallel RPM downloads

Do not trade correctness for speed. Reuse remains exact-match only.

## Borrow From `uv` Architecture

`uv` has a few performance patterns worth copying directly, even though `rgbs` is working in the
RPM/chroot world instead of Python wheels:

### 1. Versioned cache buckets plus atomic writes

- split cache storage into versioned buckets for raw repo metadata, parsed metadata, solver state, downloaded RPMs, source staging, and root snapshots
- write cache entries through temporary files and atomic rename
- keep normal cache operation append-only and reserve destructive cleanup for explicit cache-prune flows

### 2. Cache normalized metadata, not only raw downloads

- store raw `repomd.xml`, primary metadata, and `build.conf` by URL plus checksum
- also persist a normalized binary representation that is fast to reload into the dependency solver
- treat metadata parsing as a hot-path optimization target, because warm builds should avoid repeating decompression and XML parsing work

### 3. Use explicit cache invalidation and refresh knobs

- keep exact fingerprints as the default reuse boundary
- add targeted refresh controls for repo metadata, solver state, source staging, and buildroot snapshots instead of forcing full cold rebuilds
- make source-staging fingerprints cheap by preferring git object identity and scoped dirty-file state over whole-tree walks when possible

### 4. Separate concurrency domains

- use independent limits for repo metadata fetch, RPM downloads, and root materialization
- avoid one global "parallelism" knob because the best values differ for network, disk, and chroot setup work
- apply conservative exponential backoff for transient network failures

### 5. Remember remote capability failures

- cache per-repo facts such as supported compression, successful validators, and whether a transport optimization is unsupported
- once an optimization fails for a repo, stop retrying it on every run until the repo fingerprint changes

### 6. Reuse immutable artifacts, not mutable worktrees

- keep cached artifacts and cached root snapshots immutable
- create mutable working roots from those immutable snapshots
- this preserves correctness while still allowing cheap warm-start paths

What does not transfer cleanly from `uv`:

- `uv`'s PubGrub-specific resolver heuristics are much less relevant because `rgbs` should stay on `libsolv`
- wheel-specific tricks such as range-reading zip metadata map only partially to RPM, where repo metadata is already the primary dependency source

## Implementation Phases

### Phase 0: benchmark and fixture baseline

Deliverables:

- benchmark harness against old GBS local build
- fixture projects covering:
  - simple single-package build
  - package with BuildRequires
  - `--include-all`
  - `--noinit`
  - multiple architectures

Exit criteria:

- baseline timings captured
- artifact comparison harness in place

### Phase 1: CLI, config, and metadata engine

Deliverables:

- workspace and crate skeleton
- `rgbs build --help`
- `.gbs.conf` parser
- parallel repo metadata fetch and persistent cache
- `build.conf` resolution

Exit criteria:

- same profile, repo selection, and buildconf selection as old GBS on fixtures

### Phase 2: spec access and dependency solver

Deliverables:

- spec discovery
- `rpmspec`/librpm-backed evaluated BuildRequires access path
- `libsolv` integration
- bundled `libsolv` crate integration with rpm-md loader support
- solved package set output

Exit criteria:

- same dependency closure as old GBS on fixtures, or only explicitly understood differences

### Phase 3: downloader, chroot manager, and package install

Deliverables:

- RPM downloader and cache
- chroot creation
- RPM install into root
- exact-match root reuse

Exit criteria:

- same build environment contents as old GBS within accepted tolerance
- isolated execution can bootstrap the minimal host rpm runtime into the prepared root when the root does not yet contain its own `rpmbuild` stack, while still preferring a self-contained root when available

### Phase 4: source staging and `rpmbuild` runner

Deliverables:

- compatibility export/staging path
- macro injection
- `rpmbuild` execution
- artifact collection

Exit criteria:

- single-package builds succeed end-to-end
- produced RPMs match old GBS for core fixtures

### Phase 5: warm-build optimization

Deliverables:

- `--noinit` parity
- source staging reuse
- repeated-build root reuse
- `--include-all` parity

Exit criteria:

- warm builds are measurably faster than old GBS
- repeated local builds can reuse the same root safely

### Phase 6: optional exact `--incremental` parity

Deliverables:

- explicit incremental mode
- partial rebuild behavior validated on supported package layouts

Exit criteria:

- incremental rebuilds are faster than normal warm rebuilds
- accepted artifact parity is preserved

## Expected Results by Phase

### After Phase 1

- `rgbs` can parse config and fetch repo/build metadata reliably
- repeated metadata fetches are already cheaper than old GBS due to persistence

### After Phase 2

- `rgbs` can explain exactly which packages it plans to install for the build
- dependency resolution becomes deterministic and inspectable

### After Phase 3

- `rgbs` can fetch the required RPM set and prepare or reuse the target-arch build root safely

### After Phase 4

- `rgbs build` can produce real RPMs locally through chroot and `rpmbuild`

### After Phase 5

- repeated local builds are materially faster than old GBS on the same machine

## Validation and Acceptance

For each fixture package, compare:

- RPM file names
- NEVRA
- `Requires` / `Provides`
- scriptlets
- file list
- extracted payload tree

Also compare:

- selected repo set
- selected `build.conf`
- resolved build dependency closure
- buildroot reuse behavior

Acceptance rule:

- if artifacts differ, the difference must be explained and explicitly accepted

## Main Risks

### 1. `build.conf` semantics drift

Mitigation:

- conservative parsing/injection
- parity tests on real packages

### 2. Spec evaluation drift

Mitigation:

- prefer `rpmspec` / librpm in v1

### 3. Chroot population drift

Mitigation:

- `libsolv`
- compare installed package sets against old GBS

### 4. Unsafe reuse

Mitigation:

- exact fingerprint matching only

### 5. Export parity

Mitigation:

- keep compatibility bridge first
- rewrite later

## Immediate Next Step

Start with the smallest prototype that validates the hardest correctness boundary:

1. parse `.gbs.conf`
2. resolve repos and `build.conf`
3. evaluate BuildRequires for one spec
4. resolve the dependency closure with `libsolv`
5. print the exact build plan

That prototype should come before the full chroot executor.
