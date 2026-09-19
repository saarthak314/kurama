use std::{
    collections::BTreeMap,
    io::Read,
    path::{Component, Path},
};

use kurama_protocol::{KuramaError, verification::VerificationRecipe};

use crate::fs_safe::Directory;

const MAX_FILE_BYTES: u64 = 65_536;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipeFile {
    version: u32,
    #[serde(default)]
    recipes: BTreeMap<String, VerificationRecipe>,
}

/// Read project checks without granting permission to execute any of them.
pub fn read_verification_recipes(
    workspace: &Path,
) -> Result<BTreeMap<String, VerificationRecipe>, KuramaError> {
    let workspace = workspace.canonicalize()?;
    let root = Directory::open_absolute(&workspace)?;
    let file = match root
        .child(".kurama")
        .and_then(|dir| dir.open_file("verification.toml", false, false))
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("verification.toml exceeds 64 KiB"));
    }
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("verification.toml must be UTF-8"))?;
    let parsed: RecipeFile = basic_toml::from_str(text)
        .map_err(|error| invalid(format!("invalid verification.toml: {error}")))?;
    if parsed.version != 1 {
        return Err(invalid(
            "unsupported verification.toml version (expected 1)",
        ));
    }
    if parsed.recipes.len() > 32 {
        return Err(invalid(
            "verification.toml cannot contain more than 32 recipes",
        ));
    }
    for (name, recipe) in &parsed.recipes {
        if name.is_empty()
            || name.len() > 64
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(invalid(
                "recipe names must be 1–64 ASCII letters, digits, underscores, hyphens, or dots",
            ));
        }
        if recipe.command.trim().is_empty()
            || recipe.command.len() > 32_768
            || recipe.command.contains('\0')
        {
            return Err(invalid(format!(
                "recipe {name}: command must be nonblank and at most 32768 bytes without NUL"
            )));
        }
        if recipe.timeout_ms == 0 || recipe.timeout_ms > 3_600_000 {
            return Err(invalid(format!(
                "recipe {name}: timeout_ms must be between 1 and 3600000"
            )));
        }
        if recipe.cwd.is_empty() {
            return Err(invalid(format!(
                "recipe {name}: cwd must name a workspace directory"
            )));
        }
        let path = Path::new(&recipe.cwd);
        let relative = if path.is_absolute() {
            path.strip_prefix(&workspace).map_err(|_| {
                invalid(format!(
                    "recipe {name}: cwd must remain inside the workspace"
                ))
            })?
        } else {
            path
        };
        let mut directory = root.clone();
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => directory = directory.child(name)?,
                _ => {
                    return Err(invalid(format!(
                        "recipe {name}: cwd must remain inside the workspace without parent traversal"
                    )));
                }
            }
        }
    }
    Ok(parsed.recipes)
}

fn invalid(message: impl Into<String>) -> KuramaError {
    KuramaError::Configuration(message.into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn reads_defaults_and_rejects_unknown_fields_and_unsafe_paths() {
        let workspace = tempfile::tempdir().unwrap();
        let config = workspace.path().join(".kurama");
        std::fs::create_dir(&config).unwrap();
        let file = config.join("verification.toml");
        std::fs::write(&file, "version = 1\n[recipes.quick]\ncommand = 'true'\n").unwrap();
        let recipes = read_verification_recipes(workspace.path()).unwrap();
        assert_eq!(recipes["quick"].cwd, ".");
        assert_eq!(recipes["quick"].timeout_ms, 60_000);
        for extra in [
            "typo = 1",
            "cwd = '../'",
            "timeout_ms = 0",
            "timeout_ms = 3600001",
        ] {
            std::fs::write(
                &file,
                format!("version = 1\n[recipes.quick]\ncommand = 'true'\n{extra}\n"),
            )
            .unwrap();
            assert!(read_verification_recipes(workspace.path()).is_err());
        }
        std::fs::write(&file, "x".repeat(MAX_FILE_BYTES as usize + 1)).unwrap();
        assert!(read_verification_recipes(workspace.path()).is_err());
    }

    #[test]
    fn rejects_symlinked_configuration_and_working_directories() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let config = workspace.path().join(".kurama");
        std::os::unix::fs::symlink(outside.path(), &config).unwrap();
        assert!(read_verification_recipes(workspace.path()).is_err());
        std::fs::remove_file(&config).unwrap();
        std::fs::create_dir(&config).unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.path().join("outside")).unwrap();
        std::fs::write(
            config.join("verification.toml"),
            "version = 1\n[recipes.quick]\ncommand = 'true'\ncwd = 'outside'\n",
        )
        .unwrap();
        assert!(read_verification_recipes(workspace.path()).is_err());
    }
}
