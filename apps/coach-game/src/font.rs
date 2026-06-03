//! Global default-font override so Devanagari renders in the UI.
//!
//! Bevy's embedded default font (FiraSans, behind `bevy_text`'s
//! `default_font` feature, pulled in transitively by the `2d`/`ui`
//! features) covers Latin only — the Sargam-Devanagari note system
//! renders tofu without a Devanagari-capable face. Rather than thread a
//! `Handle<Font>` through every `TextFont { .., ..default() }` spawn
//! site, we replace the asset that backs the *default* font handle:
//! load Noto Sans Devanagari, then copy it over `AssetId::<Font>::default()`
//! once it finishes loading. Every `..default()` text node then picks it
//! up — including ones already spawned (text reflows when the asset
//! lands). FiraSans remains the first-frame fallback, so there's no
//! blank-text race while the .ttf loads.
//!
//! The font is a dev-time asset fetched by `scripts/fetch-assets.sh`
//! (not checked into git). If it is absent the load silently fails and
//! the UI keeps FiraSans — Latin still renders, only Devanagari tofus.
//! This lives in the production renderer path (`main`), not
//! `build_app`, because headless tests have no AssetServer.

use bevy::prelude::*;

/// Handle to the loading Devanagari font, kept alive until it has been
/// promoted to the default slot.
#[derive(Resource)]
pub struct DefaultFontLoad(Handle<Font>);

/// Kick off the async load. The path is relative to the `assets/` dir
/// (`apps/coach-game/assets/fonts/NotoSansDevanagari.ttf`).
pub fn load(mut commands: Commands, asset_server: Res<AssetServer>) {
    let handle = asset_server.load("fonts/NotoSansDevanagari.ttf");
    commands.insert_resource(DefaultFontLoad(handle));
}

/// When the font finishes loading, copy it over the default font slot
/// (`AssetId::<Font>::default()`) so every `..default()` text node
/// renders with it, then drop the marker resource so this stops running.
pub fn promote_to_default(
    mut events: MessageReader<AssetEvent<Font>>,
    mut fonts: ResMut<Assets<Font>>,
    loading: Option<Res<DefaultFontLoad>>,
    mut commands: Commands,
) {
    let Some(loading) = loading else {
        return;
    };
    for event in events.read() {
        let AssetEvent::LoadedWithDependencies { id } = event else {
            continue;
        };
        if *id != loading.0.id() {
            continue;
        }
        if let Some(font) = fonts.get(*id).cloned() {
            // Overwrite the default font slot. The only documented
            // failure is an already-freed handle, which can't apply to
            // the default id; ignore the result rather than panic.
            let _ = fonts.insert(AssetId::<Font>::default(), font);
        }
        commands.remove_resource::<DefaultFontLoad>();
    }
}
