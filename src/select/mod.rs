use anyhow::{Result, bail};
use glob::Pattern;

use crate::archive::ArchiveEntry;
use crate::config::RenderedFile;

#[derive(Debug)]
pub struct SelectedFile {
    pub name: String,
    pub contents: Vec<u8>,
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
