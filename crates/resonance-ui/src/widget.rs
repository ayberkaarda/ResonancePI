//! The corner widget: a draggable badge that grows into a list of endpoints.
//!
//! It is a plain Win32 layered window rather than a second `eframe` viewport.
//! The windowing and rendering stack behind the overlay panel is started only
//! while the panel is on screen, because the graphics driver behind even an
//! empty window holds tens of megabytes resident; this widget is visible for
//! almost the whole time the application runs, so putting it on that stack
//! would reintroduce exactly the cost the panel's lifecycle exists to avoid.
//!
//! What it shows is small and changes rarely, which is what keeps that trade
//! worth making: a badge at rest, and — after the pointer has rested on it
//! briefly — a short list of the connected endpoints, one clickable row each.
//! There is no render loop. A frame is composed and pushed only when something
//! actually changes: an animation step, a hover moving between rows, a drag, or
//! a new snapshot read as the list opens. The thread otherwise sleeps inside
//! `GetMessageW`, like the tray icon's and the shortcut's threads do.
//!
//! The window belongs to that thread, so the control handle below never touches
//! it directly: showing and hiding post a message to the thread's own queue, the
//! same way dropping the handle posts `WM_QUIT`.
//!
//! Two of the four things the user can do here are decisions this module is
//! allowed to make on its own, because they need nothing from the thread that
//! owns the interface: picking a row sends `UiCommand::SwitchEndpoint`, and
//! finishing a drag sends `UiCommand::SetWidgetPosition`. The other two —
//! opening the overlay and hiding the widget — are raised as a `UiSignal`,
//! because acting on them means starting the panel or changing this window's
//! own visibility, neither of which belongs here.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::Sender;
use resonance_core::messages::{EndpointId, EndpointState, Snapshot, UiCommand};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GdiFlush,
    GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DIB_RGB_COLORS, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE,
    DT_VCENTER, FW_NORMAL, HDC, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
// Declared with the common controls rather than with the other window
// messages, because tracking the pointer leaving a window arrived with them.
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetWindowRect, KillTimer, LoadCursorW,
    PostMessageW, PostQuitMessage, PostThreadMessageW, RegisterClassExW, SetForegroundWindow,
    SetTimer, ShowWindow, SystemParametersInfoW, TrackPopupMenuEx, TranslateMessage,
    UnregisterClassW, UpdateLayeredWindow, IDC_ARROW, MF_STRING, MSG, SPI_GETWORKAREA, SW_HIDE,
    SW_SHOWNA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, TPM_RETURNCMD, TPM_RIGHTBUTTON, ULW_ALPHA,
    WM_APP, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_NULL, WM_QUIT, WM_RBUTTONUP,
    WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::app::lock;
use crate::icon::{widget_badge, Canvas, BACKDROP_ALPHA, WIDGET_SIZE};
use crate::{theme, wake, UiSignal, Waker};

/// Window class name. Only one widget ever exists, so the class is registered
/// once when its thread starts and released when the thread ends.
const CLASS_NAME: PCWSTR = w!("ResonanceCornerWidget");

/// The badge's edge length at rest.
const REST_SIZE: i32 = WIDGET_SIZE as i32;

/// Distance from the work area's edges for the default docked position.
const MARGIN: i32 = 16;

/// Width of the grown panel.
///
/// Endpoint names are long and unabbreviated — "Speakers (Realtek(R) Audio)"
/// is a typical one — so the panel is wide enough to show a useful prefix of
/// one, with the tail ellipsised, without becoming a second overlay window.
const PANEL_WIDTH: i32 = 260;

/// Height of one row. Tall enough that a 13 px font sits in it with clear air
/// above and below, and comfortably past the ~24 px that makes a row awkward
/// to hit with the mouse.
const ROW_HEIGHT: i32 = 28;

/// Air between the panel's edge and its rows.
const PANEL_PADDING: i32 = 8;

/// The level glyph that marks each row.
const ROW_GLYPH_SIZE: i32 = 20;

/// Where a row's text starts, measured from the panel's left edge.
const ROW_TEXT_LEFT: i32 = PANEL_PADDING + ROW_GLYPH_SIZE + 10;

/// Corner radius of the grown panel.
const PANEL_RADIUS: f32 = 12.0;

/// Rows past this are not shown. A machine with a dozen endpoints would
/// otherwise grow a panel taller than the screen; the full panel lists them
/// all, and this is a shortcut, not a replacement for it.
const MAX_ROWS: usize = 8;

/// Font cell height, in pixels, as `CreateFontW` takes it (negative asks for a
/// character height rather than a cell height). Roughly 10 pt at 96 dpi.
const FONT_HEIGHT: i32 = -13;

/// How far the pointer may travel between press and release and still count as
/// a click rather than a drag. Small enough that a deliberate drag is never
/// mistaken for a click, large enough to absorb the hand tremor that moves a
/// pointer a pixel or two while pressing a button.
const DRAG_THRESHOLD: i32 = 4;

/// How long the pointer has to stay on the badge before it grows.
///
/// Without a dwell the widget would expand the instant the pointer crossed it
/// on its way somewhere else, and — because dragging is deliberately only
/// offered while the badge is at rest — there would be no moment left in which
/// the user could start a drag.
const HOVER_DWELL_MS: u32 = 250;

/// Steps the grow/shrink animation takes, and the gap between them: about
/// 125 ms end to end. Enough to read as a deliberate expansion rather than a
/// glitch, far short of long enough to feel slow.
const ANIMATION_STEPS: i32 = 5;
const ANIMATION_TICK_MS: u32 = 25;

const TIMER_DWELL: usize = 1;
const TIMER_ANIMATION: usize = 2;

/// The one command in the right-click menu. Any non-zero value works;
/// `TrackPopupMenuEx` reports zero when nothing was chosen.
const HIDE_COMMAND_ID: usize = 1;

/// Asks the widget thread to show the window. Posted to the thread's queue,
/// not sent to the window, because the caller is not the thread that owns it.
const WM_WIDGET_SHOW: u32 = WM_APP + 1;

/// Asks the widget thread to hide the window.
const WM_WIDGET_HIDE: u32 = WM_APP + 2;

// ----------------------------------------------------------------- state

thread_local! {
    /// What the window procedure needs in order to report what the user did.
    ///
    /// The procedure is a bare function pointer with no room for state, and it
    /// only ever runs on the thread that created the window — the same thread
    /// that fills this in — so a thread-local is both sufficient and the least
    /// machinery. Nothing is stored in the window itself.
    static CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) };

    /// Everything about the widget that changes while it runs.
    ///
    /// Kept apart from [`CONTEXT`] because this one is written, not just read:
    /// borrows of it are taken for single statements that touch no Win32 call,
    /// so a message arriving re-entrantly — which happens, see
    /// [`show_context_menu`] — can never find it already borrowed.
    static RUNTIME: RefCell<Runtime> = const { RefCell::new(Runtime::new()) };
}

#[derive(Clone)]
struct Context {
    signal_tx: Sender<UiSignal>,
    ui_cmd_tx: Sender<UiCommand>,
    waker: Waker,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
}

/// One line of the grown list.
#[derive(Clone)]
struct Row {
    /// `None` for the placeholder shown when there is nothing to list, which
    /// is what keeps it from being clickable.
    id: Option<EndpointId>,
    name: String,
    is_default: bool,
}

/// A drag in progress.
#[derive(Clone, Copy)]
struct Drag {
    /// Pointer position, in screen coordinates, when the button went down.
    press: (i32, i32),
    /// Where the window was at that moment.
    origin: (i32, i32),
    /// Whether the pointer has since travelled past [`DRAG_THRESHOLD`].
    moved: bool,
}

struct Runtime {
    /// The window's top-left corner *at rest*. The grown panel is positioned
    /// relative to this, and it is what gets persisted.
    origin: (i32, i32),
    /// How far through the grow animation the widget is, from 0 (resting
    /// badge) to [`ANIMATION_STEPS`] (the full list).
    progress: i32,
    /// Where `progress` is heading. Changing this mid-animation reverses it
    /// from wherever it currently is rather than restarting.
    target: i32,
    rows: Vec<Row>,
    hovered_row: Option<usize>,
    drag: Option<Drag>,
    /// Whether `TrackMouseEvent` is currently armed, so it is asked for once
    /// per entry rather than once per mouse move.
    tracking: bool,
    /// Set for the duration of `TrackPopupMenuEx`. The pointer moving onto
    /// the menu itself fires the same `WM_MOUSELEAVE` a real exit would, and
    /// without this the list would visibly collapse behind the menu it was
    /// just right-clicked from.
    menu_open: bool,
}

impl Runtime {
    const fn new() -> Self {
        Self {
            origin: (0, 0),
            progress: 0,
            target: 0,
            rows: Vec::new(),
            hovered_row: None,
            drag: None,
            tracking: false,
            menu_open: false,
        }
    }

    /// Whether the widget is showing its resting badge and nothing else, which
    /// is the only state a drag may start from.
    fn at_rest(&self) -> bool {
        self.progress == 0
    }

    fn fully_grown(&self) -> bool {
        self.progress == ANIMATION_STEPS
    }
}

/// Takes a copy of this thread's context, if the window has one.
///
/// A copy rather than a borrow: showing the menu below pumps messages from
/// inside the window procedure, so the procedure can re-enter while it is
/// already running, and a live `RefCell` borrow held across that would panic.
fn context() -> Option<Context> {
    CONTEXT.with(|context| context.borrow().clone())
}

/// Reads or updates the runtime state.
///
/// Every caller keeps the closure free of Win32 calls: a call that pumps
/// messages would re-enter the window procedure while this borrow is live.
fn with_runtime<R>(f: impl FnOnce(&mut Runtime) -> R) -> R {
    RUNTIME.with(|runtime| f(&mut runtime.borrow_mut()))
}

fn signal(context: &Context, signal: UiSignal) {
    if context.signal_tx.send(signal).is_err() {
        tracing::warn!("the interface is no longer listening for widget signals");
        return;
    }

    // Nudges the overlay if one is open, exactly as the tray icon does. When
    // it is closed there is nothing to repaint and the signal simply waits on
    // the queue.
    wake(&context.waker);
}

fn command(context: &Context, command: UiCommand) {
    if context.ui_cmd_tx.send(command).is_err() {
        tracing::warn!("the backend is no longer accepting commands from the widget");
    }
}

// ---------------------------------------------------------------- handle

/// A running corner widget. Dropping it stops the thread, which destroys the
/// window and takes the icon off the screen.
pub(crate) struct Widget {
    thread_id: u32,
    handle: Option<JoinHandle<()>>,
}

impl Widget {
    /// Puts the widget back on screen.
    pub(crate) fn show(&self) {
        self.post(WM_WIDGET_SHOW);
    }

    /// Takes the widget off screen. The thread and the window both stay alive,
    /// so showing it again costs nothing.
    pub(crate) fn hide(&self) {
        self.post(WM_WIDGET_HIDE);
    }

    fn post(&self, message: u32) {
        // SAFETY: Posting to the widget thread's message queue. The id was
        // captured on that thread, and that thread is joined only in `drop`,
        // which takes `&mut self` and so cannot overlap with this call; the id
        // therefore cannot have been recycled. No pointer crosses the call.
        let posted = unsafe { PostThreadMessageW(self.thread_id, message, WPARAM(0), LPARAM(0)) };

        if posted.is_err() {
            tracing::warn!(message, "could not signal the widget thread");
        }
    }
}

impl Drop for Widget {
    fn drop(&mut self) {
        // SAFETY: Posting WM_QUIT to the widget thread's queue. The id was
        // captured on that thread and the thread is still alive because we are
        // about to join it, so the id cannot have been recycled yet.
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };

        if posted.is_err() {
            tracing::warn!("could not signal the widget thread to stop");
        }

        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Creates the widget on its own thread and starts pumping messages for it.
///
/// The window, its class and its bitmaps are all built inside the thread and
/// never leave it: a window belongs to the thread that created it, and its
/// messages are delivered to that thread's queue, which is what the loop below
/// serves.
///
/// `visible` and `position` are the saved settings. The window is always
/// created — hiding is only a `ShowWindow` call away, and a hidden window costs
/// a handle and nothing else — so the tray menu can bring it back without
/// rebuilding it. `snapshot` is the same shared latest-snapshot cell the
/// overlay panel reads; this thread only ever reads it.
pub(crate) fn spawn(
    signal_tx: Sender<UiSignal>,
    ui_cmd_tx: Sender<UiCommand>,
    waker: Waker,
    snapshot: Arc<Mutex<Option<Snapshot>>>,
    visible: bool,
    position: Option<(f32, f32)>,
) -> Result<Widget, String> {
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<u32, String>>(1);

    let handle = std::thread::Builder::new()
        .name("resonance-widget".into())
        .spawn(move || {
            // SAFETY: GetCurrentThreadId reads the calling thread's own id and
            // cannot fail or touch memory we own.
            let thread_id = unsafe { GetCurrentThreadId() };

            let origin = initial_origin(position);
            with_runtime(|runtime| runtime.origin = origin);

            let (instance, hwnd) = match build_window(origin) {
                Ok(window) => window,
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                    return;
                }
            };

            CONTEXT.with(|context| {
                *context.borrow_mut() = Some(Context {
                    signal_tx,
                    ui_cmd_tx,
                    waker,
                    snapshot,
                });
            });

            set_visible(hwnd, visible);

            if ready_tx.send(Ok(thread_id)).is_err() {
                destroy_window(instance, hwnd);
                return;
            }

            pump_messages(hwnd);
            destroy_window(instance, hwnd);
        })
        .map_err(|e| e.to_string())?;

    match ready_rx.recv() {
        Ok(Ok(thread_id)) => Ok(Widget {
            thread_id,
            handle: Some(handle),
        }),
        Ok(Err(err)) => {
            let _ = handle.join();
            Err(err)
        }
        Err(_) => {
            let _ = handle.join();
            Err("the widget thread stopped before it was ready".to_owned())
        }
    }
}

// -------------------------------------------------------------- geometry

/// The primary monitor's work area, or `None` if the system will not say.
///
/// The work area rather than the whole screen, so the widget does not end up
/// underneath the taskbar.
fn work_area() -> Option<RECT> {
    let mut area = RECT::default();

    // SAFETY: SPI_GETWORKAREA writes a RECT through `pvparam`; `area` is a
    // live, correctly sized RECT owned by this frame for the whole call, and
    // nothing is being changed, so no update flags are needed.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(std::ptr::from_mut(&mut area).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };

    if read.is_err() {
        tracing::warn!("could not read the desktop work area");
        return None;
    }

    Some(area)
}

/// Keeps a rectangle of this size fully inside the work area.
fn clamp_to_work_area(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    let Some(area) = work_area() else {
        return (x.max(0), y.max(0));
    };

    let max_x = (area.right - width).max(area.left);
    let max_y = (area.bottom - height).max(area.top);
    (x.clamp(area.left, max_x), y.clamp(area.top, max_y))
}

/// Where the badge sits when the widget starts.
///
/// A saved position is honoured but still clamped, because the display it was
/// saved on may since have been resized, rotated or unplugged — a widget
/// restored off screen would be unreachable and look like a lost setting.
fn initial_origin(position: Option<(f32, f32)>) -> (i32, i32) {
    if let Some((x, y)) = position {
        return clamp_to_work_area(x.round() as i32, y.round() as i32, REST_SIZE, REST_SIZE);
    }

    match work_area() {
        Some(area) => (
            area.right - REST_SIZE - MARGIN,
            area.bottom - REST_SIZE - MARGIN,
        ),
        None => (MARGIN, MARGIN),
    }
}

/// How tall the panel is for a given number of rows.
fn panel_height(rows: usize) -> i32 {
    PANEL_PADDING * 2 + rows.clamp(1, MAX_ROWS) as i32 * ROW_HEIGHT
}

/// The panel's rectangle when fully grown.
///
/// It keeps the badge's bottom-right corner, so the list unfolds up and to the
/// left out of the badge rather than shoving it sideways, and is then clamped
/// into the work area for the case where the widget was dragged somewhere that
/// leaves no room in that direction.
fn grown_rect(origin: (i32, i32), rows: usize) -> (i32, i32, i32, i32) {
    let width = PANEL_WIDTH;
    let height = panel_height(rows);
    let (x, y) = clamp_to_work_area(
        origin.0 + REST_SIZE - width,
        origin.1 + REST_SIZE - height,
        width,
        height,
    );
    (x, y, width, height)
}

/// Smoothstep, so the grow starts and finishes gently instead of arriving at a
/// hard stop.
fn eased(progress: i32) -> f32 {
    let t = (progress as f32 / ANIMATION_STEPS as f32).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lerp(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// The window's rectangle at the current point in the animation.
fn current_rect(origin: (i32, i32), rows: usize, progress: i32) -> (i32, i32, i32, i32) {
    if progress <= 0 {
        return (origin.0, origin.1, REST_SIZE, REST_SIZE);
    }

    let (gx, gy, gw, gh) = grown_rect(origin, rows);
    if progress >= ANIMATION_STEPS {
        return (gx, gy, gw, gh);
    }

    let t = eased(progress);
    (
        lerp(origin.0 as f32, gx as f32, t).round() as i32,
        lerp(origin.1 as f32, gy as f32, t).round() as i32,
        lerp(REST_SIZE as f32, gw as f32, t).round() as i32,
        lerp(REST_SIZE as f32, gh as f32, t).round() as i32,
    )
}

/// Which row a point in the panel falls in, if any.
fn row_at(rows: usize, height: i32, y: i32) -> Option<usize> {
    if y < PANEL_PADDING || y >= height - PANEL_PADDING {
        return None;
    }

    let index = ((y - PANEL_PADDING) / ROW_HEIGHT) as usize;
    (index < rows).then_some(index)
}

// --------------------------------------------------------------- reading

/// The endpoints to list, read from the shared snapshot.
///
/// Only the endpoints that are actually present are offered: switching to one
/// that is unplugged or disabled would fail, and listing it invites exactly
/// that. An empty list becomes one unclickable placeholder row, which is
/// clearer than a panel with nothing in it.
fn read_rows(snapshot: &Arc<Mutex<Option<Snapshot>>>) -> Vec<Row> {
    // The guard is released at the end of this statement, before anything is
    // drawn: the snapshot is shared with the thread that produces it, and no
    // drawing or Win32 call happens while it is held.
    let current = lock(snapshot).clone();

    let mut rows: Vec<Row> = current
        .as_ref()
        .map(|snapshot| {
            snapshot
                .endpoints
                .iter()
                .filter(|endpoint| endpoint.state == EndpointState::Active)
                .take(MAX_ROWS)
                .map(|endpoint| Row {
                    id: Some(Arc::clone(&endpoint.id)),
                    name: endpoint.friendly_name.to_string(),
                    is_default: snapshot.default_endpoint.as_ref() == Some(&endpoint.id),
                })
                .collect()
        })
        .unwrap_or_default();

    if rows.is_empty() {
        rows.push(Row {
            id: None,
            name: "No devices".to_owned(),
            is_default: false,
        });
    }

    rows
}

// -------------------------------------------------------------- painting

/// A line of text for GDI to draw, once the bitmap underneath it is in place.
struct TextRun {
    rect: RECT,
    text: Vec<u16>,
    colour: COLORREF,
}

/// One composed frame: the pixels, the text still to be drawn over them, and
/// where the window should be when it appears.
struct Frame {
    canvas: Canvas,
    texts: Vec<TextRun>,
    position: (i32, i32),
}

fn colourref(colour: egui::Color32) -> COLORREF {
    let [r, g, b, _] = colour.to_srgba_unmultiplied();
    COLORREF(u32::from(r) | (u32::from(g) << 8) | (u32::from(b) << 16))
}

fn rgba(colour: egui::Color32) -> [u8; 4] {
    colour.to_srgba_unmultiplied()
}

/// Composes whatever the widget currently looks like.
fn compose(origin: (i32, i32), rows: &[Row], hovered_row: Option<usize>, progress: i32) -> Frame {
    let (x, y, width, height) = current_rect(origin, rows.len(), progress);

    if progress >= ANIMATION_STEPS {
        return compose_panel(x, y, width, height, rows, hovered_row);
    }

    // Badge, or something on its way between badge and panel. The corner
    // radius travels from half the badge's edge — which makes the rounded
    // rectangle a circle — down to the panel's, and the backdrop firms up to
    // fully opaque as it goes, so the two ends meet without a visible jump.
    let t = eased(progress);
    let canvas = widget_badge(
        width as u32,
        height as u32,
        lerp(REST_SIZE as f32 / 2.0, PANEL_RADIUS, t),
        lerp(BACKDROP_ALPHA, 255.0, t),
        // Gone well before the panel arrives: the bars belong to the badge,
        // and lingering they would sit on top of the rows.
        (1.0 - t * 2.0).clamp(0.0, 1.0),
    );

    Frame {
        canvas,
        texts: Vec::new(),
        position: (x, y),
    }
}

/// The grown panel: one row per endpoint.
///
/// The backdrop is fully opaque here, unlike the badge. Text is drawn by GDI,
/// which writes colour straight rather than multiplied by any alpha, so text
/// over a partly transparent backdrop would come out too bright; an opaque
/// panel makes GDI's output exactly right, and reads better under text anyway.
fn compose_panel(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    rows: &[Row],
    hovered_row: Option<usize>,
) -> Frame {
    let mut canvas = Canvas::new(width as u32, height as u32);
    canvas.fill_rounded(PANEL_RADIUS, rgba(theme::SURFACE.to_opaque()));

    let mut texts = Vec::with_capacity(rows.len());
    let row_width = (width - PANEL_PADDING * 2).max(0) as u32;

    for (index, row) in rows.iter().enumerate() {
        let top = PANEL_PADDING + index as i32 * ROW_HEIGHT;

        // The endpoint in use is marked with the same accent wash the full
        // panel uses for a selected row, so the two read the same way.
        if row.is_default {
            canvas.fill_rect(
                PANEL_PADDING,
                top,
                row_width,
                ROW_HEIGHT as u32,
                rgba(theme::ACCENT_TINT),
            );
        }

        if hovered_row == Some(index) && row.id.is_some() {
            canvas.fill_rect(
                PANEL_PADDING,
                top,
                row_width,
                ROW_HEIGHT as u32,
                rgba(theme::HAIRLINE),
            );
        }

        let glyph_colour = if row.is_default {
            theme::ACCENT
        } else if row.id.is_some() {
            theme::TEXT_SECONDARY
        } else {
            theme::TEXT_DISABLED
        };
        canvas.draw_level_glyph(
            PANEL_PADDING + 2,
            top + (ROW_HEIGHT - ROW_GLYPH_SIZE) / 2,
            ROW_GLYPH_SIZE as u32,
            rgba(glyph_colour),
        );

        let text_colour = if row.id.is_some() {
            theme::TEXT_PRIMARY
        } else {
            theme::TEXT_DISABLED
        };
        texts.push(TextRun {
            rect: RECT {
                left: ROW_TEXT_LEFT,
                top,
                right: width - PANEL_PADDING,
                bottom: top + ROW_HEIGHT,
            },
            text: row.name.encode_utf16().collect(),
            colour: colourref(text_colour),
        });
    }

    Frame {
        canvas,
        texts,
        position: (x, y),
    }
}

/// Recomposes and pushes a frame for whatever state the widget is in now.
fn repaint(hwnd: HWND) {
    let (origin, rows, hovered_row, progress) = with_runtime(|runtime| {
        (
            runtime.origin,
            runtime.rows.clone(),
            runtime.hovered_row,
            runtime.progress,
        )
    });

    let frame = compose(origin, &rows, hovered_row, progress);
    if let Err(err) = paint(hwnd, &frame) {
        tracing::warn!(error = %err, "could not redraw the widget");
    }
}

/// Pushes a frame into the layered window, resizing and moving it to match.
///
/// `UpdateLayeredWindow` takes its pixels from a device context holding a
/// top-down 32-bit DIB section, and — with `ULW_ALPHA` and an `AC_SRC_ALPHA`
/// blend — expects those pixels premultiplied. It copies them into the window's
/// own surface, so everything allocated here is released again before
/// returning; the window keeps its picture with no bitmap of ours alive. Given
/// a position and a size it also moves and resizes the window, which is what
/// makes each animation step one atomic change rather than a resize the user
/// could catch mid-repaint.
fn paint(hwnd: HWND, frame: &Frame) -> Result<(), String> {
    // SAFETY: A null window asks for a device context for the whole screen,
    // which is only used here as the format reference for the memory context
    // and the blend below; it is released at the end of this function.
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return Err("could not obtain a screen device context".to_owned());
    }

    let result = paint_into(hwnd, screen_dc, frame);

    // SAFETY: Releasing the context obtained above, with the same null window
    // it was obtained for, on the thread that obtained it.
    unsafe { ReleaseDC(None, screen_dc) };

    result
}

fn paint_into(hwnd: HWND, screen_dc: HDC, frame: &Frame) -> Result<(), String> {
    let width = frame.canvas.width;
    let height = frame.canvas.height;
    let pixels = premultiplied_bgra(&frame.canvas.pixels);
    let alpha_mask = frame.canvas.alpha_mask();

    // SAFETY: `screen_dc` is a valid context obtained by the caller and stays
    // valid for this whole call; the memory context returned is deleted below.
    let memory_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if memory_dc.is_invalid() {
        return Err("could not create a memory device context".to_owned());
    }

    // A negative height is what makes the rows run top down, matching the
    // order the canvas is generated in. 32 bits per pixel with no compression
    // is the only format `UpdateLayeredWindow` blends per pixel.
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();

    // SAFETY: `info` describes the bitmap being asked for and outlives the
    // call; `bits` receives a pointer to memory the bitmap owns, valid until
    // the bitmap is deleted below. No file mapping is involved.
    let bitmap = match unsafe {
        CreateDIBSection(Some(memory_dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
    } {
        Ok(bitmap) => bitmap,
        Err(err) => {
            // SAFETY: Deleting the memory context created above, which nothing
            // has been selected into yet.
            let _ = unsafe { DeleteDC(memory_dc) };
            return Err(err.to_string());
        }
    };

    let expected = (width as usize) * (height as usize) * 4;
    if bits.is_null() || pixels.len() != expected {
        // SAFETY: Releasing the two objects created above, neither of which is
        // selected anywhere.
        unsafe {
            let _ = DeleteObject(HGDIOBJ::from(bitmap));
            let _ = DeleteDC(memory_dc);
        }
        return Err("the widget bitmap came back the wrong size".to_owned());
    }

    // SAFETY: `bits` points at the bitmap's own pixel buffer, which
    // CreateDIBSection sized at exactly `expected` bytes for the header above
    // (32 bits per pixel, so every row is already a multiple of four bytes and
    // carries no padding). `pixels` holds exactly that many bytes, checked
    // just above, and the two regions belong to separate allocations.
    unsafe { std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), expected) };

    // SAFETY: Selecting the bitmap into the memory context we just created it
    // for; the previous object is kept so it can be put back before the
    // bitmap is deleted, which is what makes the bitmap safe to delete.
    let previous = unsafe { SelectObject(memory_dc, HGDIOBJ::from(bitmap)) };

    draw_texts(memory_dc, &frame.texts);

    // SAFETY: GDI batches its drawing; flushing makes every call made above
    // land in the bitmap before its bytes are touched directly below.
    let _ = unsafe { GdiFlush() };

    // SAFETY: `bits` is the bitmap's own buffer, still alive and still
    // selected into `memory_dc`, and `expected` is its exact length as
    // established above. Nothing else aliases it on this thread, and the
    // borrow ends before the bitmap is released.
    let dib = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), expected) };
    restore_alpha(dib, &alpha_mask);

    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        // The per-pixel alpha carries the whole shape; nothing further is
        // applied on top of it.
        SourceConstantAlpha: 0xFF,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };

    let size = SIZE {
        cx: width as i32,
        cy: height as i32,
    };
    let destination = POINT {
        x: frame.position.0,
        y: frame.position.1,
    };
    let source = POINT { x: 0, y: 0 };

    // SAFETY: Every pointer passed in refers to a local that outlives the
    // call. The source context holds the bitmap selected above, and the colour
    // key is unused because the blend is per-pixel alpha.
    let updated = unsafe {
        UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            Some(&destination),
            Some(&size),
            Some(memory_dc),
            Some(&source),
            COLORREF(0),
            Some(&blend),
            ULW_ALPHA,
        )
    };

    // SAFETY: Putting the original object back before deleting ours, then
    // deleting the bitmap and the context in that order. Both handles were
    // created here and neither is selected anywhere else.
    unsafe {
        SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ::from(bitmap));
        let _ = DeleteDC(memory_dc);
    }

    updated.map_err(|e| e.to_string())
}

/// Draws each row's label onto the bitmap already selected into `dc`.
fn draw_texts(dc: HDC, texts: &[TextRun]) {
    if texts.is_empty() {
        return;
    }

    // A face asked for by name, rather than the default GUI font, because the
    // stock one is a small bitmap face that looks nothing like the rest of
    // Windows. Grayscale antialiasing rather than subpixel: the panel is
    // composed off screen, so there is no known subpixel geometry to match.
    // SAFETY: Every argument is a plain integer or one of this crate's
    // constants, and the face name is a static literal the call only reads.
    let font = unsafe {
        CreateFontW(
            FONT_HEIGHT,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            0,
            w!("Segoe UI"),
        )
    };

    if font.is_invalid() {
        tracing::warn!("could not create the widget's font; its rows will have no labels");
        return;
    }

    // SAFETY: Selecting the font created above into the caller's context, and
    // asking for text to be drawn over the bitmap rather than filling the
    // cell behind each glyph, which would punch a rectangle through the panel.
    let previous_font = unsafe { SelectObject(dc, HGDIOBJ::from(font)) };
    // SAFETY: `dc` is a live memory context owned by this thread.
    unsafe { SetBkMode(dc, TRANSPARENT) };

    for run in texts {
        let mut rect = run.rect;
        let mut text = run.text.clone();

        // SAFETY: `dc` is live, `text` and `rect` are locals that outlive the
        // call, and the length of the slice is what tells DrawTextW how much
        // to read — the buffer is deliberately not null terminated.
        unsafe {
            SetTextColor(dc, run.colour);
            DrawTextW(
                dc,
                &mut text,
                &mut rect,
                DT_LEFT | DT_SINGLELINE | DT_VCENTER | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
    }

    // SAFETY: Putting the previous font back before deleting ours; a font
    // still selected into a context cannot be deleted.
    unsafe {
        SelectObject(dc, previous_font);
        let _ = DeleteObject(HGDIOBJ::from(font));
    }
}

/// Writes the composed alpha channel back over the bitmap.
///
/// GDI's text drawing writes colour into a 32-bit bitmap without preserving
/// the fourth byte, so by this point the alpha under every label is whatever
/// GDI happened to leave. The shape was computed before the text was drawn and
/// is simply restored here; wherever text landed the alpha is fully opaque, so
/// the colour GDI wrote is already the correct premultiplied value.
fn restore_alpha(pixels: &mut [u8], mask: &[u8]) {
    if pixels.len() != mask.len() * 4 {
        return;
    }

    for (index, alpha) in mask.iter().enumerate() {
        pixels[index * 4 + 3] = *alpha;
    }
}

/// Converts the composed straight-alpha RGBA to the premultiplied BGRA a
/// layered window blends.
///
/// Two changes at once, and both matter: the channels are stored blue first,
/// and each colour channel is scaled by its own alpha. Skipping the
/// multiplication does not fail — it draws, with a bright halo around every
/// soft edge — which is why it is a separate, tested function rather than a
/// loop inside the painting code.
fn premultiplied_bgra(rgba: &[u8]) -> Vec<u8> {
    let mut bgra = Vec::with_capacity(rgba.len());

    for pixel in rgba.as_chunks::<4>().0 {
        let alpha = pixel[3];
        bgra.push(premultiply(pixel[2], alpha));
        bgra.push(premultiply(pixel[1], alpha));
        bgra.push(premultiply(pixel[0], alpha));
        bgra.push(alpha);
    }

    bgra
}

/// One channel scaled by an alpha, rounded to nearest rather than truncated so
/// that a fully opaque pixel comes back unchanged.
fn premultiply(channel: u8, alpha: u8) -> u8 {
    ((u32::from(channel) * u32::from(alpha) + 127) / 255) as u8
}

// ---------------------------------------------------------------- window

/// Registers the class, creates the window and paints it once.
fn build_window(origin: (i32, i32)) -> Result<(HINSTANCE, HWND), String> {
    // SAFETY: A null module name asks for the handle of this process's own
    // executable, which is always loaded, so no pointer is passed in and the
    // returned handle needs no release.
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
    let instance = HINSTANCE::from(module);

    // SAFETY: IDC_ARROW is one of the predefined cursor identifiers, which are
    // passed by value in place of a string pointer; a null instance is what
    // selects the system's own cursors.
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(|e| e.to_string())?;

    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hCursor: cursor,
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };

    // SAFETY: `class` is a fully initialised WNDCLASSEXW that outlives the
    // call, and every pointer inside it — the class name literal and the
    // window procedure — is static. The system copies the structure.
    let atom = unsafe { RegisterClassExW(&class) };
    if atom == 0 {
        return Err("could not register the widget window class".to_owned());
    }

    // SAFETY: Both string pointers are static literals, the parent, menu and
    // creation parameter are absent, and the instance handle is this process's
    // own. The window is created on, and stays on, this thread.
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS_NAME,
            w!("Resonance"),
            WS_POPUP,
            origin.0,
            origin.1,
            REST_SIZE,
            REST_SIZE,
            None,
            None,
            Some(instance),
            None,
        )
    }
    .map_err(|e| e.to_string())?;

    let frame = compose(origin, &[], None, 0);
    if let Err(err) = paint(hwnd, &frame) {
        destroy_window(instance, hwnd);
        return Err(err);
    }

    Ok((instance, hwnd))
}

fn set_visible(hwnd: HWND, visible: bool) {
    let command = if visible { SW_SHOWNA } else { SW_HIDE };

    // SAFETY: `hwnd` belongs to this thread and is alive for as long as the
    // pump this runs from. Showing without activating keeps the widget from
    // stealing focus from whatever the user is working in.
    let _ = unsafe { ShowWindow(hwnd, command) };
}

fn destroy_window(instance: HINSTANCE, hwnd: HWND) {
    // SAFETY: Destroying a window on the thread that created it, which is the
    // only thread allowed to, after its message pump has stopped.
    if unsafe { DestroyWindow(hwnd) }.is_err() {
        tracing::warn!("could not destroy the widget window");
    }

    // SAFETY: Releasing the class registered by this thread for this process.
    // The window using it has just been destroyed, which is the condition for
    // unregistering; a failure here only leaks a class until the process ends.
    if unsafe { UnregisterClassW(CLASS_NAME, Some(instance)) }.is_err() {
        tracing::trace!("the widget window class was still in use");
    }
}

/// Standard message pump, with two thread messages of our own.
///
/// Show and hide arrive as thread messages rather than window messages because
/// they come from another thread, which may not touch this window. They have
/// no window to be dispatched to, so they are handled here instead of in the
/// window procedure.
fn pump_messages(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a live, correctly aligned MSG for the duration of
        // the call. A null window filter asks for every message belonging to
        // this thread, which covers both the window's and our own.
        let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };

        // Zero means WM_QUIT was received; -1 means the queue broke. Either
        // way this thread is done.
        if result.0 <= 0 {
            break;
        }

        match msg.message {
            WM_WIDGET_SHOW => set_visible(hwnd, true),
            WM_WIDGET_HIDE => set_visible(hwnd, false),
            // SAFETY: `msg` was just filled in by a successful GetMessageW
            // call and stays valid and owned by this thread across both calls.
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }
}

// ----------------------------------------------------------- interaction

/// The client-relative point a mouse message carries.
fn client_point(lparam: LPARAM) -> (i32, i32) {
    let packed = lparam.0 as u32;
    (
        (packed & 0xFFFF) as u16 as i16 as i32,
        ((packed >> 16) & 0xFFFF) as u16 as i16 as i32,
    )
}

/// Where the pointer is, in screen coordinates.
fn cursor_position() -> Option<(i32, i32)> {
    let mut cursor = POINT::default();

    // SAFETY: `cursor` is a live POINT owned by this frame for the whole call.
    if unsafe { GetCursorPos(&mut cursor) }.is_err() {
        return None;
    }

    Some((cursor.x, cursor.y))
}

/// Whether the pointer is currently within `hwnd`'s on-screen bounds.
///
/// Used only where a real `WM_MOUSELEAVE`/`WM_MOUSEMOVE` cannot be relied on
/// to answer this — e.g. right after a modal call like `TrackPopupMenuEx`
/// returns, which can swallow a leave event that fired while it was pumping.
/// Missing either coordinate (a transient `GetCursorPos`/`GetWindowRect`
/// failure) is treated as "not over it": failing to shrink a widget that
/// should have is a smaller mistake than leaving it stuck open.
fn cursor_is_over(hwnd: HWND) -> bool {
    let Some((x, y)) = cursor_position() else {
        return false;
    };

    let mut rect = RECT::default();
    // SAFETY: `hwnd` is this thread's own window, alive for the call, and
    // `rect` is a live RECT owned by this frame for its whole duration.
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }

    x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom
}

fn start_timer(hwnd: HWND, id: usize, interval: u32) {
    // SAFETY: `hwnd` is this thread's own window. Setting a timer that already
    // exists restarts it, which is exactly what the callers want; a null
    // callback asks for WM_TIMER on this window instead.
    let started = unsafe { SetTimer(Some(hwnd), id, interval, None) };
    if started == 0 {
        tracing::warn!(id, "could not start a widget timer");
    }
}

fn stop_timer(hwnd: HWND, id: usize) {
    // SAFETY: `hwnd` is this thread's own window. Killing a timer that is not
    // running is reported as an error and is otherwise harmless, which is why
    // the result is dropped.
    let _ = unsafe { KillTimer(Some(hwnd), id) };
}

/// Asks for a `WM_MOUSELEAVE` when the pointer next leaves the window.
///
/// Windows does not volunteer one: a window is told the pointer moved, over
/// and over, and never told that it stopped. Tracking has to be armed again
/// after every leave, which is what the `tracking` flag keeps straight.
fn arm_mouse_tracking(hwnd: HWND) {
    let mut tracking = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };

    // SAFETY: `tracking` is a live, fully initialised TRACKMOUSEEVENT owned by
    // this frame; the call reads it and records the request against `hwnd`,
    // which belongs to this thread.
    if unsafe { TrackMouseEvent(&mut tracking) }.is_err() {
        tracing::trace!("could not track the pointer leaving the widget");
    }
}

/// Starts a grow or a shrink.
///
/// Nothing here restarts the animation from either end: only the target moves,
/// so reversing mid-flight carries on from the size currently on screen
/// instead of jumping back to where the previous transition began.
fn animate_to(hwnd: HWND, target: i32) {
    let changed = with_runtime(|runtime| {
        if runtime.target == target {
            return false;
        }
        runtime.target = target;
        true
    });

    if changed {
        start_timer(hwnd, TIMER_ANIMATION, ANIMATION_TICK_MS);
    }
}

fn on_mouse_move(hwnd: HWND, lparam: LPARAM) {
    // A drag owns the pointer while it lasts; nothing else reacts to movement.
    let drag = with_runtime(|runtime| runtime.drag);
    if let Some(drag) = drag {
        continue_drag(hwnd, drag);
        return;
    }

    let arm = with_runtime(|runtime| {
        if runtime.tracking {
            return false;
        }
        runtime.tracking = true;
        true
    });

    if arm {
        arm_mouse_tracking(hwnd);
        // The pointer has to settle before the list opens, so that crossing
        // the badge does not expand it and so a drag still has a moment to
        // start in.
        start_timer(hwnd, TIMER_DWELL, HOVER_DWELL_MS);
    }

    // While the list is up, follow which row the pointer is over. This is the
    // only mouse movement that redraws anything, and only when the answer
    // actually changes.
    let (_, y) = client_point(lparam);
    let changed = with_runtime(|runtime| {
        if !runtime.fully_grown() {
            return false;
        }

        let height = panel_height(runtime.rows.len());
        let hovered =
            row_at(runtime.rows.len(), height, y).filter(|index| runtime.rows[*index].id.is_some());

        if hovered == runtime.hovered_row {
            return false;
        }
        runtime.hovered_row = hovered;
        true
    });

    if changed {
        repaint(hwnd);
    }
}

fn on_mouse_leave(hwnd: HWND) {
    let shrink = with_runtime(|runtime| {
        runtime.tracking = false;
        runtime.hovered_row = None;
        // A drag can take the pointer off the window; that is not a reason to
        // collapse anything, and the drag's own release will sort it out. The
        // context menu does the same thing for the same reason: the pointer
        // moving onto it is not the user backing away from the list.
        runtime.drag.is_none() && !runtime.menu_open
    });

    stop_timer(hwnd, TIMER_DWELL);

    if shrink {
        animate_to(hwnd, 0);
        repaint(hwnd);
    }
}

/// The pointer has stayed long enough: read the endpoints and open the list.
fn on_dwell_elapsed(hwnd: HWND) {
    stop_timer(hwnd, TIMER_DWELL);

    let Some(context) = context() else {
        return;
    };

    // Read outside the borrow below: this takes another lock, and holding two
    // at once is how deadlocks are built.
    let rows = read_rows(&context.snapshot);

    let grow = with_runtime(|runtime| {
        if runtime.drag.is_some() {
            return false;
        }
        runtime.rows = rows;
        runtime.hovered_row = None;
        true
    });

    if grow {
        animate_to(hwnd, ANIMATION_STEPS);
    }
}

fn on_animation_tick(hwnd: HWND) {
    let done = with_runtime(|runtime| {
        if runtime.progress < runtime.target {
            runtime.progress += 1;
        } else if runtime.progress > runtime.target {
            runtime.progress -= 1;
        }
        runtime.progress == runtime.target
    });

    if done {
        stop_timer(hwnd, TIMER_ANIMATION);
    }

    repaint(hwnd);
}

fn on_button_down(hwnd: HWND) {
    let Some(press) = cursor_position() else {
        return;
    };

    // Dragging is offered only from the resting badge. While the list is open
    // a press belongs to whichever row it landed on, and the two gestures
    // would otherwise be the same movement on the same window.
    let started = with_runtime(|runtime| {
        if !runtime.at_rest() {
            return false;
        }

        runtime.drag = Some(Drag {
            press,
            origin: runtime.origin,
            moved: false,
        });
        true
    });

    if started {
        // A pending grow would fire in the middle of the drag.
        stop_timer(hwnd, TIMER_DWELL);

        // SAFETY: `hwnd` belongs to this thread, so it may take the pointer.
        // Capture is released on the button going back up, on every path.
        unsafe { SetCapture(hwnd) };
    }
}

fn continue_drag(hwnd: HWND, drag: Drag) {
    let Some((x, y)) = cursor_position() else {
        return;
    };

    let dx = x - drag.press.0;
    let dy = y - drag.press.1;
    let moved = drag.moved || dx.abs() > DRAG_THRESHOLD || dy.abs() > DRAG_THRESHOLD;
    if !moved {
        return;
    }

    let origin = clamp_to_work_area(drag.origin.0 + dx, drag.origin.1 + dy, REST_SIZE, REST_SIZE);

    with_runtime(|runtime| {
        runtime.origin = origin;
        if let Some(drag) = runtime.drag.as_mut() {
            drag.moved = true;
        }
    });

    repaint(hwnd);
}

fn on_button_up(lparam: LPARAM) {
    let Some(context) = context() else {
        return;
    };

    let (_, y) = client_point(lparam);

    enum Outcome {
        Nothing,
        Click,
        Dropped((i32, i32)),
        Chose(EndpointId),
    }

    let outcome = with_runtime(|runtime| {
        if let Some(drag) = runtime.drag.take() {
            return if drag.moved {
                Outcome::Dropped(runtime.origin)
            } else {
                // Pressed and released without really moving: the user meant
                // to click the badge, not to move it.
                Outcome::Click
            };
        }

        if runtime.fully_grown() {
            let height = panel_height(runtime.rows.len());
            return match row_at(runtime.rows.len(), height, y)
                .and_then(|index| runtime.rows[index].id.clone())
            {
                Some(id) => Outcome::Chose(id),
                None => Outcome::Nothing,
            };
        }

        if runtime.at_rest() {
            return Outcome::Click;
        }

        Outcome::Nothing
    });

    if !matches!(outcome, Outcome::Nothing) {
        // SAFETY: Releasing a capture this thread may or may not hold; the
        // call reports an error in the latter case and changes nothing.
        let _ = unsafe { ReleaseCapture() };
    }

    match outcome {
        Outcome::Nothing => {}
        Outcome::Click => signal(&context, UiSignal::ToggleOverlay),
        Outcome::Dropped((x, y)) => {
            // Sent once, here, rather than on every intermediate move: the
            // channel and the store behind it only care where it came to rest.
            command(&context, UiCommand::SetWidgetPosition(x as f32, y as f32));
        }
        Outcome::Chose(id) => command(&context, UiCommand::SwitchEndpoint(id)),
    }

    // After a drag the pointer is still on the badge, and tracking was armed
    // before the press. Clearing it lets the next movement arm the dwell again
    // so the list can still be opened without moving away and back.
    with_runtime(|runtime| runtime.tracking = false);
}

/// Runs on the widget thread, from inside the pump above.
///
/// Both clicks are handled on button *up*: reacting to button-down would fire
/// before the user has committed to the click, and for the right button it
/// would leave the menu fighting the button release.
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            on_mouse_move(hwnd, lparam);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            on_mouse_leave(hwnd);
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                TIMER_DWELL => on_dwell_elapsed(hwnd),
                TIMER_ANIMATION => on_animation_tick(hwnd),
                other => tracing::trace!(id = other, "ignoring an unknown widget timer"),
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_button_down(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_button_up(lparam);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            show_context_menu(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: Posting a quit to this thread's own queue. Nothing is
            // read after the pump stops, so this only matters if the window is
            // destroyed from underneath us while the pump is still running.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: Handing every other message back to the default handler with
        // the parameters exactly as they were received.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Shows the one-item menu and reports the choice.
///
/// Choosing "Hide" raises a signal rather than hiding the window here: the
/// visibility is a saved setting, and only the loop holding the command sender
/// can record it. That loop hides the window as part of handling the signal.
fn show_context_menu(hwnd: HWND) {
    let Some(context) = context() else {
        return;
    };

    let Some((x, y)) = cursor_position() else {
        return;
    };

    // SAFETY: Creates an empty menu owned by this thread; it is destroyed on
    // every path out of this function.
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };

    // SAFETY: `menu` was just created, and the label is a static literal that
    // the system copies.
    let appended = unsafe { AppendMenuW(menu, MF_STRING, HIDE_COMMAND_ID, w!("Hide")) };

    if appended.is_err() {
        // SAFETY: Destroying the menu created just above, which is not on
        // screen and owns nothing else.
        let _ = unsafe { DestroyMenu(menu) };
        return;
    }

    // A popup menu is dismissed by clicking away from it only when its owner
    // is the foreground window; without this the menu stays up after the user
    // clicks elsewhere.
    // SAFETY: `hwnd` is this thread's own window and is alive here.
    let _ = unsafe { SetForegroundWindow(hwnd) };

    // Set for exactly the span `TrackPopupMenuEx` pumps messages: the pointer
    // moving from the badge onto the menu fires a real `WM_MOUSELEAVE`, and
    // without this the list would collapse behind the menu it was just
    // opened from.
    with_runtime(|runtime| runtime.menu_open = true);

    // SAFETY: `menu` and `hwnd` are both alive and owned by this thread. The
    // call pumps messages internally until the menu closes — which is why the
    // context was copied out of the thread-local above rather than borrowed,
    // and why no runtime borrow is held across it — and TPM_RETURNCMD makes it
    // report the chosen command instead of posting one.
    let chosen =
        unsafe { TrackPopupMenuEx(menu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, x, y, hwnd, None) };

    with_runtime(|runtime| runtime.menu_open = false);

    // The suppressed `WM_MOUSELEAVE` (if the pointer left for the menu) is
    // gone for good — Windows does not resend it — so whether to shrink now
    // has to be decided here instead, from where the pointer actually ended
    // up once the menu closed.
    if !cursor_is_over(hwnd) {
        animate_to(hwnd, 0);
        repaint(hwnd);
    }

    // The documented companion to taking the foreground above: it lets the
    // menu release properly when the user clicks away without choosing.
    // SAFETY: Posting an inert message to this thread's own window.
    let _ = unsafe { PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)) };

    // SAFETY: The menu is closed by now — TrackPopupMenuEx does not return
    // until it is — so destroying it releases nothing still in use.
    let _ = unsafe { DestroyMenu(menu) };

    // Zero means the menu was dismissed without a choice.
    if chosen.0 as usize == HIDE_COMMAND_ID {
        signal(&context, UiSignal::HideWidget);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The window, its class and its message pump are not exercised here: they
    // need a real window station and a running pump, which is also how the
    // tray icon's and the keyboard shortcut's own thread-owning parts are
    // treated in this crate. What is tested is the pure geometry and pixel
    // work — the parts that fail silently rather than loudly.

    fn rows(count: usize) -> Vec<Row> {
        (0..count)
            .map(|index| Row {
                id: Some(EndpointId::from(format!("endpoint-{index}").as_str())),
                name: format!("Device {index}"),
                is_default: index == 0,
            })
            .collect()
    }

    #[test]
    fn an_opaque_pixel_only_changes_channel_order() {
        // One opaque pixel, red.
        let bgra = premultiplied_bgra(&[0xFF, 0x00, 0x00, 0xFF]);
        assert_eq!(bgra, vec![0x00, 0x00, 0xFF, 0xFF]);

        // ...and one opaque grey, where the order alone would hide a mistake.
        let bgra = premultiplied_bgra(&[0x10, 0x20, 0x30, 0xFF]);
        assert_eq!(bgra, vec![0x30, 0x20, 0x10, 0xFF]);
    }

    #[test]
    fn a_transparent_pixel_loses_its_colour_entirely() {
        // White at zero alpha: every colour channel has to go to zero, or the
        // window shows a white fringe where the icon is meant to be clear.
        let bgra = premultiplied_bgra(&[0xFF, 0xFF, 0xFF, 0x00]);
        assert_eq!(bgra, vec![0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn a_partly_transparent_pixel_is_scaled_by_its_alpha() {
        let bgra = premultiplied_bgra(&[0xFF, 0xFF, 0xFF, 0x80]);
        assert_eq!(bgra, vec![0x80, 0x80, 0x80, 0x80]);

        // A quarter-opaque pure blue.
        let bgra = premultiplied_bgra(&[0x00, 0x00, 0xFF, 0x40]);
        assert_eq!(bgra, vec![0x40, 0x00, 0x00, 0x40]);
    }

    #[test]
    fn no_channel_of_a_composed_frame_exceeds_its_alpha() {
        // The invariant a layered window relies on. A channel above its own
        // alpha is what a missed multiplication looks like, and it shows up as
        // a bright halo rather than as a failure.
        for progress in 0..=ANIMATION_STEPS {
            let frame = compose((100, 100), &rows(3), Some(1), progress);
            let bgra = premultiplied_bgra(&frame.canvas.pixels);

            for (index, pixel) in bgra.as_chunks::<4>().0.iter().enumerate() {
                let alpha = pixel[3];
                assert!(
                    pixel[0] <= alpha && pixel[1] <= alpha && pixel[2] <= alpha,
                    "step {progress}, pixel {index} is not premultiplied: {pixel:?}"
                );
            }
        }
    }

    #[test]
    fn the_widget_grows_from_a_square_badge_into_a_wider_panel() {
        let origin = (400, 400);
        let list = rows(3);

        let (_, _, rest_w, rest_h) = current_rect(origin, list.len(), 0);
        assert_eq!((rest_w, rest_h), (REST_SIZE, REST_SIZE));

        let (_, _, grown_w, grown_h) = current_rect(origin, list.len(), ANIMATION_STEPS);
        assert_eq!(grown_w, PANEL_WIDTH);
        assert_eq!(grown_h, panel_height(3));

        // Every intermediate step lies between the two and never goes
        // backwards, which is what keeps the animation from stuttering.
        let mut previous = rest_w;
        for progress in 1..ANIMATION_STEPS {
            let (_, _, width, height) = current_rect(origin, list.len(), progress);
            assert!(width >= previous, "step {progress} shrank mid-grow");
            assert!(width <= grown_w && height <= grown_h);
            previous = width;
        }
    }

    #[test]
    fn the_panel_keeps_the_badges_bottom_right_corner() {
        // Grown from a badge in open space, the panel unfolds up and left, so
        // the corner the badge occupied stays put.
        let origin = (800, 700);
        let (x, y, width, height) = grown_rect(origin, 3);

        assert_eq!(x + width, origin.0 + REST_SIZE);
        assert_eq!(y + height, origin.1 + REST_SIZE);
    }

    #[test]
    fn a_panel_grown_from_a_corner_stays_on_screen() {
        // Dragged hard into the top-left, there is no room up and to the left,
        // so the panel has to give up the anchor rather than leave the screen.
        let (x, y, _, _) = grown_rect((0, 0), 4);

        assert!(x >= 0, "the panel ran off the left edge");
        assert!(y >= 0, "the panel ran off the top edge");
    }

    #[test]
    fn rows_are_found_by_where_they_are_on_the_panel() {
        let height = panel_height(3);

        // The padding above the first row and below the last belongs to no
        // row: clicking it must not switch anything.
        assert_eq!(row_at(3, height, 0), None);
        assert_eq!(row_at(3, height, height - 1), None);

        assert_eq!(row_at(3, height, PANEL_PADDING), Some(0));
        assert_eq!(row_at(3, height, PANEL_PADDING + ROW_HEIGHT - 1), Some(0));
        assert_eq!(row_at(3, height, PANEL_PADDING + ROW_HEIGHT), Some(1));
        assert_eq!(row_at(3, height, PANEL_PADDING + 2 * ROW_HEIGHT), Some(2));

        // A point past the last row of a panel sized for fewer rows.
        assert_eq!(row_at(1, panel_height(1), PANEL_PADDING + ROW_HEIGHT), None);
    }

    #[test]
    fn every_row_gets_a_label_to_draw() {
        let list = rows(3);
        let (x, y, width, height) = grown_rect((600, 600), list.len());
        let frame = compose_panel(x, y, width, height, &list, None);

        assert_eq!(frame.texts.len(), 3);
        for (index, run) in frame.texts.iter().enumerate() {
            let text = String::from_utf16(&run.text).expect("utf-16");
            assert_eq!(text, format!("Device {index}"));

            // The label sits clear of the row's marker and inside the panel.
            assert_eq!(run.rect.left, ROW_TEXT_LEFT);
            assert!(run.rect.right <= width - PANEL_PADDING);
            assert_eq!(run.rect.bottom - run.rect.top, ROW_HEIGHT);
        }
    }

    #[test]
    fn the_easing_runs_from_end_to_end_without_overshooting() {
        assert_eq!(eased(0), 0.0);
        assert_eq!(eased(ANIMATION_STEPS), 1.0);

        let mut previous = 0.0;
        for progress in 0..=ANIMATION_STEPS {
            let value = eased(progress);
            assert!(
                (0.0..=1.0).contains(&value),
                "step {progress} left the range"
            );
            assert!(value >= previous, "step {progress} went backwards");
            previous = value;
        }
    }

    #[test]
    fn a_panel_never_grows_past_the_row_cap() {
        // A machine with more endpoints than the shortcut shows must still
        // produce a panel of a bounded height.
        assert_eq!(panel_height(MAX_ROWS + 5), panel_height(MAX_ROWS));
        assert_eq!(panel_height(0), panel_height(1));
    }
}
