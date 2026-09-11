import { useEffect, useRef, useState } from "react";
import { CheckCircle2, Copy, ExternalLink, Loader2, XCircle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { api } from "@/api";

type OAuthStep =
  | { kind: "input" }
  | { kind: "waiting"; sessionId: string; url: string }
  | { kind: "done" }
  | { kind: "error"; message: string };

type DeviceStep =
  | { kind: "idle" }
  | { kind: "waiting"; sessionId: string; url: string; code: string }
  | { kind: "done" }
  | { kind: "error"; message: string };

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="outline"
      size="icon"
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
      {copied ? <CheckCircle2 className="h-4 w-4 text-emerald-400" /> : <Copy className="h-4 w-4" />}
    </Button>
  );
}

function OAuthCard({ initialName, onDone }: { initialName: string; onDone: () => void }) {
  const [name, setName] = useState(initialName);
  const [step, setStep] = useState<OAuthStep>({ kind: "input" });
  const [pasted, setPasted] = useState("");
  const [submitting, setSubmitting] = useState(false);

  const start = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmed = name.trim();
    if (!trimmed) return;
    try {
      const r = await api.startOAuthLogin(trimmed);
      if (r.error || !r.session_id) {
        setStep({ kind: "error", message: r.error ?? "发起登录失败" });
        return;
      }
      setStep({ kind: "waiting", sessionId: r.session_id, url: r.authorize_url ?? "" });
    } catch (err) {
      setStep({ kind: "error", message: String(err) });
    }
  };

  const complete = async () => {
    if (step.kind !== "waiting" || !pasted.trim()) return;
    setSubmitting(true);
    try {
      const r = await api.completeOAuthLogin(step.sessionId, pasted.trim());
      if (r.ok) {
        setStep({ kind: "done" });
      } else {
        setStep({ kind: "error", message: r.error ?? "登录失败" });
      }
    } catch (err) {
      setStep({ kind: "error", message: String(err) });
    } finally {
      setSubmitting(false);
    }
  };

  const reset = () => {
    setStep({ kind: "input" });
    setName("");
    setPasted("");
  };

  return (
    <Card className="max-w-2xl">
      <CardHeader>
        <CardTitle className="text-base">浏览器授权登录（推荐）</CardTitle>
        <CardDescription>
          与官方 <code>codex login</code> 完全相同的 OAuth 流程。授权后浏览器会跳转到打不开的
          localhost:1455 页面——把地址栏完整 URL 粘贴回来即可完成。token 交换与凭证落盘均为官方代码。
        </CardDescription>
      </CardHeader>
      <CardContent>
        {step.kind === "input" && (
          <form onSubmit={start} className="space-y-4">
            <div className="space-y-2">
              <Label htmlFor="oname">账号名</Label>
              <Input
                id="oname"
                placeholder="例如 work、personal"
                value={name}
                onChange={(e) => setName(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">作为账号目录名，不能包含 / \ . 字符</p>
            </div>
            <Button type="submit" disabled={!name.trim()}>
              发起浏览器授权
            </Button>
          </form>
        )}

        {step.kind === "waiting" && (
          <div className="space-y-4">
            <div className="space-y-2">
              <Label>1. 打开授权链接并完成登录</Label>
              <div className="flex gap-2">
                <Input readOnly value={step.url} onFocus={(e) => e.target.select()} />
                <CopyButton text={step.url} />
                <Button
                  variant="outline"
                  size="icon"
                  onClick={() => window.open(step.url, "_blank")}
                  title="打开链接"
                >
                  <ExternalLink className="h-4 w-4" />
                </Button>
              </div>
            </div>
            <div className="space-y-2">
              <Label>2. 粘贴最终跳转的完整 URL（localhost:1455 那个打不开的页面）</Label>
              <div className="flex gap-2">
                <Input
                  placeholder="http://localhost:1455/auth/callback?code=...&state=..."
                  value={pasted}
                  onChange={(e) => setPasted(e.target.value)}
                  className="font-mono text-xs"
                />
                <Button onClick={complete} disabled={submitting || !pasted.trim()}>
                  {submitting ? <Loader2 className="h-4 w-4 animate-spin" /> : null}
                  完成登录
                </Button>
              </div>
            </div>
            <Button variant="ghost" size="sm" onClick={reset}>
              取消
            </Button>
          </div>
        )}

        {step.kind === "done" && (
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-emerald-400">
              <CheckCircle2 className="h-5 w-5" />
              登录成功，账号池已自动热重载
            </div>
            <div className="flex gap-2">
              <Button onClick={onDone}>查看账号</Button>
              <Button variant="outline" onClick={reset}>
                再添加一个
              </Button>
            </div>
          </div>
        )}

        {step.kind === "error" && (
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-destructive">
              <XCircle className="h-5 w-5" />
              {step.message}
            </div>
            <Button variant="outline" onClick={reset}>
              重试
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

function DeviceCard({ initialName, onDone }: { initialName: string; onDone: () => void }) {
  const [name, setName] = useState(initialName);
  const [step, setStep] = useState<DeviceStep>({ kind: "idle" });
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const stopPolling = () => {
    if (pollRef.current) {
      clearInterval(pollRef.current);
      pollRef.current = null;
    }
  };
  useEffect(() => stopPolling, []);

  const start = async (e: React.FormEvent) => {
    e.preventDefault();
    const trimmed = name.trim();
    if (!trimmed) return;
    try {
      const r = await api.startDeviceLogin(trimmed);
      if (r.error || !r.session_id) {
        setStep({ kind: "error", message: r.error ?? "发起登录失败" });
        return;
      }
      setStep({
        kind: "waiting",
        sessionId: r.session_id,
        url: r.verification_url ?? "",
        code: r.user_code ?? "",
      });
      pollRef.current = setInterval(async () => {
        try {
          const s = await api.deviceLoginStatus(r.session_id!);
          if (s.status === "done") {
            stopPolling();
            setStep({ kind: "done" });
          } else if (s.status === "error") {
            stopPolling();
            setStep({ kind: "error", message: s.error ?? "登录失败" });
          }
        } catch {
          /* 轮询瞬时失败忽略 */
        }
      }, 3000);
    } catch (err) {
      setStep({ kind: "error", message: String(err) });
    }
  };

  const reset = () => {
    stopPolling();
    setStep({ kind: "idle" });
    setName("");
  };

  return (
    <Card className="max-w-2xl">
      <CardHeader>
        <CardTitle className="text-base text-muted-foreground">设备码登录（备用）</CardTitle>
        <CardDescription>
          在任意设备输入验证码完成登录。注意：设备码通道比浏览器授权更容易触发上游风控，能用上面的方式就别用这里。
        </CardDescription>
      </CardHeader>
      <CardContent>
        {step.kind === "idle" && (
          <form onSubmit={start} className="flex items-end gap-3">
            <div className="space-y-2 flex-1">
              <Label htmlFor="dname">账号名</Label>
              <Input id="dname" value={name} onChange={(e) => setName(e.target.value)} />
            </div>
            <Button type="submit" variant="outline" disabled={!name.trim()}>
              发起设备码登录
            </Button>
          </form>
        )}

        {step.kind === "waiting" && (
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              等待授权完成（最长 15 分钟）…
            </div>
            <div className="space-y-2">
              <Label>1. 打开验证链接</Label>
              <div className="flex gap-2">
                <Input readOnly value={step.url} onFocus={(e) => e.target.select()} />
                <Button
                  variant="outline"
                  size="icon"
                  onClick={() => window.open(step.url, "_blank")}
                  title="打开链接"
                >
                  <ExternalLink className="h-4 w-4" />
                </Button>
              </div>
            </div>
            <div className="space-y-2">
              <Label>2. 输入验证码</Label>
              <div className="flex gap-2">
                <div className="flex-1 rounded-md border bg-secondary/50 px-3 py-2 text-center font-mono text-xl tracking-[0.2em]">
                  {step.code}
                </div>
                <CopyButton text={step.code} />
              </div>
            </div>
            <Button variant="ghost" size="sm" onClick={reset}>
              取消
            </Button>
          </div>
        )}

        {step.kind === "done" && (
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-emerald-400">
              <CheckCircle2 className="h-5 w-5" />
              登录成功，账号池已自动热重载
            </div>
            <Button onClick={onDone}>查看账号</Button>
          </div>
        )}

        {step.kind === "error" && (
          <div className="space-y-4">
            <div className="flex items-center gap-2 text-destructive">
              <XCircle className="h-5 w-5" />
              {step.message}
            </div>
            <Button variant="outline" onClick={reset}>
              重试
            </Button>
          </div>
        )}
      </CardContent>
    </Card>
  );
}

export default function AddAccountPage({ initialName = "", onDone }: { initialName?: string; onDone: () => void }) {
  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">{initialName ? "重新登录" : "添加账号"}</h1>
      {initialName && (
        <p className="text-sm text-muted-foreground">
          将为账号 <span className="font-medium text-foreground">{initialName}</span> 重新写入凭证（同名覆盖，完成后账号池自动热重载，失效状态自动清除）。
        </p>
      )}
      <OAuthCard initialName={initialName} onDone={onDone} />
      <DeviceCard initialName={initialName} onDone={onDone} />
    </div>
  );
}
