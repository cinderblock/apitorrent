import { describe, expect, test } from "bun:test";

import { daemonManifest, daemonTarget, desktopPlatforms, latestJson, workspaceVersion, type Release } from "./release-manifests";

const release: Release = {
  version: "1.2.3",
  tag: "v1.2.3",
  repoUrl: "https://github.com/cinderblock/apitorrent",
  pubDate: "2026-11-09T20:54:00Z",
};

describe("release manifests", () => {
  test("daemon targets come from bare executables only", () => {
    expect(daemonTarget("apitorrentd-v1.2.3-x86_64-unknown-linux-gnu", "v1.2.3")).toBe("x86_64-unknown-linux-gnu");
    expect(daemonTarget("apitorrentd-v1.2.3-x86_64-pc-windows-msvc.exe", "v1.2.3")).toBe("x86_64-pc-windows-msvc");
    expect(daemonTarget("apitorrentd-v1.2.3-aarch64-apple-darwin", "v1.2.3")).toBe("aarch64-apple-darwin");
    expect(daemonTarget("apitorrentd-v1.2.3-x86_64-unknown-linux-gnu.tar.gz", "v1.2.3")).toBeNull();
    expect(daemonTarget("apitorrentd-v1.2.3-x86_64-pc-windows-msvc.zip", "v1.2.3")).toBeNull();
    expect(daemonTarget("apitorrentd-v1.2.3-x86_64-unknown-linux-gnu.sig", "v1.2.3")).toBeNull();
    expect(daemonTarget("apitorrentd-v1.2.2-x86_64-unknown-linux-gnu", "v1.2.3")).toBeNull();
  });

  test("the daemon manifest hashes each build", () => {
    const m = daemonManifest(release, [
      { name: "apitorrentd-v1.2.3-x86_64-unknown-linux-gnu", bytes: new TextEncoder().encode("abc") },
      { name: "apitorrentd-v1.2.3-x86_64-unknown-linux-gnu.tar.gz", bytes: new Uint8Array(1) },
    ]);
    expect(m.product).toBe("apitorrentd");
    expect(m.version).toBe("1.2.3");
    expect(m.assets).toEqual([
      {
        target: "x86_64-unknown-linux-gnu",
        url: "https://github.com/cinderblock/apitorrent/releases/download/v1.2.3/apitorrentd-v1.2.3-x86_64-unknown-linux-gnu",
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        size: 3,
      },
    ]);
  });

  test("latest.json lists signed installers by platform", () => {
    expect(desktopPlatforms("apitorrent_1.2.3_x64-setup.exe")).toEqual(["windows-x86_64", "windows-x86_64-nsis"]);
    expect(desktopPlatforms("apitorrent_1.2.3_aarch64.app.tar.gz")).toEqual(["darwin-aarch64", "darwin-aarch64-app"]);
    expect(desktopPlatforms("apitorrent_1.2.3_amd64.AppImage")).toEqual(["linux-x86_64", "linux-x86_64-appimage"]);
    expect(desktopPlatforms("apitorrent_1.2.3_aarch64.dmg")).toEqual([]);
    const names = ["apitorrent_1.2.3_x64-setup.exe", "apitorrent_1.2.3_x64_en-US.msi", "apitorrent_1.2.3_amd64.deb"];
    const sigs = new Map([
      ["apitorrent_1.2.3_x64-setup.exe", "c2lnMQ==\n"],
      ["apitorrent_1.2.3_x64_en-US.msi", "c2lnMg=="],
    ]);
    const l = latestJson(release, names, sigs);
    expect(Object.keys(l.platforms).sort()).toEqual(["windows-x86_64", "windows-x86_64-msi", "windows-x86_64-nsis"]);
    expect(l.platforms["windows-x86_64"]).toEqual({
      signature: "c2lnMQ==",
      url: "https://github.com/cinderblock/apitorrent/releases/download/v1.2.3/apitorrent_1.2.3_x64-setup.exe",
    });
  });

  test("the workspace version", () => {
    expect(workspaceVersion('[workspace]\nmembers = []\n\n[workspace.package]\nversion = "0.1.0"\nedition = "2024"\n')).toBe("0.1.0");
    expect(() => workspaceVersion("[package]\nversion = \"1.0.0\"\n")).toThrow();
  });
});
