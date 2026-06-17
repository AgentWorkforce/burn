import SwiftUI
import AppKit
import Combine

@main
struct BurnApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate

    var body: some Scene {
        // No SwiftUI scene drives the menu bar. The status item is created once,
        // imperatively, by AppDelegate. SwiftUI's `MenuBarExtra` can duplicate
        // its status item when the app's scene body re-evaluates — that runaway
        // ("endless menu bar flames") panicked the machine. An AppKit
        // NSStatusItem created a single time cannot be duplicated.
        Settings { EmptyView() }
    }
}

/// Owns the single menu bar status item and its popover. Created once in
/// `applicationDidFinishLaunching`; the flame image is mirrored from the view
/// model's cached icon. No SwiftUI rendering touches the status item, so there
/// is no render path that can storm or duplicate it.
@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let viewModel = UsageViewModel()
    private var statusItem: NSStatusItem?
    private let popover = NSPopover()
    private var iconObserver: AnyCancellable?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory) // menu-bar-only, no Dock icon

        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.button?.image = viewModel.menuBarIcon
        item.button?.action = #selector(togglePopover(_:))
        item.button?.target = self
        statusItem = item

        popover.behavior = .transient
        popover.contentViewController = NSHostingController(
            rootView: ContentView(viewModel: viewModel))

        // Mirror the cached flame onto the status button whenever it changes.
        iconObserver = viewModel.$menuBarIcon.sink { [weak item] image in
            item?.button?.image = image
        }
    }

    @objc private func togglePopover(_ sender: NSStatusBarButton) {
        if popover.isShown {
            popover.performClose(sender)
            return
        }
        popover.show(relativeTo: sender.bounds, of: sender, preferredEdge: .minY)
        popover.contentViewController?.view.window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

/// Renders the colored menu bar flame to a non-template `NSImage` (so the menu
/// bar preserves the color instead of flattening it to monochrome). Called off
/// any view-render path — only by the view model when usage changes.
enum MenuBarIcon {
    /// Fixed size — a usage-varying size would shift the menu bar layout. Usage
    /// is conveyed by color and fill instead.
    private static let size: CGFloat = 15

    @MainActor
    static func render(usage: Int?, offTarget: Bool) -> NSImage {
        let symbol = offTarget ? "flame.fill" : "flame"
        let color: Color
        if offTarget {
            color = .red
        } else {
            let t = min(1, Double(usage ?? 0) / 100)
            color = Color(red: 1.0, green: 0.55 - 0.32 * t, blue: 0.19 * t) // orange→red
        }
        let renderer = ImageRenderer(content:
            Image(systemName: symbol)
                .font(.system(size: size, weight: .semibold))
                .foregroundStyle(color)
                .padding(1)
        )
        renderer.scale = 2
        let image = renderer.nsImage ?? NSImage()
        image.isTemplate = false
        return image
    }
}
