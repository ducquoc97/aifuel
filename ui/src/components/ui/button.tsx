import { cva, type VariantProps } from "class-variance-authority";
import { forwardRef, type ButtonHTMLAttributes } from "react";
import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-1.5 whitespace-nowrap rounded-full text-[12.5px] font-semibold tracking-[-0.004em] transition-colors cursor-pointer active:scale-[0.97] disabled:opacity-50 disabled:cursor-default disabled:pointer-events-none [&_svg]:size-3.5 [&_svg]:shrink-0",
  {
    variants: {
      variant: {
        // envi pill: accent text on a hairline fill.
        default: "text-accent bg-fill-4 hover:bg-ink/[0.06]",
        primary: "text-white bg-accent hover:bg-accent-hover",
        ghost: "text-ink-48 bg-transparent hover:bg-ink/[0.06] hover:text-ink",
        danger: "text-err bg-err-tint hover:bg-err/15",
      },
      size: {
        default: "px-3.5 py-[7px]",
        sm: "px-3 py-1 text-xs",
        icon: "size-7 rounded-lg p-0",
      },
    },
    defaultVariants: { variant: "default", size: "default" },
  },
);

export interface ButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {}

const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, type = "button", ...props }, ref) => (
    <button
      ref={ref}
      type={type}
      className={cn(buttonVariants({ variant, size }), className)}
      {...props}
    />
  ),
);
Button.displayName = "Button";

export { Button, buttonVariants };
