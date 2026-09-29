//! Clears the phantom Wintun device a previous session leaves behind.
//!
//! Mihomo creates its TUN adapter through Wintun with a GUID derived from the
//! adapter name (sing-tun: MD5 of "wintun" + name, little-endian GUID). When
//! the adapter is torn down, Windows keeps its software-device node as a
//! non-present ("phantom") entry. The next Connect asks for the same GUID,
//! Wintun fails with "Cannot create a file when that file already exists",
//! and Mihomo only brings the adapter up after its own 15-second fallback -
//! past the core readiness window, so every Connect after the first
//! disconnect failed with a 500 ("Mihomo did not start in time") on a real
//! Windows 10 machine and in a clean VM. Removing the phantom node first
//! brings the adapter up in 1-3 seconds. Only a non-present node with this
//! exact instance ID is touched; a live adapter or another product's
//! adapter never is.

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM, CM_Locate_DevNodeW, CM_Uninstall_DevNode,
    CR_SUCCESS,
};

/// `SWD\WINTUN\{GUID}` for the adapter named `MultiCore` (the `tun.device`
/// the daemon writes and `tun_device_is_up` waits for): the GUID is
/// MD5("wintunMultiCore") read as a little-endian GUID.
const MULTICORE_TUN_INSTANCE_ID: &str = r"SWD\WINTUN\{F0E30BD9-AADA-D161-BEB6-0607F5DD5D65}";

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhantomCleanup {
    /// No node at all, or a live (present) adapter that must be left alone.
    NothingToDo,
    Removed,
    Failed(u32),
}

pub(crate) fn remove_phantom_tun_adapter() -> PhantomCleanup {
    let instance_id = MULTICORE_TUN_INSTANCE_ID
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<u16>>();
    let mut node = 0_u32;
    // SAFETY: `instance_id` is NUL-terminated and outlives the call; `node`
    // is a valid out pointer.
    if unsafe { CM_Locate_DevNodeW(&mut node, instance_id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) }
        == CR_SUCCESS
    {
        return PhantomCleanup::NothingToDo;
    }
    // SAFETY: as above.
    if unsafe { CM_Locate_DevNodeW(&mut node, instance_id.as_ptr(), CM_LOCATE_DEVNODE_PHANTOM) }
        != CR_SUCCESS
    {
        return PhantomCleanup::NothingToDo;
    }
    // SAFETY: `node` was just located as a phantom device instance.
    match unsafe { CM_Uninstall_DevNode(node, 0) } {
        CR_SUCCESS => PhantomCleanup::Removed,
        code => PhantomCleanup::Failed(code),
    }
}

#[cfg(test)]
mod tests {
    use super::MULTICORE_TUN_INSTANCE_ID;

    #[test]
    fn the_instance_id_is_the_wintun_node_for_the_multicore_adapter() {
        // The adapter name is fixed by the generated Mihomo config; if it
        // ever changes, this GUID must be re-derived.
        assert!(MULTICORE_TUN_INSTANCE_ID.starts_with(r"SWD\WINTUN\{"));
        assert_eq!(MULTICORE_TUN_INSTANCE_ID.len(), r"SWD\WINTUN\".len() + 38);
    }
}
