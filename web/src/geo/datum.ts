/**
 * Vertical datums: turning what a feed reports into a height Cesium can use.
 *
 * This is the single most-likely-to-be-wrong file in the client, and the
 * failure is quiet. An aircraft placed against the wrong reference surface
 * still looks like an aircraft; it is just buried under a hill, or floating a
 * hundred metres over the runway, and nothing in the render says so.
 *
 * The identity that governs everything here:
 *
 *     h = H + N
 *
 * `h` is ELLIPSOIDAL height — the distance from the WGS84 ellipsoid, and the
 * only thing `Cartesian3.fromDegrees` accepts. `H` is ORTHOMETRIC height,
 * "height above mean sea level", which is what almost every real feed reports.
 * `N` is the geoid undulation: the gap between the ellipsoid and the geoid at
 * that point, roughly -106 m to +85 m worldwide. Ignoring N is not a rounding
 * error — it is a hundred-metre error, and it is the reason coastal airports
 * sit at negative ellipsoidal height (a JFK ramp is about -30 m).
 *
 * The grid comes from `egm96-universal` (MIT), which embeds the NGA EGM96 15'
 * grid and returns N directly. It is a ~2.7 MB payload, so it is loaded by
 * dynamic import and never lands in the eager bundle — the globe is usable
 * before it arrives.
 *
 * What this file deliberately does NOT do is guess. Until the grid is loaded,
 * or for a datum that needs terrain this module cannot see, it returns a result
 * that says so, and the caller decides. A geoid-corrected height that is
 * silently uncorrected is worse than a height labelled unknown, because only
 * one of the two can be noticed.
 */

import type { AltitudeDatum } from "../net/types.ts";

/** A height, and how much it should be trusted. */
export interface ResolvedHeight {
  /** Metres above the WGS84 ellipsoid, ready for Cesium. */
  ellipsoidalM: number;
  /**
   * How the value was arrived at. Carried so the renderer can decline to
   * ground-clamp an approximate height, and so the entity card can say what it
   * is showing.
   */
  basis: HeightBasis;
}

export type HeightBasis =
  /** Reported against the ellipsoid already; nothing was assumed. */
  | "ellipsoidal"
  /** Orthometric height corrected by a real geoid lookup. */
  | "geoid_corrected"
  /**
   * Orthometric height used uncorrected because the grid is not loaded yet.
   * Off by up to ~100 m. Transient, and never cached as if it were final.
   */
  | "geoid_pending"
  /** Height above ground; needs terrain the caller must supply. */
  | "above_ground"
  /** No altitude reported at all. */
  | "unknown";

let grid: { meanSeaLevel(lat: number, lon: number): number } | null = null;
let loading: Promise<void> | null = null;

/**
 * Start loading the geoid grid. Safe to call repeatedly; the import happens
 * once.
 *
 * Failure is survivable and is treated as such: a client that cannot fetch a
 * 2.7 MB chunk should still show the map, with heights marked `geoid_pending`,
 * rather than refusing to start.
 */
export function loadGeoid(): Promise<void> {
  if (!loading) {
    loading = import("egm96-universal")
      .then((module) => {
        grid = module as unknown as {
          meanSeaLevel(lat: number, lon: number): number;
        };
      })
      .catch((error: unknown) => {
        console.warn(
          "argus: geoid grid unavailable; heights stay orthometric",
          error,
        );
        // Deliberately not rethrown, and `loading` is left resolved so this is
        // not retried on every single entity.
      });
  }
  return loading;
}

export function geoidReady(): boolean {
  return grid !== null;
}

/**
 * Geoid undulation N at a point, in metres, or `null` if the grid is not
 * loaded. Never guesses a value — zero is a real undulation somewhere, so
 * returning it as a stand-in for "unknown" would be indistinguishable from an
 * answer.
 */
export function undulationM(latDeg: number, lonDeg: number): number | null {
  if (!grid) return null;
  return grid.meanSeaLevel(latDeg, lonDeg);
}

/**
 * Resolve a reported altitude to an ellipsoidal height.
 *
 * `terrainM`, when the caller has sampled it, is the ellipsoidal height of the
 * ground beneath the point. It is only consulted for `above_ground`, which is
 * the one datum that cannot be resolved from a grid.
 */
export function resolveHeight(
  altM: number | null,
  datum: AltitudeDatum | null,
  latDeg: number,
  lonDeg: number,
  terrainM?: number | null,
): ResolvedHeight {
  if (altM === null || !Number.isFinite(altM)) {
    return { ellipsoidalM: 0, basis: "unknown" };
  }

  switch (datum) {
    case "wgs84_ellipsoid":
      return { ellipsoidalM: altM, basis: "ellipsoidal" };

    // Barometric altitude is pressure altitude against the 1013.25 hPa standard
    // datum, which is not mean sea level and not the ellipsoid either. Treating
    // it as orthometric is the right approximation: on a standard day they
    // agree, and on a high- or low-pressure day the residual error is tens of
    // metres, against the ~100 m error of ignoring the geoid entirely. The
    // honest fix is the QNH the feed sometimes carries, which belongs in the
    // driver, not here.
    case "geoid":
    case "barometric":
    case null: {
      const n = undulationM(latDeg, lonDeg);
      if (n === null) {
        return { ellipsoidalM: altM, basis: "geoid_pending" };
      }
      return { ellipsoidalM: altM + n, basis: "geoid_corrected" };
    }

    case "above_ground": {
      if (terrainM === null || terrainM === undefined) {
        return { ellipsoidalM: altM, basis: "above_ground" };
      }
      return { ellipsoidalM: terrainM + altM, basis: "above_ground" };
    }
  }
}

/**
 * Whether a height is solid enough to place an entity against terrain.
 *
 * The distinction matters at exactly one moment: a contact on the ground. An
 * approximate height for something at 10 km is invisible; the same error on a
 * parked aircraft either buries it under the apron or floats it above the
 * terminal roof.
 */
export function isTrustworthy(basis: HeightBasis): boolean {
  return basis === "ellipsoidal" || basis === "geoid_corrected";
}
