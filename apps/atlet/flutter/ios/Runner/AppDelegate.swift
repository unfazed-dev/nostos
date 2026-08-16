import Flutter
import UIKit
import UserNotifications

@main
@objc class AppDelegate: FlutterAppDelegate, FlutterImplicitEngineDelegate {
  override func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]?
  ) -> Bool {
    // Foreground order banners (ADR-0037 push pilot): iOS suppresses
    // notifications while the app is foregrounded unless the delegate says
    // otherwise — present them as banners, mirroring Android's heads-up.
    UNUserNotificationCenter.current().delegate = self
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

  // Foreground presentation: show the banner + sound instead of dropping it.
  override func userNotificationCenter(
    _ center: UNUserNotificationCenter,
    willPresent notification: UNNotification,
    withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
  ) {
    completionHandler([.banner, .sound])
  }
}
