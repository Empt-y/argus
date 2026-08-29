/**
 * The REST half of talking to argusd.
 *
 * Thin on purpose. Every endpoint here is one the server already shaped for a
 * client, so anything clever happening in this file would mean the API was
 * wrong.
 */

import { ARGUS_BASE, deviceToken } from "../config.ts";
import type {
  ClientKeys,
  EntitiesResponse,
  Entity,
  Health,
  Layer,
  Source,
  TrackResponse,
} from "./types.ts";

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "ApiError";
  }
}

/** A viewport request, in the shape the server's query parameters take. */
export interface Viewport {
  bbox?: [number, number, number, number];
  layers?: string[];
  kinds?: string[];
  /** The DVR instant. Absent is live. */
  at?: Date | null;
  limit?: number;
}

export function viewportParams(view: Viewport): URLSearchParams {
  const params = new URLSearchParams();
  if (view.bbox) params.set("bbox", view.bbox.join(","));
  if (view.layers?.length) params.set("layers", view.layers.join(","));
  if (view.kinds?.length) params.set("kinds", view.kinds.join(","));
  if (view.at) params.set("at", view.at.toISOString());
  if (view.limit) params.set("limit", String(view.limit));
  return params;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const token = deviceToken();
  const headers = new Headers(init?.headers);
  if (token) headers.set("Authorization", `Bearer ${token}`);

  const response = await fetch(`${ARGUS_BASE}${path}`, { ...init, headers });
  if (response.status === 204) return undefined as T;
  if (!response.ok) {
    // The server's error envelope is `{ error: { code, message } }`. Falling
    // back to the status line matters: a proxy or a dead server answers with
    // something else entirely, and "Unexpected token < in JSON" is not a
    // useful thing to show someone.
    let code = "http_error";
    let message = `${response.status} ${response.statusText}`;
    try {
      const body = await response.json();
      if (body?.error) {
        code = body.error.code ?? code;
        message = body.error.message ?? message;
      }
    } catch {
      /* keep the status line */
    }
    throw new ApiError(response.status, code, message);
  }
  return (await response.json()) as T;
}

export const api = {
  health: () => request<Health>("/v1/health"),

  layers: () => request<{ layers: Layer[] }>("/v1/layers").then((r) => r.layers),

  sources: () =>
    request<{ sources: Source[] }>("/v1/sources").then((r) => r.sources),

  clientKeys: () => request<ClientKeys>("/v1/client-keys"),

  entities: (view: Viewport) =>
    request<EntitiesResponse>(`/v1/entities?${viewportParams(view)}`),

  entity: (kind: string, key: string) =>
    request<Entity>(`/v1/entities/${kind}/${encodeURIComponent(key)}`),

  track: (kind: string, key: string, from?: Date, to?: Date) => {
    const params = new URLSearchParams();
    if (from) params.set("from", from.toISOString());
    if (to) params.set("to", to.toISOString());
    return request<TrackResponse>(
      `/v1/entities/${kind}/${encodeURIComponent(key)}/track?${params}`,
    );
  },
};
