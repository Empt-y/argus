/**
 * The overlay: layer rail, source health, DVR scrubber, status line.
 *
 * Plain DOM, no framework. The whole surface is a few dozen elements that
 * change on a timer, and a framework here would be more code than it replaces
 * plus a build-time dependency on somebody else's release cadence.
 *
 * The one rule this file exists to keep: a source that is off because nobody
 * configured a key must not render like a source that is failing. That
 * distinction has been carried carefully from the driver through the store and
 * the API, and this is the last place it can be thrown away.
 */

import type { Layer, Source, SourceState } from "../net/types";
import type { StreamStatus } from "../net/stream";
import type { LayerRenderer } from "../layers/entities";

/** How far back the scrubber reaches. Matches the `tracks_1m` retention. */
const DVR_SPAN_MINUTES = 90 * 24 * 60;

export class Hud {
  readonly #status: HTMLElement;
  readonly #layerPanel: HTMLElement;
  readonly #sourcePanel: HTMLElement;
  readonly #scrubber: HTMLInputElement;
  readonly #scrubLabel: HTMLElement;
  readonly #notes: HTMLElement;

  #enabled = new Set<string>();
  #renderer: LayerRenderer | null = null;
  #layers: Layer[] = [];
  #minutesBack = 0;
  #onChange: (() => void) | null = null;

  constructor(root: HTMLElement) {
    root.innerHTML = `
      <div class="panel rail" style="grid-column:1;grid-row:1/span 2;max-height:calc(100vh - 24px)">
        <h2>Layers</h2>
        <div data-layers></div>
        <h2 style="margin-top:12px">Sources</h2>
        <div data-sources></div>
      </div>
      <div class="panel" style="grid-column:3;grid-row:1;min-width:16rem">
        <h2>Status</h2>
        <div data-status>starting…</div>
        <div data-notes style="color:var(--dim);margin-top:6px"></div>
      </div>
      <div class="panel" style="grid-column:2;grid-row:3;min-width:28rem">
        <h2>DVR — <span data-scrublabel>live</span></h2>
        <input data-scrub type="range" min="-${DVR_SPAN_MINUTES}" max="0"
               value="0" step="1" style="width:100%" />
      </div>
      <div class="attrib">Imagery ©&nbsp;<a href="https://www.openstreetmap.org/copyright" target="_blank" rel="noreferrer">OpenStreetMap</a> contributors · CesiumJS</div>
    `;
    this.#status = root.querySelector("[data-status]")!;
    this.#notes = root.querySelector("[data-notes]")!;
    this.#layerPanel = root.querySelector("[data-layers]")!;
    this.#sourcePanel = root.querySelector("[data-sources]")!;
    this.#scrubber = root.querySelector("[data-scrub]")!;
    this.#scrubLabel = root.querySelector("[data-scrublabel]")!;

    this.#scrubber.addEventListener("input", () => {
      this.#minutesBack = -Number(this.#scrubber.value);
      this.#scrubLabel.textContent = this.#describeInstant();
    });
    // Re-query on release rather than on every pixel of the drag: each change
    // is a full viewport query against the rollup.
    this.#scrubber.addEventListener("change", () => this.#onChange?.());
  }

  onSelectionChange(handler: () => void): void {
    this.#onChange = handler;
  }

  /** The DVR instant, or `null` for live. */
  dvrInstant(): Date | null {
    if (this.#minutesBack === 0) return null;
    return new Date(Date.now() - this.#minutesBack * 60_000);
  }

  enabledLayers(): string[] {
    return [...this.#enabled];
  }

  setLayers(layers: Layer[], renderer: LayerRenderer): void {
    this.#layers = layers;
    this.#renderer = renderer;
    for (const layer of layers) this.#enabled.add(layer.id);
    for (const layer of layers) renderer.setLayerVisible(layer.id, true);
    this.#renderLayers();
  }

  setSources(sources: Source[]): void {
    this.#sourcePanel.replaceChildren(
      ...sources.map((source) => {
        const row = document.createElement("div");
        row.className = "layer-row";
        row.style.cursor = "default";
        const chip = document.createElement("span");
        chip.className = `chip ${source.state}`;
        chip.textContent = stateLabel(source.state);
        const name = document.createElement("span");
        name.textContent = source.source_id;
        const count = document.createElement("span");
        count.className = "count";
        count.textContent = source.observations.toLocaleString();
        row.append(chip, name, count);
        if (source.last_error) row.title = source.last_error;
        return row;
      }),
    );
  }

  setCount(count: number): void {
    this.#renderLayers();
    this.#status.dataset.count = String(count);
  }

  setStreamStatus(status: StreamStatus, detail?: string): void {
    const label =
      status === "open"
        ? "streaming"
        : status === "connecting"
          ? "connecting…"
          : status === "error"
            ? "error"
            : "disconnected";
    const cls =
      status === "open" ? "live" : status === "connecting" ? "delayed" : "stale";
    this.#status.innerHTML = `<span class="chip ${cls}">${label}</span> ${
      detail ?? ""
    }`;
  }

  setPhotoreal(available: boolean): void {
    if (!available) {
      this.note("photoreal off — no Google Maps key configured");
    }
  }

  note(text: string): void {
    const line = document.createElement("div");
    line.textContent = text;
    this.#notes.append(line);
  }

  fail(text: string): void {
    this.#status.innerHTML = `<span class="chip failed">down</span> ${text}`;
  }

  #describeInstant(): string {
    const at = this.dvrInstant();
    if (!at) return "live";
    return at.toISOString().replace("T", " ").slice(0, 16) + "Z";
  }

  #renderLayers(): void {
    const renderer = this.#renderer;
    this.#layerPanel.replaceChildren(
      ...this.#layers.map((layer) => {
        const row = document.createElement("div");
        const on = this.#enabled.has(layer.id);
        row.className = `layer-row${on ? "" : " off"}`;

        const swatch = document.createElement("span");
        swatch.className = "swatch";
        swatch.style.background = layer.style.color;

        const name = document.createElement("span");
        name.textContent = layer.display_name || layer.id;

        const count = document.createElement("span");
        count.className = "count";
        // The catalogue's own count, not what is drawn: they differ, and the
        // difference is honest — the client only holds what is in view.
        count.textContent = layer.live_entities.toLocaleString();

        row.append(swatch, name, count);
        row.title = `${layer.id} · ${layer.state} · sources: ${layer.sources.join(", ")}`;
        row.addEventListener("click", () => {
          if (this.#enabled.has(layer.id)) this.#enabled.delete(layer.id);
          else this.#enabled.add(layer.id);
          renderer?.setLayerVisible(layer.id, this.#enabled.has(layer.id));
          this.#renderLayers();
          this.#onChange?.();
        });
        return row;
      }),
    );
  }
}

/** Wording that keeps a configured-off source from reading as a fault. */
function stateLabel(state: SourceState): string {
  switch (state) {
    case "key_required":
      return "no key";
    case "hardware_absent":
      return "no hw";
    case "unknown":
      return "waiting";
    default:
      return state;
  }
}
