//
//  GurukulApp.swift
//  Gurukul
//
//  Created by Kaiwalya Kher on 5/16/26.
//

import SwiftUI

@main
struct GurukulApp: App {
    /// Construct the audio infrastructure once at launch, before any
    /// window opens. ContentView and SettingsView both reference the
    /// same instances so changes in one are visible in the other.
    ///
    /// `pipeline` is a `let` (not `@State`) because:
    ///   1. SwiftUI's App.init runs once per process lifetime, so we
    ///      don't need @State's "stash on first read" semantics.
    ///   2. AudioPipeline is `nonisolated final class` — it must stay
    ///      thread-agnostic for the audio thread. Wrapping in
    ///      @StateObject would drag in ObservableObject's MainActor
    ///      isolation in Swift 6.
    private let pipeline: AudioPipeline
    @StateObject private var catalog = AudioDeviceCatalog()
    private let initialSettings: AudioSettings

    init() {
        let loaded = Prefs.loadAudioSettings()
        self.initialSettings = loaded
        self.pipeline = AudioPipeline(initialSettings: loaded)
    }

    var body: some Scene {
        WindowGroup {
            ContentView(pipeline: pipeline)
        }
        Settings {
            SettingsView(
                pipeline: pipeline,
                catalog: catalog,
                initial: initialSettings
            )
        }
    }
}
