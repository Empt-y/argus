package org.argus.droid.ui

import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import org.argus.droid.Connection
import org.argus.droid.DVR_WINDOW_HOURS
import org.argus.droid.UiState
import org.argus.droid.net.LayerView
import java.time.Instant
import java.time.ZoneId
import java.time.format.DateTimeFormatter

private val PANEL = Color(0xCC101418)

internal val STAMP: DateTimeFormatter =
    DateTimeFormatter.ofPattern("dd MMM HH:mm:ss").withZone(ZoneId.systemDefault())

@Composable
private fun Panel(modifier: Modifier = Modifier, content: @Composable () -> Unit) {
    Box(
        modifier = modifier
            .clip(RoundedCornerShape(10.dp))
            .background(PANEL)
            .padding(horizontal = 10.dp, vertical = 8.dp),
    ) { content() }
}

/**
 * Whether the server is answering, and what instant the map is showing.
 *
 * The two failure modes are kept apart on purpose. A wrong address and a
 * missing token both produce an empty map, and someone looking at it has no way
 * to tell which they are looking at unless the app says so — one is fixed by
 * typing a different address and the other by scanning a QR.
 */
@Composable
fun StatusBar(
    state: UiState,
    onRetry: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val connection = state.connection
    val (dot, line) = when (connection) {
        is Connection.Connecting -> Color(0xFFF2C94C) to "connecting to ${state.baseUrl}"
        is Connection.Ok -> Color(0xFF6FCF97) to
            "argus ${connection.version} · ${state.layers.size} layers"
        is Connection.Unauthorized -> Color(0xFFEB5757) to "not paired with this server"
        is Connection.Unreachable -> Color(0xFFEB5757) to "unreachable: ${connection.reason}"
    }

    Panel(modifier = modifier.widthIn(max = 300.dp)) {
        Column {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Box(Modifier.size(8.dp).clip(CircleShape).background(dot))
                Spacer(Modifier.width(8.dp))
                Text(
                    text = line,
                    color = Color.White,
                    style = MaterialTheme.typography.labelMedium,
                )
            }
            Spacer(Modifier.size(4.dp))
            Text(
                text = if (state.live) "LIVE" else "DVR ${STAMP.format(state.at)}",
                color = if (state.live) Color(0xFF6FCF97) else Color(0xFFF2C94C),
                fontFamily = FontFamily.Monospace,
                fontWeight = FontWeight.Bold,
                style = MaterialTheme.typography.labelMedium,
            )
            Spacer(Modifier.size(6.dp))
            // Just the one action now. Everything else that used to crowd
            // this panel — sources, alerts, offline, pairing, theme — is a tab,
            // which is the whole point of the tab bar existing.
            HudAction("retry", onRetry)
        }
    }
}

@Composable
private fun HudAction(label: String, onClick: () -> Unit, colour: Color? = null) {
    Text(
        text = label,
        color = colour ?: Color(0xFF56CCF2),
        style = MaterialTheme.typography.labelSmall,
        modifier = Modifier.clickable(onClick = onClick),
    )
}

/**
 * One row per layer the server declares, with its honest health.
 *
 * Note what the count is: `live_entities`, straight from `/v1/layers`. A layer
 * that is configured but has never returned anything reads as zero and grey
 * rather than being quietly left out, because "there is nothing there" and "we
 * are not looking" are different facts and only the server knows which is true.
 */
@Composable
fun LayerRail(
    layers: List<LayerView>,
    hidden: Set<String>,
    live: Boolean,
    onToggle: (String) -> Unit,
    modifier: Modifier = Modifier,
) {
    if (layers.isEmpty()) return
    Panel(modifier = modifier.widthIn(max = 210.dp)) {
        Column(
            modifier = Modifier.verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(6.dp),
        ) {
            layers.forEach { layer ->
                val on = layer.id !in hidden
                Row(
                    verticalAlignment = Alignment.CenterVertically,
                    modifier = Modifier.clickable { onToggle(layer.id) },
                ) {
                    Box(
                        Modifier
                            .size(9.dp)
                            .clip(CircleShape)
                            .background(layer.style.color.toComposeColor().copy(alpha = if (on) 1f else 0.25f))
                    )
                    Spacer(Modifier.width(8.dp))
                    Column(Modifier.weight(1f)) {
                        Text(
                            text = layer.displayName,
                            color = if (on) Color.White else Color(0x66FFFFFF),
                            style = MaterialTheme.typography.labelMedium,
                            // Two lines, because the names are the server's and
                            // some of them are long: "Satellites (CelesTrak,
                            // core groups)" truncated to "Satellites (CelesTrak,
                            // core" reads as a bug in the catalogue.
                            maxLines = 2,
                        )
                        // `/v1/layers` reports the present and has no `at`, so
                        // while the DVR is rewound this count describes now and
                        // not what is drawn. Saying "now" is cheaper than
                        // letting the number quietly contradict the map.
                        Text(
                            text = if (live) "${layer.state} · ${layer.liveEntities}"
                            else "${layer.state} · ${layer.liveEntities} now",
                            color = if (live) layer.state.healthColor()
                            else layer.state.healthColor().copy(alpha = 0.5f),
                            style = MaterialTheme.typography.labelSmall,
                            maxLines = 1,
                        )
                    }
                }
            }
        }
    }
}

/**
 * The scrubber.
 *
 * The slider is anchored to an instant captured when the user starts dragging,
 * not to a `now` that advances underneath them: without that, holding the thumb
 * still walks the map backwards a second per second. The far right of the
 * travel is live, which is a different state from "one second ago" — it is the
 * only position where the map keeps refreshing itself.
 */
@Composable
fun DvrBar(at: Instant?, onSeek: (Instant?) -> Unit, modifier: Modifier = Modifier) {
    var anchor by remember { mutableStateOf(Instant.now()) }
    var position by remember { mutableFloatStateOf(1f) }
    var dragging by remember { mutableStateOf(false) }

    // Coming back to live from elsewhere (or landing here fresh) re-anchors, so
    // the next drag measures from now rather than from whenever this composable
    // was first created.
    if (at == null && !dragging && position != 1f) {
        position = 1f
        anchor = Instant.now()
    }

    val windowSeconds = DVR_WINDOW_HOURS * 3600
    val previewed: Instant? =
        if (position >= 0.999f) null
        else anchor.minusSeconds(((1f - position) * windowSeconds).toLong())

    Panel(modifier = modifier) {
        Column {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    text = "-${DVR_WINDOW_HOURS}h",
                    color = Color(0x99FFFFFF),
                    style = MaterialTheme.typography.labelSmall,
                )
                Slider(
                    value = position,
                    onValueChange = {
                        if (!dragging) {
                            dragging = true
                            anchor = Instant.now()
                        }
                        position = it
                    },
                    onValueChangeFinished = {
                        dragging = false
                        onSeek(previewed)
                    },
                    modifier = Modifier.weight(1f).padding(horizontal = 8.dp),
                )
                Text(
                    text = "live",
                    color = if (previewed == null) Color(0xFF6FCF97) else Color(0x99FFFFFF),
                    style = MaterialTheme.typography.labelSmall,
                    modifier = Modifier.clickable {
                        position = 1f
                        anchor = Instant.now()
                        onSeek(null)
                    },
                )
            }
            Text(
                text = previewed?.let { STAMP.format(it) } ?: "now",
                color = Color.White,
                fontFamily = FontFamily.Monospace,
                style = MaterialTheme.typography.labelSmall,
                modifier = Modifier.align(Alignment.CenterHorizontally),
            )
        }
    }
}

/** `#rrggbb` from the server's layer catalogue. Falls back rather than throwing. */
internal fun String.toComposeColor(): Color = runCatching {
    Color(android.graphics.Color.parseColor(this))
}.getOrDefault(Color(0xFF888888))

internal fun String.healthColor(): Color = when (this) {
    "live" -> Color(0xFF6FCF97)
    "degraded", "stale" -> Color(0xFFF2C94C)
    "failed", "unconfigured" -> Color(0xFFEB5757)
    else -> Color(0x99FFFFFF)
}
