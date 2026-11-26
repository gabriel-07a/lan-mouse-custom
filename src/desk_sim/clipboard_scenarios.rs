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

/// A folder and a file copied on A (what a file manager puts on the
/// clipboard: their paths) are pasted on B as files: extracted on B with
/// their contents and structure, B's clipboard pointing at B's copies. Writing
/// them must not bounce back to A as a new copy.
#[tokio::test(flavor = "current_thread")]
async fn copied_files_and_folders_paste_on_the_other_pc() {
    use input_clipboard::files::{local_item, local_paths};
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            let src =
                std::env::temp_dir().join(format!("lan-mouse-desk-files-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&src);
            std::fs::create_dir_all(src.join("Relatório/2026")).unwrap();
            std::fs::write(src.join("Relatório/2026/notas.txt"), "olá do A").unwrap();
            let png = fake_png(200 * 1024);
            std::fs::write(src.join("foto.png"), &png).unwrap();
            let copied = vec![src.join("Relatório"), src.join("foto.png")];
            let on_a = ClipboardSnapshot::from_items([(Format::Files, local_item(&copied))]);
            desk.copy("A", on_a.clone());

            desk.wait_until("B pastes the files", Duration::from_secs(20), |desk| {
                desk.paste("B").get(Format::Files).is_some()
            })
            .await;
            let pasted = desk.paste("B");
            let paths = local_paths(pasted.get(Format::Files).expect("files"));
            assert_eq!(paths.len(), 2, "{paths:?}");
            assert!(
                paths.iter().all(|p| !p.starts_with(&src)),
                "B points at its own copies"
            );
            assert!(paths[0].ends_with("Relatório") && paths[1].ends_with("foto.png"));
            assert_eq!(
                std::fs::read_to_string(paths[0].join("2026/notas.txt")).unwrap(),
                "olá do A"
            );
            assert_eq!(std::fs::read(&paths[1]).unwrap(), png);
            // plain-text paste gives B's paths
            let text = String::from_utf8(pasted.get(Format::Text).expect("text").to_vec()).unwrap();
            assert_eq!(text.lines().count(), 2);

            // no echo: A keeps its own copy
            tokio::time::sleep(Duration::from_millis(600)).await;
            assert_eq!(desk.paste("A"), on_a, "{}", desk.report());
            let _ = std::fs::remove_dir_all(&src);
        })
        .await;
}

/// How fast a large copied file reaches the other PC (MB/s), and that it
/// arrives intact.
#[tokio::test(flavor = "current_thread")]
async fn a_large_copied_file_arrives_quickly() {
    use input_clipboard::files::{local_item, local_paths};
    init_log();
    LocalSet::new()
        .run_until(async {
            let desk = a_with_b_on_the_right().await;
            let src =
                std::env::temp_dir().join(format!("lan-mouse-desk-bigfile-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&src);
            std::fs::create_dir_all(&src).unwrap();
            // incompressible-looking bytes
            let size: usize = std::env::var("BIG_FILE_KIB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8 * 1024)
                * 1024;
            let data: Vec<u8> = (0..size as u64)
                .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
                .collect();
            std::fs::write(src.join("video.bin"), &data).unwrap();
            let started = std::time::Instant::now();
            desk.copy(
                "A",
                ClipboardSnapshot::from_items([(
                    Format::Files,
                    local_item(&[src.join("video.bin")]),
                )]),
            );
            desk.wait_until("B pastes the big file", Duration::from_secs(120), |desk| {
                desk.paste("B").get(Format::Files).is_some()
            })
            .await;
            let took = started.elapsed();
            let mbps = size as f64 / 1e6 / took.as_secs_f64();
            eprintln!("{} KiB file: {took:?} ({mbps:.2} MB/s)", size / 1024);
            // 3.8 MB/s on a developer machine; 0.8 MB/s before the wider send
            // window. Loose, so a slow CI runner does not flake.
            assert!(mbps > 1.2, "a large copy crawls at {mbps:.2} MB/s");
            let pasted = desk.paste("B");
            let paths = local_paths(pasted.get(Format::Files).expect("files"));
            assert_eq!(
                std::fs::read(&paths[0]).unwrap(),
                data,
                "the file arrived intact"
            );
            let _ = std::fs::remove_dir_all(&src);
        })
        .await;
}
