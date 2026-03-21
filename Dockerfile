# ---------------------------------------------------------------------------
# Build stage – cross-compile both architectures on the build host.
# Uses musl for fully static binaries so the final image can be scratch.
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM rust:1-bullseye AS builder

RUN apt-get update && apt-get install -y \
    protobuf-compiler \
    musl-tools \
    gcc-aarch64-linux-gnu \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

# Install extra CA certificates if present in build context (for corporate
# proxies). Drop .crt files in the project root before building.
RUN if ls /build/*.crt 1>/dev/null 2>&1; then \
      cp /build/*.crt /usr/local/share/ca-certificates/ && \
      update-ca-certificates; \
    fi

RUN rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

RUN cargo build --target=x86_64-unknown-linux-musl --release
RUN cargo build --target=aarch64-unknown-linux-musl --release

# ---------------------------------------------------------------------------
# Intermediate stages to map Docker TARGETARCH (amd64/arm64) to the Rust
# target directory names (x86_64/aarch64).
# ---------------------------------------------------------------------------
FROM scratch AS bin-linux-amd64
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/bb_runner /bb_runner

FROM scratch AS bin-linux-arm64
COPY --from=builder /build/target/aarch64-unknown-linux-musl/release/bb_runner /bb_runner

# ---------------------------------------------------------------------------
# Select the correct binary for the target architecture.
# Subsequent stages reference this by name.
# ---------------------------------------------------------------------------
ARG TARGETARCH
FROM bin-linux-${TARGETARCH} AS bin-selected

# ---------------------------------------------------------------------------
# Standalone runner image – just the static binary on scratch.
# Build with:
#   docker buildx build --target runner --platform linux/amd64,linux/arm64 -t bb_runner .
# ---------------------------------------------------------------------------
FROM bin-selected AS runner
ENTRYPOINT ["/bb_runner"]

# ---------------------------------------------------------------------------
# Installer image (default) – copies the binary into a mounted volume.
# Used as a Kubernetes init container: mount an emptyDir at /bb/.
# Build with:
#   docker buildx build --platform linux/amd64,linux/arm64 -t bb_runner_installer .
# ---------------------------------------------------------------------------
FROM busybox:stable-uclibc AS installer
COPY --from=bin-selected /bb_runner /bb_runner
ENTRYPOINT ["cp", "/bb_runner", "/bb/"]
