# syntax=docker/dockerfile:1
#
# OpenClaw's gateway plus the wats binary, for compose.yaml next to this
# file. OpenClaw runs `wats setup openclaw` and starts `wats mcp`; those
# reach the wats node through the shared volume /wats-link (its socket and
# the OpenClaw tokens), never through the node's data directory: this
# container has no access to the node's key. Both containers run as uid
# 65532, the wats image's user, as the node's socket requires.
#
# OPENCLAW_IMAGE has no default: wats does not pick an OpenClaw image for
# you. Use the one OpenClaw's documentation gives (docs.openclaw.ai), pinned
# to a version or a digest. Its default command must start the gateway on
# port 18789, and it needs a shell for the RUN below.
#
#   docker build -t wats-openclaw --build-arg OPENCLAW_IMAGE=<image> deploy/openclaw

ARG WATS_IMAGE=ghcr.io/wearetheswarm/wats:0.4.4
ARG OPENCLAW_IMAGE

FROM ${WATS_IMAGE} AS wats

FROM ${OPENCLAW_IMAGE}
USER root
# /wats-link is created here, owned by 65532, so that the shared volume
# takes that owner when this container mounts it first.
RUN install -d -o 65532 -g 65532 -m 0700 /home/openclaw /wats-link
COPY --from=wats /usr/local/bin/wats /usr/local/bin/wats
# WATS_DIR is a scratch data directory for setup (its own config.toml),
# not the node's.
ENV HOME=/home/openclaw \
    WATS_DIR=/home/openclaw/wats \
    WATS_MCP_SOCKET=/wats-link/mcp.sock \
    WATS_OPENCLAW_DIR=/wats-link
VOLUME ["/home/openclaw", "/wats-link"]
USER 65532:65532
