// Stages npm packages for a release.
//   node npm/stage-platform.mjs <platform> <binary> <version>   one platform package in npm/dist/<platform>
//   node npm/stage-platform.mjs --main <version>                 versions the main package and its optional dependencies,
//                                                                and copies in the agent skill
import { chmodSync, copyFileSync, cpSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { basename } from "node:path";

const PLATFORMS = {
  "linux-x64-gnu": { os: "linux", cpu: "x64", libc: "glibc" },
  "linux-arm64-gnu": { os: "linux", cpu: "arm64", libc: "glibc" },
  "linux-x64-musl": { os: "linux", cpu: "x64", libc: "musl" },
  "linux-arm64-musl": { os: "linux", cpu: "arm64", libc: "musl" },
  "darwin-x64": { os: "darwin", cpu: "x64" },
  "darwin-arm64": { os: "darwin", cpu: "arm64" },
  "win32-x64-msvc": { os: "win32", cpu: "x64" },
  "win32-arm64-msvc": { os: "win32", cpu: "arm64" },
};

const [mode, ...rest] = process.argv.slice(2);
const write = (path, value) => writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);

if (mode === "--main") {
  const [version] = rest;
  const path = "npm/lantern/package.json";
  const main = JSON.parse(readFileSync(path, "utf8"));
  main.version = version;
  main.optionalDependencies = Object.fromEntries(
    Object.keys(PLATFORMS).map((name) => [`@fadhilp/lantern-${name}`, version]),
  );
  write(path, main);
  cpSync("skills", "npm/lantern/skills", { recursive: true });
} else {
  const [binary, version] = rest;
  const target = PLATFORMS[mode];
  if (!target || !binary || !version) {
    console.error("usage: stage-platform.mjs <platform> <binary> <version> | --main <version>");
    process.exit(2);
  }
  const dir = `npm/dist/${mode}`;
  mkdirSync(`${dir}/bin`, { recursive: true });
  const destination = `${dir}/bin/${basename(binary)}`;
  copyFileSync(binary, destination);
  chmodSync(destination, 0o755);
  write(`${dir}/package.json`, {
    name: `@fadhilp/lantern-${mode}`,
    version,
    description: `Lantern binary for ${mode}`,
    repository: { type: "git", url: "git+https://github.com/fadhilp/lantern.git" },
    os: [target.os],
    cpu: [target.cpu],
    ...(target.libc ? { libc: [target.libc] } : {}),
    files: ["bin"],
  });
}
