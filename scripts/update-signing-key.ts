#!/usr/bin/env bun
/**
 * Make apitorrent's update signing key, once. Run it by hand, never in CI:
 *
 *   bun scripts/update-signing-key.ts [--repo cinderblock/apitorrent]
 *
 * It needs the GitHub CLI (`gh`), signed in to an account that can set the repository's
 * Actions secrets. In order, it:
 *
 * 1. generates a minisign (Ed25519) key pair with the Tauri CLI, the private half
 *    encrypted with a random password, in a fresh private temporary directory;
 * 2. stores the private key and its password as the secrets `TAURI_SIGNING_PRIVATE_KEY`
 *    and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (`gh secret set`, fed on stdin);
 * 3. writes the public key to `release/update-signing.pub`;
 * 4. overwrites and deletes every local copy of the private key and the password, even if
 *    a step failed.
 *
 * Then commit `release/update-signing.pub`: builds from then on install updates.
 *
 * It refuses to run when `release/update-signing.pub` already exists. Replacing the key
 * strands every installed copy (they only accept the old key), so that takes deleting the
 * file by hand first, and a release signed with the old key that ships the new one.
 *
 * The password passes to the Tauri CLI as an argument (it has no other way), so another
 * process of the same user could read it for the second the CLI runs.
 */

import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdtemp, open, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const TAURI_CLI = "@tauri-apps/cli@2.10.1";
const root = resolve(import.meta.dir, "..");
const pubPath = join(root, "release", "update-signing.pub");

const args = process.argv.slice(2);
const repoAt = args.indexOf("--repo");
const repo = repoAt >= 0 && args[repoAt + 1] ? args[repoAt + 1]! : "cinderblock/apitorrent";

async function run(cmd: string[], stdin?: string): Promise<string> {
  const p = Bun.spawn(cmd, {
    stdin: stdin === undefined ? "ignore" : new TextEncoder().encode(stdin),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [out, err, code] = await Promise.all([new Response(p.stdout).text(), new Response(p.stderr).text(), p.exited]);
  if (code !== 0) throw new Error(`${cmd.slice(0, 3).join(" ")} … failed (${code}):\n${err || out}`);
  return out;
}

/** Overwrite a file with zeros, then delete it. */
async function shred(path: string): Promise<void> {
  try {
    const { size } = await stat(path);
    const f = await open(path, "r+");
    await f.write(new Uint8Array(size), 0, size, 0);
    await f.sync();
    await f.close();
  } catch {
    // Missing already.
  }
  await rm(path, { force: true });
}

if (existsSync(pubPath)) {
  console.error(`${pubPath} already exists: the key was made already. See the comment at the top of this script.`);
  process.exit(1);
}

await run(["gh", "auth", "status"]);
await run(["gh", "repo", "view", repo, "--json", "nameWithOwner"]);

const dir = await mkdtemp(join(tmpdir(), "apitorrent-key-"));
const keyPath = join(dir, "update-signing.key");
let password = randomBytes(32).toString("base64url");
try {
  await run(["bunx", TAURI_CLI, "signer", "generate", "--ci", "-p", password, "-w", keyPath]);
  const privateKey = (await readFile(keyPath, "utf8")).trim();
  const publicKey = (await readFile(`${keyPath}.pub`, "utf8")).trim();

  await run(["gh", "secret", "set", "TAURI_SIGNING_PRIVATE_KEY", "--repo", repo], privateKey);
  await run(["gh", "secret", "set", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD", "--repo", repo], password);
  const listed = await run(["gh", "secret", "list", "--repo", repo]);
  for (const name of ["TAURI_SIGNING_PRIVATE_KEY", "TAURI_SIGNING_PRIVATE_KEY_PASSWORD"]) {
    if (!new RegExp(`^${name}\\s`, "m").test(listed)) throw new Error(`${name} isn't listed after setting it`);
  }

  try {
    await writeFile(pubPath, `${publicKey}\n`);
  } catch (e) {
    // The secrets are set; don't lose the public half. It isn't secret.
    console.error(`Couldn't write ${pubPath} (${String(e)}). Save this line there yourself:\n${publicKey}`);
    process.exitCode = 1;
  }
  console.log(`The private key and its password are now secrets of ${repo}, and nowhere else.`);
  console.log(`Public key: ${pubPath}. Commit it.`);
} finally {
  await shred(keyPath);
  await shred(`${keyPath}.pub`);
  await rm(dir, { recursive: true, force: true });
  password = "";
}
