//! Composer. Editorial form: no surrounding frame; a hairline rule under
//! the input field; primary "SEND" button to the right. The token meter
//! lives in the bottom status bar — there is no duplicate beneath the
//! composer. When a ticket has been written this turn, the ticket id is
//! shown on a single tracked-caps line under the field.
//!
//! Image input:
//!   * Paperclip button -> native file picker
//!   * Drop image files anywhere in the chat panel
//!   * Ctrl+V pastes a clipboard image
//! Attached but not yet sent images sit on a chip strip above the field.
//! Pressing Esc while a turn is streaming sends `InterruptTurn` to stop it.

use std::path::Path;

use base64::Engine as _;
use egui::{Stroke, Vec2};

use protocol::Request;

use crate::app::{rgb, App, PendingAttachment};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{caps_label, ghost_button, primary_button_enabled};

const SEND_BUTTON_W: f32 = 84.0;
const ATTACH_BUTTON_W: f32 = 36.0;
const THUMB_SIZE: f32 = 56.0;
/// 16 MB raw bytes — guard against accidentally attaching a video file etc.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

pub fn draw(app: &mut App, ui: &mut egui::Ui, disabled: bool) {
    handle_dropped_files(app, ui);
    handle_paste(app, ui);
    handle_escape(app, ui);

    let p = app.palette;

    // Thumbnail strip for pending attachments (above the input row).
    if !app.chat.pending_images.is_empty() {
        draw_pending_strip(app, ui);
        ui.add_space(6.0);
    }

    let turn_in_flight = last_turn_in_flight(app);

    ui.horizontal(|ui| {
        let spacing = ui.spacing().item_spacing.x;
        let input_w = (ui.available_width()
            - SEND_BUTTON_W
            - ATTACH_BUTTON_W
            - spacing * 2.0)
            .max(80.0);

        // Paperclip button to the left of the input field.
        let attach_resp = ghost_button(ui, &p, "📎");
        if attach_resp.clicked() && !disabled {
            pick_file_and_attach(app);
        }
        attach_resp.on_hover_text("Attach image");

        let input = egui::TextEdit::multiline(&mut app.chat.draft)
            .hint_text(if disabled { "(disabled — connect an LLM)" } else { "Type a message…" })
            .desired_width(input_w)
            .desired_rows(2)
            .frame(false);
        let resp = ui.add_enabled(!disabled, input);

        // Hairline directly under the input rect — replaces the egui chrome.
        ui.painter().hline(
            resp.rect.x_range(),
            resp.rect.bottom() + 2.0,
            Stroke::new(1.0, if resp.has_focus() { rgb(p.accent) } else { rgb(p.hairline) }),
        );

        // While a turn streams, swap the SEND button for STOP. Same slot so
        // the cursor doesn't have to hunt.
        if turn_in_flight {
            let stop_resp = primary_button_enabled(ui, &p, "Stop", true);
            if stop_resp.clicked() {
                let session_id = app.chat.session_id;
                app.send(UiCommand::SendRequest(Request::InterruptTurn { session_id }));
            }
        } else {
            let send_enabled = !disabled
                && (!app.chat.draft.trim().is_empty() || !app.chat.pending_images.is_empty());
            let send_resp = primary_button_enabled(ui, &p, "Send", send_enabled);
            let send = send_resp.clicked();
            let submit_via_keys = !disabled
                && resp.has_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.ctrl);
            if (send || submit_via_keys) && send_enabled {
                send_message(app);
            }
        }
    });

    if let Some(ticket) = app.chat.idealist.last_ticket.clone() {
        ui.add_space(8.0);
        caps_label(ui, &format!("TICKET {ticket}"), rgb(p.muted));
    }
}

/// Build the outgoing `SendUserMessage`, draining `pending_images`.
fn send_message(app: &mut App) {
    let text = std::mem::take(&mut app.chat.draft);
    let attachments = std::mem::take(&mut app.chat.pending_images);
    let images = attachments.iter().map(PendingAttachment::to_user_image).collect::<Vec<_>>();
    let history_images = attachments
        .iter()
        .map(|a| crate::app::Attachment {
            mime: a.mime.clone(),
            data_base64: a.data_base64.clone(),
            texture: None,
        })
        .collect::<Vec<_>>();

    if text.trim().is_empty() && images.is_empty() {
        return;
    }

    let session_id = app.chat.session_id;
    app.chat.turns.push(crate::app::Turn {
        session_id,
        turn_id: 0,
        user: text.clone(),
        assistant: String::new(),
        reasoning: String::new(),
        finished: false,
        finish_reason: None,
        tool_chips: Vec::new(),
        reasoning_collapsed: false,
        images: history_images,
    });
    app.chat.scroll_to_bottom = true;
    app.send(UiCommand::SendRequest(Request::SendUserMessage {
        session_id,
        text,
        images,
    }));
}

/// Draw a horizontal strip of small thumbnails for the pending attachments,
/// each with an `×` button that removes the entry.
fn draw_pending_strip(app: &mut App, ui: &mut egui::Ui) {
    let p = app.palette;
    let ctx = ui.ctx().clone();
    ui.horizontal_wrapped(|ui| {
        let mut remove_idx: Option<usize> = None;
        for (i, att) in app.chat.pending_images.iter_mut().enumerate() {
            ui.vertical(|ui| {
                let tex = ensure_texture(&ctx, &mut att.texture, &att.mime, &att.data_base64, i);
                match tex {
                    Some(handle) => {
                        let size = fit_thumb_size(handle.size_vec2());
                        ui.image((handle.id(), size));
                    }
                    None => {
                        // Could not decode (corrupt file or unsupported format) —
                        // fall back to a small placeholder so the chip still
                        // surfaces a remove button.
                        let (rect, _) = ui.allocate_exact_size(
                            Vec2::new(THUMB_SIZE, THUMB_SIZE),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 2.0, rgb(p.surface_sunk));
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "!",
                            egui::FontId::monospace(20.0),
                            rgb(p.muted),
                        );
                    }
                }
                ui.horizontal(|ui| {
                    let label = short_filename(&att.filename, 14);
                    ui.label(
                        egui::RichText::new(label)
                            .color(rgb(p.muted))
                            .small(),
                    );
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        remove_idx = Some(i);
                    }
                });
            });
            ui.add_space(6.0);
        }
        if let Some(i) = remove_idx {
            app.chat.pending_images.remove(i);
        }
    });
}

fn fit_thumb_size(natural: Vec2) -> Vec2 {
    if natural.x <= 0.0 || natural.y <= 0.0 {
        return Vec2::new(THUMB_SIZE, THUMB_SIZE);
    }
    let scale = (THUMB_SIZE / natural.x).min(THUMB_SIZE / natural.y);
    Vec2::new(natural.x * scale, natural.y * scale)
}

fn short_filename(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.into();
    }
    let head: String = name.chars().take(max - 1).collect();
    format!("{head}…")
}

/// Load `data_base64` as an image and upload it as an egui texture on first
/// render. Caches the handle in-place so subsequent frames are cheap. Returns
/// `None` when the bytes can't be decoded.
pub fn ensure_texture(
    ctx: &egui::Context,
    slot: &mut Option<egui::TextureHandle>,
    mime: &str,
    data_base64: &str,
    nonce: usize,
) -> Option<egui::TextureHandle> {
    if let Some(h) = slot {
        return Some(h.clone());
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64)
        .ok()?;
    let img = image::load_from_memory(&bytes).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw());
    // The texture name only needs to be unique-ish — caller passes a nonce so
    // multiple thumbs in one strip don't collide.
    let handle = ctx.load_texture(
        format!("attach-{mime}-{nonce}-{}", data_base64.len()),
        color,
        Default::default(),
    );
    *slot = Some(handle.clone());
    Some(handle)
}

/// Open a native file picker for one image, base64-encode the bytes, and push
/// onto `pending_images`. Runs synchronously on the UI thread — egui frames
/// pause for the dialog, which is fine because the user is actively choosing.
fn pick_file_and_attach(app: &mut App) {
    let picked = rfd::FileDialog::new()
        .add_filter("Images", &["png", "jpg", "jpeg", "webp", "gif", "bmp"])
        .pick_file();
    let Some(path) = picked else { return };
    if let Err(e) = attach_from_path(app, &path) {
        app.push_log(crate::app::LogKind::Error, format!("attach failed: {e}"));
    }
}

fn attach_from_path(app: &mut App, path: &Path) -> std::io::Result<()> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("file too large ({} bytes); cap is {}", bytes.len(), MAX_IMAGE_BYTES),
        ));
    }
    let mime = mime_from_path(path).unwrap_or("application/octet-stream").to_string();
    if !mime.starts_with("image/") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("not an image: {}", path.display()),
        ));
    }
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    let filename = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    app.chat.pending_images.push(PendingAttachment {
        mime,
        data_base64,
        filename,
        size_bytes: bytes.len(),
        texture: None,
    });
    Ok(())
}

fn mime_from_path(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|s| s.to_str()).map(|s| s.to_ascii_lowercase()) {
        Some(ext) => Some(match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "webp" => "image/webp",
            "gif" => "image/gif",
            "bmp" => "image/bmp",
            _ => return None,
        }),
        None => None,
    }
}

/// Drain `raw.dropped_files` once per frame and treat any local image path as
/// an attachment. Non-image paths and remote-only URL drops are ignored.
fn handle_dropped_files(app: &mut App, ui: &mut egui::Ui) {
    let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
    for f in dropped {
        if let Some(path) = f.path.as_ref() {
            if let Err(e) = attach_from_path(app, path) {
                app.push_log(crate::app::LogKind::Error, format!("drop attach failed: {e}"));
            }
        }
    }
}

/// On Ctrl+V, query the system clipboard for an image and, if present, encode
/// it as PNG and attach. Text paste continues to flow through egui's default
/// TextEdit handler.
fn handle_paste(app: &mut App, ui: &mut egui::Ui) {
    let pressed = ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::V));
    if !pressed {
        return;
    }
    // arboard returns RGBA8 raw bytes; encode to PNG so the LLM gets a known
    // format. `Clipboard::new()` opens a transient handle and drops at the end
    // of this block.
    let mut clip = match arboard::Clipboard::new() {
        Ok(c) => c,
        Err(_) => return,
    };
    let img = match clip.get_image() {
        Ok(i) => i,
        Err(_) => return,
    };
    let w = img.width as u32;
    let h = img.height as u32;
    let raw = img.bytes.into_owned();
    let buf = match image::RgbaImage::from_raw(w, h, raw) {
        Some(b) => b,
        None => return,
    };
    let mut png: Vec<u8> = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut png);
    if image::DynamicImage::ImageRgba8(buf)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .is_err()
    {
        return;
    }
    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&png);
    app.chat.pending_images.push(PendingAttachment {
        mime: "image/png".into(),
        data_base64,
        filename: "clipboard.png".into(),
        size_bytes: png.len(),
        texture: None,
    });
}

/// Esc cancels the in-flight turn. Only fires when the last turn is still
/// streaming, so Esc inside the input field doesn't fight TextEdit's own
/// behaviour (TextEdit doesn't consume Esc by default in egui).
fn handle_escape(app: &mut App, ui: &mut egui::Ui) {
    let pressed = ui.input(|i| i.key_pressed(egui::Key::Escape));
    if !pressed {
        return;
    }
    if last_turn_in_flight(app) {
        let session_id = app.chat.session_id;
        app.send(UiCommand::SendRequest(Request::InterruptTurn { session_id }));
    }
}

fn last_turn_in_flight(app: &App) -> bool {
    app.chat.turns.last().map(|t| !t.finished).unwrap_or(false)
}
