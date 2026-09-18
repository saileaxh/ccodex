import { Waypoints } from "lucide-react";
import { cn } from "@/lib/utils";

export function BrandMark({ size = "md" }: { size?: "md" | "lg" }) {
  const box = size === "lg" ? "h-11 w-11 rounded-xl" : "h-8 w-8 rounded-lg";
  const icon = size === "lg" ? "h-5 w-5" : "h-4 w-4";
  return (
    <div
      className={cn(
        "flex shrink-0 items-center justify-center bg-gradient-to-br from-indigo-400 to-violet-500",
        "shadow-[0_0_18px_-2px_hsl(239_84%_64%/0.55)]",
        box
      )}
    >
      <Waypoints className={cn("text-white", icon)} strokeWidth={2.2} />
    </div>
  );
}
