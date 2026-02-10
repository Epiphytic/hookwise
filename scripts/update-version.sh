#!/usr/bin/env bash
# Called by semantic-release prepareCmd to update version across all manifests.
# Usage: scripts/update-version.sh <version>
set -euo pipefail

VERSION="${1:?Usage: update-version.sh <version>}"

# Strip leading 'v' if present
VERSION="${VERSION#v}"

echo "Updating version to ${VERSION}"

# Cargo.toml — only the package-level version (starts at column 0)
sed -i.bak 's/^version = ".*"/version = "'"${VERSION}"'"/' Cargo.toml
rm -f Cargo.toml.bak

# .claude-plugin/plugin.json
sed -i.bak 's/"version": ".*"/"version": "'"${VERSION}"'"/' .claude-plugin/plugin.json
rm -f .claude-plugin/plugin.json.bak

# .claude-plugin/marketplace.json
sed -i.bak 's/"version": ".*"/"version": "'"${VERSION}"'"/' .claude-plugin/marketplace.json
rm -f .claude-plugin/marketplace.json.bak

# gemini-extension.json
sed -i.bak 's/"version": ".*"/"version": "'"${VERSION}"'"/' gemini-extension.json
rm -f gemini-extension.json.bak

# Regenerate Cargo.lock
cargo update --workspace

echo "Version updated to ${VERSION} in all manifests"
