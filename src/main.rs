use clap::{CommandFactory, Parser, Subcommand};
use colored::*;
use nix::unistd::{getpid, setpgid};
use std::path::PathBuf;
use std::process::{Command, Stdio, exit};

mod build_dirs;
mod command;
mod flatpak_manager;
mod manifest;
mod process;
mod state;
mod utils;

use flatpak_manager::FlatpakManager;
use process::{PgidGuard, is_process_running, kill_process_group};
use state::State;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize a Flatpak build, update the dependencies & build them
    Build,
    /// Build or rebuild the application then run it
    BuildAndRun,
    /// Clean the Flatpak repo directory and rebuild the application
    Rebuild,
    /// Stop the currently running task
    Stop,
    /// Run the application
    Run,
    /// Download/Update the dependencies and builds them
    UpdateDependencies,
    /// Clean the Flatpak repo directory
    Clean,
    /// Spawn a new terminal inside the specified SDK
    RuntimeTerminal,
    /// Spawn a new terminal inside the current build repository
    BuildTerminal,
    /// Export .flatpak bundle from the build
    ExportBundle,
    /// Select or change the active manifest
    SelectManifest {
        /// Path to the manifest file to select
        path: Option<PathBuf>,
    },
    /// Generate shell completion scripts for your shell
    Completions {
        /// The shell to generate completions for (e.g., bash, zsh, fish)
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

macro_rules! handle_command {
    ($command:expr) => {
        if let Err(err) = $command {
            eprintln!("{}: {}", "Error".red(), err);
        }
    };
}

fn get_base_dir() -> PathBuf {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output();

    if let Ok(output) = output
        && output.status.success()
    {
        return PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    }
    PathBuf::from(".")
}

fn check_dependencies() -> anyhow::Result<()> {
    let required = [("git", "git"), ("flatpak", "flatpak")];
    let mut missing = Vec::new();

    for (cmd, name) in &required {
        if Command::new(cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            missing.push(*name);
        }
    }

    if Command::new("flatpak-builder")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
        && Command::new("flatpak")
            .args(["run", "org.flatpak.Builder", "--version"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
    {
        missing.push("flatpak-builder or org.flatpak.Builder");
    }

    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "Missing required dependencies: {}",
            missing.join(", ")
        ));
    }

    Ok(())
}

fn main() {
    let cli = Cli::parse();

    if let Some(Commands::Completions { shell }) = cli.command {
        use clap_complete::generate;
        use std::io;
        let mut cmd = Cli::command();
        generate(shell, &mut cmd, "flatplay", &mut io::stdout());
        return;
    }

    if let Err(err) = check_dependencies() {
        eprintln!("{}: {}", "Error".red(), err);
        exit(1);
    }

    let base_dir = get_base_dir();
    let mut state = State::load(base_dir.clone()).unwrap();

    if cli.command.is_none() {
        handle_command!(kill_process_group(&mut state));
    } else if let Some(Commands::Stop) = cli.command {
        handle_command!(kill_process_group(&mut state));
        return;
    }

    if let Some(pgid) = state.process_group_id
        && is_process_running(pgid)
    {
        eprintln!(
            "{}: Another instance of flatplay is already running (PID: {}).",
            "Error".red(),
            pgid
        );
        eprintln!("Run '{}' to terminate it.", "flatplay stop".bold().italic());
        return;
    }

    let pid = getpid();
    if let Err(e) = setpgid(pid, pid) {
        eprintln!("Failed to set process group ID: {e}");
        return;
    }
    state.process_group_id = Some(pid.as_raw() as u32);

    let mut flatpak_manager = match FlatpakManager::new(&mut state) {
        Ok(manager) => manager,
        Err(e) => {
            eprintln!("{}: {}", "Error".red(), e);
            exit(1);
        }
    };
    let _guard = PgidGuard::new(base_dir);

    match &cli.command {
        Some(Commands::Completions { .. }) => unreachable!(),
        Some(Commands::Stop) => {}

        Some(Commands::Build) => handle_command!(flatpak_manager.build()),
        Some(Commands::BuildAndRun) => handle_command!(flatpak_manager.build_and_run()),
        Some(Commands::Rebuild) => handle_command!(flatpak_manager.rebuild()),
        Some(Commands::Run) => handle_command!(flatpak_manager.run()),
        Some(Commands::UpdateDependencies) => {
            handle_command!(flatpak_manager.update_dependencies())
        }
        Some(Commands::Clean) => handle_command!(flatpak_manager.clean()),
        Some(Commands::RuntimeTerminal) => handle_command!(flatpak_manager.runtime_terminal()),
        Some(Commands::BuildTerminal) => handle_command!(flatpak_manager.build_terminal()),
        Some(Commands::ExportBundle) => handle_command!(flatpak_manager.export_bundle()),
        Some(Commands::SelectManifest { path }) => {
            handle_command!(flatpak_manager.select_manifest(path.clone()))
        }
        None => handle_command!(flatpak_manager.build_and_run()),
    }
}
