package internal.atlet.atlet

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import android.os.Bundle
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Heads-up banner channel for nostos visible pushes (ADR-0037): FCM
        // targets this channel_id; without a HIGH-importance channel Android
        // posts them silently on the DEFAULT fallback channel (no banner).
        // Explicit double-buzz pattern (WhatsApp-style): channel vibration
        // settings lock at FIRST creation, so already-installed apps keep
        // whatever they got until the channel is deleted or app data cleared.
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(NotificationManager::class.java)
            val channel = NotificationChannel(
                "nostos",
                "Nostos updates",
                NotificationManager.IMPORTANCE_HIGH,
            )
            channel.enableVibration(true)
            channel.vibrationPattern = longArrayOf(0, 300, 200, 300)
            nm.createNotificationChannel(channel)
            // ADR-0048: the channel was "cairn" before the rename; left alone it
            // sits in the app's notification settings, empty, forever.
            nm.deleteNotificationChannel("cairn") // rename:hold — pre-rename channel id
        }
    }

    override fun configureFlutterEngine(engine: FlutterEngine) {
        super.configureFlutterEngine(engine)
        // Foreground order banner: posts a LOCAL heads-up notification on the
        // same channel as nostos's FCM pushes, so the online (live-sync)
        // experience looks identical to the background push one — slide-in
        // banner, then it sits in the tray. Tapping one opens the order event
        // it came from (lib/main.dart openHistoryEvent).
        val channel = MethodChannel(engine.dartExecutor.binaryMessenger, "atlet/notify")
        notifyChannel = channel
        channel.setMethodCallHandler { call, result ->
            // Cold start: the tap that launched the app arrives before Dart has
            // a handler, so it waits here until Dart asks. The ask is also what
            // proves someone is listening — iOS does the same (AppDelegate).
            if (call.method == "take_pending_tap") {
                dartReady = true
                val tap = pendingTap
                pendingTap = null
                return@setMethodCallHandler result.success(tap)
            }
            if (call.method != "order_update") {
                return@setMethodCallHandler result.notImplemented()
            }
            val body = call.argument<String>("body")
            if (body == null) {
                result.error("ARG", "body required", null)
                return@setMethodCallHandler
            }
            val data = call.argument<Map<String, Any?>>("data").orEmpty()
            // The routing keys travel as intent extras, which is what comes
            // back on a tap — the Android twin of iOS's userInfo.
            val tapIntent = Intent(this, MainActivity::class.java)
                .setAction(Intent.ACTION_MAIN)
                .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP)
            for (key in TAP_KEYS) {
                (data[key] as? String)?.let { tapIntent.putExtra(key, it) }
            }
            val id = body.hashCode()
            nm().notify(
                id,
                Notification.Builder(this, "nostos")
                    .setSmallIcon(applicationInfo.icon)
                    .setContentTitle(call.argument<String>("title") ?: "Atlet order update")
                    .setContentText(body)
                    .setAutoCancel(true)
                    .setContentIntent(
                        PendingIntent.getActivity(
                            this, id, tapIntent,
                            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
                        )
                    )
                    .build()
            )
            result.success(null)
        }
        // The intent that launched this activity may itself be a tap.
        tapFrom(intent)?.let { deliverTap(it) }
    }

    // singleTop: a tap while the app is alive re-delivers here, not to onCreate.
    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        tapFrom(intent)?.let { deliverTap(it) }
    }

    // Our keys out of a launch intent — either the extras a banner carried, or
    // an `atlet://history/<id>` deep link (see the manifest's intent-filter).
    private fun tapFrom(intent: Intent?): Map<String, String>? {
        if (intent == null) return null
        val route = intent.data?.let { uri ->
            if (uri.scheme != "atlet") null
            else if (uri.host == "history") "/history${uri.path}" else uri.path
        }
        val tap = mutableMapOf<String, String>()
        if (route != null && route.startsWith("/history/")) tap["nostos_route"] = route
        for (key in TAP_KEYS) {
            intent.getStringExtra(key)?.let { tap[key] = it }
        }
        return tap.ifEmpty { null }
    }

    private fun deliverTap(tap: Map<String, String>) {
        val channel = notifyChannel
        if (!dartReady || channel == null) {
            pendingTap = tap
            return
        }
        channel.invokeMethod("notification_tap", tap)
    }

    private fun nm() = getSystemService(NotificationManager::class.java)

    private var notifyChannel: MethodChannel? = null
    private var dartReady = false
    private var pendingTap: Map<String, String>? = null

    private companion object {
        val TAP_KEYS = listOf("event_id", "nostos_route", "deep_link", "order_id", "status")
    }
}
