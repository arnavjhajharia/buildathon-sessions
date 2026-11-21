#!/bin/sh
# Builds bin/sessions-linux-x86_64, the static binary the root Dockerfile ships.
# Toolchain: Rust 1.100.0 with the x86_64-unknown-linux-musl target, linked by
# rust-lld (no C toolchain needed), for x86-64-v3 (AVX2; the scorers are Intel
# Xeon Cascade Lake or newer). Dockerfile.source does the same inside docker.
set -eu
cd "$(dirname "$0")/.."
T=x86_64-unknown-linux-musl
case "$(rustc --version)" in
  "rustc 1.100.0 "*) ;;
  *) echo "expected rustc 1.100.0, got: $(rustc --version)" >&2; exit 1 ;;
esac
RUSTFLAGS="-C linker=rust-lld -C target-cpu=x86-64-v3" \
  cargo build --release --target "$T"
mkdir -p bin
cp "target/$T/release/sessions" bin/sessions-linux-x86_64
shasum -a 256 bin/sessions-linux-x86_64 | tee bin/sessions-linux-x86_64.sha256
