# =================================================================
#
# Part 1: Prerequisite Build Stages
#
# These stages are used as building blocks for the final images.
# They prepare the Rust binary and other resources.
#
# =================================================================

# -----------------------------------------------------------------
# Stage 0: Fetch static FFmpeg binaries
# -----------------------------------------------------------------
# We use a static build to ensure it works in the scratch image without libraries
# Static ffmpeg image is multi-arch, so it pulls correct arch automatically
FROM mwader/static-ffmpeg:9.0.2-1@sha256:f5d100184884eb3e82b0eb14bb5ebf98da99826ac824989720297fb9ef21c822 AS ffmpeg-static

# The cross image provides the target compiler and sysroot, but no Rust toolchain.
FROM rust:1.99.0-trixie@sha256:81015672d6939b0265f09adf96d5de125482bb1330106076956b0518e74edc90 AS rust-toolchain

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY vendor/reqwest ./vendor/reqwest/
COPY shared ./shared/
COPY frontend/Cargo.toml frontend/Trunk.toml ./frontend/
COPY backend/app/Cargo.toml backend/app/build.rs ./backend/app/
COPY backend/auth/Cargo.toml ./backend/auth/
COPY backend/btree/Cargo.toml ./backend/btree/
COPY backend/config-loader/Cargo.toml ./backend/config-loader/
COPY backend/core/Cargo.toml ./backend/core/
COPY backend/curation/Cargo.toml ./backend/curation/
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
COPY backend/scripting/Cargo.toml ./backend/scripting/
COPY backend/session/Cargo.toml ./backend/session/
COPY tools/tuliprox-testkit/Cargo.toml ./tools/tuliprox-testkit/

RUN mkdir -p frontend/src backend/app/src/tools tools/tuliprox-testkit/src && \
    printf 'fn main() {}\n' > frontend/src/main.rs && \
    printf 'fn main() {}\n' > backend/app/src/main.rs && \
    printf 'fn main() {}\n' > backend/app/src/tools/flags_builder.rs && \
    printf '\n' > tools/tuliprox-testkit/src/lib.rs && \
    printf 'fn main() {}\n' > tools/tuliprox-testkit/src/main.rs && \
    for crate in auth btree config-loader core curation dvr hls iptv library media-server metadata messaging mpegts parser processing repository scripting session; do \
      mkdir -p "backend/${crate}/src"; \
      printf '\n' > "backend/${crate}/src/lib.rs"; \
    done && \
    cargo metadata --no-deps --format-version 1 > /dev/null

# -----------------------------------------------------------------
# Stage 1: Build the Rust binary for production
# -----------------------------------------------------------------
FROM ghcr.io/cross-rs/x86_64-unknown-linux-musl:main@sha256:e30ffbc1c5958b55ab2ed7263709f1ccc2fae1e2d49263553daa0c312117aecb AS rust-build

COPY --from=rust-toolchain /usr/local/cargo /usr/local/cargo
COPY --from=rust-toolchain /usr/local/rustup /usr/local/rustup
COPY --from=rust-toolchain /src /src
ENV CARGO_HOME=/usr/local/cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:$PATH

# Get target architecture
ARG RUST_TARGET

# Add the requested compilation target to the pinned Rust toolchain
RUN rustup target add $RUST_TARGET

# Set Rust compiler flags for better optimization and reproducibility
ENV RUSTFLAGS='--remap-path-prefix $HOME=~ -C target-feature=+crt-static'

WORKDIR /src
# Build dependencies (this layer will be cached unless dependencies change)
RUN cargo build -p tuliprox --target $RUST_TARGET --release

# Copy the backend source only after its dependency layer is complete.
COPY backend ./backend/
RUN find backend -type f -name '*.rs' -exec touch {} + && \
    cargo build -p tuliprox --target $RUST_TARGET --release

# -----------------------------------------------------------------
# Stage 2: Build the rust frontend
# -----------------------------------------------------------------
FROM rust-toolchain AS trunk-build

ARG RUST_TARGET=wasm32-unknown-unknown

# Install dependencies for Trunk and WebAssembly
RUN apt-get update && apt-get install -y --no-install-recommends \
  pkg-config libssl-dev curl binaryen

# Add wasm target & install trunk
RUN rustup target add wasm32-unknown-unknown
RUN cargo install --locked trunk wasm-bindgen-cli

# Pre-build dependencies
WORKDIR /src/frontend
RUN cargo build --target wasm32-unknown-unknown --release

# Now copy the actual frontend source and build the project
WORKDIR /src
COPY frontend ./frontend/

WORKDIR /src/frontend

# Run Trunk build
RUN find src -type f -name '*.rs' -exec touch {} + && trunk build --release

# -----------------------------------------------------------------
# Stage 3: Build video resources with ffmpeg
# -----------------------------------------------------------------
FROM linuxserver/ffmpeg:version-9.0-cli@sha256:a7182d4fe498feea393622b43513cfaecb3fd073dcd3c7f38e9d74a10a4e8702 AS resource-build

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
FROM alpine:3.24.3@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a AS tz-prep

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
# These stages build the final, runnable images by assembling
# the artifacts from the prerequisite stages above.
#
# =================================================================

# -----------------------------------------------------------------
# Final Image #1: Debugging Environment (Alpine-based)
# -----------------------------------------------------------------
FROM rust:1.99.0-alpine3.24@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627 AS debug

# For Alpine, the correct target architecture uses 'musl'
ARG RUST_TARGET=x86_64-unknown-linux-musl
ARG TZ=UTC

# Make the build-time argument available as a run-time environment variable
ENV RUST_TARGET=${RUST_TARGET}
ENV TZ=${TZ}

# Install build dependencies and the lldb debugger for Alpine
# Added ffmpeg for probing
RUN apk add --no-cache bash build-base lldb gdb perl ffmpeg

# Add the necessary target to the pinned Rust toolchain
RUN rustup target add $RUST_TARGET

# Create the final application layout to mimic production
WORKDIR /app
COPY --from=trunk-build /src/frontend/dist ./web
COPY --from=resource-build /src/resources ./resources

# Create a separate directory for the source code
WORKDIR /usr/src/tuliprox
COPY . .

# Copy the entrypoint script and set it as the entrypoint for the container.
COPY ./docker/debug/debug-entrypoint.sh /usr/local/bin/entrypoint.sh
# Ensure the script is executable
RUN chmod +x /usr/local/bin/entrypoint.sh
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]

# The CMD will be passed as arguments to the entrypoint script.
CMD ["tail", "-f", "/dev/null"]

# -----------------------------------------------------------------
# Final Image #2: Production image based on scratch
# -----------------------------------------------------------------
FROM scratch AS scratch-final

ARG RUST_TARGET

# Copy timezone data and localtime from tz-prep
COPY --from=tz-prep /output/usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=tz-prep /output/etc/localtime /etc/localtime

# Certificates
COPY --from=rust-build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Copy static ffmpeg/ffprobe binaries
COPY --from=ffmpeg-static /ffmpeg /usr/local/bin/
COPY --from=ffmpeg-static /ffprobe /usr/local/bin/

# RUN ln -sf /usr/share/zoneinfo/${TZ} /etc/localtime

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
FROM alpine:3.24.3@sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a AS alpine-final

ARG RUST_TARGET
ARG TZ=UTC
ENV TZ=${TZ}

# Install dependencies including ffmpeg
RUN apk add --no-cache bash curl ca-certificates tini ffmpeg

COPY --from=rust-build /usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=rust-build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

# Default timezone fallback
RUN ln -sf /usr/share/zoneinfo/${TZ} /etc/localtime

WORKDIR /app
COPY --from=rust-build /src/target/${RUST_TARGET}/release/tuliprox ./tuliprox
COPY --from=trunk-build /src/frontend/dist ./web
COPY --from=resource-build /src/resources ./resources

ENTRYPOINT ["/sbin/tini", "--", "/app/tuliprox"]
CMD ["-s", "-p", "/app/config"]
