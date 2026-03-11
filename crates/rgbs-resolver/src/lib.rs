use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;

use libc::{FILE, c_char, c_int};
use rattler_libsolv_c as solv;
use rgbs_common::{Result, RgbsError, atomic_write, cache_root, ensure_dir, sha256_hex};
use rgbs_repo::{PackageRecord, RepositoryState, ResolvedRepository};
use rgbs_spec::{Requirement, SpecInfo};
use serde::{Deserialize, Serialize};

// `rattler_libsolv_c` links the bundled `libsolvext`, but it does not expose the
// rpm-md loader symbol we need in its generated Rust bindings.
unsafe extern "C" {
    fn repo_add_rpmmd(
        repo: *mut solv::Repo,
        fp: *mut FILE,
        language: *const c_char,
        flags: c_int,
    ) -> c_int;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolvedPackage {
    pub name: String,
    pub nevra: String,
    pub repo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolveResult {
    pub backend: String,
    pub fingerprint: String,
    pub cache_hit: bool,
    pub selected: Vec<SolvedPackage>,
    pub unresolved: Vec<String>,
    pub problems: Vec<String>,
}

pub fn solve_build_requires(
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
) -> Result<SolveResult> {
    if let Some(mut cached) = load_cached_solve(repo_state, spec, arch)? {
        cached.cache_hit = true;
        return Ok(cached);
    }

    let pool = PoolHandle::new()?;
    unsafe {
        (*pool.0).disttype = solv::DISTTYPE_RPM as i32;
    }
    let arch_c = CString::new(arch)
        .map_err(|_| RgbsError::message(format!("invalid arch for libsolv: {arch}")))?;
    unsafe {
        solv::pool_setarch(pool.0, arch_c.as_ptr());
    }

    for (index, repository) in repo_state.repositories.iter().enumerate() {
        load_repository(pool.0, repository, index)?;
    }

    unsafe {
        solv::pool_addfileprovides(pool.0);
        solv::pool_createwhatprovides(pool.0);
    }

    let package_index = repo_state
        .packages
        .iter()
        .map(|package| {
            (
                (
                    package.repo_name.clone(),
                    package.name.clone(),
                    package.arch.clone(),
                    package.evr(),
                ),
                package,
            )
        })
        .collect::<HashMap<_, _>>();

    let mut job = QueueHandle::new();
    for requirement in spec
        .build_requires
        .iter()
        .filter(|item| !is_ignored_requirement(&item.name))
    {
        let depid = requirement_to_depid(pool.0, requirement)?;
        unsafe {
            solv::queue_insert2(
                &mut job.0,
                job.0.count,
                (solv::SOLVER_INSTALL | solv::SOLVER_SOLVABLE_PROVIDES) as i32,
                depid,
            );
        }
    }

    let solver = SolverHandle::new(pool.0)?;
    unsafe {
        solv::solver_set_flag(solver.0, solv::SOLVER_FLAG_SPLITPROVIDES as c_int, 1);
        solv::solver_set_flag(solver.0, solv::SOLVER_FLAG_IGNORE_RECOMMENDED as c_int, 1);
        solv::solver_set_flag(solver.0, solv::SOLVER_FLAG_STRICT_REPO_PRIORITY as c_int, 1);
    }

    let problem_count = unsafe { solv::solver_solve(solver.0, &mut job.0) };
    let problems = collect_problem_strings(solver.0);
    let unresolved = if problem_count == 0 {
        Vec::new()
    } else {
        spec.build_requires
            .iter()
            .map(format_requirement)
            .collect::<Vec<_>>()
    };

    let transaction = TransactionHandle::new(unsafe { solv::solver_create_transaction(solver.0) })?;
    unsafe {
        solv::transaction_order(transaction.0, 0);
    }
    let mut selected = selected_packages(pool.0, transaction.0, &package_index)?;
    selected.sort_by(|left, right| left.nevra.cmp(&right.nevra));

    let result = SolveResult {
        backend: "libsolv".to_string(),
        fingerprint: sha256_hex(
            selected
                .iter()
                .map(|package| package.nevra.clone())
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        cache_hit: false,
        selected,
        unresolved,
        problems,
    };
    save_cached_solve(repo_state, spec, arch, &result)?;
    Ok(result)
}

#[derive(Debug, Serialize)]
struct SolveCacheKey<'a> {
    schema: &'static str,
    arch: &'a str,
    repo_fingerprint: &'a str,
    spec_name: &'a str,
    spec_version: &'a str,
    spec_release: &'a str,
    build_requires: &'a [Requirement],
}

fn solve_cache_path(repo_state: &RepositoryState, spec: &SpecInfo, arch: &str) -> Result<PathBuf> {
    solve_cache_path_in(
        &cache_root()?.join("v1").join("solver"),
        repo_state,
        spec,
        arch,
    )
}

fn solve_cache_path_in(
    cache_dir: &Path,
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
) -> Result<PathBuf> {
    let key = SolveCacheKey {
        schema: "solve-v1",
        arch,
        repo_fingerprint: &repo_state.fingerprint,
        spec_name: &spec.name,
        spec_version: &spec.version,
        spec_release: &spec.release,
        build_requires: &spec.build_requires,
    };
    let bytes = serde_json::to_vec(&key)
        .map_err(|err| RgbsError::message(format!("solver cache key: {err}")))?;
    Ok(cache_dir.join(format!("{}.json", sha256_hex(bytes))))
}

fn load_cached_solve(
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
) -> Result<Option<SolveResult>> {
    let path = solve_cache_path(repo_state, spec, arch)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|err| RgbsError::io(&path, err))?;
    match serde_json::from_slice::<SolveResult>(&bytes) {
        Ok(result) => Ok(Some(result)),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
fn load_cached_solve_from(
    cache_dir: &Path,
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
) -> Result<Option<SolveResult>> {
    let path = solve_cache_path_in(cache_dir, repo_state, spec, arch)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|err| RgbsError::io(&path, err))?;
    match serde_json::from_slice::<SolveResult>(&bytes) {
        Ok(result) => Ok(Some(result)),
        Err(_) => Ok(None),
    }
}

fn save_cached_solve(
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
    result: &SolveResult,
) -> Result<()> {
    let path = solve_cache_path(repo_state, spec, arch)?;
    let bytes = serde_json::to_vec_pretty(result)
        .map_err(|err| RgbsError::message(format!("serialize solver cache: {err}")))?;
    atomic_write(&path, &bytes)
}

#[cfg(test)]
fn save_cached_solve_to(
    cache_dir: &Path,
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
    result: &SolveResult,
) -> Result<()> {
    let path = solve_cache_path_in(cache_dir, repo_state, spec, arch)?;
    let bytes = serde_json::to_vec_pretty(result)
        .map_err(|err| RgbsError::message(format!("serialize solver cache: {err}")))?;
    atomic_write(&path, &bytes)
}

fn load_repository(
    pool: *mut solv::Pool,
    repository: &ResolvedRepository,
    index: usize,
) -> Result<()> {
    let repo_name = CString::new(repository.name.clone())
        .map_err(|_| RgbsError::message(format!("invalid repo name: {}", repository.name)))?;
    let repo = unsafe { solv::repo_create(pool, repo_name.as_ptr()) };
    if repo.is_null() {
        return Err(RgbsError::message(format!(
            "failed to create libsolv repo for {}",
            repository.name
        )));
    }

    unsafe {
        (*repo).priority = (1000 - index as i32) as c_int;
    }

    let primary_path = PathBuf::from(&repository.primary_cache_path);
    let solv_cache_path = solv_cache_path(repository)?;
    let add_result = if solv_cache_path.exists() {
        with_c_file(&solv_cache_path, "rb", |fp| unsafe {
            solv::repo_add_solv(repo, fp, 0)
        })?
    } else {
        let parsed = with_c_file(&primary_path, "rb", |fp| unsafe {
            repo_add_rpmmd(repo, fp, ptr::null(), 0)
        })?;
        if parsed == 0 {
            unsafe { solv::repo_internalize(repo) };
            ensure_dir(
                solv_cache_path
                    .parent()
                    .ok_or_else(|| RgbsError::message("solv cache path has no parent"))?,
            )?;
            with_c_file(&solv_cache_path, "wb", |fp| unsafe {
                solv::repo_write(repo, fp)
            })?;
        }
        parsed
    };

    if add_result != 0 {
        return Err(RgbsError::message(pool_error(pool)));
    }

    unsafe {
        solv::repo_internalize(repo);
    }
    Ok(())
}

fn solv_cache_path(repository: &ResolvedRepository) -> Result<PathBuf> {
    let primary_path = PathBuf::from(&repository.primary_cache_path);
    let cache_dir = primary_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| RgbsError::message("unable to derive libsolv cache dir"))?
        .join("solv");
    let cache_key = repository
        .repomd_checksum
        .clone()
        .unwrap_or_else(|| sha256_hex(repository.primary_cache_path.as_bytes()));
    Ok(cache_dir.join(format!("{cache_key}.solv")))
}

fn requirement_to_depid(pool: *mut solv::Pool, requirement: &Requirement) -> Result<solv::Id> {
    let name = CString::new(requirement.name.as_str())
        .map_err(|_| RgbsError::message(format!("invalid requirement: {}", requirement.name)))?;
    let name_id = unsafe { solv::pool_str2id(pool, name.as_ptr(), 1) };
    if name_id == 0 {
        return Err(RgbsError::message(format!(
            "failed to intern dependency name: {}",
            requirement.name
        )));
    }

    match (&requirement.flags, &requirement.version) {
        (Some(flags), Some(version)) => {
            let version = CString::new(version.as_str()).map_err(|_| {
                RgbsError::message(format!("invalid dependency version: {version}"))
            })?;
            let version_id = unsafe { solv::pool_str2id(pool, version.as_ptr(), 1) };
            let rel_flags = match_relation(flags)?;
            Ok(unsafe { solv::pool_rel2id(pool, name_id, version_id, rel_flags, 1) })
        }
        _ => Ok(name_id),
    }
}

fn match_relation(flags: &str) -> Result<c_int> {
    let relation = match flags {
        "=" | "==" | "EQ" => solv::REL_EQ as c_int,
        ">" | "GT" => solv::REL_GT as c_int,
        ">=" | "GE" => (solv::REL_GT | solv::REL_EQ) as c_int,
        "<" | "LT" => solv::REL_LT as c_int,
        "<=" | "LE" => (solv::REL_LT | solv::REL_EQ) as c_int,
        _ => {
            return Err(RgbsError::message(format!(
                "unsupported dependency relation for libsolv: {flags}"
            )));
        }
    };
    Ok(relation)
}

fn selected_packages(
    pool: *mut solv::Pool,
    transaction: *mut solv::Transaction,
    package_index: &HashMap<(String, String, String, String), &PackageRecord>,
) -> Result<Vec<SolvedPackage>> {
    let mut selected = Vec::new();
    let steps = unsafe { &(*transaction).steps };
    for index in 0..steps.count {
        let solvable_id = unsafe { *steps.elements.add(index as usize) };
        let action = unsafe {
            solv::transaction_type(
                transaction,
                solvable_id,
                solv::SOLVER_TRANSACTION_RPM_ONLY as c_int,
            )
        };
        if action != solv::SOLVER_TRANSACTION_INSTALL as i32
            && action != solv::SOLVER_TRANSACTION_MULTIINSTALL as i32
        {
            continue;
        }

        let solvable = unsafe { (*pool).solvables.add(solvable_id as usize) };
        let name = unsafe { pool_id_to_string(pool, (*solvable).name) };
        let arch = unsafe { pool_id_to_string(pool, (*solvable).arch) };
        let evr = unsafe { pool_id_to_string(pool, (*solvable).evr) };
        let repo_name = unsafe { c_string((*(*solvable).repo).name) };

        if let Some(package) =
            package_index.get(&(repo_name.clone(), name.clone(), arch.clone(), evr.clone()))
        {
            selected.push(SolvedPackage {
                name: package.name.clone(),
                nevra: package.nevra(),
                repo: package.repo_location.clone(),
            });
            continue;
        }

        selected.push(SolvedPackage {
            name: name.clone(),
            nevra: format!("{name}-{evr}.{arch}"),
            repo: repo_name,
        });
    }
    Ok(selected)
}

fn collect_problem_strings(solver: *mut solv::Solver) -> Vec<String> {
    let count = unsafe { solv::solver_problem_count(solver) };
    let mut problems = Vec::new();
    for problem in 1..=count {
        let problem = problem as i32;
        let text = unsafe { c_string(solv::solver_problem2str(solver, problem)) };
        if !text.is_empty() {
            problems.push(text);
        }
    }
    problems
}

fn with_c_file<T>(path: &Path, mode: &str, callback: impl FnOnce(*mut FILE) -> T) -> Result<T> {
    let file = CFile::open(path, mode)?;
    Ok(callback(file.0))
}

fn pool_error(pool: *mut solv::Pool) -> String {
    let ptr = unsafe { solv::pool_errstr(pool) };
    c_string(ptr)
}

fn pool_id_to_string(pool: *mut solv::Pool, id: solv::Id) -> String {
    let ptr = unsafe { solv::pool_id2str(pool, id) };
    c_string(ptr)
}

fn c_string(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

fn is_ignored_requirement(name: &str) -> bool {
    name.starts_with("rpmlib(") || name.starts_with("config(") || name.starts_with('/')
}

fn format_requirement(requirement: &Requirement) -> String {
    match (&requirement.flags, &requirement.version) {
        (Some(flags), Some(version)) => format!("{} {} {}", requirement.name, flags, version),
        _ => requirement.name.clone(),
    }
}

struct PoolHandle(*mut solv::Pool);

impl PoolHandle {
    fn new() -> Result<Self> {
        let pool = unsafe { solv::pool_create() };
        if pool.is_null() {
            return Err(RgbsError::message("failed to create libsolv pool"));
        }
        Ok(Self(pool))
    }
}

impl Drop for PoolHandle {
    fn drop(&mut self) {
        unsafe {
            solv::pool_free(self.0);
        }
    }
}

struct SolverHandle(*mut solv::Solver);

impl SolverHandle {
    fn new(pool: *mut solv::Pool) -> Result<Self> {
        let solver = unsafe { solv::solver_create(pool) };
        if solver.is_null() {
            return Err(RgbsError::message("failed to create libsolv solver"));
        }
        Ok(Self(solver))
    }
}

impl Drop for SolverHandle {
    fn drop(&mut self) {
        unsafe {
            solv::solver_free(self.0);
        }
    }
}

struct TransactionHandle(*mut solv::Transaction);

impl TransactionHandle {
    fn new(transaction: *mut solv::Transaction) -> Result<Self> {
        if transaction.is_null() {
            return Err(RgbsError::message("failed to create libsolv transaction"));
        }
        Ok(Self(transaction))
    }
}

impl Drop for TransactionHandle {
    fn drop(&mut self) {
        unsafe {
            solv::transaction_free(self.0);
        }
    }
}

struct QueueHandle(solv::Queue);

impl QueueHandle {
    fn new() -> Self {
        let mut queue = unsafe { std::mem::zeroed() };
        unsafe {
            solv::queue_init(&mut queue);
        }
        Self(queue)
    }
}

impl Drop for QueueHandle {
    fn drop(&mut self) {
        unsafe {
            solv::queue_free(&mut self.0);
        }
    }
}

struct CFile(*mut FILE);

impl CFile {
    fn open(path: &Path, mode: &str) -> Result<Self> {
        let display_path = path.display().to_string();
        let path_cstr = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            RgbsError::message(format!("invalid path for fopen: {}", path.display()))
        })?;
        let mode = CString::new(mode)
            .map_err(|_| RgbsError::message(format!("invalid fopen mode: {mode}")))?;
        let file = unsafe { libc::fopen(path_cstr.as_ptr(), mode.as_ptr()) };
        if file.is_null() {
            return Err(RgbsError::message(format!(
                "failed to open file for libsolv: {}",
                display_path
            )));
        }
        Ok(Self(file))
    }
}

impl Drop for CFile {
    fn drop(&mut self) {
        unsafe {
            libc::fclose(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use rgbs_common::path_to_string;
    use rgbs_config::ResolvedRepo;
    use rgbs_repo::{ResolveRequest, resolve_repositories};
    use tempfile::TempDir;

    use super::*;

    static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn solves_simple_build_requires_with_libsolv() {
        let fixture = Fixture::new();
        let repo = fixture.create_repo();
        let state = with_cache_home(&fixture.root.join("cache"), || {
            resolve_repositories(&ResolveRequest {
                arch: "aarch64".to_string(),
                repos: vec![ResolvedRepo {
                    name: "repo.local".to_string(),
                    kind: rgbs_config::RepoKind::LocalPath,
                    location: path_to_string(&repo),
                    raw_location: path_to_string(&repo),
                    source: "test".to_string(),
                    user: None,
                    authenticated: false,
                    password: None,
                }],
                explicit_buildconf: None,
                clean_cache: true,
            })
        })
        .unwrap();
        let spec = SpecInfo {
            packaging_dir: String::new(),
            spec_path: String::new(),
            name: "fake".to_string(),
            version: "1.0".to_string(),
            release: "1".to_string(),
            binary_packages: vec!["fake".to_string()],
            provides: vec!["fake".to_string()],
            build_requires: vec![
                Requirement {
                    name: "bash".to_string(),
                    flags: None,
                    version: None,
                },
                Requirement {
                    name: "pkgconfig(alsa)".to_string(),
                    flags: None,
                    version: None,
                },
            ],
            sources: Vec::new(),
        };

        let solved = solve_build_requires(&state, &spec, "aarch64").unwrap();
        assert_eq!(solved.backend, "libsolv");
        assert!(solved.problems.is_empty());
        assert_eq!(solved.selected.len(), 2);
        assert!(solved.selected.iter().any(|item| item.name == "bash"));
        assert!(
            solved
                .selected
                .iter()
                .any(|item| item.name == "pkgconfig-alsa")
        );
    }

    #[test]
    fn persists_solver_cache_by_exact_inputs() {
        let fixture = Fixture::new();
        let repo = fixture.create_repo();
        let state = with_cache_home(&fixture.root.join("cache"), || {
            resolve_repositories(&ResolveRequest {
                arch: "aarch64".to_string(),
                repos: vec![ResolvedRepo {
                    name: "repo.local".to_string(),
                    kind: rgbs_config::RepoKind::LocalPath,
                    location: path_to_string(&repo),
                    raw_location: path_to_string(&repo),
                    source: "test".to_string(),
                    user: None,
                    authenticated: false,
                    password: None,
                }],
                explicit_buildconf: None,
                clean_cache: true,
            })
        })
        .unwrap();
        let spec = sample_spec();
        let cache_dir = fixture.root.join("solver-cache");

        let uncached = SolveResult {
            backend: "libsolv".to_string(),
            fingerprint: "abc123".to_string(),
            cache_hit: false,
            selected: vec![SolvedPackage {
                name: "bash".to_string(),
                nevra: "bash-5.0-1.aarch64".to_string(),
                repo: path_to_string(&repo),
            }],
            unresolved: Vec::new(),
            problems: Vec::new(),
        };
        save_cached_solve_to(&cache_dir, &state, &spec, "aarch64", &uncached).unwrap();

        let cached = load_cached_solve_from(&cache_dir, &state, &spec, "aarch64")
            .unwrap()
            .unwrap();
        assert_eq!(cached.backend, "libsolv");
        assert_eq!(cached.fingerprint, "abc123");
        assert!(!cached.cache_hit);
        assert_eq!(cached.selected.len(), 1);
    }

    fn sample_spec() -> SpecInfo {
        SpecInfo {
            packaging_dir: String::new(),
            spec_path: String::new(),
            name: "fake".to_string(),
            version: "1.0".to_string(),
            release: "1".to_string(),
            binary_packages: vec!["fake".to_string()],
            provides: vec!["fake".to_string()],
            build_requires: vec![
                Requirement {
                    name: "bash".to_string(),
                    flags: None,
                    version: None,
                },
                Requirement {
                    name: "pkgconfig(alsa)".to_string(),
                    flags: None,
                    version: None,
                },
            ],
            sources: Vec::new(),
        }
    }

    fn with_cache_home<T>(path: &Path, callback: impl FnOnce() -> T) -> T {
        let _guard = CACHE_ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("XDG_CACHE_HOME", path);
        }
        let result = callback();
        unsafe {
            std::env::remove_var("XDG_CACHE_HOME");
        }
        result
    }

    struct Fixture {
        _temp_dir: TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp_dir = TempDir::new().unwrap();
            let root = temp_dir.path().join("repo");
            fs::create_dir_all(root.join("repodata")).unwrap();
            fs::create_dir_all(root.join("packages")).unwrap();
            Self {
                _temp_dir: temp_dir,
                root,
            }
        }

        fn create_repo(&self) -> PathBuf {
            fs::write(self.root.join("packages/bash.rpm"), b"rpm").unwrap();
            fs::write(self.root.join("packages/pkgconfig-alsa.rpm"), b"rpm").unwrap();
            let primary = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" packages="2">
  <package type="rpm">
    <name>bash</name>
    <arch>aarch64</arch>
    <version epoch="0" ver="5.0" rel="1"/>
    <checksum type="sha256" pkgid="YES">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</checksum>
    <location href="packages/bash.rpm"/>
    <format xmlns:rpm="http://linux.duke.edu/metadata/rpm">
      <rpm:provides>
        <rpm:entry name="bash" flags="EQ" ver="5.0" rel="1" epoch="0"/>
      </rpm:provides>
      <rpm:requires />
    </format>
  </package>
  <package type="rpm">
    <name>pkgconfig-alsa</name>
    <arch>noarch</arch>
    <version epoch="0" ver="1.0" rel="1"/>
    <checksum type="sha256" pkgid="YES">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</checksum>
    <location href="packages/pkgconfig-alsa.rpm"/>
    <format xmlns:rpm="http://linux.duke.edu/metadata/rpm">
      <rpm:provides>
        <rpm:entry name="pkgconfig(alsa)" flags="EQ" ver="1.0" rel="1" epoch="0"/>
      </rpm:provides>
      <rpm:requires />
    </format>
  </package>
</metadata>
"#;
            let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(primary.as_bytes()).unwrap();
            fs::write(
                self.root.join("repodata/primary.xml.gz"),
                encoder.finish().unwrap(),
            )
            .unwrap();
            let repomd = r#"<?xml version="1.0" encoding="UTF-8"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary">
    <checksum type="sha256">cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc</checksum>
    <location href="repodata/primary.xml.gz"/>
  </data>
</repomd>
"#;
            fs::write(self.root.join("repodata/repomd.xml"), repomd).unwrap();
            self.root.clone()
        }
    }
}
