use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::select::SelectedPackage;

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

pub fn package_install_dir(install_dir: &Path, package_name: &str) -> PathBuf {
    install_dir.join("packages").join(package_name)
}

pub fn install_package_tree(
    package_name: &str,
    package: &SelectedPackage,
    install_dir: &Path,
) -> Result<()> {
    validate_package_name(package_name)?;

    fs::create_dir_all(install_dir)
        .with_context(|| format!("create install dir {}", install_dir.display()))?;

    let packages_dir = install_dir.join("packages");
    fs::create_dir_all(&packages_dir)
        .with_context(|| format!("create packages dir {}", packages_dir.display()))?;

    let temp_dir = tempfile::Builder::new()
        .prefix(&format!(".{}-", package_name))
        .tempdir_in(&packages_dir)
        .context("create package temp dir")?;

    for file in &package.files {
        let destination = temp_dir.path().join(&file.relative_path);
        write_package_file(&destination, &file.contents, file.mode)
            .with_context(|| format!("install package file {}", file.relative_path.display()))?;
    }

    ensure_link_targets_executable(temp_dir.path(), package)?;

    let package_dir = package_install_dir(install_dir, package_name);
    let temp_path = temp_dir.keep();
    replace_package_dir(&temp_path, &package_dir, &packages_dir, package_name)?;

    for link in &package.links {
        let target = package_dir.join(&link.target);
        let link_path = install_dir.join(&link.name);
        replace_symlink(&target, &link_path)
            .with_context(|| format!("link {} -> {}", link_path.display(), target.display()))?;
    }

    Ok(())
}

fn validate_package_name(package_name: &str) -> Result<()> {
    if package_name.is_empty()
        || package_name == "."
        || package_name == ".."
        || package_name.contains('/')
        || package_name.contains('\\')
    {
        bail!("unsafe package name: {package_name}");
    }

    Ok(())
}

fn write_package_file(destination: &Path, contents: &[u8], mode: Option<u32>) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    fs::write(destination, contents).with_context(|| format!("write {}", destination.display()))?;

    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(destination, fs::Permissions::from_mode(mode & 0o777))
            .with_context(|| format!("set permissions on {}", destination.display()))?;
    }

    Ok(())
}

fn ensure_link_targets_executable(package_dir: &Path, package: &SelectedPackage) -> Result<()> {
    for link in &package.links {
        let target = package_dir.join(&link.target);
        let metadata = fs::metadata(&target)
            .with_context(|| format!("stat link target {}", target.display()))?;
        if !metadata.is_file() {
            bail!("link target is not a file: {}", target.display());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = metadata.permissions();
            let mode = permissions.mode();
            if mode & 0o111 == 0 {
                permissions.set_mode((mode & !0o777) | 0o755);
                fs::set_permissions(&target, permissions).with_context(|| {
                    format!("set executable permissions on {}", target.display())
                })?;
            }
        }
    }

    Ok(())
}

fn replace_package_dir(
    temp_path: &Path,
    package_dir: &Path,
    packages_dir: &Path,
    package_name: &str,
) -> Result<()> {
    let package_dir_exists = match fs::symlink_metadata(package_dir) {
        Ok(_) => true,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("stat {}", package_dir.display()));
        }
    };

    let backup_path = if package_dir_exists {
        let backup = tempfile::Builder::new()
            .prefix(&format!(".{}-old-", package_name))
            .tempdir_in(packages_dir)
            .context("create package backup dir")?;
        let backup_path = backup.keep();
        remove_existing_path(&backup_path)?;
        fs::rename(package_dir, &backup_path).with_context(|| {
            format!(
                "move existing package {} to {}",
                package_dir.display(),
                backup_path.display()
            )
        })?;
        Some(backup_path)
    } else {
        None
    };

    if let Err(error) = fs::rename(temp_path, package_dir) {
        if let Some(backup_path) = &backup_path {
            let _ = fs::rename(backup_path, package_dir);
        }
        let _ = remove_existing_path(temp_path);
        return Err(error).with_context(|| {
            format!(
                "replace package dir {} with {}",
                package_dir.display(),
                temp_path.display()
            )
        });
    }

    if let Some(backup_path) = backup_path {
        remove_existing_path(&backup_path)
            .with_context(|| format!("remove old package dir {}", backup_path.display()))?;
    }

    Ok(())
}

fn remove_existing_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            fs::remove_dir_all(path).with_context(|| format!("remove {}", path.display()))?;
        }
        Ok(_) => {
            fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
    }

    Ok(())
}

fn replace_symlink(target: &Path, link_path: &Path) -> Result<()> {
    let parent = link_path
        .parent()
        .ok_or_else(|| anyhow!("link path has no parent: {}", link_path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    match fs::symlink_metadata(link_path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            bail!("refusing to replace directory {}", link_path.display());
        }
        Ok(_) => {
            fs::remove_file(link_path)
                .with_context(|| format!("remove {}", link_path.display()))?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("stat {}", link_path.display())),
    }

    create_symlink(target, link_path)
}

#[cfg(unix)]
fn create_symlink(target: &Path, link_path: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link_path)
        .with_context(|| format!("create symlink {}", link_path.display()))
}

#[cfg(windows)]
fn create_symlink(target: &Path, link_path: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(target, link_path)
        .with_context(|| format!("create symlink {}", link_path.display()))
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link_path: &Path) -> Result<()> {
    bail!("package installs require symlink support on this platform")
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
