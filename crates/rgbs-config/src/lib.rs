use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bzip2::read::BzDecoder;
use indexmap::{IndexMap, IndexSet};
use rgbs_common::{
    Result, RgbsError, canonicalize_target_arch, expand_tilde, path_to_string,
    supported_target_arch_list,
};
use serde::Serialize;
use url::Url;

const DEFAULT_TMPDIR: &str = "/var/tmp";
const DEFAULT_BUILDROOT: &str = "~/GBS-ROOT/";
const DEFAULT_PACKAGING_DIR: &str = "packaging";
const DEFAULT_WORK_DIR: &str = ".";

#[derive(Debug, Clone, PartialEq, Eq)]
struct IniDocument {
    sections: IndexMap<String, IndexMap<String, String>>,
}

#[derive(Debug, Clone)]
struct ConfigLayer {
    path: PathBuf,
    doc: IniDocument,
}

#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub explicit: Option<PathBuf>,
    pub cwd: PathBuf,
    pub home_dir: PathBuf,
    pub system_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct BuildRequest {
    pub git_dir: PathBuf,
    pub arch: String,
    pub profile: Option<String>,
    pub repositories: Vec<String>,
    pub dist: Option<String>,
    pub buildroot: Option<String>,
    pub defines: Vec<String>,
    pub spec: Option<PathBuf>,
    pub include_all: bool,
    pub noinit: bool,
    pub clean: bool,
    pub keep_packs: bool,
    pub overwrite: bool,
    pub fail_fast: bool,
    pub clean_repos: bool,
    pub skip_srcrpm: bool,
    pub perf: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Profile,
    LegacyBuildSections,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepoKind {
    Remote,
    LocalPath,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRepo {
    pub name: String,
    pub kind: RepoKind,
    pub location: String,
    #[serde(skip_serializing)]
    pub raw_location: String,
    pub source: String,
    pub user: Option<String>,
    pub authenticated: bool,
    #[serde(skip_serializing)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedObs {
    pub name: String,
    pub location: String,
    #[serde(skip_serializing)]
    pub raw_location: String,
    pub user: Option<String>,
    pub authenticated: bool,
    pub base_project: Option<String>,
    pub target_project: Option<String>,
    #[serde(skip_serializing)]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedProfile {
    pub kind: ProfileKind,
    pub name: String,
    pub repos: Vec<ResolvedRepo>,
    pub buildroot: Option<String>,
    pub buildconf: Option<String>,
    pub exclude_packages: Vec<String>,
    pub obs: Option<ResolvedObs>,
    pub source: Option<String>,
    pub depends: Option<String>,
    pub pkgs: Option<String>,
    #[serde(skip_serializing)]
    pub common_user: Option<String>,
    #[serde(skip_serializing)]
    pub common_password: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedBuildPlan {
    pub execution: String,
    pub config_files: Vec<String>,
    pub git_dir: String,
    pub arch: String,
    pub profile: ResolvedProfile,
    pub buildroot: String,
    pub packaging_dir: String,
    pub work_dir: String,
    pub buildconf: Option<String>,
    pub repos: Vec<ResolvedRepo>,
    pub defines: Vec<String>,
    pub spec: Option<String>,
    pub include_all: bool,
    pub noinit: bool,
    pub clean: bool,
    pub keep_packs: bool,
    pub overwrite: bool,
    pub fail_fast: bool,
    pub clean_repos: bool,
    pub skip_srcrpm: bool,
    pub perf: bool,
}

#[derive(Debug, Clone)]
pub struct Config {
    layers: Vec<ConfigLayer>,
    cwd: PathBuf,
    home_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct AuthContext {
    user: Option<String>,
    password: Option<String>,
}

impl Default for AuthContext {
    fn default() -> Self {
        Self {
            user: None,
            password: None,
        }
    }
}

impl LoadOptions {
    pub fn discover(explicit: Option<PathBuf>) -> Result<Self> {
        let cwd = std::env::current_dir().map_err(|err| RgbsError::io(".", err))?;
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| RgbsError::config("HOME is not set"))?;

        Ok(Self {
            explicit,
            cwd,
            home_dir: home_dir.clone(),
            system_path: PathBuf::from("/etc/gbs.conf"),
        })
    }
}

pub fn load(options: &LoadOptions) -> Result<Config> {
    let mut paths = Vec::new();

    if let Some(explicit) = options.explicit.clone() {
        paths.push(explicit);
    }

    if let Some(project) = find_repo_root_gbs_conf(&options.cwd) {
        push_unique(&mut paths, project);
    }

    push_unique(&mut paths, options.cwd.join(".gbs.conf"));
    push_unique(&mut paths, options.home_dir.join(".gbs.conf"));
    push_unique(&mut paths, options.system_path.clone());

    let mut layers = Vec::new();
    for path in paths {
        if path.exists() {
            let text = fs::read_to_string(&path).map_err(|err| RgbsError::io(&path, err))?;
            layers.push(ConfigLayer {
                path: path.clone(),
                doc: parse_ini(&path, &text)?,
            });
        }
    }

    Ok(Config {
        layers,
        cwd: options.cwd.clone(),
        home_dir: options.home_dir.clone(),
    })
}

impl Config {
    pub fn config_files(&self) -> Vec<PathBuf> {
        self.layers.iter().map(|layer| layer.path.clone()).collect()
    }

    pub fn render_debug_config_snapshot(&self, plan: &ResolvedBuildPlan) -> Result<String> {
        let mut out = String::new();
        writeln!(&mut out, "# rgbs merged debug config snapshot").unwrap();
        if !self.layers.is_empty() {
            writeln!(&mut out, "# config files in priority order:").unwrap();
            for path in self.config_files() {
                writeln!(&mut out, "# - {}", path.display()).unwrap();
            }
        }
        writeln!(&mut out).unwrap();

        write_snapshot_section(&mut out, "general", &self.snapshot_general_entries(plan)?)?;

        match plan.profile.kind {
            ProfileKind::Profile => {
                write_snapshot_section(
                    &mut out,
                    &plan.profile.name,
                    &self.snapshot_profile_entries(&plan.profile),
                )?;
                for repo in &plan.profile.repos {
                    write_snapshot_section(&mut out, &repo.name, &snapshot_repo_entries(repo))?;
                }
                if let Some(obs) = &plan.profile.obs {
                    write_snapshot_section(&mut out, &obs.name, &snapshot_obs_entries(obs))?;
                }
            }
            ProfileKind::LegacyBuildSections => {
                write_snapshot_section(
                    &mut out,
                    "build",
                    &snapshot_legacy_build_entries(&plan.profile.repos),
                )?;
                if let Some(obs) = &plan.profile.obs {
                    write_snapshot_section(
                        &mut out,
                        "remotebuild",
                        &snapshot_legacy_remote_build_entries(obs),
                    )?;
                }
            }
        }

        Ok(out)
    }

    pub fn resolve_build_plan(&self, request: &BuildRequest) -> Result<ResolvedBuildPlan> {
        if request.noinit && request.clean {
            return Err(RgbsError::config(
                "--noinit cannot be specified together with --clean",
            ));
        }
        let arch = canonicalize_target_arch(&request.arch).ok_or_else(|| {
            RgbsError::config(format!(
                "unsupported target arch `{}`; supported targets: {}",
                request.arch,
                supported_target_arch_list()
            ))
        })?;

        let profile = self.resolve_profile(request.profile.as_deref())?;
        let buildroot_value = request
            .buildroot
            .clone()
            .or(profile.buildroot.clone())
            .or_else(|| {
                self.merged_get_raw("general", "buildroot")
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| DEFAULT_BUILDROOT.to_string());
        let buildroot = expand_value(self, &buildroot_value)?;
        let work_dir = self.general_value("work_dir", DEFAULT_WORK_DIR)?;
        let packaging_dir = self.general_value("packaging_dir", DEFAULT_PACKAGING_DIR)?;
        let buildconf = match request.dist.as_deref() {
            Some(dist) => Some(expand_value(self, dist)?),
            None => profile.buildconf.clone(),
        };

        let mut repos = profile.repos.clone();
        for repo in &request.repositories {
            repos.push(resolve_cli_repo(repo, &self.home_dir)?);
        }

        if repos.is_empty() && !request.noinit {
            return Err(RgbsError::config("no package repository specified"));
        }

        Ok(ResolvedBuildPlan {
            execution: "prototype_config_resolution".to_string(),
            config_files: self
                .config_files()
                .iter()
                .map(|path| path_to_string(path))
                .collect(),
            git_dir: path_to_string(&request.git_dir),
            arch: arch.to_string(),
            profile,
            buildroot,
            packaging_dir,
            work_dir,
            buildconf,
            repos,
            defines: request.defines.clone(),
            spec: request.spec.as_ref().map(|path| path_to_string(path)),
            include_all: request.include_all,
            noinit: request.noinit,
            clean: request.clean,
            keep_packs: request.keep_packs,
            overwrite: request.overwrite,
            fail_fast: request.fail_fast,
            clean_repos: request.clean_repos,
            skip_srcrpm: request.skip_srcrpm,
            perf: request.perf,
        })
    }

    pub fn resolve_profile(&self, profile_override: Option<&str>) -> Result<ResolvedProfile> {
        if let Some(profile_name) = profile_override {
            let normalized = normalize_profile_name(profile_name);
            return self.resolve_profile_style(&normalized);
        }

        if let Some(profile_name) = self.merged_get_raw("general", "profile") {
            return self.resolve_profile_style(&normalize_profile_name(profile_name));
        }

        self.resolve_legacy_build_sections()
    }

    fn resolve_profile_style(&self, name: &str) -> Result<ResolvedProfile> {
        if !name.starts_with("profile.") {
            return Err(RgbsError::config(format!(
                "general.profile must start with `profile.`: {name}"
            )));
        }
        if !self.has_section(name) {
            return Err(RgbsError::config(format!("no such section: {name}")));
        }

        let general_auth = AuthContext {
            user: self.section_user("general")?,
            password: self.section_password("general")?,
        };
        let profile_auth = AuthContext {
            user: self.section_user(name)?.or(general_auth.user.clone()),
            password: self
                .section_password(name)?
                .or(general_auth.password.clone()),
        };

        let mut repos = Vec::new();
        if let Some(repo_list) = self.merged_get_raw(name, "repos") {
            for repo_name in split_csv(repo_list) {
                if !repo_name.starts_with("repo.") {
                    continue;
                }
                repos.push(self.resolve_repo_section(&repo_name, "config", &profile_auth)?);
            }
        }

        let obs = if let Some(obs_name) = self.merged_get_raw(name, "obs") {
            Some(self.resolve_obs_section(obs_name, &profile_auth)?)
        } else {
            None
        };

        Ok(ResolvedProfile {
            kind: ProfileKind::Profile,
            name: name.to_string(),
            repos,
            buildroot: self
                .merged_get_raw(name, "buildroot")
                .map(|value| expand_value(self, value))
                .transpose()?,
            buildconf: self
                .merged_get_raw(name, "buildconf")
                .map(|value| expand_value(self, value))
                .transpose()?,
            exclude_packages: self
                .merged_get_raw(name, "exclude_packages")
                .map(split_csv)
                .unwrap_or_default(),
            obs,
            source: self.merged_get_raw(name, "source").map(ToOwned::to_owned),
            depends: self.merged_get_raw(name, "depends").map(ToOwned::to_owned),
            pkgs: self.merged_get_raw(name, "pkgs").map(ToOwned::to_owned),
            common_user: profile_auth.user,
            common_password: profile_auth.password,
        })
    }

    fn resolve_legacy_build_sections(&self) -> Result<ResolvedProfile> {
        let general_auth = AuthContext {
            user: self.section_user("general")?,
            password: self.section_password("general")?,
        };

        let mut grouped = BTreeMap::<String, IndexMap<String, String>>::new();
        for option in self.merged_options("build") {
            if !option.starts_with("repo") {
                continue;
            }
            let (repo_key, field) = option
                .split_once('.')
                .ok_or_else(|| RgbsError::config(format!("invalid repo option: {option}")))?;
            if field != "url" && field != "user" && field != "passwd" && field != "passwdx" {
                return Err(RgbsError::config(format!("invalid repo option: {option}")));
            }
            if let Some(value) = self.merged_get_raw("build", &option) {
                grouped
                    .entry(repo_key.to_string())
                    .or_default()
                    .insert(field.to_string(), value.to_string());
            }
        }

        let mut repos = Vec::new();
        for (repo_key, fields) in grouped {
            let section_name = format!("repo.{repo_key}");
            let raw_url = fields
                .get("url")
                .ok_or_else(|| RgbsError::config(format!("missing `{repo_key}.url` in [build]")))?;
            let raw_url = expand_value(self, raw_url)?;
            let auth = AuthContext {
                user: fields.get("user").cloned().or(general_auth.user.clone()),
                password: legacy_repo_password(&fields)?.or(general_auth.password.clone()),
            };
            repos.push(resolve_endpoint(
                &section_name,
                &raw_url,
                &auth,
                &self.home_dir,
                "config",
            )?);
        }

        let obs = if let Some(server) = self.merged_get_raw("remotebuild", "build_server") {
            let server = expand_value(self, server)?;
            let auth = AuthContext {
                user: self.section_user("remotebuild")?,
                password: self.section_password("remotebuild")?,
            };
            let endpoint =
                resolve_endpoint("obs.remotebuild", &server, &auth, &self.home_dir, "config")?;
            Some(ResolvedObs {
                name: endpoint.name,
                location: endpoint.location,
                raw_location: endpoint.raw_location,
                user: endpoint.user,
                authenticated: endpoint.authenticated,
                base_project: self
                    .merged_get_raw("remotebuild", "base_prj")
                    .map(ToOwned::to_owned),
                target_project: self
                    .merged_get_raw("remotebuild", "target_prj")
                    .map(ToOwned::to_owned),
                password: endpoint.password,
            })
        } else {
            None
        };

        Ok(ResolvedProfile {
            kind: ProfileKind::LegacyBuildSections,
            name: "profile.current".to_string(),
            repos,
            buildroot: None,
            buildconf: None,
            exclude_packages: Vec::new(),
            obs,
            source: None,
            depends: None,
            pkgs: None,
            common_user: None,
            common_password: None,
        })
    }

    fn resolve_repo_section(
        &self,
        name: &str,
        source: &str,
        parent_auth: &AuthContext,
    ) -> Result<ResolvedRepo> {
        if !self.has_section(name) {
            return Err(RgbsError::config(format!("no such section: {name}")));
        }
        let raw_url = self
            .merged_get_raw(name, "url")
            .ok_or_else(|| RgbsError::config(format!("missing `url` in section {name}")))?;
        let raw_url = expand_value(self, raw_url)?;
        resolve_endpoint(
            name,
            &raw_url,
            &self.section_auth(name, parent_auth)?,
            &self.home_dir,
            source,
        )
    }

    fn resolve_obs_section(&self, name: &str, parent_auth: &AuthContext) -> Result<ResolvedObs> {
        if !name.starts_with("obs.") {
            return Err(RgbsError::config(format!(
                "obs section name should start with `obs.`: {name}"
            )));
        }
        let raw_url = self
            .merged_get_raw(name, "url")
            .ok_or_else(|| RgbsError::config(format!("missing `url` in section {name}")))?;
        let raw_url = expand_value(self, raw_url)?;
        let endpoint = resolve_endpoint(
            name,
            &raw_url,
            &self.section_auth(name, parent_auth)?,
            &self.home_dir,
            "config",
        )?;
        Ok(ResolvedObs {
            name: endpoint.name,
            location: endpoint.location,
            raw_location: endpoint.raw_location,
            user: endpoint.user,
            authenticated: endpoint.authenticated,
            base_project: self.merged_get_raw(name, "base_prj").map(ToOwned::to_owned),
            target_project: self
                .merged_get_raw(name, "target_prj")
                .map(ToOwned::to_owned),
            password: endpoint.password,
        })
    }

    fn section_auth(&self, section: &str, parent_auth: &AuthContext) -> Result<AuthContext> {
        Ok(AuthContext {
            user: self.section_user(section)?.or(parent_auth.user.clone()),
            password: self
                .section_password(section)?
                .or(parent_auth.password.clone()),
        })
    }

    fn section_user(&self, section: &str) -> Result<Option<String>> {
        match self.merged_get_raw(section, "user") {
            Some(value) => {
                let resolved = self.expand_auth_tokens(value)?;
                if resolved.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(resolved))
                }
            }
            None => Ok(None),
        }
    }

    fn section_password(&self, section: &str) -> Result<Option<String>> {
        if let Some(value) = self.merged_get_raw(section, "passwd") {
            let resolved = self.expand_auth_tokens(value)?;
            if !resolved.is_empty() {
                return Ok(Some(resolved));
            }
        }
        if let Some(value) = self.merged_get_raw(section, "passwdx") {
            return Ok(Some(decode_passwdx(value)?));
        }
        Ok(None)
    }

    fn expand_auth_tokens(&self, value: &str) -> Result<String> {
        let general_user = self.merged_get_raw("general", "user").unwrap_or_default();
        let general_pass = self
            .section_password("general")?
            .unwrap_or_default()
            .to_string();

        Ok(value
            .replace("${user}", general_user)
            .replace("${passwd}", &general_pass))
    }

    fn general_value(&self, key: &str, default: &str) -> Result<String> {
        let raw = self.merged_get_raw("general", key).unwrap_or(default);
        if key == "work_dir" && raw == DEFAULT_WORK_DIR {
            return Ok(path_to_string(&self.cwd));
        }
        expand_value(self, raw)
    }

    fn merged_get_raw(&self, section: &str, option: &str) -> Option<&str> {
        for layer in &self.layers {
            if let Some(value) = layer.doc.get(section, option) {
                return Some(value);
            }
        }
        default_value(section, option)
    }

    fn merged_options(&self, section: &str) -> IndexSet<String> {
        let mut options = IndexSet::new();
        for layer in &self.layers {
            if let Some(keys) = layer.doc.options(section) {
                options.extend(keys.into_iter().cloned());
            }
        }
        if let Some(defaults) = default_section(section) {
            options.extend(defaults.keys().cloned());
        }
        options
    }

    fn has_section(&self, section: &str) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.doc.has_section(section))
            || default_section(section).is_some()
    }

    fn snapshot_general_entries(&self, plan: &ResolvedBuildPlan) -> Result<Vec<(String, String)>> {
        let mut entries = Vec::new();
        if matches!(plan.profile.kind, ProfileKind::Profile)
            || self.merged_get_raw("general", "profile").is_some()
        {
            entries.push(("profile".to_string(), plan.profile.name.clone()));
        }
        entries.push((
            "tmpdir".to_string(),
            self.general_value("tmpdir", DEFAULT_TMPDIR)?,
        ));
        entries.push(("buildroot".to_string(), plan.buildroot.clone()));
        entries.push(("packaging_dir".to_string(), plan.packaging_dir.clone()));
        entries.push(("work_dir".to_string(), plan.work_dir.clone()));
        if let Some(user) = self.section_user("general")? {
            entries.push(("user".to_string(), user));
        }
        if self.section_password("general")?.is_some() {
            entries.push(("passwd".to_string(), "******".to_string()));
        }
        Ok(entries)
    }

    fn snapshot_profile_entries(&self, profile: &ResolvedProfile) -> Vec<(String, String)> {
        let mut entries = Vec::new();
        if !profile.repos.is_empty() {
            entries.push((
                "repos".to_string(),
                profile
                    .repos
                    .iter()
                    .map(|repo| repo.name.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            ));
        }
        if let Some(obs) = &profile.obs {
            entries.push(("obs".to_string(), obs.name.clone()));
        }
        if let Some(buildroot) = &profile.buildroot {
            entries.push(("buildroot".to_string(), buildroot.clone()));
        }
        if let Some(buildconf) = &profile.buildconf {
            entries.push(("buildconf".to_string(), buildconf.clone()));
        }
        if !profile.exclude_packages.is_empty() {
            entries.push((
                "exclude_packages".to_string(),
                profile.exclude_packages.join(", "),
            ));
        }
        if let Some(source) = &profile.source {
            entries.push(("source".to_string(), source.clone()));
        }
        if let Some(depends) = &profile.depends {
            entries.push(("depends".to_string(), depends.clone()));
        }
        if let Some(pkgs) = &profile.pkgs {
            entries.push(("pkgs".to_string(), pkgs.clone()));
        }
        if let Some(user) = &profile.common_user {
            entries.push(("user".to_string(), user.clone()));
        }
        if profile.common_password.is_some() {
            entries.push(("passwd".to_string(), "******".to_string()));
        }
        entries
    }
}

fn snapshot_repo_entries(repo: &ResolvedRepo) -> Vec<(String, String)> {
    let mut entries = vec![("url".to_string(), repo.location.clone())];
    if let Some(user) = &repo.user {
        entries.push(("user".to_string(), user.clone()));
    }
    if repo.password.is_some() {
        entries.push(("passwd".to_string(), "******".to_string()));
    }
    entries
}

fn snapshot_obs_entries(obs: &ResolvedObs) -> Vec<(String, String)> {
    let mut entries = vec![("url".to_string(), obs.location.clone())];
    if let Some(user) = &obs.user {
        entries.push(("user".to_string(), user.clone()));
    }
    if obs.password.is_some() {
        entries.push(("passwd".to_string(), "******".to_string()));
    }
    if let Some(base_project) = &obs.base_project {
        entries.push(("base_prj".to_string(), base_project.clone()));
    }
    if let Some(target_project) = &obs.target_project {
        entries.push(("target_prj".to_string(), target_project.clone()));
    }
    entries
}

fn snapshot_legacy_build_entries(repos: &[ResolvedRepo]) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for repo in repos {
        let key = repo.name.strip_prefix("repo.").unwrap_or(&repo.name);
        entries.push((format!("{key}.url"), repo.location.clone()));
        if let Some(user) = &repo.user {
            entries.push((format!("{key}.user"), user.clone()));
        }
        if repo.password.is_some() {
            entries.push((format!("{key}.passwd"), "******".to_string()));
        }
    }
    entries
}

fn snapshot_legacy_remote_build_entries(obs: &ResolvedObs) -> Vec<(String, String)> {
    let mut entries = vec![("build_server".to_string(), obs.location.clone())];
    if let Some(user) = &obs.user {
        entries.push(("user".to_string(), user.clone()));
    }
    if obs.password.is_some() {
        entries.push(("passwd".to_string(), "******".to_string()));
    }
    if let Some(base_project) = &obs.base_project {
        entries.push(("base_prj".to_string(), base_project.clone()));
    }
    if let Some(target_project) = &obs.target_project {
        entries.push(("target_prj".to_string(), target_project.clone()));
    }
    entries
}

fn write_snapshot_section(
    out: &mut String,
    section: &str,
    entries: &[(String, String)],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    writeln!(out, "[{section}]")
        .map_err(|err| RgbsError::message(format!("render config snapshot section: {err}")))?;
    for (key, value) in entries {
        writeln!(out, "{key} = {value}")
            .map_err(|err| RgbsError::message(format!("render config snapshot entry: {err}")))?;
    }
    writeln!(out)
        .map_err(|err| RgbsError::message(format!("render config snapshot spacing: {err}")))?;
    Ok(())
}

fn default_section(section: &str) -> Option<IndexMap<String, String>> {
    if section == "general" {
        return Some(IndexMap::from([
            ("tmpdir".to_string(), DEFAULT_TMPDIR.to_string()),
            ("buildroot".to_string(), DEFAULT_BUILDROOT.to_string()),
            (
                "packaging_dir".to_string(),
                DEFAULT_PACKAGING_DIR.to_string(),
            ),
            ("work_dir".to_string(), DEFAULT_WORK_DIR.to_string()),
        ]));
    }
    None
}

fn default_value(section: &str, option: &str) -> Option<&'static str> {
    if section == "general" {
        match option {
            "tmpdir" => Some(DEFAULT_TMPDIR),
            "buildroot" => Some(DEFAULT_BUILDROOT),
            "packaging_dir" => Some(DEFAULT_PACKAGING_DIR),
            "work_dir" => Some(DEFAULT_WORK_DIR),
            _ => None,
        }
    } else {
        None
    }
}

fn parse_ini(path: &Path, text: &str) -> Result<IniDocument> {
    let mut sections = IndexMap::<String, IndexMap<String, String>>::new();
    let mut current_section: Option<String> = None;

    for (index, raw_line) in text.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with(';')
            || (raw_line.starts_with("rem ") || raw_line.eq_ignore_ascii_case("rem"))
        {
            continue;
        }

        if trimmed.starts_with('[') {
            if !trimmed.ends_with(']') {
                return Err(RgbsError::config(format!(
                    "{}:{}: malformed section header",
                    path.display(),
                    index + 1
                )));
            }
            let name = trimmed[1..trimmed.len() - 1].trim().to_ascii_lowercase();
            sections.entry(name.clone()).or_default();
            current_section = Some(name);
            continue;
        }

        let section = current_section.clone().ok_or_else(|| {
            RgbsError::config(format!(
                "{}:{}: missing section header before key/value pair",
                path.display(),
                index + 1
            ))
        })?;

        let (key, value) = raw_line
            .split_once('=')
            .or_else(|| raw_line.split_once(':'))
            .ok_or_else(|| {
                RgbsError::config(format!(
                    "{}:{}: invalid key/value syntax",
                    path.display(),
                    index + 1
                ))
            })?;

        let key = key.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        sections.entry(section).or_default().insert(key, value);
    }

    Ok(IniDocument { sections })
}

impl IniDocument {
    fn get(&self, section: &str, option: &str) -> Option<&str> {
        self.sections
            .get(&section.to_ascii_lowercase())
            .and_then(|options| options.get(&option.to_ascii_lowercase()))
            .map(String::as_str)
    }

    fn options(&self, section: &str) -> Option<impl Iterator<Item = &String>> {
        self.sections
            .get(&section.to_ascii_lowercase())
            .map(|options| options.keys())
    }

    fn has_section(&self, section: &str) -> bool {
        self.sections.contains_key(&section.to_ascii_lowercase())
    }
}

fn decode_passwdx(passwdx: &str) -> Result<String> {
    let decoded = BASE64_STANDARD
        .decode(passwdx)
        .map_err(|err| RgbsError::config(format!("passwdx:{err}")))?;
    let mut decoder = BzDecoder::new(decoded.as_slice());
    let mut out = String::new();
    decoder
        .read_to_string(&mut out)
        .map_err(|err| RgbsError::config(format!("passwdx:{err}")))?;
    Ok(out)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn expand_value(config: &Config, raw: &str) -> Result<String> {
    let work_dir = match config
        .merged_get_raw("general", "work_dir")
        .unwrap_or(DEFAULT_WORK_DIR)
    {
        "." => path_to_string(&config.cwd),
        other => path_to_string(&expand_tilde(other, &config.home_dir)),
    };

    let replacements = [
        (
            "tmpdir",
            config
                .merged_get_raw("general", "tmpdir")
                .unwrap_or(DEFAULT_TMPDIR),
        ),
        (
            "buildroot",
            config
                .merged_get_raw("general", "buildroot")
                .unwrap_or(DEFAULT_BUILDROOT),
        ),
        (
            "packaging_dir",
            config
                .merged_get_raw("general", "packaging_dir")
                .unwrap_or(DEFAULT_PACKAGING_DIR),
        ),
        ("work_dir", &work_dir),
    ];

    let mut out = raw.to_string();
    let mut seen = HashSet::new();
    loop {
        let next = expand_percent_tokens(config, &out)?;
        let next = replacements.iter().fold(next, |acc, (key, value)| {
            acc.replace(&format!("${{{key}}}"), value)
        });
        if next == out {
            break;
        }
        if !seen.insert(out.clone()) {
            return Err(RgbsError::config(format!(
                "cyclic interpolation detected in `{raw}`"
            )));
        }
        out = next;
    }

    Ok(path_to_string(&expand_tilde(&out, &config.home_dir)))
}

fn expand_percent_tokens(config: &Config, raw: &str) -> Result<String> {
    let mut out = raw.to_string();
    let mut cursor = 0usize;
    while let Some(start_rel) = out[cursor..].find("%(") {
        let start = cursor + start_rel;
        let tail = &out[start + 2..];
        let end_rel = tail.find(")s").ok_or_else(|| {
            RgbsError::config(format!("unterminated interpolation token in `{raw}`"))
        })?;
        let token = &tail[..end_rel];
        let replacement = config
            .merged_get_raw("general", token)
            .or_else(|| config.merged_get_raw("remote", token))
            .ok_or_else(|| RgbsError::config(format!("unknown interpolation key `{token}`")))?;
        let before = out[..start].to_string();
        let after = out[start + 2 + end_rel + 2..].to_string();
        out = format!("{before}{replacement}{after}");
        cursor = before.len() + replacement.len();
    }
    Ok(out)
}

fn legacy_repo_password(fields: &IndexMap<String, String>) -> Result<Option<String>> {
    if let Some(passwd) = fields.get("passwd") {
        if !passwd.is_empty() {
            return Ok(Some(passwd.clone()));
        }
    }
    if let Some(passwdx) = fields.get("passwdx") {
        return Ok(Some(decode_passwdx(passwdx)?));
    }
    Ok(None)
}

fn resolve_cli_repo(raw: &str, home_dir: &Path) -> Result<ResolvedRepo> {
    let normalized = normalize_pathish(raw, home_dir);
    if let Ok(url) = Url::parse(&normalized) {
        return Ok(ResolvedRepo {
            name: normalized.clone(),
            kind: RepoKind::Remote,
            location: redact_url(&url),
            raw_location: normalized.clone(),
            source: "cli".to_string(),
            user: if url.username().is_empty() {
                None
            } else {
                Some(url.username().to_string())
            },
            authenticated: !url.username().is_empty() || url.password().is_some(),
            password: url.password().map(ToOwned::to_owned),
        });
    }

    Ok(ResolvedRepo {
        name: normalized.clone(),
        kind: RepoKind::LocalPath,
        location: normalized.clone(),
        raw_location: normalized,
        source: "cli".to_string(),
        user: None,
        authenticated: false,
        password: None,
    })
}

fn resolve_endpoint(
    name: &str,
    raw_url: &str,
    auth: &AuthContext,
    home_dir: &Path,
    source: &str,
) -> Result<ResolvedRepo> {
    let normalized = normalize_pathish(raw_url, home_dir);
    if let Ok(mut url) = Url::parse(&normalized) {
        if url.username().is_empty() && url.password().is_none() {
            if let Some(user) = &auth.user {
                url.set_username(user).map_err(|_| RgbsError::InvalidUrl {
                    url: normalized.clone(),
                    message: "failed to set username".to_string(),
                })?;
            }
            if let Some(password) = &auth.password {
                if auth.user.is_none() {
                    return Err(RgbsError::config(format!(
                        "password specified without username for remote url `{normalized}`"
                    )));
                }
                url.set_password(Some(password))
                    .map_err(|_| RgbsError::InvalidUrl {
                        url: normalized.clone(),
                        message: "failed to set password".to_string(),
                    })?;
            }
        }

        return Ok(ResolvedRepo {
            name: name.to_string(),
            kind: RepoKind::Remote,
            location: redact_url(&url),
            raw_location: url.to_string(),
            source: source.to_string(),
            user: if url.username().is_empty() {
                None
            } else {
                Some(url.username().to_string())
            },
            authenticated: !url.username().is_empty() || url.password().is_some(),
            password: url.password().map(ToOwned::to_owned),
        });
    }

    Ok(ResolvedRepo {
        name: name.to_string(),
        kind: RepoKind::LocalPath,
        location: normalized.clone(),
        raw_location: normalized,
        source: source.to_string(),
        user: None,
        authenticated: false,
        password: None,
    })
}

fn normalize_profile_name(profile_name: &str) -> String {
    if profile_name.starts_with("profile.") {
        profile_name.to_ascii_lowercase()
    } else {
        format!("profile.{}", profile_name.to_ascii_lowercase())
    }
}

fn normalize_pathish(raw: &str, home_dir: &Path) -> String {
    let expanded = expand_tilde(raw, home_dir);
    if raw.contains("://") {
        return raw.to_string();
    }
    if expanded.exists() {
        return path_to_string(&expanded.canonicalize().unwrap_or(expanded));
    }
    path_to_string(&expanded)
}

fn redact_url(url: &Url) -> String {
    let mut redacted = url.clone();
    if redacted.password().is_some() {
        let _ = redacted.set_password(Some("******"));
    }
    redacted.to_string()
}

fn find_repo_root_gbs_conf(start_dir: &Path) -> Option<PathBuf> {
    let mut current = start_dir.to_path_buf();
    loop {
        let candidate = current.join(".gbs.conf");
        if current.join(".repo").exists() && candidate.exists() {
            return Some(candidate);
        }
        if !current.pop() {
            return None;
        }
    }
}

fn push_unique(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const PASSWDX_SECRET: &str = "QlpoOTFBWSZTWYfNdxYAAAIBgAoAHAAgADDNAMNEA24u5IpwoSEPmu4s";

    #[test]
    fn resolves_profile_oriented_config() {
        let fixture = r#"
[general]
profile = profile.tz

[profile.tz]
user = Alice
passwdx = QlpoOTFBWSZTWYfNdxYAAAIBgAoAHAAgADDNAMNEA24u5IpwoSEPmu4s
repos = repo.ia32_main, repo.ia32_non-oss, repo.ia32_base, repo.local
obs = obs.tz

[obs.tz]
url = https://api.tz/path
base_prj = base
target_prj = target

[repo.ia32_main]
url = https://repo/ia32/main

[repo.ia32_non-oss]
url = https://repo/ia32/non-oss

[repo.ia32_base]
url = https://repo/ia32/base
user = Bob
passwdx = QlpoOTFBWSZTWRwZil4AAACBgC8kCAAgADEMCCAPKGaQLT4u5IpwoSA4MxS8

[repo.local]
url = /local/path
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let profile = config.resolve_profile(None).unwrap();

        assert_eq!(profile.kind, ProfileKind::Profile);
        assert_eq!(profile.name, "profile.tz");
        assert_eq!(profile.repos.len(), 4);
        assert_eq!(profile.repos[0].user.as_deref(), Some("Alice"));
        assert_eq!(profile.repos[0].password.as_deref(), Some("secret"));
        assert_eq!(profile.repos[2].user.as_deref(), Some("Bob"));
        assert_eq!(profile.repos[2].password.as_deref(), Some("classified"));
        assert_eq!(profile.repos[3].kind, RepoKind::LocalPath);
        assert!(!profile.repos[3].authenticated);
        assert_eq!(
            profile.obs.as_ref().unwrap().location,
            "https://Alice:******@api.tz/path"
        );
    }

    #[test]
    fn resolves_legacy_subcommand_style_config() {
        let fixture = r#"
[remotebuild]
build_server = https://api/build/server
user = Alice
passwdx = QlpoOTFBWSZTWYfNdxYAAAIBgAoAHAAgADDNAMNEA24u5IpwoSEPmu4s
base_prj = Main
target_prj = Target

[build]
repo1.url = https://repo1/path
repo1.user = Alice
repo1.passwdx = QlpoOTFBWSZTWYfNdxYAAAIBgAoAHAAgADDNAMNEA24u5IpwoSEPmu4s
repo2.url = https://repo2/path
repo2.user = Alice
repo2.passwdx = QlpoOTFBWSZTWYfNdxYAAAIBgAoAHAAgADDNAMNEA24u5IpwoSEPmu4s
repo3.url = /local/path/repo
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let profile = config.resolve_profile(None).unwrap();

        assert_eq!(profile.kind, ProfileKind::LegacyBuildSections);
        assert_eq!(profile.name, "profile.current");
        assert_eq!(profile.repos.len(), 3);
        assert_eq!(profile.repos[0].location, "https://Alice:******@repo1/path");
        assert_eq!(profile.repos[1].location, "https://Alice:******@repo2/path");
        assert_eq!(profile.repos[2].kind, RepoKind::LocalPath);
        assert_eq!(
            profile.obs.as_ref().unwrap().base_project.as_deref(),
            Some("Main")
        );
    }

    #[test]
    fn merges_project_over_home_and_defaults() {
        let home = format!(
            r#"
[general]
profile = profile.demo
buildroot = ~/custom-root

[profile.demo]
repos = repo.home

[repo.home]
url = https://repo/home
"#
        );
        let project = r#"
[general]
packaging_dir = packaging-project

[profile.demo]
buildconf = ${work_dir}/build.conf
"#;
        let harness = TestHarness::new();
        harness.write_home(&home);
        harness.write_project(project);

        let config = load(&harness.options()).unwrap();
        let plan = config
            .resolve_build_plan(&BuildRequest {
                git_dir: harness.cwd.clone(),
                arch: "aarch64".to_string(),
                profile: None,
                repositories: Vec::new(),
                dist: None,
                buildroot: None,
                defines: Vec::new(),
                spec: None,
                include_all: false,
                noinit: false,
                clean: false,
                keep_packs: false,
                overwrite: false,
                fail_fast: false,
                clean_repos: false,
                skip_srcrpm: false,
                perf: true,
            })
            .unwrap();

        assert_eq!(plan.packaging_dir, "packaging-project");
        assert_eq!(
            plan.buildroot,
            path_to_string(&harness.home_dir.join("custom-root"))
        );
        assert_eq!(plan.work_dir, path_to_string(&harness.cwd));
        let expected_buildconf = path_to_string(&harness.cwd.join("build.conf"));
        assert_eq!(plan.buildconf.as_deref(), Some(expected_buildconf.as_str()));
        assert!(plan.perf);
    }

    #[test]
    fn renders_redacted_profile_config_snapshot() {
        let fixture = format!(
            r#"
[general]
profile = profile.demo
user = Alice
passwdx = {PASSWDX_SECRET}

[profile.demo]
repos = repo.demo
obs = obs.demo
buildconf = ${{work_dir}}/build.conf

[repo.demo]
url = https://repo/demo

[obs.demo]
url = https://api/demo
target_prj = DemoTarget
"#
        );
        let harness = TestHarness::new();
        harness.write_home(&fixture);

        let config = load(&harness.options()).unwrap();
        let plan = config
            .resolve_build_plan(&BuildRequest {
                git_dir: harness.cwd.clone(),
                arch: "aarch64".to_string(),
                profile: None,
                repositories: Vec::new(),
                dist: None,
                buildroot: None,
                defines: Vec::new(),
                spec: None,
                include_all: false,
                noinit: false,
                clean: false,
                keep_packs: false,
                overwrite: false,
                fail_fast: false,
                clean_repos: false,
                skip_srcrpm: false,
                perf: false,
            })
            .unwrap();
        let snapshot = config.render_debug_config_snapshot(&plan).unwrap();

        assert!(snapshot.contains("[general]"));
        assert!(snapshot.contains("profile = profile.demo"));
        assert!(snapshot.contains("passwd = ******"));
        assert!(snapshot.contains("[profile.demo]"));
        assert!(snapshot.contains("repos = repo.demo"));
        assert!(snapshot.contains("[repo.demo]"));
        assert!(snapshot.contains("url = https://Alice:******@repo/demo"));
        assert!(snapshot.contains("[obs.demo]"));
        assert!(snapshot.contains("target_prj = DemoTarget"));
        assert!(!snapshot.contains("secret"));
    }

    #[test]
    fn renders_redacted_legacy_config_snapshot() {
        let fixture = format!(
            r#"
[remotebuild]
build_server = https://api/build/server
user = Alice
passwdx = {PASSWDX_SECRET}
base_prj = Main

[build]
repo1.url = https://repo1/path
repo1.user = Alice
repo1.passwdx = {PASSWDX_SECRET}
repo2.url = /local/path/repo
"#
        );
        let harness = TestHarness::new();
        harness.write_home(&fixture);

        let config = load(&harness.options()).unwrap();
        let plan = config
            .resolve_build_plan(&BuildRequest {
                git_dir: harness.cwd.clone(),
                arch: "armv7l".to_string(),
                profile: None,
                repositories: Vec::new(),
                dist: None,
                buildroot: None,
                defines: Vec::new(),
                spec: None,
                include_all: false,
                noinit: false,
                clean: false,
                keep_packs: false,
                overwrite: false,
                fail_fast: false,
                clean_repos: false,
                skip_srcrpm: false,
                perf: false,
            })
            .unwrap();
        let snapshot = config.render_debug_config_snapshot(&plan).unwrap();

        assert!(snapshot.contains("[build]"));
        assert!(snapshot.contains("repo1.url = https://Alice:******@repo1/path"));
        assert!(snapshot.contains("repo1.passwd = ******"));
        assert!(snapshot.contains("[remotebuild]"));
        assert!(snapshot.contains("build_server = https://Alice:******@api/build/server"));
        assert!(snapshot.contains("base_prj = Main"));
        assert!(!snapshot.contains("secret"));
    }

    #[test]
    fn bad_passwdx_fails_cleanly() {
        let fixture = r#"
[general]
profile = profile.demo

[profile.demo]
repos = repo.demo

[repo.demo]
url = https://repo/demo
passwdx = not-valid-base64
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let err = config.resolve_profile(None).unwrap_err().to_string();
        assert!(err.contains("passwdx"));
    }

    #[test]
    fn passwd_without_user_for_remote_repo_is_rejected() {
        let fixture = format!(
            r#"
[general]
profile = profile.demo

[profile.demo]
repos = repo.demo

[repo.demo]
url = https://repo/demo
passwdx = {PASSWDX_SECRET}
"#
        );
        let harness = TestHarness::new();
        harness.write_home(&fixture);

        let config = load(&harness.options()).unwrap();
        let err = config.resolve_profile(None).unwrap_err().to_string();
        assert!(err.contains("password specified without username"));
    }

    #[test]
    fn supports_percent_interpolation() {
        let fixture = r#"
[general]
profile = profile.demo
base = abc

[profile.demo]
repos = repo.demo

[repo.demo]
url = %(base)s/def
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let profile = config.resolve_profile(None).unwrap();
        assert_eq!(profile.repos[0].location, "abc/def");
    }

    #[test]
    fn rejects_noinit_with_clean() {
        let fixture = r#"
[general]
profile = profile.demo

[profile.demo]
repos = repo.demo

[repo.demo]
url = https://repo/demo
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let err = config
            .resolve_build_plan(&BuildRequest {
                git_dir: harness.cwd.clone(),
                arch: "aarch64".to_string(),
                profile: None,
                repositories: Vec::new(),
                dist: None,
                buildroot: None,
                defines: Vec::new(),
                spec: None,
                include_all: false,
                noinit: true,
                clean: true,
                keep_packs: false,
                overwrite: false,
                fail_fast: false,
                clean_repos: false,
                skip_srcrpm: false,
                perf: false,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("--noinit"));
        assert!(err.contains("--clean"));
    }

    #[test]
    fn canonicalizes_supported_target_arch_aliases() {
        let fixture = r#"
[general]
profile = profile.demo

[profile.demo]
repos = repo.demo

[repo.demo]
url = https://repo/demo
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let plan = config
            .resolve_build_plan(&BuildRequest {
                git_dir: harness.cwd.clone(),
                arch: "armhf".to_string(),
                profile: None,
                repositories: Vec::new(),
                dist: None,
                buildroot: None,
                defines: Vec::new(),
                spec: None,
                include_all: false,
                noinit: false,
                clean: false,
                keep_packs: false,
                overwrite: false,
                fail_fast: false,
                clean_repos: false,
                skip_srcrpm: false,
                perf: false,
            })
            .unwrap();

        assert_eq!(plan.arch, "armv7l");
    }

    #[test]
    fn rejects_unsupported_target_arches() {
        let fixture = r#"
[general]
profile = profile.demo

[profile.demo]
repos = repo.demo

[repo.demo]
url = https://repo/demo
"#;
        let harness = TestHarness::new();
        harness.write_home(fixture);

        let config = load(&harness.options()).unwrap();
        let err = config
            .resolve_build_plan(&BuildRequest {
                git_dir: harness.cwd.clone(),
                arch: "x86_64".to_string(),
                profile: None,
                repositories: Vec::new(),
                dist: None,
                buildroot: None,
                defines: Vec::new(),
                spec: None,
                include_all: false,
                noinit: false,
                clean: false,
                keep_packs: false,
                overwrite: false,
                fail_fast: false,
                clean_repos: false,
                skip_srcrpm: false,
                perf: false,
            })
            .unwrap_err()
            .to_string();

        assert!(err.contains("unsupported target arch"));
        assert!(err.contains("armv7l"));
        assert!(err.contains("aarch64"));
    }

    struct TestHarness {
        _temp_dir: TempDir,
        cwd: PathBuf,
        home_dir: PathBuf,
        system_path: PathBuf,
    }

    impl TestHarness {
        fn new() -> Self {
            let temp_dir = TempDir::new().unwrap();
            let cwd = temp_dir.path().join("project");
            let home_dir = temp_dir.path().join("home");
            let system_path = temp_dir.path().join("etc").join("gbs.conf");
            fs::create_dir_all(&cwd).unwrap();
            fs::create_dir_all(&home_dir).unwrap();
            fs::create_dir_all(system_path.parent().unwrap()).unwrap();

            Self {
                _temp_dir: temp_dir,
                cwd,
                home_dir,
                system_path,
            }
        }

        fn options(&self) -> LoadOptions {
            LoadOptions {
                explicit: None,
                cwd: self.cwd.clone(),
                home_dir: self.home_dir.clone(),
                system_path: self.system_path.clone(),
            }
        }

        fn write_home(&self, content: &str) {
            fs::write(self.home_dir.join(".gbs.conf"), content).unwrap();
        }

        fn write_project(&self, content: &str) {
            fs::write(self.cwd.join(".gbs.conf"), content).unwrap();
        }
    }
}
