/**
 * Advancing a contact between position reports.
 *
 * A feed reports an aircraft every fifteen or twenty seconds. Drawn literally,
 * it teleports: it sits still, jumps five kilometres, sits still again. Dead
 * reckoning fills the gap by carrying the contact forward along the course and
 * speed it last reported, which is what makes a map of moving things read as
 * movement rather than as a slideshow.
 *
 * It is also, unavoidably, making a position up. The rules that keep that
 * honest:
 *
 *   * **It never becomes data.** Reckoning happens at render time from the last
 *     real fix. Nothing computed here is stored, sent, or written back over the
 *     observation it came from, so the DVR still replays what was actually
 *     reported and not what was drawn.
 *   * **It stops.** Extrapolation is only defensible for about as long as the
 *     gap it is bridging. An aircraft that stopped reporting three minutes ago
 *     has very possibly turned, and carrying it on in a straight line invents a
 *     track it never flew. Past [`MAX_COAST_MS`] the contact holds its last
 *     known position instead — visibly stale, which is true, rather than
 *     confidently wrong.
 *   * **It is visible.** [`reckonedFor`] reports how long a contact has been
 *     coasting so the renderer can fade it and the entity card can say so.
 *
 * A great-circle advance on a sphere, not a rhumb line and not an ellipsoid.
 * Over the tens of kilometres this ever spans, the difference is metres.
 */

import { destination } from "./spherical.ts";
import type { Entity } from "../net/types.ts";

/**
 * How far past its last fix a contact may be carried.
 *
 * A little over three typical ADS-B update intervals: long enough to ride out a
 * missed poll or a provider failover, short enough that a contact which has
 * genuinely gone quiet stops moving before the invented track gets long enough
 * to mislead.
 */
export const MAX_COAST_MS = 60_000;

/** Below this there is nothing to advance and no point pretending otherwise. */
const MIN_SPEED_MPS = 0.5;

export interface Reckoned {
  lon: number;
  lat: number;
  /** Metres, in whatever datum the original fix used. */
  altM: number | null;
  /** Milliseconds of extrapolation behind this position. Zero means the fix. */
  coastedMs: number;
}

/**
 * Whether this contact is one dead reckoning can help at all.
 *
 * A weather alert has no course; a parked aircraft has no speed; a station
 * never moves. All three should be drawn exactly where they were reported.
 */
export function isMovable(entity: Entity): boolean {
  return (
    entity.lon !== null &&
    entity.lat !== null &&
    entity.speed_mps !== null &&
    entity.speed_mps > MIN_SPEED_MPS &&
    (entity.course_deg ?? entity.heading_deg) !== null
  );
}

/**
 * Where a contact should be drawn now.
 *
 * Returns `null` when there is nothing to advance, so the caller can use the
 * reported position unchanged rather than being handed a copy of it.
 */
export function reckon(entity: Entity, nowMs = Date.now()): Reckoned | null {
  if (!isMovable(entity)) return null;
  const lon = entity.lon;
  const lat = entity.lat;
  const speed = entity.speed_mps;
  if (lon === null || lat === null || speed === null) return null;

  const course = entity.course_deg ?? entity.heading_deg;
  if (course === null) return null;

  const fixedAt = Date.parse(entity.observed_at);
  if (!Number.isFinite(fixedAt)) return null;

  // A fix from the future is a clock disagreement, not a reason to reckon
  // backwards, which would draw the contact behind where it was reported.
  const elapsed = Math.min(Math.max(nowMs - fixedAt, 0), MAX_COAST_MS);
  if (elapsed === 0) return null;

  const [newLon, newLat] = destination(lat, lon, course, speed * (elapsed / 1000));

  // Climb and descent are carried too where the feed reports them. An aircraft
  // on approach that holds its cruise altitude between fixes and then drops
  // 300 m in one step is the same slideshow problem in the vertical.
  const altM =
    entity.alt_m === null
      ? null
      : entity.alt_m + (entity.vrate_mps ?? 0) * (elapsed / 1000);

  return { lon: newLon, lat: newLat, altM, coastedMs: elapsed };
}

/**
 * How long a contact has been coasting, in milliseconds, capped at
 * [`MAX_COAST_MS`]. Zero for anything being drawn where it was reported.
 */
export function reckonedFor(entity: Entity, nowMs = Date.now()): number {
  if (!isMovable(entity)) return 0;
  const fixedAt = Date.parse(entity.observed_at);
  if (!Number.isFinite(fixedAt)) return 0;
  return Math.min(Math.max(nowMs - fixedAt, 0), MAX_COAST_MS);
}
