//! Read the same system account information used for Unix ownership checks.

use std::ffi::{CStr, CString};
use std::io;
use std::mem::MaybeUninit;
use std::ptr;

/// Lookup functions kept together so tests can supply deterministic Unix identities.
pub(super) struct Accounts {
    pub user: fn(&str) -> io::Result<Option<u32>>,
    pub group: fn(&str) -> io::Result<Option<u32>>,
}

impl Accounts {
    pub const SYSTEM: Self = Self { user: user_id, group: group_id };
}

fn user_id(name: &str) -> io::Result<Option<u32>> {
    lookup(name, |name, record, buffer, result| {
        // SAFETY: lookup owns valid storage for the output record, buffer and result.
        unsafe { libc::getpwnam_r(name.as_ptr(), record, buffer.as_mut_ptr(), buffer.len(), result) }
    })
    .map(|record| record.map(|entry| entry.pw_uid))
}

fn group_id(name: &str) -> io::Result<Option<u32>> {
    lookup(name, |name, record, buffer, result| {
        // SAFETY: lookup owns valid storage for the output record, buffer and result.
        unsafe { libc::getgrnam_r(name.as_ptr(), record, buffer.as_mut_ptr(), buffer.len(), result) }
    })
    .map(|record| record.map(|entry| entry.gr_gid))
}

fn lookup<T>(
    name: &str,
    find: impl Fn(&CStr, *mut T, &mut [libc::c_char], &mut *mut T) -> libc::c_int,
) -> io::Result<Option<T>> {
    let name = CString::new(name)?;
    let mut buffer = vec![0; 1024];
    loop {
        let mut record = MaybeUninit::uninit();
        let mut result = ptr::null_mut();
        match find(&name, record.as_mut_ptr(), &mut buffer, &mut result) {
            0 if result.is_null() => return Ok(None),
            // SAFETY: the successful lookup with non-null result initialized the record.
            // Only its numeric UID or GID is read by the caller.
            0 => return Ok(Some(unsafe { record.assume_init() })),
            libc::ERANGE => buffer.resize(buffer.len() * 2, 0),
            code => return Err(io::Error::from_raw_os_error(code)),
        }
    }
}
