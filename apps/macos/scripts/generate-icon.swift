import AppKit

let S: CGFloat = 1024
let canvas = NSImage(size: NSSize(width: S, height: S))
canvas.lockFocus()

// Rounded-rect (squircle-ish) background with a dark slate gradient.
let inset: CGFloat = 0  // full-bleed squircle (macOS masks corners itself on recent OS, but we bake them)
let rect = NSRect(x: inset, y: inset, width: S - 2*inset, height: S - 2*inset)
let bg = NSBezierPath(roundedRect: rect, xRadius: 230, yRadius: 230)
bg.addClip()
NSGradient(colors: [NSColor(red:0.17,green:0.17,blue:0.19,alpha:1),
                    NSColor(red:0.10,green:0.10,blue:0.12,alpha:1)])!
    .draw(in: rect, angle: -90)

// Gradient-filled flame, centered.
let cfg = NSImage.SymbolConfiguration(pointSize: 640, weight: .bold)
let base = NSImage(systemSymbolName: "flame.fill", accessibilityDescription: nil)!
    .withSymbolConfiguration(cfg)!
let fs = base.size
let gradImg = NSImage(size: fs); gradImg.lockFocus()
NSGradient(colors: [NSColor(red:1.0,green:0.62,blue:0.16,alpha:1),   // orange (top)
                    NSColor(red:0.91,green:0.15,blue:0.12,alpha:1)])! // red (bottom)
    .draw(in: NSRect(origin: .zero, size: fs), angle: -90)
gradImg.unlockFocus()
let flame = NSImage(size: fs); flame.lockFocus()
base.draw(in: NSRect(origin: .zero, size: fs))
gradImg.draw(in: NSRect(origin: .zero, size: fs), from: .zero, operation: .sourceAtop, fraction: 1)
flame.unlockFocus()
let fx = (S - fs.width)/2, fy = (S - fs.height)/2
flame.draw(in: NSRect(x: fx, y: fy, width: fs.width, height: fs.height))

canvas.unlockFocus()
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "/tmp/icon_1024.png"
let png = NSBitmapImageRep(data: canvas.tiffRepresentation!)!.representation(using: .png, properties: [:])!
try! png.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
