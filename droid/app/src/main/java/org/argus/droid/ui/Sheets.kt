package org.argus.droid.ui

import android.os.Build
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
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
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.ModalBottomSheet
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import kotlinx.serialization.json.JsonPrimitive
import org.argus.droid.Selection
import org.argus.droid.net.Alert
import org.argus.droid.net.GeofenceView
import org.argus.droid.net.SourceRow
import java.time.Instant
import kotlin.math.roundToInt

/**
 * Everything known about one contact.
 *
 * The layout follows one rule: values that describe the instant the map is
 * showing come from the feature that was tapped, and values that describe the
 * entity in general come from the detail fetch. Mixing them would let a card
 * report a satellite's live altitude under a heading saying the map is rewound
 * two hours, which is the kind of quiet lie a DVR exists to avoid.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun EntitySheet(
    selection: Selection,
    at: Instant?,
    onDismiss: () -> Unit,
    onGoTo: (Double, Double) -> Unit,
) {
    val tapped = selection.tapped
    val detail = selection.detail

    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            modifier = Modifier
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp)
                .heightIn(max = 520.dp)
                .verticalScroll(rememberScrollState()),
        ) {
            Text(
                text = tapped.label ?: tapped.key,
                style = MaterialTheme.typography.titleLarge,
                fontWeight = FontWeight.SemiBold,
            )
            Text(
                text = buildString {
                    append(tapped.kind)
                    append(" · ")
                    append(tapped.key)
                    detail?.layerId?.let { append(" · $it") }
                },
                style = MaterialTheme.typography.labelMedium,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Spacer(Modifier.size(12.dp))
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(
                    text = tapped.quality?.uppercase() ?: "UNKNOWN",
                    style = MaterialTheme.typography.labelSmall,
                    fontFamily = FontFamily.Monospace,
                    color = when (tapped.quality) {
                        "live" -> Color(0xFF2E9E63)
                        "delayed" -> Color(0xFFB8860B)
                        else -> MaterialTheme.colorScheme.onSurfaceVariant
                    },
                )
                Spacer(Modifier.size(10.dp))
                Text(
                    text = tapped.observedAt?.let { "fixed ${STAMP.format(it)}" } ?: "no fix time",
                    style = MaterialTheme.typography.labelSmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            Spacer(Modifier.size(14.dp))
            Field("position", "%.5f, %.5f".format(tapped.lat, tapped.lon))
            tapped.altM?.let {
                Field(
                    "altitude",
                    "${it.roundToInt()} m" + (detail?.altDatum?.let { d -> " ($d)" } ?: ""),
                )
            }
            tapped.courseDeg?.let { Field("course", "${it.roundToInt()}°") }
            tapped.speedMps?.let {
                Field("speed", "${it.roundToInt()} m/s · ${(it * 1.94384f).roundToInt()} kt")
            }
            detail?.vrateMps?.let { Field("vertical rate", "%+.1f m/s".format(it)) }
            Field("reported by", tapped.sourceId ?: detail?.sourceId ?: "—")

            Spacer(Modifier.size(10.dp))
            HorizontalDivider()
            Spacer(Modifier.size(10.dp))

            when {
                selection.loading -> Text(
                    "loading history…",
                    style = MaterialTheme.typography.bodySmall,
                )
                selection.track.isEmpty() -> Text(
                    "no history in the last two hours",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
                else -> Text(
                    "${selection.track.size} fixes drawn, ending ${
                        at?.let { STAMP.format(it) } ?: "now"
                    }",
                    style = MaterialTheme.typography.bodySmall,
                )
            }

            // Said out loud rather than hidden: `/v1/entities/{kind}/{key}` has
            // no `at`, so while the DVR is rewound the extra attributes below
            // are the entity's present state, not its state then. The values
            // above are from the tile and are correct for the instant shown.
            if (at != null && detail != null) {
                Spacer(Modifier.size(6.dp))
                Text(
                    "attributes below are current, not as of ${STAMP.format(at)}",
                    style = MaterialTheme.typography.labelSmall,
                    color = Color(0xFFB8860B),
                )
            }

            detail?.attrs?.takeIf { it.isNotEmpty() }?.let { attrs ->
                Spacer(Modifier.size(10.dp))
                attrs.entries.sortedBy { it.key }.forEach { (name, value) ->
                    Field(name, (value as? JsonPrimitive)?.content ?: value.toString())
                }
            }

            selection.error?.let {
                Spacer(Modifier.size(10.dp))
                Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
            }

            Spacer(Modifier.size(14.dp))
            TextButton(onClick = { onGoTo(tapped.lat, tapped.lon) }) { Text("centre on this") }
        }
    }
}

@Composable
internal fun Field(name: String, value: String) {
    Row(
        modifier = Modifier.fillMaxWidth().padding(vertical = 2.dp),
        horizontalArrangement = Arrangement.SpaceBetween,
    ) {
        Text(
            name,
            style = MaterialTheme.typography.bodySmall,
            color = MaterialTheme.colorScheme.onSurfaceVariant,
        )
        Text(
            value,
            style = MaterialTheme.typography.bodySmall,
            fontFamily = FontFamily.Monospace,
        )
    }
}

/** Per-provider health, including the error text when there is one. */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SourceSheet(sources: List<SourceRow>, onDismiss: () -> Unit) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            modifier = Modifier
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp)
                .heightIn(max = 520.dp)
                .verticalScroll(rememberScrollState()),
        ) {
            Text("sources", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.size(10.dp))
            if (sources.isEmpty()) {
                Text("nothing reported yet", style = MaterialTheme.typography.bodySmall)
            }
            // Server order already nests each chain immediately above its own
            // providers, so indentation is all this needs — no grouping pass,
            // and a layer that gains a provider tomorrow renders correctly
            // without an app release.
            sources.forEach { source ->
                val member = source.memberOf != null
                Column(
                    Modifier
                        .padding(vertical = 6.dp)
                        .padding(start = if (member) 16.dp else 0.dp)
                ) {
                    Row(horizontalArrangement = Arrangement.SpaceBetween, modifier = Modifier.fillMaxWidth()) {
                        Text(
                            text = if (member) "↳ ${source.displayName}" else source.displayName,
                            style = if (member) MaterialTheme.typography.bodySmall
                            else MaterialTheme.typography.bodyMedium,
                            color = if (member) MaterialTheme.colorScheme.onSurfaceVariant
                            else MaterialTheme.colorScheme.onSurface,
                        )
                        Text(
                            source.state,
                            style = MaterialTheme.typography.labelSmall,
                            color = source.state.healthColor(),
                        )
                    }
                    Text(
                        buildString {
                            // A member's layer is its chain's; naming it again
                            // on every row is noise.
                            if (!member) {
                                append(source.layerId)
                                append(" · ")
                            }
                            append(source.observations)
                            append(" obs")
                            source.lastLagMs?.let { append(" · ${it} ms lag") }
                            source.lastSuccess?.let {
                                append(" · last ")
                                append(runCatching { STAMP.format(Instant.parse(it)) }.getOrDefault(it))
                            }
                        },
                        style = MaterialTheme.typography.labelSmall,
                        color = MaterialTheme.colorScheme.onSurfaceVariant,
                    )
                    // The error is the useful part of an unhealthy source and
                    // the whole reason this sheet exists rather than a colour.
                    source.lastError?.let {
                        Text(
                            it,
                            style = MaterialTheme.typography.labelSmall,
                            color = MaterialTheme.colorScheme.error,
                        )
                    }
                }
            }
        }
    }
}

/**
 * Point this device at a daemon, and get a token from it.
 *
 * The code is typed rather than scanned. The daemon's QR encodes
 * `argus://pair?server=…&code=…`, which the phone's own camera app already
 * knows how to open — this app declares that scheme, so scanning it lands here
 * with both fields filled in. Building a second QR scanner into the app would
 * add a camera permission and a vision library to duplicate something the
 * platform does better.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PairingSheet(
    baseUrl: String,
    initialCode: String = "",
    paired: Boolean,
    onDismiss: () -> Unit,
    onSetServer: (String) -> Unit,
    onPair: (String, String, String, (String?) -> Unit) -> Unit,
    onUnpair: () -> Unit,
) {
    var server by remember { mutableStateOf(baseUrl) }
    var code by remember { mutableStateOf(initialCode) }
    var name by remember { mutableStateOf(Build.MODEL ?: "android") }
    var busy by remember { mutableStateOf(false) }
    var message by remember { mutableStateOf<String?>(null) }

    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            modifier = Modifier
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp)
                .verticalScroll(rememberScrollState()),
        ) {
            Text("server", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.size(8.dp))
            OutlinedTextField(
                value = server,
                onValueChange = { server = it },
                label = { Text("daemon address") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.size(6.dp))
            Text(
                if (paired) "this device is paired" else "not paired — loopback only",
                style = MaterialTheme.typography.labelSmall,
                color = if (paired) Color(0xFF2E9E63) else MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.size(8.dp))
            TextButton(onClick = { onSetServer(server); onDismiss() }) {
                Text("use this address")
            }

            Spacer(Modifier.size(8.dp))
            HorizontalDivider()
            Spacer(Modifier.size(12.dp))

            Text("pair", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.size(4.dp))
            Text(
                "argusd prints a QR and a code on its console. Scan it with the " +
                    "camera to fill this in, or type the code.",
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Spacer(Modifier.size(8.dp))
            OutlinedTextField(
                value = code,
                onValueChange = { code = it },
                label = { Text("pairing code") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            Spacer(Modifier.size(8.dp))
            OutlinedTextField(
                value = name,
                onValueChange = { name = it },
                label = { Text("device name") },
                singleLine = true,
                modifier = Modifier.fillMaxWidth(),
            )
            message?.let {
                Spacer(Modifier.size(8.dp))
                Text(it, color = MaterialTheme.colorScheme.error, style = MaterialTheme.typography.bodySmall)
            }
            Spacer(Modifier.size(12.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
                Button(
                    enabled = !busy && code.isNotBlank() && name.isNotBlank(),
                    onClick = {
                        busy = true
                        message = null
                        onPair(server, code, name) { error ->
                            busy = false
                            if (error == null) onDismiss() else message = error
                        }
                    },
                ) { Text(if (busy) "pairing…" else "pair") }
                if (paired) {
                    TextButton(onClick = { onUnpair(); onDismiss() }) { Text("forget token") }
                }
            }
        }
    }
}

/**
 * What the geofences have caught, newest first.
 *
 * Acknowledging is a server-side act rather than a local dismissal: an alert
 * dealt with on the phone should not reappear on the tablet an hour later as
 * though it were news, and `alerts.acknowledged_at` is what makes that true for
 * every client at once.
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun AlertSheet(
    alerts: List<Alert>,
    geofences: List<GeofenceView>,
    onAcknowledge: (Long) -> Unit,
    onDismiss: () -> Unit,
    onGoTo: (Double, Double) -> Unit,
) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            modifier = Modifier
                .padding(horizontal = 20.dp)
                .padding(bottom = 28.dp)
                .heightIn(max = 540.dp)
                .verticalScroll(rememberScrollState()),
        ) {
            Text("alerts", style = MaterialTheme.typography.titleMedium)
            Spacer(Modifier.size(4.dp))
            Text(
                if (geofences.isEmpty()) {
                    "No geofences are armed. Draw one with POST /v1/geofences and " +
                        "it appears on the map within thirty seconds."
                } else {
                    geofences.joinToString(", ") { it.name } +
                        " · ${geofences.count { it.enabled }} armed"
                },
                style = MaterialTheme.typography.bodySmall,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )

            Spacer(Modifier.size(12.dp))
            if (alerts.isEmpty()) {
                Text(
                    "nothing has fired",
                    style = MaterialTheme.typography.bodySmall,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }

            alerts.forEach { alert ->
                val acked = alert.acknowledgedAt != null
                Column(Modifier.padding(vertical = 6.dp)) {
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                    ) {
                        Column(Modifier.weight(1f)) {
                            Text(
                                alert.message,
                                style = MaterialTheme.typography.bodyMedium,
                                color = if (acked) MaterialTheme.colorScheme.onSurfaceVariant
                                else MaterialTheme.colorScheme.onSurface,
                            )
                            Text(
                                buildString {
                                    append(
                                        runCatching { STAMP.format(Instant.parse(alert.firedAt)) }
                                            .getOrDefault(alert.firedAt)
                                    )
                                    append(" · ")
                                    append(alert.severity)
                                    alert.attrs["alt_m"]?.let { append(" · ${it}m") }
                                },
                                style = MaterialTheme.typography.labelSmall,
                                fontFamily = FontFamily.Monospace,
                                color = alert.severity.severityColour(),
                            )
                        }
                        if (!acked) {
                            TextButton(onClick = { onAcknowledge(alert.alertId) }) { Text("ack") }
                        }
                    }
                    if (alert.lat != null && alert.lon != null) {
                        TextButton(onClick = { onGoTo(alert.lat, alert.lon) }) {
                            Text("show where", style = MaterialTheme.typography.labelSmall)
                        }
                    }
                }
                HorizontalDivider()
            }
        }
    }
}

private fun String.severityColour(): Color = when (this) {
    "critical" -> Color(0xFFEB5757)
    "warning" -> Color(0xFFF2994A)
    "notice" -> Color(0xFFF2C94C)
    else -> Color(0xFF9E9E9E)
}
