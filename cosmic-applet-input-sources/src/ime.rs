// SPDX-License-Identifier: GPL-3.0-only
//! Control1 client: discover active IME and mirror ModeLabel / MenuItems.

use cosmic::iced::{Subscription, stream};
use cosmic_comp_config::CosmicCompConfig;
use futures::StreamExt;
use ime_control::{INTERFACE, MenuItem};

use crate::Message;

/// Bus id for an IME mapped to `layout` (`chewingwl` / `pinyinwl`), if any.
///
/// Uses `app_id` only — cosmic-session launches IMEs by `app_id`.
pub fn ime_bus_id_for_layout(cfg: &CosmicCompConfig, layout: &str) -> Option<String> {
    if layout.is_empty() {
        return None;
    }
    let entry = cfg.input_method_map.get(layout)?;
    if entry.app_id.is_empty() {
        return None;
    }
    let id = ime_control::bus_id_from(&entry.app_id);
    if id.is_empty() { None } else { Some(id) }
}

pub fn subscription(command: String) -> Subscription<Message> {
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
    let _ = output.send(Message::ImeModeLabel(label)).await;

    let items: Vec<(String, String, bool, bool)> = proxy.get_property("MenuItems").await?;
    let _ = output
        .send(Message::ImeMenu(
            items.into_iter().map(MenuItem::from_tuple).collect(),
        ))
        .await;

    let mut label_changes = proxy.receive_property_changed::<String>("ModeLabel").await;
    let mut menu_changes = proxy
        .receive_property_changed::<Vec<(String, String, bool, bool)>>("MenuItems")
        .await;

    loop {
        tokio::select! {
            change = label_changes.next() => {
                let Some(change) = change else { break };
                if let Ok(label) = change.get().await {
                    let _ = output.send(Message::ImeModeLabel(label)).await;
                }
            }
            change = menu_changes.next() => {
                let Some(change) = change else { break };
                if let Ok(items) = change.get().await {
                    let _ = output
                        .send(Message::ImeMenu(items.into_iter().map(MenuItem::from_tuple).collect()))
                        .await;
                }
            }
        }
    }
    Ok(())
}

pub async fn dbus_activate(command: &str, id: &str) -> zbus::Result<()> {
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
