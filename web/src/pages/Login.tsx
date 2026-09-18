import { useState } from "react";
import { KeyRound, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { api, ApiError, setApiKey } from "@/api";
import { BrandMark } from "@/components/brand";

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
    // Validate the key with a real request before entering the console.
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
    <div className="flex min-h-screen items-center justify-center p-4">
      <div className="w-full max-w-sm">
        <div className="mb-6 flex flex-col items-center gap-3">
          <BrandMark size="lg" />
          <div className="text-lg font-semibold tracking-tight">ccodex manager</div>
        </div>
        <Card className="border-white/[0.08] shadow-[0_24px_60px_-24px_rgba(0,0,0,0.8)]">
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base">
              {setup ? <ShieldCheck className="h-4 w-4 text-indigo-300" /> : <KeyRound className="h-4 w-4 text-indigo-300" />}
              {setup ? "初始化管理密钥" : "登录"}
            </CardTitle>
            {!setup && <CardDescription>输入面板管理密钥登录（不是 sk 访问密钥）</CardDescription>}
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
            </form>
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
