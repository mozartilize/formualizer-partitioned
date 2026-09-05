#!/usr/bin/env bash
# Fetch real xlsx workbooks published by the Federal Reserve into ./fed.
#
# These are used by ../golden_diff.py as a real-world correctness
# corpus: 23 sheets each, mixed value types, and heavy use of defined names,
# which is a case generated fixtures do not reach. They are not useful as a
# memory benchmark: each holds ~31 formulas against ~43k cells, so almost all of
# their memory is data rather than evaluation.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p fed

base="https://www.federalreserve.gov/supervisionreg/files"
for f in \
  ccar-2025-stress-test-severely-adverse-market-shocks.xlsx \
  ccar-2025-exploratory-market-shocks.xlsx \
  2024-stress-test-severely-adverse-market-shocks.xlsx \
  2024-exploratory-A-market-shocks.xlsx \
  2024-exploratory-B-market-shocks.xlsx
do
  if [ -s "fed/$f" ]; then
    echo "have  $f"
    continue
  fi
  echo "fetch $f"
  curl -sL --max-time 120 -o "fed/$f" "$base/$f"
  # A missing file comes back as an HTML error page, not a zip.
  if [ "$(head -c2 "fed/$f")" != "PK" ]; then
    echo "  not an xlsx, removing" >&2
    rm -f "fed/$f"
  fi
done
ls -la fed
