//! Desk scenarios for clipboard sync: Ctrl+C on one PC, Ctrl+V on another.
//!
//! Every PC has an in-memory system clipboard plugged into the real
//! `ClipboardTask`, so a copy travels through the real sync layer (offer,
//! chunked transfer, acknowledgement after apply) over the real DTLS link.

use std::time::Duration;

use input_capture::Position;
use input_clipboard::{ClipboardSnapshot, Format};
use tokio::task::LocalSet;

use super::{Desk, describe_snapshot};

fn init_log() {
    let _ = env_logger::builder()
        .is_test(true)
        .parse_env(env_logger::Env::new().filter("LAN_MOUSE_LOG_LEVEL"))
        .try_init();
}

async fn a_with_b_on_the_right() -> Desk {
    Desk::builder()
        .pc("A", 1920, 1080)
        .pc("B", 1920, 1080)
        .place("B", Position::Right, "A")
        .start()
        .await
}

/// PNG-looking bytes that do not compress (deterministic pseudo-random body).
fn fake_png(len: usize) -> Vec<u8> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut bytes = SIGNATURE.to_vec();
    let mut state: u32 = 0x1234_5678;
    while bytes.len() < len {
        // xorshift32
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn text(value: &str) -> ClipboardSnapshot {
    ClipboardSnapshot::from_items([(Format::Text, value.as_bytes().to_vec())])
}

#[tokio::test(flavor = "current_thread")]
async fn copied_text_pastes_on_the_other_pc_in_both_directions() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;

            desk.copy_text("A", "olá do A — com acentuação ✓");
            desk.wait_paste("B", &text("olá do A — com acentuação ✓"))
                .await;

            // B only has the incoming session from A: the copy still goes back
            desk.copy_text("B", "resposta do B");
            desk.wait_paste("A", &text("resposta do B")).await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn copied_image_pastes_byte_for_byte() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            // ~700 chunks of 1 KiB on the wire
            let png = fake_png(700 * 1024);
            desk.copy_image("A", png.clone());
            let expected = ClipboardSnapshot::from_items([(Format::Png, png)]);
            desk.wait_paste("B", &expected).await;
        })
        .await;
}

/// A copy from a browser carries several representations of the same thing;
/// all of them must arrive together.
#[tokio::test(flavor = "current_thread")]
async fn a_rich_copy_keeps_every_format() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            let rich = ClipboardSnapshot::from_items([
                (Format::Text, b"tabela".to_vec()),
                (
                    Format::Html,
                    b"<table><tr><td>tabela</td></tr></table>".to_vec(),
                ),
                (Format::Png, fake_png(32 * 1024)),
            ]);
            desk.copy("A", rich.clone());
            desk.wait_paste("B", &rich).await;
        })
        .await;
}

/// Copying twice in a row: the second copy is what gets pasted, even if it
/// races the transfer of the first one.
#[tokio::test(flavor = "current_thread")]
async fn the_latest_copy_wins() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            desk.copy_image("A", fake_png(300 * 1024));
            desk.copy_text("A", "segunda cópia");
            desk.wait_paste("B", &text("segunda cópia")).await;
            desk.hold("B keeps the latest copy", Duration::from_millis(500), |d| {
                d.paste("B") == text("segunda cópia")
            })
            .await;
        })
        .await;
}

/// The receiver refuses an image above its own limit (here 64 KiB), keeps
/// what it had, and later copies still go through.
///
/// A small limit keeps the test fast and meaningful: without the check the
/// 100 KiB image would arrive well within the hold window.
#[tokio::test(flavor = "current_thread")]
async fn the_receiver_refuses_an_image_over_its_limit() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = Desk::builder()
                .pc("A", 1920, 1080)
                .pc("B", 1920, 1080)
                .place("B", Position::Right, "A")
                .config("B", "[clipboard]\nmax_image_bytes = 65536")
                .start()
                .await;
            assert_refused_then_recovers(&desk).await;
        })
        .await;
}

/// The sender does not even offer an image above its own limit.
#[tokio::test(flavor = "current_thread")]
async fn the_sender_does_not_offer_an_image_over_its_limit() {
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = Desk::builder()
                .pc("A", 1920, 1080)
                .pc("B", 1920, 1080)
                .place("B", Position::Right, "A")
                .config("A", "[clipboard]\nmax_image_bytes = 65536")
                .start()
                .await;
            assert_refused_then_recovers(&desk).await;
        })
        .await;
}

async fn assert_refused_then_recovers(desk: &Desk) {
    desk.copy_text("A", "antes");
    desk.wait_paste("B", &text("antes")).await;

    desk.copy_image("A", fake_png(100 * 1024));
    desk.hold(
        "B keeps the previous copy",
        Duration::from_millis(800),
        |d| d.paste("B") == text("antes"),
    )
    .await;
    assert_eq!(
        desk.pc("B").clipboard.applied().len(),
        1,
        "only the first copy may be applied on B; B has {}",
        describe_snapshot(&desk.paste("B"))
    );

    // a refused copy must not wedge the sync
    desk.copy_text("A", "depois");
    desk.wait_paste("B", &text("depois")).await;
}
