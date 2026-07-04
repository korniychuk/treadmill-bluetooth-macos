// One-time build-time asset generator — NOT part of the shipped Rust binary.
// Rasterizes the SF Symbol "figure.walk" onto a rounded, gradient-filled
// square so the notification/app icon looks like a proper app icon rather
// than a bare glyph. Run via `swift scripts/generate-icon.swift <output.png>`;
// scripts/build-icon.sh drives the full PNG -> iconset -> .icns pipeline.
import AppKit

let size = 1024.0
let outputPath = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon-1024.png"

guard let symbol = NSImage(systemSymbolName: "figure.walk", accessibilityDescription: nil) else {
    fatalError("SF Symbol 'figure.walk' not found on this OS")
}
let config = NSImage.SymbolConfiguration(pointSize: size * 0.58, weight: .semibold)
guard let glyph = symbol.withSymbolConfiguration(config) else {
    fatalError("could not apply symbol configuration")
}

// Tint the glyph white on its own transparent canvas first: sourceAtop only
// affects pixels where destination alpha > 0, so this must happen on a layer
// that starts fully transparent. Doing it directly on the opaque background
// would flood-fill the glyph's whole bounding box white instead of just its
// silhouette (that was the first, broken attempt).
let glyphSize = glyph.size
let whiteGlyph = NSImage(size: glyphSize)
whiteGlyph.lockFocus()
glyph.draw(at: .zero, from: .zero, operation: .sourceOver, fraction: 1.0)
NSColor.white.set()
NSRect(origin: .zero, size: glyphSize).fill(using: .sourceAtop)
whiteGlyph.unlockFocus()

let image = NSImage(size: NSSize(width: size, height: size))
image.lockFocus()

let bgRect = NSRect(x: 0, y: 0, width: size, height: size)
let cornerRadius = size * 0.22
let bgPath = NSBezierPath(roundedRect: bgRect, xRadius: cornerRadius, yRadius: cornerRadius)
let gradient = NSGradient(
    colors: [
        NSColor(calibratedRed: 0.13, green: 0.58, blue: 0.96, alpha: 1.0),
        NSColor(calibratedRed: 0.04, green: 0.28, blue: 0.62, alpha: 1.0),
    ]
)
gradient?.draw(in: bgPath, angle: -90)

let origin = NSPoint(x: (size - glyphSize.width) / 2, y: (size - glyphSize.height) / 2 - size * 0.02)
whiteGlyph.draw(at: origin, from: .zero, operation: .sourceOver, fraction: 1.0)

image.unlockFocus()

guard let tiff = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff),
    let png = bitmap.representation(using: .png, properties: [:])
else {
    fatalError("could not encode PNG")
}
try! png.write(to: URL(fileURLWithPath: outputPath))
print("wrote \(outputPath)")
