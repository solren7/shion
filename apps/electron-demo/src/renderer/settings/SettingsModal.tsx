import { useApp, useConnection } from "../app-context";
import { MemoriesTab, RunsTab, StatusTab, TasksTab } from "./panels";
import { Dialog, DialogContent, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Badge } from "@/components/ui/badge";
import { Switch } from "@/components/ui/switch";

const TABS: [string, string][] = [
  ["general", "常规"],
  ["tasks", "任务"],
  ["memories", "记忆"],
  ["runs", "运行"],
];

export function SettingsModal({ onClose }: { onClose: () => void }) {
  const { connected } = useConnection();

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open) onClose();
      }}
    >
      <DialogContent className="flex flex-col gap-0 p-0 sm:max-w-[620px] max-h-[82vh] overflow-hidden">
        <DialogHeader className="px-5 pt-4">
          <DialogTitle>设置</DialogTitle>
        </DialogHeader>

        <Tabs defaultValue="general" className="mt-3 flex flex-col min-h-0 gap-0">
          <TabsList
            variant="line"
            className="w-full h-auto justify-start rounded-none border-b border-border px-5"
          >
            {TABS.map(([v, label]) => (
              <TabsTrigger key={v} value={v} className="flex-none">
                {label}
              </TabsTrigger>
            ))}
          </TabsList>

          <div className="flex-1 min-h-0 overflow-y-auto px-5 py-4">
            <TabsContent value="general">
              <GeneralTab />
            </TabsContent>
            <TabsContent value="tasks">
              {connected ? <TasksTab /> : <Empty>未连接到 gateway。</Empty>}
            </TabsContent>
            <TabsContent value="memories">
              {connected ? <MemoriesTab /> : <Empty>未连接到 gateway。</Empty>}
            </TabsContent>
            <TabsContent value="runs">
              {connected ? <RunsTab /> : <Empty>未连接到 gateway。</Empty>}
            </TabsContent>
          </div>
        </Tabs>
      </DialogContent>
    </Dialog>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return <div className="flex items-center justify-center py-8 text-(--mc-fg-faint)">{children}</div>;
}

function GeneralTab() {
  const { connected } = useConnection();
  const { mode, setMode } = useApp();
  return (
    <div className="flex flex-col">
      <label className="flex items-center justify-between gap-4 py-3 border-b border-(--mc-border) cursor-pointer">
        <div>
          <div className="text-sm text-(--mc-fg)">信任模式（自动批准）</div>
          <div className="text-xs text-(--mc-fg-muted) mt-0.5">
            开启后副作用工具自动批准（等同 komo chat）；关闭则弹出审批。
          </div>
        </div>
        <Switch
          checked={mode === "trusted"}
          onCheckedChange={(v) => setMode(v ? "trusted" : "interactive")}
        />
      </label>

      <div className="flex items-center justify-between gap-4 py-3 border-b border-(--mc-border)">
        <div>
          <div className="text-sm text-(--mc-fg)">连接状态</div>
          <div className="text-xs text-(--mc-fg-muted) mt-0.5">komo gateway 的实时连接。</div>
        </div>
        <Badge variant={connected ? "ok" : "warn"} className="rounded-full px-2 py-1">
          {connected ? "已连接" : "未连接"}
        </Badge>
      </div>

      {connected && (
        <div className="pt-4">
          <StatusTab />
        </div>
      )}
    </div>
  );
}
