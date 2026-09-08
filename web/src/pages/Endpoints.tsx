import { useState } from "react";
import { CheckCircle2, Copy } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <Button
      variant="ghost"
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

export default function EndpointsPage() {
  // Panel and API share an origin, so the address the viewer used IS the service address.
  const origin = window.location.origin;
  const wsOrigin = origin.replace(/^http/, "ws");

  const rows = [
    {
      name: "Responses（SSE 流）",
      method: "POST",
      url: `${origin}/v1/responses`,
      note: "主端点；Codex CLI 与任意 Responses 客户端走这里",
    },
    {
      name: "Responses（官方路径别名）",
      method: "POST",
      url: `${origin}/backend-api/codex/responses`,
      note: "与官方后端同形态；/responses 亦可",
    },
    {
      name: "Responses（WebSocket）",
      method: "WS",
      url: `${wsOrigin}/v1/responses`,
      note: "官方 WS 协议：发 response.create 帧，事件帧原样回发",
    },
    {
      name: "模型清单",
      method: "GET",
      url: `${origin}/v1/models`,
      note: "/backend-api/codex/models 亦可",
    },
    {
      name: "健康检查",
      method: "GET",
      url: `${origin}/health`,
      note: "无需鉴权；账号数/上游基线/身份版本",
    },
  ];

  const cliConfig = `model_provider = "ccodex"

[model_providers.ccodex]
name = "ccodex"
base_url = "${origin}"
wire_api = "responses"
env_key = "OPENAI_API_KEY"   # 值填「密钥」页生成的 sk- 访问密钥（不是面板登录密钥）`;

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <h1 className="text-2xl font-semibold tracking-tight">端点</h1>
        <Badge variant="secondary">{origin}</Badge>
      </div>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">API 服务地址</CardTitle>
          <CardDescription>
            当前访问来源即服务地址；除 /health 与面板外均需 <code>Authorization: Bearer &lt;api_key&gt;</code>
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>端点</TableHead>
                <TableHead className="w-[70px]">方法</TableHead>
                <TableHead>地址</TableHead>
                <TableHead>说明</TableHead>
                <TableHead className="w-[50px]" />
              </TableRow>
            </TableHeader>
            <TableBody>
              {rows.map((r) => (
                <TableRow key={r.name}>
                  <TableCell className="font-medium">{r.name}</TableCell>
                  <TableCell>
                    <Badge variant="outline">{r.method}</Badge>
                  </TableCell>
                  <TableCell className="font-mono text-xs text-muted-foreground">{r.url}</TableCell>
                  <TableCell className="text-xs text-muted-foreground">{r.note}</TableCell>
                  <TableCell>
                    <CopyButton text={r.url} />
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="flex flex-row items-center justify-between space-y-0">
          <div>
            <CardTitle className="text-base">Codex CLI 接入</CardTitle>
            <CardDescription>写入客户端的 ~/.codex/config.toml</CardDescription>
          </div>
          <CopyButton text={cliConfig} />
        </CardHeader>
        <CardContent>
          <pre className="rounded-md border bg-secondary/40 p-4 text-xs font-mono overflow-x-auto whitespace-pre">
            {cliConfig}
          </pre>
        </CardContent>
      </Card>
    </div>
  );
}
