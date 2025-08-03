use std::panic;
use std::path::PathBuf;
use std::process::Command;

use clap::{CommandFactory, Parser, Subcommand};
use colored::*;
use nix::unistd::{getpid, setpgid};

use flatplay::FlatpakManager;
use flatplay::process::{is_process_running, kill_process_group};
use flatplay::state::State;

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

    if let Ok(output) = output {
        if output.status.success() {
            return PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
        }
    }
    PathBuf::from(".")
}

fn main() {
    let cli = Cli::parse();

    // Handle shell completions first.
    if let Some(Commands::Completions { shell }) = cli.command {
        use clap_complete::generate;
        use std::io;
        let mut cmd = Cli::command();
        generate(shell, &mut cmd, "flatplay", &mut io::stdout());
        return;
    }

    let base_dir = get_base_dir();
    let base_dir_for_panic_hook = base_dir.clone();
    let mut state = State::load(base_dir).unwrap();

    // Handle the "stop" command early.
    if let Some(Commands::Stop) = cli.command {
        handle_command!(kill_process_group(&mut state));
        return;
    }

    // Check if another instance is already running.
    if let Some(pgid) = state.process_group_id {
        if is_process_running(pgid) {
            eprintln!(
                "{}: Another instance of flatplay is already running (PID: {}).",
                "Error".red(),
                pgid
            );
            eprintln!("Run '{}' to terminate it.", "flatplay stop".bold().italic());
            return;
        }
    }

    // Become a process group leader.
    // This also makes the pid the process group ID.
    let pid = getpid();
    if let Err(e) = setpgid(pid, pid) {
        eprintln!("Failed to set process group ID: {e}");
        return;
    }

    // Save the process group ID to the state.
    state.process_group_id = Some(pid.as_raw() as u32);
    if let Err(e) = state.save() {
        eprintln!("Failed to save state: {e}");
        return;
    }

    // Handle unclean ends where possible.
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        if let Ok(mut state) = State::load(base_dir_for_panic_hook.clone()) {
            state.process_group_id = None;
            if let Err(e) = state.save() {
                eprintln!("Failed to save state in panic hook: {e}");
            }
        }
        original_hook(panic_info);
    }));

    let mut flatpak_manager = match FlatpakManager::new(&mut state) {
        Ok(manager) => manager,
        Err(e) => {
            eprintln!("{}: {}", "Error".red(), e);
            std::process::exit(1);
        }
    };

    match &cli.command {
        // Handled earlier.
        Some(Commands::Completions { shell: _ }) => {}
        Some(Commands::Stop) => {}

        Some(Commands::Build) => handle_command!(flatpak_manager.build()),
        Some(Commands::BuildAndRun) => handle_command!(flatpak_manager.build_and_run()),
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

    // Clean up pgid in the state file on normal exit.
    state.process_group_id = None;
    state.save().unwrap();
}
