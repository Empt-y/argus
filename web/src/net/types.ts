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
  | "vehicle"
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
  /** The entity as a person reads it. Only the detail endpoint builds one;
   *  a row off the stream or a tile has none. */
  card?: Card;
}

/**
 * What the server says an entity looks like to a person: `attrs` decoded
 * into words and units by the layer's presenter, so a METAR reads as a
 * sentence here without this client knowing what a METAR is.
 */
export interface Card {
  title: string;
  subtitle?: string;
  summary?: string;
  sections: CardSection[];
  links?: CardLink[];
}

export interface CardSection {
  heading?: string;
  rows: CardRow[];
}

export interface CardRow {
  label: string;
  value: string;
  /** A quieter second line: the raw code behind a decoded word, a caveat. */
  note?: string;
}

export interface CardLink {
  label: string;
  url: string;
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
  /**
   * The failover chain this provider sits inside, or null if it stands on its
   * own. A chain and the provider currently serving it are otherwise
   * indistinguishable rows describing the same work.
   */
  member_of: string | null;
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

/**
 * A raster product a client can drape over the globe — satellite imagery,
 * night lights, sea temperature — from `/v1/overlays`. Nothing is stored
 * for these; the server resolves the day and the template, the client
 * fetches the tiles from NASA directly.
 */
export interface Overlay {
  id: string;
  name: string;
  description: string;
  /** XYZ template with `{z}`, `{x}`, `{y}`; the date is already in it. */
  tiles: string;
  date: string;
  min_zoom: number;
  max_zoom: number;
  tile_size: number;
  opacity: number;
  attribution: { provider: string; url: string; license: string; notice: string };
}

export interface OverlaysResponse {
  date: string;
  overlays: Overlay[];
}
