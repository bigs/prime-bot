//! Installs the embedded Prime Agent skill.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use include_dir::{Dir, DirEntry, include_dir};

static SKILL: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/skill");

pub fn default_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".prime/agent/skills/prime-bot"))
}

/// Replaces `target` with the embedded skill files.
pub fn install(target: &Path) -> Result<usize> {
    if target.exists() {
        std::fs::remove_dir_all(target)
            .with_context(|| format!("removing {}", target.display()))?;
    }
    std::fs::create_dir_all(target)?;
    let mut count = 0;
    write_dir(&SKILL, target, &mut count)?;
    Ok(count)
}

fn write_dir(dir: &Dir, target: &Path, count: &mut usize) -> Result<()> {
    for entry in dir.entries() {
        if entry
            .path()
            .components()
            .any(|c| c.as_os_str() == "__pycache__")
        {
            continue;
        }
        let dest = target.join(entry.path());
        match entry {
            DirEntry::Dir(d) => {
                std::fs::create_dir_all(&dest)?;
                write_dir(d, target, count)?;
            }
            DirEntry::File(f) => {
                std::fs::write(&dest, f.contents())
                    .with_context(|| format!("writing {}", dest.display()))?;
                *count += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn installs_skill_files() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("prime-bot");
        super::install(&target).unwrap();
        assert!(target.join("SKILL.md").is_file());
        assert!(target.join("references/bot.md").is_file());
        assert!(target.join("src/prime_bot/__init__.py").is_file());
        // Reinstalling replaces the directory.
        super::install(&target).unwrap();
    }
}
