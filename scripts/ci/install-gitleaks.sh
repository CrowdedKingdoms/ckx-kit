#!/usr/bin/env bash
# gitleaks into the working directory, verified against the release's published SHA-256 before
# it is unpacked or run.
set -euo pipefail
VERSION=8.21.2
SHA256=5bc41815076e6ed6ef8fbecc9d9b75bcae31f39029ceb55da08086315316e3ba
curl -sSL -o gitleaks.tar.gz "https://github.com/gitleaks/gitleaks/releases/download/v${VERSION}/gitleaks_${VERSION}_linux_x64.tar.gz"
echo "${SHA256}  gitleaks.tar.gz" | sha256sum -c -
tar -xzf gitleaks.tar.gz gitleaks
rm gitleaks.tar.gz
