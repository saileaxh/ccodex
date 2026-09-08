import { useEffect, useState } from "react";
import { RefreshCw } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api, type AccountInfo, type ProxyInfo, type QuotaInfo } from "@/api";
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
      <div className="space-y-0.5 text-xs text-muted-foreground">
        {plan && (
          <div>
            <Badge variant="outline">{PLAN_LABELS[plan] ?? plan}</Badge>
          </div>
        )}
        {q?.primary?.used_percent != null && (
          <div>
            {windowLabel(q.primary.window_minutes)} 已用 {Math.round(q.primary.used_percent)}%
            {q.primary.resets_at ? ` · ${formatReset(q.primary.resets_at)}` : ""}
          </div>
        )}
        {q?.secondary?.used_percent != null && (
          <div>
            {windowLabel(q.secondary.window_minutes)} 已用 {Math.round(q.secondary.used_percent)}%
            {q.secondary.resets_at ? ` · ${formatReset(q.secondary.resets_at)}` : ""}
          </div>
        )}
        {q?.credits?.unlimited && <div>积分无限</div>}
        {q?.credits && !q.credits.unlimited && q.credits.balance != null && (
          <div>积分余额 {q.credits.balance}</div>
        )}
        {period && (
          <div>
            本周期用量 {fmtTokens(period.totals.input_tokens + period.totals.output_tokens)}
            {" · 等效 "}
            {fmtCost(period.totals.cost_usd)}
          </div>
        )}
        {!q && !period && "—（流量经过时自动快照，或点右侧按钮主动查询）"}
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

export default function AccountsPage() {
  const [accounts, setAccounts] = useState<AccountInfo[] | null>(null);
  const [proxies, setProxies] = useState<ProxyInfo[]>([]);
  const [error, setError] = useState("");
  const [reloading, setReloading] = useState(false);
  const [binding, setBinding] = useState<string | null>(null);
  const [quotaBusy, setQuotaBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState("");

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
    setNotice("");
    try {
      const r = await api.setAccountProxy(account, value === "" ? null : value);
      setNotice(r.ok ? `${account} 出口已切换，客户端已重建` : (r.error ?? "绑定失败"));
      await load();
    } catch (e) {
      setNotice(String(e));
    } finally {
      setBinding(null);
    }
  };

  const refreshQuota = async (account: string) => {
    setQuotaBusy(account);
    setNotice("");
    try {
      const r = await api.refreshAccountQuota(account);
      setNotice(r.ok ? `${account} 配额已更新` : `${account} 查询失败: ${r.error ?? "未知错误"}`);
      await load();
    } catch (e) {
      setNotice(String(e));
    } finally {
      setQuotaBusy(null);
    }
  };

  const reload = async () => {
    setReloading(true);
    setNotice("");
    try {
      const r = await api.reloadAccounts();
      setNotice(r.ok ? `已重载，共 ${r.accounts} 个账号` : `重载失败: ${r.error}`);
      await load();
    } catch (e) {
      setNotice(String(e));
    } finally {
      setReloading(false);
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
      {notice && <p className="text-sm text-muted-foreground">{notice}</p>}
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
                    <Badge variant={acc.available ? "success" : "destructive"}>
                      {acc.available ? "可用" : "冷却中"}
                    </Badge>
                  </TableCell>
                  <TableCell>{formatCooldown(acc.cooldown_remaining_secs)}</TableCell>
                  <TableCell>
                    <select
                      className="rounded-md border bg-background px-2 py-1 text-sm"
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
                </TableRow>
              ))}
              {accounts && accounts.length === 0 && (
                <TableRow>
                  <TableCell colSpan={6} className="text-center text-muted-foreground py-8">
                    暂无账号，请到「添加账号」页登录
                  </TableCell>
                </TableRow>
              )}
              {!accounts && (
                <TableRow>
                  <TableCell colSpan={6} className="text-center text-muted-foreground py-8">
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
