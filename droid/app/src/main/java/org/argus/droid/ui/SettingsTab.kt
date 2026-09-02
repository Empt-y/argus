package org.argus.droid.ui

import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import org.argus.droid.ArgusViewModel
import org.argus.droid.PairLink

/**
 * Everything that configures this device rather than describing the world.
 *
 * One scrolling page rather than four modal sheets fighting for the same
 * upward gesture. The order is the order someone actually needs them in: reach
 * the server, then choose how it looks, then decide what to keep offline.
 */
@Composable
fun SettingsTab(
    vm: ArgusViewModel,
    pairLink: PairLink? = null,
    onPairLinkHandled: () -> Unit = {},
    modifier: Modifier = Modifier,
) {
    val state by vm.state.collectAsStateWithLifecycle()

    // Downloaded regions are the one thing here the app does not already know:
    // they live in MapLibre's own store rather than in our state.
    LaunchedEffect(Unit) { vm.refreshRegions() }

    Column(
        modifier = modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .padding(bottom = 28.dp),
    ) {
        PairingContent(
            baseUrl = pairLink?.server ?: state.baseUrl,
            initialCode = pairLink?.code.orEmpty(),
            paired = state.paired,
            onSetServer = vm::setServer,
            onPair = { server, code, name, done ->
                vm.pair(server, code, name) { error ->
                    if (error == null) onPairLinkHandled()
                    done(error)
                }
            },
            onUnpair = vm::unpair,
        )

        Spacer(Modifier.size(8.dp))
        HorizontalDivider()
        Spacer(Modifier.size(16.dp))

        BasemapContent(
            offered = state.basemapsOffered,
            current = state.basemap,
            onPick = vm::setBasemap,
        )

        Spacer(Modifier.size(8.dp))
        HorizontalDivider()
        Spacer(Modifier.size(16.dp))

        OfflineContent(
            bounds = vm.lastViewport,
            regions = state.regions,
            progress = state.download,
            onDownload = { name, minZoom, maxZoom ->
                vm.lastViewport?.let { vm.downloadRegion(it, name, minZoom, maxZoom) }
            },
            onDelete = vm::deleteRegion,
        )

        Spacer(Modifier.size(24.dp))
        Text(
            "argus ${(state.connection as? org.argus.droid.Connection.Ok)?.version ?: "—"} · " +
                state.baseUrl,
            style = MaterialTheme.typography.labelSmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
            modifier = Modifier.padding(horizontal = 20.dp),
        )
    }
}
