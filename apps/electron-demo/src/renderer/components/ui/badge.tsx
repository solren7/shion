import * as React from "react";
import { cva, type VariantProps } from "class-variance-authority";

import { cn } from "@/lib/utils";

const badgeVariants = cva(
  "inline-flex items-center justify-center gap-1 rounded-md border px-1.5 py-0.5 text-[11px] font-medium w-fit whitespace-nowrap shrink-0 [&>svg]:size-3",
  {
    variants: {
      variant: {
        default: "border-border bg-background text-foreground",
        pill: "rounded-full border-border bg-background text-foreground",
        ok: "border-[var(--mc-ok)] bg-background text-[var(--mc-ok)]",
        warn: "border-[var(--mc-warn)] bg-background text-[var(--mc-warn)]",
        danger: "border-[var(--mc-danger)] bg-background text-[var(--mc-danger)]",
      },
    },
    defaultVariants: {
      variant: "default",
    },
  },
);

function Badge({
  className,
  variant,
  ...props
}: React.ComponentProps<"span"> & VariantProps<typeof badgeVariants>) {
  return <span data-slot="badge" className={cn(badgeVariants({ variant }), className)} {...props} />;
}

export { Badge, badgeVariants };
