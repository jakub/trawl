---
title: Vector Integration
description: Configure Vector to ship logs to trawl.
---

:::note
This guide is a work in progress. Check back soon for complete Vector configuration examples.
:::

[Vector](https://vector.dev) is the recommended log shipper for trawl. It handles collection from multiple sources, batching, compression, and reliable delivery.

## Basic sink configuration

```toml
[sinks.trawl]
type = "http"
inputs = ["your_source"]
uri = "https://your-trawl-server:5514/api/v1/ingest"
encoding.codec = "json"
compression = "gzip"
batch.max_bytes = 1048576
batch.timeout_secs = 5

[sinks.trawl.request]
headers.authorization = "Bearer ${TRAWL_INGEST_TOKEN}"

[sinks.trawl.tls]
verify_certificate = false  # if using self-signed certs
```

## Common sources

Examples for syslog, journald, Docker, and file tailing coming soon.
