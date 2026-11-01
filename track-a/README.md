# Track A: Ingest

The task, the input files and the exact output format are in the
[spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md). In short:
read `requests.log` (239 MB, about 3.1 million lines of timestamp, method, endpoint, status and
latency) and write, for every hour and endpoint, the request count, the nearest-rank p50, p95 and
p99 latency, the 5xx count and the 5xx rate in parts per million.

Board result: best **18.92 ms, 20.45 MB, 440 lines, 84.48 points** (run-ac86c3ef1a30); the shipped
code (bbee872, the same code paths in 437 lines) ran 19.15 ms (run-9237e503e115). The reference
takes 32.047 s and 228 MB. [PERF.md](PERF.md) has every step with its run ID and the experiments
that lost.

## Approach

**Mapping the input.** The log is mapped read-only, so the page cache that already holds it is not
charged to us. At least 4 KB of zeros sit just past its end, so the parser may always read 64 bytes
ahead without a bounds check.

**Splitting the work.** The file is cut into 16 line-aligned chunks per thread, and threads claim
them from a shared counter in a strided order. Threads that run at the same time therefore work far
apart in the file: they fault in different page tables and fill different hours' regions.

**Parsing.** Every line becomes one 4-byte record: 11 bits of endpoint id, one bit for a 5xx status
and 20 bits of latency. A latency of 2^20 − 2048 or more keeps its low 11 bits in the record and
the rest in a second word, whose endpoint id is 0. Nothing on a line is searched byte by byte:
- **The hour** comes from one 8-byte load of the timestamp's "MM-DDTHH", the digit pairs combined
  in one multiply. Lines come in long runs of one hour, so the hour and its write cursor stay in
  registers until that word changes.
- **The method's and the endpoint's ends** come from one AVX2 compare of 64 bytes: the first
  separator ends the method, the second ends the endpoint.
- **The latency's length and value** come from a SWAR test for the first non-digit and three
  multiplies.
- **Two cache lines 1 KB ahead** are prefetched for every line. The hardware prefetcher stops at
  each 4 KB page, and page-cache pages are scattered in memory.

**Endpoint ids.** All threads share one open-addressing table of 8,192 slots. A CRC32 of the
endpoint's length, first 16 and last 8 bytes picks the slot. The name is then compared byte for
byte against its copy in a small arena, padded to whole 64-byte lines, so a lookup usually reads one
cache line. A new name is added under a lock, 1,842 times in all.

**Storing records.** Each record goes to the memory region of its hour, in blocks of 128 words
handed out by a per-hour counter. The regions are one large reservation, charged only where
touched. A thread that finishes a chunk gives that chunk's input pages back to the kernel at once,
so unmapping the 239 MB is spread over all threads instead of left to exit on one core.

**Statistics, hour by hour.** After a barrier, one thread sorts the endpoint ids by name and the
hours by size, largest first. Then each thread takes hours in turn and:
1. counts the records per endpoint (the 5xx count rides in the same 64-bit counter);
2. scatters the latencies so each endpoint's are contiguous and in name order; the second words of
   large latencies have id 0, so they land after the real groups like padding;
3. finds the nearest-rank p50, p95 and p99. For groups of up to 32 (90% of them) every value
   counts, in AVX2 lanes, the values below and above it, which gives each rank without a branch.
   Larger groups use three narrowing quickselects;
4. writes six numbers per group over the hour's dead records, returns the rest of the region to the
   kernel, and adds up the exact length of the hour's output.

**Writing the output.** After a second barrier the output's exact size is known. One thread sizes
and maps `summary.txt` (opened read-write, so the mapping can be shared), and each thread formats
whole hours straight into it, two digits at a time. Each hour's output pages are dropped from our
page tables once written; the data stays in the file. The process then exits without freeing
anything.

**Why memory stays low.** `/out` is a tmpfs, so the 10.1 MB answer counts as memory. The order of
the phases keeps the records, the statistics and the output from all being alive at once. Every big
buffer is our own `mmap` marked `MADV_NOHUGEPAGE`: the scoring box backs memory with 2 MB pages, and
`memory.peak` would charge a whole one for its first touched byte. Thread stacks are 256 KB and
malloc keeps one arena.

**Threads.** One pool of threads runs all three phases, with a barrier between them.

## Build and run

```
docker build -t featherweight/track-a track-a
docker run --rm --network=none --read-only -v "$PWD/data/a:/data:ro" --tmpfs /out featherweight/track-a
```

The binary reads `/data/requests.log` and writes `/out/summary.txt`. `FW_THREADS` overrides the
thread count. `tools/bench.py a --gate G1|G2|G3` builds, runs with the board's container flags and
checks the published hashes (see PERF.md).

## Files

| File | What it is |
|---|---|
| `src/main.rs` | The whole program: the parse, the interner, the per-hour statistics, the output |
| `Cargo.toml` | No dependencies; release profile with fat LTO, one codegen unit, `panic = "abort"` |
| `.cargo/config.toml` | Build flags: AVX2, BMI1/2, LZCNT, POPCNT, SSE4.2 (the i5-13500's instruction sets) and a static binary |
| `Dockerfile` | The board's two-stage Rust build, offline |
| `PERF.md` | Every board run, the measurements behind each choice, and what did not work |

## Dependencies

None: Rust standard library only.

## With another 24 hours

- Format each hour's text in the statistics pass, so the output phase becomes a copy (estimated
  −1.5%, +10 lines).
- A software-pipelined interner: hash the next line and prefetch its slot while this one is
  compared.
- Stream finished hours during the parse: the data never lags more than one hour behind. This needs
  an exact disorder check with a full fallback, and would cut about 2-3 MB at the peak.
- Find p50, p95 and p99 of the largest groups from one log-bucket histogram instead of three
  quickselects.
