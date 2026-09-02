package org.argus.droid

import android.content.Context
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow

/**
 * Where the daemon is, and what this device may say to it.
 *
 * The one thing that is genuinely different about the Android client: `localhost`
 * on a phone is the phone. Under the emulator the host loopback is reachable at
 * 10.0.2.2; over USB `adb reverse tcp:8787 tcp:8787` puts the development
 * machine's daemon on the handset's own loopback; on a LAN or a tailnet it is a
 * real address. Only the first two land on the daemon as 127.0.0.1, which is
 * what its `loopback_exempt` policy needs, so anything else requires a token.
 *
 * Both values are persisted, because a phone that must be re-paired every time
 * it is restarted is a phone nobody carries. They live in plain
 * `SharedPreferences`, which is private to this app's uid and is as far as this
 * goes: `EncryptedSharedPreferences` would protect the token from someone who
 * has already rooted the device and taken the keystore with it, which is not
 * the threat this token is for. The token's defence is that it is revocable —
 * `DELETE /v1/devices/{id}` from any other paired client.
 */
class Settings private constructor(context: Context) {
    private val prefs = context.applicationContext
        .getSharedPreferences("argus", Context.MODE_PRIVATE)

    private val _baseUrl = MutableStateFlow(
        prefs.getString(KEY_BASE_URL, null) ?: EMULATOR_HOST
    )
    val baseUrl: StateFlow<String> = _baseUrl.asStateFlow()

    private val _token = MutableStateFlow(prefs.getString(KEY_TOKEN, null))
    val token: StateFlow<String?> = _token.asStateFlow()

    /** Which device this token belongs to, so it can be named when revoking. */
    val deviceId: String? get() = prefs.getString(KEY_DEVICE_ID, null)

    /**
     * The basemap the user picked, or null for whatever the server defaults to.
     *
     * Stored by *name* rather than by URL or position: the server owns the list,
     * and a remembered index would silently come to mean a different map the
     * next time the config changed.
     */
    private val _basemap = MutableStateFlow(prefs.getString(KEY_BASEMAP, null))
    val basemap: StateFlow<String?> = _basemap.asStateFlow()

    fun setBasemap(name: String?) {
        prefs.edit().apply { if (name == null) remove(KEY_BASEMAP) else putString(KEY_BASEMAP, name) }.apply()
        _basemap.value = name
    }

    fun setBaseUrl(url: String) {
        val cleaned = url.trim().trimEnd('/')
        if (cleaned.isEmpty() || cleaned == _baseUrl.value) return
        // A token is issued by one server and means nothing to another, so
        // moving the address drops it rather than leaving a token that will
        // produce 401s the user has no way to explain.
        val serverChanged = cleaned != _baseUrl.value
        prefs.edit().putString(KEY_BASE_URL, cleaned).apply()
        _baseUrl.value = cleaned
        if (serverChanged) clearToken()
    }

    fun setToken(token: String, deviceId: String?) {
        prefs.edit()
            .putString(KEY_TOKEN, token)
            .putString(KEY_DEVICE_ID, deviceId)
            .apply()
        _token.value = token
    }

    fun clearToken() {
        prefs.edit().remove(KEY_TOKEN).remove(KEY_DEVICE_ID).apply()
        _token.value = null
    }

    companion object {
        const val EMULATOR_HOST = "http://10.0.2.2:8787"

        private const val KEY_BASE_URL = "base_url"
        private const val KEY_TOKEN = "token"
        private const val KEY_DEVICE_ID = "device_id"
        private const val KEY_BASEMAP = "basemap"

        @Volatile
        private var instance: Settings? = null

        /**
         * Process-wide, because MapLibre's HTTP stack is process-wide.
         *
         * The map does not fetch its tiles through anything this app holds a
         * reference to — it calls out to whatever `Call.Factory` was handed to
         * `HttpRequestUtil`. So the token that authorises those requests has to
         * be reachable from a place with no Activity and no ViewModel, and that
         * makes it a singleton whether or not one is aesthetically welcome.
         */
        fun get(context: Context): Settings =
            instance ?: synchronized(this) {
                instance ?: Settings(context).also { instance = it }
            }
    }
}
