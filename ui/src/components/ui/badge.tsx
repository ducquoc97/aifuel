import { cva, type VariantProps } from "class-variance-authority";
import type { HTMLAttributes } from "react";
import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-block text-[11px] font-semibold tracking-[-0.002em] leading-none px-2 py-1 rounded-full",
  {
    variants: {
      variant: {
        default: "text-ink-48 bg-ink/[0.06]",
        ok: "text-ok bg-ok-tint",
        warn: "text-warn bg-warn-tint",
        err: "text-err bg-err-tint",
        info: "text-info bg-info-tint",
        accent: "text-accent bg-accent-tint",
      },
    },
    defaultVariants: { variant: "default" },
  },
);

export interface BadgeProps
  extends HTMLAttributes<HTMLSpanElement>,
    VariantProps<typeof badgeVariants> {}

export function Badge({ className, variant, ...props }: BadgeProps) {
  return <span className={cn(badgeVariants({ variant }), className)} {...props} />;
}
