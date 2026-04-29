use std::io::{self, Cursor};

use anyhow::{Context, Result};

use super::{ArchiveEntry, Extractor};

pub struct ZstExtractor;

impl Extractor for ZstExtractor {
    fn entries(&self, artifact_name: &str, bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
        let mut reader = zstd::Decoder::new(Cursor::new(bytes)).context("create zstd decoder")?;

        let archive_path = artifact_name.strip_suffix(".zst").unwrap_or(artifact_name);
        let mut contents = Vec::new();
        io::copy(&mut reader, &mut contents).context("decompress zstd artifact")?;

        Ok(vec![ArchiveEntry {
            archive_path: archive_path.to_string(),
            contents,
        }])
    }
}
