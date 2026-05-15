use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;

use super::{Asset, DownloadSource, Release, ResolvedArtifact};

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    url: String,
}

pub struct GitHubDownloadSource {
    client: reqwest::Client,
}

impl GitHubDownloadSource {
    pub fn new() -> Result<Self> {
        let client = build_github_client()?;
        Ok(Self { client })
    }
}

#[async_trait]
impl DownloadSource for GitHubDownloadSource {
    async fn latest_release(&self, repo_url: &str) -> Result<Release> {
        let (owner, repo) = parse_github_repo(repo_url)?;
        let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
        let response = self
            .client
            .get(url)
            .send()
            .await
            .context("send GitHub request")?
            .error_for_status()
            .context("GitHub latest release request failed")?;

        let gh_release: GitHubRelease = response.json().await.context("decode GitHub release")?;

        Ok(Release {
            tag_name: gh_release.tag_name,
            assets: gh_release
                .assets
                .into_iter()
                .map(|asset| Asset {
                    name: asset.name,
                    download_url: asset.url,
                })
                .collect(),
        })
    }

    async fn download(&self, artifact: &ResolvedArtifact) -> Result<bytes::Bytes> {
        self.client
            .get(&artifact.download_url)
            .header(ACCEPT, "application/octet-stream")
            .send()
            .await
            .context("send asset download request")?
            .error_for_status()
            .context("asset download request failed")?
            .bytes()
            .await
            .context("read asset bytes")
    }
}

fn build_github_client() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("rsbin"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );

    let token = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.trim().is_empty())
        .or_else(|| {
            std::process::Command::new("gh")
                .args(["auth", "token"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .and_then(|out| String::from_utf8(out.stdout).ok())
        });

    if let Some(token) = token {
        let token = token.trim();
        if !token.is_empty() {
            let value = HeaderValue::from_str(&format!("Bearer {}", token))
                .context("build GitHub authorization header")?;
            headers.insert(AUTHORIZATION, value);
        }
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("build HTTP client")
}

pub fn parse_github_repo(repo_url: &str) -> Result<(String, String)> {
    let trimmed = repo_url.trim().trim_end_matches('/');
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))
        .ok_or_else(|| anyhow!("unsupported GitHub repo URL: {repo_url}"))?;

    let mut parts = path.split('/');
    let owner = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| anyhow!("missing GitHub owner in {repo_url}"))?;
    let repo = parts
        .next()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| anyhow!("missing GitHub repo in {repo_url}"))?;

    Ok((owner.to_string(), repo.trim_end_matches(".git").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_release_assets_with_api_download_url() {
        let release: GitHubRelease = serde_json::from_str(
            r#"{
                "tag_name": "0.1.0",
                "assets": [
                    {
                        "name": "swcli-x86_64-unknown-linux-gnu.tar.gz",
                        "url": "https://api.github.com/repos/owner/repo/releases/assets/123",
                        "browser_download_url": "https://github.com/owner/repo/releases/download/0.1.0/swcli-x86_64-unknown-linux-gnu.tar.gz"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(release.assets.len(), 1);
        assert_eq!(
            release.assets[0].url,
            "https://api.github.com/repos/owner/repo/releases/assets/123"
        );
    }
}
