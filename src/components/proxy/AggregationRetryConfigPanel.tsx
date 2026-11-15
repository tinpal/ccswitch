import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { Loader2, Save } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { Alert, AlertDescription } from "@/components/ui/alert";
import {
  useAggregationRetryConfig,
  useUpdateAggregationRetryConfig,
} from "@/lib/query/proxy";
import { toast } from "@/lib/toast";
import { extractErrorMessage } from "@/utils/errorUtils";
import { RetryDelayField } from "./RetryDelayField";
import { retryDelayConfig, validRetryDelay } from "@/lib/retryDelay";

export function AggregationRetryConfigPanel({
  appType = "codex",
}: {
  appType?: "codex" | "claude";
}) {
  const { t } = useTranslation();
  const { data: config, isLoading, error } = useAggregationRetryConfig(appType);
  const update = useUpdateAggregationRetryConfig(appType);
  const [enabled, setEnabled] = useState(true);
  const [parallelEnabled, setParallelEnabled] = useState(false);
  const [parallelRequests, setParallelRequests] = useState("2");
  const [retryIntervalSeconds, setRetryIntervalSeconds] = useState("0");
  const [retryDelay, setRetryDelay] = useState(retryDelayConfig);
  const [unlimited, setUnlimited] = useState(false);
  const [maxRetries, setMaxRetries] = useState("20");
  const [maxElapsedSeconds, setMaxElapsedSeconds] = useState("600");

  useEffect(() => {
    if (config) {
      setEnabled(config.enabled);
      setParallelEnabled(config.parallelEnabled ?? false);
      setParallelRequests(String(config.parallelRequests ?? 2));
      setRetryIntervalSeconds(String(config.retryIntervalSeconds ?? 0));
      setRetryDelay(retryDelayConfig(config.retryDelay));
      setUnlimited(config.maxRetries === null);
      setMaxRetries(String(config.maxRetries ?? 20));
      setMaxElapsedSeconds(String(config.maxElapsedSeconds));
    }
  }, [config]);

  const save = () => {
    if (!validRetryDelay(retryDelay)) {
      toast.error(t("retryDelay.invalid"));
      return;
    }
    const retries = Number(maxRetries.trim());
    const elapsed = Number(maxElapsedSeconds.trim());
    const parallel = Number(parallelRequests.trim());
    const interval = Number(retryIntervalSeconds.trim());
    if (
      !/^\d+$/.test(retryIntervalSeconds.trim()) ||
      !Number.isInteger(interval) ||
      interval < 0 ||
      interval > 86_400
    ) {
      toast.error(t("proxy.aggregationRetry.invalidRetryInterval"));
      return;
    }
    if (
      !/^\d+$/.test(parallelRequests.trim()) ||
      !Number.isInteger(parallel) ||
      parallel < 1 ||
      parallel > 64
    ) {
      toast.error(t("proxy.aggregationRetry.invalidParallelRequests"));
      return;
    }
    if (
      (!unlimited &&
        (!/^\d+$/.test(maxRetries.trim()) ||
          !Number.isInteger(retries) ||
          retries < 0 ||
          retries > 4_294_967_295)) ||
      !/^\d+$/.test(maxElapsedSeconds.trim()) ||
      !Number.isInteger(elapsed) ||
      elapsed < 1 ||
      elapsed > 86_400
    ) {
      toast.error(t("proxy.aggregationRetry.invalidConfig"));
      return;
    }
    update.mutate({
      enabled,
      parallelEnabled,
      parallelRequests: parallel,
      retryIntervalSeconds: interval,
      retryDelay,
      maxRetries: unlimited ? null : retries,
      maxElapsedSeconds: elapsed,
    });
  };

  if (isLoading) {
    return (
      <Loader2
        className="h-4 w-4 animate-spin text-fg-2"
        aria-label={t("common.loading")}
      />
    );
  }

  const disabled = !config || update.isPending;
  return (
    <section
      className="space-y-3 border-b border-border pb-3"
      aria-label={t("proxy.aggregationRetry.title")}
    >
      <h3 className="text-body font-semibold">
        {t("proxy.aggregationRetry.title")}
      </h3>
      <p className="text-caption text-fg-2">
        {t(
          appType === "claude"
            ? "proxy.aggregationRetry.descriptionClaude"
            : "proxy.aggregationRetry.description",
        )}
      </p>
      {error && (
        <Alert variant="destructive">
          <AlertDescription>{extractErrorMessage(error)}</AlertDescription>
        </Alert>
      )}
      <div className="flex items-center justify-between gap-3">
        <Label htmlFor="aggregation-retry-enabled">
          {t("proxy.aggregationRetry.enabled")}
        </Label>
        <Switch
          id="aggregation-retry-enabled"
          checked={enabled}
          onCheckedChange={setEnabled}
          disabled={disabled}
        />
      </div>
      <div className="flex items-center justify-between gap-3">
        <Label htmlFor="aggregation-retry-parallel">
          {t("proxy.aggregationRetry.parallelEnabled")}
        </Label>
        <Switch
          id="aggregation-retry-parallel"
          checked={parallelEnabled}
          onCheckedChange={setParallelEnabled}
          disabled={disabled || !enabled}
        />
      </div>
      {parallelEnabled && (
        <div className="space-y-1.5">
          <Label htmlFor="aggregation-retry-parallel-count">
            {t("proxy.aggregationRetry.parallelRequests")}
          </Label>
          <Input
            id="aggregation-retry-parallel-count"
            type="number"
            min={1}
            max={64}
            step={1}
            value={parallelRequests}
            onChange={(event) => setParallelRequests(event.target.value)}
            disabled={disabled || !enabled}
          />
          <p className="text-caption text-fg-2">
            {t("proxy.aggregationRetry.parallelHint")}
          </p>
        </div>
      )}
      <RetryDelayField
        value={retryDelay}
        onChange={(next) => {
          setRetryDelay(next);
          if (
            next.mode === "exponential_jitter" &&
            (!/^\d+$/.test(retryIntervalSeconds.trim()) ||
              Number(retryIntervalSeconds) > 86400)
          )
            setRetryIntervalSeconds("0");
        }}
        disabled={disabled || !enabled}
      />
      {retryDelay.mode === "fixed" && (
        <div className="space-y-1.5">
          <Label htmlFor="aggregation-retry-interval">
            {t("proxy.aggregationRetry.retryIntervalSeconds")}
          </Label>
          <Input
            id="aggregation-retry-interval"
            type="number"
            min={0}
            max={86_400}
            step={1}
            value={retryIntervalSeconds}
            onChange={(event) => setRetryIntervalSeconds(event.target.value)}
            disabled={disabled || !enabled}
          />
          <p className="text-caption text-fg-2">
            {t("proxy.aggregationRetry.retryIntervalHint")}
          </p>
        </div>
      )}
      <div className="flex items-center justify-between gap-3">
        <Label htmlFor="aggregation-retry-unlimited">
          {t("proxy.aggregationRetry.unlimited")}
        </Label>
        <Switch
          id="aggregation-retry-unlimited"
          checked={unlimited}
          onCheckedChange={setUnlimited}
          disabled={disabled || !enabled}
        />
      </div>
      <p className="text-caption text-fg-2">
        {t("proxy.aggregationRetry.unlimitedHint")}
      </p>
      <div className="space-y-1.5">
        <Label htmlFor="aggregation-retry-count">
          {t("proxy.aggregationRetry.maxRetries")}
        </Label>
        <Input
          id="aggregation-retry-count"
          type="number"
          min={0}
          max={4_294_967_295}
          step={1}
          value={maxRetries}
          onChange={(event) => setMaxRetries(event.target.value)}
          disabled={disabled || !enabled || unlimited}
        />
        <p className="text-caption text-fg-2">
          {t("proxy.aggregationRetry.maxRetriesHint")}
        </p>
      </div>
      <div className="space-y-1.5">
        <Label htmlFor="aggregation-retry-duration">
          {t("proxy.aggregationRetry.maxElapsedSeconds")}
        </Label>
        <Input
          id="aggregation-retry-duration"
          type="number"
          min={1}
          max={86_400}
          step={1}
          value={maxElapsedSeconds}
          onChange={(event) => setMaxElapsedSeconds(event.target.value)}
          disabled={disabled || !enabled}
        />
        <p className="text-caption text-fg-2">
          {t("proxy.aggregationRetry.durationHint")}
        </p>
      </div>
      <p className="text-caption text-fg-2">
        {t("proxy.aggregationRetry.savedHint")}
      </p>
      <Button size="compact" onClick={save} disabled={disabled}>
        {update.isPending ? (
          <Loader2 className="h-3.5 w-3.5 animate-spin" />
        ) : (
          <Save className="h-3.5 w-3.5" />
        )}
        {t("proxy.aggregationRetry.save")}
      </Button>
    </section>
  );
}
