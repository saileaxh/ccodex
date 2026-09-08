const KEY_STORAGE = "ccodex_api_key";

export function getApiKey(): string {
  try {
    return sessionStorage.getItem(KEY_STORAGE) ?? "";
  } catch {
    return "";
  }
}

export function setApiKey(key: string) {
  try {
    sessionStorage.setItem(KEY_STORAGE, key);
  } catch {
    /* 无痕模式等场景忽略 */
  }
}

export class ApiError extends Error {
  status: number;
  constructor(status: number, message: string) {
    super(message);
    this.status = status;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(path, {
    ...init,
    headers: {
      Authorization: `Bearer ${getApiKey()}`,
      ...(init?.body ? { "Content-Type": "application/json" } : {}),
      ...init?.headers,
    },
  });
  if (resp.status === 401) throw new ApiError(401, "登录密钥无效或已过期");
  const text = await resp.text();
  let data: any = {};
  try {
    data = text ? JSON.parse(text) : {};
  } catch {
    /* 非 JSON 响应 */
  }
  if (!resp.ok) throw new ApiError(resp.status, data?.error ?? `HTTP ${resp.status}`);
  return data as T;
}

export interface Overview {
  accounts: number;
  available: number;
  identity_version: string;
  upstream_commit: string;
  listen: string;
  auth_required: boolean;
}

export interface UsageTotals {
  requests: number;
  input_tokens: number;
  cached_input_tokens: number;
  output_tokens: number;
  cost_usd: number;
}

export interface PeriodUsage {
  start_unix: number;
  end_unix: number;
  totals: UsageTotals;
}

export interface AccountInfo {
  name: string;
  account_id: string | null;
  email: string | null;
  plan: string | null;
  available: boolean;
  cooldown_remaining_secs: number;
  quotas: QuotaInfo[] | null;
  proxy: string | null;
  usage: { total: UsageTotals; period: PeriodUsage | null } | null;
}

export interface ProxyCheck {
  ok: boolean;
  ip: string | null;
  latency_ms: number | null;
  error: string | null;
  checked_at_unix: number;
}

export interface ProxyInfo {
  name: string;
  url: string;
  check: ProxyCheck | null;
}

export interface ProxiesResponse {
  config_default: string | null;
  proxies: ProxyInfo[];
  assignments: Record<string, string>;
}

export interface QuotaInfo {
  limit_id?: string | null;
  limit_name?: string | null;
  plan_type?: string | null;
  primary?: { used_percent?: number | null; window_minutes?: number | null; resets_at?: number | null } | null;
  secondary?: { used_percent?: number | null; window_minutes?: number | null; resets_at?: number | null } | null;
  credits?: { has_credits: boolean; unlimited: boolean; balance?: string | null } | null;
}

export interface DeviceLoginStart {
  session_id?: string;
  verification_url?: string;
  user_code?: string;
  error?: string;
}

export interface OAuthLoginStart {
  session_id?: string;
  authorize_url?: string;
  error?: string;
}

export interface DeviceLoginStatus {
  status: "pending" | "done" | "error" | "unknown";
  error?: string | null;
  verification_url?: string;
  user_code?: string;
}

export interface KeyInfo {
  name: string;
  key: string;
  created_at_unix: number | null;
  usage: UsageTotals | null;
}

export interface AuthStatus {
  setup_required: boolean;
  updated_at_unix: number | null;
}

export interface PriceEntry {
  model: string;
  input_per_m: number;
  cached_input_per_m: number;
  output_per_m: number;
  override: boolean;
}

export interface UpstreamVersion {
  baked: string;
  latest: string | null;
  checked_at_unix: number;
  error: string | null;
  ttl_secs: number;
  source: "cache" | "fresh";
}

export const api = {
  authStatus: () => request<AuthStatus>("/admin/api/auth-status"),
  setAdminKey: (current: string, next: string) =>
    request<{ ok: boolean; setup?: boolean; error?: string }>("/admin/api/admin-key", {
      method: "POST",
      body: JSON.stringify({ current, new: next }),
    }),
  overview: () => request<Overview>("/admin/api/overview"),
  accounts: () => request<{ accounts: AccountInfo[] }>("/admin/api/accounts"),
  reloadAccounts: () => request<{ ok: boolean; accounts?: number; error?: string }>(
    "/admin/api/accounts/reload",
    { method: "POST" }
  ),
  startDeviceLogin: (name: string) =>
    request<DeviceLoginStart>("/admin/api/accounts/device-login", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  deviceLoginStatus: (id: string) =>
    request<DeviceLoginStatus>(`/admin/api/accounts/device-login/${id}`),
  startOAuthLogin: (name: string) =>
    request<OAuthLoginStart>("/admin/api/accounts/oauth-login", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  completeOAuthLogin: (id: string, redirectUrl: string) =>
    request<{ ok: boolean; error?: string }>(
      `/admin/api/accounts/oauth-login/${id}/complete`,
      { method: "POST", body: JSON.stringify({ redirect_url: redirectUrl }) }
    ),
  proxies: () => request<ProxiesResponse>("/admin/api/proxies"),
  addProxy: (name: string, url: string) =>
    request<{ ok: boolean; error?: string }>("/admin/api/proxies", {
      method: "POST",
      body: JSON.stringify({ name, url }),
    }),
  deleteProxy: (name: string) =>
    request<{ ok: boolean; error?: string }>(`/admin/api/proxies/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }),
  testProxy: (name?: string | null) =>
    request<ProxyCheck & { error?: string }>("/admin/api/proxies/test", {
      method: "POST",
      body: JSON.stringify({ name: name ?? null }),
    }),
  setAccountProxy: (account: string, proxy: string | null) =>
    request<{ ok: boolean; accounts?: number; error?: string }>(
      `/admin/api/accounts/${encodeURIComponent(account)}/proxy`,
      { method: "PUT", body: JSON.stringify({ proxy }) }
    ),
  refreshAccountQuota: (account: string) =>
    request<{ ok: boolean; quotas?: QuotaInfo[]; error?: string }>(
      `/admin/api/accounts/${encodeURIComponent(account)}/quota-refresh`,
      { method: "POST" }
    ),
  keys: () => request<{ keys: KeyInfo[] }>("/admin/api/keys"),
  addKey: (name: string) =>
    request<{ ok: boolean; name?: string; key?: string; error?: string }>("/admin/api/keys", {
      method: "POST",
      body: JSON.stringify({ name }),
    }),
  deleteKey: (name: string) =>
    request<{ ok: boolean; error?: string }>(`/admin/api/keys/${encodeURIComponent(name)}`, {
      method: "DELETE",
    }),
  upstreamVersion: () => request<UpstreamVersion>("/admin/api/upstream-version"),
  refreshUpstreamVersion: () =>
    request<UpstreamVersion>("/admin/api/upstream-version/refresh", { method: "POST" }),
  pricing: () => request<{ entries: PriceEntry[] }>("/admin/api/pricing"),
  putPricing: (model: string, input: number, cached: number, output: number) =>
    request<{ ok: boolean; error?: string }>("/admin/api/pricing", {
      method: "PUT",
      body: JSON.stringify({
        model,
        input_per_m: input,
        cached_input_per_m: cached,
        output_per_m: output,
      }),
    }),
  deletePricing: (model: string) =>
    request<{ ok: boolean; error?: string }>(
      `/admin/api/pricing/${encodeURIComponent(model)}`,
      { method: "DELETE" }
    ),
};
