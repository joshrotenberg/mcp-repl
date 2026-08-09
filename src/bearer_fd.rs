//! Ephemeral bearer input from an inherited Unix file descriptor.
//!
//! The descriptor is an ownership transfer: this process reads it once and
//! closes it before the HTTP client or async runtime is built. Errors name the
//! descriptor and the violated constraint, never any bytes read from it.

/// Large enough for opaque and JWT-style credentials, while remaining below
/// the header limits commonly enforced by HTTP servers and proxies.
pub(crate) const MAX_BEARER_BYTES: usize = 16 * 1024;

#[cfg(unix)]
pub(crate) fn read(fd: i32) -> Result<String, String> {
    use std::io::Read;
    use std::os::fd::FromRawFd;

    if fd < 3 {
        return Err(format!(
            "invalid --bearer-fd {fd}: stdin, stdout, and stderr (0-2) are reserved"
        ));
    }
    validate_open(fd)?;

    // SAFETY: `validate_open` established that `fd` is live, and the CLI
    // contract transfers ownership to mcp-repl. No async runtime or worker
    // thread exists yet, so nothing inside this process can race the claim.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::new();
    let result = {
        let mut limited = (&mut file).take((MAX_BEARER_BYTES + 1) as u64);
        limited.read_to_end(&mut bytes)
    };
    // Close before inspecting or returning the input, on success and error.
    drop(file);
    result.map_err(|error| format!("cannot read --bearer-fd {fd}: {error}"))?;

    decode(fd, bytes)
}

#[cfg(unix)]
fn validate_open(fd: i32) -> Result<(), String> {
    loop {
        // `F_GETFD` is a non-mutating validity probe. It is required before
        // the unsafe ownership conversion: constructing `File` from a closed
        // descriptor would violate `FromRawFd`'s contract.
        let result = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if result >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return Err(format!(
            "invalid --bearer-fd {fd}: descriptor is not open ({error})"
        ));
    }
}

#[cfg(not(unix))]
pub(crate) fn read(fd: i32) -> Result<String, String> {
    Err(format!(
        "--bearer-fd {fd} is supported only on Unix; use MCP_BEARER or a secure OAuth profile on this platform"
    ))
}

fn decode(fd: i32, mut bytes: Vec<u8>) -> Result<String, String> {
    if bytes.len() > MAX_BEARER_BYTES {
        return Err(format!(
            "--bearer-fd {fd} exceeds the {MAX_BEARER_BYTES}-byte input limit"
        ));
    }

    // Producers conventionally write either `token\n` or `token\r\n`.
    // Remove one line ending, not arbitrary whitespace or repeated lines.
    if bytes.ends_with(b"\r\n") {
        bytes.truncate(bytes.len() - 2);
    } else if bytes.ends_with(b"\n") {
        bytes.truncate(bytes.len() - 1);
    }

    if bytes.is_empty() {
        return Err(format!("--bearer-fd {fd} supplied an empty bearer token"));
    }
    let token =
        String::from_utf8(bytes).map_err(|_| format!("--bearer-fd {fd} is not valid UTF-8"))?;
    if !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(format!(
            "--bearer-fd {fd} contains whitespace, control, or non-ASCII characters that cannot appear in a bearer token"
        ));
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_exactly_one_line_ending() {
        assert_eq!(decode(9, b"secret\n".to_vec()).unwrap(), "secret");
        assert_eq!(decode(9, b"secret\r\n".to_vec()).unwrap(), "secret");
        assert!(decode(9, b"secret\n\n".to_vec()).is_err());
    }

    #[test]
    fn rejects_empty_oversized_and_non_utf8_inputs_without_echoing_them() {
        let empty = decode(9, b"\n".to_vec()).unwrap_err();
        assert!(empty.contains("empty"));

        let oversized_secret = vec![b'x'; MAX_BEARER_BYTES + 1];
        let oversized = decode(9, oversized_secret).unwrap_err();
        assert!(oversized.contains("input limit"));
        assert!(!oversized.contains("xxxx"));

        let invalid = decode(9, vec![0xff, 0xfe]).unwrap_err();
        assert!(invalid.contains("UTF-8"));
    }

    #[cfg(unix)]
    #[test]
    fn reads_and_closes_the_owned_descriptor() {
        use std::io::Write;
        use std::net::Shutdown;
        use std::os::fd::IntoRawFd;
        use std::os::unix::net::UnixStream;

        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"ephemeral\n").unwrap();
        writer.shutdown(Shutdown::Write).unwrap();
        let fd = reader.into_raw_fd();

        assert_eq!(read(fd).unwrap(), "ephemeral");
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
    }

    #[cfg(unix)]
    #[test]
    fn reserved_and_closed_descriptors_are_rejected() {
        for fd in [0, 1, 2] {
            assert!(read(fd).unwrap_err().contains("reserved"));
        }
        assert!(read(i32::MAX).unwrap_err().contains("not open"));
    }
}
