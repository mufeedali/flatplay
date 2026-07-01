//! Bubblewrap-based sandbox (no `flatpak` / `flatpak-builder` CLI).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use super::install::DeployedRef;
use super::{RunSpec, SandboxRunner};
use crate::command::{InterruptedError, is_interrupted_error};
use crate::utils::{command_header, path_to_str, verbose};

/// Build/run commands inside an SDK or runtime tree via `bwrap`.
#[derive(Debug, Clone)]
pub struct BwrapRunner {
    /// SDK (or runtime when running apps) files directory (`.../active/files`).
    pub usr_files: PathBuf,
    pub app_id: String,
    /// Host paths always exposed to the sandbox (project tree, etc.).
    pub default_filesystems: Vec<PathBuf>,
}

impl BwrapRunner {
    pub fn for_sdk(sdk: &DeployedRef, app_id: impl Into<String>, workspace: &Path) -> Self {
        Self {
            usr_files: sdk.files_dir.clone(),
            app_id: app_id.into(),
            default_filesystems: vec![workspace.to_path_buf()],
        }
    }

    fn base_bwrap_args(&self, repo_dir: &Path, share_network: bool) -> Result<Vec<String>> {
        let usr = path_to_str(&self.usr_files)?;
        let files = repo_dir.join("files");
        let var = repo_dir.join("var");
        std::fs::create_dir_all(&files)?;
        std::fs::create_dir_all(&var)?;

        let mut args = vec![
            "--die-with-parent".into(),
            "--unshare-pid".into(),
            "--unshare-uts".into(),
            "--unshare-cgroup-try".into(),
            "--proc".into(),
            "/proc".into(),
            "--dev".into(),
            "/dev".into(),
            "--tmpfs".into(),
            "/tmp".into(),
            "--tmpfs".into(),
            "/run".into(),
            "--ro-bind".into(),
            usr.into(),
            "/usr".into(),
            "--symlink".into(),
            "usr/bin".into(),
            "/bin".into(),
            "--symlink".into(),
            "usr/sbin".into(),
            "/sbin".into(),
            "--symlink".into(),
            "usr/lib".into(),
            "/lib".into(),
        ];

        // Multi-arch SDK layouts often provide lib64.
        if self.usr_files.join("lib64").is_dir() {
            args.extend([
                "--symlink".into(),
                "usr/lib64".into(),
                "/lib64".into(),
            ]);
        }

        // Writable tmpfs /etc so we can add host resolv.conf (cannot overlay files onto a
        // read-only SDK /etc bind — bwrap fails with "Can't create file at /etc/resolv.conf").
        args.extend(["--tmpfs".into(), "/etc".into()]);
        let sdk_etc = self.usr_files.join("etc");
        if sdk_etc.is_dir() {
            // Expose useful SDK etc trees (ssl certs, fonts, …). Skip dangling symlinks
            // (e.g. mtab) via --ro-bind-try.
            if let Ok(entries) = std::fs::read_dir(&sdk_etc) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    // Skip files we override from the host.
                    if matches!(name_str, "resolv.conf" | "hosts" | "mtab") {
                        continue;
                    }
                    let src = entry.path();
                    // Path::exists follows symlinks; skip broken ones.
                    if src.is_symlink() && !src.exists() {
                        continue;
                    }
                    let dest = format!("/etc/{name_str}");
                    args.extend([
                        "--ro-bind-try".into(),
                        path_to_str(&src)?.into(),
                        dest,
                    ]);
                }
            }
        }
        for host_file in ["/etc/resolv.conf", "/etc/hosts"] {
            if Path::new(host_file).exists() {
                args.extend([
                    "--ro-bind-try".into(),
                    host_file.into(),
                    host_file.into(),
                ]);
            }
        }

        // Mirror the environment Flatpak injects so apps find data under /app/share
        // (e.g. GLib.get_system_data_dirs() → wordbook/wn-*.db.zst).
        args.extend([
            "--bind".into(),
            path_to_str(&files)?.into(),
            "/app".into(),
            "--bind".into(),
            path_to_str(&var)?.into(),
            "/var".into(),
            "--setenv".into(),
            "PATH".into(),
            "/app/bin:/usr/bin".into(),
            "--setenv".into(),
            "LD_LIBRARY_PATH".into(),
            "/app/lib".into(),
            "--setenv".into(),
            "PKG_CONFIG_PATH".into(),
            "/app/lib/pkgconfig:/app/share/pkgconfig:/usr/lib/pkgconfig:/usr/share/pkgconfig".into(),
            "--setenv".into(),
            "XDG_DATA_DIRS".into(),
            "/app/share:/usr/share:/usr/share/runtime/share".into(),
            "--setenv".into(),
            "XDG_CONFIG_DIRS".into(),
            "/app/etc/xdg:/etc/xdg".into(),
            "--setenv".into(),
            "GI_TYPELIB_PATH".into(),
            "/app/lib/girepository-1.0".into(),
            "--setenv".into(),
            "GSETTINGS_SCHEMA_DIR".into(),
            "/app/share/glib-2.0/schemas".into(),
            "--setenv".into(),
            "FLATPAK_ID".into(),
            self.app_id.clone(),
            "--setenv".into(),
            "FLATPAK_DEST".into(),
            "/app".into(),
            "--setenv".into(),
            "FLATPAK_ARCH".into(),
            crate::sources::flatpak_arch(),
            "--chdir".into(),
            "/".into(),
        ]);

        if share_network {
            // Default bwrap shares net namespace with host unless --unshare-net.
        } else {
            args.push("--unshare-net".into());
        }

        for fs in &self.default_filesystems {
            if let Ok(canon) = fs.canonicalize() {
                let s = path_to_str(&canon)?;
                args.extend(["--bind".into(), s.into(), s.into()]);
            }
        }

        Ok(args)
    }

    fn apply_spec_overlays(args: &mut Vec<String>, spec: &RunSpec) -> Result<()> {
        for (key, value) in &spec.env {
            args.extend(["--setenv".into(), key.clone(), value.clone()]);
        }

        for fs in &spec.filesystem_binds {
            // Accept either `--filesystem=PATH` / `--filesystem=PATH:ro` or raw paths,
            // and `--bind-mount=DEST=SRC` style from existing helpers.
            if let Some(rest) = fs.strip_prefix("--filesystem=") {
                let (path, ro) = rest
                    .split_once(':')
                    .map_or((rest, false), |(p, mode)| (p, mode == "ro"));
                let path = expand_user(path);
                if Path::new(&path).exists() {
                    let flag = if ro { "--ro-bind" } else { "--bind" };
                    args.extend([flag.into(), path.clone(), path]);
                }
            } else if let Some(rest) = fs.strip_prefix("--bind-mount=") {
                if let Some((dest, src)) = rest.split_once('=') {
                    if Path::new(src).exists() {
                        args.extend([
                            "--bind".into(),
                            src.into(),
                            dest.into(),
                        ]);
                    }
                }
            } else if let Some(rest) = fs.strip_prefix("--bind=") {
                // uncommon
                let _ = rest;
            } else if fs.starts_with("--") {
                // Permissions like --socket=wayland are not mapped 1:1; bind common sockets.
                apply_permission_flag(args, fs);
            } else {
                let path = expand_user(fs);
                if Path::new(&path).exists() {
                    args.extend(["--bind".into(), path.clone(), path]);
                }
            }
        }

        for extra in &spec.extra_args {
            if extra.starts_with("--env=") {
                if let Some((k, v)) = extra.trim_start_matches("--env=").split_once('=') {
                    args.extend(["--setenv".into(), k.into(), v.into()]);
                }
            } else if extra.starts_with("--filesystem=")
                || extra.starts_with("--bind-mount=")
                || extra.starts_with("--socket")
                || extra.starts_with("--device")
                || extra.starts_with("--share")
                || extra.starts_with("--talk-name")
                || extra.starts_with("--allow")
            {
                apply_permission_flag(args, extra);
            }
        }

        Ok(())
    }
}

fn expand_user(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    path.to_string()
}

fn apply_permission_flag(args: &mut Vec<String>, flag: &str) {
    // Best-effort host integration without the flatpak portal stack.
    if flag == "--socket=wayland" || flag.starts_with("--socket=wayland") {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let wayland = format!("{runtime_dir}/wayland-0");
            if Path::new(&wayland).exists() {
                args.extend([
                    "--bind".into(),
                    wayland.clone(),
                    wayland,
                    "--setenv".into(),
                    "WAYLAND_DISPLAY".into(),
                    "wayland-0".into(),
                    "--setenv".into(),
                    "XDG_RUNTIME_DIR".into(),
                    runtime_dir,
                ]);
            }
        }
    } else if flag.contains("pulseaudio") {
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let pulse = format!("{runtime_dir}/pulse/native");
            if Path::new(&pulse).exists() {
                args.extend(["--bind".into(), pulse.clone(), pulse]);
            }
        }
    } else if flag.contains("dri") {
        if Path::new("/dev/dri").exists() {
            args.extend(["--dev-bind".into(), "/dev/dri".into(), "/dev/dri".into()]);
        }
    } else if flag == "--share=network" || flag.starts_with("--share=network") {
        // Network shared by not passing --unshare-net (handled via share_network).
    } else if flag.starts_with("--talk-name=") || flag.starts_with("--own-name=") {
        // Session bus: bind the user bus socket when available.
        if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
            let bus = format!("{runtime_dir}/bus");
            if Path::new(&bus).exists() {
                args.extend([
                    "--bind".into(),
                    bus.clone(),
                    bus.clone(),
                    "--setenv".into(),
                    "DBUS_SESSION_BUS_ADDRESS".into(),
                    format!("unix:path={bus}"),
                ]);
            }
        }
    }
    verbose(format!("permission flag best-effort: {flag}"));
}

impl SandboxRunner for BwrapRunner {
    fn run(&self, spec: &RunSpec) -> Result<()> {
        let mut args = self.base_bwrap_args(&spec.repo_dir, spec.share_network)?;
        Self::apply_spec_overlays(&mut args, spec)?;

        if let Some(cwd) = &spec.cwd {
            let cwd_str = path_to_str(cwd)?;
            // Ensure cwd is visible (bind if under a path we might not have).
            if cwd.exists() {
                let canon = cwd.canonicalize().unwrap_or_else(|_| cwd.clone());
                let s = path_to_str(&canon)?;
                if !args.windows(3).any(|w| {
                    w[0] == "--bind" && (w[1] == s || w[2] == s)
                }) {
                    args.extend(["--bind".into(), s.into(), s.into()]);
                }
                args.extend(["--chdir".into(), s.into()]);
            } else {
                let _ = cwd_str;
            }
        }

        args.push("--".into());
        args.extend(spec.argv.iter().cloned());

        let args_display: Vec<&str> = args.iter().map(String::as_str).collect();
        command_header("bwrap", &args_display);

        let mut child = Command::new("bwrap")
            .args(&args)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context(
                "Failed to spawn bwrap — install bubblewrap (package often named bubblewrap)",
            )?;

        let status = child.wait()?;
        if status.success() {
            return Ok(());
        }
        if status.code() == Some(130) || crate::is_interrupted() {
            return Err(InterruptedError.into());
        }
        let _ = is_interrupted_error;
        anyhow::bail!("Sandbox command failed with {status}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_user_home() {
        // Only checks non-crash; HOME may vary.
        let p = expand_user("/tmp/x");
        assert_eq!(p, "/tmp/x");
    }
}
