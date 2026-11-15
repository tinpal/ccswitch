import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { UpstreamErrorsPage } from "@/components/upstream-errors/UpstreamErrorsPage";
import { upstreamErrorsApi } from "@/lib/api/upstreamErrors";
import type { UpstreamErrorLog } from "@/types/upstreamErrors";

vi.mock("@/lib/api/upstreamErrors", () => ({
  upstreamErrorsApi: { list: vi.fn(), detail: vi.fn(), clear: vi.fn() },
}));
vi.mock("@/components/common/FullScreenPanel", () => ({
  FullScreenPanel: ({ isOpen, title, children, actions, onClose }: any) =>
    isOpen ? (
      <section aria-label={title}>
        {actions}
        <button onClick={onClose}>Close detail</button>
        {children}
      </section>
    ) : null,
}));

const row: UpstreamErrorLog = {
  id: 10,
  createdAt: 1794704400000,
  source: "keep_alive",
  appType: "codex",
  providerId: "provider-a",
  providerName: "Gateway A",
  model: "model-a",
  requestId: "local-request-a",
  upstreamRequestId: "upstream-request-a",
  attempt: 3,
  statusCode: 500,
  errorCode: "get_channel_failed",
  message: "No available channel",
  responseBody: null,
  bodyTruncated: false,
  elapsedMs: 132,
};
const second: UpstreamErrorLog = {
  ...row,
  id: 11,
  source: "aggregation_retry",
  model: "model-b",
  requestId: "local-request-b",
  statusCode: 200,
};

function setup() {
  const client = new QueryClient({
    defaultOptions: {
      queries: { retry: false, gcTime: 0 },
      mutations: { retry: false },
    },
  });
  render(
    <QueryClientProvider client={client}>
      <UpstreamErrorsPage />
    </QueryClientProvider>,
  );
  return userEvent.setup();
}

beforeEach(() => {
  vi.mocked(upstreamErrorsApi.list).mockResolvedValue({
    data: [second, row],
    total: 2,
    page: 1,
    pageSize: 50,
    retentionLimit: 10000,
  });
  vi.mocked(upstreamErrorsApi.detail).mockResolvedValue({
    ...second,
    responseBody:
      '{"error":{"code":"get_channel_failed","api_key":"[redacted]"}}',
    bodyTruncated: true,
  });
  vi.mocked(upstreamErrorsApi.clear).mockResolvedValue(2);
});

describe("UpstreamErrorsPage", () => {
  it("lists both diagnostic sources and uses the independent API", async () => {
    setup();
    expect(await screen.findByText("model-a")).toBeInTheDocument();
    expect(screen.getByText("model-b")).toBeInTheDocument();
    expect(
      screen.getAllByText("upstreamErrors.sources.keep_alive").length,
    ).toBeGreaterThan(0);
    expect(
      screen.getAllByText("upstreamErrors.sources.aggregation_retry").length,
    ).toBeGreaterThan(0);
    expect(upstreamErrorsApi.list).toHaveBeenCalledWith(
      expect.objectContaining({ source: undefined, appType: undefined }),
      1,
      50,
    );
  });

  it("combines source, app, HTTP and search filters", async () => {
    const user = setup();
    await screen.findByText("model-a");
    await user.selectOptions(
      screen.getByLabelText("upstreamErrors.source"),
      "keep_alive",
    );
    await user.selectOptions(
      screen.getByLabelText("upstreamErrors.app"),
      "codex",
    );
    await user.selectOptions(
      screen.getByLabelText("upstreamErrors.status"),
      "500",
    );
    await user.type(
      screen.getByRole("textbox", { name: "upstreamErrors.search" }),
      "channel",
    );
    await waitFor(() =>
      expect(upstreamErrorsApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({
          source: "keep_alive",
          appType: "codex",
          statusCode: 500,
          query: "channel",
        }),
        1,
        50,
      ),
    );
    await user.click(
      screen.getByRole("button", { name: "upstreamErrors.reset" }),
    );
    await waitFor(() =>
      expect(upstreamErrorsApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({
          source: undefined,
          appType: undefined,
          query: undefined,
        }),
        1,
        50,
      ),
    );
  });

  it("opens and copies persisted details including semantic HTTP 200 errors", async () => {
    const user = setup();
    const clipboard = vi
      .spyOn(navigator.clipboard, "writeText")
      .mockResolvedValue();
    await user.click(
      await screen.findByRole("button", {
        name: "upstreamErrors.details · model-b · #3",
      }),
    );
    const detail = screen.getByRole("region", {
      name: "upstreamErrors.detailTitle",
    });
    expect(
      await within(detail).findByText("upstream-request-a"),
    ).toBeInTheDocument();
    expect(
      within(detail).getByText("upstreamErrors.semanticError"),
    ).toBeInTheDocument();
    expect(
      within(detail).getByText("upstreamErrors.truncated"),
    ).toBeInTheDocument();
    await user.click(
      within(detail).getByRole("button", { name: "upstreamErrors.copy" }),
    );
    expect(clipboard).toHaveBeenCalledWith(
      expect.stringContaining("[redacted]"),
    );
    expect(upstreamErrorsApi.detail).toHaveBeenCalledWith(11);
  });

  it("clears history only after confirmation and refreshes the independent list", async () => {
    const user = setup();
    await screen.findByText("model-a");
    await user.click(
      screen.getByRole("button", { name: "upstreamErrors.clear" }),
    );
    expect(upstreamErrorsApi.clear).not.toHaveBeenCalled();
    await user.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "common.cancel",
      }),
    );
    expect(upstreamErrorsApi.clear).not.toHaveBeenCalled();
    await user.click(
      screen.getByRole("button", { name: "upstreamErrors.clear" }),
    );
    vi.mocked(upstreamErrorsApi.list).mockResolvedValue({
      data: [],
      total: 0,
      page: 1,
      pageSize: 50,
      retentionLimit: 10000,
    });
    await user.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "upstreamErrors.clear",
      }),
    );
    await screen.findByText("upstreamErrors.empty");
    expect(upstreamErrorsApi.clear).toHaveBeenCalledTimes(1);
  });

  it("shows load failures and recovers when refreshed", async () => {
    vi.mocked(upstreamErrorsApi.list).mockRejectedValueOnce(
      new Error("database unavailable"),
    );
    const user = setup();
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "upstreamErrors.loadFailed",
    );
    await user.click(
      screen.getByRole("button", { name: "upstreamErrors.refresh" }),
    );
    expect(await screen.findByText("model-a")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("handles a record pruned between listing and opening", async () => {
    vi.mocked(upstreamErrorsApi.detail).mockResolvedValue(null);
    const user = setup();
    await user.click(
      await screen.findByRole("button", {
        name: "upstreamErrors.details · model-a · #3",
      }),
    );
    expect(
      await screen.findByText("upstreamErrors.expired"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "upstreamErrors.copy" }),
    ).toBeDisabled();
  });

  it("returns to page one after changing a filter", async () => {
    vi.mocked(upstreamErrorsApi.list).mockImplementation(
      async (_filters, page = 1) => ({
        data: [row],
        total: 51,
        page,
        pageSize: 50,
        retentionLimit: 10000,
      }),
    );
    const user = setup();
    await screen.findByText("model-a");
    await user.click(
      screen.getByRole("button", { name: "upstreamErrors.next" }),
    );
    await waitFor(() =>
      expect(upstreamErrorsApi.list).toHaveBeenLastCalledWith(
        expect.any(Object),
        2,
        50,
      ),
    );
    fireEvent.change(screen.getByLabelText("upstreamErrors.source"), {
      target: { value: "aggregation_retry" },
    });
    await waitFor(() =>
      expect(upstreamErrorsApi.list).toHaveBeenLastCalledWith(
        expect.objectContaining({ source: "aggregation_retry" }),
        1,
        50,
      ),
    );
  });
});
