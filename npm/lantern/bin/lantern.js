#!/usr/bin/env node
"use strict";

const { spawnSync } = require("node:child_process");

const { platform, arch } = process;

/** Platform package suffixes to try, best first. A static musl binary also runs on glibc. */
function candidates() {
  const base = `${platform}-${arch}`;
  if (platform === "win32") return [`${base}-msvc`];
  if (platform !== "linux") return [base];
  const glibc = process.report?.getReport?.().header?.glibcVersionRuntime;
  return glibc ? [`${base}-gnu`, `${base}-musl`] : [`${base}-musl`];
}

function binary() {
  const exe = platform === "win32" ? "lantern.exe" : "lantern";
  for (const suffix of candidates()) {
    try {
      return require.resolve(`@fadhilp/lantern-${suffix}/bin/${exe}`);
    } catch {
      // Not installed for this platform; try the next candidate.
    }
  }
  console.error(
    `lantern: no prebuilt binary for ${platform}-${arch}; ` +
      "install from source with `cargo install --git https://github.com/fadhilp/lantern lantern-cli`",
  );
  process.exit(1);
}

const result = spawnSync(binary(), process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`lantern: ${result.error.message}`);
  process.exit(1);
}
if (result.signal) {
  process.kill(process.pid, result.signal);
}
process.exit(result.status ?? 1);
