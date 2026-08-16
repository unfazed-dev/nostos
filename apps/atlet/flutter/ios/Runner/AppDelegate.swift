import Flutter
import UIKit
import UserNotifications

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    // NOTE: do NOT assign UNUserNotificationCenter.current().delegate here.
    // Taking the delegate starves FlutterFire of foreground deliveries —
    // onMessage never fires for visible pushes (order-leg smoke). Foreground
    // banners go through the atlet/notify local-notification path below;
    // backgrounded banners are OS-default.
    //
    // Action categories (ADR-0037 §2 `action` mode): category registration
    // is registry-level — no delegate takeover needed. The server's
    // `aps.category` renders these buttons on lock screen / Notification
    // Center with the app killed. Category id is the operator contract
    // (NOSTOS_PUSH_TABLES `table:action:<category>:…`).
    let track = UNNotificationAction(
      identifier: "track_order", title: "Track order", options: [.foreground]
    )
    let markReceived = UNNotificationAction(
      identifier: "mark_received", title: "Mark received", options: [.foreground]
    )
    UNUserNotificationCenter.current().setNotificationCategories([
      UNNotificationCategory(
        identifier: "order_status",
        actions: [track, markReceived],
        intentIdentifiers: []
      )
    ])
    return super.application(application, didFinishLaunchingWithOptions: launchOptions)
  }

  func didInitializeImplicitFlutterEngine(_ engineBridge: FlutterImplicitEngineBridge) {
    GeneratedPluginRegistrant.register(with: engineBridge.pluginRegistry)
    // iOS twin of MainActivity's atlet/notify handler: the Dart order-banner
    // posts a local notification (same UX as the FCM push) on live sync
    // changes while the app is open.
    FlutterMethodChannel(
      name: "atlet/notify", binaryMessenger: engineBridge.applicationRegistrar.messenger()
    ).setMethodCallHandler { call, result in
      guard call.method == "order_update", let body = call.arguments as? [String: Any], let text = body["body"] as? String else {
        result(FlutterMethodNotImplemented)
        return
      }
      let content = UNMutableNotificationContent()
      content.title = "Atlet order update"
      content.body = text
      content.sound = .default
      let request = UNNotificationRequest(identifier: text, content: content, trigger: nil)
      UNUserNotificationCenter.current().add(request) { error in
        if let error = error {
          result(FlutterError(code: "NOTIFY", message: error.localizedDescription, details: nil))
        } else {
          result(nil)
        }
      }
    }
  }
}
