#!/usr/bin/env bash
# Build the release binary and install it.
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
rm -f "$dest/ilar"
cp "$root/target/release/ilar" "$dest/ilar"

# Re-sign only if the fresh inode was not enough; an ad-hoc signature is
# what the toolchain would have applied anyway.
if ! version=$("$dest/ilar" --version 2>/dev/null); then
    echo "installed binary would not run; re-signing" >&2
    codesign --force -s - "$dest/ilar"
    version=$("$dest/ilar" --version)
fi

echo "$version -> $dest/ilar"
case ":$PATH:" in
    *":$dest:"*) ;;
    *) echo "note: $dest is not on PATH" >&2 ;;
esac
