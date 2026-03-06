# Run the enki CLI (passes through any args)
run *ARGS:
    cargo run --bin enki -- {{ARGS}}

# Run the TUI chat example
chat:
    cargo run -p enki-tui --example chat

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
    sed -i '' "s/^version = \".*\"/version = \"${version}\"/" Cargo.toml
    cargo generate-lockfile --quiet
    git add Cargo.toml Cargo.lock
    git commit -m "release ${tag}"
    echo "Tagging $tag"
    git tag "$tag"
    git push origin "$tag"
    git push origin HEAD

    # Update homebrew tap
    tap_dir="${HOME}/dev/homebrew-tap"
    if [ ! -d "$tap_dir" ]; then
        echo "homebrew-tap not found at $tap_dir, skipping tap update"
        exit 0
    fi
    echo "Updating homebrew tap..."
    tarball_url="https://github.com/korbindeman/enki/archive/refs/tags/${tag}.tar.gz"
    sha=$(curl -sL "$tarball_url" | shasum -a 256 | cut -d' ' -f1)
    formula="$tap_dir/Formula/enki.rb"
    # Strip leading 'v' for the version field
    tap_version="${tag#v}"
    sed -i '' "s|^  url .*|  url \"${tarball_url}\"|" "$formula"
    sed -i '' "s|^  version .*|  version \"${tap_version}\"|" "$formula"
    sed -i '' "s|^  sha256 .*|  sha256 \"${sha}\"|" "$formula"
    git -C "$tap_dir" add Formula/enki.rb
    git -C "$tap_dir" commit -m "enki ${tag}"
    git -C "$tap_dir" push
