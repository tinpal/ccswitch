export type UpstreamErrorSource = "keep_alive" | "aggregation_retry";

export interface UpstreamErrorLog {
  id: number;
  createdAt: number;
  source: UpstreamErrorSource;
  appType: string;
  providerId: string;
  providerName: string;
  model: string;
  requestId: string;
  upstreamRequestId: string | null;
  attempt: number;
  statusCode: number | null;
  errorCode: string;
  message: string;
  responseBody: string | null;
  bodyTruncated: boolean;
  elapsedMs: number;
}

export interface UpstreamErrorFilters {
  source?: UpstreamErrorSource;
  appType?: string;
  statusCode?: number;
  query?: string;
  since?: number;
}

export interface PaginatedUpstreamErrors {
  data: UpstreamErrorLog[];
  total: number;
  page: number;
  pageSize: number;
  retentionLimit: number;
}
