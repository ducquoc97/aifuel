import type { HTMLAttributes } from "react";
import { cn } from "@/lib/utils";

export function Card({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
  return (
    <div
      className={cn(
        "bg-canvas border-[0.5px] border-line rounded-[11px] shadow-[0_1px_2px_rgba(29,29,31,0.05)]",
        className,
      )}
      {...props}
    />
  );
}
