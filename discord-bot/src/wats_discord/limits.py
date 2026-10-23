"""Rate limits of the bot, in memory (ADR 0040). Standard library only."""

import time
from collections import defaultdict, deque


class RateLimit:
    """At most `count` events per `window` seconds, per key."""

    def __init__(self, count, window):
        self.count = count
        self.window = window
        self.seen = defaultdict(deque)

    def allow(self, key, now=None):
        now = time.monotonic() if now is None else now
        times = self.seen[key]
        while times and now - times[0] >= self.window:
            times.popleft()
        if len(times) >= self.count:
            return False
        times.append(now)
        return True
