package internal.atlet.atlet

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
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
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val nm = getSystemService(NotificationManager::class.java)
            nm.createNotificationChannel(
                NotificationChannel(
                    "cairn",
                    "Nostos updates",
                    NotificationManager.IMPORTANCE_HIGH,
                )
            )
        }
    }

    override fun configureFlutterEngine(engine: FlutterEngine) {
        super.configureFlutterEngine(engine)
        // Foreground order banner: posts a LOCAL heads-up notification on the
        // same channel as cairn's FCM pushes, so the online (live-sync)
        // experience looks identical to the background push one — slide-in
        // banner, then it sits in the tray. Same-body posts share an id and
        // replace (the orders watch stream emits a few times per change).
        MethodChannel(engine.dartExecutor.binaryMessenger, "atlet/notify")
            .setMethodCallHandler { call, result ->
                if (call.method != "order_update") {
                    return@setMethodCallHandler result.notImplemented()
                }
                val body = call.argument<String>("body")
                if (body == null) {
                    result.error("ARG", "body required", null)
                    return@setMethodCallHandler
                }
                val nm = getSystemService(NotificationManager::class.java)
                nm.notify(
                    body.hashCode(),
                    Notification.Builder(this, "cairn")
                        .setSmallIcon(applicationInfo.icon)
                        .setContentTitle("Atlet order update")
                        .setContentText(body)
                        .setAutoCancel(true)
                        .build()
                )
                result.success(null)
            }
    }
}
