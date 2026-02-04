# Release Workflow Design

## Goal

Automate building release tarballs for three target platforms and publishing
them as GitHub Releases.

## Target Platforms

| Platform   | Runner             | Nix system        |
|------------|--------------------|-------------------|
| macOS ARM  | `macos-latest`     | `aarch64-darwin`  |
| Linux x86  | `ubuntu-latest`    | `x86_64-linux`    |
| Linux ARM  | `ubuntu-24.04-arm` | `aarch64-linux`   |

## Triggers

- **Tag push** matching `v*` (e.g., `v0.1.0`) — creates a published release.
- **Manual dispatch** (`workflow_dispatch`) — creates a draft release for
  testing.

## Deliverables

### 1. `.github/workflows/release.yml`

Two jobs:

**`build` (matrix × 3 platforms):**

1. Checkout repo.
2. Install Nix (`DeterminateSystems/nix-installer-action`).
3. Enable Nix cache (`DeterminateSystems/magic-nix-cache-action`).
4. Run `nix build .#tarball`.
5. Copy tarball from `result` symlink to working directory.
6. Upload as artifact (`actions/upload-artifact`).

The existing flake already names tarballs correctly:
`jjq-<version>-<system>.tar.gz`.

**`release` (runs after all builds succeed):**

1. Download all artifacts.
2. Create GitHub Release via `gh release create`:
   - Tag-triggered: published release named after the tag.
   - Manual dispatch: draft release.
3. Attach all three tarballs.

Permissions: `release` job needs `contents: write`.

### 2. `scripts/tag-release`

Shell script wrapping the tag-and-push workflow for jj-colocated repos.

Usage: `scripts/tag-release v0.1.0`

Steps:

1. Validate argument matches `v<number>.<number>.<number>`.
2. Extract version from `Cargo.toml` and verify it matches (minus `v` prefix).
3. Check tag doesn't already exist.
4. `jj git export`
5. `git tag <version>`
6. `git push origin <version>`
