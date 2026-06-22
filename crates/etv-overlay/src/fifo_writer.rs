use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use nix::libc;
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;

/// How often the writer-side nonblocking open re-checks for a reader (and the
/// shutdown flag) while no ffmpeg is attached to the fifo.
const OPEN_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Outcome of waiting to open the fifo for writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    /// A reader attached and the fifo is now open for writing.
    Opened,
    /// The shutdown flag was set before any reader attached.
    Shutdown,
}

pub struct FifoWriter {
    path: PathBuf,
    owns_path: bool,
    file: Option<std::fs::File>,
}

impl FifoWriter {
    pub fn create(path: PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| anyhow::anyhow!("remove stale fifo {}: {e}", path.display()))?;
        }
        mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR)
            .map_err(|e| anyhow::anyhow!("mkfifo {}: {e}", path.display()))?;
        Ok(Self {
            path,
            owns_path: true,
            file: None,
        })
    }

    pub fn attach(path: PathBuf) -> Self {
        Self {
            path,
            owns_path: false,
            file: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn open_for_writing(&mut self, shutdown: &AtomicBool) -> anyhow::Result<OpenOutcome> {
        if self.file.is_some() {
            return Ok(OpenOutcome::Opened);
        }
        // Open write-only so that when the reader (the channel's ffmpeg) goes
        // away — etv-next spawns a fresh ffmpeg per playout item — the next
        // write returns BrokenPipe and we can reopen() on a frame boundary. An
        // O_RDWR fd (writer also holding a read end) never observes the reader
        // leaving, so every post-first-item ffmpeg would attach mid-frame and
        // render a torn, tiled overlay.
        //
        // A plain blocking O_WRONLY open would park uninterruptibly until a
        // reader appears, wedging a permanently-idle channel until SIGKILL.
        // Instead open O_NONBLOCK and retry: a writer-side nonblocking open
        // returns ENXIO while no reader is present, so we poll on a short
        // interval and bail promptly when `shutdown` is set. Once a reader is
        // there the open succeeds and we clear O_NONBLOCK, so writes block
        // normally (back-pressure on a slow reader instead of EAGAIN).
        loop {
            if shutdown.load(Ordering::SeqCst) {
                return Ok(OpenOutcome::Shutdown);
            }
            match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
            {
                Ok(file) => {
                    clear_nonblocking(&file)?;
                    self.file = Some(file);
                    return Ok(OpenOutcome::Opened);
                }
                // ENXIO == fifo opened write-only with no reader yet. Wait and
                // retry rather than failing.
                Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                    thread::sleep(OPEN_POLL_INTERVAL);
                }
                Err(e) => {
                    return Err(anyhow::anyhow!("open fifo {}: {e}", self.path.display()));
                }
            }
        }
    }

    /// Close the current write fd and reopen the fifo, waiting for the next
    /// reader. Called after a BrokenPipe so the next consumer's stream begins
    /// on a fresh frame boundary. Returns [`OpenOutcome::Shutdown`] if the
    /// shutdown flag is set while waiting.
    pub fn reopen(&mut self, shutdown: &AtomicBool) -> anyhow::Result<OpenOutcome> {
        self.file = None;
        self.open_for_writing(shutdown)
    }

    pub fn write_frame(&mut self, frame: &[u8]) -> std::io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("fifo not open"))?;
        file.write_all(frame)?;
        Ok(())
    }
}

impl Drop for FifoWriter {
    fn drop(&mut self) {
        self.file = None;
        if self.owns_path && self.path.exists() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Clear O_NONBLOCK on an fd opened nonblocking, so subsequent writes block
/// (apply back-pressure) instead of returning EAGAIN. Uses libc fcntl directly
/// to avoid nix's fcntl API drift across versions.
fn clear_nonblocking(file: &std::fs::File) -> anyhow::Result<()> {
    let fd = file.as_raw_fd();
    // SAFETY: fd is owned by `file` for the duration of these calls.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

pub fn default_fifo_path(label: &str) -> PathBuf {
    let pid = std::process::id();
    PathBuf::from(format!("/tmp/etv-overlay-{label}-{pid}.fifo"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::Arc;

    // Regression test for the overlay garble bug: a reader that disconnects
    // mid-frame must NOT leave the next reader misaligned. With the old O_RDWR
    // fifo the writer never saw the reader leave, so a reconnecting reader read
    // from a mid-frame byte offset and the overlay tiled/sheared. The
    // O_WRONLY + reopen() path must hand each new reader a fresh, frame-aligned
    // stream.
    #[test]
    fn reader_reconnect_gets_frame_aligned_stream() {
        let dir = tempfile::tempdir().unwrap();
        let fifo_path = dir.path().join("overlay.fifo");
        // Create the fifo up front so reader 1 can't race the writer thread's
        // creation; the writer attaches to it.
        mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

        // 64-byte "frames": byte i == i, so a correctly-aligned read begins at 0.
        const FRAME_LEN: usize = 64;
        let frame: Vec<u8> = (0..FRAME_LEN as u8).collect();

        let shutdown = Arc::new(AtomicBool::new(false));

        // Writer: create the fifo, then write the frame in a loop, reopening on
        // BrokenPipe (reader gone) so each new reader starts on a frame boundary
        // — exactly what the per-item ffmpeg reattach relies on.
        let writer_frame = frame.clone();
        let writer_path = fifo_path.clone();
        let writer_shutdown = Arc::clone(&shutdown);
        let writer = thread::spawn(move || {
            let mut w = FifoWriter::attach(writer_path);
            if w.open_for_writing(&writer_shutdown).unwrap() == OpenOutcome::Shutdown {
                return;
            }
            loop {
                if writer_shutdown.load(Ordering::SeqCst) {
                    return;
                }
                match w.write_frame(&writer_frame) {
                    Ok(()) => thread::sleep(Duration::from_millis(5)),
                    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
                        if w.reopen(&writer_shutdown).unwrap() == OpenOutcome::Shutdown {
                            return;
                        }
                    }
                    Err(e) => panic!("unexpected write error: {e}"),
                }
            }
        });

        // Reader 1: read one aligned frame, then a partial frame, then drop —
        // leaving the pipe mid-frame at disconnect, the case that misaligned the
        // next reader under O_RDWR.
        {
            let mut f = OpenOptions::new().read(true).open(&fifo_path).unwrap();
            let mut buf = vec![0u8; FRAME_LEN];
            f.read_exact(&mut buf).unwrap();
            assert_eq!(buf, frame, "reader 1 should get an aligned frame");
            let mut partial = vec![0u8; 17];
            f.read_exact(&mut partial).unwrap();
        }

        // Let the writer hit BrokenPipe and park in reopen() waiting for the next
        // reader — mirrors the gap between two playout items' ffmpegs.
        thread::sleep(Duration::from_millis(250));

        // Reader 2: a fresh reader must start on a frame boundary, not mid-frame.
        {
            let mut f = OpenOptions::new().read(true).open(&fifo_path).unwrap();
            let mut buf = vec![0u8; FRAME_LEN];
            f.read_exact(&mut buf).unwrap();
            assert_eq!(
                buf, frame,
                "reader 2 must get a frame-aligned stream after reconnect"
            );
        }

        // The writer polls the shutdown flag every OPEN_POLL_INTERVAL, so this
        // unblocks it whether it's writing or parked in reopen().
        shutdown.store(true, Ordering::SeqCst);
        writer.join().unwrap();
    }
}
