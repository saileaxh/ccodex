import { useState } from "react";
import { KeyRound, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { api, ApiError, setApiKey } from "@/api";

export default function LoginPage({
  mode,
  onSuccess,
}: {
  mode: "login" | "setup";
  onSuccess: (key: string) => void;
}) {
  const [key, setKey] = useState("");
  const [confirm, setConfirm] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);

  const submitSetup = async () => {
    const next = key.trim();
    if (next.length < 8) {
      setError("登录密钥至少 8 个字符");
      return;
    }
    if (next !== confirm) {
      setError("两次输入不一致");
      return;
    }
    setLoading(true);
    setError("");
    try {
      const r = await api.setAdminKey("", next);
      if (!r.ok) {
        setError(r.error ?? "设置失败");
        return;
      }
      setApiKey(next);
      onSuccess(next);
    } catch (err) {
      setError(`连接失败: ${err}`);
    } finally {
      setLoading(false);
    }
  };

  const submitLogin = async () => {
    if (!key.trim()) {
      setError("请输入管理密钥");
      return;
    }
    setLoading(true);
    setError("");
    // 先试一次真实请求验证 key，通过后再进入
    setApiKey(key.trim());
    try {
      await api.overview();
      onSuccess(key.trim());
    } catch (err) {
      setApiKey("");
      setError(err instanceof ApiError && err.status === 401 ? "管理密钥无效" : `连接失败: ${err}`);
    } finally {
      setLoading(false);
    }
  };

  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (mode === "setup") void submitSetup();
    else void submitLogin();
  };

  const setup = mode === "setup";

  return (
    <div className="min-h-screen flex items-center justify-center p-4">
      <Card className="w-full max-w-sm">
        <CardHeader className="text-center">
          <div className="mx-auto mb-2 flex h-11 w-11 items-center justify-center rounded-lg bg-secondary">
            {setup ? <ShieldCheck className="h-5 w-5" /> : <KeyRound className="h-5 w-5" />}
          </div>
          <CardTitle className="text-xl">ccodex 控制台</CardTitle>
          <CardDescription>
            {setup
              ? "首次使用：设置面板管理密钥（仅用于登录本面板，与访问 /v1 接口的 sk 密钥相互独立）"
              : "输入面板管理密钥登录（不是 sk 访问密钥）"}
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="space-y-4">
            <div className="space-y-2">
              <Label htmlFor="adminkey">{setup ? "设置管理密钥" : "管理密钥"}</Label>
              <Input
                id="adminkey"
                type="password"
                placeholder={setup ? "至少 8 个字符" : "面板管理密钥"}
                value={key}
                onChange={(e) => setKey(e.target.value)}
                autoFocus
              />
            </div>
            {setup && (
              <div className="space-y-2">
                <Label htmlFor="adminkey2">确认管理密钥</Label>
                <Input
                  id="adminkey2"
                  type="password"
                  placeholder="再输入一次"
                  value={confirm}
                  onChange={(e) => setConfirm(e.target.value)}
                />
              </div>
            )}
            {error && <p className="text-sm text-destructive">{error}</p>}
            <Button type="submit" className="w-full" disabled={loading}>
              {loading ? (setup ? "设置中…" : "验证中…") : setup ? "设置并进入" : "登录"}
            </Button>
            {setup && (
              <p className="text-xs text-muted-foreground">
                密钥以 SHA-256 哈希存储在服务端 admin.json（0600）；忘记后在服务器上删除该文件即可重新设置。
              </p>
            )}
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
