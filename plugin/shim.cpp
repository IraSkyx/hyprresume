// C++ bridge between Hyprland's plugin API and the Rust cdylib.
//
// The three plugin entry points (pluginAPIVersion, pluginInit, pluginExit)
// live here because they return C++ types (std::string, PLUGIN_DESCRIPTION_INFO).
// Everything else is forwarded to Rust via extern "C" calls.

#include <hyprland/src/plugins/PluginAPI.hpp>
#include <hyprland/src/event/EventBus.hpp>
#include <hyprland/src/Compositor.hpp>
#include <hyprland/src/desktop/view/Window.hpp>
#include <hyprland/src/SharedDefs.hpp>

#include <string>

using PreMapCb = void (*)(void*);
using OpenCb   = void (*)(void*);
using IpcCb    = const char* (*)(int, const char*);

static CHyprSignalListener s_preMapListener;
static CHyprSignalListener s_openListener;
static SP<SHyprCtlCommand> s_cmd;
static IpcCb               s_ipcCb = nullptr;
static std::string         s_ipcBuf;

extern "C" {

void shim_register_events(void* handle, PreMapCb onPreMap, OpenCb onOpen) {
    s_preMapListener = Event::bus()->m_events.window.preMap.listen(
        [onPreMap](PHLWINDOW w) { onPreMap(w.get()); });
    s_openListener = Event::bus()->m_events.window.open.listen(
        [onOpen](PHLWINDOW w) { onOpen(w.get()); });
}

void shim_register_ipc(void* handle, IpcCb cb) {
    s_ipcCb = cb;
    s_cmd = HyprlandAPI::registerHyprCtlCommand(static_cast<HANDLE>(handle), SHyprCtlCommand{
        .name  = "sessionctl",
        .exact = false,
        .fn    = [](eHyprCtlOutputFormat fmt, std::string args) -> std::string {
            if (!s_ipcCb)
                return "error: plugin not ready";
            const char* r = s_ipcCb(static_cast<int>(fmt), args.c_str());
            s_ipcBuf = r ? r : "";
            return s_ipcBuf;
        },
    });
}

void shim_cleanup() {
    s_preMapListener.reset();
    s_openListener.reset();
    s_cmd.reset();
    s_ipcCb = nullptr;
}

const char* shim_window_class(void* w) {
    auto* win = static_cast<Desktop::View::CWindow*>(w);
    auto& cls = win->m_class.empty() ? win->m_initialClass : win->m_class;
    return cls.c_str();
}

void shim_window_set_workspace(void* w, const char* ws) {
    auto* win = static_cast<Desktop::View::CWindow*>(w);
    win->m_preMapRequestedWorkspace = std::string(ws) + " silent";
}

void shim_window_set_monitor(void* w, const char* name) {
    auto mon = g_pCompositor->getMonitorFromName(name);
    if (mon)
        static_cast<Desktop::View::CWindow*>(w)->m_monitor = mon;
}

void shim_window_set_floating(void* w) {
    auto* win = static_cast<Desktop::View::CWindow*>(w);
    win->m_isFloating    = true;
    win->m_requestsFloat = true;
}

void shim_window_set_fullscreen(void* w) {
    static_cast<Desktop::View::CWindow*>(w)->m_wantsInitialFullscreen = true;
}

void shim_window_set_geometry(void* w, double x, double y, double gw, double gh) {
    auto* win = static_cast<Desktop::View::CWindow*>(w);
    win->m_position = Vector2D(x, y);
    win->m_size     = Vector2D(gw, gh);
    win->m_realPosition->setValueAndWarp(win->m_position);
    win->m_realSize->setValueAndWarp(win->m_size);
}

} // extern "C"

// Rust entry points (lib.rs)
extern "C" void rust_plugin_init(void* handle);
extern "C" void rust_plugin_exit();

// Hyprland plugin entry points

APICALL EXPORT std::string PLUGIN_API_VERSION() {
    return HYPRLAND_API_VERSION;
}

APICALL EXPORT PLUGIN_DESCRIPTION_INFO PLUGIN_INIT(HANDLE handle) {
    rust_plugin_init(handle);
    return {
        "hyprland-sessionctl",
        "IPC-driven session restore for hyprresume",
        "skyx",
        "0.1.0",
    };
}

APICALL EXPORT void PLUGIN_EXIT() {
    rust_plugin_exit();
}
