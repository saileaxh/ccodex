import { useEffect, useState } from "react";
import { LayoutDashboard, Users, UserPlus, Globe, Plug, KeyRound, Coins, LogOut } from "lucide-react";
import { Button } from "@/components/ui/button";
import { api, getApiKey, setApiKey } from "@/api";
import LoginPage from "@/pages/Login";
import OverviewPage from "@/pages/Overview";
import AccountsPage from "@/pages/Accounts";
import AddAccountPage from "@/pages/AddAccount";
import ProxiesPage from "@/pages/Proxies";
import EndpointsPage from "@/pages/Endpoints";
import KeysPage from "@/pages/Keys";
import CostPage from "@/pages/Cost";

type Tab = "overview" | "accounts" | "keys" | "cost" | "proxies" | "endpoints" | "add-account";

const TABS: { key: Tab; label: string; icon: typeof LayoutDashboard }[] = [
  { key: "overview", label: "概览", icon: LayoutDashboard },
  { key: "accounts", label: "账号", icon: Users },
  { key: "keys", label: "密钥", icon: KeyRound },
  { key: "cost", label: "成本", icon: Coins },
  { key: "proxies", label: "代理", icon: Globe },
  { key: "endpoints", label: "端点", icon: Plug },
  { key: "add-account", label: "添加账号", icon: UserPlus },
];

export default function App() {
  // undefined = 还在探测（auth-status 未返回）
  const [setupRequired, setSetupRequired] = useState<boolean | undefined>(undefined);
  const [authed, setAuthed] = useState(() => !!getApiKey());
  const [tab, setTab] = useState<Tab>("overview");

  useEffect(() => {
    api
      .authStatus()
      .then((s) => setSetupRequired(s.setup_required))
      .catch(() => setSetupRequired(false));
  }, []);

  if (setupRequired === undefined) {
    return <div className="min-h-screen" />;
  }

  if (setupRequired) {
    return (
      <LoginPage
        mode="setup"
        onSuccess={(key) => {
          setApiKey(key);
          setSetupRequired(false);
          setAuthed(true);
        }}
      />
    );
  }

  if (!authed) {
    return (
      <LoginPage
        mode="login"
        onSuccess={(key) => {
          setApiKey(key);
          setAuthed(true);
        }}
      />
    );
  }

  const logout = () => {
    setApiKey("");
    setAuthed(false);
  };

  return (
    <div className="flex min-h-screen">
      <aside className="w-56 shrink-0 border-r bg-card/50 flex flex-col">
        <div className="px-5 py-5 border-b">
          <div className="text-lg font-semibold tracking-tight">ccodex</div>
          <div className="text-xs text-muted-foreground mt-0.5">官方行为复刻中转</div>
        </div>
        <nav className="flex-1 p-3 space-y-1">
          {TABS.map(({ key, label, icon: Icon }) => (
            <button
              key={key}
              onClick={() => setTab(key)}
              className={`w-full flex items-center gap-2.5 rounded-md px-3 py-2 text-sm transition-colors ${
                tab === key
                  ? "bg-accent text-accent-foreground font-medium"
                  : "text-muted-foreground hover:bg-accent/50 hover:text-foreground"
              }`}
            >
              <Icon className="h-4 w-4" />
              {label}
            </button>
          ))}
        </nav>
        <div className="p-3 border-t">
          <Button variant="ghost" size="sm" className="w-full justify-start" onClick={logout}>
            <LogOut className="h-4 w-4" />
            退出登录
          </Button>
        </div>
      </aside>
      <main className="flex-1 p-6 max-w-5xl">
        {tab === "overview" && <OverviewPage />}
        {tab === "accounts" && <AccountsPage />}
        {tab === "keys" && <KeysPage />}
        {tab === "cost" && <CostPage />}
        {tab === "proxies" && <ProxiesPage />}
        {tab === "endpoints" && <EndpointsPage />}
        {tab === "add-account" && <AddAccountPage onDone={() => setTab("accounts")} />}
      </main>
    </div>
  );
}
