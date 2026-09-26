//! Agent Viewer: a GNOME viewer for Microsoft Agent character files.

mod audio;
mod player;
mod speech;
mod stage;
mod window;

use std::path::PathBuf;

use adw::prelude::*;
use gtk::glib;

const APP_ID: &str = "org.gnome.AgentViewer";

const USAGE: &str = "\
Agent Viewer — play Microsoft Agent characters

Usage:
  agentview [OPTIONS] [FILE.acs|FILE.act]

Options:
  -a, --animation NAME   Play this animation once the character loads
  -s, --say TEXT         Speak this text once the character loads
      --tts ENGINE       Speech engine: espeak-ng (default) or kokoro
      --kokoro-voice ID  Kokoro voice ID (defaults based on character gender)
  -h, --help             Show this help
";

/// What to do automatically once a character finishes loading.
#[derive(Clone, Default)]
pub struct Startup {
    pub animation: Option<String>,
    pub say: Option<String>,
    pub speech_engine: speech::Engine,
    pub kokoro_voice: Option<String>,
}

fn main() -> glib::ExitCode {
    let mut startup = Startup::default();
    let mut file: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                return glib::ExitCode::SUCCESS;
            }
            "-a" | "--animation" => startup.animation = args.next(),
            "-s" | "--say" => startup.say = args.next(),
            "--tts" => match args.next().as_deref().and_then(speech::Engine::parse) {
                Some(engine) => startup.speech_engine = engine,
                None => {
                    eprintln!("--tts must be either espeak-ng or kokoro");
                    return glib::ExitCode::FAILURE;
                }
            },
            "--kokoro-voice" => startup.kokoro_voice = args.next(),
            other => file = Some(PathBuf::from(other)),
        }
    }

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        window::build(app, file.clone(), startup.clone()).present();
    });

    // Arguments are parsed above, so hand the toolkit an empty list.
    app.run_with_args::<&str>(&[])
}
