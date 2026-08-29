/**
 * Spherical geometry the renderer needs and Cesium does not hand over directly.
 *
 * A sphere, not the WGS84 ellipsoid. The error is about 0.3% — a few hundred
 * metres over a 100 km radius — which is invisible for the two things this
 * serves: drawing a modelled circle whose radius is itself an estimate, and
 * advancing a contact between position reports. Anything needing better than
 * that wants a real geodesic solver, and should say so.
 */

/** Mean Earth radius (IUGG), metres. Matches `argus_core::geo`. */
export const EARTH_RADIUS_M = 6_371_008.8;

/**
 * Travel from a point along a bearing and return where you end up.
 *
 * Longitude is wrapped back into `-180..180`, which is the whole reason this
 * exists rather than a naive `lon + distance/scale`: a contact crossing the
 * antimeridian at 179.9° must come out at -179.9°, not 180.1°, or it flies the
 * long way round the planet on screen.
 */
export function destination(
  latDeg: number,
  lonDeg: number,
  bearingDeg: number,
  distanceM: number,
): [lon: number, lat: number] {
  const angular = distanceM / EARTH_RADIUS_M;
  const bearing = (bearingDeg * Math.PI) / 180;
  const lat1 = (latDeg * Math.PI) / 180;
  const lon1 = (lonDeg * Math.PI) / 180;

  const lat2 = Math.asin(
    Math.sin(lat1) * Math.cos(angular) +
      Math.cos(lat1) * Math.sin(angular) * Math.cos(bearing),
  );
  const lon2 =
    lon1 +
    Math.atan2(
      Math.sin(bearing) * Math.sin(angular) * Math.cos(lat1),
      Math.cos(angular) - Math.sin(lat1) * Math.sin(lat2),
    );

  return [normalizeLon((lon2 * 180) / Math.PI), (lat2 * 180) / Math.PI];
}

/** Wrap a longitude into `-180..180`. */
export function normalizeLon(lon: number): number {
  let wrapped = (lon + 180) % 360;
  if (wrapped < 0) wrapped += 360;
  return wrapped - 180;
}

/**
 * A closed ring of `[lon, lat]` pairs approximating a circle.
 *
 * Flat `[lon, lat, lon, lat, …]`, which is what `Cartesian3.fromDegreesArray`
 * wants. 72 segments — five degrees apart — is smooth at any zoom a circle of
 * this kind is legible at, and cheap enough to rebuild whenever the radius
 * changes.
 */
export function circleRing(
  latDeg: number,
  lonDeg: number,
  radiusM: number,
  segments = 72,
): number[] {
  const ring: number[] = [];
  for (let i = 0; i <= segments; i++) {
    const [lon, lat] = destination(
      latDeg,
      lonDeg,
      (360 * i) / segments,
      radiusM,
    );
    ring.push(lon, lat);
  }
  return ring;
}
