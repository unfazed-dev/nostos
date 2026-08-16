package internal.atlet.atlet

import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Build
import io.flutter.embedding.android.FlutterActivity

class MainActivity : FlutterActivity() {
    override fun onCreate(savedInstanceState: android.os.Bundle?) {
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
}
