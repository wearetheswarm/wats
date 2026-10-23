# syntax=docker/dockerfile:1
#
# The WeAreTheSwarm Discord bot (ADR 0040): a visitor through its own wats
# node, read-only in phase 1. See README.md next to this file.
#
#   docker build -t wats-discord discord-bot
#   docker run -d --name wats-discord -e DISCORD_TOKEN -e ALLOWED_CHANNEL_ID \
#     -e GUILD_ID -v wats-discord:/data wats-discord

ARG WATS_IMAGE=ghcr.io/wearetheswarm/wats:0.4.9
ARG PYTHON_IMAGE=python:3.12-slim-bookworm

FROM ${WATS_IMAGE} AS wats

FROM ${PYTHON_IMAGE}
COPY requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r /app/requirements.txt \
 && useradd --system --uid 10001 --home-dir /data bot \
 && install -d -o bot -g bot -m 0700 /data
COPY --from=wats /usr/local/bin/wats /usr/local/bin/wats
COPY src/wats_discord /app/wats_discord
ENV WATS_DIR=/data/wats \
    WATS_PROFILE=discord \
    PYTHONUNBUFFERED=1 \
    RUST_LOG=wats=info
VOLUME ["/data"]
WORKDIR /app
USER bot
CMD ["python", "-m", "wats_discord.bot"]
