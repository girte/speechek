//! Build flavors: one compile-time table fixing the identity, the location of
//! the settings profile, the first-run launcher chord and the first-run
//! loopback port of a build.
//!
//! A shipped executable is always [`BuildFlavor::Production`]; a debug build
//! is a separate flavor, so a development or test run can never touch the
//! installed application's settings document, key container, WebView2 data
//! directory or single-instance guard. The flavor is decided by
//! `debug_assertions` and the `test-provider` feature alone: there is no
//! runtime switch and no environment variable that could turn a release build
//! into another flavor, and [`defaults`] is the single place these values are
//! written down. The identifier and the product name are copied into the Tauri
//! context before the shell is built, so the single-instance guard, the
//! WebView2 user-data directory and the tray belong to the flavor's own
//! identity even when a debug executable is started by double-clicking it.
//!
//! * [`BuildFlavor::Production`] - the distributed shell:
//!   `app.speechek.desktop`, `%APPDATA%/Speechek`, `F2`, port `4173`.
//! * [`BuildFlavor::Development`] - a portable debug build that talks to the
//!   real provider: `app.speechek.dev`, `settings.json` and `secrets.bin`
//!   beside the executable it runs from (never `%APPDATA%`),
//!   `Ctrl+Shift+F9`, port `4174`.
//! * [`BuildFlavor::Test`] - a debug build with the local fake-provider
//!   harness (`--features test-provider`): `app.speechek.test`,
//!   `%APPDATA%/Speechek-Test`, `Ctrl+Shift+F10`, port `4175`.

/// Which build this is. [`ACTIVE`] fixes the variant at compile time, so one
/// process can never be two flavors and a released build cannot be talked into
/// another flavor's profile.
///
/// The table's other rows are not constructed in every build - the flavor is a
/// compile-time choice - so they are kept without being built here.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BuildFlavor {
    Production,
    Development,
    Test,
}

/// The values one flavor fixes: the bundle identifier and product name the
/// shell runs under, the `%APPDATA%` directory its settings document lives in
/// for the flavors that keep one, and the launcher chord and loopback port its
/// first-run document spells out.
pub(crate) struct ProfileDefaults {
    pub(crate) identifier: &'static str,
    pub(crate) title: &'static str,
    /// The `%APPDATA%` directory production and test keep their settings
    /// document in; empty for the development flavor, which resolves beside
    /// its executable instead.
    pub(crate) directory: &'static str,
    pub(crate) hotkey: &'static str,
    pub(crate) port: u16,
}

/// The compile-time flavor table: the single source of every value above, so a
/// profile directory, a first-run chord and a port are never written down
/// twice.
pub(crate) const fn defaults(flavor: BuildFlavor) -> ProfileDefaults {
    match flavor {
        BuildFlavor::Production => ProfileDefaults {
            identifier: "app.speechek.desktop",
            title: "Speechek",
            directory: "Speechek",
            hotkey: "F2",
            port: 4173,
        },
        BuildFlavor::Development => ProfileDefaults {
            identifier: "app.speechek.dev",
            title: "Speechek Dev",
            // The development flavor never resolves through `%APPDATA%`: its
            // `settings.json` and `secrets.bin` live beside the executable it
            // runs from, so no profile directory is named here.
            directory: "",
            hotkey: "Ctrl+Shift+F9",
            port: 4174,
        },
        BuildFlavor::Test => ProfileDefaults {
            identifier: "app.speechek.test",
            title: "Speechek Test",
            directory: "Speechek-Test",
            hotkey: "Ctrl+Shift+F10",
            port: 4175,
        },
    }
}

/// The flavor this build runs as: any non-debug build is the production shell,
/// a debug build without the fake-provider harness is the isolated development
/// flavor, and the harness itself makes the test flavor.
#[cfg(not(debug_assertions))]
pub(crate) const ACTIVE: BuildFlavor = BuildFlavor::Production;

#[cfg(all(debug_assertions, feature = "test-provider"))]
pub(crate) const ACTIVE: BuildFlavor = BuildFlavor::Test;

#[cfg(all(debug_assertions, not(feature = "test-provider")))]
pub(crate) const ACTIVE: BuildFlavor = BuildFlavor::Development;
