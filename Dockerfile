FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        liblzo2-2 \
        liblzo2-dev \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /workspace

# The repository's Linux profile targets Zen 4 (`znver4`). Docker Desktop's
# Linux VM may expose an older virtual CPU, so use the portable x86-64 baseline
# unless a caller explicitly overrides RUSTFLAGS.
ENV RUSTFLAGS="-C target-cpu=x86-64"

# The Rust workspace is under /workspace/tools. Keeping the source mounted at
# runtime lets the container run the current checkout without rebuilding the
# image after every source change.
CMD ["bash"]
