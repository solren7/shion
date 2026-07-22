import * as React from "react";
import { Switch as BaseSwitch } from "@base-ui-components/react/switch";

import { cn } from "@/lib/utils";

function Switch({ className, ...props }: React.ComponentProps<typeof BaseSwitch.Root>) {
  return (
    <BaseSwitch.Root
      data-slot="switch"
      className={cn(
        "peer inline-flex h-5 w-9 shrink-0 items-center rounded-full border border-transparent shadow-xs transition-colors outline-none cursor-pointer",
        "focus-visible:ring-[3px] focus-visible:ring-ring/50 disabled:cursor-not-allowed disabled:opacity-50",
        "data-[unchecked]:bg-[var(--mc-border-strong)] data-[checked]:[background:var(--mc-accent-grad)]",
        className,
      )}
      {...props}
    >
      <BaseSwitch.Thumb
        className={cn(
          "pointer-events-none block size-4 rounded-full bg-white ring-0 transition-transform",
          "data-[unchecked]:translate-x-0.5 data-[checked]:translate-x-[18px]",
        )}
      />
    </BaseSwitch.Root>
  );
}

export { Switch };
