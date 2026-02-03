use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::Result;
use colored::*;

// Returns true if running inside a Flatpak sandbox.
fn is_sandboxed() -> bool {
    Path::new("/.flatpak-info").exists()
}

// Returns true if running inside a container like Toolbx or distrobox.
fn is_inside_container() -> bool {
    Path::new("/run/.containerenv").exists()
}

// Returns true if the given command with arguments executes successfully.
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
        command_args.push("--disable-rofiles-fuse");
    }

    let (program, final_args) = if is_sandboxed() {
        if command_succeeds("host-spawn", &["--version"]) {
            let mut new_args = vec![command];
            new_args.extend_from_slice(&command_args);
            ("host-spawn", new_args)
        } else {
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

    println!(
        "\n{} {} {}",
        ">".purple().bold(),
        program.italic(),
        final_args.join(" ").italic()
    );
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
        return Err(anyhow::anyhow!(
            "Command failed with exit code: {}",
            status.code().unwrap_or(1)
        ));
    }

    Ok(())
}

// Runs flatpak-builder, preferring the native binary, then the Flatpak app.
pub fn flatpak_builder(args: &[&str], working_dir: Option<&Path>) -> Result<()> {
    if command_succeeds("flatpak-builder", &["--version"]) {
        run_command("flatpak-builder", args, working_dir)
    } else if command_succeeds("flatpak", &["run", "org.flatpak.Builder", "--version"]) {
        let mut new_args = vec!["run", "org.flatpak.Builder"];
        new_args.extend_from_slice(args);
        run_command("flatpak", &new_args, working_dir)
    } else {
        Err(anyhow::anyhow!(
            "Flatpak builder not found. Please install either `flatpak-builder` from your distro repositories or `org.flatpak.Builder` through `flatpak install`."
        ))
    }
}
