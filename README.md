# gh-config-cli

[![CI](https://github.com/gitkb/gh-config-cli/actions/workflows/ci.yml/badge.svg)](https://github.com/gitkb/gh-config-cli/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Releases](https://img.shields.io/github/v/release/gitkb/gh-config-cli)](https://github.com/gitkb/gh-config-cli/releases)

A fast, declarative CLI tool to manage GitHub organization configuration as code, written in Rust.

---

## Table of Contents

- [Features](#features)
- [Why use gh-config-cli?](#why-use-gh-config-cli)
- [Supported Platforms](#supported-platforms)
- [Installation](#installation)
- [Usage](#usage)
- [Configuration](#configuration)
- [GitHub Actions Integration](#github-actions-integration)
- [Local Development](#local-development)
- [Security Considerations](#security-considerations)
- [Contributing](#contributing)
- [License](#license)
- [Documentation](#documentation)
- [Getting Help](#getting-help)
---

## Features

- **Declarative YAML config** for your entire GitHub org: repos, teams, members, permissions, webhooks, branch protections.
- **Diff mode**: See what would change before applying.
- **Dry run**: Validate changes without modifying anything.
- **Sync from org**: Generate a config file from your current GitHub org state.
- **GitHub Apps**: Validate versioned manifests and register org-owned Apps through GitHub's browser approval flow.
- **Cross-platform**: Linux, macOS, Windows.
- **Fast**: Built in Rust.
- **Easy CI integration**: Automate validation and updates.
- **No vendor lock-in**: Open source under MIT license.

---

## Why use gh-config-cli?

Managing GitHub orgs manually or with scattered scripts is error-prone. `gh-config-cli` lets you:

- Version control your org structure.
- Review changes via diffs and pull requests.
- Enforce consistent settings across repos.
- Automate org management in CI/CD.
- Avoid surprises from manual changes.

---

## Supported Platforms

- Linux (x86_64, ARM64)
- macOS (Intel & Apple Silicon)
- Windows

---

## Installation

### Using [UBI](https://github.com/houseabsolute/ubi)

```bash
mkdir -p "$HOME/bin"
curl --silent --location https://raw.githubusercontent.com/houseabsolute/ubi/master/bootstrap/bootstrap-ubi.sh | sh
"$HOME/bin/ubi" --project gitkb/gh-config-cli --exe gh-config --in "$HOME/bin"
```

### From GitHub Releases

Download the latest binary for your platform from [Releases](https://github.com/gitkb/gh-config-cli/releases), make it executable, and put it in your PATH.

### With Cargo

```bash
cargo install --locked --git https://github.com/gitkb/gh-config-cli
```

### From Source

Requires Rust stable:

```bash
git clone https://github.com/gitkb/gh-config-cli.git
cd gh-config-cli
cargo build --release
./target/release/gh-config --help
```

---

## Usage

### Authentication

- Use a **GitHub Personal Access Token (PAT)** with appropriate permissions (repo admin, org admin).
- Pass via `--token` flag or `GITHUB_TOKEN` environment variable.

### Commands

- **Diff local config with GitHub:**

```bash
gh-config --token <your-pat> diff config.yaml
```

- **Apply config (sync):**

```bash
gh-config --token <your-pat> sync config.yaml
```

- **Dry run (validate):**

```bash
gh-config --token <your-pat> sync config.yaml --dry-run
```

- **Generate config from org:**

```bash
gh-config --token <your-pat> sync-from-org config.yaml --org gitkb
```

- **Register an organization-owned GitHub App:**

```bash
gh-config app validate --manifest apps/rabbit.json
gh-config app create --org gitkb --manifest apps/rabbit.json \
  --credentials-dir "$HOME/rabbit-app-credentials"
```

Open the printed loopback URL in a browser signed into GitHub with permission
to create Apps under the organization. Confirm the organization on GitHub's
creation page. The CLI rejects personal-account fallback, validates the
callback state, exchanges the one-time code and saves `credentials.json`,
`private-key.pem` and nonsecret `app.json`. No PAT is required or sent by these
App commands. The manifest's `redirect_url` is replaced with the local callback.

Credential output requires Unix, a new directory outside Git and an existing
parent. The directory is mode 0700; files are mode 0600 and never overwritten.
Move the secret files into your team's secret manager. Do not commit them or
upload them as CI artifacts. Keep only the manifest in your org-config repo.

App registration is a browser-approved operation; an owner PAT in Actions does
not bypass GitHub's registration page. Install the resulting App separately
using the printed installation URL. Registration expires locally after 15
minutes. See [GitHub's manifest flow](https://docs.github.com/en/apps/sharing-github-apps/registering-a-github-app-from-a-manifest).

### Example diff output

```yaml
- allow_merge_commit: true
+ allow_merge_commit: false
```

---

## Releases

Releases use the package version declared in `Cargo.toml`. After main's checks
pass, a new version is tagged and built locally in this repository's workflows;
no private reusable workflow is required. Release builds cover Linux x86_64
and arm64, macOS Intel and Apple Silicon, and Windows x86_64. The previous
release remains available for FreeBSD, Windows arm64 and 32-bit platforms.

## Configuration

gh-config uses a declarative YAML schema. Repo `settings` take the fields of GitHub's repository endpoints in gh-config's API mapping (`src/github_api_mapping_generated.rs`): endpoints under `/repos/{owner}/{repo}` that need nothing but the repo's name. A key the mapping sends to the org or a user is refused, so a misplaced key can't change either.

Every command refuses a config with a key it doesn't know before it contacts GitHub, naming the file, where the key is and the keys allowed there:

```text
config.yaml: repos[3].settings (repo old-tool): unknown setting `archive`. Did you mean `archived`? ...
config.yaml: repos[3].settings (repo old-tool): `default_repository_permission` is not a repository setting: it would be sent to PATCH /orgs/{org}, which is not the repo. ...
config.yaml: repos[3].archived: unknown field `archived`, expected one of `name`, `settings`, `visibility`, `use_default_webhook`, `webhook`, `branch_protections`, `required_checks`
```

**Example `config.yaml`:**
```yaml
org: gitkb
repos:
  - name: harmony
    settings:
      allow_merge_commit: false
      allow_squash_merge: true
      allow_rebase_merge: true
      # Any other GitHub repo setting in the API mapping
teams:
  - name: core-team
    members:
      - adminuser
      - otheruser
users:
  - login: adminuser
    role: admin
assignments:
  - repo: harmony
    team: core-team
    permission: push
```

### Archiving repos

`archived` is a repo setting like any other:

```yaml
repos:
  - name: old-tool
    settings:
      archived: true    # archive it; `archived: false` unarchives it
```

- `sync-from-org` writes `archived: true` for repos that are archived on GitHub, and nothing for the others.
- `sync` unarchives a repo before changing anything else on it, and archives repos after every other change, because GitHub makes an archived repo read-only.
- A repo that is archived on GitHub and has no `archived: false` keeps its settings and webhook: `sync` leaves them as they are and logs one line saying so (shown with `RUST_LOG=info`, like its other per-change lines). Leaving `archived` out never unarchives a repo.
- Team access (`assignments`) is sent for archived repos too, so `sync` applies what `diff` shows. GitHub's docs say an archived repo's permissions are read-only; if GitHub refuses a change, `sync` stops with GitHub's message. Unarchive the repo first (`archived: false`) or change the assignment to match GitHub.
- `diff` shows archiving and unarchiving as a change to the `archived` line. A repo archived on GitHub whose config leaves `archived` out shows `- archived: true` alone: the config doesn't record it yet (sync-from-org would add it), and `sync` leaves the repo archived.

### Creating repos

`sync` creates a repo the config lists and GitHub doesn't have, with the config's `visibility`, then applies its settings, rulesets and team access. A missing repo without `visibility` is refused rather than guessed. `sync --dry-run` prints `Would create` and skips the rest of that repo, since there is nothing to read yet.

### Auto-merge and Actions access

```yaml
repos:
  - name: service
    visibility: private
    settings:
      allow_auto_merge: true        # PRs may merge themselves once their required checks pass
      access_level: organization    # other private repos in the org may call this repo's reusable workflows
```

- Like `archived`, `sync-from-org` writes `allow_auto_merge: true` and an `access_level` other than `none` only when GitHub has them; `diff` treats `allow_auto_merge: false` and `access_level: none` in a config as GitHub's defaults, not differences.
- `access_level` (GitHub's `/actions/permissions/access`) exists only for private repos. `sync` reads it only for repos whose config names it, and writes it only when it differs. `sync-from-org` and `diff` read it for every private repo: one extra request each.

### Required checks

```yaml
repos:
  - name: service
    required_checks: [Format, Unit Tests, "Clippy & Docs"]
```

- `sync` keeps them in the repo's own ruleset named `Require CI`: active, on the default branch (`~DEFAULT_BRANCH`), not strict (a PR needn't be up to date with the branch), one `required_status_checks` rule. It creates the ruleset if the repo has none by that name and rewrites it when its checks (in any order) or any of those properties differ. An existing `Require CI` ruleset is adopted by name.
- gh-config owns every rule in `Require CI`: if it holds any other rule, `sync` stops with an error naming the repo and the rule instead of dropping it. Keep other rules in other rulesets.
- A repo with no `required_checks` is left alone: `sync` doesn't read or delete its rulesets. To stop requiring checks, delete the ruleset on GitHub and the key.
- `sync-from-org` records the checks of each repo's `Require CI` ruleset, in the file's order when the file names the same ones; `diff` compares them as a set. It costs one rulesets request per repo, plus one when `Require CI` exists. A ruleset that can't be read keeps what the file has, and `diff` names the repo and exits 1. Properties other than the checks aren't recorded, so `diff` shows no change when only those differ; `sync` still corrects them.

### Webhooks

A repo gets a webhook only if the config says so: its own `webhook`, or `use_default_webhook: true` for the org's `default_webhook`. Being listed connects nothing, and `sync` never deletes a webhook.

```yaml
default_webhook:
  url: https://example.com/hook
  content_type: json
  events: [push, pull_request]
repos:
  - name: on-the-feed
    use_default_webhook: true
  - name: not-on-the-feed
```

- `sync --dry-run` prints every webhook it would create or update, by repo, whatever `RUST_LOG` says. Each line shows the URL's host, not the whole URL.
- `sync-from-org` records what GitHub has. It keeps the file's `default_webhook` and sets `use_default_webhook: true` only on repos that have that webhook, so regenerating adds no repo to it. A config holds one webhook per repo, so a repo with several is recorded with its `webhook` from the file if it has that one, else the default, else its first. If the file has no default, the first webhook found becomes it. If a repo's webhooks can't be read, as with a token that can't read hooks, it stays as the file has it and the command names those repos.
- `diff` never calls webhooks it couldn't read a match: it names those repos and exits 1.
- A repo opted in but not yet synced has no webhook on GitHub, so regenerating clears its `use_default_webhook`. The same goes for an archived repo, which `sync` skips: run `sync` first, and don't opt in archived repos.
- To take a repo off the default, delete its webhook on GitHub as well as the key: while the webhook exists, `diff` reports it and `sync-from-org` opts the repo back in.
- Changing `default_webhook.url` adds a second webhook on each opted-in repo, and the old one keeps delivering until you delete it.
- A repo with both `use_default_webhook: true` and its own `webhook`, or with no `default_webhook` to use, is refused.

**Upgrading from 1.3.0:** a config with `default_webhook` and no `use_default_webhook` keeps working but connects nothing new. Add the key to each repo that should have the default, or run `sync-from-org` to mark the repos that already have it. That marks every repo holding the webhook, so delete any that shouldn't have it first.

### Defaults and Merging

You can provide a `defaults.config.yaml` file to specify org-wide or policy defaults. All fields are optional. The main config takes precedence over defaults.

**Example `defaults.config.yaml`:**
```yaml
repos:
  - settings:
      allow_merge_commit: true
default_webhook:
  url: https://discord.com/api/webhooks/...
  content_type: json
  events: [push, pull_request]
default_branch_protections:
  - pattern: main
    enforce_admins: true
    allow_deletions: false
    allow_force_pushes: false
```

**Merging rules:**
- If a setting exists in both main config and defaults, main config wins.
- If a setting exists only in defaults, it is used unless overridden.

**Supports:**
- All GitHub API-manageable settings (see [GitHub REST API docs](https://docs.github.com/en/rest?apiVersion=2022-11-28))
- An org-wide default webhook for the repos that opt in
- Repo settings (merge options, visibility, webhooks, etc.)
- Teams and members
- User roles
- Team-repo permissions
- Branch protection rules

---

## GitHub Actions Integration

Automate validation and updates.

### Validate on PRs, apply on main branch push

```yaml
permissions:
  contents: write
  pull-requests: write

jobs:
  validate-config:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install UBI
        run: |
          mkdir -p "$HOME/bin"
          curl --silent --location https://raw.githubusercontent.com/houseabsolute/ubi/master/bootstrap/bootstrap-ubi.sh | sh
      - name: Install gh-config
        run: |
          "$HOME/bin/ubi" --project gitkb/gh-config-cli --exe gh-config --in "$HOME/bin"
      - name: Validate config
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: |
          $HOME/bin/gh-config sync config.yaml --dry-run

  apply-config:
    if: github.ref == 'refs/heads/main' && github.event_name == 'push'
    runs-on: ubuntu-latest
    needs: validate-config
    steps:
      - uses: actions/checkout@v4
      - name: Install UBI
        run: |
          mkdir -p "$HOME/bin"
          curl --silent --location https://raw.githubusercontent.com/houseabsolute/ubi/master/bootstrap/bootstrap-ubi.sh | sh
      - name: Install gh-config
        run: |
          "$HOME/bin/ubi" --project gitkb/gh-config-cli --exe gh-config --in "$HOME/bin"
      - name: Apply config
        env:
          GITHUB_TOKEN: ${{ secrets.GH_PAT }}
        run: |
          $HOME/bin/gh-config sync config.yaml
```

---

## Local Development

- Requires Rust stable (`rustup.rs`)
- Clone repo, then:

```bash
cargo build --release
./target/release/gh-config --help
```

- Use `make` targets for common tasks:

```bash
make build
make diff CONFIG_FILE=config.yaml
make sync CONFIG_FILE=config.yaml
make sync-from-github CONFIG_FILE=config.yaml GITHUB_ORG=gitkb
```

---

## Security Considerations

- Use fine-grained PATs with least privilege.
- Store tokens in environment variables or GitHub Secrets.
- Never commit secrets to version control.
- Review diffs before applying changes.

---

## Contributing
- Issues and PRs welcome!
- Please file bugs, suggest features, or contribute code.
- See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.
- Please read and follow our [Code of Conduct](CODE_OF_CONDUCT.md).


---

## License

MIT License. See [LICENSE](LICENSE) for details.

## Documentation

- [Creating a GitHub Personal Access Token (PAT)](docs/pat-setup.md)
- [Usage Guide](docs/usage.md)
- [GitHub Actions CI Examples](docs/ci-examples.md)
- [Development Guide](docs/development.md)
- [Architecture Overview](docs/architecture.md)
- [FAQ](docs/faq.md)
- [Changelog](docs/changelog.md)

More detailed guides and examples can be found in the `docs/` directory.

## Getting Help

- See the [FAQ](docs/faq.md) for common questions.
- Open an issue on [GitHub](https://github.com/gitkb/gh-config-cli/issues).
