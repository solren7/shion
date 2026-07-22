import * as React from "react";
import { useRender } from "@base-ui-components/react/use-render";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

const buttonVariants = cva(
  "inline-flex items-center justify-center gap-2 whitespace-nowrap rounded-md text-sm font-medium transition-all disabled:pointer-events-none disabled:opacity-50 [&_svg]:pointer-events-none [&_svg:not([class*='size-'])]:size-4 shrink-0 outline-none focus-visible:ring-[3px] focus-visible:ring-ring/50 cursor-pointer",
  {
    variants: {
      variant: {
        // Signature mineclaw CTA: the accent gradient.
        gradient:
          "text-white [background:var(--mc-accent-grad)] shadow-(--mc-shadow-glow) hover:opacity-90",
        default: "bg-primary text-primary-foreground shadow-xs hover:bg-primary/90",
        secondary:
          "bg-secondary text-secondary-foreground border border-border hover:border-ring",
        outline: "border border-input bg-transparent hover:bg-accent hover:text-accent-foreground",
        ghost: "hover:bg-accent hover:text-accent-foreground",
        destructive:
          "bg-secondary text-destructive border border-border hover:border-destructive",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        default: "h-9 px-4 py-2",
        sm: "h-8 rounded-md px-3 text-[13px]",
        lg: "h-11 rounded-[14px] px-5",
        icon: "size-9",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  },
);

type ButtonProps = React.ComponentProps<"button"> &
  VariantProps<typeof buttonVariants> & {
    /** Base UI render prop: swap the underlying element (e.g. `render={<a />}`). */
    render?: useRender.RenderProp;
  };

function Button({ className, variant, size, render, ...props }: ButtonProps) {
  return useRender({
    render: render ?? <button />,
    props: { "data-slot": "button", className: cn(buttonVariants({ variant, size, className })), ...props },
  });
}

export { Button, buttonVariants };
