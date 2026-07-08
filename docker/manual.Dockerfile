FROM alpine:latest AS tz-prep

ARG TZ=UTC
ENV TZ=${TZ}

RUN apk add --no-cache tzdata \
  && mkdir -p /output/etc \
  && mkdir -p /output/usr/share \
  && cp -r /usr/share/zoneinfo /output/usr/share/zoneinfo \
  && ln -sf /usr/share/zoneinfo/${TZ} /output/etc/localtime \
  && mkdir -p /output/etc/ssl/certs \
  && cp /etc/ssl/certs/ca-certificates.crt /output/etc/ssl/certs/ca-certificates.crt

# Fetch static FFmpeg binaries
FROM mwader/static-ffmpeg:7.1 AS ffmpeg-static

# Binary selector stage
FROM alpine:latest AS binary-selector

ARG TARGETARCH

WORKDIR /app

# Copy all binaries
COPY ./binaries ./binaries

# Select the appropriate binary based on target architecture
RUN case "${TARGETARCH}" in \
        "amd64") cp ./binaries/tuliprox-x86_64-unknown-linux-musl ./tuliprox ;; \
        "arm64") cp ./binaries/tuliprox-aarch64-unknown-linux-musl ./tuliprox ;; \
        *) echo "Unsupported architecture: ${TARGETARCH}" && exit 1 ;; \
    esac

# Scratch Final container
FROM scratch AS scratch-final

ARG TZ=UTC
ENV TZ=${TZ}

# Copy timezone data and localtime from tz-prep
COPY --from=tz-prep /output/usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=tz-prep /output/etc/localtime /etc/localtime
COPY --from=tz-prep /output/etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Copy static ffmpeg/ffprobe binaries
COPY --from=ffmpeg-static /ffmpeg /usr/local/bin/
COPY --from=ffmpeg-static /ffprobe /usr/local/bin/

# RUN ln -sf /usr/share/zoneinfo/${TZ} /etc/localtime

WORKDIR /app

# Copy the selected binary from binary-selector stage
COPY --from=binary-selector /app/tuliprox ./tuliprox
COPY ./web ./web
COPY ./resources ./resources

CMD ["/app/tuliprox", "-s", "-p", "/app/config"]

# Alpine Final container
FROM alpine:latest AS alpine-final

ARG TZ=UTC
ENV TZ=${TZ}

# Install dependencies including ffmpeg
RUN apk add --no-cache bash curl strace tcpdump bind-tools nano ca-certificates tini ffmpeg

# Copy timezone data and localtime from tz-prep
COPY --from=tz-prep /output/usr/share/zoneinfo /usr/share/zoneinfo
COPY --from=tz-prep /output/etc/localtime /etc/localtime
COPY --from=tz-prep /output/etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

RUN ln -sf /usr/share/zoneinfo/${TZ} /etc/localtime

WORKDIR /app

# Copy the selected binary from binary-selector stage
COPY --from=binary-selector /app/tuliprox ./tuliprox
COPY ./web ./web
COPY ./resources ./resources
# config should be mounted as volume
# COPY ./config ./config

ENTRYPOINT ["/sbin/tini", "--", "/app/tuliprox"]
CMD ["-s", "-p", "/app/config"]