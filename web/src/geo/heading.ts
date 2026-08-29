/**
 * Pointing an icon along a real-world course.
 *
 * This is subtler than it sounds, and the naive version is wrong in a way that
 * looks right most of the time. A Cesium billboard is a camera-facing quad, so
 * its `rotation` is a screen-space angle — but a course is a bearing on the
 * globe's surface. `rotation = -course` is only correct for a camera looking
 * straight down with north up. Tilt the camera towards the horizon, or orbit
 * it, and every icon points somewhere it is not going.
 *
 * The fix is to do the whole thing in the camera's own basis:
 *
 *   1. build the course as a vector in the entity's local east-north-up frame,
 *   2. rotate that into world space with the ENU-to-fixed-frame transform,
 *   3. project it onto `camera.rightWC` and `camera.upWC`,
 *   4. the screen angle is `atan2` of those two components.
 *
 * Note what step 3 deliberately does NOT do: probe a second point some distance
 * ahead and project *that* to window coordinates. That is the obvious
 * implementation and it fails exactly when it is most visible — the probe point
 * can be off-screen, behind the camera, or on the far side of the limb, and the
 * icon then snaps to a garbage angle or vanishes. Projecting the vector itself
 * has no such point to lose.
 *
 * The cost of that choice, stated rather than hidden: projecting onto the
 * camera basis is exact at the centre of the viewport and an orthographic
 * approximation away from it, because it drops the course vector's depth
 * component. A contact in the corner of a strongly tilted view can differ by a
 * degree or two from a true pinhole projection. That is the right trade — the
 * error is invisible and the alternative's failure mode is not.
 */

import {
  Cartesian3,
  Math as CesiumMath,
  Matrix4,
  Transforms,
  type Scene,
} from "cesium";

/**
 * Length of the local course vector. Any positive value gives the same angle;
 * a large one keeps the dot products well away from floating-point noise.
 */
const PROBE_M = 2000;

/**
 * Below this, the course points almost straight into or out of the screen and
 * the resulting angle is noise rather than information. An aircraft flying
 * directly away from a horizon-level camera is the real case.
 */
const MIN_SCREEN_COMPONENT_M = 0.5;

/**
 * Angular changes smaller than this are ignored, so an icon does not shimmer
 * from projection noise while the camera is still.
 */
const DEADBAND_RAD = CesiumMath.toRadians(0.5);

const enuScratch = new Matrix4();
const localScratch = new Cartesian3();
const worldScratch = new Cartesian3();

/**
 * Screen rotation, in radians, for a sprite whose texture points "up".
 *
 * Returns `previous` when the angle cannot be trusted — an unknown course, a
 * degenerate projection — because holding the last good heading is much less
 * distracting than snapping to zero, which reads as "now flying north".
 */
export function screenRotation(
  scene: Scene,
  position: Cartesian3,
  courseDeg: number | null,
  previous: number | null = null,
): number | null {
  if (courseDeg === null || !Number.isFinite(courseDeg)) return previous;
  const camera = scene.camera;
  if (!camera) return previous;

  const course = CesiumMath.toRadians(courseDeg);
  // East-north-up: x is east, y is north, and a bearing is clockwise from
  // north — hence sin on east and cos on north, not the other way round.
  Cartesian3.fromElements(
    Math.sin(course) * PROBE_M,
    Math.cos(course) * PROBE_M,
    0,
    localScratch,
  );

  Transforms.eastNorthUpToFixedFrame(position, undefined, enuScratch);
  // As a vector, not a point: the frame's translation is the entity's own
  // position, and including it would produce a direction from the centre of the
  // Earth rather than a heading.
  Matrix4.multiplyByPointAsVector(enuScratch, localScratch, worldScratch);

  const x = Cartesian3.dot(worldScratch, camera.rightWC);
  const y = Cartesian3.dot(worldScratch, camera.upWC);
  if (Math.hypot(x, y) < MIN_SCREEN_COMPONENT_M) return previous;

  // Cesium's billboard rotation is counter-clockwise from the screen's +x axis,
  // and the sprite is drawn pointing up, so a quarter turn takes "up" to "+x".
  const rotation = Math.atan2(y, x) - Math.PI / 2;

  if (previous !== null && Math.abs(angleDelta(rotation, previous)) < DEADBAND_RAD) {
    return previous;
  }
  return rotation;
}

/** Signed difference between two angles, wrapped to `-PI..PI`. */
export function angleDelta(a: number, b: number): number {
  let d = (a - b) % (Math.PI * 2);
  if (d > Math.PI) d -= Math.PI * 2;
  if (d < -Math.PI) d += Math.PI * 2;
  return d;
}
