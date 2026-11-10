// Words for invitations, links and users, kept free of React so they can be tested.

import type { EnrollmentState } from "../api/generated/EnrollmentState";
import type { InvitationInfo } from "../api/generated/InvitationInfo";
import type { Role } from "../api/generated/Role";
import type { SpaceRole } from "../api/generated/SpaceRole";

export const ROLE_TEXT: Record<Role, string> = {
  admin: "Server admin",
  user: "User",
  viewer: "Viewer (reads only, everywhere)",
};

export const SPACE_ROLE_TEXT: Record<SpaceRole, string> = {
  viewer: "Viewer",
  user: "User",
  admin: "Admin",
};

export const STATE_TEXT: Record<EnrollmentState, string> = {
  pending: "Waiting",
  used: "Used",
  revoked: "Revoked",
  expired: "Expired",
};

/** Link lifetimes the forms offer, in days. The server takes 1 to 30. */
export const LIFETIMES = [1, 3, 7, 14, 30] as const;

/** "tv (user), movies (viewer)": what an invitation grants. */
export function describeGrants(inv: Pick<InvitationInfo, "spaces">): string {
  if (inv.spaces.length === 0) return "no spaces";
  return inv.spaces.map((s) => `${s.name} (${SPACE_ROLE_TEXT[s.role].toLowerCase()})`).join(", ");
}

/** "in 3 days", "in 5 hours", "expired": how long a link has left, from `now`. */
export function timeLeft(expiresAt: number, now: number): string {
  const ms = expiresAt - now;
  if (ms <= 0) return "expired";
  const hours = Math.floor(ms / 3_600_000);
  if (hours >= 48) return `in ${Math.floor(hours / 24)} days`;
  if (hours >= 1) return `in ${hours} hour${hours === 1 ? "" : "s"}`;
  const minutes = Math.max(1, Math.floor(ms / 60_000));
  return `in ${minutes} minute${minutes === 1 ? "" : "s"}`;
}

/**
 * The same enrollment page somewhere passkeys work (`passkeyHint` is an origin such as
 * `http://localhost:8742`), keeping the link's secret in the fragment.
 */
export function enrollLinkAt(origin: string, token: string): string {
  return `${origin.replace(/\/+$/, "")}/enroll#token=${encodeURIComponent(token)}`;
}

/** Whether a username is acceptable to the server: 1 to 64 of a-z, 0-9, `.`, `_`, `-`. */
export function validUsername(name: string): boolean {
  return /^[a-z0-9][a-z0-9._-]{0,63}$/.test(name.trim().toLowerCase());
}
