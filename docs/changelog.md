# Changelog

All notable changes to this project will be documented in this file.

## Unreleased

- `sync` creates repos the config lists and GitHub doesn't have, with their `visibility`.
- `sync-from-org` and `diff` record `allow_auto_merge` and private repos' Actions `access_level`, so the drift check keeps them; `sync` reads `access_level` before writing it.
- New repo field `required_checks`, kept in the repo ruleset `Require CI`.

## [v1.3.2] - 2026-11-09

- Skip organization membership and team permission writes when the configured values already match GitHub.
- Serialize organization API calls and wait at least one second between writes.
- Retry HTTP 429 and rate-limited HTTP 403 responses up to three times, honoring `Retry-After` and primary rate-limit reset headers, with exponential backoff when no cooldown is supplied.
- Include the HTTP status in API errors so missing memberships and assignments are recognized even when GitHub returns only `Not Found` in its response body.

## [Unreleased]

- Initial open source release preparations
- Added CONTRIBUTING.md, CODE_OF_CONDUCT.md, LICENSE, and expanded documentation
- Improved documentation structure and clarity

## [v0.1.0] - 2025-04-10

- First public release
- Core features:
  - Declarative YAML config for GitHub orgs
  - Diff, sync, dry-run, and sync-from-org commands
  - Cross-platform support (Linux, macOS, Windows)
  - CI/CD integration examples
  - Fast, reliable, and written in Rust

---
For previous internal changes, see commit history.
