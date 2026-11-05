#!/bin/sh
# Builds the plugin and stages a package directory in pkg/.
set -e
cd "$(dirname "$0")"
cargo build --release
mkdir -p pkg/bin
cp manifest.json pkg/manifest.json
cp "target/release/strm_plugin" pkg/bin/plugin
echo "plugin staged in pkg/"
