use crate::os::windows::prelude::*;

use crate::ffi::OsStr;
use crate::io::{self, IoSlice, IoSliceMut};
use crate::mem;
use crate::path::Path;
use crate::ptr;
use crate::slice;
use crate::sync::atomic::AtomicUsize;
use crate::sync::atomic::Ordering::SeqCst;
use crate::sys::c;
use crate::sys::fs::{File, OpenOptions};
use crate::sys::handle::Handle;
use crate::sys::hashmap_random_keys;
use crate::sys_common::IntoInner;

////////////////////////////////////////////////////////////////////////////////
// Anonymous pipes
////////////////////////////////////////////////////////////////////////////////

// A 64kb pipe capacity is the same as a typical Linux default.
const PIPE_BUFFER_CAPACITY: u32 = 64 * 1024;

pub enum AnonPipe {
    Sync(Handle),
    Async(Handle),
}

impl IntoInner<Handle> for AnonPipe {
    fn into_inner(self) -> Handle {
        match self {
            Self::Sync(handle) => handle,
            Self::Async(handle) => handle,
        }
    }
}

pub struct Pipes {
    pub ours: AnonPipe,
    pub theirs: AnonPipe,
}
impl Pipes {
    /// Create a new pair of pipes where both pipes are synchronous.
    ///
    /// These must not be used asynchronously.
    pub fn new_synchronous(
        ours_readable: bool,
        their_handle_inheritable: bool,
    ) -> io::Result<Self> {
        unsafe {
            // If `CreatePipe` succeeds, these will be our pipes.
            let mut read = ptr::null_mut();
            let mut write = ptr::null_mut();

            if c::CreatePipe(&mut read, &mut write, ptr::null(), PIPE_BUFFER_CAPACITY) == 0 {
                Err(io::Error::last_os_error())
            } else {
                let (ours, theirs) = if ours_readable { (read, write) } else { (write, read) };
                let ours = Handle::from_raw_handle(ours);
                #[cfg(not(target_vendor = "uwp"))]
                let theirs = Handle::from_raw_handle(theirs);
                #[cfg(target_vendor = "uwp")]
                let mut theirs = Handle::from_raw_handle(theirs);

                if their_handle_inheritable {
                    #[cfg(not(target_vendor = "uwp"))]
                    {
                        theirs.set_inheritable()?;
                    }

                    #[cfg(target_vendor = "uwp")]
                    {
                        theirs = theirs.duplicate(0, true, c::DUPLICATE_SAME_ACCESS)?;
                    }
                }

                Ok(Pipes { ours: AnonPipe::Sync(ours), theirs: AnonPipe::Sync(theirs) })
            }
        }
    }
}

/// Although this looks similar to `anon_pipe` in the Unix module it's actually
/// subtly different. Here we'll return two pipes in the `Pipes` return value,
/// but one is intended for "us" where as the other is intended for "someone
/// else".
///
/// Currently the only use case for this function is pipes for stdio on
/// processes in the standard library, so "ours" is the one that'll stay in our
/// process whereas "theirs" will be inherited to a child.
///
/// The ours/theirs pipes are *not* specifically readable or writable. Each
/// one only supports a read or a write, but which is which depends on the
/// boolean flag given. If `ours_readable` is `true`, then `ours` is readable and
/// `theirs` is writable. Conversely, if `ours_readable` is `false`, then `ours`
/// is writable and `theirs` is readable.
///
/// Also note that the `ours` pipe is always a handle opened up in overlapped
/// mode. This means that technically speaking it should only ever be used
/// with `OVERLAPPED` instances, but also works out ok if it's only ever used
/// once at a time (which we do indeed guarantee).
pub fn anon_pipe(ours_readable: bool, their_handle_inheritable: bool) -> io::Result<Pipes> {
    // Note that we specifically do *not* use `CreatePipe` here because
    // unfortunately the anonymous pipes returned do not support overlapped
    // operations. Instead, we create a "hopefully unique" name and create a
    // named pipe which has overlapped operations enabled.
    //
    // Once we do this, we connect do it as usual via `CreateFileW`, and then
    // we return those reader/writer halves. Note that the `ours` pipe return
    // value is always the named pipe, whereas `theirs` is just the normal file.
    // This should hopefully shield us from child processes which assume their
    // stdout is a named pipe, which would indeed be odd!
    unsafe {
        let ours;
        let mut name;
        let mut tries = 0;
        let mut reject_remote_clients_flag = c::PIPE_REJECT_REMOTE_CLIENTS;
        loop {
            tries += 1;
            name = format!(
                r"\\.\pipe\__rust_anonymous_pipe1__.{}.{}",
                c::GetCurrentProcessId(),
                random_number()
            );
            let wide_name = OsStr::new(&name).encode_wide().chain(Some(0)).collect::<Vec<_>>();
            let mut flags = c::FILE_FLAG_FIRST_PIPE_INSTANCE | c::FILE_FLAG_OVERLAPPED;
            if ours_readable {
                flags |= c::PIPE_ACCESS_INBOUND;
            } else {
                flags |= c::PIPE_ACCESS_OUTBOUND;
            }

            let handle = c::CreateNamedPipeW(
                wide_name.as_ptr(),
                flags,
                c::PIPE_TYPE_BYTE
                    | c::PIPE_READMODE_BYTE
                    | c::PIPE_WAIT
                    | reject_remote_clients_flag,
                1,
                PIPE_BUFFER_CAPACITY,
                PIPE_BUFFER_CAPACITY,
                0,
                ptr::null_mut(),
            );

            // We pass the `FILE_FLAG_FIRST_PIPE_INSTANCE` flag above, and we're
            // also just doing a best effort at selecting a unique name. If
            // `ERROR_ACCESS_DENIED` is returned then it could mean that we
            // accidentally conflicted with an already existing pipe, so we try
            // again.
            //
            // Don't try again too much though as this could also perhaps be a
            // legit error.
            // If `ERROR_INVALID_PARAMETER` is returned, this probably means we're
            // running on pre-Vista version where `PIPE_REJECT_REMOTE_CLIENTS` is
            // not supported, so we continue retrying without it. This implies
            // reduced security on Windows versions older than Vista by allowing
            // connections to this pipe from remote machines.
            // Proper fix would increase the number of FFI imports and introduce
            // significant amount of Windows XP specific code with no clean
            // testing strategy
            // For more info, see https://github.com/rust-lang/rust/pull/37677.
            if handle == c::INVALID_HANDLE_VALUE {
                let err = io::Error::last_os_error();
                let raw_os_err = err.raw_os_error();
                if tries < 10 {
                    if raw_os_err == Some(c::ERROR_ACCESS_DENIED as i32) {
                        continue;
                    } else if reject_remote_clients_flag != 0
                        && raw_os_err == Some(c::ERROR_INVALID_PARAMETER as i32)
                    {
                        reject_remote_clients_flag = 0;
                        tries -= 1;
                        continue;
                    }
                }
                return Err(err);
            }
            ours = Handle::from_raw_handle(handle);
            break;
        }

        // Connect to the named pipe we just created. This handle is going to be
        // returned in `theirs`, so if `ours` is readable we want this to be
        // writable, otherwise if `ours` is writable we want this to be
        // readable.
        //
        // Additionally we don't enable overlapped mode on this because most
        // client processes aren't enabled to work with that.
        let mut opts = OpenOptions::new();
        opts.write(ours_readable);
        opts.read(!ours_readable);
        opts.share_mode(0);
        let size = mem::size_of::<c::SECURITY_ATTRIBUTES>();
        let mut sa = c::SECURITY_ATTRIBUTES {
            nLength: size as c::DWORD,
            lpSecurityDescriptor: ptr::null_mut(),
            bInheritHandle: their_handle_inheritable as i32,
        };
        opts.security_attributes(&mut sa);
        let theirs = File::open(Path::new(&name), &opts)?;
        let theirs = AnonPipe::Sync(theirs.into_inner());

        Ok(Pipes { ours: AnonPipe::Async(ours), theirs })
    }
}

/// Takes an asynchronous source pipe and returns a synchronous pipe suitable
/// for sending to a child process.
///
/// This is achieved by creating a new set of pipes and spawning a thread that
/// relays messages between the source and the synchronous pipe.
pub fn spawn_pipe_relay(
    source: &Handle,
    ours_readable: bool,
    their_handle_inheritable: bool,
) -> io::Result<AnonPipe> {
    // We need this handle to live for the lifetime of the thread spawned below.
    let source = AnonPipe::Async(source.duplicate(0, true, c::DUPLICATE_SAME_ACCESS)?);

    // create a new pair of anon pipes.
    let Pipes { theirs, ours } = anon_pipe(ours_readable, their_handle_inheritable)?;

    // Spawn a thread that passes messages from one pipe to the other.
    // Any errors will simply cause the thread to exit.
    let (reader, writer) = if ours_readable { (ours, source) } else { (source, ours) };
    crate::thread::spawn(move || {
        let mut buf = [0_u8; 4096];
        'reader: while let Ok(len) = reader.read(&mut buf) {
            if len == 0 {
                break;
            }
            let mut start = 0;
            while let Ok(written) = writer.write(&buf[start..len]) {
                start += written;
                if start == len {
                    continue 'reader;
                }
            }
            break;
        }
    });

    // Return the pipe that should be sent to the child process.
    Ok(theirs)
}

fn random_number() -> usize {
    static N: AtomicUsize = AtomicUsize::new(0);
    loop {
        if N.load(SeqCst) != 0 {
            return N.fetch_add(1, SeqCst);
        }

        N.store(hashmap_random_keys().0 as usize, SeqCst);
    }
}

// Abstracts over `ReadFileEx` and `WriteFileEx`
type AlertableIoFn = unsafe extern "system" fn(
    BorrowedHandle<'_>,
    c::LPVOID,
    c::DWORD,
    c::LPOVERLAPPED,
    c::LPOVERLAPPED_COMPLETION_ROUTINE,
) -> c::BOOL;

impl AnonPipe {
    pub fn handle(&self) -> &Handle {
        match self {
            Self::Async(ref handle) => handle,
            Self::Sync(ref handle) => handle,
        }
    }
    pub fn into_handle(self) -> Handle {
        self.into_inner()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let result = unsafe {
            let len = crate::cmp::min(buf.len(), c::DWORD::MAX as usize) as c::DWORD;
            match self {
                Self::Sync(ref handle) => handle.read(buf),
                Self::Async(_) => {
                    self.alertable_io_internal(c::ReadFileEx, buf.as_mut_ptr() as _, len)
                }
            }
        };

        match result {
            // The special treatment of BrokenPipe is to deal with Windows
            // pipe semantics, which yields this error when *reading* from
            // a pipe after the other end has closed; we interpret that as
            // EOF on the pipe.
            Err(ref e) if e.kind() == io::ErrorKind::BrokenPipe => Ok(0),
            _ => result,
        }
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        io::default_read_vectored(|buf| self.read(buf), bufs)
    }

    #[inline]
    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let len = crate::cmp::min(buf.len(), c::DWORD::MAX as usize) as c::DWORD;
            match self {
                Self::Sync(ref handle) => handle.write(buf),
                Self::Async(_) => {
                    self.alertable_io_internal(c::WriteFileEx, buf.as_ptr() as _, len)
                }
            }
        }
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        io::default_write_vectored(|buf| self.write(buf), bufs)
    }

    #[inline]
    pub fn is_write_vectored(&self) -> bool {
        false
    }

    /// Synchronizes asynchronous reads or writes using our anonymous pipe.
    ///
    /// This is a wrapper around [`ReadFileEx`] or [`WriteFileEx`] that uses
    /// [Asynchronous Procedure Call] (APC) to synchronize reads or writes.
    ///
    /// Note: This should not be used for handles we don't create.
    ///
    /// # Safety
    ///
    /// `buf` must be a pointer to a buffer that's valid for reads or writes
    /// up to `len` bytes. The `AlertableIoFn` must be either `ReadFileEx` or `WriteFileEx`
    ///
    /// [`ReadFileEx`]: https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-readfileex
    /// [`WriteFileEx`]: https://docs.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-writefileex
    /// [Asynchronous Procedure Call]: https://docs.microsoft.com/en-us/windows/win32/sync/asynchronous-procedure-calls
    unsafe fn alertable_io_internal(
        &self,
        io: AlertableIoFn,
        buf: c::LPVOID,
        len: c::DWORD,
    ) -> io::Result<usize> {
        // Use "alertable I/O" to synchronize the pipe I/O.
        // This has four steps.
        //
        // STEP 1: Start the asynchronous I/O operation.
        //         This simply calls either `ReadFileEx` or `WriteFileEx`,
        //         giving it a pointer to the buffer and callback function.
        //
        // STEP 2: Enter an alertable state.
        //         The callback set in step 1 will not be called until the thread
        //         enters an "alertable" state. This can be done using `SleepEx`.
        //
        // STEP 3: The callback
        //         Once the I/O is complete and the thread is in an alertable state,
        //         the callback will be run on the same thread as the call to
        //         `ReadFileEx` or `WriteFileEx` done in step 1.
        //         In the callback we simply set the result of the async operation.
        //
        // STEP 4: Return the result.
        //         At this point we'll have a result from the callback function
        //         and can simply return it. Note that we must not return earlier,
        //         while the I/O is still in progress.

        // The result that will be set from the asynchronous callback.
        let mut async_result: Option<AsyncResult> = None;
        struct AsyncResult {
            error: u32,
            transfered: u32,
        }

        // STEP 3: The callback.
        unsafe extern "system" fn callback(
            dwErrorCode: u32,
            dwNumberOfBytesTransfered: u32,
            lpOverlapped: *mut c::OVERLAPPED,
        ) {
            // Set `async_result` using a pointer smuggled through `hEvent`.
            let result = AsyncResult { error: dwErrorCode, transfered: dwNumberOfBytesTransfered };
            *(*lpOverlapped).hEvent.cast::<Option<AsyncResult>>() = Some(result);
        }

        // STEP 1: Start the I/O operation.
        let mut overlapped: c::OVERLAPPED = crate::mem::zeroed();
        // `hEvent` is unused by `ReadFileEx` and `WriteFileEx`.
        // Therefore the documentation suggests using it to smuggle a pointer to the callback.
        overlapped.hEvent = &mut async_result as *mut _ as *mut _;

        // Asynchronous read of the pipe.
        // If successful, `callback` will be called once it completes.
        let result = io(self.handle().as_handle(), buf, len, &mut overlapped, callback);
        if result == c::FALSE {
            // We can return here because the call failed.
            // After this we must not return until the I/O completes.
            return Err(io::Error::last_os_error());
        }

        // Wait indefinitely for the result.
        let result = loop {
            // STEP 2: Enter an alertable state.
            // The second parameter of `SleepEx` is used to make this sleep alertable.
            c::SleepEx(c::INFINITE, c::TRUE);
            if let Some(result) = async_result {
                break result;
            }
        };
        // STEP 4: Return the result.
        // `async_result` is always `Some` at this point
        match result.error {
            c::ERROR_SUCCESS => Ok(result.transfered as usize),
            error => Err(io::Error::from_raw_os_error(error as _)),
        }
    }
}

pub fn read2(p1: AnonPipe, v1: &mut Vec<u8>, p2: AnonPipe, v2: &mut Vec<u8>) -> io::Result<()> {
    let p1 = p1.into_handle();
    let p2 = p2.into_handle();

    let mut p1 = AsyncPipe::new(p1, v1)?;
    let mut p2 = AsyncPipe::new(p2, v2)?;
    let objs = [p1.event.as_raw_handle(), p2.event.as_raw_handle()];

    // In a loop we wait for either pipe's scheduled read operation to complete.
    // If the operation completes with 0 bytes, that means EOF was reached, in
    // which case we just finish out the other pipe entirely.
    //
    // Note that overlapped I/O is in general super unsafe because we have to
    // be careful to ensure that all pointers in play are valid for the entire
    // duration of the I/O operation (where tons of operations can also fail).
    // The destructor for `AsyncPipe` ends up taking care of most of this.
    loop {
        let res = unsafe { c::WaitForMultipleObjects(2, objs.as_ptr(), c::FALSE, c::INFINITE) };
        if res == c::WAIT_OBJECT_0 {
            if !p1.result()? || !p1.schedule_read()? {
                return p2.finish();
            }
        } else if res == c::WAIT_OBJECT_0 + 1 {
            if !p2.result()? || !p2.schedule_read()? {
                return p1.finish();
            }
        } else {
            return Err(io::Error::last_os_error());
        }
    }
}

struct AsyncPipe<'a> {
    pipe: Handle,
    event: Handle,
    overlapped: Box<c::OVERLAPPED>, // needs a stable address
    dst: &'a mut Vec<u8>,
    state: State,
}

#[derive(PartialEq, Debug)]
enum State {
    NotReading,
    Reading,
    Read(usize),
}

impl<'a> AsyncPipe<'a> {
    fn new(pipe: Handle, dst: &'a mut Vec<u8>) -> io::Result<AsyncPipe<'a>> {
        // Create an event which we'll use to coordinate our overlapped
        // operations, this event will be used in WaitForMultipleObjects
        // and passed as part of the OVERLAPPED handle.
        //
        // Note that we do a somewhat clever thing here by flagging the
        // event as being manually reset and setting it initially to the
        // signaled state. This means that we'll naturally fall through the
        // WaitForMultipleObjects call above for pipes created initially,
        // and the only time an even will go back to "unset" will be once an
        // I/O operation is successfully scheduled (what we want).
        let event = Handle::new_event(true, true)?;
        let mut overlapped: Box<c::OVERLAPPED> = unsafe { Box::new(mem::zeroed()) };
        overlapped.hEvent = event.as_raw_handle();
        Ok(AsyncPipe { pipe, overlapped, event, dst, state: State::NotReading })
    }

    /// Executes an overlapped read operation.
    ///
    /// Must not currently be reading, and returns whether the pipe is currently
    /// at EOF or not. If the pipe is not at EOF then `result()` must be called
    /// to complete the read later on (may block), but if the pipe is at EOF
    /// then `result()` should not be called as it will just block forever.
    fn schedule_read(&mut self) -> io::Result<bool> {
        assert_eq!(self.state, State::NotReading);
        let amt = unsafe {
            let slice = slice_to_end(self.dst);
            self.pipe.read_overlapped(slice, &mut *self.overlapped)?
        };

        // If this read finished immediately then our overlapped event will
        // remain signaled (it was signaled coming in here) and we'll progress
        // down to the method below.
        //
        // Otherwise the I/O operation is scheduled and the system set our event
        // to not signaled, so we flag ourselves into the reading state and move
        // on.
        self.state = match amt {
            Some(0) => return Ok(false),
            Some(amt) => State::Read(amt),
            None => State::Reading,
        };
        Ok(true)
    }

    /// Wait for the result of the overlapped operation previously executed.
    ///
    /// Takes a parameter `wait` which indicates if this pipe is currently being
    /// read whether the function should block waiting for the read to complete.
    ///
    /// Returns values:
    ///
    /// * `true` - finished any pending read and the pipe is not at EOF (keep
    ///            going)
    /// * `false` - finished any pending read and pipe is at EOF (stop issuing
    ///             reads)
    fn result(&mut self) -> io::Result<bool> {
        let amt = match self.state {
            State::NotReading => return Ok(true),
            State::Reading => self.pipe.overlapped_result(&mut *self.overlapped, true)?,
            State::Read(amt) => amt,
        };
        self.state = State::NotReading;
        unsafe {
            let len = self.dst.len();
            self.dst.set_len(len + amt);
        }
        Ok(amt != 0)
    }

    /// Finishes out reading this pipe entirely.
    ///
    /// Waits for any pending and schedule read, and then calls `read_to_end`
    /// if necessary to read all the remaining information.
    fn finish(&mut self) -> io::Result<()> {
        while self.result()? && self.schedule_read()? {
            // ...
        }
        Ok(())
    }
}

impl<'a> Drop for AsyncPipe<'a> {
    fn drop(&mut self) {
        match self.state {
            State::Reading => {}
            _ => return,
        }

        // If we have a pending read operation, then we have to make sure that
        // it's *done* before we actually drop this type. The kernel requires
        // that the `OVERLAPPED` and buffer pointers are valid for the entire
        // I/O operation.
        //
        // To do that, we call `CancelIo` to cancel any pending operation, and
        // if that succeeds we wait for the overlapped result.
        //
        // If anything here fails, there's not really much we can do, so we leak
        // the buffer/OVERLAPPED pointers to ensure we're at least memory safe.
        if self.pipe.cancel_io().is_err() || self.result().is_err() {
            let buf = mem::take(self.dst);
            let overlapped = Box::new(unsafe { mem::zeroed() });
            let overlapped = mem::replace(&mut self.overlapped, overlapped);
            mem::forget((buf, overlapped));
        }
    }
}

unsafe fn slice_to_end(v: &mut Vec<u8>) -> &mut [u8] {
    if v.capacity() == 0 {
        v.reserve(16);
    }
    if v.capacity() == v.len() {
        v.reserve(1);
    }
    slice::from_raw_parts_mut(v.as_mut_ptr().add(v.len()), v.capacity() - v.len())
}
