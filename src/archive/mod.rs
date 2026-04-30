use anyhow::Result;

pub mod bz2;
pub mod tar_gz;
pub mod tar_xz;
pub mod zip;
pub mod zst;

#[derive(Debug)]
pub struct ArchiveEntry {
    pub archive_path: String,
    pub contents: Vec<u8>,
    pub mode: Option<u32>,
}

pub trait Extractor {
    fn entries(&self, artifact_name: &str, bytes: &[u8]) -> Result<Vec<ArchiveEntry>>;
}

pub fn select_extractor(artifact_name: &str) -> Result<Box<dyn Extractor>> {
    if artifact_name.ends_with(".tar.gz") || artifact_name.ends_with(".tgz") {
        Ok(Box::new(tar_gz::TarGzExtractor))
    } else if artifact_name.ends_with(".tar.xz") || artifact_name.ends_with(".txz") {
        Ok(Box::new(tar_xz::TarXzExtractor))
    } else if artifact_name.ends_with(".zip") {
        Ok(Box::new(zip::ZipExtractor))
    } else if artifact_name.ends_with(".bz2") {
        Ok(Box::new(bz2::Bz2Extractor))
    } else if artifact_name.ends_with(".zst") {
        Ok(Box::new(zst::ZstExtractor))
    } else {
        anyhow::bail!("unsupported artifact format: {}", artifact_name)
    }
}
