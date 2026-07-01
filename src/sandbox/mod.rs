//! Sandbox execution — **no `flatpak` / `flatpak-builder` CLI**.
//!
//! Runtimes/SDKs are located on disk under the Flatpak install tree; commands run
//! via bubblewrap (`bwrap`).

mod bwrap;
mod install;

pub use bwrap::BwrapRunner;
pub use install::{DeployedRef, ensure_sdk_and_runtime, find_deployed, installation_roots};

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{Context, Result};

use crate::utils::{status, verbose};

/// Specification for a sandboxed command.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Build directory (metadata / files / var).
    pub repo_dir: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Host filesystem exposes and permission-like flags (`--filesystem=…`, `--socket=…`, …).
    pub filesystem_binds: Vec<String>,
    pub extra_args: Vec<String>,
    pub share_network: bool,
    pub cwd: Option<PathBuf>,
}

pub trait SandboxRunner {
    fn run(&self, spec: &RunSpec) -> Result<()>;
}

/// Create build dir layout without the `flatpak` CLI.
pub fn ensure_build_initialized(
    repo_dir: &Path,
    app_id: &str,
    sdk: &str,
    runtime: &str,
    runtime_version: &str,
    _cwd: Option<&Path>,
) -> Result<()> {
    let metadata = repo_dir.join("metadata");
    let files = repo_dir.join("files");
    let var = repo_dir.join("var");
    if metadata.is_file() && files.is_dir() && var.is_dir() {
        // Still verify SDK/runtime exist so failures are early and clear.
        ensure_sdk_and_runtime(sdk, runtime, runtime_version)?;
        return Ok(());
    }

    status("Initializing build environment...");
    ensure_sdk_and_runtime(sdk, runtime, runtime_version)?;
    write_minimal_build_layout(repo_dir, app_id, sdk, runtime, runtime_version)
}

pub fn write_minimal_build_layout(
    repo_dir: &Path,
    app_id: &str,
    sdk: &str,
    runtime: &str,
    runtime_version: &str,
) -> Result<()> {
    std::fs::create_dir_all(repo_dir.join("files"))?;
    std::fs::create_dir_all(repo_dir.join("var"))?;
    let arch = crate::sources::flatpak_arch();
    let metadata = format!(
        "[Application]\n\
         name={app_id}\n\
         runtime={runtime}/{arch}/{runtime_version}\n\
         sdk={sdk}/{arch}/{runtime_version}\n"
    );
    std::fs::write(repo_dir.join("metadata"), metadata)
        .context("Failed to write build metadata")?;
    verbose(format!(
        "Wrote build metadata for {app_id} using {sdk}//{runtime_version}"
    ));
    Ok(())
}

/// Host runner for unit tests only.
#[derive(Debug, Default)]
pub struct HostRunner;

impl SandboxRunner for HostRunner {
    fn run(&self, spec: &RunSpec) -> Result<()> {
        let program = spec
            .argv
            .first()
            .context("RunSpec argv must not be empty")?;
        let args: Vec<&str> = spec.argv.iter().skip(1).map(String::as_str).collect();
        let mut cmd = std::process::Command::new(program);
        cmd.args(&args);
        for (key, value) in &spec.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        let status: ExitStatus = cmd.status()?;
        if status.success() {
            Ok(())
        } else {
            anyhow::bail!("Host command failed with {status}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn minimal_layout_writes_metadata() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        write_minimal_build_layout(
            &repo,
            "org.example.App",
            "org.gnome.Sdk",
            "org.gnome.Platform",
            "47",
        )
        .unwrap();
        assert!(repo.join("metadata").is_file());
        assert!(repo.join("files").is_dir());
        assert!(repo.join("var").is_dir());
        let meta = std::fs::read_to_string(repo.join("metadata")).unwrap();
        assert!(meta.contains("name=org.example.App"));
        assert!(meta.contains("org.gnome.Sdk"));
    }

    #[test]
    fn host_runner_echo() {
        let runner = HostRunner;
        let dir = tempdir().unwrap();
        runner
            .run(&RunSpec {
                repo_dir: dir.path().to_path_buf(),
                argv: vec!["true".into()],
                env: vec![],
                filesystem_binds: vec![],
                extra_args: vec![],
                share_network: false,
                cwd: None,
            })
            .unwrap();
    }
}
