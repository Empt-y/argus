package org.argus.droid

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import org.argus.droid.net.Alert
import org.argus.droid.net.AlertStream
import org.argus.droid.net.ArgusClient

/**
 * Holds the alert socket open while the app is not in front.
 *
 * A foreground service, which on Android is the only honest way to say "this
 * app is doing something for you in the background": it costs a permanent
 * notification and in exchange the process is not killed the moment the map is
 * dismissed. A geofence the user drew and then closed the app on is exactly the
 * geofence they most want to hear from.
 *
 * There is no push provider here and there will not be one. Argus talks to a
 * daemon the user runs; routing a notification about their own aircraft through
 * Google's servers to come back to their own phone would be a strange thing to
 * build, and the project ruled it out at the start.
 */
class AlertService : Service() {
    private lateinit var stream: AlertStream
    private var connected = false

    override fun onCreate() {
        super.onCreate()
        createChannels()
        val settings = Settings.get(this)
        val client = ArgusClient(settings)
        stream = AlertStream(
            client = client.http,
            baseUrl = { settings.baseUrl.value },
            onAlerts = { alerts -> alerts.forEach(::notifyAlert) },
            onState = { up ->
                connected = up
                // The ongoing notification doubles as the connection indicator:
                // a silent alert stream and a dead one look identical, and only
                // one of them is fine.
                NotificationManagerCompat.from(this).notify(ONGOING_ID, ongoing())
            },
        )
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(ONGOING_ID, ongoing())
        stream.start()
        // Restarted if the system kills it: an alert watcher that stays dead
        // after a memory-pressure kill is worse than one that never ran, because
        // the user believes it is running.
        return START_STICKY
    }

    override fun onDestroy() {
        stream.stop()
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun ongoing(): Notification =
        NotificationCompat.Builder(this, CHANNEL_ONGOING)
            .setContentTitle("Argus")
            .setContentText(
                if (connected) "watching geofences" else "reconnecting to ${Settings.get(this).baseUrl.value}"
            )
            .setSmallIcon(android.R.drawable.ic_menu_mylocation)
            .setOngoing(true)
            .setPriority(NotificationCompat.PRIORITY_MIN)
            .setContentIntent(openApp())
            .build()

    private fun notifyAlert(alert: Alert) {
        val channel = channelFor(alert.severity)
        val notification = NotificationCompat.Builder(this, channel)
            .setContentTitle(alert.attrs["geofence"]?.toString()?.trim('"') ?: "Geofence")
            .setContentText(alert.message)
            .setSmallIcon(android.R.drawable.ic_dialog_info)
            .setAutoCancel(true)
            .setCategory(NotificationCompat.CATEGORY_ALARM)
            .setPriority(priorityFor(alert.severity))
            .setContentIntent(openApp())
            .build()
        // Keyed by alert id, so the same alert arriving twice — a replay racing
        // a live delivery — replaces rather than stacks.
        NotificationManagerCompat.from(this).notify(alert.alertId.toInt(), notification)
    }

    private fun openApp(): PendingIntent = PendingIntent.getActivity(
        this,
        0,
        Intent(this, MainActivity::class.java),
        PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT,
    )

    /**
     * One channel per severity, because the user — not this app — should decide
     * how loud each one is. A `critical` fence and an `info` fence are different
     * subscriptions in the notification settings, and collapsing them into one
     * channel takes that choice away.
     */
    private fun createChannels() {
        if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL_ONGOING, "Argus is running", NotificationManager.IMPORTANCE_MIN)
                .apply { description = "The persistent notice that the alert watcher is alive." }
        )
        for ((id, name, importance) in listOf(
            Triple(CHANNEL_INFO, "Info", NotificationManager.IMPORTANCE_LOW),
            Triple(CHANNEL_NOTICE, "Notice", NotificationManager.IMPORTANCE_DEFAULT),
            Triple(CHANNEL_WARNING, "Warning", NotificationManager.IMPORTANCE_HIGH),
            Triple(CHANNEL_CRITICAL, "Critical", NotificationManager.IMPORTANCE_HIGH),
        )) {
            manager.createNotificationChannel(
                NotificationChannel(id, name, importance).apply {
                    description = "Geofence alerts at $name severity."
                    enableVibration(importance >= NotificationManager.IMPORTANCE_DEFAULT)
                }
            )
        }
    }

    private fun channelFor(severity: String) = when (severity) {
        "critical" -> CHANNEL_CRITICAL
        "warning" -> CHANNEL_WARNING
        "notice" -> CHANNEL_NOTICE
        else -> CHANNEL_INFO
    }

    private fun priorityFor(severity: String) = when (severity) {
        "critical", "warning" -> NotificationCompat.PRIORITY_HIGH
        "notice" -> NotificationCompat.PRIORITY_DEFAULT
        else -> NotificationCompat.PRIORITY_LOW
    }

    companion object {
        const val CHANNEL_ONGOING = "argus.ongoing"
        const val CHANNEL_INFO = "argus.info"
        const val CHANNEL_NOTICE = "argus.notice"
        const val CHANNEL_WARNING = "argus.warning"
        const val CHANNEL_CRITICAL = "argus.critical"
        private const val ONGOING_ID = 1

        fun start(context: Context) {
            val intent = Intent(context, AlertService::class.java)
            context.startForegroundService(intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, AlertService::class.java))
        }
    }
}
