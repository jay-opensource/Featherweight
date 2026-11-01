# Track A: Ingest

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

**Mapping the input.** The log is mapped read-only, so the page cache that holds it is not charged to us. Just past its end sit a few zero pages, so the parser can always read 64 bytes ahead without checking bounds.

**Parsing.** Twelve threads claim pairs of adjacent newline-aligned chunks from a shared counter. Each thread runs two cursors, one per chunk, parsing a line from each in turn so that their dependency chains overlap. Every line becomes one 4-byte record:
- 11 bits of endpoint id;
- one bit for a 5xx status;
- a 20-bit latency, with a second word after the rare latency that does not fit.

Nothing on a line is searched byte by byte:
- **The hour** comes from one 8-byte load of the timestamp's "MM-DDTHH", the digit pairs combined in a single multiply.
- **The method and endpoint ends** come from one AVX2 compare of the 64 bytes after the timestamp: the first separator ends the method, the second ends the endpoint.
- **The latency's length and value** come from a SWAR test for the first non-digit and three multiplies.

**Endpoint ids.** All threads share one open-addressing table. A CRC32 of the endpoint's first 16 and last 8 bytes picks the slot. The full name is then compared byte for byte against a copy kept in a small arena, padded to whole 64-byte lines, so a lookup usually reads one cache line. A new name is inserted under a lock, which happens 1,842 times in total.

**Storing records.** Each record goes to a memory region of its own hour, in small blocks handed out by a per-hour counter. Every region is part of one large reservation that is charged only where it is touched. A thread that has finished a chunk gives that chunk's input pages back to the kernel at once. This spreads the work of unmapping 239 MB across all threads instead of leaving it to exit on one core.

**Statistics, hour by hour.** When the parse is done, one thread sorts the endpoint names for the output while the others already start on the hours. The hours are handed out largest first, and for each hour a thread:
1. counts the records per endpoint (the 5xx count rides along in the same 64-bit counter);
2. scatters the latencies so each endpoint's are contiguous;
3. finds the nearest-rank p50, p95 and p99 by sorting groups of up to 32 and by three narrowing quickselects above that;
4. writes five numbers per endpoint over the hour's now-dead records and returns the rest of the region to the kernel.

**Writing the output.** The output's exact size is known at this point. The file is sized and mapped, and each thread formats whole hours straight into it, in name order, then drops those pages from its page table. A final `fdatasync` makes the mapped output safe on a write-back filesystem; on tmpfs it costs nothing.

**Why memory stays low.** `/out` is a tmpfs, so the 10 MB answer counts as memory. The order of the phases keeps the records, the statistics and the output from all being alive at once.

**Threads.** One pool of threads runs all three phases, with a barrier between them.

## Dependencies

None: Rust standard library only.

The build enables AVX2, BMI, LZCNT, POPCNT and SSE4.2 in `.cargo/config.toml`, the instruction sets of the scoring box's i5-13500, and links statically.

## With another 24 hours

- Stream finished hours during the parse: the data never lags more than one hour behind. This needs an exact disorder check with a full fallback, and would cut about 3 MB at the peak.
- Find p50, p95 and p99 of the largest groups from one log-bucket histogram, instead of three quickselects.
- Overlap the formatting of early hours with the statistics of late ones.
