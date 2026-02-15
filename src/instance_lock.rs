use anyhow::{Context, Result};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::utils::{status_info, status_success, status_warn, verbose};

const STATE_DIR: &str = ".flatplay";
const LOCK_FILE_NAME: &str = "instance.lock";
const TAKEOVER_WAIT: Duration = Duration::from_secs(5);
const TAKEOVER_POLL: Duration = Duration::from_millis(100);

#[derive(Debug, Serialize, Deserialize)]
struct InstanceMetadata {
    process_id: u32,
    process_group_id: u32,
    process_start_time_ticks: u64,
}

pub struct InstanceLock {
    file: Flock<File>,
}

impl InstanceLock {
    pub fn acquire_or_takeover(base_dir: &Path, process_group_id: u32) -> Result<Self> {
        let lock_file_path = lock_file_path(base_dir);
        if let Some(parent) = lock_file_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_file_path)
            .with_context(|| {
                format!(
                    "Failed to open instance lock file at {}",
                    lock_file_path.display()
                )
            })?;

        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file) => {
                let mut lock = Self { file };
                lock.write_current_metadata(process_group_id)?;
                Ok(lock)
            }
            Err((file, Errno::EWOULDBLOCK)) => {
                verbose("Instance lock is held by another process; requesting takeover.");
                request_shutdown_from_lock(base_dir)?;

                let deadline = Instant::now() + TAKEOVER_WAIT;
                let mut unlocked_file = file;
                loop {
                    match Flock::lock(unlocked_file, FlockArg::LockExclusiveNonblock) {
                        Ok(file) => {
                            let mut lock = Self { file };
                            lock.write_current_metadata(process_group_id)?;
                            return Ok(lock);
                        }
                        Err((file, Errno::EWOULDBLOCK)) => {
                            if Instant::now() >= deadline {
                                anyhow::bail!(
                                    "Could not acquire flatplay instance lock within {}s.",
                                    TAKEOVER_WAIT.as_secs()
                                );
                            }
                            unlocked_file = file;
                            thread::sleep(TAKEOVER_POLL);
                        }
                        Err((_file, error)) => {
                            return Err(anyhow::anyhow!(
                                "Failed to acquire instance lock: {error}"
                            ));
                        }
                    }
                }
            }
            Err((_file, error)) => Err(anyhow::anyhow!("Failed to acquire instance lock: {error}")),
        }
    }

    fn write_current_metadata(&mut self, process_group_id: u32) -> Result<()> {
        let process_id = std::process::id();
        let metadata = InstanceMetadata {
            process_id,
            process_group_id,
            process_start_time_ticks: process_start_time_ticks(process_id)?,
        };

        let file = &mut *self.file;
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        serde_json::to_writer_pretty(&mut *file, &metadata)?;
        file.write_all(b"\n")?;
        file.sync_data()?;
        Ok(())
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        if let Err(error) = self.file.set_len(0) {
            verbose(format!("Failed to clear instance lock metadata: {error}"));
        }
    }
}

pub fn request_shutdown_from_lock(base_dir: &Path) -> Result<()> {
    let lock_file_path = lock_file_path(base_dir);
    let Some(previous_instance) = read_metadata(&lock_file_path)? else {
        status_info("No running flatplay process found.");
        return Ok(());
    };

    if !is_same_process_instance_running(&previous_instance) {
        status_warn("No running flatplay process found (stale lock metadata). Cleaning up.");
        clear_lock_metadata(&lock_file_path)?;
        return Ok(());
    }

    let process_group = Pid::from_raw(previous_instance.process_group_id as i32);
    match killpg(process_group, Signal::SIGTERM) {
        Ok(_) => {
            status_success(format!(
                "Successfully stopped flatplay process group (PGID: {})",
                previous_instance.process_group_id
            ));
            Ok(())
        }
        Err(Errno::ESRCH) => {
            status_warn("No running flatplay process found (stale lock metadata). Cleaning up.");
            clear_lock_metadata(&lock_file_path)?;
            Ok(())
        }
        Err(error) => Err(anyhow::anyhow!(
            "Failed to terminate existing flatplay instance: {error}"
        )),
    }
}

fn lock_file_path(base_dir: &Path) -> PathBuf {
    base_dir.join(STATE_DIR).join(LOCK_FILE_NAME)
}

fn clear_lock_metadata(lock_file_path: &Path) -> Result<()> {
    if !lock_file_path.exists() {
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .write(true)
        .open(lock_file_path)
        .with_context(|| format!("Failed to open lock file at {}", lock_file_path.display()))?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}

fn read_metadata(lock_file_path: &Path) -> Result<Option<InstanceMetadata>> {
    if !lock_file_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(lock_file_path)
        .with_context(|| format!("Failed to read lock file at {}", lock_file_path.display()))?;

    if content.trim().is_empty() {
        return Ok(None);
    }

    match serde_json::from_str(&content) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) => {
            verbose(format!(
                "Could not parse instance lock metadata: {error}. Proceeding without takeover signal."
            ));
            Ok(None)
        }
    }
}

fn is_same_process_instance_running(metadata: &InstanceMetadata) -> bool {
    let Ok(current_start_time) = process_start_time_ticks(metadata.process_id) else {
        return false;
    };

    current_start_time == metadata.process_start_time_ticks
}

fn process_start_time_ticks(process_id: u32) -> Result<u64> {
    let stat_path = format!("/proc/{process_id}/stat");
    let stat = fs::read_to_string(&stat_path)
        .with_context(|| format!("Failed to read process stat file at {stat_path}"))?;
    parse_start_time_ticks_from_stat(&stat)
}

fn parse_start_time_ticks_from_stat(stat_line: &str) -> Result<u64> {
    let Some(right_parenthesis_index) = stat_line.rfind(')') else {
        anyhow::bail!("Process stat line is malformed.");
    };

    let remaining = stat_line
        .get(right_parenthesis_index + 2..)
        .context("Process stat line missing fields after command name")?;

    let fields: Vec<&str> = remaining.split_whitespace().collect();
    let start_time_field = fields
        .get(19)
        .context("Process stat line missing start time field")?;

    start_time_field
        .parse::<u64>()
        .context("Failed to parse process start time field")
}

#[cfg(test)]
mod tests {
    use super::parse_start_time_ticks_from_stat;

    #[test]
    fn parses_start_time_from_stat_line() {
        let stat = "12345 (flatplay) S 1 12345 12345 0 -1 4194560 100 0 0 0 3 1 0 0 20 0 1 0 987654 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let start_time = parse_start_time_ticks_from_stat(stat)
            .expect("test stat line should include a valid start time");
        assert_eq!(start_time, 987654);
    }
}
