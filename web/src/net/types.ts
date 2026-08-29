/**
 * The wire contract, mirrored from `argus-api`.
 *
 * Hand-written rather than generated, deliberately: it is small, it changes
 * rarely, and writing it out is what forces a reader of this client to notice
 * that `quality` exists and has to be honoured. A generated file would be
 * skimmed past.
 */

export type EntityKind =
  | "aircraft"
  | "vessel"
  | "satellite"
  | "event"
  | "station"
  | "feature"
  | "measure";

/**
 * How much to trust a position. Propagated everywhere from the driver that
 * produced it, and the client's contract is that it must never render
 * `modeled` or `estimated` the same way it renders `live`.
 */
export type Quality = "live" | "delayed" | "modeled" | "estimated" | "stale";

/** Which surface an altitude was measured against. See `geo/datum.ts`. */
export type AltitudeDatum =
  | "wgs84_ellipsoid"
  | "geoid"
  | "above_ground"
  | "barometric";

export type SourceState =
  | "live"
  | "delayed"
  | "stale"
  | "degraded"
  | "key_required"
  | "hardware_absent"
  | "unknown"
  | "failed";

export type GeometryClass = "point" | "line" | "area" | "mixed";

export interface LayerStyle {
  geometry: GeometryClass;
  color: string;
  min_zoom: number;
  max_zoom: number;
  rotates_with_course: boolean;
}

export interface Layer {
  id: string;
  display_name: string;
  kind: EntityKind;
  state: SourceState;
  sources: string[];
  last_success: string | null;
  observations: number;
  live_entities: number;
  attribution: unknown;
  style: LayerStyle;
  tileable: boolean;
}

export interface Entity {
  entity_kind: EntityKind;
  entity_key: string;
  source_id: string;
  layer_id: string;
  observed_at: string;
  lon: number | null;
  lat: number | null;
  /** GeoJSON, for entities whose shape is their meaning. */
  geom: GeoJsonGeometry | null;
  alt_m: number | null;
  alt_datum: AltitudeDatum | null;
  course_deg: number | null;
  heading_deg: number | null;
  speed_mps: number | null;
  vrate_mps: number | null;
  quality: Quality;
  label: string | null;
  attrs: Record<string, unknown>;
}

export type GeoJsonGeometry =
  | { type: "Point"; coordinates: [number, number] }
  | { type: "LineString"; coordinates: [number, number][] }
  | { type: "Polygon"; coordinates: [number, number][][] }
  | { type: "MultiPoint"; coordinates: [number, number][] }
  | { type: "MultiLineString"; coordinates: [number, number][][] }
  | { type: "MultiPolygon"; coordinates: [number, number][][][] }
  | { type: "GeometryCollection"; geometries: GeoJsonGeometry[] };

export interface EntitiesResponse {
  at: string;
  live: boolean;
  count: number;
  /** The server hit its limit. Render this; a truncated view that looks
   *  complete is how a map convinces someone a region is quiet. */
  truncated: boolean;
  entities: Entity[];
}

export interface Source {
  source_id: string;
  layer_id: string;
  display_name: string;
  entity_kind: EntityKind;
  cost_class: "free" | "metered" | "local";
  state: SourceState;
  state_since: string;
  last_success: string | null;
  last_error: string | null;
  last_lag_ms: number | null;
  observations: number;
  attribution: unknown;
}

export interface Health {
  status: string;
  version: string;
  started_at: string;
  uptime_seconds: number;
  loopback_exempt: boolean;
}

export interface ClientKeys {
  google_maps_api_key: string | null;
  cesium_ion_token: string | null;
  /** A 3D Tiles buildings tileset, or `null` to draw none. Not a credential. */
  buildings_tileset_url: string | null;
}

export interface TrackPoint {
  at: string;
  lon: number | null;
  lat: number | null;
  alt_m: number | null;
  alt_datum: AltitudeDatum | null;
  course_deg: number | null;
  speed_mps: number | null;
}

export interface TrackResponse {
  entity: string;
  kind: string;
  from: string;
  to: string;
  points: TrackPoint[];
}

/** Frames the server sends on `WS /v1/stream`. */
export type ServerFrame =
  | { type: "snapshot"; at: string; count: number; entities: Entity[] }
  | { type: "delta"; at: string; count: number; entities: Entity[] }
  | { type: "pong" }
  | { type: "error"; message: string };

/** A stable identity for an entity across polls and deltas. */
export function entityId(e: Pick<Entity, "entity_kind" | "entity_key">): string {
  return `${e.entity_kind}:${e.entity_key}`;
}
