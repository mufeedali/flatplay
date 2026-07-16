use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use walkdir::WalkDir;

pub fn is_valid_dbus_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }

    if !name.contains('.') {
        return false;
    }

    name.split('.').all(|element| {
        let mut chars = element.chars();
        match chars.next() {
            Some(c) if !c.is_ascii_digit() => {
                chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            }
            _ => false,
        }
    })
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "kebab-case")]
pub struct BuildOptions {
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub config_opts: Vec<String>,
    pub prepend_path: Option<String>,
    pub append_path: Option<String>,
    pub prepend_ld_library_path: Option<String>,
    pub append_ld_library_path: Option<String>,
    pub prepend_pkg_config_path: Option<String>,
    pub append_pkg_config_path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
#[allow(clippy::large_enum_variant)]
pub enum Module {
    #[serde(rename_all = "kebab-case")]
    Object {
        name: String,
        #[serde(default)]
        buildsystem: Option<String>,
        #[serde(default)]
        builddir: Option<bool>,
        #[serde(default)]
        subdir: Option<String>,
        #[serde(default)]
        config_opts: Option<Vec<String>>,
        #[serde(default)]
        build_commands: Option<Vec<String>>,
        #[serde(default)]
        build_options: Option<BuildOptions>,
        #[serde(default)]
        post_install: Option<Vec<String>>,
        #[serde(default)]
        sources: Vec<serde_json::Value>,
        /// Nested modules (flatpak-builder module files often group children here).
        #[serde(default)]
        modules: Vec<Module>,
    },
    Reference(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct Manifest {
    #[serde(alias = "app-id")]
    pub id: String,
    pub sdk: String,
    pub runtime: String,
    pub runtime_version: String,
    pub command: String,
    #[serde(default)]
    pub x_run_args: Option<Vec<String>>,
    #[serde(default)]
    pub modules: Vec<Module>,
    #[serde(default)]
    pub finish_args: Vec<String>,
    #[serde(default)]
    pub build_options: BuildOptions,
    #[serde(default)]
    pub sdk_extensions: Vec<String>,
    #[serde(default)]
    pub cleanup: Vec<String>,
}

impl Manifest {
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let manifest: Self = match path.extension().and_then(|s| s.to_str()) {
            Some("json") => serde_json::from_str(&content)?,
            Some("yaml" | "yml") => serde_saphyr::from_str(&content)?,
            _ => return Err(anyhow::anyhow!("Unsupported manifest format")),
        };
        if !is_valid_dbus_name(&manifest.id) {
            return Err(anyhow::anyhow!("Invalid application ID: {}", manifest.id));
        }
        Ok(manifest)
    }

    pub fn finish_args_filtered(&self) -> Vec<String> {
        self.finish_args
            .iter()
            .filter(|arg| {
                let key = arg.split('=').next().unwrap_or("");
                key != "--metadata" && key != "--require-version"
            })
            .cloned()
            .collect()
    }

    pub fn merged_config_opts<'a>(
        &'a self,
        module_config_opts: Option<&'a [String]>,
    ) -> Vec<&'a str> {
        self.build_options
            .config_opts
            .iter()
            .chain(module_config_opts.into_iter().flatten())
            .map(std::string::String::as_str)
            .collect()
    }

    pub fn merged_env(
        &self,
        module_build_options: Option<&BuildOptions>,
    ) -> HashMap<String, String> {
        let mut merged = self.build_options.env.clone();
        if let Some(mbo) = module_build_options {
            merged.extend(mbo.env.clone());
        }
        merged
    }

    pub fn path_overrides(&self, module_build_options: Option<&BuildOptions>) -> Vec<String> {
        let mut overrides = Vec::new();

        let mpp = module_build_options.and_then(|b| b.prepend_path.as_deref());
        let mpa = module_build_options.and_then(|b| b.append_path.as_deref());
        let mlp = module_build_options.and_then(|b| b.prepend_ld_library_path.as_deref());
        let mla = module_build_options.and_then(|b| b.append_ld_library_path.as_deref());
        let m_prepend_pkg = module_build_options.and_then(|b| b.prepend_pkg_config_path.as_deref());
        let m_append_pkg = module_build_options.and_then(|b| b.append_pkg_config_path.as_deref());

        // Only inject PATH/LD/PKG_CONFIG when the manifest or module asks for
        // prepend/append. Always replacing hides SDK extension bins.
        if let Some(arg) = Self::build_path_override(
            "PATH",
            &["/app/bin", "/usr/bin"],
            self.build_options.prepend_path.as_deref(),
            mpp,
            self.build_options.append_path.as_deref(),
            mpa,
        ) {
            overrides.push(arg);
        }

        if let Some(arg) = Self::build_path_override(
            "LD_LIBRARY_PATH",
            &["/app/lib"],
            self.build_options.prepend_ld_library_path.as_deref(),
            mlp,
            self.build_options.append_ld_library_path.as_deref(),
            mla,
        ) {
            overrides.push(arg);
        }

        if let Some(arg) = Self::build_path_override(
            "PKG_CONFIG_PATH",
            &[
                "/app/lib/pkgconfig",
                "/app/share/pkgconfig",
                "/usr/lib/pkgconfig",
                "/usr/share/pkgconfig",
            ],
            self.build_options.prepend_pkg_config_path.as_deref(),
            m_prepend_pkg,
            self.build_options.append_pkg_config_path.as_deref(),
            m_append_pkg,
        ) {
            overrides.push(arg);
        }

        overrides
    }

    /// Last buildable module after expanding nested/referenced modules.
    pub fn application_module(&self, manifest_path: &Path) -> Result<Module> {
        let parent = manifest_path
            .parent()
            .context("Manifest path has no parent directory")?;
        let expanded = Self::expand_modules(&self.modules, parent)?;
        expanded
            .into_iter()
            .rev()
            .find(|m| matches!(m, Module::Object { .. }))
            .context("Manifest has no buildable application module")
    }

    pub fn last_module_name(&self, manifest_path: &Path) -> Result<String> {
        match self.application_module(manifest_path)? {
            Module::Object { name, .. } => Ok(name),
            Module::Reference(name) => Ok(name),
        }
    }

    pub fn load_module_file(path: &Path) -> Result<Vec<Module>> {
        let content = fs::read_to_string(path)?;
        let modules: Vec<Module> = match path.extension().and_then(|s| s.to_str()) {
            Some("json") => serde_json::from_str(&content)
                .or_else(|_| serde_json::from_str::<Module>(&content).map(|m| vec![m]))
                .map_err(|error| {
                    anyhow::anyhow!("Failed to parse module file {}: {error}", path.display())
                })?,
            Some("yaml" | "yml") => serde_saphyr::from_str(&content)
                .or_else(|_| serde_saphyr::from_str::<Module>(&content).map(|m| vec![m]))
                .map_err(|error| {
                    anyhow::anyhow!("Failed to parse module file {}: {error}", path.display())
                })?,
            _ => return Err(anyhow::anyhow!("Unsupported module file format")),
        };
        Ok(modules)
    }

    /// Flatten module references and nested `modules` arrays (as flatpak-builder does).
    pub fn expand_modules(modules: &[Module], manifest_dir: &Path) -> Result<Vec<Module>> {
        let mut out = Vec::new();
        for module in modules {
            match module {
                Module::Reference(ref_name) => {
                    let ref_path = manifest_dir.join(ref_name);
                    let loaded = Self::load_module_file(&ref_path)?;
                    let nested_dir = ref_path
                        .parent()
                        .map_or_else(|| manifest_dir.to_path_buf(), Path::to_path_buf);
                    out.extend(Self::expand_modules(&loaded, &nested_dir)?);
                }
                Module::Object {
                    name,
                    buildsystem,
                    build_commands,
                    sources,
                    modules: nested,
                    ..
                } if !nested.is_empty() => {
                    // Grouping module: expand children first (usual pattern for
                    // pypi-dependencies.json), then the parent only if it has its own work.
                    out.extend(Self::expand_modules(nested, manifest_dir)?);
                    let parent_has_work = !sources.is_empty()
                        || build_commands
                            .as_ref()
                            .is_some_and(|commands| !commands.is_empty())
                        || buildsystem.as_ref().is_some_and(|bs| bs != "simple");
                    if parent_has_work {
                        let mut parent = module.clone();
                        if let Module::Object {
                            modules: parent_nested,
                            ..
                        } = &mut parent
                        {
                            parent_nested.clear();
                        }
                        out.push(parent);
                    } else {
                        crate::utils::verbose(format!(
                            "Expanded grouping module '{name}' to its nested modules only"
                        ));
                    }
                }
                other => out.push(other.clone()),
            }
        }
        Ok(out)
    }

    fn build_path_override(
        var: &str,
        defaults: &[&str],
        manifest_prepend: Option<&str>,
        module_prepend: Option<&str>,
        manifest_append: Option<&str>,
        module_append: Option<&str>,
    ) -> Option<String> {
        // Do not inject host PATH/LD_LIBRARY_PATH/PKG_CONFIG_PATH into the sandbox;
        // host libraries cause subtle link failures inside the SDK.
        // Only emit when prepend/append is set so we do not clobber the SDK env.
        let has_override = manifest_prepend.is_some()
            || module_prepend.is_some()
            || manifest_append.is_some()
            || module_append.is_some();
        if !has_override {
            return None;
        }
        let parts: Vec<&str> = manifest_prepend
            .into_iter()
            .chain(module_prepend)
            .chain(defaults.iter().copied())
            .chain(manifest_append)
            .chain(module_append)
            .collect();
        Some(format!("--env={var}={}", parts.join(":")))
    }
}

/// Best-effort directories to skip while scanning for manifests (not exhaustive).
const MANIFEST_SCAN_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "_build",
    "build",
    "dist",
    ".flatplay",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".git",
    ".cargo",
    ".npm",
    ".cache",
];

/// Cheap prefilter: file text looks like a Flatpak manifest before full parse.
fn looks_like_manifest(content: &str) -> bool {
    let head = content.get(..4096).unwrap_or(content);
    head.contains("app-id")
        || head.contains("\"app_id\"")
        || head.contains("app_id")
        || (head.contains("runtime") && head.contains("sdk") && head.contains("modules"))
}

/// Recursively finds manifest files in the given path, optionally excluding a prefix subtree.
/// Returns a sorted Vec of manifest file paths, prioritizing ".Devel." manifests and shallower paths.
pub fn find_manifests_in_path(path: &Path, exclude_prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut manifests = vec![];

    let path = match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) => {
            crate::utils::verbose(format!(
                "Failed to canonicalize path {}: {error}",
                path.display()
            ));
            path.to_path_buf()
        }
    };
    let exclude_prefix = exclude_prefix.map(|prefix| match prefix.canonicalize() {
        Ok(canonical) => canonical,
        Err(error) => {
            crate::utils::verbose(format!(
                "Failed to canonicalize exclude prefix {}: {error}",
                prefix.display()
            ));
            prefix.to_path_buf()
        }
    });

    // Prefer conventional shallow locations first (still validated as manifests).
    for rel in ["", "build-aux", "flatpak", "data"] {
        let dir = if rel.is_empty() {
            path.clone()
        } else {
            path.join(rel)
        };
        if !dir.is_dir() {
            continue;
        }
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if !p.is_file() {
                    continue;
                }
                if !matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("json" | "yaml" | "yml")
                ) {
                    continue;
                }
                if try_push_manifest(&mut manifests, &p) {
                    // keep collecting
                }
            }
        }
    }

    let walker = WalkDir::new(&path)
        .max_depth(6)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            // Skip VCS/tooling dirs; allow other hidden dirs (e.g. project-specific).
            if entry.file_type().is_dir() {
                if name == ".git" || name == ".flatplay" || name == ".tox" || name == ".cargo" {
                    return false;
                }
                if MANIFEST_SCAN_SKIP_DIRS.iter().any(|d| *d == name) {
                    return false;
                }
            }
            if let Some(prefix) = &exclude_prefix
                && entry.path().starts_with(prefix)
            {
                return false;
            }
            true
        });

    for entry_result in walker {
        let entry = match entry_result {
            Ok(entry) => entry,
            Err(error) => {
                crate::utils::verbose(format!("Error scanning directory entry: {error}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if !matches!(
            entry.path().extension().and_then(|s| s.to_str()),
            Some("json" | "yaml" | "yml")
        ) {
            continue;
        }
        try_push_manifest(&mut manifests, entry.path());
    }

    manifests.sort_by(|a, b| {
        let a_is_devel = a.to_str().is_some_and(|s| s.contains(".Devel."));
        let b_is_devel = b.to_str().is_some_and(|s| s.contains(".Devel."));
        b_is_devel
            .cmp(&a_is_devel)
            .then_with(|| a.components().count().cmp(&b.components().count()))
    });
    manifests.dedup();

    manifests
}

fn try_push_manifest(manifests: &mut Vec<PathBuf>, path: &Path) -> bool {
    if manifests.iter().any(|p| p == path) {
        return false;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    if !looks_like_manifest(&content) {
        return false;
    }
    if Manifest::from_file(path).is_ok() {
        manifests.push(path.to_path_buf());
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_is_valid_dbus_name() {
        assert!(is_valid_dbus_name("org.example.App"));
        assert!(is_valid_dbus_name("com.github.user.Application"));
        assert!(is_valid_dbus_name("io.github.user_name.app-name"));

        assert!(!is_valid_dbus_name(""));
        assert!(!is_valid_dbus_name("single"));
        assert!(!is_valid_dbus_name("org.123invalid"));
        assert!(!is_valid_dbus_name("org..double"));
        assert!(!is_valid_dbus_name(".org.example"));
        assert!(!is_valid_dbus_name("org.example."));

        let long_name = format!("org.{}.App", "a".repeat(250));
        assert!(!is_valid_dbus_name(&long_name));
    }

    #[test]
    fn test_manifest_parsing_json() {
        let manifest_content = r#"{
            "app-id": "org.example.TestApp",
            "sdk": "org.gnome.Sdk",
            "runtime": "org.gnome.Platform",
            "runtime-version": "47",
            "command": "test-app",
            "modules": [
                {
                    "name": "test-module",
                    "buildsystem": "meson",
                    "sources": []
                }
            ],
            "finish-args": [
                "--share=network"
            ]
        }"#;

        let mut temp_file = NamedTempFile::with_suffix(".json").unwrap();
        temp_file.write_all(manifest_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let manifest = Manifest::from_file(temp_file.path()).unwrap();
        assert_eq!(manifest.id, "org.example.TestApp");
        assert_eq!(manifest.sdk, "org.gnome.Sdk");
        assert_eq!(manifest.runtime, "org.gnome.Platform");
        assert_eq!(manifest.runtime_version, "47");
        assert_eq!(manifest.command, "test-app");
        assert_eq!(manifest.finish_args.len(), 1);
        assert_eq!(manifest.modules.len(), 1);
    }

    #[test]
    fn test_manifest_parsing_yaml() {
        let manifest_content = r#"
app-id: org.example.TestApp
sdk: org.gnome.Sdk
runtime: org.gnome.Platform
runtime-version: "47"
command: test-app
modules:
  - name: test-module
    buildsystem: meson
    sources: []
finish-args:
  - --share=network
"#;

        let mut temp_file = NamedTempFile::with_suffix(".yaml").unwrap();
        temp_file.write_all(manifest_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let manifest = Manifest::from_file(temp_file.path()).unwrap();
        assert_eq!(manifest.id, "org.example.TestApp");
        assert_eq!(manifest.sdk, "org.gnome.Sdk");
        assert_eq!(manifest.command, "test-app");
    }

    #[test]
    fn test_manifest_invalid_app_id() {
        let manifest_content = r#"{
            "app-id": "invalid",
            "sdk": "org.gnome.Sdk",
            "runtime": "org.gnome.Platform",
            "runtime-version": "47",
            "command": "test-app",
            "modules": []
        }"#;

        let mut temp_file = NamedTempFile::with_suffix(".json").unwrap();
        temp_file.write_all(manifest_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let result = Manifest::from_file(temp_file.path());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid application ID")
        );
    }
}
