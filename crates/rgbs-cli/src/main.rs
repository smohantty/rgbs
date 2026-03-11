use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use clap::{ArgAction, Parser, Subcommand};
use rgbs_builder::execute_build;
use rgbs_common::{
    Result, RgbsError, canonicalize_target_arch, normalize_arch, path_to_string,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorStatus {
    Ok,
    Missing,
    Warning,
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
}

#[derive(Debug, Clone)]
struct DoctorReport {
    host_arch: String,
    target_arch: Option<String>,
    checks: Vec<DoctorCheck>,
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

fn collect_doctor_report(target_arch: Option<&str>) -> DoctorReport {
    let host_arch = host_arch();
    let mut checks = Vec::new();

    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "git",
        &["git"],
        true,
        "required for source export and git status inspection",
        "install the package that provides `git`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "tar",
        &["tar"],
        true,
        "required for source archive creation and extraction",
        "install the package that provides `tar`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "rpm",
        &["rpm"],
        true,
        "required for rpmdb initialization and buildroot package installation",
        "install the RPM tooling package that provides `rpm`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "rpmbuild",
        &["rpmbuild"],
        true,
        "required for final RPM/SRPM creation",
        "install the RPM build tooling package that provides `rpmbuild`",
    );
    push_command_check(
        &mut checks,
        "Required for `rgbs build`",
        "rpmspec",
        &["rpmspec"],
        true,
        "required for spec evaluation and BuildRequires extraction",
        "install the RPM build tooling package that provides `rpmspec`",
    );

    push_command_check(
        &mut checks,
        "Recommended extras",
        "bubblewrap",
        &["bwrap"],
        false,
        "enables isolated builds instead of falling back to host `rpmbuild`",
        "install `bubblewrap` to use the isolated backend",
    );
    push_command_check(
        &mut checks,
        "Recommended extras",
        "createrepo_c",
        &["createrepo_c"],
        false,
        "refreshes metadata for the output RPM repo after a build",
        "install `createrepo_c` if you want local output repos to carry fresh repodata",
    );

    push_command_check(
        &mut checks,
        "Build `rgbs` from source",
        "rustc",
        &["rustc"],
        false,
        "needed only when building `rgbs` from source",
        "install the Rust toolchain if you want to build `rgbs` locally",
    );
    push_command_check(
        &mut checks,
        "Build `rgbs` from source",
        "cargo",
        &["cargo"],
        false,
        "needed only when building `rgbs` from source",
        "install the Rust toolchain if you want to build `rgbs` locally",
    );
    push_command_check(
        &mut checks,
        "Build `rgbs` from source",
        "cmake",
        &["cmake"],
        false,
        "used by the vendored `libsolv` build",
        "install `cmake` if you want to build `rgbs` from source",
    );
    push_pkg_config_module_check(
        &mut checks,
        "Build `rgbs` from source",
        "expat",
        "used by the vendored `libsolv` rpm-md build",
        "install the Expat development package that exposes `pkg-config --exists expat`",
    );

    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "C compiler",
        &["cc", "gcc", "clang"],
        false,
        "common native compiler choices on the host",
        "install a native C compiler such as `gcc` or `clang` if your package workflow expects one on the host",
    );
    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "C++ compiler",
        &["c++", "g++", "clang++"],
        false,
        "common native C++ compiler choices on the host",
        "install a native C++ compiler such as `g++` or `clang++` if your package workflow expects one on the host",
    );
    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "make",
        &["make"],
        false,
        "common host build tool used by many specs and source builds",
        "install `make` if your package workflow expects host build tools",
    );
    push_command_check(
        &mut checks,
        "Host toolchain hints",
        "pkg-config",
        &["pkg-config"],
        false,
        "common host dependency discovery tool",
        "install `pkg-config` if your package workflow expects host development tooling",
    );

    if let Some(target_arch) = target_arch {
        if !same_arch(target_arch, &host_arch) {
            let candidates = cross_compiler_candidates(target_arch);
            if !candidates.is_empty() {
                push_command_check(
                    &mut checks,
                    "Cross toolchain hints",
                    &format!("cross C compiler for {target_arch}"),
                    &candidates,
                    false,
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
        }),
        None => checks.push(DoctorCheck {
            section,
            name: name.to_string(),
            status: DoctorStatus::Missing,
            found: None,
            note: note.to_string(),
            recommendation: Some(recommendation.to_string()),
            required_runtime,
        }),
    }
}

fn push_pkg_config_module_check(
    checks: &mut Vec<DoctorCheck>,
    section: &'static str,
    module: &str,
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
        }),
        Err(err) => checks.push(DoctorCheck {
            section,
            name: format!("pkg-config module `{module}`"),
            status: DoctorStatus::Warning,
            found: None,
            note: format!("failed to execute `pkg-config`: {err}"),
            recommendation: Some(recommendation.to_string()),
            required_runtime: false,
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

#[cfg(test)]
mod tests {
    use super::{cross_compiler_candidates, parse_target_arch_arg, same_arch};

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
    fn same_arch_normalizes_common_aliases() {
        assert!(same_arch("amd64", "x86_64"));
        assert!(same_arch("i586", "i686"));
        assert!(!same_arch("aarch64", "x86_64"));
    }
}
