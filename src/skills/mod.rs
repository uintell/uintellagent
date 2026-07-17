// Local instruction skills selected explicitly with `--skill <name>`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

const MAX_SKILLS: usize = 8;
const MAX_INSTRUCTIONS_BYTES: u64 = 64 * 1024;
const SKILL_FORMAT_VERSION: u32 = 1;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SkillMeta {
    #[serde(default = "current_format_version")]
    pub format_version: u32,
    pub name: String,
    pub description: String,
    pub version: String,
    pub entrypoint: String,
    pub author: Option<String>,
}

fn current_format_version() -> u32 {
    SKILL_FORMAT_VERSION
}

pub fn skills_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".uintell").join("skills")
}

pub fn list_skills() -> Result<Vec<SkillMeta>> {
    list_skills_in(&skills_dir())
}

fn list_skills_in(root: &Path) -> Result<Vec<SkillMeta>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut skills = Vec::new();
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("read skills directory {}", root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let directory_name = entry.file_name().to_string_lossy().into_owned();
        if directory_name.starts_with('.') {
            continue;
        }
        validate_name(&directory_name)?;
        let metadata = read_metadata(&entry.path())?;
        if metadata.name != directory_name {
            bail!(
                "skill metadata name '{}' does not match directory '{}'",
                metadata.name,
                directory_name
            );
        }
        skills.push(metadata);
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

pub fn create_skill(name: &str, description: &str) -> Result<()> {
    create_skill_in(&skills_dir(), name, description)
}

fn create_skill_in(root: &Path, name: &str, description: &str) -> Result<()> {
    validate_name(name)?;
    let description = description.trim();
    if description.is_empty() || description.chars().count() > 500 {
        bail!("skill description must contain 1 to 500 characters");
    }

    create_private_dir(root)?;
    let directory = root.join(name);
    if directory.exists() {
        bail!("skill already exists: {name}");
    }
    let metadata = SkillMeta {
        format_version: SKILL_FORMAT_VERSION,
        name: name.to_string(),
        description: description.to_string(),
        version: "1.0.0".to_string(),
        entrypoint: "SKILL.md".to_string(),
        author: None,
    };
    let metadata = toml::to_string_pretty(&metadata)?;

    let title = name.replace(['-', '_'], " ");
    let instructions = format!(
        "# {title}\n\n{description}\n\n## Instructions\n\n- Add the concrete behavior this skill should apply.\n"
    );
    let staging = root.join(format!(
        ".{name}-{}-{:x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    create_private_dir_new(&staging)?;
    let result = (|| -> Result<()> {
        write_private_new(&staging.join("SKILL.md"), instructions.as_bytes())?;
        write_private_new(&staging.join("skill.toml"), metadata.as_bytes())?;
        std::fs::rename(&staging, &directory)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(staging);
    }
    result?;
    Ok(())
}

pub fn compose_preamble(base: &str, selected: &[String]) -> Result<String> {
    compose_preamble_from(&skills_dir(), base, selected)
}

fn compose_preamble_from(root: &Path, base: &str, selected: &[String]) -> Result<String> {
    if selected.len() > MAX_SKILLS {
        bail!("at most {MAX_SKILLS} skills may be selected");
    }

    let mut seen = HashSet::new();
    let mut preamble = base.to_string();
    for name in selected {
        validate_name(name)?;
        if !seen.insert(name.as_str()) {
            bail!("skill selected more than once: {name}");
        }
        let directory = root.join(name);
        let metadata =
            read_metadata(&directory).with_context(|| format!("load selected skill '{name}'"))?;
        if metadata.name != *name {
            bail!("selected skill metadata does not match its directory: {name}");
        }
        let entrypoint = safe_entrypoint(&directory, &metadata.entrypoint)?;
        let file = std::fs::File::open(&entrypoint)
            .with_context(|| format!("open skill entrypoint {}", entrypoint.display()))?;
        let size = file.metadata()?.len();
        if size > MAX_INSTRUCTIONS_BYTES {
            bail!(
                "skill '{}' exceeds the {} byte instruction limit",
                name,
                MAX_INSTRUCTIONS_BYTES
            );
        }
        let instructions = std::fs::read_to_string(&entrypoint)
            .with_context(|| format!("read skill entrypoint {}", entrypoint.display()))?;
        if instructions.trim().is_empty() {
            bail!("skill '{name}' has an empty instruction file");
        }
        preamble.push_str("\n\nSELECTED SKILL: ");
        preamble.push_str(&metadata.name);
        preamble.push_str("\nDESCRIPTION: ");
        preamble.push_str(&metadata.description);
        preamble.push_str("\n\n");
        preamble.push_str(&instructions);
    }
    Ok(preamble)
}

fn read_metadata(directory: &Path) -> Result<SkillMeta> {
    let path = directory.join("skill.toml");
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("read skill metadata {}", path.display()))?;
    let metadata: SkillMeta = toml::from_str(&contents)
        .with_context(|| format!("parse skill metadata {}", path.display()))?;
    if metadata.format_version != SKILL_FORMAT_VERSION {
        bail!(
            "unsupported skill format {} in {} (expected {})",
            metadata.format_version,
            path.display(),
            SKILL_FORMAT_VERSION
        );
    }
    Ok(metadata)
}

fn safe_entrypoint(directory: &Path, entrypoint: &str) -> Result<PathBuf> {
    let relative = Path::new(entrypoint);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("skill entrypoint must be a relative path without traversal");
    }
    let directory = std::fs::canonicalize(directory)
        .with_context(|| format!("resolve skill directory {}", directory.display()))?;
    let entrypoint = std::fs::canonicalize(directory.join(relative))
        .with_context(|| format!("resolve skill entrypoint {entrypoint}"))?;
    if !entrypoint.starts_with(&directory) || !entrypoint.is_file() {
        bail!("skill entrypoint escapes its skill directory");
    }
    Ok(entrypoint)
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("skill name must contain 1 to 64 ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)?;
    set_private_dir_permissions(path)
}

fn create_private_dir_new(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

fn set_private_dir_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn write_private_new(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root() -> PathBuf {
        std::env::temp_dir().join(format!(
            "uintell-skills-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn created_skill_is_loadable_and_selected_explicitly() {
        let root = test_root();
        create_skill_in(&root, "rust-review", "Review Rust changes").unwrap();
        let skills = list_skills_in(&root).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].entrypoint, "SKILL.md");

        let preamble = compose_preamble_from(&root, "base", &["rust-review".to_string()]).unwrap();
        assert!(preamble.contains("SELECTED SKILL: rust-review"));
        assert!(preamble.contains("Review Rust changes"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_names_and_entrypoints_cannot_escape_the_store() {
        let root = test_root();
        assert!(create_skill_in(&root, "../outside", "bad").is_err());
        create_skill_in(&root, "safe", "Safe skill").unwrap();
        let metadata_path = root.join("safe/skill.toml");
        let mut metadata: SkillMeta =
            toml::from_str(&std::fs::read_to_string(&metadata_path).unwrap()).unwrap();
        metadata.entrypoint = "../outside.md".into();
        std::fs::write(&metadata_path, toml::to_string(&metadata).unwrap()).unwrap();
        assert!(compose_preamble_from(&root, "base", &["safe".into()]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn future_skill_formats_are_refused() {
        let root = test_root();
        create_skill_in(&root, "future", "Future skill").unwrap();
        let metadata_path = root.join("future/skill.toml");
        let mut metadata: SkillMeta =
            toml::from_str(&std::fs::read_to_string(&metadata_path).unwrap()).unwrap();
        metadata.format_version += 1;
        std::fs::write(&metadata_path, toml::to_string(&metadata).unwrap()).unwrap();
        assert!(compose_preamble_from(&root, "base", &["future".into()]).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn created_skill_files_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root();
        create_skill_in(&root, "private", "Private skill").unwrap();
        let directory_mode = std::fs::metadata(root.join("private"))
            .unwrap()
            .permissions()
            .mode();
        let file_mode = std::fs::metadata(root.join("private/SKILL.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(directory_mode & 0o077, 0);
        assert_eq!(file_mode & 0o077, 0);
        std::fs::remove_dir_all(root).unwrap();
    }
}
