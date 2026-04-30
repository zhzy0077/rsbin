use std::io::{self, Cursor};

use anyhow::{Context, Result};

use super::{ArchiveEntry, Extractor};

pub struct Bz2Extractor;

impl Extractor for Bz2Extractor {
    fn entries(&self, artifact_name: &str, bytes: &[u8]) -> Result<Vec<ArchiveEntry>> {
        let mut reader = bzip2::read::BzDecoder::new(Cursor::new(bytes));

        let archive_path = artifact_name.strip_suffix(".bz2").unwrap_or(artifact_name);
        let mut contents = Vec::new();
        io::copy(&mut reader, &mut contents).context("decompress bz2 artifact")?;

        Ok(vec![ArchiveEntry {
            archive_path: archive_path.to_string(),
            contents,
            mode: None,
        }])
    }
}
