/**
 * Terrain from this server, with a global fallback underneath it.
 *
 * Cesium's world terrain is a hosted service that needs an account. Argus
 * prefers its own: England publishes 1 m LiDAR under the Open Government
 * Licence, which is roughly thirty times finer than the global product, so over
 * the area this system actually watches, self-hosting is an upgrade. The daemon
 * serves that grid; see `crates/argus-api/src/routes/terrain.rs`.
 *
 * It is a *local* grid, though, and the rest of the planet still has to have
 * terrain. So this provider delegates: tiles wholly inside the survey are
 * answered from Argus, and every other tile is passed through to whatever
 * global provider was configured — ion when there is a token, the plain
 * ellipsoid when there is not. The seam that leaves at the coverage boundary is
 * a step of a metre or two between two honest sources, which is better than
 * either inventing heights beyond the survey or throwing away the good ones
 * inside it.
 *
 * **The datum conversion is the whole game.** The Environment Agency publishes
 * orthometric heights — above Ordnance Datum Newlyn, a geoid — and Cesium wants
 * heights above the WGS84 ellipsoid. In southern England those differ by about
 * 46 m. Serving the raw numbers would sink the entire landscape by the height
 * of a fifteen-storey building, so the geoid undulation is added here, reusing
 * the EGM96 model already verified against published values in `datum.ts`.
 * Until that model has loaded this provider refuses to answer and lets the
 * fallback take the tile — briefly coarse beats immediately wrong.
 */

import {
  HeightmapTerrainData,
  type Request,
  type TerrainData,
  type TerrainProvider,
} from "cesium";
import { geoidReady, undulationM } from "./datum.ts";

/** One prepared survey. */
export interface GridMeta {
  /** `[west, south, east, north]` degrees. */
  bounds: [number, number, number, number];
  /** `"orthometric"` or `"ellipsoidal"`. Never assume. */
  datum: string;
  attribution: string;
  ground_metres: number;
}

export interface TerrainMeta {
  available: boolean;
  /** Finest first. */
  grids: GridMeta[];
  /** Envelope of every grid — a quick reject, never a coverage test. */
  bounds: [number, number, number, number] | null;
  tile_size: number;
  min_level: number;
  max_level: number;
}

/**
 * The rectangle of a tile in Cesium's geographic tiling scheme, in degrees.
 * Mirrors `tile_rect` on the server; the two must agree exactly or tiles land
 * in the wrong place.
 */
function tileRect(
  z: number,
  x: number,
  y: number,
): [west: number, south: number, east: number, north: number] {
  const tilesX = 2 ** z * 2;
  const tilesY = 2 ** z;
  const lonSpan = 360 / tilesX;
  const latSpan = 180 / tilesY;
  const west = -180 + lonSpan * x;
  const north = 90 - latSpan * y;
  return [west, north - latSpan, west + lonSpan, north];
}

export class ArgusTerrainProvider {
  readonly #base: TerrainProvider;
  readonly #meta: TerrainMeta;
  readonly #baseUrl: string;
  readonly #token: string | null;
  /** Tiles answered from the local grid, for the HUD to report honestly. */
  #served = 0;
  #delegated = 0;

  constructor(
    base: TerrainProvider,
    meta: TerrainMeta,
    baseUrl: string,
    token: string | null,
  ) {
    if (!base) {
      // Wrapping an undefined provider fails later, inside Cesium, as an
      // unreadable property access on a getter. Say so here instead.
      throw new Error("ArgusTerrainProvider needs a base provider to fall back to");
    }
    this.#base = base;
    this.#meta = meta;
    this.#baseUrl = baseUrl.replace(/\/$/, "");
    this.#token = token;
  }

  get servedLocally(): number {
    return this.#served;
  }

  get delegated(): number {
    return this.#delegated;
  }

  // --- the parts that are simply the fallback's ----------------------------
  get tilingScheme() {
    return this.#base.tilingScheme;
  }
  get errorEvent() {
    return this.#base.errorEvent;
  }
  get credit() {
    return this.#base.credit;
  }
  get hasWaterMask() {
    return false;
  }
  get hasVertexNormals() {
    return false;
  }
  get availability() {
    return this.#base.availability;
  }
  getLevelMaximumGeometricError(level: number): number {
    return this.#base.getLevelMaximumGeometricError(level);
  }
  loadTileDataAvailability(x: number, y: number, level: number) {
    return this.#base.loadTileDataAvailability(x, y, level);
  }

  /**
   * Our tiles are always available, which is what lets the globe refine past
   * the depth the global provider would stop at. Anywhere else the fallback
   * decides, because it is the one that knows.
   */
  getTileDataAvailable(
    x: number,
    y: number,
    level: number,
  ): boolean | undefined {
    if (this.#isOurs(x, y, level)) return true;
    return this.#base.getTileDataAvailable(x, y, level);
  }

  requestTileGeometry(
    x: number,
    y: number,
    level: number,
    request?: Request,
  ): Promise<TerrainData> | undefined {
    if (!this.#isOurs(x, y, level)) {
      this.#delegated++;
      return this.#base.requestTileGeometry(x, y, level, request);
    }
    return this.#requestOurs(x, y, level, request);
  }

  /**
   * Whether some single grid wholly covers this tile.
   *
   * One grid, not the union of several: two surveys can between them enclose a
   * tile that neither covers alone, and answering that from a mosaic would mean
   * inventing the gap between them.
   */
  #gridFor(x: number, y: number, level: number): GridMeta | null {
    if (!this.#meta.available) return null;
    if (level < this.#meta.min_level || level > this.#meta.max_level) {
      return null;
    }
    const [west, south, east, north] = tileRect(level, x, y);
    return (
      this.#meta.grids.find(
        (g) =>
          west >= g.bounds[0] &&
          east <= g.bounds[2] &&
          south >= g.bounds[1] &&
          north <= g.bounds[3] &&
          // Without the geoid an orthometric grid would be 46 m out, which is
          // worse than being coarse. The model loads once, early, so this is a
          // brief window rather than a permanent refusal.
          (g.datum !== "orthometric" || geoidReady()),
      ) ?? null
    );
  }

  #isOurs(x: number, y: number, level: number): boolean {
    return this.#gridFor(x, y, level) !== null;
  }

  async #requestOurs(
    x: number,
    y: number,
    level: number,
    request?: Request,
  ): Promise<TerrainData> {
    try {
      const headers = new Headers();
      if (this.#token) headers.set("Authorization", `Bearer ${this.#token}`);
      const res = await fetch(`${this.#baseUrl}/v1/terrain/${level}/${x}/${y}`, {
        headers,
      });
      if (res.ok) {
        const heights = new Float32Array(await res.arrayBuffer());
        const size = this.#meta.tile_size;
        if (heights.length === size * size) {
          const grid = this.#gridFor(x, y, level);
          this.#toEllipsoidal(heights, level, x, y, size, grid);
          this.#served++;
          return new HeightmapTerrainData({
            buffer: heights,
            width: size,
            height: size,
            structure: {
              heightScale: 1,
              heightOffset: 0,
              elementsPerHeight: 1,
              stride: 1,
              elementMultiplier: 256,
              isBigEndian: false,
            },
          });
        }
      }
    } catch {
      /* fall through: a terrain tile is never worth failing the scene over */
    }
    this.#delegated++;
    const fallback = this.#base.requestTileGeometry(x, y, level, request);
    if (fallback) return fallback;
    throw new Error(`no terrain for ${level}/${x}/${y}`);
  }

  /**
   * Orthometric to ellipsoidal, in place.
   *
   * The undulation is taken at the tile's four corners and interpolated across
   * it rather than looked up per vertex. Over a tile this size the geoid is
   * smooth to a few centimetres, and this turns four thousand lookups into
   * four — which matters, because this runs for every tile the camera reaches.
   */
  #toEllipsoidal(
    heights: Float32Array,
    level: number,
    x: number,
    y: number,
    size: number,
    grid: GridMeta | null,
  ): void {
    if (grid?.datum !== "orthometric") return;
    const [west, south, east, north] = tileRect(level, x, y);
    const nw = undulationM(north, west);
    const ne = undulationM(north, east);
    const sw = undulationM(south, west);
    const se = undulationM(south, east);
    if (nw === null || ne === null || sw === null || se === null) return;

    for (let row = 0; row < size; row++) {
      const fy = row / (size - 1);
      const wEdge = nw + (sw - nw) * fy;
      const eEdge = ne + (se - ne) * fy;
      for (let col = 0; col < size; col++) {
        const fx = col / (size - 1);
        heights[row * size + col]! += wEdge + (eEdge - wEdge) * fx;
      }
    }
  }
}

/** Ask the server what it can serve. Never throws; absence is a valid answer. */
export async function fetchTerrainMeta(
  baseUrl: string,
  token: string | null,
): Promise<TerrainMeta | null> {
  try {
    const headers = new Headers();
    if (token) headers.set("Authorization", `Bearer ${token}`);
    const res = await fetch(`${baseUrl.replace(/\/$/, "")}/v1/terrain/meta`, {
      headers,
    });
    if (!res.ok) return null;
    return (await res.json()) as TerrainMeta;
  } catch {
    return null;
  }
}
