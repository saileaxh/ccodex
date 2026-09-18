import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center gap-1.5 rounded-full border px-2 py-0.5 text-[11px] font-medium leading-4 transition-colors",
  {
    variants: {
      variant: {
        default: "border-transparent bg-primary text-primary-foreground",
        secondary: "border-white/[0.06] bg-secondary text-secondary-foreground",
        destructive: "border-red-400/25 bg-red-400/10 text-red-300",
        outline: "border-white/[0.08] text-muted-foreground",
        success: "border-emerald-400/25 bg-emerald-400/10 text-emerald-300",
        warning: "border-amber-400/25 bg-amber-400/10 text-amber-300",
        info: "border-sky-400/25 bg-sky-400/10 text-sky-300",
      },
    },
    defaultVariants: { variant: "default" },
  }
);

export interface BadgeProps
  extends React.HTMLAttributes<HTMLDivElement>,
    VariantProps<typeof badgeVariants> {}

function Badge({ className, variant, ...props }: BadgeProps) {
  return <div className={cn(badgeVariants({ variant }), className)} {...props} />;
}

/** Colored status dot with a soft glow — pairs a Badge with its state color. */
function StatusDot({ tone, className }: { tone: "ok" | "warn" | "bad" | "idle"; className?: string }) {
  const color = {
    ok: "bg-emerald-400 shadow-[0_0_6px_1px_hsl(152_68%_50%/0.5)]",
    warn: "bg-amber-400 shadow-[0_0_6px_1px_hsl(43_90%_55%/0.5)]",
    bad: "bg-red-400 shadow-[0_0_6px_1px_hsl(0_72%_60%/0.5)]",
    idle: "bg-zinc-500",
  }[tone];
  return <span className={cn("inline-block h-1.5 w-1.5 rounded-full", color, className)} />;
}

export { Badge, badgeVariants, StatusDot };
