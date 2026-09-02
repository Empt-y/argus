package org.argus.droid.ui

import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.windowInsetsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Notifications
import androidx.compose.material.icons.filled.Place
import androidx.compose.material.icons.filled.Settings
import androidx.compose.material.icons.filled.Share
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.Icon
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import org.argus.droid.ArgusViewModel
import org.argus.droid.PairLink

/**
 * The tabs. `WORLD` is the map; everything else is a place for a feature to
 * live that would otherwise have become another word in the map's corner.
 */
enum class Tab(val label: String, val icon: ImageVector) {
    WORLD("World", Icons.Filled.Place),
    ALERTS("Alerts", Icons.Filled.Notifications),
    SOURCES("Sources", Icons.Filled.Share),
    SETTINGS("Settings", Icons.Filled.Settings),
}

/**
 * The app shell.
 *
 * The map is deliberately **always in the composition**, with the other tabs
 * drawn over it rather than replacing it. MapLibre's `MapView` owns a GL
 * surface and a native map object: letting it leave the composition when you
 * switch tabs would tear that down, and coming back would mean a fresh view, a
 * re-downloaded style, re-fetched tiles and a camera reset to the default. A
 * map that forgets where you were looking every time you check the alerts list
 * is not worth the memory it saves.
 *
 * What it does instead is stop *rendering* while hidden — see [WorldTab] — so
 * the cost of keeping it alive is memory rather than battery.
 */
@Composable
fun ArgusApp(
    vm: ArgusViewModel,
    pairLink: PairLink? = null,
    onPairLinkHandled: () -> Unit = {},
) {
    val state by vm.state.collectAsStateWithLifecycle()
    var tab by rememberSaveable { mutableStateOf(Tab.WORLD) }

    // A pairing link is a request to go and pair, so it takes the user there
    // rather than opening a sheet over whatever they were looking at.
    val pending = remember { mutableStateOf<PairLink?>(null) }
    if (pairLink != null && pending.value != pairLink) {
        pending.value = pairLink
        tab = Tab.SETTINGS
    }

    Scaffold(
        bottomBar = {
            NavigationBar {
                Tab.entries.forEach { entry ->
                    NavigationBarItem(
                        selected = tab == entry,
                        onClick = { tab = entry },
                        label = { Text(entry.label) },
                        icon = {
                            // The unacknowledged count belongs on the tab, not
                            // buried inside it: an alert waiting for a person is
                            // the only thing in this app that is waiting for a
                            // person rather than reporting a state.
                            if (entry == Tab.ALERTS && state.unacknowledged > 0) {
                                BadgedBox(badge = { Badge { Text("${state.unacknowledged}") } }) {
                                    Icon(entry.icon, contentDescription = entry.label)
                                }
                            } else {
                                Icon(entry.icon, contentDescription = entry.label)
                            }
                        },
                    )
                }
            }
        },
    ) { padding ->
        Box(Modifier.fillMaxSize()) {
            // Always composed; see the note on this function.
            WorldTab(
                vm = vm,
                visible = tab == Tab.WORLD,
                contentPadding = padding,
            )

            if (tab != Tab.WORLD) {
                // Fills to the top edge so the map does not show as a strip
                // above the content; the inner padding keeps text clear of the
                // status bar. Only the bottom comes from the Scaffold, which is
                // where the tab bar is.
                Surface(
                    Modifier
                        .fillMaxSize()
                        .padding(bottom = padding.calculateBottomPadding()),
                ) {
                    // The rule, after being bitten twice: a *screen* scrolls,
                    // a *section* does not. These section composables are also
                    // stacked inside the Settings page, and a scrollable inside
                    // a scrollable is measured with unbounded height and throws
                    // rather than degrading. So this container only handles
                    // insets, and each tab below decides its own scrolling.
                    Box(
                        Modifier
                            .fillMaxSize()
                            .windowInsetsPadding(
                                WindowInsets.safeDrawing.only(
                                    WindowInsetsSides.Top + WindowInsetsSides.Horizontal
                                )
                            )
                    ) {
                    when (tab) {
                        Tab.ALERTS -> AlertsContent(
                            modifier = Modifier.verticalScroll(rememberScrollState()),
                            alerts = state.alerts,
                            geofences = state.geofences,
                            onAcknowledge = vm::acknowledgeAlert,
                            // Showing where an alert fired is a map action, so
                            // it moves the map and then shows it.
                            onGoTo = { lat, lon ->
                                vm.focus(lat, lon)
                                tab = Tab.WORLD
                            },
                        )

                        Tab.SOURCES -> SourcesContent(
                            sources = state.sources,
                            modifier = Modifier.verticalScroll(rememberScrollState()),
                        )

                        Tab.SETTINGS -> SettingsTab(
                            vm = vm,
                            pairLink = pending.value,
                            onPairLinkHandled = {
                                pending.value = null
                                onPairLinkHandled()
                            },
                        )

                        Tab.WORLD -> Unit
                    }
                    }
                }
            }
        }
    }
}
