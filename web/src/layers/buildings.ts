/**
 * Global 3D buildings.
 *
 * The one thing Google's photorealistic tileset gives you that open data does
 * not is a *textured* mesh of the real world. Everything else it provides —
 * that a city reads as a city, that a contact on approach has something to be
 * on approach over — extruded footprints provide too, from data that is free,
 * needs no key, and can be mirrored onto this machine.
 *
 * The honesty rule that applies everywhere else in Argus applies here as well,
 * and matters more than usual. These heights are **estimated**, not surveyed:
 * the upstream service resolves a plausible height per building from its class
 * and footprint area where no real height is tagged. That is fine for context
 * and wrong for measurement, so the buildings are drawn as unlit, translucent
 * massing rather than as convincing architecture. A viewer should be able to
 * tell at a glance that this is a model of a city and not a photograph of one.
 *
 * Attribution is not optional — the data is ODbL, and the licence names three
 * parties. [`BUILDINGS_ATTRIBUTION`] is rendered whenever the tileset is on.
 */

import {
  Cesium3DTileset,
  Cesium3DTileStyle,
  type Scene,
  ShadowMode,
} from "cesium";

/** Required by ODbL. Shown in the HUD while the tileset is drawn. */
export const BUILDINGS_ATTRIBUTION =
  "Buildings © Re:Earth Buildings · OpenStreetMap contributors · Overture Maps Foundation (ODbL)";

export class Buildings {
  #tileset: Cesium3DTileset | null = null;
  #scene: Scene;
  #show = true;

  private constructor(scene: Scene) {
    this.#scene = scene;
  }

  /**
   * Load the tileset, or return a `Buildings` that draws nothing.
   *
   * A failure here is deliberately not fatal and deliberately not silent. The
   * service publishes no SLA and says so, so it going down must degrade the
   * client to "no buildings" rather than to "no map" — the same rule the
   * server applies to a feed that stops answering.
   */
  static async load(
    scene: Scene,
    url: string | null | undefined,
  ): Promise<{ buildings: Buildings; error: string | null }> {
    const buildings = new Buildings(scene);
    if (!url) return { buildings, error: null };
    try {
      const tileset = await Cesium3DTileset.fromUrl(url, {
        // Massing, not architecture. Skipping levels of detail keeps the
        // request count down on a client that is already streaming contacts.
        skipLevelOfDetail: true,
        baseScreenSpaceError: 1024,
        maximumScreenSpaceError: 24,
        // The globe already costs what it costs; buildings must not add
        // shadow passes to it.
        shadows: ShadowMode.DISABLED,
      });
      tileset.style = new Cesium3DTileStyle({
        color: "color('#8aa0b8', 0.55)",
      });
      scene.primitives.add(tileset);
      tileset.show = buildings.#show;
      buildings.#tileset = tileset;
      return { buildings, error: null };
    } catch (error: unknown) {
      return {
        buildings,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /** Whether a tileset actually loaded, as opposed to being switched off. */
  get available(): boolean {
    return this.#tileset !== null;
  }

  get shown(): boolean {
    return this.#show && this.available;
  }

  setShow(show: boolean): void {
    this.#show = show;
    if (this.#tileset) this.#tileset.show = show;
    // `requestRenderMode` means nothing redraws on its own, so a toggle that
    // does not ask for a frame appears to do nothing at all.
    this.#scene.requestRender();
  }
}
