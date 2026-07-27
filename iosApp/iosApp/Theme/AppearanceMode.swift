import Foundation

/// The PWA's three appearance modes (U18, KTD-11, R12), persisted under the
/// same `theme` key with the same string values (`hybrid`/`dark`/`light`) as
/// the PWA's localStorage entry and Android's DataStore entry — parity in
/// behavior and vocabulary.
///
/// - hybrid (default): bone accent floods Home/Activity/tab bar; Send /
///   Receive / Settings stay warm near-black.
/// - dark: warm near-black everywhere, ember on the action.
/// - light: warm paper everywhere, ember on the action.
enum AppearanceMode: String, CaseIterable {
    case hybrid
    case dark
    case light

    static let `default` = AppearanceMode.hybrid

    /// PWA localStorage key parity: `theme`.
    static let storageKey = "theme"

    /// Unknown/absent stored values fall back to the default, like the PWA.
    static func fromStorage(_ value: String?) -> AppearanceMode {
        value.flatMap(AppearanceMode.init(rawValue:)) ?? .default
    }

    /// Synchronous read for pre-first-frame theme application (KTD-11: the
    /// persisted selection is applied at scene setup, before render —
    /// WalletModel reads this in its initializer).
    static func loadPersisted(from defaults: UserDefaults = .standard) -> AppearanceMode {
        fromStorage(defaults.string(forKey: storageKey))
    }

    func persist(to defaults: UserDefaults = .standard) {
        defaults.set(rawValue, forKey: Self.storageKey)
    }
}
