#!/usr/bin/env bash
# Package this checkout with Docker metadata's published primary image tag.
# Never resolve another ref or edit the source chart's values in place.
set -euo pipefail
repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
version=${1:?usage: package-chart.sh RELEASE_TAG IMAGE_TAG DESTINATION}
version=${version#v}
image_tag=${2:?usage: package-chart.sh RELEASE_TAG IMAGE_TAG DESTINATION}
destination=${3:?usage: package-chart.sh RELEASE_TAG IMAGE_TAG DESTINATION}
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT
cp -R "$repo_root/chart/trawl" "$work_dir/trawl"
python3 - "$work_dir/trawl/values.yaml" "$image_tag" <<'PY'
import json
import re
from pathlib import Path
import sys

if re.fullmatch(r"[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}", sys.argv[2]) is None or sys.argv[2] == "latest":
    raise SystemExit("expected a valid versioned Docker image tag")
values = Path(sys.argv[1])
source = values.read_text()
placeholder = '  tag: ""\n'
if source.count(placeholder) != 1:
    raise SystemExit("expected exactly one empty source image.tag")
values.write_text(source.replace(placeholder, f"  tag: {json.dumps(sys.argv[2])}\n"))
PY
helm package "$work_dir/trawl" --destination "$destination" \
  --version "$version" --app-version "$version"
