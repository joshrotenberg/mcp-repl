use std::future::Future;
use std::io;
use std::mem::size_of;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use super::{HOST, Host, MAX_FRAME, Outgoing, SHUTDOWN_GRACE, Withdrawal, lost_error, receive};

const QUEUE_SIZE: usize = 64;

pub(super) fn claim(fd: i32) -> Result<UnixStream, String> {
    if fd < 3 {
        return Err("--host-input-fd must be at least 3; standard streams are reserved".into());
    }
    // SAFETY: These probes take a descriptor value and cannot take ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err("--host-input-fd is not open".into());
    }
    let mut kind: libc::c_int = 0;
    let mut size = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: The output buffer has exactly the supplied size.
    let status = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut kind as *mut libc::c_int).cast(),
            &mut size,
        )
    };
    let mut address = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut size = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: The output buffer has exactly the supplied size.
    let peer = unsafe { libc::getpeername(fd, address.as_mut_ptr().cast(), &mut size) };
    // SAFETY: The buffer was zero-initialized even if getpeername failed.
    let family = unsafe { address.assume_init().ss_family };
    if status != 0 || kind != libc::SOCK_STREAM || peer != 0 || i32::from(family) != libc::AF_UNIX {
        return Err("--host-input-fd must be a connected Unix stream socket".into());
    }
    // SAFETY: The caller transfers an open Unix socket before the runtime or
    // any worker can race the ownership conversion or spawn a child.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    // SAFETY: We own fd and only alter its descriptor flags.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
        return Err("cannot set close-on-exec on --host-input-fd".into());
    }
    stream
        .set_nonblocking(true)
        .map_err(|_| "cannot set --host-input-fd nonblocking".to_owned())?;
    Ok(stream)
}

pub(super) async fn run<T>(
    stream: UnixStream,
    timeout: Duration,
    run: impl Future<Output = tower_mcp::Result<T>>,
) -> tower_mcp::Result<T> {
    let stream = tokio::net::UnixStream::from_std(stream)
        .map_err(|_| super::transport_error("cannot register host input socket"))?;
    let mut nonce = [0_u8; 16];
    getrandom::fill(&mut nonce)
        .map_err(|_| super::transport_error("cannot generate host input session identity"))?;
    let (outgoing, receiver) = mpsc::channel(QUEUE_SIZE);
    let host = HOST.get_or_init(|| Host {
        nonce: nonce.iter().map(|byte| format!("{byte:02x}")).collect(),
        timeout,
        outgoing,
        lost: Default::default(),
        ledger: Default::default(),
    });
    let io = tokio::spawn(async move {
        let (reader, writer) = stream.into_split();
        tokio::select! {
            () = read(BufReader::new(reader), host) => {}
            () = write(writer, receiver, timeout) => {}
        }
        host.lost.cancel();
        host.withdraw_all(Withdrawal::HostGone);
    });
    tokio::pin!(run);
    let result = tokio::select! {
        biased;
        () = host.lost.cancelled() => {
            // Let the active command and transport close first. This cap
            // also covers a host that disappears during startup or an RPC
            // that would otherwise wait indefinitely.
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, run).await;
            Err(lost_error())
        }
        result = &mut run => result,
    };
    io.abort();
    let _ = io.await;
    result
}

async fn read(mut reader: impl AsyncBufRead + Unpin, host: &Host) {
    let mut frame = Vec::new();
    while read_frame(&mut reader, &mut frame).await.unwrap_or(false) {
        receive(host, &frame);
    }
}

async fn read_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    frame: &mut Vec<u8>,
) -> io::Result<bool> {
    frame.clear();
    loop {
        let buf = reader.fill_buf().await?;
        if buf.is_empty() {
            return if frame.is_empty() {
                Ok(false)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        let newline = buf.iter().position(|&byte| byte == b'\n');
        let bytes = newline.map_or(buf.len(), |offset| offset + 1);
        if frame.len() + bytes > MAX_FRAME {
            return Err(io::ErrorKind::InvalidData.into());
        }
        frame.extend_from_slice(&buf[..bytes]);
        reader.consume(bytes);
        if newline.is_some() {
            return Ok(true);
        }
    }
}

async fn write(
    mut writer: impl AsyncWrite + Unpin,
    mut receiver: mpsc::Receiver<Outgoing>,
    stall: Duration,
) {
    while let Some(message) = receiver.recv().await {
        match message {
            Outgoing::Frame(frame) => {
                if write_frame(&mut writer, &frame, stall).await.is_err() {
                    return;
                }
            }
            // UnixStream is unbuffered. All frames before this marker were
            // written, including the last acknowledgement in one-shot mode.
            Outgoing::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    mut bytes: &[u8],
    stall: Duration,
) -> io::Result<()> {
    while !bytes.is_empty() {
        let written = tokio::time::timeout(stall, writer.write(bytes)).await??;
        if written == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsRawFd, IntoRawFd};

    #[test]
    fn claim_sets_close_on_exec_and_rejects_invalid_descriptors() {
        assert!(claim(2).unwrap_err().contains("reserved"));
        assert!(claim(i32::MAX).unwrap_err().contains("not open"));
        let file = tempfile::tempfile().unwrap();
        assert!(claim(file.as_raw_fd()).unwrap_err().contains("Unix stream"));
        let (socket, _peer) = UnixStream::pair().unwrap();
        let fd = socket.into_raw_fd();
        // SAFETY: fd remains open throughout, with ownership passed to claim next.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }, 0);
        let socket = claim(fd).unwrap();
        // SAFETY: socket owns this open descriptor.
        let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
    }

    #[tokio::test]
    async fn framing_bounds_complete_and_partial_input() {
        let mut input = vec![b'a'; MAX_FRAME];
        input[MAX_FRAME - 1] = b'\n';
        let mut frame = Vec::new();
        assert!(
            read_frame(&mut BufReader::new(input.as_slice()), &mut frame)
                .await
                .unwrap()
        );
        assert_eq!(frame.len(), MAX_FRAME);
        input.insert(0, b'a');
        let result = read_frame(&mut BufReader::new(input.as_slice()), &mut frame).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
        let result = read_frame(&mut BufReader::new(&b"truncated"[..]), &mut frame).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[tokio::test]
    async fn a_write_stalls_after_filling_the_buffer() {
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let result = write_frame(&mut writer, b"ab", Duration::ZERO).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        let mut written = [0; 1];
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut written)
            .await
            .unwrap();
        assert_eq!(&written, b"a");
    }
}
