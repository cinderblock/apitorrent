// Client-side auth decisions, kept free of React and the DOM so they can be tested.

import { ApiRequestError } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";

export type AuthView = "loading" | "unreachable" | "setup" | "login" | "secondFactor" | "app";

/** Which page to show for a status (`null` while loading, `undefined` if it failed). */
export function viewFor(status: AuthStatus | null | undefined): AuthView {
  if (status === null) return "loading";
  if (status === undefined) return "unreachable";
  if (status.user) return "app";
  if (status.setupRequired) return "setup";
  if (status.secondFactorRequired) return "secondFactor";
  return "login";
}

/**
 * The setup token from a `#token=…` fragment. The daemon prints the link that way because
 * a fragment never reaches the server, so the token can't end up in an access log.
 */
export function readSetupToken(hash: string): string | null {
  const params = new URLSearchParams(hash.replace(/^#/, ""));
  const token = params.get("token")?.trim();
  return token ? token : null;
}

/**
 * Run `action`; if the server asks for a fresh sign-in first (`reauth_required`), run
 * `reauth` and try once more. Credential changes need a sign-in from the last few minutes.
 */
export async function withReauth<T>(action: () => Promise<T>, reauth: () => Promise<void>): Promise<T> {
  try {
    return await action();
  } catch (e) {
    if (e instanceof ApiRequestError && e.code === "reauth_required") {
      await reauth();
      return await action();
    }
    throw e;
  }
}

/** Thrown when the user dismisses a dialog; callers show nothing for it. */
export class Cancelled extends Error {
  constructor() {
    super("cancelled");
    this.name = "Cancelled";
  }
}

/** A message for the user, from an API error or a WebAuthn `DOMException`. */
export function describeError(e: unknown): string | null {
  if (e instanceof Cancelled) return null;
  if (e instanceof ApiRequestError) {
    if (e.code === "rate_limited") return e.message;
    if (e.code === "unauthorized") return "That didn't work. Check your passkey or password and try again.";
    return e.message;
  }
  const name = e instanceof Error ? e.name : "";
  switch (name) {
    case "NotAllowedError":
      return "The passkey request was cancelled or timed out.";
    case "InvalidStateError":
      return "This authenticator already has a passkey for this account.";
    case "SecurityError":
      return "The browser refused: passkeys need https, or http://localhost.";
    case "NotSupportedError":
      return "This browser or authenticator doesn't support passkeys.";
    case "AbortError":
      return null;
  }
  return e instanceof Error ? e.message : String(e);
}

/** A short name for the device a session came from, from its user agent. */
export function describeUserAgent(ua: string | null): string {
  if (!ua) return "Unknown device";
  const browser =
    /Edg\//.test(ua) ? "Edge"
    : /Firefox\//.test(ua) ? "Firefox"
    : /Chrome\//.test(ua) ? "Chrome"
    : /Safari\//.test(ua) ? "Safari"
    : null;
  const os =
    /Windows/.test(ua) ? "Windows"
    : /iPhone|iPad/.test(ua) ? "iOS"
    : /Mac OS X/.test(ua) ? "macOS"
    : /Android/.test(ua) ? "Android"
    : /Linux/.test(ua) ? "Linux"
    : null;
  if (browser && os) return `${browser} on ${os}`;
  if (browser ?? os) return (browser ?? os)!;
  return ua.length > 40 ? `${ua.slice(0, 40)}…` : ua;
}
