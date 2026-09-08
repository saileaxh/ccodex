import { useEffect, useState } from "react";
import { Plus, RotateCcw, Save } from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "@/components/ui/table";
import { api, type AccountInfo, type KeyInfo, type PriceEntry, type UsageTotals } from "@/api";

export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return `${n}`;
}

export function fmtCost(usd: number): string {
  if (usd === 0) return "$0";
  if (usd < 0.01) return `$${usd.toFixed(4)}`;
  return `$${usd.toFixed(2)}`;
}

export function totalTokens(u: UsageTotals): number {
  return u.input_tokens + u.output_tokens;
}

function UsageSummary() {
  const [keys, setKeys] = useState<KeyInfo[] | null>(null);
  const [accounts, setAccounts] = useState<AccountInfo[] | null>(null);

  useEffect(() => {
    api.keys().then((r) => setKeys(r.keys)).catch(() => {});
    api.accounts().then((r) => setAccounts(r.accounts)).catch(() => {});
  }, []);

  const used = (keys ?? []).filter((k) => k.usage && k.usage.requests > 0);
  const grand = used.reduce(
    (acc, k) => ({
      requests: acc.requests + (k.usage?.requests ?? 0),
      tokens: acc.tokens + (k.usage ? totalTokens(k.usage) : 0),
      cost: acc.cost + (k.usage?.cost_usd ?? 0),
    }),
    { requests: 0, tokens: 0, cost: 0 }
  );

  return (
    <>
      <Card>
        <CardHeader>
          <CardTitle className="text-base">密钥用量</CardTitle>
          <CardDescription>
            按下游 Bearer 密钥归因（指纹标识，服务端不重复存储密钥本体）；成本为等效 API 定价估算
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>密钥</TableHead>
                <TableHead>请求数</TableHead>
                <TableHead>Tokens（入+出）</TableHead>
                <TableHead>其中缓存命中</TableHead>
                <TableHead>等效成本</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {used.map((k) => (
                <TableRow key={k.name}>
                  <TableCell className="font-medium">{k.name}</TableCell>
                  <TableCell>{k.usage?.requests}</TableCell>
                  <TableCell>{k.usage ? fmtTokens(totalTokens(k.usage)) : "—"}</TableCell>
                  <TableCell className="text-muted-foreground">
                    {k.usage ? fmtTokens(k.usage.cached_input_tokens) : "—"}
                  </TableCell>
                  <TableCell>{k.usage ? fmtCost(k.usage.cost_usd) : "—"}</TableCell>
                </TableRow>
              ))}
              <TableRow className="font-medium">
                <TableCell>合计</TableCell>
                <TableCell>{grand.requests}</TableCell>
                <TableCell>{fmtTokens(grand.tokens)}</TableCell>
                <TableCell />
                <TableCell>{fmtCost(grand.cost)}</TableCell>
              </TableRow>
            </TableBody>
          </Table>
        </CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">账号周期用量</CardTitle>
          <CardDescription>
            周期按上游主限流窗口对齐（跟随配额快照的 resets_at 滚动），另列累计用量
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>账号</TableHead>
                <TableHead>本周期 Tokens</TableHead>
                <TableHead>本周期成本</TableHead>
                <TableHead>累计 Tokens</TableHead>
                <TableHead>累计成本</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {(accounts ?? []).map((a) => (
                <TableRow key={a.name}>
                  <TableCell className="font-medium">{a.email ?? a.name}</TableCell>
                  <TableCell>
                    {a.usage?.period ? fmtTokens(totalTokens(a.usage.period.totals)) : "—"}
                  </TableCell>
                  <TableCell>
                    {a.usage?.period ? fmtCost(a.usage.period.totals.cost_usd) : "—"}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {a.usage ? fmtTokens(totalTokens(a.usage.total)) : "—"}
                  </TableCell>
                  <TableCell className="text-muted-foreground">
                    {a.usage ? fmtCost(a.usage.total.cost_usd) : "—"}
                  </TableCell>
                </TableRow>
              ))}
              {accounts && accounts.length === 0 && (
                <TableRow>
                  <TableCell colSpan={5} className="text-center text-muted-foreground py-8">
                    暂无账号
                  </TableCell>
                </TableRow>
              )}
            </TableBody>
          </Table>
        </CardContent>
      </Card>
    </>
  );
}

function PricingTable() {
  const [entries, setEntries] = useState<PriceEntry[] | null>(null);
  const [drafts, setDrafts] = useState<Record<string, { input: string; cached: string; output: string }>>({});
  const [newModel, setNewModel] = useState("");
  const [notice, setNotice] = useState("");
  const [error, setError] = useState("");

  const load = () => {
    api
      .pricing()
      .then((r) => {
        setEntries(r.entries);
        const d: Record<string, { input: string; cached: string; output: string }> = {};
        for (const e of r.entries) {
          d[e.model] = {
            input: `${e.input_per_m}`,
            cached: `${e.cached_input_per_m}`,
            output: `${e.output_per_m}`,
          };
        }
        setDrafts(d);
      })
      .catch((e) => setError(String(e)));
  };
  useEffect(load, []);

  const save = async (model: string) => {
    const d = drafts[model];
    if (!d) return;
    const [input, cached, output] = [Number(d.input), Number(d.cached), Number(d.output)];
    if ([input, cached, output].some((v) => !Number.isFinite(v) || v < 0)) {
      setError(`${model}: 价格需为非负数字`);
      return;
    }
    setNotice("");
    setError("");
    try {
      const r = await api.putPricing(model, input, cached, output);
      if (r.ok) {
        setNotice(`${model} 定价已保存`);
        load();
      } else {
        setError(r.error ?? "保存失败");
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const reset = async (model: string) => {
    setNotice("");
    setError("");
    try {
      const r = await api.deletePricing(model);
      if (r.ok) {
        setNotice(`${model} 已恢复默认定价`);
        load();
      } else {
        setError(r.error ?? "恢复失败");
      }
    } catch (e) {
      setError(String(e));
    }
  };

  const addModel = async (e: React.FormEvent) => {
    e.preventDefault();
    const m = newModel.trim();
    if (!m) return;
    setNewModel("");
    const r = await api.putPricing(m, 0, 0, 0);
    if (r.ok) load();
    else setError(r.error ?? "添加失败");
  };

  const setDraft = (model: string, field: "input" | "cached" | "output", value: string) => {
    setDrafts((prev) => ({ ...prev, [model]: { ...prev[model], [field]: value } }));
  };

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">模型定价表</CardTitle>
        <CardDescription>
          单位：美元 / 1M tokens，默认取自官方 API 定价页（标准短上下文费率）；"*" 为未知模型的兜底价。
          订阅账号不按 token 计费，此表仅用于等效成本估算；自定义覆盖持久化在服务端 pricing.json。
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {notice && <p className="text-sm text-emerald-400">{notice}</p>}
        {error && <p className="text-sm text-destructive">{error}</p>}
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>模型</TableHead>
              <TableHead>输入</TableHead>
              <TableHead>缓存输入</TableHead>
              <TableHead>输出</TableHead>
              <TableHead>来源</TableHead>
              <TableHead className="w-[90px]"></TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {(entries ?? []).map((e) => (
              <TableRow key={e.model}>
                <TableCell className="font-mono text-xs">{e.model}</TableCell>
                {(["input", "cached", "output"] as const).map((field) => (
                  <TableCell key={field}>
                    <Input
                      className="h-7 w-24 text-xs"
                      value={drafts[e.model]?.[field] ?? ""}
                      onChange={(ev) => setDraft(e.model, field, ev.target.value)}
                    />
                  </TableCell>
                ))}
                <TableCell>
                  <Badge variant={e.override ? "default" : "outline"}>
                    {e.override ? "自定义" : "官方默认"}
                  </Badge>
                </TableCell>
                <TableCell className="text-right">
                  <div className="flex justify-end gap-1">
                    <Button variant="ghost" size="icon" className="h-7 w-7" title="保存" onClick={() => save(e.model)}>
                      <Save className="h-3.5 w-3.5" />
                    </Button>
                    {e.override && (
                      <Button variant="ghost" size="icon" className="h-7 w-7" title="恢复默认" onClick={() => reset(e.model)}>
                        <RotateCcw className="h-3.5 w-3.5" />
                      </Button>
                    )}
                  </div>
                </TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
        <form onSubmit={addModel} className="flex items-center gap-3">
          <Input
            placeholder="添加模型名（精确匹配，如 gpt-x.y-zzz）"
            value={newModel}
            onChange={(e) => setNewModel(e.target.value)}
            className="max-w-xs"
          />
          <Button type="submit" variant="outline" disabled={!newModel.trim()}>
            <Plus className="h-4 w-4" />
            添加
          </Button>
        </form>
      </CardContent>
    </Card>
  );
}

export default function CostPage() {
  return (
    <div className="space-y-6">
      <h1 className="text-2xl font-semibold tracking-tight">成本</h1>
      <UsageSummary />
      <PricingTable />
    </div>
  );
}
