// SPDX-License-Identifier: GPL-3.0-only
//! Generic IME panel applet.
//!
//! Discovers the active IME from cosmic-comp `input_method_map` + `active_layout`,
//! then mirrors `ModeLabel` / `MenuItems` from `org.inputmethod.Control1`.

use std::sync::LazyLock;

use cosmic::{
    app,
    app::Core,
    applet,
    iced::Subscription,
    iced::core::window,
    iced::{
        Task,
        platform_specific::shell::commands::popup::{destroy_popup, get_popup},
        stream,
        window::Id,
    },
    prelude::*,
    widget::{self, autosize},
};
use cosmic_comp_config::CosmicCompConfig;
use futures::StreamExt;
use ime_control::{INTERFACE, MenuItem};

const APP_ID: &str = "com.system76.CosmicAppletIme";

static AUTOSIZE_MAIN_ID: LazyLock<widget::Id> = LazyLock::new(|| widget::Id::new("autosize-main"));

pub fn run() -> cosmic::iced::Result {
    tracing_subscriber::fmt().with_env_filter("warn").init();
    let _ = tracing_log::LogTracer::init();
    cosmic::applet::run::<ImeApplet>(())
}

struct ImeApplet {
    core: Core,
    popup: Option<Id>,
    mode_label: String,
    menu: Vec<MenuItem>,
    settings_command: String,
    /// Active IME bus id (`pinyinwl`, `chewingwl`, …), not a filesystem path.
    active_command: Option<String>,
    /// Whether the panel currently maps this applet (applet-driven visibility).
    panel_visible: bool,
    comp_config: CosmicCompConfig,
}

#[derive(Clone, Debug)]
enum Message {
    TogglePopup,
    PopupClosed(Id),
    CompConfig(Box<CosmicCompConfig>),
    ModeLabel(String),
    Menu(Vec<MenuItem>),
    SettingsCommand(String),
    Activate(String),
}

impl cosmic::Application for ImeApplet {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, app::Task<Self::Message>) {
        (
            ImeApplet {
                core,
                popup: None,
                mode_label: String::from("—"),
                menu: Vec::new(),
                settings_command: String::new(),
                active_command: None,
                // Panel maps the applet on launch; hide on first config if no IME.
                panel_visible: true,
                comp_config: CosmicCompConfig::default(),
            },
            Task::none(),
        )
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                return if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = Id::unique();
                    self.popup = Some(new_id);
                    let popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    get_popup(popup_settings)
                };
            }
            Message::PopupClosed(id) => {
                if self.popup == Some(id) {
                    self.popup = None;
                }
            }
            Message::CompConfig(cfg) => {
                self.comp_config = *cfg;
                let cmd = active_ime_bus_id(&self.comp_config);
                let want_visible = cmd.is_some();
                let mut task = Task::none();
                if cmd != self.active_command {
                    self.active_command = cmd;
                    // Clear until the new IME's D-Bus watch fills in.
                    self.mode_label = String::from("—");
                    self.menu.clear();
                    self.settings_command.clear();
                }
                if want_visible != self.panel_visible {
                    self.panel_visible = want_visible;
                    if let Some(id) = self.core.main_window_id() {
                        task = self.core.applet.set_visible(id, want_visible);
                    }
                    if !want_visible {
                        if let Some(p) = self.popup.take() {
                            task = task.chain(destroy_popup(p));
                        }
                    }
                }
                return task;
            }
            Message::ModeLabel(text) => {
                self.mode_label = text;
            }
            Message::Menu(items) => {
                self.menu = items;
            }
            Message::SettingsCommand(cmd) => {
                self.settings_command = cmd;
            }
            Message::Activate(id) => {
                let Some(command) = self.active_command.clone() else {
                    return Task::none();
                };
                let settings_fallback = self.settings_command.clone();
                tokio::spawn(async move {
                    if let Err(e) = dbus_activate(&command, &id).await {
                        tracing::warn!("Activate({id}) failed: {e}");
                        if id == ime_control::action::SETTINGS && !settings_fallback.is_empty() {
                            launch_settings_fallback(&settings_fallback);
                        }
                    }
                });
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p);
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let label = if self.mode_label.is_empty() {
            "—".to_string()
        } else {
            self.mode_label.clone()
        };
        let content: Element<'_, Self::Message> = self
            .core
            .applet
            .text_button(self.core.applet.text(label), Message::TogglePopup)
            .into();
        autosize::autosize(content, AUTOSIZE_MAIN_ID.clone()).into()
    }

    fn view_window(&self, _id: Id) -> Element<'_, Self::Message> {
        let mut list = widget::column::with_capacity(self.menu.len().max(1) + 1).padding([8, 0]);

        if self.menu.is_empty() {
            list = list.push(applet::padded_control(widget::text::body(
                "No active input method",
            )));
        } else {
            for item in &self.menu {
                let id = item.id.clone();
                let btn = applet::menu_button(widget::text::body(item.label.clone()));
                list = list.push(if item.enabled {
                    btn.on_press(Message::Activate(id))
                } else {
                    btn
                });
            }
        }

        self.core.applet.popup_container(list).into()
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let config = self.core.watch_config("com.system76.CosmicComp").map(
            |update: cosmic::cosmic_config::Update<CosmicCompConfig>| {
                if !update.errors.is_empty() {
                    tracing::error!(
                        "errors loading config {:?}: {:?}",
                        update.keys,
                        update.errors
                    );
                }
                Message::CompConfig(Box::new(update.config))
            },
        );

        let ime = match &self.active_command {
            Some(cmd) => ime_subscription(cmd.clone()),
            None => Subscription::none(),
        };

        Subscription::batch([config, ime])
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

/// Bus id for the active IME (`chewingwl` / `pinyinwl`), never a full path.
fn active_ime_bus_id(cfg: &CosmicCompConfig) -> Option<String> {
    let layout = cfg.active_layout.as_str();
    if layout.is_empty() {
        return None;
    }
    let entry = cfg.input_method_map.get(layout)?;

    let raw = if !entry.app_id.is_empty() {
        entry.app_id.as_str()
    } else {
        entry.command.as_str()
    };
    let id = ime_control::bus_id_from(raw);
    if id.is_empty() { None } else { Some(id) }
}

fn ime_subscription(command: String) -> Subscription<Message> {
    Subscription::run_with(command, |command| {
        let command = command.clone();
        stream::channel(8, move |mut output| async move {
            loop {
                if let Err(e) = watch_ime(&command, &mut output).await {
                    tracing::debug!("IME watch ended ({command}): {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        })
    })
}

async fn watch_ime(
    command: &str,
    output: &mut futures::channel::mpsc::Sender<Message>,
) -> zbus::Result<()> {
    use futures::SinkExt;

    let bus = ime_control::bus_name_for_command(command);
    let path = ime_control::object_path_for_command(command);
    let conn = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&conn, bus.as_str(), path.as_str(), INTERFACE).await?;

    let label: String = proxy.get_property("ModeLabel").await?;
    let _ = output.send(Message::ModeLabel(label)).await;

    let items: Vec<(String, String, bool, bool)> = proxy.get_property("MenuItems").await?;
    let _ = output
        .send(Message::Menu(
            items.into_iter().map(MenuItem::from_tuple).collect(),
        ))
        .await;

    if let Ok(cmd) = proxy.get_property::<String>("SettingsCommand").await {
        let _ = output.send(Message::SettingsCommand(cmd)).await;
    }

    let mut label_changes = proxy.receive_property_changed::<String>("ModeLabel").await;
    let mut menu_changes = proxy
        .receive_property_changed::<Vec<(String, String, bool, bool)>>("MenuItems")
        .await;
    let mut settings_changes = proxy
        .receive_property_changed::<String>("SettingsCommand")
        .await;

    loop {
        tokio::select! {
            change = label_changes.next() => {
                let Some(change) = change else { break };
                if let Ok(label) = change.get().await {
                    let _ = output.send(Message::ModeLabel(label)).await;
                }
            }
            change = menu_changes.next() => {
                let Some(change) = change else { break };
                if let Ok(items) = change.get().await {
                    let _ = output
                        .send(Message::Menu(items.into_iter().map(MenuItem::from_tuple).collect()))
                        .await;
                }
            }
            change = settings_changes.next() => {
                let Some(change) = change else { break };
                if let Ok(cmd) = change.get().await {
                    let _ = output.send(Message::SettingsCommand(cmd)).await;
                }
            }
        }
    }
    Ok(())
}

async fn dbus_activate(command: &str, id: &str) -> zbus::Result<()> {
    let bus = ime_control::bus_name_for_command(command);
    let path = ime_control::object_path_for_command(command);
    let conn = zbus::Connection::session().await?;
    conn.call_method(
        Some(bus.as_str()),
        path.as_str(),
        Some(INTERFACE),
        "Activate",
        &id,
    )
    .await?;
    Ok(())
}

fn launch_settings_fallback(settings_command: &str) {
    let mut parts = settings_command.split_whitespace();
    let Some(bin) = parts.next() else {
        return;
    };
    let args: Vec<&str> = parts.collect();
    match std::process::Command::new(bin).args(args).spawn() {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => tracing::error!("Failed to launch settings ({settings_command}): {e}"),
    }
}
