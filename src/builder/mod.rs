//! In-process module builder — **no `flatpak-builder` CLI**.
//!
//! Used for both dependency modules and the application module so semantics stay unified.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::manifest::{Manifest, Module};
use crate::sandbox::{FlatpakRunner, RunSpec, SandboxRunner};
use crate::sources::{self, DownloadCache, Source};
use crate::utils::{copy_dir_all, path_to_str, status, status_warn, verbose};

/// Options for building a single module.
#[derive(Debug, Clone, Copy)]
pub struct BuildModuleOpts {
    /// When true, skip configure/setup and only rebuild + install.
    pub rebuild: bool,
}

/// Download/materialize all non-app modules' sources into the cache / module dirs.
pub fn update_dependencies(
    manifest: &Manifest,
    manifest_path: &Path,
    build_dir: &Path,
    stop_at: &str,
) -> Result<()> {
    status("Updating dependencies (in-process)...");
    let manifest_dir = manifest_path
        .parent()
        .context("Manifest has no parent directory")?;
    let cache = DownloadCache::new(build_dir);

    for module in modules_before_stop(manifest, manifest_path, stop_at)? {
        let Module::Object { name, sources, .. } = &module else {
            status_warn(format!(
                "Skipping unexpected module form during update: {module:?}"
            ));
            continue;
        };
        let typed = Source::from_values(sources)?;
        // Directory-only modules (pointing at the project tree) need no download.
        if typed.iter().all(|s| matches!(s, Source::Dir { .. })) {
            verbose(format!(
                "Module {name}: dir sources only, nothing to download"
            ));
            continue;
        }
        let source_dir = build_dir.join(name);
        sources::materialize_sources(&typed, &source_dir, manifest_dir, &cache, name)?;
    }
    Ok(())
}

/// Build all modules before `stop_at` into `repo_dir` via flatpak build + SDK.
pub fn build_dependencies(
    manifest: &Manifest,
    manifest_path: &Path,
    repo_dir: &Path,
    build_dir: &Path,
    stop_at: &str,
    workspace: &Path,
    runner: &FlatpakRunner,
) -> Result<()> {
    status("Building dependencies (in-process)...");
    let paths = ModulePaths {
        repo_dir,
        build_dir,
        workspace,
        manifest_path,
    };
    for module in modules_before_stop(manifest, manifest_path, stop_at)? {
        build_module(
            manifest,
            &module,
            &paths,
            runner,
            BuildModuleOpts { rebuild: false },
        )?;
    }
    Ok(())
}

/// Paths shared by a module build.
pub struct ModulePaths<'a> {
    pub repo_dir: &'a Path,
    pub build_dir: &'a Path,
    pub workspace: &'a Path,
    pub manifest_path: &'a Path,
}

/// Build a single module (dependency or application) with unified semantics.
pub fn build_module(
    manifest: &Manifest,
    module: &Module,
    paths: &ModulePaths<'_>,
    runner: &FlatpakRunner,
    opts: BuildModuleOpts,
) -> Result<()> {
    let Module::Object {
        name,
        buildsystem,
        builddir,
        subdir,
        config_opts,
        build_commands,
        build_options,
        post_install,
        sources,
        ..
    } = module
    else {
        return Err(anyhow::anyhow!("Module is not a defined object module"));
    };

    let manifest_dir = paths
        .manifest_path
        .parent()
        .context("Manifest has no parent directory")?;
    let cache = DownloadCache::new(paths.build_dir);
    let num_cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

    status(format!("Building module {name}..."));
    let typed = Source::from_values(sources)?;
    let source_dir =
        resolve_and_materialize_sources(name, &typed, manifest_dir, paths.build_dir, &cache)?;

    let source_dir = if let Some(subdir) = subdir {
        source_dir.join(subdir)
    } else {
        source_dir
    };

    let merged_config: Vec<String> = manifest
        .merged_config_opts(config_opts.as_deref())
        .into_iter()
        .map(str::to_string)
        .collect();

    let env_pairs: Vec<(String, String)> = manifest
        .merged_env(build_options.as_ref())
        .into_iter()
        .collect();

    let path_overrides = manifest.path_overrides(build_options.as_ref());

    // Canonicalize when possible so /run/build/$name is a stable host path.
    let module_root = source_dir
        .canonicalize()
        .unwrap_or_else(|_| source_dir.clone());

    let ctx = ModuleBuildCtx {
        runner,
        repo_dir: paths.repo_dir,
        workspace: paths.workspace,
        module_name: name,
        module_root: &module_root,
        env_pairs: &env_pairs,
        path_overrides: &path_overrides,
    };

    match buildsystem.as_deref() {
        Some("meson") => build_meson(
            &ctx,
            &source_dir,
            paths.build_dir,
            name,
            &merged_config,
            opts.rebuild,
        )?,
        Some("cmake" | "cmake-ninja") => build_cmake(
            &ctx,
            &source_dir,
            paths.build_dir,
            name,
            &merged_config,
            opts.rebuild,
        )?,
        Some("simple") => build_simple(
            &ctx,
            &source_dir,
            manifest,
            name,
            build_commands.as_ref(),
            num_cpus,
        )?,
        Some("qmake") => {
            return Err(anyhow::anyhow!("qmake build system is not supported"));
        }
        _ => build_autotools(
            &ctx,
            &source_dir,
            paths.build_dir,
            AutotoolsOpts {
                name,
                use_builddir: builddir.as_ref().copied().unwrap_or(false),
                config: &merged_config,
                num_cpus,
                rebuild: opts.rebuild,
            },
        )?,
    }

    if let Some(post_install) = post_install {
        for command in post_install {
            let processed = substitute_vars(command, &manifest.id, name, num_cpus);
            let argv = command_to_argv(&processed)?;
            ctx.run_argv(&argv, Some(&source_dir))?;
        }
    }

    Ok(())
}

fn resolve_and_materialize_sources(
    name: &str,
    typed: &[Source],
    manifest_dir: &Path,
    build_dir: &Path,
    cache: &DownloadCache,
) -> Result<PathBuf> {
    if let Some(dir_path) = sources::resolve_dir_source(typed, manifest_dir) {
        // Live project/dir tree — materialize non-dir sources into it.
        let non_dir: Vec<_> = typed
            .iter()
            .filter(|s| !matches!(s, Source::Dir { .. }))
            .cloned()
            .collect();
        if !non_dir.is_empty() {
            let staging = build_dir.join(format!("{name}-staging"));
            sources::materialize_sources(&non_dir, &staging, manifest_dir, cache, name)?;
            // Recursive copy so archive layouts with subdirs are preserved.
            for entry in std::fs::read_dir(&staging)? {
                let entry = entry?;
                let dest = dir_path.join(entry.file_name());
                let ft = entry.file_type()?;
                if ft.is_dir() {
                    copy_dir_all(&entry.path(), &dest)?;
                } else if ft.is_file() {
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(entry.path(), &dest)?;
                }
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        return Ok(dir_path);
    }

    let source_dir = build_dir.join(name);
    if !source_dir.exists() {
        sources::materialize_sources(typed, &source_dir, manifest_dir, cache, name)?;
    }
    Ok(source_dir)
}

fn modules_before_stop(
    manifest: &Manifest,
    manifest_path: &Path,
    stop_at: &str,
) -> Result<Vec<Module>> {
    let manifest_dir = manifest_path
        .parent()
        .context("Manifest path has no parent directory")?;
    let expanded = Manifest::expand_modules(&manifest.modules, manifest_dir)?;
    let mut out = Vec::new();
    for module in expanded {
        let name = match &module {
            Module::Object { name, .. } => name.as_str(),
            Module::Reference(path) => path.as_str(),
        };
        if name == stop_at {
            return Ok(out);
        }
        out.push(module);
    }
    anyhow::bail!("stop-at module '{stop_at}' not found in manifest (after expanding references)")
}

struct ModuleBuildCtx<'a> {
    runner: &'a FlatpakRunner,
    repo_dir: &'a Path,
    workspace: &'a Path,
    module_name: &'a str,
    /// Module source/work tree (flatpak-builder's `/run/build/$name` root).
    module_root: &'a Path,
    env_pairs: &'a [(String, String)],
    path_overrides: &'a [String],
}

impl ModuleBuildCtx<'_> {
    fn run_argv(&self, argv: &[String], cwd: Option<&Path>) -> Result<()> {
        let mut filesystem_binds = vec![format!("--filesystem={}", path_to_str(self.workspace)?)];
        let module_root_s = path_to_str(self.module_root)?;
        filesystem_binds.push(format!("--filesystem={module_root_s}"));
        if let Some(cwd) = cwd {
            let cwd_s = path_to_str(cwd)?;
            if cwd_s != module_root_s {
                filesystem_binds.push(format!("--filesystem={cwd_s}"));
            }
        }

        // flatpak-builder: module sources live at /run/build/$name (stable for the module).
        filesystem_binds.push(format!(
            "--bind-mount=/run/build/{}={}",
            self.module_name, module_root_s
        ));

        let mut env = self.env_pairs.to_vec();
        for override_arg in self.path_overrides {
            if let Some(rest) = override_arg.strip_prefix("--env=")
                && let Some((key, value)) = rest.split_once('=')
            {
                env.push((key.to_string(), value.to_string()));
            }
        }

        self.runner.run(&RunSpec {
            repo_dir: self.repo_dir.to_path_buf(),
            argv: argv.to_vec(),
            env,
            filesystem_binds,
            extra_args: vec![],
            share_network: true,
            cwd: cwd.map(Path::to_path_buf),
        })
    }
}

pub fn substitute_vars(
    command: &str,
    flatpak_id: &str,
    module_name: &str,
    num_cpus: usize,
) -> String {
    command
        .replace("${FLATPAK_ID}", flatpak_id)
        .replace("${FLATPAK_ARCH}", &crate::sources::flatpak_arch())
        .replace("${FLATPAK_DEST}", "/app")
        .replace("${FLATPAK_BUILDER_N_JOBS}", &num_cpus.to_string())
        .replace(
            "${FLATPAK_BUILDER_BUILDDIR}",
            &format!("/run/build/{module_name}"),
        )
}

fn meson_configured(module_build: &Path) -> bool {
    module_build.join("build.ninja").is_file() || module_build.join("meson-info").is_dir()
}

fn cmake_configured(module_build: &Path) -> bool {
    module_build.join("build.ninja").is_file() || module_build.join("CMakeCache.txt").is_file()
}

fn build_meson(
    ctx: &ModuleBuildCtx<'_>,
    source_dir: &Path,
    build_dir: &Path,
    name: &str,
    config: &[String],
    rebuild: bool,
) -> Result<()> {
    let module_build = build_dir.join(format!("_build-{name}"));
    std::fs::create_dir_all(&module_build)?;
    let source_canon = source_dir.canonicalize()?;
    let source_s = path_to_str(&source_canon)?.to_string();
    let build_s = path_to_str(&module_build)?.to_string();

    if !rebuild && !meson_configured(&module_build) {
        let mut setup = vec!["meson".into(), "setup".into(), "--prefix=/app".into()];
        setup.extend(config.iter().cloned());
        setup.push(source_s);
        setup.push(build_s.clone());
        ctx.run_argv(&setup, Some(&module_build))?;
    } else if rebuild && !meson_configured(&module_build) {
        return Err(anyhow::anyhow!(
            "Cannot rebuild module {name}: no existing meson build dir at {}",
            module_build.display()
        ));
    }

    ctx.run_argv(
        &["ninja".into(), "-C".into(), build_s.clone()],
        Some(&module_build),
    )?;
    ctx.run_argv(
        &["meson".into(), "install".into(), "-C".into(), build_s],
        Some(&module_build),
    )
}

fn build_cmake(
    ctx: &ModuleBuildCtx<'_>,
    source_dir: &Path,
    build_dir: &Path,
    name: &str,
    config: &[String],
    rebuild: bool,
) -> Result<()> {
    let module_build = build_dir.join(format!("_build-{name}"));
    std::fs::create_dir_all(&module_build)?;
    let source_canon = source_dir.canonicalize()?;
    let source_s = path_to_str(&source_canon)?.to_string();
    let build_s = path_to_str(&module_build)?.to_string();

    if !rebuild && !cmake_configured(&module_build) {
        let mut configure = vec![
            "cmake".into(),
            "-G".into(),
            "Ninja".into(),
            format!("-B{build_s}"),
            "-DCMAKE_BUILD_TYPE=RelWithDebInfo".into(),
            "-DCMAKE_INSTALL_PREFIX=/app".into(),
        ];
        configure.extend(config.iter().cloned());
        configure.push(source_s);
        ctx.run_argv(&configure, Some(&module_build))?;
    } else if rebuild && !cmake_configured(&module_build) {
        return Err(anyhow::anyhow!(
            "Cannot rebuild module {name}: no existing cmake build dir at {}",
            module_build.display()
        ));
    }

    ctx.run_argv(
        &["ninja".into(), "-C".into(), build_s.clone()],
        Some(&module_build),
    )?;
    ctx.run_argv(
        &["ninja".into(), "-C".into(), build_s, "install".into()],
        Some(&module_build),
    )
}

fn build_simple(
    ctx: &ModuleBuildCtx<'_>,
    source_dir: &Path,
    manifest: &Manifest,
    name: &str,
    build_commands: Option<&Vec<String>>,
    num_cpus: usize,
) -> Result<()> {
    let Some(commands) = build_commands else {
        return Ok(());
    };
    let source_canon = source_dir
        .canonicalize()
        .unwrap_or_else(|_| source_dir.to_path_buf());
    for command in commands {
        let processed = substitute_vars(command, &manifest.id, name, num_cpus)
            // flatpak-builder uses ${PWD} as the module build directory.
            .replace("${PWD}", path_to_str(&source_canon)?)
            .replace("$PWD", path_to_str(&source_canon)?);
        let argv = command_to_argv(&processed)?;
        ctx.run_argv(&argv, Some(&source_canon))?;
    }
    Ok(())
}

/// flatpak-builder always runs `build-commands` / `post-install` through a shell.
fn command_to_argv(command: &str) -> Result<Vec<String>> {
    Ok(vec!["/bin/sh".into(), "-c".into(), command.to_string()])
}

struct AutotoolsOpts<'a> {
    name: &'a str,
    use_builddir: bool,
    config: &'a [String],
    num_cpus: usize,
    rebuild: bool,
}

fn build_autotools(
    ctx: &ModuleBuildCtx<'_>,
    source_dir: &Path,
    build_dir: &Path,
    opts: AutotoolsOpts<'_>,
) -> Result<()> {
    let AutotoolsOpts {
        name,
        use_builddir,
        config,
        num_cpus,
        rebuild,
    } = opts;
    let source_canon = source_dir.canonicalize()?;
    let source_s = path_to_str(&source_canon)?;

    // Generate configure if needed.
    if !rebuild && !source_canon.join("configure").exists() {
        for candidate in ["autogen.sh", "bootstrap", "bootstrap.sh"] {
            let script = source_canon.join(candidate);
            if script.exists() {
                ctx.run_argv(
                    &["/bin/sh".into(), path_to_str(&script)?.into()],
                    Some(&source_canon),
                )?;
                break;
            }
        }
    }

    let jobs = format!("-j{num_cpus}");
    if use_builddir {
        let module_build = build_dir.join(format!("_build-{name}"));
        std::fs::create_dir_all(&module_build)?;
        if !rebuild {
            let mut configure = vec![format!("{source_s}/configure"), "--prefix=/app".into()];
            configure.extend(config.iter().cloned());
            ctx.run_argv(&configure, Some(&module_build))?;
        }
        ctx.run_argv(
            &["make".into(), "V=0".into(), jobs, "install".into()],
            Some(&module_build),
        )?;
    } else {
        if !rebuild {
            let mut configure = vec![format!("{source_s}/configure"), "--prefix=/app".into()];
            configure.extend(config.iter().cloned());
            ctx.run_argv(&configure, Some(&source_canon))?;
        }
        ctx.run_argv(
            &["make".into(), "V=0".into(), jobs, "install".into()],
            Some(&source_canon),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BuildOptions, Manifest, Module};
    use std::path::PathBuf;

    #[test]
    fn substitute_vars_uses_flatpak_arch() {
        let out = substitute_vars("arch=${FLATPAK_ARCH}", "org.example.App", "mod", 4);
        assert_eq!(out, format!("arch={}", crate::sources::flatpak_arch()));
        let out = substitute_vars("${FLATPAK_BUILDER_BUILDDIR}", "org.example.App", "mymod", 1);
        assert_eq!(out, "/run/build/mymod");
    }

    #[test]
    fn modules_before_stop_excludes_app() {
        let manifest = Manifest {
            id: "org.example.App".into(),
            sdk: "org.gnome.Sdk".into(),
            runtime: "org.gnome.Platform".into(),
            runtime_version: "47".into(),
            command: "app".into(),
            x_run_args: None,
            modules: vec![
                Module::Object {
                    name: "dep".into(),
                    buildsystem: Some("meson".into()),
                    builddir: None,
                    subdir: None,
                    config_opts: None,
                    build_commands: None,
                    build_options: None,
                    post_install: None,
                    sources: vec![],
                    modules: vec![],
                },
                Module::Object {
                    name: "app".into(),
                    buildsystem: Some("meson".into()),
                    builddir: None,
                    subdir: None,
                    config_opts: None,
                    build_commands: None,
                    build_options: None,
                    post_install: None,
                    sources: vec![],
                    modules: vec![],
                },
            ],
            finish_args: vec![],
            build_options: BuildOptions::default(),
            sdk_extensions: vec![],
            cleanup: vec![],
        };
        let manifest_path = PathBuf::from("/tmp/manifest.json");
        let mods = modules_before_stop(&manifest, &manifest_path, "app").unwrap();
        assert_eq!(mods.len(), 1);
    }
}
