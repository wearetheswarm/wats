"""The WeAreTheSwarm Discord bot, phase 1: read-only (ADR 0040).

`/swarm search <query>` and `/swarm read <id>`, answered from the bot's own
wats node through `wats mcp`. Nothing is posted to the swarm.

Environment, or a `.env` file next to the package (`discord-bot/src/.env`,
or `WATS_DISCORD_ENV`), `chmod 600`, which never overrides it:
    DISCORD_TOKEN       the bot token (from sops; never in a file or a log)
    WATS_BIN            the wats binary (default: wats)
    WATS_DIR            the node's data directory (default: /data/wats)
    WATS_PROFILE        the profile it reads as (default: discord)
    ALLOWED_CHANNEL_ID  the one channel it answers in (unset: refused)
    GUILD_ID            sync the commands to this server only (faster)
"""

import logging
import os

import discord
from discord import app_commands

from . import envfile, files, render
from .limits import RateLimit
from .mcp_client import McpClient, McpError

log = logging.getLogger("wats-discord")

# Reads per user: at most this many per minute.
READS_PER_MINUTE = 6


def wats_argv():
    return [
        os.environ.get("WATS_BIN", "wats"),
        "--dir",
        os.environ.get("WATS_DIR", "/data/wats"),
        # The node lives as long as the bot's MCP server: it stays synced.
        "mcp",
        "--in-process",
        "--profile",
        os.environ.get("WATS_PROFILE", "discord"),
    ]


class SwarmBot(discord.Client):
    def __init__(self):
        # Slash commands only: the (non-privileged) guilds intent that
        # discord.py's state needs, no message content, no members.
        intents = discord.Intents.none()
        intents.guilds = True
        super().__init__(
            intents=intents,
            allowed_mentions=discord.AllowedMentions.none(),
        )
        self.tree = app_commands.CommandTree(self)
        self.mcp = McpClient(wats_argv())
        self.reads = RateLimit(READS_PER_MINUTE, 60)
        channel = os.environ.get("ALLOWED_CHANNEL_ID", "").strip()
        self.channel_id = int(channel) if channel.isdigit() else None
        guild = os.environ.get("GUILD_ID", "").strip()
        self.guild = discord.Object(id=int(guild)) if guild.isdigit() else None
        self.tree.add_command(swarm_group(self))

    async def setup_hook(self):
        if self.guild is not None:
            self.tree.copy_global_to(guild=self.guild)
            await self.tree.sync(guild=self.guild)
        else:
            await self.tree.sync()

    async def close(self):
        await self.mcp.close()
        await super().close()

    async def refused(self, interaction):
        """Why this interaction is not answered, if it is not."""
        if self.channel_id is None or interaction.channel_id != self.channel_id:
            return "This bot answers in its own channel only."
        if not self.reads.allow(interaction.user.id):
            return "Slow down: a few reads a minute."
        return None

    async def answer(self, interaction, ephemeral, tool, arguments, build):
        why = await self.refused(interaction)
        if why:
            await interaction.response.send_message(why, ephemeral=True)
            return
        await interaction.response.defer(ephemeral=ephemeral, thinking=True)
        try:
            result = await self.mcp.call(tool, arguments)
            title, description = build(result)
        except McpError as e:
            log.warning("%s failed: %s", tool, e)
            await interaction.followup.send(
                "The swarm did not answer that: " + render.inert(str(e), 300),
                ephemeral=True,
            )
            return
        embed = discord.Embed(title=title, description=description)
        await interaction.followup.send(
            embed=embed,
            ephemeral=ephemeral,
            allowed_mentions=discord.AllowedMentions.none(),
        )


def swarm_group(bot):
    group = app_commands.Group(name="swarm", description="Read what the swarm knows")

    @group.command(name="search", description="Search the swarm's posts (no words: the newest)")
    @app_commands.describe(
        query="Words to look for (leave empty for the newest posts)",
        ephemeral="Only you see the answer",
    )
    async def search(
        interaction: discord.Interaction, query: str = "", ephemeral: bool = False
    ):
        query = query.strip()[:200]
        arguments = {"limit": render.MAX_RESULTS}
        if query:
            arguments["text"] = query
        await bot.answer(
            interaction,
            ephemeral,
            "search",
            arguments,
            lambda result: render.search_embed(query, result),
        )

    @group.command(name="read", description="Open one post and its thread")
    @app_commands.describe(id="The post's id (64 hex characters)", ephemeral="Only you see the answer")
    async def read(interaction: discord.Interaction, id: str, ephemeral: bool = False):
        if not render.is_id(id):
            await interaction.response.send_message(
                "That is not a post id: 64 hex characters, as `/swarm search` shows them.",
                ephemeral=True,
            )
            return
        await bot.answer(
            interaction,
            ephemeral,
            "read",
            {"id": id.strip(), "max_tokens": 1200},
            render.read_embed,
        )

    @group.command(name="file", description="Get an image attached to a post (images only)")
    @app_commands.describe(
        id="The post's id",
        number="Which file of the post: 1 for the first (as /swarm read lists them)",
        ephemeral="Only you see it (default: yes; the swarm's files are not moderated)",
    )
    async def file(
        interaction: discord.Interaction, id: str, number: int = 1, ephemeral: bool = True
    ):
        if not render.is_id(id):
            await interaction.response.send_message(
                "That is not a post id: 64 hex characters, as `/swarm search` shows them.",
                ephemeral=True,
            )
            return
        why = await bot.refused(interaction)
        if why:
            await interaction.response.send_message(why, ephemeral=True)
            return
        await interaction.response.defer(ephemeral=ephemeral, thinking=True)
        try:
            result = await bot.mcp.call(
                "read", {"id": id.strip(), "fetch_blobs": True, "max_tokens": 300}
            )
            blobs = (result.get("post") or {}).get("blobs") or []
            if not 1 <= number <= len(blobs):
                raise files.Refused(f"this post has {len(blobs)} attached file(s)")
            blob = blobs[number - 1]
            name, _ = files.check(blob.get("local_path"), blob.get("name"))
        except (McpError, files.Refused, OSError) as e:
            await interaction.followup.send(
                "Not sent: " + render.inert(str(e), 300), ephemeral=True
            )
            return
        await interaction.followup.send(
            content=f"{render.code(name)} from post {render.code(id.strip())}: an image "
            "made by an agent of the swarm, not verified.",
            file=discord.File(blob["local_path"], filename=name),
            ephemeral=ephemeral,
            allowed_mentions=discord.AllowedMentions.none(),
        )

    return group


def main():
    logging.basicConfig(level=logging.INFO)
    try:
        loaded = envfile.load()
    except envfile.EnvFileError as e:
        raise SystemExit(str(e)) from None
    if loaded:
        log.info("settings from %s", loaded)
    token = os.environ.get("DISCORD_TOKEN", "")
    if not token:
        raise SystemExit("DISCORD_TOKEN is not set")
    bot = SwarmBot()
    # The token is passed, never logged; discord.py's own logging is kept.
    bot.run(token, log_handler=None)


if __name__ == "__main__":
    main()
