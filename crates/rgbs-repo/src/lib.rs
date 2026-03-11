use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use rgbs_common::{
    Result, RgbsError, atomic_write, cache_root, ensure_dir, path_to_string, sha256_hex,
};
use rgbs_config::{RepoKind, ResolvedRepo};
use roxmltree::{Document, Node};
use serde::Serialize;
use url::Url;
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub arch: String,
    pub repos: Vec<ResolvedRepo>,
    pub explicit_buildconf: Option<String>,
    pub clean_cache: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryLayout {
    Standard,
    LegacyBuildXml,
}

#[derive(Debug, Clone, Serialize)]
pub struct Capability {
    pub name: String,
    pub flags: Option<String>,
    pub version: Option<String>,
    pub preinstall: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackageRecord {
    pub name: String,
    pub arch: String,
    pub epoch: Option<String>,
    pub version: String,
    pub release: String,
    pub checksum: Option<String>,
    pub location: String,
    pub repo_name: String,
    pub repo_location: String,
    #[serde(skip_serializing)]
    pub repo_location_raw: String,
    #[serde(skip_serializing)]
    pub repo_priority: usize,
    pub provides: Vec<Capability>,
    pub requires: Vec<Capability>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedRepository {
    pub name: String,
    pub location: String,
    pub layout: RepositoryLayout,
    pub repomd_checksum: Option<String>,
    pub primary_cache_path: String,
    pub buildconf_cache_path: Option<String>,
    pub package_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepositoryState {
    pub cache_root: String,
    pub fingerprint: String,
    pub buildconf: Option<String>,
    pub repositories: Vec<ResolvedRepository>,
    pub package_count: usize,
    pub packages: Vec<PackageRecord>,
}

#[derive(Debug, Clone)]
struct RepoSource {
    name: String,
    display_location: String,
    raw_location: String,
    kind: RepoKind,
}

#[derive(Debug, Clone)]
struct StandardRepoData {
    repository: ResolvedRepository,
    packages: Vec<PackageRecord>,
    buildconf: Option<PathBuf>,
    fingerprint_parts: Vec<String>,
}

#[derive(Debug, Default)]
struct LegacyBuildMeta {
    buildconf: Option<String>,
    repos: Vec<String>,
    archs: Vec<String>,
    id: Option<String>,
}

#[derive(Debug, Default)]
struct RepomdData {
    primary_href: Option<String>,
    primary_checksum: Option<String>,
    build_href: Option<String>,
    build_checksum: Option<String>,
    repomd_checksum: Option<String>,
}

impl PackageRecord {
    pub fn nevra(&self) -> String {
        format!(
            "{}-{}:{}-{}.{}",
            self.name,
            self.epoch.as_deref().unwrap_or("0"),
            self.version,
            self.release,
            self.arch
        )
    }

    pub fn evr(&self) -> String {
        match self.epoch.as_deref().unwrap_or("0") {
            "0" => format!("{}-{}", self.version, self.release),
            epoch => format!("{epoch}:{}-{}", self.version, self.release),
        }
    }
}

pub fn resolve_repositories(request: &ResolveRequest) -> Result<RepositoryState> {
    let cache_dir = cache_root()?.join("v1").join("repo");
    if request.clean_cache && cache_dir.exists() {
        fs::remove_dir_all(&cache_dir).map_err(|err| RgbsError::io(&cache_dir, err))?;
    }
    ensure_dir(&cache_dir)?;

    let mut repositories = Vec::new();
    let mut packages = Vec::new();
    let mut fingerprint_parts = Vec::new();
    let mut buildconf = request
        .explicit_buildconf
        .as_deref()
        .map(|value| materialize_explicit_buildconf(value, &cache_dir))
        .transpose()?;

    for (priority, repo) in request.repos.iter().enumerate() {
        let source = RepoSource::from_resolved(repo);
        resolve_one_repo(
            &source,
            priority,
            &request.arch,
            &cache_dir,
            &mut repositories,
            &mut packages,
            &mut fingerprint_parts,
            &mut buildconf,
        )?;
    }

    let fingerprint = sha256_hex(fingerprint_parts.join("\n"));
    Ok(RepositoryState {
        cache_root: path_to_string(&cache_dir),
        fingerprint,
        buildconf: buildconf.as_ref().map(|path| path_to_string(path)),
        package_count: packages.len(),
        repositories,
        packages,
    })
}

fn resolve_one_repo(
    source: &RepoSource,
    priority: usize,
    arch: &str,
    cache_dir: &Path,
    repositories: &mut Vec<ResolvedRepository>,
    packages: &mut Vec<PackageRecord>,
    fingerprint_parts: &mut Vec<String>,
    buildconf: &mut Option<PathBuf>,
) -> Result<()> {
    if let Some(data) = load_standard_repo(source, priority, cache_dir)? {
        if buildconf.is_none() {
            *buildconf = data.buildconf.clone();
        }
        fingerprint_parts.extend(data.fingerprint_parts);
        packages.extend(data.packages);
        repositories.push(data.repository);
        return Ok(());
    }

    if let Some(meta) = load_legacy_build_meta(source, cache_dir)? {
        let derived = derive_repos_from_legacy(source, arch, &meta)?;
        if buildconf.is_none() {
            *buildconf = materialize_legacy_buildconf(source, cache_dir, &meta)?;
        }
        for derived_source in derived {
            let data =
                load_standard_repo(&derived_source, priority, cache_dir)?.ok_or_else(|| {
                    RgbsError::message(format!(
                        "derived repository is missing repodata: {}",
                        derived_source.display_location
                    ))
                })?;
            if buildconf.is_none() {
                *buildconf = data.buildconf.clone();
            }
            fingerprint_parts.extend(data.fingerprint_parts);
            packages.extend(data.packages);
            repositories.push(ResolvedRepository {
                layout: RepositoryLayout::LegacyBuildXml,
                ..data.repository
            });
        }
        return Ok(());
    }

    if source_exists(source, "build.xml")? {
        return Err(RgbsError::message(
            "repository root contains build.xml; specify the real RPM repo with repodata",
        ));
    }

    Err(RgbsError::message(format!(
        "unsupported repository layout: {}",
        source.display_location
    )))
}

fn load_standard_repo(
    source: &RepoSource,
    priority: usize,
    cache_dir: &Path,
) -> Result<Option<StandardRepoData>> {
    let repomd_bytes = match fetch_optional(source, "repodata/repomd.xml", cache_dir, true, None)? {
        Some(bytes) => bytes,
        None => return Ok(None),
    };
    let repomd = parse_repomd(&repomd_bytes)?;
    let primary_href = repomd.primary_href.clone().ok_or_else(|| {
        RgbsError::message(format!(
            "repo has no primary metadata: {}",
            source.display_location
        ))
    })?;
    let primary_bytes = fetch_optional(
        source,
        &primary_href,
        cache_dir,
        false,
        repomd.primary_checksum.as_deref(),
    )?
    .ok_or_else(|| {
        RgbsError::message(format!(
            "repo primary metadata is missing: {}/{}",
            source.display_location, primary_href
        ))
    })?;
    let decoded_primary = decode_metadata(&primary_href, &primary_bytes)?;
    let primary_cache_path = cache_metadata(
        cache_dir,
        "primary",
        &primary_href,
        repomd.primary_checksum.as_deref(),
        &decoded_primary,
    )?;
    let buildconf = if let Some(build_href) = repomd.build_href.as_deref() {
        let raw = fetch_optional(
            source,
            build_href,
            cache_dir,
            false,
            repomd.build_checksum.as_deref(),
        )?;
        raw.map(|bytes| decode_metadata(build_href, &bytes))
            .transpose()?
            .map(|decoded| {
                cache_metadata(
                    cache_dir,
                    "buildconf",
                    build_href,
                    repomd.build_checksum.as_deref(),
                    &decoded,
                )
            })
            .transpose()?
    } else {
        None
    };
    let packages = parse_primary_packages(source, priority, &decoded_primary)?;
    let repository = ResolvedRepository {
        name: source.name.clone(),
        location: source.display_location.clone(),
        layout: RepositoryLayout::Standard,
        repomd_checksum: repomd.repomd_checksum.clone(),
        primary_cache_path: path_to_string(&primary_cache_path),
        buildconf_cache_path: buildconf.as_ref().map(|path| path_to_string(path)),
        package_count: packages.len(),
    };
    let mut fingerprint_parts = vec![
        source.raw_location.clone(),
        repomd
            .repomd_checksum
            .clone()
            .unwrap_or_else(|| sha256_hex(&repomd_bytes)),
        repomd
            .primary_checksum
            .unwrap_or_else(|| sha256_hex(&decoded_primary)),
    ];
    if let Some(path) = &buildconf {
        let bytes = fs::read(path).map_err(|err| RgbsError::io(path, err))?;
        fingerprint_parts.push(sha256_hex(bytes));
    }
    Ok(Some(StandardRepoData {
        repository,
        packages,
        buildconf,
        fingerprint_parts,
    }))
}

fn load_legacy_build_meta(
    source: &RepoSource,
    cache_dir: &Path,
) -> Result<Option<LegacyBuildMeta>> {
    let Some(bytes) = fetch_optional(source, "builddata/build.xml", cache_dir, false, None)? else {
        return Ok(None);
    };
    parse_legacy_build_xml(&bytes).map(Some)
}

fn derive_repos_from_legacy(
    source: &RepoSource,
    arch: &str,
    meta: &LegacyBuildMeta,
) -> Result<Vec<RepoSource>> {
    let arch_matches = if meta.archs.is_empty() {
        true
    } else {
        meta.archs.iter().any(|item| item == arch)
    };
    if !arch_matches {
        return Ok(Vec::new());
    }

    let mut derived = Vec::new();
    for repo_name in &meta.repos {
        let relative = format!("repos/{repo_name}/{arch}/packages");
        let raw_location = join_location(&source.raw_location, &relative)?;
        derived.push(RepoSource {
            name: format!("{}::{repo_name}/{arch}", source.name),
            display_location: redact_location(&raw_location),
            raw_location,
            kind: source.kind.clone(),
        });
    }
    Ok(derived)
}

fn materialize_legacy_buildconf(
    source: &RepoSource,
    cache_dir: &Path,
    meta: &LegacyBuildMeta,
) -> Result<Option<PathBuf>> {
    let Some(buildconf_name) = meta.buildconf.as_deref() else {
        return Ok(None);
    };
    let relative = format!("builddata/{buildconf_name}");
    let Some(raw) = fetch_optional(source, &relative, cache_dir, false, None)? else {
        return Ok(None);
    };
    let decoded = decode_metadata(buildconf_name, &raw)?;
    let cache_key = meta
        .id
        .as_deref()
        .map(|value| value.replace('-', ""))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| buildconf_name.to_string());
    let target = cache_dir
        .join("buildconf")
        .join(format!("{}-legacy.conf", sha256_hex(cache_key)));
    atomic_write(&target, &decoded)?;
    Ok(Some(target))
}

fn materialize_explicit_buildconf(value: &str, cache_dir: &Path) -> Result<PathBuf> {
    if let Ok(url) = Url::parse(value) {
        let temp_source = RepoSource {
            name: "buildconf".to_string(),
            display_location: redact_location(value),
            raw_location: url.to_string(),
            kind: RepoKind::Remote,
        };
        let bytes = fetch_optional(&temp_source, "", cache_dir, true, None)?
            .ok_or_else(|| RgbsError::message(format!("failed to fetch buildconf: {value}")))?;
        let decoded = decode_metadata(value, &bytes)?;
        let target = cache_dir
            .join("buildconf")
            .join(format!("{}-explicit.conf", sha256_hex(value)));
        atomic_write(&target, &decoded)?;
        return Ok(target);
    }

    let path = PathBuf::from(value);
    if !path.exists() {
        return Err(RgbsError::message(format!(
            "explicit buildconf does not exist: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn fetch_optional(
    source: &RepoSource,
    relative: &str,
    cache_dir: &Path,
    fresh: bool,
    checksum_hint: Option<&str>,
) -> Result<Option<Vec<u8>>> {
    let full_location = if relative.is_empty() {
        source.raw_location.clone()
    } else {
        join_location(&source.raw_location, relative)?
    };

    let cache_path = metadata_cache_path(cache_dir, &full_location, checksum_hint);
    if !fresh && cache_path.exists() {
        return fs::read(&cache_path)
            .map(Some)
            .map_err(|err| RgbsError::io(&cache_path, err));
    }

    let bytes = match source.kind {
        RepoKind::LocalPath => {
            let path = if relative.is_empty() {
                PathBuf::from(&source.raw_location)
            } else {
                PathBuf::from(&source.raw_location).join(relative)
            };
            if !path.exists() {
                return Ok(None);
            }
            fs::read(&path).map_err(|err| RgbsError::io(&path, err))?
        }
        RepoKind::Remote => fetch_remote(&full_location)?,
    };
    atomic_write(&cache_path, &bytes)?;
    Ok(Some(bytes))
}

fn fetch_remote(url: &str) -> Result<Vec<u8>> {
    let mut parsed = Url::parse(url).map_err(|err| RgbsError::InvalidUrl {
        url: url.to_string(),
        message: err.to_string(),
    })?;
    let username = if parsed.username().is_empty() {
        None
    } else {
        Some(parsed.username().to_string())
    };
    let password = parsed.password().map(ToOwned::to_owned);
    if username.is_some() {
        parsed.set_username("").map_err(|_| RgbsError::InvalidUrl {
            url: url.to_string(),
            message: "failed to clear username".to_string(),
        })?;
        parsed
            .set_password(None)
            .map_err(|_| RgbsError::InvalidUrl {
                url: url.to_string(),
                message: "failed to clear password".to_string(),
            })?;
    }

    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let mut request = agent.get(parsed.as_str());
    if let Some(user) = username {
        let header =
            BASE64_STANDARD.encode(format!("{user}:{}", password.as_deref().unwrap_or("")));
        request = request.set("Authorization", &format!("Basic {header}"));
    }

    let response = match request.call() {
        Ok(response) => response,
        Err(ureq::Error::Status(404, _)) => return Err(RgbsError::message("404")),
        Err(err) => return Err(RgbsError::message(err.to_string())),
    };
    let mut bytes = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|err| RgbsError::message(err.to_string()))?;
    Ok(bytes)
}

fn decode_metadata(path_hint: &str, bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match () {
        _ if path_hint.ends_with(".gz") => {
            GzDecoder::new(bytes).read_to_end(&mut out).map_err(|err| {
                RgbsError::message(format!("failed to decode gzip {path_hint}: {err}"))
            })?;
        }
        _ if path_hint.ends_with(".bz2") || path_hint.ends_with(".tbz2") => {
            BzDecoder::new(bytes).read_to_end(&mut out).map_err(|err| {
                RgbsError::message(format!("failed to decode bzip2 {path_hint}: {err}"))
            })?;
        }
        _ if path_hint.ends_with(".xz") => {
            XzDecoder::new(bytes).read_to_end(&mut out).map_err(|err| {
                RgbsError::message(format!("failed to decode xz {path_hint}: {err}"))
            })?;
        }
        _ if path_hint.ends_with(".zst") || path_hint.ends_with(".zstd") => {
            ZstdDecoder::new(bytes)
                .map_err(|err| {
                    RgbsError::message(format!("failed to decode zstd {path_hint}: {err}"))
                })?
                .read_to_end(&mut out)
                .map_err(|err| {
                    RgbsError::message(format!("failed to decode zstd {path_hint}: {err}"))
                })?;
        }
        _ => return Ok(bytes.to_vec()),
    }
    Ok(out)
}

fn parse_repomd(bytes: &[u8]) -> Result<RepomdData> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| RgbsError::message(format!("repomd.xml is not utf8: {err}")))?;
    let document = Document::parse(text)
        .map_err(|err| RgbsError::message(format!("invalid repomd.xml: {err}")))?;

    let mut data = RepomdData {
        repomd_checksum: Some(sha256_hex(bytes)),
        ..RepomdData::default()
    };
    for node in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "data")
    {
        let Some(kind) = node.attribute("type") else {
            continue;
        };
        let location = node
            .children()
            .find(|child| child.is_element() && child.tag_name().name() == "location")
            .and_then(|child| child.attribute("href"))
            .map(ToOwned::to_owned);
        let checksum = node
            .children()
            .find(|child| child.is_element() && child.tag_name().name() == "checksum")
            .and_then(|child| child.text())
            .map(str::trim)
            .map(ToOwned::to_owned);

        match kind {
            "primary" => {
                data.primary_href = location;
                data.primary_checksum = checksum;
            }
            "build" => {
                data.build_href = location;
                data.build_checksum = checksum;
            }
            _ => {}
        }
    }
    Ok(data)
}

fn parse_legacy_build_xml(bytes: &[u8]) -> Result<LegacyBuildMeta> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| RgbsError::message(format!("build.xml is not utf8: {err}")))?;
    let document = Document::parse(text)
        .map_err(|err| RgbsError::message(format!("invalid build.xml: {err}")))?;
    let root = document.root_element();
    if root.attribute("version").is_some() {
        return Err(RgbsError::message(
            "new-format repository roots are not supported; specify the actual RPM repo with repodata",
        ));
    }

    let mut meta = LegacyBuildMeta::default();
    meta.buildconf = child_text(root, "buildconf");
    meta.id = child_text(root, "id");
    if let Some(repos) = child(root, "repos") {
        meta.repos = repos
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "repo")
            .filter_map(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }
    if let Some(archs) = child(root, "archs") {
        meta.archs = archs
            .children()
            .filter(|node| node.is_element() && node.tag_name().name() == "arch")
            .filter_map(|node| node.text())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
            .collect();
    }
    Ok(meta)
}

fn parse_primary_packages(
    source: &RepoSource,
    priority: usize,
    bytes: &[u8],
) -> Result<Vec<PackageRecord>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|err| RgbsError::message(format!("primary.xml is not utf8: {err}")))?;
    let document = Document::parse(text)
        .map_err(|err| RgbsError::message(format!("invalid primary.xml: {err}")))?;

    let mut packages = Vec::new();
    for package in document
        .descendants()
        .filter(|node| node.is_element() && node.tag_name().name() == "package")
    {
        if package.attribute("type") != Some("rpm") {
            continue;
        }

        let name = child_text(package, "name")
            .ok_or_else(|| RgbsError::message("primary.xml package is missing <name>"))?;
        let arch = child_text(package, "arch")
            .ok_or_else(|| RgbsError::message("primary.xml package is missing <arch>"))?;
        let version_node = child(package, "version")
            .ok_or_else(|| RgbsError::message("primary.xml package is missing <version>"))?;
        let version = version_node
            .attribute("ver")
            .unwrap_or_default()
            .to_string();
        let release = version_node
            .attribute("rel")
            .unwrap_or_default()
            .to_string();
        let epoch = version_node
            .attribute("epoch")
            .map(ToOwned::to_owned)
            .filter(|value| value != "0");
        let checksum = child_text(package, "checksum");
        let location = child(package, "location")
            .and_then(|node| node.attribute("href"))
            .map(ToOwned::to_owned)
            .ok_or_else(|| RgbsError::message("primary.xml package is missing <location>"))?;
        let format = child(package, "format");
        let provides = format
            .and_then(|node| child(node, "provides"))
            .map(parse_capabilities)
            .transpose()?
            .unwrap_or_default();
        let requires = format
            .and_then(|node| child(node, "requires"))
            .map(parse_capabilities)
            .transpose()?
            .unwrap_or_default();
        packages.push(PackageRecord {
            name,
            arch,
            epoch,
            version,
            release,
            checksum,
            location,
            repo_name: source.name.clone(),
            repo_location: source.display_location.clone(),
            repo_location_raw: source.raw_location.clone(),
            repo_priority: priority,
            provides,
            requires,
        });
    }
    Ok(packages)
}

fn parse_capabilities(node: Node<'_, '_>) -> Result<Vec<Capability>> {
    let mut items = Vec::new();
    for entry in node
        .children()
        .filter(|child| child.is_element() && child.tag_name().name() == "entry")
    {
        let name = entry.attribute("name").unwrap_or_default().to_string();
        if name.is_empty() {
            continue;
        }
        items.push(Capability {
            name,
            flags: entry.attribute("flags").map(ToOwned::to_owned),
            version: entry_to_evr(entry),
            preinstall: entry.attribute("pre") == Some("1"),
        });
    }
    Ok(items)
}

fn entry_to_evr(node: Node<'_, '_>) -> Option<String> {
    let version = node.attribute("ver")?;
    let release = node.attribute("rel");
    let epoch = node.attribute("epoch").unwrap_or("0");
    let mut out = String::new();
    if epoch != "0" {
        out.push_str(epoch);
        out.push(':');
    }
    out.push_str(version);
    if let Some(release) = release {
        out.push('-');
        out.push_str(release);
    }
    Some(out)
}

fn child<'a>(node: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    child(node, name)
        .and_then(|node| node.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn source_exists(source: &RepoSource, relative: &str) -> Result<bool> {
    match source.kind {
        RepoKind::LocalPath => Ok(PathBuf::from(&source.raw_location).join(relative).exists()),
        RepoKind::Remote => {
            let url = join_location(&source.raw_location, relative)?;
            let response = ureq::head(&url).timeout(Duration::from_secs(15)).call();
            match response {
                Ok(_) => Ok(true),
                Err(ureq::Error::Status(404, _)) => Ok(false),
                Err(err) => Err(RgbsError::message(err.to_string())),
            }
        }
    }
}

fn join_location(base: &str, relative: &str) -> Result<String> {
    if let Ok(url) = Url::parse(base) {
        return url
            .join(relative)
            .map(|value| value.to_string())
            .map_err(|err| RgbsError::InvalidUrl {
                url: base.to_string(),
                message: err.to_string(),
            });
    }

    Ok(path_to_string(&PathBuf::from(base).join(relative)))
}

fn redact_location(raw: &str) -> String {
    if let Ok(mut url) = Url::parse(raw) {
        if url.password().is_some() {
            let _ = url.set_password(Some("******"));
        }
        return url.to_string();
    }
    raw.to_string()
}

fn metadata_cache_path(
    cache_dir: &Path,
    full_location: &str,
    checksum_hint: Option<&str>,
) -> PathBuf {
    if let Some(checksum) = checksum_hint.filter(|value| !value.is_empty()) {
        let prefix = &checksum[..checksum.len().min(2)];
        let file_name = format!(
            "{}-{}",
            checksum,
            Path::new(full_location)
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("metadata")
        );
        return cache_dir.join("objects").join(prefix).join(file_name);
    }

    let key = sha256_hex(full_location);
    cache_dir.join("objects").join(&key[..2]).join(key)
}

fn cache_metadata(
    cache_dir: &Path,
    bucket: &str,
    href: &str,
    checksum_hint: Option<&str>,
    decoded: &[u8],
) -> Result<PathBuf> {
    let target = if let Some(checksum) = checksum_hint.filter(|value| !value.is_empty()) {
        cache_dir.join(bucket).join(format!("{checksum}.xml"))
    } else {
        cache_dir
            .join(bucket)
            .join(format!("{}.xml", sha256_hex(href.as_bytes())))
    };
    atomic_write(&target, decoded)?;
    Ok(target)
}

impl RepoSource {
    fn from_resolved(repo: &ResolvedRepo) -> Self {
        Self {
            name: repo.name.clone(),
            display_location: repo.location.clone(),
            raw_location: repo.raw_location.clone(),
            kind: repo.kind.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::Path;
    use std::sync::Mutex;

    use flate2::Compression;
    use flate2::write::GzEncoder;
    use tempfile::TempDir;

    use super::*;

    static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolves_local_standard_repo() {
        let fixture = RepoFixture::new();
        let repo = fixture.create_standard_repo("repo-a");
        let state = with_cache_home(&fixture.root.join("cache"), || {
            resolve_repositories(&ResolveRequest {
                arch: "aarch64".to_string(),
                repos: vec![ResolvedRepo {
                    name: "repo.local".to_string(),
                    kind: RepoKind::LocalPath,
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

        assert_eq!(state.repositories.len(), 1);
        assert_eq!(state.package_count, 2);
        assert!(state.buildconf.as_deref().unwrap().ends_with(".xml"));
        assert_eq!(state.packages[0].repo_name, "repo.local");
    }

    #[test]
    fn resolves_legacy_repo_root() {
        let fixture = RepoFixture::new();
        let legacy_root = fixture.create_legacy_repo("legacy-root");
        let state = with_cache_home(&fixture.root.join("cache"), || {
            resolve_repositories(&ResolveRequest {
                arch: "aarch64".to_string(),
                repos: vec![ResolvedRepo {
                    name: "repo.legacy".to_string(),
                    kind: RepoKind::LocalPath,
                    location: path_to_string(&legacy_root),
                    raw_location: path_to_string(&legacy_root),
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

        assert_eq!(state.repositories.len(), 1);
        assert_eq!(
            state.repositories[0].layout,
            RepositoryLayout::LegacyBuildXml
        );
        assert_eq!(state.package_count, 2);
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

    struct RepoFixture {
        _temp_dir: TempDir,
        root: PathBuf,
    }

    impl RepoFixture {
        fn new() -> Self {
            let temp_dir = TempDir::new().unwrap();
            let root = temp_dir.path().join("repos");
            fs::create_dir_all(&root).unwrap();
            Self {
                _temp_dir: temp_dir,
                root,
            }
        }

        fn create_standard_repo(&self, name: &str) -> PathBuf {
            let repo = self.root.join(name);
            let repodata = repo.join("repodata");
            let packages = repo.join("packages");
            fs::create_dir_all(&repodata).unwrap();
            fs::create_dir_all(&packages).unwrap();

            fs::write(packages.join("bash.rpm"), b"not-an-rpm").unwrap();
            fs::write(packages.join("pkgconfig-alsa.rpm"), b"not-an-rpm").unwrap();

            let primary = r#"<?xml version="1.0" encoding="UTF-8"?>
<metadata xmlns="http://linux.duke.edu/metadata/common" packages="2">
  <package type="rpm">
    <name>bash</name>
    <arch>aarch64</arch>
    <version epoch="0" ver="5.0" rel="1"/>
    <checksum type="sha256" pkgid="YES">aaa</checksum>
    <location href="packages/bash.rpm"/>
    <format xmlns:rpm="http://linux.duke.edu/metadata/rpm">
      <rpm:provides>
        <rpm:entry name="bash" flags="EQ" ver="5.0" rel="1" epoch="0"/>
      </rpm:provides>
      <rpm:requires>
        <rpm:entry name="/bin/sh"/>
      </rpm:requires>
    </format>
  </package>
  <package type="rpm">
    <name>pkgconfig-alsa</name>
    <arch>noarch</arch>
    <version epoch="0" ver="1.0" rel="1"/>
    <checksum type="sha256" pkgid="YES">bbb</checksum>
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
            let primary_gz = gzip(primary.as_bytes());
            fs::write(repodata.join("primary.xml.gz"), primary_gz).unwrap();

            let buildconf_gz = gzip(b"%define distro test\n");
            fs::write(repodata.join("build.conf.gz"), buildconf_gz).unwrap();

            let repomd = r#"<?xml version="1.0" encoding="UTF-8"?>
<repomd xmlns="http://linux.duke.edu/metadata/repo">
  <data type="primary">
    <checksum type="sha256">primarychecksum</checksum>
    <location href="repodata/primary.xml.gz"/>
  </data>
  <data type="build">
    <checksum type="sha256">buildchecksum</checksum>
    <location href="repodata/build.conf.gz"/>
  </data>
</repomd>
"#;
            fs::write(repodata.join("repomd.xml"), repomd).unwrap();
            repo
        }

        fn create_legacy_repo(&self, name: &str) -> PathBuf {
            let root = self.root.join(name);
            let builddata = root.join("builddata");
            fs::create_dir_all(&builddata).unwrap();
            fs::write(builddata.join("test.conf"), b"%define distro legacy\n").unwrap();
            let build_xml = r#"<build>
  <buildconf>test.conf</buildconf>
  <repos><repo>main</repo></repos>
  <archs><arch>aarch64</arch></archs>
  <id>test_1</id>
</build>
"#;
            fs::write(builddata.join("build.xml"), build_xml).unwrap();
            let _ = self.create_standard_repo("legacy-root/repos/main/aarch64/packages");
            root
        }
    }

    fn gzip(input: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(input).unwrap();
        encoder.finish().unwrap()
    }
}
