#!/usr/bin/env node
"use strict";

const { KIND_HOOK, NOOP_OUTPUT, ensureBinary, runBinary } = require("./difflore-runtime");

function hookArgs(argv) {
  if (argv.length === 0) return ["--client", "codex"];
  return argv;
}

function main(argv) {
  if (argv.includes("--self-test")) {
    process.stdout.write(`${ensureBinary(KIND_HOOK)}\n`);
    return 0;
  }
  try {
    const status = runBinary(KIND_HOOK, hookArgs(argv));
    if (status !== 0) {
      process.stderr.write(`[difflore plugin] difflore-hook exited with status ${status}\n`);
      process.stdout.write(`${NOOP_OUTPUT}\n`);
    }
    return 0;
  } catch (error) {
    process.stderr.write(`[difflore plugin] ${error.message}\n`);
    process.stdout.write(`${NOOP_OUTPUT}\n`);
    return 0;
  }
}

if (require.main === module) {
  process.exit(main(process.argv.slice(2)));
}

module.exports = { hookArgs, main };
