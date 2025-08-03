use std::path::PathBuf;

const BUILD_DIR: &str = ".flatplay";

pub struct BuildDirs {
    pub base: PathBuf,
}

impl BuildDirs {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }
    pub fn build_dir(&self) -> PathBuf {
        self.base.join(BUILD_DIR)
    }
    pub fn repo_dir(&self) -> PathBuf {
        self.build_dir().join("repo")
    }
    pub fn build_system_dir(&self) -> PathBuf {
        self.build_dir().join("_build")
    }
    pub fn flatpak_builder_dir(&self) -> PathBuf {
        self.build_dir().join("flatpak-builder")
    }
    pub fn finalized_repo_dir(&self) -> PathBuf {
        self.build_dir().join("finalized-repo")
    }
    pub fn ostree_dir(&self) -> PathBuf {
        self.build_dir().join("ostree")
    }
    pub fn metadata_file(&self) -> PathBuf {
        self.repo_dir().join("metadata")
    }
    pub fn files_dir(&self) -> PathBuf {
        self.repo_dir().join("files")
    }
    pub fn var_dir(&self) -> PathBuf {
        self.repo_dir().join("var")
    }
}
