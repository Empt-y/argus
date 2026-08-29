/**
 * Pure geometry, tested without a browser.
 *
 * Node runs TypeScript directly, so `node --test` covers the modules that are
 * just maths. Everything that needs a GL context or a Cesium camera is verified
 * against headless chromium instead — the two are complementary, and this is
 * the half that should fail in a second rather than in twenty.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import { destination, normalizeLon, circleRing, EARTH_RADIUS_M } from "./spherical.ts";
import { reckon, reckonedFor, isMovable, MAX_COAST_MS } from "./reckon.ts";
import type { Entity } from "../net/types.ts";

const R = EARTH_RADIUS_M;
const rad = (d: number) => (d * Math.PI) / 180;

function haversineM(lat1: number, lon1: number, lat2: number, lon2: number): number {
  const dLat = rad(lat2 - lat1);
  const dLon = rad(lon2 - lon1);
  const a =
    Math.sin(dLat / 2) ** 2 +
    Math.cos(rad(lat1)) * Math.cos(rad(lat2)) * Math.sin(dLon / 2) ** 2;
  return 2 * R * Math.asin(Math.sqrt(a));
}

test("travelling a known distance arrives that far away", () => {
  const [lon, lat] = destination(51.5, -0.1, 90, 10_000);
  assert.ok(Math.abs(haversineM(51.5, -0.1, lat, lon) - 10_000) < 1);
});

test("due north and due south change only latitude", () => {
  const [lonN, latN] = destination(0, 0, 0, 111_195);
  assert.ok(Math.abs(lonN) < 1e-9, `expected no longitude change, got ${lonN}`);
  assert.ok(Math.abs(latN - 1) < 0.01, `expected ~1 degree north, got ${latN}`);
  const [, latS] = destination(0, 0, 180, 111_195);
  assert.ok(Math.abs(latS + 1) < 0.01);
});

test("crossing the antimeridian wraps instead of running off the map", () => {
  // The bug this prevents: a contact at 179.9 heading east comes out at
  // -179.9, not 180.1, or it visibly flies the long way round the planet.
  const [lon] = destination(0, 179.9, 90, 50_000);
  assert.ok(lon >= -180 && lon <= 180, `longitude escaped range: ${lon}`);
  assert.ok(lon < 0, `expected a wrap to negative, got ${lon}`);
});

test("longitudes normalise in both directions", () => {
  assert.ok(Math.abs(normalizeLon(190) + 170) < 1e-9);
  assert.ok(Math.abs(normalizeLon(-190) - 170) < 1e-9);
  assert.equal(normalizeLon(0), 0);
});

test("a circle ring closes and sits at the requested radius", () => {
  const ring = circleRing(51.5, -0.1, 25_000, 36);
  assert.equal(ring.length, (36 + 1) * 2, "flat lon,lat pairs, closed");
  assert.ok(Math.abs(ring[0]! - ring[ring.length - 2]!) < 1e-9, "closes in longitude");
  assert.ok(Math.abs(ring[1]! - ring[ring.length - 1]!) < 1e-9, "closes in latitude");
  for (let i = 0; i < ring.length; i += 2) {
    const d = haversineM(51.5, -0.1, ring[i + 1]!, ring[i]!);
    assert.ok(Math.abs(d - 25_000) < 5, `vertex ${i / 2} was ${d.toFixed(0)} m out`);
  }
});

// --- dead reckoning --------------------------------------------------------

function aircraft(overrides: Partial<Entity> = {}): Entity {
  return {
    entity_kind: "aircraft",
    entity_key: "abc123",
    source_id: "adsb-lol",
    layer_id: "flights",
    observed_at: new Date().toISOString(),
    lon: -0.4,
    lat: 51.5,
    geom: null,
    alt_m: 10_000,
    alt_datum: "wgs84_ellipsoid",
    course_deg: 90,
    heading_deg: null,
    speed_mps: 200,
    vrate_mps: null,
    quality: "live",
    label: "TEST01",
    attrs: {},
    ...overrides,
  };
}

test("a contact advances along its course at its reported speed", () => {
  const now = Date.now();
  const e = aircraft({ observed_at: new Date(now - 10_000).toISOString() });
  const r = reckon(e, now);
  assert.ok(r, "expected a reckoned position");
  const moved = haversineM(e.lat!, e.lon!, r.lat, r.lon);
  assert.ok(Math.abs(moved - 2000) < 1, `expected 2000 m, got ${moved.toFixed(1)}`);
  assert.ok(r.lon > e.lon!, "heading 090 must increase longitude");
});

test("extrapolation stops rather than running forever", () => {
  // An aircraft that stopped reporting ten minutes ago has very possibly
  // turned; carrying it on in a straight line invents a track it never flew.
  const now = Date.now();
  const e = aircraft({ observed_at: new Date(now - 600_000).toISOString() });
  const r = reckon(e, now)!;
  const moved = haversineM(e.lat!, e.lon!, r.lat, r.lon);
  assert.equal(r.coastedMs, MAX_COAST_MS);
  assert.ok(
    Math.abs(moved - 200 * (MAX_COAST_MS / 1000)) < 1,
    `capped travel expected, got ${moved.toFixed(0)} m`,
  );
});

test("a fix from the future is not reckoned backwards", () => {
  // Clock disagreement between us and a provider is real — one of them was an
  // hour off. Drawing a contact behind where it was reported is never right.
  const now = Date.now();
  const e = aircraft({ observed_at: new Date(now + 30_000).toISOString() });
  assert.equal(reckon(e, now), null);
  assert.equal(reckonedFor(e, now), 0);
});

test("things that do not move are left exactly where they were reported", () => {
  assert.equal(isMovable(aircraft({ speed_mps: 0 })), false);
  assert.equal(isMovable(aircraft({ speed_mps: null })), false);
  assert.equal(isMovable(aircraft({ course_deg: null, heading_deg: null })), false);
  assert.equal(isMovable(aircraft({ lon: null })), false);
  assert.equal(isMovable(aircraft()), true);
  // Heading stands in when no course is reported.
  assert.equal(isMovable(aircraft({ course_deg: null, heading_deg: 270 })), true);
});

test("a climb is carried into the altitude", () => {
  const now = Date.now();
  const e = aircraft({
    observed_at: new Date(now - 20_000).toISOString(),
    vrate_mps: 5,
  });
  const r = reckon(e, now)!;
  assert.equal(r.altM, 10_000 + 100);
});

test("a coasting contact stops counting as moving once it is pinned", () => {
  // What decides whether the render loop keeps running. A movable contact
  // whose fix has aged past the cap is drawn at a constant position, so
  // animating for it burns GPU over a map where nothing can move.
  const now = Date.now();
  const moving = aircraft({ observed_at: new Date(now - 10_000).toISOString() });
  const pinned = aircraft({ observed_at: new Date(now - 600_000).toISOString() });

  const animates = (e: Entity) => {
    const coasted = reckonedFor(e, now);
    return isMovable(e) && coasted > 0 && coasted < MAX_COAST_MS;
  };

  assert.equal(animates(moving), true);
  assert.equal(animates(pinned), false, "a pinned contact must not drive frames");
  assert.equal(reckonedFor(pinned, now), MAX_COAST_MS);
});
