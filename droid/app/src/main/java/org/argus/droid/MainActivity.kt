package org.argus.droid

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.result.contract.ActivityResultContracts
import androidx.activity.viewModels
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.ui.graphics.Color
import org.argus.droid.ui.MapScreen
import org.maplibre.android.MapLibre
import org.maplibre.android.module.http.HttpRequestUtil

/** A `argus://pair?server=…&code=…` link, as scanned from the daemon's console. */
data class PairLink(val server: String, val code: String)

class MainActivity : ComponentActivity() {
    private val vm: ArgusViewModel by viewModels()
    private var pairLink by mutableStateOf<PairLink?>(null)

    /** Start watching either way: a refused permission means silent alerts,
     *  not no alerts — they still reach the list in the app. */
    private val notificationPermission =
        registerForActivityResult(ActivityResultContracts.RequestPermission()) {
            AlertService.start(this)
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        // A server address supplied by the launcher wins, so a device can be
        // pointed at a daemon without a rebuild:
        //   adb shell am start -n org.argus.droid/.MainActivity -e server http://…
        intent?.getStringExtra("server")?.takeIf { it.isNotBlank() }?.let(vm::setServer)

        // Must happen before any MapView is constructed.
        MapLibre.getInstance(this)
        // And this must happen before the map fetches anything. MapLibre has
        // its own process-global HTTP stack; handing it the app's client is the
        // only way a style or a tile can carry this device's bearer token —
        // and that client is careful to attach the token to the daemon alone,
        // never to the third-party basemap the style also names.
        HttpRequestUtil.setOkHttpClient(vm.client.http)

        handleDeepLink(intent)

        // Asked for before the service starts, because a foreground service
        // whose notifications are blocked is a process holding a socket open to
        // no purpose. Declined is a legitimate answer — the map still works and
        // the alerts sheet still lists what fired.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            notificationPermission.launch(Manifest.permission.POST_NOTIFICATIONS)
        } else {
            AlertService.start(this)
        }

        setContent {
            MaterialTheme(colorScheme = ARGUS_DARK) {
                Surface(modifier = Modifier.fillMaxSize(), color = Color.Black) {
                    MapScreen(vm = vm, pairLink = pairLink, onPairLinkHandled = { pairLink = null })
                }
            }
        }
    }

    /**
     * The QR the daemon prints is a link, and the phone's own camera already
     * knows how to open one. Declaring the scheme means scanning it lands here
     * with the address and the code both filled in, which is the whole pairing
     * flow without this app ever asking for the camera.
     */
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        handleDeepLink(intent)
    }

    private fun handleDeepLink(intent: Intent?) {
        val data: Uri = intent?.data ?: return
        if (data.scheme != "argus" || data.host != "pair") return
        val server = data.getQueryParameter("server")
        val code = data.getQueryParameter("code")
        if (server.isNullOrBlank() || code.isNullOrBlank()) return
        pairLink = PairLink(server = server.trimEnd('/'), code = code)
    }
}

/**
 * Dark, because this is a map at night as often as not, and a light chrome
 * around a dark basemap is a torch pointed at the reader.
 */
private val ARGUS_DARK = darkColorScheme(
    primary = Color(0xFF56CCF2),
    secondary = Color(0xFF6FCF97),
    error = Color(0xFFEB5757),
    background = Color(0xFF0B0E11),
    surface = Color(0xFF141A20),
)
