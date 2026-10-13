#!/usr/bin/env bash
# Builds the skills mirror and publishes it to the LynShen software library
# (product `skills`) when anything in it changed.
#
#   deploy/skills-mirror/publish.sh
#
# Needs SOFTWARE_SIGNING_KEY and SOFTWARE_SSH, as for
# deploy/software-library/publish.sh, and GitHub access; GITHUB_TOKEN is
# optional. Run it on the machine that holds the signing key, never on the
# server. See docs/software-library.md, "Skills mirror".
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${SOFTWARE_SIGNING_KEY:?path to the signing key}" "${SOFTWARE_SSH:?user@host}"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
python3 -I "$here/build.py" build "$work/files"
if curl -fsS --max-time 30 "https://software.lynshen.org/skills/latest.json" -o "$work/published.json" &&
	python3 -I "$here/build.py" unchanged "$work/files" <"$work/published.json"; then
	echo "skills mirror unchanged"
	exit 0
fi
# A version that only grows: the UTC date, then the time of day.
version="$(date -u +%Y%m%d).$((10#$(date -u +%H%M))).0"
"$here/../software-library/publish.sh" skills "$version" "$work/files"
