use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::{Select, theme::SimpleTheme};
use nix::unistd::geteuid;

use crate::build_dirs::BuildDirs;
use crate::builder;
use crate::manifest::{BuildOptions, Manifest, Module, find_manifests_in_path};
use crate::sandbox::{self, BwrapRunner, RunSpec, SandboxRunner, ensure_sdk_and_runtime};
use crate::sources::{self, DownloadCache, Source};
use crate::state::State;
use crate::utils::{
    build_font_config, copy_dir_all, get_a11y_bus_args, get_fonts_args, get_host_env, path_to_str,
    status, status_info, status_success, status_warn, verbose,
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
        // Include referenced module files and source fingerprints so edits invalidate state.
        if let Ok(manifest) = Manifest::from_file(path) {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            for module in &manifest.modules {
                match module {
                    Module::Reference(name) => {
                        let ref_path = parent.join(name);
                        if ref_path.is_file() {
                            Self::hash_file_into(&mut hasher, &ref_path)?;
                        }
                    }
                    Module::Object { sources, name, .. } => {
                        hasher.update(name.as_bytes());
                        if let Ok(typed) = Source::from_values(sources) {
                            hasher.update(sources::sources_fingerprint(&typed).as_bytes());
                        }
                    }
                }
            }
        }
        let result = hasher.finalize();
        let mut hash = String::with_capacity(64);
        for b in result {
            write!(&mut hash, "{b:02x}")?;
        }
        Ok(hash)
    }

    fn hash_file_into(hasher: &mut Sha256, path: &Path) -> Result<()> {
        use std::io::Read;
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
        // Without the flatpak CLI we only ensure the SDK/runtime trees exist.
        if let Some(required) = manifest.finish_args.iter().find_map(|arg| {
            let (key, value) = arg.split_once('=')?;
            (key == "--require-version").then_some(value)
        }) {
            verbose(format!(
                "Manifest requests flatpak >= {required}; skipping CLI version probe (no flatpak CLI dependency)"
            ));
        }
        Ok(())
    }

    fn bwrap_runner(&self) -> Result<BwrapRunner> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let (sdk, _runtime) =
            ensure_sdk_and_runtime(&manifest.sdk, &manifest.runtime, &manifest.runtime_version)?;
        Ok(BwrapRunner::for_sdk(
            &sdk,
            manifest.id.clone(),
            self.state.base_dir.as_path(),
        ))
    }

    fn run_sandboxed(
        &self,
        runner: &BwrapRunner,
        sandbox: &BuildSandbox,
        repo_dir: &Path,
        argv: &[String],
        extra_fs: &[&str],
        share_network: bool,
        cwd: Option<&Path>,
    ) -> Result<()> {
        let mut filesystem_binds = vec![sandbox.fs_ws.clone(), sandbox.fs_repo.clone()];
        filesystem_binds.extend(extra_fs.iter().map(|s| (*s).to_string()));

        let mut env = Vec::new();
        for arg in sandbox
            .env_args
            .iter()
            .chain(sandbox.path_overrides.iter())
        {
            if let Some(rest) = arg.strip_prefix("--env=")
                && let Some((k, v)) = rest.split_once('=')
            {
                env.push((k.to_string(), v.to_string()));
            }
        }

        runner.run(&RunSpec {
            repo_dir: repo_dir.to_path_buf(),
            argv: argv.to_vec(),
            env,
            filesystem_binds,
            extra_args: vec![],
            share_network,
            cwd: cwd.map(Path::to_path_buf),
        })
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

        self.download_application_sources()?;

        let module = self.application_module()?;
        let Module::Object {
            name,
            buildsystem,
            config_opts,
            build_commands,
            build_options: module_build_options,
            post_install,
            ..
        } = module
        else {
            return Err(anyhow::anyhow!(
                "Application module is not a defined module"
            ));
        };

        let merged_config = manifest.merged_config_opts(config_opts.as_deref());
        let module_bo = module_build_options.as_ref();
        let num_cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

        let runner = self.bwrap_runner()?;
        let repo_dir = self.build_dirs.repo_dir();

        match buildsystem.as_deref() {
            Some("meson") => {
                self.run_meson(&runner, &repo_dir, rebuild, &merged_config, module_bo)?;
            }
            Some("cmake" | "cmake-ninja") => {
                self.run_cmake(&runner, &repo_dir, rebuild, &merged_config, module_bo)?;
            }
            Some("simple") => self.run_simple(
                &runner,
                &repo_dir,
                manifest,
                build_commands.as_ref(),
                module_bo,
                &name,
                num_cpus,
            )?,
            Some("qmake") => {
                return Err(anyhow::anyhow!("qmake build system is not supported"));
            }
            _ => self.run_autotools(
                &runner,
                &repo_dir,
                rebuild,
                &merged_config,
                module_bo,
                num_cpus,
            )?,
        }
        if let Some(post_install) = post_install {
            let sandbox = self.build_sandbox(module_bo, manifest);
            for command in &post_install {
                let processed = Self::substitute_vars(command, &manifest.id, &name, num_cpus);
                let argv = Self::parse_command_line(&processed)?;
                self.run_sandboxed(&runner, &sandbox, &repo_dir, &argv, &[], true, None)?;
            }
        }

        Ok(())
    }

    fn download_application_sources(&self) -> Result<()> {
        let module = self.application_module()?;
        let Module::Object { name, sources, .. } = module else {
            return Err(anyhow::anyhow!(
                "Application module is not a defined module"
            ));
        };
        let typed = Source::from_values(&sources)?;
        // Pure `dir` sources are resolved at build time (no copy into .flatplay).
        if typed.iter().all(|s| matches!(s, Source::Dir { .. })) {
            verbose(format!(
                "Application module {name} uses directory sources only; skipping materialize"
            ));
            return Ok(());
        }

        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        let manifest_dir = manifest_path
            .parent()
            .context("Manifest path has no parent directory")?;
        let source_dir = self.build_dirs.module_source_dir(&name);
        let cache = DownloadCache::new(&self.build_dirs.build_dir());
        sources::materialize_sources(&typed, &source_dir, manifest_dir, &cache, &name)
    }

    fn build_sandbox(
        &self,
        module_build_options: Option<&BuildOptions>,
        manifest: &Manifest,
    ) -> BuildSandbox {
        BuildSandbox {
            fs_ws: format!("--filesystem={}", self.state.base_dir.display()),
            fs_repo: format!("--filesystem={}", self.build_dirs.repo_dir().display()),
            env_args: manifest
                .merged_env(module_build_options)
                .iter()
                .map(|(k, v)| format!("--env={k}={v}"))
                .collect(),
            path_overrides: manifest.path_overrides(module_build_options),
        }
    }

    fn sandbox_args<'s>(
        sandbox: &'s BuildSandbox,
        repo_dir_str: &'s str,
        extra_fs: &'s [&'s str],
    ) -> Vec<&'s str> {
        let mut args: Vec<&str> =
            vec!["build", "--share=network", &sandbox.fs_ws, &sandbox.fs_repo];
        args.extend_from_slice(extra_fs);
        args.extend(sandbox.env_args.iter().map(String::as_str));
        args.extend(sandbox.path_overrides.iter().map(String::as_str));
        args.push(repo_dir_str);
        args
    }

    /// Build a `flatpak build …` argv. `command_argv` is a pre-split command (use
    /// [`Self::parse_command_line`] for manifest `build-commands` / `post-install`).
    fn build_command<'s>(
        sandbox: &'s BuildSandbox,
        repo_dir_str: &'s str,
        command_argv: &'s [String],
        extra_fs: &'s [&'s str],
        extra_args: &'s [&'s str],
    ) -> Vec<&'s str> {
        let mut args = Self::sandbox_args(sandbox, repo_dir_str, extra_fs);
        args.extend(command_argv.iter().map(String::as_str));
        args.extend_from_slice(extra_args);
        args
    }

    fn parse_command_line(command: &str) -> Result<Vec<String>> {
        shell_words::split(command)
            .with_context(|| format!("Failed to parse command line: {command}"))
    }

    fn substitute_vars(
        command: &str,
        flatpak_id: &str,
        module_name: &str,
        num_cpus: usize,
    ) -> String {
        command
            .replace("${FLATPAK_ID}", flatpak_id)
            .replace("${FLATPAK_ARCH}", std::env::consts::ARCH)
            .replace("${FLATPAK_DEST}", "/app")
            .replace("${FLATPAK_BUILDER_N_JOBS}", &num_cpus.to_string())
            .replace(
                "${FLATPAK_BUILDER_BUILDDIR}",
                &format!("/run/build/{module_name}"),
            )
    }

    fn run_meson(
        &self,
        runner: &BwrapRunner,
        repo_dir: &Path,
        rebuild: bool,
        config_opts: &[&str],
        module_build_options: Option<&BuildOptions>,
    ) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let module = self.application_module()?;
        let Module::Object {
            name,
            subdir,
            sources,
            ..
        } = module
        else {
            return Err(anyhow::anyhow!(
                "Application module is not a defined module"
            ));
        };
        let source_dir = self.resolve_module_source_dir(&name, &sources, subdir.as_deref())?;
        let source_dir_str = path_to_str(&source_dir)?;
        let build_dir = self.build_dirs.build_system_dir();
        std::fs::create_dir_all(&build_dir)?;
        let build_dir_str = path_to_str(&build_dir)?;
        let sandbox = self.build_sandbox(module_build_options, manifest);
        let fs_builddir = format!("--filesystem={build_dir_str}");
        let extra_fs = [fs_builddir.as_str()];

        if !rebuild {
            let mut setup: Vec<String> = vec!["meson".into(), "setup".into()];
            setup.extend(config_opts.iter().map(|s| (*s).to_string()));
            setup.extend([
                "--prefix=/app".into(),
                source_dir_str.into(),
                build_dir_str.into(),
            ]);
            self.run_sandboxed(runner, &sandbox, repo_dir, &setup, &extra_fs, true, None)?;
        }

        let ninja_cmd = vec![
            "ninja".to_string(),
            "-C".to_string(),
            build_dir_str.to_string(),
        ];
        self.run_sandboxed(runner, &sandbox, repo_dir, &ninja_cmd, &extra_fs, true, None)?;
        let install_cmd = vec![
            "meson".to_string(),
            "install".to_string(),
            "-C".to_string(),
            build_dir_str.to_string(),
        ];
        self.run_sandboxed(runner, &sandbox, repo_dir, &install_cmd, &extra_fs, true, None)
    }

    fn resolve_module_source_dir(
        &self,
        name: &str,
        sources: &[serde_json::Value],
        subdir: Option<&str>,
    ) -> Result<PathBuf> {
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        let manifest_dir = manifest_path
            .parent()
            .context("Manifest path has no parent directory")?;
        let typed = Source::from_values(sources).unwrap_or_else(|_| vec![]);
        let base = sources::resolve_dir_source(&typed, manifest_dir)
            .unwrap_or_else(|| self.build_dirs.module_source_dir(name));
        let path = if let Some(subdir) = subdir {
            base.join(subdir)
        } else {
            base
        };
        path.canonicalize()
            .with_context(|| format!("Source directory not found: {}", path.display()))
    }

    fn run_cmake(
        &self,
        runner: &BwrapRunner,
        repo_dir: &Path,
        rebuild: bool,
        config_opts: &[&str],
        module_build_options: Option<&BuildOptions>,
    ) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let module = self.application_module()?;
        let Module::Object {
            name,
            subdir,
            sources,
            ..
        } = module
        else {
            return Err(anyhow::anyhow!(
                "Application module is not a defined module"
            ));
        };
        let source_dir = self.resolve_module_source_dir(&name, &sources, subdir.as_deref())?;
        let source_dir_str = path_to_str(&source_dir)?;
        let build_dir = self.build_dirs.build_system_dir();
        let build_dir_str = path_to_str(&build_dir)?;
        let sandbox = self.build_sandbox(module_build_options, manifest);
        let fs_builddir = format!("--filesystem={build_dir_str}");
        let extra_fs = [fs_builddir.as_str()];

        if !rebuild {
            let mut configure = vec![
                "cmake".into(),
                "-G".into(),
                "Ninja".into(),
                format!("-B{build_dir_str}"),
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=1".into(),
                "-DCMAKE_BUILD_TYPE=RelWithDebInfo".into(),
                "-DCMAKE_INSTALL_PREFIX=/app".into(),
            ];
            configure.extend(config_opts.iter().map(|s| (*s).to_string()));
            configure.push(source_dir_str.to_string());
            self.run_sandboxed(runner, &sandbox, repo_dir, &configure, &extra_fs, true, None)?;
        }

        let ninja_cmd = vec![
            "ninja".to_string(),
            "-C".to_string(),
            build_dir_str.to_string(),
        ];
        self.run_sandboxed(runner, &sandbox, repo_dir, &ninja_cmd, &extra_fs, true, None)?;
        let install_cmd = vec![
            "ninja".to_string(),
            "-C".to_string(),
            build_dir_str.to_string(),
            "install".to_string(),
        ];
        self.run_sandboxed(runner, &sandbox, repo_dir, &install_cmd, &extra_fs, true, None)
    }

    fn run_simple(
        &self,
        runner: &BwrapRunner,
        repo_dir: &Path,
        manifest: &Manifest,
        build_commands: Option<&Vec<String>>,
        module_build_options: Option<&BuildOptions>,
        module_name: &str,
        num_cpus: usize,
    ) -> Result<()> {
        if let Some(commands) = build_commands {
            let sandbox = self.build_sandbox(module_build_options, manifest);
            for command in commands {
                let processed = Self::substitute_vars(command, &manifest.id, module_name, num_cpus);
                let argv = Self::parse_command_line(&processed)?;
                self.run_sandboxed(runner, &sandbox, repo_dir, &argv, &[], true, None)?;
            }
        }
        Ok(())
    }

    fn run_autotools(
        &self,
        runner: &BwrapRunner,
        repo_dir: &Path,
        rebuild: bool,
        config_opts: &[&str],
        module_build_options: Option<&BuildOptions>,
        num_cpus: usize,
    ) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let module = self.application_module()?;
        let Module::Object {
            name,
            builddir,
            subdir,
            ..
        } = module
        else {
            return Err(anyhow::anyhow!(
                "Application module is not a defined module"
            ));
        };
        let source_dir = {
            let base = self.build_dirs.build_dir().join(&name);
            if let Some(subdir) = &subdir {
                base.join(subdir)
            } else {
                base
            }
        };
        let use_builddir = builddir.unwrap_or(false);

        let sandbox = self.build_sandbox(module_build_options, manifest);
        let source_dir_str = path_to_str(&source_dir)?;

        let make_argv = |jobs_flag: &str| {
            vec![
                "make".to_string(),
                "V=0".to_string(),
                jobs_flag.to_string(),
                "install".to_string(),
            ]
        };

        match (rebuild, use_builddir) {
            (false, true) => {
                let mut configure = vec![
                    format!("{source_dir_str}/configure"),
                    "--prefix=/app".into(),
                ];
                configure.extend(config_opts.iter().map(|s| (*s).to_string()));
                self.run_sandboxed(runner, &sandbox, repo_dir, &configure, &[], true, None)?;

                let build_dir = self.build_dirs.build_system_dir();
                let build_dir_str = path_to_str(&build_dir)?;
                let fs_builddir = format!("--filesystem={build_dir_str}");
                let extra_fs = [fs_builddir.as_str()];
                let jobs_flag = format!("-j{num_cpus}");
                let make_cmd = make_argv(&jobs_flag);
                self.run_sandboxed(runner, &sandbox, repo_dir, &make_cmd, &extra_fs, true, None)
            }
            (false, false) => {
                let mut configure = vec![
                    format!("{source_dir_str}/configure"),
                    "--prefix=/app".into(),
                ];
                configure.extend(config_opts.iter().map(|s| (*s).to_string()));
                self.run_sandboxed(runner, &sandbox, repo_dir, &configure, &[], true, None)?;

                let jobs_flag = format!("-j{num_cpus}");
                let make_cmd = make_argv(&jobs_flag);
                self.run_sandboxed(runner, &sandbox, repo_dir, &make_cmd, &[], true, None)
            }
            (true, true) => {
                let build_dir = self.build_dirs.build_system_dir();
                let build_dir_str = path_to_str(&build_dir)?;
                let fs_builddir = format!("--filesystem={build_dir_str}");
                let extra_fs = [fs_builddir.as_str()];
                let jobs_flag = format!("-j{num_cpus}");
                let make_cmd = make_argv(&jobs_flag);
                self.run_sandboxed(runner, &sandbox, repo_dir, &make_cmd, &extra_fs, true, None)
            }
            (true, false) => {
                let jobs_flag = format!("-j{num_cpus}");
                let make_cmd = make_argv(&jobs_flag);
                self.run_sandboxed(runner, &sandbox, repo_dir, &make_cmd, &[], true, None)
            }
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
        let runner = self.bwrap_runner()?;
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

    fn sandbox_run_args(
        manifest: &Manifest,
        repo_dir: &Path,
        sandbox: &BuildSandbox,
        with_dev_paths: bool,
    ) -> Result<Vec<String>> {
        let uid = geteuid();
        let bind_mount_arg = format!(
            "--bind-mount=/run/user/{uid}/doc=/run/user/{uid}/doc/by-app/{}",
            manifest.id
        );

        let mut args: Vec<String> = vec![
            "build".to_string(),
            "--with-appdir".to_string(),
            "--allow=devel".to_string(),
            bind_mount_arg,
            sandbox.fs_ws.clone(),
            sandbox.fs_repo.clone(),
            "--talk-name=org.freedesktop.portal.*".to_string(),
            "--talk-name=org.a11y.Bus".to_string(),
        ];

        let host_env = get_host_env();
        verbose(format!(
            "Forwarding host env vars: {:?}",
            host_env.keys().collect::<Vec<_>>()
        ));
        args.extend(
            host_env
                .into_iter()
                .map(|(key, value)| format!("--env={key}={value}")),
        );

        match get_a11y_bus_args() {
            Ok(a11y_args) => args.extend(a11y_args),
            Err(error) => verbose(format!("a11y bus not available: {error:#}")),
        }

        if with_dev_paths {
            args.push("--share=network".to_string());
            args.extend(sandbox.path_overrides.clone());
        }

        match build_font_config().and_then(|config_path| get_fonts_args(&config_path)) {
            Ok(fonts_args) => args.extend(fonts_args),
            Err(error) => verbose(format!("fonts not available: {error:#}")),
        }

        args.extend(manifest.finish_args_filtered());
        args.push(path_to_str(repo_dir)?.to_string());

        Ok(args)
    }

    pub fn run(&self) -> Result<()> {
        if !self.state.application_built {
            return Err(anyhow::anyhow!(
                "Application not built. Please run `build` first."
            ));
        }
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let runner = self.bwrap_runner()?;
        let mut argv = vec![manifest.command.clone()];
        if let Some(x_run_args) = &manifest.x_run_args {
            argv.extend(x_run_args.clone());
        }
        self.run_app_spec(&runner, &repo_dir, manifest, argv, false)
    }

    fn run_app_spec(
        &self,
        runner: &BwrapRunner,
        repo_dir: &Path,
        manifest: &Manifest,
        argv: Vec<String>,
        with_dev_paths: bool,
    ) -> Result<()> {
        let sandbox = self.build_sandbox(None, manifest);
        let mut filesystem_binds = vec![sandbox.fs_ws.clone(), sandbox.fs_repo.clone()];
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

        runner.run(&RunSpec {
            repo_dir: repo_dir.to_path_buf(),
            argv,
            env,
            filesystem_binds,
            extra_args: vec![],
            share_network: with_dev_paths
                || manifest
                    .finish_args
                    .iter()
                    .any(|a| a.contains("share=network")),
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

        if finalized_repo_dir.is_dir() {
            fs::remove_dir_all(&finalized_repo_dir)?;
        }
        copy_dir_all(&repo_dir, &finalized_repo_dir)?;

        // Write finish metadata without the flatpak CLI.
        let meta_path = finalized_repo_dir.join("metadata");
        let mut meta = fs::read_to_string(&meta_path).unwrap_or_default();
        if !meta.contains("[Context]") {
            use std::fmt::Write as _;
            let _ = writeln!(meta, "\n[Context]");
            for arg in manifest.finish_args_filtered() {
                let _ = writeln!(meta, "# finish-arg: {arg}");
            }
            let _ = writeln!(meta, "\n[Application]");
            let _ = writeln!(meta, "command={}", manifest.command);
            fs::write(&meta_path, meta)?;
        }

        let bundle_name = format!("{}-files.tar", manifest.id);
        let bundle_path = self.state.base_dir.join(&bundle_name);
        let files = finalized_repo_dir.join("files");
        // Portable "bundle": tar of /app files (not an OSTree .flatpak; no flatpak CLI).
        let status = std::process::Command::new("tar")
            .args([
                "-cf",
                path_to_str(&bundle_path)?,
                "-C",
                path_to_str(&files)?,
                ".",
            ])
            .status()
            .context("Failed to spawn tar for export (host tar is used only for packaging)")?;
        if !status.success() {
            anyhow::bail!("tar export failed with {status}");
        }

        status_success(format!(
            "Exported {bundle_name} (files tree; install-time OSTree bundles need a separate ostree toolchain)"
        ));
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
        // Empty app tree is fine; drop into SDK with bash.
        let runner = self.bwrap_runner()?;
        self.run_app_spec(&runner, &repo_dir, manifest, vec!["bash".into()], true)
    }

    pub fn build_terminal(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let runner = self.bwrap_runner()?;
        self.run_app_spec(&runner, &repo_dir, manifest, vec!["bash".into()], true)
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
