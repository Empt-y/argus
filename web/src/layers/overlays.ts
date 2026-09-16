/**
 * Satellite imagery and other rasters draped over the globe.
 *
 * The server's `/v1/overlays` names the products and resolves the day; this
 * draws them. Each is a Cesium imagery layer that starts hidden, so an
 * overlay nobody has turned on costs nothing, and all of them follow the
 * DVR: rewinding re-dates every template, so last Tuesday's contacts sit on
 * last Tuesday's Earth rather than yesterday's.
 */

import {
  ImageryLayer,
  UrlTemplateImageryProvider,
  WebMercatorTilingScheme,
  type Viewer,
} from "cesium";
import type { Overlay } from "../net/types.ts";

export class Overlays {
  readonly #layers = new Map<string, ImageryLayer>();
  readonly #enabled = new Set<string>();
  #catalogue: Overlay[] = [];
  #date = "";

  constructor(private readonly viewer: Viewer) {}

  catalogue(): Overlay[] {
    return this.#catalogue;
  }

  date(): string {
    return this.#date;
  }

  /** Replace the catalogue — on first load, and whenever the DVR moves to a
   *  different day. Overlays that were on stay on, on the new day. */
  update(date: string, overlays: Overlay[]): void {
    if (date === this.#date && overlays.length === this.#catalogue.length) return;
    this.#date = date;
    this.#catalogue = overlays;
    for (const [id, layer] of this.#layers) {
      this.viewer.imageryLayers.remove(layer, true);
      this.#layers.delete(id);
    }
    for (const id of this.#enabled) this.#show(id);
    this.viewer.scene.requestRender();
  }

  isEnabled(id: string): boolean {
    return this.#enabled.has(id);
  }

  setEnabled(id: string, on: boolean): void {
    if (on) {
      this.#enabled.add(id);
      this.#show(id);
    } else {
      this.#enabled.delete(id);
      const layer = this.#layers.get(id);
      if (layer) {
        this.viewer.imageryLayers.remove(layer, true);
        this.#layers.delete(id);
      }
    }
    this.viewer.scene.requestRender();
  }

  #show(id: string): void {
    if (this.#layers.has(id)) return;
    const overlay = this.#catalogue.find((o) => o.id === id);
    if (!overlay) return;
    const provider = new UrlTemplateImageryProvider({
      url: overlay.tiles,
      tilingScheme: new WebMercatorTilingScheme(),
      minimumLevel: overlay.min_zoom,
      maximumLevel: overlay.max_zoom,
      tileWidth: overlay.tile_size,
      tileHeight: overlay.tile_size,
      credit: overlay.attribution.notice,
    });
    const layer = this.viewer.imageryLayers.addImageryProvider(provider);
    layer.alpha = overlay.opacity;
    // Imagery goes over the base map and under nothing else Cesium draws
    // as imagery; contacts are entities and always draw on top.
    this.#layers.set(id, layer);
  }
}
