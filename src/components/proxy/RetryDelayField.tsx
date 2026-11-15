import { useId } from "react";
import { useTranslation } from "react-i18next";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import {
  DEFAULT_RETRY_DELAY,
  retryDelayConfig,
  validRetryDelay,
  type RetryDelayConfig,
  type RetryDelayMode,
} from "@/lib/retryDelay";

export function RetryDelayField({
  value,
  onChange,
  disabled = false,
}: {
  value?: RetryDelayConfig;
  onChange: (config: RetryDelayConfig) => void;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const id = useId();
  const config = retryDelayConfig(value);
  const exponential = config.mode === "exponential_jitter";
  const initialValid =
    Number.isInteger(config.initialIntervalSeconds) &&
    config.initialIntervalSeconds >= 1 &&
    config.initialIntervalSeconds <= 86400;
  const maxValid =
    Number.isInteger(config.maxIntervalSeconds) &&
    config.maxIntervalSeconds >= config.initialIntervalSeconds &&
    config.maxIntervalSeconds <= 86400;

  return (
    <div className="space-y-2">
      <div className="space-y-1.5">
        <Label htmlFor={`${id}-mode`}>{t("retryDelay.mode")}</Label>
        <Select
          value={config.mode}
          disabled={disabled}
          onValueChange={(value) => {
            const mode = value as RetryDelayMode;
            // Hidden, partially edited backoff fields must not prevent saving fixed
            // mode. Valid values are preserved across every mode switch.
            onChange({
              ...(mode === "fixed" && !validRetryDelay(config)
                ? DEFAULT_RETRY_DELAY
                : config),
              mode,
            });
          }}
        >
          <SelectTrigger id={`${id}-mode`} aria-label={t("retryDelay.mode")}>
            <SelectValue />
          </SelectTrigger>
          <SelectContent>
            <SelectItem value="fixed">{t("retryDelay.fixed")}</SelectItem>
            <SelectItem value="exponential_jitter">
              {t("retryDelay.exponentialJitter")}
            </SelectItem>
          </SelectContent>
        </Select>
      </div>
      {exponential && (
        <>
          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1.5">
              <Label htmlFor={`${id}-initial`}>
                {t("retryDelay.initialIntervalSeconds")}
              </Label>
              <Input
                id={`${id}-initial`}
                type="number"
                min={1}
                max={86400}
                step={1}
                required
                disabled={disabled}
                value={
                  Number.isFinite(config.initialIntervalSeconds)
                    ? config.initialIntervalSeconds
                    : ""
                }
                aria-invalid={!initialValid}
                onChange={(event) =>
                  onChange({
                    ...config,
                    initialIntervalSeconds: event.target.valueAsNumber,
                  })
                }
              />
            </div>
            <div className="space-y-1.5">
              <Label htmlFor={`${id}-maximum`}>
                {t("retryDelay.maxIntervalSeconds")}
              </Label>
              <Input
                id={`${id}-maximum`}
                type="number"
                min={initialValid ? config.initialIntervalSeconds : 1}
                max={86400}
                step={1}
                required
                disabled={disabled}
                value={
                  Number.isFinite(config.maxIntervalSeconds)
                    ? config.maxIntervalSeconds
                    : ""
                }
                aria-invalid={!maxValid}
                onChange={(event) =>
                  onChange({
                    ...config,
                    maxIntervalSeconds: event.target.valueAsNumber,
                  })
                }
              />
            </div>
          </div>
          <p className="text-caption leading-relaxed text-fg-2">
            {t("retryDelay.exponentialHint")}
          </p>
          {!validRetryDelay(config) && (
            <p className="text-caption text-destructive">
              {t("retryDelay.invalid")}
            </p>
          )}
        </>
      )}
    </div>
  );
}
