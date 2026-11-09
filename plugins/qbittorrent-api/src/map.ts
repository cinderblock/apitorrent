/**
 * apitorrent's torrents in qBittorrent's shapes (Web API v2, as of qBittorrent 4.6).
 *
 * The approximations, where apitorrent keeps something different:
 *
 * - **hash** is the v1 info-hash, or the first 40 hex digits of the v2 one for a v2-only
 *   torrent, as qBittorrent reports them.
 * - **completion_on** is unknown: `-1` until finished, the time it was added after.
 *   **last_activity** is now while data moves, otherwise when it was added.
 * - Session totals equal all-time totals; **force_start**, **super_seeding**,
 *   **seq_dl** and **auto_tmm** are always false.
 */

import type { AppInfo, SeedLimits, TorrentDetail, TorrentState, TorrentSummary } from "@apitorrent/plugin-sdk";

import { joinPath } from "./paths";

/** qBittorrent's "infinite" ETA (100 days). */
export const ETA_INFINITY = 8_640_000;

export function qbHash(t: Pick<TorrentSummary, "infoHashV1" | "infoHashV2" | "id">): string {
  return (t.infoHashV1 ?? t.infoHashV2?.slice(0, 40) ?? t.id).toLowerCase();
}

/** Whether a hash a client sent names this torrent (any of its forms, any case). */
export function hashMatches(t: TorrentSummary, hash: string): boolean {
  const h = hash.toLowerCase();
  return h === qbHash(t) || h === t.id.toLowerCase() || h === t.infoHashV2?.toLowerCase();
}

export function qbState(t: TorrentSummary): string {
  const done = t.progress >= 1;
  const active: Record<TorrentState, string> = {
    metadata: "metaDL",
    checking: done ? "checkingUP" : "checkingDL",
    downloading: t.downloadRate > 0 ? "downloading" : "stalledDL",
    seeding: t.uploadRate > 0 ? "uploading" : "stalledUP",
    paused: done ? "pausedUP" : "pausedDL",
    queued: done ? "queuedUP" : "queuedDL",
    error: "error",
  };
  if (t.error && t.state !== "paused") return "error";
  return active[t.state];
}

/** Effective limits as qBittorrent's per-torrent numbers: `-1` for none. */
function limits(l: SeedLimits) {
  return {
    ratio: l.ratio > 0 ? l.ratio : -1,
    minutes: l.seedMinutes > 0 ? l.seedMinutes : -1,
  };
}

export function ratio(t: TorrentSummary): number {
  const base = t.downloaded > 0 ? t.downloaded : t.size;
  return base > 0 ? Math.round((t.uploaded / base) * 10_000) / 10_000 : 0;
}

/** Bytes of the wanted files still missing. */
function amountLeft(t: TorrentSummary, d: TorrentDetail | undefined): number {
  const wanted = d ? d.files.filter((f) => f.priority > 0).reduce((n, f) => n + f.length, 0) : t.size;
  return Math.max(0, Math.round(wanted * (1 - t.progress)));
}

function wantedSize(t: TorrentSummary, d: TorrentDetail | undefined): number {
  if (!d || d.files.length === 0) return t.size;
  return d.files.filter((f) => f.priority > 0).reduce((n, f) => n + f.length, 0);
}

/**
 * Where the torrent's content is: the file of a single-file torrent, else its top folder.
 * Without metadata yet, the save path and the name.
 */
export function contentPath(t: TorrentSummary, d: TorrentDetail | undefined): string {
  const files = d?.files ?? [];
  if (files.length === 0) return joinPath(t.savePath, t.name);
  if (files.length === 1) return joinPath(t.savePath, files[0]!.path);
  const tops = new Set(files.map((f) => f.path.split("/")[0]));
  if (tops.size === 1 && files.every((f) => f.path.includes("/"))) {
    return joinPath(t.savePath, [...tops][0]!);
  }
  return t.savePath;
}

function trackerUrl(d: TorrentDetail | undefined): string {
  const working = d?.trackerStatus.find((s) => !s.error && s.peers !== null);
  return working?.url ?? "";
}

const secs = (ms: number) => Math.floor(ms / 1000);

/** A torrent as `torrents/info` lists it. */
export function torrentInfo(
  t: TorrentSummary,
  d: TorrentDetail | undefined,
  tags: string[],
  now: number,
) {
  const eff = d?.effectiveSeedLimits ?? { ratio: 0, seedMinutes: 0, action: "pause" as const };
  const lim = limits(eff);
  const left = amountLeft(t, d);
  const size = wantedSize(t, d);
  const addedOn = secs(t.addedAt);
  const moving = t.downloadRate > 0 || t.uploadRate > 0;
  const eta = t.progress >= 1 ? 0 : t.downloadRate > 0 ? Math.ceil(left / t.downloadRate) : ETA_INFINITY;
  return {
    added_on: addedOn,
    amount_left: left,
    auto_tmm: false,
    availability: -1,
    category: t.label ?? "",
    completed: Math.max(0, size - left),
    completion_on: t.progress >= 1 ? addedOn : -1,
    content_path: contentPath(t, d),
    dl_limit: d && d.limits.downloadRate > 0 ? d.limits.downloadRate : -1,
    dlspeed: t.downloadRate,
    download_path: "",
    downloaded: t.downloaded,
    downloaded_session: t.downloaded,
    eta,
    f_l_piece_prio: false,
    force_start: false,
    hash: qbHash(t),
    inactive_seeding_time_limit: -1,
    infohash_v1: t.infoHashV1 ?? "",
    infohash_v2: t.infoHashV2 ?? "",
    last_activity: moving ? secs(now) : addedOn,
    magnet_uri: d?.magnet ?? "",
    max_inactive_seeding_time: -1,
    max_ratio: lim.ratio,
    max_seeding_time: lim.minutes,
    name: t.name,
    num_complete: Math.max(0, ...(d?.trackerStatus.map((s) => s.seeders ?? 0) ?? [0])),
    num_incomplete: Math.max(0, ...(d?.trackerStatus.map((s) => s.leechers ?? 0) ?? [0])),
    num_leechs: Math.max(0, t.peers - t.seeds),
    num_seeds: t.seeds,
    priority: 0,
    private: t.private,
    progress: t.progress,
    ratio: ratio(t),
    ratio_limit: lim.ratio,
    save_path: t.savePath,
    seeding_time: d?.seedingSecs ?? 0,
    seeding_time_limit: lim.minutes,
    seen_complete: t.progress >= 1 ? addedOn : -1,
    seq_dl: false,
    size,
    state: qbState(t),
    super_seeding: false,
    tags: tags.join(", "),
    time_active: Math.max(0, secs(now - t.addedAt)),
    total_size: t.size,
    tracker: trackerUrl(d),
    trackers_count: d?.trackers.flat().length ?? 0,
    up_limit: d && d.limits.uploadRate > 0 ? d.limits.uploadRate : -1,
    uploaded: t.uploaded,
    uploaded_session: t.uploaded,
    upspeed: t.uploadRate,
  };
}

export type TorrentInfo = ReturnType<typeof torrentInfo>;

/** `torrents/properties`. */
export function torrentProperties(t: TorrentSummary, d: TorrentDetail, now: number) {
  const info = torrentInfo(t, d, [], now);
  const elapsed = Math.max(0, secs(now - t.addedAt));
  return {
    addition_date: info.added_on,
    comment: d.comment ?? "",
    completion_date: info.completion_on,
    created_by: d.createdBy ?? "",
    creation_date: d.creationDate === null ? -1 : secs(d.creationDate),
    dl_limit: info.dl_limit,
    dl_speed: t.downloadRate,
    dl_speed_avg: elapsed > 0 ? Math.round(t.downloaded / elapsed) : 0,
    download_path: "",
    eta: info.eta,
    hash: info.hash,
    infohash_v1: info.infohash_v1,
    infohash_v2: info.infohash_v2,
    is_private: t.private,
    isPrivate: t.private,
    last_seen: info.last_activity,
    name: t.name,
    nb_connections: t.peers,
    nb_connections_limit: d.limits.maxConnections ?? -1,
    peers: Math.max(0, t.peers - t.seeds),
    peers_total: info.num_incomplete,
    piece_size: d.pieceLength,
    pieces_have: t.piecesHave,
    pieces_num: t.piecesTotal,
    reannounce: 0,
    save_path: t.savePath,
    seeding_time: d.seedingSecs,
    seeds: t.seeds,
    seeds_total: info.num_complete,
    share_ratio: info.ratio,
    time_elapsed: elapsed,
    total_downloaded: t.downloaded,
    total_downloaded_session: t.downloaded,
    total_size: t.size,
    total_uploaded: t.uploaded,
    total_uploaded_session: t.uploaded,
    total_wasted: 0,
    up_limit: info.up_limit,
    up_speed: t.uploadRate,
    up_speed_avg: elapsed > 0 ? Math.round(t.uploaded / elapsed) : 0,
  };
}

/** qBittorrent's file priorities: 0 skip, 1 normal, 6 high, 7 maximal. */
function filePriority(p: number): number {
  if (p <= 0) return 0;
  if (p >= 7) return 7;
  return p >= 4 ? 6 : 1;
}

/** `torrents/files`. */
export function torrentFiles(d: TorrentDetail) {
  let offset = 0;
  const pieceLength = Math.max(1, d.pieceLength);
  return d.files.map((f) => {
    const first = Math.floor(offset / pieceLength);
    const last = Math.max(first, Math.floor((offset + Math.max(0, f.length - 1)) / pieceLength));
    offset += f.length;
    return {
      availability: -1,
      index: f.index,
      is_seed: f.progress >= 1,
      name: f.path,
      piece_range: [first, last],
      priority: filePriority(f.priority),
      progress: f.progress,
      size: f.length,
    };
  });
}

/** qBittorrent's tracker statuses. */
const TRACKER_DISABLED = 0;
const TRACKER_NOT_CONTACTED = 1;
const TRACKER_WORKING = 2;
const TRACKER_NOT_WORKING = 4;

/** `torrents/trackers`: the DHT, PeX and LSD pseudo-entries first, as qBittorrent lists them. */
export function torrentTrackers(t: TorrentSummary, d: TorrentDetail) {
  const pseudo = ["** [DHT] **", "** [PeX] **", "** [LSD] **"].map((url) => ({
    msg: "",
    num_downloaded: 0,
    num_leeches: 0,
    num_peers: 0,
    num_seeds: 0,
    status: t.private ? TRACKER_DISABLED : TRACKER_WORKING,
    tier: -1,
    url,
  }));
  const real = d.trackerStatus.map((s) => ({
    msg: s.error ?? s.warning ?? "",
    num_downloaded: -1,
    num_leeches: s.leechers ?? -1,
    num_peers: s.peers ?? -1,
    num_seeds: s.seeders ?? -1,
    status: s.error ? TRACKER_NOT_WORKING : s.peers === null ? TRACKER_NOT_CONTACTED : TRACKER_WORKING,
    tier: s.tier,
    url: s.url,
  }));
  return [...pseudo, ...real];
}

/** `app/preferences`: what the *arr apps and friends read, from apitorrent's settings. */
export function preferences(app: AppInfo) {
  const l = app.seedLimits;
  const action = { pause: 0, remove: 1, removeWithData: 3 }[l.action] ?? 0;
  const q = app.queue;
  const limit = (n: number) => (n > 0 ? n : -1);
  return {
    auto_tmm_enabled: false,
    create_subfolder_enabled: true,
    dht: true,
    dl_limit: 0,
    listen_port: app.peerPort,
    lsd: true,
    max_active_downloads: limit(q.maxActiveDownloads),
    max_active_torrents: limit(q.maxActive),
    max_active_uploads: limit(q.maxActiveSeeds),
    max_inactive_seeding_time: -1,
    max_inactive_seeding_time_enabled: false,
    max_ratio: l.ratio > 0 ? l.ratio : -1,
    max_ratio_act: action,
    max_ratio_enabled: l.ratio > 0,
    max_seeding_time: l.seedMinutes > 0 ? l.seedMinutes : -1,
    max_seeding_time_enabled: l.seedMinutes > 0,
    pex: true,
    queueing_enabled: q.maxActiveDownloads > 0 || q.maxActiveSeeds > 0 || q.maxActive > 0,
    save_path: app.downloadDir,
    start_paused_enabled: false,
    temp_path: "",
    temp_path_enabled: false,
    torrent_content_layout: "Original",
    up_limit: 0,
    web_ui_address: "*",
  };
}
