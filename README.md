# rsbin

`rsbin` is a tiny binary updater for tools published as GitHub release assets.
It reads a YAML config, resolves the current OS/architecture, downloads matching
release artifacts, extracts configured files, and installs them into
`~/.local/bin`.

## Usage

```bash
rsbin update
rsbin update --dry
rsbin update --force uv worktrunk
rsbin update --config /path/to/config.yml codex
```

- `update` updates all configured packages, or only the package names provided.
- `--dry` queries GitHub and validates matching artifacts without installing.
- `--force` reinstalls even when the local lock file says a package is current.
- `--config` overrides the default config path.

Default paths:

- Config: `~/.config/rsbin/config.yml`
- Lock file: `~/.config/rsbin/rsbin.lock.yml`
- Install directory: `~/.local/bin`

## Config

```yaml
def:
  linux:
    - rust-triple:
        - "{arch}-unknown-linux-gnu"
        - "{arch}-unknown-linux-musl"
  windows:
    - rust-triple: "{arch}-pc-windows-msvc"

packages:
  - name: uv
    repo: https://github.com/astral-sh/uv
    artifact: uv-{rust-triple}.tar.gz
    file:
      - name: uv
        path: uv-{rust-triple}/uv
      - name: uvx
        path: uv-{rust-triple}/uvx
```

`def` values can be either a string or an ordered list. Lists are tried in
order, so the example prefers GNU Linux assets and falls back to MUSL assets
when a package only publishes MUSL.

`file[].path` is the path inside the release artifact. `file[].name` is the
installed filename under the install directory.

By default, configured files are copied directly into `~/.local/bin`. For
tools that need adjacent runtime files, set `install: package`:

```yaml
packages:
  - name: pi
    repo: https://github.com/badlogic/pi-mono
    artifact: pi-{os}-{arch}.{ext}
    install: package
    file:
      - name: pi
        path: pi/pi
```

Package installs extract the whole artifact into
`~/.local/bin/packages/<name>` and symlink each configured `file[]` entry into
`~/.local/bin`. The example above matches pi-mono release assets such as
`pi-linux-x64.tar.gz`, installs the package tree under
`~/.local/bin/packages/pi`, and creates `~/.local/bin/pi` as a symlink to the
packaged `pi/pi` binary.

Supported template variables include:

- `{os}`
- `{arch}`
- Values defined under the matching `def.<os>` entry, such as `{rust-triple}`

Supported archive formats:

- `.tar.gz` / `.tgz`
- `.tar.xz` / `.txz`
- single-file `.zst`

## Lock File

After a successful install, `rsbin` records package versions in:

```yaml
packages:
  uv: 0.11.7
  worktrunk: v0.44.0
```

If the latest GitHub release tag matches the lock entry, `rsbin update` skips
the download and install. Use `--force` to reinstall anyway.

## Releases

Pushing a tag such as `v0.1.3` triggers the GitHub Actions release workflow.
The workflow creates a GitHub Release and uploads prebuilt `rsbin` binaries for
the configured Rust Tier 1 targets.
