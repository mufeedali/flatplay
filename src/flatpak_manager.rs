use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::{Select, theme::SimpleTheme};
use nix::unistd::geteuid;

use crate::build_dirs::BuildDirs;
use crate::command::{flatpak_builder, run_command};
use crate::manifest::{BuildOptions, Manifest, Module, find_manifests_in_path};
use crate::state::State;
use crate::utils::{
    build_font_config, download_file, extract_archive, get_a11y_bus_args, get_fonts_args,
    get_host_env, guess_archive_type, path_to_str, status, status_info, status_success,
    status_warn, verbose, verify_sha256_hex, version_less_than,
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
        let content = fs::read(path)?;
        let mut hasher = Sha256::new();
        hasher.update(&content);
        let result = hasher.finalize();

        let mut hash = String::with_capacity(64);
        for b in result {
            write!(&mut hash, "{b:02x}")?;
        }
        Ok(hash)
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
            if key == "--require-version" {
                Some(value)
            } else {
                None
            }
        });
        if let Some(required) = required {
            let version = String::from_utf8_lossy(
                &std::process::Command::new("flatpak")
                    .arg("--version")
                    .output()
                    .context("Failed to get flatpak version")?
                    .stdout,
            )
            .replace("Flatpak ", "")
            .trim()
            .to_string();
            if version_less_than(&version, required) {
                return Err(anyhow::anyhow!(
                    "Manifest requires flatpak >= {required} but {version} is installed"
                ));
            }
        }
        Ok(())
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

    fn init_build(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();

        status(format!("{}", "Initializing build environment...".bold()));
        run_command(
            "flatpak",
            &[
                "build-init",
                path_to_str(&repo_dir)?,
                &manifest.id,
                &manifest.sdk,
                &manifest.runtime,
                &manifest.runtime_version,
            ],
            Some(self.state.base_dir.as_path()),
        )
    }

    fn init(&self) -> Result<()> {
        if self.is_build_initialized() {
            return Ok(());
        }

        self.init_build()?;
        Ok(())
    }

    fn build_application(&self, rebuild: bool) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let repo_dir_str = path_to_str(&repo_dir)?;

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

        match buildsystem.as_deref() {
            Some("meson") => self.run_meson(repo_dir_str, rebuild, &merged_config, module_bo)?,
            Some("cmake" | "cmake-ninja") => {
                self.run_cmake(repo_dir_str, rebuild, &merged_config, module_bo)?;
            }
            Some("simple") => self.run_simple(
                manifest,
                repo_dir_str,
                build_commands.as_ref(),
                module_bo,
                &name,
                num_cpus,
            )?,
            Some("qmake") => {
                return Err(anyhow::anyhow!("qmake build system is not supported"));
            }
            _ => self.run_autotools(repo_dir_str, rebuild, &merged_config, module_bo, num_cpus)?,
        }
        if let Some(post_install) = post_install {
            let sandbox = self.build_sandbox(module_bo, manifest);
            for command in &post_install {
                let processed = Self::substitute_vars(command, &manifest.id, &name, num_cpus);
                let args = Self::build_command(&sandbox, repo_dir_str, &processed, &[], &[]);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
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
        let source_dir = self.build_dirs.build_dir().join(&name);

        if source_dir.exists() {
            fs::remove_dir_all(&source_dir)?;
        }

        for source in &sources {
            let Some(source_type) = source.get("type").and_then(|v| v.as_str()) else {
                return Err(anyhow::anyhow!(
                    "Source in module '{name}' is missing a type field"
                ));
            };
            match source_type {
                "git" => self.handle_git_source(source, &name, &source_dir)?,
                "dir" => verbose(format!("Using local directory source for {name}")),
                "archive" => self.handle_archive_source(source, &name, &source_dir)?,
                "file" => self.handle_file_source(source, &name, &source_dir)?,
                other => {
                    return Err(anyhow::anyhow!(
                        "Source type '{other}' in module '{name}' is not yet supported"
                    ));
                }
            }
        }
        Ok(())
    }

    fn handle_git_source(
        &self,
        source: &serde_json::Value,
        name: &str,
        source_dir: &Path,
    ) -> Result<()> {
        let url = source
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Git source in module '{name}' must specify url"))?;
        let tag = source.get("tag").and_then(|v| v.as_str());
        let commit = source.get("commit").and_then(|v| v.as_str());
        let branch = source.get("branch").and_then(|v| v.as_str());

        match (commit, tag, branch) {
            (Some(commit), _, _) => {
                status(format!("Cloning {name} from {url} (commit {commit})"));
                run_command(
                    "git",
                    &[
                        "clone",
                        "--recurse-submodules",
                        url,
                        path_to_str(source_dir)?,
                    ],
                    Some(self.state.base_dir.as_path()),
                )?;
                run_command(
                    "git",
                    &["-C", path_to_str(source_dir)?, "checkout", commit],
                    Some(self.state.base_dir.as_path()),
                )?;
            }
            (None, Some(tag), _) => {
                status(format!("Cloning {name} from {url} (tag {tag})"));
                run_command(
                    "git",
                    &[
                        "clone",
                        "--recurse-submodules",
                        "--branch",
                        tag,
                        "--depth",
                        "1",
                        url,
                        path_to_str(source_dir)?,
                    ],
                    Some(self.state.base_dir.as_path()),
                )?;
            }
            (None, None, Some(branch)) => {
                status(format!("Cloning {name} from {url} (branch {branch})"));
                run_command(
                    "git",
                    &[
                        "clone",
                        "--recurse-submodules",
                        "--branch",
                        branch,
                        "--depth",
                        "1",
                        url,
                        path_to_str(source_dir)?,
                    ],
                    Some(self.state.base_dir.as_path()),
                )?;
            }
            (None, None, None) => {
                return Err(anyhow::anyhow!(
                    "Git source in module '{name}' must specify one of: tag, commit, branch"
                ));
            }
        }
        Ok(())
    }

    fn handle_archive_source(
        &self,
        source: &serde_json::Value,
        name: &str,
        source_dir: &Path,
    ) -> Result<()> {
        let is_url = source.get("url").and_then(|v| v.as_str()).is_some();
        let url = source
            .get("url")
            .and_then(|v| v.as_str())
            .or_else(|| source.get("path").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                anyhow::anyhow!("Archive source in module '{name}' must specify url or path")
            })?;
        let sha256 = source
            .get("sha256")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!("Archive source in module '{name}' must specify sha256")
            })?;
        let strip = source
            .get("strip-components")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(1);
        let archive_type = source
            .get("archive-type")
            .and_then(|v| v.as_str())
            .map_or_else(|| guess_archive_type(url), ToString::to_string);
        let filename = source
            .get("dest-filename")
            .and_then(|v| v.as_str())
            .map_or_else(
                || {
                    Path::new(url)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("archive")
                        .to_string()
                },
                ToString::to_string,
            );
        let archive_path = self.build_dirs.build_dir().join(&filename);

        if is_url {
            status(format!("Downloading {name} from {url}"));
            download_file(url, &archive_path)?;
        } else {
            status(format!("Copying {name} from {url}"));
            fs::copy(url, &archive_path)?;
        }
        verify_sha256_hex(&archive_path, sha256)?;
        extract_archive(&archive_path, &archive_type, source_dir, strip)?;
        fs::remove_file(&archive_path).ok();
        Ok(())
    }

    fn handle_file_source(
        &self,
        source: &serde_json::Value,
        name: &str,
        source_dir: &Path,
    ) -> Result<()> {
        let url = source
            .get("url")
            .and_then(|v| v.as_str())
            .or_else(|| source.get("path").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                anyhow::anyhow!("File source in module '{name}' must specify url or path")
            })?;
        let filename = source
            .get("dest-filename")
            .and_then(|v| v.as_str())
            .map_or_else(
                || {
                    Path::new(url)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("file")
                        .to_string()
                },
                ToString::to_string,
            );

        if let Some(expected) = source.get("sha256").and_then(|v| v.as_str()) {
            let is_url = source.get("url").and_then(|v| v.as_str()).is_some();
            let temp_path = self.build_dirs.build_dir().join(&filename);
            if is_url {
                status(format!("Downloading {name} from {url}"));
                download_file(url, &temp_path)?;
            } else {
                status(format!("Copying {name} from {url}"));
                fs::copy(url, &temp_path)?;
            }
            verify_sha256_hex(&temp_path, expected)?;
            let dest_path = source_dir.join(&filename);
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&temp_path, &dest_path)?;
        } else {
            status(format!("Copying {name} from {url}"));
            fs::copy(url, source_dir.join(&filename))?;
        }
        Ok(())
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

    fn build_command<'s>(
        sandbox: &'s BuildSandbox,
        repo_dir_str: &'s str,
        command: &'s str,
        extra_fs: &'s [&'s str],
        extra_args: &'s [&'s str],
    ) -> Vec<&'s str> {
        let mut args = Self::sandbox_args(sandbox, repo_dir_str, extra_fs);
        let split: Vec<&str> = command.split_whitespace().collect();
        args.extend(&split);
        args.extend_from_slice(extra_args);
        args
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
        repo_dir_str: &str,
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
        let source_dir = {
            let base = if let Some(source) = sources.first() {
                if let (Some("dir"), Some(path)) = (
                    source.get("type").and_then(|v| v.as_str()),
                    source.get("path").and_then(|v| v.as_str()),
                ) {
                    let manifest_path = self
                        .state
                        .active_manifest
                        .as_ref()
                        .context("No active manifest")?;
                    let manifest_dir = manifest_path
                        .parent()
                        .context("Manifest path has no parent directory")?;
                    manifest_dir.join(path)
                } else {
                    self.build_dirs.build_dir().join(&name)
                }
            } else {
                self.build_dirs.build_dir().join(&name)
            };
            if let Some(subdir) = &subdir {
                base.join(subdir)
            } else {
                base
            }
        };
        let source_dir = source_dir
            .canonicalize()
            .context("Source directory not found")?;
        let source_dir_str = path_to_str(&source_dir)?;
        let build_dir = self.build_dirs.build_system_dir();
        let build_dir_str = path_to_str(&build_dir)?;
        let sandbox = self.build_sandbox(module_build_options, manifest);
        let fs_builddir = format!("--filesystem={build_dir_str}");
        let extra_fs = [fs_builddir.as_str()];

        if !rebuild {
            let mut args = Self::sandbox_args(&sandbox, repo_dir_str, &extra_fs);
            args.extend(&["meson", "setup"]);
            args.extend_from_slice(config_opts);
            args.extend(&["--prefix=/app", source_dir_str, build_dir_str]);
            run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
        }

        {
            let ninja_cmd = format!("ninja -C {build_dir_str}");
            let args = Self::build_command(&sandbox, repo_dir_str, &ninja_cmd, &extra_fs, &[]);
            run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
        }
        {
            let install_cmd = format!("meson install -C {build_dir_str}");
            let args = Self::build_command(&sandbox, repo_dir_str, &install_cmd, &extra_fs, &[]);
            run_command("flatpak", &args, Some(self.state.base_dir.as_path()))
        }
    }

    fn run_cmake(
        &self,
        repo_dir_str: &str,
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
        let source_dir = {
            let base = if let Some(source) = sources.first() {
                if let (Some("dir"), Some(path)) = (
                    source.get("type").and_then(|v| v.as_str()),
                    source.get("path").and_then(|v| v.as_str()),
                ) {
                    let manifest_path = self
                        .state
                        .active_manifest
                        .as_ref()
                        .context("No active manifest")?;
                    let manifest_dir = manifest_path
                        .parent()
                        .context("Manifest path has no parent directory")?;
                    manifest_dir.join(path)
                } else {
                    self.build_dirs.build_dir().join(&name)
                }
            } else {
                self.build_dirs.build_dir().join(&name)
            };
            if let Some(subdir) = &subdir {
                base.join(subdir)
            } else {
                base
            }
        }
        .canonicalize()
        .context("Source directory not found")?;
        let source_dir_str = path_to_str(&source_dir)?;
        let build_dir = self.build_dirs.build_system_dir();
        let build_dir_str = path_to_str(&build_dir)?;
        let sandbox = self.build_sandbox(module_build_options, manifest);
        let fs_builddir = format!("--filesystem={build_dir_str}");
        let extra_fs = [fs_builddir.as_str()];

        if !rebuild {
            let b_flag = format!("-B{build_dir_str}");
            let mut args = Self::sandbox_args(&sandbox, repo_dir_str, &extra_fs);
            args.extend(&["cmake", "-G", "Ninja", &b_flag]);
            args.extend(&[
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=1",
                "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
                "-DCMAKE_INSTALL_PREFIX=/app",
            ]);
            args.extend_from_slice(config_opts);
            args.push(source_dir_str);
            run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
        }

        {
            let ninja_cmd = format!("ninja -C {build_dir_str}");
            let args = Self::build_command(&sandbox, repo_dir_str, &ninja_cmd, &extra_fs, &[]);
            run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
        }
        {
            let install_cmd = format!("ninja -C {build_dir_str} install");
            let args = Self::build_command(&sandbox, repo_dir_str, &install_cmd, &extra_fs, &[]);
            run_command("flatpak", &args, Some(self.state.base_dir.as_path()))
        }
    }

    fn run_simple(
        &self,
        manifest: &Manifest,
        repo_dir_str: &str,
        build_commands: Option<&Vec<String>>,
        module_build_options: Option<&BuildOptions>,
        module_name: &str,
        num_cpus: usize,
    ) -> Result<()> {
        if let Some(commands) = build_commands {
            let sandbox = self.build_sandbox(module_build_options, manifest);
            for command in commands {
                let processed = Self::substitute_vars(command, &manifest.id, module_name, num_cpus);
                let args = Self::build_command(&sandbox, repo_dir_str, &processed, &[], &[]);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;
            }
        }
        Ok(())
    }

    fn run_autotools(
        &self,
        repo_dir_str: &str,
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

        match (rebuild, use_builddir) {
            (false, true) => {
                let mut args = Self::sandbox_args(&sandbox, repo_dir_str, &[]);
                let configure_path = format!("{source_dir_str}/configure");
                args.extend(&[&configure_path, "--prefix=/app"]);
                args.extend_from_slice(config_opts);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;

                let build_dir = self.build_dirs.build_system_dir();
                let build_dir_str = path_to_str(&build_dir)?;
                let fs_builddir = format!("--filesystem={build_dir_str}");
                let extra_fs = [fs_builddir.as_str()];
                let jobs_flag = format!("-j{num_cpus}");
                let make_args = ["V=0", jobs_flag.as_str(), "install"];
                let args =
                    Self::build_command(&sandbox, repo_dir_str, "make", &extra_fs, &make_args);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))
            }
            (false, false) => {
                let mut args = Self::sandbox_args(&sandbox, repo_dir_str, &[]);
                let configure_path = format!("{source_dir_str}/configure");
                args.extend(&[&configure_path, "--prefix=/app"]);
                args.extend_from_slice(config_opts);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))?;

                let jobs_flag = format!("-j{num_cpus}");
                let make_args = ["V=0", jobs_flag.as_str(), "install"];
                let args = Self::build_command(&sandbox, repo_dir_str, "make", &[], &make_args);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))
            }
            (true, true) => {
                let build_dir = self.build_dirs.build_system_dir();
                let build_dir_str = path_to_str(&build_dir)?;
                let fs_builddir = format!("--filesystem={build_dir_str}");
                let extra_fs = [fs_builddir.as_str()];
                let jobs_flag = format!("-j{num_cpus}");
                let make_args = ["V=0", jobs_flag.as_str(), "install"];
                let args =
                    Self::build_command(&sandbox, repo_dir_str, "make", &extra_fs, &make_args);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))
            }
            (true, false) => {
                let jobs_flag = format!("-j{num_cpus}");
                let make_args = ["V=0", jobs_flag.as_str(), "install"];
                let args = Self::build_command(&sandbox, repo_dir_str, "make", &[], &make_args);
                run_command("flatpak", &args, Some(self.state.base_dir.as_path()))
            }
        }
    }

    fn build_dependencies(&mut self) -> Result<()> {
        status(format!("{}", "Building dependencies...".bold()));
        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        let repo_dir = self.build_dirs.repo_dir();
        let state_dir = self.build_dirs.flatpak_builder_dir();
        let stop_at = self.last_module_name()?;
        flatpak_builder(
            &[
                "--ccache",
                "--force-clean",
                "--disable-updates",
                "--disable-download",
                "--build-only",
                "--keep-build-dirs",
                &format!("--state-dir={}", path_to_str(&state_dir)?),
                &format!("--stop-at={stop_at}"),
                path_to_str(&repo_dir)?,
                path_to_str(manifest_path)?,
            ],
            Some(self.state.base_dir.as_path()),
        )?;
        self.state.dependencies_built = true;
        self.state.save()
    }

    pub fn update_dependencies(&mut self) -> Result<()> {
        status(format!("{}", "Updating dependencies...".bold()));

        let manifest_path = self
            .state
            .active_manifest
            .as_ref()
            .context("No active manifest")?;
        let repo_dir = self.build_dirs.repo_dir();
        let state_dir = self.build_dirs.flatpak_builder_dir();
        let stop_at = self.last_module_name()?;
        flatpak_builder(
            &[
                "--ccache",
                "--force-clean",
                "--disable-updates",
                "--download-only",
                &format!("--state-dir={}", path_to_str(&state_dir)?),
                &format!("--stop-at={stop_at}"),
                path_to_str(&repo_dir)?,
                path_to_str(manifest_path)?,
            ],
            Some(self.state.base_dir.as_path()),
        )?;
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
        let sandbox = self.build_sandbox(None, manifest);

        let mut args = Self::sandbox_run_args(manifest, &repo_dir, &sandbox, false)?;
        args.push(manifest.command.clone());
        if let Some(x_run_args) = &manifest.x_run_args {
            args.extend(x_run_args.clone());
        }

        let args_str: Vec<&str> = args.iter().map(String::as_str).collect();
        run_command("flatpak", &args_str, Some(self.state.base_dir.as_path()))
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

        // Remove finalized repo
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
            Some(self.state.base_dir.as_path()),
        )?;

        // Finalize build
        let mut args: Vec<String> = vec!["build-finish".to_string()];

        args.extend(manifest.finish_args_filtered());
        args.push(format!("--command={}", manifest.command));
        args.push(path_to_str(&finalized_repo_dir)?.to_string());

        let args_str: Vec<&str> = args.iter().map(String::as_str).collect();

        run_command("flatpak", &args_str, Some(self.state.base_dir.as_path()))?;

        // Export build
        run_command(
            "flatpak",
            &[
                "build-export",
                path_to_str(&ostree_dir)?,
                path_to_str(&finalized_repo_dir)?,
            ],
            Some(self.state.base_dir.as_path()),
        )?;

        // Bundle build
        let bundle_name = format!("{}.flatpak", manifest.id);
        run_command(
            "flatpak",
            &[
                "build-bundle",
                path_to_str(&ostree_dir)?,
                &bundle_name,
                manifest.id.as_str(),
            ],
            Some(self.state.base_dir.as_path()),
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
        let sdk_id = format!("{}//{}", manifest.sdk, manifest.runtime_version);
        run_command(
            "flatpak",
            &["run", "--command=bash", &sdk_id],
            Some(self.state.base_dir.as_path()),
        )
    }

    pub fn build_terminal(&self) -> Result<()> {
        let manifest = self.manifest.as_ref().context("No manifest available")?;
        let repo_dir = self.build_dirs.repo_dir();
        let sandbox = self.build_sandbox(None, manifest);

        let mut args = Self::sandbox_run_args(manifest, &repo_dir, &sandbox, true)?;
        args.push("bash".to_string());

        let args_str: Vec<&str> = args.iter().map(String::as_str).collect();
        run_command("flatpak", &args_str, Some(self.state.base_dir.as_path()))
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
