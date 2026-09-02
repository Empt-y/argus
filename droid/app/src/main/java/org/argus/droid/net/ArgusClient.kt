package org.argus.droid.net

import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull
import okhttp3.Interceptor
import okhttp3.MediaType.Companion.toMediaType
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.RequestBody.Companion.toRequestBody
import okhttp3.Response
import org.argus.droid.Settings
import java.io.IOException
import java.time.Instant
import java.time.format.DateTimeFormatter
import java.util.concurrent.TimeUnit

/** A request that reached the server and was refused, with the server's reason. */
class ApiException(val code: Int, val detail: String) : IOException("HTTP $code: $detail")

/**
 * Every call this app makes to the daemon, and the one HTTP client the map
 * shares with it.
 *
 * The shared client is not an optimisation. MapLibre fetches tiles and the
 * style through its own process-global `Call.Factory`, and those requests need
 * the same bearer token as everything else — so either they go through this
 * client or the map is the one part of the app that cannot authenticate.
 */
class ArgusClient(private val settings: Settings) {

    private val json = Json {
        ignoreUnknownKeys = true
        explicitNulls = false
    }

    val http: OkHttpClient = OkHttpClient.Builder()
        .connectTimeout(5, TimeUnit.SECONDS)
        .readTimeout(20, TimeUnit.SECONDS)
        .addInterceptor(BearerToOwnServerOnly(settings))
        .build()

    private val base: String get() = settings.baseUrl.value

    /**
     * The style to display: an instant if the DVR is rewound, and the chosen
     * basemap if there is one.
     *
     * Both are query parameters on one document, so the whole visual state of
     * the map is a single URL — which is what makes changing either of them one
     * `setStyle` call rather than a pile of in-place edits.
     */
    fun styleUrl(at: Instant?, basemap: String? = null): String {
        val params = buildList {
            if (at != null) add("at=${INSTANT.format(at)}")
            if (!basemap.isNullOrBlank()) add("basemap=$basemap")
        }
        return if (params.isEmpty()) "$base/v1/style.json"
        else "$base/v1/style.json?${params.joinToString("&")}"
    }

    /**
     * Ground with nothing on it — what an offline region is cut from.
     *
     * MapLibre's offline manager downloads every tile the style it is given
     * references, so handing it the full style would package the live layers as
     * well and replay an hour-old sky as current.
     */
    fun basemapStyleUrl(): String = "$base/v1/style.json?basemap_only=true"

    suspend fun health(): Health = get("/v1/health")

    suspend fun layers(): List<LayerView> = get<LayersResponse>("/v1/layers").layers

    suspend fun sources(): List<SourceRow> = get<SourcesResponse>("/v1/sources").sources

    suspend fun entity(kind: String, key: String): EntityRow =
        get("/v1/entities/$kind/${enc(key)}")

    /**
     * One entity's history, ending at [to].
     *
     * The window ends where the map is looking rather than at `now`: a track
     * drawn under a contact the DVR has rewound to must not run on ahead of it.
     */
    suspend fun track(kind: String, key: String, to: Instant?, hours: Long = 2): TrackResponse {
        val end = to ?: Instant.now()
        val from = end.minusSeconds(hours * 3600)
        return get(
            "/v1/entities/$kind/${enc(key)}/track" +
                "?from=${INSTANT.format(from)}&to=${INSTANT.format(end)}"
        )
    }

    suspend fun geofences(): List<GeofenceView> =
        get<GeofencesResponse>("/v1/geofences").geofences

    suspend fun alerts(limit: Int = 100): List<Alert> =
        get<AlertsResponse>("/v1/alerts?limit=$limit").alerts

    suspend fun acknowledgeAlert(alertId: Long) = withContext(Dispatchers.IO) {
        val request = Request.Builder()
            .url("$base/v1/alerts/$alertId/ack")
            .post("".toRequestBody(null))
            .build()
        http.newCall(request).execute().use { it.bodyOrThrow() }
    }

    /**
     * Redeem a pairing code for this device's bearer token.
     *
     * Deliberately takes the server URL as an argument rather than reading the
     * configured one: pairing is the moment the server address is being decided,
     * and reading it back from settings would make the order of two writes
     * matter.
     */
    suspend fun pair(serverUrl: String, code: String, deviceName: String): PairResponse =
        withContext(Dispatchers.IO) {
            val url = serverUrl.trim().trimEnd('/')
            val body = json.encodeToString(PairRequest.serializer(), PairRequest(code.trim(), deviceName))
            val request = Request.Builder()
                .url("$url/v1/pair")
                .post(body.toRequestBody(JSON_MEDIA))
                .build()
            http.newCall(request).execute().use { response ->
                json.decodeFromString(PairResponse.serializer(), response.bodyOrThrow())
            }
        }

    private suspend inline fun <reified T> get(path: String): T = withContext(Dispatchers.IO) {
        val request = Request.Builder().url(base + path).build()
        http.newCall(request).execute().use { response ->
            json.decodeFromString<T>(response.bodyOrThrow())
        }
    }

    private fun Response.bodyOrThrow(): String {
        val text = body?.string().orEmpty()
        if (!isSuccessful) {
            Log.w(TAG, "${request.url} -> $code")
            throw ApiException(code, text.take(400).ifBlank { message })
        }
        return text
    }

    private fun enc(segment: String): String =
        java.net.URLEncoder.encode(segment, "UTF-8").replace("+", "%20")

    /**
     * Attaches the bearer token, and only to the daemon.
     *
     * The scoping is the important half. MapLibre uses this same client for the
     * basemap, which is somebody else's tile server on the public internet — an
     * interceptor that added the header unconditionally would hand this
     * device's credential to every host the style happens to name. So the token
     * goes out only to the host and port the app is configured to talk to.
     */
    private class BearerToOwnServerOnly(private val settings: Settings) : Interceptor {
        override fun intercept(chain: Interceptor.Chain): Response {
            val token = settings.token.value
            val server = settings.baseUrl.value.toHttpUrlOrNull()
            val request = chain.request()
            return if (token != null && server != null && request.url.sameEndpoint(server)) {
                chain.proceed(
                    request.newBuilder()
                        .header("Authorization", "Bearer $token")
                        .build()
                )
            } else {
                chain.proceed(request)
            }
        }

        private fun HttpUrl.sameEndpoint(other: HttpUrl): Boolean =
            host == other.host && port == other.port && scheme == other.scheme
    }

    companion object {
        private const val TAG = "ArgusClient"
        private val JSON_MEDIA = "application/json".toMediaType()

        /**
         * `Z`-form, to match what the server normalises to. A numeric `+00:00`
         * offset decodes to a space in a query string; the daemon repairs that,
         * but a URL that never needs rescuing is better than one that does.
         */
        val INSTANT: DateTimeFormatter = DateTimeFormatter.ISO_INSTANT
    }
}
