trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query --format json \
  'service=tutorial _severity>=error last=1h | table message, duration'
