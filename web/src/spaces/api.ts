// `/api/v1/space` (the page's space and the caller's spaces) and `/api/v1/spaces` with
// members (server admins, and a space's admins for its members). Users are in `users/api`.

import { request } from "../api/client";
import type { CreateSpaceRequest } from "../api/generated/CreateSpaceRequest";
import type { Space } from "../api/generated/Space";
import type { SpaceContext } from "../api/generated/SpaceContext";
import type { SpaceMember } from "../api/generated/SpaceMember";
import type { SpaceRole } from "../api/generated/SpaceRole";
import type { SpaceView } from "../api/generated/SpaceView";
import type { UpdateSpaceRequest } from "../api/generated/UpdateSpaceRequest";

const enc = encodeURIComponent;

export const spacesApi = {
  context: () => request<SpaceContext>("GET", "/space"),
  list: () => request<SpaceView[]>("GET", "/spaces"),
  get: (slug: string) => request<Space>("GET", `/spaces/${enc(slug)}`),
  create: (req: CreateSpaceRequest) => request<Space>("POST", "/spaces", req),
  update: (slug: string, req: UpdateSpaceRequest) => request<Space>("PUT", `/spaces/${enc(slug)}`, req),
  remove: (slug: string) => request<void>("DELETE", `/spaces/${enc(slug)}`),
  members: (slug: string) => request<SpaceMember[]>("GET", `/spaces/${enc(slug)}/members`),
  setMember: (slug: string, user: string, role: SpaceRole) =>
    request<void>("PUT", `/spaces/${enc(slug)}/members/${enc(user)}`, { role }),
  removeMember: (slug: string, user: string) =>
    request<void>("DELETE", `/spaces/${enc(slug)}/members/${enc(user)}`),
};
