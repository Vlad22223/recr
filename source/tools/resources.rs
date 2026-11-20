use std::ffi::c_void;
use std::io;
use std::os::windows::ffi::OsStrExt;

type Handle = *mut c_void;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn BeginUpdateResourceW(file: *const u16, delete_existing: i32) -> Handle;
    fn UpdateResourceW(
        file: Handle,
        kind: *const u16,
        name: *const u16,
        language: u16,
        data: *const c_void,
        size: u32,
    ) -> i32;
    fn EndUpdateResourceW(file: Handle, discard: i32) -> i32;
}

const ICON: &[u8] = include_bytes!("../assets/idle.ico");
const MANIFEST: &[u8] = include_bytes!("../assets/recr.manifest");
const RT_ICON: usize = 3;
const RT_GROUP_ICON: usize = 14;
const RT_MANIFEST: usize = 24;

fn main() -> io::Result<()> {
    let file = std::env::args_os()
        .nth(1)
        .ok_or_else(|| io::Error::other("Usage: resources.exe path-to-executable"))?;
    let file: Vec<u16> = file.encode_wide().chain([0]).collect();
    let handle = unsafe { BeginUpdateResourceW(file.as_ptr(), 1) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }

    // The single-image ICO directory entry ends in a file offset. Windows uses an icon ID there.
    let mut group = ICON[..18].to_vec();
    group.extend_from_slice(&1_u16.to_le_bytes());
    for (kind, bytes) in [
        (RT_ICON, &ICON[22..]),
        (RT_GROUP_ICON, &group),
        (RT_MANIFEST, MANIFEST),
    ] {
        let ok = unsafe {
            UpdateResourceW(
                handle,
                kind as *const u16,
                1_usize as *const u16,
                0,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
            )
        };
        if ok == 0 {
            let error = io::Error::last_os_error();
            unsafe { EndUpdateResourceW(handle, 1) };
            return Err(error);
        }
    }
    if unsafe { EndUpdateResourceW(handle, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
