trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query --format json \
  'service=tutorial last=1h | stats count() by service'
