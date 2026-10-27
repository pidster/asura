//! The session state a Keychain error records as field evidence
//! (`docs/designs/release-signing.md`, Keychain field evidence, F3).
//!
//! [`Session::read`] makes three system calls: `sysctl kern.boottime`, a
//! `stat` of `/dev/console` and `getaudit_addr`. None of them waits on
//! `securityd`, but the secret store worker still makes them on its own
//! thread, after a failed Keychain call (AE-02). The result has four
//! fixed-size fields and no string from the system: no user, account or
//! path name.
use std::ffi::c_int;

/// `AU_SESSION_FLAG_HAS_GRAPHIC_ACCESS` (`<bsm/audit_session.h>`).
pub const FLAG_HAS_GRAPHIC_ACCESS: u64 = 0x0010;
/// `AU_SESSION_FLAG_IS_REMOTE`: set for an SSH login session.
pub const FLAG_IS_REMOTE: u64 = 0x1000;

/// Who owns the console (`/dev/console`) compared with this process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Console {
    /// This process's real user owns the console: a GUI login.
    SameUser,
    /// Another user owns the console.
    OtherUser,
    /// Root owns the console: the login window, nobody logged in.
    LoginWindow,
    /// The `stat` failed.
    Unknown,
}

impl Console {
    /// The audit name: `self`, `other`, `none` or `unknown`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameUser => "self",
            Self::OtherUser => "other",
            Self::LoginWindow => "none",
            Self::Unknown => "unknown",
        }
    }
}

/// How the process's audit session started, from its flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A remote login (SSH).
    Remote,
    /// A GUI login session (`loginwindow` and `launchd`).
    Graphic,
    /// Neither, for example a `launchd` background session.
    Other,
    /// `getaudit_addr` failed.
    Unknown,
}

impl Origin {
    /// The audit name: `remote`, `graphic`, `other` or `unknown`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Remote => "remote",
            Self::Graphic => "graphic",
            Self::Other => "other",
            Self::Unknown => "unknown",
        }
    }
}

/// The session state at one Keychain error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Session {
    /// Seconds since 1970 at boot; `None` when the `sysctl` failed.
    pub boot_time_s: Option<u64>,
    pub console: Console,
    /// The raw audit session flags; `None` when `getaudit_addr` failed.
    pub audit_flags: Option<u64>,
}

impl Session {
    /// Reads the session state now. Each failed system call leaves its
    /// field absent or unknown; the read itself never fails.
    pub fn read() -> Self {
        // SAFETY: `getuid` takes no argument and cannot fail.
        let uid = unsafe { libc::getuid() };
        Self {
            boot_time_s: boot_time(),
            console: console_from(console_owner(), uid),
            audit_flags: audit_flags(),
        }
    }

    /// The origin derived from [`Session::audit_flags`].
    pub fn origin(&self) -> Origin {
        origin_from(self.audit_flags)
    }
}

/// The console state from the owner of `/dev/console` and this user.
pub fn console_from(owner: Option<u32>, uid: u32) -> Console {
    match owner {
        None => Console::Unknown,
        Some(0) => Console::LoginWindow,
        Some(owner) if owner == uid => Console::SameUser,
        Some(_) => Console::OtherUser,
    }
}

/// The origin from the audit session flags: remote wins over graphic.
pub fn origin_from(flags: Option<u64>) -> Origin {
    match flags {
        None => Origin::Unknown,
        Some(flags) if flags & FLAG_IS_REMOTE != 0 => Origin::Remote,
        Some(flags) if flags & FLAG_HAS_GRAPHIC_ACCESS != 0 => Origin::Graphic,
        Some(_) => Origin::Other,
    }
}

fn boot_time() -> Option<u64> {
    let mut value = libc::timeval {
        tv_sec: 0,
        tv_usec: 0,
    };
    let mut length = std::mem::size_of::<libc::timeval>();
    // SAFETY: the name is a NUL-terminated literal; `value` and `length` are
    // live locals and `length` holds the size of `value`, so the kernel
    // writes at most that many bytes. No new value is set (null, 0).
    let code = unsafe {
        libc::sysctlbyname(
            c"kern.boottime".as_ptr(),
            (&raw mut value).cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    };
    (code == 0 && length == std::mem::size_of::<libc::timeval>())
        .then(|| u64::try_from(value.tv_sec).ok())
        .flatten()
}

fn console_owner() -> Option<u32> {
    // SAFETY: an all-zero `stat` is a valid value of the plain C struct.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the path is a NUL-terminated literal and `status` is a live
    // local the call fills.
    let code = unsafe { libc::stat(c"/dev/console".as_ptr(), &mut status) };
    (code == 0).then_some(status.st_uid)
}

/// `auditinfo_addr_t` of `<bsm/audit.h>` on macOS: 48 bytes, the flags at
/// offset 40 (checked against the macOS 27.0 SDK header).
#[repr(C)]
struct AuditInfoAddr {
    auid: u32,
    mask: [u32; 2],
    port: i32,
    kind: u32,
    address: [u32; 4],
    asid: i32,
    flags: u64,
}

const _: () = assert!(std::mem::size_of::<AuditInfoAddr>() == 48);
const _: () = assert!(std::mem::offset_of!(AuditInfoAddr, flags) == 40);

unsafe extern "C" {
    fn getaudit_addr(info: *mut AuditInfoAddr, length: c_int) -> c_int;
}

fn audit_flags() -> Option<u64> {
    let mut info = AuditInfoAddr {
        auid: 0,
        mask: [0; 2],
        port: 0,
        kind: 0,
        address: [0; 4],
        asid: 0,
        flags: 0,
    };
    // SAFETY: `info` is a live local of the documented layout and the length
    // passed is its size, so the kernel writes only inside it.
    let code = unsafe { getaudit_addr(&mut info, std::mem::size_of::<AuditInfoAddr>() as c_int) };
    (code == 0).then_some(info.flags)
}
