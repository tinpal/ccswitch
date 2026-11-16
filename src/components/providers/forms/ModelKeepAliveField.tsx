import { useId, useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronDown, ChevronRight } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Collapsible, CollapsibleContent } from "@/components/ui/collapsible";
import {
  modelKeepAliveConfig,
  validModelKeepAlive,
} from "@/lib/modelKeepAlive";
import { RetryDelayField } from "@/components/proxy/RetryDelayField";
import type { ModelKeepAliveConfig } from "@/types";
import type { ModelKeepAliveStatus } from "@/types/proxy";

interface Props {
  model: string;
  value?: ModelKeepAliveConfig;
  onChange: (value: ModelKeepAliveConfig) => void;
  status?: ModelKeepAliveStatus;
  statusError?: boolean;
}

const fields = [
  ["intervalSeconds", 1, 86400],
  ["retryIntervalSeconds", 0, 86400],
  ["timeoutSeconds", 1, 3600],
  ["recoveryConcurrency", 1, 64],
] as const;

export function ModelKeepAliveField({
  model,
  value,
  onChange,
  status,
  statusError,
}: Props) {
  const { t } = useTranslation();
  const id = useId();
  const [expanded, setExpanded] = useState(value?.enabled === true);
  const config = modelKeepAliveConfig(value);
  const changed =
    status &&
    JSON.stringify(config) !==
      JSON.stringify(modelKeepAliveConfig(status.config));
  const time = (raw: string) => new Date(raw).toLocaleTimeString();
  const stateLabel = statusError
    ? t("modelKeepAlive.statusUnavailable")
    : changed || (!status && config.enabled)
      ? t("modelKeepAlive.pendingSave")
      : status?.reason && status.phase !== "disabled"
        ? t(`modelKeepAlive.reasons.${status.reason}`, {
            defaultValue: status.reason,
          })
        : t(`modelKeepAlive.phases.${status?.phase ?? "disabled"}`);

  return (
    <Collapsible
      open={expanded}
      onOpenChange={setExpanded}
      className="col-span-full rounded-md border border-border bg-muted/20 px-3 py-2"
    >
      <div className="flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-2">
          <Switch
            id={`${id}-enabled`}
            size="sm"
            checked={config.enabled}
            disabled={!model.trim() && !config.enabled}
            aria-label={`${model || t("modelKeepAlive.model")} ${t("modelKeepAlive.label")}`}
            onCheckedChange={(enabled) => {
              onChange({ ...config, enabled });
              if (enabled) setExpanded(true);
            }}
          />
          <Label htmlFor={`${id}-enabled`} className="text-xs">
            {t("modelKeepAlive.label")}
          </Label>
        </div>
        <span role="status" className="text-xs text-fg-2">
          {stateLabel}
          {status &&
            !changed &&
            status.phase === "requesting" &&
            ` · ${status.activeRequests}/${status.mode === "keepalive" ? 1 : config.recoveryConcurrency}`}
        </span>
        <Button
          type="button"
          variant="ghost"
          size="sm"
          className="ms-auto h-6 gap-1 px-2 text-xs"
          aria-expanded={expanded}
          aria-controls={`${id}-settings`}
          onClick={() => setExpanded(!expanded)}
        >
          {expanded ? (
            <ChevronDown className="h-3 w-3" />
          ) : (
            <ChevronRight className="h-3 w-3" />
          )}
          {t("modelKeepAlive.settings")}
        </Button>
      </div>
      <CollapsibleContent id={`${id}-settings`} className="space-y-3 pt-3">
        <div className="space-y-1.5">
          <div className="flex items-center gap-2">
            <Switch
              id={`${id}-minimal-prompt`}
              size="sm"
              checked={config.useMinimalPrompt === true}
              aria-describedby={`${id}-prompt-hint`}
              onCheckedChange={(useMinimalPrompt) =>
                onChange({ ...config, useMinimalPrompt })
              }
            />
            <Label htmlFor={`${id}-minimal-prompt`} className="text-xs">
              {t("modelKeepAlive.useMinimalPrompt")}
            </Label>
          </div>
          <p
            id={`${id}-prompt-hint`}
            className="text-xs leading-relaxed text-fg-2"
          >
            {t(
              config.useMinimalPrompt
                ? "modelKeepAlive.minimalPromptHint"
                : "modelKeepAlive.fullPromptHint",
            )}
          </p>
        </div>
        <RetryDelayField
          value={config.retryDelay}
          onChange={(retryDelay) =>
            onChange({
              ...config,
              retryDelay,
              retryIntervalSeconds:
                Number.isInteger(config.retryIntervalSeconds) &&
                config.retryIntervalSeconds >= 0 &&
                config.retryIntervalSeconds <= 86400
                  ? config.retryIntervalSeconds
                  : 5,
            })
          }
        />
        <div className="grid grid-cols-2 gap-3 xl:grid-cols-4">
          {fields
            .filter(
              ([field]) =>
                field !== "retryIntervalSeconds" ||
                config.retryDelay?.mode !== "exponential_jitter",
            )
            .map(([field, min, max]) => (
              <div key={field} className="space-y-1">
                <Label htmlFor={`${id}-${field}`} className="text-xs text-fg-2">
                  {t(`modelKeepAlive.${field}`)}
                </Label>
                <Input
                  id={`${id}-${field}`}
                  type="number"
                  inputMode="numeric"
                  min={min}
                  max={max}
                  step={1}
                  required
                  value={Number.isFinite(config[field]) ? config[field] : ""}
                  aria-invalid={
                    !Number.isInteger(config[field]) ||
                    config[field] < min ||
                    config[field] > max
                  }
                  onChange={(event) =>
                    onChange({ ...config, [field]: event.target.valueAsNumber })
                  }
                  className="h-8"
                />
                {field === "retryIntervalSeconds" && (
                  <p className="text-xs leading-relaxed text-fg-2">
                    {t("modelKeepAlive.retryIntervalHint")}
                  </p>
                )}
              </div>
            ))}
        </div>
        {!validModelKeepAlive(config) && (
          <p className="text-xs text-destructive">
            {t("modelKeepAlive.invalid")}
          </p>
        )}
        <p className="text-xs leading-relaxed text-fg-2">
          {t("modelKeepAlive.help")}
        </p>
        <p className="text-xs leading-relaxed text-fg-2">
          {t("modelKeepAlive.applyHint")}
        </p>
        {status && (
          <div className="space-y-1 text-xs text-fg-2">
            {status.clientUserAgent && (
              <p className="break-words">
                {t("modelKeepAlive.client", { client: status.clientUserAgent })}
              </p>
            )}
            <p>
              {t("modelKeepAlive.counts", {
                total: status.totalRequests,
                success: status.successCount,
                failure: status.failureCount,
              })}
            </p>
            {status.lastSuccessAt && (
              <p>
                {t("modelKeepAlive.lastSuccess", {
                  time: time(status.lastSuccessAt),
                })}
              </p>
            )}
            {status.nextRequestAt && !changed && (
              <p>
                {t("modelKeepAlive.nextRequest", {
                  time: time(status.nextRequestAt),
                })}
              </p>
            )}
            {status.lastError && (
              <p className="break-words">
                {t("modelKeepAlive.lastError", {
                  error: t(`modelKeepAlive.errors.${status.lastError}`, {
                    defaultValue: status.lastError,
                  }),
                })}
                {status.lastHttpStatus
                  ? ` (HTTP ${status.lastHttpStatus})`
                  : ""}
              </p>
            )}
          </div>
        )}
      </CollapsibleContent>
    </Collapsible>
  );
}
