// Copyright 2024 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

mod ime;
mod localize;

use cctk::{
    cosmic_protocols::keyboard_layout::v1::client::zcosmic_keyboard_layout_v1::ZcosmicKeyboardLayoutV1,
    wayland_client::{Connection, Proxy, backend::Backend},
};
use cosmic::{
    app,
    app::Core,
    applet::{
        self, column as applet_column, cosmic_panel_config::PanelAnchor, row as applet_row,
    },
    cosmic_theme::Spacing,
    iced::Subscription,
    iced::core::window,
    iced::{
        self, Alignment, Length, Rectangle, Task,
        event::wayland::{Event as WaylandEvent, OutputEvent},
        window::Id,
    },
    prelude::*,
    surface, theme,
    widget::{
        self, autosize,
        rectangle_tracker::{RectangleTracker, RectangleUpdate, rectangle_tracker_subscription},
    },
};
use cosmic_comp_config::{CosmicCompConfig, XkbConfig};
use ime_control::MenuItem;
use std::{
    os::unix::{
        io::{FromRawFd, RawFd},
        net::UnixStream,
    },
    sync::LazyLock,
};
use xkb_data::KeyboardLayout;

mod wayland;

static AUTOSIZE_MAIN_ID: LazyLock<widget::Id> = LazyLock::new(|| widget::Id::new("autosize-main"));
pub const ID: &str = "com.system76.CosmicAppletInputSources";

pub fn run() -> cosmic::iced::Result {
    let socket = std::env::var("X_PRIVILEGED_WAYLAND_SOCKET")
        .ok()
        .and_then(|fd| {
            fd.parse::<RawFd>()
                .ok()
                .map(|fd| unsafe { UnixStream::from_raw_fd(fd) })
        });
    let wayland_connection = if let Some(socket) = socket {
        Some(Connection::from_socket(socket).unwrap())
    } else {
        None
    };

    localize::localize();

    let layouts = match xkb_data::all_keyboard_layouts() {
        Ok(layouts) => layouts,
        Err(why) => {
            tracing::error!("could not get keyboard layouts data: {:?}", why);
            return Ok(());
        }
    };

    cosmic::applet::run::<Window>(Flags {
        wayland_connection,
        layouts: layouts.layout_list.layout,
    })
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ActiveLayout {
    layout: String,
    description: String,
    variant: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupKind {
    Layout,
    Ime,
}

pub struct Window {
    core: Core,
    popup: Option<(Id, PopupKind)>,
    layouts: Vec<KeyboardLayout>,
    active_layouts: Vec<ActiveLayout>,
    comp_config: CosmicCompConfig,
    rectangle_tracker: Option<RectangleTracker<u32>>,
    rectangle: Rectangle,
    wayland_connection: Option<Connection>,
    keyboard_layout: Option<ZcosmicKeyboardLayoutV1>,
    current_layout: usize,
    /// Active IME bus id for the *current* layout (`pinyinwl`, `chewingwl`, …).
    active_ime: Option<String>,
    ime_mode_label: String,
    ime_menu: Vec<MenuItem>,
}

#[derive(Clone, Debug)]
pub enum Message {
    ToggleLayoutPopup,
    ToggleImePopup,
    PopupClosed(Id),
    CompConfig(Box<CosmicCompConfig>),
    SetActiveLayout(usize),
    KeyboardSettings,
    Surface(surface::Action<Message>),
    Rectangle(RectangleUpdate<u32>),
    WaylandConnection(Backend),
    Wayland(wayland::Event),
    ImeModeLabel(String),
    ImeMenu(Vec<MenuItem>),
    ImeActivate(String),
}

#[derive(Debug)]
pub struct Flags {
    wayland_connection: Option<Connection>,
    pub layouts: Vec<KeyboardLayout>,
}

impl cosmic::Application for Window {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = Flags;
    type Message = Message;

    const APP_ID: &'static str = ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let window = Window {
            layouts: flags.layouts,
            core,
            popup: None,
            active_layouts: Vec::new(),
            comp_config: CosmicCompConfig::default(),
            rectangle_tracker: None,
            rectangle: Rectangle::default(),
            wayland_connection: flags.wayland_connection,
            keyboard_layout: None,
            current_layout: 0,
            active_ime: None,
            ime_mode_label: String::new(),
            ime_menu: Vec::new(),
        };
        (window, Task::none())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::PopupClosed(id))
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::ToggleLayoutPopup => {
                return self.toggle_popup(PopupKind::Layout);
            }
            Message::ToggleImePopup => {
                if self.active_ime.is_none() {
                    return Task::none();
                }
                return self.toggle_popup(PopupKind::Ime);
            }
            Message::PopupClosed(id) => {
                if self.popup.as_ref().is_some_and(|(p, _)| *p == id) {
                    self.popup = None;
                }
            }
            Message::CompConfig(config) => {
                self.active_layouts = self.update_xkb(&config.xkb_config);
                self.comp_config = *config;
                // Prefer config active_layout index when XKB group has not caught up.
                if let Some(pos) = self
                    .active_layouts
                    .iter()
                    .position(|l| l.layout == self.comp_config.active_layout)
                {
                    self.current_layout = pos;
                }
                return self.refresh_active_ime();
            }
            Message::KeyboardSettings => {
                let mut cmd = std::process::Command::new("cosmic-settings");
                cmd.arg("keyboard");
                tokio::spawn(cosmic::process::spawn(cmd));
            }
            Message::SetActiveLayout(pos) => {
                if let Some(keyboard_layout) = &self.keyboard_layout {
                    keyboard_layout.set_group(pos as u32);
                    if let Some(backend) = keyboard_layout.backend().upgrade() {
                        let _ = backend.flush();
                    }
                }
                // cosmic-comp SetGroup syncs the IME and persists active_layout.
                self.current_layout = pos;
                let close = self.take_popup_destroy();
                let refresh = self.refresh_active_ime();
                return close.chain(refresh);
            }
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Surface(a));
            }
            Message::Rectangle(u) => match u {
                RectangleUpdate::Rectangle(r) => {
                    self.rectangle = r.1;
                }
                RectangleUpdate::Init(tracker) => {
                    self.rectangle_tracker = Some(tracker);
                }
            },
            Message::WaylandConnection(backend) => {
                if self.wayland_connection.is_none() {
                    self.wayland_connection = Some(Connection::from_backend(backend));
                }
            }
            Message::Wayland(w) => match w {
                wayland::Event::KeyboardLayout(keyboard_layout) => {
                    self.keyboard_layout = Some(keyboard_layout);
                }
                wayland::Event::Group(group) => {
                    self.current_layout = group as usize;
                    return self.refresh_active_ime();
                }
            },
            Message::ImeModeLabel(text) => {
                self.ime_mode_label = text;
            }
            Message::ImeMenu(items) => {
                self.ime_menu = items;
            }
            Message::ImeActivate(id) => {
                let Some(command) = self.active_ime.clone() else {
                    return Task::none();
                };
                tokio::spawn(async move {
                    if let Err(e) = ime::dbus_activate(&command, &id).await {
                        tracing::warn!("Activate({id}) failed: {e}");
                    }
                });
                return self.take_popup_destroy();
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let layout_btn: Element<'_, Self::Message> = self
            .core
            .applet
            .text_button(
                self.core.applet.text(self.layout_button_text()),
                Message::ToggleLayoutPopup,
            )
            .into();

        let overlap = -(self.core.applet.suggested_padding(true).0 as f32)
            * self.core.applet.padding_overlap;

        // Always two buttons (IME left, layout right). A zero-width IME placeholder
        // keeps iced widget state stable when the IME control appears/disappears;
        // swapping Button <-> Row crashes with "Downcast widget state".
        let ime_btn = if self.active_ime.is_some() {
            let mode = if self.ime_mode_label.is_empty() {
                "—"
            } else {
                self.ime_mode_label.as_str()
            };
            self.core.applet.text_button(
                self.core.applet.text(mode.to_string()),
                Message::ToggleImePopup,
            )
        } else {
            self.core
                .applet
                .text_button(self.core.applet.text(""), Message::ToggleImePopup)
                .padding(0)
                .width(Length::Fixed(0.0))
        };

        let children = [ime_btn.into(), layout_btn];
        let content: Element<'_, Self::Message> = match self.core.applet.anchor {
            PanelAnchor::Left | PanelAnchor::Right => applet_column::Column::with_children(children)
                .align_x(Alignment::Center)
                .height(Length::Shrink)
                .spacing(overlap)
                .into(),
            PanelAnchor::Top | PanelAnchor::Bottom => applet_row::Row::with_children(children)
                .align_y(Alignment::Center)
                .width(Length::Shrink)
                .spacing(overlap)
                .into(),
        };

        autosize::autosize(
            if let Some(tracker) = self.rectangle_tracker.as_ref() {
                Element::from(tracker.container(0, content).ignore_bounds(true))
            } else {
                content
            },
            AUTOSIZE_MAIN_ID.clone(),
        )
        .into()
    }

    fn view_window(&self, id: Id) -> Element<'_, Self::Message> {
        let kind = self
            .popup
            .as_ref()
            .filter(|(p, _)| *p == id)
            .map(|(_, k)| *k)
            .unwrap_or(PopupKind::Layout);

        match kind {
            PopupKind::Layout => self.layout_popup(),
            PopupKind::Ime => self.ime_popup(),
        }
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subscriptions = vec![
            rectangle_tracker_subscription(0).map(|e| Message::Rectangle(e.1)),
            self.core
                .watch_config("com.system76.CosmicComp")
                .map(|update| {
                    if !update.errors.is_empty() {
                        tracing::error!(
                            "errors loading config {:?}: {:?}",
                            update.keys,
                            update.errors
                        );
                    }
                    Message::CompConfig(Box::new(update.config))
                }),
        ];
        subscriptions.push(if let Some(connection) = &self.wayland_connection {
            wayland::subscription(connection.clone()).map(Message::Wayland)
        } else {
            iced::event::listen_with(|evt, _, _| match evt {
                iced::Event::PlatformSpecific(iced::event::PlatformSpecific::Wayland(evt)) => {
                    if let WaylandEvent::Output(OutputEvent::Created(_), output) = evt {
                        if let Some(backend) = output.backend().upgrade() {
                            return Some(Message::WaylandConnection(backend));
                        }
                    }
                    None
                }
                _ => None,
            })
        });
        if let Some(cmd) = &self.active_ime {
            subscriptions.push(ime::subscription(cmd.clone()));
        }
        Subscription::batch(subscriptions)
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

impl Window {
    fn layout_button_text(&self) -> String {
        let Some(l) = self.active_layouts.get(self.current_layout) else {
            return String::new();
        };
        if let Some(name) = self
            .comp_config
            .input_method_map
            .get(&l.layout)
            .map(|e| e.label.as_str())
            .filter(|s| !s.is_empty())
        {
            return name.to_string();
        }
        if !l.variant.is_empty() {
            format!("{} ({})", l.layout, l.variant)
        } else {
            l.layout.clone()
        }
    }

    fn layout_label_code(&self) -> Option<&str> {
        self.active_layouts
            .get(self.current_layout)
            .map(|l| l.layout.as_str())
    }

    /// Bind Control1 to the IME for the *current* XKB layout (not stale config alone).
    fn refresh_active_ime(&mut self) -> app::Task<Message> {
        let cmd = self
            .layout_label_code()
            .and_then(|layout| ime::ime_bus_id_for_layout(&self.comp_config, layout));
        if cmd == self.active_ime {
            return Task::none();
        }
        self.active_ime = cmd;
        self.ime_mode_label.clear();
        self.ime_menu.clear();
        // Close IME popup if we left an IME layout.
        if self.active_ime.is_none()
            && self.popup.as_ref().is_some_and(|(_, k)| *k == PopupKind::Ime)
        {
            return self.take_popup_destroy();
        }
        Task::none()
    }

    fn take_popup_destroy(&mut self) -> app::Task<Message> {
        if let Some((p, _)) = self.popup.take() {
            crate::surface::surface_task(crate::surface::action::destroy_popup(p))
        } else {
            Task::none()
        }
    }

    fn toggle_popup(&mut self, kind: PopupKind) -> app::Task<Message> {
        if let Some((p, open_kind)) = self.popup.take() {
            let destroy = crate::surface::surface_task(crate::surface::action::destroy_popup(p));
            if open_kind == kind {
                return destroy;
            }
            // Switch to the other popup.
            let open = self.open_popup(kind);
            return destroy.chain(open);
        }
        self.open_popup(kind)
    }

    fn open_popup(&mut self, kind: PopupKind) -> app::Task<Message> {
        crate::surface::surface_task(crate::surface::action::app_popup(
            |_| Default::default(),
            move |app: &mut Window| {
                let new_id = Id::unique();
                app.popup.replace((new_id, kind));
                app.core.applet.get_popup_settings(
                    app.core.main_window_id().unwrap(),
                    new_id,
                    None,
                    None,
                    None,
                )
            },
            None,
        ))
    }

    fn layout_popup(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs, space_s, ..
        } = theme::active().cosmic().spacing;

        let mut content_list =
            widget::column::with_capacity(4 + self.active_layouts.len()).padding([8, 0]);
        for (id, layout) in self.active_layouts.iter().enumerate() {
            let font = if id == self.current_layout {
                cosmic::font::bold()
            } else {
                cosmic::font::default()
            };
            let group = widget::column::with_capacity(2)
                .push(widget::text::body(layout.description.as_str()).font(font))
                .push(widget::text::caption(layout.layout.as_str()));
            content_list = content_list
                .push(applet::menu_button(group).on_press(Message::SetActiveLayout(id)));
        }
        if !self.active_layouts.is_empty() {
            content_list = content_list.push(
                applet::padded_control(widget::divider::horizontal::default())
                    .padding([space_xxs, space_s])
                    .apply(Element::from),
            );
        }
        content_list = content_list.push(
            applet::menu_button(widget::text::body(fl!("keyboard-settings")))
                .on_press(Message::KeyboardSettings),
        );

        self.core.applet.popup_container(content_list).into()
    }

    fn ime_popup(&self) -> Element<'_, Message> {
        let mut list =
            widget::column::with_capacity(self.ime_menu.len().max(1)).padding([8, 0]);
        for item in &self.ime_menu {
            let id = item.id.clone();
            let btn = applet::menu_button(widget::text::body(item.label.clone()));
            list = list.push(if item.enabled {
                btn.on_press(Message::ImeActivate(id))
            } else {
                btn
            });
        }
        self.core.applet.popup_container(list).into()
    }

    fn update_xkb(&self, xkb: &XkbConfig) -> Vec<ActiveLayout> {
        let mut active_layouts = Vec::new();

        let layouts = xkb.layout.split_terminator(',');

        let variants = xkb
            .variant
            .split_terminator(',')
            .chain(std::iter::repeat(""));

        'outer: for (layout, variant) in layouts.zip(variants) {
            for xkb_layout in &self.layouts {
                if layout != xkb_layout.name() {
                    continue;
                }

                if variant.is_empty() {
                    let active_layout = ActiveLayout {
                        description: xkb_layout.description().to_owned(),
                        layout: layout.to_owned(),
                        variant: variant.to_owned(),
                    };

                    active_layouts.push(active_layout);
                    continue 'outer;
                }

                let Some(xkb_variants) = xkb_layout.variants() else {
                    continue;
                };

                for xkb_variant in xkb_variants {
                    if variant != xkb_variant.name() {
                        continue;
                    }

                    let active_layout = ActiveLayout {
                        description: xkb_variant.description().to_owned(),
                        layout: layout.to_owned(),
                        variant: variant.to_owned(),
                    };

                    active_layouts.push(active_layout);
                    continue 'outer;
                }
            }
        }

        active_layouts
    }
}
