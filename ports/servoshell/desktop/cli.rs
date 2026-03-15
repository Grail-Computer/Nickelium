/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::path::Path;
use std::{env, panic};

use crate::desktop::app::App;
use crate::desktop::event_loop::ServoShellEventLoop;
use crate::panic_hook;
use crate::prefs::{ArgumentParsingResult, parse_command_line_arguments};

pub fn main() {
    crate::crash_handler::install();
    crate::init_crypto();
    crate::resources::init();

    // TODO: once log-panics is released, can this be replaced by
    // log_panics::init()?
    panic::set_hook(Box::new(panic_hook::panic_hook));

    // Skip the first argument, which is the binary name.
    let mut args = injected_args_for_current_binary();
    args.extend(env::args().skip(1));
    let (opts, preferences, servoshell_preferences) = match parse_command_line_arguments(&*args) {
        ArgumentParsingResult::ContentProcess(token) => return servo::run_content_process(token),
        ArgumentParsingResult::ChromeProcess(opts, preferences, servoshell_preferences) => {
            (opts, preferences, servoshell_preferences)
        },
        ArgumentParsingResult::Exit => {
            std::process::exit(0);
        },
        ArgumentParsingResult::ErrorParsing => {
            std::process::exit(1);
        },
    };

    crate::init_tracing(servoshell_preferences.tracing_filter.as_deref());

    let clean_shutdown = servoshell_preferences.clean_shutdown;
    let event_loop = match servoshell_preferences.headless {
        true => ServoShellEventLoop::headless(),
        false => ServoShellEventLoop::headed(),
    };

    {
        let mut app = App::new(opts, preferences, servoshell_preferences, &event_loop);
        event_loop.run_app(&mut app);
    }

    crate::platform::deinit(clean_shutdown)
}

fn injected_args_for_current_binary() -> Vec<String> {
    let executable_name = env::args_os()
        .next()
        .and_then(|path| Path::new(&path).file_stem().map(|stem| stem.to_owned()))
        .and_then(|stem| stem.to_str().map(str::to_owned));

    if matches!(
        executable_name.as_deref(),
        Some("servo-agent") | Some("nickelium")
    ) {
        return vec![
            "--agent-profile".into(),
            "workflow".into(),
            "--headless".into(),
        ];
    }

    Vec::new()
}
