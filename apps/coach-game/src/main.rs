//! Production entry point. The real wiring lives in `lib::build_app`;
//! this file just adds the renderer + the real `Coach` construction.

use bevy::prelude::*;
use coach_game::{build_app, coach};

fn main() {
    let mut app = App::new();
    app.add_plugins(DefaultPlugins);
    app.add_systems(Startup, (spawn_camera, coach::spawn_coach));
    build_app(&mut app);
    app.run();
}

fn spawn_camera(mut commands: Commands) {
    commands.spawn(Camera2d);
}
