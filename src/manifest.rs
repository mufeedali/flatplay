use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

fn is_valid_dbus_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    let elements: Vec<&str> = name.split('.').collect();
    if elements.len() < 2 {
        return false;
    }
    elements.iter().all(|element| {
        if let Some(first_char) = element.chars().next() {
            !element.is_empty()
                && !first_char.is_ascii_digit()
                && element
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        } else {
            false
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
            Some("yaml") | Some("yml") => serde_yaml::from_str(&content)?,
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
        let a_is_devel = a.to_str().unwrap().contains(".Devel.");
        let b_is_devel = b.to_str().unwrap().contains(".Devel.");
        b_is_devel
            .cmp(&a_is_devel)
            .then_with(|| a.components().count().cmp(&b.components().count()))
    });

    Ok(manifests)
}
