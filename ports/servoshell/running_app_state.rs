/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Shared state and methods for desktop and EGL implementations.

use std::cell::{Cell, Ref, RefCell};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::Path;
use std::rc::Rc;

use crossbeam_channel::{Receiver, Sender, unbounded};
use euclid::{Point2D, Rect};
use image::{DynamicImage, ImageFormat, RgbaImage};
#[cfg(all(
    any(coverage, llvm_pgo),
    any(target_os = "android", target_env = "ohos")
))]
use libc::c_char;
use log::{error, info, warn};
use serde::Deserialize;
use serde_json::{Value, json};
use servo::{
    AllowOrDenyRequest, AuthenticationRequest, CSSPixel, ConsoleLogLevel, CreateNewWebViewRequest,
    Code, CompositionEvent, CompositionState, DeviceIntPoint, DeviceIntSize, EmbedderControl,
    EmbedderControlId, EventLoopWaker, GenericSender, ImeEvent, InputEvent, InputEventId,
    InputEventResult, JSValue, Key, KeyState, KeyboardEvent, LoadStatus, Location,
    MediaSessionEvent, Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent,
    MouseMoveEvent, NamedKey, PermissionRequest, PrefValue, Preferences,
    ScreenshotCaptureError, Servo, ServoDelegate, ServoError, TraversalId,
    UserContentManager, WebDriverCommandMsg, WebDriverJSResult, WebDriverLoadStatus,
    WebDriverScriptCommand, WebDriverSenders, WebView, WebViewDelegate, WebViewId,
};
use url::Url;

#[cfg(all(
    feature = "gamepad",
    not(any(target_os = "android", target_env = "ohos"))
))]
use servo::pref;

#[cfg(all(
    feature = "gamepad",
    not(any(target_os = "android", target_env = "ohos"))
))]
pub(crate) use crate::desktop::gamepad::ServoshellGamepadProvider;
use crate::agent_control::{self, AgentControlMessage};
use crate::prefs::{EXPERIMENTAL_PREFS, ServoShellPreferences};
use crate::webdriver::WebDriverEmbedderControls;
use crate::window::{PlatformWindow, ServoShellWindow, ServoShellWindowId};

#[cfg(all(
    any(coverage, llvm_pgo),
    any(target_os = "android", target_env = "ohos")
))]
unsafe extern "C" {
    fn __llvm_profile_set_filename(file: *const c_char);
    fn __llvm_profile_write_file();
}

#[derive(Default)]
pub struct WebViewCollection {
    /// List of top-level browsing contexts.
    /// Modified by EmbedderMsg::WebViewOpened and EmbedderMsg::WebViewClosed,
    /// and we exit if it ever becomes empty.
    webviews: HashMap<WebViewId, WebView>,

    /// The order in which the webviews were created.
    pub(crate) creation_order: Vec<WebViewId>,

    /// The [`WebView`] that is currently active. This is the [`WebView`] that is shown and has
    /// input focus.
    active_webview_id: Option<WebViewId>,
}

impl WebViewCollection {
    pub fn add(&mut self, webview: WebView) {
        let id = webview.id();
        self.creation_order.push(id);
        self.webviews.insert(id, webview);
    }

    /// Removes a webview from the collection by [`WebViewId`]. If the removed [`WebView`] was the active
    /// [`WebView`] then the next newest [`WebView`] will be activated.
    pub fn remove(&mut self, id: WebViewId) -> Option<WebView> {
        self.creation_order.retain(|&webview_id| webview_id != id);
        let removed_webview = self.webviews.remove(&id);

        if self.active_webview_id == Some(id) {
            self.active_webview_id = None;
            if let Some(newest) = self.creation_order.last() {
                self.activate_webview(*newest);
            }
        }

        removed_webview
    }

    pub fn get(&self, id: WebViewId) -> Option<&WebView> {
        self.webviews.get(&id)
    }

    pub fn contains(&self, id: WebViewId) -> bool {
        self.webviews.contains_key(&id)
    }

    pub fn active(&self) -> Option<&WebView> {
        self.active_webview_id.and_then(|id| self.webviews.get(&id))
    }

    pub fn active_id(&self) -> Option<WebViewId> {
        self.active_webview_id
    }

    /// Gets a reference to the most recently created webview, if any.
    pub fn newest(&self) -> Option<&WebView> {
        self.creation_order
            .last()
            .and_then(|id| self.webviews.get(id))
    }

    pub fn all_in_creation_order(&self) -> impl Iterator<Item = (WebViewId, &WebView)> {
        self.creation_order
            .iter()
            .filter_map(move |id| self.webviews.get(id).map(|webview| (*id, webview)))
    }

    /// Returns an iterator over all webview references (in arbitrary order).
    pub fn values(&self) -> impl Iterator<Item = &WebView> {
        self.webviews.values()
    }

    /// Returns true if the collection contains no webviews.
    pub fn is_empty(&self) -> bool {
        self.webviews.is_empty()
    }

    pub(crate) fn activate_webview(&mut self, id_to_activate: WebViewId) {
        assert!(self.creation_order.contains(&id_to_activate));

        self.active_webview_id = Some(id_to_activate);
        for (webview_id, webview) in self.all_in_creation_order() {
            if id_to_activate == webview_id {
                webview.show();
                webview.focus();
            } else {
                webview.hide();
                webview.blur();
            }
        }
    }

    pub(crate) fn activate_webview_by_index(&mut self, index: usize) {
        self.activate_webview(
            *self
                .creation_order
                .get(index)
                .expect("Tried to activate an unknown WebView"),
        );
    }
}

/// A command received via the user interacting with the user interface.
#[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
pub(crate) enum UserInterfaceCommand {
    Go(String),
    Back,
    Forward,
    Reload,
    ReloadAll,
    NewWebView,
    CloseWebView(WebViewId),
    NewWindow,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentFillKind {
    TextControl,
    ContentEditable,
}

#[derive(Deserialize)]
struct AgentResolvedPoint {
    x: f32,
    y: f32,
}

#[derive(Deserialize)]
struct AgentResolvedFillTarget {
    x: f32,
    y: f32,
    kind: AgentFillKind,
}

enum PendingAgentInputAction {
    Respond {
        response: Sender<Result<Value, String>>,
        payload: Value,
    },
    ContinueFill {
        webview_id: WebViewId,
        selector: String,
        value: String,
        kind: AgentFillKind,
        response: Sender<Result<Value, String>>,
    },
    VerifyFill {
        webview_id: WebViewId,
        selector: String,
        response: Sender<Result<Value, String>>,
    },
}

pub(crate) struct RunningAppState {
    /// The gamepad provider, used for handling gamepad events and set on each WebView.
    /// May be `None` if gamepad support is disabled or failed to initialize.
    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    gamepad_provider: Option<Rc<ServoshellGamepadProvider>>,

    /// The [`WebDriverSenders`] used to reply to pending WebDriver requests.
    pub(crate) webdriver_senders: RefCell<WebDriverSenders>,

    /// When running in WebDriver mode, [`WebDriverEmbedderControls`] is a virtual container
    /// for all embedder controls. This overrides the normal behavior where these controls
    /// are shown in the GUI or not processed at all in headless mode.
    pub(crate) webdriver_embedder_controls: WebDriverEmbedderControls,

    /// A [`HashMap`] of pending WebDriver events. It is the WebDriver embedder's responsibility
    /// to inform the WebDriver server when the event has been fully handled. This map is used
    /// to report back to WebDriver when that happens.
    pub(crate) pending_webdriver_events: RefCell<HashMap<InputEventId, Sender<()>>>,

    /// A [`Receiver`] for receiving commands from a running WebDriver server, if WebDriver
    /// was enabled.
    pub(crate) webdriver_receiver: Option<Receiver<WebDriverCommandMsg>>,

    /// A [`Receiver`] for receiving commands from the native agent control server, if enabled.
    pub(crate) agent_control_receiver: Option<Receiver<AgentControlMessage>>,

    /// A [`Sender`] used for scheduling follow-up native agent control work from async callbacks.
    pub(crate) agent_control_sender: Option<Sender<AgentControlMessage>>,

    /// servoshell specific preferences created during startup of the application.
    pub(crate) servoshell_preferences: ServoShellPreferences,

    /// A handle to the Servo instance.
    pub(crate) servo: Servo,

    /// Whether or not the application has achieved stable image output. This is used
    /// for the `exit_after_stable_image` option.
    pub(crate) achieved_stable_image: Rc<Cell<bool>>,

    /// The [`UserContentManager`] for all `WebView`s created.
    pub(crate) user_content_manager: Rc<UserContentManager>,

    /// Whether or not program exit has been triggered. This means that all windows
    /// will be destroyed and shutdown will start at the end of the current event loop.
    exit_scheduled: Cell<bool>,

    /// Whether the user has enabled experimental preferences.
    experimental_preferences_enabled: Cell<bool>,

    /// Pending native agent input sequences waiting for a specific Servo input event to finish.
    pending_agent_input_events: RefCell<HashMap<InputEventId, PendingAgentInputAction>>,

    /// The set of [`ServoShellWindow`]s that currently exist for this instance of servoshell.
    // This is the last field of the struct to ensure that windows are dropped *after* all
    // other references to the relevant rendering contexts have been destroyed.
    // See https://github.com/servo/servo/issues/36711.
    windows: RefCell<HashMap<ServoShellWindowId, Rc<ServoShellWindow>>>,

    /// The currently focused [`ServoShellWindow`], if one is focused.
    focused_window: RefCell<Option<Rc<ServoShellWindow>>>,
}

impl RunningAppState {
    pub(crate) fn new(
        servo: Servo,
        servoshell_preferences: ServoShellPreferences,
        event_loop_waker: Box<dyn EventLoopWaker>,
        user_content_manager: Rc<UserContentManager>,
        default_preferences: Preferences,
        #[cfg(all(
            feature = "gamepad",
            not(any(target_os = "android", target_env = "ohos"))
        ))]
        gamepad_provider: Option<Rc<ServoshellGamepadProvider>>,
    ) -> Self {
        servo.set_delegate(Rc::new(ServoShellServoDelegate));

        let webdriver_receiver = servoshell_preferences.webdriver_port.get().map(|port| {
            let (embedder_sender, embedder_receiver) = unbounded();
            webdriver_server::start_server(
                port,
                embedder_sender,
                event_loop_waker.clone_box(),
                default_preferences,
            );
            embedder_receiver
        });

        let (agent_control_sender, agent_control_receiver) =
            if let Some(port) = servoshell_preferences.agent_control_port {
                let (command_sender, command_receiver) = unbounded();
                agent_control::start_server(
                    port,
                    command_sender.clone(),
                    event_loop_waker.clone_box(),
                );
                (Some(command_sender), Some(command_receiver))
            } else {
                (None, None)
            };

        let experimental_preferences_enabled =
            Cell::new(servoshell_preferences.experimental_preferences_enabled);

        Self {
            windows: Default::default(),
            focused_window: Default::default(),
            #[cfg(all(
                feature = "gamepad",
                not(any(target_os = "android", target_env = "ohos"))
            ))]
            gamepad_provider,
            webdriver_senders: RefCell::default(),
            webdriver_embedder_controls: Default::default(),
            pending_webdriver_events: Default::default(),
            webdriver_receiver,
            agent_control_receiver,
            agent_control_sender,
            servoshell_preferences,
            servo,
            achieved_stable_image: Default::default(),
            exit_scheduled: Default::default(),
            user_content_manager,
            experimental_preferences_enabled,
            pending_agent_input_events: Default::default(),
        }
    }

    pub(crate) fn open_window(
        self: &Rc<Self>,
        platform_window: Rc<dyn PlatformWindow>,
        initial_url: Url,
    ) -> Rc<ServoShellWindow> {
        let window = Rc::new(ServoShellWindow::new(platform_window.clone()));
        window.create_and_activate_toplevel_webview(self.clone(), initial_url);
        self.windows
            .borrow_mut()
            .insert(window.id(), window.clone());

        // If the window already has platform focus, mark it as focused in our application state.
        if platform_window.has_platform_focus() {
            self.focus_window(window.clone());
        }

        window
    }

    pub(crate) fn windows<'a>(
        &'a self,
    ) -> Ref<'a, HashMap<ServoShellWindowId, Rc<ServoShellWindow>>> {
        self.windows.borrow()
    }

    pub(crate) fn focused_window(&self) -> Option<Rc<ServoShellWindow>> {
        self.focused_window.borrow().clone()
    }

    pub(crate) fn focus_window(&self, window: Rc<ServoShellWindow>) {
        window.focus();
        *self.focused_window.borrow_mut() = Some(window);
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn window(&self, id: ServoShellWindowId) -> Option<Rc<ServoShellWindow>> {
        self.windows.borrow().get(&id).cloned()
    }

    pub(crate) fn webview_by_id(&self, webview_id: WebViewId) -> Option<WebView> {
        self.maybe_window_for_webview_id(webview_id)?
            .webview_by_id(webview_id)
    }

    pub(crate) fn webdriver_receiver(&self) -> Option<&Receiver<WebDriverCommandMsg>> {
        self.webdriver_receiver.as_ref()
    }

    pub(crate) fn agent_control_receiver(&self) -> Option<&Receiver<AgentControlMessage>> {
        self.agent_control_receiver.as_ref()
    }

    pub(crate) fn agent_control_sender(&self) -> Option<Sender<AgentControlMessage>> {
        self.agent_control_sender.clone()
    }

    pub(crate) fn servo(&self) -> &Servo {
        &self.servo
    }

    fn primary_agent_webview(&self) -> Option<WebView> {
        if let Some(window) = self.focused_window() {
            if let Some(webview) = window.active_or_newest_webview() {
                return Some(webview);
            }
        }

        self.windows()
            .values()
            .next()
            .and_then(|window| window.active_or_newest_webview())
    }

    fn direct_agent_status(&self) -> Value {
        let Some(webview) = self.primary_agent_webview() else {
            return json!({
                "running": false,
            });
        };

        json!({
            "running": true,
            "url": webview.url(),
            "title": webview.page_title(),
            "load_status": format!("{:?}", webview.load_status()).to_lowercase(),
        })
    }

    fn should_repaint_on_new_frame(&self) -> bool {
        !self.servoshell_preferences.headless ||
            !self.servoshell_preferences.agent_render_on_demand ||
            self.servoshell_preferences.exit_after_stable_image ||
            self.servoshell_preferences.output_image_path.is_some()
    }

    fn send_agent_input_event(
        &self,
        webview: &WebView,
        event: InputEvent,
        completion: Option<PendingAgentInputAction>,
    ) {
        let event_id = webview.notify_input_event(event);
        if let Some(completion) = completion {
            self.pending_agent_input_events
                .borrow_mut()
                .insert(event_id, completion);
        }
    }

    fn dispatch_agent_click_sequence(
        &self,
        webview: &WebView,
        point: Point2D<f32, CSSPixel>,
        completion: PendingAgentInputAction,
    ) {
        let point = point.into();
        self.send_agent_input_event(webview, InputEvent::MouseMove(MouseMoveEvent::new(point)), None);
        self.send_agent_input_event(
            webview,
            InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Down,
                MouseButton::Left,
                point,
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::MouseButton(MouseButtonEvent::new(
                MouseButtonAction::Up,
                MouseButton::Left,
                point,
            )),
            Some(completion),
        );
    }

    fn command_shortcut_modifier() -> (Key, Code, Modifiers) {
        #[cfg(target_os = "macos")]
        {
            (Key::Named(NamedKey::Meta), Code::MetaLeft, Modifiers::META)
        }

        #[cfg(not(target_os = "macos"))]
        {
            (
                Key::Named(NamedKey::Control),
                Code::ControlLeft,
                Modifiers::CONTROL,
            )
        }
    }

    fn keyboard_event(
        state: KeyState,
        key: Key,
        code: Code,
        location: Location,
        modifiers: Modifiers,
    ) -> KeyboardEvent {
        KeyboardEvent::new_without_event(state, key, code, location, modifiers, false, false)
    }

    fn dispatch_agent_fill_sequence(
        &self,
        webview: &WebView,
        selector: String,
        value: String,
        kind: AgentFillKind,
        response: Sender<Result<Value, String>>,
    ) {
        let (modifier_key, modifier_code, modifier_mask) = Self::command_shortcut_modifier();
        let modifier_location = Location::Left;

        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Down,
                modifier_key.clone(),
                modifier_code,
                modifier_location,
                Modifiers::empty(),
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Down,
                Key::Character("a".into()),
                Code::KeyA,
                Location::Standard,
                modifier_mask,
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Up,
                Key::Character("a".into()),
                Code::KeyA,
                Location::Standard,
                modifier_mask,
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Up,
                modifier_key,
                modifier_code,
                modifier_location,
                modifier_mask,
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Down,
                Key::Named(NamedKey::Backspace),
                Code::Backspace,
                Location::Standard,
                Modifiers::empty(),
            )),
            None,
        );

        let completion = PendingAgentInputAction::VerifyFill {
            webview_id: webview.id(),
            selector,
            response,
        };
        if value.is_empty() {
            self.send_agent_input_event(
                webview,
                InputEvent::Keyboard(Self::keyboard_event(
                    KeyState::Up,
                    Key::Named(NamedKey::Backspace),
                    Code::Backspace,
                    Location::Standard,
                    Modifiers::empty(),
                )),
                Some(completion),
            );
            return;
        }

        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Up,
                Key::Named(NamedKey::Backspace),
                Code::Backspace,
                Location::Standard,
                Modifiers::empty(),
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Down,
                Key::Named(NamedKey::Process),
                Code::Unidentified,
                Location::Standard,
                Modifiers::empty(),
            )),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Ime(ImeEvent::Composition(CompositionEvent {
                state: CompositionState::End,
                data: value,
            })),
            None,
        );
        self.send_agent_input_event(
            webview,
            InputEvent::Keyboard(Self::keyboard_event(
                KeyState::Up,
                Key::Named(NamedKey::Process),
                Code::Unidentified,
                Location::Standard,
                Modifiers::empty(),
            )),
            Some(completion),
        );

        let _ = kind;
    }

    fn continue_agent_fill(
        &self,
        webview_id: WebViewId,
        selector: String,
        value: String,
        kind: AgentFillKind,
        response: Sender<Result<Value, String>>,
    ) {
        let Some(webview) = self.webview_by_id(webview_id) else {
            let _ = response.send(Err("No active Nickelium webview is available".into()));
            return;
        };

        self.dispatch_agent_fill_sequence(&webview, selector, value, kind, response);
    }

    fn verify_agent_fill(
        &self,
        webview_id: WebViewId,
        selector: String,
        response: Sender<Result<Value, String>>,
    ) {
        let Some(webview) = self.webview_by_id(webview_id) else {
            let _ = response.send(Err("No active Nickelium webview is available".into()));
            return;
        };

        webview.evaluate_javascript(agent_fill_value_script(&selector), move |result| {
            let payload = match result {
                Ok(value) => match js_value_to_json(value) {
                    Value::String(value) => Ok(json!({
                        "filled": true,
                        "value": value,
                    })),
                    Value::Null => Err(format!(
                        "Nickelium could not verify the filled value for selector `{selector}`"
                    )),
                    other => Err(format!(
                        "Nickelium fill verification returned unexpected payload: {other}"
                    )),
                },
                Err(error) => Err(format!("Failed to verify Nickelium fill result: {error:?}")),
            };
            let _ = response.send(payload);
        });
    }

    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    pub(crate) fn gamepad_provider(&self) -> Option<Rc<ServoshellGamepadProvider>> {
        self.gamepad_provider.clone()
    }

    pub(crate) fn schedule_exit(&self) {
        // When explicitly required to shutdown, unset webdriver port
        // which allows normal shutdown.
        // Note that when not explicitly required to shutdown, we still keep Servo alive
        // when all tabs are closed when `webdriver_port` enabled, which is necessary
        // to run wpt test using servodriver.
        self.servoshell_preferences.webdriver_port.set(None);
        self.exit_scheduled.set(true);

        #[cfg(all(
            any(coverage, llvm_pgo),
            any(target_os = "android", target_env = "ohos")
        ))]
        {
            use std::ffi::CString;

            use crate::prefs::default_config_dir;

            let mut profile_path = default_config_dir().expect("Need a config dir");
            profile_path.push("profiles/");
            let filename = format!(
                "{}/profile-%h-%p.profraw",
                profile_path.to_str().expect("Should be unicode")
            );
            let c_filename = CString::new(filename).expect("Need a valid cstring");
            unsafe {
                __llvm_profile_set_filename(c_filename.as_ptr() as *const c_char);
                __llvm_profile_write_file()
            }
        }
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn experimental_preferences_enabled(&self) -> bool {
        self.experimental_preferences_enabled.get()
    }

    #[cfg_attr(any(target_os = "android", target_env = "ohos"), expect(dead_code))]
    pub(crate) fn set_experimental_preferences_enabled(&self, new_value: bool) {
        let old_value = self.experimental_preferences_enabled.replace(new_value);
        if old_value == new_value {
            return;
        }
        for pref in EXPERIMENTAL_PREFS {
            self.servo.set_preference(pref, PrefValue::Bool(new_value));
        }
    }

    /// Close any [`ServoShellWindow`] that doesn't have an open [`WebView`].
    fn close_empty_windows(&self) {
        self.windows.borrow_mut().retain(|_, window| {
            if !self.exit_scheduled.get() && !window.should_close() {
                return true;
            }

            if let Some(focused_window) = self.focused_window() {
                if Rc::ptr_eq(window, &focused_window) {
                    *self.focused_window.borrow_mut() = None;
                }
            }
            false
        });
    }

    /// Spins the internal application event loop.
    ///
    /// - Notifies Servo about incoming gamepad events
    /// - Spin the Servo event loop, which will update Servo's embedding layer and trigger
    ///   delegate methods.
    ///
    /// Returns true if the event loop should continue spinning and false if it should exit.
    pub(crate) fn spin_event_loop(
        self: &Rc<Self>,
        create_platform_window: Option<&dyn Fn(Url) -> Rc<dyn PlatformWindow>>,
    ) -> bool {
        // We clone here to avoid a double borrow. User interface commands can update the list of windows.
        let windows: Vec<_> = self.windows.borrow().values().cloned().collect();
        for window in windows {
            window.handle_interface_commands(self, create_platform_window);
        }

        self.handle_webdriver_messages(create_platform_window);
        self.handle_agent_control_messages();

        #[cfg(all(
            feature = "gamepad",
            not(any(target_os = "android", target_env = "ohos"))
        ))]
        if pref!(dom_gamepad_enabled) {
            self.handle_gamepad_events();
        }

        self.servo.spin_event_loop();

        for window in self.windows.borrow().values() {
            window.update_and_request_repaint_if_necessary(self);
        }

        if self.servoshell_preferences.exit_after_stable_image && self.achieved_stable_image.get() {
            self.schedule_exit();
        }

        self.close_empty_windows();

        // When no more windows are open, exit the application. Do not do this when
        // running WebDriver or the native Nickelium agent control server, both of which
        // expect to keep the process alive with no user-facing windows open.
        if self.servoshell_preferences.webdriver_port.get().is_none() &&
            self.servoshell_preferences.agent_control_port.is_none() &&
            self.windows.borrow().is_empty()
        {
            self.schedule_exit()
        }

        !self.exit_scheduled.get()
    }

    pub(crate) fn maybe_window_for_webview_id(
        &self,
        webview_id: WebViewId,
    ) -> Option<Rc<ServoShellWindow>> {
        for window in self.windows.borrow().values() {
            if window.contains_webview(webview_id) {
                return Some(window.clone());
            }
        }
        None
    }

    pub(crate) fn window_for_webview_id(&self, webview_id: WebViewId) -> Rc<ServoShellWindow> {
        self.maybe_window_for_webview_id(webview_id)
            .unwrap_or_else(|| panic!("Looking for unexpected WebView: {webview_id:?}"))
    }

    pub(crate) fn platform_window_for_webview_id(
        &self,
        webview_id: WebViewId,
    ) -> Rc<dyn PlatformWindow> {
        self.window_for_webview_id(webview_id).platform_window()
    }

    /// If we are exiting after achieving a stable image or we want to save the display of the
    /// [`WebView`] to an image file, request a screenshot of the [`WebView`].
    fn maybe_request_screenshot(&self, webview: WebView) {
        let output_path = self.servoshell_preferences.output_image_path.clone();
        if !self.servoshell_preferences.exit_after_stable_image && output_path.is_none() {
            return;
        }

        // Never request more than a single screenshot for now.
        let achieved_stable_image = self.achieved_stable_image.clone();
        if achieved_stable_image.get() {
            return;
        }

        webview.take_screenshot(None, move |image| {
            achieved_stable_image.set(true);

            let Some(output_path) = output_path else {
                return;
            };

            let image = match image {
                Ok(image) => image,
                Err(error) => {
                    error!("Could not take screenshot: {error:?}");
                    return;
                },
            };

            if let Err(error) = save_image_to_path(&image, &output_path) {
                error!("Failed to save screenshot: {error}.");
            }
        });
    }

    pub(crate) fn set_pending_traversal(
        &self,
        traversal_id: TraversalId,
        sender: GenericSender<WebDriverLoadStatus>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .pending_traversals
            .insert(traversal_id, sender);
    }

    pub(crate) fn set_load_status_sender(
        &self,
        webview_id: WebViewId,
        sender: GenericSender<WebDriverLoadStatus>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .load_status_senders
            .insert(webview_id, sender);
    }

    fn remove_load_status_sender(&self, webview_id: WebViewId) {
        self.webdriver_senders
            .borrow_mut()
            .load_status_senders
            .remove(&webview_id);
    }

    fn set_script_command_interrupt_sender(
        &self,
        sender: Option<GenericSender<WebDriverJSResult>>,
    ) {
        self.webdriver_senders
            .borrow_mut()
            .script_evaluation_interrupt_sender = sender;
    }

    pub(crate) fn handle_webdriver_input_event(
        &self,
        webview_id: WebViewId,
        input_event: InputEvent,
        response_sender: Option<Sender<()>>,
    ) {
        if let Some(webview) = self.webview_by_id(webview_id) {
            let event_id = webview.notify_input_event(input_event);
            if let Some(response_sender) = response_sender {
                self.pending_webdriver_events
                    .borrow_mut()
                    .insert(event_id, response_sender);
            }
        } else {
            error!("Could not find WebView ({webview_id:?}) for WebDriver event: {input_event:?}");
        };
    }

    pub(crate) fn handle_webdriver_screenshot(
        &self,
        webview_id: WebViewId,
        rect: Option<Rect<f32, CSSPixel>>,
        result_sender: Sender<Result<RgbaImage, ScreenshotCaptureError>>,
    ) {
        if let Some(webview) = self.webview_by_id(webview_id) {
            let rect = rect.map(|rect| rect.to_box2d().into());
            webview.take_screenshot(rect, move |result| {
                if let Err(error) = result_sender.send(result) {
                    warn!("Failed to send response to TakeScreenshot: {error}");
                }
            });
        } else if let Err(error) =
            result_sender.send(Err(ScreenshotCaptureError::WebViewDoesNotExist))
        {
            error!("Failed to send response to TakeScreenshot: {error}");
        }
    }

    pub(crate) fn handle_webdriver_script_command(&self, script_command: &WebDriverScriptCommand) {
        match script_command {
            WebDriverScriptCommand::ExecuteScriptWithCallback(_webview_id, response_sender) => {
                // Give embedder a chance to interrupt the script command.
                // Webdriver only handles 1 script command at a time, so we can
                // safely set a new interrupt sender and remove the previous one here.
                self.set_script_command_interrupt_sender(Some(response_sender.clone()));
            },
            WebDriverScriptCommand::AddLoadStatusSender(webview_id, load_status_sender) => {
                self.set_load_status_sender(*webview_id, load_status_sender.clone());
            },
            WebDriverScriptCommand::RemoveLoadStatusSender(webview_id) => {
                self.remove_load_status_sender(*webview_id);
            },
            _ => {
                self.set_script_command_interrupt_sender(None);
            },
        }
    }

    pub(crate) fn handle_webdriver_load_url(
        &self,
        webview_id: WebViewId,
        url: Url,
        load_status_sender: GenericSender<WebDriverLoadStatus>,
    ) {
        let Some(webview) = self.webview_by_id(webview_id) else {
            return;
        };

        self.platform_window_for_webview_id(webview_id)
            .dismiss_embedder_controls_for_webview(webview_id);

        info!("Loading URL in webview {}: {}", webview_id, url);
        self.set_load_status_sender(webview_id, load_status_sender);
        webview.load(url);
    }

    pub(crate) fn handle_agent_control_messages(&self) {
        let Some(agent_control_receiver) = self.agent_control_receiver() else {
            return;
        };

        while let Ok(message) = agent_control_receiver.try_recv() {
            match message {
                AgentControlMessage::Status { response } => {
                    let _ = response.send(Ok(self.direct_agent_status()));
                },
                AgentControlMessage::Open { url, response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    info!("Loading URL in Nickelium direct agent mode: {}", url);
                    self.platform_window_for_webview_id(webview.id())
                        .dismiss_embedder_controls_for_webview(webview.id());
                    let response_url = url.clone();
                    webview.load(url);
                    let _ = response.send(Ok(json!({
                        "started": true,
                        "url": response_url,
                    })));
                },
                AgentControlMessage::Click { selector, response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };
                    let Some(agent_control_sender) = self.agent_control_sender() else {
                        let _ = response.send(Err(
                            "Nickelium agent control follow-up channel is unavailable".into(),
                        ));
                        continue;
                    };

                    let script = agent_click_target_script(&selector);
                    let webview_id = webview.id();
                    let response_on_send_error = response.clone();
                    webview.evaluate_javascript(script, move |result| {
                        let follow_up = AgentControlMessage::ResolvedClick {
                            webview_id,
                            response,
                            result: result
                                .map(js_value_to_json)
                                .map_err(|error| {
                                    format!(
                                        "Failed to resolve Nickelium click target: {error:?}"
                                    )
                                }),
                        };
                        if let Err(error) = agent_control_sender.send(follow_up) {
                            let _ = response_on_send_error.send(Err(format!(
                                "Failed to queue Nickelium native click: {error}"
                            )));
                        }
                    });
                },
                AgentControlMessage::ResolvedClick {
                    webview_id,
                    response,
                    result,
                } => {
                    let Some(webview) = self.webview_by_id(webview_id) else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    let point: AgentResolvedPoint = match result
                        .and_then(|value| {
                            serde_json::from_value(value).map_err(|error| {
                                format!("Failed to decode Nickelium click target: {error}")
                            })
                        }) {
                        Ok(point) => point,
                        Err(error) => {
                            let _ = response.send(Err(error));
                            continue;
                        },
                    };

                    self.dispatch_agent_click_sequence(
                        &webview,
                        Point2D::new(point.x, point.y),
                        PendingAgentInputAction::Respond {
                            response,
                            payload: json!({
                                "clicked": true,
                                "point": {
                                    "x": point.x,
                                    "y": point.y,
                                },
                            }),
                        },
                    );
                },
                AgentControlMessage::Fill {
                    selector,
                    value,
                    response,
                } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };
                    let Some(agent_control_sender) = self.agent_control_sender() else {
                        let _ = response.send(Err(
                            "Nickelium agent control follow-up channel is unavailable".into(),
                        ));
                        continue;
                    };

                    let script = agent_fill_target_script(&selector);
                    let webview_id = webview.id();
                    let response_on_send_error = response.clone();
                    webview.evaluate_javascript(script, move |result| {
                        let follow_up = AgentControlMessage::ResolvedFillTarget {
                            webview_id,
                            selector,
                            value,
                            response,
                            result: result
                                .map(js_value_to_json)
                                .map_err(|error| {
                                    format!(
                                        "Failed to resolve Nickelium fill target: {error:?}"
                                    )
                                }),
                        };
                        if let Err(error) = agent_control_sender.send(follow_up) {
                            let _ = response_on_send_error.send(Err(format!(
                                "Failed to queue Nickelium native fill: {error}"
                            )));
                        }
                    });
                },
                AgentControlMessage::ResolvedFillTarget {
                    webview_id,
                    selector,
                    value,
                    response,
                    result,
                } => {
                    let Some(webview) = self.webview_by_id(webview_id) else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    let target: AgentResolvedFillTarget = match result
                        .and_then(|value| {
                            serde_json::from_value(value).map_err(|error| {
                                format!("Failed to decode Nickelium fill target: {error}")
                            })
                        }) {
                        Ok(target) => target,
                        Err(error) => {
                            let _ = response.send(Err(error));
                            continue;
                        },
                    };

                    self.dispatch_agent_click_sequence(
                        &webview,
                        Point2D::new(target.x, target.y),
                        PendingAgentInputAction::ContinueFill {
                            webview_id,
                            selector,
                            value,
                            kind: target.kind,
                            response,
                        },
                    );
                },
                AgentControlMessage::Eval { script, response } |
                AgentControlMessage::DomBatch { script, response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    webview.evaluate_javascript(script, move |result| {
                        let payload = result
                            .map(|value| json!({ "value": js_value_to_json(value) }))
                            .map_err(|error| {
                                format!("Failed to evaluate JavaScript in Nickelium: {error:?}")
                            });
                        let _ = response.send(payload);
                    });
                },
                AgentControlMessage::Html { response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    webview.evaluate_javascript(agent_html_script(), move |result| {
                        let payload = result
                            .map(|value| json!({ "html": js_value_to_json(value) }))
                            .map_err(|error| {
                                format!("Failed to capture HTML in Nickelium: {error:?}")
                            });
                        let _ = response.send(payload);
                    });
                },
                AgentControlMessage::Snapshot { response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    let url = webview.url();
                    let title = webview.page_title();
                    webview.evaluate_javascript(agent_snapshot_script(), move |result| {
                        let payload = result
                            .map(|value| {
                                json!({
                                    "url": url,
                                    "title": title,
                                    "value": js_value_to_json(value),
                                })
                            })
                            .map_err(|error| {
                                format!("Failed to capture DOM snapshot in Nickelium: {error:?}")
                            });
                        let _ = response.send(payload);
                    });
                },
                AgentControlMessage::Text { selector, response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    let selector_json =
                        serde_json::to_string(&selector).expect("selector should serialize");
                    let script = format!(
                        "(() => {{ const node = document.querySelector({selector_json}); return node ? (node.textContent ?? '') : null; }})()"
                    );
                    webview.evaluate_javascript(script, move |result| {
                        let payload = result
                            .map(|value| json!({ "text": js_value_to_json(value) }))
                            .map_err(|error| {
                                format!("Failed to read DOM text in Nickelium: {error:?}")
                            });
                        let _ = response.send(payload);
                    });
                },
                AgentControlMessage::Screenshot { path, response } => {
                    let Some(webview) = self.primary_agent_webview() else {
                        let _ = response.send(Err("No active Nickelium webview is available".into()));
                        continue;
                    };

                    if self.servoshell_preferences.agent_render_on_demand {
                        self.window_for_webview_id(webview.id()).repaint_webviews();
                    }

                    webview.take_screenshot(None, move |result| {
                        let payload = match result {
                            Ok(image) => {
                                if let Err(error) = save_image_to_path(&image, &path) {
                                    Err(format!(
                                        "Failed to save Nickelium screenshot {}: {error}",
                                        path.display()
                                    ))
                                } else {
                                    Ok(json!({ "path": path }))
                                }
                            },
                            Err(error) => {
                                Err(format!("Failed to capture Nickelium screenshot: {error:?}"))
                            },
                        };
                        let _ = response.send(payload);
                    });
                },
                AgentControlMessage::Shutdown { response } => {
                    self.schedule_exit();
                    let _ = response.send(Ok(json!({ "running": false })));
                },
            }
        }
    }

    #[cfg(all(
        feature = "gamepad",
        not(any(target_os = "android", target_env = "ohos"))
    ))]
    pub(crate) fn handle_gamepad_events(&self) {
        let Some(gamepad_provider) = self.gamepad_provider.as_ref() else {
            return;
        };
        let Some(active_webview) = self
            .focused_window()
            .and_then(|window| window.active_webview())
        else {
            return;
        };
        gamepad_provider.handle_gamepad_events(active_webview);
    }

    pub(crate) fn handle_focused(&self, window: Rc<ServoShellWindow>) {
        *self.focused_window.borrow_mut() = Some(window);
    }

    /// Interrupt any ongoing WebDriver-based script evaluation.
    ///
    /// From <https://w3c.github.io/webdriver/#dfn-execute-a-function-body>:
    /// > The rules to execute a function body are as follows. The algorithm returns
    /// > an ECMAScript completion record.
    /// >
    /// > If at any point during the algorithm a user prompt appears, immediately return
    /// > Completion { Type: normal, Value: null, Target: empty }, but continue to run the
    /// >  other steps of this algorithm in parallel.
    fn interrupt_webdriver_script_evaluation(&self) {
        if let Some(sender) = &self
            .webdriver_senders
            .borrow()
            .script_evaluation_interrupt_sender
        {
            sender.send(Ok(JSValue::Null)).unwrap_or_else(|err| {
                info!(
                    "Notify dialog appear failed. Maybe the channel to webdriver is closed: {err}"
                );
            });
        }
    }
}

fn save_image_to_path(
    image: &RgbaImage,
    output_path: impl AsRef<Path>,
) -> Result<(), image::ImageError> {
    let output_path = output_path.as_ref();
    if let Some(parent) = output_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let image_format = ImageFormat::from_path(output_path).unwrap_or(ImageFormat::Png);
    DynamicImage::ImageRgba8(image.clone()).save_with_format(output_path, image_format)
}

fn js_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("string should serialize")
}

fn agent_click_target_script(selector: &str) -> String {
    let selector = js_string_literal(selector);
    format!(
        r#"
(() => {{
  const node = document.querySelector({selector});
  if (!node) {{
    throw new Error("No element matches selector: " + {selector});
  }}
  node.scrollIntoView?.({{ block: "center", inline: "center" }});
  const rect = node.getBoundingClientRect();
  if (!rect.width || !rect.height) {{
    throw new Error("Target element has no clickable area: " + {selector});
  }}
  return {{
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2,
  }};
}})()
"#
    )
}

fn agent_fill_target_script(selector: &str) -> String {
    let selector = js_string_literal(selector);
    format!(
        r#"
(() => {{
  const node = document.querySelector({selector});
  if (!node) {{
    throw new Error("No element matches selector: " + {selector});
  }}
  node.scrollIntoView?.({{ block: "center", inline: "center" }});
  const rect = node.getBoundingClientRect();
  if (!rect.width || !rect.height) {{
    throw new Error("Target element has no editable area: " + {selector});
  }}
  if (node instanceof HTMLInputElement) {{
    const unsupported = new Set([
      "button",
      "checkbox",
      "color",
      "file",
      "hidden",
      "image",
      "radio",
      "range",
      "reset",
      "submit"
    ]);
    if (node.disabled || node.readOnly || unsupported.has(node.type)) {{
      throw new Error("Target input cannot be edited with keyboard input: " + {selector});
    }}
    return {{
      x: rect.left + rect.width / 2,
      y: rect.top + rect.height / 2,
      kind: "text_control"
    }};
  }}
  if (node instanceof HTMLTextAreaElement) {{
    if (node.disabled || node.readOnly) {{
      throw new Error("Target textarea cannot be edited with keyboard input: " + {selector});
    }}
    return {{
      x: rect.left + rect.width / 2,
      y: rect.top + rect.height / 2,
      kind: "text_control"
    }};
  }}
  if (node.isContentEditable) {{
    return {{
      x: rect.left + rect.width / 2,
      y: rect.top + rect.height / 2,
      kind: "content_editable"
    }};
  }}
  throw new Error("Target element does not support text input: " + {selector});
}})()
"#
    )
}

fn agent_fill_value_script(selector: &str) -> String {
    let selector = js_string_literal(selector);
    format!(
        r#"
(() => {{
  const node = document.querySelector({selector});
  if (!node) {{
    return null;
  }}
  if (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement) {{
    return node.value ?? "";
  }}
  if (node.isContentEditable) {{
    return node.textContent ?? "";
  }}
  return null;
}})()
"#
    )
}

fn agent_html_script() -> String {
    "(() => document.documentElement ? document.documentElement.outerHTML : \"\")()".into()
}

fn agent_snapshot_script() -> String {
    r#"
(() => ({
  text: document.body ? document.body.innerText : "",
  html: document.documentElement ? document.documentElement.outerHTML : "",
}))()
"#
    .into()
}

fn js_value_to_json(value: JSValue) -> Value {
    match value {
        JSValue::Undefined | JSValue::Null => Value::Null,
        JSValue::Boolean(value) => Value::Bool(value),
        JSValue::Number(value) => json!(value),
        JSValue::String(value) |
        JSValue::Element(value) |
        JSValue::ShadowRoot(value) |
        JSValue::Frame(value) |
        JSValue::Window(value) => Value::String(value),
        JSValue::Array(values) => Value::Array(values.into_iter().map(js_value_to_json).collect()),
        JSValue::Object(entries) => Value::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key, js_value_to_json(value)))
                .collect(),
        ),
    }
}

impl WebViewDelegate for RunningAppState {
    fn screen_geometry(&self, webview: WebView) -> Option<servo::ScreenGeometry> {
        Some(
            self.platform_window_for_webview_id(webview.id())
                .screen_geometry(),
        )
    }

    fn notify_status_text_changed(&self, webview: WebView, _status: Option<String>) {
        self.window_for_webview_id(webview.id()).set_needs_update();
    }

    fn notify_history_changed(&self, webview: WebView, _entries: Vec<Url>, _current: usize) {
        self.window_for_webview_id(webview.id()).set_needs_update();
    }

    fn notify_page_title_changed(&self, webview: WebView, _: Option<String>) {
        self.window_for_webview_id(webview.id()).set_needs_update();
    }

    fn notify_traversal_complete(&self, _webview: WebView, traversal_id: TraversalId) {
        let mut webdriver_state = self.webdriver_senders.borrow_mut();
        if let Entry::Occupied(entry) = webdriver_state.pending_traversals.entry(traversal_id) {
            let sender = entry.remove();
            let _ = sender.send(WebDriverLoadStatus::Complete);
        }
    }

    fn request_move_to(&self, webview: WebView, new_position: DeviceIntPoint) {
        self.platform_window_for_webview_id(webview.id())
            .set_position(new_position);
    }

    fn request_resize_to(&self, webview: WebView, requested_outer_size: DeviceIntSize) {
        self.platform_window_for_webview_id(webview.id())
            .request_resize(&webview, requested_outer_size);
    }

    fn request_authentication(
        &self,
        webview: WebView,
        authentication_request: AuthenticationRequest,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .show_http_authentication_dialog(webview.id(), authentication_request);
    }

    fn request_create_new(&self, parent_webview: WebView, request: CreateNewWebViewRequest) {
        let window = self.window_for_webview_id(parent_webview.id());
        let platform_window = window.platform_window();
        let webview = request
            .builder(platform_window.rendering_context())
            .hidpi_scale_factor(platform_window.hidpi_scale_factor())
            .delegate(parent_webview.delegate())
            .build();

        webview.notify_theme_change(platform_window.theme());
        window.add_webview(webview.clone());

        // When WebDriver is enabled, do not focus and raise the WebView to the top,
        // as that is what the specification expects. Otherwise, we would like `window.open()`
        // to create a new foreground tab
        if self.servoshell_preferences.webdriver_port.get().is_none() {
            window.activate_webview(webview.id());
        } else {
            webview.hide();
        }
    }

    fn notify_closed(&self, webview: WebView) {
        self.window_for_webview_id(webview.id())
            .close_webview(webview.id())
    }

    fn notify_input_event_handled(
        &self,
        webview: WebView,
        id: InputEventId,
        result: InputEventResult,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .notify_input_event_handled(&webview, id, result);
        if let Some(response_sender) = self.pending_webdriver_events.borrow_mut().remove(&id) {
            let _ = response_sender.send(());
        }
        let pending_action = self.pending_agent_input_events.borrow_mut().remove(&id);
        match pending_action {
            Some(PendingAgentInputAction::Respond { response, payload }) => {
                let _ = response.send(Ok(payload));
            },
            Some(PendingAgentInputAction::ContinueFill {
                webview_id,
                selector,
                value,
                kind,
                response,
            }) => {
                self.continue_agent_fill(webview_id, selector, value, kind, response);
            },
            Some(PendingAgentInputAction::VerifyFill {
                webview_id,
                selector,
                response,
            }) => {
                self.verify_agent_fill(webview_id, selector, response);
            },
            None => {},
        }
    }

    fn notify_cursor_changed(&self, webview: WebView, cursor: servo::Cursor) {
        self.platform_window_for_webview_id(webview.id())
            .set_cursor(cursor);
    }

    fn notify_load_status_changed(&self, webview: WebView, status: LoadStatus) {
        self.window_for_webview_id(webview.id()).set_needs_update();

        if status == LoadStatus::Complete {
            if let Some(sender) = self
                .webdriver_senders
                .borrow_mut()
                .load_status_senders
                .remove(&webview.id())
            {
                let _ = sender.send(WebDriverLoadStatus::Complete);
            }
            self.maybe_request_screenshot(webview);
        }
    }

    fn notify_fullscreen_state_changed(&self, webview: WebView, fullscreen_state: bool) {
        self.platform_window_for_webview_id(webview.id())
            .set_fullscreen(fullscreen_state);
    }

    fn show_bluetooth_device_dialog(
        &self,
        webview: WebView,
        devices: Vec<String>,
        response_sender: GenericSender<Option<String>>,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .show_bluetooth_device_dialog(webview.id(), devices, response_sender);
    }

    fn request_permission(&self, webview: WebView, permission_request: PermissionRequest) {
        self.platform_window_for_webview_id(webview.id())
            .show_permission_dialog(webview.id(), permission_request);
    }

    fn notify_new_frame_ready(&self, webview: WebView) {
        if self.should_repaint_on_new_frame() {
            self.window_for_webview_id(webview.id()).set_needs_repaint();
        }
    }

    fn show_embedder_control(&self, webview: WebView, embedder_control: EmbedderControl) {
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            if matches!(&embedder_control, EmbedderControl::SimpleDialog(..)) {
                self.interrupt_webdriver_script_evaluation();

                // Dialogs block the page load, so need need to notify WebDriver
                if let Some(sender) = self
                    .webdriver_senders
                    .borrow_mut()
                    .load_status_senders
                    .get(&webview.id())
                {
                    let _ = sender.send(WebDriverLoadStatus::Blocked);
                };
            }

            self.webdriver_embedder_controls
                .show_embedder_control(webview.id(), embedder_control);
            return;
        }

        self.window_for_webview_id(webview.id())
            .show_embedder_control(webview, embedder_control);
    }

    fn hide_embedder_control(&self, webview: WebView, embedder_control_id: EmbedderControlId) {
        if self.servoshell_preferences.webdriver_port.get().is_some() {
            self.webdriver_embedder_controls
                .hide_embedder_control(webview.id(), embedder_control_id);
            return;
        }

        self.window_for_webview_id(webview.id())
            .hide_embedder_control(webview, embedder_control_id);
    }

    fn notify_favicon_changed(&self, webview: WebView) {
        self.window_for_webview_id(webview.id())
            .notify_favicon_changed(webview);
    }

    fn notify_media_session_event(&self, webview: WebView, event: MediaSessionEvent) {
        self.platform_window_for_webview_id(webview.id())
            .notify_media_session_event(event);
    }

    fn notify_crashed(&self, webview: WebView, reason: String, backtrace: Option<String>) {
        self.platform_window_for_webview_id(webview.id())
            .notify_crashed(webview, reason, backtrace);
    }

    fn show_console_message(&self, webview: WebView, level: ConsoleLogLevel, message: String) {
        self.platform_window_for_webview_id(webview.id())
            .show_console_message(level, &message);
    }

    fn notify_accessibility_tree_update(
        &self,
        webview: WebView,
        tree_update: accesskit::TreeUpdate,
    ) {
        self.platform_window_for_webview_id(webview.id())
            .notify_accessibility_tree_update(webview, tree_update);
    }
}

struct ServoShellServoDelegate;
impl ServoDelegate for ServoShellServoDelegate {
    fn notify_devtools_server_started(&self, port: u16, _token: String) {
        info!("Devtools Server running on port {port}");
    }

    fn request_devtools_connection(&self, request: AllowOrDenyRequest) {
        request.allow();
    }

    fn notify_error(&self, error: ServoError) {
        error!("Saw Servo error: {error:?}!");
    }

    fn show_console_message(&self, level: ConsoleLogLevel, message: String) {
        // For messages without a WebView context, apply platform-specific behavior
        #[cfg(not(any(target_os = "android", target_env = "ohos")))]
        println!("{message}");
        log::log!(level.into(), "{message}");
    }
}
