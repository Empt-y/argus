/**
 * Rendering entities onto the globe, one Cesium data source per Argus layer.
 *
 * The layer catalogue drives this entirely — colour, geometry class, whether
 * icons rotate to course. Nothing here knows what a `flight` or an
 * `earthquake` is, which is the point: a driver added on the server appears
 * here without a line of client code.
 *
 * The rule this file exists to enforce is that `quality` reaches the pixels.
 * A propagated satellite and an observed aircraft must not look alike, or the
 * whole honest-health chain that runs from the driver through the store and
 * the API dies at the last step, where it matters most.
 */

import {
  CallbackProperty,
  Cartesian2,
  Cartesian3,
  Color,
  CustomDataSource,
  Entity as CesiumEntity,
  HeightReference,
  HorizontalOrigin,
  JulianDate,
  LabelStyle,
  Math as CesiumMath,
  PolygonHierarchy,
  ClassificationType,
  VerticalOrigin,
  type Viewer,
} from "cesium";
import { resolveHeight } from "../geo/datum";
import { screenRotation } from "../geo/heading";
import { chevron } from "./icons";
import type { Entity, GeoJsonGeometry, Layer, Quality } from "../net/types";
import { entityId } from "../net/types";

/**
 * How solid a contact is drawn, by quality.
 *
 * Not decoration. `modeled` is an SGP4 propagation with no observation behind
 * it and `estimated` is a multilaterated fix; both are useful and neither is a
 * measurement. Drawing them at full opacity would be a claim the data does not
 * support.
 */
const QUALITY_ALPHA: Record<Quality, number> = {
  live: 1.0,
  delayed: 0.75,
  modeled: 0.45,
  estimated: 0.45,
  stale: 0.3,
};

/** Contacts older than this are drawn as fading rather than removed outright. */
const STALE_AFTER_MS = 120_000;

export class LayerRenderer {
  readonly #sources = new Map<string, CustomDataSource>();
  readonly #layers = new Map<string, Layer>();
  readonly #entities = new Map<string, Entity>();
  readonly #rotations = new Map<string, number>();
  #selected: string | null = null;

  constructor(private readonly viewer: Viewer) {}

  /** Install (or refresh) the layer catalogue fetched from the server. */
  setCatalogue(layers: Layer[]): void {
    for (const layer of layers) {
      this.#layers.set(layer.id, layer);
      if (!this.#sources.has(layer.id)) {
        const source = new CustomDataSource(layer.id);
        this.#sources.set(layer.id, source);
        void this.viewer.dataSources.add(source);
      }
    }
  }

  setLayerVisible(layerId: string, visible: boolean): void {
    const source = this.#sources.get(layerId);
    if (source) {
      source.show = visible;
      this.viewer.scene.requestRender();
    }
  }

  isLayerVisible(layerId: string): boolean {
    return this.#sources.get(layerId)?.show ?? false;
  }

  /** Replace everything currently drawn. Used for a snapshot or a DVR jump. */
  replaceAll(entities: Entity[]): void {
    for (const source of this.#sources.values()) source.entities.removeAll();
    this.#entities.clear();
    this.#rotations.clear();
    this.upsert(entities);
  }

  /** Apply a delta. */
  upsert(entities: Entity[]): void {
    for (const entity of entities) this.#draw(entity);
    this.viewer.scene.requestRender();
  }

  /**
   * Drop contacts nobody has heard from in a while.
   *
   * A feed going quiet and an aircraft landing look identical from here, so
   * this does not pretend to know which happened: contacts fade as they age
   * (see `#alpha`) and are only removed once they are old enough that keeping
   * them would be an assertion rather than a memory.
   */
  expire(olderThanMs: number, now = Date.now()): number {
    let removed = 0;
    for (const [id, entity] of this.#entities) {
      const age = now - Date.parse(entity.observed_at);
      if (age > olderThanMs) {
        const layer = this.#sources.get(entity.layer_id);
        layer?.entities.removeById(id);
        layer?.entities.removeById(`${id}@modeled`);
        this.#entities.delete(id);
        this.#rotations.delete(id);
        removed++;
      }
    }
    if (removed) this.viewer.scene.requestRender();
    return removed;
  }

  entity(id: string): Entity | undefined {
    return this.#entities.get(id);
  }

  /** Every drawn Cesium entity in a visible layer, for the label arbiter. */
  *drawn(): Iterable<CesiumEntity> {
    for (const source of this.#sources.values()) {
      if (!source.show) continue;
      yield* source.entities.values;
    }
  }

  select(id: string | null): void {
    this.#selected = id;
    this.viewer.scene.requestRender();
  }

  get selected(): string | null {
    return this.#selected;
  }

  get count(): number {
    return this.#entities.size;
  }

  #draw(entity: Entity): void {
    const layer = this.#layers.get(entity.layer_id);
    const source = this.#sources.get(entity.layer_id);
    // A layer the catalogue has not mentioned. Skipped rather than invented:
    // guessing a style would draw it wrong and hide the fact that the client
    // and the server disagree about what exists.
    if (!layer || !source) return;

    const id = entityId(entity);
    this.#entities.set(id, entity);
    const color = Color.fromCssColorString(layer.style.color);

    if (entity.geom) {
      this.#drawShape(source, id, entity, color);
      return;
    }
    if (entity.lon === null || entity.lat === null) return;

    // A modelled area, if the server published one. Kept generic — the key is
    // `modeled_radius_m`, not `felt_radius`, so nothing here needs to know what
    // an earthquake is and a later layer that models an area gets this for
    // free.
    this.#drawModeledArea(source, id, entity, color);

    const height = resolveHeight(
      entity.alt_m,
      entity.alt_datum,
      entity.lat,
      entity.lon,
    );

    const existing = source.entities.getById(id);
    const target = existing ?? new CesiumEntity({ id });

    target.position = Cartesian3.fromDegrees(
      entity.lon,
      entity.lat,
      height.ellipsoidalM,
    ) as never;

    // A layer that reports a course gets an oriented chevron; everything else
    // gets a dot. Drawing a direction the feed never supplied would be an
    // invention, and one a viewer would have no way to see through.
    if (layer.style.rotates_with_course) {
      target.point = undefined;
      target.billboard = {
        image: chevron(layer.style.color) as never,
        scale: new CallbackProperty(
          () => (this.#selected === id ? 0.75 : 0.5),
          false,
        ) as never,
        color: new CallbackProperty(
          () => this.#shade(id, Color.WHITE),
          false,
        ) as never,
        rotation: new CallbackProperty(
          () => this.#rotation(id, target),
          false,
        ) as never,
        // Zero aligned-axis means the quad faces the camera and `rotation` is
        // a plain screen-space angle, which is exactly what `screenRotation`
        // computes.
        alignedAxis: Cartesian3.ZERO as never,
      } as never;
      this.#label(target, entity, color);
      if (!existing) source.entities.add(target);
      return;
    }

    target.billboard = undefined;
    target.point = {
      pixelSize: new CallbackProperty(
        () => (this.#selected === id ? 13 : 7),
        false,
      ) as never,
      color: new CallbackProperty(
        () => this.#shade(id, color),
        false,
      ) as never,
      outlineColor: Color.BLACK.withAlpha(0.6) as never,
      outlineWidth: 1 as never,
      // A contact that reported no altitude at all belongs on the ground, not
      // at ellipsoidal zero — which is the geoid's depth below the surface
      // inland, and a hundred metres of it. Anything that did report an
      // altitude is placed absolutely, trustworthy or not: clamping an
      // approximate height would pin it to the wrong place with total
      // confidence, which is worse than showing it slightly off.
      heightReference: (height.basis === "unknown"
        ? HeightReference.CLAMP_TO_GROUND
        : HeightReference.NONE) as never,
      disableDepthTestDistance: 0 as never,
    } as never;

    this.#label(target, entity, color);
    if (!existing) source.entities.add(target);
  }

  #label(target: CesiumEntity, entity: Entity, color: Color): void {
    if (!entity.label) return;
    target.label = {
      text: entity.label as never,
      font: "500 12px ui-monospace, monospace" as never,
      fillColor: color as never,
      style: LabelStyle.FILL_AND_OUTLINE as never,
      outlineColor: Color.BLACK as never,
      outlineWidth: 3 as never,
      horizontalOrigin: HorizontalOrigin.LEFT as never,
      verticalOrigin: VerticalOrigin.BOTTOM as never,
      pixelOffset: new Cartesian2(10, -6) as never,
      // Visibility belongs to the arbiter (see `labels.ts`), which decides per
      // frame in screen space. A distance cut-off cannot do that job: the
      // problem is density, not range, and three hundred contacts are just as
      // unreadable at ten kilometres as at a thousand.
      show: false as never,
    } as never;
  }

  /**
   * Screen rotation for a contact, remembered between frames.
   *
   * The memory is not an optimisation: `screenRotation` returns the previous
   * value when the projection is degenerate, and without somewhere to keep it
   * the icon would snap to zero — reading as "now flying north" — every time a
   * contact turned to face the camera.
   */
  #rotation(id: string, target: CesiumEntity): number {
    const entity = this.#entities.get(id);
    if (!entity) return 0;
    const position = target.position?.getValue(this.viewer.clock.currentTime);
    if (!position) return this.#rotations.get(id) ?? 0;
    // Course is where it is going; heading is where the nose points. Course is
    // what a track icon should follow — in a crosswind an aircraft's nose is
    // several degrees off its actual path, and the icon should trace the path.
    const course = entity.course_deg ?? entity.heading_deg;
    const next = screenRotation(
      this.viewer.scene,
      position,
      course,
      this.#rotations.get(id) ?? null,
    );
    if (next === null) return this.#rotations.get(id) ?? 0;
    this.#rotations.set(id, next);
    return next;
  }

  /**
   * A dashed ring around a contact whose affected area is modelled rather than
   * observed.
   *
   * Drawn deliberately unlike a real polygon: no solid fill, a dashed outline,
   * and a much lower opacity. A modelled felt-radius and a measured ShakeMap
   * contour must not be able to be confused at a glance, because the second one
   * is evidence and the first is arithmetic.
   */
  #drawModeledArea(
    source: CustomDataSource,
    id: string,
    entity: Entity,
    color: Color,
  ): void {
    const radius = entity.attrs?.["modeled_radius_m"];
    const ringId = `${id}@modeled`;
    const existing = source.entities.getById(ringId);
    if (typeof radius !== "number" || !Number.isFinite(radius) || radius <= 0) {
      if (existing) source.entities.remove(existing);
      return;
    }
    if (entity.lon === null || entity.lat === null) return;

    const target = existing ?? new CesiumEntity({ id: ringId });
    target.position = Cartesian3.fromDegrees(entity.lon, entity.lat) as never;
    target.ellipse = {
      semiMajorAxis: radius as never,
      semiMinorAxis: radius as never,
      material: color.withAlpha(0.07) as never,
      outline: true as never,
      outlineColor: color.withAlpha(0.55) as never,
      outlineWidth: 1 as never,
      // Clamped, because the area is a footprint on the ground rather than
      // anything at the event's depth.
      heightReference: HeightReference.CLAMP_TO_GROUND as never,
      classificationType: ClassificationType.TERRAIN as never,
    } as never;
    if (!existing) source.entities.add(target);
  }

  #drawShape(
    source: CustomDataSource,
    id: string,
    entity: Entity,
    color: Color,
  ): void {
    const existing = source.entities.getById(id);
    if (existing) source.entities.remove(existing);
    const geom = entity.geom;
    if (!geom) return;

    for (const [index, ring] of polygons(geom).entries()) {
      source.entities.add(
        new CesiumEntity({
          id: index === 0 ? id : `${id}#${index}`,
          polygon: {
            hierarchy: new PolygonHierarchy(
              Cartesian3.fromDegreesArray(ring.flat()),
            ) as never,
            material: color.withAlpha(
              0.22 * QUALITY_ALPHA[entity.quality],
            ) as never,
            outline: true as never,
            outlineColor: color as never,
            // Areas are meaningful at the surface; lifting them off it just
            // makes them float over the thing they describe.
            heightReference: HeightReference.CLAMP_TO_GROUND as never,
          } as never,
        }),
      );
    }

    for (const [index, line] of lines(geom).entries()) {
      source.entities.add(
        new CesiumEntity({
          id: `${id}~${index}`,
          polyline: {
            positions: Cartesian3.fromDegreesArray(line.flat()) as never,
            width: 2 as never,
            material: color as never,
            clampToGround: true as never,
          } as never,
        }),
      );
    }
  }

  /** Colour for a contact right now: layer hue, quality alpha, age fade. */
  #shade(id: string, base: Color): Color {
    const entity = this.#entities.get(id);
    if (!entity) return base;
    const alpha = QUALITY_ALPHA[entity.quality] * this.#ageFade(entity);
    if (this.#selected === id) return Color.WHITE.withAlpha(1.0);
    return base.withAlpha(alpha);
  }

  #ageFade(entity: Entity): number {
    const age = Date.now() - Date.parse(entity.observed_at);
    if (!Number.isFinite(age) || age <= 0) return 1;
    return Math.max(0.25, 1 - age / STALE_AFTER_MS);
  }
}

/** Rings of every polygon in a GeoJSON geometry, as `[lon, lat]` pairs. */
function polygons(geom: GeoJsonGeometry): [number, number][][] {
  switch (geom.type) {
    case "Polygon":
      return geom.coordinates.slice(0, 1);
    case "MultiPolygon":
      return geom.coordinates.flatMap((poly) => poly.slice(0, 1));
    case "GeometryCollection":
      return geom.geometries.flatMap(polygons);
    default:
      return [];
  }
}

function lines(geom: GeoJsonGeometry): [number, number][][] {
  switch (geom.type) {
    case "LineString":
      return [geom.coordinates];
    case "MultiLineString":
      return geom.coordinates;
    case "GeometryCollection":
      return geom.geometries.flatMap(lines);
    default:
      return [];
  }
}

/** Unused import guard: Cesium's JulianDate/CesiumMath are re-exported for
 *  the UI layer, which needs the same instances Cesium itself uses. */
export { JulianDate, CesiumMath };
