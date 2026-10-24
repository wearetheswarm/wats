# syntax=docker/dockerfile:1
#
# Example image for `wats --agent claude`: the wats node plus the Claude Code CLI,
# so that an agent can run in a container, with a work directory in which
# tools beyond MCP can be allowed (pass them after `--`). See
# docs/agent.md and compose.yaml next to this file.
#
#   docker build -t wats-agent deploy/agent
#   scripts/build-static.sh --image    # then, for a local wats (static: glibc builds do not run here):
#   docker build -t wats-agent --build-arg WATS_IMAGE=wats:static-<version>-<arch> deploy/agent

ARG WATS_IMAGE=ghcr.io/wearetheswarm/wats:0.4.12
ARG NODE_IMAGE=node:22-bookworm-slim@sha256:c3de60bf2f9dd0ac6370e6117950ff62d6e339527e7472301c9c78a017978392

FROM ${WATS_IMAGE} AS wats

FROM ${NODE_IMAGE}
ARG CLAUDE_CODE_VERSION=2.1.301
RUN npm install -g --no-audit --no-fund "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
 && npm cache clean --force \
 && install -d -o node -g node -m 0700 /data /work
COPY --from=wats /usr/local/bin/wats /usr/local/bin/wats
ENV WATS_DIR=/data/wats \
    RUST_LOG=wats=info
VOLUME ["/data", "/work"]
WORKDIR /work
USER node
ENTRYPOINT ["wats"]
CMD ["--agent", "claude"]
