// triage-notify: macOS notification helper using UNUserNotificationCenter.
//
// Two modes — distinguished by whether `--title` was passed:
//
//   POST MODE  (`--title <text> ...`):
//     called by triage to schedule a notification. We request authorization,
//     post the notification with the action shell command embedded in
//     userInfo, then exit shortly. The user's click happens later and is
//     delivered to a separate response-mode instance launched by macOS.
//
//   RESPONSE MODE  (no args):
//     macOS launches the bundle automatically when the user interacts with a
//     pending notification. This instance has no argv from the original
//     post; instead, the action command lives in the notification's
//     userInfo dict. We set up the delegate, run a brief RunLoop to receive
//     the queued response, run the action via /bin/sh -c, and exit.
//
// Without this two-mode split, response-mode launches see no `--title`,
// bail out, and the click event is silently lost. (That bug was caught
// by debug logging — see /tmp/triage-notify.log).

import Foundation
import UserNotifications

// Diagnostics gated on TRIAGE_NOTIFY_DEBUG=1 so a normal install doesn't
// write /tmp/triage-notify.log on every notification. Set the env var (in
// the parent triage process or via `launchctl setenv`) to re-enable when
// debugging click delivery or permission state.
let debugEnabled = ProcessInfo.processInfo.environment["TRIAGE_NOTIFY_DEBUG"] == "1"

func dbg(_ msg: String) {
    guard debugEnabled else { return }
    let line = "[\(Date())] \(msg)\n"
    guard let data = line.data(using: .utf8) else { return }
    let url = URL(fileURLWithPath: "/tmp/triage-notify.log")
    if FileManager.default.fileExists(atPath: url.path) {
        if let h = try? FileHandle(forWritingTo: url) {
            h.seekToEndOfFile()
            h.write(data)
            try? h.close()
        }
    } else {
        try? data.write(to: url)
    }
}

final class Args {
    var title: String?
    var subtitle: String?
    var message: String?
    var action: String?
    var timeout: TimeInterval = 30
}

func parseArgs() -> Args {
    let result = Args()
    let argv = CommandLine.arguments
    var i = 1
    while i < argv.count {
        let flag = argv[i]
        let next: String? = (i + 1 < argv.count) ? argv[i + 1] : nil
        switch flag {
        case "--title":    result.title = next;    i += 2
        case "--subtitle": result.subtitle = next; i += 2
        case "--message":  result.message = next;  i += 2
        case "--action":   result.action = next;   i += 2
        case "--timeout":
            if let n = next, let t = TimeInterval(n) { result.timeout = t }
            i += 2
        default: i += 1
        }
    }
    return result
}

final class Delegate: NSObject, UNUserNotificationCenterDelegate {
    var done = false

    // Show banners + sound even when the foreground app is the helper itself.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification,
        withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void
    ) {
        dbg("willPresent fired (banner shown)")
        if #available(macOS 11.0, *) {
            completionHandler([.banner, .sound])
        } else {
            completionHandler([.alert, .sound])
        }
    }

    // Click / action-button callback. Fires both for body taps (default
    // action) and for the explicit "open" action button. The action shell
    // command lives in notification.request.content.userInfo, NOT on this
    // delegate instance — because in response mode the helper that runs
    // didn't post the notification originally and has no other source.
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse,
        withCompletionHandler completionHandler: @escaping () -> Void
    ) {
        let id = response.actionIdentifier
        dbg("didReceive actionIdentifier=\(id)")
        let isClick = id == UNNotificationDefaultActionIdentifier || id == "open"
        if isClick {
            let userInfo = response.notification.request.content.userInfo
            if let cmd = userInfo["actionCommand"] as? String {
                dbg("running action: \(cmd)")
                let task = Process()
                task.launchPath = "/bin/sh"
                task.arguments = ["-c", cmd]
                do {
                    try task.run()
                    task.waitUntilExit()
                    dbg("action exited \(task.terminationStatus)")
                } catch {
                    dbg("action failed: \(error)")
                }
            } else {
                dbg("click but no actionCommand in userInfo")
            }
        } else {
            dbg("non-click action (id=\(id)) — ignoring")
        }
        done = true
        completionHandler()
    }
}

let args = parseArgs()
dbg("startup pid=\(getpid()) bundleId=\(Bundle.main.bundleIdentifier ?? "nil") hasTitle=\(args.title != nil)")

let delegate = Delegate()
let center = UNUserNotificationCenter.current()
center.delegate = delegate

if let title = args.title {
    // ── POST MODE ─────────────────────────────────────────────────────
    let authGroup = DispatchGroup()
    authGroup.enter()
    center.requestAuthorization(options: [.alert, .sound]) { granted, err in
        defer { authGroup.leave() }
        dbg("requestAuthorization granted=\(granted) err=\(err?.localizedDescription ?? "nil")")
        if !granted {
            FileHandle.standardError.write(Data("triage-notify: notification permission denied\n".utf8))
        }
        if let err = err {
            FileHandle.standardError.write(Data("triage-notify: auth error: \(err.localizedDescription)\n".utf8))
        }
    }
    authGroup.wait()

    let content = UNMutableNotificationContent()
    content.title = title
    if let s = args.subtitle { content.subtitle = s }
    if let m = args.message { content.body = m }
    if let a = args.action {
        // Embed the action shell command in userInfo so a fresh response-
        // mode instance can recover it on click.
        content.userInfo = ["actionCommand": a]
    }

    if args.action != nil {
        let openAction = UNNotificationAction(
            identifier: "open",
            title: "Open",
            options: [.foreground]
        )
        let category = UNNotificationCategory(
            identifier: "triage.click",
            actions: [openAction],
            intentIdentifiers: [],
            options: []
        )
        center.setNotificationCategories([category])
        content.categoryIdentifier = "triage.click"
    }

    let request = UNNotificationRequest(
        identifier: UUID().uuidString,
        content: content,
        trigger: nil
    )
    dbg("posting notification id=\(request.identifier) categoryId=\(content.categoryIdentifier) hasAction=\(args.action != nil)")
    let postGroup = DispatchGroup()
    postGroup.enter()
    center.add(request) { err in
        dbg("center.add completion err=\(err?.localizedDescription ?? "nil")")
        if let err = err {
            FileHandle.standardError.write(Data("triage-notify: post error: \(err.localizedDescription)\n".utf8))
        }
        postGroup.leave()
    }
    postGroup.wait()

    // Brief wait so an in-process click (user racing the banner appearance)
    // gets handled before we exit. Real clicks usually arrive seconds
    // later and route to a separate response-mode instance.
    dbg("post-mode RunLoop wait, timeout=\(args.timeout)")
    let deadline = Date().addingTimeInterval(args.timeout)
    while !delegate.done && Date() < deadline {
        RunLoop.current.run(mode: .default, before: Date(timeIntervalSinceNow: 0.5))
    }
    dbg("post-mode exiting (done=\(delegate.done))")
    exit(0)
}

// ── RESPONSE MODE ────────────────────────────────────────────────────────
// Launched by macOS to deliver a queued notification response. The delegate
// is set above; just RunLoop until didReceive fires (or we time out).
dbg("response-mode RunLoop wait")
let responseDeadline = Date().addingTimeInterval(10)
while !delegate.done && Date() < responseDeadline {
    RunLoop.current.run(mode: .default, before: Date(timeIntervalSinceNow: 0.2))
}
dbg("response-mode exiting (done=\(delegate.done))")
exit(0)
