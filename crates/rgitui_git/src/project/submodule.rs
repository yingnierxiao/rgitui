use anyhow::{Context as _, Result};
use git2::Repository;
use std::path::{Component, Path, PathBuf};

fn validate_submodule_path(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        || path
            .components()
            .any(|component| component.as_os_str() == ".git")
    {
        anyhow::bail!("Submodule path must be a repository-relative path without '..' or '.git'");
    }
    Ok(())
}

fn validate_submodule_url(url: &str) -> Result<()> {
    if url.trim().is_empty() || url.chars().any(char::is_control) {
        anyhow::bail!("Submodule URL cannot be empty or contain control characters");
    }
    Ok(())
}

fn run_submodule_git(repo_path: &Path, args: &[String]) -> Result<String> {
    let output = super::git_command()
        .current_dir(repo_path)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "protocol.file.allow")
        .env("GIT_CONFIG_VALUE_0", "always")
        .output()
        .context("Failed to start Git submodule command")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let details = if stderr.is_empty() { stdout } else { stderr };
        anyhow::bail!("Git submodule command failed: {details}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Add a submodule with Git's normal .gitmodules, index and checkout semantics.
pub fn submodule_add(
    repo_path: &Path,
    url: &str,
    path: &Path,
    branch: Option<&str>,
    recursive: bool,
) -> Result<()> {
    validate_submodule_url(url)?;
    validate_submodule_path(path)?;
    if let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) {
        super::argsafe::validate_branch_name(branch)?;
    }

    let mut args = vec!["submodule".to_string(), "add".to_string()];
    if let Some(branch) = branch.filter(|branch| !branch.trim().is_empty()) {
        args.extend(["--branch".to_string(), branch.to_string()]);
    }
    args.push("--".to_string());
    args.push(url.to_string());
    args.push(path.to_string_lossy().into_owned());
    run_submodule_git(repo_path, &args)?;

    if recursive {
        let mut update = vec![
            "submodule".to_string(),
            "update".to_string(),
            "--init".to_string(),
            "--recursive".to_string(),
            "--".to_string(),
        ];
        update.push(path.to_string_lossy().into_owned());
        run_submodule_git(repo_path, &update)?;
    }
    Ok(())
}

/// Remove a submodule from the index and .gitmodules, preserving Git's refusal
/// when the checked-out submodule contains local changes.
pub fn submodule_remove(repo_path: &Path, path: &Path) -> Result<()> {
    validate_submodule_path(path)?;
    run_submodule_git(
        repo_path,
        &[
            "submodule".to_string(),
            "deinit".to_string(),
            "--".to_string(),
            path.to_string_lossy().into_owned(),
        ],
    )?;
    run_submodule_git(
        repo_path,
        &[
            "rm".to_string(),
            "--".to_string(),
            path.to_string_lossy().into_owned(),
        ],
    )?;
    Ok(())
}

/// Change a submodule URL in `.gitmodules` and synchronize the local Git config.
pub fn submodule_set_url(repo_path: &Path, _name: &str, path: &Path, url: &str) -> Result<()> {
    validate_submodule_url(url)?;
    validate_submodule_path(path)?;
    run_submodule_git(
        repo_path,
        &[
            "submodule".to_string(),
            "set-url".to_string(),
            "--".to_string(),
            path.to_string_lossy().into_owned(),
            url.to_string(),
        ],
    )?;
    Ok(())
}

/// Information about a submodule in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmoduleInfo {
    /// Name of the submodule (from .gitmodules)
    pub name: String,
    /// Path relative to repository root
    pub path: PathBuf,
    /// URL of the submodule
    pub url: String,
    /// Branch configured for submodule (if any)
    pub branch: Option<String>,
    /// OID in HEAD tree (what the superproject expects)
    pub head_oid: Option<String>,
    /// OID in index (staged version)
    pub index_oid: Option<String>,
    /// OID in working directory (currently checked out)
    pub workdir_oid: Option<String>,
    /// Whether the submodule is initialized
    pub is_initialized: bool,
}

impl SubmoduleInfo {
    /// Returns a human-readable status string for this submodule.
    pub fn status(&self) -> &'static str {
        if !self.is_initialized {
            return "not initialized";
        }
        if self.head_oid.is_none() {
            return "no commit";
        }
        if self.workdir_oid.is_none() {
            return "not checked out";
        }
        if self.index_oid != self.head_oid || self.workdir_oid != self.index_oid {
            return "modified";
        }
        "up to date"
    }
}

/// Compute the status of all submodules in the repository.
pub fn compute_submodules(repo: &Repository) -> Result<Vec<SubmoduleInfo>> {
    if let Some(workdir) = repo.workdir() {
        let gitmodules = workdir.join(".gitmodules");
        if gitmodules.is_file()
            && std::fs::read_to_string(&gitmodules)
                .map(|contents| contents.trim().is_empty())
                .unwrap_or(false)
        {
            return Ok(Vec::new());
        }
    }

    let submodules = repo
        .submodules()
        .context("Failed to enumerate submodules")?;

    let mut result = Vec::new();
    for submodule in submodules {
        let name = submodule.name().unwrap_or("unknown").to_string();
        let path = submodule.path().to_path_buf();
        let url = submodule.url().unwrap_or("").to_string();
        let branch = submodule.branch().map(str::to_string);

        let head_oid = submodule.head_id().map(|oid| oid.to_string());
        let index_oid = submodule.index_id().map(|oid| oid.to_string());
        let workdir_oid = submodule.workdir_id().map(|oid| oid.to_string());

        // A submodule is initialized if its repository exists
        let is_initialized = submodule.open().is_ok();

        result.push(SubmoduleInfo {
            name,
            path,
            url,
            branch,
            head_oid,
            index_oid,
            workdir_oid,
            is_initialized,
        });
    }

    Ok(result)
}

/// Initialize a submodule by name.
/// This copies submodule info from .gitmodules to .git/config.
pub fn submodule_init(repo: &Repository, name: &str) -> Result<()> {
    let submodules = repo
        .submodules()
        .context("Failed to enumerate submodules")?;

    for mut submodule in submodules {
        if submodule.name() == Some(name) {
            submodule
                .init(false)
                .context("Failed to initialize submodule")?;
            return Ok(());
        }
    }

    anyhow::bail!("Submodule '{}' not found", name)
}

/// Update a submodule by name.
/// This fetches and checks out the submodule to the expected commit.
pub fn submodule_update(repo: &Repository, name: &str, init: bool) -> Result<()> {
    let submodules = repo
        .submodules()
        .context("Failed to enumerate submodules")?;

    let path = submodules
        .iter()
        .find(|submodule| submodule.name() == Some(name))
        .map(|submodule| submodule.path().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("Submodule '{}' not found", name))?;
    let workdir = repo
        .workdir()
        .context("Bare repositories cannot update submodules")?;
    let mut args = vec!["submodule".to_string(), "update".to_string()];
    if init {
        args.push("--init".to_string());
    }
    args.extend([
        "--recursive".to_string(),
        "--".to_string(),
        path.to_string_lossy().into_owned(),
    ]);
    run_submodule_git(workdir, &args)?;
    Ok(())
}

/// Initialize all submodules.
pub fn submodule_init_all(repo: &Repository) -> Result<Vec<String>> {
    let submodules = repo
        .submodules()
        .context("Failed to enumerate submodules")?;

    let mut initialized = Vec::new();
    for mut submodule in submodules {
        let name = submodule.name().unwrap_or("unknown").to_string();
        submodule
            .init(false)
            .context(format!("Failed to initialize submodule '{}'", name))?;
        initialized.push(name);
    }

    Ok(initialized)
}

/// Update all submodules.
pub fn submodule_update_all(repo: &Repository, init: bool) -> Result<Vec<String>> {
    let submodules = repo
        .submodules()
        .context("Failed to enumerate submodules")?;

    let updated = submodules
        .iter()
        .map(|submodule| submodule.name().unwrap_or("unknown").to_string())
        .collect::<Vec<_>>();
    let workdir = repo
        .workdir()
        .context("Bare repositories cannot update submodules")?;
    let mut args = vec!["submodule".to_string(), "update".to_string()];
    if init {
        args.push("--init".to_string());
    }
    args.extend(["--recursive".to_string(), "--".to_string()]);
    run_submodule_git(workdir, &args)?;
    Ok(updated)
}

use gpui::{AsyncApp, Context, Task, WeakEntity};

use super::GitProject;

impl GitProject {
    /// Get submodule status synchronously.
    pub fn submodules(&self) -> Result<Vec<SubmoduleInfo>> {
        let repo = self.open_repo()?;
        compute_submodules(&repo)
    }

    /// Get submodule status asynchronously.
    pub fn submodules_async(&self, cx: &mut Context<Self>) -> Task<Result<Vec<SubmoduleInfo>>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move {
                    let repo = Repository::open(&repo_path).context("Failed to open repository")?;
                    compute_submodules(&repo)
                })
                .await
        })
    }

    pub fn submodule_add_async(
        &self,
        url: String,
        path: PathBuf,
        branch: Option<String>,
        recursive: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move {
                    submodule_add(&repo_path, &url, &path, branch.as_deref(), recursive)
                })
                .await
        })
    }

    pub fn submodule_remove_async(
        &self,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move { submodule_remove(&repo_path, &path) })
                .await
        })
    }

    pub fn submodule_set_url_async(
        &self,
        name: String,
        path: PathBuf,
        url: String,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move { submodule_set_url(&repo_path, &name, &path, &url) })
                .await
        })
    }

    /// Initialize a submodule by name (async).
    pub fn submodule_init_async(&self, name: String, cx: &mut Context<Self>) -> Task<Result<()>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move {
                    let repo = Repository::open(&repo_path).context("Failed to open repository")?;
                    submodule_init(&repo, &name)
                })
                .await
        })
    }

    /// Update a submodule by name (async).
    pub fn submodule_update_async(
        &self,
        name: String,
        init: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move {
                    let repo = Repository::open(&repo_path).context("Failed to open repository")?;
                    submodule_update(&repo, &name, init)
                })
                .await
        })
    }

    /// Initialize all submodules (async).
    pub fn submodule_init_all_async(&self, cx: &mut Context<Self>) -> Task<Result<Vec<String>>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move {
                    let repo = Repository::open(&repo_path).context("Failed to open repository")?;
                    submodule_init_all(&repo)
                })
                .await
        })
    }

    /// Update all submodules (async).
    pub fn submodule_update_all_async(
        &self,
        init: bool,
        cx: &mut Context<Self>,
    ) -> Task<Result<Vec<String>>> {
        let repo_path = self.repo_path().to_path_buf();
        cx.spawn(async move |_this: WeakEntity<Self>, cx: &mut AsyncApp| {
            cx.background_executor()
                .spawn(async move {
                    let repo = Repository::open(&repo_path).context("Failed to open repository")?;
                    submodule_update_all(&repo, init)
                })
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{submodule_add, submodule_remove, submodule_set_url, SubmoduleInfo};
    use git2::Repository;
    use std::path::Path;
    use tempfile::TempDir;

    fn git(path: &Path, args: &[&str]) {
        let output = super::super::git_command()
            .current_dir(path)
            .args(args)
            .output()
            .expect("git is available");
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let parent = temp.path().join("parent");
        git(temp.path(), &["init", source.to_str().unwrap()]);
        git(&source, &["config", "user.name", "Test"]);
        git(&source, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(source.join("README"), "source\n").unwrap();
        git(&source, &["add", "README"]);
        git(&source, &["commit", "-m", "source"]);
        git(temp.path(), &["init", parent.to_str().unwrap()]);
        git(&parent, &["config", "user.name", "Test"]);
        git(&parent, &["config", "user.email", "test@example.invalid"]);
        (temp, parent, source)
    }

    fn make_submodule(
        is_initialized: bool,
        head_oid: Option<&str>,
        workdir_oid: Option<&str>,
    ) -> SubmoduleInfo {
        SubmoduleInfo {
            name: "test-sub".to_string(),
            path: std::path::PathBuf::from("libs/test-sub"),
            url: "https://example.com/repo".to_string(),
            branch: None,
            head_oid: head_oid.map(String::from),
            index_oid: head_oid.map(String::from),
            workdir_oid: workdir_oid.map(String::from),
            is_initialized,
        }
    }

    #[test]
    fn status_not_initialized() {
        let sub = make_submodule(false, Some("abc123"), Some("abc123"));
        assert_eq!(sub.status(), "not initialized");
    }

    #[test]
    fn status_up_to_date() {
        let sub = make_submodule(true, Some("abc123"), Some("abc123"));
        assert_eq!(sub.status(), "up to date");
    }

    #[test]
    fn status_modified_head_differs_from_workdir() {
        let sub = make_submodule(true, Some("abc123"), Some("def456"));
        assert_eq!(sub.status(), "modified");
    }

    #[test]
    fn status_modified_when_gitlink_is_staged() {
        let mut sub = make_submodule(true, Some("abc123"), Some("abc123"));
        sub.index_oid = Some("def456".to_string());
        assert_eq!(sub.status(), "modified");
    }

    #[test]
    fn status_not_checked_out_has_head_no_workdir() {
        let sub = make_submodule(true, Some("abc123"), None);
        assert_eq!(sub.status(), "not checked out");
    }

    #[test]
    fn status_no_commit() {
        let sub = make_submodule(true, None, None);
        assert_eq!(sub.status(), "no commit");
    }

    #[test]
    fn status_no_commit_with_workdir() {
        // Even with a workdir OID, if there's no head OID, it's "no commit"
        let sub = make_submodule(true, None, Some("def456"));
        assert_eq!(sub.status(), "no commit");
    }

    #[test]
    fn add_update_url_and_remove_follow_git_submodule_metadata() {
        let (_temp, parent, source) = fixture();
        let source_url = source.to_string_lossy().to_string();
        let path = Path::new("libs/source");
        submodule_add(&parent, &source_url, path, None, false).unwrap();

        let repo = Repository::open(&parent).unwrap();
        let listed = super::compute_submodules(&repo).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].path, path);
        assert!(listed[0].is_initialized);

        let new_url = format!("{source_url}-renamed");
        submodule_set_url(&parent, &listed[0].name, path, &new_url).unwrap();
        let repo = Repository::open(&parent).unwrap();
        let updated = super::compute_submodules(&repo).unwrap();
        assert_eq!(updated[0].url, new_url);

        git(&parent, &["add", ".gitmodules"]);
        git(&parent, &["commit", "-m", "update submodule URL"]);
        drop(repo);
        submodule_remove(&parent, path).unwrap();
        let repo = Repository::open(&parent).unwrap();
        let remaining = super::compute_submodules(&repo).unwrap();
        assert!(remaining.is_empty());
        assert!(!parent.join(path).exists());
    }

    #[test]
    fn submodule_path_validation_rejects_escape_and_git_metadata_paths() {
        let (_temp, parent, source) = fixture();
        let source_url = source.to_string_lossy().to_string();
        assert!(submodule_add(&parent, &source_url, Path::new("../outside"), None, false).is_err());
        assert!(submodule_add(&parent, &source_url, Path::new(".git/config"), None, false).is_err());
    }
}
