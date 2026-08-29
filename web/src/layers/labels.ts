/**
 * Screen-space label de-confliction.
 *
 * Without this, three hundred aircraft over the London TMA render three hundred
 * callsigns on top of each other and the result is a grey smear that is worse
 * than no labels at all — it hides the contacts underneath it as well as being
 * unreadable itself.
 *
 * The approach is a uniform screen grid: at most one label per cell, and when
 * several compete the nearest to the camera wins. It is deliberately much less
 * than a real label arbiter — no leader lines, no displacement, no hysteresis
 * beyond the cell quantisation — because those are worth building against a
 * measured need rather than guessed at, and this removes the smear today.
 *
 * The one behaviour it does buy deliberately is stability. A label that flickers
 * as two contacts trade a cell is more distracting than either outcome, so a
 * label that currently holds a cell keeps it until it actually leaves, rather
 * than being re-decided from scratch on every frame.
 */

import {
  Cartesian2,
  SceneTransforms,
  type Entity as CesiumEntity,
  type Viewer,
} from "cesium";

/** Cell size in CSS pixels. Roughly one callsign wide by one line tall. */
const CELL_W = 108;
const CELL_H = 26;

/** Never draw more than this many labels, whatever the grid allows. */
const MAX_LABELS = 220;

export class LabelArbiter {
  /** Which entity currently owns each cell, so ownership is sticky. */
  #owner = new Map<string, string>();

  constructor(private readonly viewer: Viewer) {}

  /**
   * Decide which of `candidates` may show a label this frame.
   *
   * Called from `preRender`, so it runs once per actual render — which, under
   * `requestRenderMode`, means only when something moved.
   */
  arbitrate(candidates: Iterable<CesiumEntity>, selectedId: string | null): void {
    const scene = this.viewer.scene;
    const width = scene.canvas.clientWidth;
    const height = scene.canvas.clientHeight;
    if (width === 0 || height === 0) return;

    const cameraPos = this.viewer.camera.positionWC;
    type Candidate = {
      entity: CesiumEntity;
      cell: string;
      distance: number;
    };
    const visible: Candidate[] = [];

    const scratch = new Cartesian2();
    for (const entity of candidates) {
      const label = entity.label;
      if (!label) continue;
      const position = entity.position?.getValue(
        this.viewer.clock.currentTime,
      );
      if (!position) {
        continue;
      }
      const screen = SceneTransforms.worldToWindowCoordinates(
        scene,
        position,
        scratch,
      );
      // Off-screen, or behind the globe: `worldToWindowCoordinates` returns
      // undefined for a point the camera cannot see, and a point just outside
      // the canvas has no label to compete for.
      if (!screen) {
        label.show = false as never;
        continue;
      }
      if (
        screen.x < 0 ||
        screen.y < 0 ||
        screen.x > width ||
        screen.y > height
      ) {
        label.show = false as never;
        continue;
      }

      const cell = `${Math.floor(screen.x / CELL_W)},${Math.floor(screen.y / CELL_H)}`;
      const dx = position.x - cameraPos.x;
      const dy = position.y - cameraPos.y;
      const dz = position.z - cameraPos.z;
      visible.push({
        entity,
        cell,
        distance: dx * dx + dy * dy + dz * dz,
      });
      label.show = false as never;
    }

    // Nearest first, so the contact a viewer is most likely looking at wins its
    // cell. Selection overrides distance entirely.
    visible.sort((a, b) => {
      const aSel = a.entity.id === selectedId ? 0 : 1;
      const bSel = b.entity.id === selectedId ? 0 : 1;
      return aSel - bSel || a.distance - b.distance;
    });

    const taken = new Map<string, string>();
    let shown = 0;
    // First pass: whoever held a cell last frame and is still in it keeps it.
    // This is the whole of the anti-flicker behaviour.
    for (const candidate of visible) {
      if (shown >= MAX_LABELS) break;
      const id = String(candidate.entity.id);
      if (this.#owner.get(candidate.cell) === id) {
        taken.set(candidate.cell, id);
        candidate.entity.label!.show = true as never;
        shown++;
      }
    }
    for (const candidate of visible) {
      if (shown >= MAX_LABELS) break;
      if (taken.has(candidate.cell)) continue;
      taken.set(candidate.cell, String(candidate.entity.id));
      candidate.entity.label!.show = true as never;
      shown++;
    }

    this.#owner = taken;
  }
}
