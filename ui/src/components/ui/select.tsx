import { forwardRef, type SelectHTMLAttributes } from "react";
import { cn } from "@/lib/utils";

// Native select under the dashboard's control styling - keeps browser
// a11y and works inside the narrow graph nodes.
const Select = forwardRef<HTMLSelectElement, SelectHTMLAttributes<HTMLSelectElement>>(
  ({ className, ...props }, ref) => (
    <select
      ref={ref}
      className={cn(
        "appearance-none text-[12.5px] tracking-[-0.004em] text-ink bg-pearl border-[0.5px] border-line rounded-lg pl-2.5 pr-7 py-[5px] disabled:opacity-55 bg-no-repeat bg-[right_8px_center] bg-[url('data:image/svg+xml;utf8,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 width=%2210%22 height=%226%22 viewBox=%220 0 10 6%22%3E%3Cpath d=%22M1 1l4 4 4-4%22 stroke=%22%236b6b70%22 fill=%22none%22 stroke-width=%221.5%22 stroke-linecap=%22round%22/%3E%3C/svg%3E')]",
        className,
      )}
      {...props}
    />
  ),
);
Select.displayName = "Select";

export { Select };
