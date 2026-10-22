'use strict';
// Test-only entry point: lib/launcher.js's main() with the options of
// WATS_NPM_TEST_OPTS (the throwaway key, the local mirror, the throwaway
// cache). The published bin/wats.js takes no such overrides.
const opts = JSON.parse(process.env.WATS_NPM_TEST_OPTS);
opts.env = { ...process.env };
delete opts.env.WATS_NPM_TEST_OPTS;
require('../lib/launcher').main(process.argv.slice(2), opts);
