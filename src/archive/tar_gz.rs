use std::io::Cursor;

use anyhow::{Context, Result};

use super::{ArchiveEntry, Extractor};

pub struct TarGzExtractor;

impl Extractor for TarGzExtractor {
    fn entries(&self, _artifact_name: &str, bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
        let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
        read_tar_entries(decoder)
    }
}

fn read_tar_entries<R: std::io::Read>(reader: R) -> Result<Vec<ArchiveEntry>> {
    let mut entries = Vec::new();
    let mut archive = tar::Archive::new(reader);

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry.path().context("read tar entry path")?.into_owned();
        let archive_path = path.to_string_lossy().to_string();
        let mut contents = Vec::new();
        std::io::copy(&mut entry, &mut contents)
            .with_context(|| format!("read {}", archive_path))?;

        entries.push(ArchiveEntry {
            archive_path,
            contents,
        });
    }

    Ok(entries)
}
