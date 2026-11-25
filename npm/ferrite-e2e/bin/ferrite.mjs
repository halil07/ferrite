#!/usr/bin/env node
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const __dirname = path.dirname(fileURLToPath(import.meta.url));

const os = {
  darwin: "darwin",
  linux: "linux",
  win32: "windows",
}[process.platform];

const arch =
  process.platform === "win32"
    ? "x64"
    : { x64: "x64", arm64: "arm64" }[process.arch];

function resolveBinary() {
  if (process.env.FERRITE_BIN && existsSync(process.env.FERRITE_BIN)) {
    return process.env.FERRITE_BIN;
  }

  const exeName = process.platform === "win32" ? "ferrite.exe" : "ferrite";

  if (os && arch) {
    const pkgName = `@ferrite-e2e/${os}-${arch}`;
    try {
      const pkgDir = path.dirname(require.resolve(`${pkgName}/package.json`));
      const binPath = path.join(pkgDir, "bin", exeName);
      if (existsSync(binPath)) return binPath;
    } catch {
      // Optional platform package not installed; check local monorepo / cargo build outputs.
    }
  }

  const localCandidates = [
    path.resolve(__dirname, "..", "..", "..", "target", "release", exeName),
    path.resolve(__dirname, "..", "..", "..", "target", "debug", exeName),
    path.resolve(process.cwd(), "target", "release", exeName),
  ];
  for (const candidate of localCandidates) {
    if (existsSync(candidate)) return candidate;
  }

  return undefined;
}

const bin = resolveBinary();
if (!bin) {
  console.error(
    `ferrite-e2e: no native Ferrite binary found for ${process.platform}-${process.arch}.\n` +
      `Install platform optionalDependencies, set FERRITE_BIN, or build locally with: cargo build --release -p ferrite-cli`,
  );
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), {
  stdio: "inherit",
  windowsHide: true,
});

for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"]) {
  process.on(signal, () => {
    if (child.exitCode === null && child.signalCode === null) {
      child.kill(signal);
    }
  });
}

child.on("error", (err) => {
  console.error(`ferrite-e2e: failed to start ${bin}: ${err.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});
