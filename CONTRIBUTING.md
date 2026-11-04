# Contributing to gh-config

Thank you for your interest in contributing! We welcome issues, feature requests, and pull requests.

## How to Contribute

- **Open an Issue:** For bugs, feature requests, or questions.
- **Fork the Repo:** Create your own branch from `main`.
- **Write Tests:** Cover new features or bug fixes with tests.
- **Submit a Pull Request:** Describe your changes and reference related issues.

## Development Setup

1. **Install Rust (stable):**
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
2. **Clone the repository:**
   ```bash
   git clone https://github.com/gitkb/gh-config-cli.git
   cd gh-config-cli
   ```
3. **Build the project:**
   ```bash
   cargo build --release
   ```

## Running Tests

```bash
cargo test
```

## Generated code

`src/github_api_mapping_generated.rs` is generated from the pinned GitHub OpenAPI spec in `mappings/`. Regenerate it with `cargo xtask gen-api-mappings` (add `--fetch` to download the latest spec first); CI runs `cargo xtask gen-api-mappings --check`.

## Linting

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Commit Messages

- Use clear, descriptive commit messages.
- Reference issues/PRs when relevant.

## Code Style

- Follow Rust community conventions.
- Use `cargo fmt` before submitting.

## Reporting Security Issues

Please create GitHub issues with any relevant security finds.

## Code of Conduct

Please read and follow our [Code of Conduct](CODE_OF_CONDUCT.md).

---
Thank you for helping make gh-config better!