use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, RgbsError>;
pub const SUPPORTED_TARGET_ARCHES: [&str; 2] = ["armv7l", "aarch64"];

#[derive(Debug, Error)]
pub enum RgbsError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("config error: {0}")]
    Config(String),
    #[error("invalid url `{url}`: {message}")]
    InvalidUrl { url: String, message: String },
    #[error("command failed: {command}: {message}")]
    Command { command: String, message: String },
    #[error("{0}")]
    Message(String),
}

impl RgbsError {
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn command(command: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Command {
            command: command.into(),
            message: message.into(),
        }
    }
}

pub fn expand_tilde(value: &str, home_dir: &Path) -> PathBuf {
    if value == "~" {
        return home_dir.to_path_buf();
    }

    if let Some(rest) = value.strip_prefix("~/") {
        return home_dir.join(rest);
    }

    PathBuf::from(value)
}

pub fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).map_err(|err| RgbsError::io(path, err))
}

pub fn cache_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("rgbs"));
    }

    let home_dir = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| RgbsError::config("HOME is not set"))?;
    Ok(home_dir.join(".cache").join("rgbs"))
}

pub fn canonicalize_target_arch(arch: &str) -> Option<&'static str> {
    match arch {
        "aarch64" | "arm64" => Some("aarch64"),
        "armv7l" | "armv7hl" | "armhf" => Some("armv7l"),
        _ => None,
    }
}

pub fn supported_target_arch_list() -> &'static str {
    "armv7l, aarch64"
}

pub fn normalize_arch(arch: &str) -> &str {
    match arch {
        "amd64" => "x86_64",
        "i386" | "i486" | "i586" | "i686" => "x86",
        "arm64" => "aarch64",
        "armv7hl" | "armhf" => "armv7l",
        other => other,
    }
}

pub fn sha256_hex(input: impl AsRef<[u8]>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_ref());
    hex::encode(hasher.finalize())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| RgbsError::message(format!("path has no parent: {}", path.display())))?;
    ensure_dir(parent)?;
    let temp_name = format!(
        ".{}.tmp-{}",
        path.file_name()
            .and_then(OsStr::to_str)
            .unwrap_or("rgbs-write"),
        std::process::id()
    );
    let temp_path = parent.join(temp_name);
    {
        let mut file =
            fs::File::create(&temp_path).map_err(|err| RgbsError::io(&temp_path, err))?;
        file.write_all(bytes)
            .map_err(|err| RgbsError::io(&temp_path, err))?;
        file.flush().map_err(|err| RgbsError::io(&temp_path, err))?;
    }
    fs::rename(&temp_path, path).map_err(|err| RgbsError::io(path, err))?;
    Ok(())
}

pub fn render_command(command: &Command) -> String {
    let program = command.get_program().to_string_lossy();
    let args = command
        .get_args()
        .map(|arg| shell_escape(arg.to_string_lossy().as_ref()))
        .collect::<Vec<_>>();
    if args.is_empty() {
        program.into_owned()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

pub fn run_command(command: &mut Command) -> Result<Output> {
    let rendered = render_command(command);
    let output = command
        .output()
        .map_err(|err| RgbsError::command(&rendered, err.to_string()))?;
    if output.status.success() {
        return Ok(output);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let message = if stderr.is_empty() {
        if stdout.is_empty() {
            format!("exit status {}", output.status)
        } else {
            stdout
        }
    } else {
        stderr
    };
    Err(RgbsError::command(rendered, message))
}

fn shell_escape(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "-_./=:".contains(ch))
    {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::{canonicalize_target_arch, normalize_arch, supported_target_arch_list};

    #[test]
    fn canonicalizes_supported_target_arch_aliases() {
        assert_eq!(canonicalize_target_arch("aarch64"), Some("aarch64"));
        assert_eq!(canonicalize_target_arch("arm64"), Some("aarch64"));
        assert_eq!(canonicalize_target_arch("armv7l"), Some("armv7l"));
        assert_eq!(canonicalize_target_arch("armhf"), Some("armv7l"));
        assert_eq!(canonicalize_target_arch("x86_64"), None);
    }

    #[test]
    fn normalizes_common_arch_aliases() {
        assert_eq!(normalize_arch("amd64"), "x86_64");
        assert_eq!(normalize_arch("i686"), "x86");
        assert_eq!(normalize_arch("arm64"), "aarch64");
        assert_eq!(normalize_arch("armhf"), "armv7l");
    }

    #[test]
    fn supported_target_arch_list_matches_scope() {
        assert_eq!(supported_target_arch_list(), "armv7l, aarch64");
    }
}
