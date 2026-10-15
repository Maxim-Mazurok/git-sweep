#!/usr/bin/env swift
import AppKit

// AppKit renders the SVG directly at every target size. No raster scaling or
// third-party SVG renderer is needed; the source remains the single authority.
func render(_ image: NSImage, pixels: Int, to destination: URL) throws {
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
        isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    ), let context = NSGraphicsContext(bitmapImageRep: bitmap) else {
        throw NSError(domain: "GitSweepIcon", code: 1,
                      userInfo: [NSLocalizedDescriptionKey: "Cannot create icon bitmap"])
    }
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = context
    context.imageInterpolation = .high
    let rect = NSRect(x: 0, y: 0, width: pixels, height: pixels)
    context.cgContext.clear(rect)
    image.draw(in: rect, from: .zero, operation: .copy, fraction: 1)
    guard let png = bitmap.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "GitSweepIcon", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "Cannot encode icon PNG"])
    }
    try png.write(to: destination, options: .atomic)
}

do {
    guard CommandLine.arguments.count == 4,
          let image = NSImage(contentsOfFile: CommandLine.arguments[1]) else {
        throw NSError(domain: "GitSweepIcon", code: 3,
                      userInfo: [NSLocalizedDescriptionKey: "Usage: render_icon.swift SOURCE.svg ICONSET PREVIEW.png"])
    }
    let iconset = URL(fileURLWithPath: CommandLine.arguments[2], isDirectory: true)
    for points in [16, 32, 128, 256, 512] {
        for scale in [1, 2] {
            let suffix = scale == 2 ? "@2x" : ""
            let name = "icon_\(points)x\(points)\(suffix).png"
            try render(image, pixels: points * scale, to: iconset.appendingPathComponent(name))
        }
    }
    try render(image, pixels: 1024, to: URL(fileURLWithPath: CommandLine.arguments[3]))
} catch {
    FileHandle.standardError.write(Data("\(error.localizedDescription)\n".utf8))
    exit(1)
}
