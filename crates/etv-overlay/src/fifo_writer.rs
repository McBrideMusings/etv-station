use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use nix::sys::stat::Mode;
use nix::unistd::mkfifo;

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

    pub fn open_for_writing(&mut self) -> anyhow::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        // Open write-only. O_WRONLY blocks until a reader (the channel's
        // ffmpeg) opens the fifo, then unblocks — exactly the rendezvous a
        // fifo is built for, so there is no deadlock with ffmpeg's own
        // blocking open-for-read. Crucially, a write-only fd means that when
        // the reader goes away — etv-next spawns a fresh ffmpeg per playout
        // item — the next write returns BrokenPipe, which lets us reopen() and
        // start the next reader's stream on a frame boundary. An O_RDWR fd
        // (the writer also holding a read end) never observes the reader
        // leaving, so every post-first-item ffmpeg would attach mid-frame and
        // render a torn, tiled overlay.
        let file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|e| anyhow::anyhow!("open fifo {}: {e}", self.path.display()))?;
        self.file = Some(file);
        Ok(())
    }

    /// Close the current write fd and reopen the fifo, blocking until the next
    /// reader attaches. Called after a BrokenPipe so the next consumer's
    /// stream begins on a fresh frame boundary.
    pub fn reopen(&mut self) -> anyhow::Result<()> {
        self.file = None;
        self.open_for_writing()
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

pub fn default_fifo_path(label: &str) -> PathBuf {
    let pid = std::process::id();
    PathBuf::from(format!("/tmp/etv-overlay-{label}-{pid}.fifo"))
}
