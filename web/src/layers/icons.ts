/**
 * Contact glyphs, drawn at runtime.
 *
 * Canvas rather than an sprite sheet, for one reason that matters: the layer
 * catalogue supplies colours from the server, so a new layer arrives with a
 * colour this client has never seen. A shipped sprite sheet could not tint it
 * without either a shader or a per-layer asset, and both are more machinery
 * than drawing a triangle.
 *
 * Glyphs point UP in their own texture space; `geo/heading.ts` supplies the
 * screen rotation that turns "up" into "where it is going".
 */

const cache = new Map<string, string>();
const SIZE = 32;

/** A directional chevron, for anything with a course. */
export function chevron(color: string): string {
  return cached(`chevron:${color}`, (ctx) => {
    ctx.beginPath();
    ctx.moveTo(SIZE / 2, 3);
    ctx.lineTo(SIZE - 6, SIZE - 5);
    ctx.lineTo(SIZE / 2, SIZE - 11);
    ctx.lineTo(6, SIZE - 5);
    ctx.closePath();
    ctx.fillStyle = color;
    ctx.fill();
    // A dark rim so a light contact stays visible over pale terrain and a dark
    // one over sea. Without it, half the fleet disappears over one or the other.
    ctx.lineWidth = 1.5;
    ctx.strokeStyle = "rgba(0,0,0,0.75)";
    ctx.stroke();
  });
}

function cached(key: string, draw: (ctx: CanvasRenderingContext2D) => void): string {
  const hit = cache.get(key);
  if (hit) return hit;
  const canvas = document.createElement("canvas");
  canvas.width = SIZE;
  canvas.height = SIZE;
  const ctx = canvas.getContext("2d");
  if (!ctx) return "";
  draw(ctx);
  const url = canvas.toDataURL("image/png");
  cache.set(key, url);
  return url;
}
