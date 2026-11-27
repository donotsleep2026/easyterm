//! Where a daemon running on this machine announces itself.
//!
//! `easyterm --daemon` binds an ephemeral port by default (`127.0.0.1:0`), and
//! frontends have no way to guess it; being told the port every time is exactly
//! the kind of thing a machine should remember for itself. The daemon therefore
//! **publishes** the address it bound into a named shared-memory segment, and a
//! frontend reads it back with [`discover`].
//!
//! Why shared memory rather than a file:
//!
//! * Windows: the segment is a named file mapping, and the OS destroys it when
//!   the last handle closes — a daemon that crashes cannot leave a stale address
//!   behind for the next frontend to dial.
//! * Unix: the segment is a POSIX shared-memory object, unlinked when the daemon
//!   shuts down. A crash can leave the object in `/dev/shm`, so the payload
//!   carries the publisher's pid and [`discover`] ignores one whose process is
//!   gone.
//!
//! The payload is one NUL-terminated line, versioned so a future format cannot
//! be misread as this one:
//!
//! ```text
//! EASYTERM1 <pid> <addr>
//! ```
//!
//! Publishing is best effort by design: a platform that refuses (no `/dev/shm`,
//! no permission) must not stop a daemon from serving. [`publish`] returns the
//! error so the daemon can log it, and frontends can use its address explicitly.

use std::fmt;

pub mod connection_auth;

/// The segment a daemon publishes its address into.
///
/// One per machine (and, on Windows, per login session): there is one daemon to
/// find, and the last one to start is the one that answers.
pub const SEGMENT: &str = "easyterm-daemon-addr";

/// The most a payload may be. The address is a loopback `host:port`; the rest is
/// slack so a longer future payload (extra fields) still fits.
const CAPACITY: usize = 256;

/// Payload magic: the format's name and version, so a segment written by another
/// program (or a later format) is not mistaken for this one.
const MAGIC: &str = "EASYTERM1";

/// What a daemon announced: where to connect, and who announced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    /// `host:port`, as the daemon bound it.
    pub addr: String,
    /// The publishing process, for the liveness check on platforms whose
    /// segments outlive the process.
    pub pid: u32,
}

impl fmt::Display for Announcement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (pid {})", self.addr, self.pid)
    }
}

/// A published address. Keep it alive for as long as the daemon runs: dropping
/// it removes the announcement.
pub struct Published {
    mapping: imp::Mapping,
    addr: String,
}

impl Published {
    /// The address a frontend will find.
    pub fn addr(&self) -> &str {
        &self.addr
    }
}

impl fmt::Debug for Published {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Published({})", self.addr)
    }
}

/// Announce `addr` (a `host:port` a frontend can connect to) on [`SEGMENT`].
pub fn publish(addr: &str) -> Result<Published, String> {
    publish_as(SEGMENT, addr)
}

/// The address the last daemon published, if one is there and looks live.
pub fn discover() -> Option<Announcement> {
    discover_on(SEGMENT)
}

/// [`discover`], as a plain `host:port`.
pub fn discover_addr() -> Option<String> {
    discover().map(|a| a.addr)
}

/// [`publish`] into a named segment (tests use their own, so they cannot
/// disturb a daemon running on the same machine).
pub fn publish_as(segment: &str, addr: &str) -> Result<Published, String> {
    let payload = encode(addr, std::process::id());
    let mapping = imp::create(segment, CAPACITY)?;
    imp::write(&mapping, &payload);
    Ok(Published {
        mapping,
        addr: addr.to_string(),
    })
}

/// [`discover`] from a named segment.
pub fn discover_on(segment: &str) -> Option<Announcement> {
    let mapping = imp::open(segment, CAPACITY)?;
    let bytes = imp::read(&mapping);
    let announcement = parse(&bytes)?;
    if !publisher_alive(announcement.pid) {
        log::debug!(
            "{} was published by pid {} which is gone; ignoring it",
            segment,
            announcement.pid
        );
        return None;
    }
    Some(announcement)
}

/// `EASYTERM1 <pid> <addr>\0`, padded by the caller's mapping.
fn encode(addr: &str, pid: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(CAPACITY);
    out.extend_from_slice(MAGIC.as_bytes());
    out.push(b' ');
    out.extend_from_slice(pid.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(addr.as_bytes());
    out.push(0);
    out
}

/// Read a payload back, or `None` for anything that is not ours (including an
/// empty or half-written segment).
fn parse(bytes: &[u8]) -> Option<Announcement> {
    let end = bytes.iter().position(|b| *b == 0)?;
    let text = std::str::from_utf8(&bytes[..end]).ok()?;
    let mut fields = text.split(' ');
    if fields.next()? != MAGIC {
        return None;
    }
    let pid = fields.next()?.parse::<u32>().ok()?;
    let addr = fields.next()?;
    if addr.is_empty() {
        return None;
    }
    Some(Announcement {
        addr: addr.to_string(),
        pid,
    })
}

/// Whether the publishing process is still there.
///
/// Only meaningful where a segment can outlive its publisher. On Windows the OS
/// destroys the mapping with the process, so this is always true.
#[cfg(unix)]
fn publisher_alive(pid: u32) -> bool {
    if pid == std::process::id() {
        return true;
    }
    // Signal 0 is the POSIX "does this process exist" probe: it performs the
    // permission and existence checks without sending anything.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn publisher_alive(_pid: u32) -> bool {
    true
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::ptr;
    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
        FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };

    /// A mapped view of a named segment.
    pub struct Mapping {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        len: usize,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }

    // SAFETY: a mapping is a process-wide handle, not thread-affine: Windows
    // unmaps it from whichever thread closes it. The view is written exactly
    // once, by the thread that created it, inside `publish_as`; afterwards the
    // handle is only *held* (so a daemon can move it into the task that lives as
    // long as the server) and finally dropped.
    unsafe impl Send for Mapping {}

    fn wide(name: &str) -> Vec<u16> {
        name.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn last_error() -> u32 {
        unsafe { GetLastError() }
    }

    /// Create (or replace) the named segment and map it writable.
    pub fn create(name: &str, len: usize) -> Result<Mapping, String> {
        let name = wide(name);
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                0,
                len as u32,
                name.as_ptr(),
            );
            if handle == 0 {
                return Err(format!("CreateFileMappingW failed (error {})", last_error()));
            }
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len);
            if view.Value.is_null() {
                let error = last_error();
                CloseHandle(handle);
                return Err(format!("MapViewOfFile failed (error {error})"));
            }
            Ok(Mapping { handle, view, len })
        }
    }

    /// Open an existing segment read-only, or `None` when there is none.
    pub fn open(name: &str, len: usize) -> Option<Mapping> {
        let name = wide(name);
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr());
            if handle == 0 {
                return None;
            }
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, len);
            if view.Value.is_null() {
                CloseHandle(handle);
                return None;
            }
            Some(Mapping { handle, view, len })
        }
    }

    pub fn write(mapping: &Mapping, bytes: &[u8]) {
        let len = bytes.len().min(mapping.len);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.view.Value as *mut u8, len);
        }
    }

    pub fn read(mapping: &Mapping) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(mapping.view.Value as *const u8, mapping.len).to_vec() }
    }

    // `c_void` is named in the `MEMORY_MAPPED_VIEW_ADDRESS` field's type; this
    // keeps the import honest if that ever changes.
    const _: fn() = || {
        let _ = std::mem::size_of::<c_void>();
    };
}

#[cfg(unix)]
mod imp {
    use std::ffi::CString;
    use std::ptr;

    /// A mapped view of a POSIX shared-memory object.
    pub struct Mapping {
        name: CString,
        ptr: *mut u8,
        len: usize,
        /// Only the creator unlinks: a reader must not remove the daemon's
        /// announcement when it closes its view.
        owner: bool,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr as *mut libc::c_void, self.len);
                if self.owner {
                    libc::shm_unlink(self.name.as_ptr());
                }
            }
        }
    }

    // SAFETY: see the Windows impl — `munmap`/`shm_unlink` are process-wide
    // operations, and the mapping's bytes are written only by the thread that
    // created it, before the handle is handed to anyone.
    unsafe impl Send for Mapping {}

    /// Create (or truncate) the named object and map it writable.
    pub fn create(name: &str, len: usize) -> Result<Mapping, String> {
        let cname = CString::new(name).map_err(|e| format!("bad segment name: {e}"))?;
        unsafe {
            // O_CREAT without O_EXCL: the previous daemon's object is replaced,
            // and 0600 keeps it to this user (the same boundary as the loopback
            // listener).
            let fd = libc::shm_open(cname.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            if fd < 0 {
                return Err(format!(
                    "shm_open {name} failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                let error = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(format!("ftruncate {name} failed: {error}"));
            }
            let ptr = libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            if ptr == libc::MAP_FAILED {
                return Err(format!(
                    "mmap {name} failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(Mapping {
                name: cname,
                ptr: ptr as *mut u8,
                len,
                owner: true,
            })
        }
    }

    /// Open an existing object read-only, or `None` when there is none.
    pub fn open(name: &str, len: usize) -> Option<Mapping> {
        let cname = CString::new(name).ok()?;
        unsafe {
            let fd = libc::shm_open(cname.as_ptr(), libc::O_RDONLY, 0);
            if fd < 0 {
                return None;
            }
            // Map at most what the object actually holds: a creator that died
            // between `shm_open` and `ftruncate` left a zero-length object.
            let mut stat: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut stat) != 0 {
                libc::close(fd);
                return None;
            }
            let len = (stat.st_size as usize).clamp(1, len);
            let ptr = libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            libc::close(fd);
            if ptr == libc::MAP_FAILED {
                return None;
            }
            Some(Mapping {
                name: cname,
                ptr: ptr as *mut u8,
                len,
                owner: false,
            })
        }
    }

    pub fn write(mapping: &Mapping, bytes: &[u8]) {
        let len = bytes.len().min(mapping.len);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.ptr, len);
        }
    }

    pub fn read(mapping: &Mapping) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(mapping.ptr as *const u8, mapping.len).to_vec() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every test uses its own segment: a daemon may be running on this machine
    /// and must not be disturbed by the tests.
    fn segment(tag: &str) -> String {
        format!(
            "easyterm-discovery-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    #[test]
    fn a_published_address_is_discoverable() {
        let segment = segment("roundtrip");
        let published = publish_as(&segment, "127.0.0.1:58769").expect("publish");
        assert_eq!(published.addr(), "127.0.0.1:58769");
        assert_eq!(
            discover_on(&segment),
            Some(Announcement {
                addr: "127.0.0.1:58769".to_string(),
                pid: std::process::id(),
            })
        );
    }

    #[test]
    fn the_newest_publisher_wins() {
        // Two daemons (the GUI's in-process one and a standalone one, say): the
        // last to start is the one that answers.
        let segment = segment("newest");
        let _first = publish_as(&segment, "127.0.0.1:1000").expect("publish first");
        let second = publish_as(&segment, "127.0.0.1:2000").expect("publish second");
        assert_eq!(discover_on(&segment).map(|a| a.addr), Some("127.0.0.1:2000".to_string()));
        drop(second);
    }

    #[test]
    fn nothing_is_discovered_where_nothing_was_published() {
        assert_eq!(discover_on(&segment("absent")), None);
    }

    #[test]
    fn the_payload_is_versioned_and_bounded() {
        let encoded = encode("127.0.0.1:58769", 4242);
        assert!(encoded.ends_with(&[0]), "NUL-terminated");
        assert!(encoded.len() < CAPACITY, "fits the segment");
        let parsed = parse(&encoded).expect("parses back");
        assert_eq!(parsed.addr, "127.0.0.1:58769");
        assert_eq!(parsed.pid, 4242);
    }

    #[test]
    fn a_foreign_or_partial_payload_is_ignored() {
        // Another program's data, an older format, and a half-written segment
        // must all read as "no daemon announced itself".
        assert_eq!(parse(b"someone-elses-data\0"), None);
        assert_eq!(parse(b"EASYTERM0 1 127.0.0.1:1\0"), None);
        assert_eq!(parse(b""), None);
        assert_eq!(parse(&[b'E', b'A', b'S', b'Y']), None, "no NUL: not written yet");
        assert_eq!(parse(b"EASYTERM1 notapid 127.0.0.1:1\0"), None);
        assert_eq!(parse(b"EASYTERM1 7 \0"), None);
    }

    #[test]
    fn an_ids_address_is_not_an_announcement() {
        let encoded = encode("", 1);
        assert_eq!(parse(&encoded), None);
    }
}
