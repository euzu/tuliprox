# =================================================================
#
# Dockerfile for local build (nativ, without Cross-Compilation)
#
# =================================================================

# -----------------------------------------------------------------
# Stage 0: Fetch static FFmpeg binaries
# -----------------------------------------------------------------
# We use a static build to ensure it works in the scratch image without libraries
FROM mwader/static-ffmpeg:7.1 AS ffmpeg-static

# Workspace skeleton shared by the backend and frontend dependency builds.
FROM rust:1.95 AS workspace-skeleton

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY shared ./shared/
COPY frontend/Cargo.toml frontend/Trunk.toml ./frontend/
COPY backend/app/Cargo.toml backend/app/build.rs ./backend/app/
COPY backend/auth/Cargo.toml ./backend/auth/
COPY backend/btree/Cargo.toml ./backend/btree/
COPY backend/config-loader/Cargo.toml ./backend/config-loader/
COPY backend/core/Cargo.toml ./backend/core/
COPY backend/dvr/Cargo.toml ./backend/dvr/
COPY backend/hls/Cargo.toml ./backend/hls/
COPY backend/iptv/Cargo.toml ./backend/iptv/
COPY backend/library/Cargo.toml ./backend/library/
COPY backend/media-server/Cargo.toml ./backend/media-server/
COPY backend/metadata/Cargo.toml ./backend/metadata/
COPY backend/messaging/Cargo.toml ./backend/messaging/
COPY backend/mpegts/Cargo.toml ./backend/mpegts/
COPY backend/parser/Cargo.toml ./backend/parser/
COPY backend/processing/Cargo.toml ./backend/processing/
COPY backend/repository/Cargo.toml ./backend/repository/
COPY backend/session/Cargo.toml ./backend/session/

RUN mkdir -p frontend/src backend/app/src/tools && \
    printf 'fn main() {}\n' > frontend/src/main.rs && \
    printf 'fn main() {}\n' > backend/app/src/main.rs && \
    printf 'fn main() {}\n' > backend/app/src/tools/flags_builder.rs && \
    for crate in auth btree config-loader core dvr hls iptv library media-server metadata messaging mpegts parser processing repository session; do \
      mkdir -p "backend/${crate}/src"; \
      printf '\n' > "backend/${crate}/src/lib.rs"; \
    done

# -----------------------------------------------------------------
# Stage 1: Build the Rust binary for production
# -----------------------------------------------------------------
FROM workspace-skeleton AS rust-build

# Get target architecture from build argument
ARG RUST_TARGET=x86_64-unknown-linux-musl

# Install build dependencies for Debian (trixie)
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config musl-tools && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Set Rust compiler flags for static linking against musl
ENV RUSTFLAGS='-C target-feature=+crt-static'

# Build dependencies (this layer is cached until a manifest or shared code changes)
RUN rustup target add ${RUST_TARGET}
RUN cargo build -p tuliprox --target ${RUST_TARGET} --release

# Copy the backend source only after its dependency layer is complete.
COPY backend ./backend/
RUN find backend -type f -name '*.rs' -exec touch {} + && \
    cargo build -p tuliprox --target ${RUST_TARGET} --release

# -----------------------------------------------------------------
# Stage 2: Build the rust frontend
# -----------------------------------------------------------------
FROM workspace-skeleton AS trunk-build

# Install dependencies for Trunk and WebAssembly
RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    pkg-config libssl-dev curl binaryen && \
    apt-get clean && \
    rm -rf /var/lib/apt/lists/*

# Add wasm target & install trunk
RUN rustup target add wasm32-unknown-unknown
RUN cargo install --locked trunk wasm-bindgen-cli

# Pre-build dependencies
WORKDIR /src/frontend
RUN cargo build --target wasm32-unknown-unknown --release

# Now copy the rest of the source code and build the project
WORKDIR /src
COPY frontend ./frontend/

WORKDIR /src/frontend

# Run Trunk build
RUN find src -type f -name '*.rs' -exec touch {} + && trunk build --release

# -----------------------------------------------------------------
# Stage 3: Build video resources with ffmpeg (from linuxserver used for build time only)
# -----------------------------------------------------------------
FROM linuxserver/ffmpeg:latest AS resource-build

WORKDIR /src
COPY resources ./resources

# Combine ffmpeg commands into a single layer to reduce image size
# Convert every .jpg file in resources into a matching .ts file.
RUN set -eu; \
    find ./resources -maxdepth 1 -type f -name '*.jpg' | sort | while IFS= read -r image; do \
      output="${image%.jpg}.ts"; \
      resource_name="$(basename "${image%.jpg}")"; \
      ffmpeg -y -nostdin -loop 1 -framerate 30 -i "${image}" \
            -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 \
            -t 10 -shortest \
            -c:v libx264 -pix_fmt yuv420p -preset veryfast -crf 23 \
            -x264-params "keyint=30:min-keyint=30:scenecut=0:bframes=0:open_gop=0" \
            -c:a aac -b:a 128k -ac 2 -ar 48000 \
            -mpegts_flags +resend_headers \
            -muxdelay 0 -muxpreload 0 \
            -f mpegts "${output}" || exit 1; \
      if [ "${resource_name}" = "panel_api_provisioning" ]; then \
        rm -f ./resources/panel_api_provisioning_hls_*.ts ./resources/panel_api_provisioning_hls.m3u8; \
        ffmpeg -y -nostdin -loop 1 -framerate 30 -i "${image}" \
              -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=48000 \
              -t 12 -shortest \
              -c:v libx264 -pix_fmt yuv420p -preset veryfast -crf 23 \
              -g 60 -keyint_min 60 \
              -force_key_frames "expr:gte(t,n_forced*2)" \
              -x264-params "scenecut=0:bframes=0:open_gop=0" \
              -c:a aac -b:a 128k -ac 2 -ar 48000 \
              -mpegts_flags +resend_headers \
              -muxdelay 0 -muxpreload 0 \
              -f hls \
              -hls_time 2 \
              -hls_list_size 0 \
              -hls_segment_type mpegts \
              -hls_segment_filename "./resources/panel_api_provisioning_hls_%03d.ts" \
              ./resources/panel_api_provisioning_hls.m3u8 || exit 1; \
        rm -f ./resources/panel_api_provisioning_hls.m3u8; \
      fi; \
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

# Install build dependencies, debugger and ffmpeg for Alpine.
# 'build-base' contains gcc, make, etc.
# 'gdb' is required for the debugger (lldb is often more complicated in Alpine repos)
# 'ffmpeg' is required for the probe feature
RUN apk add --no-cache bash build-base perl lldb gdb ffmpeg

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

# Copy static ffmpeg/ffprobe binaries
COPY --from=ffmpeg-static /ffmpeg /usr/local/bin/
COPY --from=ffmpeg-static /ffprobe /usr/local/bin/

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

# Install dependencies including ffmpeg
RUN apk add --no-cache bash curl ca-certificates tini ffmpeg

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
