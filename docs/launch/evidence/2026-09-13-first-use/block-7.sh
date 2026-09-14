printf '[server]\nurl = "https://localhost:15514"\ntoken = "%s"\ninsecure = true\n' \
  "$(cat "$TRAWL_TUTORIAL_DIR/reader.token")" > "$TRAWL_TUTORIAL_DIR/client.toml"
unset TRAWL_TOKEN TRAWL_URL TRAWL_PROFILE TRAWL_INSECURE
