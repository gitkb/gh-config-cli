# gh-config-cli Usage Guide

This guide provides detailed instructions and examples for using `gh-config-cli` to manage your GitHub organization.

Organization sync reads current roles and team permissions before applying
changes, so matching values cause no writes. All organization API calls are
serialized, with at least one second between writes. Rate-limited responses
are retried up to three times using `Retry-After`, the primary rate-limit reset
time, or waits of 60, 120 and 240 seconds when neither is available. Other
authorization failures stop immediately; exhausted retries return an error.

---

## Available Commands

| Command                                 | Description                                                                                  |
|------------------------------------------|----------------------------------------------------------------------------------------------|
| `diff <config.yaml>`                     | Compare your local config file with the current GitHub org state and show differences.       |
| `sync <config.yaml>`                     | Apply your local config to GitHub, creating/updating repos, teams, users, permissions, etc.  |
| `sync <config.yaml> --dry-run`           | Validate your config without making any changes (dry run/preview mode).                      |
| `sync-from-org <config.yaml> [--org <org>]` | Export your current GitHub org state into a config file.                                 |
| `--help`                                 | Show all available options and commands.                                                     |

All commands accept `--token <your-pat>` or the `GITHUB_TOKEN` environment variable for authentication.

---

## Authentication

You can provide your GitHub Personal Access Token (PAT) in two ways:

- As a CLI argument: `--token <your-pat>`
- As an environment variable: `GITHUB_TOKEN=<your-pat>`

---

## Configuration Schema

The configuration file is written in YAML and describes your organization, repositories, teams, users, assignments, and default settings. Below is the schema with all top-level keys and their meanings.

```yaml
org: gitkb                # (string) Name of the GitHub organization

repos:                            # (list) Repository configurations
  - name: my-repo                 # (string) Repository name
    settings:                     # (map) GitHub repo settings from the API mapping (see below)
      allow_merge_commit: false   # (bool) Example setting
      allow_squash_merge: true
      allow_rebase_merge: true
      # ...any supported GitHub repo setting
      allow_auto_merge: true      # (bool) Let PRs merge themselves once required checks pass
      access_level: organization  # (string, private repos) Who may call this repo's workflows: none, user, organization, enterprise
    visibility: public            # (string, optional) "public" or "private"; required for sync to create a missing repo
    required_checks:              # (list, optional) Checks a PR to the default branch must pass (ruleset `Require CI`)
      - Unit Tests
    webhook:                      # (object, optional) This repo's own webhook
      url: "http://example.com"   # (string) Webhook endpoint URL
      content_type: "json"        # (string) Payload content type
      events:                     # (list) Events that trigger the webhook
        - push
        - pull_request
    branch_protections:           # (list, optional) Branch protection rules
      - pattern: main             # (string) Branch name or glob pattern
        enforce_admins: true      # (bool)
        allow_deletions: false    # (bool)
        allow_force_pushes: false # (bool)
  - name: other-repo
    use_default_webhook: true     # (bool, optional) Give this repo default_webhook; a repo can't also have its own webhook

teams:                            # (list) Team configurations
  - name: core-team               # (string) Team name
    members:                      # (list) Usernames belonging to the team
      - alice
      - bob

users:                            # (list) User configurations
  - login: alice                  # (string) GitHub username
    role: admin                   # (string) Role in the org ("admin", "member", etc.)

assignments:                      # (list) Team-to-repo permission assignments
  - repo: my-repo                 # (string) Repository name
    team: core-team               # (string) Team name
    permission: admin             # (string) Permission ("admin", "write", "read")

default_webhook:                  # (object, optional) Webhook of repos with use_default_webhook
  url: "http://default.com"
  content_type: "json"
  events:
    - push

default_branch_protections:       # (list, optional) Default branch protection rules
  - pattern: main
    enforce_admins: true
    allow_deletions: false
    allow_force_pushes: false
```

### Notes

- All fields marked as "optional" can be omitted.
- The `settings` map under each repo takes the repository fields in gh-config's API mapping (see [Extensible Schema](#extensible-schema-and-advanced-usage)).
- Any other key is an error: every command refuses the config before contacting GitHub and names the file, where the key is (e.g. `repos[3].archived`) and the keys allowed there.

---

## Commands and Examples

### Show Diff

Compare your local config file with the current GitHub org state.

```bash
gh-config --token <your-pat> diff config.yaml
# or
GITHUB_TOKEN=<your-pat> gh-config diff config.yaml
```

---

### Apply Changes (Sync)

Apply your local config to GitHub, creating/updating repos, teams, permissions, etc.

```bash
gh-config --token <your-pat> sync config.yaml
# or
GITHUB_TOKEN=<your-pat> gh-config sync config.yaml
```

---

### Dry Run (Validation)

Validate your config without making any changes.

```bash
gh-config --token <your-pat> sync config.yaml --dry-run
# or
GITHUB_TOKEN=<your-pat> gh-config sync config.yaml --dry-run
```

It prints every webhook it would create or update, by repo, showing the URL's host rather than the whole URL:

```text
[Dry Run] Would create webhook on gitkb/my-repo: example.com, content_type json, events ["pull_request", "push"]
```

---

### Generate Config from GitHub Org

Export your current GitHub org state into a config file.

```bash
gh-config --token <your-pat> sync-from-org config.yaml --org gitkb
# or
GITHUB_TOKEN=<your-pat> gh-config sync-from-org config.yaml --org gitkb
```

---

## Extensible Schema and Advanced Usage

A repo's `settings` take any field of GitHub's repository endpoints that is in gh-config's API mapping: an endpoint under `/repos/{owner}/{repo}` that needs nothing but the repo's name. A key that would write the org, a user or anything else is refused.

- The tool uses a mapping generated from the GitHub OpenAPI spec to translate config keys to API endpoints and payloads.
- To add a new setting, just add the field to your config. If it's present in the mapping, it will be managed automatically; if it isn't, every command refuses the config and names the setting.
- To update the mapping, run `cargo xtask gen-api-mappings --fetch` to fetch the latest API spec and regenerate the mapping (without `--fetch` it regenerates from the pinned spec in `mappings/`).

### Troubleshooting

- If a setting is refused as unknown, check that the config key matches the GitHub API field name; if it does, update the mapping as described above.
- For advanced troubleshooting, see [Architecture Overview](./architecture.md).

---

## Help

Run:

```bash
gh-config --help
```

to see all available options and commands.

---

## See also

- [Creating a PAT](./pat-setup.md)
- [GitHub Actions examples](./ci-examples.md)
- [Configuration schema](../README.md#configuration)
- [Architecture Overview](./architecture.md)
