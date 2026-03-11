use std::path::PathBuf;

use clap::{ArgAction, Parser, Subcommand};
use rgbs_builder::execute_build;
use rgbs_common::Result;
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
}

#[derive(Debug, clap::Args)]
struct BuildArgs {
    gitdir: Option<PathBuf>,
    #[arg(short = 'A', long = "arch")]
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
