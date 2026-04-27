use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::Deserialize;
use tempfile::NamedTempFile;

#[derive(Parser, Debug)]
#[command(version, about = "Tiny binary updater for GitHub release assets")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Update configured binaries from GitHub latest releases.
    Update {
        /// Resolve remote releases and planned installs without writing files.
        #[arg(long)]
        dry: bool,

        /// Path to the YAML config file.
        #[arg(long)]
        config: Option<PathBuf>,

        /// Optional package names. If omitted, all packages are updated.
        packages: Vec<String>,
    },
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(rename = "def")]
    definitions: HashMap<String, Vec<HashMap<String, String>>>,
    packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    repo: String,
    artifact: String,
    file: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
struct FileEntry {
    name: String,
    path: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedPackage {
    artifact: String,
    files: Vec<RenderedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RenderedFile {
    name: String,
    path: PathBuf,
}

#[derive(Debug)]
struct MatchedAsset {
    rendered: RenderedPackage,
    download_url: String,
    tag_name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Update {
            dry,
            config,
            packages,
        } => update(config, packages, dry).await,
    }
}

async fn update(config_path: Option<PathBuf>, package_names: Vec<String>, dry: bool) -> Result<()> {
    let config_path = config_path.unwrap_or(default_config_path()?);
    let config = load_config(&config_path)?;
    validate_config(&config)?;

    let requested = requested_packages(&package_names);
    let packages = select_packages(&config, requested.as_ref())?;
    let client = github_client()?;
    let install_dir = default_install_dir()?;

    for package in packages {
        let release = fetch_latest_release(&client, &package.repo)
            .await
            .with_context(|| format!("fetch latest release for {}", package.name))?;
        let matched = match_asset(package, &config, &release)
            .with_context(|| format!("match release asset for {}", package.name))?;

        println!(
            "{} {} -> {}",
            package.name, matched.tag_name, matched.rendered.artifact
        );
        for file in &matched.rendered.files {
            println!(
                "  {} -> {}",
                file.path.display(),
                install_dir.join(&file.name).display()
            );
        }

        if dry {
            continue;
        }

        install_package(&client, package, &matched, &install_dir)
            .await
            .with_context(|| format!("install {}", package.name))?;
    }

    Ok(())
}

fn load_config(path: &Path) -> Result<Config> {
    let input =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    serde_yaml::from_str(&input).with_context(|| format!("parse config {}", path.display()))
}

fn validate_config(config: &Config) -> Result<()> {
    if config.packages.is_empty() {
        bail!("config must contain at least one package");
    }

    let mut names = BTreeSet::new();
    for package in &config.packages {
        if package.name.trim().is_empty() {
            bail!("package name cannot be empty");
        }
        if !names.insert(&package.name) {
            bail!("duplicate package name: {}", package.name);
        }
        if package.file.is_empty() {
            bail!("package {} must contain at least one file", package.name);
        }
    }

    Ok(())
}

fn requested_packages(package_names: &[String]) -> Option<BTreeSet<&str>> {
    if package_names.is_empty() {
        return None;
    }

    Some(package_names.iter().map(String::as_str).collect())
}

fn select_packages<'a>(
    config: &'a Config,
    requested: Option<&BTreeSet<&str>>,
) -> Result<Vec<&'a Package>> {
    let packages: Vec<_> = config
        .packages
        .iter()
        .filter(|package| requested.is_none_or(|names| names.contains(package.name.as_str())))
        .collect();

    if let Some(requested) = requested {
        let found: BTreeSet<_> = packages
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        let missing: Vec<_> = requested
            .iter()
            .copied()
            .filter(|name| !found.contains(name))
            .collect();
        if !missing.is_empty() {
            bail!("unknown package(s): {}", missing.join(", "));
        }
    }

    Ok(packages)
}

fn github_client() -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(USER_AGENT, HeaderValue::from_static("rsbin"));
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );

    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.trim().is_empty()
    {
        let value = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .context("build GitHub authorization header")?;
        headers.insert(AUTHORIZATION, value);
    }

    reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .context("build HTTP client")
}

async fn fetch_latest_release(client: &reqwest::Client, repo_url: &str) -> Result<GitHubRelease> {
    let (owner, repo) = parse_github_repo(repo_url)?;
    let url = format!("https://api.github.com/repos/{owner}/{repo}/releases/latest");
    let response = client
        .get(url)
        .send()
        .await
        .context("send GitHub request")?
        .error_for_status()
        .context("GitHub latest release request failed")?;

    response.json().await.context("decode GitHub release")
}

fn parse_github_repo(repo_url: &str) -> Result<(String, String)> {
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

fn match_asset(
    package: &Package,
    config: &Config,
    release: &GitHubRelease,
) -> Result<MatchedAsset> {
    for rendered in render_package_candidates(package, config)? {
        if let Some(asset) = release
            .assets
            .iter()
            .find(|asset| asset.name == rendered.artifact)
        {
            return Ok(MatchedAsset {
                rendered,
                download_url: asset.browser_download_url.clone(),
                tag_name: release.tag_name.clone(),
            });
        }
    }

    let available = release
        .assets
        .iter()
        .map(|asset| asset.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no matching asset for {}; available assets: {}",
        package.name,
        available
    )
}

fn render_package_candidates(package: &Package, config: &Config) -> Result<Vec<RenderedPackage>> {
    let os = current_os_key()?;
    let definitions = config
        .definitions
        .get(os)
        .ok_or_else(|| anyhow!("missing def entry for current OS: {os}"))?;
    let arches = current_arch_aliases()?;
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();

    for arch in arches {
        for definition in definitions {
            let mut vars = BTreeMap::new();
            vars.insert("os".to_string(), os.to_string());
            vars.insert("arch".to_string(), arch.to_string());

            for (key, value) in definition {
                vars.insert(key.clone(), expand_template(value, &vars)?);
            }

            let rendered = RenderedPackage {
                artifact: expand_template(&package.artifact, &vars)?,
                files: package
                    .file
                    .iter()
                    .map(|file| {
                        Ok(RenderedFile {
                            name: expand_template(&file.name, &vars)?,
                            path: PathBuf::from(expand_template(&file.path, &vars)?),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            };

            if seen.insert(rendered.artifact.clone()) {
                candidates.push(rendered);
            }
        }
    }

    Ok(candidates)
}

fn expand_template(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut output = String::new();
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '{' {
            output.push(ch);
            continue;
        }

        let mut key = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '}' {
                closed = true;
                break;
            }
            key.push(next);
        }

        if !closed {
            bail!("unclosed template placeholder in {input}");
        }

        let value = vars
            .get(&key)
            .ok_or_else(|| anyhow!("unknown template variable {{{key}}} in {input}"))?;
        output.push_str(value);
    }

    Ok(output)
}

fn current_os_key() -> Result<&'static str> {
    match std::env::consts::OS {
        "linux" => Ok("linux"),
        "windows" => Ok("windows"),
        "macos" => Ok("macos"),
        other => bail!("unsupported OS: {other}"),
    }
}

fn current_arch_aliases() -> Result<Vec<&'static str>> {
    match std::env::consts::ARCH {
        "aarch64" => Ok(vec!["arm64", "aarch64"]),
        "x86_64" => Ok(vec!["x86_64", "amd64"]),
        "arm" => Ok(vec!["arm"]),
        other => bail!("unsupported arch: {other}"),
    }
}

async fn install_package(
    client: &reqwest::Client,
    package: &Package,
    matched: &MatchedAsset,
    install_dir: &Path,
) -> Result<()> {
    let bytes = client
        .get(&matched.download_url)
        .send()
        .await
        .context("send asset download request")?
        .error_for_status()
        .context("asset download request failed")?
        .bytes()
        .await
        .context("read asset bytes")?;

    let temp_dir = tempfile::tempdir().context("create extraction temp dir")?;
    let extracted = extract_artifact(&matched.rendered, &bytes, temp_dir.path())
        .with_context(|| format!("extract {}", matched.rendered.artifact))?;

    fs::create_dir_all(install_dir)
        .with_context(|| format!("create install dir {}", install_dir.display()))?;
    for file in extracted {
        install_file(&file.source, &install_dir.join(&file.name))
            .with_context(|| format!("install file {} for {}", file.name, package.name))?;
    }

    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ExtractedFile {
    name: String,
    source: PathBuf,
}

fn extract_artifact(
    rendered: &RenderedPackage,
    bytes: &[u8],
    temp_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    if rendered.artifact.ends_with(".tar.gz") || rendered.artifact.ends_with(".tgz") {
        extract_tar_gz(rendered, bytes, temp_dir)
    } else if rendered.artifact.ends_with(".zst") {
        extract_single_zst(rendered, bytes, temp_dir)
    } else {
        bail!("unsupported artifact format: {}", rendered.artifact)
    }
}

fn extract_tar_gz(
    rendered: &RenderedPackage,
    bytes: &[u8],
    temp_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    let wanted: BTreeMap<PathBuf, String> = rendered
        .files
        .iter()
        .map(|file| (file.path.clone(), file.name.clone()))
        .collect();
    let mut extracted = Vec::new();
    let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry.path().context("read tar entry path")?.into_owned();
        let Some(name) = wanted.get(&path) else {
            continue;
        };

        let target = temp_dir.join(name);
        entry
            .unpack(&target)
            .with_context(|| format!("extract {}", path.display()))?;
        extracted.push(ExtractedFile {
            name: name.clone(),
            source: target,
        });
    }

    ensure_all_files_extracted(rendered, &extracted)?;
    Ok(extracted)
}

fn extract_single_zst(
    rendered: &RenderedPackage,
    bytes: &[u8],
    temp_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    if rendered.files.len() != 1 {
        bail!(
            "{} is a single-file .zst artifact but declares {} files",
            rendered.artifact,
            rendered.files.len()
        );
    }

    let file = &rendered.files[0];
    let target = temp_dir.join(&file.name);
    let mut reader = zstd::Decoder::new(Cursor::new(bytes)).context("create zstd decoder")?;
    let mut output =
        File::create(&target).with_context(|| format!("create {}", target.display()))?;
    io::copy(&mut reader, &mut output).context("decompress zstd artifact")?;

    Ok(vec![ExtractedFile {
        name: file.name.clone(),
        source: target,
    }])
}

fn ensure_all_files_extracted(
    rendered: &RenderedPackage,
    extracted: &[ExtractedFile],
) -> Result<()> {
    let extracted_names: BTreeSet<_> = extracted.iter().map(|file| file.name.as_str()).collect();
    let missing = rendered
        .files
        .iter()
        .filter(|file| !extracted_names.contains(file.name.as_str()))
        .map(|file| file.path.display().to_string())
        .collect::<Vec<_>>();

    if !missing.is_empty() {
        bail!(
            "artifact missing configured file(s): {}",
            missing.join(", ")
        );
    }

    Ok(())
}

fn install_file(source: &Path, destination: &Path) -> Result<()> {
    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("destination has no parent: {}", destination.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent).context("create install temp file")?;
    let mut input = File::open(source).with_context(|| format!("open {}", source.display()))?;
    io::copy(&mut input, temp.as_file_mut()).context("copy binary to install temp file")?;

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

fn default_config_path() -> Result<PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config directory"))?;
    Ok(config_dir.join("rsbin").join("config.yml"))
}

fn default_install_dir() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow!("could not determine user home directory"))?;
    Ok(home.join(".local").join("bin"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::io::Write;
    use tar::{Builder, Header};

    fn sample_config() -> Config {
        serde_yaml::from_str(
            r#"
def:
  linux:
    - rust-triple: "{arch}-unknown-linux-gnu"

packages:
  - name: uv
    repo: https://github.com/astral-sh/uv
    artifact: uv-{rust-triple}-gnu.tar.gz
    file:
      - name: uv
        path: uv
      - name: uvx
        path: uvx
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_sample_config() {
        let config = sample_config();
        assert_eq!(config.packages[0].name, "uv");
        assert_eq!(config.definitions["linux"].len(), 1);
    }

    #[test]
    fn expands_templates() {
        let mut vars = BTreeMap::new();
        vars.insert("arch".to_string(), "x86_64".to_string());
        vars.insert(
            "rust-triple".to_string(),
            "{arch}-unknown-linux-gnu".to_string(),
        );

        let triple = expand_template(&vars["rust-triple"], &vars).unwrap();
        vars.insert("rust-triple".to_string(), triple);

        assert_eq!(
            expand_template("uv-{rust-triple}.tar.gz", &vars).unwrap(),
            "uv-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn parses_github_urls() {
        assert_eq!(
            parse_github_repo("https://github.com/openai/codex/").unwrap(),
            ("openai".to_string(), "codex".to_string())
        );
        assert_eq!(
            parse_github_repo("github.com/astral-sh/uv.git").unwrap(),
            ("astral-sh".to_string(), "uv".to_string())
        );
        assert!(parse_github_repo("https://example.com/openai/codex").is_err());
    }

    #[test]
    fn rejects_zst_with_multiple_files() {
        let rendered = RenderedPackage {
            artifact: "tool.zst".to_string(),
            files: vec![
                RenderedFile {
                    name: "a".to_string(),
                    path: PathBuf::from("a"),
                },
                RenderedFile {
                    name: "b".to_string(),
                    path: PathBuf::from("b"),
                },
            ],
        };

        assert!(extract_artifact(&rendered, b"not zstd", Path::new("/tmp")).is_err());
    }

    #[test]
    fn extracts_tar_gz_files() {
        let mut tar_bytes = Vec::new();
        {
            let encoder = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut tar = Builder::new(encoder);
            append_file(&mut tar, "uv", b"uv-bin");
            append_file(&mut tar, "uvx", b"uvx-bin");
            tar.finish().unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let rendered = RenderedPackage {
            artifact: "uv.tar.gz".to_string(),
            files: vec![
                RenderedFile {
                    name: "uv".to_string(),
                    path: PathBuf::from("uv"),
                },
                RenderedFile {
                    name: "uvx".to_string(),
                    path: PathBuf::from("uvx"),
                },
            ],
        };

        let extracted = extract_artifact(&rendered, &tar_bytes, temp.path()).unwrap();
        assert_eq!(extracted.len(), 2);
        assert_eq!(fs::read(temp.path().join("uv")).unwrap(), b"uv-bin");
        assert_eq!(fs::read(temp.path().join("uvx")).unwrap(), b"uvx-bin");
    }

    #[test]
    fn extracts_single_zst_file() {
        let compressed = zstd::encode_all(Cursor::new(b"tool-bin"), 0).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let rendered = RenderedPackage {
            artifact: "tool.zst".to_string(),
            files: vec![RenderedFile {
                name: "tool".to_string(),
                path: PathBuf::from("tool"),
            }],
        };

        let extracted = extract_artifact(&rendered, &compressed, temp.path()).unwrap();
        assert_eq!(extracted.len(), 1);
        assert_eq!(fs::read(temp.path().join("tool")).unwrap(), b"tool-bin");
    }

    fn append_file<W: Write>(tar: &mut Builder<W>, path: &str, bytes: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, path, Cursor::new(bytes))
            .unwrap();
    }
}
