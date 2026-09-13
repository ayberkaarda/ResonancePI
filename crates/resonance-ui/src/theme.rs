//! The overlay's visual language.
//!
//! Every colour, corner radius and line weight the overlay draws with is
//! named here, and [`apply_style`] folds the same values into the `egui`
//! style so that stock widgets — sliders, buttons, tooltips, scroll bars —
//! match the shapes the overlay paints by hand.
//!
//! The palette is deliberately monochrome: one warm neutral ramp for text on
//! a near-black ground, translucent white for every fill and edge, and a
//! single accent used only to mark state. Nothing large is ever coloured.

use egui::style::{HandleShape, Selection, WidgetVisuals, Widgets};
use egui::{Color32, CornerRadius, FontFamily, FontId, Shadow, Stroke, TextStyle, Visuals};

// ---------------------------------------------------------------- palette

/// The panel ground. Only its alpha changes with the opacity setting, so this
/// is also what the surface is cleared to when the window is opaque.
///
/// Near-black but not black: a true black panel reads as a hole punched in the
/// desktop rather than as a pane laid over it.
pub(crate) const SURFACE: Color32 = Color32::from_rgb(29, 29, 31);

/// Primary text. Warm off-white rather than pure white, which glares against
/// a dark translucent ground.
pub(crate) const TEXT_PRIMARY: Color32 = Color32::from_rgb(245, 245, 243);

/// Supporting text: section headings, readouts, hints.
pub(crate) const TEXT_SECONDARY: Color32 = Color32::from_rgb(168, 168, 166);

/// Text belonging to something that cannot be used right now.
pub(crate) const TEXT_DISABLED: Color32 = Color32::from_rgb(110, 110, 108);

/// The one accent. Reserved for state — the selected device, the filled part
/// of a slider — and never used to fill a large area.
pub(crate) const ACCENT: Color32 = Color32::from_rgb(94, 158, 255);

/// The accent behind a selected row: enough to read as "this one", far too
/// little to read as a coloured surface.
pub(crate) const ACCENT_TINT: Color32 = Color32::from_rgba_unmultiplied_const(94, 158, 255, 40);

/// The accent as a hairline around a selected row.
pub(crate) const ACCENT_EDGE: Color32 = Color32::from_rgba_unmultiplied_const(94, 158, 255, 110);

/// Desaturated red, used only to mark a muted session and to tint the hover
/// state of the control that drops a saved entry.
pub(crate) const DANGER: Color32 = Color32::from_rgb(255, 107, 107);

/// [`DANGER`] behind a control that is being hovered, at the same weight as
/// the neutral hover fill.
pub(crate) const DANGER_TINT: Color32 = Color32::from_rgba_unmultiplied_const(255, 107, 107, 38);

/// Edge of the panel and of the cards inside it. A single translucent
/// hairline is what separates one surface from the next; there are no heavier
/// borders anywhere.
pub(crate) const HAIRLINE: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 26);

/// [`HAIRLINE`] for an edge that is being hovered.
pub(crate) const HAIRLINE_STRONG: Color32 =
    Color32::from_rgba_unmultiplied_const(255, 255, 255, 40);

/// Face of a card or a resting button: a barely-there lift off the ground.
pub(crate) const FILL_RAISED: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 13);

/// Face of something under the pointer.
pub(crate) const FILL_HOVER: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 24);

/// Face of something being pressed.
pub(crate) const FILL_PRESSED: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 38);

/// Unfilled part of a slider rail and the off state of a switch.
pub(crate) const FILL_TRACK: Color32 = Color32::from_rgba_unmultiplied_const(255, 255, 255, 56);

// ---------------------------------------------------------------- metrics

/// Corner radius of the panel itself.
pub(crate) const RADIUS_PANEL: u8 = 12;

/// Corner radius of the card-like groupings inside it — a device row, a
/// session card. Rounded, but never a pill: a pill at this size reads as a
/// button rather than as a surface.
pub(crate) const RADIUS_CARD: u8 = 9;

/// Corner radius of the small controls. Larger than half the height of a
/// slider rail or a switch, which the renderer clamps, so those come out as
/// true pills while buttons keep a soft rectangle.
pub(crate) const RADIUS_CONTROL: u8 = 7;

/// Inset between the panel edge and its contents.
pub(crate) const PANEL_PADDING: f32 = 14.0;

/// Transparent border left around the panel for its shadow to fall into.
///
/// It has to be at least as wide as the shadow reaches, because nothing can be
/// drawn outside the window: [`PANEL_SHADOW`] is sized to fit within it.
pub(crate) const PANEL_MARGIN: f32 = 8.0;

/// Inset between a card's edge and its contents. Typed as the margin unit
/// `egui` uses so it can be handed straight to a frame.
pub(crate) const CARD_PADDING: i8 = 10;

/// One line weight for every icon the overlay draws, so a close cross, a gear
/// and a marker dot all carry the same visual weight.
pub(crate) const ICON_STROKE_WIDTH: f32 = 1.25;

/// Optical size every painted icon is drawn within, regardless of how large
/// the control around it is.
pub(crate) const GLYPH_SIZE: f32 = 11.0;

/// A single soft shadow under the panel, wide and very faint — enough to
/// separate the overlay from a busy desktop, not enough to read as a drop
/// shadow. Sized so `blur / 2 + offset` stays within [`PANEL_MARGIN`].
pub(crate) const PANEL_SHADOW: Shadow = Shadow {
    offset: [0, 2],
    blur: 12,
    spread: 0,
    color: Color32::from_black_alpha(48),
};

/// The shadow under a tooltip, which floats above the panel rather than above
/// the desktop and so needs less separation.
const POPUP_SHADOW: Shadow = Shadow {
    offset: [0, 2],
    blur: 10,
    spread: 0,
    color: Color32::from_black_alpha(60),
};

/// The hairline used for every edge in the overlay.
pub(crate) fn hairline() -> Stroke {
    Stroke::new(1.0, HAIRLINE)
}

// ----------------------------------------------------------------- style

/// Text sizes. There is one font cut available, so the hierarchy is carried by
/// size and colour alone; keeping the set small also keeps the glyph atlas
/// small, which is the reason the overlay embeds a single face to begin with.
fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use FontFamily::{Monospace, Proportional};

    [
        (TextStyle::Small, FontId::new(11.0, Proportional)),
        (TextStyle::Body, FontId::new(13.0, Proportional)),
        (TextStyle::Button, FontId::new(13.0, Proportional)),
        (TextStyle::Heading, FontId::new(15.0, Proportional)),
        (TextStyle::Monospace, FontId::new(13.0, Monospace)),
    ]
    .into()
}

/// Builds the widget visuals for one interaction state.
fn widget(
    bg_fill: Color32,
    weak_bg_fill: Color32,
    bg_stroke: Color32,
    fg: Color32,
) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill,
        weak_bg_fill,
        bg_stroke: Stroke::new(1.0, bg_stroke),
        corner_radius: CornerRadius::same(RADIUS_CONTROL),
        // The width matters only where this stroke is drawn as a line rather
        // than used as a text colour: the ring around a slider handle, and a
        // checkmark.
        fg_stroke: Stroke::new(1.4, fg),
        // Widgets keep their size when hovered. Growing them would break the
        // alignment of every row they sit in, which is what a list of
        // identically shaped cards is for.
        expansion: 0.0,
    }
}

/// Installs the palette above as the `egui` style.
///
/// Applied once when the overlay window is created; the drawing code then
/// reads colours either from this style (for stock widgets) or from the
/// constants above (for the shapes it paints itself), and the two agree
/// because they are the same values.
pub(crate) fn apply_style(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();

    visuals.widgets = Widgets {
        // Plain text and separators.
        noninteractive: widget(FILL_RAISED, Color32::TRANSPARENT, HAIRLINE, TEXT_PRIMARY),
        // At rest. `bg_fill` is both the unfilled slider rail and the resting
        // slider handle, which the slider widget takes from the same field;
        // the handle is told apart by the bright ring its `fg_stroke` draws.
        inactive: widget(FILL_TRACK, FILL_RAISED, HAIRLINE, TEXT_PRIMARY),
        // Under the pointer: button faces lift, and a slider handle goes from
        // a ring to a solid disc.
        hovered: widget(TEXT_PRIMARY, FILL_HOVER, HAIRLINE_STRONG, TEXT_PRIMARY),
        active: widget(TEXT_PRIMARY, FILL_PRESSED, HAIRLINE_STRONG, TEXT_PRIMARY),
        open: widget(TEXT_PRIMARY, FILL_HOVER, HAIRLINE_STRONG, TEXT_PRIMARY),
    };

    visuals.selection = Selection {
        bg_fill: ACCENT_TINT,
        stroke: Stroke::new(1.0, TEXT_PRIMARY),
    };

    visuals.panel_fill = SURFACE;
    visuals.window_fill = SURFACE;
    visuals.window_stroke = hairline();
    visuals.window_corner_radius = CornerRadius::same(RADIUS_CARD);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS_CARD);
    visuals.window_shadow = POPUP_SHADOW;
    visuals.popup_shadow = POPUP_SHADOW;

    visuals.faint_bg_color = FILL_RAISED;
    visuals.extreme_bg_color = FILL_TRACK;
    visuals.code_bg_color = FILL_RAISED;
    visuals.hyperlink_color = ACCENT;
    visuals.error_fg_color = DANGER;
    visuals.warn_fg_color = DANGER;

    visuals.weak_text_color = Some(TEXT_SECONDARY);

    // The filled part of a rail carries the accent; the handle is a round
    // knob, which is what every rail-and-knob control on this desktop looks
    // like.
    visuals.slider_trailing_fill = true;
    visuals.handle_shape = HandleShape::Circle;
    visuals.striped = false;

    let mut style = egui::Style {
        visuals,
        text_styles: text_styles(),
        ..Default::default()
    };

    let spacing = &mut style.spacing;
    spacing.item_spacing = egui::vec2(8.0, 8.0);
    spacing.button_padding = egui::vec2(9.0, 4.0);
    spacing.window_margin = egui::Margin::same(CARD_PADDING);
    spacing.menu_margin = egui::Margin::same(CARD_PADDING);
    // Every clickable thing is at least this tall, which is what gives the
    // rows their unhurried spacing without padding each one by hand.
    spacing.interact_size = egui::vec2(24.0, 24.0);
    spacing.slider_rail_height = 5.0;
    spacing.icon_width = 16.0;
    spacing.icon_width_inner = 9.0;
    spacing.icon_spacing = 6.0;
    spacing.tooltip_width = 220.0;

    // The overlay is a dark pane whatever the desktop is set to, so the same
    // style is installed for both themes and the preference is pinned; that
    // way a system theme change cannot swap a palette out from under a window
    // whose every colour was chosen against this one ground.
    let style = std::sync::Arc::new(style);
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
    ctx.set_theme(egui::ThemePreference::Dark);
}
