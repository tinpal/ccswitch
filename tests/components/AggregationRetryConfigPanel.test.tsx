import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { http, HttpResponse } from "msw";
import { AggregationRetryConfigPanel } from "@/components/proxy/AggregationRetryConfigPanel";
import { server } from "../msw/server";
import {
  getCodexAggregationRetryConfig,
  setCodexAggregationRetryConfig,
  getSettings,
  setSettings,
} from "../msw/state";
import { createTestQueryClient } from "../utils/testQueryClient";

const ENABLED = "proxy.aggregationRetry.enabled";
const UNLIMITED = "proxy.aggregationRetry.unlimited";
const COUNT = "proxy.aggregationRetry.maxRetries";
const DURATION = "proxy.aggregationRetry.maxElapsedSeconds";
const SAVE = "proxy.aggregationRetry.save";

function renderPanel() {
  return render(
    <QueryClientProvider client={createTestQueryClient()}>
      <AggregationRetryConfigPanel />
    </QueryClientProvider>,
  );
}

describe("AggregationRetryConfigPanel", () => {
  beforeEach(() => {
    setCodexAggregationRetryConfig({
      enabled: true,
      parallelEnabled: false,
      parallelRequests: 2,
      retryIntervalSeconds: 0,
      maxRetries: 20,
      maxElapsedSeconds: 600,
    });
  });
  it("loads the enabled, 20-retry, 600-second defaults", async () => {
    renderPanel();
    expect(await screen.findByRole("switch", { name: ENABLED })).toBeChecked();
    expect(screen.getByRole("switch", { name: UNLIMITED })).not.toBeChecked();
    expect(screen.getByRole("spinbutton", { name: COUNT })).toHaveValue(20);
    expect(screen.getByRole("spinbutton", { name: DURATION })).toHaveValue(600);
  });

  it("saves limits independently and reloads them on reopening", async () => {
    const user = userEvent.setup();
    setSettings({ codexStackSubagentModel: "ccs-agentrouter/gpt-6.1-sol" });
    const settings = getSettings();
    const panel = renderPanel();
    const count = await screen.findByRole("spinbutton", { name: COUNT });
    await user.clear(count);
    await user.type(count, "3");
    const duration = screen.getByRole("spinbutton", { name: DURATION });
    await user.clear(duration);
    await user.type(duration, "45");
    await user.click(screen.getByRole("button", { name: SAVE }));
    await waitFor(() =>
      expect(getCodexAggregationRetryConfig()).toEqual({
        enabled: true,
        parallelEnabled: false,
        parallelRequests: 2,
        retryIntervalSeconds: 0,
        maxRetries: 3,
        maxElapsedSeconds: 45,
      }),
    );
    expect(getSettings()).toEqual(settings);
    panel.unmount();
    renderPanel();
    expect(await screen.findByRole("spinbutton", { name: COUNT })).toHaveValue(
      3,
    );
    expect(screen.getByRole("spinbutton", { name: DURATION })).toHaveValue(45);
  });

  it("saves unlimited retries while keeping a required time budget", async () => {
    const user = userEvent.setup();
    renderPanel();
    await user.click(await screen.findByRole("switch", { name: UNLIMITED }));
    expect(screen.getByRole("spinbutton", { name: COUNT })).toBeDisabled();
    expect(
      screen.getByRole("spinbutton", { name: DURATION }),
    ).not.toBeDisabled();
    await user.click(screen.getByRole("button", { name: SAVE }));
    await waitFor(() =>
      expect(getCodexAggregationRetryConfig().maxRetries).toBeNull(),
    );
    expect(getCodexAggregationRetryConfig().maxElapsedSeconds).toBe(600);
  });

  it("ignores an obsolete success-only setting and saves the remaining limits", async () => {
    const user = userEvent.setup();
    server.use(
      http.post("http://tauri.local/get_codex_aggregation_retry_config", () =>
        HttpResponse.json({
          enabled: true,
          successOnly: true,
          maxRetries: 20,
          maxElapsedSeconds: 600,
        }),
      ),
    );
    renderPanel();
    const duration = await screen.findByRole("spinbutton", { name: DURATION });
    expect(
      screen.queryByRole("switch", {
        name: "proxy.aggregationRetry.successOnly",
      }),
    ).not.toBeInTheDocument();
    expect(duration).not.toBeDisabled();
    await user.click(screen.getByRole("switch", { name: UNLIMITED }));
    await user.clear(duration);
    await user.type(duration, "60");
    await user.click(screen.getByRole("button", { name: SAVE }));
    await waitFor(() =>
      expect(getCodexAggregationRetryConfig()).toEqual({
        enabled: true,
        parallelEnabled: false,
        parallelRequests: 2,
        retryIntervalSeconds: 0,
        maxRetries: null,
        maxElapsedSeconds: 60,
      }),
    );
  });

  it("persists disabling retries and retains the user's limits", async () => {
    const user = userEvent.setup();
    setCodexAggregationRetryConfig({
      enabled: true,
      parallelEnabled: false,
      parallelRequests: 2,
      retryIntervalSeconds: 0,
      maxRetries: 7,
      maxElapsedSeconds: 30,
    });
    const panel = renderPanel();
    await user.click(await screen.findByRole("switch", { name: ENABLED }));
    await user.click(screen.getByRole("button", { name: SAVE }));
    await waitFor(() =>
      expect(getCodexAggregationRetryConfig()).toEqual({
        enabled: false,
        parallelEnabled: false,
        parallelRequests: 2,
        retryIntervalSeconds: 0,
        maxRetries: 7,
        maxElapsedSeconds: 30,
      }),
    );
    panel.unmount();
    renderPanel();
    expect(
      await screen.findByRole("switch", { name: ENABLED }),
    ).not.toBeChecked();
    expect(screen.getByRole("spinbutton", { name: COUNT })).toBeDisabled();
  });

  it.each(["0", "86401"])(
    "rejects an invalid time budget of %s",
    async (value) => {
      const user = userEvent.setup();
      const save = vi.fn(() => HttpResponse.json(null));
      server.use(
        http.post(
          "http://tauri.local/set_codex_aggregation_retry_config",
          save,
        ),
      );
      renderPanel();
      const duration = await screen.findByRole("spinbutton", {
        name: DURATION,
      });
      await user.clear(duration);
      await user.type(duration, value);
      await user.click(screen.getByRole("button", { name: SAVE }));
      expect(save).not.toHaveBeenCalled();
    },
  );

  it("allows a zero retry limit", async () => {
    const user = userEvent.setup();
    renderPanel();
    const count = await screen.findByRole("spinbutton", { name: COUNT });
    await user.clear(count);
    await user.type(count, "0");
    await user.click(screen.getByRole("button", { name: SAVE }));
    await waitFor(() =>
      expect(getCodexAggregationRetryConfig().maxRetries).toBe(0),
    );
  });

  it("keeps saving disabled if the persisted config cannot be read", async () => {
    server.use(
      http.post("http://tauri.local/get_codex_aggregation_retry_config", () =>
        HttpResponse.json({ message: "database unavailable" }, { status: 500 }),
      ),
    );
    renderPanel();
    expect(await screen.findByRole("alert")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: SAVE })).toBeDisabled();
  });
});
