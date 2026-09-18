import { useEffect, useState } from "react";
import { LogIn, RefreshCw, Trash2 } from "lucide-react";
import { Badge, StatusDot } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api, type AccountInfo, type ProxyInfo } from "@/api";
import { toast } from "@/components/ui/toast";
import { cn } from "@/lib/utils";
import { fmtCost, fmtTokens } from "@/pages/Cost";

const PLAN_LABELS: Record<string, string> = {
  free: "Free",
  go: "Go",
  plus: "Plus",
  pro: "Pro",
  pro_lite: "Pro Lite",
  team: "Team",
  business: "Business",
};

function windowLabel(minutes: number | null | undefined): string {
  if (minutes == null) return "窗口";
  if (minutes >= 43200) return "月窗";
  if (minutes >= 10080) return "周窗";
  if (minutes >= 1440) return "日窗";
  return `${Math.round(minutes / 60)}h窗`;
}

function formatReset(resetsAt: number | null | undefined): string {
  if (!resetsAt) return "";
  const d = new Date(resetsAt * 1000);
  const mm = `${d.getMinutes()}`.padStart(2, "0");
  const hh = `${d.getHours()}`.padStart(2, "0");
  if (Date.now() + 24 * 3600 * 1000 > resetsAt * 1000) return `${hh}:${mm} 重置`;
  return `${d.getMonth() + 1}-${d.getDate()} ${hh}:${mm} 重置`;
}

/** Thin quota meter; fill color is threshold status (<70% ok, 70–90% warn, ≥90% critical). */
function QuotaMeter({ pct }: { pct: number }) {
  const tone = pct >= 90 ? "bg-red-400" : pct >= 70 ? "bg-amber-400" : "bg-emerald-400";
  return (
    <div className="h-1 w-16 shrink-0 overflow-hidden rounded-full bg-white/[0.07]">
      <div
        className={cn("h-full rounded-full transition-all", tone)}
        style={{ width: `${Math.min(100, Math.max(0, pct))}%` }}
      />
    </div>
  );
}

function QuotaWindow({
  window: w,
}: {
  window: { used_percent?: number | null; window_minutes?: number | null; resets_at?: number | null } | null | undefined;
}) {
  if (w?.used_percent == null) return null;
  const reset = formatReset(w.resets_at);
  return (
    <div className="flex items-center gap-2">
      <span className="w-10 shrink-0 text-muted-foreground">{windowLabel(w.window_minutes)}</span>
      <QuotaMeter pct={w.used_percent} />
      <span className="tabular-nums text-foreground/90">{Math.round(w.used_percent)}%</span>
      {reset && <span className="text-muted-foreground/70">{reset}</span>}
    </div>
  );
}

function QuotaCell({
  acc,
  refreshing,
  onRefresh,
}: {
  acc: AccountInfo;
  refreshing: boolean;
  onRefresh: () => void;
}) {
  const list = acc.quotas ?? [];
  const q = list.find((x) => x.limit_id === "codex") ?? list[0];
  // 套餐以账号凭证声明（id_token）为准，快照里的 plan_type 兜底
  const plan = acc.plan ?? q?.plan_type;
  const period = acc.usage?.period;
  return (
    <div className="flex items-start gap-1.5">
      <div className="space-y-1.5 text-xs">
        {plan && (
          <div>
            <Badge variant="outline">{PLAN_LABELS[plan] ?? plan}</Badge>
          </div>
        )}
        <QuotaWindow window={q?.primary} />
        <QuotaWindow window={q?.secondary} />
        {q?.credits?.unlimited && <div className="text-muted-foreground">积分无限</div>}
        {q?.credits && !q.credits.unlimited && q.credits.balance != null && (
          <div className="text-muted-foreground">积分余额 {q.credits.balance}</div>
        )}
        {period && (
          <div className="space-y-1 text-muted-foreground">
            <div>
              {windowLabel((period.end_unix - period.start_unix) / 60)}本机记录 {" "}
              {fmtTokens(period.totals.input_tokens + period.totals.output_tokens)}
              {" · 等效 "}{fmtCost(period.totals.cost_usd)}
            </div>
            <div>
              {period.totals.requests} 次 · 输入 {fmtTokens(period.totals.input_tokens)}
              （缓存 {fmtTokens(period.totals.cached_input_tokens)}）· 输出 {fmtTokens(period.totals.output_tokens)}
            </div>
          </div>
        )}
        {!q && !period && (
          <span className="text-muted-foreground/70">—（流量经过时自动快照，或点右侧按钮主动查询）</span>
        )}
      </div>
      <Button
        variant="ghost"
        size="icon"
        className="h-6 w-6 shrink-0"
        title="主动查询上游配额/套餐"
        disabled={refreshing}
        onClick={onRefresh}
      >
        <RefreshCw className={`h-3.5 w-3.5 ${refreshing ? "animate-spin" : ""}`} />
      </Button>
    </div>
  );
}

function formatCooldown(secs: number): string {
  if (secs <= 0) return "—";
  if (secs < 60) return `${secs}s`;
  if (secs < 3600) return `${Math.floor(secs / 60)}m${secs % 60}s`;
  return `${Math.floor(secs / 3600)}h${Math.floor((secs % 3600) / 60)}m`;
}

function formatUnix(unix: number): string {
  const d = new Date(unix * 1000);
  const mm = `${d.getMinutes()}`.padStart(2, "0");
  const hh = `${d.getHours()}`.padStart(2, "0");
  return `${d.getMonth() + 1}-${d.getDate()} ${hh}:${mm}`;
}

/** 状态列：凭证失效（红，附原因）> 冷却中 > 可用 > 待验证（加载后尚无流量裁决） */
function StatusCell({ acc }: { acc: AccountInfo }) {
  const st = acc.auth_status?.state ?? "unknown";
  return (
    <div className="space-y-1">
      <div>
        {st === "invalid" ? (
          <Badge variant="destructive">
            <StatusDot tone="bad" />
            凭证失效
          </Badge>
        ) : !acc.available ? (
          <Badge variant="warning">
            <StatusDot tone="warn" />
            冷却中
          </Badge>
        ) : st === "ok" ? (
          <Badge variant="success">
            <StatusDot tone="ok" />
            可用
          </Badge>
        ) : (
          <Badge variant="outline">
            <StatusDot tone="idle" />
            待验证
          </Badge>
        )}
      </div>
      {st === "invalid" && (
        <div className="text-[10px] text-destructive/80 max-w-60">
          {acc.auth_status?.since_unix ? `${formatUnix(acc.auth_status.since_unix)} · ` : ""}
          {acc.auth_status?.reason ?? "上游拒绝了该账号凭证"}
        </div>
      )}
    </div>
  );
}

export default function AccountsPage({ onRelogin }: { onRelogin: (name: string) => void }) {
  const [accounts, setAccounts] = useState<AccountInfo[] | null>(null);
  const [proxies, setProxies] = useState<ProxyInfo[]>([]);
  const [error, setError] = useState("");
  const [reloading, setReloading] = useState(false);
  const [binding, setBinding] = useState<string | null>(null);
  const [quotaBusy, setQuotaBusy] = useState<string | null>(null);

  const load = () => {
    api
      .accounts()
      .then((r) => setAccounts(r.accounts))
      .catch((e) => setError(String(e)));
    api
      .proxies()
      .then((r) => setProxies(r.proxies))
      .catch(() => {});
  };

  useEffect(() => {
    load();
    const timer = setInterval(load, 5000);
    return () => clearInterval(timer);
  }, []);

  const bind = async (account: string, value: string) => {
    setBinding(account);
    try {
      const r = await api.setAccountProxy(account, value === "" ? null : value);
      if (r.ok) toast.success(`${account} 出口已切换，客户端已重建`);
      else toast.error(r.error ?? "绑定失败");
      await load();
    } catch (e) {
      toast.error(String(e));
    } finally {
      setBinding(null);
    }
  };

  const refreshQuota = async (account: string) => {
    setQuotaBusy(account);
    try {
      const r = await api.refreshAccountQuota(account);
      if (r.ok) toast.success(`${account} 配额已更新`);
      else toast.error(`${account} 查询失败: ${r.error ?? "未知错误"}`);
      await load();
    } catch (e) {
      toast.error(String(e));
    } finally {
      setQuotaBusy(null);
    }
  };

  const reload = async () => {
    setReloading(true);
    try {
      const r = await api.reloadAccounts();
      if (r.ok) toast.success(`已重载，共 ${r.accounts} 个账号`);
      else toast.error(`重载失败: ${r.error}`);
      await load();
    } catch (e) {
      toast.error(String(e));
    } finally {
      setReloading(false);
    }
  };

  const remove = async (acc: AccountInfo) => {
    if (
      !window.confirm(
        `删除账号「${acc.name}」？其凭证目录将被移除，使用该账号的请求立即失败（历史用量统计保留）。`
      )
    )
      return;
    try {
      const r = await api.removeAccount(acc.name);
      if (r.ok) {
        toast.success(`已删除 ${acc.name}，剩余 ${r.accounts ?? "?"} 个账号`);
        load();
      } else {
        toast.error(r.error ?? "删除失败");
      }
    } catch (e) {
      toast.error(String(e));
    }
  };

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold tracking-tight">账号</h1>
        <Button variant="outline" size="sm" onClick={reload} disabled={reloading}>
          <RefreshCw className={`h-4 w-4 ${reloading ? "animate-spin" : ""}`} />
          热重载
        </Button>
      </div>
      {error && <p className="text-sm text-destructive">{error}</p>}
      <Card>
        <CardHeader>
          <CardTitle className="text-base">账号池</CardTitle>
          <CardDescription>每个账号一个官方凭证目录，认证与刷新由官方 AuthManager 处理</CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>名称</TableHead>
                <TableHead>ChatGPT 账号</TableHead>
                <TableHead>状态</TableHead>
                <TableHead>冷却剩余</TableHead>
                <TableHead>出口代理</TableHead>
                <TableHead>上游配额</TableHead>
                <TableHead>操作</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(accounts ?? []).map((acc) => (
                <TableRow key={acc.name}>
                  <TableCell className="font-medium">{acc.name}</TableCell>
                  <TableCell className="text-muted-foreground text-xs">
                    {acc.email ? (
                      <div>
                        <div className="text-foreground">{acc.email}</div>
                        <div className="text-[10px] opacity-60">{acc.account_id}</div>
                      </div>
                    ) : (
                      (acc.account_id ?? "—")
                    )}
                  </TableCell>
                  <TableCell>
                    <StatusCell acc={acc} />
                  </TableCell>
                  <TableCell>{formatCooldown(acc.cooldown_remaining_secs)}</TableCell>
                  <TableCell>
                    <select
                      className="rounded-md border border-white/[0.08] bg-white/[0.03] px-2 py-1 text-xs transition-colors focus:border-transparent focus:outline-none focus:ring-2 focus:ring-ring/60 disabled:opacity-50"
                      value={acc.proxy ?? ""}
                      disabled={binding === acc.name}
                      onChange={(e) => bind(acc.name, e.target.value)}
                    >
                      <option value="">默认</option>
                      {proxies.map((p) => (
                        <option key={p.name} value={p.name}>
                          {p.name}
                        </option>
                      ))}
                      <option value="direct">直连</option>
                    </select>
                  </TableCell>
                  <TableCell>
                    <QuotaCell
                      acc={acc}
                      refreshing={quotaBusy === acc.name}
                      onRefresh={() => refreshQuota(acc.name)}
                    />
                  </TableCell>
                  <TableCell>
                    <div className="flex items-center gap-1">
                      {acc.auth_status?.state === "invalid" && (
                        <Button
                          variant="outline"
                          size="sm"
                          className="h-7 px-2 text-xs"
                          title="用同一账号名重新登录，覆盖已失效的凭证"
                          onClick={() => onRelogin(acc.name)}
                        >
                          <LogIn className="h-3.5 w-3.5 mr-1" />
                          重新登录
                        </Button>
                      )}
                      <Button
                        variant="ghost"
                        size="icon"
                        className="h-7 w-7 text-muted-foreground hover:text-destructive"
                        title="删除账号（凭证目录移除，用量统计保留）"
                        onClick={() => remove(acc)}
                      >
                        <Trash2 className="h-3.5 w-3.5" />
                      </Button>
                    </div>
                  </TableCell>
                </TableRow>
              ))}
              {accounts && accounts.length === 0 && (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground py-8">
                    暂无账号，请到「添加账号」页登录
                  </TableCell>
                </TableRow>
              )}
              {!accounts && (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground py-8">
                    加载中…
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </div>
  );
}
