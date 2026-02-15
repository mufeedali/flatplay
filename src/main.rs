use clap::{CommandFactory, Parser, Subcommand};
use nix::unistd::{getpid, setpgid};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

mod build_dirs;
mod command;
mod flatpak_manager;
mod instance_lock;
mod manifest;
mod state;
mod utils;

use flatpak_manager::FlatpakManager;
use instance_lock::{InstanceLock, request_shutdown_from_lock};
use state::State;
use utils::verbose;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub(crate) fn is_interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

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
    verbose("Not in a git repository, using current directory as base");
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

fn run(command: Option<Commands>) -> anyhow::Result<()> {
    // Suppress default SIGINT exit so errors propagate through the return
    // path, ensuring InstanceLock drops and cleans up the lock file.
    ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
    })?;

    let base_dir = get_base_dir();
    let mut state = State::load(base_dir.clone())?;

    if let Some(Commands::Stop) = &command {
        request_shutdown_from_lock(&base_dir)?;
        return Ok(());
    }

    let requires_build_runtime = !matches!(
        command,
        Some(Commands::SelectManifest { .. } | Commands::Clean)
    );
    if requires_build_runtime {
        check_dependencies()?;
    }

    let pid = getpid();
    setpgid(pid, pid)
        .map_err(|error| anyhow::anyhow!("Failed to set process group ID: {error}"))?;
    let process_group_id = pid.as_raw() as u32;

    let _instance_lock = InstanceLock::acquire_or_takeover(&base_dir, process_group_id)?;

    match command {
        Some(Commands::SelectManifest { path }) => {
            let mut flatpak_manager = FlatpakManager::new(&mut state)?;
            flatpak_manager.select_manifest(path)
        }
        Some(Commands::Clean) => {
            let mut flatpak_manager = FlatpakManager::new(&mut state)?;
            flatpak_manager.clean()
        }
        None => {
            let mut flatpak_manager = FlatpakManager::new(&mut state)?;
            flatpak_manager.ensure_ready(true)?;
            flatpak_manager.build_and_run()
        }
        _ => {
            let mut flatpak_manager = FlatpakManager::new(&mut state)?;
            flatpak_manager.ensure_ready(false)?;
            match command {
                Some(Commands::Build) => flatpak_manager.build(),
                Some(Commands::BuildAndRun) => flatpak_manager.build_and_run(),
                Some(Commands::Rebuild) => flatpak_manager.rebuild(),
                Some(Commands::Run) => flatpak_manager.run(),
                Some(Commands::UpdateDependencies) => flatpak_manager.update_dependencies(),
                Some(Commands::RuntimeTerminal) => flatpak_manager.runtime_terminal(),
                Some(Commands::BuildTerminal) => flatpak_manager.build_terminal(),
                Some(Commands::ExportBundle) => flatpak_manager.export_bundle(),
                Some(
                    Commands::SelectManifest { .. }
                    | Commands::Clean
                    | Commands::Completions { .. }
                    | Commands::Stop,
                )
                | None => unreachable!(),
            }
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    utils::set_verbose(cli.verbose);

    match cli.command {
        Some(Commands::Completions { shell }) => {
            use clap_complete::generate;
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, "flatplay", &mut std::io::stdout());
            ExitCode::SUCCESS
        }
        command => {
            if let Err(error) = run(command) {
                // Check if this was an intentional interruption (Ctrl+C)
                if crate::command::is_interrupted_error(&error) {
                    eprintln!();
                    utils::status_info("Interrupted");
                    return ExitCode::from(130);
                }
                utils::status_error(format!("Error: {error}"));
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
    }
}
