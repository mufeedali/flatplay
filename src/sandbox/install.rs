//! Locate installed Flatpak runtimes/SDKs on disk (no `flatpak` CLI).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::sources::flatpak_arch;

/// A resolved runtime or SDK deployment.
#[derive(Debug, Clone)]
pub struct DeployedRef {
    pub id: String,
    pub arch: String,
    pub branch: String,
    /// `.../active` directory.
    pub active_dir: PathBuf,
    /// `.../active/files` — bind this at `/usr` in the sandbox.
    pub files_dir: PathBuf,
    pub metadata_path: PathBuf,
}

impl DeployedRef {
    pub fn ref_string(&self) -> String {
        format!("{}/{}/{}", self.id, self.arch, self.branch)
    }
}

/// Search user then system Flatpak installations for a runtime/SDK.
pub fn find_deployed(id: &str, branch: &str) -> Result<DeployedRef> {
    let arch = flatpak_arch();
    let mut tried = Vec::new();
    for root in installation_roots() {
        let active = root
            .join("runtime")
            .join(id)
            .join(&arch)
            .join(branch)
            .join("active");
        tried.push(active.display().to_string());
        let files = active.join("files");
        let metadata = active.join("metadata");
        if files.is_dir() && metadata.is_file() {
            return Ok(DeployedRef {
                id: id.to_string(),
                arch: arch.clone(),
                branch: branch.to_string(),
                active_dir: active,
                files_dir: files,
                metadata_path: metadata,
            });
        }
    }
    anyhow::bail!(
        "Flatpak ref {id}/{arch}/{branch} not found on disk (looked in: {}).\n\
         Install the SDK/runtime (e.g. via GNOME Software or `flatpak install`) — \
         flatplay reads the install tree and does not invoke the flatpak CLI.",
        tried.join(", ")
    )
}

pub fn installation_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/flatpak"));
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        let p = PathBuf::from(xdg).join("flatpak");
        if !roots.contains(&p) {
            roots.push(p);
        }
    }
    roots.push(PathBuf::from("/var/lib/flatpak"));
    roots
}

/// Parse a flatpak-style version string from an on-disk metadata file if present.
pub fn read_flatpak_version_from_lib() -> Option<String> {
    // Best-effort: package managers may ship a version file; not required for builds.
    None
}

pub fn ensure_sdk_and_runtime(sdk: &str, runtime: &str, branch: &str) -> Result<(DeployedRef, DeployedRef)> {
    let sdk_ref = find_deployed(sdk, branch)
        .with_context(|| format!("SDK {sdk}//{branch} is required"))?;
    let runtime_ref = find_deployed(runtime, branch)
        .with_context(|| format!("Runtime {runtime}//{branch} is required"))?;
    Ok((sdk_ref, runtime_ref))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installation_roots_include_user_and_system() {
        let roots = installation_roots();
        assert!(roots.iter().any(|r| r.ends_with("flatpak")));
        assert!(roots.iter().any(|r| r == Path::new("/var/lib/flatpak")));
    }
}
