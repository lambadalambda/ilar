#!/usr/bin/env bash
# Build the release binaries — ilar and ilar-gateway — and install them.
#
#   scripts/install.sh [destination]     # default: ~/.local/bin
#
# The one non-obvious step is the delete before the copy. Writing over an
# existing binary keeps its inode, macOS's cached code signature no longer
# matches what is there, and the next launch dies with SIGKILL before main
# runs. Removing first gives the copy a fresh inode.
set -euo pipefail

dest=${1:-${ILAR_INSTALL_DIR:-$HOME/.local/bin}}
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

cargo build --release --manifest-path "$root/Cargo.toml"

mkdir -p "$dest"
for bin in ilar ilar-gateway; do
    rm -f "$dest/$bin"
    cp "$root/target/release/$bin" "$dest/$bin"

    # Re-sign only if the fresh inode was not enough; an ad-hoc signature is
    # what the toolchain would have applied anyway.
    if ! version=$("$dest/$bin" --version 2>/dev/null); then
        echo "installed binary would not run; re-signing" >&2
        codesign --force -s - "$dest/$bin"
        version=$("$dest/$bin" --version)
    fi

    echo "$version -> $dest/$bin"
done
case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "note: $dest is not on PATH" >&2 ;;
esac
