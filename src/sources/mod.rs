//! Typed Flatpak sources and content-addressed download cache.

mod cache;

pub use cache::DownloadCache;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::git_source::{GitRef, fetch_git_source};
use crate::utils::{
    copy_dir_all, download_file, extract_archive, guess_archive_type, status, verify_sha256_hex,
};

/// A Flatpak module source entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Source {
    Git {
        url: String,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        commit: Option<String>,
        #[serde(default)]
        branch: Option<String>,
        #[serde(default)]
        dest: Option<String>,
    },
    Archive {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        path: Option<String>,
        sha256: String,
        #[serde(default, rename = "archive-type")]
        archive_type: Option<String>,
        #[serde(default, rename = "strip-components")]
        strip_components: Option<u64>,
        #[serde(default, rename = "dest-filename")]
        dest_filename: Option<String>,
        #[serde(default)]
        dest: Option<String>,
        #[serde(default, rename = "only-arches")]
        only_arches: Option<Vec<String>>,
    },
    File {
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        sha256: Option<String>,
        #[serde(default, rename = "dest-filename")]
        dest_filename: Option<String>,
        #[serde(default)]
        dest: Option<String>,
        #[serde(default, rename = "only-arches")]
        only_arches: Option<Vec<String>>,
    },
    Dir {
        path: String,
        #[serde(default)]
        dest: Option<String>,
    },
    Patch {
        path: String,
        #[serde(default)]
        dest: Option<String>,
    },
    /// Unknown / not-yet-supported source type (kept for forward compatibility).
    #[serde(rename = "_flatplay_other")]
    Other,
}

impl Source {
    pub fn from_value(value: &serde_json::Value) -> Result<Self> {
        match serde_json::from_value::<Self>(value.clone()) {
            Ok(source) => Ok(source),
            Err(_) => {
                // Unknown type or extra fields we do not model yet.
                if value.get("type").and_then(|t| t.as_str()).is_some() {
                    Ok(Self::Other)
                } else {
                    Err(anyhow::anyhow!("Failed to parse source entry: {value}"))
                }
            }
        }
    }

    pub fn from_values(values: &[serde_json::Value]) -> Result<Vec<Self>> {
        values.iter().map(Self::from_value).collect()
    }

    fn matches_arch(&self) -> bool {
        let only = match self {
            Self::Archive { only_arches, .. } | Self::File { only_arches, .. } => {
                only_arches.as_ref()
            }
            _ => None,
        };
        match only {
            None => true,
            Some(arches) => {
                let host = flatpak_arch();
                arches.iter().any(|a| a == &host)
            }
        }
    }

    fn dest_subdir(&self) -> Option<&str> {
        match self {
            Self::Git { dest, .. }
            | Self::Archive { dest, .. }
            | Self::File { dest, .. }
            | Self::Dir { dest, .. }
            | Self::Patch { dest, .. } => dest.as_deref(),
            Self::Other => None,
        }
    }
}

pub fn flatpak_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" => "x86_64".into(),
        "aarch64" => "aarch64".into(),
        "arm" => "arm".into(),
        "x86" | "i686" => "i386".into(),
        other => other.into(),
    }
}

/// Materialize all sources for a module into `source_dir` (created fresh).
pub fn materialize_sources(
    sources: &[Source],
    source_dir: &Path,
    manifest_dir: &Path,
    cache: &DownloadCache,
    module_name: &str,
) -> Result<()> {
    if source_dir.exists() {
        std::fs::remove_dir_all(source_dir)?;
    }
    std::fs::create_dir_all(source_dir)?;

    for source in sources {
        if !source.matches_arch() {
            continue;
        }
        let target = match source.dest_subdir() {
            Some(sub) => {
                let p = source_dir.join(sub);
                std::fs::create_dir_all(&p)?;
                p
            }
            None => source_dir.to_path_buf(),
        };
        materialize_one(source, &target, manifest_dir, cache, module_name)?;
    }
    Ok(())
}

fn materialize_one(
    source: &Source,
    target: &Path,
    manifest_dir: &Path,
    cache: &DownloadCache,
    module_name: &str,
) -> Result<()> {
    match source {
        Source::Git {
            url,
            tag,
            commit,
            branch,
            ..
        } => {
            // Git clones into `target` directly.
            if target.exists() && target != Path::new("") {
                // If target is source_dir itself and empty, ok; if dest subdir, fetch there.
            }
            let clone_dir = if target
                .read_dir()
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
            {
                target.to_path_buf()
            } else {
                // Should not normally happen for first source.
                target.to_path_buf()
            };
            fetch_git_source(
                &clone_dir,
                module_name,
                GitRef {
                    url,
                    commit: commit.as_deref(),
                    tag: tag.as_deref(),
                    branch: branch.as_deref(),
                },
            )?;
        }
        Source::Archive {
            url,
            path,
            sha256,
            archive_type,
            strip_components,
            dest_filename,
            ..
        } => {
            let location = url
                .as_deref()
                .or(path.as_deref())
                .context("Archive source must specify url or path")?;
            let is_url = url.is_some();
            let filename = dest_filename.clone().unwrap_or_else(|| {
                Path::new(location)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("archive")
                    .to_string()
            });
            let archive_path = cache.cached_path(&filename, sha256);
            if let Some(parent) = archive_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !archive_path.exists() {
                if is_url {
                    status(format!("Downloading {module_name} from {location}"));
                    download_file(location, &archive_path)?;
                } else {
                    let src = resolve_path(manifest_dir, location);
                    status(format!("Copying {module_name} from {}", src.display()));
                    std::fs::copy(&src, &archive_path)?;
                }
                verify_sha256_hex(&archive_path, sha256)?;
            } else {
                verify_sha256_hex(&archive_path, sha256)?;
            }
            let strip = strip_components
                .and_then(|v| usize::try_from(v).ok())
                .unwrap_or(1);
            let atype = archive_type
                .clone()
                .unwrap_or_else(|| guess_archive_type(location));
            extract_archive(&archive_path, &atype, target, strip)?;
        }
        Source::File {
            url,
            path,
            sha256,
            dest_filename,
            ..
        } => {
            let location = url
                .as_deref()
                .or(path.as_deref())
                .context("File source must specify url or path")?;
            let is_url = url.is_some();
            let filename = dest_filename.clone().unwrap_or_else(|| {
                Path::new(location)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file")
                    .to_string()
            });
            let dest_path = target.join(&filename);
            if let Some(parent) = dest_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            if let Some(expected) = sha256 {
                let cached = cache.cached_path(&filename, expected);
                if let Some(parent) = cached.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if !cached.exists() {
                    if is_url {
                        status(format!("Downloading {module_name} from {location}"));
                        download_file(location, &cached)?;
                    } else {
                        let src = resolve_path(manifest_dir, location);
                        status(format!("Copying {module_name} from {}", src.display()));
                        std::fs::copy(&src, &cached)?;
                    }
                    verify_sha256_hex(&cached, expected)?;
                }
                if cached != dest_path {
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(&cached, &dest_path)?;
                }
            } else if is_url {
                status(format!("Downloading {module_name} from {location}"));
                download_file(location, &dest_path)?;
            } else {
                let src = resolve_path(manifest_dir, location);
                status(format!("Copying {module_name} from {}", src.display()));
                std::fs::copy(&src, &dest_path)?;
            }
        }
        Source::Dir { path, .. } => {
            let src = resolve_path(manifest_dir, path);
            status(format!(
                "Using directory source for {module_name}: {}",
                src.display()
            ));
            // Prefer lightweight presence: if target is empty module dir and path is project
            // root, leave a marker file with the resolved path for buildsystems to read.
            // For dependency builds that need full tree, copy (can be large).
            if target
                .read_dir()
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
                && source_is_module_root(target)
            {
                std::fs::write(
                    target.join(".flatplay-dir-source"),
                    src.canonicalize()
                        .unwrap_or(src)
                        .to_string_lossy()
                        .as_bytes(),
                )?;
            } else {
                copy_dir_all(&src, target)?;
            }
        }
        Source::Patch { path, .. } => {
            let patch_path = resolve_path(manifest_dir, path);
            apply_patch(&patch_path, target)?;
        }
        Source::Other => {
            anyhow::bail!("Unsupported source type in module '{module_name}'");
        }
    }
    Ok(())
}

fn source_is_module_root(target: &Path) -> bool {
    target
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| !n.is_empty())
}

fn resolve_path(manifest_dir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        manifest_dir.join(p)
    }
}

fn apply_patch(patch_path: &Path, target: &Path) -> Result<()> {
    // Minimal unified-diff apply via `patch` if present; otherwise error with guidance.
    // Prefer pure approach: read patch and use the `patch` crate — avoid host CLI.
    // For Phase B we shell to `patch -p1` only when the binary exists; Wordbook has no patches.
    let patch_body =
        std::fs::read_to_string(patch_path).with_context(|| patch_path.display().to_string())?;
    let _ = (patch_body, target);
    // Try lib: apply with patch crate if we add it; for now use std process patch as last resort
    // for rare manifests — documented limitation.
    let status = std::process::Command::new("patch")
        .args(["-p1", "--forward", "--batch"])
        .current_dir(target)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                let data = std::fs::read(patch_path)?;
                stdin.write_all(&data)?;
            }
            child.wait()
        });
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => anyhow::bail!(
            "Failed to apply patch {} (exit {:?})",
            patch_path.display(),
            s.code()
        ),
        Err(error) => Err(error).context(format!(
            "Failed to apply patch {} (is `patch` available for rare patch sources?)",
            patch_path.display()
        )),
    }
}

/// Resolve the directory path for a `type: dir` source (for meson/cmake).
pub fn resolve_dir_source(sources: &[Source], manifest_dir: &Path) -> Option<PathBuf> {
    for source in sources {
        if let Source::Dir { path, .. } = source {
            return Some(resolve_path(manifest_dir, path));
        }
    }
    None
}

/// Fingerprint sources for cache invalidation (stable-ish string).
pub fn sources_fingerprint(sources: &[Source]) -> String {
    serde_json::to_string(sources).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_source() {
        let v = serde_json::json!({
            "type": "git",
            "url": "https://example.com/repo.git",
            "branch": "main"
        });
        let s = Source::from_value(&v).unwrap();
        match s {
            Source::Git { url, branch, .. } => {
                assert_eq!(url, "https://example.com/repo.git");
                assert_eq!(branch.as_deref(), Some("main"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn only_arches_filters() {
        let host = flatpak_arch();
        let other = if host == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        let skip = Source::File {
            url: Some("http://x".into()),
            path: None,
            sha256: None,
            dest_filename: None,
            dest: None,
            only_arches: Some(vec![other.into()]),
        };
        assert!(!skip.matches_arch());
        let keep = Source::File {
            url: Some("http://x".into()),
            path: None,
            sha256: None,
            dest_filename: None,
            dest: None,
            only_arches: Some(vec![host]),
        };
        assert!(keep.matches_arch());
    }

    #[test]
    fn resolve_dir_source_relative() {
        let sources = vec![Source::Dir {
            path: "../../.".into(),
            dest: None,
        }];
        let manifest_dir = PathBuf::from("/proj/build-aux/flatpak");
        let resolved = resolve_dir_source(&sources, &manifest_dir).unwrap();
        assert_eq!(resolved, PathBuf::from("/proj/build-aux/flatpak/../../."));
    }
}
