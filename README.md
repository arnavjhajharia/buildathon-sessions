# Shopping Sessions engine

Turns the click-stream CSV logs in `/data` into the two daily Parquet tables
`/output/user_daily/` and `/output/campaign_daily/`, following `rules.md` of the
Databricks Shopping Sessions track.

## How it works

One Rust binary with no dependencies beyond the Rust standard library:

1. Links: parse every `links/*.csv` in parallel, union-find, name each person by
   the byte-wise smallest ID.
2. Events and clicks: memory-map every CSV, split into line-aligned chunks, and
   parse and validate in parallel with a hand-written parser for the dialect in
   the rules (whole-field quotes, `\r` as data, strict timestamp forms). IDs of
   up to 15 bytes are packed into a `u128` that keeps byte-wise order; longer IDs
   switch the whole run to a general byte-slice path.
3. Event dedup by `event_id` (smallest `ingest_ts` wins, before validation),
   partitioned by a hash of `event_id`.
4. Per person partition: sort by time, sessionize (gap over 30 min), credit each
   session to the latest click in the 7 days up to its start (tie: smallest
   `click_id`) and count assists.
5. A small built-in Parquet writer (PLAIN, uncompressed, `day` as DATE) writes
   one file per worker thread.

## Build and run

```sh
scripts/build-bin.sh                      # static x86-64 binary into bin/
docker build -t sessions .                # the scored image (copies bin/)
docker build -f Dockerfile.source -t sessions-src .   # the same, from source
cargo build --release && target/release/sessions DATA_DIR OUTPUT_DIR   # native
```

The image's `CMD` runs with no arguments and reads `DATA_DIR`, `OUTPUT_DIR` and
`NCPU` from the environment (defaults `/data`, `/output`, all cores).
`PROFILE=1` prints stage timings to stderr.

## Open-source software

- Rust standard library (MIT OR Apache-2.0). No other crates.
- Toolchain: Rust 1.100.0, `rust-lld`, musl (static linking).

## License

MIT, see `LICENSE`.
