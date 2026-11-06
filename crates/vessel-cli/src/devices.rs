//! `vessel node pair` and `vessel node devices` (ADR 0010).
//!
//! [Decision Log]
//! - `node pair` works over any connection that holds the `admin` scope:
//!   the node's own token, SSH, or an already paired device. The node makes
//!   the link; the CLI only shows it.
//! - The link goes to stdout, alone on its line, so it can be piped or
//!   copied; the QR code (for a phone, later) only when stdout is a
//!   terminal, and the instructions on stderr. The link holds a single-use
//!   secret: it is printed for the person pairing and never logged.
//! - The QR code is drawn with Unicode half blocks, light modules as
//!   blocks, so it reads right on the usual dark terminal background.
//! - `devices revoke` takes an id or a unique prefix of one, as sessions do.

use std::fmt::Write as _;
use std::io::IsTerminal as _;

use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use vessel_proto::DeviceInfo;

use crate::args::{DevicesCommand, Target};
use crate::connect::connect;
use crate::error::CliError;
use crate::io;

/// `vessel node pair`.
pub(crate) async fn pair(target: &Target, label: Option<String>, ttl: u32) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    let invitation = client.invite_device(label, ttl).await?;
    let link = invitation.link.expose();
    let mut text = format!("{link}\n");
    if std::io::stdout().is_terminal() {
        match qr(link) {
            Some(code) => text.push_str(&code),
            None => io::note("the link is too long for a QR code; copy it instead\n").await?,
        }
    }
    io::out(text.as_bytes()).await?;
    io::note(&format!(
        "valid once, until {}; on the new device run\n  vessel nodes add NAME --pair\n\
         and paste the link at its prompt (never put it on a command line)\n",
        invitation.expires.format("%Y-%m-%d %H:%M:%S UTC")
    ))
    .await?;
    Ok(())
}

/// The QR code of `text`, or `None` if it does not fit one.
fn qr(text: &str) -> Option<String> {
    let code = QrCode::new(text.as_bytes()).ok()?;
    let drawn = code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    Some(format!("{drawn}\n"))
}

/// `vessel node devices [revoke ID]`.
pub(crate) async fn devices(
    target: &Target,
    command: Option<DevicesCommand>,
) -> Result<(), CliError> {
    let client = connect(target).await?.client;
    let devices = client.devices().await?;
    match command {
        None => Ok(io::out(list(&devices).as_bytes()).await?),
        Some(DevicesCommand::Revoke { device }) => {
            let chosen = find(&devices, &device)?;
            client.revoke_device(chosen.id).await?;
            io::note(&format!(
                "revoked {} ({}); its open connections are closed\n",
                chosen.label, chosen.id
            ))
            .await?;
            Ok(())
        }
    }
}

/// The device whose id is `text` or starts with it.
fn find<'a>(devices: &'a [DeviceInfo], text: &str) -> Result<&'a DeviceInfo, CliError> {
    let wanted = text.trim().to_ascii_lowercase();
    let matches: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|device| !wanted.is_empty() && device.id.to_string().starts_with(&wanted))
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(CliError::NoSuchDevice(text.to_owned())),
        several => Err(CliError::AmbiguousDevice {
            prefix: text.to_owned(),
            count: several.len(),
        }),
    }
}

fn list(devices: &[DeviceInfo]) -> String {
    if devices.is_empty() {
        return "no paired devices; make a link with `vessel node pair`\n".to_owned();
    }
    let width = devices
        .iter()
        .map(|device| device.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(5);
    let mut text = format!(
        "{:<8}  {:<width$}  {:<16}  {:<16}  STATE\n",
        "ID", "LABEL", "PAIRED", "LAST SEEN"
    );
    for device in devices {
        let id = device.id.to_string();
        let paired = device.created_at.format(MINUTE).to_string();
        let seen = device
            .last_seen
            .map_or_else(|| "-".to_owned(), |at| at.format(MINUTE).to_string());
        let state = device.revoked_at.map_or_else(
            || "active".to_owned(),
            |at| format!("revoked {}", at.format(MINUTE)),
        );
        let _infallible = writeln!(
            text,
            "{:<8}  {:<width$}  {paired:<16}  {seen:<16}  {state}",
            id.get(..8).unwrap_or(&id),
            device.label,
        );
    }
    text
}

const MINUTE: &str = "%Y-%m-%d %H:%M";

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use vessel_proto::{DeviceId, Scope};

    use super::*;

    fn device(id: &str, label: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.parse::<DeviceId>().unwrap(),
            label: label.to_owned(),
            created_at: DateTime::UNIX_EPOCH,
            last_seen: None,
            scopes: Scope::ALL.to_vec(),
            revoked_at: Some(DateTime::UNIX_EPOCH),
        }
    }

    #[test]
    fn devices_are_named_by_a_unique_id_prefix_and_listed_with_their_state() {
        let devices = [
            device("3fa9c2d1-0000-4000-8000-000000000001", "laptop"),
            device("3fa9e000-0000-4000-8000-000000000002", "phone"),
        ];
        assert_eq!(find(&devices, "3FA9C").unwrap().label, "laptop");
        assert!(matches!(
            find(&devices, "3fa9"),
            Err(CliError::AmbiguousDevice { count: 2, .. })
        ));
        assert!(matches!(
            find(&devices, "ff"),
            Err(CliError::NoSuchDevice(_))
        ));
        assert!(matches!(find(&devices, ""), Err(CliError::NoSuchDevice(_))));

        let text = list(&devices);
        assert!(text.starts_with("ID "), "{text}");
        assert!(text.contains("3fa9c2d1  laptop"), "{text}");
        assert!(text.contains("revoked 1970-01-01 00:00"), "{text}");
        assert!(list(&[]).contains("vessel node pair"));
    }

    #[test]
    fn a_link_fits_a_qr_code() {
        let code = qr("vessel://pair#eyJ2IjoxfQ").unwrap();
        assert!(code.contains('▀') || code.contains('▄') || code.contains('█'));
    }
}
