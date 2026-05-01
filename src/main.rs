mod archive;
mod config;
mod install;
mod select;
mod source;

use std::collections::BTreeSet;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use config::{Config, InstallMode, Package, RenderedPackage};
use source::github::GitHubDownloadSource;
use source::{DownloadSource, ResolvedArtifact};

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
    let config_path = config_path.unwrap_or(install::default_config_path()?);
    let config = config::load_config(&config_path)?;
    config::validate_config(&config)?;

    let requested = requested_packages(&package_names);
    let packages = select_packages(&config, requested.as_ref())?;
    let source = GitHubDownloadSource::new()?;
    let install_dir = install::default_install_dir()?;
    let lock_path = install::default_lock_path()?;
    let mut lock = install::load_lock_file(&lock_path)?;

    for package in packages {
        let release = source
            .latest_release(&package.repo)
            .await
            .with_context(|| format!("fetch latest release for {}", package.name))?;

        if install::is_locked_latest(&lock, &package.name, &release.tag_name, force) {
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
        print_install_plan(package, &matched, &install_dir);

        if dry {
            validate_package_archive(&source, &matched)
                .await
                .with_context(|| format!("validate archive for {}", package.name))?;
            continue;
        }

        install_package(&source, package, &matched, &install_dir)
            .await
            .with_context(|| format!("install {}", package.name))?;
        install::record_successful_install(
            &lock_path,
            &mut lock,
            &package.name,
            &matched.tag_name,
        )?;
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

fn match_asset(
    package: &Package,
    config: &Config,
    release: &source::Release,
) -> Result<MatchedAsset> {
    for rendered in config::render_package_candidates(package, config)? {
        if let Some(asset) = release.assets.iter().find(|asset| {
            glob::Pattern::new(&rendered.artifact)
                .map(|p| p.matches(&asset.name))
                .unwrap_or(false)
        }) {
            return Ok(MatchedAsset {
                rendered,
                download_url: asset.download_url.clone(),
                tag_name: release.tag_name.clone(),
                asset_name: asset.name.clone(),
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

#[derive(Debug)]
struct MatchedAsset {
    rendered: RenderedPackage,
    download_url: String,
    tag_name: String,
    /// Actual matched asset name (not the glob pattern)
    asset_name: String,
}

async fn install_package(
    source: &impl DownloadSource,
    package: &Package,
    matched: &MatchedAsset,
    install_dir: &std::path::Path,
) -> Result<()> {
    let artifact = ResolvedArtifact {
        download_url: matched.download_url.clone(),
        artifact_name: matched.rendered.artifact.clone(),
        tag_name: matched.tag_name.clone(),
    };

    let bytes = source.download(&artifact).await?;
    let extractor = archive::select_extractor(&matched.asset_name)?;
    let entries = extractor
        .entries(&matched.asset_name, &bytes)
        .with_context(|| format!("extract {}", matched.asset_name))?;

    std::fs::create_dir_all(install_dir)
        .with_context(|| format!("create install dir {}", install_dir.display()))?;

    match matched.rendered.install {
        InstallMode::Bin => {
            let selected = select::select_extracted_files(&matched.rendered.files, &entries)?;
            for file in selected {
                install::install_bytes(&file.contents, &install_dir.join(&file.name))
                    .with_context(|| format!("install file {} for {}", file.name, package.name))?;
            }
        }
        InstallMode::Package => {
            let selected = select::select_package_install(&matched.rendered.files, &entries)?;
            install::install_package_tree(&package.name, &selected, install_dir)?;
        }
    }

    Ok(())
}

async fn validate_package_archive(
    source: &impl DownloadSource,
    matched: &MatchedAsset,
) -> Result<()> {
    let artifact = ResolvedArtifact {
        download_url: matched.download_url.clone(),
        artifact_name: matched.rendered.artifact.clone(),
        tag_name: matched.tag_name.clone(),
    };

    let bytes = source.download(&artifact).await?;
    let extractor = archive::select_extractor(&matched.asset_name)?;
    let entries = extractor
        .entries(&matched.asset_name, &bytes)
        .with_context(|| format!("validate {}", matched.asset_name))?;

    match matched.rendered.install {
        InstallMode::Bin => {
            select::select_extracted_files(&matched.rendered.files, &entries)?;
        }
        InstallMode::Package => {
            select::select_package_install(&matched.rendered.files, &entries)?;
        }
    }

    Ok(())
}

fn print_install_plan(package: &Package, matched: &MatchedAsset, install_dir: &std::path::Path) {
    match matched.rendered.install {
        InstallMode::Bin => {
            for file in &matched.rendered.files {
                println!(
                    "  {} -> {}",
                    file.path.display(),
                    install_dir.join(&file.name).display()
                );
            }
        }
        InstallMode::Package => {
            let package_dir = install::package_install_dir(install_dir, &package.name);
            println!("  package -> {}", package_dir.display());
            for file in &matched.rendered.files {
                println!(
                    "  {} -> {}",
                    install_dir.join(&file.name).display(),
                    package_dir.join(&file.path).display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use install::LockFile;
    use tar::Builder;

    #[test]
    fn parses_sample_config() {
        let yaml = r#"
def:
  linux:
    - arch:
        - amd64
        - arm64
packages:
  - name: example
    repo: https://github.com/example/example
    artifact: "example-{arch}.tar.gz"
    file:
      - name: example
        path: "example-{arch}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.packages.len(), 1);
        assert_eq!(config.packages[0].name, "example");
        assert_eq!(config.packages[0].install, InstallMode::Bin);
    }

    #[test]
    fn parses_package_install_mode() {
        let yaml = r#"
def:
  linux:
    - arch: amd64
packages:
  - name: rtk
    repo: https://github.com/rtk-ai/rtk
    artifact: "rtk-{arch}.tar.gz"
    install: package
    file:
      - name: rtk
        path: rtk
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.packages[0].install, InstallMode::Package);
    }

    #[test]
    fn parses_ordered_definition_values() {
        let yaml = r#"
def:
  linux:
    - arch:
        - amd64
        - arm64
packages:
  - name: example
    repo: https://github.com/example/example
    artifact: "example-{arch}.tar.gz"
    file:
      - name: example
        path: "example-{arch}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let definitions = config.definitions.by_os.get("linux").unwrap();
        assert_eq!(definitions.len(), 1);
        assert_eq!(
            definitions[0].values.get("arch").unwrap().0,
            vec!["amd64", "arm64"]
        );
    }

    #[test]
    fn renders_ordered_definition_fallback_candidates() {
        let yaml = r#"
def:
  linux:
    - arch:
        - amd64
        - arm64
packages:
  - name: example
    repo: https://github.com/example/example
    artifact: "example-{arch}.tar.gz"
    install: package
    file:
      - name: example
        path: "example-{arch}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let package = &config.packages[0];
        let candidates = config::render_package_candidates(package, &config).unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].artifact, "example-amd64.tar.gz");
        assert_eq!(candidates[0].install, InstallMode::Package);
        assert_eq!(candidates[1].artifact, "example-arm64.tar.gz");
    }

    #[test]
    fn expands_templates() {
        let mut vars = std::collections::BTreeMap::new();
        vars.insert("os".to_string(), "linux".to_string());
        vars.insert("arch".to_string(), "amd64".to_string());

        let result = config::expand_template("example-{os}-{arch}", &vars).unwrap();
        assert_eq!(result, "example-linux-amd64");
    }

    #[test]
    fn parses_github_urls() {
        assert_eq!(
            source::github::parse_github_repo("https://github.com/owner/repo").unwrap(),
            ("owner".to_string(), "repo".to_string())
        );
        assert_eq!(
            source::github::parse_github_repo("github.com/owner/repo").unwrap(),
            ("owner".to_string(), "repo".to_string())
        );
        assert_eq!(
            source::github::parse_github_repo("https://github.com/owner/repo.git").unwrap(),
            ("owner".to_string(), "repo".to_string())
        );
    }

    #[test]
    fn missing_lock_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.yml");
        let lock = install::load_lock_file(&path).unwrap();
        assert!(lock.packages.is_empty());
    }

    #[test]
    fn writes_and_loads_minimal_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock.yml");

        let mut lock = LockFile::default();
        lock.packages
            .insert("test".to_string(), "v1.0.0".to_string());
        install::save_lock_file(&path, &lock).unwrap();

        let loaded = install::load_lock_file(&path).unwrap();
        assert_eq!(loaded, lock);
    }

    #[test]
    fn lock_match_skips_unless_forced() {
        let mut lock = LockFile::default();
        lock.packages
            .insert("test".to_string(), "v1.0.0".to_string());

        assert!(install::is_locked_latest(&lock, "test", "v1.0.0", false));
        assert!(!install::is_locked_latest(&lock, "test", "v1.0.0", true));
        assert!(!install::is_locked_latest(&lock, "test", "v2.0.0", false));
    }

    #[test]
    fn successful_install_is_persisted_before_later_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.lock.yml");

        let mut lock = LockFile::default();
        install::record_successful_install(&path, &mut lock, "pkg1", "v1.0.0").unwrap();
        assert_eq!(lock.packages.get("pkg1"), Some(&"v1.0.0".to_string()));

        let loaded = install::load_lock_file(&path).unwrap();
        assert_eq!(loaded.packages.get("pkg1"), Some(&"v1.0.0".to_string()));
    }

    #[test]
    fn rejects_zst_with_multiple_files() {
        let yaml = r#"
def:
  linux:
    - arch:
        - amd64
packages:
  - name: example
    repo: https://github.com/example/example
    artifact: "example-{arch}.zst"
    file:
      - name: example
        path: "example-{arch}"
      - name: extra
        path: "extra-{arch}"
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        let package = &config.packages[0];
        let candidates = config::render_package_candidates(package, &config).unwrap();
        let rendered = &candidates[0];

        let bytes = b"fake zst content";
        let result = archive::select_extractor(&rendered.artifact)
            .unwrap()
            .entries(&rendered.artifact, bytes);

        assert!(result.is_err());
    }

    fn append_file(builder: &mut Builder<Vec<u8>>, name: &str, content: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o755);
        header.set_entry_type(tar::EntryType::Regular);
        builder.append_data(&mut header, name, content).unwrap();
    }

    #[test]
    fn extracts_tar_gz_files() {
        let mut tar_builder = Builder::new(Vec::new());
        append_file(&mut tar_builder, "example-amd64", b"binary content");
        tar_builder.finish().unwrap();
        let tar_data = tar_builder.into_inner().unwrap();

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let gz_data = encoder.finish().unwrap();

        let extractor = archive::select_extractor("example.tar.gz").unwrap();
        let entries = extractor.entries("example.tar.gz", &gz_data).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].archive_path, "example-amd64");
        assert_eq!(entries[0].contents, b"binary content");
        assert_eq!(entries[0].mode, Some(0o755));
    }

    #[test]
    fn extracts_tar_xz_files() {
        let mut tar_builder = Builder::new(Vec::new());
        append_file(&mut tar_builder, "example-amd64", b"binary content");
        tar_builder.finish().unwrap();
        let tar_data = tar_builder.into_inner().unwrap();

        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        std::io::Write::write_all(&mut encoder, &tar_data).unwrap();
        let xz_data = encoder.finish().unwrap();

        let extractor = archive::select_extractor("example.tar.xz").unwrap();
        let entries = extractor.entries("example.tar.xz", &xz_data).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].archive_path, "example-amd64");
        assert_eq!(entries[0].contents, b"binary content");
        assert_eq!(entries[0].mode, Some(0o755));
    }

    #[test]
    fn extracts_single_zst_file() {
        let content = b"binary content";
        let mut encoder = zstd::Encoder::new(Vec::new(), 3).unwrap();
        std::io::Write::write_all(&mut encoder, content).unwrap();
        let zst_data = encoder.finish().unwrap();

        let extractor = archive::select_extractor("example.zst").unwrap();
        let entries = extractor.entries("example.zst", &zst_data).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].archive_path, "example");
        assert_eq!(entries[0].contents, b"binary content");
    }

    #[test]
    fn extracts_single_bz2_file() {
        let content = b"binary content";
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        std::io::Write::write_all(&mut encoder, content).unwrap();
        let bz2_data = encoder.finish().unwrap();

        let extractor = archive::select_extractor("example.bz2").unwrap();
        let entries = extractor.entries("example.bz2", &bz2_data).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].archive_path, "example");
        assert_eq!(entries[0].contents, b"binary content");
    }

    #[test]
    fn extracts_zip_files() {
        let mut zip_data = Vec::new();
        {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_data));
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zip.start_file("example-amd64", options).unwrap();
            std::io::Write::write_all(&mut zip, b"binary content").unwrap();
            zip.finish().unwrap();
        }

        let extractor = archive::select_extractor("example.zip").unwrap();
        let entries = extractor.entries("example.zip", &zip_data).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].archive_path, "example-amd64");
        assert_eq!(entries[0].contents, b"binary content");
    }

    #[test]
    fn select_matches_exact_path() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "example".to_string(),
            path: std::path::PathBuf::from("example-amd64"),
        }];

        let entries = vec![ArchiveEntry {
            archive_path: "example-amd64".to_string(),
            contents: b"binary content".to_vec(),
            mode: None,
        }];

        let selected = select::select_extracted_files(&configured, &entries).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "example");
        assert_eq!(selected[0].contents, b"binary content");
    }

    #[test]
    fn select_matches_glob_pattern() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "example".to_string(),
            path: std::path::PathBuf::from("example-*"),
        }];

        let entries = vec![ArchiveEntry {
            archive_path: "example-amd64".to_string(),
            contents: b"binary content".to_vec(),
            mode: None,
        }];

        let selected = select::select_extracted_files(&configured, &entries).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "example");
    }

    #[test]
    fn select_fails_on_missing_configured_file() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "example".to_string(),
            path: std::path::PathBuf::from("missing-file"),
        }];

        let entries = vec![ArchiveEntry {
            archive_path: "example-amd64".to_string(),
            contents: b"binary content".to_vec(),
            mode: None,
        }];

        let result = select::select_extracted_files(&configured, &entries);
        assert!(result.is_err());
    }

    #[test]
    fn select_ignores_unconfigured_archive_entries() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "example".to_string(),
            path: std::path::PathBuf::from("example-amd64"),
        }];

        let entries = vec![
            ArchiveEntry {
                archive_path: "example-amd64".to_string(),
                contents: b"binary content".to_vec(),
                mode: None,
            },
            ArchiveEntry {
                archive_path: "README.md".to_string(),
                contents: b"readme".to_vec(),
                mode: None,
            },
        ];

        let selected = select::select_extracted_files(&configured, &entries).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "example");
    }

    #[test]
    fn select_package_install_keeps_tree_and_links_configured_file() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "rtk".to_string(),
            path: std::path::PathBuf::from("rtk"),
        }];

        let entries = vec![
            ArchiveEntry {
                archive_path: "rtk".to_string(),
                contents: b"binary content".to_vec(),
                mode: Some(0o755),
            },
            ArchiveEntry {
                archive_path: "theme/dark.json".to_string(),
                contents: b"{}".to_vec(),
                mode: Some(0o644),
            },
        ];

        let selected = select::select_package_install(&configured, &entries).unwrap();

        assert_eq!(selected.files.len(), 2);
        assert_eq!(
            selected.files[0].relative_path,
            std::path::PathBuf::from("rtk")
        );
        assert_eq!(selected.links.len(), 1);
        assert_eq!(selected.links[0].name, std::path::PathBuf::from("rtk"));
        assert_eq!(selected.links[0].target, std::path::PathBuf::from("rtk"));
    }

    #[test]
    fn select_package_install_fails_on_missing_link_target() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "rtk".to_string(),
            path: std::path::PathBuf::from("missing"),
        }];

        let entries = vec![ArchiveEntry {
            archive_path: "rtk".to_string(),
            contents: b"binary content".to_vec(),
            mode: Some(0o755),
        }];

        let result = select::select_package_install(&configured, &entries);
        assert!(result.is_err());
    }

    #[test]
    fn select_package_install_rejects_unsafe_archive_path() {
        use archive::ArchiveEntry;
        use config::RenderedFile;

        let configured = vec![RenderedFile {
            name: "rtk".to_string(),
            path: std::path::PathBuf::from("rtk"),
        }];

        let entries = vec![
            ArchiveEntry {
                archive_path: "rtk".to_string(),
                contents: b"binary content".to_vec(),
                mode: Some(0o755),
            },
            ArchiveEntry {
                archive_path: "../outside".to_string(),
                contents: b"bad".to_vec(),
                mode: Some(0o644),
            },
        ];

        let result = select::select_package_install(&configured, &entries);
        assert!(result.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn install_package_tree_writes_files_and_symlinks_bin() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let selected = select::SelectedPackage {
            files: vec![
                select::SelectedPackageFile {
                    relative_path: std::path::PathBuf::from("rtk"),
                    contents: b"binary content".to_vec(),
                    mode: Some(0o644),
                },
                select::SelectedPackageFile {
                    relative_path: std::path::PathBuf::from("theme/dark.json"),
                    contents: b"{}".to_vec(),
                    mode: Some(0o644),
                },
            ],
            links: vec![select::SelectedPackageLink {
                name: std::path::PathBuf::from("rtk"),
                target: std::path::PathBuf::from("rtk"),
            }],
        };

        install::install_package_tree("rtk", &selected, dir.path()).unwrap();

        let package_file = dir.path().join("packages").join("rtk").join("rtk");
        let link_path = dir.path().join("rtk");

        assert_eq!(std::fs::read(&package_file).unwrap(), b"binary content");
        assert!(
            std::fs::symlink_metadata(&link_path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_link(&link_path).unwrap(), package_file);
        assert_ne!(
            std::fs::metadata(&package_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}
