#!/usr/bin/env bash
# This Source Code Form is subject to the terms of the Mozilla Public
# License, v. 2.0. If a copy of the MPL was not distributed with this
# file, You can obtain one at https://mozilla.org/MPL/2.0/.

# Syntax-check the deployment guide's nginx reverse-proxy recipe with
# `nginx -t`. The guide holds exactly one ```nginx fence. The check gives it
# a throwaway certificate pair at the paths it names and loads it as
# /etc/nginx/conf.d/trawl.conf in a digest-pinned nginx 1.26 image, the
# version Debian 13 ships, with no network.
set -euo pipefail

image="nginx:1.26@sha256:41b194461e4bae16f9b25d68b0976ed4735b89ca625c89aad88e1c1c3b7e8860"
docs="$(cd "$(dirname "$0")/.." && pwd)"
page="$docs/src/content/docs/operate/deployment.md"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Copy every ```nginx fence out of the page, without the list indentation
# of its opening line, and count the fences.
fences="$(awk -v out="$work/trawl.conf" '
  !inside && /^ *```nginx$/ {
    inside = 1; fences++; indent = index($0, "`") - 1; next
  }
  inside && $0 ~ "^ {" indent "}```$" { inside = 0; next }
  inside { print substr($0, indent + 1) > out }
  END { print fences + 0 }
' "$page")"
if [[ "$fences" -ne 1 ]]; then
  echo "check-nginx-recipe: expected one nginx block in $page, found $fences" >&2
  exit 1
fi

directive() {
  awk -v name="$1" '$1 == name { sub(/;$/, "", $2); print $2 }' "$work/trawl.conf" | sort -u
}
cert="$(directive ssl_certificate)"
key="$(directive ssl_certificate_key)"
for path in "$cert" "$key"; do
  if [[ "$path" != /* || "$path" == *$'\n'* ]]; then
    echo "check-nginx-recipe: the recipe must name one absolute certificate and key path, got: $path" >&2
    exit 1
  fi
done

# The pair only has to load; nobody connects.
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj "/CN=trawl.example.com" \
  -keyout "$work/key.pem" -out "$work/cert.pem" 2>/dev/null
chmod 0644 "$work/key.pem" "$work/cert.pem" "$work/trawl.conf"

docker run --rm --network none \
  --mount "type=bind,source=$work/trawl.conf,target=/etc/nginx/conf.d/trawl.conf,readonly" \
  --mount "type=bind,source=$work/cert.pem,target=$cert,readonly" \
  --mount "type=bind,source=$work/key.pem,target=$key,readonly" \
  --entrypoint nginx "$image" -t
