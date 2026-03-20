# trawl TUI autocomplete suggestions

canonical list of all ghost-text autocomplete candidates. source of truth
is `crates/trawl-core/src/parser/suggest.rs` — this file is a human-readable
reference only.

## pipe stages (after `|`)

| stage | description |
|-------|-------------|
| `stats` | aggregate with optional grouping |
| `timechart` | time-bucketed aggregation |
| `where` | filter on computed values |
| `sort` | order results (`-` prefix = descending) |
| `limit` | cap result count |
| `head` | alias for limit |
| `tail` | last N rows |
| `let` | computed/derived fields |
| `eval` | alias for let |
| `extract` | regex field extraction |
| `rex` | alias for extract |
| `table` | select output columns |
| `fields` | alias for table |
| `top` | most frequent values |
| `rare` | least frequent values |
| `dedup` | remove duplicates |
| `drop` | exclude columns |
| `rename` | rename columns |
| `pivot` | pivot table transformation |
| `sample` | random sample |
| `eventstats` | inline aggregation (preserves rows) |

## aggregate functions (inside `stats`, `timechart`, `eventstats`)

| function | args | description |
|----------|------|-------------|
| `count()` | 0-1 | row count (or non-null count) |
| `avg(field)` | 1 | mean |
| `sum(field)` | 1 | total |
| `min(field)` | 1 | minimum |
| `max(field)` | 1 | maximum |
| `dc(field)` | 1 | distinct count |
| `distinct_count(field)` | 1 | alias for dc |
| `p50(field)` | 1 | median (50th percentile) |
| `p90(field)` | 1 | 90th percentile |
| `p95(field)` | 1 | 95th percentile |
| `p99(field)` | 1 | 99th percentile |
| `first(field)` | 1 | first value |
| `last(field)` | 1 | last value |
| `values(field)` | 1 | list of distinct values |
| `list(field)` | 1 | alias for values |
| `median(field)` | 1 | median value |
| `stddev(field)` | 1 | standard deviation |

## scalar functions (inside `let`/`eval`, `where`, expressions)

| function | args | description |
|----------|------|-------------|
| `lower(field)` | 1 | lowercase |
| `upper(field)` | 1 | uppercase |
| `length(field)` | 1 | string length |
| `len(field)` | 1 | alias for length |
| `coalesce(a, b, ...)` | 1+ | first non-null value |
| `if(cond, then, else)` | 3 | ternary conditional |
| `replace(field, old, new)` | 3 | string replacement |
| `substr(field, start[, len])` | 2-3 | substring extraction |
| `trim(field)` | 1 | strip whitespace |
| `ltrim(field)` | 1 | strip leading whitespace |
| `rtrim(field)` | 1 | strip trailing whitespace |
| `isnull(x)` | 1 | null check |
| `isnotnull(x)` | 1 | not-null check |
| `abs(x)` | 1 | absolute value |
| `ceil(x)` | 1 | round up |
| `ceiling(x)` | 1 | alias for ceil |
| `floor(x)` | 1 | round down |
| `round(x[, n])` | 1-2 | round to n decimal places |
| `now()` | 0 | current timestamp |
| `typeof(x)` | 1 | value type name |
| `tonumber(x)` | 1 | cast to numeric |
| `tostring(x)` | 1 | cast to string |
| `contains(field, substr)` | 2 | substring check |
| `startswith(field, prefix)` | 2 | prefix check |
| `endswith(field, suffix)` | 2 | suffix check |
| `split(field, delim, idx)` | 3 | split and index |
| `concat(a, b, ...)` | 1+ | concatenate strings |
| `date_part(part, ts)` | 2 | extract date component |
| `date_trunc(part, ts)` | 2 | truncate timestamp |
| `date_diff(part, ts1, ts2)` | 3 | timestamp difference |
| `strftime(ts, fmt)` | 2 | format timestamp |
| `strptime(str, fmt)` | 2 | parse timestamp |
| `case(cond, val, ...)` | 2+ | case expression |
| `json(field, path)` | 2 | extract JSON string value |
| `json_extract(field, path)` | 2 | extract JSON value |
| `json_extract_string(field, path)` | 2 | alias for json |
| `json_valid(field)` | 1 | JSON validity check |
| `json_keys(field)` | 1 | JSON object keys |
| `json_array_length(field)` | 1 | JSON array length |

## field names

field name suggestions come from the server's schema cache (`/api/v1/schema`)
and are populated at TUI startup. these are dynamic per-deployment.

## context keywords

| keyword | suggested in |
|---------|-------------|
| `by` | after aggregation in stats/timechart |
| `as` | after field in rename/stats alias |
| `from` | after pattern in extract/rex |
| `on` | after aggregation in pivot |
