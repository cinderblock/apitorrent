#!/usr/bin/env bun
/**
 * Build every first-party plugin (`plugins/*`) into `dist/first-party-plugins/`, the
 * directory `apitorrentd --features first-party-plugins` embeds. The directory is emptied
 * first, so it never holds a stale version next to the current one.
 *
 *   bun run build:first-party [--out <dir>]
 */

import { mkdir, readdir, rm } from "node:fs/promises";
import { join, resolve } from "node:path";

import { buildPlugin } from "../packages/plugin-sdk/src/build";

const root = resolve(import.meta.dir, "..");
const args = process.argv.slice(2);
const outAt = args.indexOf("--out");
const out = resolve(outAt >= 0 && args[outAt + 1] ? args[outAt + 1]! : join(root, "dist", "first-party-plugins"));

await rm(out, { recursive: true, force: true });
await mkdir(out, { recursive: true });
const dirs = (await readdir(join(root, "plugins"), { withFileTypes: true }))
  .filter((d) => d.isDirectory())
  .map((d) => join(root, "plugins", d.name))
  .sort();
for (const dir of dirs) {
  const { path, bundle } = await buildPlugin(dir, { outDir: out });
  console.log(`${bundle.manifest.id} ${bundle.manifest.version}: ${path}`);
}
