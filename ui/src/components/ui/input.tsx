import { forwardRef, type InputHTMLAttributes } from "react";
import { cn } from "@/lib/utils";

const Input = forwardRef<HTMLInputElement, InputHTMLAttributes<HTMLInputElement>>(
  ({ className, ...props }, ref) => (
    <input
      ref={ref}
      className={cn(
        "appearance-none text-[12.5px] tracking-[-0.004em] text-ink bg-pearl border-[0.5px] border-line rounded-lg px-3 py-[7px] placeholder:text-ink-ter focus:border-accent focus:outline-none focus:ring-[3px] focus:ring-accent/15 disabled:opacity-55",
        className,
      )}
      {...props}
    />
  ),
);
Input.displayName = "Input";

export { Input };
