use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{ArgAction, Parser, Subcommand};
use rgbs_builder::execute_build;
use rgbs_common::{
    Result, RgbsError, canonicalize_target_arch, normalize_arch, path_to_string, render_command,
    supported_target_arch_list,
};
use rgbs_config::{BuildRequest, LoadOptions, load};

#[derive(Debug, Parser)]
#[command(name = "rgbs")]
#[command(about = "Rust GBS prototype", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Build(BuildArgs),
    Doctor(DoctorArgs),
    Fix(FixArgs),
}

#[derive(Debug, clap::Args)]
struct BuildArgs {
    gitdir: Option<PathBuf>,
    #[arg(
        short = 'A',
        long = "arch",
        value_parser = parse_target_arch_arg,
        help = "target arch: armv7l or aarch64"
    )]
    arch: String,
    #[arg(short = 'P', long = "profile")]
    profile: Option<String>,
    #[arg(short = 'R', long = "repository", action = ArgAction::Append)]
    repositories: Vec<String>,
    #[arg(short = 'D', long = "dist")]
    dist: Option<String>,
    #[arg(short = 'B', long = "buildroot")]
    buildroot: Option<String>,
    #[arg(long = "define", action = ArgAction::Append)]
    defines: Vec<String>,
    #[arg(long = "spec")]
    spec: Option<PathBuf>,
    #[arg(long = "include-all", default_value_t = false)]
    include_all: bool,
    #[arg(long = "noinit", default_value_t = false)]
    noinit: bool,
    #[arg(long = "clean", default_value_t = false)]
    clean: bool,
    #[arg(long = "keep-packs", default_value_t = false)]
    keep_packs: bool,
    #[arg(long = "overwrite", default_value_t = false)]
    overwrite: bool,
    #[arg(long = "fail-fast", default_value_t = false)]
    fail_fast: bool,
    #[arg(long = "clean-repos", default_value_t = false)]
    clean_repos: bool,
    #[arg(long = "skip-srcrpm", default_value_t = false)]
    skip_srcrpm: bool,
    #[arg(long = "perf", visible_alias = "time", default_value_t = false)]
    perf: bool,
}

#[derive(Debug, clap::Args)]
struct DoctorArgs {
    #[arg(
        short = 'A',
        long = "arch",
        value_parser = parse_target_arch_arg,
        help = "target arch: armv7l or aarch64"
    )]
    arch: Option<String>,
}

#[derive(Debug, clap::Args)]
struct FixArgs {
    #[arg(
        short = 'A',
        long = "arch",
        value_parser = parse_target_arch_arg,
        help = "target arch: armv7l or aarch64"
    )]
    arch: Option<String>,
    #[arg(long = "with-source-build", default_value_t = false)]
    with_source_build: bool,
    #[arg(long = "dry-run", default_value_t = false)]
    dry_run: bool,
    #[arg(short = 'y', long = "yes", default_value_t = false)]
    yes: bool,
    #[arg(long = "update", default_value_t = false)]
    update: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorStatus {
    Ok,
    Missing,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallClass {
    RuntimeRequired,
    Recommended,
    HostToolchain,
    CrossToolchain,
    SourceBuild,
}

impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Missing => "MISSING",
            Self::Warning => "WARN",
        }
    }
}

#[derive(Debug, Clone)]
struct DoctorCheck {
    section: &'static str,
    name: String,
    status: DoctorStatus,
    found: Option<String>,
    note: String,
    recommendation: Option<String>,
    required_runtime: bool,
    install_class: InstallClass,
    apt_packages: Vec<String>,
}

#[derive(Debug, Clone)]
struct DoctorReport {
    host_arch: String,
    target_arch: Option<String>,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug)]
struct FixPlan {
    install_packages: Vec<String>,
    unavailable_packages: Vec<String>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build(args) => run_build(args)?,
        Commands::Doctor(args) => run_doctor(args)?,
        Commands::Fix(args) => run_fix(args)?,
    }

    Ok(())
}

fn run_build(args: BuildArgs) -> Result<()> {
    let options = LoadOptions::discover(None)?;
    let config = load(&options)?;
    let plan = config.resolve_build_plan(&BuildRequest {
        git_dir: args.gitdir.unwrap_or(options.cwd),
        arch: args.arch,
        profile: args.profile,
        repositories: args.repositories,
        dist: args.dist,
        buildroot: args.buildroot,
        defines: args.defines,
        spec: args.spec,
        include_all: args.include_all,
        noinit: args.noinit,
        clean: args.clean,
        keep_packs: args.keep_packs,
        overwrite: args.overwrite,
        fail_fast: args.fail_fast,
        clean_repos: args.clean_repos,
        skip_srcrpm: args.skip_srcrpm,
        perf: args.perf,
    })?;
    let outcome = execute_build(&plan)?;

    println!("{}", serde_json::to_string_pretty(&outcome).unwrap());
    Ok(())
}

fn run_doctor(args: DoctorArgs) -> Result<()> {
    let report = collect_doctor_report(args.arch.as_deref());
    print_doctor_report(&report);
    if report
        .checks
        .iter()
        .any(|check| check.required_runtime && check.status == DoctorStatus::Missing)
    {
        return Err(RgbsError::message(
            "doctor found missing required runtime tools for `rgbs build`",
        ));
    }
    Ok(())
}

fn run_fix(args: FixArgs) -> Result<()> {
    ensure_ubuntu_fix_support()?;

    let report = collect_doctor_report(args.arch.as_deref());
    let plan = build_fix_plan(&report, args.with_source_build)?;
    print_fix_plan(
        &report,
        &plan,
        args.with_source_build,
        args.update,
        args.dry_run,
    );

    if plan.install_packages.is_empty() {
        if plan.unavailable_packages.is_empty() {
            println!("nothing to install");
            return Ok(());
        }
        return Err(RgbsError::message(
            "no installable Ubuntu packages identified for the requested fix scope",
        ));
    }

    if args.dry_run {
        return Ok(());
    }

    if args.update {
        run_apt_install_command(&["update".to_string()], false)?;
    }
    run_apt_install_command(&plan.install_packages, args.yes)?;
    Ok(())
}

fn collect_doctor_report(target_arch: Option<&str>) -> DoctorReport {
    let host_arch = host_arch();
    let mut checks = Vec::new();

    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "git",
        &["git"],
        true,
        InstallClass::RuntimeRequired,
        &["git"],
        "required for source export and git status inspection",
        "install the package that provides `git`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "tar",
        &["tar"],
        true,
        InstallClass::RuntimeRequired,
        &["tar"],
        "required for source archive creation and extraction",
        "install the package that provides `tar`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "rpm",
        &["rpm"],
        true,
        InstallClass::RuntimeRequired,
        &["rpm"],
        "required for rpmdb initialization and buildroot package installation",
        "install the RPM tooling package that provides `rpm`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "rpmbuild",
        &["rpmbuild"],
        true,
        InstallClass::RuntimeRequired,
        &["rpm"],
        "required for final RPM/SRPM creation",
        "install the RPM build tooling package that provides `rpmbuild`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "rpmspec",
        &["rpmspec"],
        true,
        InstallClass::RuntimeRequired,
        &["rpm"],
        "required for spec evaluation and BuildRequires extraction",
        "install the RPM build tooling package that provides `rpmspec`",
    );

    push_command_check(
        &mut checks,
        "Recommended extras",
        "bubblewrap",
        &["bwrap"],
        false,
        InstallClass::Recommended,
        &["bubblewrap"],
        "enables isolated builds instead of falling back to host `rpmbuild`",
        "install `bubblewrap` to use the isolated backend",
    );
    push_command_check(
        &mut checks,
        "Recommended extras",
        "createrepo_c",
        &["createrepo_c"],
        false,
        InstallClass::Recommended,
        &["createrepo-c"],
        "refreshes metadata for the output RPM repo after a build",
        "install `createrepo_c` if you want local output repos to carry fresh repodata",
    );

    push_command_check(
        &mut checks,
        "Build `rgbs` from source",
        "rustc",
        &["rustc"],
        false,
        InstallClass::SourceBuild,
        &["rustc"],
        "needed only when building `rgbs` from source",
        "install the Rust toolchain if you want to build `rgbs` locally",
    );
    push_command_check(
        &mut checks,
        "Build `rgbs` from source",
        "cargo",
        &["cargo"],
        false,
        InstallClass::SourceBuild,
        &["cargo"],
        "needed only when building `rgbs` from source",
        "install the Rust toolchain if you want to build `rgbs` locally",
    );
    push_command_check(
        &mut checks,
        "Build `rgbs` from source",
        "cmake",
        &["cmake"],
        false,
        InstallClass::SourceBuild,
        &["cmake"],
        "used by the vendored `libsolv` build",
        "install `cmake` if you want to build `rgbs` from source",
    );
    push_pkg_config_module_check(
        &mut checks,
        "Build `rgbs` from source",
        "expat",
        InstallClass::SourceBuild,
        &["libexpat1-dev"],
        "used by the vendored `libsolv` rpm-md build",
        "install the Expat development package that exposes `pkg-config --exists expat`",
    );

    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "C compiler",
        &["cc", "gcc", "clang"],
        false,
        InstallClass::HostToolchain,
        &["build-essential"],
        "common native compiler choices on the host",
        "install a native C compiler such as `gcc` or `clang` if your package workflow expects one on the host",
    );
    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "C++ compiler",
        &["c++", "g++", "clang++"],
        false,
        InstallClass::HostToolchain,
        &["build-essential"],
        "common native C++ compiler choices on the host",
        "install a native C++ compiler such as `g++` or `clang++` if your package workflow expects one on the host",
    );
    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "make",
        &["make"],
        false,
        InstallClass::HostToolchain,
        &["build-essential"],
        "common host build tool used by many specs and source builds",
        "install `make` if your package workflow expects host build tools",
    );
    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "pkg-config",
        &["pkg-config"],
        false,
        InstallClass::HostToolchain,
        &["pkg-config"],
        "common host dependency discovery tool",
        "install `pkg-config` if your package workflow expects host development tooling",
    );

    if let Some(target_arch) = target_arch {
        if !same_arch(target_arch, &host_arch) {
            let candidates = cross_compiler_candidates(target_arch);
            let apt_packages = cross_compiler_apt_packages(target_arch);
            if !candidates.is_empty() {
                push_command_check(
                    &mut checks,
                    "Cross toolchain hints",
                    &format!("cross C compiler for {target_arch}"),
                    &candidates,
                    false,
                    InstallClass::CrossToolchain,
                    &apt_packages,
                    "common cross-compiler names for the selected target arch; exact use still depends on your RPM macros and buildconf",
                    &format!(
                        "install a cross compiler for `{target_arch}` if your package expects one on the host; common names: {}",
                        candidates.join(", ")
                    ),
                );
            } else {
                checks.push(DoctorCheck {
                    section: "Cross toolchain hints",
                    name: format!("cross C compiler for {target_arch}"),
                    status: DoctorStatus::Warning,
                    found: None,
                    note: "no built-in candidate list for this arch; `rgbs` currently relies on RPM macros and environment for explicit host cross-toolchain selection".to_string(),
                    recommendation: Some(
                        "check your profile/buildconf macros and install the host cross toolchain they expect".to_string(),
                    ),
                    required_runtime: false,
                    install_class: InstallClass::CrossToolchain,
                    apt_packages: apt_packages
                        .iter()
                        .map(|package| (*package).to_string())
                        .collect(),
                });
            }
        }
    }

    DoctorReport {
        host_arch,
        target_arch: target_arch.map(ToOwned::to_owned),
        checks,
    }
}

fn print_doctor_report(report: &DoctorReport) {
    let required_ok = report
        .checks
        .iter()
        .filter(|check| check.required_runtime && check.status == DoctorStatus::Ok)
        .count();
    let required_missing = report
        .checks
        .iter()
        .filter(|check| check.required_runtime && check.status == DoctorStatus::Missing)
        .count();
    let advisory_issues = report
        .checks
        .iter()
        .filter(|check| !check.required_runtime && check.status != DoctorStatus::Ok)
        .count();

    println!("rgbs doctor");
    println!("host arch: {}", report.host_arch);
    if let Some(target_arch) = &report.target_arch {
        println!("target arch: {target_arch}");
    }
    println!(
        "summary: {required_ok} required ok, {required_missing} required missing, {advisory_issues} advisory issue(s)"
    );

    let mut current_section = None;
    for check in &report.checks {
        if current_section != Some(check.section) {
            println!();
            println!("{}:", check.section);
            current_section = Some(check.section);
        }

        let found = check.found.as_deref().unwrap_or("-");
        println!("{:<8} {:<28} {}", check.status.label(), check.name, found);
        println!("         {}", check.note);
        if check.status != DoctorStatus::Ok {
            if let Some(recommendation) = &check.recommendation {
                println!("         recommendation: {recommendation}");
            }
        }
    }
}

fn push_command_check(
    checks: &mut Vec<DoctorCheck>,
    section: &'static str,
    name: &str,
    candidates: &[&str],
    required_runtime: bool,
    install_class: InstallClass,
    apt_packages: &[&str],
    note: &str,
    recommendation: &str,
) {
    let found = candidates
        .iter()
        .find_map(|candidate| find_program(candidate).map(|path| ((*candidate).to_string(), path)));
    match found {
        Some((candidate, path)) => checks.push(DoctorCheck {
            section,
            name: name.to_string(),
            status: DoctorStatus::Ok,
            found: Some(path_to_string(&path)),
            note: if candidates.len() == 1 {
                note.to_string()
            } else {
                format!("{note}; using `{candidate}`")
            },
            recommendation: None,
            required_runtime,
            install_class,
            apt_packages: apt_packages
                .iter()
                .map(|package| (*package).to_string())
                .collect(),
        }),
        None => checks.push(DoctorCheck {
            section,
            name: name.to_string(),
            status: DoctorStatus::Missing,
            found: None,
            note: note.to_string(),
            recommendation: Some(recommendation.to_string()),
            required_runtime,
            install_class,
            apt_packages: apt_packages
                .iter()
                .map(|package| (*package).to_string())
                .collect(),
        }),
    }
}

fn push_pkg_config_module_check(
    checks: &mut Vec<DoctorCheck>,
    section: &'static str,
    module: &str,
    install_class: InstallClass,
    apt_packages: &[&str],
    note: &str,
    recommendation: &str,
) {
    let Some(pkg_config) = find_program("pkg-config") else {
        checks.push(DoctorCheck {
            section,
            name: format!("pkg-config module `{module}`"),
            status: DoctorStatus::Warning,
            found: None,
            note: format!("cannot verify `{module}` because `pkg-config` is not available"),
            recommendation: Some(recommendation.to_string()),
            required_runtime: false,
            install_class,
            apt_packages: apt_packages
                .iter()
                .map(|package| (*package).to_string())
                .collect(),
        });
        return;
    };

    let status = Command::new(&pkg_config)
        .arg("--exists")
        .arg(module)
        .status();
    match status {
        Ok(status) if status.success() => {
            let version = Command::new(&pkg_config)
                .arg("--modversion")
                .arg(module)
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
                .filter(|value| !value.is_empty());
            checks.push(DoctorCheck {
                section,
                name: format!("pkg-config module `{module}`"),
                status: DoctorStatus::Ok,
                found: version,
                note: note.to_string(),
                recommendation: None,
                required_runtime: false,
                install_class,
                apt_packages: apt_packages
                    .iter()
                    .map(|package| (*package).to_string())
                    .collect(),
            });
        }
        Ok(_) => checks.push(DoctorCheck {
            section,
            name: format!("pkg-config module `{module}`"),
            status: DoctorStatus::Missing,
            found: None,
            note: note.to_string(),
            recommendation: Some(recommendation.to_string()),
            required_runtime: false,
            install_class,
            apt_packages: apt_packages
                .iter()
                .map(|package| (*package).to_string())
                .collect(),
        }),
        Err(err) => checks.push(DoctorCheck {
            section,
            name: format!("pkg-config module `{module}`"),
            status: DoctorStatus::Warning,
            found: None,
            note: format!("failed to execute `pkg-config`: {err}"),
            recommendation: Some(recommendation.to_string()),
            required_runtime: false,
            install_class,
            apt_packages: apt_packages
                .iter()
                .map(|package| (*package).to_string())
                .collect(),
        }),
    }
}

fn find_program(program: &str) -> Option<PathBuf> {
    if Path::new(program).is_absolute() {
        return PathBuf::from(program)
            .is_file()
            .then(|| PathBuf::from(program));
    }

    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths).find_map(|path| {
            let candidate = path.join(program);
            candidate.is_file().then_some(candidate)
        })
    })
}

fn host_arch() -> String {
    std::env::consts::ARCH.to_string()
}

fn same_arch(target_arch: &str, host_arch: &str) -> bool {
    normalize_arch(target_arch) == normalize_arch(host_arch)
}

fn parse_target_arch_arg(value: &str) -> std::result::Result<String, String> {
    canonicalize_target_arch(value)
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            format!(
                "unsupported target arch `{value}`; supported targets: {}",
                supported_target_arch_list()
            )
        })
}

fn build_fix_plan(report: &DoctorReport, include_source_build: bool) -> Result<FixPlan> {
    let selected_classes = selected_install_classes(include_source_build);
    let requested_packages = report
        .checks
        .iter()
        .filter(|check| check.status != DoctorStatus::Ok)
        .filter(|check| selected_classes.contains(&check.install_class))
        .flat_map(|check| check.apt_packages.iter().cloned())
        .collect::<BTreeSet<_>>();

    let mut install_packages = Vec::new();
    let mut unavailable_packages = Vec::new();
    for package in requested_packages {
        if ubuntu_package_has_candidate(&package)? {
            install_packages.push(package);
        } else {
            unavailable_packages.push(package);
        }
    }

    Ok(FixPlan {
        install_packages,
        unavailable_packages,
    })
}

fn print_fix_plan(
    report: &DoctorReport,
    plan: &FixPlan,
    include_source_build: bool,
    update: bool,
    dry_run: bool,
) {
    let includes_cross_toolchain = report
        .target_arch
        .as_deref()
        .map(|target_arch| !same_arch(target_arch, &report.host_arch))
        .unwrap_or(false);

    println!("rgbs fix");
    println!("host arch: {}", report.host_arch);
    if let Some(target_arch) = &report.target_arch {
        println!("target arch: {target_arch}");
    }
    println!(
        "scope: runtime, recommended extras, host toolchain{}",
        if includes_cross_toolchain {
            ", cross toolchain"
        } else {
            ""
        }
    );
    if include_source_build {
        println!("source-build prerequisites: included");
    }
    if !plan.install_packages.is_empty() {
        println!("install packages: {}", plan.install_packages.join(", "));
    }
    if !plan.unavailable_packages.is_empty() {
        println!(
            "unavailable packages: {}",
            plan.unavailable_packages.join(", ")
        );
    }
    if dry_run {
        if update {
            let command = apt_command_for(&["update".to_string()], false).unwrap();
            println!("dry-run update: {}", render_command(&command));
        }
        if !plan.install_packages.is_empty() {
            let command = apt_command_for(&plan.install_packages, false).unwrap();
            println!("dry-run install: {}", render_command(&command));
        }
    }
}

fn cross_compiler_candidates(arch: &str) -> Vec<&'static str> {
    match canonicalize_target_arch(arch) {
        Some("aarch64") => vec![
            "aarch64-linux-gnu-gcc",
            "aarch64-none-linux-gnu-gcc",
            "aarch64-linux-gcc",
        ],
        Some("armv7l") => vec![
            "arm-linux-gnueabihf-gcc",
            "armv7hl-linux-gnueabi-gcc",
            "arm-linux-gnu-gcc",
        ],
        _ => Vec::new(),
    }
}

fn cross_compiler_apt_packages(arch: &str) -> Vec<&'static str> {
    match canonicalize_target_arch(arch) {
        Some("aarch64") => vec!["gcc-aarch64-linux-gnu"],
        Some("armv7l") => vec!["gcc-arm-linux-gnueabihf"],
        _ => Vec::new(),
    }
}

fn selected_install_classes(include_source_build: bool) -> Vec<InstallClass> {
    let mut classes = vec![
        InstallClass::RuntimeRequired,
        InstallClass::Recommended,
        InstallClass::HostToolchain,
        InstallClass::CrossToolchain,
    ];
    if include_source_build {
        classes.push(InstallClass::SourceBuild);
    }
    classes
}

fn ensure_ubuntu_fix_support() -> Result<()> {
    if find_program("apt-get").is_none() || find_program("apt-cache").is_none() {
        return Err(RgbsError::message(
            "`rgbs fix` currently requires Ubuntu-style `apt-get` and `apt-cache`",
        ));
    }

    let os_release = parse_os_release_file("/etc/os-release")?;
    let distro_id = os_release.get("ID").map(String::as_str).unwrap_or("");
    if distro_id != "ubuntu" {
        return Err(RgbsError::message(format!(
            "`rgbs fix` currently supports Ubuntu only; detected `{distro_id}`",
        )));
    }

    Ok(())
}

fn parse_os_release_file(path: &str) -> Result<BTreeMap<String, String>> {
    let text = fs::read_to_string(path).map_err(|err| RgbsError::io(path, err))?;
    Ok(parse_os_release_text(&text))
}

fn parse_os_release_text(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            Some((key.to_string(), unquote_os_release_value(value)))
        })
        .collect()
}

fn unquote_os_release_value(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('"') && trimmed.ends_with('"') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

fn ubuntu_package_has_candidate(package: &str) -> Result<bool> {
    let apt_cache = find_program("apt-cache")
        .ok_or_else(|| RgbsError::message("`apt-cache` is not available on this host"))?;
    let output = Command::new(apt_cache)
        .arg("policy")
        .arg(package)
        .output()
        .map_err(|err| {
            RgbsError::command(format!("apt-cache policy {package}"), err.to_string())
        })?;
    if !output.status.success() {
        return Ok(false);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Candidate: "))
        .map(|candidate| candidate != "(none)")
        .unwrap_or(false))
}

fn apt_command_for(packages: &[String], yes: bool) -> Result<Command> {
    let apt_get = find_program("apt-get")
        .ok_or_else(|| RgbsError::message("`apt-get` is not available on this host"))?;
    let mut command = if running_as_root()? {
        Command::new(apt_get)
    } else {
        let sudo = find_program("sudo").ok_or_else(|| {
            RgbsError::message("`sudo` is required to run `rgbs fix` as a non-root user")
        })?;
        let mut command = Command::new(sudo);
        command.arg(apt_get);
        command
    };

    if let Some(first) = packages.first() {
        if first == "update" {
            command.arg("update");
            return Ok(command);
        }
    }

    command.arg("install");
    if yes {
        command.arg("-y");
    }
    command.args(packages);
    Ok(command)
}

fn run_apt_install_command(packages: &[String], yes: bool) -> Result<()> {
    let mut command = apt_command_for(packages, yes)?;
    let rendered = render_command(&command);
    let status = command
        .status()
        .map_err(|err| RgbsError::command(&rendered, err.to_string()))?;
    if status.success() {
        return Ok(());
    }

    Err(RgbsError::command(
        rendered,
        format!("exit status {status}"),
    ))
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

#[cfg(test)]
mod tests {
    use super::{
        DoctorCheck, DoctorReport, DoctorStatus, FixPlan, InstallClass,
        cross_compiler_apt_packages, cross_compiler_candidates, parse_os_release_text,
        parse_target_arch_arg, same_arch,
    };

    #[test]
    fn cross_compiler_candidates_cover_supported_arches() {
        assert!(
            cross_compiler_candidates("aarch64")
                .iter()
                .any(|candidate| *candidate == "aarch64-linux-gnu-gcc")
        );
        assert!(
            cross_compiler_candidates("armv7l")
                .iter()
                .any(|candidate| *candidate == "arm-linux-gnueabihf-gcc")
        );
        assert!(cross_compiler_candidates("x86_64").is_empty());
    }

    #[test]
    fn target_arch_parser_canonicalizes_supported_aliases() {
        assert_eq!(parse_target_arch_arg("arm64").unwrap(), "aarch64");
        assert_eq!(parse_target_arch_arg("armhf").unwrap(), "armv7l");
        assert!(parse_target_arch_arg("x86_64").is_err());
    }

    #[test]
    fn cross_compiler_apt_packages_cover_supported_arches() {
        assert_eq!(
            cross_compiler_apt_packages("aarch64"),
            vec!["gcc-aarch64-linux-gnu"]
        );
        assert_eq!(
            cross_compiler_apt_packages("armv7l"),
            vec!["gcc-arm-linux-gnueabihf"]
        );
        assert!(cross_compiler_apt_packages("x86_64").is_empty());
    }

    #[test]
    fn parses_os_release_key_values() {
        let parsed = parse_os_release_text(
            r#"
ID=ubuntu
NAME="Ubuntu"
ID_LIKE=debian
"#,
        );
        assert_eq!(parsed.get("ID").map(String::as_str), Some("ubuntu"));
        assert_eq!(parsed.get("NAME").map(String::as_str), Some("Ubuntu"));
        assert_eq!(parsed.get("ID_LIKE").map(String::as_str), Some("debian"));
    }

    #[test]
    fn fix_plan_skips_source_build_packages_by_default() {
        let report = DoctorReport {
            host_arch: "x86_64".to_string(),
            target_arch: Some("aarch64".to_string()),
            checks: vec![
                DoctorCheck {
                    section: "Required",
                    name: "rpm".to_string(),
                    status: DoctorStatus::Missing,
                    found: None,
                    note: String::new(),
                    recommendation: None,
                    required_runtime: true,
                    install_class: InstallClass::RuntimeRequired,
                    apt_packages: vec!["rpm".to_string()],
                },
                DoctorCheck {
                    section: "Source",
                    name: "cargo".to_string(),
                    status: DoctorStatus::Missing,
                    found: None,
                    note: String::new(),
                    recommendation: None,
                    required_runtime: false,
                    install_class: InstallClass::SourceBuild,
                    apt_packages: vec!["cargo".to_string()],
                },
            ],
        };

        let plan = build_fix_plan_for_test(&report, false);
        assert_eq!(plan.install_packages, vec!["rpm".to_string()]);
    }

    #[test]
    fn fix_plan_includes_source_build_when_requested() {
        let report = DoctorReport {
            host_arch: "x86_64".to_string(),
            target_arch: Some("armv7l".to_string()),
            checks: vec![
                DoctorCheck {
                    section: "Host",
                    name: "make".to_string(),
                    status: DoctorStatus::Missing,
                    found: None,
                    note: String::new(),
                    recommendation: None,
                    required_runtime: false,
                    install_class: InstallClass::HostToolchain,
                    apt_packages: vec!["build-essential".to_string()],
                },
                DoctorCheck {
                    section: "Cross",
                    name: "cross".to_string(),
                    status: DoctorStatus::Missing,
                    found: None,
                    note: String::new(),
                    recommendation: None,
                    required_runtime: false,
                    install_class: InstallClass::CrossToolchain,
                    apt_packages: vec!["gcc-arm-linux-gnueabihf".to_string()],
                },
                DoctorCheck {
                    section: "Source",
                    name: "cmake".to_string(),
                    status: DoctorStatus::Missing,
                    found: None,
                    note: String::new(),
                    recommendation: None,
                    required_runtime: false,
                    install_class: InstallClass::SourceBuild,
                    apt_packages: vec!["cmake".to_string()],
                },
            ],
        };

        let plan = build_fix_plan_for_test(&report, true);
        assert!(
            plan.install_packages
                .contains(&"build-essential".to_string())
        );
        assert!(
            plan.install_packages
                .contains(&"gcc-arm-linux-gnueabihf".to_string())
        );
        assert!(plan.install_packages.contains(&"cmake".to_string()));
    }

    #[test]
    fn same_arch_normalizes_common_aliases() {
        assert!(same_arch("amd64", "x86_64"));
        assert!(same_arch("i586", "i686"));
        assert!(!same_arch("aarch64", "x86_64"));
    }

    fn build_fix_plan_for_test(report: &DoctorReport, include_source_build: bool) -> FixPlan {
        let mut requested = report
            .checks
            .iter()
            .filter(|check| check.status != DoctorStatus::Ok)
            .filter(|check| {
                let selected = super::selected_install_classes(include_source_build);
                selected.contains(&check.install_class)
            })
            .flat_map(|check| check.apt_packages.iter().cloned())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        requested.sort();
        FixPlan {
            install_packages: requested,
            unavailable_packages: Vec::new(),
        }
    }
}
