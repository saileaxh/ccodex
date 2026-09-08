import { useEffect, useState } from "react";
import { Activity, ArrowUpCircle, GitCommit, Globe, RefreshCw, Server, Users } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { api, type Overview, type UpstreamVersion } from "@/api";

function formatCheckedAt(unix: number): string {
  if (!unix) return "尚未检测";
  const d = new Date(unix * 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

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
      setError(String(e));
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
          {data.available > 0 ? "运行中" : "无可用账号"}
        </Badge>
      </div>
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-4">
        {items.map(({ label, value, icon: Icon }) => (
          <Card key={label}>
            <CardHeader className="flex flex-row items-center justify-between space-y-0 pb-2">
              <CardTitle className="text-sm font-medium text-muted-foreground">{label}</CardTitle>
              <Icon className="h-4 w-4 text-muted-foreground" />
            </CardHeader>
            <CardContent>
              <div className="text-xl font-semibold">{value}</div>
            </CardContent>
          </Card>
        ))}
      </div>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0">
          <CardTitle className="text-sm font-medium text-muted-foreground flex items-center gap-2">
            <ArrowUpCircle className="h-4 w-4" /> 官方最新版本
          </CardTitle>
          <Button variant="ghost" size="sm" onClick={refreshVersion} disabled={refreshing}>
            <RefreshCw className={`h-4 w-4 ${refreshing ? "animate-spin" : ""}`} />
            刷新
          </Button>
        </CardHeader>
        <CardContent className="space-y-2">
          <div className="flex items-center gap-3">
            <span className="text-xl font-semibold">
              {version?.latest ? `v${version.latest}` : "未知"}
            </span>
            {upgradeAvailable && <Badge variant="success">可升级（当前 v{version?.baked}）</Badge>}
            {version && !upgradeAvailable && version.latest && (
              <Badge variant="secondary">已是最新</Badge>
            )}
          </div>
          <p className="text-xs text-muted-foreground">
            来源 npm registry @openai/codex · 检测于 {formatCheckedAt(version?.checked_at_unix ?? 0)}
            {version?.source === "cache" && ` · 缓存（${Math.round(version.ttl_secs / 3600)} 小时内访问不重复查询）`}
            {version?.error && <span className="text-destructive"> · 上次查询失败: {version.error}</span>}
          </p>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-sm font-medium text-muted-foreground flex items-center gap-2">
            <GitCommit className="h-4 w-4" /> 上游代码基线
          </CardTitle>
        </CardHeader>
        <CardContent>
          <code className="text-sm break-all">{data.upstream_commit}</code>
          <p className="text-xs text-muted-foreground mt-2">
            请求头、指令、认证行为均直接来自 openai/codex 官方源码（该 commit）。
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
