import { useEffect, useState } from "react";

import { api } from "../api/client";
import type { TlsCertificateSource } from "../api/generated/TlsCertificateSource";
import type { TlsStatus } from "../api/generated/TlsStatus";
import { formatDateTime } from "../format";
import { SettingCard } from "../settings/controls";

const SOURCE: Record<TlsCertificateSource, string> = {
  provided: "your own certificate (reloaded when the files change)",
  selfSigned: "a self-signed certificate",
  acme: "a certificate from ACME (renewed automatically)",
};

/** HTTPS as the server sees it: the certificate, its pairing code, and ACME progress. */
export function TlsSection() {
  const [status, setStatus] = useState<TlsStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api.tls().then(setStatus, (e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  if (error) return <SettingCard><p className="error-text">{error}</p></SettingCard>;
  if (!status) return <SettingCard><p className="muted">Loading…</p></SettingCard>;
  if (!status.enabled) {
    return (
      <SettingCard title="Not served here">
        <p className="muted">
          The server doesn&apos;t serve HTTPS itself. If you reach it through a TLS reverse proxy, that&apos;s expected. Otherwise see &quot;Built-in
          HTTPS&quot; in the README (<code>--tls-cert</code>, <code>--acme-domain</code> or <code>--tls-self-signed</code>).
        </p>
      </SettingCard>
    );
  }
  return (
    <>
      <SettingCard title="Certificate">
        {!status.certificate && <p className="muted">Waiting for the first certificate from ACME.</p>}
        {status.certificate && (
          <dl className="facts">
            <dt>Serving</dt>
            <dd>{SOURCE[status.certificate.source]}</dd>
            <dt>Names</dt>
            <dd>{[...status.certificate.dnsNames, ...status.certificate.ipAddresses].join(", ") || "none"}</dd>
            <dt>Valid until</dt>
            <dd>{formatDateTime(status.certificate.notAfterMs)}</dd>
            <dt>Pairing code</dt>
            <dd>
              <code className="key">{status.certificate.pairingCode}</code>
              <div className="muted">The desktop app and CLI show this code for the key they see. Pair only if the two match.</div>
            </dd>
          </dl>
        )}
      </SettingCard>
      {status.acme && (
        <SettingCard title="ACME">
          <dl className="facts">
            <dt>Domains</dt>
            <dd>{status.acme.domains.join(", ")}</dd>
            <dt>From</dt>
            <dd>{status.acme.directoryUrl}</dd>
            {status.acme.lastIssuedMs != null && (
              <>
                <dt>Last issued</dt>
                <dd>{formatDateTime(status.acme.lastIssuedMs)}</dd>
              </>
            )}
            {status.acme.nextAttemptMs != null && (
              <>
                <dt>Next attempt</dt>
                <dd>{formatDateTime(status.acme.nextAttemptMs)}</dd>
              </>
            )}
            {status.acme.lastError && (
              <>
                <dt>Last error</dt>
                <dd className="error-text">
                  {status.acme.lastError}
                  {status.acme.consecutiveFailures > 1 && ` (${status.acme.consecutiveFailures} failures in a row)`}
                </dd>
              </>
            )}
          </dl>
        </SettingCard>
      )}
    </>
  );
}
