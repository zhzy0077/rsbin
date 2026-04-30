use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use glob::Pattern;

use crate::archive::ArchiveEntry;
use crate::config::RenderedFile;

#[derive(Debug)]
pub struct SelectedFile {
    pub name: String,
    pub contents: Vec<u8>,
}

#[derive(Debug)]
pub struct SelectedPackage {
    pub files: Vec<SelectedPackageFile>,
    pub links: Vec<SelectedPackageLink>,
}

#[derive(Debug)]
pub struct SelectedPackageFile {
    pub relative_path: PathBuf,
    pub contents: Vec<u8>,
    pub mode: Option<u32>,
}

#[derive(Debug)]
pub struct SelectedPackageLink {
    pub name: PathBuf,
    pub target: PathBuf,
}

pub fn select_extracted_files(
    configured_files: &[RenderedFile],
    extracted_entries: &[ArchiveEntry],
) -> Result<Vec<SelectedFile>> {
    let mut selected = Vec::new();

    for file in configured_files {
        let pattern = Pattern::new(&file.path.to_string_lossy())
            .map_err(|e| anyhow::anyhow!("invalid glob pattern {}: {}", file.path.display(), e))?;

        let mut found = false;
        for entry in extracted_entries {
            if pattern.matches(&entry.archive_path) {
                selected.push(SelectedFile {
                    name: file.name.clone(),
                    contents: entry.contents.clone(),
                });
                found = true;
                break;
            }
        }

        if !found {
            bail!("artifact missing configured file: {}", file.path.display());
        }
    }

    Ok(selected)
}

pub fn select_package_install(
    configured_files: &[RenderedFile],
    extracted_entries: &[ArchiveEntry],
) -> Result<SelectedPackage> {
    let files = extracted_entries
        .iter()
        .map(|entry| {
            Ok(SelectedPackageFile {
                relative_path: safe_relative_path(&entry.archive_path)?,
                contents: entry.contents.clone(),
                mode: entry.mode,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let links = configured_files
        .iter()
        .map(|file| {
            let pattern = Pattern::new(&file.path.to_string_lossy()).map_err(|e| {
                anyhow::anyhow!("invalid glob pattern {}: {}", file.path.display(), e)
            })?;

            let target = extracted_entries
                .iter()
                .find(|entry| pattern.matches(&entry.archive_path))
                .ok_or_else(|| {
                    anyhow::anyhow!("artifact missing configured file: {}", file.path.display())
                })
                .and_then(|entry| safe_relative_path(&entry.archive_path))?;

            Ok(SelectedPackageLink {
                name: safe_relative_path(&file.name)?,
                target,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(SelectedPackage { files, links })
}

fn safe_relative_path(raw: &str) -> Result<PathBuf> {
    if raw.is_empty() || raw.contains('\\') {
        bail!("unsafe archive path: {raw}");
    }

    let mut safe = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => safe.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("unsafe archive path: {raw}");
            }
        }
    }

    if safe.as_os_str().is_empty() {
        bail!("unsafe archive path: {raw}");
    }

    Ok(safe)
}
