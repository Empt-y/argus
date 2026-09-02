package org.argus.droid.ui

import android.graphics.PointF
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import org.argus.droid.ArgusViewModel
import org.argus.droid.PairLink
import org.maplibre.android.camera.CameraPosition
import org.maplibre.android.geometry.LatLng
import org.maplibre.android.maps.MapLibreMap
import org.maplibre.android.maps.Style

/**
 * The whole client: a map the server describes, and four overlays that let a
 * person interrogate it.
 *
 * Nothing here knows what a layer is. The style comes from `/v1/style.json`, the
 * rail comes from `/v1/layers`, and the card comes from whatever tags the tiler
 * put on the feature under the finger. A driver added to the daemon this
 * afternoon appears in all three without a line changing in this file — which
 * is the claim the Android client exists to test.
 */
@Composable
fun MapScreen(
    vm: ArgusViewModel,
    pairLink: PairLink? = null,
    onPairLinkHandled: () -> Unit = {},
) {
    val state by vm.state.collectAsStateWithLifecycle()
    val mapView = rememberMapView()
    var map by remember { mutableStateOf<MapLibreMap?>(null) }
    var style by remember { mutableStateOf<Style?>(null) }
    var showPairing by remember { mutableStateOf(false) }
    var showSources by remember { mutableStateOf(false) }
    var showOffline by remember { mutableStateOf(false) }
    var showAlerts by remember { mutableStateOf(false) }
    var showTheme by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) { vm.startPolling() }
    LaunchedEffect(pairLink) { if (pairLink != null) showPairing = true }

    // One effect per thing that can change, rather than one that rebuilds the
    // map: a style reload costs a visible flash, so it must happen when the
    // style URL changes and at no other time.
    LaunchedEffect(mapView) {
        mapView.getMapAsync { loaded ->
            loaded.cameraPosition = CameraPosition.Builder()
                .target(LatLng(51.47, -0.45))
                .zoom(8.0)
                .build()
            loaded.addOnMapClickListener { point ->
                val current = loaded.style ?: return@addOnMapClickListener false
                val screen = loaded.projection.toScreenLocation(point)
                val hit = loaded.featureAt(PointF(screen.x, screen.y), current)
                if (hit != null) vm.select(hit) else vm.clearSelection()
                hit != null
            }
            map = loaded
        }
    }

    LaunchedEffect(map, state.styleUrl, state.styleNonce) {
        val loaded = map ?: return@LaunchedEffect
        style = null
        loaded.setStyle(state.styleUrl) { newStyle ->
            // The style says which basemaps exist, so the picker needs no
            // second endpoint and no hard-coded list — the same arrangement
            // that lets a new layer reach the phone without an app release.
            vm.noteBasemapsOffered(basemapsFrom(newStyle.json))
            // Images first: a symbol layer whose icon is not registered draws
            // nothing at all, so the glyphs have to exist before the style is
            // handed on to anything that might render it.
            Sprites.install(newStyle, state.layers)
            newStyle.installTrackLayers()
            style = newStyle
        }
    }

    // Visibility is re-applied whenever either side of it moves: the user's
    // choices, or a style that has just been replaced and knows nothing of them.
    LaunchedEffect(style, state.layers) {
        // The layer catalogue and the style load independently, so whichever
        // lands second has to fill in what the first could not: a style loaded
        // before `/v1/layers` answered has no per-layer glyphs and would draw
        // every contact with the fallback marker.
        val current = style ?: return@LaunchedEffect
        if (state.layers.isNotEmpty()) Sprites.install(current, state.layers)
    }

    LaunchedEffect(style, state.hidden, state.layers) {
        val current = style ?: return@LaunchedEffect
        state.layers.forEach { layer ->
            current.setLayerVisible(layer.id, layer.id !in state.hidden)
        }
    }

    LaunchedEffect(style, state.selection?.track) {
        style?.showTrack(state.selection?.track.orEmpty())
    }

    LaunchedEffect(style, state.geofences) {
        style?.showGeofences(state.geofences)
    }

    Box(modifier = Modifier.fillMaxSize()) {
        // The map goes edge to edge — it is a map, and a bezel of chrome around
        // it is wasted glass. The overlays do not: with targetSdk 35 the system
        // draws its bars over the window, so without this the status chip sits
        // under the clock and the scrubber under the navigation bar, which is
        // exactly where a thumb tries to drag it.
        AndroidView(modifier = Modifier.fillMaxSize(), factory = { mapView })

        Box(modifier = Modifier.fillMaxSize().windowInsetsPadding(WindowInsets.safeDrawing)) {
        StatusBar(
            state = state,
            onRetry = vm::refreshCatalogue,
            onPair = { showPairing = true },
            onSources = { showSources = true },
            onOffline = { vm.refreshRegions(); showOffline = true },
            onAlerts = { showAlerts = true },
            onTheme = { showTheme = true },
            modifier = Modifier.align(Alignment.TopStart).padding(12.dp),
        )

        LayerRail(
            layers = state.layers,
            hidden = state.hidden,
            live = state.live,
            onToggle = vm::toggleLayer,
            modifier = Modifier.align(Alignment.TopEnd).padding(top = 12.dp, end = 12.dp),
        )

        DvrBar(
            at = state.at,
            onSeek = vm::seek,
            modifier = Modifier
                .align(Alignment.BottomCenter)
                .padding(horizontal = 12.dp, vertical = 16.dp),
        )
        }
    }

    state.selection?.let { selection ->
        EntitySheet(
            selection = selection,
            at = state.at,
            onDismiss = vm::clearSelection,
            onGoTo = { lat, lon ->
                map?.cameraPosition = CameraPosition.Builder()
                    .target(LatLng(lat, lon))
                    .zoom(maxOf(map?.cameraPosition?.zoom ?: 8.0, 10.0))
                    .build()
            },
        )
    }

    if (showPairing) {
        PairingSheet(
            baseUrl = pairLink?.server ?: state.baseUrl,
            initialCode = pairLink?.code.orEmpty(),
            paired = state.paired,
            onDismiss = { showPairing = false; onPairLinkHandled() },
            onSetServer = vm::setServer,
            onPair = vm::pair,
            onUnpair = vm::unpair,
        )
    }

    if (showSources) {
        SourceSheet(sources = state.sources, onDismiss = { showSources = false })
    }

    if (showTheme) {
        ThemeSheet(
            offered = state.basemapsOffered,
            current = state.basemap,
            onPick = { vm.setBasemap(it); showTheme = false },
            onDismiss = { showTheme = false },
        )
    }

    if (showAlerts) {
        AlertSheet(
            alerts = state.alerts,
            geofences = state.geofences,
            onAcknowledge = vm::acknowledgeAlert,
            onDismiss = { showAlerts = false },
            onGoTo = { lat, lon ->
                map?.cameraPosition = CameraPosition.Builder()
                    .target(LatLng(lat, lon))
                    .zoom(maxOf(map?.cameraPosition?.zoom ?: 8.0, 11.0))
                    .build()
            },
        )
    }

    if (showOffline) {
        OfflineSheet(
            // The box on screen right now: an offline area you chose by looking
            // at it needs no second map to draw a rectangle on.
            bounds = map?.projection?.visibleRegion?.latLngBounds,
            regions = state.regions,
            progress = state.download,
            onDismiss = { showOffline = false },
            onDownload = { name, minZoom, maxZoom ->
                map?.projection?.visibleRegion?.latLngBounds?.let {
                    vm.downloadRegion(it, name, minZoom, maxZoom)
                }
            },
            onDelete = vm::deleteRegion,
        )
    }
}

/**
 * The basemap names a style document offers.
 *
 * Parsed from the raw JSON because MapLibre's `Style` exposes no accessor for
 * `metadata`. Failing quietly to an empty list is right: the picker simply has
 * nothing to offer against a server too old to advertise any.
 */
private fun basemapsFrom(styleJson: String): List<String> = runCatching {
    val metadata = org.json.JSONObject(styleJson).optJSONObject("metadata")
        ?: return emptyList()
    val names = metadata.optJSONArray("argus:basemaps") ?: return emptyList()
    (0 until names.length()).mapNotNull { names.optString(it).takeIf(String::isNotBlank) }
}.getOrDefault(emptyList())
