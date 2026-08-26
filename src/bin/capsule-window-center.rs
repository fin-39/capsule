//! Manage trusted helpers for Gamescope's private X display.
//!
//! Gamescope sees Wine's virtual desktop as one X11 surface. Older games can
//! create a smaller child at (0, 0), leaving the game in the upper-left even
//! though Gamescope correctly centers the outer surface. This trusted helper
//! runs beside (not inside) the untrusted sandbox, connects only to Gamescope's
//! private Xwayland display, and moves direct children of the configured Wine
//! desktop. It never resizes a client and remembers each observed size so it
//! does not fight later user movement. When explicitly enabled it also owns
//! the private X11 text clipboard and imports bounded text obtained from the
//! host compositor through the trusted `wl-paste` executable.

use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::ptr;
use std::thread;
use std::time::{Duration, Instant};

use rustix::process::{Signal, set_parent_process_death_signal};

const MIN_CLIENT_DIMENSION: u32 = 64;
const MAX_X_WINDOWS: u32 = 4_096;
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(400);
const CLIPBOARD_READ_TIMEOUT: Duration = Duration::from_millis(750);
const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
const STABLE_POLLS_BEFORE_CENTERING: u8 = 3;
const IS_VIEWABLE: c_int = 2;
const SELECTION_REQUEST: c_int = 30;
const SELECTION_NOTIFY: c_int = 31;
const PROP_MODE_REPLACE: c_int = 0;
const XA_ATOM: c_ulong = 4;
const CURRENT_TIME: c_ulong = 0;

type XWindow = c_ulong;

#[repr(C)]
struct XDisplay {
    _private: [u8; 0],
}

#[repr(C)]
struct XErrorEvent {
    _private: [u8; 0],
}

#[repr(C)]
struct XWindowAttributes {
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    border_width: c_int,
    depth: c_int,
    visual: *mut c_void,
    root: XWindow,
    class: c_int,
    bit_gravity: c_int,
    win_gravity: c_int,
    backing_store: c_int,
    backing_planes: c_ulong,
    backing_pixel: c_ulong,
    save_under: c_int,
    colormap: c_ulong,
    map_installed: c_int,
    map_state: c_int,
    all_event_masks: c_long,
    your_event_mask: c_long,
    do_not_propagate_mask: c_long,
    override_redirect: c_int,
    screen: *mut c_void,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XSelectionRequestEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    owner: XWindow,
    requestor: XWindow,
    selection: c_ulong,
    target: c_ulong,
    property: c_ulong,
    time: c_ulong,
}

#[derive(Clone, Copy)]
#[repr(C)]
struct XSelectionEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    requestor: XWindow,
    selection: c_ulong,
    target: c_ulong,
    property: c_ulong,
    time: c_ulong,
}

#[repr(C)]
union XEvent {
    type_: c_int,
    selection_request: XSelectionRequestEvent,
    selection: XSelectionEvent,
    pad: [c_long; 24],
}

type XErrorHandler = Option<unsafe extern "C" fn(*mut XDisplay, *mut XErrorEvent) -> c_int>;

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut XDisplay;
    fn XCloseDisplay(display: *mut XDisplay) -> c_int;
    fn XDefaultRootWindow(display: *mut XDisplay) -> XWindow;
    fn XQueryTree(
        display: *mut XDisplay,
        window: XWindow,
        root_return: *mut XWindow,
        parent_return: *mut XWindow,
        children_return: *mut *mut XWindow,
        child_count_return: *mut c_uint,
    ) -> c_int;
    fn XGetWindowAttributes(
        display: *mut XDisplay,
        window: XWindow,
        attributes_return: *mut XWindowAttributes,
    ) -> c_int;
    fn XMoveWindow(display: *mut XDisplay, window: XWindow, x: c_int, y: c_int) -> c_int;
    fn XCreateSimpleWindow(
        display: *mut XDisplay,
        parent: XWindow,
        x: c_int,
        y: c_int,
        width: c_uint,
        height: c_uint,
        border_width: c_uint,
        border: c_ulong,
        background: c_ulong,
    ) -> XWindow;
    fn XInternAtom(display: *mut XDisplay, name: *const c_char, only_if_exists: c_int) -> c_ulong;
    fn XSetSelectionOwner(
        display: *mut XDisplay,
        selection: c_ulong,
        owner: XWindow,
        time: c_ulong,
    ) -> c_int;
    fn XPending(display: *mut XDisplay) -> c_int;
    fn XNextEvent(display: *mut XDisplay, event_return: *mut XEvent) -> c_int;
    fn XChangeProperty(
        display: *mut XDisplay,
        window: XWindow,
        property: c_ulong,
        property_type: c_ulong,
        format: c_int,
        mode: c_int,
        data: *const u8,
        element_count: c_int,
    ) -> c_int;
    fn XSendEvent(
        display: *mut XDisplay,
        window: XWindow,
        propagate: c_int,
        event_mask: c_long,
        event: *mut XEvent,
    ) -> c_int;
    fn XFlush(display: *mut XDisplay) -> c_int;
    fn XSync(display: *mut XDisplay, discard: c_int) -> c_int;
    fn XSetErrorHandler(handler: XErrorHandler) -> XErrorHandler;
    fn XFree(data: *mut c_void) -> c_int;
}

unsafe extern "C" {
    fn getppid() -> c_int;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Geometry {
    width: u32,
    height: u32,
    border: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CenteringObservation {
    geometry: Geometry,
    stable_polls: u8,
    centered: bool,
}

struct DisplayHandle(*mut XDisplay);

struct ClipboardState {
    window: XWindow,
    clipboard: c_ulong,
    targets: c_ulong,
    utf8_string: c_ulong,
    text: c_ulong,
    string: c_ulong,
    content: Vec<u8>,
    last_host_content: Option<Vec<u8>>,
    next_poll: Instant,
    wl_paste: PathBuf,
}

impl Drop for DisplayHandle {
    fn drop(&mut self) {
        // SAFETY: `DisplayHandle` is constructed only from a successful
        // XOpenDisplay call and owns the connection for its entire lifetime.
        unsafe {
            XCloseDisplay(self.0);
        }
    }
}

fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(1);
    let Some(width) = arguments.next().and_then(|value| value.parse::<u32>().ok()) else {
        print_usage();
        return ExitCode::FAILURE;
    };
    let Some(height) = arguments.next().and_then(|value| value.parse::<u32>().ok()) else {
        print_usage();
        return ExitCode::FAILURE;
    };
    let Some(expected_parent_pid) = arguments
        .next()
        .and_then(|value| value.parse::<c_int>().ok())
    else {
        print_usage();
        return ExitCode::FAILURE;
    };
    let Some(clipboard_enabled) = arguments.next().and_then(|value| match value.as_str() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }) else {
        print_usage();
        return ExitCode::FAILURE;
    };
    let Some(wl_paste) = arguments.next() else {
        print_usage();
        return ExitCode::FAILURE;
    };
    let dimensions_valid = (width == 0 && height == 0) || (width > 0 && height > 0);
    let wl_paste = PathBuf::from(wl_paste);
    if !dimensions_valid
        || expected_parent_pid <= 1
        || (clipboard_enabled && !wl_paste.is_absolute())
        || (!clipboard_enabled && !wl_paste.as_os_str().is_empty())
        || arguments.next().is_some()
    {
        print_usage();
        return ExitCode::FAILURE;
    }

    // Capture the shell which spawned us before doing any other fallible work.
    // It immediately execs Sandwine without changing PID.
    // SAFETY: getppid has no preconditions.
    let parent_pid = unsafe { getppid() };
    if parent_pid != expected_parent_pid {
        eprintln!("capsule-window-center launch supervisor already exited");
        return ExitCode::FAILURE;
    }
    if let Err(error) = set_parent_process_death_signal(Some(Signal::TERM)) {
        eprintln!("could not bind window helper lifetime to Sandwine: {error}");
        return ExitCode::FAILURE;
    }
    // Close the race where the original parent exits immediately before the
    // kernel parent-death signal is registered.
    // SAFETY: getppid has no preconditions.
    if unsafe { getppid() } != parent_pid {
        return ExitCode::FAILURE;
    }

    // Gamescope replaces Capsule's nonexistent DISPLAY sentinel before this
    // helper starts. Passing null deliberately asks Xlib to use only that
    // inherited private DISPLAY value.
    // SAFETY: Xlib accepts a null display-name pointer and returns either a
    // valid owned connection or null. This process uses Xlib on one thread.
    let display = unsafe {
        XSetErrorHandler(Some(ignore_x_error));
        XOpenDisplay(ptr::null())
    };
    if display.is_null() {
        eprintln!("could not connect to Gamescope's private X display");
        return ExitCode::FAILURE;
    }
    let display = DisplayHandle(display);

    let desktop_size = (width > 0).then_some((width, height));
    let clipboard = clipboard_enabled.then_some(wl_paste);
    // SAFETY: the display remains open for the polling loop.
    unsafe { display_loop(display.0, desktop_size, clipboard, parent_pid) };
    ExitCode::SUCCESS
}

fn print_usage() {
    eprintln!("usage: capsule-window-center WIDTH HEIGHT EXPECTED-PARENT-PID CLIPBOARD WL-PASTE");
}

unsafe extern "C" fn ignore_x_error(_: *mut XDisplay, _: *mut XErrorEvent) -> c_int {
    // A window can disappear between XQueryTree and XMoveWindow. Ignore that
    // asynchronous race and discover the current tree on the next poll.
    0
}

unsafe fn display_loop(
    display: *mut XDisplay,
    desktop_size: Option<(u32, u32)>,
    wl_paste: Option<PathBuf>,
    parent_pid: c_int,
) {
    // SAFETY: `display` is a live Xlib connection owned by the caller.
    let root = unsafe { XDefaultRootWindow(display) };
    let mut observations: HashMap<XWindow, CenteringObservation> = HashMap::new();
    let mut clipboard = wl_paste.map(|path| unsafe { ClipboardState::new(display, root, path) });

    loop {
        // SAFETY: getppid has no preconditions. A changed parent means the
        // exec'd Sandwine supervisor ended and this sidecar must end too.
        if unsafe { getppid() } != parent_pid {
            return;
        }
        if let Some(clipboard) = clipboard.as_mut() {
            clipboard.poll_host(display);
            unsafe { clipboard.handle_events(display) };
        }

        if let Some((desktop_width, desktop_height)) = desktop_size {
            let mut present = HashSet::new();
            // SAFETY: every queried window belongs to the live private display.
            if let Some(desktops) = unsafe { query_children(display, root) } {
                for desktop in desktops {
                    // An exact match prevents this compatibility helper from
                    // moving children of unrelated X clients on the private
                    // display. Wine's virtual desktop is created at this size.
                    let Some((geometry, true)) = (unsafe { query_window(display, desktop) }) else {
                        continue;
                    };
                    if geometry.width != desktop_width || geometry.height != desktop_height {
                        continue;
                    }
                    // SAFETY: `desktop` was returned by XQueryTree above.
                    let Some(children) = (unsafe { query_children(display, desktop) }) else {
                        continue;
                    };
                    for child in children {
                        // SAFETY: `child` is current in the desktop's tree.
                        let Some((child_geometry, true)) =
                            (unsafe { query_window(display, child) })
                        else {
                            continue;
                        };
                        let Some((x, y)) = centered_position(geometry, child_geometry) else {
                            continue;
                        };
                        present.insert(child);
                        if !observe_window(&mut observations, child, child_geometry) {
                            continue;
                        }
                        // SAFETY: both the connection and XID came from Xlib. A
                        // concurrent destroy is handled by the error handler.
                        unsafe {
                            XMoveWindow(display, child, x, y);
                        }
                    }
                    // One Wine virtual desktop is configured per run. Bounding
                    // work to its direct children prevents a hostile X client
                    // from multiplying the per-query window cap across thousands
                    // of same-sized fake parents.
                    break;
                }
            }
            observations.retain(|window, _| present.contains(window));
        }
        // SAFETY: flush requests on the live connection without discarding
        // pending events.
        unsafe {
            XSync(display, 0);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

impl ClipboardState {
    unsafe fn new(display: *mut XDisplay, root: XWindow, wl_paste: PathBuf) -> Self {
        // SAFETY: the caller owns a live Xlib display. These atom names are
        // fixed NUL-terminated strings and the one-pixel window remains on
        // the private display for the helper's lifetime.
        let window = unsafe { XCreateSimpleWindow(display, root, -10, -10, 1, 1, 0, 0, 0) };
        Self {
            window,
            clipboard: unsafe { XInternAtom(display, c"CLIPBOARD".as_ptr(), 0) },
            targets: unsafe { XInternAtom(display, c"TARGETS".as_ptr(), 0) },
            utf8_string: unsafe { XInternAtom(display, c"UTF8_STRING".as_ptr(), 0) },
            text: unsafe { XInternAtom(display, c"TEXT".as_ptr(), 0) },
            string: unsafe { XInternAtom(display, c"STRING".as_ptr(), 0) },
            content: Vec::new(),
            last_host_content: None,
            next_poll: Instant::now(),
            wl_paste,
        }
    }

    fn poll_host(&mut self, display: *mut XDisplay) {
        let now = Instant::now();
        if now < self.next_poll {
            return;
        }
        self.next_poll = now + CLIPBOARD_POLL_INTERVAL;
        let Some(content) = read_host_clipboard(&self.wl_paste) else {
            return;
        };
        if self.last_host_content.as_ref() == Some(&content) {
            return;
        }
        self.last_host_content = Some(content.clone());
        self.content = content;
        // SAFETY: the atom and helper window were created on this live
        // private display. Owning CLIPBOARD makes Wine request data only from
        // this bounded in-memory copy, never from a host socket.
        unsafe {
            XSetSelectionOwner(display, self.clipboard, self.window, CURRENT_TIME);
            XFlush(display);
        }
    }

    unsafe fn handle_events(&self, display: *mut XDisplay) {
        // SAFETY: all events are read from the helper's live Xlib connection.
        while unsafe { XPending(display) } > 0 {
            let mut event = std::mem::MaybeUninit::<XEvent>::zeroed();
            unsafe {
                XNextEvent(display, event.as_mut_ptr());
            }
            // SAFETY: XNextEvent initialized the complete union, whose first
            // field is the common X event type.
            let event = unsafe { event.assume_init() };
            if unsafe { event.type_ } == SELECTION_REQUEST {
                // SAFETY: SelectionRequest identifies this union member.
                let request = unsafe { event.selection_request };
                unsafe { self.answer_request(display, request) };
            }
        }
    }

    unsafe fn answer_request(&self, display: *mut XDisplay, request: XSelectionRequestEvent) {
        let mut response = XSelectionEvent {
            type_: SELECTION_NOTIFY,
            serial: 0,
            send_event: 1,
            display,
            requestor: request.requestor,
            selection: request.selection,
            target: request.target,
            property: 0,
            time: request.time,
        };
        let property = if request.property == 0 {
            request.target
        } else {
            request.property
        };

        if request.selection == self.clipboard && request.target == self.targets {
            let supported = [self.targets, self.utf8_string, self.text, self.string];
            // Xlib requires a native-long array for format 32 even though only
            // the low 32 bits are transferred by the protocol.
            unsafe {
                XChangeProperty(
                    display,
                    request.requestor,
                    property,
                    XA_ATOM,
                    32,
                    PROP_MODE_REPLACE,
                    supported.as_ptr().cast(),
                    supported.len() as c_int,
                );
            }
            response.property = property;
        } else if request.selection == self.clipboard
            && matches!(request.target, target if target == self.utf8_string || target == self.text || target == self.string)
        {
            unsafe {
                XChangeProperty(
                    display,
                    request.requestor,
                    property,
                    request.target,
                    8,
                    PROP_MODE_REPLACE,
                    self.content.as_ptr(),
                    self.content.len() as c_int,
                );
            }
            response.property = property;
        }

        let mut event = XEvent {
            selection: response,
        };
        unsafe {
            XSendEvent(display, request.requestor, 0, 0, &mut event);
            XFlush(display);
        }
    }
}

fn read_host_clipboard(wl_paste: &Path) -> Option<Vec<u8>> {
    let mut child = Command::new(wl_paste)
        .args(["--no-newline", "--type", "text"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut content = Vec::new();
        stdout
            .take((MAX_CLIPBOARD_BYTES + 1) as u64)
            .read_to_end(&mut content)
            .map(|_| content)
    });
    let deadline = Instant::now() + CLIPBOARD_READ_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let content = reader.join().ok()?.ok()?;
    if !status?.success()
        || content.len() > MAX_CLIPBOARD_BYTES
        || std::str::from_utf8(&content).is_err()
    {
        return None;
    }
    Some(content)
}

fn observe_window(
    observations: &mut HashMap<XWindow, CenteringObservation>,
    window: XWindow,
    geometry: Geometry,
) -> bool {
    let observation = observations.entry(window).or_insert(CenteringObservation {
        geometry,
        stable_polls: 0,
        centered: false,
    });
    if observation.geometry != geometry {
        *observation = CenteringObservation {
            geometry,
            stable_polls: 0,
            centered: false,
        };
    }
    if observation.centered {
        return false;
    }
    observation.stable_polls = observation.stable_polls.saturating_add(1);
    if observation.stable_polls < STABLE_POLLS_BEFORE_CENTERING {
        return false;
    }
    observation.centered = true;
    true
}

fn centered_position(parent: Geometry, child: Geometry) -> Option<(c_int, c_int)> {
    if child.width < MIN_CLIENT_DIMENSION || child.height < MIN_CLIENT_DIMENSION {
        return None;
    }
    let child_outer_width = child.width.checked_add(child.border.checked_mul(2)?)?;
    let child_outer_height = child.height.checked_add(child.border.checked_mul(2)?)?;
    if child_outer_width > parent.width || child_outer_height > parent.height {
        return None;
    }
    Some((
        ((parent.width - child_outer_width) / 2) as c_int,
        ((parent.height - child_outer_height) / 2) as c_int,
    ))
}

unsafe fn query_window(display: *mut XDisplay, window: XWindow) -> Option<(Geometry, bool)> {
    let mut attributes = std::mem::MaybeUninit::<XWindowAttributes>::uninit();
    // SAFETY: all output pointers refer to initialized local storage and the
    // caller obtained the connection and XID from Xlib.
    let status = unsafe { XGetWindowAttributes(display, window, attributes.as_mut_ptr()) };
    if status == 0 {
        return None;
    }
    // SAFETY: XGetWindowAttributes returned success and initialized the whole
    // C XWindowAttributes structure.
    let attributes = unsafe { attributes.assume_init() };
    let width = u32::try_from(attributes.width).ok()?;
    let height = u32::try_from(attributes.height).ok()?;
    let border = u32::try_from(attributes.border_width).ok()?;
    Some((
        Geometry {
            width,
            height,
            border,
        },
        attributes.map_state == IS_VIEWABLE,
    ))
}

unsafe fn query_children(display: *mut XDisplay, window: XWindow) -> Option<Vec<XWindow>> {
    let mut root = 0;
    let mut parent = 0;
    let mut children = ptr::null_mut();
    let mut count = 0;
    // SAFETY: all output pointers refer to initialized local storage and the
    // caller obtained the connection and XID from Xlib.
    let status = unsafe {
        XQueryTree(
            display,
            window,
            &mut root,
            &mut parent,
            &mut children,
            &mut count,
        )
    };
    if status == 0 {
        return None;
    }
    let result = if count > MAX_X_WINDOWS {
        None
    } else if count == 0 || children.is_null() {
        Some(Vec::new())
    } else {
        // SAFETY: on success XQueryTree returns `count` XIDs in Xlib-owned
        // storage, valid until XFree below. Copy them before releasing it.
        Some(unsafe { std::slice::from_raw_parts(children, count as usize) }.to_vec())
    };
    if !children.is_null() {
        // SAFETY: this pointer was allocated and returned by XQueryTree.
        unsafe {
            XFree(children.cast());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(width: u32, height: u32) -> Geometry {
        Geometry {
            width,
            height,
            border: 0,
        }
    }

    #[test]
    fn centers_a_legacy_game_inside_the_compatibility_desktop() {
        assert_eq!(
            centered_position(geometry(800, 600), geometry(640, 480)),
            Some((80, 60))
        );
    }

    #[test]
    fn includes_window_borders_and_rejects_oversized_clients() {
        assert_eq!(
            centered_position(
                geometry(800, 600),
                Geometry {
                    width: 640,
                    height: 480,
                    border: 4,
                }
            ),
            Some((76, 56))
        );
        assert_eq!(
            centered_position(geometry(640, 480), geometry(800, 600)),
            None
        );
    }

    #[test]
    fn ignores_internal_one_pixel_windows() {
        assert_eq!(centered_position(geometry(800, 600), geometry(1, 1)), None);
    }

    #[test]
    fn waits_for_a_visible_window_to_stabilize_then_centers_only_once() {
        let mut observations = HashMap::new();
        let window = 42;
        let initial = geometry(320, 240);
        assert!(!observe_window(&mut observations, window, initial));
        assert!(!observe_window(&mut observations, window, initial));
        assert!(observe_window(&mut observations, window, initial));
        assert!(!observe_window(&mut observations, window, initial));

        let resized = geometry(640, 480);
        assert!(!observe_window(&mut observations, window, resized));
        assert!(!observe_window(&mut observations, window, resized));
        assert!(observe_window(&mut observations, window, resized));
    }
}
