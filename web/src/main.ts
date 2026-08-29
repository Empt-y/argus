/**
 * Entry point: bring up the globe, then keep it fed.
 *
 * The order matters. The map is drawable before the geoid grid, the layer
 * catalogue or the first entity arrives, and each of those improves it when it
 * lands. A client that waits for everything before showing anything looks
 * broken on a slow link and is indistinguishable from one that is.
 */

import { api, ApiError } from "./net/client";
import { DeltaStream } from "./net/stream";
import { createViewer, onCameraSettled, viewportBbox } from "./core/viewer";
import { loadGeoid, geoidReady, undulationM, resolveHeight } from "./geo/datum";
import { LayerRenderer } from "./layers/entities";
import { LabelArbiter } from "./layers/labels";
import type { ClientKeys, Entity, Layer } from "./net/types";
import { Hud } from "./ui/hud";
import { EntityCard } from "./ui/card";
import { SensorStyles } from "./styles/sensors";
import { ScreenSpaceEventHandler, ScreenSpaceEventType } from "cesium";

/** Contacts unheard-of for this long stop being drawn at all. */
const EXPIRY_MS = 10 * 60_000;
/** How often to sweep for them. */
const EXPIRY_SWEEP_MS = 30_000;
/** Camera settle delay before re-querying the viewport. */
const CAMERA_SETTLE_MS = 350;

async function main(): Promise<void> {
  const container = document.getElementById("globe");
  const hudRoot = document.getElementById("hud");
  if (!container || !hudRoot) throw new Error("missing mount points");

  const hud = new Hud(hudRoot);

  // The grid is 2.7 MB and nothing waits for it; heights are marked
  // `geoid_pending` until it lands and correct afterwards.
  void loadGeoid();

  let keys: ClientKeys | null = null;
  try {
    keys = await api.clientKeys();
  } catch (error) {
    // Not fatal, and not even unusual: an unpaired client gets a 401 here, and
    // the keyless globe is the normal case.
    if (!(error instanceof ApiError)) throw error;
    hud.note(`client keys unavailable (${error.code})`);
  }

  const { viewer, photoreal, terrain } = createViewer(container, keys);
  const renderer = new LayerRenderer(viewer);
  const labels = new LabelArbiter(viewer);
  hud.setPhotoreal(photoreal);
  if (!terrain) hud.note("flat ellipsoid — no Cesium ion token configured");
  const sensors = new SensorStyles(viewer);
  hud.setSensorStyles(sensors);

  // Labels are decided in screen space, so they can only be decided once the
  // camera is where it is going to be for this frame. `preRender` fires once
  // per actual render, which under `requestRenderMode` means only when
  // something moved.
  viewer.scene.preRender.addEventListener(() => {
    labels.arbitrate(renderer.drawn(), renderer.selected);
  });

  let layers: Layer[] = [];
  try {
    layers = await api.layers();
    renderer.setCatalogue(layers);
    hud.setLayers(layers, renderer);
  } catch (error) {
    hud.fail(
      error instanceof ApiError
        ? `cannot reach argusd: ${error.message}`
        : String(error),
    );
    return;
  }

  const stream = new DeltaStream({
    onSnapshot: (entities: Entity[]) => {
      renderer.replaceAll(entities);
      hud.setCount(renderer.count);
    },
    onDelta: (entities: Entity[]) => {
      renderer.upsert(entities);
      hud.setCount(renderer.count);
    },
    onStatus: (status, detail) => hud.setStreamStatus(status, detail),
  });

  const resubscribe = () => {
    const bbox = viewportBbox(viewer);
    // No rectangle means the camera is out past the limb. Subscribing globally
    // then is right rather than lazy: that view really is looking at the whole
    // planet, and the server's own limit is what bounds the answer.
    stream.subscribe({
      bbox: bbox ?? undefined,
      layers: hud.enabledLayers(),
      at: hud.dvrInstant(),
    });
  };

  // Click to inspect. `scene.pick` returns the drawn primitive; its entity id
  // is the same `kind:key` the renderer stored it under, so the lookup needs no
  // second index.
  const card = new EntityCard(viewer, hudRoot);
  const picker = new ScreenSpaceEventHandler(viewer.canvas);
  picker.setInputAction((movement: { position: unknown }) => {
    const picked = viewer.scene.pick(movement.position as never);
    const id = picked?.id?.id ?? picked?.id;
    if (typeof id !== "string") {
      renderer.select(null);
      card.close();
      return;
    }
    // Shapes, tracks and modelled rings carry decorated ids (`id#0`, `id~0`,
    // `id@modeled`); the contact they belong to is the part before it.
    const base = id.split(/[#~@%]/)[0] ?? id;
    const entity = renderer.entity(base);
    if (!entity) {
      renderer.select(null);
      card.close();
      return;
    }
    renderer.select(base);
    void card.show(entity);
  }, ScreenSpaceEventType.LEFT_CLICK);

  hud.onSelectionChange(resubscribe);
  onCameraSettled(viewer, CAMERA_SETTLE_MS, resubscribe);
  resubscribe();

  setInterval(() => renderer.expire(EXPIRY_MS), EXPIRY_SWEEP_MS);

  // Health drives the honest status line: the point of the whole chain is that
  // a feed with no key does not look like a feed that is broken.
  const refreshHealth = async () => {
    try {
      hud.setSources(await api.sources());
    } catch {
      /* the stream indicator already says the server is unreachable */
    }
  };
  void refreshHealth();
  setInterval(() => void refreshHealth(), 15_000);

  // Exposed for the console and for the headless smoke test. Not a public
  // interface — nothing in the client reads it.
  Object.assign(window, {
    argus: { viewer, renderer, stream, api, card, sensors, geo: { loadGeoid, geoidReady, undulationM, resolveHeight } },
  });
}

void main().catch((error: unknown) => {
  console.error("argus: fatal", error);
  const hud = document.getElementById("hud");
  if (hud) {
    hud.innerHTML = `<div class="panel"><h2>Argus failed to start</h2><pre>${String(
      error,
    )}</pre></div>`;
  }
});
