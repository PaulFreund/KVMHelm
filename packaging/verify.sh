#!/bin/sh
set -eu
CDPATH= cd -- "$(dirname -- "$0")"
count=0
while IFS= read -r line; do
    expected=${line%% *}
    relative=${line#*  }
    case "$relative" in /*|..|../*|*/..|*/../*|*\\*) echo 'Invalid package path' >&2; exit 1;; esac
    [ "${#expected}" -eq 64 ] && [ -f "$relative" ] || exit 1
    part=$relative
    while [ "$part" != . ]; do
        [ ! -L "$part" ] || { echo 'Package links are not supported' >&2; exit 1; }
        part=$(dirname -- "$part")
    done
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum -- "$relative")
    else
        actual=$(shasum -a 256 -- "$relative")
    fi
    [ "${actual%% *}" = "$expected" ] || { echo "Checksum mismatch: $relative" >&2; exit 1; }
    count=$((count + 1))
done < SHA256SUMS
echo "Verified $count package files."
