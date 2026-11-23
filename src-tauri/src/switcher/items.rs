//! Building a card from a window.

use windows::Win32::UI::WindowsAndMessaging::IsIconic;

use crate::{
    icons::{app_colors, cache_lookup, icon_cache, name_cache},
    ipc::SwitchItem,
    settings_model::CardStyle,
    targets::{
        identity::{fallback_name, target_key},
        Target,
    },
};

/// Builds a card using only cached icons and display names. Called from the
/// keyboard hook, so it must never touch GDI, COM, or the shell.
pub(crate) fn target_to_item_cached(target: &Target, style: CardStyle) -> SwitchItem {
    let key = target_key(target);
    let subtitle = target
        .app_id
        .as_ref()
        .and_then(|_| cache_lookup(name_cache(), &key))
        .flatten()
        .unwrap_or_else(|| fallback_name(target));
    let title = if target.title.is_empty() {
        subtitle.clone()
    } else {
        target.title.clone()
    };
    SwitchItem {
        id: target.hwnd.0 as u64,
        title,
        subtitle: subtitle.clone(),
        icon: cache_lookup(icon_cache(), &key).flatten(),
        accent: accent_for(&subtitle),
        colors: app_colors(&key),
        window_count: target.window_count,
        // IsIconic is a cheap user32 read, safe on the hook thread.
        thumbnail: style == CardStyle::Preview && !unsafe { IsIconic(target.hwnd) }.as_bool(),
    }
}

fn accent_for(value: &str) -> String {
    const PALETTE: [&str; 7] = [
        "#2678d9", "#8b54d6", "#d55d77", "#0c9b88", "#c77d19", "#4f7aa9", "#8c5a42",
    ];
    let hash = value.bytes().fold(0usize, |total, byte| {
        total.wrapping_mul(31).wrapping_add(byte as usize)
    });
    PALETTE[hash % PALETTE.len()].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accents_are_deterministic_palette_colors() {
        assert_eq!(accent_for(""), "#2678d9");
        assert_eq!(accent_for("a"), "#8c5a42");
        assert_eq!(accent_for("Firefox"), "#c77d19");
        assert_eq!(accent_for("Visual Studio Code"), "#8c5a42");
        // Multi-byte text hashes its UTF-8 bytes.
        assert_eq!(accent_for("Explorateur de fichiers"), "#8b54d6");
        for name in ["Firefox", "Notepad", "Paint", "日本語"] {
            assert_eq!(accent_for(name), accent_for(name));
            assert!(accent_for(name).starts_with('#') && accent_for(name).len() == 7);
        }
    }
}
