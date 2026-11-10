#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");
const { findBinary } = require("./binary.js");

const binary = findBinary();
if (!binary) {
  console.error(
    `lantern: no prebuilt binary for ${process.platform}-${process.arch}; ` +
      "install from source with `cargo install --git https://github.com/fadhilp/lantern lantern-cli`",
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`lantern: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
