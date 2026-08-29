//! Showing a pairing invitation on the console.
//!
//! A QR code in the terminal rather than a web page, because the alternative is
//! a chicken-and-egg problem: the pairing page would itself have to be served
//! without authentication, which is exactly the door pairing exists to keep
//! shut. The console is a channel that only someone already on this machine can
//! read, which is the trust level the code needs.

use qrcode::render::unicode;
use qrcode::{EcLevel, QrCode};

/// Print the pairing URL as a scannable QR plus its plain text.
///
/// The text matters as much as the code: a terminal without unicode block
/// support, a session over a serial console, or a phone that will not focus all
/// leave the operator needing to type the code by hand, and a QR that cannot be
/// scanned with no fallback is a dead end.
pub fn print_invitation(url: &str, code: &str) {
    match QrCode::with_error_correction_level(url, EcLevel::L) {
        Ok(qr) => {
            // Half-block rendering: one terminal row per two QR rows, which is
            // what keeps the code square rather than stretched to twice its
            // height by the cell aspect ratio. A stretched QR still scans, but
            // a square one scans from further away.
            let rendered = qr
                .render::<unicode::Dense1x2>()
                .dark_color(unicode::Dense1x2::Light)
                .light_color(unicode::Dense1x2::Dark)
                .quiet_zone(true)
                .build();
            println!("\n{rendered}");
        }
        Err(err) => {
            tracing::warn!("could not render the pairing QR: {err}");
        }
    }
    println!("  Pair a device by scanning the code above, or enter it by hand:");
    println!("      server : {}", url.split("server=").nth(1).unwrap_or(url).split("&code=").next().unwrap_or(url));
    println!("      code   : {code}");
    println!("  The code is good for one device and expires in 10 minutes.\n");
}
