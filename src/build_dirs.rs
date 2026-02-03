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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_dirs_paths() {
        let base = PathBuf::from("/tmp/test-project");
        let dirs = BuildDirs::new(base.clone());

        assert_eq!(dirs.build_dir(), base.join(".flatplay"));
        assert_eq!(dirs.repo_dir(), base.join(".flatplay/repo"));
        assert_eq!(dirs.build_system_dir(), base.join(".flatplay/_build"));
        assert_eq!(
            dirs.flatpak_builder_dir(),
            base.join(".flatplay/flatpak-builder")
        );
        assert_eq!(
            dirs.finalized_repo_dir(),
            base.join(".flatplay/finalized-repo")
        );
        assert_eq!(dirs.ostree_dir(), base.join(".flatplay/ostree"));
        assert_eq!(dirs.metadata_file(), base.join(".flatplay/repo/metadata"));
        assert_eq!(dirs.files_dir(), base.join(".flatplay/repo/files"));
        assert_eq!(dirs.var_dir(), base.join(".flatplay/repo/var"));
    }
}
