//! Main application window.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::{gdk::RGBA, pango};

use acs::{Character, CharacterInfo, MouthShape, RgbaImage};

use crate::audio::AudioPlayer;
use crate::player::Player;
use crate::speech::{self, Speech};
use crate::stage::{Backdrop, Balloon, Stage};

/// How long the balloon lingers after the audio finishes.
const BALLOON_LINGER_MS: u64 = 900;

struct ActiveSpeech {
    speech: Speech,
    started_us: i64,
    /// Animation to return to once speaking ends.
    restore_animation: Option<usize>,
}

#[derive(Default)]
struct State {
    path: Option<PathBuf>,
    player: Option<Player>,
    speech: Option<ActiveSpeech>,
    last_frame_us: i64,
    last_mouth: Option<MouthShape>,
    last_visible_chars: usize,
    sound_enabled: bool,
    /// The balloon currently on screen, kept so paced reveal can update just
    /// its visible length.
    balloon: Option<Balloon>,
}

struct Ui {
    window: adw::ApplicationWindow,
    stage: Stage,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    play_button: gtk::Button,
    loop_button: gtk::ToggleButton,
    sound_button: gtk::ToggleButton,
    frame_label: gtk::Label,
    say_entry: gtk::Entry,
    speak_button: gtk::Button,
    split: adw::OverlaySplitView,
    placeholder: gtk::Label,
}

pub fn build(app: &adw::Application, initial: Option<PathBuf>) -> adw::ApplicationWindow {
    let state = Rc::new(RefCell::new(State { sound_enabled: true, ..State::default() }));
    let audio = AudioPlayer::new();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .default_width(940)
        .default_height(660)
        .title("Agent Viewer")
        .build();

    let title = adw::WindowTitle::new("Agent Viewer", "No character loaded");
    let header = adw::HeaderBar::builder().title_widget(&title).build();

    let open_button = gtk::Button::builder()
        .icon_name("document-open-symbolic")
        .tooltip_text("Open a character (Ctrl+O)")
        .build();
    header.pack_start(&open_button);

    let info_button = gtk::Button::builder()
        .icon_name("dialog-information-symbolic")
        .tooltip_text("Character details (Ctrl+I)")
        .sensitive(false)
        .build();
    header.pack_end(&info_button);

    let menu = gio::Menu::new();
    menu.append(Some("Open Character…"), Some("win.open"));
    menu.append(Some("Character Details"), Some("win.info"));
    menu.append(Some("About Agent Viewer"), Some("win.about"));
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .tooltip_text("Main menu")
        .build();
    header.pack_end(&menu_button);

    // --- Sidebar: animation list --------------------------------------------

    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search animations")
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();

    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();

    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.append(&search);
    sidebar.append(&list_scroll);

    let sidebar_page = adw::ToolbarView::builder().content(&sidebar).build();
    sidebar_page.add_top_bar(&adw::HeaderBar::builder()
        .title_widget(&adw::WindowTitle::new("Animations", ""))
        .show_end_title_buttons(false)
        .build());

    // --- Content: stage and controls ----------------------------------------

    let stage = Stage::new();
    stage.set_hexpand(true);
    stage.set_vexpand(true);

    let placeholder = gtk::Label::builder()
        .label("Open a Microsoft Agent character (.acs) to begin")
        .css_classes(["dim-label", "title-4"])
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();

    let stage_stack = gtk::Overlay::builder().child(&stage).build();
    stage_stack.add_overlay(&placeholder);

    let play_button = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Play or pause (Space)")
        .sensitive(false)
        .build();
    let stop_button = gtk::Button::builder()
        .icon_name("media-playback-stop-symbolic")
        .tooltip_text("Stop and rewind")
        .sensitive(false)
        .build();
    let loop_button = gtk::ToggleButton::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .tooltip_text("Loop the animation")
        .active(true)
        .build();
    let sound_button = gtk::ToggleButton::builder()
        .icon_name("audio-volume-high-symbolic")
        .tooltip_text("Play the character's sound effects")
        .active(true)
        .build();

    let frame_label = gtk::Label::builder()
        .label("")
        .css_classes(["dim-label", "numeric"])
        .hexpand(true)
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .build();

    let zoom_out = gtk::Button::builder()
        .icon_name("zoom-out-symbolic")
        .tooltip_text("Zoom out")
        .build();
    let zoom_in = gtk::Button::builder()
        .icon_name("zoom-in-symbolic")
        .tooltip_text("Zoom in")
        .build();
    let zoom_reset = gtk::Button::builder()
        .icon_name("zoom-fit-best-symbolic")
        .tooltip_text("Reset zoom")
        .build();
    let backdrop_button = gtk::Button::builder()
        .icon_name("view-reveal-symbolic")
        .tooltip_text("Change the backdrop")
        .build();

    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    let playback_group = gtk::Box::builder().css_classes(["linked"]).build();
    playback_group.append(&play_button);
    playback_group.append(&stop_button);
    controls.append(&playback_group);
    controls.append(&loop_button);
    controls.append(&sound_button);
    controls.append(&frame_label);
    let zoom_group = gtk::Box::builder().css_classes(["linked"]).build();
    zoom_group.append(&zoom_out);
    zoom_group.append(&zoom_reset);
    zoom_group.append(&zoom_in);
    controls.append(&zoom_group);
    controls.append(&backdrop_button);

    let say_entry = gtk::Entry::builder()
        .placeholder_text("Say something…")
        .hexpand(true)
        .sensitive(false)
        .build();
    let speak_button = gtk::Button::builder()
        .label("Speak")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();
    let say_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .build();
    say_row.append(&say_entry);
    say_row.append(&speak_button);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&stage_stack);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&controls);
    content.append(&say_row);

    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar_page)
        .content(&content)
        .min_sidebar_width(200.0)
        .max_sidebar_width(280.0)
        .sidebar_width_fraction(0.26)
        .build();

    let sidebar_toggle = gtk::ToggleButton::builder()
        .icon_name("sidebar-show-symbolic")
        .tooltip_text("Toggle the animation list (F9)")
        .active(true)
        .build();
    sidebar_toggle
        .bind_property("active", &split, "show-sidebar")
        .bidirectional()
        .sync_create()
        .build();
    header.pack_start(&sidebar_toggle);

    let toolbar = adw::ToolbarView::builder().content(&split).build();
    toolbar.add_top_bar(&header);

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&toolbar));
    window.set_content(Some(&toasts));

    let ui = Rc::new(Ui {
        window: window.clone(),
        stage: stage.clone(),
        title,
        toasts,
        list: list.clone(),
        search: search.clone(),
        play_button: play_button.clone(),
        loop_button: loop_button.clone(),
        sound_button: sound_button.clone(),
        frame_label,
        say_entry: say_entry.clone(),
        speak_button: speak_button.clone(),
        split,
        placeholder,
    });

    wire(&ui, &state, &audio, &open_button, &info_button, &stop_button, &zoom_in, &zoom_out,
         &zoom_reset, &backdrop_button, app);

    if let Some(path) = initial {
        load(&ui, &state, &audio, &path);
    }

    window
}

#[allow(clippy::too_many_arguments)]
fn wire(
    ui: &Rc<Ui>,
    state: &Rc<RefCell<State>>,
    audio: &AudioPlayer,
    open_button: &gtk::Button,
    info_button: &gtk::Button,
    stop_button: &gtk::Button,
    zoom_in: &gtk::Button,
    zoom_out: &gtk::Button,
    zoom_reset: &gtk::Button,
    backdrop_button: &gtk::Button,
    app: &adw::Application,
) {
    // --- Open ---------------------------------------------------------------
    let open = {
        let ui = ui.clone();
        let state = state.clone();
        let audio = audio.clone();
        move || {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Microsoft Agent characters"));
            filter.add_pattern("*.acs");
            filter.add_pattern("*.ACS");
            let all = gtk::FileFilter::new();
            all.set_name(Some("All files"));
            all.add_pattern("*");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            filters.append(&all);

            let dialog = gtk::FileDialog::builder()
                .title("Open Character")
                .filters(&filters)
                .modal(true)
                .build();

            let ui = ui.clone();
            let state = state.clone();
            let audio = audio.clone();
            dialog.open(Some(&ui.window.clone()), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        load(&ui, &state, &audio, &path);
                    }
                }
            });
        }
    };

    {
        let open = open.clone();
        open_button.connect_clicked(move |_| open());
    }

    // --- Window actions and shortcuts ---------------------------------------
    let action_open = gio::SimpleAction::new("open", None);
    {
        let open = open.clone();
        action_open.connect_activate(move |_, _| open());
    }
    ui.window.add_action(&action_open);

    let action_info = gio::SimpleAction::new("info", None);
    {
        let ui = ui.clone();
        let state = state.clone();
        action_info.connect_activate(move |_, _| show_info(&ui, &state));
    }
    ui.window.add_action(&action_info);

    let action_about = gio::SimpleAction::new("about", None);
    {
        let ui = ui.clone();
        action_about.connect_activate(move |_, _| show_about(&ui));
    }
    ui.window.add_action(&action_about);

    let action_play = gio::SimpleAction::new("playpause", None);
    {
        let ui = ui.clone();
        let state = state.clone();
        action_play.connect_activate(move |_, _| toggle_play(&ui, &state));
    }
    ui.window.add_action(&action_play);

    let action_sidebar = gio::SimpleAction::new("sidebar", None);
    {
        let ui = ui.clone();
        action_sidebar.connect_activate(move |_, _| {
            ui.split.set_show_sidebar(!ui.split.shows_sidebar());
        });
    }
    ui.window.add_action(&action_sidebar);

    app.set_accels_for_action("win.open", &["<Control>o"]);
    app.set_accels_for_action("win.info", &["<Control>i"]);
    app.set_accels_for_action("win.playpause", &["space"]);
    app.set_accels_for_action("win.sidebar", &["F9"]);
    app.set_accels_for_action("window.close", &["<Control>w"]);

    {
        let ui = ui.clone();
        let state = state.clone();
        info_button.connect_clicked(move |_| show_info(&ui, &state));
    }

    // --- Drag and drop ------------------------------------------------------
    let drop_target = gtk::DropTarget::new(gio::File::static_type(), gdk::DragAction::COPY);
    {
        let ui = ui.clone();
        let state = state.clone();
        let audio = audio.clone();
        drop_target.connect_drop(move |_, value, _, _| {
            if let Ok(file) = value.get::<gio::File>() {
                if let Some(path) = file.path() {
                    load(&ui, &state, &audio, &path);
                    return true;
                }
            }
            false
        });
    }
    ui.window.add_controller(drop_target);

    // --- Animation list -----------------------------------------------------
    {
        let ui = ui.clone();
        let state = state.clone();
        let audio = audio.clone();
        ui.list.clone().connect_row_activated(move |_, row| {
            select_animation(&ui, &state, &audio, row.index() as usize);
        });
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        let audio = audio.clone();
        ui.list.clone().connect_row_selected(move |_, row| {
            if let Some(row) = row {
                select_animation(&ui, &state, &audio, row.index() as usize);
            }
        });
    }
    {
        // Rows are added in animation order and only ever hidden, so a row's
        // index stays equal to its animation index.
        let search = ui.search.clone();
        ui.list.clone().set_filter_func(move |row| {
            let needle = search.text().to_lowercase();
            if needle.is_empty() {
                return true;
            }
            row.child()
                .and_then(|c| c.first_child())
                .and_downcast::<gtk::Label>()
                .map(|l| l.label().to_lowercase().contains(&needle))
                .unwrap_or(true)
        });
    }
    {
        let list = ui.list.clone();
        ui.search.connect_search_changed(move |_| list.invalidate_filter());
    }

    // --- Playback controls --------------------------------------------------
    {
        let ui = ui.clone();
        let state = state.clone();
        ui.play_button.clone().connect_clicked(move |_| toggle_play(&ui, &state));
    }
    {
        let ui = ui.clone();
        let state = state.clone();
        stop_button.connect_clicked(move |_| {
            if let Some(player) = state.borrow_mut().player.as_mut() {
                player.stop();
            }
            refresh_frame(&ui, &state, true);
            update_controls(&ui, &state);
        });
    }
    {
        let state = state.clone();
        ui.loop_button.clone().connect_toggled(move |b| {
            if let Some(player) = state.borrow_mut().player.as_mut() {
                player.set_looping(b.is_active());
            }
        });
    }
    {
        let state = state.clone();
        let audio = audio.clone();
        ui.sound_button.clone().connect_toggled(move |b| {
            state.borrow_mut().sound_enabled = b.is_active();
            if !b.is_active() {
                audio.stop_all();
            }
        });
    }

    {
        let stage = ui.stage.clone();
        zoom_in.connect_clicked(move |_| {
            stage.set_fit(false);
            stage.set_zoom(stage.zoom() * 1.25);
        });
    }
    {
        let stage = ui.stage.clone();
        zoom_out.connect_clicked(move |_| {
            stage.set_fit(false);
            stage.set_zoom(stage.zoom() / 1.25);
        });
    }
    {
        let stage = ui.stage.clone();
        zoom_reset.connect_clicked(move |_| {
            stage.set_zoom(1.0);
            stage.set_fit(true);
        });
    }
    {
        let stage = ui.stage.clone();
        backdrop_button.connect_clicked(move |b| {
            let (next, icon) = match stage.backdrop() {
                Backdrop::Checker => (Backdrop::Dark, "weather-clear-night-symbolic"),
                Backdrop::Dark => (Backdrop::Light, "weather-clear-symbolic"),
                Backdrop::Light => (Backdrop::Checker, "view-reveal-symbolic"),
            };
            stage.set_backdrop(next);
            b.set_icon_name(icon);
        });
    }

    // --- Speech -------------------------------------------------------------
    let speak = {
        let ui = ui.clone();
        let state = state.clone();
        let audio = audio.clone();
        move || start_speaking(&ui, &state, &audio)
    };
    {
        let speak = speak.clone();
        ui.speak_button.clone().connect_clicked(move |_| speak());
    }
    {
        let speak = speak.clone();
        ui.say_entry.clone().connect_activate(move |_| speak());
    }

    // --- Frame clock --------------------------------------------------------
    {
        let ui = ui.clone();
        let state = state.clone();
        let audio = audio.clone();
        ui.stage.clone().add_tick_callback(move |_, clock| {
            tick(&ui, &state, &audio, clock.frame_time());
            glib::ControlFlow::Continue
        });
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

fn load(ui: &Rc<Ui>, state: &Rc<RefCell<State>>, audio: &AudioPlayer, path: &Path) {
    audio.stop_all();

    let character = match acs::load(path) {
        Ok(c) => c,
        Err(e) => {
            toast(ui, &format!("{}: {}", file_label(path), e));
            return;
        }
    };

    let primary = system_primary_language();
    let name = character.info.name_for(primary).unwrap_or("Unnamed character").to_string();
    let animation_count = character.animations.len();

    let character = Rc::new(character);
    let mut player = Player::new(Rc::clone(&character));
    player.set_looping(ui.loop_button.is_active());

    {
        let mut s = state.borrow_mut();
        s.path = Some(path.to_path_buf());
        s.speech = None;
        s.last_mouth = None;
        s.last_visible_chars = 0;
        s.last_frame_us = 0;
        s.player = Some(player);
    }

    ui.title.set_title(&name);
    ui.title.set_subtitle(&format!("{} · {} animations", file_label(path), animation_count));
    ui.window.set_title(Some(&format!("{} — Agent Viewer", name)));
    ui.placeholder.set_visible(false);
    ui.stage.set_balloon(None);
    ui.say_entry.set_sensitive(true);
    ui.speak_button.set_sensitive(true);

    populate_list(ui, &character);

    // Prefer a resting pose so the character opens in a natural state.
    let initial = pick_initial_animation(&character);
    if let Some(index) = initial {
        ui.list.select_row(ui.list.row_at_index(index as i32).as_ref());
    } else {
        toast(ui, "This character has no playable animations");
    }
    update_controls(ui, state);
}

fn populate_list(ui: &Rc<Ui>, character: &Character) {
    while let Some(child) = ui.list.first_child() {
        ui.list.remove(&child);
    }

    for animation in &character.animations {
        let name = gtk::Label::builder()
            .label(&animation.name)
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::End)
            .build();

        let frames = animation.frames.len();
        let detail = if frames == 0 {
            "unreadable".to_string()
        } else {
            format!("{} frames · {:.1}s", frames, animation.duration_ms() as f64 / 1000.0)
        };
        let subtitle = gtk::Label::builder()
            .label(&detail)
            .xalign(0.0)
            .css_classes(["dim-label", "caption"])
            .ellipsize(pango::EllipsizeMode::End)
            .build();

        let boxed = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        boxed.append(&name);
        boxed.append(&subtitle);

        let row = gtk::ListBoxRow::builder().child(&boxed).build();
        row.set_sensitive(frames > 0);
        ui.list.append(&row);
    }
}

/// Chooses the animation to show when a character is first opened.
fn pick_initial_animation(character: &Character) -> Option<usize> {
    let playable = |i: usize| character.animations.get(i).is_some_and(|a| !a.frames.is_empty());

    for preferred in ["RestPose", "Idle1_1", "Greet", "Show", "Showing"] {
        if let Some(i) = character
            .animations
            .iter()
            .position(|a| a.name.eq_ignore_ascii_case(preferred))
        {
            if playable(i) {
                return Some(i);
            }
        }
    }
    // Otherwise the first animation belonging to the SHOWING state, then any.
    for state in &character.info.states {
        if state.name.eq_ignore_ascii_case("showing") {
            for name in &state.animations {
                if let Some(i) =
                    character.animations.iter().position(|a| a.name.eq_ignore_ascii_case(name))
                {
                    if playable(i) {
                        return Some(i);
                    }
                }
            }
        }
    }
    (0..character.animations.len()).find(|&i| playable(i))
}

fn select_animation(
    ui: &Rc<Ui>,
    state: &Rc<RefCell<State>>,
    audio: &AudioPlayer,
    index: usize,
) {
    let sound = {
        let mut s = state.borrow_mut();
        if s.player.as_ref().and_then(|p| p.animation_index()) == Some(index) && s.speech.is_some()
        {
            return;
        }
        let enabled = s.sound_enabled;
        match s.player.as_mut() {
            Some(player) => player.select(index).filter(|_| enabled),
            None => None,
        }
    };

    play_sound(state, audio, sound);
    refresh_frame(ui, state, true);
    update_controls(ui, state);
}

fn toggle_play(ui: &Rc<Ui>, state: &Rc<RefCell<State>>) {
    {
        let mut s = state.borrow_mut();
        if let Some(player) = s.player.as_mut() {
            if player.is_playing() {
                player.pause();
            } else {
                player.play();
            }
        }
    }
    update_controls(ui, state);
}

// ---------------------------------------------------------------------------
// Frame clock
// ---------------------------------------------------------------------------

fn tick(ui: &Rc<Ui>, state: &Rc<RefCell<State>>, audio: &AudioPlayer, now_us: i64) {
    let mut needs_render = false;
    let mut sound = None;
    let mut finished = false;
    let mut speech_ended = false;
    let mut balloon_chars = None;

    {
        let mut s = state.borrow_mut();
        let last = s.last_frame_us;
        s.last_frame_us = now_us;
        if last == 0 {
            return;
        }
        let dt_us = (now_us - last).clamp(0, 250_000) as u64;

        if let Some(player) = s.player.as_mut() {
            let result = player.advance(dt_us);
            needs_render |= result.frame_changed;
            finished = result.finished;
            if s.sound_enabled {
                sound = result.sound;
            }
        }

        // Drive the mouth and the balloon from the active utterance. Values are
        // read out first so the borrow ends before the state is updated.
        let utterance = s.speech.as_ref().map(|active| {
            let elapsed_ms = ((now_us - active.started_us).max(0) / 1000) as u64;
            (
                elapsed_ms,
                active.speech.duration_ms,
                active.speech.mouth_at(elapsed_ms),
                active.speech.text.chars().count(),
                active.speech.progress(elapsed_ms),
            )
        });

        if let Some((elapsed_ms, duration_ms, mouth, total, progress)) = utterance {
            if elapsed_ms > duration_ms + BALLOON_LINGER_MS {
                speech_ended = true;
            } else {
                if s.last_mouth != Some(mouth) {
                    s.last_mouth = Some(mouth);
                    needs_render = true;
                }
                let shown = ((progress * total as f64).ceil() as usize).min(total);
                if shown != s.last_visible_chars {
                    s.last_visible_chars = shown;
                    balloon_chars = Some(shown);
                }
            }
        }
    }

    play_sound(state, audio, sound);

    if speech_ended {
        end_speech(ui, state);
        needs_render = true;
    }

    if let Some(chars) = balloon_chars {
        update_balloon_progress(ui, state, chars);
    }

    if needs_render {
        refresh_frame(ui, state, false);
    }
    if finished {
        update_controls(ui, state);
    }
}

fn play_sound(state: &Rc<RefCell<State>>, audio: &AudioPlayer, index: Option<usize>) {
    let Some(index) = index else { return };
    let data = {
        let s = state.borrow();
        s.player
            .as_ref()
            .and_then(|p| p.character().audio(index))
            .map(|d| d.to_vec())
    };
    if let Some(data) = data {
        audio.play(data);
    }
}

/// Recomposites the current frame and hands it to the stage as a texture.
fn refresh_frame(ui: &Rc<Ui>, state: &Rc<RefCell<State>>, update_label: bool) {
    let (image, label) = {
        let mut s = state.borrow_mut();
        let mouth = s.last_mouth.filter(|_| s.speech.is_some());
        let Some(player) = s.player.as_mut() else { return };
        let image = player.render(mouth);
        let label = if update_label {
            player.current_animation().map(|a| {
                format!("{} · {} frames", a.name, a.frames.len())
            })
        } else {
            None
        };
        (image, label)
    };

    if let Some(image) = image {
        ui.stage.set_texture(Some(texture_from(image)));
    }
    if let Some(label) = label {
        ui.frame_label.set_text(&label);
    }
}

fn texture_from(image: RgbaImage) -> gdk::Texture {
    let stride = image.stride();
    let (w, h) = (image.width as i32, image.height as i32);
    let bytes = glib::Bytes::from_owned(image.data);
    gdk::MemoryTexture::new(w, h, gdk::MemoryFormat::R8g8b8a8, &bytes, stride).upcast()
}

fn update_controls(ui: &Rc<Ui>, state: &Rc<RefCell<State>>) {
    let s = state.borrow();
    let playing = s.player.as_ref().is_some_and(|p| p.is_playing());
    let has_frames = s.player.as_ref().is_some_and(|p| p.frame_count() > 0);

    ui.play_button.set_icon_name(if playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    });
    ui.play_button.set_sensitive(has_frames);
}

// ---------------------------------------------------------------------------
// Speech
// ---------------------------------------------------------------------------

fn start_speaking(ui: &Rc<Ui>, state: &Rc<RefCell<State>>, audio: &AudioPlayer) {
    let text = ui.say_entry.text().to_string();
    if text.trim().is_empty() {
        return;
    }

    let voice = {
        let s = state.borrow();
        let Some(player) = s.player.as_ref() else { return };
        player.character().info.voice.clone()
    };

    ui.speak_button.set_sensitive(false);
    ui.speak_button.set_label("Speaking…");

    let ui = ui.clone();
    let state = state.clone();
    let audio = audio.clone();
    glib::spawn_future_local(async move {
        // espeak-ng runs to completion before playback, so keep it off the
        // main loop to avoid stalling the frame clock.
        let result = gio::spawn_blocking(move || speech::synthesize(&text, voice.as_ref())).await;

        ui.speak_button.set_sensitive(true);
        ui.speak_button.set_label("Speak");

        let speech = match result {
            Ok(Ok(speech)) => speech,
            Ok(Err(message)) => {
                toast(&ui, &message);
                return;
            }
            Err(_) => {
                toast(&ui, "Speech synthesis was interrupted");
                return;
            }
        };

        begin_speech(&ui, &state, &audio, speech);
    });
}

fn begin_speech(
    ui: &Rc<Ui>,
    state: &Rc<RefCell<State>>,
    audio: &AudioPlayer,
    speech: Speech,
) {
    let now_us = ui
        .stage
        .frame_clock()
        .map(|c| c.frame_time())
        .unwrap_or_else(|| glib::monotonic_time());

    let (balloon, switched_from) = {
        let mut s = state.borrow_mut();
        let Some(player) = s.player.as_mut() else { return };

        // Lip sync needs a frame carrying mouth overlays; switch to a speaking
        // animation when the current one has none, and restore it afterwards.
        let current = player.animation_index();
        let has_overlays = current.is_some_and(|i| animation_has_overlays(player.character(), i));
        let mut switched_from = None;
        if !has_overlays {
            if let Some(target) = speaking_animation(player.character()) {
                player.select(target);
                player.set_looping(true);
                switched_from = current;
            }
        }
        player.play();

        let balloon = balloon_for(&player.character().info, &speech.text);
        s.last_visible_chars = 0;
        s.last_mouth = None;
        s.balloon = Some(balloon.clone());
        (balloon, switched_from)
    };

    ui.stage.set_balloon(Some(balloon));

    audio.stop_all();
    audio.play(speech.wav.clone());

    state.borrow_mut().speech =
        Some(ActiveSpeech { speech, started_us: now_us, restore_animation: switched_from });

    update_controls(ui, state);
}

fn end_speech(ui: &Rc<Ui>, state: &Rc<RefCell<State>>) {
    let restore = {
        let mut s = state.borrow_mut();
        let restore = s.speech.take().and_then(|a| a.restore_animation);
        s.last_mouth = None;
        s.last_visible_chars = 0;
        s.balloon = None;
        restore
    };

    ui.stage.set_balloon(None);

    if let Some(index) = restore {
        let mut s = state.borrow_mut();
        if let Some(player) = s.player.as_mut() {
            player.select(index);
            player.set_looping(ui.loop_button.is_active());
        }
    }
}

fn update_balloon_progress(ui: &Rc<Ui>, state: &Rc<RefCell<State>>, visible_chars: usize) {
    // Only the revealed length changes; everything else stays as authored.
    let balloon = {
        let mut s = state.borrow_mut();
        let Some(balloon) = s.balloon.as_mut() else { return };
        balloon.visible_chars = visible_chars;
        balloon.clone()
    };
    ui.stage.set_balloon(Some(balloon));
}

fn animation_has_overlays(character: &Character, index: usize) -> bool {
    character
        .animations
        .get(index)
        .is_some_and(|a| a.frames.iter().any(|f| !f.overlays.is_empty()))
}

/// Finds an animation suitable for talking over: one that actually carries
/// mouth overlays, preferring whatever the character assigns to its SPEAKING
/// state.
fn speaking_animation(character: &Character) -> Option<usize> {
    let index_of = |name: &str| {
        character.animations.iter().position(|a| a.name.eq_ignore_ascii_case(name))
    };

    for state in &character.info.states {
        if state.name.eq_ignore_ascii_case("speaking") {
            for name in &state.animations {
                if let Some(i) = index_of(name) {
                    if animation_has_overlays(character, i) {
                        return Some(i);
                    }
                }
            }
        }
    }
    for preferred in ["RestPose", "Idle1_1", "Explain", "Announce"] {
        if let Some(i) = index_of(preferred) {
            if animation_has_overlays(character, i) {
                return Some(i);
            }
        }
    }
    (0..character.animations.len()).find(|&i| animation_has_overlays(character, i))
}

fn balloon_for(info: &CharacterInfo, text: &str) -> Balloon {
    let default_fg = RGBA::new(0.0, 0.0, 0.0, 1.0);
    let default_bg = RGBA::new(1.0, 1.0, 0.94, 1.0);
    let default_border = RGBA::new(0.25, 0.25, 0.25, 1.0);

    let to_rgba = |c: acs::Rgb| {
        RGBA::new(c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, 1.0)
    };

    Balloon {
        text: text.to_string(),
        visible_chars: 0,
        foreground: info.balloon.as_ref().map(|b| to_rgba(b.foreground)).unwrap_or(default_fg),
        background: info.balloon.as_ref().map(|b| to_rgba(b.background)).unwrap_or(default_bg),
        border: info.balloon.as_ref().map(|b| to_rgba(b.border)).unwrap_or(default_border),
        font_family: info
            .balloon
            .as_ref()
            .map(|b| b.font_name.clone())
            .filter(|f| !f.is_empty())
            .unwrap_or_default(),
        // Authored font heights are Windows logical units at 96 dpi.
        font_size_pt: info
            .balloon
            .as_ref()
            .map(|b| (b.font_height.unsigned_abs() as f64 * 72.0 / 96.0).clamp(9.0, 20.0))
            .unwrap_or(11.0),
        italic: info.balloon.as_ref().is_some_and(|b| b.italic),
        bold: info.balloon.as_ref().is_some_and(|b| b.font_weight >= 600),
        chars_per_line: info
            .balloon
            .as_ref()
            .map(|b| b.chars_per_line as usize)
            .filter(|&c| c > 0)
            .unwrap_or(32),
    }
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

fn show_info(ui: &Rc<Ui>, state: &Rc<RefCell<State>>) {
    let s = state.borrow();
    let Some(player) = s.player.as_ref() else {
        toast(ui, "No character is loaded");
        return;
    };
    let character = player.character();
    let info = &character.info;
    let primary = system_primary_language();

    let page = adw::PreferencesPage::new();

    let general = adw::PreferencesGroup::builder().title("Character").build();
    add_row(&general, "Name", info.name_for(primary).unwrap_or("—"));
    if let Some(description) = info.description_for(primary) {
        add_row(&general, "Description", description);
    }
    add_row(&general, "Size", &format!("{} × {} px", info.width, info.height));
    add_row(&general, "Format version", &format!("{}.{}", info.major_version, info.minor_version));
    add_row(&general, "Palette", &format!("{} colours", info.palette.len()));
    if let Some(path) = s.path.as_ref() {
        add_row(&general, "File", &path.display().to_string());
    }
    page.add(&general);

    let contents = adw::PreferencesGroup::builder().title("Contents").build();
    add_row(&contents, "Animations", &character.animations.len().to_string());
    add_row(&contents, "Images", &character.image_count().to_string());
    add_row(&contents, "Sounds", &character.audio_count().to_string());
    add_row(&contents, "States", &info.states.len().to_string());
    let locales = info.localized.len();
    if locales > 0 {
        add_row(&contents, "Localisations", &locales.to_string());
    }
    page.add(&contents);

    if let Some(voice) = &info.voice {
        let group = adw::PreferencesGroup::builder()
            .title("Voice")
            .description("Used as defaults when speaking through espeak-ng")
            .build();
        add_row(&group, "Speed", &format!("{} wpm", voice.speed));
        add_row(&group, "Pitch", &voice.pitch.to_string());
        if let Some(gender) = voice.gender {
            let label = match gender {
                1 => "Female",
                2 => "Male",
                _ => "Unspecified",
            };
            add_row(&group, "Gender", label);
        }
        page.add(&group);
    }

    if !info.states.is_empty() {
        let group = adw::PreferencesGroup::builder().title("States").build();
        let mut states: Vec<_> = info.states.iter().collect();
        states.sort_by(|a, b| a.name.cmp(&b.name));
        for st in states {
            add_row(&group, &st.name, &st.animations.join(", "));
        }
        page.add(&group);
    }

    let dialog = adw::PreferencesDialog::builder().title("Character Details").build();
    dialog.add(&page);
    dialog.present(Some(&ui.window));
}

fn add_row(group: &adw::PreferencesGroup, title: &str, value: &str) {
    let row = adw::ActionRow::builder().title(title).subtitle(value).build();
    row.set_subtitle_selectable(true);
    row.add_css_class("property");
    group.add(&row);
}

fn show_about(ui: &Rc<Ui>) {
    let about = adw::AboutDialog::builder()
        .application_name("Agent Viewer")
        .application_icon("face-smile-symbolic")
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Built with GTK 4 and libadwaita")
        .comments(
            "Opens Microsoft Agent character files, plays their animations and sounds, \
             and speaks text with espeak-ng using the character's own voice settings.\n\n\
             Frames are composited from the character's palette and drawn as GPU textures \
             through GSK.",
        )
        .license_type(gtk::License::MitX11)
        .build();
    about.present(Some(&ui.window));
}

fn toast(ui: &Rc<Ui>, message: &str) {
    ui.toasts.add_toast(adw::Toast::new(message));
}

fn file_label(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.display().to_string())
}

/// Windows primary language id matching the user's locale, for picking among
/// the localised names a character ships.
fn system_primary_language() -> Option<u16> {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            if !value.is_empty() {
                if let Some(id) = acs::types::primary_language_from_locale(&value) {
                    return Some(id);
                }
            }
        }
    }
    None
}
