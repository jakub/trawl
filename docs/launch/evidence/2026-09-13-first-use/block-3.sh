fleet-admin roles create --name tutorial-reader \
  --perm trawl:query --perm trawl:schema_read --perm trawl:validate \
  --perm trawl:export --perm trawl:stream --perm trawl:saved_query \
  --perm trawl:query_cancel
fleet-admin roles create --name tutorial-ingest --perm trawl:ingest
fleet-admin keys create --name tutorial-reader --kind human \
  --role tutorial-reader > "$TRAWL_TUTORIAL_DIR/reader.token"
fleet-admin keys create --name tutorial-ingest --kind service \
  --role tutorial-ingest > "$TRAWL_TUTORIAL_DIR/ingest.token"
unset DATABASE_URL
