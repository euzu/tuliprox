# =================================================================
#
# Dockerfile for local build (nativ, without Cross-Compilation)
#
# =================================================================

# -----------------------------------------------------------------
# Stage 1: Build the Rust binary for production
# -----------------------------------------------------------------
FROM rust:bookworm AS rust-build

# Get target architecture from build argument
ARG RUST_TARGET=x86_64-unknown-linux-musl

# Install build dependencies for Debian (bookworm)
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libssl-dev musl-tools && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Set Rust compiler flags for static linking against musl
ENV RUSTFLAGS='-C target-feature=+crt-static'

# Copy dependency files and the full 'shared' crate for dependency caching
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY shared ./shared/
COPY backend/Cargo.toml ./backend/
COPY frontend/Cargo.toml ./frontend/

# Create dummy main.rs for other crates to allow dependency build
RUN mkdir -p backend/src frontend/src && \
    echo "fn main() {}" > backend/src/main.rs && \
    echo "fn main() {}" > frontend/src/main.rs

# Build dependencies (this layer will be cached unless dependencies or 'shared' change)
RUN rustup target add ${RUST_TARGET}
RUN cargo build -p tuliprox --target ${RUST_TARGET} --release || true

# Now copy the rest of the source code and build the project
COPY backend ./backend/
COPY frontend ./frontend/
RUN cargo build -p tuliprox --target ${RUST_TARGET} --release

# -----------------------------------------------------------------
# Stage 2: Build the rust frontend
# -----------------------------------------------------------------
FROM rust:bookworm AS trunk-build

# Install dependencies for Trunk and WebAssembly
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl binaryen && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Add wasm target & install trunk
RUN rustup target add wasm32-unknown-unknown
RUN cargo install --locked trunk wasm-bindgen-cli

# Set working directory
WORKDIR /src

# Copy dependency files and the full 'shared' crate for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY shared ./shared/
COPY backend/Cargo.toml ./backend/
COPY frontend/Cargo.toml frontend/Trunk.toml ./frontend/

# Create dummy main.rs for BOTH frontend AND backend to satisfy workspace requirements
RUN mkdir -p frontend/src backend/src && \
    echo "fn main() {}" > frontend/src/main.rs && \
    echo "fn main() {}" > backend/src/main.rs

# Pre-build dependencies
WORKDIR /src/frontend
RUN cargo build --target wasm32-unknown-unknown --release || true

# Now copy the rest of the source code and build the project
WORKDIR /src
COPY frontend ./frontend/

WORKDIR /src/frontend

# Run Trunk build
RUN trunk build --release

# -----------------------------------------------------------------
# Stage 3: Build video resources with ffmpeg
# -----------------------------------------------------------------
FROM linuxserver/ffmpeg:latest AS resource-build

WORKDIR /src
COPY resources ./resources

# Combine ffmpeg commands into a single layer to reduce image size
RUN for img in channel_unavailable user_connections_exhausted provider_connections_exhausted user_account_expired panel_api_provisioning; do \
      ffmpeg -y -nostdin -loop 1 -i ./resources/${img}.jpg \
        -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=44100 \
        -c:v libx264 -r 30 -g 30 -keyint_min 30 -sc_threshold 0 -pix_fmt yuv420p -preset veryfast -crf 23 \
        -c:a aac -b:a 128k -ac 2 \
        -t 10 -muxrate 2000k \
        -f mpegts ./resources/${img}.ts || exit 1; \
    done

# -----------------------------------------------------------------
# Stage 4: Prepare timezone data
# -----------------------------------------------------------------
FROM alpine:latest AS tz-prep

ARG TZ=UTC
ENV TZ=${TZ}

RUN apk add --no-cache tzdata \
  && mkdir -p /output/etc \
  && mkdir -p /output/usr/share \
  && cp -r /usr/share/zoneinfo /output/usr/share/zoneinfo \
  && ln -sf /usr/share/zoneinfo/${TZ} /output/etc/localtime


# =================================================================
#
# Part 2: Final Image Stages
#
# =================================================================

# -----------------------------------------------------------------
# Final Image #1: Debugging Environment (Alpine-based)
# -----------------------------------------------------------------
FROM rust:alpine AS debug

# Make the build-time argument available as a run-time environment variable
ARG RUST_TARGET=x86_64-unknown-linux-musl
ARG TZ=UTC

# Make the build-time argument available as a run-time environment variable
ENV RUST_TARGET=${RUST_TARGET}
ENV TZ=${TZ}

# Install build dependencies and debugger for Alpine.
# 'build-base' contains gcc, make, etc.
# 'gdb' is required for the debugger (lldb is often more complicated in Alpine repos)
RUN apk add --no-cache bash build-base openssl-dev perl lldb gdb

# Update Rust toolchain and add the necessary target
RUN rustup update && rustup target add $RUST_TARGET

# Create the app layout to simulate the production environment
WORKDIR /app
COPY --from=trunk-build /src/frontend/dist ./web
COPY --from=resource-build /src/resources ./resources

# Create a separate directory for the source code
WORKDIR /usr/src/tuliprox
COPY . .

# Copy the entrypoint script and set it as the entry point for the container
COPY ./docker/debug/debug-entrypoint.sh /usr/local/bin/entrypoint.sh

# Ensure the script is executable
RUN chmod +x /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

# The CMD is passed as an argument to the Entrypoint script. 
# This keeps the container running indefinitely for interactive debugging.
CMD ["tail", "-f", "/dev/null"]

# -----------------------------------------------------------------
# Final Image #2: Production image based on scratch
# -----------------------------------------------------------------
FROM scratch AS scratch-final

ARG RUST_TARGET=x86_64-unknown-linux-musl

# Copy timezone data and localtime from tz-prep
COPY --from=tz-prep /output/usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=tz-prep /output/etc/localtime /etc/localtime

# Copy SSL certificates from a builder stage that has them
COPY --from=rust-build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# App
WORKDIR /app
COPY --from=rust-build /src/target/${RUST_TARGET}/release/tuliprox ./tuliprox
COPY --from=trunk-build /src/frontend/dist ./web
COPY --from=resource-build /src/resources ./resources

ENTRYPOINT ["/app/tuliprox"]
CMD ["-s", "-p", "/app/config"]

# -----------------------------------------------------------------
# Final Image #3: Production image based on Alpine
# -----------------------------------------------------------------
FROM alpine:latest AS alpine-final

ARG RUST_TARGET=x86_64-unknown-linux-musl
ARG TZ=UTC
ENV TZ=${TZ}

RUN apk add --no-cache bash curl ca-certificates tini

# Copy timezone data and localtime from tz-prep
COPY --from=tz-prep /output/usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=tz-prep /output/etc/localtime /etc/localtime

# Copy SSL certificates `ca-certificates` from Alpine-based Stage (tz-prep ist Alpine)
COPY --from=tz-prep /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Default timezone fallback
RUN ln -sf /usr/share/zoneinfo/${TZ} /etc/localtime

WORKDIR /app
COPY --from=rust-build /src/target/${RUST_TARGET}/release/tuliprox ./tuliprox
COPY --from=trunk-build /src/frontend/dist ./web
COPY --from=resource-build /src/resources ./resources

ENTRYPOINT ["/sbin/tini", "--", "/app/tuliprox"]
CMD ["-s", "-p", "/app/config"]
