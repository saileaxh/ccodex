import { useEffect, useState } from "react";
import { Activity, ArrowUpCircle, GitCommit, Globe, RefreshCw, Server, Users } from "lucide-react";
import { Badge, StatusDot } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, type Overview, type UpstreamVersion } from "@/api";
import { toast } from "@/components/ui/toast";

export default function OverviewPage() {
  const [data, setData] = useState<Overview | null>(null);
  const [version, setVersion] = useState<UpstreamVersion | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState("");

  // Overview stats poll every 5s; the version check goes through a backend TTL cache
  // (hours), so it is fetched only on mount and on manual refresh.
  useEffect(() => {
    const load = () =>
      api
        .overview()
        .then(setData)
        .catch((e) => setError(String(e)));
    load();
    const timer = setInterval(load, 5000);
    return () => clearInterval(timer);
  }, []);

  useEffect(() => {
    api
      .upstreamVersion()
      .then(setVersion)
      .catch(() => {});
  }, []);

  const refreshVersion = async () => {
    setRefreshing(true);
    try {
      setVersion(await api.refreshUpstreamVersion());
    } catch (e) {
      toast.error(String(e));
    } finally {
      setRefreshing(false);
    }
  };

  if (error) return <p className="text-destructive">{error}</p>;
  if (!data) return <p className="text-muted-foreground">加载中…</p>;

  const upgradeAvailable =
    version?.latest != null && version.latest !== version.baked;

  const items = [
    { label: "账号总数", value: String(data.accounts), icon: Users },
    { label: "可用账号", value: String(data.available), icon: Activity },
    { label: "上游版本（伪装指纹）", value: `v${data.identity_version}`, icon: Server },
    { label: "监听地址", value: data.listen, icon: Globe },
  ];

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold tracking-tight">概览</h1>
        <Badge variant={data.available > 0 ? "success" : "destructive"}>
          <StatusDot tone={data.available > 0 ? "ok" : "bad"} />
          {data.available > 0 ? "运行中" : "无可用账号"}
        </Badge>
      </div>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {items.map(({ label, value, icon: Icon }) => (
          <Card key={label} className="transition-colors hover:border-white/[0.1]">
            <CardContent className="flex items-center gap-4 p-5">
              <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-lg bg-indigo-400/10 text-indigo-300 ring-1 ring-inset ring-indigo-400/20">
                <Icon className="h-[18px] w-[18px]" />
              </div>
              <div className="min-w-0">
                <div className="truncate text-xs text-muted-foreground">{label}</div>
                <div className="mt-0.5 truncate text-xl font-semibold tabular-nums tracking-tight">{value}</div>
              </div>
            </CardContent>
          </Card>
        ))}
      </div>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0">
          <CardTitle className="flex items-center gap-2 text-sm font-medium text-muted-foreground">
            <ArrowUpCircle className="h-4 w-4" /> 官方最新版本
          </CardTitle>
          <Button variant="ghost" size="sm" onClick={refreshVersion} disabled={refreshing}>
            <RefreshCw className={`h-4 w-4 ${refreshing ? "animate-spin" : ""}`} />
            刷新
          </Button>
        </CardHeader>
        <CardContent className="space-y-2">
          <div className="flex items-center gap-3">
            <span className="text-xl font-semibold tabular-nums">
              {version?.latest ? `v${version.latest}` : "未知"}
            </span>
            {upgradeAvailable && (
              <Badge variant="warning">
                <StatusDot tone="warn" />
                可升级（当前 v{version?.baked}）
              </Badge>
            )}
            {version && !upgradeAvailable && version.latest && (
              <Badge variant="secondary">
                <StatusDot tone="ok" />
                已是最新
              </Badge>
            )}
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2 text-sm font-medium text-muted-foreground">
            <GitCommit className="h-4 w-4" /> 上游代码基线
          </CardTitle>
        </CardHeader>
        <CardContent>
          <code className="rounded-md bg-white/[0.04] px-2 py-1 font-mono text-xs break-all text-indigo-200/90">
            {data.upstream_commit}
          </code>
          <p className="mt-2 text-xs text-muted-foreground">
            请求头、指令、认证行为均直接来自 openai/codex 官方源码（该 commit）。
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
