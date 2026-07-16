//! Sandbox execution via the **`flatpak` CLI** (`flatpak build` / `build-init`).
//!
//! Dependency modules and app sources are still built in-process; the Flatpak CLI
//! provides the SDK sandbox and lifecycle. **`flatpak-builder` is not used.**

mod flatpak;

pub use flatpak::{FlatpakRunner, ensure_build_initialized};

use std::path::PathBuf;

use anyhow::Result;

/// Specification for a sandboxed command.
#[derive(Debug, Clone)]
pub struct RunSpec {
    /// Build directory (metadata / files / var) — the `flatpak build` DIRECTORY.
    pub repo_dir: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Flatpak-style flags (`--filesystem=…`, `--bind-mount=…`, finish-args, …).
    pub filesystem_binds: Vec<String>,
    pub extra_args: Vec<String>,
    pub share_network: bool,
    pub cwd: Option<PathBuf>,
}

pub trait SandboxRunner {
    fn run(&self, spec: &RunSpec) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use std::process::ExitStatus;
    use tempfile::tempdir;

    /// Host runner for unit tests only.
    #[derive(Debug, Default)]
    struct HostRunner;

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
