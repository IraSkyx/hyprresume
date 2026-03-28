use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fmt::Write as _;
use std::os::raw::{c_char, c_int, c_void};
use std::sync::Mutex;

// C++ shim FFI (shim.cpp)

type PreMapCb = extern "C" fn(*mut c_void);
type OpenCb = extern "C" fn(*mut c_void);
type IpcCb = extern "C" fn(c_int, *const c_char) -> *const c_char;

unsafe extern "C" {
    fn shim_register_events(handle: *mut c_void, pre_map: PreMapCb, open: OpenCb);
    fn shim_register_ipc(handle: *mut c_void, cb: IpcCb);
    fn shim_cleanup();

    fn shim_window_class(w: *mut c_void) -> *const c_char;
    fn shim_window_set_workspace(w: *mut c_void, ws: *const c_char);
    fn shim_window_set_monitor(w: *mut c_void, name: *const c_char);
    fn shim_window_set_floating(w: *mut c_void);
    fn shim_window_set_fullscreen(w: *mut c_void);
    fn shim_window_set_geometry(w: *mut c_void, x: f64, y: f64, w: f64, h: f64);
}

// State

struct Expectation {
    workspace: String,
    monitor: String,
    floating: bool,
    fullscreen: bool,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

struct PendingGeo {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

struct State {
    expectations: HashMap<String, Vec<Expectation>>,
    pending_geo: HashMap<usize, PendingGeo>,
    active: bool,
}

static STATE: std::sync::LazyLock<Mutex<State>> = std::sync::LazyLock::new(|| {
    Mutex::new(State {
        expectations: HashMap::new(),
        pending_geo: HashMap::new(),
        active: false,
    })
});

// Thread-local buffer for IPC return strings. The C++ shim copies
// the result immediately, so the pointer only needs to live until
// the shim's lambda returns.
thread_local! {
    static IPC_BUF: std::cell::RefCell<CString> = std::cell::RefCell::new(CString::default());
}

// Event callbacks

fn window_class(w: *mut c_void) -> String {
    unsafe {
        let p = shim_window_class(w);
        if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

extern "C" fn on_pre_map(w: *mut c_void) {
    let cls = window_class(w);
    let mut st = STATE.lock().unwrap();
    if !st.active {
        return;
    }
    let queue = match st.expectations.get_mut(&cls) {
        Some(q) if !q.is_empty() => q,
        _ => return,
    };

    let workspace = queue[0].workspace.clone();
    let monitor = queue[0].monitor.clone();
    let floating = queue[0].floating;
    let fullscreen = queue[0].fullscreen;
    let (x, y, gw, gh) = (queue[0].x, queue[0].y, queue[0].w, queue[0].h);

    // keep last entry alive for forking apps
    if queue.len() > 1 {
        queue.remove(0);
    }

    if let Some(s) = (!workspace.is_empty()).then(|| CString::new(workspace).ok()).flatten() {
        unsafe { shim_window_set_workspace(w, s.as_ptr()) };
    }
    if let Some(s) = (!monitor.is_empty()).then(|| CString::new(monitor).ok()).flatten() {
        unsafe { shim_window_set_monitor(w, s.as_ptr()) };
    }
    if floating {
        unsafe { shim_window_set_floating(w) };
    }
    if fullscreen {
        unsafe { shim_window_set_fullscreen(w) };
    }
    if floating && gw > 0.0 && gh > 0.0 {
        st.pending_geo.insert(w as usize, PendingGeo { x, y, w: gw, h: gh });
    }
}

extern "C" fn on_open(w: *mut c_void) {
    let mut st = STATE.lock().unwrap();
    if let Some(g) = st.pending_geo.remove(&(w as usize)) {
        unsafe { shim_window_set_geometry(w, g.x, g.y, g.w, g.h) };
    }
}

// IPC handler

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn ok(json: bool) -> String {
    if json { r#"{"ok": true}"#.into() } else { "ok".into() }
}

fn err(msg: &str, json: bool) -> String {
    if json {
        format!(r#"{{"ok": false, "error": "{}"}}"#, json_escape(msg))
    } else {
        format!("error: {msg}")
    }
}

fn ipc_dispatch(args: &str, json: bool) -> String {
    let args = args.strip_prefix("sessionctl").unwrap_or(args).trim_start();
    let (cmd, rest) = args.split_once(' ').unwrap_or((args, ""));

    match cmd {
        "begin" => {
            let mut st = STATE.lock().unwrap();
            st.expectations.clear();
            st.pending_geo.clear();
            st.active = true;
            drop(st);
            ok(json)
        }
        "expect" => {
            let mut st = STATE.lock().unwrap();
            if !st.active {
                return err("no active restore session (call begin first)", json);
            }
            let p: Vec<&str> = rest.split_whitespace().collect();
            if p.len() < 10 {
                return err("usage: sessionctl expect <app> <ws> <mon> <float> <fs> <max> <x> <y> <w> <h>", json);
            }
            st.expectations.entry(p[0].into()).or_default().push(Expectation {
                workspace: p[1].into(),
                monitor: p[2].into(),
                floating: p[3] != "0",
                fullscreen: p[4] != "0",
                x: p[6].parse().unwrap_or(0.0),
                y: p[7].parse().unwrap_or(0.0),
                w: p[8].parse().unwrap_or(0.0),
                h: p[9].parse().unwrap_or(0.0),
            });
            drop(st);
            ok(json)
        }
        "end" => {
            let active = STATE.lock().unwrap().active;
            if active { ok(json) } else { err("no active restore session", json) }
        }
        "finish" => {
            let mut st = STATE.lock().unwrap();
            if !st.active {
                return err("no active restore session", json);
            }
            st.expectations.clear();
            st.pending_geo.clear();
            st.active = false;
            drop(st);
            ok(json)
        }
        "status" => {
            let st = STATE.lock().unwrap();
            let total: usize = st.expectations.values().map(Vec::len).sum();
            let result = if json {
                let mut o = format!(r#"{{"active":{},"pending":{total},"expectations":{{"#, st.active);
                for (i, (id, v)) in st.expectations.iter().enumerate() {
                    if i > 0 { o.push(','); }
                    let _ = write!(o, r#""{}":"#, json_escape(id));
                    let _ = write!(o, "{}", v.len());
                }
                o.push_str("}}");
                o
            } else {
                let mut o = format!("active: {}\npending: {total}\n", st.active);
                for (id, v) in &st.expectations {
                    let _ = writeln!(o, "  {id}: {} remaining", v.len());
                }
                o
            };
            drop(st);
            result
        }
        _ => err("unknown subcommand. usage: sessionctl <begin|expect|end|finish|status>", json),
    }
}

extern "C" fn ipc_handler(fmt: c_int, args: *const c_char) -> *const c_char {
    let args = unsafe { CStr::from_ptr(args) }.to_string_lossy();
    let result = ipc_dispatch(&args, fmt == 1);
    IPC_BUF.with(|cell| {
        let cs = CString::new(result).unwrap_or_default();
        *cell.borrow_mut() = cs;
        cell.borrow().as_ptr()
    })
}

// Called from shim.cpp

/// # Safety
///
/// `handle` must be a valid Hyprland plugin HANDLE from `pluginInit`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_plugin_init(handle: *mut c_void) {
    unsafe {
        shim_register_events(handle, on_pre_map, on_open);
        shim_register_ipc(handle, ipc_handler);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_plugin_exit() {
    unsafe { shim_cleanup() };
    if let Ok(mut st) = STATE.lock() {
        st.expectations.clear();
        st.pending_geo.clear();
        st.active = false;
    }
}
