use std::env;
use std::fmt::Write;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::{Select, theme::SimpleTheme};
use nix::unistd::geteuid;

use crate::build_dirs::BuildDirs;
use crate::builder;
use crate::command::{run_command, run_command_output};
use crate::manifest::{Manifest, Module, find_manifests_in_path};
use crate::sandbox::{self, FlatpakRunner, RunSpec, SandboxRunner};
use crate::sources::{self, Source};
use crate::state::State;
use crate::utils::{
    build_font_config, get_a11y_bus_args, get_fonts_args, get_host_env, path_to_str, status,
    status_info, status_success, status_warn, verbose, version_less_than,
};

use sha2::{Digest, Sha256};

struct BuildSandbox {
    fs_ws: String,
    fs_repo: String,
    env_args: Vec<String>,
    path_overrides: Vec<String>,
}

pub struct FlatpakManager<'a> {
    state: &'a mut State,
    manifest: Option<Manifest>,
    build_dirs: BuildDirs,
}

impl<'a> FlatpakManager<'a> {
    fn application_module(&self) -> Result<Module> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        manifest.application_module(manifest_path)
    }

    fn last_module_name(&self) -> Result<String> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        manifest.last_module_name(manifest_path)
    }
    fn compute_manifest_hash(path: &Path) -> Result<String> {
        let mut hasher = Sha256::new();
        Self::hash_file_into(&mut hasher, path)?;
        // Hash referenced module files fully (buildsystem/config edits) and
        // fingerprint expanded inline/nested modules' sources.
        if let Ok(manifest) = Manifest::from_file(path) {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            Self::hash_module_tree(&mut hasher, &manifest.modules, parent)?;
        }
        let result = hasher.finalize();
        let mut hash = String::with_capacity(64);
        for b in result {
            write!(&mut hash, "{b:02x}")?;
        }
        Ok(hash)
    }

    fn hash_module_tree(hasher: &mut Sha256, modules: &[Module], dir: &Path) -> Result<()> {
        for module in modules {
            match module {
                Module::Reference(name) => {
                    let ref_path = dir.join(name);
                    if ref_path.is_file() {
                        Self::hash_file_into(hasher, &ref_path)?;
                        // Nested refs inside that file are covered by the file hash.
                    } else {
                        hasher.update(name.as_bytes());
                    }
                }
                Module::Object {
                    name,
                    sources,
                    buildsystem,
                    config_opts,
                    build_commands,
                    post_install,
                    modules: nested,
                    ..
                } => {
                    hasher.update(name.as_bytes());
                    if let Some(bs) = buildsystem {
                        hasher.update(bs.as_bytes());
                    }
                    if let Some(opts) = config_opts {
                        for o in opts {
                            hasher.update(o.as_bytes());
                        }
                    }
                    if let Some(cmds) = build_commands {
                        for c in cmds {
                            hasher.update(c.as_bytes());
                        }
                    }
                    if let Some(pi) = post_install {
                        for c in pi {
                            hasher.update(c.as_bytes());
                        }
                    }
                    if let Ok(typed) = Source::from_values(sources) {
                        hasher.update(sources::sources_fingerprint(&typed).as_bytes());
                    }
                    if !nested.is_empty() {
                        Self::hash_module_tree(hasher, nested, dir)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn hash_file_into(hasher: &mut Sha256, path: &Path) -> Result<()> {
        let mut file = fs::File::open(path)
            .with_context(|| format!("Failed to open {} for hashing", path.display()))?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        Ok(())
    }

    fn find_manifests(&self) -> Result<Vec<PathBuf>> {
        let current_dir = env::current_dir()?;
        let current_dir_canon = current_dir.canonicalize()?;
        let base_dir_canon = self.state.base_dir.canonicalize()?;

        let mut manifests = find_manifests_in_path(&current_dir, None);
        if current_dir_canon != base_dir_canon {
            manifests.extend(find_manifests_in_path(
                &self.state.base_dir,
                Some(&current_dir),
            ));
        }
        manifests.dedup();
        Ok(manifests)
    }

    fn auto_select_manifest(&mut self) -> Result<bool> {
        let manifests = self.find_manifests()?;
        if let Some(manifest_path) = manifests.into_iter().next() {
            let display_path = manifest_path
                .strip_prefix(&self.state.base_dir)
                .unwrap_or(manifest_path.as_path());
            status_success(format!(
                "Auto-selected manifest: {}",
                display_path.display()
            ));
            let manifest = Manifest::from_file(&manifest_path)?;
            self.set_active_manifest(&manifest_path, Some(manifest))?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn print_manifest_info(&self) {
        if let Some(manifest) = &self.manifest {
            status_info(format!(
                "Manifest: {} ({}//{})",
                manifest.id, manifest.runtime, manifest.runtime_version
            ));
            verbose("Manifest Info:");
            verbose(format!("  App ID: {}", manifest.id));
            verbose(format!("  SDK: {}", manifest.sdk));
            verbose(format!("  Runtime: {}", manifest.runtime));
            verbose(format!("  Runtime Version: {}", manifest.runtime_version));
        }
    }

    pub fn new(state: &'a mut State) -> Self {
        let manifest =
            state
                .active_manifest
                .as_ref()
                .and_then(|path| match Manifest::from_file(path) {
                    Ok(manifest) => Some(manifest),
                    Err(error) => {
                        verbose(format!(
                            "Failed to load active manifest {}: {error:#}",
                            path.display()
                        ));
                        None
                    }
                });
        let build_dirs = BuildDirs::new(state.base_dir.clone());
        Self {
            state,
            manifest,
            build_dirs,
        }
    }

    fn check_required_version(manifest: &Manifest) -> Result<()> {
        let required = manifest.finish_args.iter().find_map(|arg| {
            let (key, value) = arg.split_once('=')?;
            (key == "--require-version").then_some(value)
        });
        let Some(required) = required else {
            return Ok(());
        };
        let output = run_command_output("flatpak", &["--version"])
            .context("Failed to get flatpak version")?;
        let version = String::from_utf8_lossy(&output.stdout)
            .replace("Flatpak ", "")
            .trim()
            .to_string();
        if version_less_than(&version, required) {
            return Err(anyhow::anyhow!(
                "Manifest requires flatpak >= {required} but {version} is installed"
            ));
        }
        Ok(())
    }

    fn flatpak_runner(&self) -> FlatpakRunner {
        FlatpakRunner::new(self.state.base_dir.as_path())
    }

    pub fn validate_manifest(&self, allow_auto_select: bool) -> Result<()> {
        if let Some(manifest) = &self.manifest {
            Self::check_required_version(manifest)?;
            return Ok(());
        }

        if let Some(path) = &self.state.active_manifest {
            if Manifest::from_file(path).is_ok() {
                return Ok(());
            }

            if !allow_auto_select {
                return Err(anyhow::anyhow!(
                    "Active manifest is invalid. Run `flatplay select-manifest` to choose a different manifest."
                ));
            }

            let display_path = path.strip_prefix(&self.state.base_dir).unwrap_or(path);
            status_warn(format!(
                "Failed to load manifest at {}. Attempting to auto-select...",
                display_path.display()
            ));
        }

        if !allow_auto_select {
            return Err(anyhow::anyhow!(
                "No manifest selected. Run `flatplay select-manifest` to select a manifest."
            ));
        }

        let manifests = self.find_manifests()?;
        if manifests.is_empty() {
            return Err(anyhow::anyhow!(
                "No manifests found in project. Run `flatplay select-manifest <path>` to specify a manifest."
            ));
        }

        Ok(())
    }

    pub fn ensure_ready(&mut self, allow_auto_select: bool) -> Result<()> {
        if self.manifest.is_none() {
            if allow_auto_select {
                if !self.auto_select_manifest()? {
                    return Err(anyhow::anyhow!(
                        "No manifests found in project. Run `flatplay select-manifest <path>` to specify a manifest."
                    ));
                }
            } else {
                return Err(anyhow::anyhow!(
                    "No manifest selected. Run `flatplay select-manifest` to select a manifest."
                ));
            }
        }

        if self.manifest.is_some() {
            self.print_manifest_info();
            self.check_manifest_changed()?;
        }

        self.init()?;
        self.state.save()?;

        Ok(())
    }

    fn is_build_initialized(&self) -> bool {
        let metadata_file = self.build_dirs.metadata_file();
        let files_dir = self.build_dirs.files_dir();
        let var_dir = self.build_dirs.var_dir();

        // From gnome-builder: https://gitlab.gnome.org/GNOME/gnome-builder/-/blob/8579055f5047a0af5462e8a587b0742014d71d64/src/plugins/flatpak/gbp-flatpak-pipeline-addin.c#L220
        metadata_file.is_file() && files_dir.is_dir() && var_dir.is_dir()
    }

    fn init(&self) -> Result<()> {
        if self.is_build_initialized() {
            return Ok(());
        }
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        sandbox::ensure_build_initialized(
            &repo_dir,
            &manifest.id,
            &manifest.sdk,
            &manifest.runtime,
            &manifest.runtime_version,
            Some(self.state.base_dir.as_path()),
        )
    }

    fn build_application(&self, rebuild: bool) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        let module = self.application_module()?;
        let runner = self.flatpak_runner();
        let repo_dir = self.build_dirs.repo_dir();
        let build_dir = self.build_dirs.build_dir();
        let paths = builder::ModulePaths {
            repo_dir: &repo_dir,
            build_dir: &build_dir,
            workspace: self.state.base_dir.as_path(),
            manifest_path,
        };
        builder::build_module(
            manifest,
            &module,
            &paths,
            &runner,
            builder::BuildModuleOpts { rebuild },
        )
    }

    fn build_sandbox(&self, manifest: &Manifest) -> BuildSandbox {
        BuildSandbox {
            fs_ws: format!("--filesystem={}", self.state.base_dir.display()),
            fs_repo: format!("--filesystem={}", self.build_dirs.repo_dir().display()),
            env_args: manifest
                .merged_env(None)
                .iter()
                .map(|(k, v)| format!("--env={k}={v}"))
                .collect(),
            path_overrides: manifest.path_overrides(None),
        }
    }

    fn build_dependencies(&mut self) -> Result<()> {
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?
            .clone();
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let build_dir = self.build_dirs.build_dir();
        let stop_at = self.last_module_name()?;
        let runner = self.flatpak_runner();
        builder::build_dependencies(
            manifest,
            &manifest_path,
            &repo_dir,
            &build_dir,
            &stop_at,
            self.state.base_dir.as_path(),
            &runner,
        )?;
        self.state.dependencies_built = true;
        self.state.save()
    }

    pub fn update_dependencies(&mut self) -> Result<()> {
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?
            .clone();
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let build_dir = self.build_dirs.build_dir();
        let stop_at = self.last_module_name()?;
        builder::update_dependencies(manifest, &manifest_path, &build_dir, &stop_at)?;
        self.state.dependencies_updated = true;
        self.state.save()
    }

    fn check_manifest_changed(&mut self) -> Result<()> {
        if let Some(manifest_path) = &self.state.active_manifest {
            let hash = Self::compute_manifest_hash(manifest_path)?;

            if let Some(stored_hash) = &self.state.manifest_hash {
                if *stored_hash != hash {
                    status_warn("Manifest changed, resetting build state...");
                    self.state.reset();
                    self.state.manifest_hash = Some(hash);
                    self.state.save()?;
                }
            } else {
                status_warn("Manifest hash missing, resetting build state...");
                self.state.reset();
                self.state.manifest_hash = Some(hash);
                self.state.save()?;
            }
        }
        Ok(())
    }

    pub fn build(&mut self) -> Result<()> {
        if !self.state.dependencies_updated {
            self.update_dependencies()?;
        }
        if !self.state.dependencies_built {
            self.build_dependencies()?;
        }
        self.build_application(false)?;
        self.state.application_built = true;
        self.state.save()
    }

    pub fn rebuild(&mut self) -> Result<()> {
        status(format!("{}", "Rebuilding application...".bold()));
        self.build_application(true)?;
        self.state.application_built = true;
        self.state.save()
    }

    pub fn build_and_run(&mut self) -> Result<()> {
        self.build()?;
        self.run()
    }

    pub fn run(&self) -> Result<()> {
        if !self.state.application_built {
            return Err(anyhow::anyhow!(
                "Application not built. Please run `build` first."
            ));
        }
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let runner = self.flatpak_runner();
        let mut argv = vec![manifest.command.clone()];
        if let Some(x_run_args) = &manifest.x_run_args {
            argv.extend(x_run_args.clone());
        }
        self.run_app_spec(&runner, &repo_dir, manifest, argv, true, false)
    }

    /// Run an app (or shell) inside the build directory via `flatpak build`.
    ///
    /// * `devel` — `--allow=devel` and host path overrides (build/debug shell).
    /// * `with_dev_paths` — forward path overrides + treat as networked like build.
    fn run_app_spec(
        &self,
        runner: &FlatpakRunner,
        repo_dir: &Path,
        manifest: &Manifest,
        argv: Vec<String>,
        devel: bool,
        with_dev_paths: bool,
    ) -> Result<()> {
        let sandbox = self.build_sandbox(manifest);
        let uid = geteuid();
        let mut filesystem_binds = vec![
            "--with-appdir".into(),
            format!(
                "--bind-mount=/run/user/{uid}/doc=/run/user/{uid}/doc/by-app/{}",
                manifest.id
            ),
            sandbox.fs_ws.clone(),
            sandbox.fs_repo.clone(),
            "--talk-name=org.freedesktop.portal.*".into(),
            "--talk-name=org.a11y.Bus".into(),
        ];
        if devel {
            filesystem_binds.push("--allow=devel".into());
        }
        filesystem_binds.extend(manifest.finish_args_filtered());
        if with_dev_paths {
            filesystem_binds.extend(sandbox.path_overrides.clone());
        }

        let mut env = Vec::new();
        for (k, v) in get_host_env() {
            env.push((k, v));
        }
        for arg in &sandbox.env_args {
            if let Some(rest) = arg.strip_prefix("--env=")
                && let Some((k, v)) = rest.split_once('=')
            {
                env.push((k.to_string(), v.to_string()));
            }
        }

        match get_a11y_bus_args() {
            Ok(a11y_args) => filesystem_binds.extend(a11y_args),
            Err(error) => verbose(format!("a11y bus not available: {error:#}")),
        }
        match build_font_config().and_then(|config_path| get_fonts_args(&config_path)) {
            Ok(fonts_args) => filesystem_binds.extend(fonts_args),
            Err(error) => verbose(format!("fonts not available: {error:#}")),
        }

        // Exact match — substring would treat `--unshare=network` as share.
        let share_network =
            with_dev_paths || manifest.finish_args.iter().any(|a| a == "--share=network");

        runner.run(&RunSpec {
            repo_dir: repo_dir.to_path_buf(),
            argv,
            env,
            filesystem_binds,
            extra_args: vec![],
            share_network,
            cwd: None,
        })
    }

    pub fn export_bundle(&self) -> Result<()> {
        if !self.state.application_built {
            return Err(anyhow::anyhow!(
                "Application not built. Please run `build` first."
            ));
        }
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let finalized_repo_dir = self.build_dirs.finalized_repo_dir();
        let ostree_dir = self.build_dirs.ostree_dir();
        let base = self.state.base_dir.as_path();

        if finalized_repo_dir.is_dir() {
            fs::remove_dir_all(&finalized_repo_dir)?;
        }

        run_command(
            "cp",
            &[
                "-r",
                path_to_str(&repo_dir)?,
                path_to_str(&finalized_repo_dir)?,
            ],
            Some(base),
        )?;

        let mut finish_args: Vec<String> = vec!["build-finish".to_string()];
        finish_args.extend(manifest.finish_args_filtered());
        finish_args.push(format!("--command={}", manifest.command));
        finish_args.push(path_to_str(&finalized_repo_dir)?.to_string());
        let finish_ref: Vec<&str> = finish_args.iter().map(String::as_str).collect();
        run_command("flatpak", &finish_ref, Some(base))?;

        run_command(
            "flatpak",
            &[
                "build-export",
                path_to_str(&ostree_dir)?,
                path_to_str(&finalized_repo_dir)?,
            ],
            Some(base),
        )?;

        let bundle_name = format!("{}.flatpak", manifest.id);
        run_command(
            "flatpak",
            &[
                "build-bundle",
                path_to_str(&ostree_dir)?,
                &bundle_name,
                manifest.id.as_str(),
            ],
            Some(base),
        )?;

        status_success(format!("Exported {bundle_name}"));
        Ok(())
    }

    pub fn clean(&mut self) -> Result<()> {
        let build_dir = self.build_dirs.build_dir();
        if fs::metadata(&build_dir).is_ok() {
            fs::remove_dir_all(&build_dir)?;
            status_success("Cleaned .flatplay directory.");
            self.state.reset();
        }
        Ok(())
    }

    pub fn runtime_terminal(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        // App-like shell: finish-args only, no devel / path overrides.
        let runner = self.flatpak_runner();
        self.run_app_spec(
            &runner,
            &repo_dir,
            manifest,
            vec!["bash".into()],
            false,
            false,
        )
    }

    pub fn build_terminal(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        // SDK/build shell: devel + path overrides + network.
        let runner = self.flatpak_runner();
        self.run_app_spec(
            &runner,
            &repo_dir,
            manifest,
            vec!["bash".into()],
            true,
            true,
        )
    }

    pub fn select_manifest(&mut self, path: Option<PathBuf>) -> Result<()> {
        if let Some(path) = path {
            let manifest_path = {
                let p = if path.is_absolute() {
                    path
                } else {
                    self.state.base_dir.join(&path)
                };
                p.canonicalize()?
            };
            if !manifest_path.exists() {
                return Err(anyhow::anyhow!(
                    "Manifest file not found at {}",
                    manifest_path.display()
                ));
            }
            let manifest = Manifest::from_file(&manifest_path)?;
            self.set_active_manifest(&manifest_path, Some(manifest))?;
            self.print_manifest_info();
            self.print_selection_message(true);
            return Ok(());
        }

        verbose("Searching for manifest files...");
        let manifests = self.find_manifests()?;

        if manifests.is_empty() {
            status_warn("No manifest files found.");
            return Ok(());
        }

        let manifest_strings: Vec<String> = manifests
            .iter()
            .filter_map(|p| {
                let display_path = p.strip_prefix(&self.state.base_dir).unwrap_or(p.as_path());
                let path_str = display_path.to_str()?.to_string();
                Some(if self.state.active_manifest.as_ref() == Some(p) {
                    format!("* {path_str}")
                } else {
                    format!("  {path_str}")
                })
            })
            .collect();

        let default_selection = manifests
            .iter()
            .position(|p| self.state.active_manifest.as_ref() == Some(p))
            .unwrap_or(0);

        let theme = SimpleTheme;
        let prompt = format!("{} {}", "│".blue(), "Select a manifest".blue());
        let selection = Select::with_theme(&theme)
            .with_prompt(&prompt)
            .items(&manifest_strings)
            .default(default_selection)
            .interact()?;

        self.set_active_manifest(&manifests[selection], None)?;
        self.print_manifest_info();
        self.print_selection_message(false);
        Ok(())
    }

    fn set_active_manifest(
        &mut self,
        manifest_path: &Path,
        manifest: Option<Manifest>,
    ) -> Result<()> {
        let should_clean = self
            .state
            .active_manifest
            .as_ref()
            .is_none_or(|active| active != manifest_path);
        if should_clean {
            self.clean()?;

            self.state.active_manifest = Some(manifest_path.to_path_buf());

            self.state.manifest_hash = Some(Self::compute_manifest_hash(manifest_path)?);

            self.state.save()?;
        }

        self.manifest = if let Some(manifest) = manifest {
            Some(manifest)
        } else {
            Some(Manifest::from_file(manifest_path)?)
        };

        Ok(())
    }

    fn print_selection_message(&self, show_path: bool) {
        if show_path {
            if let Some(manifest_path) = &self.state.active_manifest {
                let display_path = manifest_path
                    .strip_prefix(&self.state.base_dir)
                    .unwrap_or(manifest_path.as_path());

                status_success(format!(
                    "Selected manifest: {}. You can now run `flatplay`.",
                    display_path.display(),
                ));
            }
        } else {
            status_success("Ready. Run `flatplay` to build.");
        }
    }
}
