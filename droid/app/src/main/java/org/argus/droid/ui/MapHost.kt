package org.argus.droid.ui

import android.graphics.Color
import android.graphics.PointF
import android.graphics.RectF
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.platform.LocalContext
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleEventObserver
import org.argus.droid.TappedFeature
import org.argus.droid.net.TrackPoint
import org.maplibre.android.maps.MapLibreMap
import org.maplibre.android.maps.MapView
import org.maplibre.android.maps.Style
import org.maplibre.android.style.layers.Layer
import org.maplibre.android.style.layers.FillLayer
import org.maplibre.android.style.layers.LineLayer
import org.maplibre.android.style.layers.Property
import org.maplibre.android.style.layers.PropertyFactory
import org.maplibre.android.style.sources.GeoJsonSource
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonObject
import org.maplibre.geojson.Feature
import org.maplibre.geojson.FeatureCollection
import org.maplibre.geojson.LineString
import org.maplibre.geojson.Point
import java.time.Instant

/** Ids the app owns in the style. Everything else came from the server. */
const val TRACK_SOURCE = "argus-track"
const val TRACK_LINE = "argus-track-line"
const val FENCE_SOURCE = "argus-geofences"
const val FENCE_FILL = "argus-geofence-fill"
const val FENCE_LINE = "argus-geofence-line"
const val BASEMAP_LAYER = "basemap"

/** Geofences are drawn in one colour, not the layer palette: they are the
 *  user's own annotation on the map rather than something Argus collected. */
private const val FENCE_COLOUR = Color.CYAN

/**
 * A [MapView] that follows the composition's lifecycle.
 *
 * MapLibre's view is not a plain Android view: it holds a GL surface and a
 * native map object, and it expects every one of these callbacks. Missing them
 * is not a crash, which is the problem — the first version of this app called
 * `onCreate` alone and looked fine, while leaking the renderer on every
 * rotation and never releasing it on the way out.
 */
@Composable
fun rememberMapView(): MapView {
    val context = LocalContext.current
    val mapView = remember { MapView(context).apply { onCreate(null) } }
    val lifecycleOwner = LocalLifecycleOwner.current

    DisposableEffect(lifecycleOwner) {
        val observer = LifecycleEventObserver { _, event ->
            when (event) {
                Lifecycle.Event.ON_START -> mapView.onStart()
                Lifecycle.Event.ON_RESUME -> mapView.onResume()
                Lifecycle.Event.ON_PAUSE -> mapView.onPause()
                Lifecycle.Event.ON_STOP -> mapView.onStop()
                Lifecycle.Event.ON_DESTROY -> mapView.onDestroy()
                else -> Unit
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            mapView.onStop()
            mapView.onDestroy()
        }
    }
    return mapView
}

/**
 * Hide or show a whole Argus layer.
 *
 * A layer in the catalogue becomes several style layers — a fill, an outline, a
 * line, a circle — named `<id>-something` by the server. The app does not need
 * to know which of those exist for a given layer, only that they share the
 * prefix, which is what keeps a new geometry class on the server from needing a
 * change here.
 */
fun Style.setLayerVisible(layerId: String, visible: Boolean) {
    val value = if (visible) Property.VISIBLE else Property.NONE
    layers.filter { it.id == layerId || it.id.startsWith("$layerId-") }
        .forEach { it.setProperties(PropertyFactory.visibility(value)) }
}

fun Style.setBasemapVisible(visible: Boolean) {
    getLayer(BASEMAP_LAYER)?.setProperties(
        PropertyFactory.visibility(if (visible) Property.VISIBLE else Property.NONE)
    )
}

/**
 * Install the layers the app draws itself, on top of everything the server sent.
 *
 * Re-run after every `setStyle`, because a style load replaces the whole
 * document — these are ours, so nothing on the server side will put them back.
 */
fun Style.installTrackLayers() {
    if (getSource(TRACK_SOURCE) != null) return

    // Geofences go in first so they sit under the track and under the contacts:
    // a fence is context for what is moving through it, and a translucent slab
    // painted over the aircraft it is about would be exactly backwards.
    addSource(GeoJsonSource(FENCE_SOURCE))
    addLayer(
        FillLayer(FENCE_FILL, FENCE_SOURCE).withProperties(
            PropertyFactory.fillColor(FENCE_COLOUR),
            PropertyFactory.fillOpacity(0.12f),
        )
    )
    addLayer(
        LineLayer(FENCE_LINE, FENCE_SOURCE).withProperties(
            PropertyFactory.lineColor(FENCE_COLOUR),
            PropertyFactory.lineWidth(1.5f),
            // Dashed, so a fence is never mistaken for a coastline or an
            // airspace boundary the basemap drew.
            PropertyFactory.lineDasharray(arrayOf(3f, 2f)),
        )
    )

    addSource(GeoJsonSource(TRACK_SOURCE))
    addLayer(
        LineLayer(TRACK_LINE, TRACK_SOURCE).withProperties(
            PropertyFactory.lineColor(Color.WHITE),
            PropertyFactory.lineWidth(2.0f),
            PropertyFactory.lineOpacity(0.85f),
            PropertyFactory.lineCap(Property.LINE_CAP_ROUND),
            PropertyFactory.lineJoin(Property.LINE_JOIN_ROUND),
        )
    )
}

/**
 * Draw the selected entity's history, or clear it.
 *
 * Points with no position are dropped rather than interpolated across. A track
 * is evidence of where something was; joining two fixes over a gap the server
 * never reported would be the client inventing history, which is the one thing
 * a DVR must not do.
 */
fun Style.showTrack(points: List<TrackPoint>) {
    val source = getSourceAs<GeoJsonSource>(TRACK_SOURCE) ?: return
    val coordinates = points.mapNotNull { p ->
        val lon = p.lon
        val lat = p.lat
        if (lon == null || lat == null) null else Point.fromLngLat(lon, lat)
    }
    if (coordinates.size < 2) {
        source.setGeoJson(Feature.fromGeometry(LineString.fromLngLats(emptyList())))
    } else {
        source.setGeoJson(Feature.fromGeometry(LineString.fromLngLats(coordinates)))
    }
}

/**
 * Draw the armed geofences, or clear them.
 *
 * Only the enabled ones: a disarmed fence still exists and still owns its
 * history, but drawing it would say the map is watching a box it is not.
 */
fun Style.showGeofences(fences: List<org.argus.droid.net.GeofenceView>) {
    val source = getSourceAs<GeoJsonSource>(FENCE_SOURCE) ?: return
    val features = fences
        .filter { it.enabled }
        .mapNotNull { fence ->
            runCatching {
                Feature.fromJson(
                    buildJsonObject {
                        put("type", JsonPrimitive("Feature"))
                        put("geometry", fence.geometry)
                        put(
                            "properties",
                            buildJsonObject { put("name", JsonPrimitive(fence.name)) },
                        )
                    }.toString()
                )
            }.getOrNull()
        }
    source.setGeoJson(FeatureCollection.fromFeatures(features))
}

/**
 * What the user meant to tap.
 *
 * Contacts are drawn as four-pixel circles, so a hit test at the exact touch
 * point misses almost every time. The query is a box around the finger instead,
 * and the nearest feature inside it wins — which is also why the search is
 * ordered by distance rather than taking MapLibre's first result, since that
 * is in draw order and would prefer whatever happens to be on top.
 */
fun MapLibreMap.featureAt(screen: PointF, style: Style, slopPx: Float = 36f): TappedFeature? {
    val box = RectF(
        screen.x - slopPx,
        screen.y - slopPx,
        screen.x + slopPx,
        screen.y + slopPx,
    )
    val queryable = style.layers
        .map(Layer::getId)
        .filter { it != BASEMAP_LAYER && !it.startsWith("argus-") }
        .toTypedArray()
    if (queryable.isEmpty()) return null

    return queryRenderedFeatures(box, *queryable)
        .mapNotNull { it.toTapped() }
        .minByOrNull { candidate ->
            val point = projection.toScreenLocation(
                org.maplibre.android.geometry.LatLng(candidate.lat, candidate.lon)
            )
            val dx = point.x - screen.x
            val dy = point.y - screen.y
            dx * dx + dy * dy
        }
}

private fun Feature.toTapped(): TappedFeature? {
    val kind = getStringProperty("kind") ?: return null
    val key = getStringProperty("key") ?: return null
    val geometry = geometry()
    // Polygons and lines are tapped too — an alert area has no single point, so
    // the card is opened from the feature's identity and located by whatever
    // vertex the geometry starts at rather than pretending to a centroid.
    val position: Point? = when (geometry) {
        is Point -> geometry
        is LineString -> geometry.coordinates().firstOrNull()
        else -> null
    }
    val lon = position?.longitude() ?: return null
    val lat = position.latitude()

    return TappedFeature(
        kind = kind,
        key = key,
        label = getStringProperty("label"),
        sourceId = getStringProperty("source"),
        quality = getStringProperty("quality"),
        observedAt = getNumberProperty("t")?.let { Instant.ofEpochSecond(it.toLong()) },
        altM = getNumberProperty("alt_m")?.toDouble(),
        courseDeg = getNumberProperty("course_deg")?.toFloat(),
        speedMps = getNumberProperty("speed_mps")?.toFloat(),
        lon = lon,
        lat = lat,
    )
}
