package org.argus.droid.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import org.argus.droid.DownloadProgress
import org.argus.droid.StoredRegion
import org.argus.droid.estimateBytes
import org.argus.droid.estimateTiles
import org.maplibre.android.geometry.LatLngBounds

/**
 * Keep the ground for the box on screen.
 *
 * Only the basemap is downloaded — see [org.argus.droid.Offline] for why. The
 * sheet says so out loud, because "offline maps" on a spatial-intelligence tool
 * would otherwise read as a promise that the contacts come too, and a phone
 * confidently drawing yesterday's aircraft in a dead spot is worse than one
 * drawing none.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun OfflineContent(
    bounds: LatLngBounds?,
    regions: List<StoredRegion>,
    progress: DownloadProgress?,
    onDownload: (String, Int, Int) -> Unit,
    onDelete: (Long) -> Unit,
    modifier: Modifier = Modifier,
) {
    var maxZoom by remember { mutableIntStateOf(12) }
    var name by remember { mutableStateOf("") }
    val minZoom = 6

    Box(modifier) {
        Column(
            modifier = Modifier
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp),
        ) {
            Text("offline ground", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.size(4.dp))
            Text(
                "Downloads the basemap for the area on screen. Contacts are not " +
                    "stored — with no signal the map shows ground and no traffic, " +
                    "rather than traffic that has stopped being true.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Spacer(Modifier.size(14.dp))
            if (bounds == null) {
                Text("waiting for the map", style = MaterialTheme.typography.bodySmall)
            } else {
                val tiles = estimateTiles(bounds, minZoom, maxZoom)
                Field(
                    "area",
                    "%.2f, %.2f → %.2f, %.2f".format(
                        bounds.latitudeSouth, bounds.longitudeWest,
                        bounds.latitudeNorth, bounds.longitudeEast,
                    ),
                )
                Field("zoom", "$minZoom – $maxZoom")
                // Each level quadruples the count, so the number is shown before
                // the button rather than discovered as a stalled download.
                Field("tiles", "≈ $tiles · ~${estimateBytes(tiles).asMib()}")
                Slider(
                    value = maxZoom.toFloat(),
                    onValueChange = { maxZoom = it.toInt() },
                    valueRange = 8f..15f,
                    steps = 6,
                    modifier = Modifier.fillMaxWidth(),
                )
                Spacer(Modifier.size(8.dp))
                Button(
                    enabled = progress == null || progress.complete || progress.error != null,
                    onClick = {
                        onDownload(
                            name.ifBlank {
                                "%.2f,%.2f".format(bounds.latitudeNorth, bounds.longitudeWest)
                            },
                            minZoom,
                            maxZoom,
                        )
                    },
                ) { Text("download this area") }
            }

            progress?.let {
                Spacer(Modifier.size(12.dp))
                when {
                    it.error != null -> Text(
                        it.error,
                        color = MaterialTheme.colorScheme.error,
                        style = MaterialTheme.typography.bodySmall,
                    )
                    it.complete -> Text(
                        "${it.name}: done — ${it.tiles} tiles, ${it.bytes.asMib()}",
                        style = MaterialTheme.typography.bodySmall,
                    )
                    else -> Column {
                        Text(
                            "${it.name}: ${it.tiles} tiles, ${it.bytes.asMib()}",
                            style = MaterialTheme.typography.bodySmall,
                        )
                        Spacer(Modifier.size(6.dp))
                        LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                    }
                }
            }

            Spacer(Modifier.size(14.dp))
            HorizontalDivider()
            Spacer(Modifier.size(10.dp))
            Text("stored", style = MaterialTheme.typography.titleSmall)
            Spacer(Modifier.size(6.dp))
            if (regions.isEmpty()) {
                Text(
                    "nothing downloaded",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
            regions.forEach { region ->
                Row(
                    modifier = Modifier.fillMaxWidth().padding(vertical = 4.dp),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    Column(Modifier.weight(1f)) {
                        Text(region.name, style = MaterialTheme.typography.bodyMedium)
                        Text(
                            buildString {
                                append("z${region.minZoom}–${region.maxZoom}")
                                if (region.complete) {
                                    append(" · ${region.tiles} tiles · ${region.bytes.asMib()}")
                                } else {
                                    // An interrupted download leaves a real
                                    // region holding real tiles; saying so beats
                                    // showing it as though it were whole.
                                    append(" · incomplete")
                                }
                            },
                            style = MaterialTheme.typography.labelSmall,
                            fontFamily = FontFamily.Monospace,
                            color = MaterialTheme.colorScheme.onSurfaceVariant,
                        )
                    }
                    TextButton(onClick = { onDelete(region.id) }) { Text("delete") }
                }
            }
        }
    }
}

internal fun Long.asMib(): String = "%.1f MiB".format(this / 1024.0 / 1024.0)
