// Create or edit a connection profile: a remote server (URL, API token, how its certificate
// is trusted, path mappings) or the core running on this computer.

import { useEffect, useRef, useState } from "react";

import {
  desktop,
  type LanSettings,
  type MappingStatus,
  type PathMapping,
  type ProbeView,
  type Profile,
  type PairedDevice,
  type PairingStarted,
  type PairingStatus,
  type ProfileView,
  type Suggestion,
  type Trust,
} from "./bridge";
import { FolderInput } from "./FolderInput";
import { describe, splitList } from "./util";

interface Props {
  /** The profile to edit, or a fresh one to create. */
  initial: ProfileView;
  /** This profile is the active connection (so its mappings can be checked). */
  connected: boolean;
  onSaved: (p: ProfileView) => void;
  onCancel: () => void;
}

export function newRemote(): ProfileView {
  return { id: "", name: "", mode: "remote", baseUrl: "https://", trust: { kind: "os" }, mappings: [], hasToken: false };
}

export function newLocal(): ProfileView {
  return { id: "", name: "This computer", mode: "local", dataDir: null, downloadDir: null, peerPort: 6881, lan: null, hasToken: false };
}

export function ProfileEditor({ initial, connected, onSaved, onCancel }: Props) {
  const [name, setName] = useState(initial.name);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // Remote
  const remote = initial.mode === "remote" ? initial : null;
  const [baseUrl, setBaseUrl] = useState(remote?.baseUrl ?? "https://");
  const [token, setToken] = useState("");
  const [removeToken, setRemoveToken] = useState(false);
  const [trust, setTrust] = useState<Trust>(remote?.trust ?? { kind: "os" });
  const [mappings, setMappings] = useState<PathMapping[]>(remote?.mappings ?? []);
  // An approved device pairing, saved with the profile.
  const [pairing, setPairing] = useState<string | null>(null);

  // Local
  const local = initial.mode === "local" ? initial : null;
  const [peerPort, setPeerPort] = useState(local?.peerPort ?? 6881);
  const [downloadDir, setDownloadDir] = useState(local?.downloadDir ?? "");
  const [dataDir, setDataDir] = useState(local?.dataDir ?? "");
  const [lan, setLan] = useState<LanSettings | null>(local?.lan ?? null);

  async function save(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    if (initial.mode === "remote" && trust.kind === "pinned" && trust.spkiSha256 === "") {
      setError("Pair with the server's code (Pair, under \"Trusting the server's certificate\") before saving.");
      return;
    }
    setBusy(true);
    const profile: Profile =
      initial.mode === "remote"
        ? { id: initial.id, name: name.trim(), mode: "remote", baseUrl: baseUrl.trim(), trust, mappings, device: remote?.device ?? null }
        : {
            id: initial.id,
            name: name.trim(),
            mode: "local",
            peerPort,
            downloadDir: downloadDir.trim() || null,
            dataDir: dataDir.trim() || null,
            lan,
          };
    const tokenArg = initial.mode !== "remote" || pairing ? undefined : token.trim() ? token.trim() : removeToken ? "" : undefined;
    try {
      onSaved(await desktop.saveProfile(profile, tokenArg, pairing ?? undefined));
    } catch (err) {
      setError(describe(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <form onSubmit={(e) => void save(e)} className="profile-editor">
      <h2>{initial.id ? `Edit ${initial.name}` : initial.mode === "remote" ? "Connect to a server" : "Run apitorrent on this computer"}</h2>
      <label className="field">
        <span>Name</span>
        <input value={name} required onChange={(e) => setName(e.target.value)} placeholder={initial.mode === "remote" ? "Seedbox" : "This computer"} />
      </label>
      {initial.mode === "remote" ? (
        <>
          <label className="field">
            <span>Server URL</span>
            <input value={baseUrl} required onChange={(e) => setBaseUrl(e.target.value)} placeholder="https://torrents.example.com" />
          </label>
          <p className="hint muted">Use https://. Plain http:// works only for localhost, such as an SSH tunnel.</p>
          <TrustField baseUrl={baseUrl} trust={trust} setTrust={setTrust} />
          <SignInField
            baseUrl={baseUrl}
            trust={trust}
            device={remote?.device ?? null}
            hasToken={initial.hasToken}
            onPaired={setPairing}
            token={<TokenField hasToken={initial.hasToken} token={token} setToken={setToken} remove={removeToken} setRemove={setRemoveToken} />}
          />
          <MappingsField mappings={mappings} setMappings={setMappings} connected={connected} />
        </>
      ) : (
        <>
          <div className="field-row">
            <label className="field grow">
              <span>Download folder <span className="muted">(empty: Downloads\apitorrent)</span></span>
              <FolderInput value={downloadDir} onChange={setDownloadDir} placeholder="Default" pickerTitle="Download folder" />
            </label>
            <label className="field">
              <span>BitTorrent port</span>
              <input
                type="number"
                min={1}
                max={65534}
                value={peerPort}
                onChange={(e) => setPeerPort(Math.min(65534, Math.max(1, Math.floor(Number(e.target.value) || 6881))))}
              />
            </label>
          </div>
          <label className="field">
            <span>Data folder <span className="muted">(databases and state; empty: the app's own folder)</span></span>
            <FolderInput value={dataDir} onChange={setDataDir} placeholder="Default" pickerTitle="Data folder" />
          </label>
          <LanField lan={lan} setLan={setLan} />
        </>
      )}
      {error && <p className="error-text">{error}</p>}
      <div className="actions">
        <button type="button" onClick={onCancel}>
          Cancel
        </button>
        <button type="submit" className="primary" disabled={busy}>
          {busy ? "Saving…" : "Save"}
        </button>
      </div>
    </form>
  );
}

/** The page's view of a pairing: what to show, from start to approval. */
type PairView = { started: PairingStarted; status: PairingStatus["status"]; deviceName?: string };

function SignInField(props: {
  baseUrl: string;
  trust: Trust;
  device: PairedDevice | null;
  hasToken: boolean;
  onPaired: (pairingId: string | null) => void;
  token: React.ReactNode;
}) {
  const { baseUrl, trust, onPaired } = props;
  const [deviceName, setDeviceName] = useState("");
  const [pair, setPair] = useState<PairView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [showToken, setShowToken] = useState(props.hasToken && !props.device);
  const live = useRef<string | null>(null);

  // Poll while the request waits for approval; stop when it's decided or the editor closes.
  useEffect(() => {
    if (!pair || pair.status !== "pending") return;
    const id = pair.started.pairingId;
    const t = setTimeout(() => {
      desktop.pairPoll(id).then(
        (s) => {
          if (live.current !== id) return;
          setPair({ ...pair, status: s.status, deviceName: s.status === "approved" ? s.deviceName : undefined });
          if (s.status === "approved") onPaired(id);
        },
        (e: unknown) => {
          if (live.current !== id) return;
          setError(describe(e));
          setPair({ ...pair, status: "expired" });
        },
      );
    }, pair.started.intervalSecs * 1000);
    return () => clearTimeout(t);
  }, [pair, onPaired]);

  // A pairing that never got saved is dropped in the app when the editor closes.
  useEffect(
    () => () => {
      if (live.current) void desktop.pairCancel(live.current);
    },
    [],
  );

  const notPairedYet = trust.kind === "pinned" && trust.spkiSha256 === "";

  async function start() {
    setBusy(true);
    setError(null);
    onPaired(null);
    if (live.current) void desktop.pairCancel(live.current);
    try {
      const started = await desktop.pairStart(baseUrl.trim(), trust, deviceName.trim() || undefined);
      live.current = started.pairingId;
      setPair({ started, status: "pending" });
      // Straight to the browser, where the user is signed in to the server.
      await desktop.pairOpen(started.pairingId).catch(() => {});
    } catch (e) {
      setError(describe(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <fieldset className="settings-section">
      <legend>Signing in</legend>
      <p className="hint muted">
        Pair this app with your account: the server shows a short code, you approve it in your browser (where your passkey works), and the app is listed on
        your Account page as a device you can remove at any time. Its credential is kept in this computer's keychain, never in a file.
      </p>
      {props.device && !pair && (
        <p className="hint">
          Paired as <strong>{props.device.name}</strong>. Pair again to replace it.
        </p>
      )}
      {!props.device && props.hasToken && !pair && <p className="hint">Signed in with an API token.</p>}
      <div className="inline-edit">
        <input
          aria-label="Device name"
          value={deviceName}
          placeholder="Device name (default: this computer's)"
          onChange={(e) => setDeviceName(e.target.value)}
        />
        <button type="button" disabled={busy || notPairedYet || !baseUrl.startsWith("http")} onClick={() => void start()}>
          {pair ? "Start again" : props.device ? "Pair again" : "Pair this device"}
        </button>
      </div>
      {notPairedYet && <p className="hint muted">Pair with the server's key first (above), so the app knows it's talking to your server.</p>}
      {pair && <PairProgress pair={pair} />}
      {error && <p className="error-text">{error}</p>}
      {!showToken ? (
        <button type="button" className="link" onClick={() => setShowToken(true)}>
          Use an API token instead
        </button>
      ) : (
        props.token
      )}
    </fieldset>
  );
}

function PairProgress({ pair }: { pair: PairView }) {
  const { started, status } = pair;
  if (status === "approved") {
    return <p className="ok-text">Approved as {pair.deviceName}. Save to keep it.</p>;
  }
  if (status === "denied") return <p className="error-text">The request was refused on the server.</p>;
  if (status === "expired") return <p className="error-text">The request expired. Start again.</p>;
  return (
    <div className="pair-progress" role="status">
      <p>
        Approve this code on the server: <code className="key pair-code">{started.userCode}</code>
      </p>
      <p className="hint muted">
        Your browser should have opened the server's pairing page; check that it shows the same code, then approve.{" "}
        <button type="button" className="link" onClick={() => void desktop.pairOpen(started.pairingId)}>
          Open it again
        </button>{" "}
        or enter the code under Account → Paired devices.
        {started.bound && " The device will be bound to the key you paired with."}
      </p>
      <p className="muted">Waiting for approval…</p>
    </div>
  );
}

function TokenField(props: { hasToken: boolean; token: string; setToken: (t: string) => void; remove: boolean; setRemove: (r: boolean) => void }) {
  return (
    <div className="indent">
      <p className="hint muted">
        Or sign in with an API token: in the server's web UI, open Account → API tokens, create one (scope <strong>full</strong> to manage torrents,{" "}
        <strong>read</strong> to only watch), and paste it here. Passkeys can't be used in this window: they belong to the server's own web address.
      </p>
      <label className="field">
        <span>{props.hasToken ? "A token is saved. Paste a new one to replace it, or leave this empty." : "Token"}</span>
        <input
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={props.token}
          placeholder="apt_…"
          onChange={(e) => props.setToken(e.target.value)}
        />
      </label>
      {props.hasToken && (
        <label className="check">
          <input type="checkbox" checked={props.remove} onChange={(e) => props.setRemove(e.target.checked)} />
          Remove the saved token
        </label>
      )}
    </div>
  );
}

function TrustField({ baseUrl, trust, setTrust }: { baseUrl: string; trust: Trust; setTrust: (t: Trust) => void }) {
  const [code, setCode] = useState("");
  const [probe, setProbe] = useState<ProbeView | null>(null);
  const [message, setMessage] = useState<{ ok: boolean; text: string } | null>(null);
  const [busy, setBusy] = useState(false);

  async function run(action: () => Promise<void>) {
    setBusy(true);
    setMessage(null);
    try {
      await action();
    } catch (err) {
      setMessage({ ok: false, text: describe(err) });
    } finally {
      setBusy(false);
    }
  }

  const check = () =>
    run(async () => {
      // Not paired yet: there is no key to check against. Pairing with the typed code checks
      // the connection too.
      if (trust.kind === "pinned" && trust.spkiSha256 === "") {
        if (code.trim() === "") {
          setMessage({ ok: false, text: "Type the pairing code the server shows, then press Pair." });
          return;
        }
        const t = await desktop.pair(baseUrl.trim(), code);
        setTrust(t);
        setMessage({ ok: true, text: "Paired: the server's key matches the code. Save to keep it." });
        return;
      }
      const p = await desktop.probe(baseUrl.trim(), trust);
      setProbe(p);
      setMessage(p.accepted ? { ok: true, text: "The server's certificate is trusted with this setting." } : { ok: false, text: p.error ?? "Not trusted." });
    });

  const pair = () =>
    run(async () => {
      const t = await desktop.pair(baseUrl.trim(), code);
      setTrust(t);
      setMessage({ ok: true, text: "Paired: the server's key matches the code. Save to keep it." });
    });

  async function loadCa(file: File | undefined) {
    if (!file) return;
    const text = await file.text();
    if (!text.includes("-----BEGIN CERTIFICATE-----")) {
      setMessage({ ok: false, text: `${file.name} isn't a PEM certificate file.` });
      return;
    }
    setTrust({ kind: "privateCa", caPem: text });
    setMessage(null);
  }

  const caCount = trust.kind === "privateCa" ? trust.caPem.split("-----BEGIN CERTIFICATE-----").length - 1 : 0;

  return (
    <fieldset className="settings-section">
      <legend>Trusting the server's certificate</legend>
      <p className="hint muted">apitorrent never connects to a server it can't verify, and has no way to skip the check.</p>
      <label className="check">
        <input type="radio" name="trust" checked={trust.kind === "os"} onChange={() => setTrust({ kind: "os" })} />
        Its certificate is from a public CA, or one this computer already trusts
      </label>
      <label className="check">
        <input type="radio" name="trust" checked={trust.kind === "privateCa"} onChange={() => setTrust({ kind: "privateCa", caPem: "" })} />
        It uses my own CA
      </label>
      {trust.kind === "privateCa" && (
        <label className="field indent">
          <span>{caCount > 0 ? `${caCount} CA certificate${caCount === 1 ? "" : "s"} loaded. Choose another file to replace.` : "The CA certificate (PEM)"}</span>
          <input type="file" accept=".pem,.crt,.cer" onChange={(e) => void loadCa(e.target.files?.[0])} />
        </label>
      )}
      <label className="check">
        <input
          type="radio"
          name="trust"
          checked={trust.kind === "pinned"}
          onChange={() => {
            if (trust.kind !== "pinned") setTrust({ kind: "pinned", spkiSha256: "" });
          }}
        />
        Pair with the code the server shows
      </label>
      {trust.kind === "pinned" && (
        <div className="indent">
          {trust.spkiSha256 && (
            <p className="hint">
              Paired with key <code className="key">{group(trust.spkiSha256)}</code>
            </p>
          )}
          <div className="inline-edit">
            <input
              aria-label="Pairing code"
              className="pairing-code"
              value={code}
              spellCheck={false}
              placeholder="Pairing code from the server"
              onChange={(e) => setCode(e.target.value)}
            />
            <button type="button" disabled={busy || code.trim() === ""} onClick={() => void pair()}>
              Pair
            </button>
          </div>
          <p className="hint muted">
            Type the code the server prints when it starts with <code>--tls-self-signed</code> (also under <code>GET /api/v1/tls</code>), such as
            ABCD-EFGH-IJKL-MNOP-QRST. Pairing succeeds only if the server presents exactly that key, and from then on only that key is trusted.
          </p>
        </div>
      )}
      <div className="inline-edit">
        <button type="button" disabled={busy || !baseUrl.startsWith("http")} onClick={() => void check()}>
          Check connection
        </button>
      </div>
      {probe?.presentedCode && (
        <p className="hint">
          The server presents the key with code <code className="key">{probe.presentedCode}</code>
        </p>
      )}
      {message && <p className={message.ok ? "ok-text" : "error-text"}>{message.text}</p>}
    </fieldset>
  );
}

function group(hex: string): string {
  return hex.match(/.{1,4}/g)?.join(" ") ?? hex;
}

function MappingsField({ mappings, setMappings, connected }: { mappings: PathMapping[]; setMappings: (m: PathMapping[]) => void; connected: boolean }) {
  const [statuses, setStatuses] = useState<MappingStatus[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notes, setNotes] = useState<string[]>([]);

  const update = (i: number, m: Partial<PathMapping>) => {
    setMappings(mappings.map((old, j) => (i === j ? { ...old, ...m } : old)));
    setStatuses(null);
  };

  async function check() {
    setError(null);
    try {
      setStatuses(await desktop.checkMappings(mappings));
    } catch (err) {
      setError(describe(err));
    }
  }

  async function suggest() {
    setError(null);
    setNotes([]);
    try {
      const s = await desktop.suggestions();
      const known = new Set(mappings.map((m) => m.remote));
      const fresh = s.filter((x) => !known.has(x.remote));
      if (fresh.length === 0) {
        setError(s.length === 0 ? "The server reports no storage roots." : "Every storage root already has a mapping.");
        return;
      }
      const next = [...mappings, ...fresh.map((x) => ({ remote: x.remote, local: x.local ?? "" }))];
      setMappings(next);
      setNotes(fresh.map(suggestionNote).filter((n): n is string => n !== null));
      // Show at once which of them work.
      setStatuses(await desktop.checkMappings(next));
    } catch (err) {
      setError(describe(err));
    }
  }

  return (
    <fieldset className="settings-section">
      <legend>Opening downloads from this computer</legend>
      <p className="hint muted">
        Map folders on the server to where this computer reaches them (a network share, say), to open downloads in your file manager. A mapping is used only
        after apitorrent finds the server's <code>.apitorrent-root</code> marker through it.
      </p>
      {mappings.length > 0 && (
        <table className="files mappings">
          <thead>
            <tr>
              <th>On the server</th>
              <th>On this computer</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {mappings.map((m, i) => (
              <tr key={i}>
                <td>
                  <input aria-label="Folder on the server" value={m.remote} placeholder="/srv/torrents" onChange={(e) => update(i, { remote: e.target.value })} />
                </td>
                <td>
                  <FolderInput
                    aria-label="Folder on this computer"
                    value={m.local}
                    placeholder="\\nas\torrents or /mnt/torrents"
                    pickerTitle={m.remote ? `Where this computer reaches ${m.remote}` : "Folder on this computer"}
                    onChange={(local) => update(i, { local })}
                  />
                  {statuses?.[i] && (
                    <span className={statuses[i].verified ? "ok-text" : "error-text"}>{statuses[i].verified ? "Verified" : statuses[i].problem}</span>
                  )}
                </td>
                <td>
                  <button type="button" className="link" onClick={() => setMappings(mappings.filter((_, j) => j !== i))}>
                    Remove
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <div className="inline-edit">
        <button type="button" onClick={() => setMappings([...mappings, { remote: "", local: "" }])}>
          Add mapping
        </button>
        <button type="button" disabled={!connected} onClick={() => void suggest()}>
          Suggest from the server
        </button>
        <button type="button" disabled={!connected || mappings.length === 0} onClick={() => void check()}>
          Check mappings
        </button>
      </div>
      {!connected && <p className="hint muted">Connect with this profile to check mappings against the server.</p>}
      {notes.length > 0 && (
        <ul className="hint muted">
          {notes.map((n) => (
            <li key={n}>{n}</li>
          ))}
        </ul>
      )}
      {error && <p className="error-text">{error}</p>}
    </fieldset>
  );
}

/** What to tell the user about a suggestion that isn't ready to use; null when it is. */
export function suggestionNote(s: Suggestion): string | null {
  if (s.verified) return null;
  if (s.local !== null && s.via !== null) {
    return `${s.remote}: the server shares it as ${s.via}, but its marker can't be read through ${s.local} yet. Check that the share is reachable and that you can sign in to it, then press Check mappings.`;
  }
  if (s.locators.length > 0) {
    return `${s.remote}: the server shares it as ${s.locators.join(" or ")}. Mount that share on this computer, then suggest again, or type where it is.`;
  }
  return `${s.remote}: the server's admin hasn't said where it's shared (Settings → Storage roots). Type where this computer reaches it.`;
}

function LanField({ lan, setLan }: { lan: LanSettings | null; setLan: (l: LanSettings | null) => void }) {
  const on = lan !== null;
  const l = lan ?? { listen: "0.0.0.0:8742", trustedProxies: [], publicUrls: [], allowInsecureLan: false, https: true };
  // The lists are edited as text, so a half-typed "a, " keeps its comma.
  const [proxiesText, setProxiesText] = useState(l.trustedProxies.join(", "));
  const [urlsText, setUrlsText] = useState(l.publicUrls.join(", "));
  return (
    <fieldset className="settings-section">
      <legend>Network access</legend>
      <label className="check">
        <input type="checkbox" checked={on} onChange={(e) => setLan(e.target.checked ? l : null)} />
        Also serve the web UI and API to other devices, like the daemon does
      </label>
      {on && (
        <div className="indent">
          <p className="hint muted">
            Everyone on the network must sign in; the first account is created with a one-time setup link shown here once this profile runs. This window never
            needs to sign in.
          </p>
          <label className="field">
            <span>Listen on</span>
            <input value={l.listen} onChange={(e) => setLan({ ...l, listen: e.target.value })} placeholder="0.0.0.0:8742" />
          </label>
          <label className="field">
            <span>
              Trusted reverse proxies <span className="muted">(addresses or networks, comma-separated)</span>
            </span>
            <input
              value={proxiesText}
              onChange={(e) => {
                setProxiesText(e.target.value);
                setLan({ ...l, trustedProxies: splitList(e.target.value) });
              }}
              placeholder="10.0.0.2"
            />
          </label>
          <label className="field">
            <span>
              Public https:// URLs <span className="muted">(through the proxy; passkeys work there)</span>
            </span>
            <input
              value={urlsText}
              onChange={(e) => {
                setUrlsText(e.target.value);
                setLan({ ...l, publicUrls: splitList(e.target.value) });
              }}
              placeholder="https://torrents.example.com"
            />
          </label>
          <label className="check">
            <input type="checkbox" checked={l.https} onChange={(e) => setLan({ ...l, https: e.target.checked })} />
            Serve HTTPS with a self-signed certificate
          </label>
          <p className="hint muted">
            Other apps pair with it by the code shown once it runs. Browsers warn about the certificate once, and sign in with a password (passkeys need a
            trusted certificate: use a reverse proxy with a public URL for those).
          </p>
          {!l.https && (
            <label className="check">
              <input type="checkbox" checked={l.allowInsecureLan} onChange={(e) => setLan({ ...l, allowInsecureLan: e.target.checked })} />
              Allow plain HTTP without a proxy
            </label>
          )}
          {!l.https && l.allowInsecureLan && (
            <p className="hint error-text">
              Passwords and session cookies then cross the network unencrypted, and browsers won't offer passkeys. Use a TLS reverse proxy if you can.
            </p>
          )}
        </div>
      )}
    </fieldset>
  );
}
