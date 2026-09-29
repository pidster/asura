// swift-tools-version: 6.4
import PackageDescription

let package = Package(
    name: "AsuraModelHelper",
    platforms: [.macOS("27.0")],
    products: [.executable(name: "asura-model", targets: ["AsuraModelHelper"])],
    dependencies: [
        .package(url: "https://github.com/apple/swift-protobuf.git", revision: "55d7a1cc5666b85c13464aea1c4b4a90feccb4c8"),
        .package(url: "https://github.com/apple/coreai-models.git", revision: "3f109efd54273391f9fd9f5f5b3d8c6e99836d55"),
        .package(url: "https://github.com/ml-explore/mlx-swift-lm.git", revision: "c6446cf7bfb7cea76408013b614d4b2c530eaa03"),
        .package(url: "https://github.com/huggingface/swift-transformers", revision: "c21fdcde390313a6d98d8e33a346f2c3486c3ab0")
    ],
    targets: [
        .target(name: "HelperCore", dependencies: [
            .product(name: "SwiftProtobuf", package: "swift-protobuf"),
            .product(name: "CoreAILM", package: "coreai-models"),
            .product(name: "MLXFoundationModels", package: "mlx-swift-lm"),
            .product(name: "MLXHuggingFace", package: "mlx-swift-lm"),
            .product(name: "MLXLLM", package: "mlx-swift-lm"),
            .product(name: "MLXLMCommon", package: "mlx-swift-lm"),
            .product(name: "Tokenizers", package: "swift-transformers")
        ]),
        .executableTarget(name: "AsuraModelHelper", dependencies: ["HelperCore"]),
        .testTarget(name: "HelperCoreTests", dependencies: ["HelperCore"])
    ]
)
