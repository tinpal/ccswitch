import type { ModelKeepAliveConfig } from "@/types";
import { retryDelayConfig, validRetryDelay } from "@/lib/retryDelay";

export const DEFAULT_MODEL_KEEP_ALIVE: ModelKeepAliveConfig = {
  enabled: false,
  intervalSeconds: 60,
  retryIntervalSeconds: 5,
  retryDelay: retryDelayConfig(),
  timeoutSeconds: 120,
  recoveryConcurrency: 2,
};

export function modelKeepAliveConfig(
  value?: Partial<ModelKeepAliveConfig>,
): ModelKeepAliveConfig {
  return {
    ...DEFAULT_MODEL_KEEP_ALIVE,
    ...value,
    retryDelay: retryDelayConfig(value?.retryDelay),
  };
}

export function validModelKeepAlive(value?: ModelKeepAliveConfig): boolean {
  if (!value) return true;
  const config = modelKeepAliveConfig(value);
  return (
    typeof config.enabled === "boolean" &&
    validRetryDelay(config.retryDelay) &&
    Number.isInteger(config.intervalSeconds) &&
    config.intervalSeconds >= 1 &&
    config.intervalSeconds <= 86400 &&
    Number.isInteger(config.retryIntervalSeconds) &&
    config.retryIntervalSeconds >= 0 &&
    config.retryIntervalSeconds <= 86400 &&
    Number.isInteger(config.timeoutSeconds) &&
    config.timeoutSeconds >= 1 &&
    config.timeoutSeconds <= 3600 &&
    Number.isInteger(config.recoveryConcurrency) &&
    config.recoveryConcurrency >= 1 &&
    config.recoveryConcurrency <= 64
  );
}
