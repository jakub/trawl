#!/usr/bin/env bash
# Pull a release's container image and Helm chart from ghcr.io with no
# credentials, the way an evaluator without a GitHub account pulls them.
#
# Usage: check-anonymous-pulls.sh TAG IMAGE_REPOSITORY CHART_REPOSITORY
#   TAG               a release tag, for example v0.9.0
#   IMAGE_REPOSITORY  for example ghcr.io/jakub/trawl
#   CHART_REPOSITORY  for example oci://ghcr.io/jakub/charts
#
# Docker and Helm read their registry credentials from their config files.
# Both get fresh empty ones here, so no stored login can be offered. The image
# is pulled as IMAGE_REPOSITORY:VERSION, the tag release.yml pushes, and the
# chart as CHART_REPOSITORY/trawl at VERSION, where VERSION is TAG without
# its leading v. Each pull is tried three times, because a registry may take
# a moment to serve a tag it has just accepted.
set -euo pipefail

[[ $# -eq 3 ]] || { echo "usage: check-anonymous-pulls.sh TAG IMAGE_REPOSITORY CHART_REPOSITORY" >&2; exit 2; }
tag=$1
image_repository=$2
chart_repository=$3
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([-.][0-9A-Za-z.-]+)?$ ]] || { echo "not a release tag: $tag" >&2; exit 2; }
[[ "$chart_repository" == oci://* ]] || { echo "not an OCI chart repository: $chart_repository" >&2; exit 2; }
version=${tag#v}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
export DOCKER_CONFIG="$work/docker"
export HELM_REGISTRY_CONFIG="$work/helm/registry/config.json"
export HELM_CONFIG_HOME="$work/helm/config"
export HELM_CACHE_HOME="$work/helm/cache"
export HELM_DATA_HOME="$work/helm/data"
mkdir -p "$DOCKER_CONFIG" "$(dirname "$HELM_REGISTRY_CONFIG")" "$HELM_CONFIG_HOME" "$HELM_CACHE_HOME" "$HELM_DATA_HOME" "$work/chart"
unset DOCKER_AUTH_CONFIG REGISTRY_AUTH_FILE

retry() {
  local attempt
  for attempt in 1 2 3; do
    if "$@"; then return 0; fi
    echo "attempt $attempt failed: $*" >&2
    [[ $attempt -eq 3 ]] || sleep $((attempt * 10))
  done
  return 1
}

image="$image_repository:$version"
echo "== anonymous docker pull $image"
retry docker pull "$image"
docker image inspect --format '{{.Id}} {{.Os}}/{{.Architecture}} {{join .RepoDigests " "}}' "$image"

chart="$chart_repository/trawl"
echo "== anonymous helm pull $chart --version $version"
retry helm pull "$chart" --version "$version" --destination "$work/chart"
helm show chart "$work/chart/trawl-$version.tgz"
values=$(helm show values "$work/chart/trawl-$version.tgz")
chart_image="$(sed -n 's/^  repository: *//p' <<<"$values" | head -n1):$(sed -n 's/^  tag: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' <<<"$values" | head -n1)"
echo "the chart deploys $chart_image"
[[ "$chart_image" == "$image" ]] || { echo "the chart deploys $chart_image, not $image" >&2; exit 1; }
echo "both pulled with no credentials"
