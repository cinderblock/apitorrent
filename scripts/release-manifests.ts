#!/usr/bin/env bun
/**
 * Write a release's update manifests from its built files (the release workflow's last
 * job runs this after signing every file):
 *
 * - `apitorrentd-manifest.json`: the daemon's. For each target, the bare executable's URL,
 *   size and SHA-256. The workflow signs it; the daemon trusts nothing outside it.
 * - `latest.json`: the desktop app's, in the format `tauri-plugin-updater` reads. Each
 *   platform's entry carries the installer's own signature (`<file>.sig`).
 *
 *   bun scripts/release-manifests.ts --dir <files> --tag v1.2.3 --repo owner/name
 *     [--server https://github.com] [--version-from Cargo.toml]
 *
 * The tag must match the workspace version in Cargo.toml: a daemon built as 0.1.0 but
 * released as 1.2.3 would see "1.2.3 is newer" forever after installing it.
 */

import { createHash } from "node:crypto";
import { readdir, readFile, stat, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

export const DAEMON_MANIFEST = "apitorrentd-manifest.json";

export interface DaemonAsset {
  target: string;
  url: string;
  sha256: string;
  size: number;
}

export interface DaemonManifest {
  product: "apitorrentd";
  version: string;
  pubDate: string;
  notesUrl: string;
  assets: DaemonAsset[];
}

export interface LatestJson {
  version: string;
  notes: string;
  pub_date: string;
  platforms: Record<string, { signature: string; url: string }>;
}

export interface Release {
  /** Without the `v`. */
  version: string;
  tag: string;
  /** `https://github.com/owner/name`. */
  repoUrl: string;
  pubDate: string;
}

export function downloadUrl(r: Release, name: string): string {
  return `${r.repoUrl}/releases/download/${encodeURIComponent(r.tag)}/${encodeURIComponent(name)}`;
}

/** `apitorrentd-v1.2.3-x86_64-pc-windows-msvc.exe` → the target, for bare executables. */
export function daemonTarget(name: string, tag: string): string | null {
  const prefix = `apitorrentd-${tag}-`;
  if (!name.startsWith(prefix)) return null;
  const rest = name.slice(prefix.length);
  if (/\.(zip|tar\.gz|sig|json)$/.test(rest)) return null;
  const target = rest.replace(/\.exe$/, "");
  return /^[a-z0-9_]+(-[a-z0-9_]+){2,3}$/.test(target) ? target : null;
}

function arch(name: string): string | null {
  if (/(x86_64|x64|amd64)/.test(name)) return "x86_64";
  if (/(aarch64|arm64)/.test(name)) return "aarch64";
  return null;
}

/** The updater platform keys a desktop installer serves, or none. */
export function desktopPlatforms(name: string): string[] {
  const a = arch(name);
  if (!a) return [];
  if (name.endsWith(".app.tar.gz")) return [`darwin-${a}`, `darwin-${a}-app`];
  if (name.endsWith("-setup.exe")) return [`windows-${a}`, `windows-${a}-nsis`];
  if (name.endsWith(".msi")) return [`windows-${a}-msi`];
  if (name.endsWith(".AppImage")) return [`linux-${a}`, `linux-${a}-appimage`];
  if (name.endsWith(".deb")) return [`linux-${a}-deb`];
  if (name.endsWith(".rpm")) return [`linux-${a}-rpm`];
  return [];
}

export interface FileInfo {
  name: string;
  bytes: Uint8Array;
}

export function daemonManifest(r: Release, files: FileInfo[]): DaemonManifest {
  const assets = files
    .flatMap((f) => {
      const target = daemonTarget(f.name, r.tag);
      if (!target) return [];
      return [
        {
          target,
          url: downloadUrl(r, f.name),
          sha256: createHash("sha256").update(f.bytes).digest("hex"),
          size: f.bytes.length,
        },
      ];
    })
    .sort((a, b) => a.target.localeCompare(b.target));
  const targets = new Set<string>();
  for (const a of assets) {
    if (targets.has(a.target)) throw new Error(`two daemon builds for ${a.target}`);
    targets.add(a.target);
  }
  return {
    product: "apitorrentd",
    version: r.version,
    pubDate: r.pubDate,
    notesUrl: `${r.repoUrl}/releases/tag/${encodeURIComponent(r.tag)}`,
    assets,
  };
}

/** `signatures`: file name to the contents of its `.sig`. Unsigned installers are left out. */
export function latestJson(r: Release, names: string[], signatures: Map<string, string>): LatestJson {
  const platforms: LatestJson["platforms"] = {};
  for (const name of [...names].sort()) {
    const signature = signatures.get(name);
    if (!signature) continue;
    for (const key of desktopPlatforms(name)) {
      if (platforms[key]) throw new Error(`two installers for ${key}: ${name}`);
      platforms[key] = { signature: signature.trim(), url: downloadUrl(r, name) };
    }
  }
  return {
    version: r.version,
    notes: `See ${r.repoUrl}/releases/tag/${r.tag}`,
    pub_date: r.pubDate,
    platforms,
  };
}

/** The `[workspace.package]` version in Cargo.toml. */
export function workspaceVersion(cargoToml: string): string {
  const section = cargoToml.split(/^\[workspace\.package\]\s*$/m)[1]?.split(/^\[/m)[0] ?? "";
  const m = /^version\s*=\s*"([^"]+)"/m.exec(section);
  if (!m) throw new Error("no version in [workspace.package]");
  return m[1]!;
}

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const arg = (name: string, fallback?: string): string => {
    const i = args.indexOf(`--${name}`);
    const v = i >= 0 ? args[i + 1] : fallback;
    if (v === undefined) throw new Error(`--${name} is required`);
    return v;
  };
  const dir = resolve(arg("dir"));
  const tag = arg("tag");
  const version = tag.replace(/^v/, "");
  if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) throw new Error(`${tag} isn't a version tag`);
  const cargo = workspaceVersion(await readFile(resolve(arg("version-from", "Cargo.toml")), "utf8"));
  if (cargo !== version) {
    throw new Error(`the tag says ${version} but Cargo.toml says ${cargo}: bump [workspace.package] version first`);
  }
  const release: Release = {
    version,
    tag,
    repoUrl: `${arg("server", "https://github.com")}/${arg("repo")}`,
    pubDate: new Date().toISOString().replace(/\.\d{3}Z$/, "Z"),
  };
  const names = (await readdir(dir)).sort();
  const files: FileInfo[] = [];
  const signatures = new Map<string, string>();
  for (const name of names) {
    const path = join(dir, name);
    if (!(await stat(path)).isFile()) continue;
    if (name.endsWith(".sig")) {
      signatures.set(name.slice(0, -4), await readFile(path, "utf8"));
    } else if (daemonTarget(name, tag)) {
      files.push({ name, bytes: new Uint8Array(await readFile(path)) });
    }
  }
  const daemon = daemonManifest(release, files);
  if (daemon.assets.length === 0) throw new Error(`no apitorrentd-${tag}-<target> executables in ${dir}`);
  await writeFile(join(dir, DAEMON_MANIFEST), `${JSON.stringify(daemon, null, 2)}\n`);
  const latest = latestJson(release, names, signatures);
  await writeFile(join(dir, "latest.json"), `${JSON.stringify(latest, null, 2)}\n`);
  console.log(`daemon: ${daemon.assets.map((a) => a.target).join(", ")}`);
  console.log(`desktop: ${Object.keys(latest.platforms).join(", ") || "(no signed installers)"}`);
}

if (import.meta.main) {
  await main();
}
