#!/usr/bin/env bash

set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
source_svg="$root_dir/logo.svg"
output_dir="$root_dir/assets"
temporary_dir=$(mktemp -d)
trap 'rm -rf "$temporary_dir"' EXIT

mkdir -p "$output_dir"

rsvg-convert --width 512 --height 512 --output "$temporary_dir/icon.png" "$source_svg"
magick "$temporary_dir/icon.png" -resize 128x128 "BMP3:$temporary_dir/icon.bmp"
magick "$temporary_dir/icon.png" \
  -define icon:auto-resize=256,128,64,48,32,24,16 \
  "$temporary_dir/icon.ico"

mv "$temporary_dir/icon.png" "$output_dir/icon.png"
mv "$temporary_dir/icon.bmp" "$output_dir/icon.bmp"
mv "$temporary_dir/icon.ico" "$output_dir/icon.ico"
