#!/usr/bin/env node
"use strict";

const { KIND_CLI, ensureBinary, runBinary } = require("./difflore-runtime");

function main(argv) {
  if (argv.includes("--self-test")) {
    process.stdout.write(`${ensureBinary(KIND_CLI)}\n`);
    return 0;
  }
  return runBinary(KIND_CLI, ["mcp-server"]);
}

if (require.main === module) {
  try {
    process.exit(main(process.argv.slice(2)));
  } catch (error) {
    process.stderr.write(`${error.message}\n`);
    process.exit(1);
  }
}

module.exports = { main };
