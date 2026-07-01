//! In-process dependency builder — **no `flatpak-builder` CLI**.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::manifest::{Manifest, Module};
use crate::sandbox::{BwrapRunner, RunSpec, SandboxRunner};
use crate::sources::{self, DownloadCache, Source};
use crate::utils::{path_to_str, status, status_warn, verbose};

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

    for module in modules_before_stop(manifest, stop_at)? {
        let Module::Object { name, sources, .. } = module else {
            status_warn(format!("Skipping module reference during update: {module:?}"));
            continue;
        };
        let typed = Source::from_values(sources)?;
        // Directory-only modules (pointing at the project tree) need no download.
        if typed.iter().all(|s| matches!(s, Source::Dir { .. })) {
            verbose(format!("Module {name}: dir sources only, nothing to download"));
            continue;
        }
        let source_dir = build_dir.join(name);
        sources::materialize_sources(&typed, &source_dir, manifest_dir, &cache, name)?;
    }
    Ok(())
}

/// Build all modules before `stop_at` into `repo_dir` via bwrap + SDK.
pub fn build_dependencies(
    manifest: &Manifest,
    manifest_path: &Path,
    repo_dir: &Path,
    build_dir: &Path,
    stop_at: &str,
    workspace: &Path,
    runner: &BwrapRunner,
) -> Result<()> {
    status("Building dependencies (in-process)...");
    let manifest_dir = manifest_path
        .parent()
        .context("Manifest has no parent directory")?;
    let cache = DownloadCache::new(build_dir);
    let num_cpus = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);

    for module in modules_before_stop(manifest, stop_at)? {
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
            continue;
        };

        status(format!("Building module {name}..."));
        let typed = Source::from_values(sources)?;
        let source_dir = if let Some(dir_path) = sources::resolve_dir_source(&typed, manifest_dir)
        {
            // Prefer the live project/dir tree (no recursive copy into .flatplay).
            // Non-dir sources (files/archives) are materialized directly into that tree.
            let non_dir: Vec<_> = typed
                .iter()
                .filter(|s| !matches!(s, Source::Dir { .. }))
                .cloned()
                .collect();
            if !non_dir.is_empty() {
                // Materialize into a temp module folder, then copy files to dir_path.
                let staging = build_dir.join(format!("{name}-staging"));
                sources::materialize_sources(&non_dir, &staging, manifest_dir, &cache, name)?;
                for entry in std::fs::read_dir(&staging)? {
                    let entry = entry?;
                    let dest = dir_path.join(entry.file_name());
                    if entry.file_type()?.is_file() {
                        std::fs::copy(entry.path(), &dest)?;
                    }
                }
            }
            dir_path
        } else {
            let source_dir = build_dir.join(name);
            if !source_dir.exists() {
                sources::materialize_sources(&typed, &source_dir, manifest_dir, &cache, name)?;
            }
            source_dir
        };

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

        match buildsystem.as_deref() {
            Some("meson") => build_meson(
                runner,
                repo_dir,
                &source_dir,
                build_dir,
                name,
                &merged_config,
                &env_pairs,
                &path_overrides,
                workspace,
            )?,
            Some("cmake" | "cmake-ninja") => build_cmake(
                runner,
                repo_dir,
                &source_dir,
                build_dir,
                name,
                &merged_config,
                &env_pairs,
                &path_overrides,
                workspace,
            )?,
            Some("simple") => build_simple(
                runner,
                repo_dir,
                &source_dir,
                manifest,
                name,
                build_commands.as_ref(),
                &env_pairs,
                &path_overrides,
                num_cpus,
                workspace,
            )?,
            _ => build_autotools(
                runner,
                repo_dir,
                &source_dir,
                build_dir,
                name,
                builddir.unwrap_or(false),
                &merged_config,
                &env_pairs,
                &path_overrides,
                num_cpus,
                workspace,
            )?,
        }

        if let Some(post_install) = post_install {
            for command in post_install {
                let processed = substitute_vars(command, &manifest.id, name, num_cpus);
                let argv = command_to_argv(&processed)?;
                run_argv(
                    runner,
                    repo_dir,
                    &argv,
                    &env_pairs,
                    &path_overrides,
                    Some(&source_dir),
                    workspace,
                )?;
            }
        }
    }

    Ok(())
}

fn modules_before_stop<'a>(manifest: &'a Manifest, stop_at: &str) -> Result<Vec<&'a Module>> {
    let mut out = Vec::new();
    for module in &manifest.modules {
        let name = match module {
            Module::Object { name, .. } => name.as_str(),
            Module::Reference(path) => path.as_str(),
        };
        if name == stop_at {
            return Ok(out);
        }
        out.push(module);
    }
    anyhow::bail!("stop-at module '{stop_at}' not found in manifest")
}

fn run_argv(
    runner: &BwrapRunner,
    repo_dir: &Path,
    argv: &[String],
    env_pairs: &[(String, String)],
    path_overrides: &[String],
    cwd: Option<&Path>,
    workspace: &Path,
) -> Result<()> {
    let mut filesystem_binds = vec![format!("--filesystem={}", path_to_str(workspace)?)];
    if let Some(cwd) = cwd {
        filesystem_binds.push(format!("--filesystem={}", path_to_str(cwd)?));
    }

    let mut env = env_pairs.to_vec();
    for override_arg in path_overrides {
        if let Some(rest) = override_arg.strip_prefix("--env=")
            && let Some((key, value)) = rest.split_once('=')
        {
            env.push((key.to_string(), value.to_string()));
        }
    }

    runner.run(&RunSpec {
        repo_dir: repo_dir.to_path_buf(),
        argv: argv.to_vec(),
        env,
        filesystem_binds,
        extra_args: vec![],
        share_network: true,
        cwd: cwd.map(Path::to_path_buf),
    })
}

fn substitute_vars(command: &str, flatpak_id: &str, module_name: &str, num_cpus: usize) -> String {
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

fn build_meson(
    runner: &BwrapRunner,
    repo_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
    name: &str,
    config: &[String],
    env_pairs: &[(String, String)],
    path_overrides: &[String],
    workspace: &Path,
) -> Result<()> {
    let module_build = build_dir.join(format!("_build-{name}"));
    std::fs::create_dir_all(&module_build)?;
    let source_canon = source_dir.canonicalize()?;
    let source_s = path_to_str(&source_canon)?.to_string();
    let build_s = path_to_str(&module_build)?.to_string();

    let mut setup = vec![
        "meson".into(),
        "setup".into(),
        "--prefix=/app".into(),
    ];
    setup.extend(config.iter().cloned());
    setup.push(source_s.clone());
    setup.push(build_s.clone());
    run_argv(
        runner,
        repo_dir,
        &setup,
        env_pairs,
        path_overrides,
        None,
        workspace,
    )?;
    run_argv(
        runner,
        repo_dir,
        &["ninja".into(), "-C".into(), build_s.clone()],
        env_pairs,
        path_overrides,
        None,
        workspace,
    )?;
    run_argv(
        runner,
        repo_dir,
        &[
            "meson".into(),
            "install".into(),
            "-C".into(),
            build_s,
        ],
        env_pairs,
        path_overrides,
        None,
        workspace,
    )
}

fn build_cmake(
    runner: &BwrapRunner,
    repo_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
    name: &str,
    config: &[String],
    env_pairs: &[(String, String)],
    path_overrides: &[String],
    workspace: &Path,
) -> Result<()> {
    let module_build = build_dir.join(format!("_build-{name}"));
    std::fs::create_dir_all(&module_build)?;
    let source_canon = source_dir.canonicalize()?;
    let source_s = path_to_str(&source_canon)?.to_string();
    let build_s = path_to_str(&module_build)?.to_string();
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
    run_argv(
        runner,
        repo_dir,
        &configure,
        env_pairs,
        path_overrides,
        None,
        workspace,
    )?;
    run_argv(
        runner,
        repo_dir,
        &["ninja".into(), "-C".into(), build_s.clone()],
        env_pairs,
        path_overrides,
        None,
        workspace,
    )?;
    run_argv(
        runner,
        repo_dir,
        &[
            "ninja".into(),
            "-C".into(),
            build_s,
            "install".into(),
        ],
        env_pairs,
        path_overrides,
        None,
        workspace,
    )
}

fn build_simple(
    runner: &BwrapRunner,
    repo_dir: &Path,
    source_dir: &Path,
    manifest: &Manifest,
    name: &str,
    build_commands: Option<&Vec<String>>,
    env_pairs: &[(String, String)],
    path_overrides: &[String],
    num_cpus: usize,
    workspace: &Path,
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
        run_argv(
            runner,
            repo_dir,
            &argv,
            env_pairs,
            path_overrides,
            Some(&source_canon),
            workspace,
        )?;
    }
    Ok(())
}

/// flatpak-builder runs build-commands through a shell; use `/bin/sh -c` when needed.
fn command_to_argv(command: &str) -> Result<Vec<String>> {
    let needs_shell = command.contains('$')
        || command.contains('`')
        || command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
        || command.contains('>')
        || command.contains('<')
        || command.contains('(')
        || command.contains(')');
    if needs_shell {
        Ok(vec!["/bin/sh".into(), "-c".into(), command.to_string()])
    } else {
        shell_words::split(command).with_context(|| format!("Bad build-command: {command}"))
    }
}

fn build_autotools(
    runner: &BwrapRunner,
    repo_dir: &Path,
    source_dir: &Path,
    build_dir: &Path,
    name: &str,
    use_builddir: bool,
    config: &[String],
    env_pairs: &[(String, String)],
    path_overrides: &[String],
    num_cpus: usize,
    workspace: &Path,
) -> Result<()> {
    let source_canon = source_dir.canonicalize()?;
    let source_s = path_to_str(&source_canon)?;

    // Generate configure if needed.
    if !source_canon.join("configure").exists() {
        for candidate in ["autogen.sh", "bootstrap", "bootstrap.sh"] {
            let script = source_canon.join(candidate);
            if script.exists() {
                run_argv(
                    runner,
                    repo_dir,
                    &["/bin/sh".into(), path_to_str(&script)?.into()],
                    env_pairs,
                    path_overrides,
                    Some(&source_canon),
                    workspace,
                )?;
                break;
            }
        }
    }

    let jobs = format!("-j{num_cpus}");
    if use_builddir {
        let module_build = build_dir.join(format!("_build-{name}"));
        std::fs::create_dir_all(&module_build)?;
        let build_s = path_to_str(&module_build)?;
        let mut configure = vec![
            format!("{source_s}/configure"),
            "--prefix=/app".into(),
        ];
        configure.extend(config.iter().cloned());
        run_argv(
            runner,
            repo_dir,
            &configure,
            env_pairs,
            path_overrides,
            Some(&module_build),
            workspace,
        )?;
        run_argv(
            runner,
            repo_dir,
            &[
                "make".into(),
                "V=0".into(),
                jobs.clone(),
                "install".into(),
            ],
            env_pairs,
            path_overrides,
            Some(&module_build),
            workspace,
        )?;
    } else {
        let mut configure = vec![
            format!("{source_s}/configure"),
            "--prefix=/app".into(),
        ];
        configure.extend(config.iter().cloned());
        run_argv(
            runner,
            repo_dir,
            &configure,
            env_pairs,
            path_overrides,
            Some(&source_canon),
            workspace,
        )?;
        run_argv(
            runner,
            repo_dir,
            &["make".into(), "V=0".into(), jobs, "install".into()],
            env_pairs,
            path_overrides,
            Some(&source_canon),
            workspace,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{BuildOptions, Manifest, Module};

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
                },
            ],
            finish_args: vec![],
            build_options: BuildOptions::default(),
            sdk_extensions: vec![],
            cleanup: vec![],
        };
        let mods = modules_before_stop(&manifest, "app").unwrap();
        assert_eq!(mods.len(), 1);
    }
}
