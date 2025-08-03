use anyhow::Result;
use colored::*;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

use crate::state::State;

/// Checks if a process with the given PID is currently running.
pub fn is_process_running(pgid: u32) -> bool {
    !matches!(kill(Pid::from_raw(pgid as i32), None), Err(Errno::ESRCH))
}

/// Kills the process group associated with the last running flatplay instance.
pub fn kill_process_group(state: &mut State) -> Result<()> {
    let Some(pgid) = state.process_group_id.take() else {
        println!("{} No running flatplay process found.", "ℹ".blue());
        return Ok(());
    };

    if is_process_running(pgid) {
        let pgid_raw = Pid::from_raw(pgid as i32);
        nix::sys::signal::killpg(pgid_raw, Signal::SIGTERM)?;
        println!(
            "{} Successfully stopped flatplay process group (PGID: {})",
            "✔".green(),
            pgid
        );
    } else {
        println!(
            "{} No running flatplay process found (stale PGID: {}). Cleaning up.",
            "⚠".yellow(),
            pgid
        );
    }

    // Reset process group ID in the state
    state.process_group_id = None;
    state.save()?;
    Ok(())
}
