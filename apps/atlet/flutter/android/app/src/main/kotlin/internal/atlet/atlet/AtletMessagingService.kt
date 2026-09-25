package internal.atlet.atlet

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.core.app.NotificationCompat
import com.google.firebase.messaging.RemoteMessage
import io.flutter.plugins.firebase.messaging.FlutterFirebaseMessagingService

/// PILOT (ADR-0037 §2 `action` mode) — Android side of action pushes.
///
/// System-rendered FCM notifications cannot carry action buttons, so the
/// server sends action pushes as data-only (`{title, body, category}`) and
/// THIS service posts the notification natively — foreground, background,
/// or killed, one code path (the WhatsApp pattern). super.onMessageReceived
/// still runs, so doorbells keep flowing to Dart untouched.
///
/// The buttons: "Track order" launches MainActivity; "Dismiss" cancels via
/// [DismissReceiver]. ponytail: single hardcoded action set for every
/// category; per-category action registries when a real app needs more
/// than the pilot.
class AtletMessagingService : FlutterFirebaseMessagingService() {
    override fun onMessageReceived(message: RemoteMessage) {
        val data = message.data
        if (data.containsKey("category")) {
            postActionNotification(data)
        }
        super.onMessageReceived(message)
    }

    private fun postActionNotification(data: Map<String, String>) {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        val title = data["title"] ?: return
        val body = data["body"] ?: ""
        val id = body.hashCode()

        // The service can run before any Activity — ensure the channel
        // exists here too (same id/settings as MainActivity's).
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channel = NotificationChannel(
                CHANNEL_ID, "Nostos updates", NotificationManager.IMPORTANCE_HIGH
            )
            channel.enableVibration(true)
            channel.vibrationPattern = longArrayOf(0, 300, 200, 300)
            nm.createNotificationChannel(channel)
        }

        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val dismiss = PendingIntent.getBroadcast(
            this, id,
            Intent(this, DismissReceiver::class.java).putExtra(DismissReceiver.EXTRA_ID, id),
            PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE
        )
        val track = NotificationCompat.Action.Builder(0, "Track order", open).build()
        val dismissAction =
            NotificationCompat.Action.Builder(0, "Dismiss", dismiss).build()

        // setPriority is for pre-O (channel governs on O+).
        val notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setSmallIcon(applicationInfo.icon)
            .setContentTitle(title)
            .setContentText(body)
            .setAutoCancel(true)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .addAction(track)
            .addAction(dismissAction)
            .build()
        nm.notify(id, notification)
    }

    companion object {
        const val CHANNEL_ID = "nostos"
    }
}

/// Cancels a notification whose "Dismiss" button was tapped.
class DismissReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val nm = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.cancel(intent.getIntExtra(EXTRA_ID, 0))
    }

    companion object {
        const val EXTRA_ID = "id"
    }
}
