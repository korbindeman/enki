# Run the TUI chat example
chat:
    cargo run -p enki-tui --example chat

# Run the desktop app in dev mode (uses current directory as project)
desktop:
    export ENKI_PROJECT_DIR="{{justfile_directory()}}" && cd crates/desktop && cargo tauri dev

# Build and install release binary to ~/.cargo/bin
install:
    cargo install --path crates/cli

# Tag a new release based on today's date (e.g. v2026.03.06, v2026.03.06.1)
release:
    #!/usr/bin/env bash
    set -euo pipefail
    base="v$(date +%Y.%m.%d)"
    # Find existing tags for today and pick the next suffix
    existing=$(git tag -l "${base}*" | sort -V)
    if [ -z "$existing" ]; then
        tag="$base"
    else
        last=$(echo "$existing" | tail -1)
        if [ "$last" = "$base" ]; then
            tag="${base}.1"
        else
            n=$(echo "$last" | grep -oE '[0-9]+$')
            tag="${base}.$((n + 1))"
        fi
    fi
    # Update workspace version (semver: YYYY.MMDD.N)
    year=$(date +%Y)
    mmdd=$(date +%m%d | sed 's/^0*//')
    if [ "$tag" = "$base" ]; then
        n=0
    else
        n=$(echo "$tag" | grep -oE '[0-9]+$')
    fi
    version="${year}.${mmdd}.${n}"
    sed "s/^version = \".*\"/version = \"${version}\"/" Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml
    cargo generate-lockfile --quiet
    git add Cargo.toml Cargo.lock
    git commit -m "release ${tag}"
    echo "Tagging $tag"
    git tag "$tag"
    git push origin "$tag"
    git push origin HEAD
