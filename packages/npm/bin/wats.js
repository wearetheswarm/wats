#!/usr/bin/env node
'use strict';
// `npx -y <package> mcp` runs `wats mcp` from the verified release binary
// of this package's version (lib/launcher.js, ADR 0035).
require('../lib/launcher').main(process.argv.slice(2));
