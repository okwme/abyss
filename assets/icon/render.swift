// Renders the abyss app icon ("Well", docs/brand/brand.md) as a macOS iconset.
//
//   swift assets/icon/render.swift <iconset-dir> [preview.png]
//   iconutil -c icns <iconset-dir> -o assets/icon/abyss.icns
//
// The artwork follows Apple's macOS icon grid: a 1024-point canvas with an
// 824-point body inset 100 points, continuous corners at 22.5% of the body,
// and a soft shadow under it. The well is the body shape repeated inward,
// each level one step down the public ramp, drifting slightly downward. The
// centre stays empty and dark; never add a highlight there.

import AppKit
import SwiftUI

/// The public ramp at hue 215, surface to floor (docs/brand/brand.md).
let ramp: [UInt32] = [0xDDEBEE, 0x93B1B9, 0x517C87, 0x244F59, 0x032C34, 0x001318]

/// Each level as left, top and width on a 100-unit body, outermost first.
/// The same numbers as crates/app/src/ui/mark.rs.
let well: [(x: CGFloat, y: CGFloat, width: CGFloat)] = [
    (0, 0, 100), (10, 12, 80), (19, 23, 62), (27, 33, 46), (34.5, 42, 31), (41, 50, 18),
]

/// At 32 pixels and under, six levels blur into each other. These sizes draw
/// four: the body, then every level that is still at least two pixels wide.
let fewLevels = [0, 1, 3, 5]
let fewTones = [0, 2, 4, 5]
let fewLevelsUpTo = 32

let canvas: CGFloat = 1024
let bodyInset: CGFloat = 100
let bodySize: CGFloat = 824
let cornerRatio: CGFloat = 0.225

func color(_ hex: UInt32) -> CGColor {
    CGColor(
        srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
        green: CGFloat((hex >> 8) & 0xFF) / 255,
        blue: CGFloat(hex & 0xFF) / 255,
        alpha: 1)
}

func squircle(_ rect: CGRect) -> CGPath {
    RoundedRectangle(cornerRadius: rect.width * cornerRatio, style: .continuous)
        .path(in: rect).cgPath
}

func render(pixels: Int) -> CGImage {
    let space = CGColorSpace(name: CGColorSpace.sRGB)!
    let context = CGContext(
        data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0,
        space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let scale = CGFloat(pixels) / canvas
    // Work in canvas points with the origin at the top left.
    context.translateBy(x: 0, y: CGFloat(pixels))
    context.scaleBy(x: scale, y: -scale)

    let few = pixels <= fewLevelsUpTo
    let levels = few ? fewLevels : Array(well.indices)
    let tones = few ? fewTones : Array(ramp.indices)
    let unit = bodySize / 100
    for (level, tone) in zip(levels, tones) {
        let shape = well[level]
        let rect = CGRect(
            x: bodyInset + shape.x * unit, y: bodyInset + shape.y * unit,
            width: shape.width * unit, height: shape.width * unit)
        context.saveGState()
        if level == 0 {
            // Shadow offsets are in device space: negative y is down.
            context.setShadow(
                offset: CGSize(width: 0, height: -12 * scale), blur: 28 * scale,
                color: CGColor(gray: 0, alpha: 0.3))
        }
        context.addPath(squircle(rect))
        context.setFillColor(color(ramp[tone]))
        context.fillPath()
        context.restoreGState()
    }
    return context.makeImage()!
}

func write(_ image: CGImage, to path: String) {
    let png = NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:])!
    try! png.write(to: URL(fileURLWithPath: path))
}

/// The small sizes magnified and the large ones as they are, on a light and
/// a dark ground, to judge legibility by eye.
func preview(to path: String) {
    let shown: [(pixels: Int, magnify: CGFloat)] = [(16, 8), (32, 4), (64, 2), (128, 1), (256, 1)]
    let gap: CGFloat = 24
    let rowHeight: CGFloat = 256 + 2 * gap
    let width = shown.reduce(gap) { $0 + CGFloat($1.pixels) * $1.magnify + gap }
    let space = CGColorSpace(name: CGColorSpace.sRGB)!
    let context = CGContext(
        data: nil, width: Int(width), height: Int(rowHeight * 2), bitsPerComponent: 8,
        bytesPerRow: 0, space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    context.interpolationQuality = .none
    for (row, ground) in [UInt32(0xECECEC), UInt32(0x1E1E1E)].enumerated() {
        let top = CGFloat(row) * rowHeight
        context.setFillColor(color(ground))
        context.fill(CGRect(x: 0, y: top, width: width, height: rowHeight))
        var x = gap
        for item in shown {
            let side = CGFloat(item.pixels) * item.magnify
            context.draw(
                render(pixels: item.pixels),
                in: CGRect(x: x, y: top + (rowHeight - side) / 2, width: side, height: side))
            x += side + gap
        }
    }
    write(context.makeImage()!, to: path)
}

let arguments = CommandLine.arguments
guard arguments.count >= 2 else {
    FileHandle.standardError.write(Data("usage: render.swift <iconset-dir> [preview.png]\n".utf8))
    exit(2)
}
let iconset = arguments[1]
try! FileManager.default.createDirectory(atPath: iconset, withIntermediateDirectories: true)
for points in [16, 32, 128, 256, 512] {
    write(render(pixels: points), to: "\(iconset)/icon_\(points)x\(points).png")
    write(render(pixels: points * 2), to: "\(iconset)/icon_\(points)x\(points)@2x.png")
}
if arguments.count >= 3 {
    preview(to: arguments[2])
}
