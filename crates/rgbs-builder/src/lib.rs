use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rgbs_common::{
    Result, RgbsError, atomic_write, cache_root, current_build_log_paths, ensure_dir,
    log_debug_line, log_progress_line, path_to_string, print_status, print_warning, render_command,
    run_command, sha256_hex,
};
use rgbs_config::ResolvedBuildPlan;
use rgbs_repo::{
    PackageRecord, RepositoryState, ResolveRequest, ResolvedRepository, resolve_repositories,
};
use rgbs_resolver::solve_build_requires;
use rgbs_spec::{InspectRequest, SpecInfo, inspect_spec};
use serde::{Deserialize, Serialize};
use url::Url;
use walkdir::WalkDir;

#[derive(Debug, Clone, Serialize)]
pub struct RepositorySummary {
    pub cache_root: String,
    pub fingerprint: String,
    pub buildconf: Option<String>,
    pub repositories: Vec<ResolvedRepository>,
    pub package_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyPackage {
    pub name: String,
    pub nevra: String,
    pub repo: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DependencyResolution {
    pub backend: String,
    pub fingerprint: String,
    pub cache_hit: bool,
    pub selected: Vec<DependencyPackage>,
    pub unresolved: Vec<String>,
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadedPackage {
    pub name: String,
    pub nevra: String,
    pub path: String,
    pub reused: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildRootSummary {
    pub path: String,
    pub fingerprint: String,
    pub strategy: String,
    pub package_fingerprint: String,
    pub reused: bool,
    pub installed_packages: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageSummary {
    pub topdir: String,
    pub spec_path: String,
    pub export_mode: String,
    pub source_fingerprint: String,
    pub reused: bool,
    pub generated_sources: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerSummary {
    pub backend: String,
    pub command: String,
    pub log_path: String,
    pub reused: bool,
}

#[derive(Debug)]
struct RunnerExecution {
    summary: RunnerSummary,
    warnings: Vec<String>,
}

#[derive(Debug)]
struct StageExecution {
    summary: StageSummary,
    warnings: Vec<String>,
}

#[derive(Debug, Clone)]
struct BindMount {
    source: PathBuf,
    destination: PathBuf,
}

#[derive(Debug, Clone)]
struct BwrapRuntime {
    binds: Vec<BindMount>,
}

#[derive(Debug, Clone)]
struct SourceSnapshot {
    root: PathBuf,
    packaging_rel: PathBuf,
    spec_rel: PathBuf,
    export_mode: String,
    fingerprint: String,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedBuildRoot {
    schema: String,
    arch: String,
    profile: String,
    strategy: String,
    root_path: String,
    fingerprint: String,
    package_fingerprint: String,
    dependency_fingerprint: Option<String>,
    repo_fingerprint: Option<String>,
    buildconf_path: Option<String>,
    buildconf_fingerprint: Option<String>,
    defines_fingerprint: String,
    installed_packages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildStamp {
    schema: String,
    source_fingerprint: String,
    buildroot_fingerprint: Option<String>,
    buildroot_package_fingerprint: Option<String>,
    skip_srcrpm: bool,
    spec_name: String,
    spec_version: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ArtifactSummary {
    pub rpms: Vec<String>,
    pub srpms: Vec<String>,
    pub output_repo: String,
    pub repodata_refreshed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildLogsSummary {
    pub session_dir: String,
    pub progress_log: String,
    pub debug_log: String,
    pub plan_json: String,
    pub config_snapshot: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceSummary {
    pub total_ms: u64,
    pub repository_ms: Option<u64>,
    pub spec_ms: u64,
    pub dependency_ms: Option<u64>,
    pub download_ms: Option<u64>,
    pub buildroot_ms: Option<u64>,
    pub stage_ms: u64,
    pub rpmbuild_ms: u64,
    pub artifacts_ms: u64,
    pub solver_cache_hit: Option<bool>,
    pub downloaded_packages: usize,
    pub reused_package_downloads: usize,
    pub buildroot_reused: Option<bool>,
    pub stage_reused: bool,
    pub runner_reused: bool,
    pub artifact_rpms: usize,
    pub artifact_srpms: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct BuildOutcome {
    pub execution: String,
    pub plan: ResolvedBuildPlan,
    pub repository: Option<RepositorySummary>,
    pub spec: SpecInfo,
    pub dependencies: Option<DependencyResolution>,
    pub downloads: Vec<DownloadedPackage>,
    pub buildroot: Option<BuildRootSummary>,
    pub stage: StageSummary,
    pub runner: RunnerSummary,
    pub artifacts: ArtifactSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logs: Option<BuildLogsSummary>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub performance: Option<PerformanceSummary>,
}

fn progress(action: &str, message: impl AsRef<str>) {
    let message = message.as_ref();
    print_status(action, message);
    log_progress_line(format!("{action}: {message}"));
}

fn progress_warn(message: impl AsRef<str>) {
    let message = message.as_ref();
    print_warning(message);
    log_progress_line(format!("warning: {message}"));
    log_debug_line(format!("warning: {message}"));
}

pub fn execute_build(plan: &ResolvedBuildPlan) -> Result<BuildOutcome> {
    let total_started = Instant::now();
    let cached_buildroot = if plan.noinit {
        progress(
            "Reusing",
            format!(
                "reusing cached buildroot state for --noinit [{}]",
                plan.arch
            ),
        );
        Some(load_active_buildroot(plan)?)
    } else {
        progress(
            "Resolving",
            format!(
                "resolving repositories for profile {} [{}]",
                plan.profile.name, plan.arch
            ),
        );
        None
    };
    let (repo_state, repository_ms) = if plan.noinit {
        (None, None)
    } else {
        let started = Instant::now();
        let state = resolve_repositories(&ResolveRequest {
            arch: plan.arch.clone(),
            repos: plan.repos.clone(),
            explicit_buildconf: plan.buildconf.clone(),
            clean_cache: plan.clean_repos,
        })?;
        progress(
            "Resolved",
            format!(
                "repository metadata ready: {} packages{}",
                state.package_count,
                state
                    .buildconf
                    .as_deref()
                    .map(|path| format!(", buildconf {}", path))
                    .unwrap_or_default()
            ),
        );
        (Some(state), Some(duration_ms(started.elapsed())))
    };
    let buildconf_path = repo_state
        .as_ref()
        .and_then(|state| state.buildconf.as_ref().map(PathBuf::from))
        .or_else(|| plan.buildconf.as_ref().map(PathBuf::from));
    let buildconf_path = buildconf_path.or_else(|| {
        cached_buildroot
            .as_ref()
            .and_then(|state| state.buildconf_path.as_ref().map(PathBuf::from))
    });
    let spec_started = Instant::now();
    progress("Inspecting", format!("spec under {}", plan.packaging_dir));
    let spec = inspect_spec(&InspectRequest {
        git_dir: PathBuf::from(&plan.git_dir),
        packaging_dir: plan.packaging_dir.clone(),
        spec_override: plan.spec.as_ref().map(PathBuf::from),
        buildconf: buildconf_path.clone(),
        defines: plan.defines.clone(),
    })?;
    progress(
        "Selected",
        format!(
            "selected spec {} ({}-{}-{})",
            spec.spec_path, spec.name, spec.version, spec.release
        ),
    );
    let spec_ms = duration_ms(spec_started.elapsed());

    let (
        dependencies,
        downloads,
        buildroot,
        warnings,
        dependency_ms,
        download_ms,
        buildroot_ms,
        solver_cache_hit,
        reused_package_downloads,
    ) = if let Some(repo_state) = repo_state.as_ref() {
        let dependency_started = Instant::now();
        progress("Solving", "BuildRequires");
        let dependencies = resolve_build_dependencies(repo_state, &spec, &plan.arch)?;
        let dependency_ms = Some(duration_ms(dependency_started.elapsed()));
        progress(
            "Resolved",
            format!(
                "resolved {} dependency packages{}",
                dependencies.selected.len(),
                if dependencies.cache_hit {
                    " (cache hit)"
                } else {
                    ""
                }
            ),
        );
        if !dependencies.unresolved.is_empty() || !dependencies.problems.is_empty() {
            return Err(RgbsError::message(format!(
                "dependency resolution failed: {}{}",
                if dependencies.unresolved.is_empty() {
                    String::new()
                } else {
                    format!("unresolved: {}", dependencies.unresolved.join(", "))
                },
                if dependencies.problems.is_empty() {
                    String::new()
                } else {
                    format!(
                        "{}problems: {}",
                        if dependencies.unresolved.is_empty() {
                            ""
                        } else {
                            "; "
                        },
                        dependencies.problems.join(" | ")
                    )
                }
            )));
        }

        let download_started = Instant::now();
        progress(
            "Downloading",
            format!(
                "materializing {} RPMs into the download cache",
                dependencies.selected.len()
            ),
        );
        let downloads = download_packages(&dependencies, repo_state)?;
        let download_ms = Some(duration_ms(download_started.elapsed()));
        let reused_package_downloads = downloads.iter().filter(|package| package.reused).count();
        progress(
            "Downloaded",
            format!(
                "download cache ready: {} reused, {} fetched",
                reused_package_downloads,
                downloads.len().saturating_sub(reused_package_downloads)
            ),
        );
        let buildroot_started = Instant::now();
        progress("Preparing", format!("buildroot under {}", plan.buildroot));
        let buildroot = prepare_buildroot(
            plan,
            repo_state,
            &dependencies,
            &downloads,
            buildconf_path.as_deref(),
        )?;
        progress(
            "Prepared",
            format!(
                "buildroot {} at {} ({} packages)",
                if buildroot.reused { "reused" } else { "ready" },
                buildroot.path,
                buildroot.installed_packages
            ),
        );
        let buildroot_ms = Some(duration_ms(buildroot_started.elapsed()));
        let solver_cache_hit = Some(dependencies.cache_hit);
        (
            Some(dependencies),
            downloads,
            Some(buildroot),
            Vec::new(),
            dependency_ms,
            download_ms,
            buildroot_ms,
            solver_cache_hit,
            reused_package_downloads,
        )
    } else {
        let buildroot_started = Instant::now();
        let buildroot = cached_buildroot
            .as_ref()
            .map(buildroot_summary_from_state)
            .transpose()?
            .ok_or_else(|| RgbsError::message("missing cached buildroot state for --noinit"))?;
        progress(
            "Reusing",
            format!(
                "reused cached buildroot {} ({} packages)",
                buildroot.path, buildroot.installed_packages
            ),
        );
        let buildroot_ms = Some(duration_ms(buildroot_started.elapsed()));
        (
            None,
            Vec::new(),
            Some(buildroot),
            vec![format!(
                "--noinit skipped repository resolution and reused buildroot {}",
                cached_buildroot
                    .as_ref()
                    .map(|state| state.root_path.as_str())
                    .unwrap_or_default()
            )],
            None,
            None,
            buildroot_ms,
            None,
            0,
        )
    };

    let stage_started = Instant::now();
    progress("Staging", "sources");
    let stage = stage_sources(plan, &spec)?;
    progress(
        "Staged",
        format!(
            "stage {} at {}",
            if stage.summary.reused {
                "reused"
            } else {
                "prepared"
            },
            stage.summary.topdir
        ),
    );
    let stage_ms = duration_ms(stage_started.elapsed());
    let rpmbuild_started = Instant::now();
    let runner = run_rpmbuild(
        plan,
        &spec,
        &stage.summary,
        buildconf_path.as_deref(),
        buildroot.as_ref(),
    )?;
    let rpmbuild_ms = duration_ms(rpmbuild_started.elapsed());
    let mut warnings = warnings;
    warnings.extend(stage.warnings);
    warnings.extend(runner.warnings);
    let artifacts_started = Instant::now();
    progress("Collecting", "artifacts");
    let artifacts = collect_artifacts(plan, &stage.summary)?;
    progress(
        "Collected",
        format!(
            "collected {} RPMs and {} SRPMs into {}",
            artifacts.rpms.len(),
            artifacts.srpms.len(),
            artifacts.output_repo
        ),
    );
    let artifacts_ms = duration_ms(artifacts_started.elapsed());
    let performance = plan.perf.then(|| PerformanceSummary {
        total_ms: duration_ms(total_started.elapsed()),
        repository_ms,
        spec_ms,
        dependency_ms,
        download_ms,
        buildroot_ms,
        stage_ms,
        rpmbuild_ms,
        artifacts_ms,
        solver_cache_hit,
        downloaded_packages: downloads.len(),
        reused_package_downloads,
        buildroot_reused: buildroot.as_ref().map(|root| root.reused),
        stage_reused: stage.summary.reused,
        runner_reused: runner.summary.reused,
        artifact_rpms: artifacts.rpms.len(),
        artifact_srpms: artifacts.srpms.len(),
    });
    progress(
        "Finished",
        format!(
            "build complete in {} ms",
            duration_ms(total_started.elapsed())
        ),
    );

    Ok(BuildOutcome {
        execution: "completed".to_string(),
        plan: plan.clone(),
        repository: repo_state.as_ref().map(repository_summary),
        spec,
        dependencies,
        downloads,
        buildroot,
        stage: stage.summary,
        runner: runner.summary,
        artifacts,
        logs: current_build_log_paths().map(|paths| BuildLogsSummary {
            session_dir: path_to_string(&paths.session_dir),
            progress_log: path_to_string(&paths.progress_log),
            debug_log: path_to_string(&paths.debug_log),
            plan_json: path_to_string(&paths.plan_json),
            config_snapshot: path_to_string(&paths.config_snapshot),
        }),
        warnings,
        performance,
    })
}

fn repository_summary(state: &RepositoryState) -> RepositorySummary {
    RepositorySummary {
        cache_root: state.cache_root.clone(),
        fingerprint: state.fingerprint.clone(),
        buildconf: state.buildconf.clone(),
        repositories: state.repositories.clone(),
        package_count: state.package_count,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    duration.as_millis().min(u64::MAX as u128) as u64
}

fn resolve_build_dependencies(
    repo_state: &RepositoryState,
    spec: &SpecInfo,
    arch: &str,
) -> Result<DependencyResolution> {
    let solved = solve_build_requires(repo_state, spec, arch)?;
    Ok(DependencyResolution {
        backend: solved.backend,
        fingerprint: solved.fingerprint,
        cache_hit: solved.cache_hit,
        selected: solved
            .selected
            .into_iter()
            .map(|package| DependencyPackage {
                name: package.name,
                nevra: package.nevra,
                repo: package.repo,
            })
            .collect(),
        unresolved: solved.unresolved,
        problems: solved.problems,
    })
}

fn download_packages(
    dependencies: &DependencyResolution,
    repo_state: &RepositoryState,
) -> Result<Vec<DownloadedPackage>> {
    let mut selected_lookup = dependencies
        .selected
        .iter()
        .map(|item| item.nevra.clone())
        .collect::<HashSet<_>>();
    let download_root = cache_root()?.join("v1").join("rpms");
    ensure_dir(&download_root)?;

    let mut downloaded = Vec::new();
    for package in &repo_state.packages {
        if !selected_lookup.contains(&package.nevra()) {
            continue;
        }
        let (path, reused) = materialize_package(package, &download_root)?;
        downloaded.push(DownloadedPackage {
            name: package.name.clone(),
            nevra: package.nevra(),
            path: path_to_string(&path),
            reused,
        });
        selected_lookup.remove(&package.nevra());
    }
    downloaded.sort_by(|left, right| left.nevra.cmp(&right.nevra));
    Ok(downloaded)
}

fn materialize_package(package: &PackageRecord, cache_root: &Path) -> Result<(PathBuf, bool)> {
    let full_location = if let Ok(url) = Url::parse(&package.repo_location_raw) {
        url.join(&package.location)
            .map(|value| value.to_string())
            .map_err(|err| RgbsError::message(err.to_string()))?
    } else {
        path_to_string(&PathBuf::from(&package.repo_location_raw).join(&package.location))
    };

    if Url::parse(&package.repo_location_raw).is_err() {
        let path = PathBuf::from(&package.repo_location_raw).join(&package.location);
        if !path.exists() {
            return Err(RgbsError::message(format!(
                "package file does not exist: {}",
                path.display()
            )));
        }
        return Ok((path, true));
    }

    let checksum = package
        .checksum
        .clone()
        .unwrap_or_else(|| sha256_hex(full_location.as_bytes()));
    let file_name = Path::new(&package.location)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("package.rpm");
    let target = cache_root.join(format!("{checksum}-{file_name}"));
    if target.exists() {
        return Ok((target, true));
    }
    let bytes = fetch_remote(&full_location)?;
    atomic_write(&target, &bytes)?;
    Ok((target, false))
}

fn fetch_remote(url: &str) -> Result<Vec<u8>> {
    let mut parsed = Url::parse(url).map_err(|err| RgbsError::message(err.to_string()))?;
    let username = if parsed.username().is_empty() {
        None
    } else {
        Some(parsed.username().to_string())
    };
    let password = parsed.password().map(ToOwned::to_owned);
    if username.is_some() {
        parsed
            .set_username("")
            .map_err(|_| RgbsError::message("failed to clear username"))?;
        parsed
            .set_password(None)
            .map_err(|_| RgbsError::message("failed to clear password"))?;
    }
    let agent = ureq::AgentBuilder::new().build();
    let mut request = agent.get(parsed.as_str());
    if let Some(user) = username {
        let header =
            BASE64_STANDARD.encode(format!("{user}:{}", password.as_deref().unwrap_or("")));
        request = request.set("Authorization", &format!("Basic {header}"));
    }
    let response = request
        .call()
        .map_err(|err| RgbsError::message(err.to_string()))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| RgbsError::message(err.to_string()))?;
    Ok(bytes)
}

fn prepare_buildroot(
    plan: &ResolvedBuildPlan,
    repo_state: &RepositoryState,
    dependencies: &DependencyResolution,
    downloads: &[DownloadedPackage],
    buildconf: Option<&Path>,
) -> Result<BuildRootSummary> {
    let base = PathBuf::from(&plan.buildroot);
    let strategy = if plan.keep_packs {
        "shared_keep_packs"
    } else {
        "exact_match"
    };
    let buildconf_fingerprint = optional_file_fingerprint(buildconf)?;
    let defines_fingerprint = defines_fingerprint(&plan.defines)?;
    let context_fingerprint = buildroot_context_fingerprint(
        plan,
        &repo_state.fingerprint,
        buildconf_fingerprint.as_deref(),
        &defines_fingerprint,
    )?;
    let root_fingerprint = if plan.keep_packs {
        context_fingerprint.clone()
    } else {
        sha256_hex(format!(
            "{context_fingerprint}\0{}",
            dependencies.fingerprint
        ))
    };
    let root = if plan.keep_packs {
        base.join("shared")
            .join(buildroot_profile_key(plan))
            .join(&plan.arch)
            .join(&context_fingerprint)
    } else {
        base.join("roots").join(&plan.arch).join(&root_fingerprint)
    };

    let mut root_recreated = false;
    let root_existed = root.exists();
    if root_existed && plan.clean {
        fs::remove_dir_all(&root).map_err(|err| RgbsError::io(&root, err))?;
        root_recreated = true;
    }
    if !root.exists() {
        initialize_buildroot(&root)?;
        root_recreated = true;
    }

    let expected_packages = dependencies
        .selected
        .iter()
        .map(|package| package.nevra.clone())
        .collect::<HashSet<_>>();
    let mut installed_packages = if root_recreated {
        HashSet::new()
    } else {
        load_persisted_buildroot(&root)
            .map(|state| state.installed_packages.into_iter().collect::<HashSet<_>>())
            .or_else(|_| query_installed_nevras(&root))?
    };

    let downloads_to_install = if plan.keep_packs {
        downloads
            .iter()
            .filter(|package| !installed_packages.contains(&package.nevra))
            .cloned()
            .collect::<Vec<_>>()
    } else if !expected_packages.is_subset(&installed_packages) {
        if root.exists() {
            fs::remove_dir_all(&root).map_err(|err| RgbsError::io(&root, err))?;
        }
        initialize_buildroot(&root)?;
        root_recreated = true;
        installed_packages.clear();
        downloads.to_vec()
    } else {
        Vec::new()
    };

    if !downloads_to_install.is_empty() {
        install_packages_into_buildroot(&root, &downloads_to_install)?;
    }

    installed_packages = query_installed_nevras(&root)?;
    let mut installed_list = installed_packages.into_iter().collect::<Vec<_>>();
    installed_list.sort();
    let package_fingerprint = sha256_hex(installed_list.join("\n"));
    let persisted = PersistedBuildRoot {
        schema: "buildroot-v1".to_string(),
        arch: plan.arch.clone(),
        profile: buildroot_profile_key(plan),
        strategy: strategy.to_string(),
        root_path: path_to_string(&root),
        fingerprint: root_fingerprint.clone(),
        package_fingerprint: package_fingerprint.clone(),
        dependency_fingerprint: Some(dependencies.fingerprint.clone()),
        repo_fingerprint: Some(repo_state.fingerprint.clone()),
        buildconf_path: buildconf.map(path_to_string),
        buildconf_fingerprint,
        defines_fingerprint,
        installed_packages: installed_list.clone(),
    };
    save_persisted_buildroot(&root, &persisted)?;
    save_active_buildroot(plan, &persisted)?;

    Ok(BuildRootSummary {
        path: path_to_string(&root),
        fingerprint: root_fingerprint,
        strategy: strategy.to_string(),
        package_fingerprint,
        reused: root_existed && !root_recreated,
        installed_packages: installed_list.len(),
    })
}

fn buildroot_profile_key(plan: &ResolvedBuildPlan) -> String {
    plan.profile
        .name
        .trim_start_matches("profile.")
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn buildroot_state_dir(plan: &ResolvedBuildPlan) -> PathBuf {
    PathBuf::from(&plan.buildroot)
        .join("state")
        .join(buildroot_profile_key(plan))
        .join(&plan.arch)
}

fn active_buildroot_state_path(plan: &ResolvedBuildPlan) -> PathBuf {
    buildroot_state_dir(plan).join("active-root.json")
}

fn buildroot_manifest_path(root: &Path) -> PathBuf {
    root.join(".rgbs-buildroot.json")
}

fn defines_fingerprint(defines: &[String]) -> Result<String> {
    let bytes = serde_json::to_vec(defines)
        .map_err(|err| RgbsError::message(format!("serialize defines fingerprint: {err}")))?;
    Ok(sha256_hex(bytes))
}

fn optional_file_fingerprint(path: Option<&Path>) -> Result<Option<String>> {
    path.map(|path| {
        fs::read(path)
            .map(|bytes| sha256_hex(bytes))
            .map_err(|err| RgbsError::io(path, err))
    })
    .transpose()
}

fn buildroot_context_fingerprint(
    plan: &ResolvedBuildPlan,
    repo_fingerprint: &str,
    buildconf_fingerprint: Option<&str>,
    defines_fingerprint: &str,
) -> Result<String> {
    let key = serde_json::json!({
        "schema": "buildroot-context-v2",
        "arch": plan.arch,
        "profile": buildroot_profile_key(plan),
        "repo_fingerprint": repo_fingerprint,
        "buildconf_fingerprint": buildconf_fingerprint,
        "defines_fingerprint": defines_fingerprint,
    });
    let bytes = serde_json::to_vec(&key)
        .map_err(|err| RgbsError::message(format!("serialize buildroot key: {err}")))?;
    Ok(sha256_hex(bytes))
}

fn load_active_buildroot(plan: &ResolvedBuildPlan) -> Result<PersistedBuildRoot> {
    let path = active_buildroot_state_path(plan);
    let bytes = fs::read(&path).map_err(|err| {
        RgbsError::message(format!(
            "--noinit requires a prepared buildroot; no cached state found at {} ({err})",
            path.display()
        ))
    })?;
    let state = serde_json::from_slice::<PersistedBuildRoot>(&bytes)
        .map_err(|err| RgbsError::message(format!("invalid cached buildroot state: {err}")))?;
    let root = PathBuf::from(&state.root_path);
    if !root.exists() {
        return Err(RgbsError::message(format!(
            "cached buildroot no longer exists: {}",
            root.display()
        )));
    }
    if let Some(path) = state.buildconf_path.as_ref().map(PathBuf::from) {
        if !path.exists() {
            return Err(RgbsError::message(format!(
                "cached buildconf for --noinit no longer exists: {}",
                path.display()
            )));
        }
    }
    Ok(state)
}

fn buildroot_summary_from_state(state: &PersistedBuildRoot) -> Result<BuildRootSummary> {
    let root = PathBuf::from(&state.root_path);
    if !root.exists() {
        return Err(RgbsError::message(format!(
            "cached buildroot no longer exists: {}",
            root.display()
        )));
    }
    Ok(BuildRootSummary {
        path: state.root_path.clone(),
        fingerprint: state.fingerprint.clone(),
        strategy: state.strategy.clone(),
        package_fingerprint: state.package_fingerprint.clone(),
        reused: true,
        installed_packages: state.installed_packages.len(),
    })
}

fn load_persisted_buildroot(root: &Path) -> Result<PersistedBuildRoot> {
    let path = buildroot_manifest_path(root);
    let bytes = fs::read(&path).map_err(|err| RgbsError::io(&path, err))?;
    serde_json::from_slice::<PersistedBuildRoot>(&bytes)
        .map_err(|err| RgbsError::message(format!("invalid buildroot manifest: {err}")))
}

fn save_persisted_buildroot(root: &Path, state: &PersistedBuildRoot) -> Result<()> {
    let path = buildroot_manifest_path(root);
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|err| RgbsError::message(format!("serialize buildroot manifest: {err}")))?;
    atomic_write(&path, &bytes)
}

fn save_active_buildroot(plan: &ResolvedBuildPlan, state: &PersistedBuildRoot) -> Result<()> {
    let path = active_buildroot_state_path(plan);
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|err| RgbsError::message(format!("serialize active buildroot state: {err}")))?;
    atomic_write(&path, &bytes)
}

fn initialize_buildroot(root: &Path) -> Result<()> {
    ensure_dir(root)?;
    ensure_dir(&root.join("var/lib/rpm"))?;

    let mut initdb = Command::new("rpm");
    initdb.arg("--root").arg(root).arg("--initdb");
    run_command(&mut initdb).map(|_| ())
}

fn install_packages_into_buildroot(root: &Path, packages: &[DownloadedPackage]) -> Result<()> {
    if packages.is_empty() {
        return Ok(());
    }
    progress(
        "Installing",
        format!(
            "installing {} packages into buildroot {}",
            packages.len(),
            root.display()
        ),
    );

    let existing_packages = query_installed_nevras(root)?;
    if let Some(backend) = select_buildroot_install_backend()? {
        log_debug_line(format!(
            "using buildroot install backend {} for {}",
            backend.label(),
            root.display()
        ));
        match install_packages_via_rpm_transaction(root, packages, backend) {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !existing_packages.is_empty() {
                    return Err(RgbsError::message(format!(
                        "compat buildroot backend {} failed for {} and cannot safely fall back on a reused root: {}",
                        backend.label(),
                        root.display(),
                        err
                    )));
                }
                print_warning(format!(
                    "compat buildroot backend {} failed, falling back to staged extractor: {}",
                    backend.label(),
                    err
                ));
                log_debug_line(format!(
                    "compat buildroot backend {} failed for {}: {}",
                    backend.label(),
                    root.display(),
                    err
                ));
                reset_buildroot_for_extractor_fallback(root)?;
            }
        }
    }

    install_packages_via_staged_extractor(root, packages)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildrootInstallBackend {
    RpmCompat,
}

impl BuildrootInstallBackend {
    fn label(self) -> &'static str {
        match self {
            Self::RpmCompat => "rpm-compat",
        }
    }
}

fn select_buildroot_install_backend() -> Result<Option<BuildrootInstallBackend>> {
    if command_in_path("fakeroot") {
        return Ok(Some(BuildrootInstallBackend::RpmCompat));
    }
    if running_as_root()? {
        return Ok(Some(BuildrootInstallBackend::RpmCompat));
    }
    Ok(None)
}

fn install_packages_via_rpm_transaction(
    root: &Path,
    packages: &[DownloadedPackage],
    backend: BuildrootInstallBackend,
) -> Result<()> {
    match backend {
        BuildrootInstallBackend::RpmCompat => {
            install_packages_with_compat_rpm_transaction(root, packages)
        }
    }
}

fn install_packages_with_compat_rpm_transaction(
    root: &Path,
    packages: &[DownloadedPackage],
) -> Result<()> {
    let use_fakeroot = command_in_path("fakeroot");
    let mut install = if use_fakeroot {
        let mut command = Command::new("fakeroot");
        command.arg("--").arg("rpm");
        command
    } else {
        Command::new("rpm")
    };
    install
        .arg("--root")
        .arg(root)
        .arg("--dbpath")
        .arg("/var/lib/rpm")
        .arg("-Uvh")
        .arg("--nodeps")
        .arg("--nosignature")
        .arg("--notriggers")
        .arg("--noscripts")
        .arg("--ignorearch");
    for package in packages {
        install.arg(&package.path);
    }
    run_command(&mut install).map(|_| ()).map_err(|err| {
        RgbsError::message(format!(
            "failed to install {} packages through the compat rpm transaction backend for {}: {}",
            packages.len(),
            root.display(),
            err
        ))
    })
}

fn reset_buildroot_for_extractor_fallback(root: &Path) -> Result<()> {
    if root.exists() {
        fs::remove_dir_all(root).map_err(|err| RgbsError::io(root, err))?;
    }
    initialize_buildroot(root)
}

fn install_packages_via_staged_extractor(
    root: &Path,
    packages: &[DownloadedPackage],
) -> Result<()> {
    if !command_in_path("rpm2archive") {
        return Err(RgbsError::message(
            "buildroot package install fallback requires `rpm2archive`; run `rgbs doctor` and `rgbs fix` on Ubuntu to install it",
        ));
    }
    if !command_in_path("tar") {
        return Err(RgbsError::message(
            "buildroot package install fallback requires `tar`; run `rgbs doctor` and `rgbs fix` on Ubuntu to install it",
        ));
    }

    register_packages_in_rpmdb(root, packages)?;
    relax_buildroot_directory_permissions(root)?;
    for package in packages {
        extract_package_payload(root, package).map_err(|err| {
            RgbsError::message(format!(
                "buildroot package install fallback failed for {} while extracting {} ({}): {}",
                root.display(),
                package.nevra,
                package.path,
                err
            ))
        })?;
    }
    Ok(())
}

fn running_as_root() -> Result<bool> {
    if matches!(env::var("EUID").as_deref(), Ok("0"))
        || matches!(env::var("UID").as_deref(), Ok("0"))
    {
        return Ok(true);
    }

    let output = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|err| RgbsError::command("id -u", err.to_string()))?;
    if !output.status.success() {
        return Err(RgbsError::command(
            "id -u",
            format!("exit status {}", output.status),
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim() == "0")
}

fn register_packages_in_rpmdb(root: &Path, packages: &[DownloadedPackage]) -> Result<()> {
    let mut install = Command::new("rpm");
    install
        .arg("--root")
        .arg(root)
        .arg("--dbpath")
        .arg("/var/lib/rpm")
        .arg("-ivh")
        .arg("--justdb")
        .arg("--nodeps")
        .arg("--nosignature")
        .arg("--notriggers")
        .arg("--noscripts")
        .arg("--ignorearch");
    for package in packages {
        install.arg(&package.path);
    }
    run_command(&mut install).map(|_| ()).map_err(|err| {
        RgbsError::message(format!(
            "failed to register {} packages in rpmdb for {}: {}",
            packages.len(),
            root.display(),
            err
        ))
    })
}

fn extract_package_payload(root: &Path, package: &DownloadedPackage) -> Result<()> {
    log_debug_line(format!(
        "extracting package payload for {} from {}",
        package.nevra, package.path
    ));

    let extract_root = root.join(".rgbs-extract");
    ensure_dir(&extract_root)?;
    let package_extract_dir =
        extract_root.join(sha256_hex(format!("{}\0{}", package.nevra, package.path)));
    if package_extract_dir.exists() {
        fs::remove_dir_all(&package_extract_dir)
            .map_err(|err| RgbsError::io(&package_extract_dir, err))?;
    }
    ensure_dir(&package_extract_dir)?;
    log_debug_line(format!(
        "staging rpm archive conversion in {}",
        package_extract_dir.display()
    ));
    let archive_path = package_extract_dir.join("payload.tgz");
    let payload_root = package_extract_dir.join("payload");
    let archive_file =
        fs::File::create(&archive_path).map_err(|err| RgbsError::io(&archive_path, err))?;

    let mut rpm2archive = Command::new("rpm2archive");
    rpm2archive
        .current_dir(&package_extract_dir)
        .arg(&package.path)
        .stdout(Stdio::from(archive_file))
        .stderr(Stdio::piped());
    let rendered_rpm2archive = format!(
        "(cd {} && {} > {})",
        package_extract_dir.display(),
        render_command(&rpm2archive),
        archive_path.display()
    );
    log_debug_line(format!("run: {rendered_rpm2archive}"));
    let rpm2archive_output = rpm2archive
        .output()
        .map_err(|err| RgbsError::command(&rendered_rpm2archive, err.to_string()))?;
    log_debug_line(format!(
        "exit: {rendered_rpm2archive} -> {}",
        rpm2archive_output.status
    ));
    let rpm2archive_stdout_text = String::from_utf8_lossy(&rpm2archive_output.stdout);
    let rpm2archive_stderr_text = String::from_utf8_lossy(&rpm2archive_output.stderr);
    if !rpm2archive_stdout_text.trim().is_empty() {
        log_debug_line(format!(
            "stdout for `{rendered_rpm2archive}`:\n{}",
            rpm2archive_stdout_text.trim_end()
        ));
    }
    if !rpm2archive_stderr_text.trim().is_empty() {
        log_debug_line(format!(
            "stderr for `{rendered_rpm2archive}`:\n{}",
            rpm2archive_stderr_text.trim_end()
        ));
    }
    if !rpm2archive_output.status.success() {
        return Err(RgbsError::command(
            rendered_rpm2archive,
            if rpm2archive_stderr_text.trim().is_empty() {
                format!("exit status {}", rpm2archive_output.status)
            } else {
                rpm2archive_stderr_text.trim().to_string()
            },
        ));
    }
    if !archive_path.is_file() {
        return Err(RgbsError::message(format!(
            "rpm2archive did not produce {}",
            archive_path.display()
        )));
    }
    log_debug_line(format!(
        "converted {} into archive {}",
        package.path,
        archive_path.display()
    ));
    ensure_dir(&payload_root)?;

    let mut tar = Command::new("tar");
    tar.arg("-xf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&payload_root)
        .arg("--delay-directory-restore")
        .arg("--no-same-owner")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let rendered_tar = render_command(&tar);
    log_debug_line(format!("run: {rendered_tar}"));
    let tar_output = tar
        .output()
        .map_err(|err| RgbsError::command(&rendered_tar, err.to_string()))?;

    log_debug_line(format!("exit: {rendered_tar} -> {}", tar_output.status));
    let tar_stdout_text = String::from_utf8_lossy(&tar_output.stdout);
    let tar_stderr_text = String::from_utf8_lossy(&tar_output.stderr);
    if !tar_stdout_text.trim().is_empty() {
        log_debug_line(format!(
            "stdout for `{rendered_tar}`:\n{}",
            tar_stdout_text.trim_end()
        ));
    }
    if !tar_stderr_text.trim().is_empty() {
        log_debug_line(format!(
            "stderr for `{rendered_tar}`:\n{}",
            tar_stderr_text.trim_end()
        ));
    }
    if !tar_output.status.success() {
        return Err(RgbsError::command(
            rendered_tar,
            if tar_stderr_text.trim().is_empty() {
                format!("exit status {}", tar_output.status)
            } else {
                tar_stderr_text.trim().to_string()
            },
        ));
    }
    merge_payload_tree(&payload_root, root)?;
    relax_buildroot_directory_permissions(root)?;

    fs::remove_dir_all(&package_extract_dir)
        .map_err(|err| RgbsError::io(&package_extract_dir, err))?;

    Ok(())
}

fn merge_payload_tree(staging_root: &Path, root: &Path) -> Result<()> {
    for entry in fs::read_dir(staging_root).map_err(|err| RgbsError::io(staging_root, err))? {
        let entry = entry.map_err(|err| RgbsError::io(staging_root, err))?;
        merge_payload_entry(&entry.path(), &root.join(entry.file_name()))?;
    }
    Ok(())
}

fn merge_payload_entry(src: &Path, dst: &Path) -> Result<()> {
    let src_metadata = fs::symlink_metadata(src).map_err(|err| RgbsError::io(src, err))?;
    if src_metadata.is_dir() {
        merge_payload_directory(src, dst, &src_metadata)
    } else {
        replace_payload_leaf(src, dst)
    }
}

fn merge_payload_directory(src: &Path, dst: &Path, src_metadata: &fs::Metadata) -> Result<()> {
    match fs::symlink_metadata(dst) {
        Ok(dst_metadata) => {
            if dst_metadata.is_dir() {
                for entry in fs::read_dir(src).map_err(|err| RgbsError::io(src, err))? {
                    let entry = entry.map_err(|err| RgbsError::io(src, err))?;
                    merge_payload_entry(&entry.path(), &dst.join(entry.file_name()))?;
                }
                fs::set_permissions(dst, src_metadata.permissions())
                    .map_err(|err| RgbsError::io(dst, err))?;
                fs::remove_dir_all(src).map_err(|err| RgbsError::io(src, err))?;
                Ok(())
            } else if dst_metadata.file_type().is_symlink() {
                match fs::metadata(dst) {
                    Ok(target_metadata) if target_metadata.is_dir() => {
                        log_debug_line(format!(
                            "merging staged directory {} into symlinked buildroot directory {}",
                            src.display(),
                            dst.display()
                        ));
                        for entry in fs::read_dir(src).map_err(|err| RgbsError::io(src, err))? {
                            let entry = entry.map_err(|err| RgbsError::io(src, err))?;
                            merge_payload_entry(&entry.path(), &dst.join(entry.file_name()))?;
                        }
                        fs::remove_dir_all(src).map_err(|err| RgbsError::io(src, err))?;
                        Ok(())
                    }
                    _ => {
                        remove_existing_payload_path(dst)?;
                        ensure_parent_dir(dst)?;
                        fs::rename(src, dst).map_err(|err| RgbsError::io(dst, err))
                    }
                }
            } else {
                remove_existing_payload_path(dst)?;
                ensure_parent_dir(dst)?;
                fs::rename(src, dst).map_err(|err| RgbsError::io(dst, err))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            ensure_parent_dir(dst)?;
            fs::rename(src, dst).map_err(|err| RgbsError::io(dst, err))
        }
        Err(err) => Err(RgbsError::io(dst, err)),
    }
}

fn replace_payload_leaf(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() || fs::symlink_metadata(dst).is_ok() {
        remove_existing_payload_path(dst)?;
    }
    ensure_parent_dir(dst)?;
    fs::rename(src, dst).map_err(|err| RgbsError::io(dst, err))
}

fn remove_existing_payload_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|err| RgbsError::io(path, err))?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path).map_err(|err| RgbsError::io(path, err))
    } else {
        fs::remove_file(path).map_err(|err| RgbsError::io(path, err))
    }
}

fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            ensure_dir(parent)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn relax_buildroot_directory_permissions(root: &Path) -> Result<()> {
    let mut adjusted = 0usize;
    for entry in WalkDir::new(root) {
        let entry = entry.map_err(|err| RgbsError::message(err.to_string()))?;
        if !entry.file_type().is_dir() {
            continue;
        }
        let path = entry.path();
        let metadata = fs::metadata(path).map_err(|err| RgbsError::io(path, err))?;
        let mode = metadata.permissions().mode();
        if mode & 0o200 != 0 {
            continue;
        }
        let mut permissions = metadata.permissions();
        permissions.set_mode(mode | 0o200);
        fs::set_permissions(path, permissions).map_err(|err| RgbsError::io(path, err))?;
        adjusted += 1;
    }
    if adjusted > 0 {
        log_debug_line(format!(
            "relaxed owner-write permission on {adjusted} buildroot directories under {}",
            root.display()
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn relax_buildroot_directory_permissions(_root: &Path) -> Result<()> {
    Ok(())
}

fn query_installed_nevras(root: &Path) -> Result<HashSet<String>> {
    let rpmdb = root.join("var/lib/rpm");
    if !rpmdb.exists() {
        return Ok(HashSet::new());
    }

    let mut query = Command::new("rpm");
    query
        .arg("--root")
        .arg(root)
        .arg("--dbpath")
        .arg("/var/lib/rpm")
        .arg("-qa")
        .arg("--qf")
        .arg("%{NAME}-%{EPOCHNUM}:%{VERSION}-%{RELEASE}.%{ARCH}\n");
    let output = run_command(&mut query)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn stage_sources(plan: &ResolvedBuildPlan, spec: &SpecInfo) -> Result<StageExecution> {
    let snapshot = inspect_source_snapshot(plan, spec)?;
    let stage_key = sha256_hex(
        format!(
            "{}:{}:{}:{}:{}",
            snapshot.fingerprint, spec.spec_path, spec.name, spec.version, plan.arch
        )
        .as_bytes(),
    );
    let topdir = PathBuf::from(&plan.work_dir).join(".rgbs").join(format!(
        "{}-{}-{}",
        spec.name,
        plan.arch,
        &stage_key[..12]
    ));
    let reused = topdir.exists() && !plan.clean && !plan.overwrite;
    if topdir.exists() && (plan.clean || plan.overwrite) {
        fs::remove_dir_all(&topdir).map_err(|err| RgbsError::io(&topdir, err))?;
    }
    ensure_dir(&topdir)?;
    for dir in [
        "BUILD",
        "BUILDROOT",
        "RPMS",
        "SOURCES",
        "SPECS",
        "SRPMS",
        "logs",
        "tmp",
    ] {
        ensure_dir(&topdir.join(dir))?;
    }

    let staging_root = topdir.join("tmp").join("export-tree");
    if !reused {
        if staging_root.exists() {
            fs::remove_dir_all(&staging_root).map_err(|err| RgbsError::io(&staging_root, err))?;
        }
        materialize_source_snapshot(&snapshot, &staging_root)?;
        let staged_packaging_dir = staging_root.join(&snapshot.packaging_rel);
        let staged_spec_path = staging_root.join(&snapshot.spec_rel);
        copy_packaging_tree(&staged_packaging_dir, &staged_spec_path, &topdir)?;
    }
    let generated_sources =
        ensure_source_inputs(plan, spec, &topdir, &staging_root, &snapshot.packaging_rel)?;
    let spec_path = PathBuf::from(&spec.spec_path);

    Ok(StageExecution {
        summary: StageSummary {
            topdir: path_to_string(&topdir),
            spec_path: path_to_string(
                &topdir
                    .join("SPECS")
                    .join(spec_path.file_name().unwrap_or_default()),
            ),
            export_mode: snapshot.export_mode,
            source_fingerprint: snapshot.fingerprint,
            reused,
            generated_sources,
        },
        warnings: snapshot.warnings,
    })
}

fn copy_packaging_tree(packaging_dir: &Path, main_spec: &Path, topdir: &Path) -> Result<()> {
    for entry in WalkDir::new(packaging_dir).min_depth(1) {
        let entry = entry.map_err(|err| RgbsError::message(err.to_string()))?;
        if entry.file_type().is_dir() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(packaging_dir)
            .map_err(|err| RgbsError::message(err.to_string()))?;
        let target_root = if entry.path() == main_spec
            || entry.path().extension().and_then(|value| value.to_str()) == Some("spec")
        {
            topdir.join("SPECS")
        } else {
            topdir.join("SOURCES")
        };
        let target = target_root.join(relative);
        if let Some(parent) = target.parent() {
            ensure_dir(parent)?;
        }
        fs::copy(entry.path(), &target).map_err(|err| RgbsError::io(&target, err))?;
    }
    Ok(())
}

fn ensure_source_inputs(
    _plan: &ResolvedBuildPlan,
    spec: &SpecInfo,
    topdir: &Path,
    source_root: &Path,
    packaging_rel: &Path,
) -> Result<Vec<String>> {
    let mut generated = Vec::new();
    let sources_dir = topdir.join("SOURCES");
    let packaging_dir = source_root.join(packaging_rel);
    let export_git_root = source_root;
    for source in &spec.sources {
        if source.contains("://") {
            continue;
        }
        let source_name = Path::new(source)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(source);
        let staged = sources_dir.join(source_name);
        if staged.exists() {
            continue;
        }
        let packaging_copy = packaging_dir.join(source_name);
        if packaging_copy.exists() {
            fs::copy(&packaging_copy, &staged).map_err(|err| RgbsError::io(&staged, err))?;
            continue;
        }
        let root_copy = export_git_root.join(source_name);
        if root_copy.exists() {
            fs::copy(&root_copy, &staged).map_err(|err| RgbsError::io(&staged, err))?;
            continue;
        }
        if is_archive_name(source_name) {
            generate_source_archive(
                export_git_root,
                &packaging_dir,
                &staged,
                &spec.name,
                &spec.version,
            )?;
            generated.push(path_to_string(&staged));
        }
    }
    Ok(generated)
}

fn is_archive_name(name: &str) -> bool {
    [".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".tar"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

fn generate_source_archive(
    git_dir: &Path,
    packaging_dir: &Path,
    target: &Path,
    name: &str,
    version: &str,
) -> Result<()> {
    let export_root = target
        .parent()
        .unwrap_or(git_dir)
        .join(".rgbs-export")
        .join(format!("{name}-{version}"));
    if export_root.exists() {
        fs::remove_dir_all(&export_root).map_err(|err| RgbsError::io(&export_root, err))?;
    }
    ensure_dir(&export_root)?;
    for entry in WalkDir::new(git_dir).min_depth(1) {
        let entry = entry.map_err(|err| RgbsError::message(err.to_string()))?;
        let path = entry.path();
        if path.starts_with(packaging_dir) || path.starts_with(git_dir.join(".git")) {
            continue;
        }
        let relative = path
            .strip_prefix(git_dir)
            .map_err(|err| RgbsError::message(err.to_string()))?;
        let destination = export_root.join(relative);
        if entry.file_type().is_dir() {
            ensure_dir(&destination)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = destination.parent() {
                ensure_dir(parent)?;
            }
            fs::copy(path, &destination).map_err(|err| RgbsError::io(&destination, err))?;
        }
    }

    let parent = export_root
        .parent()
        .ok_or_else(|| RgbsError::message("export root has no parent"))?;
    let target_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| RgbsError::message("archive target has no file name"))?;
    let mut tar = Command::new("tar");
    tar.arg("-C").arg(parent);
    match target_name {
        name if name.ends_with(".tar.gz") || name.ends_with(".tgz") => {
            tar.arg("-czf");
        }
        name if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") => {
            tar.arg("-cjf");
        }
        name if name.ends_with(".tar.xz") => {
            tar.arg("-cJf");
        }
        _ => {
            tar.arg("-cf");
        }
    }
    tar.arg(target).arg(format!("{name}-{version}"));
    let output = tar
        .output()
        .map_err(|err| RgbsError::message(err.to_string()))?;
    if !output.status.success() {
        return Err(RgbsError::message(format!(
            "failed to generate source archive: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn inspect_source_snapshot(plan: &ResolvedBuildPlan, spec: &SpecInfo) -> Result<SourceSnapshot> {
    let git_dir = PathBuf::from(&plan.git_dir);
    let packaging_dir = PathBuf::from(&spec.packaging_dir);
    let spec_path = PathBuf::from(&spec.spec_path);
    let git_root = detect_git_root(&git_dir).unwrap_or_else(|| git_dir.clone());
    let packaging_rel = relative_under(&git_root, &packaging_dir)?;
    let spec_rel = relative_under(&git_root, &spec_path)?;

    if let Some(head) = git_head(&git_root)? {
        let modified = git_paths(&git_root, &["diff", "--name-only", "-z", "HEAD", "--"])?;
        let untracked = git_paths(
            &git_root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let warnings = if plan.include_all {
            Vec::new()
        } else {
            excluded_worktree_warnings(&modified, &untracked)
        };
        let fingerprint = if plan.include_all {
            working_tree_fingerprint(&git_root, Some(head.as_str()), &modified, &untracked)?
        } else {
            sha256_hex(format!("git-archive\0{head}"))
        };
        let export_mode = if plan.include_all {
            "git_working_tree".to_string()
        } else {
            "git_archive".to_string()
        };
        return Ok(SourceSnapshot {
            root: git_root,
            packaging_rel,
            spec_rel,
            export_mode,
            fingerprint,
            warnings,
        });
    }

    let warnings = if plan.include_all {
        Vec::new()
    } else {
        vec!["git HEAD is unavailable; falling back to working tree snapshot".to_string()]
    };
    Ok(SourceSnapshot {
        root: git_root.clone(),
        packaging_rel,
        spec_rel,
        export_mode: "filesystem_working_tree".to_string(),
        fingerprint: hash_tree(&git_root)?,
        warnings,
    })
}

fn materialize_source_snapshot(snapshot: &SourceSnapshot, target: &Path) -> Result<()> {
    ensure_dir(target)?;
    match snapshot.export_mode.as_str() {
        "git_archive" => export_git_archive(&snapshot.root, target),
        "git_working_tree" => export_git_working_tree(&snapshot.root, target),
        _ => copy_tree_snapshot(&snapshot.root, target),
    }
}

fn export_git_archive(git_root: &Path, target: &Path) -> Result<()> {
    let archive = target
        .parent()
        .ok_or_else(|| RgbsError::message("export target has no parent"))?
        .join("snapshot.tar");
    let mut git = Command::new("git");
    git.arg("-C")
        .arg(git_root)
        .arg("archive")
        .arg("--format=tar")
        .arg("--output")
        .arg(&archive)
        .arg("HEAD");
    run_command(&mut git)?;

    let mut tar = Command::new("tar");
    tar.arg("-xf").arg(&archive).arg("-C").arg(target);
    run_command(&mut tar)?;
    if archive.exists() {
        fs::remove_file(&archive).map_err(|err| RgbsError::io(&archive, err))?;
    }
    Ok(())
}

fn export_git_working_tree(git_root: &Path, target: &Path) -> Result<()> {
    let paths = git_paths(
        git_root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    copy_selected_paths(git_root, target, &paths)
}

fn copy_tree_snapshot(root: &Path, target: &Path) -> Result<()> {
    let paths = WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| !entry.path().starts_with(root.join(".git")))
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .map_err(|err| RgbsError::message(err.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    copy_selected_paths(root, target, &paths)
}

fn copy_selected_paths(root: &Path, target: &Path, paths: &[PathBuf]) -> Result<()> {
    for relative in paths {
        let source = root.join(relative);
        if !source.exists() {
            continue;
        }
        let destination = target.join(relative);
        if let Some(parent) = destination.parent() {
            ensure_dir(parent)?;
        }
        fs::copy(&source, &destination).map_err(|err| RgbsError::io(&destination, err))?;
    }
    Ok(())
}

fn git_head(git_root: &Path) -> Result<Option<String>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(git_root)
        .arg("rev-parse")
        .arg("--verify")
        .arg("HEAD");
    match run_command(&mut command) {
        Ok(output) => Ok(Some(
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
        )),
        Err(RgbsError::Command { .. }) => Ok(None),
        Err(err) => Err(err),
    }
}

fn git_paths(git_root: &Path, args: &[&str]) -> Result<Vec<PathBuf>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(git_root).args(args);
    let output = run_command(&mut command)?;
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| PathBuf::from(String::from_utf8_lossy(chunk).into_owned()))
        .collect())
}

fn excluded_worktree_warnings(modified: &[PathBuf], untracked: &[PathBuf]) -> Vec<String> {
    let mut warnings = Vec::new();
    if !untracked.is_empty() {
        warnings.push(format!(
            "the following untracked files would NOT be included: {}",
            render_paths(untracked)
        ));
    }
    if !modified.is_empty() {
        warnings.push(format!(
            "the following uncommitted changes would NOT be included: {}",
            render_paths(modified)
        ));
    }
    if !modified.is_empty() || !untracked.is_empty() {
        warnings.push(
            "you can specify '--include-all' to include these uncommitted and untracked files"
                .to_string(),
        );
    }
    warnings
}

fn render_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn working_tree_fingerprint(
    git_root: &Path,
    head: Option<&str>,
    modified: &[PathBuf],
    untracked: &[PathBuf],
) -> Result<String> {
    let mut parts = vec![head.unwrap_or("working-tree").to_string()];
    let mut changed = modified
        .iter()
        .chain(untracked.iter())
        .cloned()
        .collect::<Vec<_>>();
    changed.sort();
    for path in &changed {
        let full_path = git_root.join(path);
        parts.push(path.to_string_lossy().into_owned());
        if full_path.exists() {
            let bytes = fs::read(&full_path).map_err(|err| RgbsError::io(&full_path, err))?;
            parts.push(sha256_hex(bytes));
        } else {
            parts.push("deleted".to_string());
        }
    }
    Ok(sha256_hex(parts.join("\0")))
}

fn hash_tree(root: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for entry in WalkDir::new(root).min_depth(1) {
        let entry = entry.map_err(|err| RgbsError::message(err.to_string()))?;
        if entry.path().starts_with(root.join(".git")) || !entry.file_type().is_file() {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|err| RgbsError::message(err.to_string()))?;
        parts.push(relative.to_string_lossy().into_owned());
        let bytes = fs::read(entry.path()).map_err(|err| RgbsError::io(entry.path(), err))?;
        parts.push(sha256_hex(bytes));
    }
    Ok(sha256_hex(parts.join("\0")))
}

fn detect_git_root(path: &Path) -> Option<PathBuf> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(path)
        .arg("rev-parse")
        .arg("--show-toplevel");
    match run_command(&mut command) {
        Ok(output) => Some(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        )),
        Err(_) => None,
    }
}

fn relative_under(root: &Path, path: &Path) -> Result<PathBuf> {
    path.strip_prefix(root).map(Path::to_path_buf).map_err(|_| {
        RgbsError::message(format!(
            "path {} is not under source root {}",
            path.display(),
            root.display()
        ))
    })
}

fn run_rpmbuild(
    plan: &ResolvedBuildPlan,
    spec: &SpecInfo,
    stage: &StageSummary,
    buildconf: Option<&Path>,
    buildroot: Option<&BuildRootSummary>,
) -> Result<RunnerExecution> {
    let stage_root = PathBuf::from(&stage.topdir);
    let spec_path = PathBuf::from(&stage.spec_path);
    let log_path = stage_root.join("logs").join("rpmbuild.log");
    let mut warnings = Vec::new();

    if let Some(execution) = reuse_completed_build(plan, spec, stage, buildroot)? {
        progress(
            "Reusing",
            format!(
                "reusing previous rpmbuild outputs (log: {})",
                execution.summary.log_path
            ),
        );
        return Ok(execution);
    }

    if let Some(root) = buildroot {
        if command_in_path("bwrap") {
            match plan_bwrap_runtime(root) {
                Ok(runtime) => {
                    progress(
                        "Running",
                        format!(
                            "running rpmbuild with bwrap backend (log: {})",
                            log_path.display()
                        ),
                    );
                    match run_bwrap_rpmbuild(
                        plan,
                        spec,
                        buildconf,
                        root,
                        &runtime,
                        &stage_root,
                        &spec_path,
                        &log_path,
                    ) {
                        Ok(summary) => {
                            save_build_stamp(
                                &stage_root,
                                spec,
                                stage,
                                buildroot,
                                plan.skip_srcrpm,
                            )?;
                            return Ok(RunnerExecution { summary, warnings });
                        }
                        Err(err) => {
                            let message = format!(
                                "bwrap backend failed, falling back to host rpmbuild: {err}"
                            );
                            progress_warn(&message);
                            warnings.push(message);
                        }
                    }
                }
                Err(err) => {
                    let message = format!(
                        "bwrap backend unavailable for this buildroot, falling back to host rpmbuild: {err}"
                    );
                    progress_warn(&message);
                    warnings.push(message);
                }
            }
        } else {
            let message = "bubblewrap is not installed; using host rpmbuild".to_string();
            progress_warn(&message);
            warnings.push(message);
        }
    }

    progress(
        "Running",
        format!(
            "running rpmbuild with host backend (log: {})",
            log_path.display()
        ),
    );
    let summary = run_host_rpmbuild(plan, spec, buildconf, &stage_root, &spec_path, &log_path)?;
    save_build_stamp(&stage_root, spec, stage, buildroot, plan.skip_srcrpm)?;
    Ok(RunnerExecution { summary, warnings })
}

fn run_host_rpmbuild(
    plan: &ResolvedBuildPlan,
    _spec: &SpecInfo,
    buildconf: Option<&Path>,
    stage_root: &Path,
    spec_path: &Path,
    log_path: &Path,
) -> Result<RunnerSummary> {
    let mut command = Command::new("rpmbuild");
    command.arg(if plan.skip_srcrpm { "-bb" } else { "-ba" });
    add_rpmbuild_defines(&mut command, stage_root, buildconf, &plan.defines)?;
    command.arg(spec_path);
    run_logged_command(command, log_path, "host")
}

fn run_bwrap_rpmbuild(
    plan: &ResolvedBuildPlan,
    _spec: &SpecInfo,
    buildconf: Option<&Path>,
    buildroot: &BuildRootSummary,
    runtime: &BwrapRuntime,
    stage_root: &Path,
    spec_path: &Path,
    log_path: &Path,
) -> Result<RunnerSummary> {
    let root = PathBuf::from(&buildroot.path);
    let mut inner = Command::new("/usr/bin/rpmbuild");
    inner.arg(if plan.skip_srcrpm { "-bb" } else { "-ba" });
    add_rpmbuild_defines(&mut inner, Path::new("/build"), buildconf, &plan.defines)?;
    inner.arg(format!(
        "/build/SPECS/{}",
        spec_path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let rendered_inner = render_command(&inner);

    let mut command = Command::new("bwrap");
    command
        .arg("--bind")
        .arg(&root)
        .arg("/")
        .arg("--bind")
        .arg(stage_root)
        .arg("/build")
        .arg("--dev-bind")
        .arg("/dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--ro-bind")
        .arg("/sys")
        .arg("/sys")
        .arg("--setenv")
        .arg("HOME")
        .arg("/root")
        .arg("--setenv")
        .arg("PATH")
        .arg("/usr/bin:/bin")
        .arg("--chdir")
        .arg("/build")
        .arg("/bin/sh")
        .arg("-lc");
    add_runtime_binds(&mut command, runtime);
    command.arg(rendered_inner);
    run_logged_command(command, log_path, "bwrap")
}

fn build_stamp_path(stage_root: &Path) -> PathBuf {
    stage_root.join("logs").join("build-stamp.json")
}

fn reuse_completed_build(
    plan: &ResolvedBuildPlan,
    spec: &SpecInfo,
    stage: &StageSummary,
    buildroot: Option<&BuildRootSummary>,
) -> Result<Option<RunnerExecution>> {
    if plan.overwrite || !stage.reused {
        return Ok(None);
    }

    let stage_root = PathBuf::from(&stage.topdir);
    let Some(stamp) = load_build_stamp(&stage_root)? else {
        return Ok(None);
    };
    if stamp.source_fingerprint != stage.source_fingerprint
        || stamp.skip_srcrpm != plan.skip_srcrpm
        || stamp.spec_name != spec.name
        || stamp.spec_version != spec.version
    {
        return Ok(None);
    }

    match buildroot {
        Some(buildroot) => {
            if stamp.buildroot_fingerprint.as_deref() != Some(buildroot.fingerprint.as_str())
                || stamp.buildroot_package_fingerprint.as_deref()
                    != Some(buildroot.package_fingerprint.as_str())
            {
                return Ok(None);
            }
        }
        None => {
            if stamp.buildroot_fingerprint.is_some()
                || stamp.buildroot_package_fingerprint.is_some()
            {
                return Ok(None);
            }
        }
    }

    if !stage_outputs_present(&stage_root, plan.skip_srcrpm)? {
        return Ok(None);
    }

    Ok(Some(RunnerExecution {
        summary: RunnerSummary {
            backend: "cached".to_string(),
            command: "existing build outputs reused".to_string(),
            log_path: path_to_string(&stage_root.join("logs").join("rpmbuild.log")),
            reused: true,
        },
        warnings: Vec::new(),
    }))
}

fn load_build_stamp(stage_root: &Path) -> Result<Option<BuildStamp>> {
    let path = build_stamp_path(stage_root);
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(&path).map_err(|err| RgbsError::io(&path, err))?;
    match serde_json::from_slice::<BuildStamp>(&bytes) {
        Ok(stamp) => Ok(Some(stamp)),
        Err(_) => Ok(None),
    }
}

fn save_build_stamp(
    stage_root: &Path,
    spec: &SpecInfo,
    stage: &StageSummary,
    buildroot: Option<&BuildRootSummary>,
    skip_srcrpm: bool,
) -> Result<()> {
    let path = build_stamp_path(stage_root);
    let stamp = BuildStamp {
        schema: "build-stamp-v1".to_string(),
        source_fingerprint: stage.source_fingerprint.clone(),
        buildroot_fingerprint: buildroot.map(|root| root.fingerprint.clone()),
        buildroot_package_fingerprint: buildroot.map(|root| root.package_fingerprint.clone()),
        skip_srcrpm,
        spec_name: spec.name.clone(),
        spec_version: spec.version.clone(),
    };
    let bytes = serde_json::to_vec_pretty(&stamp)
        .map_err(|err| RgbsError::message(format!("serialize build stamp: {err}")))?;
    atomic_write(&path, &bytes)
}

fn stage_outputs_present(stage_root: &Path, skip_srcrpm: bool) -> Result<bool> {
    let has_rpms = !collect_paths(&stage_root.join("RPMS"), "rpm")?.is_empty();
    let has_srpms = skip_srcrpm || !collect_paths(&stage_root.join("SRPMS"), "rpm")?.is_empty();
    Ok(has_rpms && has_srpms)
}

fn add_rpmbuild_defines(
    command: &mut Command,
    topdir: &Path,
    buildconf: Option<&Path>,
    defines: &[String],
) -> Result<()> {
    let mappings = [
        ("_topdir", topdir.to_path_buf()),
        ("_builddir", topdir.join("BUILD")),
        ("_buildrootdir", topdir.join("BUILDROOT")),
        ("_rpmdir", topdir.join("RPMS")),
        ("_srcrpmdir", topdir.join("SRPMS")),
        ("_sourcedir", topdir.join("SOURCES")),
        ("_specdir", topdir.join("SPECS")),
        ("_tmppath", topdir.join("tmp")),
    ];
    for (name, value) in mappings {
        command
            .arg("--define")
            .arg(format!("{name} {}", value.display()));
    }
    if let Some(path) = buildconf {
        for define in parse_buildconf_defines(path)? {
            command.arg("--define").arg(define);
        }
    }
    for define in defines {
        command.arg("--define").arg(define);
    }
    Ok(())
}

fn parse_buildconf_defines(path: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).map_err(|err| RgbsError::io(path, err))?;
    Ok(text
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("%define ") || trimmed.starts_with("%global ") {
                let mut parts = trimmed.splitn(3, char::is_whitespace);
                let _directive = parts.next();
                let name = parts.next()?;
                let value = parts.next()?;
                Some(format!("{} {}", name.trim(), value.trim()))
            } else {
                None
            }
        })
        .collect())
}

fn command_in_path(program: &str) -> bool {
    env::var_os("PATH")
        .map(|value| {
            env::split_paths(&value).any(|path| {
                let candidate = path.join(program);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

fn plan_bwrap_runtime(buildroot: &BuildRootSummary) -> Result<BwrapRuntime> {
    let root = PathBuf::from(&buildroot.path);
    let mut binds = Vec::new();
    let mut seen = HashSet::new();

    ensure_dir(&root.join("root"))?;
    ensure_dir(&root.join("tmp"))?;
    ensure_dir(&root.join("var/tmp"))?;

    for program in [
        "/bin/sh",
        "/bin/bash",
        "/usr/bin/rpmbuild",
        "/usr/bin/rpm",
        "/usr/bin/tar",
        "/usr/bin/gzip",
        "/usr/bin/bzip2",
        "/usr/bin/xz",
        "/usr/bin/patch",
        "/usr/bin/find",
        "/usr/bin/sed",
        "/usr/bin/make",
        "/usr/bin/install",
        "/usr/bin/cp",
        "/usr/bin/rm",
        "/usr/bin/mkdir",
        "/usr/bin/chmod",
        "/usr/bin/strip",
    ] {
        add_host_program_bind_if_missing(&root, Path::new(program), &mut binds, &mut seen)?;
    }

    for directory in [
        "/usr/lib/rpm",
        "/usr/lib64/rpm",
        "/usr/share/rpm",
        "/etc/rpm",
    ] {
        add_host_directory_bind_if_missing(&root, Path::new(directory), &mut binds, &mut seen)?;
    }

    if !root.join("bin/sh").exists() && !has_destination_bind(&binds, Path::new("/bin/sh")) {
        return Err(RgbsError::message(
            "no usable /bin/sh was found in the buildroot or host runtime bootstrap",
        ));
    }
    if !root.join("usr/bin/rpmbuild").exists()
        && !has_destination_bind(&binds, Path::new("/usr/bin/rpmbuild"))
    {
        return Err(RgbsError::message(
            "no usable /usr/bin/rpmbuild was found in the buildroot or host runtime bootstrap",
        ));
    }

    Ok(BwrapRuntime { binds })
}

fn add_runtime_binds(command: &mut Command, runtime: &BwrapRuntime) {
    for bind in &runtime.binds {
        command
            .arg("--ro-bind")
            .arg(&bind.source)
            .arg(&bind.destination);
    }
}

fn add_host_program_bind_if_missing(
    root: &Path,
    destination: &Path,
    binds: &mut Vec<BindMount>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    if root.join(relative_root_path(destination)?).exists() || !destination.exists() {
        return Ok(());
    }
    let source = destination
        .canonicalize()
        .map_err(|err| RgbsError::io(destination, err))?;
    push_bind(root, source.clone(), destination.to_path_buf(), binds, seen)?;
    for library in ldd_dependencies(&source)? {
        if library.exists() {
            let canonical = library
                .canonicalize()
                .map_err(|err| RgbsError::io(&library, err))?;
            push_bind(root, canonical, library, binds, seen)?;
        }
    }
    Ok(())
}

fn add_host_directory_bind_if_missing(
    root: &Path,
    destination: &Path,
    binds: &mut Vec<BindMount>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    if root.join(relative_root_path(destination)?).exists() || !destination.exists() {
        return Ok(());
    }
    let source = destination
        .canonicalize()
        .map_err(|err| RgbsError::io(destination, err))?;
    push_bind(root, source, destination.to_path_buf(), binds, seen)
}

fn push_bind(
    root: &Path,
    source: PathBuf,
    destination: PathBuf,
    binds: &mut Vec<BindMount>,
    seen: &mut HashSet<PathBuf>,
) -> Result<()> {
    if !seen.insert(destination.clone()) {
        return Ok(());
    }
    let target = root.join(relative_root_path(&destination)?);
    if source.is_dir() {
        ensure_dir(&target)?;
    } else {
        if let Some(parent) = target.parent() {
            ensure_dir(parent)?;
        }
        if !target.exists() {
            atomic_write(&target, b"")?;
        }
    }
    binds.push(BindMount {
        source,
        destination,
    });
    Ok(())
}

fn has_destination_bind(binds: &[BindMount], destination: &Path) -> bool {
    binds.iter().any(|bind| bind.destination == destination)
}

fn ldd_dependencies(binary: &Path) -> Result<Vec<PathBuf>> {
    let output = Command::new("ldd")
        .arg(binary)
        .output()
        .map_err(|err| RgbsError::message(err.to_string()))?;
    if !output.status.success() {
        return Err(RgbsError::message(format!(
            "ldd failed for {}: {}",
            binary.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(parse_ldd_output(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_ldd_output(output: &str) -> Vec<PathBuf> {
    let mut libraries = Vec::new();
    let mut seen = HashSet::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let candidate = if let Some((_, rhs)) = trimmed.split_once("=>") {
            rhs.split_whitespace().next().unwrap_or_default()
        } else {
            trimmed.split_whitespace().next().unwrap_or_default()
        };
        if !candidate.starts_with('/') {
            continue;
        }
        let path = PathBuf::from(candidate);
        if seen.insert(path.clone()) {
            libraries.push(path);
        }
    }
    libraries
}

fn relative_root_path(path: &Path) -> Result<PathBuf> {
    path.strip_prefix("/")
        .map(Path::to_path_buf)
        .map_err(|_| RgbsError::message(format!("path is not absolute: {}", path.display())))
}

fn run_logged_command(
    mut command: Command,
    log_path: &Path,
    backend: &str,
) -> Result<RunnerSummary> {
    let rendered = render_command(&command);
    let output = command
        .output()
        .map_err(|err| RgbsError::message(err.to_string()))?;
    let mut log = Vec::new();
    log.write_all(&output.stdout)
        .map_err(|err| RgbsError::message(err.to_string()))?;
    if !output.stderr.is_empty() {
        log.write_all(b"\n--- stderr ---\n")
            .map_err(|err| RgbsError::message(err.to_string()))?;
        log.write_all(&output.stderr)
            .map_err(|err| RgbsError::message(err.to_string()))?;
    }
    atomic_write(log_path, &log)?;
    if !output.status.success() {
        return Err(RgbsError::message(format!(
            "build command failed; see {}",
            log_path.display()
        )));
    }
    Ok(RunnerSummary {
        backend: backend.to_string(),
        command: rendered,
        log_path: path_to_string(log_path),
        reused: false,
    })
}

fn collect_artifacts(plan: &ResolvedBuildPlan, stage: &StageSummary) -> Result<ArtifactSummary> {
    let stage_root = PathBuf::from(&stage.topdir);
    let rpms = collect_paths(&stage_root.join("RPMS"), "rpm")?;
    let srpms = collect_paths(&stage_root.join("SRPMS"), "rpm")?;
    let profile_name = plan.profile.name.trim_start_matches("profile.");
    let output_repo = PathBuf::from(&plan.buildroot)
        .join("repos")
        .join(profile_name)
        .join(&plan.arch);
    if plan.clean_repos && output_repo.exists() {
        fs::remove_dir_all(&output_repo).map_err(|err| RgbsError::io(&output_repo, err))?;
    }
    ensure_dir(&output_repo.join("RPMS"))?;
    ensure_dir(&output_repo.join("SRPMS"))?;
    for rpm in &rpms {
        let source = PathBuf::from(rpm);
        let target = output_repo
            .join("RPMS")
            .join(source.file_name().unwrap_or_default());
        fs::copy(&source, &target).map_err(|err| RgbsError::io(&target, err))?;
    }
    for srpm in &srpms {
        let source = PathBuf::from(srpm);
        let target = output_repo
            .join("SRPMS")
            .join(source.file_name().unwrap_or_default());
        fs::copy(&source, &target).map_err(|err| RgbsError::io(&target, err))?;
    }

    let repodata_refreshed = if command_in_path("createrepo_c") {
        let output = Command::new("createrepo_c")
            .arg(output_repo.join("RPMS"))
            .output()
            .map_err(|err| RgbsError::message(err.to_string()))?;
        output.status.success()
    } else {
        false
    };

    Ok(ArtifactSummary {
        rpms,
        srpms,
        output_repo: path_to_string(&output_repo),
        repodata_refreshed,
    })
}

fn collect_paths(root: &Path, extension: &str) -> Result<Vec<String>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut files = WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| entry.into_path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some(extension))
        .map(|path| path_to_string(&path))
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::Path;
    use std::process::Command;

    use rgbs_config::{ProfileKind, ResolvedProfile};
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn parses_ldd_output_for_absolute_library_paths() {
        let output = r#"
            linux-vdso.so.1 (0x0000ffff8201a000)
            librpm.so.9 => /lib/aarch64-linux-gnu/librpm.so.9 (0x0000ffff81f20000)
            libc.so.6 => /lib/aarch64-linux-gnu/libc.so.6 (0x0000ffff81cc0000)
            /lib/ld-linux-aarch64.so.1 (0x0000ffff81fe1000)
            libmissing.so => not found
        "#;
        let parsed = parse_ldd_output(output);
        assert_eq!(parsed.len(), 3);
        assert!(
            parsed
                .iter()
                .any(|path| path == Path::new("/lib/aarch64-linux-gnu/librpm.so.9"))
        );
        assert!(
            parsed
                .iter()
                .any(|path| path == Path::new("/lib/aarch64-linux-gnu/libc.so.6"))
        );
        assert!(
            parsed
                .iter()
                .any(|path| path == Path::new("/lib/ld-linux-aarch64.so.1"))
        );
    }

    #[test]
    fn include_all_controls_materialized_snapshot_contents() {
        let fixture = TempDir::new().unwrap();
        let repo = fixture.path().join("pkg");
        fs::create_dir_all(repo.join("packaging")).unwrap();
        fs::write(repo.join("foo.txt"), "committed\n").unwrap();
        fs::write(
            repo.join("packaging/test.spec"),
            "Name: test\nVersion: 1.0\nRelease: 1\nSummary: test\nLicense: MIT\n",
        )
        .unwrap();
        init_git_repo(&repo);

        fs::write(repo.join("foo.txt"), "modified\n").unwrap();
        fs::write(repo.join("extra.txt"), "untracked\n").unwrap();

        let spec = sample_spec(&repo);

        let clean_snapshot = inspect_source_snapshot(&sample_plan(&repo, false), &spec).unwrap();
        assert_eq!(clean_snapshot.export_mode, "git_archive");
        assert!(!clean_snapshot.warnings.is_empty());
        let clean_target = fixture.path().join("clean-export");
        materialize_source_snapshot(&clean_snapshot, &clean_target).unwrap();
        assert_eq!(
            fs::read_to_string(clean_target.join("foo.txt")).unwrap(),
            "committed\n"
        );
        assert!(!clean_target.join("extra.txt").exists());

        let dirty_snapshot = inspect_source_snapshot(&sample_plan(&repo, true), &spec).unwrap();
        assert_eq!(dirty_snapshot.export_mode, "git_working_tree");
        let dirty_target = fixture.path().join("dirty-export");
        materialize_source_snapshot(&dirty_snapshot, &dirty_target).unwrap();
        assert_eq!(
            fs::read_to_string(dirty_target.join("foo.txt")).unwrap(),
            "modified\n"
        );
        assert_eq!(
            fs::read_to_string(dirty_target.join("extra.txt")).unwrap(),
            "untracked\n"
        );
    }

    #[test]
    fn loads_cached_buildroot_state_for_noinit() {
        let fixture = TempDir::new().unwrap();
        let repo = fixture.path().join("pkg");
        fs::create_dir_all(repo.join("packaging")).unwrap();
        let buildroot = fixture.path().join("buildroot");
        let root_path = buildroot.join("roots/aarch64/example");
        fs::create_dir_all(&root_path).unwrap();
        let buildconf = fixture.path().join("build.conf");
        fs::write(&buildconf, "%define distro test\n").unwrap();

        let plan = sample_plan(&repo, false);
        let persisted = PersistedBuildRoot {
            schema: "buildroot-v1".to_string(),
            arch: "aarch64".to_string(),
            profile: "current".to_string(),
            strategy: "shared_keep_packs".to_string(),
            root_path: path_to_string(&root_path),
            fingerprint: "rootfp".to_string(),
            package_fingerprint: "pkgfp".to_string(),
            dependency_fingerprint: Some("depfp".to_string()),
            repo_fingerprint: Some("repofp".to_string()),
            buildconf_path: Some(path_to_string(&buildconf)),
            buildconf_fingerprint: Some("buildconffp".to_string()),
            defines_fingerprint: "definesfp".to_string(),
            installed_packages: vec!["bash-0:5.0-1.aarch64".to_string()],
        };
        save_active_buildroot(&plan, &persisted).unwrap();

        let loaded = load_active_buildroot(&plan).unwrap();
        assert_eq!(loaded.root_path, path_to_string(&root_path));
        assert_eq!(loaded.buildconf_path, Some(path_to_string(&buildconf)));

        let summary = buildroot_summary_from_state(&loaded).unwrap();
        assert_eq!(summary.strategy, "shared_keep_packs");
        assert_eq!(summary.package_fingerprint, "pkgfp");
        assert_eq!(summary.installed_packages, 1);
    }

    #[cfg(unix)]
    #[test]
    fn relaxes_directory_permissions_between_payload_extractions() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("root");
        let src1 = fixture.path().join("src1");
        let src2 = fixture.path().join("src2");
        fs::create_dir_all(src1.join("usr/bin")).unwrap();
        fs::create_dir_all(src2.join("usr/bin")).unwrap();
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(src1.join("usr/bin"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::write(src2.join("usr/bin/find"), "payload\n").unwrap();

        let tar1 = fixture.path().join("one.tgz");
        let tar2 = fixture.path().join("two.tgz");
        run(Command::new("tar")
            .arg("-C")
            .arg(&src1)
            .arg("-czf")
            .arg(&tar1)
            .arg("."));
        run(Command::new("tar")
            .arg("-C")
            .arg(&src2)
            .arg("-czf")
            .arg(&tar2)
            .arg("."));

        run(Command::new("tar")
            .arg("-xf")
            .arg(&tar1)
            .arg("-C")
            .arg(&root)
            .arg("--delay-directory-restore")
            .arg("--no-same-owner"));

        let before = fs::metadata(root.join("usr/bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(before & 0o777, 0o555);

        relax_buildroot_directory_permissions(&root).unwrap();

        run(Command::new("tar")
            .arg("-xf")
            .arg(&tar2)
            .arg("-C")
            .arg(&root)
            .arg("--delay-directory-restore")
            .arg("--no-same-owner"));

        let after = fs::metadata(root.join("usr/bin"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(after & 0o200, 0);
        assert_eq!(
            fs::read_to_string(root.join("usr/bin/find")).unwrap(),
            "payload\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merges_staged_directories_into_symlinked_buildroot_paths() {
        let fixture = TempDir::new().unwrap();
        let root = fixture.path().join("root");
        let staging = fixture.path().join("staging");
        fs::create_dir_all(root.join("usr/lib")).unwrap();
        fs::create_dir_all(root.join("lib/firmware/existing")).unwrap();
        fs::create_dir_all(staging.join("usr/lib/firmware/updates")).unwrap();
        symlink("../../lib/firmware", root.join("usr/lib/firmware")).unwrap();
        fs::write(staging.join("usr/lib/firmware/updates/marker"), "merged\n").unwrap();

        merge_payload_tree(&staging, &root).unwrap();

        assert!(
            fs::symlink_metadata(root.join("usr/lib/firmware"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(root.join("lib/firmware/updates/marker")).unwrap(),
            "merged\n"
        );
        assert!(root.join("lib/firmware/existing").is_dir());
    }

    fn sample_plan(repo: &Path, include_all: bool) -> ResolvedBuildPlan {
        let buildroot = repo.parent().unwrap().join("buildroot");
        ResolvedBuildPlan {
            execution: "test".to_string(),
            config_files: Vec::new(),
            git_dir: path_to_string(repo),
            arch: "aarch64".to_string(),
            profile: ResolvedProfile {
                kind: ProfileKind::Profile,
                name: "profile.current".to_string(),
                repos: Vec::new(),
                buildroot: None,
                buildconf: None,
                exclude_packages: Vec::new(),
                obs: None,
                source: None,
                depends: None,
                pkgs: None,
                common_user: None,
                common_password: None,
            },
            buildroot: path_to_string(&buildroot),
            packaging_dir: "packaging".to_string(),
            work_dir: path_to_string(repo),
            buildconf: None,
            repos: Vec::new(),
            defines: Vec::new(),
            spec: None,
            include_all,
            noinit: false,
            clean: false,
            keep_packs: false,
            overwrite: false,
            fail_fast: false,
            clean_repos: false,
            skip_srcrpm: false,
            perf: false,
        }
    }

    fn sample_spec(repo: &Path) -> SpecInfo {
        SpecInfo {
            packaging_dir: path_to_string(&repo.join("packaging")),
            spec_path: path_to_string(&repo.join("packaging/test.spec")),
            name: "test".to_string(),
            version: "1.0".to_string(),
            release: "1".to_string(),
            build_requires: Vec::new(),
            sources: Vec::new(),
        }
    }

    fn init_git_repo(repo: &Path) {
        run(Command::new("git").arg("init").arg(repo));
        run(Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("config")
            .arg("user.email")
            .arg("test@example.com"));
        run(Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("config")
            .arg("user.name")
            .arg("Test User"));
        run(Command::new("git").arg("-C").arg(repo).arg("add").arg("."));
        run(Command::new("git")
            .arg("-C")
            .arg(repo)
            .arg("commit")
            .arg("-m")
            .arg("initial"));
    }

    fn run(command: &mut Command) {
        let output = command.output().unwrap();
        if !output.status.success() {
            panic!(
                "command failed: {}\nstdout: {}\nstderr: {}",
                render_command(command),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
