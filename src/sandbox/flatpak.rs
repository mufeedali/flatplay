//! Sandbox via the `flatpak` CLI (`flatpak build`), not `flatpak-builder` or raw `bwrap`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{RunSpec, SandboxRunner};
use crate::command::run_command;
use crate::utils::path_to_str;

/// Runs commands inside a Flatpak build directory with `flatpak build`.
#[derive(Debug, Clone, Default)]
pub struct FlatpakRunner {
    /// Project workspace (used as the host cwd for `flatpak` invocations).
    pub workspace: PathBuf,
}

impl FlatpakRunner {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
        }
    }

    /// Build argv for `flatpak` (without the program name): `build [flags…] DIR CMD…`.
    pub fn build_argv(spec: &RunSpec) -> Result<Vec<String>> {
        let mut args = vec!["build".to_string()];

        if spec.share_network {
            args.push("--share=network".into());
        }

        for (key, value) in &spec.env {
            args.push(format!("--env={key}={value}"));
        }

        for flag in &spec.filesystem_binds {
            if flag.starts_with("--") {
                args.push(flag.clone());
            } else {
                args.push(format!("--filesystem={flag}"));
            }
        }

        for extra in &spec.extra_args {
            args.push(extra.clone());
        }

        if let Some(cwd) = &spec.cwd {
            let cwd_s = path_to_str(cwd)?;
            // Ensure the working directory is visible inside the sandbox.
            let already = args.iter().any(|a| {
                a == &format!("--filesystem={cwd_s}")
                    || a.starts_with(&format!("--filesystem={cwd_s}:"))
            });
            if !already {
                args.push(format!("--filesystem={cwd_s}"));
            }
        }

        args.push(path_to_str(&spec.repo_dir)?.to_string());

        // `flatpak build` has no --chdir; wrap with a small shell when cwd is set.
        if let Some(cwd) = &spec.cwd {
            let cwd_s = path_to_str(cwd)?;
            args.push("/bin/sh".into());
            args.push("-c".into());
            args.push("cd \"$1\" && shift && exec \"$@\"".into());
            args.push("_".into());
            args.push(cwd_s.into());
            args.extend(spec.argv.iter().cloned());
        } else {
            args.extend(spec.argv.iter().cloned());
        }

        Ok(args)
    }
}

impl SandboxRunner for FlatpakRunner {
    fn run(&self, spec: &RunSpec) -> Result<()> {
        let args = Self::build_argv(spec)?;
        let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
        run_command("flatpak", &args_ref, Some(self.workspace.as_path()))
            .with_context(|| format!("flatpak build failed for {:?}", spec.argv))
    }
}

/// Initialize a build directory with `flatpak build-init`.
pub fn ensure_build_initialized(
    repo_dir: &Path,
    app_id: &str,
    sdk: &str,
    runtime: &str,
    runtime_version: &str,
    cwd: Option<&Path>,
) -> Result<()> {
    let metadata = repo_dir.join("metadata");
    let files = repo_dir.join("files");
    let var = repo_dir.join("var");
    if metadata.is_file() && files.is_dir() && var.is_dir() {
        return Ok(());
    }

    crate::utils::status("Initializing build environment...");
    let repo = path_to_str(repo_dir)?;
    run_command(
        "flatpak",
        &["build-init", repo, app_id, sdk, runtime, runtime_version],
        cwd,
    )
    .with_context(|| {
        format!(
            "flatpak build-init failed — is the SDK/runtime installed?\n\
             Try: flatpak install {sdk}//{runtime_version}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_argv_basic() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let args = FlatpakRunner::build_argv(&RunSpec {
            repo_dir: repo.clone(),
            argv: vec!["meson".into(), "setup".into()],
            env: vec![("FOO".into(), "bar".into())],
            filesystem_binds: vec![format!("--filesystem=/tmp/ws")],
            extra_args: vec![],
            share_network: true,
            cwd: None,
        })
        .unwrap();

        assert_eq!(args[0], "build");
        assert!(args.contains(&"--share=network".to_string()));
        assert!(args.contains(&"--env=FOO=bar".to_string()));
        assert!(args.contains(&"--filesystem=/tmp/ws".to_string()));
        assert!(args.iter().any(|a| a.ends_with("repo")));
        assert_eq!(&args[args.len() - 2..], ["meson", "setup"]);
    }

    #[test]
    fn build_argv_with_cwd_wraps_shell() {
        let dir = tempdir().unwrap();
        let repo = dir.path().join("repo");
        let cwd = dir.path().join("src");
        let args = FlatpakRunner::build_argv(&RunSpec {
            repo_dir: repo,
            argv: vec!["make".into()],
            env: vec![],
            filesystem_binds: vec![],
            extra_args: vec![],
            share_network: false,
            cwd: Some(cwd.clone()),
        })
        .unwrap();

        assert!(!args.iter().any(|a| a == "--share=network"));
        assert!(args.iter().any(|a| a == "/bin/sh"));
        assert!(
            args.iter()
                .any(|a| a == "cd \"$1\" && shift && exec \"$@\"")
        );
        assert!(args.last().map(String::as_str) == Some("make"));
    }
}
