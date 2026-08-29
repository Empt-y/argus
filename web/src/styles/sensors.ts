/**
 * Sensor looks, as Cesium post-process stages.
 *
 * These are a full-screen shader pass over the rendered frame, not a CSS
 * filter. The difference matters: a CSS filter would tint the HUD as well as
 * the globe, and the HUD is where the honest status chips live — a green NVG
 * wash over a red FAILED chip is exactly the kind of thing that makes a
 * carefully-built health model useless.
 *
 * One deliberate limitation. The scene renders on demand (`requestRenderMode`),
 * so anything animated in here would only advance when the camera moved, which
 * looks broken rather than alive. The grain is therefore a spatial hash of the
 * pixel coordinate — static, and static on purpose. Making it move would mean
 * rendering continuously, and a space heater is too high a price for shimmer.
 */

import { PostProcessStage, type Viewer } from "cesium";

export type SensorStyle = "none" | "noir" | "nvg" | "flir" | "crt" | "snow";

export const SENSOR_STYLES: { id: SensorStyle; label: string }[] = [
  { id: "none", label: "natural" },
  { id: "noir", label: "noir" },
  { id: "nvg", label: "nvg" },
  { id: "flir", label: "flir" },
  { id: "crt", label: "crt" },
  { id: "snow", label: "snow" },
];

/** Shared helpers, prepended to every stage. */
const PRELUDE = `
uniform sampler2D colorTexture;
in vec2 v_textureCoordinates;

float luma(vec3 c) { return dot(c, vec3(0.2126, 0.7152, 0.0722)); }

// Cheap deterministic hash. Static by design; see the module comment.
float grain(vec2 p) {
  return fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453);
}

// Distance from the centre, for vignetting. Squared to keep the falloff soft
// near the middle where most of the picture is.
float vignette(vec2 uv, float strength) {
  vec2 d = uv - vec2(0.5);
  return 1.0 - strength * dot(d, d) * 2.0;
}
`;

const SHADERS: Record<Exclude<SensorStyle, "none">, string> = {
  noir: `${PRELUDE}
    void main() {
      vec3 c = texture(colorTexture, v_textureCoordinates).rgb;
      float l = luma(c);
      // Filmic S-curve: crush the shadows, hold the highlights.
      l = clamp((l - 0.5) * 1.45 + 0.46, 0.0, 1.0);
      vec3 toned = mix(vec3(l), vec3(l * 0.96, l * 0.98, l * 1.06), 0.35);
      out_FragColor = vec4(toned * vignette(v_textureCoordinates, 0.65), 1.0);
    }`,

  nvg: `${PRELUDE}
    void main() {
      vec3 c = texture(colorTexture, v_textureCoordinates).rgb;
      // An image intensifier is broadband and biased to the near infrared, so
      // foliage and warm surfaces read brighter than their visible luminance.
      float gain = dot(c, vec3(0.28, 0.52, 0.20)) * 1.55 + 0.04;
      gain = pow(clamp(gain, 0.0, 1.0), 0.78);
      float g = grain(v_textureCoordinates * 900.0) * 0.09;
      vec3 phosphor = vec3(0.16, 1.0, 0.38) * (gain + g);
      out_FragColor = vec4(phosphor * vignette(v_textureCoordinates, 1.05), 1.0);
    }`,

  flir: `${PRELUDE}
    void main() {
      vec3 c = texture(colorTexture, v_textureCoordinates).rgb;
      float t = clamp(luma(c) * 1.15, 0.0, 1.0);
      // White-hot ramp through the usual black-purple-orange-white palette,
      // built from two mixes rather than a lookup texture.
      vec3 cold = mix(vec3(0.02, 0.0, 0.09), vec3(0.55, 0.06, 0.42), smoothstep(0.0, 0.5, t));
      vec3 hot  = mix(vec3(0.98, 0.55, 0.05), vec3(1.0, 1.0, 0.95), smoothstep(0.75, 1.0, t));
      vec3 mid  = mix(cold, vec3(0.98, 0.55, 0.05), smoothstep(0.35, 0.8, t));
      out_FragColor = vec4(mix(mid, hot, smoothstep(0.7, 1.0, t)), 1.0);
    }`,

  crt: `${PRELUDE}
    void main() {
      vec2 uv = v_textureCoordinates;
      // Separate the channels by a fraction of a pixel, more towards the edges,
      // the way a shadow mask misconverges away from centre.
      vec2 off = (uv - 0.5) * 0.0022;
      float r = texture(colorTexture, uv + off).r;
      float g = texture(colorTexture, uv).g;
      float b = texture(colorTexture, uv - off).b;
      vec3 c = vec3(r, g, b);
      // Scanlines in screen rows, not texture space, so they stay one pixel
      // regardless of how the globe is zoomed.
      float lines = 0.88 + 0.12 * sin(gl_FragCoord.y * 3.14159);
      c *= lines;
      c = mix(c, c * vec3(0.85, 1.05, 0.95), 0.4);
      out_FragColor = vec4(c * vignette(uv, 0.9) + grain(uv * 640.0) * 0.02, 1.0);
    }`,

  snow: `${PRELUDE}
    void main() {
      vec3 c = texture(colorTexture, v_textureCoordinates).rgb;
      float l = luma(c);
      // Bleach-bypass: keep some colour, push everything towards a cold white.
      vec3 bleached = mix(c, vec3(l), 0.72);
      bleached = pow(bleached, vec3(0.82));
      bleached *= vec3(0.93, 0.97, 1.06);
      float g = grain(v_textureCoordinates * 520.0) * 0.05;
      out_FragColor = vec4(clamp(bleached + g, 0.0, 1.0) * vignette(v_textureCoordinates, 0.35), 1.0);
    }`,
};

/**
 * Owns the single active stage.
 *
 * Stages are created lazily and destroyed on switch rather than kept warm: each
 * one holds a full-resolution framebuffer, and five of those for looks nobody
 * is using is a lot of video memory to spend on nothing.
 */
export class SensorStyles {
  #active: SensorStyle = "none";
  #stage: PostProcessStage | null = null;

  constructor(private readonly viewer: Viewer) {}

  get active(): SensorStyle {
    return this.#active;
  }

  apply(style: SensorStyle): void {
    if (style === this.#active) return;
    if (this.#stage) {
      this.viewer.scene.postProcessStages.remove(this.#stage);
      this.#stage = null;
    }
    this.#active = style;
    if (style !== "none") {
      this.#stage = this.viewer.scene.postProcessStages.add(
        new PostProcessStage({
          name: `argus-${style}`,
          fragmentShader: SHADERS[style],
        }),
      ) as PostProcessStage;
    }
    this.viewer.scene.requestRender();
  }
}
