import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Loader2, Monitor, ShieldCheck, ShieldOff } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Notice } from "@/components/ui/notice";
import { providersApi } from "@/lib/api";
import { toast } from "@/lib/toast";
import { extractErrorMessage } from "@/utils/errorUtils";

export const AGY_IDE_STATUS_KEY = ["agyIdeStatus"] as const;

/**
 * Antigravity 页顶部的接管面板。
 *
 * IDE 的语言服务器支持 --model_api_client_type=gemini 与 --override_model_name，
 * CC Switch 把 resources/bin/language_server 换成一个只在 ANTIGRAVITY_CCSWITCH_MODEL
 * 存在时才追加这两个参数的小包装脚本。这样官方启动方式不变，而用某张卡启动 IDE 时
 * Key、中转地址与模型都来自那张卡。
 */
export function AgyIdePanel() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();

  const { data: status } = useQuery({
    queryKey: AGY_IDE_STATUS_KEY,
    queryFn: () => providersApi.agyIdeStatus(),
    staleTime: 5_000,
    refetchInterval: 5_000,
    retry: false,
  });

  const refresh = () =>
    queryClient.invalidateQueries({ queryKey: AGY_IDE_STATUS_KEY });

  const install = useMutation({
    mutationFn: () => providersApi.installAgyIdeWrapper(),
    onSuccess: async () => {
      toast.success(t("agyIde.installSuccess"));
      await refresh();
    },
    onError: (error: unknown) => {
      toast.error(t("agyIde.installFailed"), {
        description: extractErrorMessage(error) || undefined,
      });
    },
  });

  const uninstall = useMutation({
    mutationFn: () => providersApi.uninstallAgyIdeWrapper(),
    onSuccess: async () => {
      toast.success(t("agyIde.uninstallSuccess"));
      await refresh();
    },
    onError: (error: unknown) => {
      toast.error(t("agyIde.uninstallFailed"), {
        description: extractErrorMessage(error) || undefined,
      });
    },
  });

  if (!status) return null;

  if (!status.supported) {
    return (
      <div className="shrink-0 px-6 pt-3">
        <Notice tone="neutral" title={t("agyIde.unsupported")} />
      </div>
    );
  }

  if (!status.installed) {
    return (
      <div className="shrink-0 px-6 pt-3">
        <Notice tone="neutral" title={t("agyIde.notDetectedTitle")}>
          {t("agyIde.notDetectedBody")}
        </Notice>
      </div>
    );
  }

  const enabled = status.wrapperInstalled;
  const busy = install.isPending || uninstall.isPending;

  return (
    <div className="shrink-0 px-6 pt-3">
      <Notice
        tone={enabled ? "direct" : "neutral"}
        icon={enabled ? ShieldCheck : Monitor}
        title={
          <span className="flex flex-wrap items-center gap-x-2 gap-y-1">
            {t("agyIde.title")}
            <span className="text-caption text-fg-2">
              {enabled ? t("agyIde.enabled") : t("agyIde.disabled")}
            </span>
            {status.version && (
              <span className="text-caption text-fg-2">
                {t("agyIde.version", { version: status.version })}
              </span>
            )}
            <span className="text-caption text-fg-2">
              {status.running ? t("agyIde.running") : t("agyIde.notRunning")}
            </span>
          </span>
        }
        actions={
          enabled ? (
            <Button
              variant="neutral"
              size="compact"
              disabled={busy}
              onClick={() => uninstall.mutate()}
            >
              {uninstall.isPending ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <ShieldOff className="h-3.5 w-3.5" strokeWidth={1.5} />
              )}
              {uninstall.isPending
                ? t("agyIde.removing")
                : t("agyIde.uninstall")}
            </Button>
          ) : (
            <Button
              variant="neutral"
              size="compact"
              disabled={busy}
              onClick={() => install.mutate()}
            >
              {install.isPending ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <ShieldCheck className="h-3.5 w-3.5" strokeWidth={1.5} />
              )}
              {install.isPending ? t("agyIde.installing") : t("agyIde.install")}
            </Button>
          )
        }
      >
        <span className="block">{t("agyIde.description")}</span>
        <span className="block">{t("agyIde.restartHint")}</span>
        <span className="mt-0.5 block truncate font-mono text-caption text-fg-3">
          {status.binary}
        </span>
      </Notice>
    </div>
  );
}
