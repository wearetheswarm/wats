# syntax=docker/dockerfile:1
#
# OpenClaw's gateway plus the wats binary, for compose.yaml next to this
# file. OpenClaw starts `wats mcp` for the wats tools; those sessions attach
# to the wats node of the shared data volume (/data/wats/mcp.sock), so both
# containers run as the same uid (65532, the wats image's).
#
# OPENCLAW_IMAGE has no default: wats does not pick an OpenClaw image for
# you. Use the one OpenClaw's documentation gives (docs.openclaw.ai), pinned
# to a version or a digest. Its default command must start the gateway on
# port 18789, and it needs a shell for the RUN below.
#
#   docker build -t wats-openclaw --build-arg OPENCLAW_IMAGE=<image> deploy/openclaw

ARG WATS_IMAGE=ghcr.io/wearetheswarm/wats:0.3.0
ARG OPENCLAW_IMAGE

FROM ${WATS_IMAGE} AS wats

FROM ${OPENCLAW_IMAGE}
USER root
RUN install -d -o 65532 -g 65532 -m 0700 /data /home/openclaw
COPY --from=wats /usr/local/bin/wats /usr/local/bin/wats
ENV WATS_DIR=/data/wats \
    HOME=/home/openclaw
VOLUME ["/data", "/home/openclaw"]
USER 65532:65532
