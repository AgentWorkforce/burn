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
/// burning off its target pace. The flame is rendered to a non-template image so
/// the menu bar preserves its color instead of flattening it to monochrome.
struct MenuBarLabel: View {
    @ObservedObject var viewModel: UsageViewModel

    var body: some View {
        Image(nsImage: flameImage)
            .renderingMode(.original)
    }

    /// Outline flame while on pace; a solid "hot" flame once over pace.
    private var symbol: String {
        viewModel.headlineOffTarget ? "flame.fill" : "flame"
    }

    /// Fixed size — a usage-varying size would shift the menu bar layout. Usage
    /// is conveyed by color and fill instead.
    private let flameSize: CGFloat = 15

    /// Warms from orange toward red as usage climbs, and is full red whenever the
    /// window is off its target pace.
    private var flameColor: Color {
        if viewModel.headlineOffTarget { return .red }
        let t = min(1, Double(viewModel.headlineUsage ?? 0) / 100)
        // orange #FF8C00 → red #FF3B30
        return Color(red: 1.0, green: 0.55 - 0.32 * t, blue: 0.19 * t)
    }

    /// Rasterizes the colored flame. `isTemplate = false` stops the menu bar from
    /// re-tinting it monochrome.
    private var flameImage: NSImage {
        let renderer = ImageRenderer(content:
            Image(systemName: symbol)
                .font(.system(size: flameSize, weight: .semibold))
                .foregroundStyle(flameColor)
                .padding(1)
        )
        renderer.scale = 2
        let image = renderer.nsImage ?? NSImage()
        image.isTemplate = false
        return image
    }
}
