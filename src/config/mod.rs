use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Deserializer};

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(rename = "def")]
    pub definitions: Definitions,
    pub packages: Vec<Package>,
}

#[derive(Debug, Deserialize)]
pub struct Definitions {
    #[serde(flatten)]
    pub by_os: HashMap<String, Vec<DefinitionSet>>,
}

#[derive(Debug, Deserialize)]
pub struct DefinitionSet {
    #[serde(flatten)]
    pub values: HashMap<String, DefinitionValueList>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionValueList(pub Vec<String>);

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
pub struct Package {
    pub name: String,
    pub repo: String,
    pub artifact: String,
    pub file: Vec<FileEntry>,
}

#[derive(Debug, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedPackage {
    pub artifact: String,
    pub files: Vec<RenderedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedFile {
    pub name: String,
    pub path: PathBuf,
}

pub fn load_config(path: &std::path::Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read config from {}", path.display()))?;
    let config: Config = serde_yaml::from_str(&content)
        .with_context(|| format!("parse config from {}", path.display()))?;
    Ok(config)
}

pub fn validate_config(config: &Config) -> Result<()> {
    if config.packages.is_empty() {
        bail!("config has no packages");
    }

    let mut names = BTreeSet::new();
    for package in &config.packages {
        if package.name.trim().is_empty() {
            bail!("package has empty name");
        }
        if !names.insert(&package.name) {
            bail!("duplicate package name: {}", package.name);
        }
        if package.file.is_empty() {
            bail!("package {} has no file entries", package.name);
        }
    }

    Ok(())
}

pub fn render_package_candidates(
    package: &Package,
    config: &Config,
) -> Result<Vec<RenderedPackage>> {
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

pub fn expand_template(input: &str, vars: &BTreeMap<String, String>) -> Result<String> {
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

use anyhow::Context;
