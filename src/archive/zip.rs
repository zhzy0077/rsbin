use std::io::{self, Cursor};

use anyhow::{Context, Result};

use super::{ArchiveEntry, Extractor};

pub struct ZipExtractor;

impl Extractor for ZipExtractor {
    fn entries(&self, _artifact_name: &str, bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
        let mut entries = Vec::new();
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("read zip archive")?;

        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).context("read zip entry")?;
            if entry.is_dir() {
                continue;
            }

            let mode = entry.unix_mode();
            let archive_path = entry.name().to_string();
            let mut contents = Vec::new();
            io::copy(&mut entry, &mut contents)
                .with_context(|| format!("read {}", archive_path))?;

            entries.push(ArchiveEntry {
                archive_path,
                contents,
                mode,
            });
        }

        Ok(entries)
    }
}
