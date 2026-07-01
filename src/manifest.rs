use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

    pub fn application_module(&self, manifest_path: &Path) -> Result<Module> {
        match self.modules.last() {
            Some(module @ Module::Object { .. }) => Ok(module.clone()),
            Some(Module::Reference(ref_name)) => {
                let parent = manifest_path
                    .parent()
                    .context("Manifest path has no parent directory")?;
                let ref_path = parent.join(ref_name);
                let modules = Self::load_module_file(&ref_path)?;
                match modules.into_iter().last() {
                    Some(module @ Module::Object { .. }) => Ok(module),
                    Some(Module::Reference(name)) => Err(anyhow::anyhow!(
                        "Nested module references not supported: {ref_name} -> {name}"
                    )),
                    None => Err(anyhow::anyhow!(
                        "Referenced module file {ref_name} contains no modules"
                    )),
                }
            }
            None => Err(anyhow::anyhow!("Manifest has no modules")),
        }
    }

    pub fn last_module_name(&self, manifest_path: &Path) -> Result<String> {
        match self.modules.last() {
            Some(Module::Object { name, .. }) => Ok(name.clone()),
            Some(Module::Reference(ref_name)) => {
                let parent = manifest_path
                    .parent()
                    .context("Manifest path has no parent directory")?;
                let ref_path = parent.join(ref_name);
                let modules = Self::load_module_file(&ref_path)?;
                match modules.into_iter().last() {
                    Some(Module::Object { name, .. }) => Ok(name),
                    _ => Ok(ref_name.clone()),
                }
            }
            None => Err(anyhow::anyhow!("Manifest has no modules")),
        }
    }

    fn load_module_file(path: &Path) -> Result<Vec<Module>> {
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
        let parts: Vec<&str> = manifest_prepend
            .into_iter()
            .chain(module_prepend)
            .chain(defaults.iter().copied())
            .chain(manifest_append)
            .chain(module_append)
            .collect();
        if parts.is_empty() {
            return None;
        }
        Some(format!("--env={var}={}", parts.join(":")))
    }
}

/// Recursively finds manifest files in the given path, optionally excluding a prefix subtree.
/// Returns a sorted Vec of manifest file paths, prioritizing ".Devel." manifests and shallower paths.
pub fn find_manifests_in_path(path: &Path, exclude_prefix: Option<&Path>) -> Vec<PathBuf> {
    use walkdir::WalkDir;

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

    const SKIP_DIRS: &[&str] = &[
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
    ];

    let walker = WalkDir::new(&path).into_iter().filter_entry(|entry| {
        if entry.depth() == 0 {
            return true;
        }
        let name = entry.file_name().to_string_lossy();
        if name.starts_with('.') {
            return false;
        }
        if entry.file_type().is_dir() && SKIP_DIRS.iter().any(|d| *d == name) {
            return false;
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
        if Manifest::from_file(entry.path()).is_ok() {
            manifests.push(entry.into_path());
        }
    }

    manifests.sort_by(|a, b| {
        let a_is_devel = a.to_str().is_some_and(|s| s.contains(".Devel."));
        let b_is_devel = b.to_str().is_some_and(|s| s.contains(".Devel."));
        b_is_devel
            .cmp(&a_is_devel)
            .then_with(|| a.components().count().cmp(&b.components().count()))
    });

    manifests
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use std::io::Write;
        use tempfile::NamedTempFile;

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
        use std::io::Write;
        use tempfile::NamedTempFile;

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
        use std::io::Write;
        use tempfile::NamedTempFile;

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
