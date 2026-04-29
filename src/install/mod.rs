use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LockFile {
    #[serde(default)]
    pub packages: BTreeMap<String, String>,
}

pub fn load_lock_file(path: &Path) -> Result<LockFile> {
    match fs::read_to_string(path) {
        Ok(input) => {
            serde_yaml::from_str(&input).with_context(|| format!("parse lock {}", path.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LockFile::default()),
        Err(error) => Err(error).with_context(|| format!("read lock {}", path.display())),
    }
}

pub fn save_lock_file(path: &Path, lock: &LockFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("lock path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent).context("create lock temp file")?;
    serde_yaml::to_writer(temp.as_file_mut(), lock)
        .with_context(|| format!("write lock temp file for {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;

    Ok(())
}

pub fn is_locked_latest(
    lock: &LockFile,
    package_name: &str,
    latest_tag: &str,
    force: bool,
) -> bool {
    !force
        && lock
            .packages
            .get(package_name)
            .is_some_and(|version| version == latest_tag)
}

pub fn record_successful_install(
    lock_path: &Path,
    lock: &mut LockFile,
    package_name: &str,
    tag_name: &str,
) -> Result<()> {
    if lock
        .packages
        .insert(package_name.to_string(), tag_name.to_string())
        != Some(tag_name.to_string())
    {
        // Persist progress package-by-package so a later failure does not cause
        // already-installed packages to be downloaded again on the next run.
        save_lock_file(lock_path, lock)?;
    }

    Ok(())
}

pub fn install_bytes(contents: &[u8], destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent).context("create install temp file")?;
    temp.as_file_mut()
        .write_all(contents)
        .context("write binary to install temp file")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o755))
            .context("set executable permissions")?;
    }

    temp.persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", destination.display()))?;

    Ok(())
}

pub fn default_config_path() -> Result<std::path::PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config directory"))?;
    Ok(config_dir.join("rsbin").join("config.yml"))
}

pub fn default_lock_path() -> Result<std::path::PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config directory"))?;
    Ok(config_dir.join("rsbin").join("rsbin.lock.yml"))
}

pub fn default_install_dir() -> Result<std::path::PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow!("could not determine user home directory"))?;
    Ok(home.join(".local").join("bin"))
}
