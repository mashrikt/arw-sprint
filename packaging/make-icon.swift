// Code-native app artwork. No external assets, downloads, or GUI access required.
import Foundation
import CoreGraphics
import ImageIO

guard CommandLine.arguments.count == 2 else {
    fatalError("usage: make-icon.swift output.iconset")
}
let output = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
let colorSpace = CGColorSpace(name: CGColorSpace.sRGB)!
func color(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: colorSpace, components: [r, g, b, a])!
}

func makeIcon(_ pixels: Int, _ name: String) throws {
    guard let context = CGContext(data: nil, width: pixels, height: pixels,
                                  bitsPerComponent: 8, bytesPerRow: pixels * 4,
                                  space: colorSpace,
                                  bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
        throw NSError(domain: "FastCullIcon", code: 1)
    }
    context.scaleBy(x: CGFloat(pixels) / 1024, y: CGFloat(pixels) / 1024)
    // macOS icon safe area, with transparent corners outside the rounded square.
    let tile = CGPath(roundedRect: CGRect(x: 64, y: 64, width: 896, height: 896),
                      cornerWidth: 198, cornerHeight: 198, transform: nil)
    context.addPath(tile)
    context.clip()
    let gradient = CGGradient(colorsSpace: colorSpace,
                              colors: [color(0.11, 0.15, 0.19), color(0.025, 0.04, 0.06)] as CFArray,
                              locations: [0, 1])!
    context.drawLinearGradient(gradient, start: CGPoint(x: 170, y: 960),
                               end: CGPoint(x: 800, y: 64), options: [])
    // A photograph frame, kept broad enough to read at 16 px.
    context.setStrokeColor(color(0.89, 0.94, 0.97))
    context.setLineWidth(42)
    context.addPath(CGPath(roundedRect: CGRect(x: 196, y: 256, width: 632, height: 512),
                           cornerWidth: 54, cornerHeight: 54, transform: nil))
    context.strokePath()
    context.setFillColor(color(0.89, 0.94, 0.97, 0.95))
    context.fillEllipse(in: CGRect(x: 268, y: 596, width: 96, height: 96))
    let landscape = CGMutablePath()
    landscape.move(to: CGPoint(x: 242, y: 326))
    landscape.addLine(to: CGPoint(x: 430, y: 548))
    landscape.addLine(to: CGPoint(x: 616, y: 326))
    landscape.closeSubpath()
    context.addPath(landscape)
    context.fillPath()
    // A gold next-frame chevron is the only accent.
    context.setStrokeColor(color(1.0, 0.71, 0.25))
    context.setLineWidth(70)
    context.setLineCap(.round)
    context.setLineJoin(.round)
    context.move(to: CGPoint(x: 664, y: 432))
    context.addLine(to: CGPoint(x: 760, y: 512))
    context.addLine(to: CGPoint(x: 664, y: 592))
    context.strokePath()
    guard let image = context.makeImage(),
          let destination = CGImageDestinationCreateWithURL(output.appendingPathComponent(name) as CFURL,
                                                            "public.png" as CFString, 1, nil) else {
        throw NSError(domain: "FastCullIcon", code: 2)
    }
    CGImageDestinationAddImage(destination, image, nil)
    guard CGImageDestinationFinalize(destination) else {
        throw NSError(domain: "FastCullIcon", code: 3)
    }
}

for points in [16, 32, 128, 256, 512] {
    try makeIcon(points, "icon_\(points)x\(points).png")
    try makeIcon(points * 2, "icon_\(points)x\(points)@2x.png")
}
