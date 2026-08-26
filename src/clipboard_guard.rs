//! Wayland registry interposer used only for clipboard-disabled Gamescope.
//!
//! Gamescope's nested Wayland backend binds the host data-device managers and
//! automatically publishes Wine's private X11 selections to the desktop. A
//! disabled Capsule clipboard permission therefore needs to hide those two
//! globals before Gamescope binds them. The release `cdylib` is preloaded only
//! into Gamescope; all other registry listeners pass through unchanged.

use std::collections::HashSet;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

const BLOCK_ENVIRONMENT: &str = "CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD";

#[repr(C)]
struct WlProxy {
    _private: [u8; 0],
}

#[derive(Clone, Copy)]
#[repr(C)]
struct RegistryListener {
    global: Option<unsafe extern "C" fn(*mut c_void, *mut WlProxy, u32, *const c_char, u32)>,
    global_remove: Option<unsafe extern "C" fn(*mut c_void, *mut WlProxy, u32)>,
}

struct GuardContext {
    original: RegistryListener,
    original_data: *mut c_void,
    suppressed: HashSet<u32>,
}

type AddListener = unsafe extern "C" fn(*mut WlProxy, *mut *mut c_void, *mut c_void) -> c_int;
type GetClass = unsafe extern "C" fn(*mut WlProxy) -> *const c_char;

#[link(name = "dl")]
unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

static GUARD_LISTENER: RegistryListener = RegistryListener {
    global: Some(guard_global),
    global_remove: Some(guard_global_remove),
};

/// Interpose the one generated Wayland call that installs registry callbacks.
///
/// The symbol remains a transparent pass-through unless the trusted launcher
/// explicitly sets `CAPSULE_BLOCK_GAMESCOPE_CLIPBOARD=1` for Gamescope.
#[unsafe(no_mangle)]
unsafe extern "C" fn wl_proxy_add_listener(
    proxy: *mut WlProxy,
    implementation: *mut *mut c_void,
    data: *mut c_void,
) -> c_int {
    let real_add_listener = unsafe { real_add_listener() };
    if std::env::var_os(BLOCK_ENVIRONMENT).as_deref() != Some("1".as_ref())
        || proxy.is_null()
        || implementation.is_null()
    {
        return unsafe { real_add_listener(proxy, implementation, data) };
    }

    let class = unsafe { real_get_class()(proxy) };
    if class.is_null() || unsafe { CStr::from_ptr(class) }.to_bytes() != b"wl_registry" {
        return unsafe { real_add_listener(proxy, implementation, data) };
    }

    // SAFETY: wl_registry_add_listener always passes the generated two-entry
    // wl_registry_listener structure. Copy it before replacing callback data.
    let original = unsafe { *implementation.cast::<RegistryListener>() };
    let context = Box::new(GuardContext {
        original,
        original_data: data,
        suppressed: HashSet::new(),
    });
    unsafe {
        real_add_listener(
            proxy,
            (&GUARD_LISTENER as *const RegistryListener)
                .cast_mut()
                .cast(),
            Box::into_raw(context).cast(),
        )
    }
}

unsafe extern "C" fn guard_global(
    data: *mut c_void,
    registry: *mut WlProxy,
    name: u32,
    interface: *const c_char,
    version: u32,
) {
    if data.is_null() || interface.is_null() {
        return;
    }
    let context = unsafe { &mut *data.cast::<GuardContext>() };
    let interface_name = unsafe { CStr::from_ptr(interface) }.to_bytes();
    if is_clipboard_global(interface_name) {
        context.suppressed.insert(name);
        return;
    }
    if let Some(callback) = context.original.global {
        unsafe {
            callback(context.original_data, registry, name, interface, version);
        }
    }
}

unsafe extern "C" fn guard_global_remove(data: *mut c_void, registry: *mut WlProxy, name: u32) {
    if data.is_null() {
        return;
    }
    let context = unsafe { &mut *data.cast::<GuardContext>() };
    if context.suppressed.remove(&name) {
        return;
    }
    if let Some(callback) = context.original.global_remove {
        unsafe { callback(context.original_data, registry, name) }
    }
}

fn is_clipboard_global(interface: &[u8]) -> bool {
    matches!(
        interface,
        b"wl_data_device_manager" | b"zwp_primary_selection_device_manager_v1"
    )
}

unsafe fn real_add_listener() -> AddListener {
    static FUNCTION: OnceLock<usize> = OnceLock::new();
    let address = *FUNCTION.get_or_init(|| unsafe {
        dlsym((-1_isize) as *mut c_void, c"wl_proxy_add_listener".as_ptr()) as usize
    });
    assert_ne!(address, 0, "real wl_proxy_add_listener is unavailable");
    unsafe { std::mem::transmute::<usize, AddListener>(address) }
}

unsafe fn real_get_class() -> GetClass {
    static FUNCTION: OnceLock<usize> = OnceLock::new();
    let address = *FUNCTION.get_or_init(|| unsafe {
        dlsym((-1_isize) as *mut c_void, c"wl_proxy_get_class".as_ptr()) as usize
    });
    assert_ne!(address, 0, "real wl_proxy_get_class is unavailable");
    unsafe { std::mem::transmute::<usize, GetClass>(address) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_only_host_clipboard_protocols() {
        assert!(is_clipboard_global(b"wl_data_device_manager"));
        assert!(is_clipboard_global(
            b"zwp_primary_selection_device_manager_v1"
        ));
        assert!(!is_clipboard_global(b"wl_compositor"));
        assert!(!is_clipboard_global(b"zwp_linux_dmabuf_v1"));
    }
}
