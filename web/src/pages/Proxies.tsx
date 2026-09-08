import { useEffect, useState } from "react";
import { Globe, Loader2, Plus, Trash2, Zap } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api, type ProxiesResponse, type ProxyCheck } from "@/api";

function formatCheck(check: ProxyCheck | null): { text: string; ok: boolean | null } {
  if (!check) return { text: "未检测", ok: null };
  if (!check.ok) return { text: check.error ?? "失败", ok: false };
  return { text: `${check.ip ?? "?"} · ${check.latency_ms ?? "?"}ms`, ok: true };
}

export default function ProxiesPage() {
  const [data, setData] = useState<ProxiesResponse | null>(null);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [name, setName] = useState("");
  const [url, setUrl] = useState("");
  const [adding, setAdding] = useState(false);
  const [testing, setTesting] = useState<string | null>(null); // name, "default", or "direct"

  const load = () =>
    api
      .proxies()
      .then(setData)
      .catch((e) => setError(String(e)));

  useEffect(() => {
    load();
  }, []);

  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    setAdding(true);
    setNotice("");
    try {
      const r = await api.addProxy(name.trim(), url.trim());
      if (!r.ok) {
        setNotice(r.error ?? "添加失败");
        return;
      }
      setName("");
      setUrl("");
      setNotice("已添加");
      await load();
    } catch (err) {
      setNotice(String(err));
    } finally {
      setAdding(false);
    }
  };

  const remove = async (proxyName: string) => {
    setNotice("");
    try {
      const r = await api.deleteProxy(proxyName);
      setNotice(r.ok ? `已删除 ${proxyName}（绑定账号回落到默认出口）` : (r.error ?? "删除失败"));
      await load();
    } catch (err) {
      setNotice(String(err));
    }
  };

  const test = async (proxyName: string | null, tag: string) => {
    setTesting(tag);
    setNotice("");
    try {
      const r = await api.testProxy(proxyName);
      if (r.ok) {
        setNotice(`${tag} 出口: ${r.ip} (${r.latency_ms}ms)`);
      } else {
        setNotice(`${tag} 检测失败: ${r.error ?? "未知错误"}`);
      }
      await load();
    } catch (err) {
      setNotice(String(err));
    } finally {
      setTesting(null);
    }
  };

  const assignedTo = (proxyName: string): string[] =>
    Object.entries(data?.assignments ?? {})
      .filter(([, target]) => target === proxyName)
      .map(([acc]) => acc);

  const directAccounts = () =>
    Object.entries(data?.assignments ?? {})
      .filter(([, target]) => target === "direct")
      .map(([acc]) => acc);

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">代理池</h1>
      {notice && <p className="text-sm text-muted-foreground">{notice}</p>}
      {error && <p className="text-sm text-destructive">{error}</p>}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">上游出口</CardTitle>
          <CardDescription>
            账号绑定代理后，其数据面流量（Responses/Models）从对应出口出去；不绑定则用 config.toml
            的 upstream_proxy 默认值。凭证刷新由官方 AuthManager 在刷新时重建客户端，始终走默认出口。
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>名称</TableHead>
                <TableHead>地址</TableHead>
                <TableHead>绑定账号</TableHead>
                <TableHead>最近检测</TableHead>
                <TableHead className="w-[120px]">操作</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              <TableRow>
                <TableCell className="font-medium">
                  默认 <Badge variant="secondary">config.toml</Badge>
                </TableCell>
                <TableCell className="text-muted-foreground text-xs font-mono">
                  {data?.config_default ?? "（未配置，直连）"}
                </TableCell>
                <TableCell className="text-muted-foreground text-xs">未绑定的账号</TableCell>
                <TableCell className="text-xs">—</TableCell>
                <TableCell>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => test(null, "默认出口")}
                    disabled={testing !== null}
                  >
                    {testing === "默认出口" ? <Loader2 className="h-4 w-4 animate-spin" /> : <Zap className="h-4 w-4" />}
                    检测
                  </Button>
                </TableCell>
              </TableRow>
              {(data?.proxies ?? []).map((p) => {
                const c = formatCheck(p.check);
                return (
                  <TableRow key={p.name}>
                    <TableCell className="font-medium">{p.name}</TableCell>
                    <TableCell className="text-muted-foreground text-xs font-mono">{p.url}</TableCell>
                    <TableCell className="text-muted-foreground text-xs">
                      {assignedTo(p.name).join("、") || "—"}
                    </TableCell>
                    <TableCell className={`text-xs ${c.ok === false ? "text-destructive" : "text-muted-foreground"}`}>
                      {c.text}
                    </TableCell>
                    <TableCell>
                      <div className="flex gap-1">
                        <Button
                          variant="ghost"
                          size="sm"
                          onClick={() => test(p.name, p.name)}
                          disabled={testing !== null}
                        >
                          {testing === p.name ? <Loader2 className="h-4 w-4 animate-spin" /> : <Zap className="h-4 w-4" />}
                        </Button>
                        <Button variant="ghost" size="sm" onClick={() => remove(p.name)}>
                          <Trash2 className="h-4 w-4" />
                        </Button>
                      </div>
                    </TableCell>
                  </TableRow>
                );
              })}
              <TableRow>
                <TableCell className="font-medium">
                  直连 <Badge variant="secondary">direct</Badge>
                </TableCell>
                <TableCell className="text-muted-foreground text-xs">不使用任何代理</TableCell>
                <TableCell className="text-muted-foreground text-xs">
                  {directAccounts().join("、") || "—"}
                </TableCell>
                <TableCell className="text-xs">—</TableCell>
                <TableCell>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => test("direct", "直连")}
                    disabled={testing !== null}
                  >
                    {testing === "直连" ? <Loader2 className="h-4 w-4 animate-spin" /> : <Zap className="h-4 w-4" />}
                    检测
                  </Button>
                </TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card className="max-w-2xl">
        <CardHeader>
          <CardTitle className="text-base">添加代理</CardTitle>
          <CardDescription>支持 http / https / socks5 / socks5h，可内嵌认证 user:pass@host</CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={add} className="flex items-end gap-3">
            <div className="space-y-2 w-44">
              <Label htmlFor="pname">名称</Label>
              <Input id="pname" placeholder="例如 b-tokyo" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <div className="space-y-2 flex-1">
              <Label htmlFor="purl">地址</Label>
              <Input
                id="purl"
                placeholder="socks5h://127.0.0.1:17890"
                value={url}
                onChange={(e) => setUrl(e.target.value)}
                className="font-mono"
              />
            </div>
            <Button type="submit" disabled={adding || !name.trim() || !url.trim()}>
              {adding ? <Loader2 className="h-4 w-4 animate-spin" /> : <Plus className="h-4 w-4" />}
              添加
            </Button>
          </form>
        </CardContent>
      </Card>

      <p className="text-xs text-muted-foreground flex items-center gap-1.5">
        <Globe className="h-3.5 w-3.5" />
        在「账号」页为每个账号选择出口代理；改动会立即重建该账号的上游客户端。
      </p>
    </div>
  );
}
