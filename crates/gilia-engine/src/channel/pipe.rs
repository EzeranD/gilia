// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::{
    os::fd::{AsFd, AsRawFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use nix::{
    errno::Errno,
    fcntl::OFlag,
    poll::{PollFd, PollFlags, PollTimeout, poll},
    unistd::{self, pipe2},
};
use pipewire::{
    loop_::{IoSource, Loop},
    spa::support::system::IoFlags,
};
use snafu::{ResultExt, Snafu};

use super::{
    Receiver, TryRecvError,
    wait::{WaitError, WaitOutcome, WaitSource, Waiter, WaiterKind, WakeError},
};

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum PipeWireError {
    #[snafu(display("channel is disconnected"))]
    Disconnected,
    #[snafu(transparent)]
    Pipe { source: PipeError },
    #[snafu(display("failed to wake PipeWire loop"))]
    Wake { source: WakeError },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum PipeError {
    #[snafu(display("failed to create notification pipe"))]
    Create { source: Errno },
    #[snafu(display("failed to read from notification pipe"))]
    Read { source: Errno },
    #[snafu(display("failed to write to notification pipe"))]
    Write { source: Errno },
    #[snafu(display("failed to poll notification pipe"))]
    Poll { source: Errno },
    #[snafu(display("unexpected notification pipe events: {events:?}"))]
    Events { events: PollFlags },
    #[snafu(display("notification pipe is closed"))]
    Closed,
}

pub struct PipeReceiver<T> {
    inner: Receiver<T>,
    pipe: Arc<PipeWaiter>,
    active: bool,
}

pub struct AttachedReceiver<'l, T> {
    _source: IoSource<'l, PipeReceiver<T>>,
}

#[derive(Debug)]
pub(super) struct PipeWaiter {
    read_fd: OwnedFd,
    write_fd: OwnedFd,
    notified: AtomicBool,
}

impl<T> PipeReceiver<T> {
    pub(super) fn new(inner: Receiver<T>) -> Result<Self, PipeError> {
        Ok(Self {
            inner,
            pipe: Arc::new(PipeWaiter::new()?),
            active: true,
        })
    }

    pub fn attach<F>(
        self,
        loop_: &Loop,
        callback: F,
    ) -> Result<AttachedReceiver<'_, T>, PipeWireError>
    where
        T: 'static,
        F: Fn(Result<T, PipeWireError>) + 'static,
    {
        self.register()?;

        let source = loop_.add_io(self, IoFlags::IN, move |receiver| {
            if !receiver.active {
                return;
            }

            receiver.unregister();

            if let Err(e) = receiver.pipe.consume() {
                receiver.finish(e.into(), &callback);
                return;
            }

            loop {
                match receiver.inner.try_recv() {
                    Ok(message) => callback(Ok(message)),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        receiver.finish(pipe_wire_error::DisconnectedSnafu.build(), &callback);
                        return;
                    }
                }
            }

            if let Err(e) = receiver.register() {
                receiver.finish(e, &callback);
            }
        });

        Ok(AttachedReceiver { _source: source })
    }

    fn register(&self) -> Result<(), PipeWireError> {
        if self.inner.is_ready() {
            self.pipe.wake().context(pipe_wire_error::WakeSnafu)?;
            return Ok(());
        }

        self.inner.register(&self.as_waiter(), WaiterKind::Observer);

        if self.inner.is_ready() {
            self.pipe.wake().context(pipe_wire_error::WakeSnafu)?;
        }

        Ok(())
    }

    fn unregister(&self) {
        self.inner.unregister(&self.as_waiter());
    }

    fn finish(&mut self, error: PipeWireError, callback: &impl Fn(Result<T, PipeWireError>)) {
        self.active = false;
        self.unregister();

        callback(Err(error));
    }

    fn as_waiter(&self) -> Arc<dyn Waiter> {
        self.pipe.clone()
    }
}

impl<T> AsRawFd for PipeReceiver<T> {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.pipe.read_fd.as_raw_fd()
    }
}

impl<T> Drop for PipeReceiver<T> {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl PipeWaiter {
    fn new() -> Result<Self, PipeError> {
        let (read_fd, write_fd) =
            pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).context(pipe_error::CreateSnafu)?;

        Ok(Self {
            read_fd,
            write_fd,
            notified: AtomicBool::new(false),
        })
    }

    fn consume(&self) -> Result<(), PipeError> {
        loop {
            match nix::unistd::read(&self.read_fd, &mut [0; 256]) {
                Ok(0) => return pipe_error::ClosedSnafu.fail(),
                Ok(_) | Err(Errno::EINTR) => {}
                Err(Errno::EAGAIN) => {
                    self.notified.store(false, Ordering::SeqCst);
                    return Ok(());
                }
                Err(source) => return Err(source).context(pipe_error::ReadSnafu),
            }
        }
    }
}

impl Waiter for PipeWaiter {
    fn wake(&self) -> Result<(), WakeError> {
        if self.notified.load(Ordering::SeqCst) || self.notified.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        loop {
            match unistd::write(&self.write_fd, &[1]) {
                Ok(_) | Err(Errno::EAGAIN) => return Ok(()),
                Err(Errno::EINTR) => {}
                Err(source) => {
                    self.notified.store(false, Ordering::SeqCst);
                    return Err(source)
                        .context(pipe_error::WriteSnafu)
                        .boxed()
                        .map_err(Into::into);
                }
            }
        }
    }

    fn wait(&self, deadline: Option<Instant>) -> Result<WaitOutcome, WaitError> {
        loop {
            let timeout = deadline.map_or(PollTimeout::NONE, |deadline| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let millis = remaining.as_nanos().div_ceil(1_000_000);
                PollTimeout::try_from(millis).unwrap_or(PollTimeout::MAX)
            });
            let mut fds = [PollFd::new(self.read_fd.as_fd(), PollFlags::POLLIN)];

            match poll(&mut fds, timeout) {
                Err(Errno::EINTR) => continue,
                Err(source) => {
                    return Err(source)
                        .context(pipe_error::PollSnafu)
                        .boxed()
                        .map_err(Into::into);
                }
                Ok(_) => {}
            }

            let events = fds[0].revents().unwrap_or(PollFlags::empty());
            if events.intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL) {
                return pipe_error::EventsSnafu { events }
                    .fail()
                    .boxed()
                    .map_err(Into::into);
            }
            if events.contains(PollFlags::POLLIN) {
                self.consume().boxed()?;
                return Ok(WaitOutcome::Retry);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(WaitOutcome::Deadline);
            }
        }
    }
}
