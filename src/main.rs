use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use glob::Pattern;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue, USER_AGENT};
use serde::{Deserialize, Deserializer, Serialize};
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

        /// Reinstall even when the lock file says a package is current.
        #[arg(long)]
        force: bool,

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
    definitions: Definitions,
    packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
struct Definitions {
    #[serde(flatten)]
    by_os: HashMap<String, Vec<DefinitionSet>>,
}

#[derive(Debug, Deserialize)]
struct DefinitionSet {
    #[serde(flatten)]
    values: HashMap<String, DefinitionValueList>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DefinitionValueList(Vec<String>);

impl<'de> Deserialize<'de> for DefinitionValueList {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawDefinitionValue {
            Scalar(String),
            List(Vec<String>),
        }

        match RawDefinitionValue::deserialize(deserializer)? {
            RawDefinitionValue::Scalar(value) => Ok(Self(vec![value])),
            RawDefinitionValue::List(values) => Ok(Self(values)),
        }
    }
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
            force,
            config,
            packages,
        } => update(config, packages, dry, force).await,
    }
}

async fn update(
    config_path: Option<PathBuf>,
    package_names: Vec<String>,
    dry: bool,
    force: bool,
) -> Result<()> {
    let config_path = config_path.unwrap_or(default_config_path()?);
    let config = load_config(&config_path)?;
    validate_config(&config)?;

    let requested = requested_packages(&package_names);
    let packages = select_packages(&config, requested.as_ref())?;
    let client = github_client()?;
    let install_dir = default_install_dir()?;
    let lock_path = default_lock_path()?;
    let mut lock = load_lock_file(&lock_path)?;

    for package in packages {
        let release = fetch_latest_release(&client, &package.repo)
            .await
            .with_context(|| format!("fetch latest release for {}", package.name))?;

        if is_locked_latest(&lock, &package.name, &release.tag_name, force) {
            println!("{} {} is already latest", package.name, release.tag_name);
            continue;
        }

        let matched = match_asset(package, &config, &release)
            .with_context(|| format!("match release asset for {}", package.name))?;

        if dry {
            println!(
                "{} {} would update -> {}",
                package.name, matched.tag_name, matched.rendered.artifact
            );
        } else {
            println!(
                "{} {} -> {}",
                package.name, matched.tag_name, matched.rendered.artifact
            );
        }
        for file in &matched.rendered.files {
            println!(
                "  {} -> {}",
                file.path.display(),
                install_dir.join(&file.name).display()
            );
        }

        if dry {
            validate_package_archive(&client, &matched)
                .await
                .with_context(|| format!("validate archive for {}", package.name))?;
            continue;
        }

        install_package(&client, package, &matched, &install_dir)
            .await
            .with_context(|| format!("install {}", package.name))?;
        record_successful_install(&lock_path, &mut lock, &package.name, &matched.tag_name)?;
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

#[derive(Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
struct LockFile {
    #[serde(default)]
    packages: BTreeMap<String, String>,
}

fn load_lock_file(path: &Path) -> Result<LockFile> {
    match fs::read_to_string(path) {
        Ok(input) => {
            serde_yaml::from_str(&input).with_context(|| format!("parse lock {}", path.display()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(LockFile::default()),
        Err(error) => Err(error).with_context(|| format!("read lock {}", path.display())),
    }
}

fn save_lock_file(path: &Path, lock: &LockFile) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("lock path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;

    let mut temp = NamedTempFile::new_in(parent).context("create lock temp file")?;
    serde_yaml::to_writer(temp.as_file_mut(), lock)
        .with_context(|| format!("write lock temp file for {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("replace {}", path.display()))?;

    Ok(())
}

fn is_locked_latest(lock: &LockFile, package_name: &str, latest_tag: &str, force: bool) -> bool {
    !force
        && lock
            .packages
            .get(package_name)
            .is_some_and(|version| version == latest_tag)
}

fn record_successful_install(
    lock_path: &Path,
    lock: &mut LockFile,
    package_name: &str,
    tag_name: &str,
) -> Result<()> {
    if lock
        .packages
        .insert(package_name.to_string(), tag_name.to_string())
        != Some(tag_name.to_string())
    {
        // Persist progress package-by-package so a later failure does not cause
        // already-installed packages to be downloaded again on the next run.
        save_lock_file(lock_path, lock)?;
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
        if let Some(asset) = release.assets.iter().find(|asset| {
            Pattern::new(&rendered.artifact)
                .map(|p| p.matches(&asset.name))
                .unwrap_or(false)
        }) {
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
        .by_os
        .get(os)
        .ok_or_else(|| anyhow!("missing def entry for current OS: {os}"))?;
    let arches = current_arch_aliases()?;
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();

    for arch in arches {
        for definition in definitions {
            let mut base_vars = BTreeMap::new();
            base_vars.insert("os".to_string(), os.to_string());
            base_vars.insert("arch".to_string(), arch.to_string());

            for vars in expand_definition_set(definition, base_vars)? {
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
    }

    Ok(candidates)
}

fn expand_definition_set(
    definition: &DefinitionSet,
    base_vars: BTreeMap<String, String>,
) -> Result<Vec<BTreeMap<String, String>>> {
    let mut candidates = vec![base_vars];

    for (key, values) in &definition.values {
        let mut next_candidates = Vec::new();

        for vars in &candidates {
            for value in &values.0 {
                let mut next_vars = vars.clone();
                next_vars.insert(key.clone(), expand_template(value, vars)?);
                next_candidates.push(next_vars);
            }
        }

        candidates = next_candidates;
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
        "x86_64" => Ok(vec!["x64", "x86_64", "amd64"]),
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
    let bytes = download_asset(client, &matched.download_url).await?;
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

async fn validate_package_archive(client: &reqwest::Client, matched: &MatchedAsset) -> Result<()> {
    let bytes = download_asset(client, &matched.download_url).await?;
    let temp_dir = tempfile::tempdir().context("create validation temp dir")?;
    extract_artifact(&matched.rendered, &bytes, temp_dir.path())
        .with_context(|| format!("validate {}", matched.rendered.artifact))?;
    Ok(())
}

async fn download_asset(client: &reqwest::Client, download_url: &str) -> Result<bytes::Bytes> {
    client
        .get(download_url)
        .send()
        .await
        .context("send asset download request")?
        .error_for_status()
        .context("asset download request failed")?
        .bytes()
        .await
        .context("read asset bytes")
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
        let decoder = flate2::read::GzDecoder::new(Cursor::new(bytes));
        extract_tar(rendered, decoder, temp_dir)
    } else if rendered.artifact.ends_with(".tar.xz") || rendered.artifact.ends_with(".txz") {
        let decoder = xz2::read::XzDecoder::new(Cursor::new(bytes));
        extract_tar(rendered, decoder, temp_dir)
    } else if rendered.artifact.ends_with(".bz2") {
        extract_single_bz2(rendered, bytes, temp_dir)
    } else if rendered.artifact.ends_with(".zst") {
        extract_single_zst(rendered, bytes, temp_dir)
    } else if rendered.artifact.ends_with(".zip") {
        extract_zip(rendered, bytes, temp_dir)
    } else {
        bail!("unsupported artifact format: {}", rendered.artifact)
    }
}

fn extract_tar<R: io::Read>(
    rendered: &RenderedPackage,
    reader: R,
    temp_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    let mut extracted = Vec::new();
    let mut archive = tar::Archive::new(reader);

    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let path = entry.path().context("read tar entry path")?.into_owned();
        let path_str = path.to_string_lossy();

        let mut matched_name = None;
        for file in &rendered.files {
            if Pattern::new(&file.path.to_string_lossy())
                .map(|p| p.matches(&path_str))
                .unwrap_or(false)
            {
                matched_name = Some(file.name.clone());
                break;
            }
        }

        let Some(name) = matched_name else {
            continue;
        };

        let target = temp_dir.join(&name);
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

fn extract_single_bz2(
    rendered: &RenderedPackage,
    bytes: &[u8],
    temp_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    if rendered.files.len() != 1 {
        bail!(
            "{} is a single-file .bz2 artifact but declares {} files",
            rendered.artifact,
            rendered.files.len()
        );
    }

    let file = &rendered.files[0];
    let target = temp_dir.join(&file.name);
    let mut reader = bzip2::read::BzDecoder::new(Cursor::new(bytes));
    let mut output =
        File::create(&target).with_context(|| format!("create {}", target.display()))?;
    io::copy(&mut reader, &mut output).context("decompress bz2 artifact")?;

    Ok(vec![ExtractedFile {
        name: file.name.clone(),
        source: target,
    }])
}

fn extract_zip(
    rendered: &RenderedPackage,
    bytes: &[u8],
    temp_dir: &Path,
) -> Result<Vec<ExtractedFile>> {
    let mut extracted = Vec::new();
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).context("read zip archive")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("read zip entry")?;
        if entry.is_dir() {
            continue;
        }

        let path_str = entry.name();
        let path = PathBuf::from(path_str);

        let mut matched_name = None;
        for file in &rendered.files {
            if Pattern::new(&file.path.to_string_lossy())
                .map(|p| p.matches(path_str))
                .unwrap_or(false)
            {
                matched_name = Some(file.name.clone());
                break;
            }
        }

        let Some(name) = matched_name else {
            continue;
        };

        let target = temp_dir.join(&name);
        let mut output =
            File::create(&target).with_context(|| format!("create {}", target.display()))?;
        io::copy(&mut entry, &mut output).with_context(|| format!("extract {}", path.display()))?;
        extracted.push(ExtractedFile {
            name: name.clone(),
            source: target,
        });
    }

    ensure_all_files_extracted(rendered, &extracted)?;
    Ok(extracted)
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

fn default_lock_path() -> Result<PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("could not determine user config directory"))?;
    Ok(config_dir.join("rsbin").join("rsbin.lock.yml"))
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
        assert_eq!(config.definitions.by_os["linux"].len(), 1);
        assert_eq!(
            config.definitions.by_os["linux"][0].values["rust-triple"],
            DefinitionValueList(vec!["{arch}-unknown-linux-gnu".to_string()])
        );
    }

    #[test]
    fn parses_ordered_definition_values() {
        let config: Config = serde_yaml::from_str(
            r#"
def:
  linux:
    - rust-triple:
        - "{arch}-unknown-linux-gnu"
        - "{arch}-unknown-linux-musl"

packages:
  - name: worktrunk
    repo: https://github.com/max-sixty/worktrunk
    artifact: worktrunk-{rust-triple}.tar.xz
    file:
      - name: wt
        path: worktrunk-{rust-triple}/wt
"#,
        )
        .unwrap();

        assert_eq!(
            config.definitions.by_os["linux"][0].values["rust-triple"],
            DefinitionValueList(vec![
                "{arch}-unknown-linux-gnu".to_string(),
                "{arch}-unknown-linux-musl".to_string()
            ])
        );
    }

    #[test]
    fn renders_ordered_definition_fallback_candidates() {
        let mut values = HashMap::new();
        values.insert(
            "rust-triple".to_string(),
            DefinitionValueList(vec![
                "{arch}-unknown-linux-gnu".to_string(),
                "{arch}-unknown-linux-musl".to_string(),
            ]),
        );
        let mut by_os = HashMap::new();
        by_os.insert(
            current_os_key().unwrap().to_string(),
            vec![DefinitionSet { values }],
        );
        let config = Config {
            definitions: Definitions { by_os },
            packages: Vec::new(),
        };
        let package = Package {
            name: "tool".to_string(),
            repo: "https://github.com/example/tool".to_string(),
            artifact: "tool-{rust-triple}.tar.xz".to_string(),
            file: vec![FileEntry {
                name: "tool".to_string(),
                path: "tool-{rust-triple}/tool".to_string(),
            }],
        };

        let arch = current_arch_aliases().unwrap()[0];
        let candidates = render_package_candidates(&package, &config).unwrap();

        assert_eq!(
            candidates[0].artifact,
            format!("tool-{arch}-unknown-linux-gnu.tar.xz")
        );
        assert_eq!(
            candidates[1].artifact,
            format!("tool-{arch}-unknown-linux-musl.tar.xz")
        );
    }

    #[test]
    fn matches_later_definition_fallback_asset() {
        let mut values = HashMap::new();
        values.insert(
            "rust-triple".to_string(),
            DefinitionValueList(vec![
                "{arch}-unknown-linux-gnu".to_string(),
                "{arch}-unknown-linux-musl".to_string(),
            ]),
        );
        let mut by_os = HashMap::new();
        by_os.insert(
            current_os_key().unwrap().to_string(),
            vec![DefinitionSet { values }],
        );
        let config = Config {
            definitions: Definitions { by_os },
            packages: Vec::new(),
        };
        let package = Package {
            name: "tool".to_string(),
            repo: "https://github.com/example/tool".to_string(),
            artifact: "tool-{rust-triple}.tar.xz".to_string(),
            file: vec![FileEntry {
                name: "tool".to_string(),
                path: "tool-{rust-triple}/tool".to_string(),
            }],
        };
        let arch = current_arch_aliases().unwrap()[0];
        let release = GitHubRelease {
            tag_name: "v1.0.0".to_string(),
            assets: vec![GitHubAsset {
                name: format!("tool-{arch}-unknown-linux-musl.tar.xz"),
                browser_download_url: "https://example.com/tool.tar.xz".to_string(),
            }],
        };

        let matched = match_asset(&package, &config, &release).unwrap();

        assert_eq!(
            matched.rendered.artifact,
            format!("tool-{arch}-unknown-linux-musl.tar.xz")
        );
    }

    #[test]
    fn matches_restic_wildcard_linux_amd64_asset() {
        let mut values = HashMap::new();
        values.insert(
            "ext".to_string(),
            DefinitionValueList(vec![
                "zst".to_string(),
                "tar.gz".to_string(),
                "tar.xz".to_string(),
                "bz2".to_string(),
                "zip".to_string(),
            ]),
        );
        let mut by_os = HashMap::new();
        by_os.insert(
            current_os_key().unwrap().to_string(),
            vec![DefinitionSet { values }],
        );
        let config = Config {
            definitions: Definitions { by_os },
            packages: Vec::new(),
        };
        let package = Package {
            name: "restic".to_string(),
            repo: "https://github.com/restic/restic".to_string(),
            artifact: "restic_*_{os}_{arch}.{ext}".to_string(),
            file: vec![FileEntry {
                name: "restic".to_string(),
                path: "restic_*_{os}_{arch}".to_string(),
            }],
        };
        let release = GitHubRelease {
            tag_name: "v0.18.1".to_string(),
            assets: vec![GitHubAsset {
                name: "restic_0.18.1_linux_amd64.bz2".to_string(),
                browser_download_url: "https://example.com/restic.bz2".to_string(),
            }],
        };

        let matched = match_asset(&package, &config, &release).unwrap();

        assert_eq!(matched.rendered.artifact, "restic_*_linux_amd64.bz2");
        assert_eq!(
            matched.rendered.files[0].path,
            PathBuf::from("restic_*_linux_amd64")
        );
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
    fn missing_lock_file_loads_as_empty() {
        let temp = tempfile::tempdir().unwrap();
        let lock = load_lock_file(&temp.path().join("missing.lock.yml")).unwrap();
        assert!(lock.packages.is_empty());
    }

    #[test]
    fn writes_and_loads_minimal_lock_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rsbin.lock.yml");
        let mut lock = LockFile::default();
        lock.packages
            .insert("codex".to_string(), "rust-v0.125.0".to_string());
        lock.packages.insert("uv".to_string(), "0.11.7".to_string());

        save_lock_file(&path, &lock).unwrap();
        let loaded = load_lock_file(&path).unwrap();

        assert_eq!(loaded, lock);
        let yaml = fs::read_to_string(path).unwrap();
        assert!(yaml.contains("packages:"));
        assert!(yaml.contains("codex: rust-v0.125.0"));
    }

    #[test]
    fn lock_match_skips_unless_forced() {
        let mut lock = LockFile::default();
        lock.packages.insert("uv".to_string(), "0.11.7".to_string());

        assert!(is_locked_latest(&lock, "uv", "0.11.7", false));
        assert!(!is_locked_latest(&lock, "uv", "0.11.7", true));
        assert!(!is_locked_latest(&lock, "uv", "0.11.8", false));
        assert!(!is_locked_latest(&lock, "codex", "0.11.7", false));
    }

    #[test]
    fn successful_install_is_persisted_before_later_failure() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rsbin.lock.yml");
        let mut lock = LockFile::default();

        record_successful_install(&path, &mut lock, "rsbin", "v0.1.6").unwrap();
        let loaded_after_success = load_lock_file(&path).unwrap();

        assert_eq!(
            loaded_after_success
                .packages
                .get("rsbin")
                .map(String::as_str),
            Some("v0.1.6")
        );
        assert!(!loaded_after_success.packages.contains_key("restic"));
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
    fn extracts_tar_xz_files() {
        let mut tar_bytes = Vec::new();
        {
            let encoder = xz2::write::XzEncoder::new(&mut tar_bytes, 6);
            let mut tar = Builder::new(encoder);
            append_file(
                &mut tar,
                "worktrunk-aarch64-unknown-linux-musl/wt",
                b"wt-bin",
            );
            append_file(
                &mut tar,
                "worktrunk-aarch64-unknown-linux-musl/git-wt",
                b"git-wt-bin",
            );
            tar.finish().unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let rendered = RenderedPackage {
            artifact: "worktrunk.tar.xz".to_string(),
            files: vec![
                RenderedFile {
                    name: "wt".to_string(),
                    path: PathBuf::from("worktrunk-aarch64-unknown-linux-musl/wt"),
                },
                RenderedFile {
                    name: "git-wt".to_string(),
                    path: PathBuf::from("worktrunk-aarch64-unknown-linux-musl/git-wt"),
                },
            ],
        };

        let extracted = extract_artifact(&rendered, &tar_bytes, temp.path()).unwrap();
        assert_eq!(extracted.len(), 2);
        assert_eq!(fs::read(temp.path().join("wt")).unwrap(), b"wt-bin");
        assert_eq!(fs::read(temp.path().join("git-wt")).unwrap(), b"git-wt-bin");
    }

    #[test]
    fn tar_gz_validation_fails_when_configured_paths_are_missing() {
        let mut tar_bytes = Vec::new();
        {
            let encoder = GzEncoder::new(&mut tar_bytes, Compression::default());
            let mut tar = Builder::new(encoder);
            append_file(&mut tar, "uv-aarch64-unknown-linux-gnu/uv", b"uv-bin");
            append_file(&mut tar, "uv-aarch64-unknown-linux-gnu/uvx", b"uvx-bin");
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

        let error = extract_artifact(&rendered, &tar_bytes, temp.path()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("artifact missing configured file(s): uv, uvx")
        );
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

    #[test]
    fn extracts_single_bz2_file() {
        use std::io::Write;
        let mut compressed = Vec::new();
        {
            let mut encoder =
                bzip2::write::BzEncoder::new(&mut compressed, bzip2::Compression::default());
            encoder.write_all(b"tool-bin").unwrap();
            encoder.finish().unwrap();
        }
        let temp = tempfile::tempdir().unwrap();
        let rendered = RenderedPackage {
            artifact: "tool.bz2".to_string(),
            files: vec![RenderedFile {
                name: "tool".to_string(),
                path: PathBuf::from("tool"),
            }],
        };

        let extracted = extract_artifact(&rendered, &compressed, temp.path()).unwrap();
        assert_eq!(extracted.len(), 1);
        assert_eq!(fs::read(temp.path().join("tool")).unwrap(), b"tool-bin");
    }

    #[test]
    fn extracts_zip_files() {
        use std::io::Write;
        let mut zip_bytes = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(Cursor::new(&mut zip_bytes));
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("restic_1.2.3.exe", options).unwrap();
            zip.write_all(b"restic-bin").unwrap();
            zip.finish().unwrap();
        }

        let temp = tempfile::tempdir().unwrap();
        let rendered = RenderedPackage {
            artifact: "restic.zip".to_string(),
            files: vec![RenderedFile {
                name: "restic".to_string(),
                path: PathBuf::from("restic*.exe"),
            }],
        };

        let extracted = extract_artifact(&rendered, &zip_bytes, temp.path()).unwrap();
        assert_eq!(extracted.len(), 1);
        assert_eq!(fs::read(temp.path().join("restic")).unwrap(), b"restic-bin");
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
