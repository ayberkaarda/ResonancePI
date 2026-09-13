//! The overlay window: its session-scoped state and its drawing code.
//!
//! The overlay is a borderless, per-pixel transparent, always-on-top window.
//! It owns no application state of its own beyond presentation preferences —
//! everything it shows comes from the latest `Snapshot`, and every change the
//! user makes leaves as a `UiCommand`.
//!
//! # Shape of the thing
//!
//! A single translucent pane, inset from the window edge so a soft shadow has
//! somewhere to fall, holding three stacked groups: the device list, the
//! per-application mixer, and — when asked for — the settings. Groups are
//! separated by their headings rather than by rules, surfaces are told apart
//! by a hairline and a barely-there fill, and colour appears only where it
//! carries meaning. Every colour and radius used here is named in
//! [`crate::theme`].

use std::sync::Arc;

use crossbeam_channel::Sender;
use egui::{
    Align, Color32, CornerRadius, Label, Layout, Painter, PointerButton, Pos2, Rect, Response,
    RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2, ViewportBuilder, ViewportCommand,
};
use resonance_core::messages::{
    EndpointId, EndpointState, HotkeyConfig, SessionState, SessionView, Snapshot, UiCommand,
};

use crate::hotkey::{self, HotkeyControl};
use crate::theme;

/// Fixed overlay size. The window is not resizable: a compact widget with a
/// predictable footprint is the point of the overlay, and a fixed size keeps
/// the transparent surface small.
pub(crate) const OVERLAY_SIZE: Vec2 = Vec2::new(340.0, 400.0);

/// Lowest backdrop opacity the user can dial in. Below this the controls
/// become genuinely hard to hit against a busy desktop, so the slider stops
/// here instead of allowing a fully invisible window.
pub(crate) const MIN_OPACITY: f32 = 0.25;

/// Default backdrop opacity for a fresh session.
pub(crate) const DEFAULT_OPACITY: f32 = 0.88;

/// Height of the drag handle and the two buttons that sit in it.
const TITLE_STRIP_HEIGHT: f32 = 26.0;

/// Height of one device row.
const TAB_ROW_HEIGHT: f32 = 30.0;

/// Floor on the height the mixer list is given, so that opening the settings
/// on a short window still leaves a usable strip of the list rather than
/// collapsing it to nothing.
const MIN_LIST_HEIGHT: f32 = 80.0;

/// Height reserved at the bottom of the panel while the settings are open.
///
/// The mixer list is capped to whatever is left, so that opening the settings
/// shortens the list instead of pushing it off the bottom of the window. The
/// settings block has fixed contents — three rows, a hint, and room for the
/// one line of feedback the shortcut recorder can show — so a constant is
/// enough; it is set a little generously, and erring high only leaves a small
/// gap.
const SETTINGS_BLOCK_HEIGHT: f32 = 168.0;

/// Shown while the settings panel is waiting for a combination to be pressed.
const RECORDING_PROMPT: &str = "Press a combination…";

/// Shown when the key pressed while recording cannot be stored in a shortcut.
const KEY_NOT_SUPPORTED: &str = "That key cannot be used. Try a letter, a digit or a function key.";

/// Shown when a combination carried no modifier. A bare key would be taken
/// from every other application on the desktop.
const MODIFIER_REQUIRED: &str = "Hold Ctrl, Alt or Shift as well.";

/// Shown when the system refused a combination, and when the shortcut could
/// not be registered at startup.
const SHORTCUT_UNAVAILABLE: &str = "Could not register that shortcut; it may be in use.";

/// Side length of the square icon buttons in the title strip.
const ICON_BUTTON_SIZE: Vec2 = Vec2::splat(22.0);

/// Diameter of the area holding the default-endpoint marker dot. Larger than
/// the dot itself so the device rows share a left margin with everything else;
/// the marker is never clicked.
const DEFAULT_MARKER_SIZE: f32 = 14.0;

/// Size of the pill-shaped on/off switches.
const SWITCH_SIZE: Vec2 = Vec2::new(30.0, 17.0);

/// Presentation state that survives the overlay window being destroyed and
/// recreated, but not a process restart.
pub(crate) struct OverlayState {
    tx: Sender<UiCommand>,
    /// The shortcut in effect, shared with the loop that waits while the
    /// overlay is closed. Recording a new combination registers it here, so the
    /// change takes hold immediately rather than on the next start.
    hotkey: Arc<HotkeyControl>,
    snapshot: Option<Snapshot>,
    selected_endpoint: Option<EndpointId>,
    opacity: f32,
    opaque_mode: bool,
    settings_open: bool,
    /// Set while the settings panel is listening for the combination to store.
    recording_hotkey: bool,
    /// One line of feedback about the shortcut, shown under its row. Cleared
    /// as soon as a combination is stored.
    hotkey_notice: Option<&'static str>,
    /// Last known top-left corner of the overlay window, used to put it back
    /// where the user left it when it is recreated.
    position: Option<Pos2>,
    /// Set from inside the overlay when the user closes it.
    close_requested: bool,
    /// Set when a setting changed that only takes effect on a freshly created
    /// window, asking for the overlay to be rebuilt immediately.
    restart_requested: bool,
}

impl OverlayState {
    pub(crate) fn new(tx: Sender<UiCommand>, hotkey: Arc<HotkeyControl>) -> Self {
        Self {
            tx,
            hotkey,
            snapshot: None,
            selected_endpoint: None,
            opacity: DEFAULT_OPACITY,
            opaque_mode: false,
            settings_open: false,
            recording_hotkey: false,
            hotkey_notice: None,
            position: None,
            close_requested: false,
            restart_requested: false,
        }
    }

    pub(crate) fn set_snapshot(&mut self, snapshot: Snapshot) {
        self.snapshot = Some(snapshot);
    }

    pub(crate) fn opaque_mode(&self) -> bool {
        self.opaque_mode
    }

    pub(crate) fn take_close_request(&mut self) -> bool {
        std::mem::take(&mut self.close_requested)
    }

    pub(crate) fn take_restart_request(&mut self) -> bool {
        std::mem::take(&mut self.restart_requested)
    }

    /// Clamps into the range the slider offers, so a value can never make the
    /// window unusable.
    pub(crate) fn set_opacity(&mut self, opacity: f32) {
        self.opacity = opacity.clamp(MIN_OPACITY, 1.0);
    }

    /// Alpha actually painted behind the controls.
    ///
    /// In opaque mode the backdrop is fully solid: that is the whole point of
    /// the mode, because on some GPU drivers a per-pixel transparent window
    /// composites as solid black instead of blending with the desktop. Filling
    /// every pixel with a solid colour makes the overlay readable no matter how
    /// the driver treats the alpha channel.
    pub(crate) fn backdrop_alpha(&self) -> u8 {
        if self.opaque_mode {
            255
        } else {
            (self.opacity.clamp(MIN_OPACITY, 1.0) * 255.0).round() as u8
        }
    }

    fn backdrop_color(&self) -> Color32 {
        let surface = theme::SURFACE;
        Color32::from_rgba_unmultiplied(
            surface.r(),
            surface.g(),
            surface.b(),
            self.backdrop_alpha(),
        )
    }

    /// The endpoint whose sessions are listed, resolved against the current
    /// snapshot.
    ///
    /// An explicit selection wins, but only while that endpoint still exists.
    /// Otherwise the system default is shown, falling back to the first
    /// endpoint so the overlay is never blank while endpoints exist.
    fn resolved_selection(&self, snapshot: &Snapshot) -> Option<EndpointId> {
        if let Some(selected) = &self.selected_endpoint {
            if snapshot.endpoints.iter().any(|e| &e.id == selected) {
                return Some(selected.clone());
            }
        }

        if let Some(default) = &snapshot.default_endpoint {
            if snapshot.endpoints.iter().any(|e| &e.id == default) {
                return Some(default.clone());
            }
        }

        snapshot.endpoints.first().map(|e| e.id.clone())
    }

    /// Starts listening for the combination to store.
    fn start_recording_hotkey(&mut self) {
        self.recording_hotkey = true;
        self.hotkey_notice = None;
    }

    /// Stops listening, leaving the shortcut as it was.
    fn cancel_recording_hotkey(&mut self) {
        self.recording_hotkey = false;
        self.hotkey_notice = None;
    }

    /// Puts a recorded combination into effect and asks the backend to keep it.
    ///
    /// Registration is what decides whether the change happened: the command
    /// is only sent once the system has accepted the combination, so a
    /// shortcut that does not work is never saved. A refusal leaves the panel
    /// recording, so the user can go straight to another combination rather
    /// than having to press the button again.
    fn store_recorded_hotkey(&mut self, config: HotkeyConfig) {
        if !hotkey::has_modifier(config) {
            self.hotkey_notice = Some(MODIFIER_REQUIRED);
            return;
        }

        if !self.hotkey.apply(config) {
            self.hotkey_notice = Some(SHORTCUT_UNAVAILABLE);
            return;
        }

        self.recording_hotkey = false;
        self.hotkey_notice = None;
        self.send(UiCommand::SetHotkey(config));
    }

    /// The line shown under the shortcut row, if there is one.
    ///
    /// A shortcut the system never accepted is reported even when the user has
    /// not touched the recorder, because otherwise the row would name a
    /// combination that does nothing.
    fn hotkey_notice(&self) -> Option<&'static str> {
        self.hotkey_notice.or_else(|| {
            (!self.recording_hotkey && !self.hotkey.is_registered()).then_some(SHORTCUT_UNAVAILABLE)
        })
    }

    fn send(&self, command: UiCommand) {
        if self.tx.send(command).is_err() {
            tracing::warn!("the backend is no longer accepting commands");
        }
    }
}

/// Sessions belonging to one endpoint, in snapshot order.
fn sessions_for<'a>(snapshot: &'a Snapshot, endpoint: &EndpointId) -> Vec<&'a SessionView> {
    snapshot
        .live_sessions
        .iter()
        .filter(|s| &s.endpoint == endpoint)
        .collect()
}

/// Describes the overlay window.
///
/// Whether a window has a per-pixel alpha surface is fixed when it is created,
/// which is why switching to opaque mode asks for the window to be rebuilt
/// rather than trying to change it in place.
pub(crate) fn overlay_viewport(state: &OverlayState) -> ViewportBuilder {
    let mut builder = ViewportBuilder::default()
        .with_title("Resonance")
        .with_inner_size(OVERLAY_SIZE)
        .with_decorations(false)
        .with_resizable(false)
        .with_transparent(!state.opaque_mode)
        .with_taskbar(false)
        .with_always_on_top()
        .with_drag_and_drop(false);

    if let Some(position) = state.position {
        builder = builder.with_position(position);
    }

    builder
}

/// Draws the whole overlay. Called by egui whenever the overlay window needs
/// repainting.
pub(crate) fn draw(ui: &mut Ui, state: &mut OverlayState) {
    remember_position(ui, state);

    if ui.ctx().input(|i| i.viewport().close_requested()) {
        request_close(state);
    }

    let panel = paint_panel(ui, state);

    ui.scope_builder(
        UiBuilder::new().max_rect(panel.shrink(theme::PANEL_PADDING)),
        |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;

            draw_title_strip(ui, state);
            hairline(ui);

            let Some(snapshot) = state.snapshot.clone() else {
                placeholder(ui, "Waiting for the audio core…");
                return;
            };

            if snapshot.endpoints.is_empty() {
                placeholder(ui, "No active playback devices.");
                return;
            }

            let selected = state.resolved_selection(&snapshot);

            section_heading(ui, "OUTPUT");
            draw_endpoint_tabs(ui, state, &snapshot, selected.as_ref());

            if let Some(endpoint) = selected {
                section_heading(ui, "APPS");
                draw_session_rows(ui, state, &snapshot, &endpoint);
            }

            if state.settings_open {
                draw_settings(ui, state);
            }
        },
    );
}

/// Paints the pane everything else is drawn on, and returns the rect it
/// occupies.
///
/// In opaque mode the pane covers every pixel of the window with square
/// corners and no shadow: rounded corners and a shadow both rely on the parts
/// of the surface outside them staying transparent, which is exactly what a
/// driver with broken transparency paints black.
fn paint_panel(ui: &Ui, state: &OverlayState) -> Rect {
    let full = ui.max_rect();

    if state.opaque_mode {
        ui.painter()
            .rect_filled(full, CornerRadius::ZERO, state.backdrop_color());
        return full;
    }

    let panel = full.shrink(theme::PANEL_MARGIN);
    let radius = CornerRadius::same(theme::RADIUS_PANEL);

    ui.painter()
        .add(theme::PANEL_SHADOW.as_shape(panel, radius));
    ui.painter().rect(
        panel,
        radius,
        state.backdrop_color(),
        theme::hairline(),
        StrokeKind::Inside,
    );

    panel
}

/// Records where the user has dragged the window so it can be restored when
/// the window is recreated.
fn remember_position(ui: &Ui, state: &mut OverlayState) {
    if let Some(rect) = ui.ctx().input(|i| i.viewport().outer_rect) {
        state.position = Some(rect.min);
    }
}

fn request_close(state: &mut OverlayState) {
    state.close_requested = true;
}

/// Reserves a full-width row of a given height and registers the row's own
/// interaction, returning the rect to paint into and the row's response.
///
/// The row is made interactive *before* anything is put inside it, so that
/// widgets added within it afterwards sit on top: a click landing on one of
/// them is taken by that widget rather than by the row behind it.
///
/// The rect is reserved rather than allocated, because the layout space is
/// claimed by [`fill_row`] instead. Claiming it in both places would leave the
/// cursor wherever the second claim put it, which — the cursor being set
/// outright rather than only ever moved forward — can pull the next row up
/// into this one.
fn begin_row(
    ui: &mut Ui,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    height: f32,
    sense: Sense,
) -> (Rect, Response) {
    let rect = Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), height));
    let response = ui.interact(rect, ui.make_persistent_id(id_salt), sense);
    (rect, response)
}

/// Lays a row's contents out inside the rect [`begin_row`] reserved, centred
/// vertically, and claims that full height from the layout.
fn fill_row(ui: &mut Ui, rect: Rect, inset: f32, add_contents: impl FnOnce(&mut Ui)) {
    ui.scope_builder(
        UiBuilder::new().max_rect(rect.shrink2(Vec2::new(inset, 0.0))),
        |ui| {
            // Holds the row open to its full height even when its contents are
            // shorter, which is what keeps every row the same size and the
            // cursor where the next row expects it.
            ui.set_min_height(rect.height());
            ui.horizontal_centered(add_contents);
        },
    );
}

/// A full-width rule, one pixel tall, in the same colour as every other edge.
fn hairline(ui: &mut Ui) {
    let (rect, _response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 1.0), Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, theme::hairline());
}

/// The label that opens a group. Small, quiet and letter-spaced, so it reads
/// as a heading without needing a heavier weight — which this single font cut
/// cannot provide.
fn section_heading(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    ui.label(
        RichText::new(text)
            .small()
            .color(theme::TEXT_SECONDARY)
            .extra_letter_spacing(0.7),
    );
}

/// Centred message shown in place of the contents when there are none.
fn placeholder(ui: &mut Ui, text: &str) {
    ui.vertical_centered(|ui| {
        ui.add_space(28.0);
        ui.label(RichText::new(text).color(theme::TEXT_SECONDARY));
    });
}

/// The drag handle plus the settings and close buttons.
///
/// The strip is made interactive *before* the buttons are added so that the
/// buttons, being the later widgets, keep priority for clicks that land on
/// them; only the bare strip starts a window drag.
fn draw_title_strip(ui: &mut Ui, state: &mut OverlayState) {
    let (strip_rect, strip_response) = begin_row(
        ui,
        "title-strip",
        TITLE_STRIP_HEIGHT,
        Sense::click_and_drag(),
    );

    if strip_response.drag_started_by(PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(ViewportCommand::StartDrag);
    }

    fill_row(ui, strip_rect, 0.0, |ui| {
        ui.label(
            RichText::new("Resonance")
                .heading()
                .color(theme::TEXT_PRIMARY)
                // A touch of negative tracking at this size tightens the title
                // into a mark rather than a sentence.
                .extra_letter_spacing(-0.2),
        );

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if icon_button(ui, paint_close_icon)
                .on_hover_text("Close overlay")
                .clicked()
            {
                request_close(state);
            }
            if icon_button(ui, paint_settings_icon)
                .on_hover_text("Settings")
                .clicked()
            {
                state.settings_open = !state.settings_open;
            }
        });
    });
}

/// A square button that paints its own icon rather than relying on a font
/// glyph, so it renders correctly regardless of which glyphs the active font
/// happens to contain.
///
/// It carries no frame at rest — a chip appears only under the pointer, which
/// is what keeps two controls in the title strip from competing with the title
/// itself. `paint_icon` receives the button's rect and the colour to draw with.
fn icon_button(ui: &mut Ui, paint_icon: impl FnOnce(&Painter, Rect, Color32)) -> Response {
    let (rect, response) = ui.allocate_exact_size(ICON_BUTTON_SIZE, Sense::click());

    if ui.is_rect_visible(rect) {
        let hovered = response.hovered();

        let fill = if response.is_pointer_button_down_on() {
            theme::FILL_PRESSED
        } else if hovered {
            theme::FILL_HOVER
        } else {
            Color32::TRANSPARENT
        };

        if fill != Color32::TRANSPARENT {
            // Half the height, so the chip is a circle.
            let radius = CornerRadius::same((rect.height() / 2.0) as u8);
            ui.painter().rect_filled(rect, radius, fill);
        }

        let color = if hovered {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_SECONDARY
        };
        paint_icon(ui.painter(), rect, color);
    }

    response
}

/// The box every painted icon is drawn inside, so icons keep one optical size
/// no matter how large the control around them is.
fn glyph_rect(rect: Rect) -> Rect {
    Rect::from_center_size(rect.center(), Vec2::splat(theme::GLYPH_SIZE))
}

fn icon_stroke(color: Color32) -> Stroke {
    Stroke::new(theme::ICON_STROKE_WIDTH, color)
}

/// Draws an "X" as two crossing diagonals, standing in for a close glyph.
fn paint_close_icon(painter: &Painter, rect: Rect, color: Color32) {
    // Inset slightly: a cross drawn to the full box reads larger than a round
    // glyph of the same box, because its corners reach further.
    let mark = glyph_rect(rect).shrink(1.4);
    let stroke = icon_stroke(color);
    painter.line_segment([mark.left_top(), mark.right_bottom()], stroke);
    painter.line_segment([mark.right_top(), mark.left_bottom()], stroke);
}

/// Draws a small stroked hub with six radial teeth, standing in for a gear
/// glyph. It only needs to read as "settings" at a glance, not reproduce a
/// literal gear outline.
fn paint_settings_icon(painter: &Painter, rect: Rect, color: Color32) {
    let glyph = glyph_rect(rect);
    let center = glyph.center();
    let outer = glyph.width() / 2.0;
    let stroke = icon_stroke(color);

    painter.circle_stroke(center, outer * 0.58, stroke);

    const TEETH: usize = 6;
    for i in 0..TEETH {
        let angle = i as f32 * std::f32::consts::TAU / TEETH as f32;
        let direction = Vec2::angled(angle);
        painter.line_segment(
            [
                center + direction * outer * 0.78,
                center + direction * outer,
            ],
            stroke,
        );
    }
}

/// Draws the filled dot that marks the system default endpoint.
fn draw_default_marker(ui: &mut Ui, is_default: bool) {
    let (rect, _response) =
        ui.allocate_exact_size(Vec2::splat(DEFAULT_MARKER_SIZE), Sense::hover());

    if is_default && ui.is_rect_visible(rect) {
        ui.painter()
            .circle_filled(rect.center(), 3.0, theme::ACCENT);
    }
}

/// A pill-shaped on/off switch, painted rather than assembled from a checkbox
/// so that it matches the rounded, filled language of the sliders next to it.
///
/// `on_fill` is the colour the track takes once the switch is on, which is the
/// only place a state colour appears in a row.
fn switch(ui: &mut Ui, on: &mut bool, on_fill: Color32) -> Response {
    let (rect, mut response) = ui.allocate_exact_size(SWITCH_SIZE, Sense::click());

    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }

    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);

        let off_fill = if response.hovered() {
            theme::FILL_PRESSED
        } else {
            theme::FILL_TRACK
        };
        let track = off_fill.lerp_to_gamma(on_fill, how_on);

        let radius = CornerRadius::same((rect.height() / 2.0) as u8);
        ui.painter().rect_filled(rect, radius, track);

        let knob = rect.height() / 2.0 - 2.5;
        let travel = (rect.left() + knob + 2.5)..=(rect.right() - knob - 2.5);
        let center = Pos2::new(egui::lerp(travel, how_on), rect.center().y);
        ui.painter()
            .circle_filled(center, knob, theme::TEXT_PRIMARY);
    }

    response
}

/// Horizontal and vertical padding inside a row button.
const ROW_BUTTON_PADDING: Vec2 = Vec2::new(9.0, 4.0);

/// A small text button with a quiet face, used for the two actions that sit
/// inside rows.
///
/// It is painted rather than built from a stock button because its face and
/// its label both change together with its state, which a widget configured
/// up front cannot do: `danger` turns the hover state red, marking the action
/// that discards something, and a disabled button loses its face entirely
/// instead of merely dimming.
fn row_button(ui: &mut Ui, label: &str, enabled: bool, danger: bool) -> Response {
    let galley = ui.painter().layout_no_wrap(
        label.to_owned(),
        egui::TextStyle::Small.resolve(ui.style()),
        Color32::PLACEHOLDER,
    );

    // Still allocated when disabled, so a row's controls keep their positions
    // whether or not the action is available, and so the reason can still be
    // shown on hover.
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(galley.size() + 2.0 * ROW_BUTTON_PADDING, sense);

    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();

        let fill = if !enabled {
            Color32::TRANSPARENT
        } else if danger && hovered {
            theme::DANGER_TINT
        } else if response.is_pointer_button_down_on() {
            theme::FILL_PRESSED
        } else if hovered {
            theme::FILL_HOVER
        } else {
            theme::FILL_RAISED
        };

        let text_color = if !enabled {
            theme::TEXT_DISABLED
        } else if danger && hovered {
            theme::DANGER
        } else {
            theme::TEXT_PRIMARY
        };

        ui.painter().rect(
            rect,
            CornerRadius::same(theme::RADIUS_CONTROL),
            fill,
            theme::hairline(),
            StrokeKind::Inside,
        );
        ui.painter()
            .galley(rect.center() - galley.size() / 2.0, galley, text_color);
    }

    response
}

fn draw_endpoint_tabs(
    ui: &mut Ui,
    state: &mut OverlayState,
    snapshot: &Snapshot,
    selected: Option<&EndpointId>,
) {
    ui.spacing_mut().item_spacing.y = 4.0;

    for endpoint in snapshot.endpoints.iter() {
        let is_selected = selected == Some(&endpoint.id);
        let is_default = snapshot.default_endpoint.as_ref() == Some(&endpoint.id);

        let (rect, response) = begin_row(
            ui,
            ("endpoint-row", endpoint.id.as_ref()),
            TAB_ROW_HEIGHT,
            Sense::click(),
        );

        if ui.is_rect_visible(rect) {
            let (fill, stroke) = if is_selected {
                (theme::ACCENT_TINT, Stroke::new(1.0, theme::ACCENT_EDGE))
            } else if response.hovered() {
                (theme::FILL_HOVER, Stroke::NONE)
            } else {
                (Color32::TRANSPARENT, Stroke::NONE)
            };

            ui.painter().rect(
                rect,
                CornerRadius::same(theme::RADIUS_CARD),
                fill,
                stroke,
                StrokeKind::Inside,
            );
        }

        fill_row(ui, rect, 8.0, |ui| {
            // Laid out from the right so the button keeps its width and the
            // device name takes whatever is left, truncating instead of
            // pushing the button off the row.
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let can_switch = !is_default && endpoint.state == EndpointState::Active;
                if row_button(ui, "Switch", can_switch, false)
                    .on_hover_text("Make this the default playback device")
                    .clicked()
                {
                    state.send(UiCommand::SwitchEndpoint(endpoint.id.clone()));
                }

                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    draw_default_marker(ui, is_default);

                    let color = if endpoint.state == EndpointState::Active {
                        theme::TEXT_PRIMARY
                    } else {
                        theme::TEXT_DISABLED
                    };
                    ui.add(
                        Label::new(RichText::new(endpoint.friendly_name.as_ref()).color(color))
                            .truncate(),
                    )
                    .on_hover_text(endpoint_state_text(endpoint.state));
                });
            });
        });

        if response.clicked() {
            state.selected_endpoint = Some(endpoint.id.clone());
        }
    }

    ui.spacing_mut().item_spacing.y = 8.0;
}

fn endpoint_state_text(state: EndpointState) -> &'static str {
    match state {
        EndpointState::Active => "Active",
        EndpointState::Disabled => "Disabled",
        EndpointState::NotPresent => "Not present",
        EndpointState::Unplugged => "Unplugged",
    }
}

fn draw_session_rows(
    ui: &mut Ui,
    state: &mut OverlayState,
    snapshot: &Snapshot,
    endpoint: &EndpointId,
) {
    let sessions = sessions_for(snapshot, endpoint);

    if sessions.is_empty() {
        ui.label(
            RichText::new("No applications are playing on this device.")
                .color(theme::TEXT_SECONDARY),
        );
        return;
    }

    // While the settings are open they take the bottom of the panel, so the
    // list gets what remains rather than growing under them.
    let reserved = if state.settings_open {
        SETTINGS_BLOCK_HEIGHT
    } else {
        0.0
    };
    let height = (ui.available_height() - reserved).max(MIN_LIST_HEIGHT);

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .max_height(height)
        .show(ui, |ui| {
            for session in sessions {
                draw_session_card(ui, state, endpoint, session);
            }
        });
}

/// One application: its name and current level on the first line, its controls
/// on the second.
fn draw_session_card(
    ui: &mut Ui,
    state: &mut OverlayState,
    endpoint: &EndpointId,
    session: &SessionView,
) {
    // The card takes its height from what it holds rather than from a constant,
    // so a change to a control's size or to the text size cannot leave content
    // hanging over the edge of its own background.
    egui::Frame::new()
        .fill(theme::FILL_RAISED)
        .stroke(theme::hairline())
        .corner_radius(CornerRadius::same(theme::RADIUS_CARD))
        .inner_margin(egui::Margin::same(theme::CARD_PADDING))
        .show(ui, |ui| {
            // Cards span the list rather than shrinking to their own contents, so
            // their right edges line up.
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 6.0;

            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if row_button(ui, "Forget", true, true)
                        .on_hover_text("Drop this application's saved entry for this device")
                        .clicked()
                    {
                        state.send(UiCommand::ForgetProfileEntry {
                            endpoint: endpoint.clone(),
                            process: session.process.clone(),
                        });
                    }

                    ui.label(
                        RichText::new(format!("{:.0}%", session.volume * 100.0))
                            .small()
                            .color(theme::TEXT_SECONDARY),
                    );

                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        let color = if session.state == SessionState::Active {
                            theme::TEXT_PRIMARY
                        } else {
                            theme::TEXT_DISABLED
                        };
                        let name = ui.add(
                            Label::new(RichText::new(session.process.as_ref()).color(color))
                                .truncate(),
                        );
                        if session.state != SessionState::Active {
                            name.on_hover_text("This session is not currently playing");
                        }
                    });
                });
            });

            ui.horizontal(|ui| {
                let mut muted = session.muted;
                let hint = if muted { "Unmute" } else { "Mute" };
                if switch(ui, &mut muted, theme::DANGER)
                    .on_hover_text(hint)
                    .changed()
                {
                    state.send(UiCommand::SetSessionMute {
                        endpoint: endpoint.clone(),
                        process: session.process.clone(),
                        muted,
                    });
                }

                // The rail takes whatever the switch left, so every card's slider
                // ends on the same line down the right edge. A point is held back
                // so a rounding difference cannot push it past the card's edge.
                ui.spacing_mut().slider_width = (ui.available_width() - 1.0).max(40.0);

                let mut volume = session.volume;
                let slider = ui.add(
                    egui::Slider::new(&mut volume, 0.0..=1.0)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                );
                if slider.changed() {
                    state.send(UiCommand::SetSessionVolume {
                        endpoint: endpoint.clone(),
                        process: session.process.clone(),
                        volume,
                    });
                }
            });
        });
}

fn draw_settings(ui: &mut Ui, state: &mut OverlayState) {
    hairline(ui);
    section_heading(ui, "SETTINGS");

    ui.horizontal(|ui| {
        ui.label(RichText::new("Opacity").color(theme::TEXT_PRIMARY));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(60.0);

            let mut opacity = state.opacity;
            if ui
                .add(
                    egui::Slider::new(&mut opacity, MIN_OPACITY..=1.0)
                        .show_value(false)
                        .custom_formatter(|v, _| format!("{:.0}%", v * 100.0)),
                )
                .changed()
            {
                state.set_opacity(opacity);
            }
        });
    });

    ui.horizontal(|ui| {
        ui.label(RichText::new("Opaque mode").color(theme::TEXT_PRIMARY));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let mut opaque_mode = state.opaque_mode;
            if switch(ui, &mut opaque_mode, theme::ACCENT)
                .on_hover_text(
                    "Draw a solid background instead of blending with the desktop. \
                     Use this if the overlay appears as a black rectangle.",
                )
                .changed()
            {
                state.opaque_mode = opaque_mode;
                // Per-pixel transparency is decided when the window is created,
                // so the change only takes effect on a rebuilt window.
                state.restart_requested = true;
            }
        });
    });

    draw_hotkey_row(ui, state);

    ui.add_space(2.0);
    ui.label(
        RichText::new("Drag the title bar to move the overlay.")
            .small()
            .color(theme::TEXT_SECONDARY),
    );
}

/// The shortcut row: what is registered now, and the control that records
/// something else.
///
/// While recording, the row shows what it is waiting for instead of the
/// current combination, and the button cancels — so the recorder can always be
/// left the way it was entered.
fn draw_hotkey_row(ui: &mut Ui, state: &mut OverlayState) {
    if state.recording_hotkey {
        read_recorded_hotkey(ui, state);
    }

    ui.horizontal(|ui| {
        ui.label(RichText::new("Shortcut").color(theme::TEXT_PRIMARY));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let recording = state.recording_hotkey;
            let label = if recording { "Cancel" } else { "Change" };

            if row_button(ui, label, true, false)
                .on_hover_text(if recording {
                    "Keep the current shortcut"
                } else {
                    "Record a new combination for showing and hiding the overlay"
                })
                .clicked()
            {
                if recording {
                    state.cancel_recording_hotkey();
                } else {
                    state.start_recording_hotkey();
                    // A widget still holding keyboard focus would act on the
                    // keys being recorded — an arrow key would move a slider
                    // the recorder is reading at the same time.
                    surrender_keyboard_focus(ui);
                }
            }

            let (text, color) = if recording {
                (RECORDING_PROMPT.to_owned(), theme::ACCENT)
            } else {
                (
                    hotkey::describe(state.hotkey.config()),
                    theme::TEXT_SECONDARY,
                )
            };
            ui.add(Label::new(RichText::new(text).small().color(color)).truncate());
        });
    });

    if let Some(notice) = state.hotkey_notice() {
        ui.add(
            Label::new(
                RichText::new(notice)
                    .small()
                    .color(if state.recording_hotkey {
                        theme::DANGER
                    } else {
                        theme::TEXT_SECONDARY
                    }),
            )
            .truncate(),
        );
    }
}

/// Turns this frame's key presses into a shortcut while the recorder is open.
///
/// Only a key that is not a modifier arrives as a key event at all, so what is
/// read here is always a complete combination: the key, plus whichever
/// modifiers were held down with it.
///
/// The Windows key cannot be recorded, because the window system this overlay
/// is drawn with does not report it as a modifier. A shortcut saved with it —
/// by hand, or by a later version — still registers and is still shown
/// correctly; it simply cannot be entered here.
fn read_recorded_hotkey(ui: &Ui, state: &mut OverlayState) {
    let events = ui.ctx().input(|input| input.events.clone());

    for event in events {
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            continue;
        };

        if key == egui::Key::Escape {
            state.cancel_recording_hotkey();
            return;
        }

        let Some(vk) = hotkey::egui_key_to_vk(key) else {
            state.hotkey_notice = Some(KEY_NOT_SUPPORTED);
            continue;
        };

        state.store_recorded_hotkey(HotkeyConfig {
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            shift: modifiers.shift,
            win: false,
            key: vk,
        });
        return;
    }
}

/// Takes keyboard focus away from whichever widget holds it.
fn surrender_keyboard_focus(ui: &Ui) {
    if let Some(id) = ui.ctx().memory(|memory| memory.focused()) {
        ui.ctx().memory_mut(|memory| memory.surrender_focus(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use resonance_core::messages::SessionState;
    use std::sync::Arc;

    /// A state whose shortcut control holds no registration, so the tests
    /// never touch the machine's real hotkey table.
    fn state() -> OverlayState {
        state_with_channel().0
    }

    fn state_with_channel() -> (OverlayState, crossbeam_channel::Receiver<UiCommand>) {
        let (tx, rx) = unbounded();
        let hotkey = HotkeyControl::new(HotkeyConfig::default(), || {});
        (OverlayState::new(tx, hotkey), rx)
    }

    fn endpoint(id: &str, name: &str) -> resonance_core::messages::EndpointView {
        resonance_core::messages::EndpointView {
            id: Arc::from(id),
            friendly_name: Arc::from(name),
            state: EndpointState::Active,
        }
    }

    fn session(instance: &str, process: &str, endpoint: &str) -> SessionView {
        SessionView {
            instance: Arc::from(instance),
            process: Arc::from(process),
            endpoint: Arc::from(endpoint),
            volume: 0.5,
            muted: false,
            state: SessionState::Active,
        }
    }

    fn snapshot(
        endpoints: Vec<resonance_core::messages::EndpointView>,
        default: Option<&str>,
        sessions: Vec<SessionView>,
    ) -> Snapshot {
        Snapshot {
            endpoints: endpoints.into(),
            default_endpoint: default.map(Arc::from),
            live_sessions: sessions.into(),
            revision: 1,
        }
    }

    #[test]
    fn opacity_is_clamped_into_the_usable_range() {
        let mut s = state();

        s.set_opacity(5.0);
        assert_eq!(s.opacity, 1.0);

        s.set_opacity(-1.0);
        assert_eq!(s.opacity, MIN_OPACITY);
    }

    #[test]
    fn opaque_mode_forces_a_solid_backdrop() {
        let mut s = state();
        s.set_opacity(MIN_OPACITY);
        assert!(s.backdrop_alpha() < 255);

        s.opaque_mode = true;
        assert_eq!(s.backdrop_alpha(), 255);
    }

    #[test]
    fn backdrop_alpha_tracks_opacity_when_transparent() {
        let mut s = state();
        s.set_opacity(1.0);
        assert_eq!(s.backdrop_alpha(), 255);

        s.set_opacity(0.5);
        assert_eq!(s.backdrop_alpha(), 128);
    }

    #[test]
    fn an_opaque_backdrop_is_exactly_the_surface_colour() {
        let mut s = state();
        s.opaque_mode = true;

        // In opaque mode the window's surface is cleared to `theme::SURFACE`
        // and the panel is painted over every pixel of it, so the two have to
        // be the same colour or they disagree along the panel's edge.
        assert_eq!(s.backdrop_color(), theme::SURFACE);
    }

    #[test]
    fn switching_to_opaque_mode_asks_for_a_rebuilt_window() {
        let mut s = state();
        assert!(!s.take_restart_request());

        s.restart_requested = true;
        assert!(s.take_restart_request());
        assert!(!s.take_restart_request());
    }

    #[test]
    fn the_window_is_transparent_unless_opaque_mode_is_on() {
        let mut s = state();
        assert_eq!(overlay_viewport(&s).transparent, Some(true));

        s.opaque_mode = true;
        assert_eq!(overlay_viewport(&s).transparent, Some(false));
    }

    #[test]
    fn selection_falls_back_to_the_default_endpoint() {
        let s = state();
        let snap = snapshot(
            vec![endpoint("a", "Speakers"), endpoint("b", "Headphones")],
            Some("b"),
            vec![],
        );

        assert_eq!(s.resolved_selection(&snap).as_deref(), Some("b"));
    }

    #[test]
    fn selection_falls_back_to_the_first_endpoint_without_a_default() {
        let s = state();
        let snap = snapshot(vec![endpoint("a", "Speakers")], None, vec![]);

        assert_eq!(s.resolved_selection(&snap).as_deref(), Some("a"));
    }

    #[test]
    fn an_explicit_selection_wins_while_the_endpoint_exists() {
        let mut s = state();
        s.selected_endpoint = Some(Arc::from("a"));
        let snap = snapshot(
            vec![endpoint("a", "Speakers"), endpoint("b", "Headphones")],
            Some("b"),
            vec![],
        );

        assert_eq!(s.resolved_selection(&snap).as_deref(), Some("a"));
    }

    #[test]
    fn a_selection_of_a_vanished_endpoint_is_dropped() {
        let mut s = state();
        s.selected_endpoint = Some(Arc::from("gone"));
        let snap = snapshot(vec![endpoint("a", "Speakers")], Some("a"), vec![]);

        assert_eq!(s.resolved_selection(&snap).as_deref(), Some("a"));
    }

    #[test]
    fn selection_is_none_when_there_are_no_endpoints() {
        let s = state();
        let snap = snapshot(vec![], None, vec![]);

        assert!(s.resolved_selection(&snap).is_none());
    }

    #[test]
    fn sessions_are_filtered_by_endpoint() {
        let snap = snapshot(
            vec![endpoint("a", "Speakers"), endpoint("b", "Headphones")],
            Some("a"),
            vec![
                session("s1", "spotify.exe", "a"),
                session("s2", "chrome.exe", "b"),
                session("s3", "discord.exe", "a"),
            ],
        );

        let on_a = sessions_for(&snap, &Arc::from("a"));
        let names: Vec<&str> = on_a.iter().map(|s| s.process.as_ref()).collect();
        assert_eq!(names, vec!["spotify.exe", "discord.exe"]);
    }

    #[test]
    fn recording_can_be_left_the_way_it_was_entered() {
        let mut s = state();
        assert!(!s.recording_hotkey);

        s.start_recording_hotkey();
        assert!(s.recording_hotkey);

        s.cancel_recording_hotkey();
        assert!(!s.recording_hotkey);
    }

    #[test]
    fn a_combination_without_a_modifier_is_not_stored() {
        let (mut s, rx) = state_with_channel();
        s.start_recording_hotkey();

        s.store_recorded_hotkey(HotkeyConfig {
            ctrl: false,
            alt: false,
            shift: false,
            win: false,
            key: 0x56,
        });

        // Still recording, so the next combination pressed is read as well.
        assert!(s.recording_hotkey);
        assert_eq!(s.hotkey_notice, Some(MODIFIER_REQUIRED));
        assert!(rx.try_recv().is_err(), "nothing may be saved");
    }

    #[test]
    fn an_unregistered_shortcut_is_reported_without_recording() {
        let s = state();

        // The control in these tests never registered anything, which is the
        // same position the overlay is in when the system refuses the saved
        // shortcut at startup.
        assert!(!s.hotkey.is_registered());
        assert_eq!(s.hotkey_notice(), Some(SHORTCUT_UNAVAILABLE));
    }

    #[test]
    fn the_recorder_shows_its_own_feedback_while_it_is_open() {
        let mut s = state();
        s.start_recording_hotkey();

        // The startup notice is replaced by what the recorder has to say, so
        // the row never carries two messages at once.
        assert_eq!(s.hotkey_notice(), None);

        s.hotkey_notice = Some(KEY_NOT_SUPPORTED);
        assert_eq!(s.hotkey_notice(), Some(KEY_NOT_SUPPORTED));
    }

    #[test]
    fn a_close_request_is_reported_once() {
        let mut s = state();
        assert!(!s.take_close_request());

        s.close_requested = true;
        assert!(s.take_close_request());
        assert!(!s.take_close_request());
    }
}
