package org.argus.droid.net

import android.util.Log
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonObject
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.math.min
import kotlin.math.pow

@Serializable
data class Alert(
    @SerialName("alert_id") val alertId: Long,
    @SerialName("geofence_id") val geofenceId: Long? = null,
    @SerialName("entity_kind") val entityKind: String,
    @SerialName("entity_key") val entityKey: String,
    @SerialName("fired_at") val firedAt: String,
    val severity: String = "info",
    val message: String,
    val attrs: JsonObject = JsonObject(emptyMap()),
    @SerialName("acknowledged_at") val acknowledgedAt: String? = null,
    val lon: Double? = null,
    val lat: Double? = null,
)

@Serializable
private data class AlertsFrame(
    val type: String,
    val count: Int = 0,
    val alerts: List<Alert> = emptyList(),
)

/**
 * The alert socket, held open for as long as the service that owns it.
 *
 * Reconnection is the whole job. A phone loses its network constantly — a lift,
 * a tunnel, a handover from WiFi to mobile — and the server's contract is that
 * an alert raised while a device was away is still pending for that device. So
 * the client's only obligation is to come back; `alerts.delivered_to` does the
 * rest, and there is no cursor to keep or replay request to make.
 *
 * Backoff is capped low deliberately. This is a socket to a daemon on the
 * user's own LAN or tailnet, not a public API being protected from a stampede,
 * and the cost of a missed minute is a notification that arrives late.
 */
class AlertStream(
    private val client: OkHttpClient,
    private val baseUrl: () -> String,
    private val onAlerts: (List<Alert>) -> Unit,
    private val onState: (Boolean) -> Unit,
) {
    private val json = Json { ignoreUnknownKeys = true }
    private val running = AtomicBoolean(false)
    private var socket: WebSocket? = null
    private var attempt = 0
    private val retry = java.util.concurrent.Executors.newSingleThreadScheduledExecutor()

    fun start() {
        if (running.getAndSet(true)) return
        connect()
    }

    fun stop() {
        running.set(false)
        socket?.close(1000, "stopping")
        socket = null
        retry.shutdownNow()
    }

    private fun connect() {
        if (!running.get()) return
        val url = baseUrl().trim().trimEnd('/')
            .replaceFirst("http://", "ws://")
            .replaceFirst("https://", "wss://")
        val request = Request.Builder().url("$url/v1/stream").build()

        socket = client.newWebSocket(
            request,
            object : WebSocketListener() {
                override fun onOpen(webSocket: WebSocket, response: Response) {
                    attempt = 0
                    onState(true)
                    // No viewport: this connection wants notifications, not a
                    // map. The server sends alerts regardless of subscription,
                    // which is why a background service does not have to
                    // pretend to be looking at anything.
                    Log.i(TAG, "alert stream open")
                }

                override fun onMessage(webSocket: WebSocket, text: String) {
                    val frame = runCatching {
                        json.decodeFromString(AlertsFrame.serializer(), text)
                    }.getOrNull() ?: return
                    if (frame.type == "alerts" && frame.alerts.isNotEmpty()) {
                        onAlerts(frame.alerts)
                    }
                }

                override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) {
                    onState(false)
                    Log.w(TAG, "alert stream failed: ${t.message}")
                    scheduleReconnect()
                }

                override fun onClosed(webSocket: WebSocket, code: Int, reason: String) {
                    onState(false)
                    scheduleReconnect()
                }
            },
        )
    }

    private fun scheduleReconnect() {
        if (!running.get() || retry.isShutdown) return
        val delay = min(2.0.pow(attempt.coerceAtMost(5)).toLong(), MAX_BACKOFF_S)
        attempt += 1
        runCatching {
            retry.schedule({ connect() }, delay, java.util.concurrent.TimeUnit.SECONDS)
        }
    }

    private companion object {
        const val TAG = "AlertStream"
        const val MAX_BACKOFF_S = 30L
    }
}
