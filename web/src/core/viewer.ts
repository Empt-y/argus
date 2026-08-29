/**
 * Cesium bootstrap.
 *
 * Two decisions worth stating up front.
 *
 * **The globe works with no credentials.** Cesium's defaults want an ion token,
 * and the photorealistic tileset wants a Google key on top. Neither is
 * something this server has by default, and a client that shows a black screen
 * until someone goes and gets an API key is a client nobody finishes setting
 * up. So the base case is keyless OpenStreetMap imagery on the plain ellipsoid,
 * and photoreal is an upgrade that lights up when `/v1/client-keys` returns
 * something — the same honest-degradation rule the server applies to a feed
 * with no key.
 *
 * **Rendering is on demand.** Cesium renders continuously by default, which
 * pins a GPU at 60 fps to redraw a globe that has not moved. `requestRenderMode`
 * makes it redraw when something actually changes. On a machine that is also
 * running the ingest daemon, that is the difference between a background tab
 * and a space heater.
 */

import {
  Cartesian3,
  Color,
  Ion,
  Math as CesiumMath,
  OpenStreetMapImageryProvider,
  Rectangle,
  ScreenSpaceEventType,
  Viewer,
} from "cesium";
import type { ClientKeys } from "../net/types";

export interface ViewerBundle {
  viewer: Viewer;
  /** Whether the photorealistic tileset is available in this session. */
  photoreal: boolean;
}

export function createViewer(
  container: HTMLElement,
  keys: ClientKeys | null,
): ViewerBundle {
  // Set before constructing the Viewer: Cesium reads it during widget setup,
  // and an empty default token makes its own asset requests fail noisily even
  // when nothing is asking for ion data.
  if (keys?.cesium_ion_token) {
    Ion.defaultAccessToken = keys.cesium_ion_token;
  }

  const viewer = new Viewer(container, {
    // Keyless. OSM's tile policy asks for a real user agent and modest volume,
    // which a single self-hosted client satisfies comfortably.
    baseLayer: false,
    baseLayerPicker: false,
    geocoder: false,
    homeButton: false,
    sceneModePicker: false,
    navigationHelpButton: false,
    animation: false,
    timeline: false,
    fullscreenButton: false,
    infoBox: false,
    selectionIndicator: false,
    // Argus draws time itself, through the DVR scrubber. Cesium's clock would
    // be a second, disagreeing source of "when".
    shouldAnimate: false,
    requestRenderMode: true,
    maximumRenderTimeChange: Infinity,
  });

  viewer.imageryLayers.addImageryProvider(
    new OpenStreetMapImageryProvider({ url: "https://tile.openstreetmap.org/" }),
  );

  const scene = viewer.scene;
  scene.globe.enableLighting = false;
  scene.globe.baseColor = Color.fromCssColorString("#0d1117");
  scene.backgroundColor = Color.fromCssColorString("#05070a");
  // Optional in Cesium's typings because a 2D/Columbus-view scene has none.
  if (scene.skyAtmosphere) scene.skyAtmosphere.show = true;
  // Depth-testing against terrain is what stops a contact on the far side of
  // the planet drawing through it.
  scene.globe.depthTestAgainstTerrain = true;

  // Cesium's default double-click "track this entity" fights with our own
  // selection, and there is no way to opt an entity out of it.
  viewer.screenSpaceEventHandler.removeInputAction(
    ScreenSpaceEventType.LEFT_DOUBLE_CLICK,
  );

  viewer.camera.setView({
    destination: Cartesian3.fromDegrees(-1.0, 50.0, 2_000_000),
    orientation: { heading: 0, pitch: CesiumMath.toRadians(-70), roll: 0 },
  });

  return { viewer, photoreal: Boolean(keys?.google_maps_api_key) };
}

/**
 * The camera's footprint as `[west, south, east, north]` degrees, or `null`
 * when the view takes in space beyond the limb.
 *
 * `null` is a real answer and the caller must handle it: at a low enough zoom
 * the visible region is not a rectangle at all, and inventing one would ask the
 * server for a box the user is not looking at.
 */
export function viewportBbox(
  viewer: Viewer,
): [number, number, number, number] | null {
  const rect = viewer.camera.computeViewRectangle(
    viewer.scene.globe.ellipsoid,
    new Rectangle(),
  );
  if (!rect) return null;

  const west = CesiumMath.toDegrees(rect.west);
  const south = CesiumMath.toDegrees(rect.south);
  const east = CesiumMath.toDegrees(rect.east);
  const north = CesiumMath.toDegrees(rect.north);
  if (![west, south, east, north].every(Number.isFinite)) return null;
  // A degenerate rectangle means the camera is looking past the horizon.
  if (south >= north) return null;
  return [west, south, east, north];
}

/**
 * Run `handler` when the camera settles, not on every frame of a drag.
 *
 * `moveEnd` alone is not enough: a continuous wheel-zoom fires it repeatedly,
 * and each one would be a fresh viewport query. The trailing delay collapses a
 * gesture into a single request.
 */
export function onCameraSettled(
  viewer: Viewer,
  delayMs: number,
  handler: () => void,
): () => void {
  let timer: ReturnType<typeof setTimeout> | null = null;
  const schedule = () => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(handler, delayMs);
  };
  viewer.camera.moveEnd.addEventListener(schedule);
  return () => {
    if (timer) clearTimeout(timer);
    viewer.camera.moveEnd.removeEventListener(schedule);
  };
}
