// Wasm Skills — extensible plugin system (stub)
//
// Future: compile-time verified Wasm plugins for custom tools
// Current: directory-based skill loader with metadata
//
// Skills live in ~/.uintell/skills/<name>/
// Each skill has:
//   skill.toml — metadata (name, description, version, entrypoint)
//   main.wasm  — compiled Wasm module (future)
//
// For now: skills are just metadata stubs. The Wasm runtime will be
// integrated once we have actual plugins to load.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub version: String,
    pub entrypoint: String,
    pub author: Option<String>,
}

pub fn skills_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".uintell").join("skills")
}

pub fn list_skills() -> std::io::Result<Vec<SkillMeta>> {
    let dir = skills_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut skills = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let toml_path = entry.path().join("skill.toml");
            if toml_path.exists() {
                if let Ok(content) = std::fs::read_to_string(&toml_path) {
                    if let Ok(meta) = toml::from_str::<SkillMeta>(&content) {
                        skills.push(meta);
                    }
                }
            }
        }
    }
    Ok(skills)
}

pub fn create_skill(name: &str, description: &str) -> std::io::Result<()> {
    let dir = skills_dir().join(name);
    std::fs::create_dir_all(&dir)?;

    let meta = SkillMeta {
        name: name.to_string(),
        description: description.to_string(),
        version: "0.1.0".to_string(),
        entrypoint: "main.wasm".to_string(),
        author: None,
    };

    let toml_str = toml::to_string_pretty(&meta).map_err(std::io::Error::other)?;

    std::fs::write(dir.join("skill.toml"), toml_str)?;
    Ok(())
}
