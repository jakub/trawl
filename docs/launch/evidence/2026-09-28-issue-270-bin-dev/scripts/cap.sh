#!/usr/bin/env bash
# Capture the tmux pane, strip trailing blanks and box-drawing padding, and
# redact the fleet-dev API key (flt_ prefix) before anything touches disk.
tmux -L j270c9b capture-pane -p -J -t dev \
  | sed -E -e 's/flt_[A-Za-z0-9_-]+/flt_<redacted>/g' -e 's/[│┃ ]+$//' -e 's/^┃[^┃]*┃│//' \
  | awk 'NF' 
