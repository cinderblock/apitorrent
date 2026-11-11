import { beforeAll, describe, expect, test } from "bun:test";

import type { Channel } from "@tauri-apps/api/core";
import { mockIPC } from "@tauri-apps/api/mocks";

import { api } from "../api/client";
import { setTransport } from "../api/transport";
import { desktop, type AppUpdateView, type SyncEvent } from "./bridge";
import { parseLocators } from "../components/StorageRootsSection";
import { toBatch } from "./links";
import { currentNotices } from "./notices";
import { suggestionNote } from "./ProfileEditor";
import { apiPath, DesktopSocket, desktopTransport } from "./transport";
import { splitList } from "./util";

beforeAll(() => {
  // The Tauri mocks install themselves on `window`.
  (globalThis as { window?: unknown }).window ??= globalThis;
});

interface Call {
  cmd: string;
  args: Record<string, unknown>;
}

/** Record every command; answer with `answer`. */
function recordIpc(answer: (c: Call) => unknown): Call[] {
  const calls: Call[] = [];
  mockIPC((cmd, args) => {
    const call = { cmd, args: (args ?? {}) as Record<string, unknown> };
    calls.push(call);
    return answer(call);
  });
  return calls;
}

describe("helpers", () => {
  test("apiPath keeps the path and query only", () => {
    expect(apiPath("/api/v1/torrents?deleteFiles=true")).toBe("/api/v1/torrents?deleteFiles=true");
    expect(apiPath("http://tauri.localhost/api/v1/info")).toBe("/api/v1/info");
  });

  test("splitList", () => {
    expect(splitList(" 10.0.0.1, 10.0.0.0/8  fd00::/8,,")).toEqual(["10.0.0.1", "10.0.0.0/8", "fd00::/8"]);
    expect(splitList("")).toEqual([]);
  });

  test("incoming links become files, magnets and notices", async () => {
    const before = currentNotices().length;
    const batch = toBatch([
      { kind: "magnet", uri: "magnet:?xt=urn:btih:abc" },
      { kind: "torrentFile", name: "a.torrent", data: btoa("d4:infoe") },
      { kind: "problem", message: "couldn't open b.torrent" },
    ]);
    expect(batch.magnets).toEqual(["magnet:?xt=urn:btih:abc"]);
    expect(batch.files).toHaveLength(1);
    expect(batch.files[0]!.name).toBe("a.torrent");
    expect(await batch.files[0]!.text()).toBe("d4:infoe");
    expect(currentNotices().length).toBe(before + 1);
    expect(currentNotices().at(-1)!.text).toBe("couldn't open b.torrent");
  });
});

describe("the desktop transport", () => {
  test("API requests go to the app, not the network", async () => {
    const calls = recordIpc(() => ({ status: 200, contentType: "application/json", body: '{"version":"1","apiVersion":1,"capabilities":[]}' }));
    setTransport(desktopTransport);
    const info = await api.info();
    expect(info.version).toBe("1");
    expect(calls).toEqual([{ cmd: "api_request", args: { method: "GET", path: "/api/v1/info", body: null, contentType: null } }]);
  });

  test("bodies, content types, empty responses and errors pass through", async () => {
    const calls = recordIpc(({ args }) =>
      args.method === "DELETE"
        ? { status: 204, contentType: null, body: "" }
        : { status: 409, contentType: "application/json", body: '{"code":"duplicate","message":"the torrent is already added"}' },
    );
    setTransport(desktopTransport);
    await api.remove("abc", true);
    expect(calls[0]!.args).toMatchObject({ method: "DELETE", path: "/api/v1/torrents/abc?deleteFiles=true" });
    await expect(api.add({ source: { kind: "magnet", uri: "magnet:?x" }, paused: false, label: null, savePath: null, filePriorities: null, unwantedPolicy: "hold" })).rejects.toMatchObject({
      status: 409,
      code: "duplicate",
    });
    expect(calls[1]!.args.contentType).toBe("application/json");
    expect(JSON.parse(calls[1]!.args.body as string).source.uri).toBe("magnet:?x");
  });

  test("the sync socket runs over a channel", async () => {
    let channel: Channel<SyncEvent> | null = null;
    const calls = recordIpc(({ cmd, args }) => {
      if (cmd === "sync_open") {
        channel = args.channel as Channel<SyncEvent>;
        return 7;
      }
      return null;
    });
    const socket = new DesktopSocket();
    const messages: unknown[] = [];
    const closes: number[] = [];
    socket.onmessage = (e) => messages.push(e.data);
    socket.onclose = (e) => closes.push(e.code);
    await Bun.sleep(0);
    expect(channel).not.toBeNull();
    channel!.onmessage({ event: "open" });
    channel!.onmessage({ event: "message", data: '{"type":"snapshot"}' });
    expect(messages).toEqual(['{"type":"snapshot"}']);
    // The server ended the credential: the UI sees the same close code as a browser would.
    channel!.onmessage({ event: "close", code: 4401, reason: "credential no longer valid" });
    expect(closes).toEqual([4401]);
    // Nothing more is delivered after a close, and closing again does nothing.
    channel!.onmessage({ event: "message", data: "late" });
    socket.close();
    expect(messages).toHaveLength(1);
    expect(closes).toEqual([4401]);
    expect(calls.map((c) => c.cmd)).toEqual(["sync_open"]);
  });

  test("closing from the page closes the app's side", async () => {
    const calls = recordIpc(({ cmd }) => (cmd === "sync_open" ? 9 : null));
    const socket = new DesktopSocket();
    const closes: number[] = [];
    socket.onclose = (e) => closes.push(e.code);
    await Bun.sleep(0);
    socket.close();
    await Bun.sleep(0);
    expect(closes).toEqual([1000]);
    expect(calls).toContainEqual({ cmd: "sync_close", args: { id: 9 } });
  });

  test("closing before the app answered still closes the app's side", async () => {
    let release: (id: number) => void = () => {};
    const calls = recordIpc(({ cmd }) => (cmd === "sync_open" ? new Promise<number>((r) => (release = r)) : null));
    const socket = new DesktopSocket();
    socket.close();
    release(11);
    await Bun.sleep(0);
    await Bun.sleep(0);
    expect(calls).toContainEqual({ cmd: "sync_close", args: { id: 11 } });
  });
});

describe("app updates", () => {
  test("check and install call the app's commands", async () => {
    const view: AppUpdateView = {
      currentVersion: "0.1.0",
      latestVersion: "0.2.0",
      available: true,
      notes: null,
      installsEnabled: true,
      installsDisabledReason: null,
      error: null,
    };
    const calls = recordIpc((c) => (c.cmd === "app_update_check" ? view : null));
    expect(await desktop.appUpdateCheck()).toEqual(view);
    await desktop.appUpdateCheck(true);
    await desktop.appUpdateInstall();
    expect(calls.map((c) => [c.cmd, c.args.force])).toEqual([
      ["app_update_check", false],
      ["app_update_check", true],
      ["app_update_install", undefined],
    ]);
  });
});

describe("the folder picker", () => {
  test("asks the app, and passes a cancel through as null", async () => {
    const calls = recordIpc(({ args }) => (args.start === "C:\\Downloads" ? "D:\\Torrents" : null));
    expect(await desktop.pickFolder("Download folder", "C:\\Downloads")).toBe("D:\\Torrents");
    expect(await desktop.pickFolder("Data folder")).toBeNull();
    expect(calls).toEqual([
      { cmd: "pick_folder", args: { title: "Download folder", start: "C:\\Downloads" } },
      { cmd: "pick_folder", args: { title: "Data folder", start: null } },
    ]);
  });
});

describe("mapping suggestions", () => {
  const base = { remote: "/srv/t", local: null, verified: false, via: null, locators: [] };
  test("say what's missing, or nothing when verified", () => {
    expect(suggestionNote({ ...base, local: "Z:\t", verified: true })).toBeNull();
    expect(suggestionNote({ ...base, local: "\\\\nas\\t", via: "smb://nas/t", locators: ["smb://nas/t"] })).toContain("can't be read through \\\\nas\\t");
    expect(suggestionNote({ ...base, locators: ["nfs://nas/srv/t"] })).toContain("Mount that share");
    expect(suggestionNote(base)).toContain("Settings → Storage roots");
  });

  test("locators are edited one per line", () => {
    expect(parseLocators(" \\\\nas\\a,b \r\n\n smb://nas/t \n")).toEqual(["\\\\nas\\a,b", "smb://nas/t"]);
  });
});
