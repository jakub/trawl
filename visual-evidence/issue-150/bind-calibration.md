# Bind-time calibration for the 512 lateral-expansion budget

ADR-0024 keeps `MAX_LATERAL_EXPANSION = 512` as a number derived from
structure, and owes one measured claim in exchange: on the calibration
host, a query the checker admits still prepares fast. This is that run.

Ten cases, each at or near a cap for one family of shapes. Every case runs
in its own process under an external `timeout 5`, warms the connection with
one discarded prepare, then measures five. The pass condition is per
sample, not per average: all five must finish under a second. The slowest
single sample in this run was 158.1 ms, on a query whose score is 24.

What this is not: a throughput benchmark, a CI gate, or a promise about
other hosts. A prepare here binds against a two-row fixture parquet the
harness writes itself, so the number is the cost of the SQL, not of a
corpus.

## Host and build identity

| | |
| --- | --- |
| CPU | AMD Ryzen 7 7800X3D 8-Core Processor (16 threads) |
| kernel | Linux 7.2.3-1-cachyos x86_64 |
| repo | trawl, branch `feat/issue-150-bound-duckdb-bind-time-lateral-alias` |
| commit at measurement | `edb9df43` plus the working tree committed as this checkpoint (the probe, the harness and this file) |
| build profile | `test` (dev: unoptimized, debuginfo). DuckDB itself is a bundled C++ library built by its own crate, so the Rust profile moves the measurement very little |
| rustc / cargo | 1.98.0 (88d9e12ae 2026-08-18) / 1.98.0 (797e8a9bc 2026-08-05) |
| duckdb / libduckdb-sys | 1.10505.0 / 1.10505.0 (Cargo.lock), workspace constraint `>=1.10500, <1.10600` |
| engine `version()` | v1.5.5 (source id d8cdaa33fd) |

## How to reproduce

```sh
# the whole table, one process per case, 5s watchdog each
scripts/bind-calibration

# one case by hand, same watchdog
cargo test -p trawl-engine --test bind_calibration --no-run
timeout 5 target/debug/deps/bind_calibration-<hash> \
  --exact --ignored --nocapture severity_full_ladder_set_one_link

# the executed claims behind the numbers (CI, not timed)
cargo nextest run -p trawl-engine --test duckdb_probe
```

The harness is `crates/trawl-engine/tests/bind_calibration.rs`, its cases
are `#[ignore]`d so a shared CI runner never turns a wall clock into a
flake, and each one re-checks admission before it measures: a case that
stopped being admitted would otherwise quietly calibrate a query the
server refuses.

## Fixture

Written by the harness into a temp dir, dropped at the end of the case:

```sql
COPY (SELECT * FROM (VALUES
  (TIMESTAMP '2026-01-01 00:00:00', 'nginx', 'h1', 200, 17, 'boom'),
  (TIMESTAMP '2026-01-01 00:01:00', 'nginx', 'h2', 500,  9, 'fine'))
  AS t(_time, service, host, status, _severity, message))
TO 'bind.parquet' (FORMAT PARQUET)
```

Six declared columns, two rows, no production data anywhere near it.

## Results

Scores are `check_pipeline_complexity_with_stats(...).lateral_delta`, the
same number the refusal is decided on. The budget is 512.

| case | lateral delta | stages | SQL bytes | s1 ms | s2 ms | s3 ms | s4 ms | s5 ms | worst ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| addition-linear-22 | 506 | 1 | 897 | 2.8 | 2.7 | 2.7 | 2.8 | 2.7 | 2.8 |
| addition-doubling-6 | 480 | 1 | 436 | 28.8 | 28.9 | 28.8 | 28.9 | 28.7 | 28.9 |
| stats-scalar-31 | 496 | 1 | 725 | 97.8 | 98.1 | 98.1 | 98.5 | 98.3 | 98.5 |
| timechart-scalar-31 | 496 | 1 | 951 | 97.8 | 98.1 | 98.2 | 98.1 | 98.2 | 98.2 |
| severity-ordered-2-links | 419 | 1 | 2768 | 25.3 | 25.2 | 25.2 | 25.1 | 25.1 | 25.3 |
| severity-set-1-point-2-links | 421 | 1 | 2766 | 38.2 | 38.2 | 38.2 | 38.2 | 38.3 | 38.3 |
| severity-set-12-points-1-link | 24 | 1 | 15147 | 158.1 | 156.8 | 157.2 | 156.1 | 156.1 | 158.1 |
| split-let-24 | 0 | 24 | 4012 | 31.4 | 30.7 | 30.1 | 30.1 | 30.0 | 31.4 |
| stages-128 | 0 | 128 | 21274 | 9.2 | 9.1 | 9.2 | 9.0 | 9.1 | 9.2 |
| alias-reuse-256 | 512 | 1 | 8735 | 19.3 | 19.1 | 18.9 | 19.1 | 19.1 | 19.3 |

Watchdog outcomes: all ten cases completed inside the 5 s window, none was
cut off, so no family was stopped early.

Two things worth reading off that table. The score does not predict the
time: `severity-set-12-points-1-link` scores 24 and is the slowest case by
60 %, because a single severity set writes its subject twelve times and
the budget bounds substitution, not the size of one rendering. And the
remedy works: 24 doublings are refused in one `| let` (score 988) and cost
31 ms once split across 24 stages, where the score is 0.

## Exact queries

### addition-linear-22

```
* | let a0 = status + 1, a1 = a0 + 1, a2 = a1 + 1, a3 = a2 + 1, a4 = a3 + 1, a5 = a4 + 1, a6 = a5 + 1, a7 = a6 + 1, a8 = a7 + 1, a9 = a8 + 1, a10 = a9 + 1, a11 = a10 + 1, a12 = a11 + 1, a13 = a12 + 1, a14 = a13 + 1, a15 = a14 + 1, a16 = a15 + 1, a17 = a16 + 1, a18 = a17 + 1, a19 = a18 + 1, a20 = a19 + 1, a21 = a20 + 1, a22 = a21 + 1
```

### addition-doubling-6

```
* | let a0 = status + status, a1 = a0 + a0, a2 = a1 + a1, a3 = a2 + a2, a4 = a3 + a3, a5 = a4 + a4, a6 = a5 + a5
```

### stats-scalar-31

```
* | stats count() as n0, abs(n0) as n1, abs(n1) as n2, abs(n2) as n3, abs(n3) as n4, abs(n4) as n5, abs(n5) as n6, abs(n6) as n7, abs(n7) as n8, abs(n8) as n9, abs(n9) as n10, abs(n10) as n11, abs(n11) as n12, abs(n12) as n13, abs(n13) as n14, abs(n14) as n15, abs(n15) as n16, abs(n16) as n17, abs(n17) as n18, abs(n18) as n19, abs(n19) as n20, abs(n20) as n21, abs(n21) as n22, abs(n22) as n23, abs(n23) as n24, abs(n24) as n25, abs(n25) as n26, abs(n26) as n27, abs(n27) as n28, abs(n28) as n29, abs(n29) as n30, abs(n30) as n31
```

### timechart-scalar-31

```
* | timechart span=1h count() as n0, abs(n0) as n1, abs(n1) as n2, abs(n2) as n3, abs(n3) as n4, abs(n4) as n5, abs(n5) as n6, abs(n6) as n7, abs(n7) as n8, abs(n8) as n9, abs(n9) as n10, abs(n10) as n11, abs(n11) as n12, abs(n12) as n13, abs(n13) as n14, abs(n14) as n15, abs(n15) as n16, abs(n16) as n17, abs(n17) as n18, abs(n18) as n19, abs(n19) as n20, abs(n20) as n21, abs(n21) as n22, abs(n22) as n23, abs(n23) as n24, abs(n24) as n25, abs(n25) as n26, abs(n26) as n27, abs(n27) as n28, abs(n28) as n29, abs(n29) as n30, abs(n30) as n31
```

### severity-ordered-2-links

```
* | let a0 = _severity + _severity, a1 = sev(a0) >= "error", a2 = sev(a1) >= "error"
```

### severity-set-1-point-2-links

```
* | let a0 = _severity + _severity, a1 = sev(a0) in (1), a2 = sev(a1) in (1)
```

### severity-set-12-points-1-link

```
* | let a0 = _severity + _severity, a1 = sev(a0) in (1,3,5,7,9,11,13,15,17,19,21,23)
```

### split-let-24

```
* | let a0 = status + status | let a1 = a0 + a0 | let a2 = a1 + a1 | let a3 = a2 + a2 | let a4 = a3 + a3 | let a5 = a4 + a4 | let a6 = a5 + a5 | let a7 = a6 + a6 | let a8 = a7 + a7 | let a9 = a8 + a8 | let a10 = a9 + a9 | let a11 = a10 + a10 | let a12 = a11 + a11 | let a13 = a12 + a12 | let a14 = a13 + a13 | let a15 = a14 + a14 | let a16 = a15 + a15 | let a17 = a16 + a16 | let a18 = a17 + a17 | let a19 = a18 + a18 | let a20 = a19 + a19 | let a21 = a20 + a20 | let a22 = a21 + a21 | let a23 = a22 + a22
```

### stages-128

```
* | let s0 = status + 0 | let s1 = status + 1 | let s2 = status + 2 | let s3 = status + 3 | let s4 = status + 4 | let s5 = status + 5 | let s6 = status + 6 | let s7 = status + 7 | let s8 = status + 8 | let s9 = status + 9 | let s10 = status + 10 | let s11 = status + 11 | let s12 = status + 12 | let s13 = status + 13 | let s14 = status + 14 | let s15 = status + 15 | let s16 = status + 16 | let s17 = status + 17 | let s18 = status + 18 | let s19 = status + 19 | let s20 = status + 20 | let s21 = status + 21 | let s22 = status + 22 | let s23 = status + 23 | let s24 = status + 24 | let s25 = status + 25 | let s26 = status + 26 | let s27 = status + 27 | let s28 = status + 28 | let s29 = status + 29 | let s30 = status + 30 | let s31 = status + 31 | let s32 = status + 32 | let s33 = status + 33 | let s34 = status + 34 | let s35 = status + 35 | let s36 = status + 36 | let s37 = status + 37 | let s38 = status + 38 | let s39 = status + 39 | let s40 = status + 40 | let s41 = status + 41 | let s42 = status + 42 | let s43 = status + 43 | let s44 = status + 44 | let s45 = status + 45 | let s46 = status + 46 | let s47 = status + 47 | let s48 = status + 48 | let s49 = status + 49 | let s50 = status + 50 | let s51 = status + 51 | let s52 = status + 52 | let s53 = status + 53 | let s54 = status + 54 | let s55 = status + 55 | let s56 = status + 56 | let s57 = status + 57 | let s58 = status + 58 | let s59 = status + 59 | let s60 = status + 60 | let s61 = status + 61 | let s62 = status + 62 | let s63 = status + 63 | let s64 = status + 64 | let s65 = status + 65 | let s66 = status + 66 | let s67 = status + 67 | let s68 = status + 68 | let s69 = status + 69 | let s70 = status + 70 | let s71 = status + 71 | let s72 = status + 72 | let s73 = status + 73 | let s74 = status + 74 | let s75 = status + 75 | let s76 = status + 76 | let s77 = status + 77 | let s78 = status + 78 | let s79 = status + 79 | let s80 = status + 80 | let s81 = status + 81 | let s82 = status + 82 | let s83 = status + 83 | let s84 = status + 84 | let s85 = status + 85 | let s86 = status + 86 | let s87 = status + 87 | let s88 = status + 88 | let s89 = status + 89 | let s90 = status + 90 | let s91 = status + 91 | let s92 = status + 92 | let s93 = status + 93 | let s94 = status + 94 | let s95 = status + 95 | let s96 = status + 96 | let s97 = status + 97 | let s98 = status + 98 | let s99 = status + 99 | let s100 = status + 100 | let s101 = status + 101 | let s102 = status + 102 | let s103 = status + 103 | let s104 = status + 104 | let s105 = status + 105 | let s106 = status + 106 | let s107 = status + 107 | let s108 = status + 108 | let s109 = status + 109 | let s110 = status + 110 | let s111 = status + 111 | let s112 = status + 112 | let s113 = status + 113 | let s114 = status + 114 | let s115 = status + 115 | let s116 = status + 116 | let s117 = status + 117 | let s118 = status + 118 | let s119 = status + 119 | let s120 = status + 120 | let s121 = status + 121 | let s122 = status + 122 | let s123 = status + 123 | let s124 = status + 124 | let s125 = status + 125 | let s126 = status + 126 | let s127 = status + 127
```

### alias-reuse-256

```
* | let base = status + 1, x1 = base + 1, x2 = base + 2, x3 = base + 3, x4 = base + 4, x5 = base + 5, x6 = base + 6, x7 = base + 7, x8 = base + 8, x9 = base + 9, x10 = base + 10, x11 = base + 11, x12 = base + 12, x13 = base + 13, x14 = base + 14, x15 = base + 15, x16 = base + 16, x17 = base + 17, x18 = base + 18, x19 = base + 19, x20 = base + 20, x21 = base + 21, x22 = base + 22, x23 = base + 23, x24 = base + 24, x25 = base + 25, x26 = base + 26, x27 = base + 27, x28 = base + 28, x29 = base + 29, x30 = base + 30, x31 = base + 31, x32 = base + 32, x33 = base + 33, x34 = base + 34, x35 = base + 35, x36 = base + 36, x37 = base + 37, x38 = base + 38, x39 = base + 39, x40 = base + 40, x41 = base + 41, x42 = base + 42, x43 = base + 43, x44 = base + 44, x45 = base + 45, x46 = base + 46, x47 = base + 47, x48 = base + 48, x49 = base + 49, x50 = base + 50, x51 = base + 51, x52 = base + 52, x53 = base + 53, x54 = base + 54, x55 = base + 55, x56 = base + 56, x57 = base + 57, x58 = base + 58, x59 = base + 59, x60 = base + 60, x61 = base + 61, x62 = base + 62, x63 = base + 63, x64 = base + 64, x65 = base + 65, x66 = base + 66, x67 = base + 67, x68 = base + 68, x69 = base + 69, x70 = base + 70, x71 = base + 71, x72 = base + 72, x73 = base + 73, x74 = base + 74, x75 = base + 75, x76 = base + 76, x77 = base + 77, x78 = base + 78, x79 = base + 79, x80 = base + 80, x81 = base + 81, x82 = base + 82, x83 = base + 83, x84 = base + 84, x85 = base + 85, x86 = base + 86, x87 = base + 87, x88 = base + 88, x89 = base + 89, x90 = base + 90, x91 = base + 91, x92 = base + 92, x93 = base + 93, x94 = base + 94, x95 = base + 95, x96 = base + 96, x97 = base + 97, x98 = base + 98, x99 = base + 99, x100 = base + 100, x101 = base + 101, x102 = base + 102, x103 = base + 103, x104 = base + 104, x105 = base + 105, x106 = base + 106, x107 = base + 107, x108 = base + 108, x109 = base + 109, x110 = base + 110, x111 = base + 111, x112 = base + 112, x113 = base + 113, x114 = base + 114, x115 = base + 115, x116 = base + 116, x117 = base + 117, x118 = base + 118, x119 = base + 119, x120 = base + 120, x121 = base + 121, x122 = base + 122, x123 = base + 123, x124 = base + 124, x125 = base + 125, x126 = base + 126, x127 = base + 127, x128 = base + 128, x129 = base + 129, x130 = base + 130, x131 = base + 131, x132 = base + 132, x133 = base + 133, x134 = base + 134, x135 = base + 135, x136 = base + 136, x137 = base + 137, x138 = base + 138, x139 = base + 139, x140 = base + 140, x141 = base + 141, x142 = base + 142, x143 = base + 143, x144 = base + 144, x145 = base + 145, x146 = base + 146, x147 = base + 147, x148 = base + 148, x149 = base + 149, x150 = base + 150, x151 = base + 151, x152 = base + 152, x153 = base + 153, x154 = base + 154, x155 = base + 155, x156 = base + 156, x157 = base + 157, x158 = base + 158, x159 = base + 159, x160 = base + 160, x161 = base + 161, x162 = base + 162, x163 = base + 163, x164 = base + 164, x165 = base + 165, x166 = base + 166, x167 = base + 167, x168 = base + 168, x169 = base + 169, x170 = base + 170, x171 = base + 171, x172 = base + 172, x173 = base + 173, x174 = base + 174, x175 = base + 175, x176 = base + 176, x177 = base + 177, x178 = base + 178, x179 = base + 179, x180 = base + 180, x181 = base + 181, x182 = base + 182, x183 = base + 183, x184 = base + 184, x185 = base + 185, x186 = base + 186, x187 = base + 187, x188 = base + 188, x189 = base + 189, x190 = base + 190, x191 = base + 191, x192 = base + 192, x193 = base + 193, x194 = base + 194, x195 = base + 195, x196 = base + 196, x197 = base + 197, x198 = base + 198, x199 = base + 199, x200 = base + 200, x201 = base + 201, x202 = base + 202, x203 = base + 203, x204 = base + 204, x205 = base + 205, x206 = base + 206, x207 = base + 207, x208 = base + 208, x209 = base + 209, x210 = base + 210, x211 = base + 211, x212 = base + 212, x213 = base + 213, x214 = base + 214, x215 = base + 215, x216 = base + 216, x217 = base + 217, x218 = base + 218, x219 = base + 219, x220 = base + 220, x221 = base + 221, x222 = base + 222, x223 = base + 223, x224 = base + 224, x225 = base + 225, x226 = base + 226, x227 = base + 227, x228 = base + 228, x229 = base + 229, x230 = base + 230, x231 = base + 231, x232 = base + 232, x233 = base + 233, x234 = base + 234, x235 = base + 235, x236 = base + 236, x237 = base + 237, x238 = base + 238, x239 = base + 239, x240 = base + 240, x241 = base + 241, x242 = base + 242, x243 = base + 243, x244 = base + 244, x245 = base + 245, x246 = base + 246, x247 = base + 247, x248 = base + 248, x249 = base + 249, x250 = base + 250, x251 = base + 251, x252 = base + 252, x253 = base + 253, x254 = base + 254, x255 = base + 255, x256 = base + 256
```

## Refused shapes, for contrast

These are never prepared. The checker stops them, so DuckDB never sees
them, and the numbers below are scores rather than timings.

| DSL | score | verdict |
| --- | ---: | --- |
| 24 doublings in one `\| let` (`a{i} = a{i-1} + a{i-1}`) | 988 | refused, names stage 1 and the output `a7` |
| 23 single-reference links in one `\| let` | 552 | refused |
| 32 scalar `stats` outputs chained through `abs()` | 528 | refused |
| 2 links of `sev(x) in (1,3,…,23)` | 60624 | refused |
| 8 links of the same, the chain ADR-0024 describes | 60624 | refused, never emitted |
| 257 outputs each naming one earlier output | 514 | refused |

The eight-link severity chain is the one shape this work must never
measure: at twelve subject copies per link it is 12^8 copies of the seed.
`a_severity_chain_multiplies_its_subject_twelve_times_per_link` in
`duckdb_probe.rs` measures the same mechanism at one and two links (24 then
288 copies of the seed in DuckDB's parse tree, from 15 KB then 191 KB of
SQL) and then asserts the checker refuses three and eight, which is why
nothing deeper is ever built.

## Historical timings, under the OLD metric

These are the numbers from the original #150 prep, kept for provenance.
They were measured before the corrected accounting existed, on the
substituted-DSL-node count that ADR-0024 replaced, and they do not
calibrate anything here. Do not compare them line by line with the table
above: the chains are not the same shape and the score is not the same
score.

| doubling depth | prepare (historical) |
| ---: | --- |
| 8 | 0.31 s |
| 12 | 2.9 s |
| 14 | 12.7 s |
| 16 | over 30 s |

The corrected metric refuses at depth 7, so the historical depth-8 row is
already outside what the server will now emit.
