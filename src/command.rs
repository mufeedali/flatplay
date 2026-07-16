use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

use crate::utils::{command_header, verbose};
use anyhow::Result;

#[derive(Debug)]
pub struct InterruptedError;

impl std::fmt::Display for InterruptedError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Command interrupted")
    }
}

impl std::error::Error for InterruptedError {}

pub fn is_interrupted_error(error: &anyhow::Error) -> bool {
    error.is::<InterruptedError>()
}

fn is_sandboxed() -> bool {
    Path::new("/.flatpak-info").exists()
}

fn command_succeeds(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// How to escape a Flatpak sandbox when flatplay itself runs inside one.
#[derive(Clone, Copy, Debug)]
enum HostEscape {
    None,
    HostSpawn,
    FlatpakSpawn,
}

fn host_escape() -> HostEscape {
    static ESCAPE: OnceLock<HostEscape> = OnceLock::new();
    *ESCAPE.get_or_init(|| {
        if !is_sandboxed() {
            return HostEscape::None;
        }
        if command_succeeds("host-spawn", &["--version"]) {
            verbose("Detected Flatpak sandbox, using host-spawn");
            HostEscape::HostSpawn
        } else {
            verbose("Detected Flatpak sandbox, using flatpak-spawn");
            HostEscape::FlatpakSpawn
        }
    })
}

fn resolve_host_argv<'a>(command: &'a str, args: &'a [&'a str]) -> (&'a str, Vec<&'a str>) {
    match host_escape() {
        HostEscape::None => (command, args.to_vec()),
        HostEscape::HostSpawn => {
            let mut new_args = vec![command];
            new_args.extend_from_slice(args);
            ("host-spawn", new_args)
        }
        HostEscape::FlatpakSpawn => {
            let mut new_args = vec![
                "--host",
                "--watch-bus",
                "--env=TERM=xterm-256color",
                command,
            ];
            new_args.extend_from_slice(args);
            ("flatpak-spawn", new_args)
        }
    }
}

/// Runs a host command, escaping a Flatpak sandbox via host-spawn / flatpak-spawn when needed.
pub fn run_command(command: &str, args: &[&str], working_dir: Option<&Path>) -> Result<()> {
    let (program, final_args) = resolve_host_argv(command, args);

    command_header(program, &final_args);
    let mut cmd = Command::new(program);
    cmd.args(&final_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }
    let mut command_process = cmd.spawn()?;

    let status = command_process.wait()?;

    if !status.success() {
        // Exit code 130 = 128 + SIGINT(2), standard for interrupted by Ctrl+C
        if status.code() == Some(130) || crate::is_interrupted() {
            return Err(InterruptedError.into());
        }
        let code = status.code().map_or_else(
            || {
                #[cfg(unix)]
                {
                    format!("signal {}", status.signal().unwrap_or(0))
                }
                #[cfg(not(unix))]
                {
                    "unknown".to_string()
                }
            },
            |code| code.to_string(),
        );
        return Err(anyhow::anyhow!("Command failed with exit code: {code}"));
    }

    Ok(())
}

/// Capture stdout of a host command (with the same sandbox escape as `run_command`).
pub fn run_command_output(command: &str, args: &[&str]) -> Result<std::process::Output> {
    let (program, final_args) = resolve_host_argv(command, args);
    command_header(program, &final_args);
    Command::new(program)
        .args(&final_args)
        .output()
        .map_err(Into::into)
}

/// True if the host can run `command` (sandbox-aware).
pub fn host_command_exists(command: &str, probe_args: &[&str]) -> bool {
    let (program, final_args) = resolve_host_argv(command, probe_args);
    Command::new(program)
        .args(&final_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
