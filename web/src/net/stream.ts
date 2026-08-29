/**
 * The WebSocket half: a viewport subscription that yields deltas.
 *
 * Reconnection is not optional here. This client is meant to be left open on a
 * screen for days, across suspends, WiFi changes and server restarts, and a
 * stream that silently stops is worse than one that never started — the map
 * keeps showing a world that stopped existing hours ago. So a dropped socket
 * reconnects with backoff and re-subscribes, and the caller is told about the
 * gap rather than left to infer it.
 */

import { ARGUS_BASE, deviceToken } from "../config";
import type { Entity, ServerFrame } from "./types";

export interface Subscription {
  bbox?: [number, number, number, number];
  layers?: string[];
  kinds?: string[];
  at?: Date | null;
}

export interface StreamHandlers {
  /** Everything in the box, sent once per subscription. */
  onSnapshot(entities: Entity[], at: Date): void;
  /** What has changed since the last frame. */
  onDelta(entities: Entity[], at: Date): void;
  /** Connection state, for an honest indicator rather than a hopeful one. */
  onStatus(status: StreamStatus, detail?: string): void;
}

export type StreamStatus = "connecting" | "open" | "closed" | "error";

const MIN_BACKOFF_MS = 500;
const MAX_BACKOFF_MS = 30_000;

export class DeltaStream {
  #socket: WebSocket | null = null;
  #subscription: Subscription | null = null;
  #backoff = MIN_BACKOFF_MS;
  #retry: ReturnType<typeof setTimeout> | null = null;
  #closedByUs = false;

  constructor(private readonly handlers: StreamHandlers) {}

  /**
   * Set or replace the viewport. Panning is a re-subscribe rather than an
   * unsubscribe-then-subscribe: there is only ever one viewport, and the
   * two-step version has a window where the client is watching nothing.
   */
  subscribe(subscription: Subscription): void {
    this.#subscription = subscription;
    if (this.#socket?.readyState === WebSocket.OPEN) {
      this.#send();
    } else {
      this.connect();
    }
  }

  connect(): void {
    if (
      this.#socket &&
      (this.#socket.readyState === WebSocket.OPEN ||
        this.#socket.readyState === WebSocket.CONNECTING)
    ) {
      return;
    }
    this.#closedByUs = false;
    this.handlers.onStatus("connecting");

    const base = ARGUS_BASE || window.location.origin;
    const url = new URL("/v1/stream", base);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    // A browser cannot set a header on a WebSocket handshake, which is exactly
    // why the server accepts a token in the query string.
    const token = deviceToken();
    if (token) url.searchParams.set("token", token);

    const socket = new WebSocket(url);
    this.#socket = socket;

    socket.onopen = () => {
      this.#backoff = MIN_BACKOFF_MS;
      this.handlers.onStatus("open");
      this.#send();
    };

    socket.onmessage = (event) => {
      let frame: ServerFrame;
      try {
        frame = JSON.parse(event.data as string) as ServerFrame;
      } catch {
        return;
      }
      switch (frame.type) {
        case "snapshot":
          this.handlers.onSnapshot(frame.entities, new Date(frame.at));
          break;
        case "delta":
          this.handlers.onDelta(frame.entities, new Date(frame.at));
          break;
        case "error":
          this.handlers.onStatus("error", frame.message);
          break;
        case "pong":
          break;
      }
    };

    socket.onerror = () => this.handlers.onStatus("error", "connection failed");

    socket.onclose = () => {
      this.#socket = null;
      if (this.#closedByUs) {
        this.handlers.onStatus("closed");
        return;
      }
      this.handlers.onStatus(
        "closed",
        `reconnecting in ${Math.round(this.#backoff / 100) / 10}s`,
      );
      this.#retry = setTimeout(() => this.connect(), this.#backoff);
      // Exponential with a ceiling: a server that is down for an hour should
      // not be probed a thousand times, and one that just restarted should be
      // found again within a second.
      this.#backoff = Math.min(this.#backoff * 2, MAX_BACKOFF_MS);
    };
  }

  close(): void {
    this.#closedByUs = true;
    if (this.#retry) clearTimeout(this.#retry);
    this.#retry = null;
    this.#socket?.close();
    this.#socket = null;
  }

  #send(): void {
    const sub = this.#subscription;
    if (!sub || this.#socket?.readyState !== WebSocket.OPEN) return;
    this.#socket.send(
      JSON.stringify({
        type: "subscribe",
        bbox: sub.bbox ? sub.bbox.join(",") : undefined,
        layers: sub.layers?.length ? sub.layers.join(",") : undefined,
        kinds: sub.kinds?.length ? sub.kinds.join(",") : undefined,
        at: sub.at ? sub.at.toISOString() : undefined,
      }),
    );
  }
}
