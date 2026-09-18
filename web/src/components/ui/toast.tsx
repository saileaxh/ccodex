import { useEffect, useState } from "react";
import { CheckCircle2, Info, XCircle } from "lucide-react";
import { cn } from "@/lib/utils";

type ToastKind = "success" | "error" | "info";
interface ToastItem {
  id: number;
  kind: ToastKind;
  text: string;
}

// Tiny module-level bus: pages call toast.*() without prop drilling, Toaster subscribes.
let nextId = 1;
type Listener = (t: ToastItem) => void;
const listeners = new Set<Listener>();

function emit(kind: ToastKind, text: string) {
  const t = { id: nextId++, kind, text };
  listeners.forEach((l) => l(t));
}

export const toast = {
  success: (text: string) => emit("success", text),
  error: (text: string) => emit("error", text),
  info: (text: string) => emit("info", text),
};

const ICONS: Record<ToastKind, { Icon: typeof Info; cls: string }> = {
  success: { Icon: CheckCircle2, cls: "text-emerald-400" },
  error: { Icon: XCircle, cls: "text-red-400" },
  info: { Icon: Info, cls: "text-indigo-300" },
};

export function Toaster() {
  const [items, setItems] = useState<ToastItem[]>([]);

  useEffect(() => {
    const push = (t: ToastItem) => {
      setItems((prev) => [...prev.slice(-3), t]); // keep at most 4 visible
      setTimeout(() => setItems((prev) => prev.filter((x) => x.id !== t.id)), 4500);
    };
    listeners.add(push);
    return () => {
      listeners.delete(push);
    };
  }, []);

  return (
    <div className="pointer-events-none fixed right-4 top-4 z-50 flex w-80 flex-col gap-2">
      {items.map((t) => {
        const { Icon, cls } = ICONS[t.kind];
        return (
          <div
            key={t.id}
            className={cn(
              "pointer-events-auto flex items-start gap-2.5 rounded-lg border border-white/[0.08]",
              "bg-card/95 px-3.5 py-3 text-[13px] leading-snug shadow-[0_8px_24px_-8px_rgba(0,0,0,0.6)] backdrop-blur",
              "animate-in fade-in slide-in-from-right-2 duration-200"
            )}
          >
            <Icon className={cn("mt-px h-4 w-4 shrink-0", cls)} />
            <span className="break-all">{t.text}</span>
          </div>
        );
      })}
    </div>
  );
}
