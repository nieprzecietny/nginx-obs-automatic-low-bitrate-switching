use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use futures_util::StreamExt;
use obws::events::EventType;
use serde::Deserialize;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::{config, error, noalbs, state::ClientStatus};

use super::BroadcastingSoftwareLogic;

pub struct Obs {
    connection: Arc<Mutex<Option<obws::Client>>>,
    connection_join: tokio::task::JoinHandle<()>,
    event_join: tokio::task::JoinHandle<()>,
}

impl Obs {
    pub fn new(connection_info: config::ObsConfig, state: noalbs::UserState) -> Self {
        // OBS connection will be held in this arc mutex
        let connection = Arc::new(Mutex::new(None));

        // Will be used to receive events from OBS
        let (event_tx, event_rx) = mpsc::channel(100);

        let connection_inner = connection.clone();
        let state_inner = state.clone();
        let connection_join = tokio::spawn(async move {
            let connection = InnerConnection {
                connection_info,
                state: state_inner,
                connection: connection_inner,
                event_sender: event_tx,
            };

            // TODO: Any errors to handle?
            connection.run().await;
        });

        let event_join = tokio::spawn(Self::event_handler(event_rx, state));

        Self {
            connection,
            connection_join,
            event_join,
        }
    }

    async fn event_handler(
        mut events: mpsc::Receiver<obws::events::Event>,
        state: noalbs::UserState,
    ) {
        while let Some(event) = events.recv().await {
            match event.ty {
                EventType::SwitchScenes {
                    scene_name,
                    sources: _,
                } => {
                    let mut l = state.write().await;

                    let switchable = &l.switcher_state.switchable_scenes;
                    if switchable.contains(&scene_name) {
                        l.broadcasting_software
                            .switch_scene_notifier()
                            .notify_waiters();
                    }

                    l.broadcasting_software.current_scene = scene_name;
                }
                EventType::StreamStarted => {
                    let mut l = state.write().await;
                    l.broadcasting_software.is_streaming = true;
                    l.broadcasting_software.last_stream_started_at = std::time::Instant::now();

                    l.broadcasting_software
                        .start_streaming_notifier()
                        .notify_waiters();
                }
                EventType::StreamStopped => {
                    let mut l = state.write().await;
                    l.broadcasting_software.is_streaming = false;
                }
                _ => continue,
            }
        }
    }

    async fn get_scenes(&self) -> Result<Vec<String>, error::Error> {
        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        let scenes = client.scenes().get_scene_list().await?;

        let mut all_scenes = Vec::new();

        for scene in scenes.scenes {
            all_scenes.push(scene.name);
        }

        Ok(all_scenes)
    }
}

#[async_trait]
impl BroadcastingSoftwareLogic for Obs {
    async fn switch_scene(&self, scene: &str) -> Result<String, error::Error> {
        let scenes = self.get_scenes().await?;
        let scene = scene.to_lowercase();

        let res = scenes
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let s = &s.to_lowercase();
                (i, strsim::normalized_damerau_levenshtein(&scene, s))
            })
            .min_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

        let scene = if let Some(s) = res {
            scenes[s.0].to_owned()
        } else {
            scene
        };

        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        client.scenes().set_current_scene(&scene).await?;
        Ok(scene)
    }

    async fn start_streaming(&self) -> Result<(), error::Error> {
        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        Ok(client.streaming().start_streaming(None).await?)
    }

    async fn stop_streaming(&self) -> Result<(), error::Error> {
        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        Ok(client.streaming().stop_streaming().await?)
    }

    async fn fix(&self) -> Result<(), error::Error> {
        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        use obws::responses::MediaState;
        let media_sources = client.sources().get_media_sources_list().await?;
        let media_playing = media_sources.iter().filter(|m| {
            matches!(
                m.media_state,
                MediaState::Playing | MediaState::Buffering | MediaState::Opening
            )
        });

        for media in media_playing {
            let media_inputs = match media.source_kind.as_ref() {
                "ffmpeg_source" => {
                    let source = client
                        .sources()
                        .get_source_settings::<FfmpegSource>(
                            &media.source_name,
                            Some(&media.source_kind),
                        )
                        .await?;

                    if let Some(input) = source.source_settings.input {
                        Vec::from([input.to_lowercase()])
                    } else {
                        continue;
                    }
                }
                "vlc_source" => client
                    .sources()
                    .get_source_settings::<VlcSource>(&media.source_name, Some(&media.source_kind))
                    .await?
                    .source_settings
                    .playlist
                    .iter()
                    .map(|s| s.value.to_lowercase())
                    .collect::<Vec<String>>(),
                s => unimplemented!("Fix not implemented for {}", s),
            };

            if !media_inputs
                .iter()
                .any(|m| m.starts_with("rtmp") || m.starts_with("srt"))
            {
                continue;
            }

            client
                .media_control()
                .stop_media(&media.source_name)
                .await?;

            client
                .media_control()
                .play_pause_media(&media.source_name, Some(false))
                .await?;
        }

        Ok(())
    }

    async fn toggle_recording(&self) -> Result<(), error::Error> {
        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        Ok(client.recording().start_stop_recording().await?)
    }

    async fn is_recording(&self) -> Result<bool, error::Error> {
        let connection = self.connection.lock().await;

        let client = match &*connection {
            Some(client) => client,
            None => return Err(error::Error::UnableInitialConnection),
        };

        let status = client.recording().get_recording_status().await?;
        Ok(status.is_recording)
    }
}

/// The real connection to OBS, automatically keeps trying to connect.
pub struct InnerConnection {
    connection_info: config::ObsConfig,
    state: noalbs::UserState,
    connection: Arc<Mutex<Option<obws::Client>>>,
    event_sender: mpsc::Sender<obws::events::Event>,
}

impl InnerConnection {
    pub async fn run(&self) {
        loop {
            let client = self.get_client().await;
            let event_stream = client.events();

            {
                let state = &mut self.state.write().await;
                let bs = &mut state.broadcasting_software;

                let scenes = client.scenes().get_scene_list().await.unwrap();
                let streaming_status = client.streaming().get_streaming_status().await.unwrap();

                bs.current_scene = scenes.current_scene;
                bs.is_streaming = streaming_status.streaming;
                bs.status = ClientStatus::Connected;

                let bs = &state.broadcasting_software;
                bs.connected_notifier().notify_waiters();

                if bs.is_streaming {
                    bs.start_streaming_notifier().notify_waiters();
                }

                if state
                    .switcher_state
                    .switchable_scenes
                    .contains(&bs.current_scene)
                {
                    bs.switch_scene_notifier().notify_waiters();
                }
            }

            if let Err(e) = &event_stream {
                error!("Error getting event stream: {}", e);
            }

            {
                let mut connection = self.connection.lock().await;
                *connection = Some(client);
            }

            Self::event_loop(event_stream.unwrap(), self.event_sender.clone()).await;

            warn!("Disconnected");

            {
                let state = &mut self.state.write().await;
                let bs = &mut state.broadcasting_software;
                bs.status = ClientStatus::Disconnected;
                bs.is_streaming = false;
            }
        }
    }

    /// Attempts to connect to OBS
    ///
    /// Blocks until a successful connection has been established.
    /// An exponential backoff strategy is used to keep retrying to connect.
    /// This will grow until the 5th retry failure after which the max seconds
    /// will be reached of 32 seconds.
    async fn get_client(&self) -> obws::Client {
        let mut retry_grow = 1;

        loop {
            info!("Connecting");
            match obws::Client::connect(&self.connection_info.host, self.connection_info.port).await
            {
                Ok(client) => {
                    info!("Connected");

                    if let Err(e) = client.login(self.connection_info.password.as_ref()).await {
                        error!("Can't authenticate {}", e);
                        info!("trying to connect again in {} seconds", 10);
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        continue;
                    }

                    break client;
                }
                Err(e) => error!("Error while trying to connect to OBS: {}", e),
            };

            let wait = 1 << retry_grow;
            warn!("Unable to connect");
            info!("trying to connect again in {} seconds", wait);
            tokio::time::sleep(Duration::from_secs(wait)).await;

            if retry_grow < 5 {
                retry_grow += 1;
            }
        }
    }

    /// Sends all received events to the MPSC
    ///
    /// Blocks until the stream gets disconnected.
    /// This most likely happens when the websocket server shuts down.
    async fn event_loop(
        events: impl futures_util::Stream<Item = obws::events::Event>,
        event_sender: mpsc::Sender<obws::events::Event>,
    ) {
        futures_util::pin_mut!(events);

        while let Some(event) = events.next().await {
            let _ = event_sender.send(event).await;
        }
    }
}

impl Drop for Obs {
    // Abort the spawned tasks
    fn drop(&mut self) {
        self.connection_join.abort();
        self.event_join.abort();
    }
}

// From obws
/// Settings specific to a **FFmpeg** video source.
#[derive(Deserialize)]
struct FfmpegSource {
    /// URL of the remote media file. Only used if [`Self::is_local_file`] is set to `false`.
    pub input: Option<String>,
}

/// Settings specific to a **VLC** video source.
#[derive(Deserialize)]
struct VlcSource {
    /// List of files to play.
    pub playlist: Vec<SlideshowFile>,
}

/// Single file as part of a [`Slideshow`].
#[derive(Deserialize)]
struct SlideshowFile {
    /// Location of the file to display.
    pub value: String,
}
