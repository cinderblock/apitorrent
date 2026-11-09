#!/usr/bin/env bun
/**
 * Build a plugin into the file apitorrent installs: `dist/<id>-<version>.plugin.json`,
 * holding the manifest and one ES module.
 *
 *   apitorrent-plugin-build [plugin dir] [--entry src/index.ts] [--out dist]
 *
 * The plugin directory has `manifest.json` (see `PluginManifest`) and, by default, its
 * entry point at `src/index.ts`. Everything it imports is bundled; the bundle is built for
 * a browser-like target, so npm packages that need Node built-ins fail here rather than at
 * run time.
 */

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

import type { PluginManifest } from "./index";

export interface BuildOptions {
  entry?: string;
  outDir?: string;
  /** Keep the code readable (the default); admins can read what they approve. */
  minify?: boolean;
}

export interface BuiltPlugin {
  path: string;
  bundle: { format: 1; manifest: PluginManifest; code: string };
}

export async function buildPlugin(dir = ".", options: BuildOptions = {}): Promise<BuiltPlugin> {
  const root = resolve(dir);
  const manifest = JSON.parse(await readFile(join(root, "manifest.json"), "utf8")) as PluginManifest;
  const entry = resolve(root, options.entry ?? "src/index.ts");
  const result = await Bun.build({
    entrypoints: [entry],
    target: "browser",
    format: "esm",
    splitting: false,
    sourcemap: "none",
    minify: options.minify ?? false,
  });
  if (!result.success) {
    throw new Error(`build failed:\n${result.logs.map(String).join("\n")}`);
  }
  if (result.outputs.length !== 1) {
    throw new Error(`expected one output file, got ${result.outputs.length}`);
  }
  const code = await result.outputs[0]!.text();
  // The host has no module loader: everything must be in this one file.
  const leftover = /^\s*import\s[^;]*from\s*["'][^"']+["']/m.exec(code);
  if (leftover) throw new Error(`the bundle still imports something: ${leftover[0]}`);
  const bundle = { format: 1 as const, manifest, code };
  const outDir = resolve(root, options.outDir ?? "dist");
  await mkdir(outDir, { recursive: true });
  const path = join(outDir, `${manifest.id}-${manifest.version}.plugin.json`);
  await writeFile(path, JSON.stringify(bundle, null, 2) + "\n");
  return { path, bundle };
}

if (import.meta.main) {
  const args = process.argv.slice(2);
  const flag = (name: string) => {
    const i = args.indexOf(name);
    if (i < 0) return undefined;
    const v = args[i + 1];
    args.splice(i, 2);
    return v;
  };
  const entry = flag("--entry");
  const outDir = flag("--out");
  const minify = args.includes("--minify");
  const dir = args.find((a) => !a.startsWith("--")) ?? ".";
  try {
    const { path, bundle } = await buildPlugin(dir, { entry, outDir, minify });
    console.log(`built ${bundle.manifest.id} ${bundle.manifest.version}: ${path} (${bundle.code.length} bytes)`);
  } catch (e) {
    console.error(e instanceof Error ? e.message : e);
    process.exit(1);
  }
}
