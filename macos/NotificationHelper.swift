// Small app bundle gives UserNotifications a stable application identity.
// No resident process: each invocation submits one request and exits.
import AppKit
import UserNotifications

final class NotificationDelegate: NSObject, NSApplicationDelegate {
    private let arguments = Array(CommandLine.arguments.dropFirst())
    private var finished = false

    func applicationDidFinishLaunching(_ notification: Notification) {
        guard arguments.count >= 2 else {
            NSApplication.shared.terminate(nil)
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 110) {
            self.finish("error: Notification helper timed out.")
        }
        let center = UNUserNotificationCenter.current()
        switch arguments[0] {
        case "request":
            center.requestAuthorization(options: [.alert]) { granted, error in
                self.finish(error.map { "error: \($0.localizedDescription)" } ?? (granted ? "granted" : "denied"))
            }
        case "notify", "test":
            center.getNotificationSettings { settings in
                guard settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional else {
                    self.finish("error: Notification permission is unavailable. Run git-sweep notifications.")
                    return
                }
                let days = self.arguments.count > 2 ? self.arguments[2] : "14"
                let content = UNMutableNotificationContent()
                let test = self.arguments[0] == "test"
                content.title = test ? "Git Sweep notifications are ready" : "Git Sweep cleanup is overdue"
                content.body = test
                    ? "This is a test notification. Git Sweep will alert you if maintenance is overdue."
                    : "No idle cleanup has completed for \(days) days. Git Sweep is waiting for an inactive session and spare resources. Run git-sweep status for details."
                let request = UNNotificationRequest(identifier: test ? "git-sweep-test" : "git-sweep-overdue", content: content, trigger: nil)
                center.add(request) { error in
                    self.finish(error.map { "error: \($0.localizedDescription)" } ?? "sent")
                }
            }
        default:
            finish("error: Unknown notification helper command.")
        }
    }

    private func finish(_ result: String) {
        DispatchQueue.main.async {
            guard !self.finished else { return }
            self.finished = true
            do {
                let url = URL(fileURLWithPath: self.arguments[1])
                try Data(result.utf8).write(to: url, options: .atomic)
                try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: url.path)
            } catch {
                fputs("Git Sweep notification result: \(error.localizedDescription)\n", stderr)
            }
            // Give Notification Center time to accept the request before exit.
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) {
                NSApplication.shared.terminate(nil)
            }
        }
    }
}

let app = NSApplication.shared
let delegate = NotificationDelegate()
app.delegate = delegate
app.setActivationPolicy(.accessory)
app.run()
