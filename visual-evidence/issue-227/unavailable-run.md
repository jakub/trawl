# A run whose stored result is gone — issue #227

Captured by `visual-evidence/issue-227/capture-unavailable-run.sh` against a
disposable full-app stack (`bin/app-experiment`: its own Postgres container,
keys, TLS, `trawld` and `trawl-web`). This file is that script's stdout.

- commit: `fdf38915c152afe5e4e1e021f8574f59fb04141d`
- experiment run: `run-1790010770915-7a1a8dafc8`
- `$UP` is the disposable daemon's loopback HTTPS origin, `$KEY` its
  browser key. Neither is printed: the key is a credential, and the instance
  is gone by the time you read this.

## The state under test

One net, `unavailable_demo`, with two successful runs of three rows each:

- run **1**, the older
- run **2**, the newer — `run=latest` selects this one and never falls
  back to the one behind it (ADR-0018 ruling 13)

One control net, `zero_row_control`, whose run **3** succeeded and
found nothing. A zero-row result has no schema to write a parquet from, so it
is recorded as a blob and has no file to lose. It is read at every step below,
unchanged, so "the file is gone" can be told apart from "the report is empty".

Every run's stored `result_path` is relative to the data root. The capture
moves run 2's file aside — the same thing an operator does by repointing
`data_dir` after an epoch bump, at the scale of one file.

## Both runs read while the file is there

```console
$ curl -sS -k "$UP/api/v1/saved/1/runs/2" -H "Authorization: Bearer $KEY"
HTTP 200
{
  "id": 2,
  "query": "experiment_run=\"run-1790010770915-7a1a8dafc8\" earliest=\"2026-01-01T00:00:00Z\" latest=\"2026-01-02T00:00:00Z\" experiment_seq<3 | fields experiment_seq, status | sort experiment_seq",
  "status": "success",
  "started_at": "2026-09-21T17:16:20.617040+00:00",
  "finished_at": "2026-09-21T17:16:20.628598+00:00",
  "duration_ms": 2,
  "row_count": 3,
  "result_path": "scheduled/run_2.parquet",
  "result": {
    "columns": [
      {
        "name": "experiment_seq"
      },
      {
        "name": "status"
      }
    ],
    "rows": [
      [
        0,
        200
      ],
      [
        1,
        200
      ],
      [
        2,
        200
      ]
    ]
  }
}
```

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=latest | stats count()"}'
HTTP 200
{
  "execution": {
    "started_at": "2026-09-21T17:16:20.948971696Z",
    "duration_ms": 2
  },
  "columns": [
    {
      "name": "count"
    }
  ],
  "rows": [
    [
      3
    ]
  ],
  "truncated": false,
  "pagination": {
    "limit": 25000,
    "offset": 0,
    "returned": 1
  }
}
```

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=all | stats count()"}'
HTTP 200
{
  "execution": {
    "started_at": "2026-09-21T17:16:20.998358771Z",
    "duration_ms": 2
  },
  "columns": [
    {
      "name": "count"
    }
  ],
  "rows": [
    [
      6
    ]
  ],
  "truncated": false,
  "pagination": {
    "limit": 25000,
    "offset": 0,
    "returned": 1
  }
}
```

## The newer run's stored result goes

```console
$ mv "$DATA/scheduled/run_2.parquet" "$ASIDE"
```

## Every read surface names run 2

The direct run endpoint. Before this fix it answered 200 with `result: null`
beside a non-zero `row_count`, which the browser drawer rendered as "No
result data (error or still running)" and the TUI as "Run has no result data" —
both false for a run that succeeded.

```console
$ curl -sS -k "$UP/api/v1/saved/1/runs/2" -H "Authorization: Bearer $KEY"
HTTP 409
{
  "error": {
    "code": "bad_request",
    "message": "report run 2 succeeded, but its stored result is unavailable; no older run was substituted"
  }
}
```

`run=2`, naming the run outright.

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=2 | stats count()"}'
HTTP 409
{
  "error": {
    "code": "bad_request",
    "message": "report run 2 succeeded, but its stored result is unavailable; no older run was substituted"
  }
}
```

`run=latest`. The older run's three rows are still on disk, and the refusal
says in so many words that they were not substituted.

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=latest | stats count()"}'
HTTP 409
{
  "error": {
    "code": "bad_request",
    "message": "report run 2 succeeded, but its stored result is unavailable; no older run was substituted"
  }
}
```

`run=all`. One missing member fails the whole union, naming that member: not
a partial union of the survivors, and not zero rows.

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=all | stats count()"}'
HTTP 409
{
  "error": {
    "code": "bad_request",
    "message": "report run 2 succeeded, but its stored result is unavailable; no older run was substituted"
  }
}
```

The older run is unaffected — it is readable, which is what makes "no older run
was substituted" an observation rather than a hope.

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=1 | stats count()"}'
HTTP 200
{
  "execution": {
    "started_at": "2026-09-21T17:16:21.255641616Z",
    "duration_ms": 2
  },
  "columns": [
    {
      "name": "count"
    }
  ],
  "rows": [
    [
      3
    ]
  ],
  "truncated": false,
  "pagination": {
    "limit": 25000,
    "offset": 0,
    "returned": 1
  }
}
```

## The control is unchanged throughout

A genuine zero-row success still answers as an empty typed result, with its
column names, and still counts zero. Absence of a file and absence of rows stay
two different answers.

```console
$ curl -sS -k "$UP/api/v1/saved/2/runs/3" -H "Authorization: Bearer $KEY"
HTTP 200
{
  "id": 3,
  "query": "experiment_run=\"run-1790010770915-7a1a8dafc8\" earliest=\"2026-01-01T00:00:00Z\" latest=\"2026-01-02T00:00:00Z\" experiment_seq<0 | fields experiment_seq, status",
  "status": "success",
  "started_at": "2026-09-21T17:16:20.710208+00:00",
  "finished_at": "2026-09-21T17:16:20.713623+00:00",
  "duration_ms": 2,
  "row_count": 0,
  "result": {
    "columns": [
      {
        "name": "experiment_seq"
      },
      {
        "name": "status"
      }
    ],
    "rows": []
  }
}
```

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved zero_row_control run=latest | stats count()"}'
HTTP 200
{
  "execution": {
    "started_at": "2026-09-21T17:16:21.354432968Z",
    "duration_ms": 2
  },
  "columns": [
    {
      "name": "count"
    }
  ],
  "rows": [
    [
      0
    ]
  ],
  "truncated": false,
  "pagination": {
    "limit": 25000,
    "offset": 0,
    "returned": 1
  }
}
```

## The file comes back

```console
$ mv "$ASIDE" "$DATA/scheduled/run_2.parquet"
```

Nothing was persisted about the absence, so nothing has to be reconciled: the
next read stats the file and finds it.

```console
$ curl -sS -k "$UP/api/v1/saved/1/runs/2" -H "Authorization: Bearer $KEY"
HTTP 200
{
  "id": 2,
  "query": "experiment_run=\"run-1790010770915-7a1a8dafc8\" earliest=\"2026-01-01T00:00:00Z\" latest=\"2026-01-02T00:00:00Z\" experiment_seq<3 | fields experiment_seq, status | sort experiment_seq",
  "status": "success",
  "started_at": "2026-09-21T17:16:20.617040+00:00",
  "finished_at": "2026-09-21T17:16:20.628598+00:00",
  "duration_ms": 2,
  "row_count": 3,
  "result_path": "scheduled/run_2.parquet",
  "result": {
    "columns": [
      {
        "name": "experiment_seq"
      },
      {
        "name": "status"
      }
    ],
    "rows": [
      [
        0,
        200
      ],
      [
        1,
        200
      ],
      [
        2,
        200
      ]
    ]
  }
}
```

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=latest | table experiment_seq, status"}'
HTTP 200
{
  "execution": {
    "started_at": "2026-09-21T17:16:21.452629109Z",
    "duration_ms": 2
  },
  "columns": [
    {
      "name": "experiment_seq"
    },
    {
      "name": "status"
    }
  ],
  "rows": [
    [
      0,
      200
    ],
    [
      1,
      200
    ],
    [
      2,
      200
    ]
  ],
  "truncated": false,
  "pagination": {
    "limit": 25000,
    "offset": 0,
    "returned": 3
  }
}
```

```console
$ curl -sS -k -X POST "$UP/api/v1/query" \
    -H "Authorization: Bearer $KEY" -H "Content-Type: application/json" \
    -d '{"query":"| from saved unavailable_demo run=all | stats count()"}'
HTTP 200
{
  "execution": {
    "started_at": "2026-09-21T17:16:21.502403402Z",
    "duration_ms": 2
  },
  "columns": [
    {
      "name": "count"
    }
  ],
  "rows": [
    [
      6
    ]
  ],
  "truncated": false,
  "pagination": {
    "limit": 25000,
    "offset": 0,
    "returned": 1
  }
}
```

## The stack

The capture ran inside the experiment runner's hold. The runner's own scenario
— ingest, browser search, live tail, compaction, restart — passed before the
hold opened, and the runner tore its container, processes and secrets down
afterwards.

```console
$ jq -r '.status' "$RUN/report.json"
passed
$ jq -c '.cleanup' "$RUN/report.json"
{"processes":true,"container":true,"secrets":true}
```
