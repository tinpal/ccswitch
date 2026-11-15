import { useEffect, useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  Copy,
  RefreshCw,
  Trash2,
} from "lucide-react";
import { AppPageHeader } from "@/components/shell/AppPageHeader";
import { FullScreenPanel } from "@/components/common/FullScreenPanel";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { Button } from "@/components/ui/button";
import { Input, fieldClass } from "@/components/ui/input";
import { upstreamErrorsApi } from "@/lib/api/upstreamErrors";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";
import type {
  UpstreamErrorFilters,
  UpstreamErrorSource,
} from "@/types/upstreamErrors";

const PAGE_SIZE = 50;
const QUERY_KEY = ["upstreamErrors"] as const;
const SOURCES: UpstreamErrorSource[] = ["keep_alive", "aggregation_retry"];

export function UpstreamErrorsPage() {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const [source, setSource] = useState<UpstreamErrorSource | "">("");
  const [app, setApp] = useState("");
  const [status, setStatus] = useState("");
  const [range, setRange] = useState("");
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [page, setPage] = useState(1);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const [selected, setSelected] = useState<number | null>(null);
  const [confirmClear, setConfirmClear] = useState(false);
  const [since, setSince] = useState<number | undefined>();

  useEffect(() => {
    const timer = window.setTimeout(() => {
      setQuery(search.trim());
      setPage(1);
    }, 250);
    return () => window.clearTimeout(timer);
  }, [search]);

  const filters = useMemo<UpstreamErrorFilters>(
    () => ({
      source: source || undefined,
      appType: app || undefined,
      statusCode: status ? Number(status) : undefined,
      query: query || undefined,
      since,
    }),
    [source, app, status, query, since],
  );

  const logs = useQuery({
    queryKey: [...QUERY_KEY, "list", filters, page],
    queryFn: () =>
      upstreamErrorsApi.list(
        {
          ...filters,
          since: range ? Date.now() - Number(range) * 3600000 : undefined,
        },
        page,
        PAGE_SIZE,
      ),
    refetchInterval:
      autoRefresh && page === 1 && selected === null ? 5000 : false,
  });
  const pages = Math.max(1, Math.ceil((logs.data?.total ?? 0) / PAGE_SIZE));
  useEffect(() => {
    if (logs.data && page > pages) setPage(pages);
  }, [logs.data, page, pages]);

  const clear = useMutation({
    mutationFn: upstreamErrorsApi.clear,
    onSuccess: async () => {
      setConfirmClear(false);
      setSelected(null);
      setPage(1);
      await queryClient.invalidateQueries({ queryKey: QUERY_KEY });
      toast.success(t("upstreamErrors.cleared"));
    },
    onError: () => toast.error(t("upstreamErrors.clearFailed")),
  });
  const date = (value: number) =>
    new Date(value).toLocaleString(i18n.resolvedLanguage || i18n.language);
  const selectClass = cn(fieldClass, "h-control w-auto min-w-[130px] py-1");
  const filtered = Boolean(source || app || status || range || search);

  return (
    <>
      <AppPageHeader
        icon={<AlertTriangle className="h-5 w-5" strokeWidth={1.5} />}
        title={t("upstreamErrors.title")}
        actions={
          <>
            <Button
              variant="outline"
              size="sm"
              onClick={() => void logs.refetch()}
              disabled={logs.isFetching}
            >
              <RefreshCw
                className={cn("h-3.5 w-3.5", logs.isFetching && "animate-spin")}
              />
              {t("upstreamErrors.refresh")}
            </Button>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => setConfirmClear(true)}
              disabled={clear.isPending}
            >
              <Trash2 className="h-3.5 w-3.5" />
              {t("upstreamErrors.clear")}
            </Button>
          </>
        }
      />
      <div
        id="main-content"
        tabIndex={-1}
        className="flex min-h-0 flex-1 flex-col overflow-auto px-6 py-5"
      >
        <p className="mb-4 text-body text-fg-2">
          {t("upstreamErrors.description")}
        </p>
        <div
          className="mb-4 flex flex-wrap items-center gap-2"
          role="search"
          aria-label={t("upstreamErrors.filters")}
        >
          <select
            className={selectClass}
            aria-label={t("upstreamErrors.source")}
            value={source}
            onChange={(event) => {
              setSource(event.target.value as UpstreamErrorSource | "");
              setPage(1);
            }}
          >
            <option value="">{t("upstreamErrors.allSources")}</option>
            {SOURCES.map((value) => (
              <option value={value} key={value}>
                {t(`upstreamErrors.sources.${value}`)}
              </option>
            ))}
          </select>
          <select
            className={selectClass}
            aria-label={t("upstreamErrors.app")}
            value={app}
            onChange={(event) => {
              setApp(event.target.value);
              setPage(1);
            }}
          >
            <option value="">{t("upstreamErrors.allApps")}</option>
            <option value="codex">Codex</option>
            <option value="claude">Claude Code</option>
          </select>
          <select
            className={selectClass}
            aria-label={t("upstreamErrors.status")}
            value={status}
            onChange={(event) => {
              setStatus(event.target.value);
              setPage(1);
            }}
          >
            <option value="">{t("upstreamErrors.allStatuses")}</option>
            {[
              200, 400, 401, 403, 404, 408, 409, 422, 429, 500, 502, 503, 504,
              529,
            ].map((value) => (
              <option value={value} key={value}>
                HTTP {value}
              </option>
            ))}
          </select>
          <select
            className={selectClass}
            aria-label={t("upstreamErrors.timeRange")}
            value={range}
            onChange={(event) => {
              const value = event.target.value;
              setRange(value);
              setSince(
                value ? Date.now() - Number(value) * 3600000 : undefined,
              );
              setPage(1);
            }}
          >
            <option value="">{t("upstreamErrors.allTime")}</option>
            <option value="1">{t("upstreamErrors.lastHour")}</option>
            <option value="24">{t("upstreamErrors.lastDay")}</option>
            <option value="168">{t("upstreamErrors.lastWeek")}</option>
          </select>
          <Input
            className="min-w-[240px] flex-1"
            value={search}
            maxLength={256}
            aria-label={t("upstreamErrors.search")}
            placeholder={t("upstreamErrors.search")}
            onChange={(event) => setSearch(event.target.value)}
          />
          {filtered && (
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setSource("");
                setApp("");
                setStatus("");
                setRange("");
                setSince(undefined);
                setSearch("");
                setQuery("");
                setPage(1);
              }}
            >
              {t("upstreamErrors.reset")}
            </Button>
          )}
        </div>
        <div className="mb-3 flex flex-wrap items-center justify-between gap-2 text-caption text-fg-2">
          <span aria-live="polite">
            {t("upstreamErrors.count", { count: logs.data?.total ?? 0 })}
          </span>
          <label className="flex cursor-pointer items-center gap-2">
            <input
              type="checkbox"
              checked={autoRefresh}
              onChange={(event) => setAutoRefresh(event.target.checked)}
              className="accent-primary"
            />
            {t("upstreamErrors.autoRefresh")}
          </label>
        </div>
        {logs.isError && (
          <div
            role="alert"
            className="mb-3 rounded-control border border-danger/30 p-3 text-body text-danger"
          >
            {t("upstreamErrors.loadFailed")}
          </div>
        )}
        <div
          className="min-h-[220px] overflow-x-auto rounded-lg border border-border"
          aria-busy={logs.isFetching}
        >
          <table className="w-full min-w-[780px] table-fixed text-start text-body">
            <thead className="bg-subtle text-caption text-fg-2">
              <tr>
                <th
                  scope="col"
                  className="w-[155px] px-3 py-2 text-start font-medium"
                >
                  {t("upstreamErrors.time")}
                </th>
                <th
                  scope="col"
                  className="w-[108px] px-3 py-2 text-start font-medium"
                >
                  {t("upstreamErrors.source")}
                </th>
                <th
                  scope="col"
                  className="w-[175px] px-3 py-2 text-start font-medium"
                >
                  {t("upstreamErrors.providerModel")}
                </th>
                <th
                  scope="col"
                  className="w-[70px] px-3 py-2 text-start font-medium"
                >
                  HTTP
                </th>
                <th
                  scope="col"
                  className="w-[70px] px-3 py-2 text-start font-medium"
                >
                  {t("upstreamErrors.attempt")}
                </th>
                <th scope="col" className="px-3 py-2 text-start font-medium">
                  {t("upstreamErrors.error")}
                </th>
                <th scope="col" className="w-[70px] px-3 py-2">
                  <span className="sr-only">{t("upstreamErrors.details")}</span>
                </th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border">
              {logs.data?.data.map((entry) => (
                <tr key={entry.id} className="hover:bg-subtle/60">
                  <td className="px-3 py-3 text-caption tabular-nums text-fg-2">
                    {date(entry.createdAt)}
                  </td>
                  <td className="px-3 py-3">
                    <span className="rounded bg-subtle px-1.5 py-1 text-caption">
                      {t(`upstreamErrors.sources.${entry.source}`)}
                    </span>
                  </td>
                  <td className="px-3 py-3">
                    <div className="truncate font-medium" title={entry.model}>
                      {entry.model}
                    </div>
                    <div
                      className="mt-1 truncate text-caption text-fg-2"
                      title={entry.providerName}
                    >
                      {entry.providerName} ·{" "}
                      {entry.appType === "codex" ? "Codex" : "Claude Code"}
                    </div>
                  </td>
                  <td className="px-3 py-3 font-mono text-caption text-danger">
                    {entry.statusCode ?? "—"}
                  </td>
                  <td className="px-3 py-3 tabular-nums text-fg-2">
                    #{entry.attempt}
                  </td>
                  <td className="px-3 py-3">
                    <div
                      className="truncate font-mono text-caption"
                      title={entry.errorCode}
                    >
                      {entry.errorCode}
                    </div>
                    <div
                      className="mt-1 truncate text-caption text-fg-2"
                      title={entry.message}
                    >
                      {entry.message}
                    </div>
                  </td>
                  <td className="px-2 py-3">
                    <Button
                      variant="ghost"
                      size="compact"
                      onClick={() => setSelected(entry.id)}
                      aria-label={`${t("upstreamErrors.details")} · ${entry.model} · #${entry.attempt}`}
                    >
                      {t("upstreamErrors.details")}
                    </Button>
                  </td>
                </tr>
              ))}
              {!logs.data?.data.length && (
                <tr>
                  <td colSpan={7} className="px-6 py-16 text-center text-fg-2">
                    {logs.isPending
                      ? t("upstreamErrors.loading")
                      : logs.isError
                        ? t("upstreamErrors.loadFailed")
                        : filtered
                          ? t("upstreamErrors.noMatches")
                          : t("upstreamErrors.empty")}
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
        <div className="mt-4 flex flex-wrap items-center justify-between gap-3 text-caption text-fg-2">
          <p>
            {t("upstreamErrors.retention", {
              count: logs.data?.retentionLimit ?? 10000,
            })}
          </p>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="compact"
              aria-label={t("upstreamErrors.previous")}
              disabled={page <= 1 || logs.isFetching}
              onClick={() => setPage(page - 1)}
            >
              <ChevronLeft className="h-4 w-4" />
            </Button>
            <span>{t("upstreamErrors.page", { page, pages })}</span>
            <Button
              variant="outline"
              size="compact"
              aria-label={t("upstreamErrors.next")}
              disabled={page >= pages || logs.isFetching}
              onClick={() => setPage(page + 1)}
            >
              <ChevronRight className="h-4 w-4" />
            </Button>
          </div>
        </div>
      </div>
      <UpstreamErrorDetail id={selected} onClose={() => setSelected(null)} />
      <ConfirmDialog
        isOpen={confirmClear}
        title={t("upstreamErrors.clear")}
        message={t("upstreamErrors.clearConfirm")}
        confirmText={t("upstreamErrors.clear")}
        pending={clear.isPending}
        onConfirm={() => clear.mutate()}
        onCancel={() => setConfirmClear(false)}
      />
    </>
  );
}

function UpstreamErrorDetail({
  id,
  onClose,
}: {
  id: number | null;
  onClose: () => void;
}) {
  const { t, i18n } = useTranslation();
  const detail = useQuery({
    queryKey: [...QUERY_KEY, "detail", id],
    queryFn: () => upstreamErrorsApi.detail(id!),
    enabled: id !== null,
  });
  const entry = detail.data;
  const copy = async () => {
    if (!entry) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(entry, null, 2));
      toast.success(t("upstreamErrors.copied"));
    } catch {
      toast.error(t("upstreamErrors.copyFailed"));
    }
  };
  const fields: [string, string | number | null][] = entry
    ? [
        [
          "time",
          new Date(entry.createdAt).toLocaleString(
            i18n.resolvedLanguage || i18n.language,
          ),
        ],
        ["source", t(`upstreamErrors.sources.${entry.source}`)],
        ["app", entry.appType === "codex" ? "Codex" : "Claude Code"],
        ["provider", entry.providerName],
        ["model", entry.model],
        ["status", entry.statusCode],
        ["attempt", entry.attempt],
        ["elapsed", `${entry.elapsedMs} ms`],
        ["requestId", entry.requestId],
        ["upstreamRequestId", entry.upstreamRequestId],
      ]
    : [];
  return (
    <FullScreenPanel
      isOpen={id !== null}
      onClose={onClose}
      title={t("upstreamErrors.detailTitle")}
      actions={
        <Button
          variant="outline"
          size="sm"
          onClick={() => void copy()}
          disabled={!entry}
        >
          <Copy className="h-3.5 w-3.5" />
          {t("upstreamErrors.copy")}
        </Button>
      }
    >
      {detail.isPending ? (
        <p>{t("upstreamErrors.loading")}</p>
      ) : detail.isError ? (
        <p role="alert">{t("upstreamErrors.loadFailed")}</p>
      ) : !entry ? (
        <p>{t("upstreamErrors.expired")}</p>
      ) : (
        <>
          <div className="rounded-lg border border-danger/25 bg-danger/5 p-4">
            <p className="break-all font-mono text-body font-medium">
              {entry.errorCode}
            </p>
            <p className="mt-2 whitespace-pre-wrap break-words text-body text-fg-2">
              {entry.message}
            </p>
          </div>
          <dl className="grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2">
            {fields.map(([label, value]) => (
              <div key={label}>
                <dt className="text-caption text-fg-2">
                  {t(`upstreamErrors.${label}`)}
                </dt>
                <dd className="mt-1 break-all text-body">{value ?? "—"}</dd>
              </div>
            ))}
          </dl>
          {entry.statusCode === 200 && (
            <p className="text-caption text-fg-2">
              {t("upstreamErrors.semanticError")}
            </p>
          )}
          <section>
            <h3 className="mb-2 text-body font-medium">
              {t("upstreamErrors.responseBody")}
            </h3>
            <p className="mb-3 text-caption text-fg-2">
              {t("upstreamErrors.redacted")}
            </p>
            <pre className="max-h-[480px] overflow-auto whitespace-pre-wrap break-all rounded-lg border border-border bg-subtle p-4 font-mono text-caption">
              {entry.responseBody || t("upstreamErrors.noBody")}
            </pre>
            {entry.bodyTruncated && (
              <p className="mt-2 text-caption text-fg-2">
                {t("upstreamErrors.truncated")}
              </p>
            )}
          </section>
        </>
      )}
    </FullScreenPanel>
  );
}
