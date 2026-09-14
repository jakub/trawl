#!/usr/bin/env bash
# Sourced by the harness and its disposable-container regression tests.

remove_stopped_harness_container() {
  local name="$1" details id running owner
  details=$(docker inspect -f '{{.Id}} {{.State.Running}} {{index .Config.Labels "trawl-crashdump-harness-run"}}' "$name" 2>/dev/null) || return 0
  read -r id running owner <<< "$details"
  if [[ ! "$owner" =~ ^[0-9]+-[0-9]+$ ]]; then
    printf 'container %s has no recognized prior harness ownership label; refusing to remove it\n' "$name" >&2
    return 1
  fi
  if [[ "$running" == true ]]; then
    printf 'container %s is RUNNING. Another run may own it, or --keep left it behind. After checking ownership and confirming nothing uses it, remove it with: docker rm -fv %s\n' "$name" "$id" >&2
    return 1
  fi
  # Address the inspected object, not a name that could have been reassigned.
  # Omit force so a concurrent start makes removal fail instead of killing it.
  # -v also removes the stopped Postgres container's anonymous data volume.
  if ! docker rm -v "$id" >/dev/null; then
    printf 'could not remove stopped harness container %s; it may now be running\n' "$name" >&2
    return 1
  fi
  printf 'container %s (stopped prior harness run %s)\n' "$name" "$owner"
}
