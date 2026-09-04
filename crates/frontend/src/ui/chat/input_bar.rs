//! Composer. A rounded composer frame holds the TextEdit; its fill
//! and stroke shift on hover/focus to flag the surface as tappable. The
//! field starts at 2 rows and grows with the draft up to `MAX_INPUT_ROWS`,
//! after which it scrolls internally so the Send button never leaves the
//! screen. The primary "SEND" button sits to the right; the token meter
//! lives in the bottom status bar — there is no duplicate beneath the
//! composer. When a ticket has been written this turn, the ticket id is
//! shown on a single tracked-caps line under the field.
//!
//! Submission keys: Enter sends, Shift+Enter inserts a newline. Enter is
//! consumed before the multiline editor sees it so the field doesn't keep
//! a stray line break.
//!
//! A draft that starts with `/` opens the slash palette (see
//! [`super::slash_menu`]), which lists every skill, agent and command in the
//! workspace. While it is open it owns ↑↓/Enter/Tab/Esc, so Enter picks a row
//! instead of sending and Esc closes the list instead of interrupting the turn.
//!
//! Image input:
//!   * Paperclip button -> native file picker
//!   * Drop image files anywhere in the chat panel
//!   * Ctrl+V pastes a clipboard image
//! Attached but not yet sent images sit on a chip strip above the field.
//! Pressing Esc while a turn is streaming sends `InterruptTurn` to stop it.

use std::path::Path;

use base64::Engine as _;
use egui::{Rounding, Stroke, Vec2};

use protocol::Request;

use crate::app::{rgb, App, PendingAttachment};
use crate::supervisor::UiCommand;
use crate::ui::widgets::{caps_label, ghost_button, primary_button_enabled};

const SEND_BUTTON_W: f32 = 84.0;
const ATTACH_BUTTON_W: f32 = 36.0;
/// Height of the empty composer, in rows.
const MIN_INPUT_ROWS: usize = 2;
/// Rows after which the field stops growing and scrolls internally.
const MAX_INPUT_ROWS: usize = 8;
const THUMB_SIZE: f32 = 56.0;
const INPUT_FRAME_RADIUS: f32 = 14.0;
const INPUT_FRAME_PAD_X: f32 = 12.0;
const INPUT_FRAME_PAD_Y: f32 = 8.0;
/// 16 MB raw bytes — guard against accidentally attaching a video file etc.
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

pub fn draw(app: &mut App, ui: &mut egui::Ui, disabled: bool) {
    handle_dropped_files(app, ui);
    handle_paste(app, ui);

    let p = app.palette;

    // Stable id so we can read the focus state from `Memory` *before* the
    // TextEdit is added — needed so we can consume Enter and suppress the
    // newline the multiline editor would otherwise insert.
    let input_id = egui::Id::new("chat_input_field");
    let input_focused = ui.memory(|m| m.has_focus(input_id));

    // The "/" palette draws above the composer and claims ↑↓/Enter/Tab/Esc
    // while it is open, so it has to run before the Esc-interrupt and
    // Enter-sends handlers below get a look at the same keys.
    let slash = super::slash_menu::draw(app, ui, input_focused);

    if !slash.open {
        handle_escape(app, ui);
    }

    // Thumbnail strip for pending attachments (above the input row).
    if !app.chat.pending_images.is_empty() {
        draw_pending_strip(app, ui);
        ui.add_space(6.0);
    }

    let turn_in_flight = last_turn_in_flight(app);

    let submit_via_enter = !disabled
        && input_focused
        && !slash.open
        && ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
        });

    // Ctrl+Enter while a turn is running *steers* it: the text lands in the
    // running turn at its next step instead of queueing behind it. Plain
    // Enter still queues, which is the safer default — a steer changes the
    // instructions of work the user has not seen the end of.
    let steer_via_enter = !disabled
        && input_focused
        && !slash.open
        && turn_in_flight
        && ui.input_mut(|i| {
            i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter)
        });

    ui.horizontal(|ui| {
        let spacing = ui.spacing().item_spacing.x;
        let frame_pad_x = INPUT_FRAME_PAD_X * 2.0;
        let input_w = (ui.available_width()
            - SEND_BUTTON_W
            - ATTACH_BUTTON_W
            - spacing * 2.0
            - frame_pad_x)
            .max(80.0);

        // Paperclip button to the left of the input field.
        let attach_resp = ghost_button(ui, &p, "📎");
        if attach_resp.clicked() && !disabled {
            pick_file_and_attach(app);
        }
        attach_resp.on_hover_text("Attach image");

        // Rounded composer frame. Fill and stroke shift on hover/focus so
        // the field reads as a tappable surface. Uses last-frame hover
        // state cached on `ChatState` to avoid a second pass.
        let hovered = app.chat.input_hovered;
        let active = hovered || input_focused;
        let fill = if active { rgb(p.surface) } else { rgb(p.surface_sunk) };
        let stroke = Stroke::new(
            1.0,
            if input_focused {
                rgb(p.accent)
            } else if hovered {
                rgb(p.accent_hover)
            } else {
                rgb(p.hairline)
            },
        );
        let frame_inner = egui::Frame::none()
            .fill(fill)
            .stroke(stroke)
            .rounding(Rounding::same(INPUT_FRAME_RADIUS))
            .inner_margin(egui::Margin::symmetric(INPUT_FRAME_PAD_X, INPUT_FRAME_PAD_Y))
            .show(ui, |ui| {
                // Height tracks the draft: we lay the text out at the field's
                // own width and ask the galley how many rows it needs, so a
                // single long pasted line opens the field exactly as far as
                // the same text typed across several lines would. Past
                // `MAX_INPUT_ROWS` the ScrollArea takes over and scrolls the
                // overflow, keeping Send on screen. Row height is resolved
                // from the live style, so changing the body font size
                // rescales the composer with it.
                let font_id = egui::TextStyle::Body.resolve(ui.style());
                let row_h = ui.fonts(|f| f.row_height(&font_id));
                // TextEdit wraps its galley at `desired_width` minus its own
                // 4px side margins; measuring at the outer width undercounts
                // by a row exactly when a line fits outside the margins but
                // not inside them — the caret then hides below the field.
                let text_w = (input_w - 8.0).max(40.0);
                let rows = wrapped_rows(ui, &app.chat.draft, &font_id, text_w)
                    .clamp(MIN_INPUT_ROWS, MAX_INPUT_ROWS);
                // Allocate the height up front instead of letting the scroll
                // area derive it from the space available. A bottom panel is
                // only as tall as last frame's content, so a scroll area that
                // shrinks to fit that space can never ask for more than it
                // already has — which pins the field at its opening height no
                // matter how much is typed. Allocating makes the panel grow.
                let field_h = row_h * rows as f32 + 4.0;
                ui.allocate_ui(egui::vec2(input_w, field_h), |ui| {
                egui::ScrollArea::vertical()
                    .id_source("chat_input_scroll")
                    .show(ui, |ui| {
                        let input = egui::TextEdit::multiline(&mut app.chat.draft)
                            .id(input_id)
                            .hint_text(if disabled {
                                "(disabled — connect an LLM)"
                            } else if turn_in_flight {
                                "Enter queues · Ctrl+Enter steers this turn…"
                            } else {
                                "Type a message, or / for skills and commands…"
                            })
                            .desired_width(input_w)
                            .desired_rows(rows)
                            // egui only inserts a newline for the configured
                            // return key; plain Enter is consumed above to
                            // send, so map newline to Shift+Enter.
                            .return_key(Some(egui::KeyboardShortcut::new(
                                egui::Modifiers::SHIFT,
                                egui::Key::Enter,
                            )))
                            .frame(false);
                        ui.add_enabled(!disabled, input)
                    })
                    .inner
                })
                .inner
            });
        let _ = frame_inner.inner;
        app.chat.input_hovered = frame_inner.response.hovered();

        // While a turn streams, swap the SEND button for STOP. Same slot so
        // the cursor doesn't have to hunt.
        let can_submit = !disabled
            && (!app.chat.draft.trim().is_empty() || !app.chat.pending_images.is_empty());
        if turn_in_flight {
            // Disabled once an interrupt is already in flight — the label
            // reports that the turn is winding down rather than inviting a
            // second click that would do nothing visible.
            let stopping = app.chat.interrupt_requested;
            let label = if stopping { "Stopping" } else { "Stop" };
            let stop_resp = primary_button_enabled(ui, &p, label, !stopping);
            if stop_resp.clicked() && !stopping {
                app.interrupt_turn();
            }
            // The composer stays live during a turn: Enter queues the
            // message behind it, Ctrl+Enter steers the turn itself.
            if (submit_via_enter || steer_via_enter) && can_submit {
                send_message(app, steer_via_enter);
                ui.memory_mut(|m| m.request_focus(input_id));
            }
        } else {
            let send_resp = primary_button_enabled(ui, &p, "Send", can_submit);
            let send = send_resp.clicked();
            if (send || submit_via_enter) && can_submit {
                send_message(app, false);
                // Re-acquire focus so the user can keep typing.
                ui.memory_mut(|m| m.request_focus(input_id));
            }
        }
    });

    if let Some(ticket) = app.chat.idealist.last_ticket.clone() {
        ui.add_space(8.0);
        caps_label(ui, &format!("TICKET {ticket}"), rgb(p.muted));
    }
}

/// Visual rows `text` occupies when wrapped at `width` — wrapped
/// continuations included, never less than one. A trailing newline gets no row
/// of its own in the galley even though the caret has moved to a fresh line,
/// so it is counted back in: pressing Shift+Enter should open the field
/// immediately, not on the next keystroke.
fn wrapped_rows(ui: &egui::Ui, text: &str, font_id: &egui::FontId, width: f32) -> usize {
    if text.is_empty() {
        return 1;
    }
    // Colour is irrelevant — only the row count is read back.
    let job = egui::text::LayoutJob::simple(
        text.to_owned(),
        font_id.clone(),
        egui::Color32::WHITE,
        width,
    );
    let galley = ui.fonts(|f| f.layout_job(job));
    galley
        .rows
        .len()
        .saturating_add(usize::from(text.ends_with('\n')))
        .max(1)
}

/// `/compact`, `/plan …`, `/permission …` run as harness commands instead
/// of model turns. The head token must match exactly (whitespace-bounded);
/// everything after it is the command input.
fn parse_harness_command(text: &str) -> Option<(String, String)> {
    let head = text.split_whitespace().next()?;
    let name = match head {
        "/compact" => "compact",
        "/plan" => "plan",
        "/permission" => "permission",
        "/goal" => "goal",
        _ => return None,
    };
    let input = text[head.len()..].trim().to_string();
    Some((name.to_string(), input))
}

/// One-line, length-capped preview for a transcript marker label.
fn one_line(text: &str, cap: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(cap) {
        Some((i, _)) => format!("{}…", &flat[..i]),
        None => flat,
    }
}

/// Build the outgoing `SendUserMessage`, draining `pending_images`.
///
/// `steer` routes the text into the *running* turn (`SteerTurn`) instead of
/// queueing it as the next one. It only applies while a turn is in flight
/// and only to plain text — there is no image channel on a steer, so an
/// attachment falls back to the ordinary send, which the backend queues.
fn send_message(app: &mut App, steer: bool) {
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

    // A steer joins the turn already on screen rather than opening one of
    // its own, so it shows as a marker in the transcript.
    if steer && images.is_empty() {
        let session_id = app.chat.session_id;
        app.chat.turns.push(crate::app::Turn::marker(
            session_id,
            crate::app::Notice {
                label:  format!("steered: {}", one_line(&text, 80)),
                detail: text.clone(),
                ok:     true,
            },
        ));
        app.chat.scroll_to_bottom = true;
        app.send(UiCommand::SendRequest(Request::SteerTurn { session_id, text }));
        return;
    }


    // Harness commands never create a model message: route them to
    // `RunCommand` instead of the turn loop (no Turn is pushed, so the
    // transcript shows only the BE's log line + result).
    if images.is_empty() {
        if let Some((name, input)) = parse_harness_command(&text) {
            let session_id = app.chat.session_id;
            app.last_command_session = Some(session_id);
            app.send(UiCommand::SendRequest(Request::RunCommand {
                session_id,
                name,
                input,
            }));
            return;
        }
    }

    let session_id = app.chat.session_id;
    // Most-recent prompt first: bump this session to the top of the list.
    if let Some(pos) = app.chat.sessions.iter().position(|s| s.id == session_id) {
        if pos > 0 {
            let meta = app.chat.sessions.remove(pos);
            app.chat.sessions.insert(0, meta);
        }
    }
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
        notice: None,
        queued: false,
    });
    app.chat.scroll_to_bottom = true;
    app.chat.interrupt_requested = false;
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
    if last_turn_in_flight(app) && !app.chat.interrupt_requested {
        app.interrupt_turn();
    }
}

fn last_turn_in_flight(app: &App) -> bool {
    app.chat.turns.last().map(|t| !t.finished).unwrap_or(false)
}
