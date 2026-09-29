//! Private launch-owner lifetime; no PID lookup or signalling policy.
use crate::{Error, Result, checked, last_error, nonblocking};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// The sole writer. Dropping it releases ownership of the launched backend.
/// It intentionally cannot be cloned and exposes no descriptor.
pub struct ServiceLease {
    _writer: OwnedFd,
}
/// Read-only lifetime observation passed to the canonical service reactor.
pub struct ServiceLifetime(OwnedFd);
impl ServiceLifetime {
    pub fn new() -> Result<(ServiceLease, Self)> {
        let (reader, writer) = crate::startup::pipe()?;
        nonblocking(reader.as_raw_fd())?;
        Ok((ServiceLease { _writer: writer }, Self(reader)))
    }
    /// Take fixed fd4 only for the private inherited-lifetime launch invocation.
    /// The caller must invoke this once, before accessing runtime state.
    pub fn inherited() -> Result<Self> {
        Self::validate_descriptor(4)?;
        // SAFETY: the private launch contract transfers validated fd4 ownership.
        let reader = unsafe { OwnedFd::from_raw_fd(4) };
        nonblocking(reader.as_raw_fd())?;
        // SAFETY: F_GETFD inspects the retained descriptor without pointers.
        let flags = unsafe { libc::fcntl(reader.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 {
            return Err(last_error());
        }
        // SAFETY: only inheritance flags of the retained descriptor change.
        checked(unsafe {
            libc::fcntl(reader.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC)
        })?;
        Ok(Self(reader))
    }
    fn validate_descriptor(fd: RawFd) -> Result<()> {
        // SAFETY: valid writable stat output; descriptor inspection takes no ownership.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(Error::InvalidNotice);
        }
        // SAFETY: F_GETFL has no pointer arguments.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0
            || stat.st_mode & libc::S_IFMT != libc::S_IFIFO
            || flags & libc::O_ACCMODE != libc::O_RDONLY
        {
            return Err(Error::InvalidNotice);
        }
        Ok(())
    }
    /// EOF ends ownership. Unexpected data is a contract error. An interrupted
    /// observation yields to the next bounded reactor turn without spinning.
    pub fn ended(&mut self) -> Result<bool> {
        let mut byte = 0u8;
        // SAFETY: owned nonblocking reader and one-byte writable output.
        let count = unsafe { libc::read(self.0.as_raw_fd(), (&mut byte as *mut u8).cast(), 1) };
        match count {
            0 => Ok(true),
            1 => Err(Error::InvalidNotice),
            _ => {
                let error = io::Error::last_os_error();
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) {
                    Ok(false)
                } else {
                    Err(error.into())
                }
            }
        }
    }
}
impl AsRawFd for ServiceLifetime {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sole_lease_drop_produces_nonblocking_eof() {
        let (lease, mut lifetime) = ServiceLifetime::new().unwrap();
        assert!(!lifetime.ended().unwrap());
        for fd in [lease._writer.as_raw_fd(), lifetime.as_raw_fd()] {
            // SAFETY: retained isolated descriptors, read-only flag query.
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        drop(lease);
        assert!(lifetime.ended().unwrap());
        assert!(lifetime.ended().unwrap());
    }
    #[test]
    fn descriptor_direction_type_and_unexpected_data_are_rejected() {
        let (lease, mut lifetime) = ServiceLifetime::new().unwrap();
        ServiceLifetime::validate_descriptor(lifetime.as_raw_fd()).unwrap();
        assert!(ServiceLifetime::validate_descriptor(lease._writer.as_raw_fd()).is_err());
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(ServiceLifetime::validate_descriptor(null.as_raw_fd()).is_err());
        assert!(ServiceLifetime::validate_descriptor(-1).is_err());
        // SAFETY: one initialized byte and isolated owned writer with live reader.
        assert_eq!(
            unsafe { libc::write(lease._writer.as_raw_fd(), b"x".as_ptr().cast(), 1) },
            1
        );
        assert!(matches!(lifetime.ended(), Err(Error::InvalidNotice)));
    }
}
