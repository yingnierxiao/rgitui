use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitIgnoreRule {
    File(PathBuf),
    Directory(PathBuf),
    Extension(String),
}

/// Add a repository-relative rule to the worktree's `.gitignore`.
pub fn add_gitignore_rule(worktree_root: &Path, rule: &GitIgnoreRule) -> Result<bool> {
    let pattern = match rule {
        GitIgnoreRule::File(path) => format!("/{}", escaped_repo_path(path)?),
        GitIgnoreRule::Directory(path) => format!("/{}/", escaped_repo_path(path)?),
        GitIgnoreRule::Extension(extension) => {
            if extension.is_empty()
                || extension.contains('/')
                || extension.contains('\\')
                || extension == "."
                || extension == ".."
            {
                bail!("Invalid file extension for a .gitignore rule");
            }
            format!("*.{}", escape_pattern_component(extension))
        }
    };

    let ignore_path = worktree_root.join(".gitignore");
    let existing = match std::fs::read_to_string(&ignore_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("Read {}", ignore_path.display())),
    };
    if existing
        .lines()
        .any(|line| line.trim_end_matches('\r') == pattern)
    {
        return Ok(false);
    }

    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ignore_path)
        .with_context(|| format!("Open {}", ignore_path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(newline.as_bytes())?;
    }
    file.write_all(pattern.as_bytes())?;
    file.write_all(newline.as_bytes())?;
    Ok(true)
}

fn escaped_repo_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let Some(value) = value.to_str() else {
                    bail!("The selected path cannot be represented in a .gitignore rule");
                };
                parts.push(escape_pattern_component(value));
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("The selected path must be inside the repository");
            }
        }
    }
    if parts.is_empty() {
        bail!("The selected path is empty");
    }
    Ok(parts.join("/"))
}

fn escape_pattern_component(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for (index, character) in value.char_indices() {
        if matches!(character, '\\' | '*' | '?' | '[' | ']')
            || (index == 0 && character == ' ')
            || (index == 0 && matches!(character, '#' | '!'))
            || (character == ' ' && value[index + character.len_utf8()..].trim().is_empty())
        {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn appends_root_anchored_file_and_directory_rules() {
        let dir = tempdir().unwrap();
        assert!(add_gitignore_rule(
            dir.path(),
            &GitIgnoreRule::File(PathBuf::from("Assets/New file.txt"))
        )
        .unwrap());
        assert!(add_gitignore_rule(
            dir.path(),
            &GitIgnoreRule::Directory(PathBuf::from("Assets/Cache"))
        )
        .unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "/Assets/New file.txt\n/Assets/Cache/\n"
        );
    }

    #[test]
    fn preserves_existing_newlines_and_does_not_duplicate_rules() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.tmp\r\n").unwrap();
        let rule = GitIgnoreRule::Extension("tmp".to_string());
        assert!(!add_gitignore_rule(dir.path(), &rule).unwrap());
        assert!(add_gitignore_rule(dir.path(), &GitIgnoreRule::Extension("log".into())).unwrap());
        assert_eq!(
            std::fs::read(dir.path().join(".gitignore")).unwrap(),
            b"*.tmp\r\n*.log\r\n"
        );
    }

    #[test]
    fn rejects_paths_outside_the_repository() {
        let dir = tempdir().unwrap();
        assert!(add_gitignore_rule(
            dir.path(),
            &GitIgnoreRule::File(PathBuf::from("../outside.txt"))
        )
        .is_err());
    }

    #[test]
    fn handles_unicode_path_components() {
        let dir = tempdir().unwrap();
        assert!(add_gitignore_rule(
            dir.path(),
            &GitIgnoreRule::File(PathBuf::from("资料/缓存.txt"))
        )
        .unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitignore")).unwrap(),
            "/资料/缓存.txt\n"
        );
    }
}
