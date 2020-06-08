//! Interact with the file system using io-uring

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cmp;
use std::fs;
use std::future::Future;
use std::io;
use std::mem::{self, ManuallyDrop};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::slice;
use std::task::{Context, Poll};

use futures_core::ready;
use futures_io::{AsyncRead, AsyncBufRead, AsyncWrite, AsyncSeek};

use crate::drive::Drive;
use crate::drive::demo::DemoDriver;
use crate::engine::Engine;
use crate::event::{OpenAt, Cancellation};
use crate::Submission;

/// A file handle that runs on io-uring
pub struct File<D: Drive = DemoDriver<'static>> {
    engine: Engine<Op, D>,
    buf: Buffer,
    pos: usize,
}

#[derive(Debug, Eq, PartialEq)]
enum Op {
    Read,
    Write,
    Close,
    Statx,
}

/// A future representing an opening file.
pub struct Open<D: Drive = DemoDriver<'static>>(Submission<OpenAt, D>);

impl<D: Drive> Future for Open<D> {
    type Output = io::Result<File<D>>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<File<D>>> {
        let (_, driver, result) = ready!(self.inner().poll(ctx));
        let fd = result? as i32;
        Poll::Ready(Ok(File::from_fd(fd, driver)))
    }
}

impl<D: Drive> Open<D> {
    fn inner(self: Pin<&mut Self>) -> Pin<&mut Submission<OpenAt, D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.0) }
    }
}

/// A future representing a file being created.
pub struct Create<D: Drive = DemoDriver<'static>>(Submission<OpenAt, D>);

impl<D: Drive> Create<D> {
    fn inner(self: Pin<&mut Self>) -> Pin<&mut Submission<OpenAt, D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.0) }
    }
}

impl<D: Drive> Future for Create<D> {
    type Output = io::Result<File<D>>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<File<D>>> {
        let (_, driver, result) = ready!(self.inner().poll(ctx));
        let fd = result? as i32;
        Poll::Ready(Ok(File::from_fd(fd, driver)))
    }
}

impl File {
    /// Open a file using the default driver
    pub fn open(path: impl AsRef<Path>) -> Open {
        File::open_on_driver(path, DemoDriver::default())
    }

    /// Create a new file using the default driver
    pub fn create(path: impl AsRef<Path>) -> Create {
        File::create_on_driver(path, DemoDriver::default())
    }
}

impl<D: Drive> File<D> {
    /// Open a file
    pub fn open_on_driver(path: impl AsRef<Path>, driver: D) -> Open<D> {
        let flags = libc::O_CLOEXEC | libc::O_RDONLY;
        let event = OpenAt::new(path, libc::AT_FDCWD, flags, 0o666);
        Open(Submission::new(event, driver))
    }

    /// Create a file
    pub fn create_on_driver(path: impl AsRef<Path>, driver: D) -> Create<D> {
        let flags = libc::O_CLOEXEC | libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC;
        let event = OpenAt::new(path, libc::AT_FDCWD, flags, 0o666);
        Create(Submission::new(event, driver))
    }

    /// Take an existing file and run its IO on an io-uring driver
    pub fn run_on_driver(file: fs::File, driver: D) -> File<D> {
        let file = ManuallyDrop::new(file);
        File::from_fd(file.as_raw_fd(), driver)
    }

    fn from_fd(fd: RawFd, driver: D) -> File<D> {
        File {
            engine: Engine::new(fd, driver),
            buf: Buffer::new(),
            pos: 0,
        }
    }

    /// Access any data that has been read into the buffer, but not consumed
    ///
    /// This is similar to the fill_buf method from AsyncBufRead, but instead of performing IO if
    /// the buffer is empty, it will just return an empty slice. This method can be used to copy
    /// out any left over buffered data before closing or performing a write.
    pub fn read_buffered(&self) -> &[u8] {
        if self.engine.active() == Some(&Op::Read) && self.buf.data != ptr::null_mut() {
            unsafe {
                let ptr = self.buf.data.offset(self.buf.consumed as isize);
                slice::from_raw_parts(ptr, (self.buf.read - self.buf.consumed) as usize)
            }
        } else { &[] }
    }

    fn cancel(self: Pin<&mut Self>) {
        let (mut engine, buf, _) = self.split();
        if let Some(active) = engine.active() {
            let cancellation = match active {
                Op::Read | Op::Write    => buf.cancellation(),
                Op::Statx               => buf.statx_cancellation(),
                Op::Close               => Cancellation::null(),
            };
            engine.as_mut().cancel(cancellation);
            engine.unset_active();
        }
    }

    fn poll_file_size(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        if !matches!(self.engine.active(), None | Some(&Op::Statx)) {
            self.as_mut().cancel();
        }

        let (mut engine, buf, _) = self.split();
        engine.as_mut().set_active(Op::Statx);
        let statx = buf.as_statx();
        let flags = libc::AT_EMPTY_PATH;
        let mask = libc::STATX_SIZE;
        unsafe {
            ready!(engine.as_mut().poll(ctx, |sqe, fd| {
                uring_sys::io_uring_prep_statx(sqe.raw_mut(), fd, &0, flags, mask, statx);
            }))?;

            Poll::Ready(Ok((*statx).stx_size as usize))
        }
    }

    #[inline(always)]
    fn split(self: Pin<&mut Self>) -> (Pin<&mut Engine<Op, D>>, &mut Buffer, &mut usize) {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            (Pin::new_unchecked(&mut this.engine), &mut this.buf, &mut this.pos)
        }
    }

    #[inline(always)]
    fn engine(self: Pin<&mut Self>) -> Pin<&mut Engine<Op, D>> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.engine) }
    }

    #[inline(always)]
    fn buf(self: Pin<&mut Self>) -> Pin<&mut Buffer> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.buf) }
    }

    #[inline(always)]
    fn pos(self: Pin<&mut Self>) -> Pin<&mut usize> {
        unsafe { Pin::map_unchecked_mut(self, |this| &mut this.pos) }
    }
}

impl<D: Drive> AsyncRead for File<D> {
    fn poll_read(mut self: Pin<&mut Self>, ctx: &mut Context<'_>, buf: &mut [u8])
        -> Poll<io::Result<usize>>
    {
        let mut inner = ready!(self.as_mut().poll_fill_buf(ctx))?;
        let len = io::Read::read(&mut inner, buf)?;
        self.consume(len);
        Poll::Ready(Ok(len))
    }
}

impl<D: Drive> AsyncBufRead for File<D> {
    fn poll_fill_buf(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        if !matches!(self.engine.active(), None | Some(&Op::Read)) {
            println!("{:?}", self.engine.active());
            self.as_mut().cancel();
        }

        let (mut engine, buf, pos) = self.split();
        engine.as_mut().set_active(Op::Read);

        if buf.consumed >= buf.read {
            buf.read = unsafe {
                ready!(engine.poll(ctx, |sqe, fd| sqe.prep_read(fd, buf.read_buf(), *pos)))? as u32
            };
            buf.consumed = 0;
            *pos += buf.read as usize;
        }

        let consumed = buf.consumed as usize;
        let read = buf.read as usize;
        Poll::Ready(Ok(&buf.data()[consumed..read]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.buf().consume(amt);
    }
}

impl<D: Drive> AsyncWrite for File<D> {
    fn poll_write(mut self: Pin<&mut Self>, ctx: &mut Context<'_>, slice: &[u8]) -> Poll<io::Result<usize>> {
        if !matches!(self.engine.active(), None | Some(&Op::Write)) {
            self.as_mut().cancel();
        }

        let (mut engine, buf, pos) = self.split();
        engine.as_mut().set_active(Op::Write);

        if buf.written == 0 {
            buf.written = io::Write::write(&mut buf.data_mut(), slice).unwrap() as u32;
        }

        let result = unsafe {
            ready!(engine.poll(ctx, |sqe, fd| sqe.prep_write(fd, buf.write_buf(), *pos)))
        };

        if let &Ok(n) = &result {
            *pos += n;
        }
        buf.written = 0;

        Poll::Ready(result)
    }

    fn poll_flush(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.poll_write(ctx, &[]))?;
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !matches!(self.engine.active(), None | Some(&Op::Close)) {
            self.as_mut().cancel();
        }
        let mut engine = self.engine();
        engine.as_mut().set_active(Op::Close);
        unsafe { 
            ready!(engine.poll(ctx, |sqe, fd| uring_sys::io_uring_prep_close(sqe.raw_mut(), fd)))?;
        }
        Poll::Ready(Ok(()))
    }
}

impl<D: Drive> AsyncSeek for File<D> {
    fn poll_seek(mut self: Pin<&mut Self>, ctx: &mut Context, pos: io::SeekFrom)
        -> Poll<io::Result<u64>>
    {
        match pos {
            io::SeekFrom::Start(n)      => *self.as_mut().pos() = n as usize,
            io::SeekFrom::Current(n)    => {
                *self.as_mut().pos() += if n < 0 { n.abs() } else { n } as usize;
            }
            io::SeekFrom::End(n)        => {
                let end = ready!(self.as_mut().poll_file_size(ctx))?;
                *self.as_mut().pos() = end + if n < 0 { n.abs() } else { n} as usize;
            }
        }
        Poll::Ready(Ok(self.pos as u64))
    }
}

impl From<fs::File> for File {
    fn from(file: fs::File) -> File {
        File::run_on_driver(file, DemoDriver::default())
    }
}

impl<D: Drive> From<File<D>> for fs::File {
    fn from(mut file: File<D>) -> fs::File {
        unsafe {
            Pin::new_unchecked(&mut file).cancel();
            let file = ManuallyDrop::new(file);
            fs::File::from_raw_fd(file.engine.fd())
        }
    }
}

impl<D: Drive> Drop for File<D> {
    fn drop(&mut self) {
        unsafe {
            if self.engine.active().is_some() {
                Pin::new_unchecked(self).cancel();
            } else {
                libc::close(self.engine.fd());
            }
        }
    }
}

struct Buffer {
    data: *mut u8,
    capacity: u32,
    consumed: u32,
    read: u32,
    written: u32,
}

impl Buffer {
    fn new() -> Buffer {
        let capacity = 4096 * 2;
        let data = ptr::null_mut();

        Buffer {
            data, capacity,
            consumed: 0,
            read: 0,
            written: 0,
        }
    }

    fn read_buf(&mut self) -> &mut [u8] {
        &mut self.data_mut()[..]
    }

    fn write_buf(&mut self) -> &mut [u8] {
        let written = self.written as usize;
        &mut self.data_mut()[..written]
    }

    fn consume(&mut self, amt: usize) {
        self.consumed = cmp::min(self.consumed + amt as u32, self.read);
    }

    fn data(&mut self) -> &[u8] {
        let data = self.lazy_alloc();
        unsafe { slice::from_raw_parts(data, self.capacity as usize) }
    }

    fn data_mut(&mut self) -> &mut [u8] {
        let data = self.lazy_alloc();
        unsafe { slice::from_raw_parts_mut(data, self.capacity as usize) }
    }

    fn cancellation(&mut self) -> Cancellation {
        let data = mem::replace(&mut self.data, ptr::null_mut());
        if data == ptr::null_mut() {
            Cancellation::null()
        } else {
            unsafe { Cancellation::buffer(data, self.capacity as usize) }
        }
    }

    fn statx_cancellation(&mut self) -> Cancellation {
        let data = mem::replace(&mut self.data, ptr::null_mut());
        if data == ptr::null_mut() {
            Cancellation::null()
        } else {
            unsafe fn callback(statx: *mut (), _: usize) {
                dealloc(statx as *mut u8, Layout::new::<libc::statx>())
            }

            Cancellation::new(data as *mut (), 0, callback)
        }
    }

    #[inline(always)]
    fn lazy_alloc(&mut self) -> *mut u8 {
        if self.data == ptr::null_mut() {
            let layout = Layout::array::<u8>(self.capacity as usize).unwrap();
            let ptr = unsafe { alloc(layout) };
            if ptr == ptr::null_mut() {
                handle_alloc_error(layout);
            }
            self.data = ptr;
        }

        self.data
    }

    #[inline(always)]
    fn as_statx(&mut self) -> *mut libc::statx {
        unsafe {
            if self.data != ptr::null_mut() {
                    dealloc(self.data, Layout::array::<u8>(self.capacity as usize).unwrap());
            }

            self.data = alloc(Layout::new::<libc::statx>());
            self.data as *mut libc::statx
        }
    }
}

unsafe impl Send for Buffer { }
unsafe impl Sync for Buffer { }

impl Drop for Buffer {
    fn drop(&mut self) {
        if self.data != ptr::null_mut() {
            unsafe {
                dealloc(self.data, Layout::array::<u8>(self.capacity as usize).unwrap());
            }
        }
    }
}
