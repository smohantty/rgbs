use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rgbs_common::{Result, RgbsError, path_to_string, run_command};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct InspectRequest {
    pub git_dir: PathBuf,
    pub packaging_dir: String,
    pub spec_override: Option<PathBuf>,
    pub buildconf: Option<PathBuf>,
    pub defines: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Requirement {
    pub name: String,
    pub flags: Option<String>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SpecInfo {
    pub packaging_dir: String,
    pub spec_path: String,
    pub name: String,
    pub version: String,
    pub release: String,
    pub build_requires: Vec<Requirement>,
    pub sources: Vec<String>,
}

pub fn inspect_spec(request: &InspectRequest) -> Result<SpecInfo> {
    let packaging_dir = resolve_packaging_dir(&request.git_dir, &request.packaging_dir);
    let spec_path = discover_spec_path(
        &request.git_dir,
        &packaging_dir,
        request.spec_override.as_ref(),
    )?;
    let buildconf_defines = request
        .buildconf
        .as_deref()
        .map(parse_buildconf_defines)
        .transpose()?
        .unwrap_or_default();
    let (name, version, release) = query_nvr(&spec_path, &buildconf_defines, &request.defines)?;
    let build_requires = query_build_requires(&spec_path, &buildconf_defines, &request.defines)?;
    let expanded = preprocess_spec(&spec_path, &buildconf_defines, &request.defines)?;
    let sources = parse_source_tags(&expanded);

    Ok(SpecInfo {
        packaging_dir: path_to_string(&packaging_dir),
        spec_path: path_to_string(&spec_path),
        name,
        version,
        release,
        build_requires,
        sources,
    })
}

pub fn discover_spec_path(
    git_dir: &Path,
    packaging_dir: &Path,
    spec_override: Option<&PathBuf>,
) -> Result<PathBuf> {
    if let Some(spec) = spec_override {
        let candidate = if spec.is_absolute() {
            spec.clone()
        } else {
            packaging_dir.join(spec)
        };
        if !candidate.exists() {
            return Err(RgbsError::message(format!(
                "no such spec file: {}",
                candidate.display()
            )));
        }
        return Ok(candidate);
    }

    let mut specs = fs::read_dir(packaging_dir)
        .map_err(|err| RgbsError::io(packaging_dir, err))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("spec"))
        .collect::<Vec<_>>();
    specs.sort();
    if specs.is_empty() {
        return Err(RgbsError::message(format!(
            "can't find any spec file under packaging dir: {}",
            packaging_dir.display()
        )));
    }

    let project_name = git_dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let preferred = packaging_dir.join(format!("{project_name}.spec"));
    if preferred.exists() {
        Ok(preferred)
    } else {
        Ok(specs.remove(0))
    }
}

fn resolve_packaging_dir(git_dir: &Path, packaging_dir: &str) -> PathBuf {
    let path = PathBuf::from(packaging_dir);
    if path.is_absolute() {
        path
    } else {
        git_dir.join(path)
    }
}

fn query_nvr(
    spec_path: &Path,
    buildconf_defines: &[String],
    defines: &[String],
) -> Result<(String, String, String)> {
    let output = rpmspec(
        spec_path,
        &[
            "--query",
            "--srpm",
            "--queryformat",
            "%{NAME}\n%{VERSION}\n%{RELEASE}\n",
        ],
        buildconf_defines,
        defines,
    )?;
    let mut lines = output.lines();
    let name = lines.next().unwrap_or_default().trim().to_string();
    let version = lines.next().unwrap_or_default().trim().to_string();
    let release = lines.next().unwrap_or_default().trim().to_string();
    if name.is_empty() || version.is_empty() || release.is_empty() {
        return Err(RgbsError::message(format!(
            "rpmspec did not return complete NVR for {}",
            spec_path.display()
        )));
    }
    Ok((name, version, release))
}

fn query_build_requires(
    spec_path: &Path,
    buildconf_defines: &[String],
    defines: &[String],
) -> Result<Vec<Requirement>> {
    let output = rpmspec(
        spec_path,
        &["--query", "--buildrequires"],
        buildconf_defines,
        defines,
    )?;
    let mut requirements = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        requirements.push(parse_requirement(trimmed));
    }
    Ok(requirements)
}

fn preprocess_spec(
    spec_path: &Path,
    buildconf_defines: &[String],
    defines: &[String],
) -> Result<String> {
    rpmspec(spec_path, &["-P"], buildconf_defines, defines)
}

fn parse_buildconf_defines(path: &Path) -> Result<Vec<String>> {
    let text = fs::read_to_string(path).map_err(|err| RgbsError::io(path, err))?;
    let mut defines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("%define ") || trimmed.starts_with("%global ") {
            let mut parts = trimmed.splitn(3, char::is_whitespace);
            let _directive = parts.next();
            let Some(name) = parts.next() else {
                continue;
            };
            let Some(value) = parts.next() else {
                continue;
            };
            defines.push(format!("{} {}", name.trim(), value.trim()));
        }
    }
    Ok(defines)
}

fn rpmspec(
    spec_path: &Path,
    args: &[&str],
    buildconf_defines: &[String],
    defines: &[String],
) -> Result<String> {
    let mut command = Command::new("rpmspec");
    command.args(args);
    for define in buildconf_defines.iter().chain(defines.iter()) {
        command.arg("--define").arg(define);
    }
    command.arg(spec_path);
    let output = run_command(&mut command)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn parse_requirement(raw: &str) -> Requirement {
    let mut parts = raw.split_whitespace();
    let name = parts.next().unwrap_or_default().to_string();
    let maybe_flags = parts.next().map(ToOwned::to_owned);
    let maybe_version = parts.collect::<Vec<_>>();

    if let Some(flags) = maybe_flags.as_deref().filter(|value| is_relation(value)) {
        return Requirement {
            name,
            flags: Some(flags.to_string()),
            version: Some(maybe_version.join(" ")).filter(|value| !value.is_empty()),
        };
    }

    Requirement {
        name: raw.to_string(),
        flags: None,
        version: None,
    }
}

fn parse_source_tags(expanded_spec: &str) -> Vec<String> {
    expanded_spec
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (tag, value) = trimmed.split_once(':')?;
            let tag = tag.trim().to_ascii_lowercase();
            if tag.starts_with("source") {
                let value = value.trim();
                if value.is_empty() {
                    None
                } else {
                    Some(value.to_string())
                }
            } else {
                None
            }
        })
        .collect()
}

fn is_relation(value: &str) -> bool {
    matches!(
        value,
        "=" | "==" | ">" | "<" | ">=" | "<=" | "EQ" | "GE" | "GT" | "LE" | "LT"
    )
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn chooses_project_named_spec() {
        let fixture = Fixture::new("demo");
        fixture.write(
            "packaging/demo.spec",
            "Name: demo\nVersion: 1\nRelease: 1\nSummary: test\nLicense: GPL\n",
        );
        fixture.write(
            "packaging/other.spec",
            "Name: other\nVersion: 1\nRelease: 1\nSummary: test\nLicense: GPL\n",
        );

        let path = discover_spec_path(&fixture.git_dir, &fixture.packaging_dir, None).unwrap();
        assert_eq!(
            path.file_name().and_then(|value| value.to_str()),
            Some("demo.spec")
        );
    }

    #[test]
    fn inspects_build_requires_and_sources() {
        let fixture = Fixture::new("fake");
        fixture.write(
            "packaging/fake.spec",
            "Name: fake\nVersion: 1.0\nRelease: 1\nSummary: test\nLicense: GPL\nSource0: %{name}-%{version}.tar.gz\nBuildRequires: flex\nBuildRequires: pkgconfig(alsa)\n%description\ntest\n",
        );

        let info = inspect_spec(&InspectRequest {
            git_dir: fixture.git_dir.clone(),
            packaging_dir: "packaging".to_string(),
            spec_override: None,
            buildconf: None,
            defines: Vec::new(),
        })
        .unwrap();

        assert_eq!(info.name, "fake");
        assert!(info.build_requires.iter().any(|item| item.name == "flex"));
        assert!(info.sources.iter().any(|item| item == "fake-1.0.tar.gz"));
    }

    struct Fixture {
        _temp_dir: TempDir,
        git_dir: PathBuf,
        packaging_dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let temp_dir = TempDir::new().unwrap();
            let git_dir = temp_dir.path().join(name);
            let packaging_dir = git_dir.join("packaging");
            fs::create_dir_all(&packaging_dir).unwrap();
            Self {
                _temp_dir: temp_dir,
                git_dir,
                packaging_dir,
            }
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.git_dir.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, content).unwrap();
        }
    }
}
