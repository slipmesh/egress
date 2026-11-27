# Packaging only - expects the binary already built for the image's arch and staged at
# target/<TARGETARCH>/release/, as CI does after
#   cargo build --release --target <arch>-unknown-linux-musl
# on a runner of that architecture. Not a `cargo build` inside this Dockerfile: a full rustc
# build segfaults under QEMU emulation for a non-native --platform.
#
# The agent's TLS clients, to the API server and to RIPEstat, read CA certificates from the
# filesystem and have no bundle compiled in, so `FROM scratch` takes Alpine's one bundle file.
FROM alpine:3.24 AS certs
RUN apk add --no-cache ca-certificates

FROM scratch
COPY --from=certs /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
ARG TARGETARCH
COPY target/${TARGETARCH}/release/egress /egress
ENTRYPOINT ["/egress"]
