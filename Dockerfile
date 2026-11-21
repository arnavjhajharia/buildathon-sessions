# Shopping Sessions: one static binary on an empty image.
# bin/sessions-linux-x86_64 is built from src/ by scripts/build-bin.sh (Rust
# 1.100.0, x86_64-unknown-linux-musl, x86-64-v3); Dockerfile.source builds the
# same thing from source inside docker. Shipping the binary keeps this build
# free of network access and makes the tested image byte-identical to the
# scored one.
FROM scratch
COPY bin/sessions-linux-x86_64 /sessions
USER 1000:1000
CMD ["/sessions"]
