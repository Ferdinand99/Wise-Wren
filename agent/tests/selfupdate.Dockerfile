# The agent image of the self-update Docker tests (docker_self_update.rs):
# runtime only, with a binary that the test built beforehand. No cache mounts,
# so nothing stays behind in the shared BuildKit cache. The release
# Dockerfile at the repository root is not used or changed.
FROM alpine:3.22
RUN apk add --no-cache ca-certificates docker-cli
ARG WW_SU_LABEL_VERSION
LABEL org.opencontainers.image.version=$WW_SU_LABEL_VERSION owner=wise-wren
COPY wren-agent /usr/local/bin/wren-agent
WORKDIR /config
ENTRYPOINT ["/usr/local/bin/wren-agent"]
