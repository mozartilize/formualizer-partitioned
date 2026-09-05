#!/usr/bin/env bash
# Download the SpreadsheetBench and Sheetpedia xlsx corpora into this directory.
# The Sheetpedia archive is several gigabytes compressed and much larger after
# extraction; run this script only when the full corpus is wanted.
set -euo pipefail

cd "$(dirname "$0")"

has_xlsx() {
  find "$1" -type f -name '*.xlsx' -print -quit | grep -q .
}

download_and_extract() {
  local name="$1"
  local dir="$2"
  local archive="$3"
  local url="$4"

  mkdir -p "$dir"
  if has_xlsx "$dir"; then
    echo "have  $name"
    return
  fi

  if [ ! -s "$archive" ]; then
    echo "fetch $name"
    curl -fL --retry 3 --retry-delay 2 --connect-timeout 30 \
      -o "$archive.part" "$url"
    mv "$archive.part" "$archive"
  else
    echo "have  $name archive"
  fi

  echo "unpack $name"
  tar -tzf "$archive" >/dev/null
  tar -xzf "$archive" -C "$dir"
}

download_and_extract \
  "SpreadsheetBench" \
  "ssb" \
  "ssb/spreadsheetbench_912_v0.1.tar.gz" \
  "https://huggingface.co/datasets/KAKA22/SpreadsheetBench/resolve/main/spreadsheetbench_912_v0.1.tar.gz?download=true"

download_and_extract \
  "Sheetpedia" \
  "sheetpedia" \
  "sheetpedia/pii_processed_xlsx_0929.tar.gz" \
  "https://huggingface.co/datasets/tianzl66/Sheetpedia_xlsx/resolve/main/pii_processed_xlsx_0929.tar.gz?download=true"

printf 'SSB workbooks: '
find ssb -type f -name '*.xlsx' | wc -l
printf 'Sheetpedia workbooks: '
find sheetpedia -type f -name '*.xlsx' | wc -l
