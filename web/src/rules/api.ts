// Automation endpoints: candidates, rules (and the dry run), channels and subscriptions.
// Reads take any credential; rules need one that may write; channels and subscriptions
// are admin-only on the server.

import { request } from "../api/client";
import type { ChannelInfo } from "../api/generated/ChannelInfo";
import type { ChannelSpec } from "../api/generated/ChannelSpec";
import type { CreateChannelRequest } from "../api/generated/CreateChannelRequest";
import type { DryRun } from "../api/generated/DryRun";
import type { RuleInfo } from "../api/generated/RuleInfo";
import type { RuleSpec } from "../api/generated/RuleSpec";
import type { StoredCandidate } from "../api/generated/StoredCandidate";
import type { SubscriptionInfo } from "../api/generated/SubscriptionInfo";
import type { SubscriptionSpec } from "../api/generated/SubscriptionSpec";

const enc = encodeURIComponent;

export const rulesApi = {
  candidates: (source?: string) =>
    request<StoredCandidate[]>("GET", `/candidates${source ? `?source=${enc(source)}` : ""}`),
  rules: () => request<RuleInfo[]>("GET", "/rules"),
  create: (rule: RuleSpec) => request<RuleInfo>("POST", "/rules", rule),
  update: (id: string, rule: RuleSpec) => request<RuleInfo>("PUT", `/rules/${enc(id)}`, rule),
  remove: (id: string) => request<void>("DELETE", `/rules/${enc(id)}`),
  /** Run every rule now; answers with the rules afterwards. */
  run: () => request<RuleInfo[]>("POST", "/rules/run"),
  dryRun: (rule: RuleSpec, ruleId: string | null) =>
    request<DryRun>("POST", "/rules/dry-run", { rule, ruleId }),

  channels: () => request<ChannelInfo[]>("GET", "/channels"),
  createChannel: (req: CreateChannelRequest) => request<ChannelInfo>("POST", "/channels", req),
  updateChannel: (id: string, spec: ChannelSpec) => request<ChannelInfo>("PUT", `/channels/${enc(id)}`, spec),
  removeChannel: (id: string) => request<void>("DELETE", `/channels/${enc(id)}`),

  subscriptions: () => request<SubscriptionInfo[]>("GET", "/subscriptions"),
  subscribe: (spec: SubscriptionSpec) => request<SubscriptionInfo>("POST", "/subscriptions", spec),
  updateSubscription: (id: string, spec: SubscriptionSpec) =>
    request<SubscriptionInfo>("PUT", `/subscriptions/${enc(id)}`, spec),
  unsubscribe: (id: string) => request<void>("DELETE", `/subscriptions/${enc(id)}`),
  refresh: (id: string) => request<SubscriptionInfo>("POST", `/subscriptions/${enc(id)}/refresh`),
};
