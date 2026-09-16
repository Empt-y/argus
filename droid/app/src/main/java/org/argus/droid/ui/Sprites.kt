package org.argus.droid.ui

import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Color
import android.graphics.Paint
import android.graphics.Path
import org.argus.droid.net.LayerView
import org.maplibre.android.maps.Style

/**
 * Contact glyphs, drawn at runtime rather than shipped as a sprite sheet.
 *
 * Same reasoning as the web client's `layers/icons.ts`: the layer catalogue
 * supplies colours from the server, so a layer added to the daemon tomorrow
 * arrives with a colour this app has never seen. A shipped sheet could not tint
 * it without a shader or a per-layer asset, and both are more machinery than
 * drawing a triangle.
 *
 * Every glyph points **up** in its own texture space. The style supplies
 * `icon-rotate` from the contact's course, and `icon-rotation-alignment: map`
 * makes that a bearing on the ground rather than an angle on the screen.
 *
 * Shape comes from the entity *kind*, which is a closed enum of eight in
 * `argus-core`; colour comes from the *layer*, of which there may be any
 * number. That split is why a new layer needs no app release and a new kind
 * would — and kinds do not appear without a schema change anyway.
 */
object Sprites {
    /** Registered by every client unconditionally; the style falls back to it
     *  so a layer whose glyph is missing shows a marker rather than nothing. */
    const val FALLBACK = "argus-contact"

    private const val SIZE = 44
    private const val SCALE = 2f

    /**
     * Draw and register a glyph for every layer, plus the fallback.
     *
     * Called after each style load, because a style replaces the whole document
     * including its images.
     */
    fun install(style: Style, layers: List<LayerView>) {
        style.addImage(FALLBACK, glyph(Kind.UNKNOWN, Color.LTGRAY))
        for (layer in layers) {
            val colour = parseColour(layer.style.color)
            style.addImage(layer.id, glyph(Kind.of(layer.kind), colour))
        }
    }

    private enum class Kind {
        AIRCRAFT, VESSEL, VEHICLE, SATELLITE, EVENT, STATION, UNKNOWN;

        companion object {
            fun of(kind: String) = when (kind) {
                "aircraft" -> AIRCRAFT
                "vessel" -> VESSEL
                "vehicle" -> VEHICLE
                "satellite" -> SATELLITE
                "event" -> EVENT
                "station" -> STATION
                else -> UNKNOWN
            }
        }
    }

    private fun glyph(kind: Kind, colour: Int): Bitmap {
        val px = (SIZE * SCALE).toInt()
        val bitmap = Bitmap.createBitmap(px, px, Bitmap.Config.ARGB_8888)
        val canvas = Canvas(bitmap)
        canvas.scale(SCALE, SCALE)

        val fill = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            this.color = colour
            style = Paint.Style.FILL
        }
        // A dark rim, so a pale contact stays visible over pale terrain and a
        // dark one over sea. Without it half the fleet disappears over one or
        // the other — the single thing that most improves legibility.
        val rim = Paint(Paint.ANTI_ALIAS_FLAG).apply {
            this.color = Color.argb(190, 0, 0, 0)
            style = Paint.Style.STROKE
            strokeWidth = 1.6f
            strokeJoin = Paint.Join.ROUND
        }

        val c = SIZE / 2f
        when (kind) {
            // A swept chevron: unmistakably directional, and readable at the
            // size a contact actually occupies on a phone.
            Kind.AIRCRAFT -> path {
                moveTo(c, 5f); lineTo(SIZE - 11f, SIZE - 9f)
                lineTo(c, SIZE - 15f); lineTo(11f, SIZE - 9f); close()
            }.draw(canvas, fill, rim)

            // A hull: pointed bow, square stern. Reads as "vessel" at a glance
            // and cannot be confused with the aircraft chevron.
            Kind.VESSEL -> path {
                moveTo(c, 6f); lineTo(SIZE - 13f, c + 2f)
                lineTo(SIZE - 15f, SIZE - 8f); lineTo(15f, SIZE - 8f)
                lineTo(13f, c + 2f); close()
            }.draw(canvas, fill, rim)

            // An oblong with a blunt nose: a bus seen from above, longer than
            // it is wide, and directional without being a chevron — a bus on
            // a road and an aircraft over it must never read as the same
            // thing at a glance.
            Kind.VEHICLE -> path {
                moveTo(c - 5f, 7f); lineTo(c + 5f, 7f)
                lineTo(c + 7f, 11f); lineTo(c + 7f, SIZE - 8f)
                lineTo(c - 7f, SIZE - 8f); lineTo(c - 7f, 11f); close()
            }.draw(canvas, fill, rim)

            // A ring with a bar through it, for something in orbit rather than
            // on a course — deliberately not directional.
            Kind.SATELLITE -> {
                canvas.drawCircle(c, c, 6.5f, fill)
                canvas.drawCircle(c, c, 6.5f, rim)
                val bar = Paint(fill).apply { strokeWidth = 3f; style = Paint.Style.STROKE }
                canvas.drawLine(c - 13f, c, c + 13f, c, bar)
                canvas.drawLine(c - 13f, c, c + 13f, c, Paint(rim).apply { strokeWidth = 1f })
            }

            // A diamond: a thing that happened at a place, not a thing moving
            // through one.
            Kind.EVENT -> path {
                moveTo(c, 6f); lineTo(SIZE - 8f, c)
                lineTo(c, SIZE - 6f); lineTo(8f, c); close()
            }.draw(canvas, fill, rim)

            // A squat pin, for a fixed installation.
            Kind.STATION -> path {
                moveTo(c, SIZE - 6f); lineTo(c - 8f, c - 1f)
                lineTo(c - 8f, 8f); lineTo(c + 8f, 8f); lineTo(c + 8f, c - 1f); close()
            }.draw(canvas, fill, rim)

            Kind.UNKNOWN -> {
                canvas.drawCircle(c, c, 7f, fill)
                canvas.drawCircle(c, c, 7f, rim)
            }
        }
        return bitmap
    }

    private fun path(build: Path.() -> Unit) = Path().apply(build)

    private fun Path.draw(canvas: Canvas, fill: Paint, rim: Paint) {
        canvas.drawPath(this, fill)
        canvas.drawPath(this, rim)
    }

    /** `#rrggbb` from the catalogue. Falls back rather than throwing. */
    private fun parseColour(hex: String): Int =
        runCatching { Color.parseColor(hex) }.getOrDefault(Color.LTGRAY)
}
