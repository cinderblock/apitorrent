// Invitations, enrollment links and user management:
//
// - `/api/v1/auth/enroll/*` (public): open an invitation or recovery link. The link's secret
//   goes in the request body, never the URL.
// - `/api/v1/invitations` and `/api/v1/spaces/<slug>/invitations`: server admins, and a
//   space's admins for that space.
// - `/api/v1/users`: server admins.

import { request } from "../api/client";
import type { AuthStatus } from "../api/generated/AuthStatus";
import type { CeremonyStart } from "../api/generated/CeremonyStart";
import type { CreateInvitationRequest } from "../api/generated/CreateInvitationRequest";
import type { CreatedInvitation } from "../api/generated/CreatedInvitation";
import type { EnrollInfo } from "../api/generated/EnrollInfo";
import type { EnrollPasswordRequest } from "../api/generated/EnrollPasswordRequest";
import type { EnrollStartRequest } from "../api/generated/EnrollStartRequest";
import type { InvitationInfo } from "../api/generated/InvitationInfo";
import type { ManagedUser } from "../api/generated/ManagedUser";
import type { UpdateUserRequest } from "../api/generated/UpdateUserRequest";

const enc = encodeURIComponent;

export const enrollApi = {
  info: (token: string) => request<EnrollInfo>("POST", "/auth/enroll/info", { token }),
  start: (req: EnrollStartRequest) => request<CeremonyStart>("POST", "/auth/enroll/start", req),
  finish: (ceremonyId: string, credential: unknown, passkeyName: string | null) =>
    request<AuthStatus>("POST", "/auth/enroll/finish", { ceremonyId, credential, passkeyName }),
  password: (req: EnrollPasswordRequest) => request<AuthStatus>("POST", "/auth/enroll/password", req),
};

export const invitationsApi = {
  create: (req: CreateInvitationRequest) => request<CreatedInvitation>("POST", "/invitations", req),
  /** Every invitation and recovery link (server admins). */
  list: () => request<InvitationInfo[]>("GET", "/invitations"),
  /** The invitations into one space (its admins). */
  ofSpace: (slug: string) => request<InvitationInfo[]>("GET", `/spaces/${enc(slug)}/invitations`),
  revoke: (id: string) => request<void>("DELETE", `/invitations/${enc(id)}`),
};

export const usersApi = {
  list: () => request<ManagedUser[]>("GET", "/users"),
  update: (id: string, req: UpdateUserRequest) => request<void>("PATCH", `/users/${enc(id)}`, req),
  remove: (id: string) => request<void>("DELETE", `/users/${enc(id)}`),
  recovery: (id: string, expiresInDays: number | null) =>
    request<CreatedInvitation>("POST", `/users/${enc(id)}/recovery`, { expiresInDays }),
};
