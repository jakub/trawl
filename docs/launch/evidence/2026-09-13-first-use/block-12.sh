trawl --config "$TRAWL_TUTORIAL_DIR/client.toml" query \
  'service=tutorial last=1h' --format parquet \
  --output "$TRAWL_TUTORIAL_DIR/tutorial.parquet"
trawl query --data "$TRAWL_TUTORIAL_DIR/tutorial.parquet" --format json \
  '* | stats count() by service'
