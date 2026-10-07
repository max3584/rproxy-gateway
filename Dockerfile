# The controller image (rproxy-gateway controller / certsync / crds / render).
# The binary is built beforehand in an Alpine container (static, musl): see
# .github/workflows/release.yml and scripts/build-image.sh. Multi-arch with
# buildx: dist/<TARGETARCH>/rproxy-gateway.
FROM alpine:3.24
ARG TARGETARCH
RUN apk add --no-cache ca-certificates
COPY dist/${TARGETARCH}/rproxy-gateway /usr/local/bin/rproxy-gateway
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/rproxy-gateway"]
CMD ["controller"]
