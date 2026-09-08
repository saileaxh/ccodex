import { useEffect, useState } from "react";
import { CheckCircle2, Copy, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api, type KeyInfo } from "@/api";
import { fmtCost, fmtTokens, totalTokens } from "@/pages/Cost";

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="outline"
      size="icon"
      className="h-7 w-7"
      title="复制"
      onClick={async () => {
        try {
          await navigator.clipboard.writeText(text);
          setCopied(true);
          setTimeout(() => setCopied(false), 1500);
        } catch {
          /* 剪贴板不可用时用户手动复制 */
        }
      }}
    >
      {copied ? <CheckCircle2 className="h-3.5 w-3.5 text-emerald-400" /> : <Copy className="h-3.5 w-3.5" />}
    </Button>
  );
}

function formatTime(unix: number | null): string {
  if (!unix) return "—";
  const d = new Date(unix * 1000);
  const pad = (n: number) => `${n}`.padStart(2, "0");
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** 面板登录密钥（管理密钥）：与下方 sk 访问密钥完全独立，服务端只存 SHA-256 哈希。 */
function AdminKeyCard() {
  const [current, setCurrent] = useState("");
  const [next, setNext] = useState("");
  const [confirm, setConfirm] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (next.trim().length < 8) {
      setError("新密钥至少 8 个字符");
      return;
    }
    if (next.trim() !== confirm) {
      setError("两次输入的新密钥不一致");
      return;
    }
    setBusy(true);
    setNotice("");
    setError("");
    try {
      const r = await api.setAdminKey(current, next.trim());
      if (r.ok) {
        setNotice("登录密钥已更新，下次登录请使用新密钥");
        setCurrent("");
        setNext("");
        setConfirm("");
      } else {
        setError(r.error ?? "修改失败");
      }
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card>
      <CardHeader>
        <div className="flex items-center gap-2">
          <ShieldCheck className="h-4 w-4 text-muted-foreground" />
          <CardTitle className="text-base">面板登录密钥</CardTitle>
        </div>
        <CardDescription>
          仅用于登录本管理面板，与下方 sk 访问密钥相互独立（不能混用）；服务端只存 SHA-256 哈希
          （admin.json，0600），忘记后在服务器上删除该文件即可重新设置。
        </CardDescription>
      </CardHeader>
      <CardContent>
        <form onSubmit={submit} className="space-y-3 max-w-md">
          <div className="space-y-1.5">
            <Label htmlFor="cur-admin-key">当前密钥</Label>
            <Input
              id="cur-admin-key"
              type="password"
              value={current}
              onChange={(e) => setCurrent(e.target.value)}
              autoComplete="current-password"
            />
          </div>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <Label htmlFor="new-admin-key">新密钥</Label>
              <Input
                id="new-admin-key"
                type="password"
                value={next}
                onChange={(e) => setNext(e.target.value)}
                autoComplete="new-password"
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor="new-admin-key2">确认新密钥</Label>
              <Input
                id="new-admin-key2"
                type="password"
                value={confirm}
                onChange={(e) => setConfirm(e.target.value)}
                autoComplete="new-password"
              />
            </div>
          </div>
          {notice && <p className="text-sm text-emerald-400">{notice}</p>}
          {error && <p className="text-sm text-destructive">{error}</p>}
          <Button type="submit" variant="outline" disabled={busy || !current || !next.trim()}>
            修改登录密钥
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}

export default function KeysPage() {
  const [keys, setKeys] = useState<KeyInfo[] | null>(null);
  const [name, setName] = useState("");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");

  const load = () => {
    api
      .keys()
      .then((r) => setKeys(r.keys))
      .catch((e) => setError(String(e)));
  };
  useEffect(load, []);

  const add = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!name.trim()) return;
    setBusy(true);
    setNotice("");
    setError("");
    try {
      const r = await api.addKey(name.trim());
      if (r.ok) {
        setNotice(`已创建 ${r.name}，key 完整显示在下表（此页面随时可回来看/复制）`);
        setName("");
        load();
      } else {
        setError(r.error ?? "创建失败");
      }
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (k: KeyInfo) => {
    if (!window.confirm(`删除密钥「${k.name}」？使用它的客户端会立即失效。`)) return;
    setNotice("");
    setError("");
    try {
      const r = await api.deleteKey(k.name);
      if (r.ok) {
        setNotice(`已删除 ${k.name}`);
        load();
      } else {
        setError(r.error ?? "删除失败");
      }
    } catch (err) {
      setError(String(err));
    }
  };

  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">密钥</h1>

      <AdminKeyCard />

      {notice && <p className="text-sm text-emerald-400">{notice}</p>}
      {error && <p className="text-sm text-destructive">{error}</p>}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">访问密钥（sk）</CardTitle>
          <CardDescription>
            客户端用 <code>Authorization: Bearer &lt;sk&gt;</code> 访问 /v1
            接口；与面板登录密钥互不通用。密钥完整可见（本页即找回途径），只存于服务端
            keys.json（0600），不来自任何配置文件。用量/成本按密钥指纹统计。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <form onSubmit={add} className="flex items-center gap-3">
            <Input
              placeholder="新密钥名称（如 laptop、ci）"
              value={name}
              onChange={(e) => setName(e.target.value)}
              className="max-w-xs"
            />
            <Button type="submit" disabled={busy || !name.trim()}>
              <Plus className="h-4 w-4" />
              生成密钥
            </Button>
          </form>

          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>名称</TableHead>
                <TableHead>Key</TableHead>
                <TableHead>请求数</TableHead>
                <TableHead>Tokens</TableHead>
                <TableHead>等效成本</TableHead>
                <TableHead>创建时间</TableHead>
                <TableHead></TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(keys ?? []).map((k) => (
                <TableRow key={k.name}>
                  <TableCell className="font-medium">{k.name}</TableCell>
                  <TableCell>
                    <div className="flex items-center gap-2">
                      <code className="text-xs break-all">{k.key}</code>
                      <CopyButton text={k.key} />
                    </div>
                  </TableCell>
                  <TableCell className="text-xs">{k.usage?.requests ?? "—"}</TableCell>
                  <TableCell className="text-xs">
                    {k.usage ? fmtTokens(totalTokens(k.usage)) : "—"}
                  </TableCell>
                  <TableCell className="text-xs">{k.usage ? fmtCost(k.usage.cost_usd) : "—"}</TableCell>
                  <TableCell className="text-muted-foreground text-xs">{formatTime(k.created_at_unix)}</TableCell>
                  <TableCell className="text-right">
                    <Button variant="ghost" size="icon" className="h-7 w-7" title="删除" onClick={() => remove(k)}>
                      <Trash2 className="h-3.5 w-3.5 text-destructive" />
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
              {keys && keys.length === 0 && (
                <TableRow>
                  <TableCell colSpan={7} className="text-center text-muted-foreground py-8">
                    没有任何访问密钥——/v1 接口当前为开放模式（任何人可访问），请立即生成一把
                  </TableCell>
                </TableRow>
              )}
              {!keys && (
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
