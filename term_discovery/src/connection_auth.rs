//! Per-run daemon connection credentials, accessible only to the OS user.
//!
//! This is separate from address discovery and from the vault: a locked vault
//! must not prevent the owner from logging in or using an offline license.
//! The public random `id` identifies a short-lived mapping, never the token.
//! No credential is persisted in the installation database or a command line.

use hmac::{Hmac, Mac};
use std::io;

const TOKEN_LEN: usize = 64;
const CAPACITY: usize = TOKEN_LEN + 1;

#[derive(Clone, Copy)]
pub enum ProofRole {
    Client,
    Server,
}

/// Fresh challenge material for one connection, independent of wall time.
pub fn new_nonce() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| io::Error::other("secure randomness unavailable"))?;
    Ok(hex::encode(bytes))
}

fn proof_mac(
    token: &str,
    id: &str,
    server_nonce: &str,
    client_nonce: &str,
    role: ProofRole,
) -> io::Result<Hmac<sha2::Sha256>> {
    if !hex_len(token, TOKEN_LEN)
        || !hex_len(id, 16)
        || !hex_len(server_nonce, 64)
        || !hex_len(client_nonce, 64)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid connection proof transcript",
        ));
    }
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(token.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(match role {
        ProofRole::Client => b"EasyTerm IPC auth v8\0client\0",
        ProofRole::Server => b"EasyTerm IPC auth v8\0server\0",
    });
    // Each field has a checked fixed width, so this transcript is unambiguous.
    mac.update(id.as_bytes());
    mac.update(server_nonce.as_bytes());
    mac.update(client_nonce.as_bytes());
    Ok(mac)
}

pub fn proof(
    token: &str,
    id: &str,
    server_nonce: &str,
    client_nonce: &str,
    role: ProofRole,
) -> io::Result<String> {
    Ok(hex::encode(
        proof_mac(token, id, server_nonce, client_nonce, role)?
            .finalize()
            .into_bytes(),
    ))
}

pub fn verify_proof(
    token: &str,
    id: &str,
    server_nonce: &str,
    client_nonce: &str,
    role: ProofRole,
    presented: &str,
) -> bool {
    if !hex_len(presented, 64) {
        return false;
    }
    let Ok(mac) = proof_mac(token, id, server_nonce, client_nonce, role) else {
        return false;
    };
    let Ok(bytes) = hex::decode(presented) else {
        return false;
    };
    mac.verify_slice(&bytes).is_ok()
}

fn hex_len(value: &str, len: usize) -> bool {
    value.len() == len && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn name(port: u16, id: &str) -> io::Result<String> {
    if port == 0 || !hex_len(id, 16) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid connection credential identity",
        ));
    }
    // The POSIX name stays below macOS's 31-byte limit, including the slash.
    Ok(format!("et-a-{port}-{id}"))
}

/// Keep alive for the lifetime of a listening daemon. Deliberately no Debug:
/// the mapping and its contents are credentials.
pub struct PublishedToken {
    _mapping: imp::Mapping,
}

pub fn publish(port: u16, id: &str, token: &str) -> io::Result<PublishedToken> {
    if !hex_len(token, TOKEN_LEN) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid connection credential",
        ));
    }
    let mapping = imp::create(&name(port, id)?, CAPACITY)?;
    let mut bytes = token.as_bytes().to_vec();
    bytes.push(0);
    imp::write(&mapping, &bytes);
    Ok(PublishedToken { _mapping: mapping })
}

pub fn read(port: u16, id: &str) -> io::Result<String> {
    let mapping = imp::open(&name(port, id)?, CAPACITY)?;
    let bytes = imp::read(&mapping);
    if bytes.len() != CAPACITY || bytes[TOKEN_LEN] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid connection credential record",
        ));
    }
    let token = std::str::from_utf8(&bytes[..TOKEN_LEN]).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid connection credential record",
        )
    })?;
    if !hex_len(token, TOKEN_LEN) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid connection credential record",
        ));
    }
    Ok(token.to_owned())
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::ptr;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
        PSID,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetSecurityInfo, SE_KERNEL_OBJECT,
    };
    use windows_sys::Win32::Security::{
        EqualSid, GetAce, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::READ_CONTROL;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
        FILE_MAP_READ, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    pub struct Mapping {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        len: usize,
    }
    // The mapping is initialized before publication and only read afterward.
    unsafe impl Send for Mapping {}
    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }
    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn sid_string(value: PSID) -> io::Result<String> {
        unsafe {
            let mut sid = ptr::null_mut();
            if ConvertSidToStringSidW(value, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *sid.add(len) != 0 {
                len += 1;
            }
            let value = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
            LocalFree(sid.cast());
            Ok(value)
        }
    }

    fn user_sid() -> io::Result<String> {
        unsafe {
            let mut token = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut needed = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
            // usize storage gives TOKEN_USER its required alignment.
            let mut data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                data.as_mut_ptr().cast(),
                needed,
                &mut needed,
            );
            let error = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(error);
            }
            let user = &*(data.as_ptr().cast::<TOKEN_USER>());
            sid_string(user.User.Sid)
        }
    }

    fn validate_security(handle: HANDLE) -> io::Result<()> {
        let current_user = user_sid()?;
        let denied = || {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "connection credential owner or permissions are invalid",
            )
        };
        unsafe {
            let mut owner = ptr::null_mut();
            let mut dacl = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            let status = GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                &mut dacl,
                ptr::null_mut(),
                &mut descriptor,
            );
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            let result = (|| {
                if owner.is_null()
                    || dacl.is_null()
                    || sid_string(owner)? != current_user
                    || (*dacl).AceCount != 1
                {
                    return Err(denied());
                }
                let mut ace = ptr::null_mut();
                if GetAce(dacl, 0, &mut ace) == 0 || ace.is_null() {
                    return Err(denied());
                }
                let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                if ace.Header.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE
                    || (ace.Header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                    || EqualSid(ptr::addr_of!(ace.SidStart).cast_mut().cast(), owner) == 0
                {
                    return Err(denied());
                }
                Ok(())
            })();
            LocalFree(descriptor);
            result
        }
    }

    pub fn create(name: &str, len: usize) -> io::Result<Mapping> {
        let sid = user_sid()?;
        let sddl = wide(&format!("O:{sid}D:P(A;;GA;;;{sid})"));
        let name = wide(&format!("Local\\{name}"));
        unsafe {
            let mut descriptor = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let security = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &security,
                PAGE_READWRITE,
                0,
                len as u32,
                name.as_ptr(),
            );
            let error = GetLastError();
            LocalFree(descriptor);
            if handle == 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            // Never reuse a pre-created object with an unknown DACL/owner.
            if error == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "connection credential identity already exists",
                ));
            }
            let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, len);
            if view.Value.is_null() {
                let error = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error);
            }
            Ok(Mapping { handle, view, len })
        }
    }
    pub fn open(name: &str, len: usize) -> io::Result<Mapping> {
        let name = wide(&format!("Local\\{name}"));
        unsafe {
            let handle = OpenFileMappingW(FILE_MAP_READ | READ_CONTROL, 0, name.as_ptr());
            if handle == 0 {
                return Err(io::Error::last_os_error());
            }
            if let Err(error) = validate_security(handle) {
                CloseHandle(handle);
                return Err(error);
            }
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, len);
            if view.Value.is_null() {
                let error = io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error);
            }
            Ok(Mapping { handle, view, len })
        }
    }
    pub fn write(mapping: &Mapping, bytes: &[u8]) {
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.view.Value.cast::<u8>(), bytes.len());
        }
    }
    pub fn read(mapping: &Mapping) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(mapping.view.Value.cast::<u8>(), mapping.len).to_vec() }
    }

    #[test]
    fn rejects_a_world_readable_security_descriptor() {
        let sid = user_sid().unwrap();
        let sddl = wide(&format!("O:{sid}D:P(A;;GA;;;{sid})(A;;GR;;;WD)"));
        unsafe {
            let mut descriptor = ptr::null_mut();
            assert_ne!(
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    1,
                    &mut descriptor,
                    ptr::null_mut()
                ),
                0
            );
            let security = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                &security,
                PAGE_READWRITE,
                0,
                65,
                ptr::null(),
            );
            LocalFree(descriptor);
            assert_ne!(handle, 0);
            let result = validate_security(handle);
            CloseHandle(handle);
            assert!(result.is_err());
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::ptr;
    pub struct Mapping {
        name: CString,
        ptr: *mut u8,
        len: usize,
        owner: bool,
    }
    unsafe impl Send for Mapping {}
    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.ptr.cast(), self.len);
                if self.owner {
                    libc::shm_unlink(self.name.as_ptr());
                }
            }
        }
    }
    fn open_mapping(name: &str, len: usize, create: bool) -> io::Result<Mapping> {
        let name = CString::new(format!("/{name}")).map_err(io::Error::other)?;
        unsafe {
            let flags = if create {
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR
            } else {
                libc::O_RDONLY
            };
            let fd = libc::shm_open(name.as_ptr(), flags, 0o600);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let result = (|| {
                if create && libc::ftruncate(fd, len as libc::off_t) != 0 {
                    return Err(io::Error::last_os_error());
                }
                let mut stat: libc::stat = std::mem::zeroed();
                if libc::fstat(fd, &mut stat) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if stat.st_uid != libc::geteuid()
                    || stat.st_mode & 0o077 != 0
                    || stat.st_size != len as libc::off_t
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "connection credential owner or permissions are invalid",
                    ));
                }
                let prot = if create {
                    libc::PROT_READ | libc::PROT_WRITE
                } else {
                    libc::PROT_READ
                };
                let ptr = libc::mmap(ptr::null_mut(), len, prot, libc::MAP_SHARED, fd, 0);
                if ptr == libc::MAP_FAILED {
                    return Err(io::Error::last_os_error());
                }
                Ok(Mapping {
                    name: name.clone(),
                    ptr: ptr.cast(),
                    len,
                    owner: create,
                })
            })();
            libc::close(fd);
            if create && result.is_err() {
                libc::shm_unlink(name.as_ptr());
            }
            result
        }
    }
    pub fn create(name: &str, len: usize) -> io::Result<Mapping> {
        open_mapping(name, len, true)
    }
    pub fn open(name: &str, len: usize) -> io::Result<Mapping> {
        open_mapping(name, len, false)
    }
    pub fn write(mapping: &Mapping, bytes: &[u8]) {
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), mapping.ptr, bytes.len());
        }
    }
    pub fn read(mapping: &Mapping) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(mapping.ptr, mapping.len).to_vec() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proofs_bind_both_nonces_identity_and_direction() {
        let token = "a".repeat(64);
        let server = "b".repeat(64);
        let client = "c".repeat(64);
        let changed = "d".repeat(64);
        let id = "0123456789abcdef";
        let value = proof(&token, id, &server, &client, ProofRole::Client).unwrap();
        assert!(verify_proof(
            &token,
            id,
            &server,
            &client,
            ProofRole::Client,
            &value
        ));
        assert!(!verify_proof(
            &token,
            id,
            &server,
            &client,
            ProofRole::Server,
            &value
        ));
        assert!(!verify_proof(
            &token,
            id,
            &changed,
            &client,
            ProofRole::Client,
            &value
        ));
        assert!(!verify_proof(
            &token,
            id,
            &server,
            &changed,
            ProofRole::Client,
            &value
        ));
        assert!(!verify_proof(
            &token,
            "1123456789abcdef",
            &server,
            &client,
            ProofRole::Client,
            &value
        ));
        assert!(!verify_proof(
            &changed,
            id,
            &server,
            &client,
            ProofRole::Client,
            &value
        ));
        assert!(proof(&token, id, "short", &client, ProofRole::Client).is_err());
    }
    #[test]
    fn credential_is_private_exclusive_and_removed_with_publisher() {
        let id = format!(
            "{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
        );
        let token = "a".repeat(TOKEN_LEN);
        let published = publish(12345, &id, &token).unwrap();
        assert_eq!(read(12345, &id).unwrap(), token);
        assert!(publish(12345, &id, &token).is_err());
        assert!(read(12346, &id).is_err());
        drop(published);
        assert!(read(12345, &id).is_err());
    }
    #[test]
    fn rejects_untrusted_mapping_names() {
        assert!(read(1234, "../other").is_err());
        assert!(read(0, "0123456789abcdef").is_err());
        assert!(publish(1234, "0123456789abcdef", "short").is_err());
    }
}
