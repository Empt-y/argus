package org.argus.droid

import android.content.Context
import android.util.Log
import org.json.JSONObject
import org.maplibre.android.geometry.LatLngBounds
import org.maplibre.android.offline.OfflineManager
import org.maplibre.android.offline.OfflineRegion
import org.maplibre.android.offline.OfflineRegionError
import org.maplibre.android.offline.OfflineRegionStatus
import org.maplibre.android.offline.OfflineTilePyramidRegionDefinition

/** A stored region, as the UI needs to show it. */
data class StoredRegion(
    val id: Long,
    val name: String,
    val minZoom: Int,
    val maxZoom: Int,
    val complete: Boolean,
    val tiles: Long,
    val bytes: Long,
)

/** Progress of a download in flight. */
data class DownloadProgress(
    val name: String,
    val tiles: Long,
    val bytes: Long,
    val complete: Boolean,
    val error: String? = null,
)

/**
 * Ground for a phone with no signal.
 *
 * The important decision is *what* gets downloaded: the basemap alone, from
 * `/v1/style.json?basemap_only=true`. MapLibre's offline manager packages every
 * tile a style references, so pointing it at the full style would bake an hour
 * of aircraft into a file and replay them forever as current. Terrain and roads
 * are worth keeping because they do not change; a contact is worth having live
 * or not at all.
 *
 * Regions are MapLibre's own SQLite store, not ours — which is why the name and
 * the zoom range are stashed in the region's metadata blob rather than in a
 * table this app would then have to keep in step with it.
 */
class Offline(context: Context) {
    private val manager = OfflineManager.getInstance(context.applicationContext)

    fun list(onResult: (List<StoredRegion>) -> Unit) {
        manager.listOfflineRegions(object : OfflineManager.ListOfflineRegionsCallback {
            override fun onList(offlineRegions: Array<OfflineRegion>?) {
                val out = offlineRegions.orEmpty().map { region ->
                    val meta = region.metadata.readMetadata()
                    StoredRegion(
                        id = region.id,
                        name = meta.optString("name", "region ${region.id}"),
                        minZoom = meta.optInt("min_zoom", 0),
                        maxZoom = meta.optInt("max_zoom", 0),
                        // Status is asynchronous and this list is a summary, so
                        // completeness is recorded at the end of a download
                        // rather than queried for every row on every open.
                        complete = meta.optBoolean("complete", false),
                        tiles = meta.optLong("tiles", 0),
                        bytes = meta.optLong("bytes", 0),
                    )
                }
                onResult(out)
            }

            override fun onError(error: String) {
                Log.w(TAG, "listing offline regions: $error")
                onResult(emptyList())
            }
        })
    }

    /**
     * Download one box of ground.
     *
     * [maxZoom] is the whole cost of this feature. Each extra level quadruples
     * the tile count, so the UI offers a small range and says how many tiles it
     * came to rather than letting someone ask for z16 over a county and wait.
     */
    fun download(
        styleUrl: String,
        name: String,
        bounds: LatLngBounds,
        minZoom: Int,
        maxZoom: Int,
        pixelRatio: Float,
        onProgress: (DownloadProgress) -> Unit,
    ) {
        val definition = OfflineTilePyramidRegionDefinition(
            styleUrl,
            bounds,
            minZoom.toDouble(),
            maxZoom.toDouble(),
            pixelRatio,
        )
        val metadata = JSONObject()
            .put("name", name)
            .put("min_zoom", minZoom)
            .put("max_zoom", maxZoom)
            .toString()
            .toByteArray()

        manager.createOfflineRegion(
            definition,
            metadata,
            object : OfflineManager.CreateOfflineRegionCallback {
                override fun onCreate(offlineRegion: OfflineRegion) {
                    offlineRegion.setObserver(object : OfflineRegion.OfflineRegionObserver {
                        override fun onStatusChanged(status: OfflineRegionStatus) {
                            onProgress(
                                DownloadProgress(
                                    name = name,
                                    tiles = status.completedTileCount,
                                    bytes = status.completedTileSize,
                                    complete = status.isComplete,
                                )
                            )
                            if (status.isComplete) {
                                // Recorded now, because a later listing has no
                                // cheap way to ask.
                                offlineRegion.updateMetadata(
                                    JSONObject(String(metadata))
                                        .put("complete", true)
                                        .put("tiles", status.completedTileCount)
                                        .put("bytes", status.completedTileSize)
                                        .toString()
                                        .toByteArray(),
                                    object : OfflineRegion.OfflineRegionUpdateMetadataCallback {
                                        override fun onUpdate(metadata: ByteArray) = Unit
                                        override fun onError(error: String) {
                                            Log.w(TAG, "metadata: $error")
                                        }
                                    },
                                )
                                offlineRegion.setDownloadState(OfflineRegion.STATE_INACTIVE)
                            }
                        }

                        override fun onError(error: OfflineRegionError) {
                            onProgress(
                                DownloadProgress(
                                    name = name,
                                    tiles = 0,
                                    bytes = 0,
                                    complete = false,
                                    error = "${error.reason}: ${error.message}",
                                )
                            )
                        }

                        /**
                         * Named for Mapbox's hosted tile limit, which does not
                         * apply here — the tiles come from the user's own
                         * daemon and OSM. Reported anyway rather than swallowed:
                         * if it ever fires, something is very different from
                         * what this code assumes.
                         */
                        override fun mapboxTileCountLimitExceeded(limit: Long) {
                            onProgress(
                                DownloadProgress(
                                    name, 0, 0, false,
                                    error = "tile count limit $limit exceeded",
                                )
                            )
                        }
                    })
                    offlineRegion.setDownloadState(OfflineRegion.STATE_ACTIVE)
                }

                override fun onError(error: String) {
                    onProgress(DownloadProgress(name, 0, 0, false, error = error))
                }
            },
        )
    }

    fun delete(id: Long, onDone: () -> Unit) {
        manager.listOfflineRegions(object : OfflineManager.ListOfflineRegionsCallback {
            override fun onList(offlineRegions: Array<OfflineRegion>?) {
                val region = offlineRegions.orEmpty().firstOrNull { it.id == id }
                if (region == null) {
                    onDone()
                    return
                }
                region.delete(object : OfflineRegion.OfflineRegionDeleteCallback {
                    override fun onDelete() = onDone()
                    override fun onError(error: String) {
                        Log.w(TAG, "deleting region $id: $error")
                        onDone()
                    }
                })
            }

            override fun onError(error: String) = onDone()
        })
    }

    private fun ByteArray?.readMetadata(): JSONObject =
        runCatching { JSONObject(String(this ?: ByteArray(0))) }.getOrDefault(JSONObject())

    private companion object {
        const val TAG = "ArgusOffline"
    }
}

/**
 * How many basemap tiles a region will actually fetch.
 *
 * Note the `+ 1` on both ends. The basemap source declares `tileSize: 256`,
 * while MapLibre's internal tile is 512, so a style zoom of *z* is served by
 * raster tiles at *z+1* — the download reaches one level deeper than the range
 * it was asked for, which is four times as many tiles at the bottom.
 *
 * Measured rather than assumed: a first version of this without the offset
 * predicted 570 tiles for the London box at z6–12 and the download fetched
 * 2,134. Shifting the range to z7–13 predicts 2,175, the remaining 2% being
 * rounding at the edges of the box. A number on screen that is wrong by 3.7x is
 * worse than no number, because the whole reason to show it is to let someone
 * decide whether to start the download.
 */
fun estimateTiles(bounds: LatLngBounds, minZoom: Int, maxZoom: Int): Long {
    var total = 0L
    for (z in (minZoom + 1)..(maxZoom + 1)) {
        val n = 1L shl z
        val x0 = lonToTileX(bounds.longitudeWest, n)
        val x1 = lonToTileX(bounds.longitudeEast, n)
        val y0 = latToTileY(bounds.latitudeNorth, n)
        val y1 = latToTileY(bounds.latitudeSouth, n)
        total += (x1 - x0 + 1).coerceAtLeast(1) * (y1 - y0 + 1).coerceAtLeast(1)
    }
    return total
}

/**
 * Rough bytes for a tile count.
 *
 * 21 KiB per tile, from a measured run: 2,134 tiles came to 44.3 MiB of OSM
 * raster. Order-of-magnitude only, and labelled that way on screen — it exists
 * to distinguish "this is fine" from "this will fill the phone".
 */
fun estimateBytes(tiles: Long): Long = tiles * 21L * 1024L

private fun lonToTileX(lon: Double, n: Long): Long =
    (((lon + 180.0) / 360.0) * n).toLong().coerceIn(0, n - 1)

private fun latToTileY(lat: Double, n: Long): Long {
    val rad = Math.toRadians(lat.coerceIn(-85.05112878, 85.05112878))
    val y = (1.0 - Math.log(Math.tan(rad) + 1.0 / Math.cos(rad)) / Math.PI) / 2.0 * n
    return y.toLong().coerceIn(0, n - 1)
}
