mod command;
mod manifest;
pub mod process;
pub mod state;
mod utils;

use std::fs;

use anyhow::Result;
use colored::*;
use command::{flatpak_builder, run_command};
use dialoguer::{Select, theme::ColorfulTheme};

use crate::manifest::{Manifest, Module, find_manifests_in_path};
use crate::process::kill_process_group;
use crate::state::State;
use crate::utils::{get_a11y_bus_args, get_host_env};

const BUILD_DIR: &str = ".flatplay";

pub struct FlatpakManager<'a> {
    state: &'a mut State,
    manifest: Option<Manifest>,
}

use std::path::PathBuf;

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
        let mut manager = Self { state, manifest };
        if manager.manifest.is_none() && !manager.auto_select_manifest()? {
            return Err(anyhow::anyhow!("No manifest found."));
        }
        Ok(manager)
    }

    fn build_dir(&self) -> PathBuf {
        self.state.base_dir.join(BUILD_DIR)
    }

    fn is_build_initialized(&self) -> Result<bool> {
        let repo_dir = self.build_dir().join("repo");
        let metadata_file = repo_dir.join("metadata");
        let files_dir = repo_dir.join("files");
        let var_dir = repo_dir.join("var");

        // Check if all required directories and files exist
        // From gnome-builder: https://gitlab.gnome.org/GNOME/gnome-builder/-/blob/8579055f5047a0af5462e8a587b0742014d71d64/src/plugins/flatpak/gbp-flatpak-pipeline-addin.c#L220
        Ok(metadata_file.is_file() && files_dir.is_dir() && var_dir.is_dir())
    }

    fn init_build(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dir().join("repo");

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

    fn ensure_initialized_build(&mut self) -> Result<()> {
        if self.is_build_initialized()? {
            println!(
                "{}",
                "Skipped build initialization. Already initialized.".green()
            );
            return Ok(());
        }

        self.init_build()?;
        Ok(())
    }

    fn build_application(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dir().join("repo");
        let repo_dir_str = repo_dir.to_str().unwrap();

        if let Some(module) = manifest.modules.last() {
            match module {
                Module::Object {
                    buildsystem,
                    config_opts,
                    build_commands,
                    ..
                } => {
                    let buildsystem = buildsystem.as_deref();
                    match buildsystem {
                        Some("meson") => {
                            let build_dir = self.build_dir().join("_build");
                            let build_dir_str = build_dir.to_str().unwrap();
                            let mut meson_args = vec!["build", repo_dir_str, "meson", "setup"];
                            if let Some(config_opts) = config_opts {
                                meson_args.extend(config_opts.iter().map(|s| s.as_str()));
                            }
                            meson_args.extend(&["--prefix=/app", build_dir_str]);
                            run_command(
                                "flatpak",
                                &meson_args,
                                Some(self.state.base_dir.as_path()),
                            )?;
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
                            )?;
                        }
                        Some("cmake") | Some("cmake-ninja") => {
                            let build_dir = self.build_dir().join("_build");
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
                            if let Some(config_opts) = config_opts {
                                cmake_args.extend(config_opts.iter().map(|s| s.as_str()));
                            }
                            cmake_args.push(".");
                            run_command(
                                "flatpak",
                                &cmake_args,
                                Some(self.state.base_dir.as_path()),
                            )?;
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
                            )?;
                        }
                        Some("simple") => {
                            if let Some(build_commands) = build_commands {
                                for command in build_commands {
                                    let mut args = vec!["build", repo_dir_str];
                                    args.extend(command.split_whitespace());
                                    run_command(
                                        "flatpak",
                                        &args,
                                        Some(self.state.base_dir.as_path()),
                                    )?;
                                }
                            }
                        }
                        _ => {
                            // Default to autotools
                            let mut autotools_args =
                                vec!["build", repo_dir_str, "./configure", "--prefix=/app"];
                            if let Some(config_opts) = config_opts {
                                autotools_args.extend(config_opts.iter().map(|s| s.as_str()));
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
                            )?;
                        }
                    }
                }
                Module::Reference(_) => {
                    // Skip string references for build_application
                }
            }
        }

        if let Some(Module::Object {
            post_install: Some(post_install),
            ..
        }) = manifest.modules.last()
        {
            for command in post_install {
                let args: Vec<&str> = command.split_whitespace().collect();
                run_command(args[0], &args[1..], Some(self.state.base_dir.as_path()))?;
            }
        }

        Ok(())
    }

    fn build_dependencies(&mut self) -> Result<()> {
        println!("{}", "Building dependencies...".bold());
        let manifest = self.manifest.as_ref().unwrap();
        let manifest_path = self.state.active_manifest.as_ref().unwrap();
        let repo_dir = self.build_dir().join("repo");
        let state_dir = self.build_dir().join("flatpak-builder");
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

        // Ensure build is initialized before updating dependencies
        self.ensure_initialized_build()?;

        let manifest = self.manifest.as_ref().unwrap();
        let manifest_path = self.state.active_manifest.as_ref().unwrap();
        let repo_dir = self.build_dir().join("repo");
        let state_dir = self.build_dir().join("flatpak-builder");
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

        // Ensure build is initialized before proceeding
        self.ensure_initialized_build()?;

        if !self.state.dependencies_updated {
            self.update_dependencies()?;
        }
        if !self.state.dependencies_built {
            self.build_dependencies()?;
        }
        if self.state.application_built {
            // TODO: Implement rebuild
            self.build_application()?;
        } else {
            self.build_application()?;
        }
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

        // Check if build is initialized
        if !self.is_build_initialized()? {
            println!(
                "{}",
                "Build not initialized. Please run `build` first.".yellow()
            );
            return Ok(());
        }

        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dir().join("repo");

        let mut args = vec![
            "build".to_string(),
            "--with-appdir".to_string(),
            "--allow=devel".to_string(),
            "--talk-name=org.freedesktop.portal.*".to_string(),
            "--talk-name=org.a11y.Bus".to_string(),
        ];

        for (key, value) in get_host_env() {
            args.push(format!("--env={key}={value}"));
        }

        for arg in get_a11y_bus_args() {
            args.push(arg);
        }

        for arg in &manifest.finish_args {
            args.push(arg.clone());
        }
        args.push(repo_dir.to_str().unwrap().to_string());
        args.push(manifest.command.clone());
        if let Some(x_run_args) = &manifest.x_run_args {
            for arg in x_run_args {
                args.push(arg.clone());
            }
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

        // Check if build is initialized
        if !self.is_build_initialized()? {
            println!(
                "{}",
                "Build not initialized. Please run `build` first.".yellow()
            );
            return Ok(());
        }

        let manifest = self.manifest.as_ref().unwrap();
        let repo_dir = self.build_dir().join("repo");
        let finalized_repo_dir = self.build_dir().join("finalized-repo");
        let ostree_dir = self.build_dir().join("ostree");

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
        let mut args = vec!["build-finish".to_string()];

        for arg in &manifest.finish_args {
            args.push(arg.clone());
        }
        args.push(format!("--command={}", manifest.command.clone()));
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
        let build_dir = self.build_dir();
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
        let repo_dir = self.build_dir().join("repo");
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

        let mut default_selection = 0;
        let manifest_strings: Vec<String> = manifests
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let path_str = p.to_str().unwrap().to_string();
                if let Some(active_manifest) = &self.state.active_manifest {
                    if active_manifest == p {
                        default_selection = i;
                        return format!("{} {}", "*".green().bold(), path_str);
                    }
                }
                format!("  {path_str}")
            })
            .collect();

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
