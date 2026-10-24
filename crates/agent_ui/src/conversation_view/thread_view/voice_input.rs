use std::path::PathBuf;

use agent_settings::SpeechToTextTriggerMode;
use audio::{AudioDeviceInfo, AvailableAudioDevices};
use chrono::Local;
use gpui::{Anchor, DismissEvent, MouseButton, MouseDownEvent, MouseUpEvent};
use livekit_client::CaptureInput;
use picker::{Picker, PickerDelegate, popover_menu::PickerPopoverMenu};
use ui::{HighlightedLabel, ListItem};
use util::paths::PathExt;
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

struct WhisperCppConfiguration {
    executable_path: PathBuf,
    model_path: PathBuf,
}

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
            VoiceRecordingState::SavingAudio | VoiceRecordingState::Transcribing => {
                self.voice_recording_state = VoiceRecordingState::Transcribing;
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

        self.resolve_whisper_cpp_configuration(cx)?;
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
        let whisper_configuration = match self.resolve_whisper_cpp_configuration(cx) {
            Ok(configuration) => configuration,
            Err(error) => {
                self.voice_recording_state = VoiceRecordingState::Idle;
                self.show_voice_recording_error(error.to_string(), cx);
                cx.notify();
                return;
            }
        };

        self.voice_recording_state = VoiceRecordingState::SavingAudio;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let save_result = cx
                .background_spawn(async move { capture_input.finish_to_path(save_path) })
                .await;

            let saved_path = match save_result {
                Ok(saved_path) => {
                    this.update_in(cx, |this, _window, cx| {
                        this.voice_recording_state = VoiceRecordingState::Transcribing;
                        cx.notify();
                    })?;
                    saved_path
                }
                Err(error) => {
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

            let transcribe_result = cx
                .background_spawn(async move {
                    Self::transcribe_voice_recording(saved_path, whisper_configuration)
                })
                .await;

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

    fn resolve_whisper_cpp_configuration(
        &self,
        cx: &App,
    ) -> anyhow::Result<WhisperCppConfiguration> {
        let settings = AgentSettings::get_global(cx);
        let executable_path = settings
            .speech_to_text
            .whisper_cpp_executable_path
            .clone()
            .or_else(|| which::which("whisper-cli").ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Voice input requires a whisper.cpp executable path or whisper-cli on PATH."
                )
            })?;
        let model_path = settings
            .speech_to_text
            .whisper_cpp_model_path
            .clone()
            .ok_or_else(|| {
                anyhow::anyhow!("Voice input requires a configured whisper.cpp model path.")
            })?;

        if !executable_path.is_file() {
            anyhow::bail!(
                "whisper.cpp executable not found at {}",
                executable_path.compact().display()
            );
        }

        if !model_path.is_file() {
            anyhow::bail!(
                "whisper.cpp model not found at {}",
                model_path.compact().display()
            );
        }

        Ok(WhisperCppConfiguration {
            executable_path,
            model_path,
        })
    }

    fn transcribe_voice_recording(
        audio_path: PathBuf,
        whisper_configuration: WhisperCppConfiguration,
    ) -> anyhow::Result<String> {
        let mut command = util::command::new_std_command(&whisper_configuration.executable_path);
        command
            .arg("-m")
            .arg(&whisper_configuration.model_path)
            .arg("-f")
            .arg(&audio_path);

        // Runs on a background executor, so blocking here does not stall the UI.
        #[allow(clippy::disallowed_methods)]
        let output = match command.output() {
            Ok(output) => output,
            Err(error) => {
                if let Err(remove_error) = std::fs::remove_file(&audio_path) {
                    log::warn!(
                        "failed to remove temporary voice recording {}: {remove_error:#}",
                        audio_path.display()
                    );
                }

                return Err(anyhow::anyhow!(
                    "failed to run whisper.cpp at {}: {}",
                    whisper_configuration.executable_path.compact().display(),
                    error
                ));
            }
        };

        if let Err(error) = std::fs::remove_file(&audio_path) {
            log::warn!(
                "failed to remove temporary voice recording {}: {error:#}",
                audio_path.display()
            );
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let details = if !stderr.is_empty() { stderr } else { stdout };
            anyhow::bail!(
                "whisper.cpp exited with status {}{}",
                output.status,
                if details.is_empty() {
                    String::new()
                } else {
                    format!(": {details}")
                }
            );
        }

        Self::extract_whisper_transcript(&output.stdout)
    }

    fn extract_whisper_transcript(stdout: &[u8]) -> anyhow::Result<String> {
        let stdout = String::from_utf8(stdout.to_vec())
            .map_err(|error| anyhow::anyhow!("whisper.cpp returned invalid UTF-8: {error}"))?;
        let transcript = stdout
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() {
                    return None;
                }

                if let Some((_, remainder)) = line.split_once(']')
                    && line.starts_with('[')
                {
                    let remainder = remainder.trim();
                    return (!remainder.is_empty()).then_some(remainder.to_string());
                }

                None
            })
            .collect::<Vec<_>>()
            .join(" ");

        Ok(transcript.trim().to_string())
    }

    fn insert_voice_transcript(
        &mut self,
        transcript: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let transcript = transcript.trim();
        if transcript.is_empty() || transcript == "[BLANK_AUDIO]" {
            return;
        }

        let needs_leading_space = self
            .message_editor
            .read(cx)
            .text(cx)
            .chars()
            .last()
            .is_some_and(|character| !character.is_whitespace());
        let text = if needs_leading_space {
            format!(" {transcript}")
        } else {
            transcript.to_string()
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
        if self.voice_recording_state.is_processing()
            || self.resolve_whisper_cpp_configuration(cx).is_err()
        {
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
        let whisper_configuration_error = self
            .resolve_whisper_cpp_configuration(cx)
            .err()
            .map(|error| error.to_string());
        let button_whisper_configuration_error = whisper_configuration_error.clone();
        let menu_whisper_configuration_error = whisper_configuration_error.clone();
        let microphone_control_disabled = is_processing || whisper_configuration_error.is_some();
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
            if let Some(error) = button_whisper_configuration_error.clone() {
                Tooltip::text(error)(window, cx)
            } else if is_saving {
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
                if let Some(error) = menu_whisper_configuration_error.clone() {
                    Tooltip::text(error)(window, cx)
                } else if is_saving {
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
