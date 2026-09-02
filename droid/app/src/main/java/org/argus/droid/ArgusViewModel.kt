package org.argus.droid

import android.app.Application
import androidx.core.app.NotificationManagerCompat
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import org.argus.droid.net.Alert
import org.argus.droid.net.ApiException
import org.argus.droid.net.ArgusClient
import org.argus.droid.net.EntityRow
import org.argus.droid.net.GeofenceView
import org.argus.droid.net.LayerView
import org.argus.droid.net.SourceRow
import org.argus.droid.net.TrackPoint
import org.maplibre.android.geometry.LatLngBounds
import java.time.Instant

/** How far back the scrubber can reach in one sitting. */
const val DVR_WINDOW_HOURS: Long = 6

/**
 * Whether the daemon is answering, and if not, why not.
 *
 * The distinction that matters is [Unauthorized] against [Unreachable]. A phone
 * at the wrong address and a phone that has never been paired both show an
 * empty map, and the fix for one is nothing like the fix for the other.
 */
sealed interface Connection {
    data object Connecting : Connection
    data class Ok(val version: String, val loopbackExempt: Boolean) : Connection
    data object Unauthorized : Connection
    data class Unreachable(val reason: String) : Connection
}

/** What the map drew where the user tapped, at the instant it is showing. */
data class TappedFeature(
    val kind: String,
    val key: String,
    val label: String?,
    /** Which provider reported it. The layer it belongs to comes with the
     *  detail fetch — a rendered feature does not carry its own source-layer. */
    val sourceId: String?,
    val quality: String?,
    val observedAt: Instant?,
    val altM: Double?,
    val courseDeg: Float?,
    val speedMps: Float?,
    val lon: Double,
    val lat: Double,
)

data class Selection(
    val tapped: TappedFeature,
    val detail: EntityRow? = null,
    val track: List<TrackPoint> = emptyList(),
    val loading: Boolean = true,
    val error: String? = null,
)

data class UiState(
    val baseUrl: String = Settings.EMULATOR_HOST,
    val paired: Boolean = false,
    val connection: Connection = Connection.Connecting,
    val layers: List<LayerView> = emptyList(),
    val sources: List<SourceRow> = emptyList(),
    /** Layer ids the user has switched off. Absence means visible. */
    val hidden: Set<String> = emptySet(),
    /** The instant the map is showing. `null` is live. */
    val at: Instant? = null,
    val selection: Selection? = null,
    /** Armed geofences, drawn on the map, and the alerts they have raised. */
    val geofences: List<GeofenceView> = emptyList(),
    val alerts: List<Alert> = emptyList(),
    /** Basemap areas already on this device, and the download in flight. */
    val regions: List<StoredRegion> = emptyList(),
    val download: DownloadProgress? = null,
    /** The style URL the map should be displaying, and a token that changes
     *  whenever it must be re-applied even though the string did not. */
    val styleUrl: String = "",
    val styleNonce: Int = 0,
    /** The basemap in use, and the names the server says it offers. */
    val basemap: String? = null,
    val basemapsOffered: List<String> = emptyList(),
) {
    val live: Boolean get() = at == null
    val unacknowledged: Int get() = alerts.count { it.acknowledgedAt == null }
}

class ArgusViewModel(app: Application) : AndroidViewModel(app) {
    private val settings = Settings.get(app)
    val client = ArgusClient(settings)
    private val offline = Offline(app)

    private val _state = MutableStateFlow(
        UiState(
            baseUrl = settings.baseUrl.value,
            paired = settings.token.value != null,
            basemap = settings.basemap.value,
            styleUrl = client.styleUrl(null, settings.basemap.value),
        )
    )
    val state: StateFlow<UiState> = _state.asStateFlow()

    private var catalogueJob: Job? = null
    private var selectionJob: Job? = null

    init {
        refreshCatalogue()
        refreshRegions()
    }

    fun refreshRegions() {
        offline.list { regions -> _state.update { it.copy(regions = regions) } }
    }

    fun downloadRegion(bounds: LatLngBounds, name: String, minZoom: Int, maxZoom: Int) {
        // The basemap style, never the live one — see [ArgusClient.basemapStyleUrl].
        offline.download(
            styleUrl = client.basemapStyleUrl(),
            name = name,
            bounds = bounds,
            minZoom = minZoom,
            maxZoom = maxZoom,
            pixelRatio = getApplication<Application>().resources.displayMetrics.density,
        ) { progress ->
            _state.update { it.copy(download = progress) }
            if (progress.complete) refreshRegions()
        }
    }

    fun deleteRegion(id: Long) {
        offline.delete(id) { refreshRegions() }
    }

    /**
     * Ask the server who it is and what it is watching.
     *
     * Health first and separately, because it is the one endpoint outside the
     * auth layer: if health answers and `/v1/layers` does not, the address is
     * right and the token is wrong, which is a different sentence to show the
     * user than "cannot reach the server".
     */
    fun refreshCatalogue() {
        catalogueJob?.cancel()
        catalogueJob = viewModelScope.launch {
            _state.update { it.copy(connection = Connection.Connecting) }
            val health = runCatching { client.health() }.getOrElse { err ->
                _state.update {
                    it.copy(connection = Connection.Unreachable(err.shortReason()))
                }
                return@launch
            }
            _state.update {
                it.copy(connection = Connection.Ok(health.version, health.loopbackExempt))
            }
            try {
                val layers = client.layers()
                val sources = client.sources()
                // Fetched on the same cadence as the catalogue rather than on
                // their own timer: the map's fences and the rail's health are
                // the same kind of fact — what the server is currently doing.
                val geofences = client.geofences()
                val alerts = client.alerts()
                _state.update {
                    // The catalogue arriving for the first time has to re-load
                    // the style, not merely repaint. MapLibre resolves an
                    // `image` expression when it lays a symbol out, so glyphs
                    // registered after that has happened do not appear until
                    // something forces a re-layout — the map sat there drawing
                    // every contact with the fallback marker.
                    //
                    // The style URL has not changed, so only the nonce can say
                    // "load this again", which is exactly what it is for.
                    val catalogueJustArrived = it.layers.isEmpty() && layers.isNotEmpty()
                    it.copy(
                        layers = layers,
                        sources = sources,
                        geofences = geofences,
                        alerts = alerts,
                        styleNonce = if (catalogueJustArrived) it.styleNonce + 1
                        else it.styleNonce,
                    )
                }
            } catch (err: ApiException) {
                if (err.code == 401 || err.code == 403) {
                    _state.update { it.copy(connection = Connection.Unauthorized) }
                } else {
                    _state.update {
                        it.copy(connection = Connection.Unreachable(err.shortReason()))
                    }
                }
            } catch (err: Exception) {
                _state.update { it.copy(connection = Connection.Unreachable(err.shortReason())) }
            }
        }
    }

    fun setServer(url: String) {
        settings.setBaseUrl(url)
        _state.update {
            it.copy(
                baseUrl = settings.baseUrl.value,
                paired = settings.token.value != null,
                layers = emptyList(),
                sources = emptyList(),
                selection = null,
                styleUrl = client.styleUrl(it.at, it.basemap),
                styleNonce = it.styleNonce + 1,
            )
        }
        refreshCatalogue()
    }

    /** Redeem a pairing code, then reload everything as the newly paired device. */
    fun pair(serverUrl: String, code: String, deviceName: String, onDone: (String?) -> Unit) {
        viewModelScope.launch {
            try {
                val response = client.pair(serverUrl, code, deviceName)
                settings.setBaseUrl(serverUrl)
                settings.setToken(response.token, response.deviceId)
                _state.update {
                    it.copy(
                        baseUrl = settings.baseUrl.value,
                        paired = true,
                        styleUrl = client.styleUrl(it.at, it.basemap),
                        // The style itself is unauthenticated no longer: it has
                        // to be fetched again with the token attached, and the
                        // URL has not changed, so only the nonce can say so.
                        styleNonce = it.styleNonce + 1,
                    )
                }
                refreshCatalogue()
                onDone(null)
            } catch (err: Exception) {
                onDone(err.shortReason())
            }
        }
    }

    fun unpair() {
        settings.clearToken()
        _state.update { it.copy(paired = false, styleNonce = it.styleNonce + 1) }
        refreshCatalogue()
    }

    /**
     * Switch basemap. Persisted, and applied by reloading the style — the whole
     * visual state of the map is that one URL.
     */
    fun setBasemap(name: String?) {
        settings.setBasemap(name)
        _state.update {
            it.copy(
                basemap = name,
                styleUrl = client.styleUrl(it.at, name),
                styleNonce = it.styleNonce + 1,
            )
        }
    }

    /** What the style document said it could offer, learnt when it loaded. */
    fun noteBasemapsOffered(names: List<String>) {
        _state.update { if (it.basemapsOffered == names) it else it.copy(basemapsOffered = names) }
    }

    fun toggleLayer(layerId: String) {
        _state.update {
            val hidden = it.hidden.toMutableSet()
            if (!hidden.add(layerId)) hidden.remove(layerId)
            it.copy(hidden = hidden)
        }
    }

    /**
     * Move the DVR, or return to live.
     *
     * Changing the instant changes the style URL, which is the whole mechanism:
     * the server bakes `at` into every tile template it generates, so one
     * `setStyle` moves the entire map through time and the client stays a
     * slider bound to a string.
     */
    fun seek(at: Instant?) {
        _state.update {
            if (it.at == at) return@update it
            it.copy(
                at = at,
                styleUrl = client.styleUrl(at, it.basemap),
                styleNonce = it.styleNonce + 1,
            )
        }
        // A track drawn at a past instant must not run on past it.
        _state.value.selection?.let { select(it.tapped) }
    }

    fun select(feature: TappedFeature) {
        selectionJob?.cancel()
        _state.update { it.copy(selection = Selection(tapped = feature)) }
        selectionJob = viewModelScope.launch {
            val at = _state.value.at
            val detail = runCatching { client.entity(feature.kind, feature.key) }
            val track = runCatching { client.track(feature.kind, feature.key, at) }
            _state.update { current ->
                val selection = current.selection ?: return@update current
                if (selection.tapped.key != feature.key) return@update current
                current.copy(
                    selection = selection.copy(
                        detail = detail.getOrNull(),
                        track = track.getOrNull()?.points.orEmpty(),
                        loading = false,
                        error = detail.exceptionOrNull()?.shortReason(),
                    )
                )
            }
        }
    }

    fun acknowledgeAlert(alertId: Long) {
        viewModelScope.launch {
            runCatching { client.acknowledgeAlert(alertId) }
            // The notification is keyed by alert id, so it can be withdrawn
            // exactly. Without this, acknowledging in the app left the banner
            // sitting in the shade — the user had dealt with it in one place
            // and was still being told about it in another, which is the
            // quickest way to teach someone to swipe the whole app away.
            NotificationManagerCompat.from(getApplication())
                .cancel(alertId.toInt())
            refreshCatalogue()
        }
    }

    fun clearSelection() {
        selectionJob?.cancel()
        _state.update { it.copy(selection = null) }
    }

    /**
     * Keep the health rail honest while the app is open.
     *
     * Only the catalogue is polled. The contacts refresh themselves: the daemon
     * gives a live tile a fifteen-second life and MapLibre re-requests it when
     * it expires, which is the only refresh mechanism a vector source has.
     */
    fun startPolling() {
        viewModelScope.launch {
            while (true) {
                delay(POLL_MS)
                if (_state.value.live) refreshCatalogue()
            }
        }
    }

    private companion object {
        const val POLL_MS = 30_000L
    }
}

/** A message worth putting on screen: the server's own words, or the exception's. */
internal fun Throwable.shortReason(): String = when (this) {
    is ApiException -> detail.take(200).ifBlank { "HTTP $code" }
    else -> (message ?: this::class.java.simpleName).take(200)
}
