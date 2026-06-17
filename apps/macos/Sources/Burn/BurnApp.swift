import SwiftUI

@main
struct BurnApp: App {
    @StateObject private var viewModel = UsageViewModel()

    var body: some Scene {
        MenuBarExtra {
            ContentView(viewModel: viewModel)
        } label: {
            MenuBarLabel(viewModel: viewModel)
        }
        .menuBarExtraStyle(.window)
    }
}

/// The label shown in the menu bar: a fixed-size flame colored by the highest
/// current usage (orange→red) that fills (turns "hot") when that window is
/// burning off its target pace.
///
/// IMPORTANT: the flame image is rendered by the view model when usage changes
/// and cached — `body` only displays it. Rendering with `ImageRenderer` *inside*
/// this `body` is re-entrant SwiftUI rendering and makes `MenuBarExtra` spawn
/// status items in a runaway loop, so never move it back here.
struct MenuBarLabel: View {
    @ObservedObject var viewModel: UsageViewModel

    var body: some View {
        Image(nsImage: viewModel.menuBarIcon)
            .renderingMode(.original)
    }
}

/// Renders the colored menu bar flame to a non-template `NSImage` (so the menu
/// bar preserves the color instead of flattening it to monochrome). Called off
/// the view-render path — see `MenuBarLabel`.
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
