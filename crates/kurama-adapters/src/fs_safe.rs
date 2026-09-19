//! Descriptor-rooted filesystem transactions. Unix path components are opened
//! without following symlinks; unsupported platforms fail closed rather than
//! pretending a pathname check provides the same containment guarantee.
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    path::{Component, Path},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

#[cfg(any(feature = "fs-store", feature = "native-credentials"))]
use std::io::Read;
#[cfg(any(feature = "fs-store", feature = "native-credentials", test))]
use std::io::Write;

#[cfg(unix)]
use rustix::fs::{self, AtFlags, Mode, OFlags};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub(crate) struct Directory(Arc<File>);

fn component(name: &OsStr) -> io::Result<()> {
    let mut parts = Path::new(name).components();
    if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one normal path component",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-rooted filesystem access requires Unix",
    )
}

impl Directory {
    /// Open an already canonical absolute directory, rejecting swapped components.
    pub(crate) fn open_absolute(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            if !path.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected absolute directory",
                ));
            }
            let mut directory = Self(Arc::new(File::from(fs::open(
                "/",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?)));
            for part in path.components() {
                match part {
                    Component::RootDir => {}
                    Component::Normal(name) => directory = directory.child(name)?,
                    _ => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "non-canonical directory",
                        ));
                    }
                }
            }
            Ok(directory)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            Err(unsupported())
        }
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials", test))]
    /// The parent may contain OS aliases (e.g. macOS /var); the requested leaf may not.
    pub(crate) fn ensure_root(path: &Path) -> io::Result<Self> {
        let absolute = std::path::absolute(path)?;
        let Some(name) = absolute.file_name() else {
            return Self::open_absolute(&absolute);
        };
        let parent = absolute.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "directory has no parent")
        })?;
        Self::open_absolute(&parent.canonicalize()?)?.ensure_dir(name)
    }

    pub(crate) fn child(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        let name = name.as_ref();
        component(name)?;
        #[cfg(unix)]
        {
            Ok(Self(Arc::new(File::from(fs::openat(
                &*self.0,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?))))
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials", test))]
    pub(crate) fn create_dir(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        let name = name.as_ref();
        component(name)?;
        #[cfg(unix)]
        {
            fs::mkdirat(&*self.0, name, Mode::from_raw_mode(0o700))?;
            self.child(name)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials", test))]
    pub(crate) fn ensure_dir(&self, name: impl AsRef<OsStr>) -> io::Result<Self> {
        let name = name.as_ref();
        let directory = match self.create_dir(name) {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self.child(name)?,
            Err(error) => return Err(error),
        };
        owner_only(&directory.0, true)?;
        // Also sync when another opener won creation: its mkdir may not yet be durable.
        self.sync()?;
        Ok(directory)
    }

    pub(crate) fn open_file(
        &self,
        name: impl AsRef<OsStr>,
        writable: bool,
        create: bool,
    ) -> io::Result<File> {
        if create {
            match self.file(name.as_ref(), writable, true, true) {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.file(name.as_ref(), writable, false, false)
                }
                result => result,
            }
        } else {
            self.file(name.as_ref(), writable, false, false)
        }
    }

    pub(crate) fn create_file(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
        self.file(name.as_ref(), true, true, true)
    }

    fn file(
        &self,
        name: &OsStr,
        writable: bool,
        create: bool,
        exclusive: bool,
    ) -> io::Result<File> {
        component(name)?;
        #[cfg(unix)]
        {
            // NONBLOCK prevents a substituted FIFO/device from blocking before type validation.
            let mut flags = OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
            flags |= if writable {
                OFlags::RDWR
            } else {
                OFlags::RDONLY
            };
            if create {
                flags |= OFlags::CREATE;
            }
            if exclusive {
                flags |= OFlags::EXCL;
            }
            let file = File::from(fs::openat(
                &*self.0,
                name,
                flags,
                Mode::from_raw_mode(0o600),
            )?);
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected a regular file",
                ));
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let _ = (writable, create, exclusive);
            Err(unsupported())
        }
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials"))]
    pub(crate) fn read(&self, name: impl AsRef<OsStr>) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_file(name, false, false)?
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials"))]
    pub(crate) fn lock(&self, name: impl AsRef<OsStr>) -> io::Result<File> {
        let file = self.open_file(name, true, true)?;
        owner_only(&file, false)?;
        file.lock()?;
        Ok(file)
    }

    #[cfg(any(feature = "fs-store", feature = "tools"))]
    /// Reopen, rather than dup, so flock serializes independently cloned handles.
    pub(crate) fn lock_handle(&self) -> io::Result<File> {
        #[cfg(unix)]
        {
            Ok(File::from(fs::openat(
                &*self.0,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?))
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials"))]
    /// Initialization is locked separately from the replaceable data inode.
    pub(crate) fn ensure_file(&self, name: impl AsRef<OsStr>, initial: &[u8]) -> io::Result<()> {
        let _lock = self.lock(".initialize.lock")?;
        match self.open_file(name.as_ref(), false, false) {
            Ok(file) => owner_only(&file, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.replace(name.as_ref(), initial)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn temporary(&self) -> io::Result<(OsString, File)> {
        loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = OsString::from(format!(".kurama-{}-{sequence}.tmp", std::process::id()));
            match self.create_file(&name) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(feature = "fs-store")]
    pub(crate) fn temporary_directory(&self) -> io::Result<(OsString, Self)> {
        loop {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = OsString::from(format!(".kurama-{}-{sequence}.tmp", std::process::id()));
            match self.create_dir(&name) {
                Ok(directory) => return Ok((name, directory)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(any(feature = "fs-store", feature = "native-credentials", test))]
    pub(crate) fn replace(&self, name: impl AsRef<OsStr>, bytes: &[u8]) -> io::Result<()> {
        let name = name.as_ref();
        // Refuse pre-existing symlinks and special files, not only symlink traversal.
        match self.open_file(name, false, false) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let (temporary, mut file) = self.temporary()?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            self.rename(&temporary, name)?;
            self.sync()
        })();
        if result.is_err() {
            let _ = self.remove_file(&temporary);
        }
        result
    }

    pub(crate) fn rename(&self, from: impl AsRef<OsStr>, to: impl AsRef<OsStr>) -> io::Result<()> {
        component(from.as_ref())?;
        component(to.as_ref())?;
        #[cfg(unix)]
        {
            Ok(fs::renameat(
                &*self.0,
                from.as_ref(),
                &*self.0,
                to.as_ref(),
            )?)
        }
        #[cfg(not(unix))]
        {
            Err(unsupported())
        }
    }

    #[cfg(feature = "fs-store")]
    pub(crate) fn publish_directory(
        &self,
        from: &OsStr,
        destination: &Self,
        to: &OsStr,
    ) -> io::Result<()> {
        component(from)?;
        component(to)?;
        #[cfg(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "ios"
        ))]
        {
            Ok(fs::renameat_with(
                &*self.0,
                from,
                &*destination.0,
                to,
                fs::RenameFlags::NOREPLACE,
            )?)
        }
        #[cfg(all(
            unix,
            not(any(
                target_os = "linux",
                target_os = "android",
                target_os = "macos",
                target_os = "ios"
            ))
        ))]
        {
            // A completed destination is non-empty and cannot be replaced by POSIX rename.
            // Cooperating creators additionally hold the stable creation lock.
            match destination.child(to) {
                Ok(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "session already exists",
                    ));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            Ok(fs::renameat(&*self.0, from, &*destination.0, to)?)
        }
        #[cfg(not(unix))]
        {
            let _ = destination;
            Err(unsupported())
        }
    }

    pub(crate) fn remove_file(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        self.remove(name.as_ref(), false)
    }

    #[cfg(feature = "fs-store")]
    pub(crate) fn remove_dir(&self, name: impl AsRef<OsStr>) -> io::Result<()> {
        self.remove(name.as_ref(), true)
    }

    fn remove(&self, name: &OsStr, directory: bool) -> io::Result<()> {
        component(name)?;
        #[cfg(unix)]
        {
            Ok(fs::unlinkat(
                &*self.0,
                name,
                if directory {
                    AtFlags::REMOVEDIR
                } else {
                    AtFlags::empty()
                },
            )?)
        }
        #[cfg(not(unix))]
        {
            let _ = directory;
            Err(unsupported())
        }
    }

    #[cfg(all(feature = "fs-store", unix))]
    pub(crate) fn entries(&self) -> io::Result<impl Iterator<Item = io::Result<OsString>> + use<>> {
        Ok(
            fs::Dir::read_from(&*self.0)?.filter_map(|entry| match entry {
                Ok(entry) => {
                    use std::os::unix::ffi::OsStrExt;
                    let name = entry.file_name().to_bytes();
                    (name != b"." && name != b"..").then(|| Ok(OsStr::from_bytes(name).to_owned()))
                }
                Err(error) => Some(Err(error.into())),
            }),
        )
    }

    #[cfg(all(feature = "fs-store", not(unix)))]
    pub(crate) fn entries(&self) -> io::Result<std::vec::IntoIter<io::Result<OsString>>> {
        Err(unsupported())
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.0.sync_all()
    }
}

#[cfg(any(feature = "fs-store", feature = "native-credentials", test))]
pub(crate) fn owner_only(file: &File, directory: bool) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::{fs::Permissions, os::unix::fs::PermissionsExt};
        file.set_permissions(Permissions::from_mode(if directory {
            0o700
        } else {
            0o600
        }))?;
    }
    #[cfg(not(unix))]
    let _ = (file, directory);
    Ok(())
}

#[cfg(feature = "tools")]
pub(crate) async fn blocking<T: Send + 'static>(
    cancel: &dyn kurama_protocol::traits::CancelSignal,
    work: impl FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Result<T, kurama_protocol::KuramaError>
    + Send
    + 'static,
) -> Result<T, kurama_protocol::KuramaError> {
    use kurama_protocol::KuramaError;
    use std::sync::atomic::AtomicBool;
    if cancel.is_cancelled() {
        return Err(KuramaError::Cancelled);
    }
    let flag = Arc::new(AtomicBool::new(false));
    // Dropping the tool future still asks the blocking transaction to stop before commit.
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let _guard = CancelOnDrop(flag.clone());
    let cancelled = cancel.cancelled();
    let mut task = tokio::task::spawn_blocking({
        let flag = flag.clone();
        move || work(flag)
    });
    let joined = tokio::select! {
        result = &mut task => result,
        () = cancelled => {
            flag.store(true, Ordering::Release);
            // Commit is non-interruptible: the transaction, not this waiter, reports its outcome.
            task.await
        }
    };
    joined.map_err(|error| KuramaError::Tool(format!("file transaction failed: {error}")))?
}

#[cfg(feature = "tools")]
pub(crate) fn checkpoint(
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(), kurama_protocol::KuramaError> {
    if cancel.load(Ordering::Acquire) {
        Err(kurama_protocol::KuramaError::Cancelled)
    } else {
        Ok(())
    }
}

#[cfg(all(test, feature = "tools", unix))]
mod tests {
    use super::*;
    use kurama_protocol::traits::{BoxFuture, CancelSignal};

    struct WatchCancel(tokio::sync::watch::Receiver<bool>);

    impl CancelSignal for WatchCancel {
        fn is_cancelled(&self) -> bool {
            *self.0.borrow()
        }
        fn cancelled(&self) -> BoxFuture<'static, ()> {
            let mut receiver = self.0.clone();
            Box::pin(async move {
                loop {
                    if *receiver.borrow_and_update() {
                        return;
                    }
                    if receiver.changed().await.is_err() {
                        return;
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn cancellation_after_commit_preserves_the_transaction_outcome() {
        let temp = tempfile::tempdir().expect("tempdir");
        let directory = Directory::ensure_root(temp.path()).expect("directory");
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let cancel = WatchCancel(receiver);
        let (committed, observed) = tokio::sync::oneshot::channel();
        let transaction = blocking(&cancel, move |cancel| {
            directory.replace("result", b"committed")?;
            committed.send(()).expect("commit observer");
            while !cancel.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Ok(())
        });
        let cancellation = async move {
            observed.await.expect("commit");
            sender.send(true).expect("cancel after commit");
        };
        let (result, ()) = tokio::join!(transaction, cancellation);
        result.expect("committed success must not become Cancelled");
        assert_eq!(
            std::fs::read(temp.path().join("result")).expect("durable result"),
            b"committed"
        );
    }
}
