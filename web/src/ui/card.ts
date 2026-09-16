/**
 * The entity card: everything Argus knows about one contact.
 *
 * Two things it must get right and neither is cosmetic.
 *
 * The first is provenance. A card that shows a position without saying which
 * feed produced it, how old it is and how much to trust it turns a carefully
 * honest pipeline into a confident-looking number. Source, quality and age are
 * therefore not an expandable detail — they are the header.
 *
 * The second is that a track is history, not a projection. The polyline drawn
 * here comes from `tracks_1m` and stops where the record stops; nothing is
 * extrapolated forward, because a dead-reckoned continuation drawn in the same
 * style as a recorded one is a lie about what was observed.
 *
 * What the entity *is* — the decoded METAR, the bus's destination, the
 * squawk's meaning — comes from the server as a `card` on the detail
 * response, built by the layer's presenter. This file lays that out and does
 * not interpret attributes itself, so a new layer reads properly here the day
 * its driver lands. The raw attributes stay behind a fold for anyone who
 * wants to see what the feed actually said.
 */

import {
  Cartesian3,
  Color,
  CustomDataSource,
  Entity as CesiumEntity,
  type Viewer,
} from "cesium";
import { api } from "../net/client.ts";
import { resolveHeight } from "../geo/datum.ts";
import { reckonedFor } from "../geo/reckon.ts";
import type { Card, Entity, Quality, TrackPoint } from "../net/types.ts";

/** How much history to draw behind a selected contact. */
const TRACK_HOURS = 2;

export class EntityCard {
  readonly #root: HTMLElement;
  readonly #track: CustomDataSource;
  #open: string | null = null;

  constructor(
    private readonly viewer: Viewer,
    parent: HTMLElement,
  ) {
    this.#root = document.createElement("div");
    this.#root.className = "panel card";
    this.#root.hidden = true;
    parent.append(this.#root);

    this.#track = new CustomDataSource("selection-track");
    void viewer.dataSources.add(this.#track);
  }

  close(): void {
    this.#open = null;
    this.#root.hidden = true;
    this.#track.entities.removeAll();
    this.viewer.scene.requestRender();
  }

  async show(entity: Entity): Promise<void> {
    const id = `${entity.entity_kind}:${entity.entity_key}`;
    this.#open = id;
    this.#root.hidden = false;
    this.#render(entity, null);

    // The card is useful immediately from what the stream already gave us; the
    // detail fetch only adds `attrs`, and the track only adds history. Neither
    // is worth an empty panel while it loads.
    try {
      const [detail, track] = await Promise.all([
        api.entity(entity.entity_kind, entity.entity_key),
        api
          .track(
            entity.entity_kind,
            entity.entity_key,
            new Date(Date.now() - TRACK_HOURS * 3600_000),
          )
          .catch(() => null),
      ]);
      // The selection may have moved on while those were in flight.
      if (this.#open !== id) return;
      this.#render(detail, track?.points ?? null);
      this.#drawTrack(track?.points ?? [], detail);
    } catch {
      /* the stream's own copy is already on screen */
    }
  }

  #render(entity: Entity, track: TrackPoint[] | null): void {
    const observed = Date.parse(entity.observed_at);
    const ageS = Math.max(0, Math.round((Date.now() - observed) / 1000));
    const height =
      entity.lat !== null && entity.lon !== null
        ? resolveHeight(entity.alt_m, entity.alt_datum, entity.lat, entity.lon)
        : null;

    const rows: [string, string][] = [];
    if (entity.lat !== null && entity.lon !== null) {
      // The reported fix, always — not the reckoned one. The card is the place
      // someone goes to find out what is actually known, and a dead-reckoned
      // number presented as a position would defeat the point of showing it.
      const coasted = reckonedFor(entity);
      rows.push([
        "position",
        `${entity.lat.toFixed(4)}, ${entity.lon.toFixed(4)}` +
          (coasted > 1000
            ? ` (drawn +${Math.round(coasted / 1000)}s dead-reckoned)`
            : ""),
      ]);
    }
    if (entity.alt_m !== null) {
      // Show the conversion only when there was one. A feed already reporting
      // ellipsoidal height converts to itself, and "2690 m -> 2690 m" is noise
      // that trains the eye to skip the row — including on the rows where the
      // geoid moved the number by a hundred metres, which is the whole reason
      // it is displayed.
      const reported = `${Math.round(entity.alt_m)} m ${entity.alt_datum ?? "datum unknown"}`;
      const converted =
        height &&
        height.basis !== "unknown" &&
        Math.round(height.ellipsoidalM) !== Math.round(entity.alt_m)
          ? ` → ${Math.round(height.ellipsoidalM)} m ellipsoidal (${height.basis.replace(/_/g, " ")})`
          : "";
      rows.push(["altitude", reported + converted]);
    }
    if (entity.speed_mps !== null) {
      rows.push([
        "speed",
        `${Math.round(entity.speed_mps)} m/s · ${Math.round(entity.speed_mps * 1.94384)} kt`,
      ]);
    }
    if (entity.course_deg !== null) rows.push(["course", `${entity.course_deg.toFixed(0)}°`]);
    if (entity.heading_deg !== null) rows.push(["heading", `${entity.heading_deg.toFixed(0)}°`]);
    if (entity.vrate_mps !== null) {
      rows.push(["vertical", `${entity.vrate_mps > 0 ? "+" : ""}${entity.vrate_mps.toFixed(1)} m/s`]);
    }
    const radius = entity.attrs?.["modeled_radius_m"];
    if (typeof radius === "number" && Number.isFinite(radius)) {
      // Named as modelled in the card as well as drawn as modelled on the
      // globe. Someone reading a number off a panel should not have to
      // remember which rings were arithmetic.
      const kind = String(entity.attrs?.["modeled_radius_kind"] ?? "area")
        .replace(/_/g, " ");
      rows.push(["modelled", `${kind} ~${Math.round(radius / 1000)} km radius (estimated)`]);
    }
    if (track) {
      rows.push([
        "track",
        track.length
          ? `${track.length} points over ${TRACK_HOURS}h`
          : `no history yet (the rollup materialises on a timer)`,
      ]);
    }

    const card = entity.card;
    const attrKeys = Object.keys(entity.attrs ?? {});
    this.#root.innerHTML = `
      <button class="card-close" title="close">×</button>
      <h2>${escape(card?.title ?? entity.label ?? entity.entity_key)}</h2>
      <div class="card-head">
        <span class="chip ${qualityClass(entity.quality)}">${entity.quality}</span>
        <span>${escape(card?.subtitle ?? entity.layer_id)}</span>
        <span class="count">via ${escape(entity.source_id)} · ${formatAge(ageS)}</span>
      </div>
      ${card?.summary ? `<p class="card-summary">${escape(card.summary)}</p>` : ""}
      <dl class="card-rows">
        ${rows
          .map(
            ([k, v]) =>
              `<dt>${escape(k)}</dt><dd>${escape(v)}</dd>`,
          )
          .join("")}
      </dl>
      ${card ? renderSections(card) : ""}
      ${card?.links?.length ? renderLinks(card) : ""}
      ${
        attrKeys.length
          ? `<details class="card-raw"><summary>raw attributes (${attrKeys.length})</summary>
             <dl class="card-rows">${attrKeys
               .sort()
               .map(
                 (k) =>
                   `<dt>${escape(k)}</dt><dd>${escape(rawValue(entity.attrs[k]))}</dd>`,
               )
               .join("")}</dl></details>`
          : ""
      }
    `;
    this.#root
      .querySelector(".card-close")
      ?.addEventListener("click", () => this.close());
  }

  #drawTrack(points: TrackPoint[], entity: Entity): void {
    this.#track.entities.removeAll();
    const positions: Cartesian3[] = [];
    for (const point of points) {
      if (point.lon === null || point.lat === null) continue;
      const height = resolveHeight(
        point.alt_m,
        point.alt_datum,
        point.lat,
        point.lon,
      );
      positions.push(
        Cartesian3.fromDegrees(point.lon, point.lat, height.ellipsoidalM),
      );
    }
    // A single point is not a line, and Cesium throws rather than ignoring it.
    if (positions.length < 2) {
      this.viewer.scene.requestRender();
      return;
    }
    this.#track.entities.add(
      new CesiumEntity({
        id: `track:${entity.entity_kind}:${entity.entity_key}`,
        polyline: {
          positions: positions as never,
          width: 2 as never,
          material: Color.WHITE.withAlpha(0.8) as never,
          // Not clamped: the track is a flight path through the air, and
          // draping it on the terrain would draw a ground track that the
          // aircraft was never on.
          clampToGround: false as never,
          arcType: undefined as never,
        } as never,
      }),
    );
    this.viewer.scene.requestRender();
  }
}

function renderSections(card: Card): string {
  return card.sections
    .map(
      (section) => `
        <section class="card-section">
          ${section.heading ? `<h3>${escape(section.heading)}</h3>` : ""}
          <dl class="card-rows">
            ${section.rows
              .map(
                (row) =>
                  `<dt>${escape(row.label)}</dt><dd>${escape(row.value)}${
                    row.note ? `<span class="card-note">${escape(row.note)}</span>` : ""
                  }</dd>`,
              )
              .join("")}
          </dl>
        </section>`,
    )
    .join("");
}

function renderLinks(card: Card): string {
  // Only http(s) targets become anchors: the server builds these from feed
  // fields, and a feed field is not a place to accept a `javascript:` URL.
  const links = (card.links ?? []).filter((l) => /^https?:\/\//i.test(l.url));
  if (!links.length) return "";
  return `<p class="card-links">${links
    .map(
      (l) =>
        `<a href="${escape(l.url)}" target="_blank" rel="noopener noreferrer">${escape(l.label)} ↗</a>`,
    )
    .join(" · ")}</p>`;
}

function rawValue(value: unknown): string {
  if (value === null || value === undefined) return "—";
  if (typeof value === "string") return value;
  return JSON.stringify(value);
}

function qualityClass(quality: Quality): string {
  switch (quality) {
    case "live":
      return "live";
    case "delayed":
      return "delayed";
    case "stale":
      return "stale";
    // Modeled and estimated are neither healthy nor broken — they are honest
    // about being computed rather than observed, and get the neutral chip.
    default:
      return "unknown";
  }
}

function formatAge(seconds: number): string {
  if (seconds < 90) return `${seconds}s ago`;
  if (seconds < 5400) return `${Math.round(seconds / 60)}m ago`;
  return `${Math.round(seconds / 3600)}h ago`;
}

function escape(text: string): string {
  return text.replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ]!,
  );
}
