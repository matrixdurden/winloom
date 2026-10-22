//! Tıklayıcı'nın winloom'a tanımı.

use ortak::tool::Tool;

pub static TOOL: Tool = Tool {
    id: "clicker",
    names: ["Clicker", "Tıklayıcı"],
    tagline: [
        "Any number of keys: each clicks, scrolls or types for you",
        "İstediğin kadar tuş: her biri senin yerine tıklar, kaydırır ya da tuşa basar",
    ],
    note: ["No internet needed", "İnternet gerekmez"],
    off_note: [
        "Its trigger keys are released and its settings are deleted.",
        "Tetik tuşları bırakılır, ayarları silinir.",
    ],
    glyph: "\u{E962}",
    icons: ortak::icons!("clicker"),
    installed: crate::installed,
    install: crate::install,
    uninstall: crate::uninstall,
    leftovers: crate::leftovers,
    start: |hwnd| Box::new(crate::Clicker::start(hwnd)),
    ..Tool::BASE
};
