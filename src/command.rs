use std::path::Path;
use std::process::{Command, Stdio};

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

// Returns true if running inside a container like Toolbx or distrobox.
fn is_inside_container() -> bool {
    Path::new("/run/.containerenv").exists()
}

fn command_succeeds(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

// Runs a command, handling Flatpak sandbox and container specifics.
pub fn run_command(command: &str, args: &[&str], working_dir: Option<&Path>) -> Result<()> {
    let mut command_args = args.to_vec();

    // Workaround for rofiles-fuse issues in containers.
    if command == "flatpak-builder"
        && is_inside_container()
        && !command_args.contains(&"--disable-rofiles-fuse")
    {
        verbose("Detected container, adding --disable-rofiles-fuse");
        command_args.push("--disable-rofiles-fuse");
    }

    let (program, final_args) = if is_sandboxed() {
        if command_succeeds("host-spawn", &["--version"]) {
            verbose("Detected Flatpak sandbox, using host-spawn");
            let mut new_args = vec![command];
            new_args.extend_from_slice(&command_args);
            ("host-spawn", new_args)
        } else {
            verbose("Detected Flatpak sandbox, using flatpak-spawn");
            let mut new_args = vec![
                "--host",
                "--watch-bus",
                "--env=TERM=xterm-256color",
                command,
            ];
            new_args.extend_from_slice(&command_args);
            ("flatpak-spawn", new_args)
        }
    } else {
        (command, command_args)
    };

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
                    use std::os::unix::process::ExitStatusExt;
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

// Legacy helper removed: dependency builds are in-process (see `builder` + `sandbox::BwrapRunner`).
