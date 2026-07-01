use anyhow::Context;
use clap::{CommandFactory, Parser, Subcommand};
use nix::unistd::{getpid, setpgid};
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

mod build_dirs;
mod builder;
mod command;
mod flatpak_manager;
mod git_source;
mod instance_lock;
mod manifest;
mod sandbox;
mod sources;
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

/// Resolve the project base directory by walking parents for a `.git` entry.
/// Falls back to the current working directory (no `git` binary required).
fn get_base_dir() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("Failed to get current directory")?;
    let mut dir = cwd.as_path();
    loop {
        let git_entry = dir.join(".git");
        if git_entry.exists() {
            return dir
                .canonicalize()
                .context("Failed to resolve base directory");
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    verbose("Not in a git repository, using current directory as base");
    cwd.canonicalize()
        .context("Failed to resolve base directory")
}

fn check_dependencies() -> anyhow::Result<()> {
    // Control plane: bubblewrap only. Flatpak *runtimes/SDKs* must exist on disk
    // (user/system install tree); we never invoke the `flatpak` or `flatpak-builder` CLIs.
    let mut missing = Vec::new();

    if Command::new("bwrap")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_err()
    {
        missing.push("bubblewrap (bwrap)");
    }

    if !missing.is_empty() {
        return Err(anyhow::anyhow!(
            "Missing required dependencies: {}",
            missing.join(", ")
        ));
    }

    Ok(())
}

fn run(command: Option<&Commands>) -> anyhow::Result<()> {
    ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
    })?;

    let base_dir = get_base_dir()?;
    let mut state = State::load(&base_dir)?;

    if matches!(&command, Some(Commands::Stop)) {
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

    if let Some(Commands::SelectManifest { path }) = &command {
        request_shutdown_from_lock(&base_dir)?;
        let mut flatpak_manager = FlatpakManager::new(&mut state);
        return flatpak_manager.select_manifest(path.clone());
    }

    if matches!(&command, Some(Commands::Clean)) {
        request_shutdown_from_lock(&base_dir)?;
        let mut flatpak_manager = FlatpakManager::new(&mut state);
        return flatpak_manager.clean();
    }

    let mut flatpak_manager = FlatpakManager::new(&mut state);
    flatpak_manager.validate_manifest(command.is_none())?;

    let pid = getpid();
    setpgid(pid, pid)
        .map_err(|error| anyhow::anyhow!("Failed to set process group ID: {error}"))?;
    let process_group_id = pid.as_raw().cast_unsigned();

    let _instance_lock = InstanceLock::acquire_or_takeover(&base_dir, process_group_id)?;

    flatpak_manager.ensure_ready(command.is_none())?;

    match command {
        None | Some(Commands::BuildAndRun) => flatpak_manager.build_and_run(),
        Some(Commands::Build) => flatpak_manager.build(),
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
        ) => unreachable!(),
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
            if let Err(error) = run(command.as_ref()) {
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
