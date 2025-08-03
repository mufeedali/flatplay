mod build_dirs;
mod command;
mod manifest;
pub mod process;
pub mod state;
mod utils;

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use colored::*;
use command::{flatpak_builder, run_command};
use dialoguer::{Select, theme::ColorfulTheme};
use nix::unistd::geteuid;

use crate::build_dirs::BuildDirs;
use crate::manifest::{Manifest, Module, find_manifests_in_path};
use crate::process::kill_process_group;
use crate::state::State;
use crate::utils::{get_a11y_bus_args, get_host_env};

pub struct FlatpakManager<'a> {
    state: &'a mut State,
    manifest: Option<Manifest>,
    build_dirs: BuildDirs,
}

impl<'a> FlatpakManager<'a> {
    fn find_manifests(&self) -> Result<Vec<PathBuf>> {
        let current_dir = std::env::current_dir()?;
        let current_dir_canon = current_dir.canonicalize()?;
        let base_dir_canon = self.state.base_dir.canonicalize()?;

        let mut manifests = find_manifests_in_path(&current_dir, None)?;
        if current_dir_canon != base_dir_canon {
            manifests.extend(find_manifests_in_path(
                &self.state.base_dir,
                Some(&current_dir),
            )?);
        }
        manifests.dedup();
        Ok(manifests)
    }

    fn auto_select_manifest(&mut self) -> Result<bool> {
        let manifests = self.find_manifests()?;
        if let Some(manifest_path) = manifests.first() {
            self.state.active_manifest = Some(manifest_path.clone());
            self.state.save()?;
            println!("{} {:?}", "Auto-selected manifest:".green(), manifest_path);
            self.manifest = Some(Manifest::from_file(manifest_path)?);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn new(state: &'a mut State) -> Result<Self> {
        let manifest = if let Some(path) = &state.active_manifest {
            Some(Manifest::from_file(path)?)
        } else {
            None
        };
        let build_dirs = BuildDirs::new(state.base_dir.clone());
        let mut manager = Self {
            state,
            manifest,
            build_dirs,
        };
        if manager.manifest.is_none() && !manager.auto_select_manifest()? {
            return Err(anyhow::anyhow!("No manifest found."));
        }
        manager.init()?;
        Ok(manager)
    }

    fn is_build_initialized(&self) -> Result<bool> {
        let metadata_file = self.build_dirs.metadata_file();
        let files_dir = self.build_dirs.files_dir();
        let var_dir = self.build_dirs.var_dir();

        // Check if all required directories and files exist
        // From gnome-builder: https://gitlab.gnome.org/GNOME/gnome-builder/-/blob/8579055f5047a0af5462e8a587b0742014d71d64/src/plugins/flatpak/gbp-flatpak-pipeline-addin.c#L220
        Ok(metadata_file.is_file() && files_dir.is_dir() && var_dir.is_dir())
    }

    fn init_build(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dirs.repo_dir();

        println!("{}", "Initializing build environment...".bold());
        run_command(
            "flatpak",
            &[
                "build-init",
                repo_dir.to_str().unwrap(),
                &manifest.id,
                &manifest.sdk,
                &manifest.runtime,
                &manifest.runtime_version,
            ],
            Some(self.state.base_dir.as_path()),
        )
    }

    pub fn init(&mut self) -> Result<()> {
        if self.is_build_initialized()? {
            return Ok(());
        }

        self.init_build()?;
        Ok(())
    }

    fn build_application(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dirs.repo_dir();
        let repo_dir_str = repo_dir.to_str().unwrap();

        if let Some(module) = manifest.modules.last() {
            match module {
                Module::Object {
                    buildsystem,
                    config_opts,
                    build_commands,
                    post_install,
                    ..
                } => {
                    match buildsystem.as_deref() {
                        Some("meson") => self.run_meson(repo_dir_str, config_opts.as_ref())?,
                        Some("cmake") | Some("cmake-ninja") => {
                            self.run_cmake(repo_dir_str, config_opts.as_ref())?
                        }
                        Some("simple") => self.run_simple(repo_dir_str, build_commands.as_ref())?,
                        _ => self.run_autotools(repo_dir_str, config_opts.as_ref())?,
                    }
                    if let Some(post_install) = post_install {
                        for command in post_install {
                            let args: Vec<&str> = command.split_whitespace().collect();
                            run_command(args[0], &args[1..], Some(self.state.base_dir.as_path()))?;
                        }
                    }
                }
                Module::Reference(_) => {
                    // Skip string references for build_application
                }
            }
        }

        Ok(())
    }

    fn run_meson(&self, repo_dir_str: &str, config_opts: Option<&Vec<String>>) -> Result<()> {
        let build_dir = self.build_dirs.build_subdir();
        let build_dir_str = build_dir.to_str().unwrap();
        let mut meson_args = vec!["build", repo_dir_str, "meson", "setup"];
        if let Some(opts) = config_opts {
            meson_args.extend(opts.iter().map(|s| s.as_str()));
        }
        meson_args.extend(&["--prefix=/app", build_dir_str]);
        run_command("flatpak", &meson_args, Some(self.state.base_dir.as_path()))?;
        run_command(
            "flatpak",
            &["build", repo_dir_str, "ninja", "-C", build_dir_str],
            Some(self.state.base_dir.as_path()),
        )?;
        run_command(
            "flatpak",
            &[
                "build",
                repo_dir_str,
                "meson",
                "install",
                "-C",
                build_dir_str,
            ],
            Some(self.state.base_dir.as_path()),
        )
    }

    fn run_cmake(&self, repo_dir_str: &str, config_opts: Option<&Vec<String>>) -> Result<()> {
        let build_dir = self.build_dirs.build_subdir();
        let build_dir_str = build_dir.to_str().unwrap();
        let b_flag = format!("-B{build_dir_str}");
        let mut cmake_args = vec![
            "build",
            repo_dir_str,
            "cmake",
            "-G",
            "Ninja",
            &b_flag,
            "-DCMAKE_EXPORT_COMPILE_COMMANDS=1",
            "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
            "-DCMAKE_INSTALL_PREFIX=/app",
        ];
        if let Some(opts) = config_opts {
            cmake_args.extend(opts.iter().map(|s| s.as_str()));
        }
        cmake_args.push(".");
        run_command("flatpak", &cmake_args, Some(self.state.base_dir.as_path()))?;
        run_command(
            "flatpak",
            &["build", repo_dir_str, "ninja", "-C", build_dir_str],
            Some(self.state.base_dir.as_path()),
        )?;
        run_command(
            "flatpak",
            &[
                "build",
                repo_dir_str,
                "ninja",
                "-C",
                build_dir_str,
                "install",
            ],
            Some(self.state.base_dir.as_path()),
        )
    }

    fn run_simple(&self, repo_dir_str: &str, build_commands: Option<&Vec<String>>) -> Result<()> {
        if let Some(commands) = build_commands {
            for command in commands {
                let mut args = vec!["build", repo_dir_str];
                args.extend(command.split_whitespace());
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
            }
        }
        Ok(())
    }

    fn run_autotools(&self, repo_dir_str: &str, config_opts: Option<&Vec<String>>) -> Result<()> {
        let mut autotools_args = vec!["build", repo_dir_str, "./configure", "--prefix=/app"];
        if let Some(opts) = config_opts {
            autotools_args.extend(opts.iter().map(|s| s.as_str()));
        }
        run_command(
            "flatpak",
            &autotools_args,
            Some(self.state.base_dir.as_path()),
        )?;
        run_command(
            "flatpak",
            &["build", repo_dir_str, "make"],
            Some(self.state.base_dir.as_path()),
        )?;
        run_command(
            "flatpak",
            &["build", repo_dir_str, "make", "install"],
            Some(self.state.base_dir.as_path()),
        )
    }

    fn build_dependencies(&mut self) -> Result<()> {
        println!("{}", "Building dependencies...".bold());
        let manifest = self.manifest.as_ref().unwrap();
        let manifest_path = self.state.active_manifest.as_ref().unwrap();
        let repo_dir = self.build_dirs.repo_dir();
        let state_dir = self.build_dirs.flatpak_builder_dir();
        flatpak_builder(
            &[
                "--ccache",
                "--force-clean",
                "--disable-updates",
                "--disable-download",
                "--build-only",
                "--keep-build-dirs",
                &format!("--state-dir={}", state_dir.to_str().unwrap()),
                &format!(
                    "--stop-at={}",
                    match manifest.modules.last().unwrap() {
                        Module::Object { name, .. } => name,
                        Module::Reference(s) => s,
                    }
                ),
                repo_dir.to_str().unwrap(),
                manifest_path.to_str().unwrap(),
            ],
            Some(self.state.base_dir.as_path()),
        )?;
        self.state.dependencies_built = true;
        self.state.save()
    }

    pub fn update_dependencies(&mut self) -> Result<()> {
        println!("{}", "Updating dependencies...".bold());

        let manifest = self.manifest.as_ref().unwrap();
        let manifest_path = self.state.active_manifest.as_ref().unwrap();
        let repo_dir = self.build_dirs.repo_dir();
        let state_dir = self.build_dirs.flatpak_builder_dir();
        flatpak_builder(
            &[
                "--ccache",
                "--force-clean",
                "--disable-updates",
                "--download-only",
                &format!("--state-dir={}", state_dir.to_str().unwrap()),
                &format!(
                    "--stop-at={}",
                    match manifest.modules.last().unwrap() {
                        Module::Object { name, .. } => name,
                        Module::Reference(s) => s,
                    }
                ),
                repo_dir.to_str().unwrap(),
                manifest_path.to_str().unwrap(),
            ],
            Some(self.state.base_dir.as_path()),
        )?;
        self.state.dependencies_updated = true;
        self.state.save()
    }

    pub fn build(&mut self) -> Result<()> {
        if self.manifest.is_none() {
            println!(
                "{}",
                "No manifest selected. Please run `select-manifest` first.".yellow()
            );
            return Ok(());
        }

        if !self.state.dependencies_updated {
            self.update_dependencies()?;
        }
        if !self.state.dependencies_built {
            self.build_dependencies()?;
        }
        self.build_application()?;
        self.state.application_built = true;
        self.state.save()
    }

    pub fn build_and_run(&mut self) -> Result<()> {
        self.build()?;
        self.run()
    }

    pub fn stop(&mut self) -> Result<()> {
        kill_process_group(self.state)
    }

    pub fn run(&self) -> Result<()> {
        if !self.state.application_built {
            println!(
                "{}",
                "Application not built. Please run `build` first.".yellow()
            );
            return Ok(());
        }
        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dirs.repo_dir();

        let mut args: Vec<String> = [
            "build",
            "--with-appdir",
            "--allow=devel",
            format!(
                "--bind-mount=/run/user/{user_id}/doc=/run/user/{user_id}/doc/by-app/{app_id}",
                user_id = geteuid(),
                app_id = manifest.id
            )
            .as_str(),
            "--talk-name=org.freedesktop.portal.*",
            "--talk-name=org.a11y.Bus",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        args.extend(
            get_host_env()
                .into_iter()
                .map(|(key, value)| format!("--env={key}={value}")),
        );

        args.extend(get_a11y_bus_args());

        args.extend(manifest.finish_args.clone());
        args.push(repo_dir.to_str().unwrap().to_string());
        args.push(manifest.command.clone());
        if let Some(x_run_args) = &manifest.x_run_args {
            args.extend(x_run_args.clone());
        }

        let args_str: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        run_command("flatpak", &args_str, Some(self.state.base_dir.as_path()))
    }

    pub fn export_bundle(&self) -> Result<()> {
        if !self.state.application_built {
            println!(
                "{}",
                "Application not built. Please run `build` first.".yellow()
            );
            return Ok(());
        }
        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dirs.repo_dir();
        let finalized_repo_dir = self.build_dirs.finalized_repo_dir();
        let ostree_dir = self.build_dirs.ostree_dir();

        // Remove finalized repo
        if finalized_repo_dir.is_dir() {
            fs::remove_dir_all(&finalized_repo_dir)?;
        }

        // Copy repo
        run_command(
            "cp",
            &[
                "-r",
                repo_dir.to_str().unwrap(),
                finalized_repo_dir.to_str().unwrap(),
            ],
            Some(self.state.base_dir.as_path()),
        )?;

        // Finalize build
        let mut args: Vec<String> = vec!["build-finish".to_string()];

        args.extend(manifest.finish_args.clone());
        args.push(format!("--command={}", manifest.command));
        args.push(finalized_repo_dir.to_str().unwrap().to_string());

        let args_str: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        run_command("flatpak", &args_str, Some(self.state.base_dir.as_path()))?;

        // Export build
        run_command(
            "flatpak",
            &[
                "build-export",
                ostree_dir.to_str().unwrap(),
                finalized_repo_dir.to_str().unwrap(),
            ],
            Some(self.state.base_dir.as_path()),
        )?;

        // Bundle build
        run_command(
            "flatpak",
            &[
                "build-bundle",
                ostree_dir.to_str().unwrap(),
                format!("{}.flatpak", manifest.id.clone()).as_str(),
                manifest.id.clone().as_str(),
            ],
            Some(self.state.base_dir.as_path()),
        )
    }

    pub fn clean(&mut self) -> Result<()> {
        let build_dir = self.build_dirs.build_dir();
        if fs::metadata(&build_dir).is_ok() {
            fs::remove_dir_all(&build_dir)?;
            println!("{} Cleaned .flatplay directory.", "✔".green());
            self.state.reset();
        }
        Ok(())
    }

    pub fn runtime_terminal(&self) -> Result<()> {
        if self.manifest.is_none() {
            println!(
                "{}",
                "No manifest selected. Please run `select-manifest` first.".yellow()
            );
            return Ok(());
        }
        let manifest = self.manifest.as_ref().unwrap();
        let sdk_id = format!("{}//{}", manifest.sdk, manifest.runtime_version);
        run_command(
            "flatpak",
            &["run", "--command=bash", &sdk_id],
            Some(self.state.base_dir.as_path()),
        )
    }

    pub fn build_terminal(&self) -> Result<()> {
        if self.manifest.is_none() {
            println!(
                "{}",
                "No manifest selected. Please run `select-manifest` first.".yellow()
            );
            return Ok(());
        }
        let manifest = self.manifest.as_ref().unwrap();
        let _app_id = &manifest.id;
        let repo_dir = self.build_dirs.repo_dir();
        run_command(
            "flatpak",
            &["build", repo_dir.to_str().unwrap(), "bash"],
            Some(self.state.base_dir.as_path()),
        )
    }

    /// Manifest selection command endpoint.
    pub fn select_manifest(&mut self, path: Option<PathBuf>) -> Result<()> {
        if let Some(path) = path {
            let manifest_path = if path.is_absolute() {
                path
            } else {
                self.state.base_dir.join(&path)
            };
            if !manifest_path.exists() {
                return Err(anyhow::anyhow!(
                    "Manifest file not found at {:?}",
                    manifest_path
                ));
            }
            let manifest = Manifest::from_file(&manifest_path)?;
            return self.set_active_manifest(manifest_path, Some(manifest));
        }

        println!("{}", "Searching for manifest files...".bold());
        let manifests = self.find_manifests()?;

        if manifests.is_empty() {
            println!("{}", "No manifest files found.".yellow());
            return Ok(());
        }

        let manifest_strings: Vec<String> = manifests
            .iter()
            .map(|p| {
                let path_str = p.to_str().unwrap().to_string();
                if self.state.active_manifest.as_ref() == Some(p) {
                    format!("{} {}", "*".green().bold(), path_str)
                } else {
                    format!("  {path_str}")
                }
            })
            .collect();

        let default_selection = manifests
            .iter()
            .position(|p| self.state.active_manifest.as_ref() == Some(p))
            .unwrap_or(0);

        let theme = ColorfulTheme::default();
        let selection = Select::with_theme(&theme)
            .with_prompt("Select a manifest")
            .items(&manifest_strings)
            .default(default_selection)
            .interact()?;

        self.set_active_manifest(manifests[selection].clone(), None)
    }

    /// Sets the active manifest and updates the state.
    fn set_active_manifest(
        &mut self,
        manifest_path: PathBuf,
        manifest: Option<Manifest>,
    ) -> Result<()> {
        let should_clean = self.state.active_manifest.as_ref() != Some(&manifest_path);
        if should_clean {
            // Clean build directory and progress since manifest has changed.
            self.clean()?;

            // Change active manifest in state.
            self.state.active_manifest = Some(manifest_path.clone());
            self.state.save()?;
        }
        if let Some(manifest) = manifest {
            self.manifest = Some(manifest);
        }
        println!(
            "{} {:?}. You can now run `{}`.",
            "Selected manifest:".green(),
            manifest_path,
            "flatplay".bold().italic(),
        );

        Ok(())
    }
}
