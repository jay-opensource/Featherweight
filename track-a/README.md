# Track A: Ingest

The task, the input files and the exact output format are in the
[spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md). In short:
read `requests.log` (239 MB, about 3.1 million lines of timestamp, method, endpoint, status and
latency) and write, for every hour and endpoint, the request count, the nearest-rank p50, p95 and
p99 latency, the 5xx count and the 5xx rate in parts per million.

Board result for the shipped code (d9663e9): **19.76 ms, 13.08 MB, 439 lines, 85.95 points**
(run-eaa93041cda1; the same code ran 19.66-20.77 ms and 12.86-13.64 MB in four other runs). The
reference takes 32.047 s and 228 MB. [PERF.md](PERF.md) has every step with its run ID and the experiments
that lost.

## Approach

**Mapping the input.** The log is mapped read-only, so the page cache that already holds it is not
charged to us. At least 4 KB of zeros sit just past its end, so the parser may always read 64 bytes
ahead without a bounds check.

**Splitting the work.** The file is cut into line-aligned chunks of 2 MB (the last 24 MB into 512 KB
chunks, so the threads finish together), and threads claim them in file order from a shared
counter, but never more than 32 chunks past the oldest one not yet parsed.

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

**Storing records.** Each record goes to the memory region of its hour, in blocks of 1,024 words (one
page, so each page has one writer) handed out by a per-hour counter. The regions are one large reservation, charged only where
touched. A thread that finishes a chunk gives that chunk's input pages back to the kernel at once,
so unmapping the 239 MB is spread over all threads instead of left to exit on one core.

**Statistics, hour by hour, during the parse.** An hour is complete once the parsed prefix of the
file (the chunks from the start that are all done) holds a line of the hour two later. Threads
take a complete hour before the next chunk, and:
1. count the records per endpoint (the 5xx count rides in the same 64-bit counter);
2. scatter the latencies so each endpoint's are contiguous and in id order; the second words of
   large latencies have id 0, so they land after the real groups like padding;
3. find the nearest-rank p50, p95 and p99. For groups of up to 32 (90% of them) every value
   counts, in AVX2 lanes, the values below and above it, which gives each rank without a branch.
   Larger groups use three narrowing quickselects;
4. keep six numbers per group in a small per-hour table, give the hour's records back to the
   kernel, and record the exact length of the hour's output.

This is exact. If a line turns up in an hour already summarised (not in the generated data), the
run starts over with no early release. At most one chunk in 32 is in work past the oldest unparsed
one (a thread past that sleeps 50 µs), so the records of unfinished hours cannot pile up.

**Writing the output.** When the last chunk is parsed, one thread sorts the endpoint ids by name,
releases the input's page tables, and maps `summary.txt` (opened read-write, so the mapping can be
shared) at an upper bound of its size; the file is cut to the exact size at the end. Each thread
formats an hour straight into it, two digits at a time, as soon as that hour and all hours before
it are summarised. Each hour's output pages are dropped from our page tables once written; the data
stays in the file. The process then exits without freeing anything.

**Why memory stays low.** `/out` is a tmpfs, so the 10.1 MB answer counts as memory. Summarising
hours during the parse means the 12.55 MB of records are never all alive at once, so the peak is
close to the output plus what the runtime already holds (memory use at the board went from 20.5 MB
to 13.1 MB). Every big buffer is our own `mmap` marked `MADV_NOHUGEPAGE`: the scoring box backs
memory with 2 MB pages, and `memory.peak` would charge a whole one for its first touched byte.
Thread stacks are 256 KB and malloc keeps one arena.

**Threads.** One pool of threads runs the parse, the statistics and the formatting, with one
barrier between the parse and the output.

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

- Overlap the formatting of the first hours with the last two or three summaries (the tail is about
  one hour's summary, ~0.6 ms on the box).
- A software-pipelined interner: hash the next line and prefetch its slot while this one is
  compared.
- Smaller chunks at the end of the parse still, to shrink the last thread's idle time.
- Find p50, p95 and p99 of the largest groups from one log-bucket histogram instead of three
  quickselects.
