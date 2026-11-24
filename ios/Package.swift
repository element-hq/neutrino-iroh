// swift-tools-version: 5.9

import PackageDescription

let package = Package(name: "Neutrino",
                      platforms: [.iOS(.v17)],
                      products: [.library(name: "Neutrino", targets: ["Neutrino"])],
                      targets: [
                          .binaryTarget(name: "NeutrinoFFI", path: "NeutrinoFFI.xcframework"),
                          .target(name: "Neutrino",
                                  dependencies: ["NeutrinoFFI"],
                                  linkerSettings: [.linkedFramework("CoreBluetooth"),
                                                   .linkedFramework("SystemConfiguration"),
                                                   .linkedFramework("Security")])
                      ])
