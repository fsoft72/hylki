#!/usr/bin/env bash
#
# Update Hylki: pulls main from the upstream remote, then runs ./install.sh.
#
# Usage:  ./scripts/update.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BRANCH="main"
REMOTE="upstream"

cd "$ROOT"

echo "==> Pulling $BRANCH from $REMOTE"
git pull "$REMOTE" "$BRANCH"

echo "==> Running install.sh"
./install.sh
