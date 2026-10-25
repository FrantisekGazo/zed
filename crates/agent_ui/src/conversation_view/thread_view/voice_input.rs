use std::path::{Path, PathBuf};

use agent_settings::{SpeechToTextSettings, SpeechToTextTriggerMode};
use anyhow::Context as _;
use audio::{AudioDeviceInfo, AvailableAudioDevices};
use chrono::Local;
use futures::AsyncReadExt as _;
use gpui::{Anchor, DismissEvent, MouseButton, MouseDownEvent, MouseUpEvent};
use http_client::{AsyncBody, HttpClient, Method, Request};
use livekit_client::CaptureInput;
use picker::{Picker, PickerDelegate, popover_menu::PickerPopoverMenu};
use serde::Deserialize;
use ui::{HighlightedLabel, ListItem};
use workspace::{Toast, notifications::NotificationId};

use super::*;
use crate::{ToggleMicrophoneMenu, ToggleVoiceRecording};

#[derive(Default)]
pub(super) enum VoiceRecordingState {
    #[default]
    Idle,
    Recording {
        capture_input: CaptureInput,
    },
    SavingAudio,
    Transcribing,
}

impl VoiceRecordingState {
    fn is_recording(&self) -> bool {
        matches!(self, Self::Recording { .. })
    }

    fn is_saving(&self) -> bool {
        matches!(self, Self::SavingAudio)
    }

    fn is_transcribing(&self) -> bool {
        matches!(self, Self::Transcribing)
    }

    fn is_processing(&self) -> bool {
        matches!(self, Self::SavingAudio | Self::Transcribing)
    }
}

struct VoiceRecordingErrorToast;

#[derive(Clone)]
struct MicrophoneOption {
    device_id: Option<String>,
    name: SharedString,
}

pub(super) struct MicrophonePickerDelegate {
    thread_view: WeakEntity<ThreadView>,
    options: Vec<MicrophoneOption>,
    filtered_options: Vec<MicrophoneOption>,
    selected_microphone_id: Option<String>,
    selected_index: usize,
    query: String,
}

impl MicrophonePickerDelegate {
    fn new(
        thread_view: WeakEntity<ThreadView>,
        options: Vec<MicrophoneOption>,
        selected_microphone_id: Option<String>,
    ) -> Self {
        let mut this = Self {
            thread_view,
            options: Vec::new(),
            filtered_options: Vec::new(),
            selected_microphone_id: None,
            selected_index: 0,
            query: String::new(),
        };
        this.refresh_options(options, selected_microphone_id);
        this
    }

    fn refresh_options(
        &mut self,
        options: Vec<MicrophoneOption>,
        selected_microphone_id: Option<String>,
    ) {
        self.options = options;
        self.selected_microphone_id = selected_microphone_id;
        self.filtered_options = self.options_for_query(&self.query);
        self.selected_index = self
            .index_of_selected()
            .unwrap_or_else(|| self.first_selectable_index().unwrap_or(0));
    }

    fn options_for_query(&self, query: &str) -> Vec<MicrophoneOption> {
        let normalized_query = query.trim().to_ascii_lowercase();
        if normalized_query.is_empty() {
            return self.options.clone();
        }

        self.options
            .iter()
            .filter(|option| option.name.to_ascii_lowercase().contains(&normalized_query))
            .cloned()
            .collect()
    }

    fn first_selectable_index(&self) -> Option<usize> {
        (!self.filtered_options.is_empty()).then_some(0)
    }

    fn index_of_selected(&self) -> Option<usize> {
        self.filtered_options
            .iter()
            .position(|option| option.device_id == self.selected_microphone_id)
    }
}

impl PickerDelegate for MicrophonePickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "microphone picker"
    }

    fn match_count(&self) -> usize {
        self.filtered_options.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix.min(self.filtered_options.len().saturating_sub(1));
        cx.notify();
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Select microphone…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No microphones match your search.".into())
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        self.query = query;
        self.filtered_options = self.options_for_query(&self.query);
        self.selected_index = self
            .index_of_selected()
            .unwrap_or_else(|| self.first_selectable_index().unwrap_or(0));
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(option) = self.filtered_options.get(self.selected_index).cloned() else {
            return;
        };

        self.thread_view
            .update(cx, |thread_view, cx| {
                thread_view.selected_microphone_id = option.device_id.clone();
                cx.notify();
            })
            .ok();

        self.selected_microphone_id = option.device_id;
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.defer_in(window, |picker, window, cx| {
            picker.set_query("", window, cx);
        });
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let option = self.filtered_options.get(ix)?;
        let is_active = option.device_id == self.selected_microphone_id;
        let query = self.query.trim().to_ascii_lowercase();
        let highlight_positions = if query.is_empty() {
            Vec::new()
        } else {
            option
                .name
                .to_ascii_lowercase()
                .find(&query)
                .map(|start| (start..start + query.len()).collect())
                .unwrap_or_default()
        };

        Some(
            ListItem::new(("microphone-picker-item", ix))
                .inset(true)
                .spacing(ui::ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    option.name.clone(),
                    highlight_positions,
                ))
                .when(is_active, |this| {
                    this.end_slot(
                        div()
                            .pr_2()
                            .child(Icon::new(IconName::Check).color(Color::Accent)),
                    )
                })
                .into_any_element(),
        )
    }
}

fn resolve_selected_microphone(
    preferred_device: Option<&str>,
    available_devices: &[AudioDeviceInfo],
) -> Option<AudioDeviceInfo> {
    let Some(preferred_device) = preferred_device else {
        return None;
    };

    available_devices
        .iter()
        .find(|device| {
            device.desc.supports_input() && device.id.to_string().as_str() == preferred_device
        })
        .cloned()
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

fn multipart_body(boundary: &str, model: &str, wav_bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    body.extend_from_slice(model.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"recording.wav\"\r\n",
    );
    body.extend_from_slice(b"Content-Type: audio/wav\r\n\r\n");
    body.extend_from_slice(wav_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

fn build_transcription_request(
    settings: &SpeechToTextSettings,
    wav_bytes: Vec<u8>,
) -> anyhow::Result<Request<AsyncBody>> {
    let uri = format!(
        "{}/audio/transcriptions",
        settings.api_url.trim_end_matches('/')
    );
    let boundary = format!("----ZedVoiceInputBoundary{:x}", rand::random::<u64>());
    let body = multipart_body(&boundary, &settings.model, &wav_bytes);

    let mut request = Request::builder().method(Method::POST).uri(uri).header(
        "Content-Type",
        format!("multipart/form-data; boundary={boundary}"),
    );
    if let Some(api_key) = &settings.api_key {
        request = request.header("Authorization", format!("Bearer {}", api_key.trim()));
    }
    Ok(request.body(AsyncBody::from(body))?)
}

fn parse_transcription_response(body: &str) -> anyhow::Result<String> {
    let response: TranscriptionResponse = serde_json::from_str(body)
        .with_context(|| format!("invalid transcription response: {body}"))?;
    Ok(response.text.trim().to_string())
}

async fn remove_voice_recording(audio_path: &Path) {
    smol::fs::remove_file(audio_path)
        .await
        .with_context(|| {
            format!(
                "failed to remove temporary voice recording {}",
                audio_path.display()
            )
        })
        .log_err();
}

async fn transcribe_recording(
    http_client: Arc<dyn HttpClient>,
    settings: SpeechToTextSettings,
    audio_path: PathBuf,
) -> anyhow::Result<String> {
    let wav_bytes = smol::fs::read(&audio_path).await;
    remove_voice_recording(&audio_path).await;
    let wav_bytes = wav_bytes
        .with_context(|| format!("failed to read voice recording {}", audio_path.display()))?;

    let request = build_transcription_request(&settings, wav_bytes)?;
    let mut response = http_client.send(request).await.with_context(|| {
        format!(
            "Is the transcription server running at {}?",
            settings.api_url
        )
    })?;

    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .context("failed to read transcription response")?;

    if !response.status().is_success() {
        anyhow::bail!(
            "transcription server returned {}: {}",
            response.status(),
            body.trim()
        );
    }

    parse_transcription_response(&body)
}

fn transcript_insertion_text(
    transcript: &str,
    preceding_character: Option<char>,
) -> Option<String> {
    let transcript = transcript.trim();
    if transcript.is_empty() {
        return None;
    }

    let needs_leading_space =
        preceding_character.is_some_and(|character| !character.is_whitespace());
    Some(if needs_leading_space {
        format!(" {transcript}")
    } else {
        transcript.to_string()
    })
}

impl ThreadView {
    pub(super) fn toggle_voice_recording(
        &mut self,
        _: &ToggleVoiceRecording,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.flip_voice_recording(window, cx);
    }

    fn flip_voice_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match std::mem::replace(&mut self.voice_recording_state, VoiceRecordingState::Idle) {
            VoiceRecordingState::Idle => match self.start_voice_recording(cx) {
                Ok(()) => {}
                Err(error) => {
                    self.voice_recording_state = VoiceRecordingState::Idle;
                    self.show_voice_recording_error(
                        format!("Failed to start voice recording: {error:#}"),
                        cx,
                    );
                    cx.notify();
                }
            },
            VoiceRecordingState::Recording { capture_input } => {
                self.finish_voice_recording(capture_input, window, cx);
            }
            processing @ (VoiceRecordingState::SavingAudio | VoiceRecordingState::Transcribing) => {
                self.voice_recording_state = processing;
            }
        }
    }

    fn start_voice_recording_from_press(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.voice_recording_state, VoiceRecordingState::Idle) {
            return;
        }

        if let Err(error) = self.start_voice_recording(cx) {
            self.voice_recording_state = VoiceRecordingState::Idle;
            self.show_voice_recording_error(
                format!("Failed to start voice recording: {error:#}"),
                cx,
            );
            cx.notify();
        }
    }

    fn stop_voice_recording_from_release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let VoiceRecordingState::Recording { capture_input } =
            std::mem::replace(&mut self.voice_recording_state, VoiceRecordingState::Idle)
        else {
            return;
        };

        self.finish_voice_recording(capture_input, window, cx);
    }

    fn start_voice_recording(&mut self, cx: &mut Context<Self>) -> anyhow::Result<()> {
        if !AgentSettings::get_global(cx).speech_to_text.enabled {
            anyhow::bail!("Voice input is disabled in agent settings.");
        }

        audio::ensure_devices_initialized(cx);
        let available_devices = cx.default_global::<AvailableAudioDevices>().0.clone();
        let selected_microphone =
            resolve_selected_microphone(self.selected_microphone_id.as_deref(), &available_devices);

        let capture_input = CaptureInput::start(selected_microphone.map(|device| device.id))?;
        self.voice_recording_state = VoiceRecordingState::Recording { capture_input };
        cx.notify();
        Ok(())
    }

    fn finish_voice_recording(
        &mut self,
        capture_input: CaptureInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let save_path = Self::next_voice_recording_path();
        let speech_to_text_settings = AgentSettings::get_global(cx).speech_to_text.clone();
        let http_client = cx.http_client();

        self.voice_recording_state = VoiceRecordingState::SavingAudio;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let save_result = cx
                .background_spawn({
                    let save_path = save_path.clone();
                    async move { capture_input.finish_to_path(save_path) }
                })
                .await;

            let saved_path = match save_result {
                Ok(saved_path) => {
                    if let Err(error) = this.update_in(cx, |this, _window, cx| {
                        this.voice_recording_state = VoiceRecordingState::Transcribing;
                        cx.notify();
                    }) {
                        remove_voice_recording(&saved_path).await;
                        return Err(error);
                    }
                    saved_path
                }
                Err(error) => {
                    if save_path.exists() {
                        remove_voice_recording(&save_path).await;
                    }
                    this.update_in(cx, |this, _window, cx| {
                        this.voice_recording_state = VoiceRecordingState::Idle;
                        this.show_voice_recording_error(
                            format!("Failed to save voice recording: {error:#}"),
                            cx,
                        );
                        cx.notify();
                    })?;
                    return anyhow::Ok(());
                }
            };

            let transcribe_result =
                transcribe_recording(http_client, speech_to_text_settings, saved_path).await;

            this.update_in(cx, |this, window, cx| {
                this.voice_recording_state = VoiceRecordingState::Idle;

                match transcribe_result {
                    Ok(transcript) => {
                        this.insert_voice_transcript(&transcript, window, cx);
                    }
                    Err(error) => {
                        this.show_voice_recording_error(
                            format!("Failed to transcribe voice recording: {error:#}"),
                            cx,
                        );
                    }
                }

                cx.notify();
            })?;

            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn next_voice_recording_path() -> PathBuf {
        let timestamp = Local::now().format("%Y-%m-%dT%H-%M-%S-%3f");
        paths::temp_dir()
            .join("agent_voice")
            .join(format!("agent-voice-{timestamp}.wav"))
    }

    fn show_voice_recording_error(&self, message: String, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        workspace.update(cx, |workspace, cx| {
            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<VoiceRecordingErrorToast>(),
                    message,
                )
                .autohide(),
                cx,
            );
        });
    }

    fn insert_voice_transcript(
        &mut self,
        transcript: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let last_character = self.message_editor.read(cx).text(cx).chars().last();
        let Some(text) = transcript_insertion_text(transcript, last_character) else {
            return;
        };

        self.message_editor.update(cx, |message_editor, cx| {
            message_editor.insert_text(&text, window, cx);
        });
        self.message_editor.focus_handle(cx).focus(window, cx);
    }

    pub(super) fn toggle_microphone_menu(
        &mut self,
        _: &ToggleMicrophoneMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.voice_recording_state.is_processing() {
            return;
        }

        let menu_handle = self.microphone_menu_handle.clone();
        window.defer(cx, move |window, cx| {
            menu_handle.toggle(window, cx);
        });
    }

    fn ensure_microphone_picker(
        &mut self,
        window: &mut Window,
        options: Vec<MicrophoneOption>,
        cx: &mut Context<Self>,
    ) -> Entity<Picker<MicrophonePickerDelegate>> {
        let picker = match &self.microphone_picker {
            Some(picker) => picker.clone(),
            None => {
                let delegate = MicrophonePickerDelegate::new(
                    cx.weak_entity(),
                    options.clone(),
                    self.selected_microphone_id.clone(),
                );
                let picker = cx.new(|cx| {
                    Picker::list(delegate, window, cx)
                        .show_scrollbar(true)
                        .initial_width(rems(18.))
                        .max_height(rems(16.))
                });
                self.microphone_picker = Some(picker.clone());
                picker
            }
        };

        let selected_microphone_id = self.selected_microphone_id.clone();
        picker.update(cx, |picker, cx| {
            picker
                .delegate
                .refresh_options(options, selected_microphone_id);
            cx.notify();
        });
        picker
    }

    pub(super) fn render_microphone_control(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !AgentSettings::get_global(cx).speech_to_text.enabled {
            return None;
        }

        audio::ensure_devices_initialized(cx);
        let available_devices = cx.default_global::<AvailableAudioDevices>().0.clone();
        let mut microphone_options = vec![MicrophoneOption {
            device_id: None,
            name: "System Default".into(),
        }];
        microphone_options.extend(
            available_devices
                .iter()
                .filter(|device| device.desc.supports_input())
                .map(|device| MicrophoneOption {
                    device_id: Some(device.id.to_string()),
                    name: SharedString::from(device.desc.name().to_string()),
                }),
        );
        let microphone_picker = self.ensure_microphone_picker(window, microphone_options, cx);

        let focus_handle = self.message_editor.focus_handle(cx);
        let trigger_mode = AgentSettings::get_global(cx).speech_to_text.trigger_mode;
        let is_recording = self.voice_recording_state.is_recording();
        let is_saving = self.voice_recording_state.is_saving();
        let is_transcribing = self.voice_recording_state.is_transcribing();
        let is_processing = self.voice_recording_state.is_processing();
        let microphone_control_disabled = is_processing;
        let microphone_menu_open = self.microphone_menu_handle.is_deployed();
        let microphone_name =
            resolve_selected_microphone(self.selected_microphone_id.as_deref(), &available_devices)
                .map(|device| device.desc.name().to_string())
                .unwrap_or_else(|| {
                    if self.selected_microphone_id.is_some() {
                        "Configured Input Device".to_string()
                    } else {
                        "System Default".to_string()
                    }
                });
        let chevron_icon = if microphone_menu_open {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };

        let indicator = match &self.voice_recording_state {
            VoiceRecordingState::Idle => None,
            VoiceRecordingState::Recording { .. } => Some(
                h_flex()
                    .justify_center()
                    .items_center()
                    .w_full()
                    .gap_0p5()
                    .absolute()
                    .top(px(-8.0))
                    .left(px(0.0))
                    .child(
                        div()
                            .child(
                                Icon::new(IconName::Circle)
                                    .size(IconSize::XSmall)
                                    .color(Color::Accent),
                            )
                            .with_animation(
                                "voice-recording-indicator",
                                Animation::new(Duration::from_secs(1))
                                    .repeat()
                                    .with_easing(pulsating_between(0.25, 1.0)),
                                |indicator, delta| indicator.opacity(delta),
                            ),
                    )
                    .into_any_element(),
            ),
            VoiceRecordingState::SavingAudio => Some(
                h_flex()
                    .justify_center()
                    .items_center()
                    .w_full()
                    .gap_0p5()
                    .absolute()
                    .top(px(-8.0))
                    .left(px(0.0))
                    .child(
                        Icon::new(IconName::Circle)
                            .size(IconSize::XSmall)
                            .color(Color::Accent),
                    )
                    .child(loading_contents_spinner(IconSize::XSmall))
                    .into_any_element(),
            ),
            VoiceRecordingState::Transcribing => Some(
                h_flex()
                    .justify_center()
                    .items_center()
                    .w_full()
                    .gap_0p5()
                    .absolute()
                    .top(px(-8.0))
                    .left(px(0.0))
                    .child(loading_contents_spinner(IconSize::XSmall))
                    .into_any_element(),
            ),
        };

        let voice_recording_button = IconButton::new(
            "toggle-voice-recording",
            if is_recording {
                IconName::Mic
            } else {
                IconName::MicMute
            },
        )
        .icon_size(IconSize::Small)
        .icon_color(if is_recording {
            Color::Accent
        } else {
            Color::Muted
        })
        .disabled(microphone_control_disabled)
        .toggle_state(is_recording)
        .selected_style(ButtonStyle::Tinted(TintColor::Accent))
        .tooltip(move |window, cx| {
            if is_saving {
                Tooltip::text("Saving recording")(window, cx)
            } else if is_transcribing {
                Tooltip::text("Transcribing recording")(window, cx)
            } else {
                let label = match trigger_mode {
                    SpeechToTextTriggerMode::Toggle => {
                        if is_recording {
                            "Stop Voice Input"
                        } else {
                            "Start Voice Input"
                        }
                    }
                    SpeechToTextTriggerMode::Hold => "Hold to Record Dictation",
                };

                Tooltip::for_action_in(label, &ToggleVoiceRecording, &focus_handle, cx)
            }
        });
        let voice_recording_button = match trigger_mode {
            SpeechToTextTriggerMode::Toggle => voice_recording_button
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.flip_voice_recording(window, cx);
                }))
                .into_any_element(),
            SpeechToTextTriggerMode::Hold => div()
                .child(voice_recording_button)
                .when(!microphone_control_disabled, |this| {
                    this.on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseDownEvent, _window, cx| {
                            this.start_voice_recording_from_press(cx);
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, window, cx| {
                            this.stop_voice_recording_from_release(window, cx);
                        }),
                    )
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, window, cx| {
                            this.stop_voice_recording_from_release(window, cx);
                        }),
                    )
                })
                .into_any_element(),
        };

        let microphone_menu = PickerPopoverMenu::new(
            microphone_picker,
            IconButton::new("select-microphone", chevron_icon)
                .icon_size(IconSize::XSmall)
                .icon_color(Color::Muted)
                .disabled(microphone_control_disabled),
            move |window, cx| {
                if is_saving {
                    Tooltip::text("Saving recording")(window, cx)
                } else if is_transcribing {
                    Tooltip::text("Transcribing recording")(window, cx)
                } else {
                    Tooltip::with_meta("Select Microphone", None, microphone_name.clone(), cx)
                }
            },
            Anchor::BottomLeft,
            cx,
        )
        .with_handle(self.microphone_menu_handle.clone())
        .offset(gpui::Point {
            x: px(0.0),
            y: px(-2.0),
        })
        .render(window, cx);

        Some(
            div()
                .relative()
                .child(
                    h_flex()
                        .items_center()
                        .gap_px()
                        .child(voice_recording_button)
                        .child(microphone_menu),
                )
                .when_some(indicator, |this, indicator| this.child(indicator))
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_settings::{DEFAULT_SPEECH_TO_TEXT_API_URL, SpeechToTextSettings};
    use gpui::TestAppContext;
    use http_client::{FakeHttpClient, Method, Response};

    fn test_settings(api_url: &str, api_key: Option<&str>) -> SpeechToTextSettings {
        SpeechToTextSettings {
            api_url: api_url.to_string(),
            api_key: api_key.map(ToString::to_string),
            ..SpeechToTextSettings::default()
        }
    }

    fn write_recording(directory: &tempfile::TempDir) -> PathBuf {
        let audio_path = directory.path().join("recording.wav");
        std::fs::write(&audio_path, b"RIFF").unwrap();
        audio_path
    }

    #[test]
    fn test_transcription_request_shape() {
        for api_url in ["http://localhost:8010/v1", "http://localhost:8010/v1/"] {
            let request =
                build_transcription_request(&test_settings(api_url, None), b"RIFF".to_vec())
                    .unwrap();
            assert_eq!(
                request.uri(),
                "http://localhost:8010/v1/audio/transcriptions"
            );
            assert_eq!(request.method(), Method::POST);
            assert!(
                request.headers()["Content-Type"]
                    .to_str()
                    .unwrap()
                    .starts_with("multipart/form-data; boundary=")
            );
            assert!(request.headers().get("Authorization").is_none());
        }

        let request = build_transcription_request(
            &test_settings(DEFAULT_SPEECH_TO_TEXT_API_URL, Some("secret")),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(request.headers()["Authorization"], "Bearer secret");
    }

    #[test]
    fn test_multipart_body() {
        let body = multipart_body("BOUNDARY", "phonon-2.5-en", b"RIFFDATA");
        let body = String::from_utf8(body).unwrap();
        assert!(body.starts_with("--BOUNDARY\r\n"));
        assert!(body.contains("name=\"model\"\r\n\r\nphonon-2.5-en\r\n"));
        assert!(body.contains("name=\"file\"; filename=\"recording.wav\""));
        assert!(body.contains("Content-Type: audio/wav\r\n\r\nRIFFDATA\r\n"));
        assert!(body.ends_with("--BOUNDARY--\r\n"));
        assert!(body.find("name=\"model\"").unwrap() < body.find("name=\"file\"").unwrap());
    }

    #[test]
    fn test_parse_transcription_response() {
        assert_eq!(
            parse_transcription_response(r#"{"text":"  hello world \n"}"#).unwrap(),
            "hello world"
        );
        assert!(parse_transcription_response("not json").is_err());
    }

    #[test]
    fn test_transcript_insertion_text() {
        assert_eq!(transcript_insertion_text("", Some('a')), None);
        assert_eq!(transcript_insertion_text("  \n", None), None);
        assert_eq!(
            transcript_insertion_text("hi there", None),
            Some("hi there".into())
        );
        assert_eq!(
            transcript_insertion_text("hi there", Some(' ')),
            Some("hi there".into())
        );
        assert_eq!(
            transcript_insertion_text(" hi ", Some('.')),
            Some(" hi".into())
        );
    }

    #[gpui::test]
    async fn test_transcribe_reports_http_error(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let audio_path = write_recording(&directory);
        let http_client = FakeHttpClient::create(|_| async {
            Ok(Response::builder().status(401).body("bad key".into())?)
        });

        let error = transcribe_recording(
            http_client,
            test_settings(DEFAULT_SPEECH_TO_TEXT_API_URL, None),
            audio_path.clone(),
        )
        .await
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("401"), "{message}");
        assert!(message.contains("bad key"), "{message}");
        assert!(!audio_path.exists());
    }

    #[gpui::test]
    async fn test_transcribe_connection_error_names_server(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let audio_path = write_recording(&directory);
        let http_client =
            FakeHttpClient::create(|_| async { Err(anyhow::anyhow!("connection refused")) });

        let error = transcribe_recording(
            http_client,
            test_settings(DEFAULT_SPEECH_TO_TEXT_API_URL, None),
            audio_path.clone(),
        )
        .await
        .unwrap_err();

        let message = format!("{error:#}");
        assert!(message.contains("http://localhost:8010/v1"), "{message}");
        assert!(
            message.contains("Is the transcription server running"),
            "{message}"
        );
        assert!(!audio_path.exists());
    }

    #[gpui::test]
    async fn test_remove_voice_recording(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let audio_path = write_recording(&directory);

        remove_voice_recording(&audio_path).await;
        assert!(!audio_path.exists());

        remove_voice_recording(&audio_path).await;
        assert!(!audio_path.exists());
    }

    #[gpui::test]
    async fn test_transcribe_success(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().unwrap();
        let audio_path = write_recording(&directory);
        let http_client = FakeHttpClient::create(|_| async {
            Ok(Response::builder()
                .status(200)
                .body(r#"{"text":"hello"}"#.into())?)
        });

        let transcript = transcribe_recording(
            http_client,
            test_settings(DEFAULT_SPEECH_TO_TEXT_API_URL, None),
            audio_path.clone(),
        )
        .await
        .unwrap();

        assert_eq!(transcript, "hello");
        assert!(!audio_path.exists());
    }
}
