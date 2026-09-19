use std::{
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
};

use crate::fs_safe::Directory;

use kurama_protocol::{KuramaError, agent::WriteScope, policy::ExecutionMode, tool::ToolContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedPath {
    pub absolute: PathBuf,
    pub external: bool,
}

impl GuardedPath {
    pub(crate) fn parent_directory(&self) -> Result<(Directory, OsString), KuramaError> {
        let parent = self
            .absolute
            .parent()
            .ok_or_else(|| KuramaError::Tool("path has no parent".into()))?;
        let name = self
            .absolute
            .file_name()
            .ok_or_else(|| KuramaError::Tool("path has no file name".into()))?;
        Ok((Directory::open_absolute(parent)?, name.to_owned()))
    }

    pub(crate) fn open_file(&self) -> Result<File, KuramaError> {
        let (parent, name) = self.parent_directory()?;
        Ok(parent.open_file(name, false, false)?)
    }
}

#[derive(Debug, Clone)]
pub struct PathGuard {
    cwd: PathBuf,
    workspace_root: PathBuf,
    unrestricted: bool,
}

impl PathGuard {
    pub fn new(context: &ToolContext) -> Result<Self, KuramaError> {
        let workspace_root = canonical_directory(&context.workspace_root, "workspace root")?;
        let cwd = canonical_directory(&context.cwd, "working directory")?;
        Ok(Self {
            cwd,
            workspace_root,
            unrestricted: context.mode == ExecutionMode::Yolo,
        })
    }

    pub fn resolve_existing(&self, path: impl AsRef<Path>) -> Result<GuardedPath, KuramaError> {
        let requested = self.absolute_from_cwd(path.as_ref())?;
        let absolute = requested.canonicalize().map_err(|error| {
            KuramaError::Tool(format!("cannot resolve {}: {error}", requested.display()))
        })?;
        Ok(GuardedPath {
            external: !absolute.starts_with(&self.workspace_root),
            absolute,
        })
    }

    pub fn resolve_existing_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<GuardedPath, KuramaError> {
        let guarded = self.resolve_existing(path)?;
        guarded.open_file()?;
        Ok(guarded)
    }

    pub fn resolve_workspace_directory(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<GuardedPath, KuramaError> {
        let guarded = self.resolve_existing(path)?;
        Directory::open_absolute(&guarded.absolute)?;
        if guarded.external && !self.unrestricted {
            return Err(KuramaError::Policy(format!(
                "working directory escapes the workspace: {}",
                guarded.absolute.display()
            )));
        }
        Ok(guarded)
    }

    pub fn resolve_write(
        &self,
        path: impl AsRef<Path>,
        scope: &WriteScope,
    ) -> Result<GuardedPath, KuramaError> {
        if scope.is_read_only() && !self.unrestricted {
            return Err(KuramaError::Policy("write scope is read-only".into()));
        }

        let requested = self.absolute_from_cwd(path.as_ref())?;
        let absolute = if requested.exists() {
            requested.canonicalize().map_err(|error| {
                KuramaError::Tool(format!("cannot resolve {}: {error}", requested.display()))
            })?
        } else {
            let file_name = requested.file_name().ok_or_else(|| {
                KuramaError::Tool(format!("invalid write path: {}", requested.display()))
            })?;
            let parent = requested.parent().ok_or_else(|| {
                KuramaError::Tool(format!("write path has no parent: {}", requested.display()))
            })?;
            let canonical_parent = canonical_directory(parent, "write parent")?;
            canonical_parent.join(file_name)
        };

        if !self.unrestricted && !self.scope_allows(&absolute, scope)? {
            return Err(KuramaError::Policy(format!(
                "write path is outside the allowed scope: {}",
                absolute.display()
            )));
        }

        Ok(GuardedPath {
            external: !absolute.starts_with(&self.workspace_root),
            absolute,
        })
    }

    fn absolute_from_cwd(&self, path: &Path) -> Result<PathBuf, KuramaError> {
        if path.as_os_str().is_empty() {
            return Err(KuramaError::Tool("path must not be empty".into()));
        }
        Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        })
    }

    fn scope_allows(&self, target: &Path, scope: &WriteScope) -> Result<bool, KuramaError> {
        for root in &scope.roots {
            let root = self.resolve_scope_path(root, true)?;
            if target.starts_with(root) {
                return Ok(true);
            }
        }
        for file in &scope.files {
            let file = self.resolve_scope_path(file, false)?;
            if target == file {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn resolve_scope_path(&self, path: &Path, directory: bool) -> Result<PathBuf, KuramaError> {
        let requested = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };
        if requested.exists() {
            return requested.canonicalize().map_err(KuramaError::Io);
        }
        if directory {
            return Err(KuramaError::Policy(format!(
                "write root does not exist: {}",
                requested.display()
            )));
        }
        let parent = requested.parent().ok_or_else(|| {
            KuramaError::Policy(format!("write file has no parent: {}", requested.display()))
        })?;
        Ok(canonical_directory(parent, "write scope parent")?.join(
            requested
                .file_name()
                .ok_or_else(|| KuramaError::Policy("write file has no name".into()))?,
        ))
    }
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, KuramaError> {
    let canonical = path.canonicalize().map_err(|error| {
        KuramaError::Tool(format!(
            "cannot resolve {label} {}: {error}",
            path.display()
        ))
    })?;
    Directory::open_absolute(&canonical)?;
    Ok(canonical)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn guarded_open_rejects_leaf_and_ancestor_swaps_after_resolution() {
        for swap_ancestor in [false, true] {
            let temp = tempfile::tempdir().expect("tempdir");
            let workspace = temp.path().join("workspace");
            let outside = temp.path().join("outside");
            fs::create_dir_all(workspace.join("dir")).expect("workspace");
            fs::create_dir(&outside).expect("outside");
            fs::write(workspace.join("dir/file"), b"inside").expect("inside");
            fs::write(outside.join("file"), b"outside sentinel").expect("outside");
            let workspace = workspace.canonicalize().expect("canonical workspace");
            let guard = PathGuard {
                cwd: workspace.clone(),
                workspace_root: workspace.clone(),
                unrestricted: false,
            };
            let guarded = guard
                .resolve_existing_file("dir/file")
                .expect("resolve before swap");
            if swap_ancestor {
                fs::rename(workspace.join("dir"), workspace.join("original"))
                    .expect("move directory");
                symlink(&outside, workspace.join("dir")).expect("ancestor swap");
            } else {
                fs::remove_file(workspace.join("dir/file")).expect("remove leaf");
                symlink(outside.join("file"), workspace.join("dir/file")).expect("leaf swap");
            }
            assert!(guarded.open_file().is_err());
            assert_eq!(
                fs::read(outside.join("file")).expect("outside sentinel"),
                b"outside sentinel"
            );
        }
    }

    #[test]
    fn anchored_replacement_cannot_be_redirected_by_parent_rename() {
        let temp = tempfile::tempdir().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let outside = temp.path().join("outside");
        fs::create_dir_all(workspace.join("dir")).expect("workspace");
        fs::create_dir(&outside).expect("outside");
        fs::write(workspace.join("dir/file"), b"inside").expect("inside");
        fs::write(outside.join("file"), b"outside sentinel").expect("outside");
        let workspace = workspace.canonicalize().expect("canonical workspace");
        let guarded = GuardedPath {
            absolute: workspace.join("dir/file"),
            external: false,
        };
        let (directory, name) = guarded.parent_directory().expect("anchor transaction");
        fs::rename(workspace.join("dir"), workspace.join("original")).expect("move directory");
        symlink(&outside, workspace.join("dir")).expect("ancestor swap");
        directory
            .replace(name, b"new inside")
            .expect("anchored replacement");
        assert_eq!(
            fs::read(workspace.join("original/file")).expect("original directory"),
            b"new inside"
        );
        assert_eq!(
            fs::read(outside.join("file")).expect("outside sentinel"),
            b"outside sentinel"
        );
    }
}
