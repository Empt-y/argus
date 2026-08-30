package org.argus.droid

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.viewinterop.AndroidView
import org.maplibre.android.MapLibre
import org.maplibre.android.camera.CameraPosition
import org.maplibre.android.geometry.LatLng
import org.maplibre.android.maps.MapView

/**
 * The map, and nothing else yet.
 *
 * This is the first runnable slice of the Android client. It is deliberately
 * thin, because the interesting claim it tests is not about Kotlin: it is that
 * the server's own `/v1/style.json` — generated from the layer catalogue, and
 * already driving the web client — is enough to render every layer on a
 * completely different map engine with no app-side knowledge of what a layer is.
 * If that holds, new drivers appear on the phone without an app release.
 */
class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // A server address supplied by the launcher wins, so a device can be
        // pointed at a daemon without a rebuild:
        //   adb shell am start -n org.argus.droid/.MainActivity -e server http://…
        intent?.getStringExtra("server")?.takeIf { it.isNotBlank() }?.let {
            Server.baseUrl = it.trimEnd('/')
        }

        // Must happen before any MapView is constructed.
        MapLibre.getInstance(this)
        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    MapScreen()
                }
            }
        }
    }
}

@Composable
private fun MapScreen() {
    var status by remember { mutableStateOf("loading ${Server.styleUrl}") }

    Box(modifier = Modifier.fillMaxSize()) {
        AndroidView(
            modifier = Modifier.fillMaxSize(),
            factory = { context ->
                MapView(context).apply {
                    onCreate(null)
                    getMapAsync { map ->
                        map.setStyle(Server.styleUrl) { style ->
                            status = "style loaded: ${style.layers.size} layers, " +
                                "${style.sources.size} sources"
                        }
                        map.cameraPosition = CameraPosition.Builder()
                            // The home AOI, so the first frame has something in it.
                            .target(LatLng(51.47, -0.45))
                            .zoom(8.0)
                            .build()
                    }
                }
            },
        )
        Text(
            text = status,
            modifier = Modifier.align(Alignment.TopStart).padding(12.dp),
            style = MaterialTheme.typography.labelMedium,
        )
    }
}
