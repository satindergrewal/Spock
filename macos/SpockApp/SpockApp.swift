import SwiftUI
import AppKit

/// Menu-bar agent entry. Dock + Cmd+Tab only while Settings/Chat are open.
@main
struct SpockApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) var appDelegate

    var body: some Scene {
        // Zero-size Settings scene only so SwiftUI has a Scene; never shown.
        Settings {
            EmptyView()
                .frame(width: 0, height: 0)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    var statusItem: NSStatusItem?
    var settingsWindow: NSWindow?
    var chatWindow: NSWindow?
    private var menuRefreshTimer: Timer?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Start as menu-bar agent (no Dock). Windows promote to .regular.
        NSApp.setActivationPolicy(.accessory)
        AppModel.shared.startProxyIfNeeded()
        setupStatusItem()
        AppModel.shared.refresh()
    }

    func applicationWillTerminate(_ notification: Notification) {
        menuRefreshTimer?.invalidate()
        AppModel.shared.stopProxyIfWeStartedIt()
    }

    /// Closing Chat/Settings must NOT quit — only tray "Quit Spock".
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    /// Dock / Cmd+Tab re-activation while a window exists: bring it forward.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if hasOpenDocumentWindows {
            if settingsWindow?.isVisible == true || settingsWindow?.isMiniaturized == true {
                showWindow(settingsWindow)
            } else if chatWindow?.isVisible == true || chatWindow?.isMiniaturized == true {
                showWindow(chatWindow)
            }
            return true
        }
        // No windows — stay accessory (menu bar only).
        return false
    }

    func applicationDidBecomeActive(_ notification: Notification) {
        // If user Cmd+Tabbed to us with no windows, drop back to accessory.
        if !hasOpenDocumentWindows {
            NSApp.setActivationPolicy(.accessory)
        }
    }

    private var hasOpenDocumentWindows: Bool {
        let settingsOpen =
            settingsWindow.map { $0.isVisible || $0.isMiniaturized } ?? false
        let chatOpen =
            chatWindow.map { $0.isVisible || $0.isMiniaturized } ?? false
        return settingsOpen || chatOpen
    }

    private func setupStatusItem() {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = item.button {
            // Icon only — no "Spock" text in the menu bar.
            button.title = ""
            button.imagePosition = .imageOnly
        }
        item.menu = buildMenu()
        statusItem = item
        updateStatusItemAppearance()

        AppModel.shared.onStatusChange = { [weak self] in
            self?.updateStatusItemAppearance()
            self?.statusItem?.menu = self?.buildMenu()
        }

        menuRefreshTimer = Timer.scheduledTimer(withTimeInterval: 2.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            AppModel.shared.refreshQuiet()
            self.updateStatusItemAppearance()
            self.statusItem?.menu = self.buildMenu()
        }
    }

    /// Green / orange / gray / red hand based on proxy health.
    private func updateStatusItemAppearance() {
        guard let button = statusItem?.button else { return }
        let status = AppModel.shared.proxyStatus
        button.image = SpockHandIcon.menuBarImage(color: status.iconColor)
        button.toolTip = "Spock · \(status.menuLabel) · \(AppModel.shared.profile) · :\(AppModel.shared.port)"
    }

    private func buildMenu() -> NSMenu {
        let menu = NSMenu()
        let model = AppModel.shared
        let status = "Spock · \(model.proxyStatus.menuLabel) · \(model.profile) · :\(model.port)"
        let statusRow = NSMenuItem(title: status, action: nil, keyEquivalent: "")
        statusRow.isEnabled = false
        menu.addItem(statusRow)
        menu.addItem(.separator())

        let chat = NSMenuItem(title: "Chat…", action: #selector(openChat), keyEquivalent: "n")
        chat.target = self
        menu.addItem(chat)

        let settings = NSMenuItem(title: "Settings…", action: #selector(openSettings), keyEquivalent: ",")
        settings.target = self
        menu.addItem(settings)

        menu.addItem(.separator())

        let profiles = NSMenuItem(title: "Profile", action: nil, keyEquivalent: "")
        let profilesMenu = NSMenu()
        for name in model.profiles {
            let item = NSMenuItem(title: name, action: #selector(selectProfile(_:)), keyEquivalent: "")
            item.target = self
            item.representedObject = name
            item.state = name == model.profile ? .on : .off
            profilesMenu.addItem(item)
        }
        if model.profiles.isEmpty {
            let empty = NSMenuItem(title: "(no profiles)", action: nil, keyEquivalent: "")
            empty.isEnabled = false
            profilesMenu.addItem(empty)
        }
        profiles.submenu = profilesMenu
        menu.addItem(profiles)

        let reload = NSMenuItem(title: "Reload config", action: #selector(reloadConfig), keyEquivalent: "r")
        reload.target = self
        menu.addItem(reload)

        menu.addItem(.separator())

        let login = NSMenuItem(title: "Login xAI…", action: #selector(loginXAI), keyEquivalent: "")
        login.target = self
        menu.addItem(login)

        let logout = NSMenuItem(title: "Logout xAI", action: #selector(logoutXAI), keyEquivalent: "")
        logout.target = self
        menu.addItem(logout)

        menu.addItem(.separator())

        let quit = NSMenuItem(title: "Quit Spock", action: #selector(quitApp), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)

        return menu
    }

    @objc private func selectProfile(_ sender: NSMenuItem) {
        guard let name = sender.representedObject as? String else { return }
        AppModel.shared.setProfile(name)
        statusItem?.menu = buildMenu()
    }

    @objc private func openChat() {
        if chatWindow == nil {
            let view = ChatView().environmentObject(AppModel.shared)
            let hosting = NSHostingController(rootView: view)
            let window = NSWindow(contentViewController: hosting)
            window.title = "Spock Chat"
            window.setContentSize(NSSize(width: 560, height: 720))
            window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
            window.isReleasedWhenClosed = false
            window.delegate = self
            window.center()
            chatWindow = window
        }
        showWindow(chatWindow)
    }

    @objc private func openSettings() {
        if settingsWindow == nil {
            let view = SettingsView().environmentObject(AppModel.shared)
            let hosting = NSHostingController(rootView: view)
            let window = NSWindow(contentViewController: hosting)
            window.title = "Spock Settings"
            // Taller for Server tools section (advisor / web search).
            window.setContentSize(NSSize(width: 780, height: 760))
            window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
            window.isReleasedWhenClosed = false
            window.delegate = self
            window.center()
            settingsWindow = window
        }
        showWindow(settingsWindow)
        AppModel.shared.refresh()
    }

    /// Show Settings/Chat as a normal app window: Dock + Cmd+Tab while open.
    private func showWindow(_ window: NSWindow?) {
        guard let window else { return }
        // .regular → appears in Dock and Cmd+Tab. Menu bar status item stays.
        NSApp.setActivationPolicy(.regular)
        if window.isMiniaturized {
            window.deminiaturize(nil)
        }
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    @objc private func reloadConfig() {
        AppModel.shared.reloadFromDisk()
        statusItem?.menu = buildMenu()
    }

    @objc private func loginXAI() { AppModel.shared.loginXAI() }
    @objc private func logoutXAI() { AppModel.shared.logoutXAI() }

    @objc private func quitApp() {
        NSApp.terminate(nil)
    }

    // MARK: - NSWindowDelegate

    func windowWillClose(_ notification: Notification) {
        // Defer until after the window is actually gone from isVisible.
        DispatchQueue.main.async { [weak self] in
            self?.enforceAccessoryIfNoWindows()
        }
    }

    func windowDidMiniaturize(_ notification: Notification) {
        // Keep .regular so user can Cmd+Tab / Dock back to the minimized window.
        NSApp.setActivationPolicy(.regular)
    }

    /// When no Settings/Chat windows remain, hide Dock icon again — do not quit.
    private func enforceAccessoryIfNoWindows() {
        guard !hasOpenDocumentWindows else {
            NSApp.setActivationPolicy(.regular)
            return
        }
        NSApp.setActivationPolicy(.accessory)
        // Drop app focus chrome; menu bar status item + proxy keep running.
        // Do not terminate.
    }
}

// MARK: - Vulcan salute tray icon (color = proxy status)

enum SpockHandIcon {
    /// Monochrome hand silhouette tinted by proxy status color.
    static func menuBarImage(color: NSColor, pointSize: CGFloat = 16) -> NSImage {
        let size = NSSize(width: pointSize + 2, height: pointSize + 2)
        let image = NSImage(size: size, flipped: false) { rect in
            color.setFill()
            color.setStroke()

            let w = rect.width
            let h = rect.height

            // Palm
            let palm = NSBezierPath(
                roundedRect: NSRect(x: w * 0.32, y: h * 0.10, width: w * 0.40, height: h * 0.36),
                xRadius: w * 0.10,
                yRadius: w * 0.10
            )
            palm.fill()

            // Fingers with Vulcan gap (between middle & ring)
            let fingerW = w * 0.12
            let baseY = h * 0.40
            let fingerH = h * 0.48
            let xs: [CGFloat] = [w * 0.30, w * 0.42, w * 0.58, w * 0.70]
            for (i, x) in xs.enumerated() {
                let fh = fingerH * ((i == 0 || i == 3) ? 0.90 : 1.0)
                let finger = NSBezierPath(
                    roundedRect: NSRect(x: x, y: baseY, width: fingerW, height: fh),
                    xRadius: fingerW * 0.45,
                    yRadius: fingerW * 0.45
                )
                finger.fill()
            }

            // Thumb
            let thumb = NSBezierPath()
            thumb.move(to: NSPoint(x: w * 0.34, y: h * 0.28))
            thumb.curve(
                to: NSPoint(x: w * 0.12, y: h * 0.50),
                controlPoint1: NSPoint(x: w * 0.10, y: h * 0.28),
                controlPoint2: NSPoint(x: w * 0.08, y: h * 0.40)
            )
            thumb.curve(
                to: NSPoint(x: w * 0.32, y: h * 0.38),
                controlPoint1: NSPoint(x: w * 0.16, y: h * 0.52),
                controlPoint2: NSPoint(x: w * 0.24, y: h * 0.42)
            )
            thumb.close()
            thumb.fill()

            return true
        }
        image.isTemplate = false
        return image
    }
}
