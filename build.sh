#!/usr/bin/env bash
# Auto-increment version on every build

set -e

CARGO_TOML="Cargo.toml"

# Get current version
CURRENT_VERSION=$(grep "^version = " "$CARGO_TOML" | head -1 | sed 's/version = "\(.*\)"/\1/')
echo "📦 Current version: $CURRENT_VERSION"

# Parse version (format: x.y.z or x.y.z+N)
if [[ $CURRENT_VERSION =~ ([0-9]+\.[0-9]+\.[0-9]+)\+([0-9]+) ]]; then
    BASE_VERSION="${BASH_REMATCH[1]}"
    BUILD_NUMBER="${BASH_REMATCH[2]}"
    NEW_BUILD_NUMBER=$((BUILD_NUMBER + 1))
    NEW_VERSION="${BASE_VERSION}+${NEW_BUILD_NUMBER}"
elif [[ $CURRENT_VERSION =~ ([0-9]+\.[0-9]+\.[0-9]+) ]]; then
    BASE_VERSION="${BASH_REMATCH[1]}"
    NEW_VERSION="${BASE_VERSION}+1"
else
    echo "❌ Unable to parse version: $CURRENT_VERSION"
    exit 1
fi

echo "🚀 New version: $NEW_VERSION"

# Update Cargo.toml
if [[ "$OSTYPE" == "darwin"* ]]; then
    # macOS
    sed -i '' "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" "$CARGO_TOML"
else
    # Linux
    sed -i "s/^version = \"$CURRENT_VERSION\"/version = \"$NEW_VERSION\"/" "$CARGO_TOML"
fi

echo "✅ Version updated in $CARGO_TOML"
