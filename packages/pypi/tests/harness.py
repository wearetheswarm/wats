"""Test-only entry point: launcher.main() with the options of
WATS_PYPI_TEST_OPTS (the throwaway key, the local mirror, the throwaway
cache). The published console scripts take no such overrides."""

import json
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "src"))

from wats_wrapper.launcher import main  # noqa: E402

opts = json.loads(os.environ.pop("WATS_PYPI_TEST_OPTS"))
opts["env"] = dict(os.environ)
main(sys.argv[1:], **opts)
