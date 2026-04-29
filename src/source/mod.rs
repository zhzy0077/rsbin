use anyhow::Result;
use async_trait::async_trait;

pub mod github;

#[derive(Debug)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug)]
pub struct Asset {
    pub name: String,
    pub download_url: String,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct ResolvedArtifact {
    pub download_url: String,
    pub artifact_name: String,
    pub tag_name: String,
}

#[async_trait]
pub trait DownloadSource {
    async fn latest_release(&self, repo_url: &str) -> Result<Release>;
    async fn download(&self, artifact: &ResolvedArtifact) -> Result<bytes::Bytes>;
}
