import { invoke } from "@tauri-apps/api/core";
import type {
  PaginatedUpstreamErrors,
  UpstreamErrorFilters,
  UpstreamErrorLog,
} from "@/types/upstreamErrors";

export const upstreamErrorsApi = {
  list: (filters: UpstreamErrorFilters, page = 1, pageSize = 50) =>
    invoke<PaginatedUpstreamErrors>("get_upstream_error_logs", {
      filters,
      page,
      pageSize,
    }),
  detail: (id: number) =>
    invoke<UpstreamErrorLog | null>("get_upstream_error_detail", { id }),
  clear: () => invoke<number>("clear_upstream_error_logs"),
};
