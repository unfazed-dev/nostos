import Foundation
import Capacitor
import UIKit
import UserNotifications

/**
 * iOS side of the Nostos Capacitor plugin — push bridge only (beta, ADR-0037).
 *
 * The sync path runs inside the webview (the WASM engine + WebSocket), so this
 * class implements exactly one bridge method: `registerForPushNotifications`.
 * It registers the app with APNs (no permission prompt — silent pushes need
 * only registration; the visible-notification authorization, the push
 * entitlement, and the APNs key/team config are app-side — see the README)
 * and hands the resulting device token to JS as a `pushToken` event. The app
 * then calls `registerPushToken("apns", token)` on its Nostos web instance,
 * which POSTs it to the server with the same JWT the sync session uses.
 *
 * Foreground bridge: user-visible notifications that arrive while the app is
 * foregrounded are surfaced to JS as a `foregroundPush` event and NOT shown
 * by the OS (the app decides — typically it reconnects/re-reads instead,
 * because a foregrounded live socket has already applied the data).
 */
@objc(NostosPlugin)
public class NostosPlugin: CAP_B_Plugin, UNUserNotificationCenterDelegate {

    override public func load() {
        // Capacitor's AppDelegate forwards the APNs registration results to
        // plugins via NotificationCenter — same wiring as the official
        // @capacitor/push-notifications plugin.
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(self.didRegisterForRemoteNotificationsWithDeviceToken(_:)),
            name: .CAPNotifications.DidRegisterForRemoteNotificationsWithDeviceToken,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(self.didFailToRegisterForRemoteNotifications(_:)),
            name: .CAPNotifications.DidFailToRegisterForRemoteNotificationsWithError,
            object: nil
        )
        // ponytail: claiming the UNUserNotificationCenter delegate means this
        // plugin owns foreground presentation — a second push plugin claiming
        // the same delegate (e.g. @capacitor/push-notifications) will fight
        // over it. Upgrade path: a presentation option on the plugin call if
        // an app ever needs both plugins.
        UNUserNotificationCenter.current().delegate = self
    }

    /// Register with APNs. Resolves immediately; the token arrives as a
    /// `pushToken` event (or `pushTokenError` on failure). Requires the
    /// app-side `aps-environment` entitlement.
    @objc func registerForPushNotifications(_ call: CAPPluginCall) {
        DispatchQueue.main.async {
            UIApplication.shared.registerForRemoteNotifications()
        }
        call.resolve()
    }

    @objc func didRegisterForRemoteNotificationsWithDeviceToken(_ notification: Notification) {
        guard let deviceToken = notification.object as? Data else {
            return
        }
        let token = deviceToken.map { String(format: "%02x", $0) }.joined()
        notifyListeners("pushToken", data: ["platform": "apns", "token": token])
    }

    @objc func didFailToRegisterForRemoteNotifications(_ notification: Notification) {
        let message = (notification.object as? Error)?.localizedDescription
            ?? "APNs registration failed"
        notifyListeners("pushTokenError", data: ["message": message])
    }

    /// Foreground bridge (beta): a user-visible notification arrived while the
    /// app was foregrounded. Hand the payload to JS and suppress the OS
    /// presentation — the doorbell contract (ADR-0037 §2) says the app syncs
    /// in this state anyway.
    public func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        notifyListeners("foregroundPush", data: ["payload": notification.request.content.userInfo])
        completionHandler([])
    }
}
