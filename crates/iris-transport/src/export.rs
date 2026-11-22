//! The export download: the MP4 export of a recorded window. The export is
//! streamed into an exclusive temporary beside its destination and renamed
//! into place once the whole file is synced. The free space of the
//! destination filesystem is checked as the file is written.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::mem;
use std::path::{Path, PathBuf};

use iris_client::api::{ApiError, Window, answer};
use iris_core::id::{CameraId, StreamKind};
use reqwest::Response;
use thiserror::Error;
use tokio::fs;
use tokio::io::AsyncWriteExt as _;
use tokio::runtime::Handle;
use tokio::task::JoinError;
use tracing::warn;

use crate::client::{Client, ClientError};

/// Exclusive temporary names an export tries before it gives up on a
/// directory in which every name it tries is taken.
const EXPORT_TEMPORARY_ATTEMPTS: usize = 16;
/// The least free space an export leaves on its filesystem; 2 % of the
/// filesystem applies when that is larger.
const EXPORT_RESERVE_MIN_BYTES: u64 = 1 << 30;
/// Bytes an export writes between free-space checks.
pub(super) const EXPORT_SPACE_CHECK_BYTES: u64 = 64 << 20;

/// Why an export stopped. Whatever the failure, the destination is left as
/// it was and the temporary is removed. A temporary that cannot be removed
/// is logged as "cannot remove an export temporary", with its path and the
/// error. Each text is the message the Linux viewer shows, and a failure the
/// export met keeps it as its source. The example of [`Client::export`]
/// returns one through `?`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ExportError {
    /// The request or the head of its answer did not travel
    /// ([`ClientError::Transport`]), or `iris-client` refused the answer
    /// ([`ClientError::Api`]). The text and the source are those of the
    /// [`ClientError`].
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The destination filesystem's free space is below the export's reserve,
    /// the larger of 1 GiB and 2 % of the filesystem. The text is "export
    /// stopped: {} MiB free on the destination filesystem is below its {}
    /// MiB reserve".
    #[error(
        "export stopped: {} MiB free on the destination filesystem is below its {} MiB reserve",
        .available_bytes >> 20,
        .reserve_bytes >> 20
    )]
    LowSpace {
        /// The bytes an unprivileged writer may still use.
        available_bytes: u64,
        /// The bytes the export leaves free.
        reserve_bytes: u64,
    },
    /// The destination ends in no file name, as `/` and a path ending in `..`
    /// do, so no temporary can be named beside it.
    #[error("export destination has no file name")]
    NoFileName,
    /// The system gave no random bytes for the temporary's name.
    #[error("creating an export temporary name")]
    TemporaryName(#[source] getrandom::Error),
    /// The task that creates the temporary ended without its answer.
    #[error("creating an export temporary")]
    TemporaryTask(#[source] JoinError),
    /// The temporary could not be created beside `destination`.
    #[error("creating export beside {}", .destination.display())]
    Create {
        /// The path the export was to write, beside which the temporary was
        /// to be created.
        destination: PathBuf,
        /// Why the temporary was not created.
        source: io::Error,
    },
    /// Every name tried for the temporary was taken.
    #[error("cannot allocate a unique export temporary file")]
    NoUniqueName,
    /// The temporary's descriptor could not be duplicated for the free-space
    /// probe.
    #[error("duplicating the export descriptor")]
    Duplicate(#[source] io::Error),
    /// The task that probes the free space ended without its answer.
    #[error("running the free-space probe")]
    ProbeTask(#[source] JoinError),
    /// The destination filesystem's free space could not be read.
    #[error("reading the export filesystem's free space")]
    FreeSpace(#[source] io::Error),
    /// The export's body failed to arrive.
    #[error("reading the export")]
    Read(#[source] reqwest::Error),
    /// The temporary could not be written.
    #[error("writing the export")]
    Write(#[source] io::Error),
    /// The count of the bytes written passed `u64::MAX`.
    #[error("export byte count overflows")]
    Overflow,
    /// The answer held no byte.
    #[error("export response is empty")]
    Empty,
    /// The temporary's buffered bytes could not be written.
    #[error("flushing the export")]
    Flush(#[source] io::Error),
    /// The temporary could not be synced to its filesystem.
    #[error("syncing the export")]
    Sync(#[source] io::Error),
    /// The temporary could not be renamed to `destination`.
    #[error("committing export to {}", .destination.display())]
    Commit {
        /// The path the temporary was to be renamed to.
        destination: PathBuf,
        /// Why the rename failed.
        source: io::Error,
    },
}

impl From<ApiError> for ExportError {
    /// The refusal of `iris-client` as the error of an export, with the text
    /// and the source of the refusal.
    fn from(refusal: ApiError) -> Self {
        Self::Client(ClientError::Api(refusal))
    }
}

/// Space on the filesystem an export writes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FilesystemSpace {
    /// Bytes an unprivileged writer may still use.
    available: u64,
    /// The filesystem's size in bytes.
    total: u64,
}

/// Reads the space of the filesystem holding an open file; it may block.
pub(super) type SpaceProbe = fn(&File) -> io::Result<FilesystemSpace>;

/// `fstatvfs` on a duplicate of the temporary's descriptor, so the figure is
/// for the filesystem the bytes land on.
pub(super) fn filesystem_space(file: &File) -> io::Result<FilesystemSpace> {
    let stat = rustix::fs::fstatvfs(file)?;
    Ok(FilesystemSpace {
        available: stat.f_bavail.saturating_mul(stat.f_frsize),
        total: stat.f_blocks.saturating_mul(stat.f_frsize),
    })
}

/// The free space an export leaves: the larger of 1 GiB and 2 % of the
/// filesystem.
fn export_reserve(total: u64) -> u64 {
    EXPORT_RESERVE_MIN_BYTES.max(total / 50)
}

fn check_export_space(space: FilesystemSpace) -> Result<(), ExportError> {
    let reserve = export_reserve(space.total);
    if space.available >= reserve {
        Ok(())
    } else {
        Err(ExportError::LowSpace {
            available_bytes: space.available,
            reserve_bytes: reserve,
        })
    }
}

impl Client {
    /// The destination filesystem's space, read on the blocking pool: a stalled
    /// mount then holds a blocking thread instead of a runtime worker the live
    /// feeds share.
    async fn destination_space(&self, file: &fs::File) -> Result<FilesystemSpace, ExportError> {
        let probe = self.space_probe;
        let file = file
            .try_clone()
            .await
            .map_err(ExportError::Duplicate)?
            .into_std()
            .await;
        tokio::task::spawn_blocking(move || probe(&file))
            .await
            .map_err(ExportError::ProbeTask)?
            .map_err(ExportError::FreeSpace)
    }

    /// Downloads the MP4 export of `window` to `path`, calling `progress`
    /// with the bytes written so far after every chunk; returns the total.
    /// The export of a main stream holds the camera's recorded audio too.
    /// Before the first byte and after every further 64 MiB it reads the
    /// destination filesystem's free space with `fstatvfs`. It stops, and
    /// removes the partial file, when the free space is below the larger of
    /// 1 GiB and 2 % of the filesystem. The file appears at `path` only
    /// whole: the export writes an exclusive temporary beside it, syncs it,
    /// and renames it over `path`, replacing a file or a symbolic link there
    /// and writing through neither.
    ///
    /// A cancelled export leaves no temporary: its drop hands the removal to
    /// the blocking pool. Cancelled before the rename, it leaves the
    /// destination as it was. The rename runs on the blocking pool and goes
    /// on when the export is cancelled during it. The destination then holds
    /// either what it held before or the whole export, never a part of one.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime with its I/O and time drivers enabled, as
    /// every call of the [`Client`] does. The request needs the I/O driver,
    /// and its deadlines need the timer. The file work runs on the runtime's
    /// blocking pool.
    ///
    /// # Errors
    ///
    /// An [`ExportError`], the destination left as it was:
    ///
    /// - [`ExportError::Client`] for an answer refused, or a request whose
    ///   answer's head did not travel ([`ClientError::Transport`]);
    /// - [`ExportError::NoFileName`] for a `path` that names no file, and
    ///   [`ExportError::TemporaryName`], [`ExportError::TemporaryTask`],
    ///   [`ExportError::Create`], or [`ExportError::NoUniqueName`] when the
    ///   temporary cannot be created beside it;
    /// - [`ExportError::Duplicate`], [`ExportError::ProbeTask`], or
    ///   [`ExportError::FreeSpace`] when the free space cannot be read, and
    ///   [`ExportError::LowSpace`] when it runs low;
    /// - [`ExportError::Read`] when the body fails to arrive,
    ///   [`ExportError::Write`] when the temporary cannot take it,
    ///   [`ExportError::Overflow`] when its count of bytes passes `u64::MAX`,
    ///   and [`ExportError::Empty`] for an answer without a byte;
    /// - [`ExportError::Flush`], [`ExportError::Sync`], or
    ///   [`ExportError::Commit`] when the temporary cannot be finished and
    ///   renamed over `path`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::error::Error;
    /// use std::env;
    /// use std::path::Path;
    /// use std::sync::Arc;
    ///
    /// use iris_client::api::Window;
    /// use iris_client::origin::{ApiOrigin, PlainHttp};
    /// use iris_client::playback;
    /// use iris_core::id::{CameraId, StreamKind};
    /// use iris_core::time::WallUs;
    /// use iris_transport::{Bearer, Client};
    ///
    /// # async fn export() -> Result<(), Box<dyn Error>> {
    /// let api = "https://nvr.example:8443";
    /// let origin = ApiOrigin::parse(api, PlainHttp::LoopbackOnly)?;
    /// let tls = Arc::unwrap_or_clone(iris_tls::client_config(None)?);
    /// // The daemon trims a presented token and treats an empty one as none.
    /// let variable = env::var("IRISD_TOKEN").unwrap_or_default();
    /// let token = Some(variable.trim()).filter(|token| !token.is_empty());
    /// let bearer = token.map(Bearer::try_new).transpose()?;
    /// let client = Client::new(origin, bearer, tls);
    /// // The first minute of 2026-11-01 UTC of the main stream, with its
    /// // sound.
    /// let from = WallUs::new(1_793_491_200_000_000);
    /// let to = WallUs::new(1_793_491_260_000_000);
    /// let window = Window::try_new(from, to)?;
    /// let camera = CameraId::try_new("cam1")?;
    /// let name = playback::export_name(&camera, StreamKind::Main, from);
    /// let path = Path::new(&name);
    /// let report = |written| eprint!("\r{written} bytes");
    /// let bytes = client
    ///     .export(&camera, StreamKind::Main, window, path, report)
    ///     .await?;
    /// println!("{bytes} bytes in {name}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn export(
        &self,
        camera: &CameraId,
        kind: StreamKind,
        window: Window,
        path: &Path,
        progress: impl FnMut(u64) + Send,
    ) -> Result<u64, ExportError> {
        let request = self.origin().export(camera, kind, window);
        let received = self.fetch(&request).await?;
        answer::export(self.origin(), &received.answer())?;
        let mut temporary = ExportTemporary::create(path).await?;
        // `HttpRequest::body_to_read` leaves an export's body unread, and
        // the file receives it here, once the head has passed its check.
        let written = match self.fill(&mut temporary, received.response, progress).await {
            Ok(written) => written,
            Err(error) => {
                temporary.discard().await;
                return Err(error);
            }
        };
        temporary.commit(path).await?;
        Ok(written)
    }

    /// Writes the body of `response` into `temporary` and gives the bytes
    /// written. It calls `progress` with the bytes written so far after
    /// every chunk. It reads the destination's free space before the first
    /// byte and once the bytes written reach each further check interval.
    async fn fill(
        &self,
        temporary: &mut ExportTemporary,
        response: Response,
        mut progress: impl FnMut(u64) + Send,
    ) -> Result<u64, ExportError> {
        use futures::StreamExt as _;

        check_export_space(self.destination_space(temporary.file()).await?)?;
        let mut next_space_check = self.space_check_bytes;
        let mut stream = response.bytes_stream();
        let mut written = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(ExportError::Read)?;
            temporary
                .file()
                .write_all(&chunk)
                .await
                .map_err(ExportError::Write)?;
            written = written
                .checked_add(chunk.len() as u64)
                .ok_or(ExportError::Overflow)?;
            if written >= next_space_check {
                check_export_space(self.destination_space(temporary.file()).await?)?;
                next_space_check = written.saturating_add(self.space_check_bytes);
            }
            progress(written);
        }
        if written == 0 {
            return Err(ExportError::Empty);
        }
        Ok(written)
    }
}

/// Fills the 16 bytes a temporary's name is written from: the system's
/// random bytes, or in a test the bytes the test chooses.
type NameBytes = fn(&mut [u8]) -> Result<(), getrandom::Error>;

/// An export's exclusive sibling temporary. [`ExportTemporary::commit`]
/// renames it into place, and [`ExportTemporary::discard`] removes it, which
/// an export that stops awaits; neither blocks a runtime worker on the
/// filesystem. One dropped without either, as a cancelled export drops it,
/// is removed by a [`Removal`] on the blocking pool.
struct ExportTemporary {
    /// Where the temporary is, beside the destination.
    path: PathBuf,
    /// The open file, until the rename places it or a removal takes it:
    /// `None` then, so that the drop removes nothing.
    file: Option<fs::File>,
}

impl ExportTemporary {
    async fn create(destination: &Path) -> Result<Self, ExportError> {
        Self::create_named(destination, getrandom::fill).await
    }

    /// Creates the temporary as [`ExportTemporary::create`] does, writing
    /// each name it tries from `name_bytes`: a name taken is tried again with
    /// the next, and any other failure to create the file ends the export.
    async fn create_named(destination: &Path, name_bytes: NameBytes) -> Result<Self, ExportError> {
        use std::fmt::Write as _;

        if destination.file_name().is_none() {
            return Err(ExportError::NoFileName);
        }
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        for _ in 0..EXPORT_TEMPORARY_ATTEMPTS {
            let mut random = [0u8; 16];
            name_bytes(&mut random).map_err(ExportError::TemporaryName)?;
            let prefix = ".irisd-export-";
            let mut name = String::with_capacity(prefix.len() + 2 * random.len());
            name.push_str(prefix);
            for byte in random {
                // Writing to a `String` cannot fail.
                let _ = write!(name, "{byte:02x}");
            }
            let path = parent.join(name).with_extension("tmp");
            // The blocking open returns the guard itself: when the export is
            // cancelled while the open runs, the runtime drops the guard and
            // with it the file the open created.
            let opened = tokio::task::spawn_blocking(move || {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)
                    .map(|file| Self {
                        path,
                        file: Some(fs::File::from_std(file)),
                    })
            })
            .await
            .map_err(ExportError::TemporaryTask)?;
            match opened {
                Ok(temporary) => return Ok(temporary),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(ExportError::Create {
                        destination: destination.to_path_buf(),
                        source,
                    });
                }
            }
        }
        Err(ExportError::NoUniqueName)
    }

    /// The open file, which the export writes.
    ///
    /// # Panics
    ///
    /// Never while the temporary lives: it holds its file until the commit or
    /// the discard takes it, and both consume the temporary.
    fn file(&mut self) -> &mut fs::File {
        self.file
            .as_mut()
            .expect("a temporary holds its file until its commit or its discard consumes it")
    }

    /// Flushes and syncs the temporary, renames it over `destination`, and
    /// closes it on the blocking pool; when the flush, the sync, or the
    /// rename fails, the temporary is removed before the error returns.
    ///
    /// The rename runs on the blocking pool, and a cancelled export does not
    /// stop it. A temporary dropped during the rename hands its removal to
    /// the pool too. The rename then moves the whole file into place, or
    /// finds the file removed and fails. Either way the destination holds
    /// the whole export or what it held before, never a part of the export.
    async fn commit(mut self, destination: &Path) -> Result<(), ExportError> {
        if let Err(error) = self.place(destination).await {
            self.discard().await;
            return Err(error);
        }
        // The file is in place, so nothing is left to remove, and it closes
        // on the blocking pool, as a removal closes it.
        let file = self.file.take();
        let _ = tokio::task::spawn_blocking(move || drop(file)).await;
        Ok(())
    }

    /// The steps of [`ExportTemporary::commit`] that can fail, with the file
    /// still held, so that a temporary cancelled during them is removed.
    async fn place(&mut self, destination: &Path) -> Result<(), ExportError> {
        let file = self.file();
        file.flush().await.map_err(ExportError::Flush)?;
        file.sync_all().await.map_err(ExportError::Sync)?;
        fs::rename(&self.path, destination)
            .await
            .map_err(|source| ExportError::Commit {
                destination: destination.to_path_buf(),
                source,
            })
    }

    /// Closes and removes the temporary on the blocking pool and waits for
    /// the removal, so an export that stops returns with its temporary gone.
    /// No runtime worker, which the live feeds share, waits on the
    /// filesystem, as one would on a stalled mount.
    async fn discard(mut self) {
        if let Some(removal) = self.removal() {
            // A pool that refuses the task, as one shutting down does, drops
            // the removal unrun, which then removes the file where it is
            // dropped; the task's result says nothing more.
            let _ = tokio::task::spawn_blocking(move || drop(removal)).await;
        }
    }

    /// The removal of the temporary, while it holds its file.
    fn removal(&mut self) -> Option<Removal> {
        self.file.take().map(|file| Removal {
            path: mem::take(&mut self.path),
            file: Some(file),
        })
    }
}

impl Drop for ExportTemporary {
    /// Removes a temporary dropped before its commit or its discard, as a
    /// cancelled export drops it. When a runtime runs, the removal goes to
    /// the blocking pool, so the runtime worker that cancelled the export
    /// goes on. Otherwise the file is removed at once.
    fn drop(&mut self) {
        if let Some(removal) = self.removal() {
            match Handle::try_current() {
                Ok(runtime) => drop(runtime.spawn_blocking(move || drop(removal))),
                Err(_) => drop(removal),
            }
        }
    }
}

/// A temporary to remove: its file closes and its path is removed when the
/// removal is dropped, once, wherever that is. Handed to the blocking pool,
/// it is dropped there as its task runs, or where the pool drops the task
/// unrun.
struct Removal {
    path: PathBuf,
    file: Option<fs::File>,
}

impl Drop for Removal {
    /// Closes the file, then removes the path, logging a removal that fails
    /// for any reason but the file being gone.
    fn drop(&mut self) {
        drop(self.file.take());
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != ErrorKind::NotFound
        {
            warn!(path = %self.path.display(), %error, "cannot remove an export temporary");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error as _;
    use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Sender};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, ThreadId};
    use std::time::{Duration, Instant};

    use async_channel::Receiver;
    use axum::body::{Body, Bytes};
    use axum::extract::Query;
    use axum::http::{StatusCode, header};
    use axum::response::Response;
    use axum::{Router, routing};
    use futures::stream;
    use iris_client::api::Rule;
    use iris_client::origin::{ApiOrigin, PlainHttp};
    use tokio::runtime::{Builder, Runtime};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::fixture;

    /// What a destination holds before an export, so that a test sees
    /// whether the export changed it.
    const SENTINEL: &[u8] = b"recording before the export";

    /// Eight chunks of 1 KiB of `byte`.
    fn chunked_export_body(byte: u8) -> Body {
        Body::from_stream(stream::iter(
            (0..8).map(move |_| Ok::<Bytes, io::Error>(Bytes::from(vec![byte; 1024]))),
        ))
    }

    /// The export fixture; the `from` parameter picks the answer.
    async fn export_fixture(Query(query): Query<HashMap<String, String>>) -> Response {
        use futures::StreamExt as _;

        let from: i64 = query["from"].parse().unwrap();
        match from {
            1 => fixture::mp4_response(Body::from_stream(stream::iter([
                Ok(Bytes::from_static(b"partial")),
                Err(io::Error::other("fixture body failure")),
            ]))),
            2 => fixture::mp4_response(Body::empty()),
            3 => Response::builder()
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("not an mp4"))
                .unwrap(),
            4 => fixture::mp4_response(Body::from("complete mp4")),
            5 => Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, "video/mp4")
                .header(header::CONTENT_RANGE, "bytes 0-6/12")
                .body(Body::from("partial"))
                .unwrap(),
            10 => fixture::mp4_response(chunked_export_body(b'A')),
            20 => fixture::mp4_response(chunked_export_body(b'B')),
            30 => {
                let body =
                    stream::once(async { Ok::<Bytes, io::Error>(Bytes::from_static(b"partial")) })
                        .chain(stream::pending::<Result<Bytes, io::Error>>());
                fixture::mp4_response(Body::from_stream(body))
            }
            // Chunks that bring the bytes written to one short of 2 KiB and then
            // to it, twice.
            40 => fixture::mp4_response(Body::from_stream(stream::iter(
                [2047, 1, 2047, 1]
                    .map(|length| Ok::<Bytes, io::Error>(Bytes::from(vec![b'C'; length]))),
            ))),
            _ => unreachable!(),
        }
    }

    async fn export_server() -> (ApiOrigin, JoinHandle<()>) {
        fixture::serve(
            Router::new()
                .route(
                    "/api/streams/{camera}/{kind}/export.mp4",
                    routing::get(export_fixture),
                )
                .route(
                    "/api/cameras/{camera}/export.mp4",
                    routing::get(export_fixture),
                ),
        )
        .await
    }

    /// Exports the fixture answer that `from` picks to `destination`.
    async fn export_case(
        client: &Client,
        from: i64,
        destination: &Path,
    ) -> Result<u64, ExportError> {
        client
            .export(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(from, from + 1),
                destination,
                |_| {},
            )
            .await
    }

    fn export_temporaries(directory: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".irisd-export-"))
            })
            .collect()
    }

    /// The `from` of the export whose body is cut short once the temporary
    /// has taken some of it: [`stopping_client`] gives it a server that cuts
    /// every answer short.
    const CUT_SHORT: i64 = 6;

    /// The answers that stop an export, by `from`: a body failure, an empty
    /// body, a body that is not MP4, partial content, and a body cut short
    /// once the temporary has taken some of it.
    const FAILING_CASES: [i64; 5] = [1, 2, 3, 5, CUT_SHORT];

    /// A client of `base` whose probe finds plenty of free space, so that no
    /// stop is one of low space whatever the test's filesystem holds.
    fn spacious_client(base: ApiOrigin) -> Client {
        let mut client = fixture::client(base);
        client.space_probe = |_| Ok(PLENTY_OF_SPACE);
        client
    }

    /// A client, with plenty of free space, of the server that stops the
    /// export `from` picks. For [`CUT_SHORT`] the server answers every
    /// export with `partial` and then ends the connection before the body's
    /// end; for any other `from` it is the export fixture.
    async fn stopping_client(from: i64) -> (Client, JoinHandle<()>) {
        let (base, server) = if from == CUT_SHORT {
            fixture::serve_cut_short(StatusCode::OK, "video/mp4", b"partial").await
        } else {
            export_server().await
        };
        (spacious_client(base), server)
    }

    #[tokio::test]
    async fn failed_exports_leave_the_destination_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        for from in FAILING_CASES {
            let (client, server) = stopping_client(from).await;
            std::fs::write(&destination, SENTINEL).unwrap();
            export_case(&client, from, &destination)
                .await
                .expect_err("the answer stops the export");
            assert_eq!(
                std::fs::read(&destination).unwrap(),
                SENTINEL,
                "from {from}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn failed_exports_leave_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        for from in FAILING_CASES {
            let (client, server) = stopping_client(from).await;
            export_case(&client, from, &destination)
                .await
                .expect_err("the answer stops the export");
            assert_eq!(
                export_temporaries(directory.path()),
                Vec::<PathBuf>::new(),
                "from {from}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn a_complete_export_replaces_the_destination() {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(&destination, SENTINEL).unwrap();
        export_case(&spacious_client(base), 4, &destination)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete mp4");
        server.abort();
    }

    #[tokio::test]
    async fn a_complete_export_gives_the_bytes_written() {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let written = export_case(&spacious_client(base), 4, &destination)
            .await
            .unwrap();
        assert_eq!(written, 12);
        server.abort();
    }

    #[tokio::test]
    async fn a_complete_export_leaves_no_temporary() {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        export_case(&spacious_client(base), 4, &destination)
            .await
            .unwrap();
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
        server.abort();
    }

    /// Exports the complete answer to a symbolic link in `directory` that
    /// points at a file of the sentinel; gives the link and the file.
    #[cfg(unix)]
    async fn export_to_a_symlink(directory: &Path) -> (PathBuf, PathBuf) {
        let (base, server) = export_server().await;
        let destination = directory.join("recording.mp4");
        let linked = directory.join("linked.mp4");
        std::fs::write(&linked, SENTINEL).unwrap();
        std::os::unix::fs::symlink(&linked, &destination).unwrap();
        export_case(&spacious_client(base), 4, &destination)
            .await
            .unwrap();
        server.abort();
        (destination, linked)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_export_does_not_write_through_a_symlink() {
        let directory = tempfile::tempdir().unwrap();
        let (_destination, linked) = export_to_a_symlink(directory.path()).await;
        assert_eq!(std::fs::read(&linked).unwrap(), SENTINEL);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_export_replaces_a_symlink_with_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let (destination, _linked) = export_to_a_symlink(directory.path()).await;
        let replaced = std::fs::symlink_metadata(&destination).unwrap();
        assert!(replaced.file_type().is_file(), "{replaced:?}");
        assert_eq!(std::fs::read(&destination).unwrap(), b"complete mp4");
    }

    /// Runs two exports to one destination in `directory` at once, of eight
    /// chunks of `A` and of `B`.
    async fn concurrent_exports(directory: &Path) -> PathBuf {
        let (base, server) = export_server().await;
        let client = spacious_client(base);
        let destination = directory.join("recording.mp4");
        let (first, second) = tokio::join!(
            export_case(&client, 10, &destination),
            export_case(&client, 20, &destination)
        );
        first.unwrap();
        second.unwrap();
        server.abort();
        destination
    }

    #[tokio::test]
    async fn concurrent_exports_leave_one_complete_file() {
        let directory = tempfile::tempdir().unwrap();
        let destination = concurrent_exports(directory.path()).await;
        let result = std::fs::read(&destination).unwrap();
        assert!(
            result == vec![b'A'; 8 * 1024] || result == vec![b'B'; 8 * 1024],
            "the destination holds one export whole"
        );
    }

    #[tokio::test]
    async fn concurrent_exports_leave_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        concurrent_exports(directory.path()).await;
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
    }

    /// Writes the sentinel to `destination`, starts an export of an answer
    /// that never ends to it, and cancels the export once its temporary is
    /// in `directory`.
    async fn cancel_an_export(directory: &Path, destination: &Path) {
        let (base, server) = export_server().await;
        let client = spacious_client(base);
        std::fs::write(destination, SENTINEL).unwrap();
        let task = tokio::spawn({
            let destination = destination.to_path_buf();
            async move { export_case(&client, 30, &destination).await }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while export_temporaries(directory).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the export creates its temporary");
        task.abort();
        let _ = task.await;
        server.abort();
    }

    #[tokio::test]
    async fn a_cancelled_export_leaves_the_destination_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        cancel_an_export(directory.path(), &destination).await;
        assert_eq!(std::fs::read(&destination).unwrap(), SENTINEL);
    }

    #[tokio::test]
    async fn a_cancelled_export_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        cancel_an_export(directory.path(), &destination).await;
        wait_for_no_temporaries(directory.path()).await;
    }

    /// Waits until no export temporary is left in `directory`: the blocking
    /// pool removes the temporary of a cancelled export after the
    /// cancellation returns.
    async fn wait_for_no_temporaries(directory: &Path) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !export_temporaries(directory).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "temporaries left behind: {:?}",
                export_temporaries(directory)
            )
        });
    }

    /// An export cancelled after its temporary was opened on the blocking
    /// thread and before the export saw the result leaves no temporary. The
    /// runtime has one thread and the test does not yield while it waits for
    /// the file, so the export cannot run in between.
    #[tokio::test]
    async fn a_temporary_cancelled_while_it_is_created_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        for _ in 0..20 {
            let task = tokio::spawn({
                let destination = destination.clone();
                async move { ExportTemporary::create(&destination).await }
            });
            // One turn of the runtime starts the open.
            tokio::task::yield_now().await;
            let deadline = Instant::now() + Duration::from_secs(5);
            while export_temporaries(directory.path()).is_empty() {
                assert!(Instant::now() < deadline, "no temporary appeared");
                thread::sleep(Duration::from_millis(1));
            }
            task.abort();
            let _ = task.await;
            wait_for_no_temporaries(directory.path()).await;
        }
    }

    #[test]
    fn the_export_reserve_is_one_gib_on_a_small_filesystem() {
        assert_eq!(export_reserve(10 << 30), 1 << 30);
    }

    #[test]
    fn the_export_reserve_is_two_percent_of_a_large_filesystem() {
        assert_eq!(export_reserve(1 << 40), (1 << 40) / 50);
    }

    /// 2 GiB free of 100 GiB is the reserve itself, and lets an export go
    /// on.
    #[test]
    fn free_space_at_the_reserve_lets_an_export_go_on() {
        let space = FilesystemSpace {
            available: 2 << 30,
            total: 100 << 30,
        };
        assert!(check_export_space(space).is_ok(), "{space:?}");
    }

    #[test]
    fn free_space_below_the_reserve_stops_an_export() {
        let space = FilesystemSpace {
            available: (2 << 30) - 1,
            total: 100 << 30,
        };
        assert!(
            matches!(check_export_space(space), Err(ExportError::LowSpace { .. })),
            "{space:?}"
        );
    }

    #[test]
    fn fstatvfs_reports_a_consistent_size() {
        let space = filesystem_space(&tempfile::tempfile().unwrap()).unwrap();
        assert!(
            space.total > 0 && space.available <= space.total,
            "{space:?}"
        );
    }

    /// `fstatvfs` on a file reports the size that `statvfs` of the file's
    /// directory reports: the figure is that of the filesystem holding the
    /// file.
    #[test]
    fn fstatvfs_reports_the_size_of_the_filesystem_holding_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = File::create(directory.path().join("probed")).unwrap();
        let space = filesystem_space(&file).unwrap();
        let holding = rustix::fs::statvfs(directory.path()).unwrap();
        assert_eq!(
            space.total,
            holding.f_blocks.saturating_mul(holding.f_frsize),
            "{space:?}"
        );
    }

    /// Calls of the plenty-then-low probe in
    /// [`space_running_low_stops_an_export_at_the_next_check`].
    static SPACE_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// Calls of the plenty-then-low probe in
    /// [`space_running_low_stops_an_export_for_low_space`].
    static STOPPING_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// 512 MiB free of 100 GiB: below the 2 GiB reserve.
    const LOW_SPACE: FilesystemSpace = FilesystemSpace {
        available: 512 << 20,
        total: 100 << 30,
    };

    /// 50 GiB free of 100 GiB.
    const PLENTY_OF_SPACE: FilesystemSpace = FilesystemSpace {
        available: 50 << 30,
        total: 100 << 30,
    };

    /// Exports eight chunks of 1 KiB to a filesystem with too little space
    /// from the start. Gives the result of the export and the last count of
    /// bytes written that the progress callback heard, zero when it heard
    /// none.
    async fn export_with_too_little_space() -> (Result<u64, ExportError>, u64) {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let mut client = fixture::client(base);
        client.space_probe = |_| Ok(LOW_SPACE);
        let written = Arc::new(AtomicU64::new(0));
        let observed = Arc::clone(&written);
        let result = client
            .export(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(10, 11),
                &destination,
                move |bytes| observed.store(bytes, Ordering::SeqCst),
            )
            .await;
        server.abort();
        (result, written.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn too_little_space_before_the_first_byte_stops_the_export_for_low_space() {
        let (result, _heard) = export_with_too_little_space().await;
        let error = result.expect_err("too little space stops the export");
        assert!(matches!(error, ExportError::LowSpace { .. }), "{error:?}");
    }

    /// Too little space from the start stops an export before the progress
    /// callback hears a byte written. The test
    /// [`the_free_space_is_read_before_the_first_byte_is_written`] checks
    /// that the first read of the free space comes before the first byte.
    #[tokio::test]
    async fn too_little_space_before_the_first_byte_reports_no_progress() {
        let (_result, heard) = export_with_too_little_space().await;
        assert_eq!(heard, 0, "the progress callback heard a byte written");
    }

    /// An export of eight chunks of 1 KiB under a check interval of 2 KiB,
    /// to a filesystem whose space runs low after the first check. `probe`
    /// reads the free space.
    async fn export_as_space_runs_low(probe: SpaceProbe) -> Result<u64, ExportError> {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let mut client = fixture::client(base);
        client.space_probe = probe;
        client.space_check_bytes = 2048;
        let result = export_case(&client, 10, &destination).await;
        server.abort();
        result
    }

    #[tokio::test]
    async fn space_running_low_stops_an_export_for_low_space() {
        let error = export_as_space_runs_low(|_| Ok(running_low(&STOPPING_PROBES)))
            .await
            .expect_err("space running low stops the export");
        assert!(matches!(error, ExportError::LowSpace { .. }), "{error:?}");
    }

    /// Space that runs low after the first check stops the export at the
    /// second: 2 KiB into eight chunks of 1 KiB, under an interval of 2 KiB.
    /// The probe is read twice and no more.
    #[tokio::test]
    async fn space_running_low_stops_an_export_at_the_next_check() {
        let _ = export_as_space_runs_low(|_| Ok(running_low(&SPACE_PROBES))).await;
        assert_eq!(SPACE_PROBES.load(Ordering::SeqCst), 2);
    }

    /// Stops an export of eight chunks of 1 KiB to `destination` for low
    /// space, with `probe` reading the free space under a check interval of
    /// 2 KiB.
    async fn low_space_stop(probe: SpaceProbe, destination: &Path) {
        let (base, server) = export_server().await;
        let mut client = fixture::client(base);
        client.space_probe = probe;
        client.space_check_bytes = 2048;
        let error = export_case(&client, 10, destination)
            .await
            .expect_err("low space stops the export");
        assert!(matches!(error, ExportError::LowSpace { .. }), "{error:?}");
        server.abort();
    }

    /// What a probe finds on a filesystem whose space runs low: plenty of
    /// space at its first call, when `calls` is zero, and too little at
    /// every later call. Each call adds one to `calls`.
    fn running_low(calls: &AtomicUsize) -> FilesystemSpace {
        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
            PLENTY_OF_SPACE
        } else {
            LOW_SPACE
        }
    }

    #[tokio::test]
    async fn too_little_space_before_the_first_byte_leaves_the_destination_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(&destination, SENTINEL).unwrap();
        low_space_stop(|_| Ok(LOW_SPACE), &destination).await;
        assert_eq!(std::fs::read(&destination).unwrap(), SENTINEL);
    }

    #[tokio::test]
    async fn too_little_space_before_the_first_byte_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        low_space_stop(|_| Ok(LOW_SPACE), &destination).await;
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
    }

    /// Calls of the probe in
    /// [`space_running_low_leaves_the_destination_as_it_was`].
    static KEPT_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// An export stopped at its second check, with 2 KiB in its temporary,
    /// leaves the destination as it was.
    #[tokio::test]
    async fn space_running_low_leaves_the_destination_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(&destination, SENTINEL).unwrap();
        low_space_stop(|_| Ok(running_low(&KEPT_PROBES)), &destination).await;
        assert_eq!(std::fs::read(&destination).unwrap(), SENTINEL);
    }

    /// Calls of the probe in [`space_running_low_leaves_no_temporary`].
    static REMOVED_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// An export stopped at its second check, with 2 KiB in its temporary,
    /// leaves no temporary.
    #[tokio::test]
    async fn space_running_low_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        low_space_stop(|_| Ok(running_low(&REMOVED_PROBES)), &destination).await;
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
    }

    /// The head of partial content is refused before the export creates a
    /// temporary: with the destination in a directory that does not exist,
    /// the refusal is the answer's, not the temporary's.
    #[tokio::test]
    async fn an_export_refuses_partial_content_before_creating_a_temporary() {
        let (base, server) = export_server().await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("missing").join("recording.mp4");
        let error = export_case(&spacious_client(base), 5, &destination)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ExportError::Client(ClientError::Api(_))),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            "requesting the export failed with 206 Partial Content"
        );
        server.abort();
    }

    /// Fails a write to a temporary beside `destination`, as a full disk
    /// fails one, and discards the temporary, as an export whose write
    /// failed does; gives where the temporary was.
    #[cfg(target_os = "linux")]
    async fn local_write_failure(destination: &Path) -> PathBuf {
        let mut temporary = ExportTemporary::create(destination).await.unwrap();
        let temporary_path = temporary.path.clone();
        temporary.file = Some(
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .await
                .unwrap(),
        );
        let file = temporary.file();
        let failed = match file.write_all(b"fails").await {
            Ok(()) => file.flush().await.is_err(),
            Err(_) => true,
        };
        assert!(failed, "/dev/full must fail the temporary write or flush");
        temporary.discard().await;
        temporary_path
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_temporary_is_removed_after_a_local_write_failure() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let temporary_path = local_write_failure(&destination).await;
        assert!(!temporary_path.exists(), "{temporary_path:?}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_local_write_failure_leaves_the_destination_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(&destination, SENTINEL).unwrap();
        local_write_failure(&destination).await;
        assert_eq!(std::fs::read(&destination).unwrap(), SENTINEL);
    }

    /// Exports the complete answer to a destination in `directory` that is
    /// itself a directory, which no rename replaces with a file, so the
    /// commit fails. The destination holds one file of the sentinel. Gives
    /// the destination and the export's error.
    async fn failed_commit(directory: &Path) -> (PathBuf, ExportError) {
        let (base, server) = export_server().await;
        let destination = directory.join("recording.mp4");
        std::fs::create_dir(&destination).unwrap();
        std::fs::write(destination.join("kept"), SENTINEL).unwrap();
        let error = export_case(&spacious_client(base), 4, &destination)
            .await
            .expect_err("a file is not renamed over a directory");
        server.abort();
        (destination, error)
    }

    /// An export whose rename fails stops as a failed commit, which names
    /// the destination in its text.
    #[tokio::test]
    async fn a_failed_commit_is_reported_with_its_destination() {
        let directory = tempfile::tempdir().unwrap();
        let (destination, error) = failed_commit(directory.path()).await;
        let named = match &error {
            ExportError::Commit { destination, .. } => Some(destination),
            _ => None,
        };
        assert_eq!(named, Some(&destination), "{error:?}");
        assert_eq!(
            error.to_string(),
            format!("committing export to {}", destination.display())
        );
    }

    #[tokio::test]
    async fn a_failed_commit_leaves_the_destination_as_it_was() {
        let directory = tempfile::tempdir().unwrap();
        let (destination, _error) = failed_commit(directory.path()).await;
        assert_eq!(std::fs::read(destination.join("kept")).unwrap(), SENTINEL);
    }

    /// An export whose commit fails returns with its temporary gone.
    #[tokio::test]
    async fn a_failed_commit_leaves_no_temporary() {
        let directory = tempfile::tempdir().unwrap();
        failed_commit(directory.path()).await;
        assert_eq!(export_temporaries(directory.path()), Vec::<PathBuf>::new());
    }

    /// A temporary at `path` whose commit fails at the sync: its file is the
    /// writing end of a pipe, which no filesystem syncs.
    #[cfg(unix)]
    fn unsyncable_temporary(path: PathBuf) -> ExportTemporary {
        use std::os::fd::OwnedFd;

        let (_reader, writer) = io::pipe().unwrap();
        ExportTemporary {
            path,
            file: Some(fs::File::from_std(File::from(OwnedFd::from(writer)))),
        }
    }

    /// A temporary that cannot be synced fails the commit as a failed sync.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_temporary_that_cannot_be_synced_fails_the_commit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsynced.tmp");
        std::fs::write(&path, b"partial").unwrap();
        let error = unsyncable_temporary(path)
            .commit(&directory.path().join("recording.mp4"))
            .await
            .expect_err("a pipe cannot be synced");
        assert!(matches!(error, ExportError::Sync(_)), "{error:?}");
        assert_eq!(error.to_string(), "syncing the export");
    }

    /// A commit syncs the temporary before it renames it: a temporary that
    /// cannot be synced is not renamed, so no file appears at the
    /// destination.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_temporary_that_cannot_be_synced_leaves_no_file_at_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsynced.tmp");
        std::fs::write(&path, b"partial").unwrap();
        let destination = directory.path().join("recording.mp4");
        let _ = unsyncable_temporary(path).commit(&destination).await;
        assert!(!destination.exists(), "the commit renamed an unsynced file");
    }

    /// A commit that fails removes its temporary on the blocking pool and
    /// returns once it is gone, as every export that stops does. The pool's
    /// one thread runs the sync that fails and then a task that holds the
    /// thread: the removal waits behind that task, and the commit waits for
    /// the removal.
    #[cfg(unix)]
    #[test]
    fn a_failed_commit_returns_once_its_temporary_is_gone() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("unsynced.tmp");
        std::fs::write(&path, b"partial").unwrap();
        let destination = directory.path().join("recording.mp4");
        let (runtime, start) = held_blocking_pool();
        runtime.block_on(async {
            let temporary = unsyncable_temporary(path.clone());
            let committing = tokio::spawn(async move { temporary.commit(&destination).await });
            // The commit hands its sync to the pool, whose thread is held.
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            // A second task to hold the thread, queued behind the sync.
            let (started, holding) = async_channel::bounded(1);
            let (release, held) = mpsc::channel::<()>();
            drop(tokio::task::spawn_blocking(move || {
                let _ = started.send_blocking(());
                let _ = held.recv();
            }));
            start.send(()).unwrap();
            // The pool runs its tasks in order on its one thread, so the
            // sync has failed by the time the second task says it runs.
            holding.recv().await.unwrap();
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            assert!(
                path.exists() && !committing.is_finished(),
                "the failed commit waits for its removal"
            );
            release.send(()).unwrap();
            committing
                .await
                .unwrap()
                .expect_err("a pipe cannot be synced");
            assert!(!path.exists(), "the commit returned with the file gone");
        });
    }

    /// Queues on the blocking pool a task that holds its thread until the
    /// sender this gives is used or dropped. Gives a receiver that hears when
    /// the task starts, and the sender.
    fn queue_holder() -> (Receiver<()>, Sender<()>) {
        let (started, starting) = async_channel::bounded(1);
        let (release, held) = mpsc::channel::<()>();
        drop(tokio::task::spawn_blocking(move || {
            let _ = started.send_blocking(());
            let _ = held.recv();
        }));
        (starting, release)
    }

    /// A commit hands the close of its file to the blocking pool once the
    /// rename has placed it, and returns after the close has run. The pool
    /// has one thread. A task that holds it is queued behind the sync, and
    /// another behind the rename. While the second holds the thread, the
    /// file is in place and the commit waits.
    #[test]
    fn a_commit_closes_its_file_on_the_blocking_pool() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("committed.tmp");
        let destination = directory.path().join("recording.mp4");
        let file = File::create(&path).unwrap();
        let (runtime, start) = held_blocking_pool();
        runtime.block_on(async {
            let temporary = ExportTemporary {
                path,
                file: Some(fs::File::from_std(file)),
            };
            let committing = tokio::spawn({
                let destination = destination.clone();
                async move { temporary.commit(&destination).await }
            });
            // The commit hands its sync to the pool, whose thread is held.
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            let (after_sync, release_after_sync) = queue_holder();
            start.send(()).unwrap();
            // The pool runs its tasks in order on its one thread, so the
            // sync is done once the task behind it runs.
            after_sync.recv().await.unwrap();
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            // The commit has handed its rename to the pool behind that task.
            let (after_rename, release_after_rename) = queue_holder();
            release_after_sync.send(()).unwrap();
            after_rename.recv().await.unwrap();
            for _ in 0..3 {
                tokio::task::yield_now().await;
            }
            assert!(
                destination.exists() && !committing.is_finished(),
                "the commit waits for the pool to close its file"
            );
            release_after_rename.send(()).unwrap();
            committing.await.unwrap().unwrap();
        });
    }

    /// The error of a task cancelled before it ended.
    async fn cancelled_task() -> JoinError {
        let task = tokio::spawn(std::future::pending::<()>());
        task.abort();
        task.await.unwrap_err()
    }

    /// Each way an export stops that keeps the failure it met, with its
    /// text.
    async fn failures_with_a_source() -> Vec<(ExportError, &'static str)> {
        let destination = PathBuf::from("/exports/recording.mp4");
        let failure = || io::Error::other("fixture failure");
        let transfer = reqwest::Client::new().get("not a url").build().unwrap_err();
        vec![
            (
                ExportError::TemporaryName(getrandom::Error::UNSUPPORTED),
                "creating an export temporary name",
            ),
            (
                ExportError::TemporaryTask(cancelled_task().await),
                "creating an export temporary",
            ),
            (
                ExportError::Create {
                    destination: destination.clone(),
                    source: failure(),
                },
                "creating export beside /exports/recording.mp4",
            ),
            (
                ExportError::Duplicate(failure()),
                "duplicating the export descriptor",
            ),
            (
                ExportError::ProbeTask(cancelled_task().await),
                "running the free-space probe",
            ),
            (
                ExportError::FreeSpace(failure()),
                "reading the export filesystem's free space",
            ),
            (ExportError::Read(transfer), "reading the export"),
            (ExportError::Write(failure()), "writing the export"),
            (ExportError::Flush(failure()), "flushing the export"),
            (ExportError::Sync(failure()), "syncing the export"),
            (
                ExportError::Commit {
                    destination,
                    source: failure(),
                },
                "committing export to /exports/recording.mp4",
            ),
        ]
    }

    /// Each way an export stops that met no error of another kind, with its
    /// text.
    fn failures_without_a_source() -> Vec<(ExportError, &'static str)> {
        vec![
            (
                ExportError::LowSpace {
                    available_bytes: (512 << 20) + 1,
                    reserve_bytes: 2 << 30,
                },
                "export stopped: 512 MiB free on the destination filesystem is below its 2048 MiB \
                 reserve",
            ),
            (
                ExportError::NoFileName,
                "export destination has no file name",
            ),
            (
                ExportError::NoUniqueName,
                "cannot allocate a unique export temporary file",
            ),
            (ExportError::Overflow, "export byte count overflows"),
            (ExportError::Empty, "export response is empty"),
        ]
    }

    /// Each way an export stops has the text the Linux viewer shows.
    #[tokio::test]
    async fn each_export_failure_has_its_text() {
        let mut failures = failures_with_a_source().await;
        failures.extend(failures_without_a_source());
        for (error, text) in failures {
            assert_eq!(error.to_string(), text, "{error:?}");
        }
    }

    #[tokio::test]
    async fn each_export_failure_that_met_an_error_keeps_it_as_its_source() {
        for (error, _) in failures_with_a_source().await {
            assert!(error.source().is_some(), "{error:?}");
        }
    }

    #[test]
    fn each_export_failure_without_a_cause_has_no_source() {
        for (error, _) in failures_without_a_source() {
            assert!(error.source().is_none(), "{error:?}");
        }
    }

    /// A refusal of `iris-client` stops an export with the refusal's text.
    #[test]
    fn a_refused_export_has_the_text_of_the_refusal() {
        let refused = ExportError::from(ApiError::Invalid(Rule::TimeRange));
        assert_eq!(refused.to_string(), "invalid time range");
    }

    /// The error of an export that `iris-client` refused has the refusal's
    /// source and adds none: a refusal without a source stops an export
    /// without one.
    #[test]
    fn a_refused_export_adds_no_source_to_the_refusal() {
        let refused = ExportError::from(ApiError::Invalid(Rule::TimeRange));
        assert!(refused.source().is_none(), "{refused:?}");
    }

    /// An export that stops has the text the Linux viewer shows: an empty
    /// answer, a head that is not an export's, and a body cut short after the
    /// temporary has taken some of it. The tests
    /// [`failed_exports_leave_the_destination_as_it_was`] and
    /// [`failed_exports_leave_no_temporary`] check what each leaves behind.
    #[tokio::test]
    async fn an_export_that_stops_has_the_text_of_its_stop() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        for (from, text) in [
            (2, "export response is empty"),
            (3, "response is not video/mp4"),
            (5, "requesting the export failed with 206 Partial Content"),
            (CUT_SHORT, "reading the export"),
        ] {
            let (client, server) = stopping_client(from).await;
            let error = export_case(&client, from, &destination).await.unwrap_err();
            assert_eq!(error.to_string(), text, "from {from}");
            server.abort();
        }
    }

    #[tokio::test]
    async fn an_export_whose_body_fails_keeps_the_transport_error_as_its_source() {
        let (client, server) = stopping_client(CUT_SHORT).await;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let failed = export_case(&client, CUT_SHORT, &destination)
            .await
            .unwrap_err();
        assert!(matches!(failed, ExportError::Read(_)), "{failed:?}");
        let source = failed
            .source()
            .expect("the transport's error is the source");
        assert!(source.is::<reqwest::Error>(), "{source:?}");
        server.abort();
    }

    #[tokio::test]
    async fn an_export_to_a_destination_without_a_file_name_is_refused() {
        let (base, server) = export_server().await;
        let error = export_case(&fixture::client(base), 4, Path::new("/"))
            .await
            .unwrap_err();
        assert!(matches!(error, ExportError::NoFileName), "{error:?}");
        assert_eq!(error.to_string(), "export destination has no file name");
        server.abort();
    }

    /// An export reads its destination's free space after every 64 MiB it
    /// writes, the interval `Client::export` documents.
    #[test]
    fn the_free_space_check_interval_is_64_mib() {
        let origin = ApiOrigin::parse("http://127.0.0.1:9", PlainHttp::LoopbackOnly).unwrap();
        assert_eq!(fixture::client(origin).space_check_bytes, 64 << 20);
    }

    /// Calls of the counting probe in
    /// [`an_export_below_its_check_interval_reads_the_free_space_once`].
    static ONCE_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// An export reads its destination's free space before its first byte
    /// and then after every 64 MiB it writes, so one of 8 KiB reads it once.
    #[tokio::test]
    async fn an_export_below_its_check_interval_reads_the_free_space_once() {
        let (base, server) = export_server().await;
        let mut client = fixture::client(base);
        client.space_probe = |_| {
            ONCE_PROBES.fetch_add(1, Ordering::SeqCst);
            Ok(PLENTY_OF_SPACE)
        };
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        export_case(&client, 10, &destination).await.unwrap();
        assert_eq!(ONCE_PROBES.load(Ordering::SeqCst), 1);
        server.abort();
    }

    /// Calls of the counting probe in
    /// [`the_free_space_is_read_once_the_bytes_written_reach_the_interval`].
    static INTERVAL_PROBES: AtomicUsize = AtomicUsize::new(0);

    /// The free space is read again once the bytes written reach the check
    /// interval, and not a byte before. The chunks are 2,047 and 1 bytes,
    /// twice, under an interval of 2 KiB. The export reads the free space
    /// before the first byte and at 2 KiB and 4 KiB, and not at 2,047 or
    /// 4,095 bytes.
    #[tokio::test]
    async fn the_free_space_is_read_once_the_bytes_written_reach_the_interval() {
        let (base, server) = export_server().await;
        let mut client = fixture::client(base);
        client.space_probe = |_| {
            INTERVAL_PROBES.fetch_add(1, Ordering::SeqCst);
            Ok(PLENTY_OF_SPACE)
        };
        client.space_check_bytes = 2048;
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        export_case(&client, 40, &destination).await.unwrap();
        assert_eq!(INTERVAL_PROBES.load(Ordering::SeqCst), 3);
        server.abort();
    }

    /// The length of the temporary at the first read of the free space in
    /// [`the_free_space_is_read_before_the_first_byte_is_written`].
    static FIRST_PROBED_LENGTH: Mutex<Option<u64>> = Mutex::new(None);

    /// An export reads the free space before it writes its first byte: the
    /// probe finds the temporary empty at its first call.
    #[tokio::test]
    async fn the_free_space_is_read_before_the_first_byte_is_written() {
        let (base, server) = export_server().await;
        let mut client = fixture::client(base);
        client.space_probe = |file| {
            let length = file.metadata()?.len();
            let mut first = FIRST_PROBED_LENGTH.lock().expect("first probed length");
            first.get_or_insert(length);
            Ok(PLENTY_OF_SPACE)
        };
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        export_case(&client, 10, &destination).await.unwrap();
        assert_eq!(
            *FIRST_PROBED_LENGTH.lock().unwrap(),
            Some(0),
            "the temporary holds no byte at the first read of the free space"
        );
        server.abort();
    }

    /// The thread of each read of the free space in
    /// [`the_free_space_is_read_on_the_blocking_pool`].
    static PROBE_THREADS: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

    /// An export reads the free space on the blocking pool, so a stalled
    /// mount holds a blocking thread and no runtime worker. The runtime of
    /// a Tokio test runs on the test's thread alone, so every other thread
    /// is one of the blocking pool.
    #[tokio::test]
    async fn the_free_space_is_read_on_the_blocking_pool() {
        let (base, server) = export_server().await;
        let mut client = fixture::client(base);
        client.space_probe = |_| {
            let mut threads = PROBE_THREADS.lock().expect("probe threads");
            threads.push(thread::current().id());
            Ok(PLENTY_OF_SPACE)
        };
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        export_case(&client, 10, &destination).await.unwrap();
        let threads = PROBE_THREADS.lock().unwrap().clone();
        assert!(
            !threads.is_empty() && !threads.contains(&thread::current().id()),
            "the free space is read on the runtime's thread: {threads:?}"
        );
        server.abort();
    }

    /// The progress callback hears the bytes written after every chunk, and
    /// may keep what it hears: chunks of 2,047 and 1 bytes, twice, report
    /// each running total.
    #[tokio::test]
    async fn progress_hears_the_bytes_written_after_every_chunk() {
        let (base, server) = export_server().await;
        let client = spacious_client(base);
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let mut heard = Vec::new();
        client
            .export(
                &fixture::camera("cam1"),
                StreamKind::Main,
                fixture::window(40, 41),
                &destination,
                |written| heard.push(written),
            )
            .await
            .unwrap();
        assert_eq!(heard, [2047, 2048, 4095, 4096]);
        server.abort();
    }

    /// A temporary at `path` that holds an open file, as an export's holds
    /// its file until its commit, so that its drop or its discard removes
    /// `path`. The file is an anonymous one.
    fn temporary_at(path: PathBuf) -> ExportTemporary {
        ExportTemporary {
            path,
            file: Some(fs::File::from_std(tempfile::tempfile().unwrap())),
        }
    }

    /// The temporary of `directory` whose name is written from 16 bytes of
    /// `byte`.
    fn temporary_named(directory: &Path, byte: u8) -> PathBuf {
        let digits = format!("{byte:02x}").repeat(16);
        directory.join(format!(".irisd-export-{digits}.tmp"))
    }

    /// A temporary's name holds its 16 bytes in order, each as two lowercase
    /// hexadecimal digits.
    #[tokio::test]
    async fn a_temporary_is_named_by_its_bytes_in_lowercase_hexadecimal() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let rising: NameBytes = |bytes| {
            for (byte, low) in bytes.iter_mut().zip(0_u8..) {
                *byte = 0xf0 | low;
            }
            Ok(())
        };
        let temporary = ExportTemporary::create_named(&destination, rising)
            .await
            .unwrap();
        let named = directory
            .path()
            .join(".irisd-export-f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff.tmp");
        assert_eq!(temporary.path, named);
        temporary.discard().await;
    }

    /// Name bytes that count up from 0, for
    /// [`a_taken_name_is_tried_again_under_the_next`].
    static NEXT_NAME: AtomicU8 = AtomicU8::new(0);

    /// A temporary whose name is taken is created under the next name.
    #[tokio::test]
    async fn a_taken_name_is_tried_again_under_the_next() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(temporary_named(directory.path(), 0), b"another export").unwrap();
        let counting: NameBytes = |bytes| {
            bytes.fill(NEXT_NAME.fetch_add(1, Ordering::SeqCst));
            Ok(())
        };
        let temporary = ExportTemporary::create_named(&destination, counting)
            .await
            .unwrap();
        let named = temporary_named(directory.path(), 1);
        assert!(
            temporary.path == named && named.is_file(),
            "{:?}",
            temporary.path
        );
        temporary.discard().await;
    }

    /// Name bytes that count up from 0, for
    /// [`a_taken_name_leaves_the_file_that_holds_it`].
    static KEPT_NAME: AtomicU8 = AtomicU8::new(0);

    /// The file that holds a taken name is left as it was.
    #[tokio::test]
    async fn a_taken_name_leaves_the_file_that_holds_it() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        let taken = temporary_named(directory.path(), 0);
        std::fs::write(&taken, b"another export").unwrap();
        let counting: NameBytes = |bytes| {
            bytes.fill(KEPT_NAME.fetch_add(1, Ordering::SeqCst));
            Ok(())
        };
        let temporary = ExportTemporary::create_named(&destination, counting)
            .await
            .unwrap();
        temporary.discard().await;
        assert_eq!(std::fs::read(&taken).unwrap(), b"another export");
    }

    /// Name bytes of zeros, the name a test takes first.
    const ZEROS: NameBytes = |bytes| {
        bytes.fill(0);
        Ok(())
    };

    /// Calls of the name source in
    /// [`an_export_whose_every_name_is_taken_stops_after_sixteen_tries`].
    static TAKEN_TRIES: AtomicUsize = AtomicUsize::new(0);

    /// When every name tried is taken, the export stops after sixteen tries.
    #[tokio::test]
    async fn an_export_whose_every_name_is_taken_stops_after_sixteen_tries() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(temporary_named(directory.path(), 0), b"another export").unwrap();
        let counted_zeros: NameBytes = |bytes| {
            TAKEN_TRIES.fetch_add(1, Ordering::SeqCst);
            ZEROS(bytes)
        };
        let _ = ExportTemporary::create_named(&destination, counted_zeros).await;
        assert_eq!(TAKEN_TRIES.load(Ordering::SeqCst), 16);
    }

    #[tokio::test]
    async fn an_export_whose_every_name_is_taken_fails_without_a_unique_name() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("recording.mp4");
        std::fs::write(temporary_named(directory.path(), 0), b"another export").unwrap();
        let error = ExportTemporary::create_named(&destination, ZEROS)
            .await
            .err()
            .expect("every name is taken");
        assert!(matches!(error, ExportError::NoUniqueName), "{error:?}");
        assert_eq!(
            error.to_string(),
            "cannot allocate a unique export temporary file"
        );
    }

    /// Calls of the name source in
    /// [`a_temporary_that_cannot_be_created_stops_the_export_at_once`].
    static CREATE_TRIES: AtomicUsize = AtomicUsize::new(0);

    /// A failure to create the temporary other than a taken name ends the
    /// export at the first try: here a destination in a directory that does
    /// not exist.
    #[tokio::test]
    async fn a_temporary_that_cannot_be_created_stops_the_export_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("missing").join("recording.mp4");
        let counted_zeros: NameBytes = |bytes| {
            CREATE_TRIES.fetch_add(1, Ordering::SeqCst);
            ZEROS(bytes)
        };
        let _ = ExportTemporary::create_named(&destination, counted_zeros).await;
        assert_eq!(CREATE_TRIES.load(Ordering::SeqCst), 1);
    }

    /// A temporary that cannot be created fails the export with the error
    /// the open met, beside the destination it names.
    #[tokio::test]
    async fn a_temporary_that_cannot_be_created_fails_with_the_error_the_open_met() {
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("missing").join("recording.mp4");
        let error = ExportTemporary::create(&destination)
            .await
            .err()
            .expect("no temporary in a directory that does not exist");
        let met = match &error {
            ExportError::Create { source, .. } => Some(source.kind()),
            _ => None,
        };
        assert_eq!(met, Some(ErrorKind::NotFound), "{error:?}");
        assert_eq!(
            error.to_string(),
            format!("creating export beside {}", destination.display())
        );
    }

    /// A temporary that cannot be removed is logged with its path and the
    /// error. A directory in the temporary's place cannot be removed as a
    /// file.
    #[test]
    fn a_temporary_that_cannot_be_removed_is_logged() {
        let directory = tempfile::tempdir().unwrap();
        let held = directory.path().join("held");
        std::fs::create_dir(&held).unwrap();
        let word = format!("path={}", held.display());
        assert_eq!(fixture::logged(&word), Vec::<String>::new());
        drop(temporary_at(held));
        let lines = fixture::logged(&word);
        let line = format!(
            " WARN iris_transport::export: cannot remove an export temporary {word} error="
        );
        assert!(lines.len() == 1 && lines[0].starts_with(&line), "{lines:?}");
    }

    /// A removal that fails leaves what holds the temporary's path: the
    /// directory in its place is still there.
    #[test]
    fn a_temporary_that_cannot_be_removed_stays_in_its_place() {
        let directory = tempfile::tempdir().unwrap();
        let held = directory.path().join("held");
        std::fs::create_dir(&held).unwrap();
        drop(temporary_at(held.clone()));
        assert!(held.is_dir(), "{held:?}");
    }

    #[test]
    fn a_temporary_already_gone_is_not_logged() {
        let directory = tempfile::tempdir().unwrap();
        let gone = directory.path().join("gone");
        let word = format!("path={}", gone.display());
        assert_eq!(fixture::logged(&word), Vec::<String>::new());
        drop(temporary_at(gone));
        assert_eq!(fixture::logged(&word), Vec::<String>::new());
    }

    #[test]
    fn a_temporary_dropped_where_no_runtime_runs_is_removed_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dropped.tmp");
        std::fs::write(&path, b"partial").unwrap();
        drop(temporary_at(path.clone()));
        assert!(!path.exists(), "the drop removed the file");
    }

    /// A runtime whose blocking pool has one thread, with a task that holds
    /// that thread until the sender given with the runtime is used or
    /// dropped. A removal handed to the pool waits behind the task, and one
    /// done on the runtime's thread does not.
    fn held_blocking_pool() -> (Runtime, Sender<()>) {
        let runtime = Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        let (release, held) = mpsc::channel();
        drop(runtime.spawn_blocking(move || held.recv()));
        (runtime, release)
    }

    /// A temporary dropped while a runtime runs, as a cancelled export drops
    /// it, is removed on the blocking pool and not on the runtime's thread.
    /// While the pool's one thread is held, the file stays. Once the thread
    /// is let go, the removal runs.
    #[test]
    fn a_dropped_temporary_is_removed_on_the_blocking_pool() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dropped.tmp");
        std::fs::write(&path, b"partial").unwrap();
        let (runtime, release) = held_blocking_pool();
        runtime.block_on(async {
            drop(temporary_at(path.clone()));
            assert!(path.exists(), "the drop did not remove the file itself");
            release.send(()).unwrap();
            // The pool runs its tasks in order on its one thread, so this
            // one ends after the removal.
            tokio::task::spawn_blocking(|| ()).await.unwrap();
            assert!(!path.exists(), "the pool removed the file");
        });
    }

    /// The teardown of an export that stops removes its temporary on the
    /// blocking pool and returns once it is gone: while the pool's one
    /// thread is held, the discard waits and the file stays.
    #[test]
    fn a_discarded_temporary_is_removed_on_the_blocking_pool() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discarded.tmp");
        std::fs::write(&path, b"partial").unwrap();
        let (runtime, release) = held_blocking_pool();
        runtime.block_on(async {
            let discarding = tokio::spawn(temporary_at(path.clone()).discard());
            tokio::task::yield_now().await;
            assert!(
                path.exists() && !discarding.is_finished(),
                "the discard waits"
            );
            release.send(()).unwrap();
            discarding.await.unwrap();
            assert!(!path.exists(), "the discard returned with the file gone");
        });
    }

    /// A removal the blocking pool drops unrun, as a runtime that is shutting
    /// down drops the tasks it is handed, removes the file where it is
    /// dropped, so no temporary outlives its export.
    #[test]
    fn a_removal_the_pool_refuses_removes_the_file_where_it_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("refused.tmp");
        std::fs::write(&path, b"partial").unwrap();
        let runtime = Builder::new_current_thread().build().unwrap();
        let handle = runtime.handle().clone();
        drop(runtime);
        let _entered = handle.enter();
        drop(temporary_at(path.clone()));
        assert!(!path.exists(), "the refused removal removed the file");
    }
}
