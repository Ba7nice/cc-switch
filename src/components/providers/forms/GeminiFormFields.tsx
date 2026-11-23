import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";
import { FormLabel } from "@/components/ui/form";
import { Download, Info, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { HelpTip } from "@/components/ui/help-tip";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { toast } from "@/lib/toast";
import EndpointSpeedTest from "./EndpointSpeedTest";
import { ApiKeySection, EndpointField, ModelInputWithFetch } from "./shared";
import {
  fetchModelsForConfig,
  getAgyModels,
  showFetchModelsError,
  type FetchedModel,
} from "@/lib/api/model-fetch";
import type { ProviderCategory } from "@/types";

interface EndpointCandidate {
  url: string;
}

interface GeminiFormFieldsProps {
  appId?: "gemini" | "agy";
  providerId?: string;
  // API Key
  shouldShowApiKey: boolean;
  apiKey: string;
  onApiKeyChange: (key: string) => void;
  category?: ProviderCategory;
  shouldShowApiKeyLink: boolean;
  websiteUrl: string;
  isPartner?: boolean;
  partnerPromotionKey?: string;

  // Base URL
  shouldShowSpeedTest: boolean;
  baseUrl: string;
  onBaseUrlChange: (url: string) => void;
  isEndpointModalOpen: boolean;
  onEndpointModalToggle: (open: boolean) => void;
  onCustomEndpointsChange: (endpoints: string[]) => void;
  autoSelect: boolean;
  onAutoSelectChange: (checked: boolean) => void;

  // Model
  shouldShowModelField: boolean;
  model: string;
  onModelChange: (value: string) => void;

  // Antigravity reasoning depth（空字符串表示用 IDE 默认值）
  thinking: string;
  onThinkingChange: (value: string) => void;

  // Speed Test Endpoints
  speedTestEndpoints: EndpointCandidate[];
}

export function GeminiFormFields({
  appId = "gemini",
  providerId,
  shouldShowApiKey,
  apiKey,
  onApiKeyChange,
  category,
  shouldShowApiKeyLink,
  websiteUrl,
  isPartner,
  partnerPromotionKey,
  shouldShowSpeedTest,
  baseUrl,
  onBaseUrlChange,
  isEndpointModalOpen,
  onEndpointModalToggle,
  onCustomEndpointsChange,
  autoSelect,
  onAutoSelectChange,
  shouldShowModelField,
  model,
  onModelChange,
  thinking,
  onThinkingChange,
  speedTestEndpoints,
}: GeminiFormFieldsProps) {
  const { t } = useTranslation();

  const [fetchedModels, setFetchedModels] = useState<FetchedModel[]>([]);
  const [isFetchingModels, setIsFetchingModels] = useState(false);

  // 检测是否为 Google 官方（使用 OAuth）
  const isGoogleOfficial =
    partnerPromotionKey?.toLowerCase() === "google-official";

  const handleFetchModels = useCallback(() => {
    // 官方 Antigravity 卡只认本机 agy 清单里的显示名，接口模型 ID 对不上；
    // 第三方卡直接把供应商的 Gemini 原生 /v1beta/models 列表拿过来即可。
    const agy = appId === "agy";
    const agyOfficial = agy && isGoogleOfficial;
    const modelBaseUrl = baseUrl.trim();
    if (!agyOfficial && (!modelBaseUrl || !apiKey)) {
      showFetchModelsError(null, t, {
        hasApiKey: !!apiKey,
        hasBaseUrl: !!modelBaseUrl,
      });
      return;
    }
    setIsFetchingModels(true);
    (agyOfficial
      ? getAgyModels({ apiKey, baseUrl: modelBaseUrl })
      : // Gemini CLI 与 Antigravity 都只说 Gemini 原生协议（v1beta + x-goog-api-key），
        // 能被它们用的网关也必然认这一套；按默认的 Bearer + /v1/models 取会 401 或回 0 条。
        fetchModelsForConfig(
          modelBaseUrl,
          apiKey,
          false,
          undefined,
          undefined,
          {
            apiFormat: "google-generative-ai",
          },
        )
    )
      .then((models) => {
        setFetchedModels(models);
        if (models.length === 0) {
          toast.info(t("providerForm.fetchModelsEmpty"));
        } else {
          toast.success(
            t("providerForm.fetchModelsSuccess", { count: models.length }),
          );
        }
      })
      .catch((err) => {
        console.warn("[ModelFetch] Failed:", err);
        showFetchModelsError(err, t);
      })
      .finally(() => setIsFetchingModels(false));
  }, [appId, baseUrl, apiKey, isGoogleOfficial, t]);

  return (
    <>
      {/* Google OAuth 提示 */}
      {isGoogleOfficial && (
        <div className="rounded-lg border border-border-strong bg-subtle p-4">
          <div className="flex gap-3">
            <Info className="h-5 w-5 flex-shrink-0 text-fg-1" />
            <div className="space-y-1">
              <p className="text-sm font-medium text-fg-1">
                {t("provider.form.gemini.oauthTitle", {
                  defaultValue: "OAuth 认证模式",
                })}
              </p>
              <p className="text-sm text-fg-1">
                {t("provider.form.gemini.oauthHint", {
                  defaultValue:
                    "Google 官方使用 OAuth 个人认证，无需填写 API Key。首次使用时会自动打开浏览器进行登录。",
                })}
              </p>
            </div>
          </div>
        </div>
      )}

      {/* API Key 输入框 */}
      {shouldShowApiKey && !isGoogleOfficial && (
        <ApiKeySection
          value={apiKey}
          onChange={onApiKeyChange}
          category={category}
          required
          shouldShowLink={shouldShowApiKeyLink}
          websiteUrl={websiteUrl}
          isPartner={isPartner}
          partnerPromotionKey={partnerPromotionKey}
        />
      )}

      {/* Base URL 输入框（统一使用与 Codex 相同的样式与交互） */}
      {shouldShowSpeedTest && (
        <EndpointField
          id="baseUrl"
          required={appId !== "agy"}
          label={t("providerForm.apiEndpoint", { defaultValue: "API 端点" })}
          value={baseUrl}
          onChange={onBaseUrlChange}
          placeholder={t("providerForm.apiEndpointPlaceholder", {
            defaultValue: "https://your-api-endpoint.com/",
          })}
          help={
            appId === "agy" ? t("provider.form.agy.endpointHelp") : undefined
          }
          onManageClick={() => onEndpointModalToggle(true)}
        />
      )}

      {/* Model 输入框 */}
      {shouldShowModelField && (
        <div className="space-y-2">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-0.5">
              <FormLabel htmlFor="gemini-model">
                {t("provider.form.gemini.model", { defaultValue: "模型" })}
              </FormLabel>
              {appId === "agy" && (
                <HelpTip
                  title={t("provider.form.gemini.model", {
                    defaultValue: "模型",
                  })}
                >
                  {t("provider.form.agy.modelHelp")}
                </HelpTip>
              )}
            </div>
            <Button
              type="button"
              variant="outline"
              size="sm"
              onClick={handleFetchModels}
              disabled={isFetchingModels}
              className="h-7 gap-1"
            >
              {isFetchingModels ? (
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
              ) : (
                <Download className="h-3.5 w-3.5" />
              )}
              {t("providerForm.fetchModels")}
            </Button>
          </div>
          <ModelInputWithFetch
            id="gemini-model"
            value={model}
            onChange={onModelChange}
            placeholder={
              appId === "agy" && !apiKey
                ? "Gemini 3.8 Flash (High)"
                : "gemini-3.6-flash"
            }
            fetchedModels={fetchedModels}
            isLoading={isFetchingModels}
          />
        </div>
      )}

      {/* Antigravity 推理深度：由 CC Switch 启动 IDE 时的本地代理改写 thinkingConfig */}
      {appId === "agy" && !isGoogleOfficial && (
        <div className="space-y-2">
          <div className="flex items-center gap-0.5">
            <FormLabel htmlFor="agy-thinking">
              {t("provider.form.agy.thinking", {
                defaultValue: "推理深度",
              })}
            </FormLabel>
            <HelpTip
              title={t("provider.form.agy.thinking", {
                defaultValue: "推理深度",
              })}
            >
              {t("provider.form.agy.thinkingHelp")}
            </HelpTip>
          </div>
          <Select
            value={thinking || "auto"}
            onValueChange={(next) => {
              if (!next) return;
              onThinkingChange(next === "auto" ? "" : next);
            }}
          >
            <SelectTrigger id="agy-thinking" className="w-full">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              <SelectItem value="auto">
                {t("provider.form.agy.thinkingAuto", {
                  defaultValue: "自动（不干预）",
                })}
              </SelectItem>
              <SelectItem value="low">
                {t("provider.form.agy.thinkingLow", {
                  defaultValue: "低（2048 tokens）",
                })}
              </SelectItem>
              <SelectItem value="medium">
                {t("provider.form.agy.thinkingMedium", {
                  defaultValue: "中（8192 tokens）",
                })}
              </SelectItem>
              <SelectItem value="high">
                {t("provider.form.agy.thinkingHigh", {
                  defaultValue: "高（24576 tokens）",
                })}
              </SelectItem>
            </SelectContent>
          </Select>
          <p className="text-caption text-fg-2">
            {t("provider.form.agy.thinkingHint", {
              defaultValue:
                "选择低/中/高后，CC Switch 会在启动 IDE 时开启本地代理。请保持 CC Switch 运行，改动后重启 IDE 生效。",
            })}
          </p>
        </div>
      )}

      {/* 端点测速弹窗 */}
      {shouldShowSpeedTest && isEndpointModalOpen && (
        <EndpointSpeedTest
          appId={appId}
          providerId={providerId}
          value={baseUrl}
          onChange={onBaseUrlChange}
          initialEndpoints={speedTestEndpoints}
          visible={isEndpointModalOpen}
          onClose={() => onEndpointModalToggle(false)}
          autoSelect={autoSelect}
          onAutoSelectChange={onAutoSelectChange}
          onCustomEndpointsChange={onCustomEndpointsChange}
        />
      )}
    </>
  );
}
