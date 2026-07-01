//! Fetch application git sources without invoking the `git` CLI (via libgit2/`git2`).

use std::path::Path;

use anyhow::{Context, Result};
use git2::build::RepoBuilder;
use git2::{FetchOptions, Repository, SubmoduleUpdateOptions};

use crate::utils::{path_to_str, status, verbose};

pub struct GitRef<'a> {
    pub url: &'a str,
    pub commit: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub branch: Option<&'a str>,
}

/// Clone `url` into `source_dir`, checking out commit, tag, or branch, then update submodules.
pub fn fetch_git_source(source_dir: &Path, name: &str, git_ref: GitRef<'_>) -> Result<()> {
    if source_dir.exists() {
        std::fs::remove_dir_all(source_dir).with_context(|| {
            format!(
                "Failed to remove existing source dir {}",
                source_dir.display()
            )
        })?;
    }
    if let Some(parent) = source_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let repo = match (git_ref.commit, git_ref.tag, git_ref.branch) {
        (Some(commit), _, _) => {
            status(format!(
                "Cloning {name} from {} (commit {commit})",
                git_ref.url
            ));
            let repo = clone_repository(git_ref.url, source_dir, None)?;
            checkout_commit(&repo, commit)?;
            repo
        }
        (None, Some(tag), _) => {
            status(format!("Cloning {name} from {} (tag {tag})", git_ref.url));
            clone_repository(git_ref.url, source_dir, Some(tag))?
        }
        (None, None, Some(branch)) => {
            status(format!(
                "Cloning {name} from {} (branch {branch})",
                git_ref.url
            ));
            clone_repository(git_ref.url, source_dir, Some(branch))?
        }
        (None, None, None) => {
            anyhow::bail!(
                "Git source in module '{name}' must specify one of: tag, commit, branch"
            );
        }
    };

    update_submodules(&repo)?;

    verbose(format!(
        "Git source for {name} ready at {}",
        path_to_str(source_dir)?
    ));
    Ok(())
}

fn clone_repository(url: &str, source_dir: &Path, branch_or_tag: Option<&str>) -> Result<Repository> {
    let mut builder = RepoBuilder::new();
    let mut fetch_opts = FetchOptions::new();
    // Depth is only applied for tip refs; arbitrary commits need a full fetch history.
    if branch_or_tag.is_some() {
        fetch_opts.depth(1);
    }
    builder.fetch_options(fetch_opts);
    if let Some(reference) = branch_or_tag {
        builder.branch(reference);
    }

    builder
        .clone(url, source_dir)
        .with_context(|| format!("Failed to clone {url} into {}", source_dir.display()))
}

fn checkout_commit(repo: &Repository, commit: &str) -> Result<()> {
    let object = repo
        .revparse_single(commit)
        .with_context(|| format!("Failed to resolve commit '{commit}'"))?;
    repo.checkout_tree(&object, None)
        .with_context(|| format!("Failed to checkout tree for '{commit}'"))?;
    repo.set_head_detached(object.id())
        .with_context(|| format!("Failed to detach HEAD at '{commit}'"))?;
    Ok(())
}

fn update_submodules(repo: &Repository) -> Result<()> {
    fn update_recursive(repo: &Repository) -> Result<()> {
        for mut submodule in repo.submodules().context("Failed to list submodules")? {
            let mut opts = SubmoduleUpdateOptions::new();
            submodule
                .update(true, Some(&mut opts))
                .with_context(|| {
                    format!(
                        "Failed to update submodule {}",
                        submodule.name().unwrap_or("<unknown>")
                    )
                })?;
            if let Ok(sub_repo) = submodule.open() {
                update_recursive(&sub_repo)?;
            }
        }
        Ok(())
    }

    match update_recursive(repo) {
        Ok(()) => Ok(()),
        Err(error) => {
            // Submodules are best-effort when the superproject has none configured oddly.
            verbose(format!("Submodule update warning: {error:#}"));
            Ok(())
        }
    }
}
