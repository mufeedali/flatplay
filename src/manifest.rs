use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
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

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Module {
    Object {
        name: String,
        #[serde(default)]
        buildsystem: Option<String>,
        #[serde(rename = "config-opts", default)]
        config_opts: Option<Vec<String>>,
        #[serde(rename = "build-commands", default)]
        build_commands: Option<Vec<String>>,
        #[serde(rename = "post-install", default)]
        post_install: Option<Vec<String>>,
        #[serde(default)]
        sources: Vec<serde_json::Value>,
    },
    Reference(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Manifest {
    #[serde(alias = "app-id")]
    pub id: String,
    pub sdk: String,
    pub runtime: String,
    #[serde(rename = "runtime-version")]
    pub runtime_version: String,
    pub command: String,
    #[serde(rename = "x-run-args")]
    pub x_run_args: Option<Vec<String>>,
    #[serde(default)]
    pub modules: Vec<Module>,
    #[serde(rename = "finish-args", default)]
    pub finish_args: Vec<String>,
    #[serde(rename = "build-options", default)]
    pub build_options: serde_json::Value,
    #[serde(default)]
    pub cleanup: Vec<String>,
}

impl Manifest {
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)?;
        let manifest: Manifest = match path.extension().and_then(|s| s.to_str()) {
            Some("json") => serde_json::from_str(&content)?,
            Some("yaml") | Some("yml") => serde_saphyr::from_str(&content)?,
            _ => return Err(anyhow::anyhow!("Unsupported manifest format")),
        };
        if !is_valid_dbus_name(&manifest.id) {
            return Err(anyhow::anyhow!("Invalid application ID: {}", manifest.id));
        }
        Ok(manifest)
    }
}

/// Recursively finds manifest files in the given path, optionally excluding a prefix subtree.
/// Returns a sorted Vec of manifest file paths, prioritizing ".Devel." manifests and shallower paths.
pub fn find_manifests_in_path(path: &Path, exclude_prefix: Option<&Path>) -> Result<Vec<PathBuf>> {
    use walkdir::WalkDir;

    let mut manifests = vec![];

    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let exclude_prefix =
        exclude_prefix.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));

    for entry in WalkDir::new(&path)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_name().to_str().is_some_and(|s| s.starts_with('.')) {
                return false;
            }
            if let Some(prefix) = &exclude_prefix
                && e.path().starts_with(prefix)
            {
                return false;
            }
            true
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            matches!(
                e.path().extension().and_then(|s| s.to_str()),
                Some("json") | Some("yaml") | Some("yml")
            )
        })
        .filter(|e| Manifest::from_file(e.path()).is_ok())
    {
        manifests.push(entry.into_path());
    }

    manifests.sort_by(|a, b| {
        let a_is_devel = a.to_str().unwrap_or("").contains(".Devel.");
        let b_is_devel = b.to_str().unwrap_or("").contains(".Devel.");
        b_is_devel
            .cmp(&a_is_devel)
            .then_with(|| a.components().count().cmp(&b.components().count()))
    });

    Ok(manifests)
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
