// Passkey ceremonies in the browser. The server sends WebAuthn options as the JSON the spec
// defines; @simplewebauthn/browser turns them into `navigator.credentials` calls and the
// results back into JSON.

import { browserSupportsWebAuthn, startAuthentication, startRegistration } from "@simplewebauthn/browser";

type CreationOptions = Parameters<typeof startRegistration>[0]["optionsJSON"];
type RequestOptions = Parameters<typeof startAuthentication>[0]["optionsJSON"];

export function passkeysSupported(): boolean {
  return browserSupportsWebAuthn();
}

/** Make a new passkey from server-issued creation options. */
export function createPasskey(options: unknown): Promise<unknown> {
  return startRegistration({ optionsJSON: options as CreationOptions });
}

/** Sign with an existing passkey for server-issued request options. */
export function signWithPasskey(options: unknown): Promise<unknown> {
  return startAuthentication({ optionsJSON: options as RequestOptions });
}
