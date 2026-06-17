import SwiftUI
import AppKit

extension ProviderName {
    /// Brand accent color — tints the monochrome marks and fills the selected
    /// provider's chip.
    var brandColor: Color {
        switch self {
        case .claude: return Color(red: 0.85, green: 0.47, blue: 0.34)   // Claude coral #D97757
        case .codex: return Color(red: 0.06, green: 0.64, blue: 0.50)    // OpenAI green #10A37F
        }
    }

    /// Resource name of the bundled lobe-icons SVG.
    private var iconResource: String {
        switch self {
        case .claude: return "claude"
        case .codex: return "openai"
        }
    }

    /// Color applied to a monochrome `currentColor` mark, or `nil` to render the
    /// SVG's own colors as-is.
    private var iconTint: NSColor? {
        switch self {
        case .claude: return nil      // claude.svg carries its own coral fill
        case .codex: return .white    // OpenAI mark rendered in white
        }
    }

    /// Loads the bundled lobe-icons SVG: self-colored marks (Claude) render as-is,
    /// monochrome marks (OpenAI) are tinted to `iconTint`. Falls back to `nil` if
    /// the asset can't be loaded (the view substitutes an SF Symbol).
    var brandImage: NSImage? {
        BrandIconCache.shared.image(named: iconResource, tint: iconTint)
    }

    /// SF Symbol fallback when the SVG can't be rendered.
    var fallbackSymbol: String {
        switch self {
        case .claude: return "sparkle"
        case .codex: return "brain"
        }
    }
}

/// Caches `NSImage`s loaded from the bundled SVG resources. A `nil` tint renders
/// the SVG's own colors; a non-nil tint recolors the opaque pixels to it while
/// preserving the glyph's transparency (for monochrome marks).
private final class BrandIconCache {
    static let shared = BrandIconCache()
    private var cache: [String: NSImage] = [:]

    func image(named name: String, tint: NSColor?) -> NSImage? {
        let key = "\(name):\(tint?.hashValue ?? 0)"
        if let cached = cache[key] { return cached }
        guard let url = Bundle.module.url(forResource: name, withExtension: "svg"),
              let base = NSImage(contentsOf: url) else {
            return nil
        }
        guard let tint else {
            base.isTemplate = false
            cache[key] = base
            return base
        }
        // The lobe-icons SVGs declare `width="1em"`, so `base.size` is 1×1 —
        // rasterize at a fixed high resolution instead (the image is later scaled
        // down to ~17pt by the view).
        let size = NSSize(width: 64, height: 64)
        let tinted = NSImage(size: size)
        let rect = NSRect(origin: .zero, size: size)
        tinted.lockFocus()
        base.draw(in: rect)
        tint.set()
        // `.sourceAtop` recolors only the already-drawn (opaque) pixels.
        rect.fill(using: .sourceAtop)
        tinted.unlockFocus()
        tinted.isTemplate = false
        cache[key] = tinted
        return tinted
    }
}

/// Renders a provider's brand icon, tinted to the given color, with an SF Symbol
/// fallback if the SVG can't be loaded.
struct ProviderIcon: View {
    let provider: ProviderName
    var size: CGFloat = 18

    var body: some View {
        Group {
            if let image = provider.brandImage {
                Image(nsImage: image)
                    .resizable()
                    .renderingMode(.original)
                    .interpolation(.high)
            } else {
                Image(systemName: provider.fallbackSymbol)
                    .resizable()
            }
        }
        .scaledToFit()
        .frame(width: size, height: size)
    }
}
