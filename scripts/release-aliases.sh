#!/usr/bin/env bash
# Adds copies of a release's installers under names that do not contain the
# version, so that
#   https://github.com/<repo>/releases/latest/download/<name>
# always points at the newest build. The README's download links rely on this.
#
# Usage: scripts/release-aliases.sh <tag> [repo]
set -euo pipefail

tag="$1"
repo="${2:-${GITHUB_REPOSITORY:-Dimtar/hexy-endpoint}}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir "$work/in" "$work/out"

gh release download "$tag" --repo "$repo" --dir "$work/in" \
  -p '*_aarch64.dmg' -p '*_x64.dmg' -p '*_x64-setup.exe' \
  -p '*_amd64.AppImage' -p '*_amd64.deb' -p '*.x86_64.rpm'

alias_for() {
  case "$1" in
    *_aarch64.dmg)    echo "Hexy-Endpoint-macOS-Apple-Silicon.dmg" ;;
    *_x64.dmg)        echo "Hexy-Endpoint-macOS-Intel.dmg" ;;
    *_x64-setup.exe)  echo "Hexy-Endpoint-Windows.exe" ;;
    *_amd64.AppImage) echo "Hexy-Endpoint-Linux.AppImage" ;;
    *_amd64.deb)      echo "Hexy-Endpoint-Linux.deb" ;;
    *.x86_64.rpm)     echo "Hexy-Endpoint-Linux.rpm" ;;
  esac
}

count=0
for file in "$work"/in/*; do
  name="$(alias_for "$(basename "$file")")"
  [ -n "$name" ] || continue
  cp "$file" "$work/out/$name"
  count=$((count + 1))
done

if [ "$count" -ne 6 ]; then
  echo "expected 6 installers in $tag, found $count" >&2
  exit 1
fi

gh release upload "$tag" --repo "$repo" --clobber "$work"/out/*
echo "uploaded $count fixed-name installers to $tag"
