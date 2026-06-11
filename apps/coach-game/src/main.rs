//! Production entry point. The real wiring lives in `lib::build_app`;
//! this file just adds the renderer, the real `Coach` construction, and the
//! UX flight recorder (`trace`) — the recorder is wired *here*, not in
//! `build_app`, so headless tests never sprout trace directories.

use std::path::PathBuf;

use bevy::prelude::*;
use bevy::window::Window;
use coach_game::trace::TracePlugin;
use coach_game::{build_app, coach, font, trace};

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "Gurukul".to_string(),
            ..default()
        }),
        ..default()
    }));

    // Build the real coach and wrap it in the recording decorator *before* it
    // becomes the `Coach` handle, then stash the shared trace buffer. The rest
    // of the app still holds a plain `Box<dyn AppCoach>` and is unaware.
    trace::install_recording_coach(app.world_mut(), coach::build_coach());

    // One trace directory per run, under a gitignored `traces/` next to the
    // working directory. Name + header stamped from wall-clock at launch.
    let (run_dir, wall_start) = trace::launch_stamp();
    app.add_plugins(TracePlugin {
        root: PathBuf::from("traces"),
        run_dir,
        wall_start,
        replay_of: None,
    });

    app.add_systems(Startup, (spawn_camera, font::load));
    // Promote the Devanagari font to the default slot once it loads.
    // Runs every frame until the asset lands, then removes its marker.
    app.add_systems(Update, font::promote_to_default);
    build_app(&mut app);
    app.run();
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}
