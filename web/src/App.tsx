import { useEffect, useState } from "react";
import { LayoutDashboard, Users, UserPlus, Globe, Plug, KeyRound, Coins, LogOut } from "lucide-react";
import { Button } from "@/components/ui/button";
import { BrandMark } from "@/components/brand";
import { api, getApiKey, setApiKey } from "@/api";
import { cn } from "@/lib/utils";
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
  // undefined = still probing (auth-status has not returned yet)
  const [setupRequired, setSetupRequired] = useState<boolean | undefined>(undefined);
  const [authed, setAuthed] = useState(() => !!getApiKey());
  const [tab, setTab] = useState<Tab>("overview");
  // Account name prefilled into AddAccount when "relogin" is clicked on the Accounts page
  // (relogin = overwrite credentials under the same name).
  const [reloginName, setReloginName] = useState("");

  const startRelogin = (name: string) => {
    setReloginName(name);
    setTab("add-account");
  };

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
      <aside className="sticky top-0 flex h-screen w-60 shrink-0 flex-col border-r border-white/[0.06] bg-[hsl(224_34%_5%)]/70 backdrop-blur">
        <div className="flex items-center gap-3 px-5 pb-5 pt-6">
          <BrandMark />
          <div>
            <div className="text-[15px] font-semibold leading-none tracking-tight">ccodex</div>
            <div className="mt-1 text-[11px] text-muted-foreground">官方行为复刻中转</div>
          </div>
        </div>
        <nav className="flex-1 space-y-0.5 px-3 py-1">
          {TABS.map(({ key, label, icon: Icon }) => {
            const active = tab === key;
            return (
              <button
                key={key}
                onClick={() => {
                  setReloginName(""); // manual navigation clears the relogin prefill
                  setTab(key);
                }}
                className={cn(
                  "group relative flex w-full items-center gap-2.5 rounded-lg px-3 py-2 text-[13px] transition-colors",
                  active
                    ? "bg-indigo-400/10 font-medium text-indigo-200"
                    : "text-muted-foreground hover:bg-white/[0.04] hover:text-foreground"
                )}
              >
                {active && (
                  <span className="absolute left-0 top-1/2 h-4 w-[3px] -translate-y-1/2 rounded-full bg-indigo-400 shadow-[0_0_8px_0_hsl(239_84%_70%/0.9)]" />
                )}
                <Icon
                  className={cn(
                    "h-4 w-4 transition-colors",
                    active ? "text-indigo-300" : "text-muted-foreground/70 group-hover:text-foreground"
                  )}
                />
                {label}
              </button>
            );
          })}
        </nav>
        <div className="border-t border-white/[0.06] p-3">
          <Button variant="ghost" size="sm" className="w-full justify-start" onClick={logout}>
            <LogOut className="h-4 w-4" />
            退出登录
          </Button>
        </div>
      </aside>
      <main className="min-w-0 flex-1">
        <div className="mx-auto w-full max-w-6xl px-8 py-7">
          {tab === "overview" && <OverviewPage />}
          {tab === "accounts" && <AccountsPage onRelogin={startRelogin} />}
          {tab === "keys" && <KeysPage />}
          {tab === "cost" && <CostPage />}
          {tab === "proxies" && <ProxiesPage />}
          {tab === "endpoints" && <EndpointsPage />}
          {tab === "add-account" && <AddAccountPage initialName={reloginName} onDone={() => setTab("accounts")} />}
        </div>
      </main>
    </div>
  );
}
