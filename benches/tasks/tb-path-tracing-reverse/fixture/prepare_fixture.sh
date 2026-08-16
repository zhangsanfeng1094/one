#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")"
if [[ ! -x mystery && -f mystery.gz ]]; then
  gzip -dc mystery.gz > mystery
  chmod +x mystery
fi
if [[ ! -f reference.ppm && -f reference.ppm.gz ]]; then
  gzip -dc reference.ppm.gz > reference.ppm || true
fi
