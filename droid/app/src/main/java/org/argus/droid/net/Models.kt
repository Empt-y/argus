package org.argus.droid.net

import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.JsonElement
import kotlinx.serialization.json.JsonObject

/**
 * The server's JSON, as the app needs it.
 *
 * Every one of these is decoded with `ignoreUnknownKeys`, which is not
 * laziness: the whole point of driving this client from `/v1/style.json` and
 * `/v1/layers` is that a driver added to the daemon reaches the phone without
 * an app release. A strict decoder would turn every server-side addition into a
 * crash on a handset that is a version behind, which is exactly the coupling
 * the design is trying to avoid.
 *
 * Instants stay as strings here and are parsed at the edge that needs them.
 * `java.time` is available from API 26, which is this app's minimum, so there
 * is no desugaring involved — it just keeps the wire types honest about being
 * wire types.
 */
@Serializable
data class Health(
    val status: String,
    val version: String,
    @SerialName("uptime_seconds") val uptimeSeconds: Long = 0,
    @SerialName("loopback_exempt") val loopbackExempt: Boolean = false,
)

@Serializable
data class LayerStyle(
    val geometry: String = "point",
    val color: String = "#888888",
    @SerialName("min_zoom") val minZoom: Int = 0,
    @SerialName("max_zoom") val maxZoom: Int = 16,
    @SerialName("rotates_with_course") val rotatesWithCourse: Boolean = false,
)

@Serializable
data class LayerView(
    val id: String,
    @SerialName("display_name") val displayName: String,
    val kind: String,
    /** `live`, `degraded`, `stale`, `unconfigured`, … — reported, not inferred. */
    val state: String,
    val sources: List<String> = emptyList(),
    @SerialName("last_success") val lastSuccess: String? = null,
    val observations: Long = 0,
    @SerialName("live_entities") val liveEntities: Long = 0,
    val tileable: Boolean = true,
    val style: LayerStyle = LayerStyle(),
)

@Serializable
data class LayersResponse(val layers: List<LayerView> = emptyList())

@Serializable
data class SourceRow(
    @SerialName("source_id") val sourceId: String,
    @SerialName("layer_id") val layerId: String,
    @SerialName("display_name") val displayName: String,
    val state: String,
    @SerialName("last_success") val lastSuccess: String? = null,
    @SerialName("last_error") val lastError: String? = null,
    @SerialName("last_lag_ms") val lastLagMs: Int? = null,
    val observations: Long = 0,
)

@Serializable
data class SourcesResponse(val sources: List<SourceRow> = emptyList())

@Serializable
data class EntityRow(
    @SerialName("entity_kind") val kind: String,
    @SerialName("entity_key") val key: String,
    @SerialName("source_id") val sourceId: String,
    @SerialName("layer_id") val layerId: String,
    @SerialName("observed_at") val observedAt: String,
    val lon: Double? = null,
    val lat: Double? = null,
    val geom: JsonElement? = null,
    @SerialName("alt_m") val altM: Double? = null,
    @SerialName("alt_datum") val altDatum: String? = null,
    @SerialName("course_deg") val courseDeg: Float? = null,
    @SerialName("heading_deg") val headingDeg: Float? = null,
    @SerialName("speed_mps") val speedMps: Float? = null,
    @SerialName("vrate_mps") val vrateMps: Float? = null,
    val quality: String = "unknown",
    val label: String? = null,
    val attrs: JsonObject = JsonObject(emptyMap()),
)

@Serializable
data class TrackPoint(
    val at: String,
    val lon: Double? = null,
    val lat: Double? = null,
    @SerialName("alt_m") val altM: Double? = null,
    @SerialName("course_deg") val courseDeg: Float? = null,
    @SerialName("speed_mps") val speedMps: Float? = null,
)

@Serializable
data class TrackResponse(
    val entity: String,
    val kind: String,
    val from: String,
    val to: String,
    val points: List<TrackPoint> = emptyList(),
)

@Serializable
data class PairRequest(val code: String, val name: String)

@Serializable
data class PairResponse(
    @SerialName("device_id") val deviceId: String,
    val name: String,
    /** The only copy of this token that will ever exist. Store it or pair again. */
    val token: String,
    val scopes: List<String> = emptyList(),
)
