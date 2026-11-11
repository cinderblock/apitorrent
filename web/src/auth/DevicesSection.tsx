// Account → Paired devices: approve an app that asks to pair (by the code it shows), and
// list or remove the devices already paired. The desktop app opens `/pair#code=…` here.

import { useEffect, useRef, useState, type FormEvent } from "react";

import { authApi } from "../api/client";
import type { DeviceInfo } from "../api/generated/DeviceInfo";
import type { DeviceRequestInfo } from "../api/generated/DeviceRequestInfo";
import type { TokenScope } from "../api/generated/TokenScope";
import { formatDateTime, formatUtc } from "../format";
import { useSpaceContext } from "../spaces/context";

type Change = (action: () => Promise<unknown>) => Promise<void>;

/** The code from a `/pair#code=ABCD-EFGH` link, if any. */
export function readPairCode(hash: string): string {
  const params = new URLSearchParams(hash.replace(/^#/, ""));
  return (params.get("code") ?? "").trim().slice(0, 32);
}

function When({ ms, empty = "–" }: { ms: number | null; empty?: string }) {
  if (ms === null) return <>{empty}</>;
  return (
    <>
      <time>{formatDateTime(ms)}</time> <small className="muted">{formatUtc(ms)}</small>
    </>
  );
}

interface Props {
  devices: DeviceInfo[];
  busy: boolean;
  change: Change;
  /** A code handed over by a `/pair` link, to look up at once. */
  initialCode: string;
}

export function DevicesSection({ devices, busy, change, initialCode }: Props) {
  const [code, setCode] = useState(initialCode);
  const [request, setRequest] = useState<DeviceRequestInfo | null>(null);
  const [scope, setScope] = useState<TokenScope>("full");
  const [outcome, setOutcome] = useState<string | null>(null);
  const space = useSpaceContext();
  const spaces = space && space.spaces.length > 1 ? space.spaces : null;
  const spaceName = (id: number) => spaces?.find((s) => s.id === id)?.name ?? `space ${id}`;

  function lookUp(e?: FormEvent) {
    e?.preventDefault();
    setOutcome(null);
    void change(async () => {
      setRequest(await authApi.deviceLookup(code));
    });
  }

  // A `/pair#code=…` link: look the code up once, straight away.
  const looked = useRef(false);
  useEffect(() => {
    if (looked.current || initialCode === "") return;
    looked.current = true;
    lookUp();
  }, [initialCode]);

  function decide(approve: boolean) {
    if (!request) return;
    void change(async () => {
      if (approve) await authApi.deviceApprove({ userCode: request.userCode, scope });
      else await authApi.deviceDeny(request.userCode);
      setOutcome(
        approve
          ? `${request.name} is paired. It finishes signing in by itself within a few seconds.`
          : `${request.name} was refused.`,
      );
      setRequest(null);
      setCode("");
    });
  }

  return (
    <section className="account-section">
      <h3>Paired devices</h3>
      <p className="muted">
        Apps that sign in as you, such as the apitorrent desktop app. To pair one, start pairing in the app, then enter the code it shows here (or open the link
        it offers). Each device can be removed on its own. Devices can&apos;t manage passkeys, passwords, tokens or other devices.
      </p>
      <form className="inline-form" onSubmit={lookUp}>
        <input
          value={code}
          onChange={(e) => {
            setCode(e.target.value);
            setRequest(null);
          }}
          placeholder="Code from the app (ABCD-EFGH)"
          aria-label="Pairing code from the app"
          className="pairing-code"
          spellCheck={false}
          autoComplete="off"
        />
        <button type="submit" disabled={busy || code.trim() === ""}>
          Look up
        </button>
      </form>
      {request && (
        <div className="device-request" role="group" aria-label="Pairing request">
          <p>
            <strong>{request.name}</strong> <span className="muted">({request.platform})</span> asks to sign in as you
            {request.ip && (
              <>
                {" "}
                from <code>{request.ip}</code>
              </>
            )}
            , showing the code <code className="key">{request.userCode}</code>.
          </p>
          <p className="banner-warn">
            Approve only if your own app shows this code right now. Someone who sends you a code is asking to use your account.
          </p>
          {request.serverKeyCode ? (
            <p className="hint muted">
              It will be bound to this server&apos;s key <code className="key">{request.serverKeyCode}</code>: if the server&apos;s key changes, pair it again.
            </p>
          ) : (
            <p className="hint muted">It isn&apos;t bound to a server key (it trusts the server through a certificate authority).</p>
          )}
          <div className="inline-form">
            <select value={scope} onChange={(e) => setScope(e.target.value as TokenScope)} aria-label="Access">
              <option value="full">Full access</option>
              <option value="read">Read only</option>
            </select>
            <button type="button" className="primary" disabled={busy} onClick={() => decide(true)}>
              Approve
            </button>
            <button type="button" disabled={busy} onClick={() => decide(false)}>
              Refuse
            </button>
          </div>
          {spaces && space && (
            <p className="hint muted">
              It will work in <strong>{space.current.name}</strong>, as tokens made here do.
            </p>
          )}
        </div>
      )}
      {outcome && (
        <p className="ok-text" role="status">
          {outcome}
        </p>
      )}
      <table className="list">
        <thead>
          <tr>
            <th>Device</th>
            {spaces && <th>Space</th>}
            <th>Access</th>
            <th>Server key</th>
            <th>Paired</th>
            <th>Last used</th>
            <th />
          </tr>
        </thead>
        <tbody>
          {devices.map((d) => (
            <tr key={d.id}>
              <td>
                {d.name} <small className="muted">{d.platform}</small>
              </td>
              {spaces && <td>{spaceName(d.space)}</td>}
              <td>{d.scope === "full" ? "Full" : "Read only"}</td>
              <td>{d.serverKeyCode ? <code className="key">{d.serverKeyCode}</code> : <span className="muted">not bound</span>}</td>
              <td>
                <When ms={d.createdAt} />
              </td>
              <td>
                <When ms={d.lastUsedAt} empty="never" />
                {d.lastIp && <small className="muted"> from {d.lastIp}</small>}
              </td>
              <td className="end">
                <button type="button" className="link danger-text" disabled={busy} onClick={() => void change(() => authApi.removeDevice(d.id))}>
                  Remove
                </button>
              </td>
            </tr>
          ))}
          {devices.length === 0 && (
            <tr>
              <td colSpan={spaces ? 7 : 6} className="muted">
                No paired devices.
              </td>
            </tr>
          )}
        </tbody>
      </table>
    </section>
  );
}
