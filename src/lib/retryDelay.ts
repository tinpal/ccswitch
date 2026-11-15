export type RetryDelayMode = "fixed" | "exponential_jitter";

export interface RetryDelayConfig {
  mode: RetryDelayMode;
  initialIntervalSeconds: number;
  maxIntervalSeconds: number;
}

export const DEFAULT_RETRY_DELAY: RetryDelayConfig = {
  mode: "fixed",
  initialIntervalSeconds: 1,
  maxIntervalSeconds: 60,
};

export function retryDelayConfig(
  value?: Partial<RetryDelayConfig>,
): RetryDelayConfig {
  return { ...DEFAULT_RETRY_DELAY, ...value };
}

export function validRetryDelay(value?: Partial<RetryDelayConfig>): boolean {
  const config = retryDelayConfig(value);
  return (
    (config.mode === "fixed" || config.mode === "exponential_jitter") &&
    Number.isInteger(config.initialIntervalSeconds) &&
    config.initialIntervalSeconds >= 1 &&
    config.initialIntervalSeconds <= 86400 &&
    Number.isInteger(config.maxIntervalSeconds) &&
    config.maxIntervalSeconds >= config.initialIntervalSeconds &&
    config.maxIntervalSeconds <= 86400
  );
}
